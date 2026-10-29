use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "rich_text.rs"]
pub mod rich_text;
pub use self::rich_text::{FragmentDefaults, NoteTextSnapshot, StyledFragment, TextRun, TextSelection,
    TextStyle, TextStylePatch};

#[path = "drawing.rs"]
pub mod drawing;
pub use self::drawing::{DrawFill, DrawObject, DrawShape, DrawStyle, DrawingData,
    DrawingHistory, DrawingSnapshot, GradientStop, Layer, Point, Transform};

#[path = "sqlite_store.rs"]
mod sqlite_store;

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

/// Optional placement of an existing image at a boundary in Note.body.
/// Offsets count Unicode scalar values, like rich_text runs and MCP ranges;
/// they do not insert placeholder characters into the original note text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineImagePlacement {
    pub image_id: String,
    pub char_offset: usize,
}

/// Lossless layout contract for a mixed text/picture editor.
/// Text ranges always index the unmodified Note.body; Image IDs resolve to
/// Note.images metadata. Legacy/unplaced images appear after the final text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteContentBlock {
    Text { start: usize, end: usize },
    Image { image_id: String },
}

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
    /// Editable vector objects; absent in legacy workspaces, preserving strokes.
    #[serde(default, skip_serializing_if = "DrawingData::is_empty")]
    pub drawing: DrawingData,
    #[serde(default)]
    pub images: Vec<MediaBlock>,
    /// Absent in old workspaces; an unplaced image stays at the end of its note.
    /// Image metadata, image.position and body text retain their old meanings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inline_media: Vec<InlineImagePlacement>,
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
            drawing: DrawingData::default(),
            images: Vec::new(),
            inline_media: Vec::new(),
            updated: now.as_secs(),
        }
    }

    pub fn touch(&mut self) {
        self.updated = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    }

    pub fn validate_inline_media(&self) -> Result<(), String> {
        if self.inline_media.len() > 100 || self.inline_media.len() > self.images.len() {
            return Err("Too many inline image placements".into());
        }
        let total = self.body.chars().count();
        let mut seen = std::collections::HashSet::new();
        for placement in &self.inline_media {
            if !seen.insert(placement.image_id.as_str()) {
                return Err("Duplicate inline image placement".into());
            }
            if !self.images.iter().any(|image| image.id == placement.image_id) {
                return Err("Inline image placement references a missing image".into());
            }
            if placement.char_offset > total {
                return Err("Inline image placement exceeds note text length".into());
            }
        }
        Ok(())
    }

    /// Ordered editor/export layout; content is never duplicated or stripped.
    /// Equal offsets follow the existing image.position order.
    pub fn content_blocks(&self) -> Result<Vec<NoteContentBlock>, String> {
        self.validate_inline_media()?;
        let end = self.body.chars().count();
        let mut placed = self.images.iter().enumerate().map(|(original, image)| {
            let offset = self.inline_media.iter()
                .find(|placement| placement.image_id == image.id)
                .map_or(end, |placement| placement.char_offset);
            (offset, image.position, original, image.id.clone())
        }).collect::<Vec<_>>();
        placed.sort_by_key(|(offset, position, original, _)| (*offset, *position, *original));
        let mut blocks = Vec::with_capacity(placed.len().saturating_mul(2).saturating_add(1));
        let mut cursor = 0;
        for (offset, _, _, image_id) in placed {
            // Include even empty editable text regions so a WYSIWYG editor
            // has a real caret target before, between and after pictures.
            blocks.push(NoteContentBlock::Text { start: cursor, end: offset });
            blocks.push(NoteContentBlock::Image { image_id });
            cursor = offset;
        }
        blocks.push(NoteContentBlock::Text { start: cursor, end });
        Ok(blocks)
    }

    /// Image anchored at a text boundary, or None for legacy end placement.
    /// A missing ID is not silently treated as an existing unplaced picture.
    pub fn image_anchor(&self, image_id: &str) -> Result<Option<usize>, String> {
        self.validate_inline_media()?;
        if !self.images.iter().any(|image| image.id == image_id) {
            return Err("Image not found".into());
        }
        Ok(self.inline_media.iter().find(|entry| entry.image_id == image_id)
            .map(|entry| entry.char_offset))
    }

    pub fn set_image_anchor(&mut self, image_id: &str, offset: Option<usize>) -> Result<(), String> {
        self.validate_inline_media()?;
        if !self.images.iter().any(|image| image.id == image_id) {
            return Err("Image not found".into());
        }
        if offset.is_some_and(|value| value > self.body.chars().count()) {
            return Err("Inline image placement exceeds note text length".into());
        }
        self.inline_media.retain(|entry| entry.image_id != image_id);
        if let Some(char_offset) = offset {
            self.inline_media.push(InlineImagePlacement { image_id: image_id.into(), char_offset });
        }
        self.touch();
        Ok(())
    }

    /// Right-biased image boundary transformation: insertion at an anchor
    /// shifts its picture after newly typed text; replaced text containing an
    /// anchor leaves it after the replacement. Call only after a successful
    /// text operation (or on a cloned candidate) to preserve atomicity.
    pub fn rebase_image_placements_for_edit(
        &mut self, first_changed_scalar: usize, last_changed_scalar: usize,
        replacement_scalar_len: usize,
    ) {
        let removed = last_changed_scalar - first_changed_scalar;
        let new_end = first_changed_scalar + replacement_scalar_len;
        for placement in &mut self.inline_media {
            let offset = placement.char_offset;
            placement.char_offset = if offset < first_changed_scalar {
                offset
            } else if offset <= last_changed_scalar {
                new_end
            } else if replacement_scalar_len >= removed {
                offset + (replacement_scalar_len - removed)
            } else {
                offset - (removed - replacement_scalar_len)
            };
        }
    }

    /// Rebase a plain-text revision using the same single contiguous edit
    /// inferred by rich_text::reconcile_body; used by editor undo/redo where
    /// the previous and restored buffers are available.
    pub fn rebase_image_placements_for_text_change(&mut self, old_body: &str) {
        if self.inline_media.is_empty() || old_body == self.body { return; }
        let old_len = old_body.chars().count();
        let new_len = self.body.chars().count();
        let prefix = old_body.chars().zip(self.body.chars())
            .take_while(|(a, b)| a == b).count();
        let suffix = old_body.chars().rev().zip(self.body.chars().rev())
            .take((old_len - prefix).min(new_len - prefix))
            .take_while(|(a, b)| a == b).count();
        self.rebase_image_placements_for_edit(prefix, old_len - suffix, new_len - prefix - suffix);
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
        self.rebase_image_placements_for_edit(start, end, replacement.chars().count());
        self.touch();
        Ok(())
    }

    /// Preserve formatting on the unchanged prefix and suffix of an edited
    /// editor buffer, retaining the exact plain text including Unicode.
    pub fn set_body_preserving_format(&mut self, edited: String) -> Result<(), String> {
        let old = if self.inline_media.is_empty() { None } else { Some(self.body.clone()) };
        rich_text::reconcile_body(&mut self.body, &mut self.rich_text, edited)?;
        if let Some(old) = old { self.rebase_image_placements_for_text_change(&old); }
        self.touch();
        Ok(())
    }

    pub fn style_at(&self, char_index: usize) -> TextStyle {
        rich_text::style_at(&self.rich_text, char_index)
    }

    pub fn validate_rich_text(&self) -> Result<(), String> {
        rich_text::validate_runs(&self.body, &self.rich_text)
    }

    pub fn validate_drawing(&self) -> Result<(), String> {
        self.drawing.validate()
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
        self.rebase_image_placements_for_edit(start, end, text.chars().count());
        let caret = start.checked_add(text.chars().count())
            .ok_or_else(|| "Edited caret position exceeds indexable length".to_string())?;
        self.touch();
        Ok(caret)
    }

    /// Edit a specific text segment from content_blocks(). Its block index
    /// disambiguates caret slots between pictures at the SAME scalar offset:
    /// earlier pictures remain before inserted text, while later pictures
    /// follow it. A bare scalar caret cannot express that distinction.
    pub fn replace_text_in_content_block(
        &mut self, block_index: usize, start: usize, end: usize,
        replacement: &str, typing_style: Option<TextStyle>,
    ) -> Result<usize, String> {
        let blocks = self.content_blocks()?;
        let Some(NoteContentBlock::Text { start: block_start, end: block_end }) =
            blocks.get(block_index) else {
            return Err("Selected content block is not editable text".into());
        };
        if start < *block_start || start > end || end > *block_end {
            return Err("Text edit is outside its content block".into());
        }
        let body_end = self.body.chars().count();
        let preceding_same_boundary = blocks[..block_index].iter().filter_map(|block| {
            let NoteContentBlock::Image { image_id } = block else { return None; };
            let offset = self.inline_media.iter().find(|item| item.image_id == *image_id)
                .map_or(body_end, |item| item.char_offset);
            (offset == start).then(|| image_id.clone())
        }).collect::<Vec<_>>();

        let caret = self.replace_selection(TextSelection { anchor:start, focus:end },
            replacement, typing_style)?;
        for image_id in preceding_same_boundary {
            self.set_image_anchor(&image_id, Some(start))?;
        }
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
        self.rebase_image_placements_for_edit(start, end, fragment.text.chars().count());
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
        let old = if self.inline_media.is_empty() { None } else { Some(self.body.clone()) };
        self.body.clone_from(&snapshot.body);
        self.rich_text.clone_from(&snapshot.rich_text);
        if let Some(old) = old { self.rebase_image_placements_for_text_change(&old); }
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
            note.validate_drawing().map_err(|error|
                io::Error::new(io::ErrorKind::InvalidData,
                    format!("Invalid vector drawing in note '{}': {error}", note.id)))?;
            note.validate_inline_media().map_err(|error|
                io::Error::new(io::ErrorKind::InvalidData,
                    format!("Invalid inline image placement in note '{}': {error}", note.id)))?;
        }
        Ok(())
    }
}

pub struct Store {
    path: PathBuf,
    read_cache: Mutex<Option<ValidatedReadCache>>,
}

// The MCP process can serve thousands of reads of an unchanged notebook.
// Cache only the parsed representation, never the source file: each access
// rereads the primary under the cross-process lock and compares ALL bytes.
// This catches edits even if an external writer preserves size and timestamps.
struct ValidatedReadCache {
    bytes: Vec<u8>,
    workspace: Arc<Workspace>,
    search_index: Option<Arc<Vec<(String, String)>>>,
}

const MAX_READ_CACHE_BYTES: usize = 32 * 1024 * 1024;

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
        Ok(Self { path: folder.join("workspace.json"), read_cache: Mutex::new(None) })
    }

    #[cfg(test)]
    pub fn in_folder(folder: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&folder)?;
        Ok(Self { path: folder.join("workspace.json"), read_cache: Mutex::new(None) })
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

    /// MCP-only read: retains a validated parsed notebook if the saved bytes
    /// are identical. Always obtains the OS lock and rereads the primary file;
    /// a corrupt primary takes the existing backup-recovery path without cache.
    pub fn read_cached(&self) -> io::Result<Arc<Workspace>> {
        let _lock = self.lock_workspace()?;
        if !self.path.exists() {
            let workspace = Workspace::default();
            self.write_unlocked(&workspace)?;
            return Ok(Arc::new(workspace));
        }
        let bytes = fs::read(&self.path)?;
        let mut cache = self.read_cache.lock().map_err(|error|
            io::Error::other(format!("Workspace read cache unavailable: {error}")))?;
        if let Some(previous) = cache.as_ref() {
            if previous.bytes == bytes {
                return Ok(Arc::clone(&previous.workspace));
            }
        }
        // Validate against the same rules used by read_unlocked. Do not cache
        // a recovered backup against bytes from a corrupt primary file.
        let workspace = serde_json::from_slice::<Workspace>(&bytes).ok()
            .filter(|workspace| workspace.validate_rich_text().is_ok());
        let Some(workspace) = workspace else {
            *cache = None;
            return self.read_unlocked().map(Arc::new);
        };
        let workspace = Arc::new(workspace);
        *cache = (bytes.len() <= MAX_READ_CACHE_BYTES).then(|| ValidatedReadCache {
            bytes, workspace: Arc::clone(&workspace), search_index: None,
        });
        Ok(workspace)
    }

    /// Returns the same verified snapshot as read_cached, with a lazily built
    /// Unicode-lowercased search index. The index belongs only to that exact
    /// byte-verified snapshot; a later on-disk edit rebuilds both together.
    pub fn read_cached_with_search_index(&self) -> io::Result<(Arc<Workspace>, Arc<Vec<(String, String)>>)> {
        let workspace = self.read_cached()?;
        let mut cache = self.read_cache.lock().map_err(|error|
            io::Error::other(format!("Workspace read cache unavailable: {error}")))?;
        let build = || Arc::new(workspace.notes.iter().map(|note|
            (note.title.to_lowercase(), note.body.to_lowercase())).collect::<Vec<_>>());
        if let Some(current) = cache.as_mut() {
            if Arc::ptr_eq(&current.workspace, &workspace) {
                let index = current.search_index.get_or_insert_with(build);
                return Ok((workspace, Arc::clone(index)));
            }
        }
        // Oversized workspaces are deliberately not retained in the cache.
        Ok((Arc::clone(&workspace), build()))
    }

    /// Search the indexed SQLite mirror while holding the same cross-process
    /// file lock as JSON writes. SQLite is disposable: the primary JSON and its
    /// recovery backup always determine the correct answer. A newly edited
    /// JSON file (even with identical length and timestamp) invalidates the
    /// mirror by its BLAKE3 content digest. Results retain JSON note ordering.
    ///
    /// This path avoids deserializing an entire large workspace on repeated
    /// cold-process searches. It does not alter the GUI's JSON save contract.
    pub fn search_indexed(&self, lowercase_query: &str) -> io::Result<Vec<Note>> {
        let _lock = self.lock_workspace()?;
        if !self.path.exists() {
            self.write_unlocked(&Workspace::default())?;
        }
        let bytes = fs::read(&self.path)?;
        let db_path = self.path.with_extension("sqlite3");
        match sqlite_store::search(&db_path, &bytes, lowercase_query) {
            Ok(notes) => Ok(notes),
            Err(_) => {
                // Includes missing/corrupt SQLite, invalid primary JSON and
                // permission failures. A readable backup is still usable.
                let workspace = self.read_unlocked()?;
                Ok(workspace.notes.into_iter().filter(|note| {
                    note.title.to_lowercase().contains(lowercase_query)
                        || note.body.to_lowercase().contains(lowercase_query)
                }).collect())
            }
        }
    }

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
        self.read_unlocked_with_primary_validity().map(|(workspace, _)| workspace)
    }

    /// The flag describes the primary file we actually validated while holding
    /// the workspace lock. A successful backup recovery must never mark a
    /// damaged primary as safe to copy over the recovery backup.
    fn read_unlocked_with_primary_validity(&self) -> io::Result<(Workspace, bool)> {
        let primary = fs::read(&self.path)?;
        let parse_valid = |bytes: &[u8]| -> io::Result<Workspace> {
            let workspace: Workspace = serde_json::from_slice(bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            workspace.validate_rich_text()?;
            Ok(workspace)
        };
        match parse_valid(&primary) {
            Ok(workspace) => Ok((workspace, true)),
            Err(error) => {
                let bak = self.path.with_extension("json.bak");
                match fs::read(&bak).ok().and_then(|bytes| parse_valid(&bytes).ok()) {
                    Some(workspace) => Ok((workspace, false)),
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
        let (mut workspace, primary_valid) = if self.path.exists() {
            self.read_unlocked_with_primary_validity()?
        } else {
            (Workspace::default(), false)
        };
        let outcome = edit(&mut workspace)?;
        self.write_unlocked_with_primary_validity(&workspace, Some(primary_valid))?;
        // Under this same OS lock, reclaim media no longer referenced by
        // either primary workspace or its previous-save backup. Newly imported
        // files use a .upload staging extension until inside this lock.
        let _ = self.prune_unused_media_locked(&workspace);
        Ok(outcome)
    }

    fn write_unlocked(&self, workspace: &Workspace) -> io::Result<()> {
        self.write_unlocked_with_primary_validity(workspace, None)
    }

    fn write_unlocked_with_primary_validity(&self, workspace: &Workspace, primary_valid: Option<bool>) -> io::Result<()> {
        workspace.validate_rich_text()?;
        // Stream identical compact JSON into a bounded write buffer instead
        // of materializing a second 9+ MB Vec for every large notebook save.
        // The temp file is still flushed and synced before backup/replace.
        let temp = self.path.with_extension("json.tmp");
        let mut file = BufWriter::with_capacity(128 * 1024, File::create(&temp)?);
        serde_json::to_writer(&mut file, workspace)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        file.flush()?;
        file.get_ref().sync_all()?;
        drop(file);
        if self.path.exists() {
            // Never replace a valid backup with a corrupt/truncated primary.
            // A corrupt primary can be recovered from backup on next launch.
            let valid = primary_valid.unwrap_or_else(|| fs::read(&self.path).ok()
                .and_then(|data| serde_json::from_slice::<Workspace>(&data).ok())
                .is_some_and(|workspace| workspace.validate_rich_text().is_ok()));
            if valid {
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
    #[ignore = "benchmark: compare old Vec serialization with streaming on a synthetic large workspace"]
    fn compare_atomic_json_serializers_benchmark() {
        use std::io::BufWriter;
        use std::time::Instant;
        let folder = std::env::temp_dir().join(format!("inkpad-serializer-bench-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&folder).unwrap();
        let mut workspace = Workspace::default();
        workspace.notes.clear();
        for index in 0..2200 {
            let mut note = Note::create("note");
            note.id = format!("bench-{index:05}");
            note.title = format!("Synthetic Unicode note #{index}");
            note.body = "Large synthetic note content and 漢字 emoji 🦆. ".repeat(77);
            workspace.notes.push(note);
        }
        let payload = serde_json::to_vec(&workspace).unwrap();
        let expected_len = payload.len();
        let mut vec_times = Vec::new();
        let mut streaming_times = Vec::new();
        let mut old_encode_times = Vec::new();
        let mut streamed_total_bytes = 0;
        for i in 0..20 {
            for kind in if i % 2 == 0 { [0,1] } else { [1,0] } {
                let path = folder.join(if kind == 0 { "legacy.json.tmp" } else { "stream.json.tmp" });
                let start = Instant::now();
                if kind == 0 {
                    let bytes = serde_json::to_vec(&workspace).unwrap();
                    let encoded = start.elapsed().as_secs_f64() * 1000.0;
                    let mut f = File::create(&path).unwrap();
                    f.write_all(&bytes).unwrap();
                    f.sync_all().unwrap();
                    old_encode_times.push(encoded);
                    vec_times.push(start.elapsed().as_secs_f64() * 1000.0);
                } else {
                    let mut writer = BufWriter::with_capacity(128 * 1024, File::create(&path).unwrap());
                    serde_json::to_writer(&mut writer, &workspace).unwrap();
                    writer.flush().unwrap();
                    writer.get_ref().sync_all().unwrap();
                    streaming_times.push(start.elapsed().as_secs_f64() * 1000.0);
                    streamed_total_bytes = fs::metadata(&path).unwrap().len();
                }
            }
        }
        let a = fs::read(folder.join("legacy.json.tmp")).unwrap();
        let b = fs::read(folder.join("stream.json.tmp")).unwrap();
        assert_eq!(a, b, "Streaming serializer must preserve exact compact JSON bytes");
        assert_eq!(expected_len as u64, streamed_total_bytes);
        let stat = |samples: &mut Vec<f64>| { samples.sort_by(f64::total_cmp);
            (samples[samples.len()/2], samples[samples.len()*95/100]) };
        let (legacy_p50,legacy_p95) = stat(&mut vec_times);
        let (stream_p50,stream_p95) = stat(&mut streaming_times);
        let (encode_p50,encode_p95) = stat(&mut old_encode_times);
        println!("json_serializer_2200_notes bytes={expected_len} legacy_serialize_only_p50_p95_ms={encode_p50:.3}/{encode_p95:.3} legacy_serialize_sync_p50_p95_ms={legacy_p50:.3}/{legacy_p95:.3} streaming_serialize_sync_p50_p95_ms={stream_p50:.3}/{stream_p95:.3} legacy_extra_heap_bytes_at_least={expected_len} stream_writer_capacity_bytes=131072 samples=20");
        fs::remove_dir_all(folder).unwrap();
    }
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
    fn mutation_reuses_validated_primary_and_preserves_recovery_backup() {
        let folder = std::env::temp_dir().join(format!("inkpad-fast-backup-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let original = store.load().unwrap();
        store.mutate(|workspace| { workspace.notes[0].body = "first edit".into(); Ok(()) }).unwrap();
        let backup: Workspace = serde_json::from_slice(&fs::read(store.data_path().with_extension("json.bak")).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&backup).unwrap(), serde_json::to_value(&original).unwrap());

        // If the primary is corrupt, a mutation can recover from the backup,
        // but must never replace that valid backup with the corrupt bytes.
        fs::write(store.data_path(), b"damaged primary").unwrap();
        store.mutate(|workspace| { workspace.notes[0].body = "recovered edit".into(); Ok(()) }).unwrap();
        let recovered_backup: Workspace = serde_json::from_slice(&fs::read(store.data_path().with_extension("json.bak")).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&recovered_backup).unwrap(), serde_json::to_value(&original).unwrap());
        assert_eq!(store.load().unwrap().notes[0].body, "recovered edit");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn parsed_read_cache_detects_same_length_external_edit_and_recovery() {
        let folder = std::env::temp_dir().join(format!("inkpad-cache-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        store.mutate(|workspace| { workspace.notes[0].body = "alpha".into(); Ok(()) }).unwrap();
        store.mutate(|_| Ok(())).unwrap(); // Keep a valid "alpha" recovery backup.
        let first = store.read_cached().unwrap();
        let same = store.read_cached().unwrap();
        assert!(Arc::ptr_eq(&first, &same));

        let raw = fs::read(store.data_path()).unwrap();
        let changed = String::from_utf8(raw).unwrap().replace("alpha", "bravo");
        fs::write(store.data_path(), changed.as_bytes()).unwrap();
        let fresh = store.read_cached().unwrap();
        assert!(!Arc::ptr_eq(&first, &fresh));
        assert_eq!(fresh.notes[0].body, "bravo");

        fs::write(store.data_path(), b"truncated").unwrap();
        let recovered = store.read_cached().unwrap();
        assert_eq!(recovered.notes[0].body, "alpha");
        assert!(!Arc::ptr_eq(&fresh, &recovered));
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn sqlite_index_is_lazy_and_exactly_tracks_external_json_even_when_length_unchanged() {
        let folder = std::env::temp_dir().join(format!("inkpad-sqlite-external-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        store.mutate(|w| { w.notes[0].title = "alpha".into(); w.notes[0].body = "alpha needle".into(); Ok(()) }).unwrap();
        let sqlite = store.data_path().with_extension("sqlite3");
        assert!(!sqlite.exists(), "SQLite must not slow ordinary JSON writes");
        let found = store.search_indexed("needle").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].body, "alpha needle");
        assert!(sqlite.exists());
        assert!(store.search_indexed("none").unwrap().is_empty());

        let json = fs::read_to_string(store.data_path()).unwrap();
        let edited = json.replace("alpha needle", "bravo needle");
        assert_eq!(json.len(), edited.len());
        fs::write(store.data_path(), edited).unwrap();
        assert_eq!(store.search_indexed("bravo").unwrap()[0].body, "bravo needle");
        assert!(store.search_indexed("alpha needle").unwrap().is_empty());

        let another_process = Store::in_folder(folder.clone()).unwrap();
        another_process.mutate(|w| { w.notes[0].body = "updated by independent writer".into(); Ok(()) }).unwrap();
        assert_eq!(store.search_indexed("updated").unwrap()[0].body, "updated by independent writer");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn sqlite_corrupt_cache_is_rebuilt_without_changing_json_or_backup() {
        let folder = std::env::temp_dir().join(format!("inkpad-sqlite-corrupt-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        store.mutate(|w| { w.notes[0].body = "before change".into(); Ok(()) }).unwrap();
        store.mutate(|w| { w.notes[0].body = "after change".into(); Ok(()) }).unwrap();
        let before_json = fs::read(store.data_path()).unwrap();
        let before_backup = fs::read(store.data_path().with_extension("json.bak")).unwrap();
        assert_eq!(store.search_indexed("after").unwrap().len(), 1);
        fs::write(store.data_path().with_extension("sqlite3"), b"not a sqlite db").unwrap();
        assert_eq!(store.search_indexed("after").unwrap().len(), 1);
        assert_eq!(fs::read(store.data_path()).unwrap(), before_json);
        assert_eq!(fs::read(store.data_path().with_extension("json.bak")).unwrap(), before_backup);

        // A malformed JSON primary must take the existing backup recovery path,
        // never returning stale SQLite data or indexing corrupt source bytes.
        fs::write(store.data_path(), b"truncated json").unwrap();
        assert_eq!(store.search_indexed("before").unwrap()[0].body, "before change");
        assert!(store.search_indexed("after").unwrap().is_empty());
        assert_eq!(fs::read(store.data_path().with_extension("json.bak")).unwrap(), before_backup);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn sqlite_legacy_media_and_unicode_search_match_full_json() {
        let folder = std::env::temp_dir().join(format!("inkpad-sqlite-legacy-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let legacy = serde_json::json!({"version": 1, "active_id": "legacy", "notes": [{
            "id":"legacy", "title":"Álpha猫", "body":"Cloud ☁️ and 🚀 stars %_ [text]",
            "kind":"note", "font_size":24, "font_choice":"Mono", "pinned":true,
            "images":[{"id":"img", "filename":"img.png", "caption":"hi", "width_percent":62, "position":0}],
            "inline_media":[{"image_id":"img", "char_offset":3}]
        }]});
        fs::write(store.data_path(), serde_json::to_vec(&legacy).unwrap()).unwrap();
        let original = store.load().unwrap();
        for needle in ["álpha", "猫", "cloud", "☁️", "🚀", "%_", "[text]", "", "STARS"] {
            let query = needle.to_lowercase();
            let expected = original.notes.iter().filter(|note|
                note.title.to_lowercase().contains(&query) || note.body.to_lowercase().contains(&query))
                .cloned().collect::<Vec<_>>();
            assert_eq!(store.search_indexed(&query).unwrap(), expected, "query {query:?}");
        }
        assert!(store.load().unwrap().notes[0].inline_media[0].char_offset == 3);
        fs::remove_dir_all(folder).unwrap();
    }

    /// Run explicitly with --ignored to compare steady-state search overhead.
    /// Synthetic data and timing are isolated from real Inkpad workspaces.
    #[test]
    #[ignore]
    fn sqlite_search_benchmark() {
        use std::time::Instant;
        let folder = std::env::temp_dir().join(format!("inkpad-sqlite-bench-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let filler = "A natural sentence containing several words. ".repeat(76);
        store.mutate(|workspace| {
            workspace.notes.clear();
            for index in 0..2200 {
                let mut note = Note::create("note");
                note.id = format!("bench-note-{index}");
                note.title = format!("Saved note {index}");
                note.body = if index == 1300 { format!("{filler} uniqueopalsearch") }
                    else { filler.clone() };
                workspace.notes.push(note);
            }
            Ok(())
        }).unwrap();
        let bytes = fs::metadata(store.data_path()).unwrap().len();
        let build = Instant::now();
        assert_eq!(store.search_indexed("uniqueopalsearch").unwrap().len(), 1);
        let build_ms = build.elapsed().as_secs_f64() * 1000.0;
        store.read_cached_with_search_index().unwrap();
        let mut old_ms = Vec::new();
        let mut sql_ms = Vec::new();
        for _ in 0..24 {
            let start = Instant::now();
            let (workspace, index) = store.read_cached_with_search_index().unwrap();
            let count = workspace.notes.iter().zip(index.iter())
                .filter(|(_, (title, body))| title.contains("uniqueopalsearch") || body.contains("uniqueopalsearch"))
                .count();
            assert_eq!(count, 1);
            old_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            assert_eq!(store.search_indexed("uniqueopalsearch").unwrap().len(), 1);
            sql_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        old_ms.sort_by(f64::total_cmp);
        sql_ms.sort_by(f64::total_cmp);
        println!("SQLite search benchmark: JSON {:.2} MiB, DB {:.2} MiB, first build {:.2}ms, hot JSON {:.3}ms, indexed SQLite {:.3}ms median",
            bytes as f64 / 1048576.0,
            fs::metadata(store.data_path().with_extension("sqlite3")).unwrap().len() as f64 / 1048576.0,
            build_ms, old_ms[12], sql_ms[12]);
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
        assert!(note.drawing.is_empty());
        assert_eq!(note.images.len(), 1);
        assert_eq!(note.font_size, 24);
        let encoded = serde_json::to_value(migrated).unwrap();
        assert!(encoded["notes"][0].get("rich_text").is_none());
        assert!(encoded["notes"][0].get("drawing").is_none());
        assert!(encoded["notes"][0].get("inline_media").is_none());
        assert_eq!(encoded["notes"][0]["images"][0]["caption"], "Hi");
    }

    #[test]
    fn inline_image_layout_preserves_legacy_body_order_and_unicode_anchor_rebasing() {
        let mut note = Note::create("note");
        note.body = "A猫🦊B".into();
        note.images = vec![
            MediaBlock { id:"first".into(), filename:"first.png".into(),
                caption:String::new(), width_percent:65, position:0 },
            MediaBlock { id:"second".into(), filename:"second.png".into(),
                caption:String::new(), width_percent:65, position:1 },
        ];
        assert_eq!(note.content_blocks().unwrap(), vec![
            NoteContentBlock::Text { start:0, end:4 },
            NoteContentBlock::Image { image_id:"first".into() },
            NoteContentBlock::Text { start:4, end:4 },
            NoteContentBlock::Image { image_id:"second".into() },
            NoteContentBlock::Text { start:4, end:4 },
        ]);
        note.set_image_anchor("second", Some(2)).unwrap();
        note.set_image_anchor("first", Some(2)).unwrap();
        assert_eq!(note.content_blocks().unwrap(), vec![
            NoteContentBlock::Text { start:0, end:2 },
            NoteContentBlock::Image { image_id:"first".into() },
            NoteContentBlock::Text { start:2, end:2 },
            NoteContentBlock::Image { image_id:"second".into() },
            NoteContentBlock::Text { start:2, end:4 },
        ]);
        let unchanged = note.body.clone();
        assert_eq!(note.image_anchor("first").unwrap(), Some(2));
        assert!(note.set_image_anchor("not-a-photo", Some(1)).is_err());
        assert!(note.set_image_anchor("first", Some(5)).is_err());
        assert_eq!(note.body, unchanged, "placing images must never add body placeholders");

        note.replace_text_range(0, 0, "💚").unwrap();
        assert_eq!(note.body, "💚A猫🦊B");
        assert_eq!(note.image_anchor("second").unwrap(), Some(3));
        // Delete the range containing the old anchor; right-bias preserves
        // the image after the replacement, not at an invalid stale offset.
        note.replace_text_range(1, 4, "").unwrap();
        assert_eq!(note.body, "💚B");
        assert_eq!(note.image_anchor("first").unwrap(), Some(1));
        let before = note.snapshot_text();
        note.set_body_preserving_format("💚é猫B".into()).unwrap();
        assert_eq!(note.image_anchor("first").unwrap(), Some(3));
        note.restore_text_snapshot(&before).unwrap();
        assert_eq!(note.body, "💚B");
        assert_eq!(note.image_anchor("first").unwrap(), Some(1));
        note.set_image_anchor("first", None).unwrap();
        assert_eq!(note.image_anchor("first").unwrap(), None);
        assert_eq!(note.content_blocks().unwrap().last(), Some(
            &NoteContentBlock::Text { start:2, end:2 }));
        assert!(note.content_blocks().unwrap().iter().any(|block| matches!(block,
            NoteContentBlock::Image { image_id } if image_id == "first")));
    }

    #[test]
    fn empty_note_has_caret_slots_before_between_and_after_multiple_images() {
        let mut note = Note::create("note");
        assert_eq!(note.content_blocks().unwrap(), vec![
            NoteContentBlock::Text { start:0, end:0 }
        ]);
        for (index, id) in ["first", "second"].into_iter().enumerate() {
            note.images.push(MediaBlock { id:id.into(), filename:format!("{id}.png"),
                caption:String::new(), width_percent:65, position:index });
            note.set_image_anchor(id, Some(0)).unwrap();
        }
        assert_eq!(note.content_blocks().unwrap(), vec![
            NoteContentBlock::Text { start:0, end:0 },
            NoteContentBlock::Image { image_id:"first".into() },
            NoteContentBlock::Text { start:0, end:0 },
            NoteContentBlock::Image { image_id:"second".into() },
            NoteContentBlock::Text { start:0, end:0 },
        ]);
        assert!(note.replace_text_in_content_block(1, 0, 0, "not text", None).is_err());
        assert!(note.replace_text_in_content_block(2, 0, 1, "invalid", None).is_err());
        let caret = note.replace_text_in_content_block(2, 0, 0, "猫", None).unwrap();
        assert_eq!(caret, 1);
        assert_eq!(note.body, "猫");
        assert_eq!(note.image_anchor("first").unwrap(), Some(0));
        assert_eq!(note.image_anchor("second").unwrap(), Some(1));
        assert_eq!(note.content_blocks().unwrap(), vec![
            NoteContentBlock::Text { start:0, end:0 },
            NoteContentBlock::Image { image_id:"first".into() },
            NoteContentBlock::Text { start:0, end:1 },
            NoteContentBlock::Image { image_id:"second".into() },
            NoteContentBlock::Text { start:1, end:1 },
        ]);
        note.replace_text_in_content_block(0, 0, 0, "A", None).unwrap();
        assert_eq!(note.body, "A猫");
        assert_eq!(note.image_anchor("first").unwrap(), Some(1));
        assert_eq!(note.image_anchor("second").unwrap(), Some(2));
        note.replace_text_in_content_block(4, 2, 2, "尾", None).unwrap();
        assert_eq!(note.body, "A猫尾");
        assert_eq!(note.image_anchor("first").unwrap(), Some(1));
        assert_eq!(note.image_anchor("second").unwrap(), Some(2));
    }

    #[test]
    fn invalid_inline_anchors_reject_write_and_recover_from_valid_backup() {
        let folder = std::env::temp_dir().join(format!("inkpad-inline-recovery-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let mut workspace = store.load().unwrap();
        let note = &mut workspace.notes[0];
        note.set_body_preserving_format("a🐈b".into()).unwrap();
        note.images.push(MediaBlock { id:"img".into(), filename:"img.png".into(),
            caption:String::new(), width_percent:65, position:0 });
        note.set_image_anchor("img", Some(2)).unwrap();
        store.save(&workspace).unwrap();
        store.save(&workspace).unwrap();
        assert_eq!(store.load().unwrap().notes[0].image_anchor("img").unwrap(), Some(2));
        let baseline = fs::read(store.data_path()).unwrap();
        for corrupt in [
            serde_json::json!([{"image_id":"img","char_offset":99}]),
            serde_json::json!([{"image_id":"orphan","char_offset":1}]),
            serde_json::json!([{"image_id":"img","char_offset":1},
                {"image_id":"img","char_offset":2}]),
        ] {
            let mut bad = serde_json::to_value(&workspace).unwrap();
            bad["notes"][0]["inline_media"] = corrupt;
            fs::write(store.data_path(), serde_json::to_vec(&bad).unwrap()).unwrap();
            let recovered = store.load().unwrap();
            assert_eq!(recovered.notes[0], workspace.notes[0]);
            assert!(fs::read(store.data_path()).unwrap().starts_with(b"{"),
                "read must not overwrite damaged primary");
        }
        fs::write(store.data_path(), &baseline).unwrap();
        workspace.notes[0].inline_media[0].char_offset = 9999;
        assert!(store.save(&workspace).is_err());
        assert_eq!(fs::read(store.data_path()).unwrap(), baseline);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn vector_drawing_persists_without_discarding_legacy_ink_or_media() {
        let folder = std::env::temp_dir().join(format!("inkpad-vector-recovery-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let mut workspace = store.load().unwrap();
        let note = &mut workspace.notes[0];
        note.strokes.push(Stroke { color: [2,3,4,255], width: 9, eraser: false,
            points: vec![[10,20],[30,40]] });
        note.images.push(MediaBlock { id: "img".into(), filename: "img.png".into(),
            caption: "Original".into(), width_percent: 40, position: 0 });
        let id = note.drawing.add_object(DrawShape::RoundedRectangle {
            width: 120.0, height: 80.0, radius: 12.0 }, None).unwrap();
        note.drawing.move_object(&id, 64.0, 40.0).unwrap();
        note.drawing.update_object(&id, |o| {
            o.style.opacity = 0.65;
            o.style.fill = Some(DrawFill::Solid { color: [255,30,10,255] });
        }).unwrap();
        let expected = note.clone();
        store.save(&workspace).unwrap();
        store.save(&workspace).unwrap();
        assert_eq!(store.load().unwrap().notes[0], expected);
        // The existing backup recovery contract includes vector objects.
        fs::write(store.data_path(), b"bad current JSON").unwrap();
        assert_eq!(store.load().unwrap().notes[0], expected);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn malformed_vector_data_does_not_replace_valid_backup() {
        let folder = std::env::temp_dir().join(format!("inkpad-vector-malformed-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let mut workspace = store.load().unwrap();
        workspace.notes[0].drawing.add_object(
            DrawShape::Ellipse { radius_x: 15.0, radius_y: 20.0 }, None).unwrap();
        store.save(&workspace).unwrap();
        store.save(&workspace).unwrap();
        let mut bad = serde_json::to_value(&workspace).unwrap();
        bad["notes"][0]["drawing"]["objects"][0]["layer_id"] = serde_json::json!("missing-layer");
        fs::write(store.data_path(), serde_json::to_vec(&bad).unwrap()).unwrap();
        assert_eq!(store.load().unwrap().notes[0], workspace.notes[0]);
        assert!(fs::read(store.data_path()).unwrap().windows(13).any(|w| w == b"missing-layer"));
        fs::remove_dir_all(folder).unwrap();
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
