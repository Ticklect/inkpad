use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub color: [u8; 4],
    pub width: u16,
    pub eraser: bool,
    pub points: Vec<[u16; 2]>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub kind: String,
    #[serde(default = "default_font_size")]
    pub font_size: i32,
    #[serde(default = "default_font")]
    pub font_choice: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub strokes: Vec<Stroke>,
    #[serde(default)]
    pub updated: u64,
}

fn default_font_size() -> i32 { 17 }
fn default_font() -> String { "Sans".to_string() }

impl Note {
    pub fn create(kind: &str) -> Self {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        Self {
            id: format!("{:x}-{:x}", now.as_nanos(), std::process::id()),
            title: if kind == "board" { "Untitled board" } else { "Untitled note" }.to_string(),
            body: String::new(),
            kind: if kind == "board" { "board" } else { "note" }.to_string(),
            font_size: default_font_size(),
            font_choice: default_font(),
            pinned: false,
            strokes: Vec::new(),
            updated: now.as_secs(),
        }
    }

    pub fn touch(&mut self) {
        self.updated = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub version: u32,
    pub active_id: String,
    pub notes: Vec<Note>,
}

impl Default for Workspace {
    fn default() -> Self {
        let mut note = Note::create("note");
        note.title = "Welcome to Inkpad".to_string();
        note.body = "Your notes live on this computer and save automatically.\n\nUse Text to write and Draw to sketch, both inside this page.\n\nThe left sidebar keeps everything organised. No account or internet connection required.".to_string();
        Self { version: 1, active_id: note.id.clone(), notes: vec![note] }
    }
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn from_system() -> io::Result<Self> {
        let folder = if let Some(custom) = std::env::var_os("INKPAD_DATA_DIR") {
            PathBuf::from(custom)
        } else if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            PathBuf::from(local).join("Inkpad")
        } else {
            std::env::current_dir()?.join("InkpadData")
        };
        fs::create_dir_all(&folder)?;
        Ok(Self { path: folder.join("workspace.json") })
    }

    #[cfg(test)]
    pub fn in_folder(folder: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&folder)?;
        Ok(Self { path: folder.join("workspace.json") })
    }

    pub fn data_path(&self) -> &std::path::Path { &self.path }

    pub fn load(&self) -> io::Result<Workspace> {
        let _lock = self.lock_workspace()?;
        if !self.path.exists() {
            let workspace = Workspace::default();
            self.write_unlocked(&workspace)?;
            return Ok(workspace);
        }
        self.read_unlocked()
    }

    pub fn read_latest(&self) -> io::Result<Workspace> { self.load() }

    fn lock_workspace(&self) -> io::Result<File> {
        let lock_path = self.path.with_extension("json.lock");
        let lock = OpenOptions::new().create(true).read(true).write(true).open(lock_path)?;
        // File::lock is an OS-managed cross-process exclusive lock. A GUI and
        // a headless agent therefore never perform overlapping read-modify-write
        // cycles. It is released automatically even if the process crashes.
        lock.lock()?;
        Ok(lock)
    }

    fn read_unlocked(&self) -> io::Result<Workspace> {
        let primary = fs::read(&self.path)?;
        match serde_json::from_slice::<Workspace>(&primary) {
            Ok(workspace) => Ok(workspace),
            Err(error) => {
                let bak = self.path.with_extension("json.bak");
                match fs::read(&bak).ok().and_then(|bytes| serde_json::from_slice::<Workspace>(&bytes).ok()) {
                    Some(workspace) => Ok(workspace),
                    None => Err(io::Error::new(io::ErrorKind::InvalidData,
                        format!("Could not read saved notes ({}). Your data was not overwritten. Path: {}", error, self.path.display()))),
                }
            }
        }
    }

    #[cfg(test)]
    pub fn save(&self, workspace: &Workspace) -> io::Result<()> {
        let _lock = self.lock_workspace()?;
        self.write_unlocked(workspace)
    }

    /// The only correct way for agents to edit a workspace: fresh read,
    /// mutation and durable write within one OS-level cross-process lock.
    pub fn mutate<T>(&self, edit: impl FnOnce(&mut Workspace) -> io::Result<T>) -> io::Result<T> {
        let _lock = self.lock_workspace()?;
        let mut workspace = if self.path.exists() { self.read_unlocked()? } else { Workspace::default() };
        let outcome = edit(&mut workspace)?;
        self.write_unlocked(&workspace)?;
        Ok(outcome)
    }

    fn write_unlocked(&self, workspace: &Workspace) -> io::Result<()> {
        let encoded = serde_json::to_vec_pretty(workspace)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let temp = self.path.with_extension("json.tmp");
        let mut file = File::create(&temp)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        drop(file);
        if self.path.exists() {
            // Never replace a valid backup with a corrupt/truncated primary.
            // A corrupt primary can be recovered from backup on next launch.
            if fs::read(&self.path).ok()
                .and_then(|data| serde_json::from_slice::<Workspace>(&data).ok())
                .is_some()
            {
                fs::copy(&self.path, self.path.with_extension("json.bak"))?;
            }
        }
        if let Err(error) = fs::rename(&temp, &self.path) {
            // On platforms with no replacement-rename support, retain the backup
            // and try the normal move after removing only the old primary.
            if error.kind() == io::ErrorKind::AlreadyExists {
                fs::remove_file(&self.path)?;
                fs::rename(&temp, &self.path)?;
            } else {
                return Err(error);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saves_and_restores_multiline_unicode_and_strokes() {
        let folder = std::env::temp_dir().join(format!("inkpad-test-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let mut workspace = store.load().unwrap();
        let mut board = Note::create("board");
        board.title = "画 / sketch".into();
        board.strokes.push(Stroke { color: [23, 42, 200, 255], width: 12, eraser: false, points: vec![[3, 4], [80, 90]] });
        workspace.notes.push(board);
        workspace.notes[0].body = "hello\nWorld 🚀".into();
        store.save(&workspace).unwrap();
        let restored = store.load().unwrap();
        assert_eq!(restored.notes[0].body, "hello\nWorld 🚀");
        assert_eq!(restored.notes[1].strokes[0].points, vec![[3, 4], [80, 90]]);
        assert_eq!(restored.notes[1].title, "画 / sketch");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn corrupt_primary_does_not_erase_backup() {
        let folder = std::env::temp_dir().join(format!("inkpad-recovery-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let original = store.load().unwrap();
        store.save(&original).unwrap();
        fs::write(store.data_path(), b"truncated").unwrap();
        assert_eq!(store.load().unwrap().notes[0].title, "Welcome to Inkpad");
        fs::remove_dir_all(folder).unwrap();
    }
}
