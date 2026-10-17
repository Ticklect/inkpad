//! Rebuildable SQLite search mirror for an authoritative workspace.json.
//!
//! Never open this database without Store's workspace.json.lock: all clients
//! must validate the JSON content fingerprint before using indexed results.
//! Newer betas may change/remove this cache without migrating user notes.

use super::{Note, Workspace};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

const MIRROR_VERSION: i64 = 1;

fn sql(error: rusqlite::Error) -> io::Error { io::Error::other(error) }

fn refresh(conn: &mut Connection, bytes: &[u8], digest: &str) -> io::Result<()> {
    // Validate the entire source, including inline-media anchors, before it
    // may become the foundation of an indexed search result.
    let workspace: Workspace = serde_json::from_slice(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    workspace.validate_rich_text()?;

    let tx = conn.transaction().map_err(sql)?;
    tx.execute_batch("DROP TABLE IF EXISTS note_fts;
        DROP TABLE IF EXISTS notes;
        DROP TABLE IF EXISTS mirror_meta;
        CREATE TABLE mirror_meta (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            version INTEGER NOT NULL,
            source_hash TEXT NOT NULL
        );
        CREATE TABLE notes (
            ordinal INTEGER PRIMARY KEY,
            id TEXT NOT NULL UNIQUE,
            title_fold TEXT NOT NULL,
            body_fold TEXT NOT NULL,
            note_json BLOB NOT NULL
        );
        CREATE INDEX idx_notes_title_fold ON notes(title_fold);
        CREATE VIRTUAL TABLE note_fts USING fts5(
            title_fold, body_fold, tokenize = 'trigram case_sensitive 1',
            content = ''
        );").map_err(sql)?;
    {
        let mut insert = tx.prepare_cached("INSERT INTO notes
            (ordinal, id, title_fold, body_fold, note_json)
            VALUES (?1, ?2, ?3, ?4, ?5)").map_err(sql)?;
        let mut fts = tx.prepare_cached("INSERT INTO note_fts
            (rowid, title_fold, body_fold) VALUES (?1, ?2, ?3)").map_err(sql)?;
        for (index, note) in workspace.notes.iter().enumerate() {
            let ordinal = i64::try_from(index)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Too many notes"))?;
            let title_fold = note.title.to_lowercase();
            let body_fold = note.body.to_lowercase();
            let note_json = serde_json::to_vec(note)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            insert.execute(params![ordinal, note.id, title_fold, body_fold, note_json])
                .map_err(sql)?;
            fts.execute(params![ordinal + 1, title_fold, body_fold]).map_err(sql)?;
        }
    }
    tx.execute("INSERT INTO mirror_meta (singleton, version, source_hash)
        VALUES (1, ?1, ?2)", params![MIRROR_VERSION, digest]).map_err(sql)?;
    tx.commit().map_err(sql)?;
    Ok(())
}

fn query_once(db_path: &Path, source: &[u8], query: &str) -> io::Result<Vec<Note>> {
    let mut conn = Connection::open(db_path).map_err(sql)?;
    conn.busy_timeout(Duration::from_secs(2)).map_err(sql)?;
    // Pages may contain private deleted note text. Zero freed SQLite cells;
    // keep one standalone cache file rather than persistent WAL files.
    conn.execute_batch("PRAGMA secure_delete = ON; PRAGMA journal_mode = DELETE;")
        .map_err(sql)?;
    let digest = blake3::hash(source).to_hex().to_string();
    let current = conn.query_row("SELECT version, source_hash FROM mirror_meta
        WHERE singleton = 1", [], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
        .optional().ok().flatten();
    if current.as_ref().is_none_or(|(version, previous)|
        *version != MIRROR_VERSION || previous != &digest) {
        refresh(&mut conn, source, &digest)?;
    }

    // The trigram virtual-table index accelerates ordinary search words of
    // >=3 ASCII characters; other text falls back to exact SQLite instr().
    // This preserves the existing Rust Unicode-lowercase substring contract
    // for short terms, emoji, punctuation and arbitrary whitespace.
    let fts_query = query.len() >= 3 && query.bytes().all(|b| b.is_ascii_alphanumeric());
    let sql_query = if fts_query {
        "SELECT notes.note_json FROM note_fts
          JOIN notes ON notes.ordinal = note_fts.rowid - 1
          WHERE note_fts MATCH ?1 ORDER BY notes.ordinal"
    } else {
        "SELECT note_json FROM notes
          WHERE instr(title_fold, ?1) > 0 OR instr(body_fold, ?1) > 0
          ORDER BY ordinal"
    };
    let term = if fts_query { format!("\"{query}\"") } else { query.to_string() };
    let mut stmt = conn.prepare_cached(sql_query).map_err(sql)?;
    let mut results = Vec::new();
    let rows = stmt.query_map(params![term], |row| row.get::<_, Vec<u8>>(0)).map_err(sql)?;
    for row in rows {
        let encoded = row.map_err(sql)?;
        let note: Note = serde_json::from_slice(&encoded)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        // An indexed match is only a candidate. Guard against FTS tokenization
        // differences and preserve exact case-fold/substring search semantics.
        if note.title.to_lowercase().contains(query) || note.body.to_lowercase().contains(query) {
            results.push(note);
        }
    }
    Ok(results)
}

pub(super) fn search(db_path: &Path, source: &[u8], query: &str) -> io::Result<Vec<Note>> {
    match query_once(db_path, source, query) {
        Ok(notes) => Ok(notes),
        Err(first_error) => {
            // The DB is exclusively disposable. The caller owns the JSON OS
            // lock; removing a corrupt SQLite cache cannot delete user notes.
            // Preserve the original failure if the filesystem won't permit
            // removing/recreating the sidecar; Store falls back to JSON then.
            fs::remove_file(db_path).map_err(|_| first_error)?;
            query_once(db_path, source, query)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_trigram_really_uses_virtual_table_index() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE note_fts USING fts5(
            title_fold, body_fold, tokenize = 'trigram case_sensitive 1');
            INSERT INTO note_fts(rowid,title_fold,body_fold) VALUES (1,'alpha','needle in haystack');")
            .unwrap();
        let plan: String = conn.query_row("EXPLAIN QUERY PLAN
            SELECT rowid FROM note_fts WHERE note_fts MATCH ?1", ["\"needle\""],
            |row| row.get(3)).unwrap();
        assert!(plan.contains("VIRTUAL TABLE INDEX"), "{plan}");
        let matched: i64 = conn.query_row("SELECT rowid FROM note_fts
            WHERE note_fts MATCH ?1", ["\"needle\""], |row| row.get(0)).unwrap();
        assert_eq!(matched, 1);
    }
}
