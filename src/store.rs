use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "rich_text.rs"]
pub mod rich_text;
pub use self::rich_text::{FragmentDefaults, NoteTextSnapshot, StyledFragment, TextRun, TextSelection,
    TextStyle, TextStylePatch};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub color: [u8; 4],
    pub width: u16,
    pub eraser: bool,
    pub points: Vec<[u16; 2]>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaBlock {
    /// Opaque ID, never a filesystem path supplied by the user or agent.
    pub id: String,
    /// Validated internal basename such as `<id>.png`.
    pub filename: String,
    #[serde(default)]
    pub caption: String,
    /// Display width as a percentage of available note content space.
    #[serde(default = "default_image_width")]
    pub width_percent: u8,
    #[serde(default)]
    pub position: usize,
}

fn default_image_width() -> u8 { 65 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Sparse, non-overlapping style spans over Unicode scalar indices in body.
    /// Legacy workspaces without this field keep the full original plain text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rich_text: Vec<TextRun>,
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
    pub images: Vec<MediaBlock>,
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
            rich_text: Vec::new(),
            kind: if kind == "board" { "board" } else { "note" }.to_string(),
            font_size: default_font_size(),
            font_choice: default_font(),
            pinned: false,
            strokes: Vec::new(),
            images: Vec::new(),
            updated: now.as_secs(),
        }
    }

    pub fn touch(&mut self) {
        self.updated = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    }

    /// Format selected text. Indices are half-open Unicode scalar positions,
    /// not UTF-8 byte offsets, UTF-16 offsets or visible grapheme clusters.
    pub fn format_text_range(&mut self, start: usize, end: usize,
                             patch: TextStylePatch) -> Result<(), String> {
        rich_text::format_range(&self.body, &mut self.rich_text, start, end, patch)?;
        self.touch();
        Ok(())
    }

    pub fn replace_text_range(&mut self, start: usize, end: usize,
                              replacement: &str) -> Result<(), String> {
        rich_text::replace_range(&mut self.body, &mut self.rich_text, start, end, replacement)?;
        self.touch();
        Ok(())
    }

    /// Preserve formatting on the unchanged prefix and suffix of an edited
    /// editor buffer, retaining the exact plain text including Unicode.
    pub fn set_body_preserving_format(&mut self, edited: String) -> Result<(), String> {
        rich_text::reconcile_body(&mut self.body, &mut self.rich_text, edited)?;
        self.touch();
        Ok(())
    }

    pub fn style_at(&self, char_index: usize) -> TextStyle {
        rich_text::style_at(&self.rich_text, char_index)
    }

    pub fn validate_rich_text(&self) -> Result<(), String> {
        rich_text::validate_runs(&self.body, &self.rich_text)
    }

    /// Derive the style used for typing at a caret or over a selection.
    pub fn caret_style(&self, selection: TextSelection) -> Result<TextStyle, String> {
        rich_text::caret_style(&self.body, &self.rich_text, selection)
    }

    /// Applying formatting with no selection updates a transient toolbar
    /// override for future typing. For selected text it updates stored runs.
    /// Returns true if the note itself changed.
    pub fn format_selection(&mut self, selection: TextSelection, patch: TextStylePatch,
                            typing_style: &mut Option<TextStyle>) -> Result<bool, String> {
        let changed = rich_text::format_selection(&self.body, &mut self.rich_text,
            selection, patch, typing_style)?;
        if changed { self.touch(); }
        Ok(changed)
    }

    /// Replace the selected text or insert at a caret. An explicit transient
    /// typing style takes precedence over adjacent text. Returns new caret.
    pub fn replace_selection(&mut self, selection: TextSelection, text: &str,
                             typing_style: Option<TextStyle>) -> Result<usize, String> {
        selection.validate(&self.body)?;
        let (start, end) = selection.range();
        rich_text::replace_range_with_style(&mut self.body, &mut self.rich_text,
            start, end, text, typing_style)?;
        let caret = start.checked_add(text.chars().count())
            .ok_or_else(|| "Edited caret position exceeds indexable length".to_string())?;
        self.touch();
        Ok(caret)
    }

    /// Copy a user-selected span with its exact styles, relative to character 0.
    pub fn copy_styled_range(&self, start: usize, end: usize) -> Result<StyledFragment, String> {
        let mut fragment = rich_text::copy_fragment(&self.body, &self.rich_text, start, end)?;
        fragment.defaults = Some(FragmentDefaults { size: self.font_size,
            font_family: self.font_choice.clone() });
        fragment.validate()?;
        Ok(fragment)
    }

    /// Insert a styled clipboard fragment and return the new caret position.
    pub fn paste_styled_range(&mut self, start: usize, end: usize,
                              fragment: &StyledFragment) -> Result<usize, String> {
        rich_text::paste_fragment(&mut self.body, &mut self.rich_text, start, end, fragment)?;
        self.touch();
        start.checked_add(fragment.text.chars().count())
            .ok_or_else(|| "Pasted caret position exceeds indexable length".to_string())
    }

    /// Snapshots exclude images, drawings and metadata. The caller owns
    /// transient undo history and should clear it after external remote edits.
    pub fn snapshot_text(&self) -> NoteTextSnapshot {
        NoteTextSnapshot { note_id: self.id.clone(), body: self.body.clone(),
            rich_text: self.rich_text.clone() }
    }

    pub fn restore_text_snapshot(&mut self, snapshot: &NoteTextSnapshot) -> Result<(), String> {
        if self.id != snapshot.note_id { return Err("Snapshot belongs to another note".into()); }
        snapshot.validate()?;
        self.body.clone_from(&snapshot.body);
        self.rich_text.clone_from(&snapshot.rich_text);
        self.touch();
        Ok(())
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

impl Workspace {
    fn validate_rich_text(&self) -> io::Result<()> {
        for note in &self.notes {
            note.validate_rich_text().map_err(|error|
                io::Error::new(io::ErrorKind::InvalidData,
                    format!("Invalid formatted text in note '{}': {error}", note.id)))?;
        }
        Ok(())
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

    pub(crate) fn lock_workspace(&self) -> io::Result<File> {
        let lock_path = self.path.with_extension("json.lock");
        let lock = OpenOptions::new().create(true).read(true).write(true).open(lock_path)?;
        // File::lock is an OS-managed cross-process exclusive lock. A GUI and
        // a headless agent therefore never perform overlapping read-modify-write
        // cycles. It is released automatically even if the process crashes.
        lock.lock()?;
        Ok(lock)
    }

    pub(crate) fn read_unlocked(&self) -> io::Result<Workspace> {
        let primary = fs::read(&self.path)?;
        let parse_valid = |bytes: &[u8]| -> io::Result<Workspace> {
            let workspace: Workspace = serde_json::from_slice(bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            workspace.validate_rich_text()?;
            Ok(workspace)
        };
        match parse_valid(&primary) {
            Ok(workspace) => Ok(workspace),
            Err(error) => {
                let bak = self.path.with_extension("json.bak");
                match fs::read(&bak).ok().and_then(|bytes| parse_valid(&bytes).ok()) {
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
        // Under this same OS lock, reclaim media no longer referenced by
        // either primary workspace or its previous-save backup. Newly imported
        // files use a .upload staging extension until inside this lock.
        let _ = self.prune_unused_media_locked(&workspace);
        Ok(outcome)
    }

    fn write_unlocked(&self, workspace: &Workspace) -> io::Result<()> {
        workspace.validate_rich_text()?;
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
                .filter(|workspace| workspace.validate_rich_text().is_ok()).is_some()
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

    #[test]
    fn legacy_json_round_trip_retains_plain_unicode_and_media_fields() {
        let old = serde_json::json!({
            "version":1,"active_id":"legacy","notes":[{
                "id":"legacy","title":"Old 🐈 note","body":"A\n猫👩‍🚀",
                "kind":"note","font_size":24,"font_choice":"Serif",
                "pinned":true,"strokes":[{"color":[1,2,3,255],"width":5,
                    "eraser":false,"points":[[4,5]]}],
                "images":[{"id":"img","filename":"img.png","caption":"Hi",
                    "width_percent":40,"position":0}],"updated":99
            }]
        });
        let migrated: Workspace = serde_json::from_value(old).unwrap();
        assert_eq!(migrated.notes[0].body, "A\n猫👩‍🚀");
        assert!(migrated.notes[0].rich_text.is_empty());
        let note = &migrated.notes[0];
        assert_eq!(note.strokes.len(), 1);
        assert_eq!(note.images.len(), 1);
        assert_eq!(note.font_size, 24);
        let encoded = serde_json::to_value(migrated).unwrap();
        assert!(encoded["notes"][0].get("rich_text").is_none());
        assert_eq!(encoded["notes"][0]["images"][0]["caption"], "Hi");
    }

    #[test]
    fn mixed_styles_survive_save_restart_and_backup_recovery() {
        let folder = std::env::temp_dir().join(format!("inkpad-rich-recovery-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let mut workspace = store.load().unwrap();
        let note = &mut workspace.notes[0];
        note.set_body_preserving_format("one 猫🦊 two".into()).unwrap();
        note.format_text_range(4, 6, TextStylePatch {
            bold: Some(true), color: Some("#cc00FF".into()), size: Some(30),
            font_family: Some("Mono".into()), ..Default::default()
        }).unwrap();
        let expected = note.clone();
        store.save(&workspace).unwrap();
        // A second save creates a backup containing the attributed text.
        store.save(&workspace).unwrap();
        assert_eq!(store.load().unwrap().notes[0], expected);
        fs::write(store.data_path(), b"{invalid json").unwrap();
        let recovered = store.load().unwrap();
        assert_eq!(recovered.notes[0], expected);
        assert_eq!(recovered.notes[0].style_at(5).font_family.as_deref(), Some("Mono"));
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn malformed_rich_ranges_fall_back_to_valid_backup_without_overwriting() {
        let folder = std::env::temp_dir().join(format!("inkpad-rich-malformed-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let workspace = store.load().unwrap();
        store.save(&workspace).unwrap();
        let mut malformed = serde_json::to_value(&workspace).unwrap();
        malformed["notes"][0]["rich_text"] = serde_json::json!([{
            "start":10000,"end":10001,"style":{"bold":true}
        }]);
        fs::write(store.data_path(), serde_json::to_vec(&malformed).unwrap()).unwrap();
        assert_eq!(store.load().unwrap().notes[0].title, "Welcome to Inkpad");
        assert!(fs::read(store.data_path()).unwrap().windows(6).any(|x| x == b"10000,"));
        fs::remove_dir_all(folder).unwrap();
    }
}
