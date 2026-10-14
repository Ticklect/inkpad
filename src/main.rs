#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod canvas;
mod editor_session;
mod instance;
mod media;
mod settings;
mod store;
mod visual_preview;
#[cfg(windows)]
mod windows_chrome;

use canvas::{parse_color, Board, HEIGHT, WIDTH};
use editor_session::{EditKind, TextHistory};
use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, StyledText, VecModel};
use std::cell::RefCell;
use std::fs;
use std::io;
use std::rc::Rc;
use std::time::{Duration, SystemTime};
use settings::{AppearanceTheme, UiSettings};
use store::{Note, Store, Stroke, StyledFragment, TextSelection, TextStyle, TextStylePatch, Workspace};
use std::path::Path;

slint::include_modules!();

struct App {
    storage: Store,
    workspace: Workspace,
    baseline: Workspace,
    board: Option<Board>,
    drawing: bool,
    edit_mode: String,
    last_seen_file: Option<(SystemTime, u64)>,
    last_seen_preferences: Option<(SystemTime, u64)>,
    filter: String,
    query: String,
    brush_size: u16,
    brush_color: String,
    eraser: bool,
    ui_settings: UiSettings,
    pending_action: Option<PendingAction>,
    text_history: TextHistory,
    typing_style: Option<TextStyle>,
    selection: TextSelection,
    rich_clipboard: Option<StyledFragment>,
}

#[derive(Debug, Clone)]
enum PendingAction {
    Delete { id: String, title: String },
    ClearDrawing { id: String, title: String },
    DeletePicture { note_id: String, image_id: String },
}

impl PendingAction {
    fn description(&self) -> (String, String, String) {
        match self {
            Self::Delete { title, .. } => (
                "Delete this page?".into(),
                format!("“{title}” and its drawing will be permanently deleted. This cannot be undone."),
                "Delete page".into(),
            ),
            Self::ClearDrawing { title, .. } => (
                "Clear this drawing?".into(),
                format!("All strokes on “{title}” will be erased. Your text will remain."),
                "Clear drawing".into(),
            ),
            Self::DeletePicture { .. } => (
                "Remove this picture?".into(),
                "The image will be removed from this page. Your text and other pictures will remain.".into(),
                "Remove picture".into(),
            ),
        }
    }
}

/// Three-way per-field merge of a note. A headless agent can insert or resize
/// images while the user is typing in the desktop editor: unrelated changes
/// are retained instead of treating the whole note as conflicting. Conflicting
/// edits to the *same* field remain blocked to prevent silent data loss.
fn merge_field<T: Clone + PartialEq>(
    base: &T, local: &T, remote: &T, field: &str,
) -> io::Result<T> {
    if local == base { return Ok(remote.clone()); }
    if remote == base || remote == local { return Ok(local.clone()); }
    Err(io::Error::new(io::ErrorKind::WouldBlock,
        format!("Both the desktop and an agent changed {field}; the desktop change remains unsaved.")))
}

fn merge_note(base: &Note, local: &Note, remote: &Note) -> io::Result<Note> {
    if base.id != local.id || local.id != remote.id {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Conflicting note identity"));
    }
    let body = merge_field(&base.body, &local.body, &remote.body, "note text")?;
    let rich_text = merge_field(&base.rich_text, &local.rich_text, &remote.rich_text, "text formatting")?;
    // The sparse formatting offsets are attached to Unicode characters in the
    // body. If one process changed the body and the other process changed its
    // formatting, rebase runs onto the winning text rather than allowing stale
    // offsets to silently colour or embolden unrelated characters.
    let style_source_body = if local.rich_text != base.rich_text {
        &local.body
    } else if remote.rich_text != base.rich_text {
        &remote.body
    } else {
        &base.body
    };
    let mut merged = Note {
        id: base.id.clone(),
        title: merge_field(&base.title, &local.title, &remote.title, "the title")?,
        body,
        rich_text,
        kind: merge_field(&base.kind, &local.kind, &remote.kind, "page type")?,
        font_size: merge_field(&base.font_size, &local.font_size, &remote.font_size, "font size")?,
        font_choice: merge_field(&base.font_choice, &local.font_choice, &remote.font_choice, "font family")?,
        pinned: merge_field(&base.pinned, &local.pinned, &remote.pinned, "pinned state")?,
        strokes: merge_field(&base.strokes, &local.strokes, &remote.strokes, "drawing strokes")?,
        images: merge_field(&base.images, &local.images, &remote.images, "picture blocks")?,
        updated: local.updated.max(remote.updated),
    };
    if style_source_body != &merged.body {
        let chosen_body = std::mem::replace(&mut merged.body, style_source_body.clone());
        merged.set_body_preserving_format(chosen_body).map_err(|error|
            io::Error::new(io::ErrorKind::WouldBlock, format!("Cannot safely merge formatted note text: {error}")))?;
    }
    merged.validate_rich_text().map_err(|error|
        io::Error::new(io::ErrorKind::InvalidData, format!("Invalid merged text formatting: {error}")))?;
    Ok(merged)
}

/// TextInput's selection fields are UTF-8 byte offsets. The stored rich
/// document uses Unicode scalar positions. Never truncate offsets into a
/// multi-byte character, even when selection is reversed (Shift+Home etc.).
fn selected_range(ui: &MainWindow, note: &Note) -> io::Result<TextSelection> {
    if ui.get_advanced_text_range() {
        let start = ui.get_format_start();
        let end = ui.get_format_end();
        if start < 0 || end < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid character range"));
        }
        let selection = TextSelection { anchor: start as usize, focus: end as usize };
        selection.validate(&note.body).map_err(io::Error::other)?;
        return Ok(selection);
    }
    let anchor = ui.get_selection_anchor_byte();
    let caret = ui.get_selection_caret_byte();
    if anchor < 0 || caret < 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid editor selection"));
    }
    TextSelection::from_utf8_bytes(&note.body, anchor as usize, caret as usize)
        .map_err(io::Error::other)
}

fn sync_format_toolbar(ui: &MainWindow, note: &Note, selection: TextSelection,
                       typed: Option<&TextStyle>) {
    let style = typed.cloned().or_else(|| note.caret_style(selection).ok()).unwrap_or_default();
    ui.set_selection_bold(style.bold);
    ui.set_text_color(style.color.unwrap_or_else(|| "#292927".into()).into());
    ui.set_selected_text_size(style.size.unwrap_or(note.font_size));
    ui.set_selected_text_family(style.font_family.unwrap_or_else(|| note.font_choice.clone()).into());
}

/// A Slint TextInput edit is delivered as a complete plain buffer. Derive the
/// minimal unchanged scalar prefix/suffix, then replace that one region with
/// a specific transient caret style if the toolbar selected one. This also
/// covers Unicode IME, native text paste, selection replace and Backspace.
fn apply_text_input_edit(note: &mut Note, edited: String,
                         typed: Option<TextStyle>) -> io::Result<bool> {
    if note.body == edited { return Ok(false); }
    let old_length = note.body.chars().count();
    let new_length = edited.chars().count();
    let start = note.body.chars().zip(edited.chars())
        .take_while(|(old, new)| old == new).count();
    let suffix = note.body.chars().rev().zip(edited.chars().rev())
        .take((old_length - start).min(new_length - start))
        .take_while(|(old, new)| old == new).count();
    let new_start_byte = edited.char_indices().nth(start).map(|(pos,_)| pos)
        .unwrap_or(edited.len());
    let new_end_byte = edited.char_indices().nth(new_length - suffix)
        .map(|(pos,_)| pos).unwrap_or(edited.len());
    let replacement = &edited[new_start_byte..new_end_byte];
    note.replace_selection(TextSelection {
        anchor: start, focus: old_length - suffix,
    }, replacement, typed).map_err(io::Error::other)?;
    debug_assert_eq!(note.body, edited);
    Ok(true)
}

/// Apply formatting to the current native TextInput selection. A caret-only
/// operation changes the pending typing style without creating an invalid
/// zero-length persisted run; a real selected range is immediately durable.
fn apply_editor_format(app: &mut App, selection: TextSelection,
                       patch: TextStylePatch) -> io::Result<bool> {
    let before = app.active().cloned().ok_or_else(||
        io::Error::new(io::ErrorKind::NotFound, "No active note"))?;
    let mut edited = before.clone();
    let mut typed = app.typing_style.clone();
    let changed = edited.format_selection(selection, patch, &mut typed)
        .map_err(io::Error::other)?;
    if changed {
        app.text_history.record(&before, EditKind::Formatting);
        if let Some(note) = app.active_mut() { *note = edited; }
        if let Some(after) = app.active().cloned() { app.text_history.commit_after(&after); }
    }
    app.typing_style = typed;
    app.selection = selection;
    if changed { app.persist()?; }
    Ok(changed)
}

/// Check the on-disk revision immediately before undo/redo. The periodic GUI
/// synchronization timer may not have fired yet; refusing a stale replay is
/// safer than rolling back a collaborator's newer text or formatting.
fn check_editor_history_fresh(app: &App) -> io::Result<()> {
    let latest = app.storage.read_latest()?;
    let base = app.baseline.notes.iter().find(|n| n.id == app.workspace.active_id);
    let remote = latest.notes.iter().find(|n| n.id == app.workspace.active_id);
    match (base, remote) {
        (Some(base), Some(remote)) if base.body == remote.body && base.rich_text == remote.rich_text => Ok(()),
        _ => Err(io::Error::new(io::ErrorKind::WouldBlock,
            "Undo/redo blocked: another editor or agent changed this note; reload before replaying history")),
    }
}

fn cut_or_paste_rich_text(app: &mut App, selection: TextSelection,
                          fragment: Option<&StyledFragment>) -> io::Result<usize> {
    let before = app.active().cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No selected note"))?;
    let mut edited = before.clone();
    let (start, end) = selection.range();
    let next = if let Some(fragment) = fragment {
        edited.paste_styled_range(start, end, fragment).map_err(io::Error::other)?
    } else {
        edited.replace_selection(selection, "", None).map_err(io::Error::other)?
    };
    if edited != before {
        app.text_history.record(&before, EditKind::Formatting);
        if let Some(note) = app.active_mut() { *note = edited; }
        if let Some(after) = app.active().cloned() { app.text_history.commit_after(&after); }
        app.selection = TextSelection::caret(next);
        app.typing_style = None;
        app.persist()?;
    }
    Ok(next)
}

fn copy_native_rich_selection(app: &mut App, ui: &MainWindow) -> io::Result<bool> {
    let note = match app.active() { Some(note) => note, None => return Ok(false) };
    let selection = selected_range(ui, note)?;
    let (start, end) = selection.range();
    if start == end { return Ok(false); }
    let fragment = note.copy_styled_range(start, end).map_err(io::Error::other)?;
    let mut clipboard = arboard::Clipboard::new().map_err(io::Error::other)?;
    clipboard.set_text(fragment.text.clone()).map_err(io::Error::other)?;
    app.rich_clipboard = Some(fragment);
    Ok(true)
}

fn refresh_text_after_clipboard(ui: &MainWindow, app: &App, next_caret: usize) {
    update_editor(ui, app);
    let Some(note) = app.active() else { return };
    let Ok((offset, _)) = TextSelection::caret(next_caret).to_utf8_bytes(&note.body) else { return };
    if offset > i32::MAX as usize { return; }
    ui.set_selection_anchor_byte(offset as i32);
    ui.set_selection_caret_byte(offset as i32);
}

impl App {
    fn load(storage: Store) -> io::Result<Self> {
        let mut workspace = storage.load()?;
        let ui_settings = match UiSettings::load(storage.data_path()) {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("Inkpad UI preferences could not be read: {error}. Using default layout.");
                UiSettings::default()
            }
        };
        // A damaged or externally edited current ID must never crash the UI.
        if !workspace.notes.iter().any(|n| n.id == workspace.active_id) {
            workspace.active_id = workspace.notes.first().map(|n| n.id.clone()).unwrap_or_default();
        }
        let edit_mode = if workspace.notes.iter().any(|n| n.id == workspace.active_id && n.kind == "board") {
            "draw"
        } else { "text" }.to_string();
        let mut app = Self {
            last_seen_file: file_stamp(storage.data_path()),
            last_seen_preferences: file_stamp(&UiSettings::path_for(storage.data_path())),
            storage, baseline: workspace.clone(), workspace, board: None, drawing: false, edit_mode,
            filter: "all".into(), query: String::new(), brush_size: 6,
            brush_color: "#263238".into(), eraser: false, ui_settings,
            pending_action: None,
            text_history: TextHistory::default(), typing_style: None,
            selection: TextSelection::caret(0),
            rich_clipboard: None,
        };
        app.reload_board();
        // Conservative cleanup only: assets younger than 24 hours or still
        // referenced by the backup are retained. Never block startup on it.
        let _ = app.storage.prune_unused_media();
        Ok(app)
    }

    fn active(&self) -> Option<&Note> {
        self.workspace.notes.iter().find(|n| n.id == self.workspace.active_id)
    }

    fn active_mut(&mut self) -> Option<&mut Note> {
        self.workspace.notes.iter_mut().find(|n| n.id == self.workspace.active_id)
    }

    fn persist(&mut self) -> io::Result<()> {
        let local = self.workspace.clone();
        let baseline = self.baseline.clone();
        let merged = self.storage.mutate(|latest| {
            // Apply *only* the GUI's own changes. Agent-created notes and
            // unrelated agent edits must not be overwritten by stale GUI state.
            for former in &baseline.notes {
                if !local.notes.iter().any(|item| item.id == former.id) {
                    if let Some(pos) = latest.notes.iter().position(|item| item.id == former.id) {
                        if latest.notes[pos] != *former {
                            return Err(io::Error::new(io::ErrorKind::WouldBlock,
                                "This note was changed by an agent; reload before deleting it."));
                        }
                        latest.notes.remove(pos);
                    }
                }
            }
            for note in &local.notes {
                let former = baseline.notes.iter().find(|item| item.id == note.id);
                if former == Some(note) { continue; }
                if let Some(pos) = latest.notes.iter().position(|item| item.id == note.id) {
                    if let Some(former) = former {
                        latest.notes[pos] = merge_note(former, note, &latest.notes[pos])?;
                    } else {
                        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Note ID conflict"));
                    }
                } else if former.is_some() {
                    return Err(io::Error::new(io::ErrorKind::NotFound,
                        format!("An agent deleted '{}'. Unsaved local edits remain in the window.", note.title)));
                } else {
                    latest.notes.push(note.clone());
                }
            }
            // Selection is a shared workspace field. Do not overwrite an
            // agent's newly requested page merely because the GUI saved an
            // unrelated note. Only the GUI's explicit selection change wins.
            if local.active_id != baseline.active_id {
                latest.active_id = local.active_id.clone();
            }
            Ok(latest.clone())
        })?;
        let selected = merged.active_id.clone();
        self.baseline = merged.clone();
        self.workspace = merged;
        self.workspace.active_id = selected;
        self.last_seen_file = file_stamp(self.storage.data_path());
        Ok(())
    }

    fn toggle_sidebar(&mut self) -> io::Result<bool> {
        self.ui_settings = UiSettings::update(self.storage.data_path(), |settings| {
            settings.sidebar_collapsed = !settings.sidebar_collapsed;
        })?;
        self.last_seen_preferences = file_stamp(&UiSettings::path_for(self.storage.data_path()));
        Ok(self.ui_settings.sidebar_collapsed)
    }

    fn set_theme(&mut self, choice: &str) -> io::Result<()> {
        let theme = AppearanceTheme::parse(choice)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid Inkpad appearance theme"))?;
        self.ui_settings = UiSettings::update(self.storage.data_path(), |settings| {
            settings.theme = theme;
        })?;
        self.last_seen_preferences = file_stamp(&UiSettings::path_for(self.storage.data_path()));
        Ok(())
    }

    fn sync_preferences(&mut self) -> io::Result<bool> {
        let stamp = file_stamp(&UiSettings::path_for(self.storage.data_path()));
        if stamp == self.last_seen_preferences { return Ok(false); }
        let fresh = UiSettings::load(self.storage.data_path())?;
        self.last_seen_preferences = stamp;
        if fresh == self.ui_settings { return Ok(false); }
        self.ui_settings = fresh;
        Ok(true)
    }

    /// Called occasionally while the window is open. This is intentionally
    /// a no-op during uncommitted UI edits, avoiding cursor jumps and lost text.
    fn sync_external(&mut self) -> io::Result<bool> {
        if self.workspace.notes != self.baseline.notes { return Ok(false); }
        // Checking one file's metadata is much cheaper than parsing the
        // entire notebook 3 times per second when no agent changed anything.
        let stamp = file_stamp(self.storage.data_path());
        if stamp == self.last_seen_file { return Ok(false); }
        let latest = self.storage.read_latest()?;
        self.last_seen_file = stamp;
        if latest.notes == self.baseline.notes && latest.active_id == self.baseline.active_id {
            return Ok(false);
        }
        // A headless agent may explicitly select a page. Honour that request
        // if the user hasn't independently changed selection since the last
        // reload. Never discard locally edited text to switch pages.
        let selected = if self.workspace.active_id != self.baseline.active_id {
            self.workspace.active_id.clone()
        } else {
            latest.active_id.clone()
        };
        self.baseline = latest.clone();
        self.workspace = latest;
        self.text_history.discard_on_external_sync();
        self.typing_style = None;
        self.selection = TextSelection::caret(0);
        self.workspace.active_id = if self.workspace.notes.iter().any(|n| n.id == selected) {
            selected
        } else {
            self.workspace.notes.first().map(|n| n.id.clone()).unwrap_or_default()
        };
        self.reload_board();
        Ok(true)
    }

    fn reload_board(&mut self) {
        self.drawing = false;
        self.board = self.active().map(|note| Board::new(&note.strokes));
    }

    fn create(&mut self, kind: &str) -> io::Result<()> {
        let note = Note::create(kind);
        self.workspace.active_id = note.id.clone();
        self.workspace.notes.push(note);
        self.edit_mode = if kind == "board" { "draw" } else { "text" }.to_string();
        self.selection = TextSelection::caret(0);
        self.typing_style = None;
        self.text_history.clear_for_other_note(&self.workspace.active_id);
        self.reload_board();
        self.persist()
    }

    fn select(&mut self, id: &str) -> io::Result<()> {
        if self.workspace.notes.iter().any(|note| note.id == id) {
            self.workspace.active_id = id.into();
            self.edit_mode = if self.active().is_some_and(|n| n.kind == "board") { "draw" } else { "text" }.into();
            self.selection = TextSelection::caret(0);
            self.typing_style = None;
            self.text_history.clear_for_other_note(&self.workspace.active_id);
            self.reload_board();
            self.persist()?;
        }
        Ok(())
    }

    fn delete_id(&mut self, id: &str) -> io::Result<()> {
        let original = self.workspace.notes.len();
        self.workspace.notes.retain(|note| note.id != id);
        if self.workspace.notes.len() == original { return Ok(()); }
        if self.workspace.active_id == id {
            self.workspace.active_id = self.workspace.notes.first().map(|note| note.id.clone()).unwrap_or_default();
            self.edit_mode = "text".into();
        }
        self.reload_board();
        self.persist()
    }

    fn request_delete(&mut self, requested_id: &str) -> Option<(String, String, String)> {
        let id = if requested_id.is_empty() { &self.workspace.active_id } else { requested_id };
        let note = self.workspace.notes.iter().find(|n| n.id == id)?;
        self.pending_action = Some(PendingAction::Delete { id: note.id.clone(), title: note.title.clone() });
        self.pending_action.as_ref().map(PendingAction::description)
    }

    fn request_clear(&mut self) -> Option<(String, String, String)> {
        let note = self.active()?;
        self.pending_action = Some(PendingAction::ClearDrawing { id: note.id.clone(), title: note.title.clone() });
        self.pending_action.as_ref().map(PendingAction::description)
    }

    fn request_image_delete(&mut self, image_id: &str) -> Option<(String, String, String)> {
        let note = self.active()?;
        if !note.images.iter().any(|image| image.id == image_id) { return None; }
        self.pending_action = Some(PendingAction::DeletePicture {
            note_id: note.id.clone(), image_id: image_id.to_string(),
        });
        self.pending_action.as_ref().map(PendingAction::description)
    }

    fn cancel_action(&mut self) { self.pending_action = None; }

    fn confirm_action(&mut self) -> io::Result<()> {
        let Some(action) = self.pending_action.take() else { return Ok(()); };
        match action {
            PendingAction::Delete { id, .. } => self.delete_id(&id),
            PendingAction::ClearDrawing { id, .. } => {
                // Do not accidentally clear whatever page the user switched to
                // while the confirmation sheet was visible.
                if self.workspace.active_id != id {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput,
                        "The selected page changed. No drawing was cleared."));
                }
                self.clear()
            }
            PendingAction::DeletePicture { note_id, image_id } => {
                if self.workspace.active_id != note_id {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "The selected page changed."));
                }
                self.persist()?;
                self.storage.delete_image(&note_id, &image_id)?;
                self.reload_after_media_change()
            }
        }
    }

    fn draw(&mut self, x: i32, y: i32, phase: i32) -> io::Result<bool> {
        if self.active().is_none() { return Ok(false); }
        let point = [x.clamp(0, WIDTH as i32 - 1) as u16, y.clamp(0, HEIGHT as i32 - 1) as u16];
        match phase {
            0 => {
                self.drawing = true;
                let stroke = Stroke {
                    color: parse_color(&self.brush_color), width: self.brush_size,
                    eraser: self.eraser, points: vec![point],
                };
                let color = if stroke.eraser { [255, 255, 255, 255] } else { stroke.color };
                if let Some(note) = self.active_mut() { note.strokes.push(stroke); note.touch(); }
                if let Some(board) = self.board.as_mut() { board.dot(point, self.brush_size, color); }
                Ok(true)
            }
            1 | 2 if self.drawing => {
                let segment = if let Some(note) = self.active_mut() {
                    note.strokes.last_mut().and_then(|stroke| {
                        let from = *stroke.points.last()?;
                        if from != point { stroke.points.push(point); }
                        let color = if stroke.eraser { [255, 255, 255, 255] } else { stroke.color };
                        Some((from, point, stroke.width, color))
                    })
                } else { None };
                if let (Some(board), Some((from, to, width, color))) = (self.board.as_mut(), segment) {
                    board.line(from, to, width, color);
                }
                if phase == 2 {
                    self.drawing = false;
                    self.persist()?;
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn undo(&mut self) -> io::Result<()> {
        self.drawing = false;
        if let Some(note) = self.active_mut() {
            note.strokes.pop(); note.touch();
        }
        self.reload_board();
        self.persist()
    }

    fn clear(&mut self) -> io::Result<()> {
        self.drawing = false;
        if let Some(note) = self.active_mut() {
            note.strokes.clear(); note.touch();
        }
        self.reload_board();
        self.persist()
    }

    fn export_active(&self) -> io::Result<()> {
        let Some(note) = self.active() else { return Ok(()); };
        let filename: String = note.title.chars().map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c }
        }).take(64).collect();
        let filename = if filename.trim().is_empty() { "Untitled".to_string() } else { filename };
        if self.edit_mode == "draw" {
            let Some(path) = rfd::FileDialog::new().add_filter("PNG image", &["png"])
                .set_file_name(format!("{}.png", filename)).save_file() else { return Ok(()); };
            let board = self.board.as_ref().expect("active board");
            fs::write(path, board.png_bytes()?)?;
        } else {
            let Some(path) = rfd::FileDialog::new().add_filter("Text document", &["txt"])
                .set_file_name(format!("{}.txt", filename)).save_file() else { return Ok(()); };
            fs::write(path, note.body.as_bytes())?;
        }
        Ok(())
    }

    fn reload_after_media_change(&mut self) -> io::Result<()> {
        let selected = self.workspace.active_id.clone();
        let updated = self.storage.read_latest()?;
        self.baseline = updated.clone();
        self.workspace = updated;
        self.workspace.active_id = selected;
        self.last_seen_file = file_stamp(self.storage.data_path());
        self.reload_board();
        Ok(())
    }

    fn import_image_file(&mut self, path: &Path) -> io::Result<()> {
        self.persist()?;
        let id = self.workspace.active_id.clone();
        self.storage.insert_image_from_file(&id, path)?;
        self.reload_after_media_change()?;
        self.edit_mode = "text".to_string();
        Ok(())
    }

    fn import_image_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.persist()?;
        let id = self.workspace.active_id.clone();
        self.storage.insert_image_bytes(&id, bytes)?;
        self.reload_after_media_change()?;
        self.edit_mode = "text".to_string();
        Ok(())
    }

    fn update_media(&mut self, id: &str, caption: Option<&str>, width: Option<u8>,
        position: Option<usize>) -> io::Result<()> {
        self.persist()?;
        let note_id = self.workspace.active_id.clone();
        self.storage.update_image(&note_id, id, caption, width, position)?;
        self.reload_after_media_change()
    }

    fn export_complete_page(&self) -> io::Result<()> {
        let Some(note) = self.active() else { return Ok(()); };
        let title: String = note.title.chars().map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c }
        }).take(65).collect();
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Web page with embedded pictures", &["html"])
            .set_file_name(format!("{}.html", if title.is_empty() { "Inkpad page" } else { &title }))
            .save_file() else { return Ok(()); };
        self.storage.export_note_html(&note.id, &path)
    }
}

fn png_from_clipboard() -> io::Result<Vec<u8>> {
    use image::ImageEncoder;
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|e| io::Error::other(format!("Clipboard unavailable: {e}")))?;
    let item = clipboard.get_image()
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("No picture in clipboard: {e}")))?;
    let (width, height) = (item.width as u32, item.height as u32);
    if width == 0 || height == 0 || (width as u64) * (height as u64) > 16_000_000 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Clipboard picture exceeds 16 megapixels"));
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&item.bytes, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(png)
}

/// Mirror Windows' "Default app mode" when Inkpad's theme is set to System.
/// Missing or inaccessible registry settings conservatively use light mode.
#[cfg(windows)]
fn system_prefers_dark() -> bool {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER,
        KEY_QUERY_VALUE, REG_DWORD,
    };
    let subkey: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0".encode_utf16().collect();
    let value_name: Vec<u16> = "AppsUseLightTheme\0".encode_utf16().collect();
    let mut key = std::ptr::null_mut();
    // SAFETY: All pointers are valid for the full call, the value buffer is
    // exactly one REG_DWORD, and an opened HKEY is always closed.
    unsafe {
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_QUERY_VALUE, &mut key) != 0 {
            return false;
        }
        let mut kind = 0u32;
        let mut size = 4u32;
        let mut value = 1u32;
        let result = RegQueryValueExW(key, value_name.as_ptr(), std::ptr::null(), &mut kind,
            (&mut value as *mut u32).cast::<u8>(), &mut size);
        let _ = RegCloseKey(key);
        result == 0 && kind == REG_DWORD && size == 4 && value == 0
    }
}

#[cfg(not(windows))]
fn system_prefers_dark() -> bool { false }

fn file_stamp(path: &std::path::Path) -> Option<(SystemTime, u64)> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

fn board_image(board: &Board) -> Image {
    let mut frame = SharedPixelBuffer::<Rgba8Pixel>::new(WIDTH, HEIGHT);
    frame.make_mut_bytes().copy_from_slice(board.bytes());
    Image::from_rgba8(frame)
}

fn update_rows(ui: &MainWindow, app: &App) {
    let query = app.query.trim().to_lowercase();
    let mut notes: Vec<&Note> = app.workspace.notes.iter().filter(|note| {
        (app.filter == "all" || app.filter == note.kind) &&
            (query.is_empty() || note.title.to_lowercase().contains(&query) || note.body.to_lowercase().contains(&query))
    }).collect();
    notes.sort_by(|a, b| b.pinned.cmp(&a.pinned).then_with(|| b.updated.cmp(&a.updated)).then_with(|| a.title.cmp(&b.title)));
    let rows = notes.into_iter().map(|note| NoteRow {
        id: note.id.clone().into(), title: note.title.clone().into(),
        subtitle: if !note.images.is_empty() {
            format!("{} picture{} · {}", note.images.len(), if note.images.len() == 1 { "" } else { "s" },
                note.body.lines().find(|line| !line.trim().is_empty()).unwrap_or("Photo page"))
                .chars().take(70).collect::<String>()
        } else if note.kind == "board" {
            format!("Sketch · {} stroke{}", note.strokes.len(), if note.strokes.len() == 1 { "" } else { "s" })
        } else {
            let preview = note.body.lines().find(|line| !line.trim().is_empty()).unwrap_or("Empty note");
            preview.chars().take(58).collect()
        }.into(),
        kind: note.kind.clone().into(), active: note.id == app.workspace.active_id,
        pinned: note.pinned,
    }).collect::<Vec<_>>();
    ui.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_note_count(app.workspace.notes.len() as i32);
}

/// Generate a safe StyledText preview from validated attributed character
/// runs. The editable Slint TextInput currently accepts plain text only; this
/// renders actual per-range bold and foreground colours without ever treating
/// note text as author-supplied Markdown/HTML. Per-run font size and family are
/// preserved in the model, and in HTML export, but remain an editor limitation.
fn note_styled_preview(note: &Note) -> StyledText {
    if note.rich_text.is_empty() {
        return StyledText::from_plain_text(&note.body);
    }
    if note.validate_rich_text().is_err() {
        return StyledText::from_plain_text(&note.body);
    }
    let mut markdown = String::with_capacity(note.body.len().saturating_mul(2));
    let mut current = TextStyle::default();
    let mut content = String::new();
    let mut run_index = 0;
    for (index, ch) in note.body.chars().enumerate() {
        while run_index < note.rich_text.len() && note.rich_text[run_index].end <= index {
            run_index += 1;
        }
        let style = note.rich_text.get(run_index)
            .filter(|run| run.start <= index)
            .map(|run| run.style.clone()).unwrap_or_default();
        if style != current {
            append_styled_segment(&mut markdown, &content, &current);
            content.clear();
            current = style;
        }
        content.push(ch);
    }
    append_styled_segment(&mut markdown, &content, &current);
    StyledText::from_markdown(&markdown)
        .unwrap_or_else(|_| StyledText::from_plain_text(&note.body))
}

fn update_visual_preview(ui: &MainWindow, note: &Note) {
    ui.set_note_styled_preview(note_styled_preview(note));
    if let Some(paragraphs) = visual_preview::build(note) {
        let rows = paragraphs.into_iter().map(|paragraph| {
            let spans = paragraph.spans.into_iter().map(|span| {
                let (has_foreground, foreground) = span.foreground
                    .map(|rgb| (true, slint::Color::from_rgb_u8(rgb[0], rgb[1], rgb[2])))
                    .unwrap_or((false, slint::Color::default()));
                RichPreviewSpan {
                    text: span.text.into(),
                    size: span.font_size,
                    family: span.family.into(),
                    bold: span.bold,
                    foreground,
                    has_foreground,
                }
            }).collect::<Vec<_>>();
            RichPreviewParagraph {
                spans: ModelRc::from(Rc::new(VecModel::from(spans))),
            }
        }).collect::<Vec<_>>();
        ui.set_rich_preview_paragraphs(ModelRc::from(Rc::new(VecModel::from(rows))));
        ui.set_rich_preview_exact(true);
    } else {
        ui.set_rich_preview_paragraphs(ModelRc::from(Rc::new(VecModel::from(Vec::<RichPreviewParagraph>::new()))));
        ui.set_rich_preview_exact(false);
    }
}

fn append_styled_segment(out: &mut String, text: &str, style: &TextStyle) {
    if text.is_empty() { return; }
    if let Some(color) = &style.color {
        // Model validation requires exactly a #RRGGBB literal.
        out.push_str("<font color='");
        out.push_str(color);
        out.push_str("'>");
    }
    if style.bold { out.push_str("**"); }
    for ch in text.chars() {
        match ch {
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '<' | '>'
                | '#' | '+' | '-' | '.' | '!' | '|' | '~' | '&' => {
                    out.push('\\'); out.push(ch);
                }
            _ => out.push(ch),
        }
    }
    if style.bold { out.push_str("**"); }
    if style.color.is_some() { out.push_str("</font>"); }
}

fn update_editor(ui: &MainWindow, app: &App) {
    if let Some(note) = app.active() {
        ui.set_has_note(true);
        ui.set_note_title(note.title.clone().into());
        ui.set_note_body(note.body.clone().into());
        update_visual_preview(ui, note);
        ui.set_selection_bold(false);
        ui.set_text_color("#292927".into());
        ui.set_font_size(note.font_size);
        ui.set_font_choice(note.font_choice.clone().into());
        ui.set_active_kind(note.kind.clone().into());
    } else {
        ui.set_has_note(false);
        ui.set_note_title(SharedString::default());
        ui.set_note_body(SharedString::default());
        ui.set_note_styled_preview(StyledText::from_plain_text(""));
        ui.set_selection_bold(false);
        ui.set_text_color("#292927".into());
        ui.set_active_kind("none".into());
    }
    ui.set_edit_mode(app.edit_mode.clone().into());
    if let Some(board) = app.board.as_ref() { ui.set_canvas_image(board_image(board)); }
    update_media_rows(ui, app);
}

fn update_media_rows(ui: &MainWindow, app: &App) {
    let mut rows = Vec::new();
    if let Some(note) = app.active() {
        let mut images = note.images.clone();
        images.sort_by_key(|image| image.position);
        for media in images {
            let preview = match app.storage.media_preview_path(&media) {
                Ok(path) => Image::load_from_path(&path).unwrap_or_default(),
                Err(error) => {
                    eprintln!("Inkpad image preview unavailable: {error}");
                    Image::default()
                }
            };
            rows.push(MediaRow {
                id: media.id.into(),
                caption: media.caption.into(),
                width_percent: media.width_percent as i32,
                position: media.position as i32,
                preview,
            });
        }
    }
    let selected = ui.get_selected_media_id().to_string();
    if !rows.iter().any(|row| row.id == selected) {
        ui.set_selected_media_id("".into());
    }
    ui.set_media_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn update_all(ui: &MainWindow, app: &App) {
    update_rows(ui, app);
    update_editor(ui, app);
    ui.set_save_status("All changes saved on this device".into());
}

fn set_status(ui: &MainWindow, result: io::Result<()>) {
    match result {
        Ok(()) => ui.set_save_status("All changes saved on this device".into()),
        Err(error) => {
            ui.set_save_status(format!("Save failed: {}", error).into());
            eprintln!("Inkpad storage problem: {error}");
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let storage = Store::from_system()?;
    let _instance_guard = match instance::InstanceGuard::acquire(storage.data_path()) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            rfd::MessageDialog::new().set_title("Inkpad is already open")
                .set_description("This notebook is already running. Switch to the existing window to continue editing.")
                .set_level(rfd::MessageLevel::Info).show();
            return Ok(());
        }
        Err(error) => {
            rfd::MessageDialog::new().set_title("Could not open Inkpad safely")
                .set_description(error).set_level(rfd::MessageLevel::Error).show();
            return Ok(());
        }
    };
    let initial = match App::load(storage) {
        Ok(app) => app,
        Err(error) => {
            rfd::MessageDialog::new().set_title("Inkpad can't open your notes")
                .set_description(error.to_string()).set_level(rfd::MessageLevel::Error).show();
            return Err(Box::new(error));
        }
    };
    let app = Rc::new(RefCell::new(initial));
    let ui = MainWindow::new()?;
    #[cfg(windows)]
    windows_chrome::install(ui.window(), {
        let weak = ui.as_weak();
        move || weak.upgrade().is_some_and(|window| !window.get_delete_confirm_open())
    }, {
        let app = app.clone();
        let weak = ui.as_weak();
        move |path| {
            let app = app.clone();
            let weak = weak.clone();
            // Schedule outside winit's native event callback to avoid
            // re-entering Slint while its window adapter is borrowed.
            slint::Timer::single_shot(Duration::from_millis(0), move || {
                let Some(ui) = weak.upgrade() else { return; };
                if app.borrow().pending_action.is_some() { return; }
                let result = app.borrow_mut().import_image_file(&path);
                if result.is_ok() { update_all(&ui, &app.borrow()); }
                set_status(&ui, result);
            });
        }
    }, {
        let app = app.clone();
        let weak = ui.as_weak();
        move || {
            let Ok(image_png) = png_from_clipboard() else { return false; };
            let app = app.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(Duration::from_millis(0), move || {
                let Some(ui) = weak.upgrade() else { return; };
                if app.borrow().pending_action.is_some() { return; }
                let result = app.borrow_mut().import_image_bytes(&image_png);
                if result.is_ok() { update_all(&ui, &app.borrow()); }
                set_status(&ui, result);
            });
            true
        }
    });
    // Frameless windows still receive native WM_CLOSE (Alt+F4, taskbar
    // close). A repeating refresh timer can keep Slint's event loop alive
    // after a window is hidden, so quit explicitly for BOTH close paths.
    ui.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });
    {
        let state = app.borrow();
        update_all(&ui, &state);
        ui.set_sidebar_collapsed(state.ui_settings.sidebar_collapsed);
        ui.set_appearance_theme(state.ui_settings.theme.as_str().into());
        ui.set_system_dark(system_prefers_dark());
        ui.set_brush_size(state.brush_size as i32);
        ui.set_brush_color(state.brush_color.clone().into());
        ui.set_eraser(state.eraser);
        ui.set_filter_kind(state.filter.clone().into());
    }

    // Coalesce bursts of typing and formatting changes to avoid blocking text
    // entry with a synchronous disk fsync after every keystroke.
    let text_save_timer = Rc::new(slint::Timer::default());
    let refresh_timer = slint::Timer::default();
    {
        let app = app.clone();
        let weak = ui.as_weak();
        refresh_timer.start(slint::TimerMode::Repeated, Duration::from_millis(350), move || {
            let Some(ui) = weak.upgrade() else { return; };
            let result = app.borrow_mut().sync_external();
            match result {
                Ok(true) => update_all(&ui, &app.borrow()),
                Ok(false) => {}
                Err(error) => set_status(&ui, Err(error)),
            }
            match app.borrow_mut().sync_preferences() {
                Ok(true) => {
                    let state = app.borrow();
                    ui.set_appearance_theme(state.ui_settings.theme.as_str().into());
                    ui.set_sidebar_collapsed(state.ui_settings.sidebar_collapsed);
                }
                Ok(false) => {}
                Err(error) => set_status(&ui, Err(error)),
            }
            // A Windows appearance change while Inkpad is open should be
            // reflected without reloading the note or moving its caret.
            if ui.get_appearance_theme().as_str() == "system" {
                ui.set_system_dark(system_prefers_dark());
            }
        });
    }

    macro_rules! bind_change {
        ($event:ident, $action:expr) => {{
            let app = app.clone();
            let weak = ui.as_weak();
            let timer = text_save_timer.clone();
            ui.$event(move |value| {
                let Some(ui) = weak.upgrade() else { return; };
                if app.borrow().pending_action.is_some() { return; }
                let result: io::Result<()> = ($action)(&mut app.borrow_mut(), value);
                let state = app.borrow();
                update_rows(&ui, &state);
                // Font buttons emit callbacks rather than two-way bindings;
                // reflect the accepted per-note value immediately in the UI.
                if let Some(note) = state.active() {
                    let style = state.typing_style.as_ref().cloned()
                        .or_else(|| note.caret_style(state.selection).ok()).unwrap_or_default();
                    ui.set_font_size(style.size.unwrap_or(note.font_size));
                    ui.set_font_choice(style.font_family.unwrap_or_else(|| note.font_choice.clone()).into());
                }
                drop(state);
                if let Err(error) = result {
                    set_status(&ui, Err(error));
                    return;
                }
                ui.set_save_status("Saving changes…".into());
                let later_app = app.clone();
                let later_window = weak.clone();
                timer.start(slint::TimerMode::SingleShot, Duration::from_millis(380), move || {
                    if let Some(ui) = later_window.upgrade() {
                        let mut state = later_app.borrow_mut();
                        let selected_before = state.workspace.active_id.clone();
                        let result = state.persist();
                        let changed_page = result.is_ok() && selected_before != state.workspace.active_id;
                        update_rows(&ui, &state);
                        // Do not reset TextInput after an ordinary keystroke:
                        // it would jump the caret. Refresh the editor only if
                        // an agent selected a different page while this edit
                        // was waiting for autosave.
                        if changed_page { update_editor(&ui, &state); }
                        drop(state);
                        set_status(&ui, result);
                    }
                });
            });
        }};
    }

    bind_change!(on_rename_note, |app: &mut App, text: SharedString| {
        if let Some(note) = app.active_mut() {
            note.title = text.to_string(); note.touch();
        }
        Ok(())
    });
    bind_change!(on_edit_body, |app: &mut App, text: SharedString| {
        let edited = text.to_string();
        if let Some(before) = app.active().cloned() {
            if before.body != edited {
                let typed = app.typing_style.clone();
                app.text_history.record(&before, EditKind::Typing);
                if let Some(note) = app.active_mut() {
                    apply_text_input_edit(note, edited, typed)?;
                }
                if let Some(after) = app.active().cloned() { app.text_history.commit_after(&after); }
            }
        }
        Ok(())
    });
    bind_change!(on_change_font_size, |app: &mut App, size: i32| {
        let size = size.clamp(12, 40);
        if let Some(before) = app.active().cloned() {
            let selection = app.selection;
            if !selection.is_caret() { app.text_history.record(&before, EditKind::Formatting); }
            let typing = &mut app.typing_style;
            if let Some(note) = app.workspace.notes.iter_mut().find(|n| n.id == app.workspace.active_id) {
                note.format_selection(selection, TextStylePatch {
                    size: Some(size), ..TextStylePatch::default()
                }, typing).map_err(io::Error::other)?;
            }
            if !selection.is_caret() {
                if let Some(after) = app.active().cloned() { app.text_history.commit_after(&after); }
            }
        }
        Ok(())
    });
    bind_change!(on_change_font_choice, |app: &mut App, choice: SharedString| {
        let choice = choice.to_string();
        if ["Sans", "Serif", "Mono"].contains(&choice.as_str()) {
            if let Some(before) = app.active().cloned() {
                let selection = app.selection;
                if !selection.is_caret() { app.text_history.record(&before, EditKind::Formatting); }
                let typing = &mut app.typing_style;
                if let Some(note) = app.workspace.notes.iter_mut().find(|n| n.id == app.workspace.active_id) {
                    note.format_selection(selection, TextStylePatch {
                        font_family: Some(choice), ..TextStylePatch::default()
                    }, typing).map_err(io::Error::other)?;
                }
                if !selection.is_caret() {
                    if let Some(after) = app.active().cloned() { app.text_history.commit_after(&after); }
                }
            }
        }
        Ok(())
    });

    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_new_note(move |kind| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let result = app.borrow_mut().create(kind.as_str());
            update_all(&ui, &app.borrow()); set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_select_note(move |id| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let result = app.borrow_mut().select(id.as_str());
            update_all(&ui, &app.borrow()); set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_request_delete_note(move |id| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            if let Some((title, body, action)) = app.borrow_mut().request_delete(id.as_str()) {
                ui.set_delete_confirm_title(title.into());
                ui.set_delete_confirm_body(body.into());
                ui.set_confirm_action_label(action.into());
                ui.set_delete_confirm_open(true);
            }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_request_clear_board(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            if let Some((title, body, action)) = app.borrow_mut().request_clear() {
                ui.set_delete_confirm_title(title.into());
                ui.set_delete_confirm_body(body.into());
                ui.set_confirm_action_label(action.into());
                ui.set_delete_confirm_open(true);
            }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_cancel_delete(move || {
            app.borrow_mut().cancel_action();
            if let Some(ui) = weak.upgrade() { ui.set_delete_confirm_open(false); }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_confirm_delete(move || {
            let Some(ui) = weak.upgrade() else { return; };
            let result = app.borrow_mut().confirm_action();
            ui.set_delete_confirm_open(false);
            if result.is_ok() { update_all(&ui, &app.borrow()); }
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_toggle_sidebar(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            match app.borrow_mut().toggle_sidebar() {
                Ok(collapsed) => ui.set_sidebar_collapsed(collapsed),
                Err(error) => set_status(&ui, Err(error)),
            }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_change_appearance_theme(move |theme| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let result = app.borrow_mut().set_theme(theme.as_str());
            if result.is_ok() {
                ui.set_appearance_theme(app.borrow().ui_settings.theme.as_str().into());
            }
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_select_body_range(move |anchor, caret| {
            let Some(ui) = weak.upgrade() else { return; };
            if anchor < 0 || caret < 0 { return; }
            let mut state = app.borrow_mut();
            if state.pending_action.is_some() { return; }
            let Some(note) = state.active() else { return; };
            let Ok(selection) = TextSelection::from_utf8_bytes(&note.body, anchor as usize, caret as usize) else {
                return; // Changes may be transient while the OS IME is composing text.
            };
            state.selection = selection;
            // The selection caption displays Unicode *characters*, not UTF-8
            // bytes. Keep the advanced manual range untouched while its input
            // fields are open so selecting text cannot silently overwrite a
            // deliberate fallback offset.
            if !ui.get_advanced_text_range() {
                let (start, end) = selection.range();
                if let (Ok(start), Ok(end)) = (i32::try_from(start), i32::try_from(end)) {
                    ui.set_format_start(start);
                    ui.set_format_end(end);
                }
            }
            let note = state.active().expect("note selected above");
            sync_format_toolbar(&ui, note, selection, state.typing_style.as_ref());
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_apply_text_format(move |action| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let outcome = (|| -> io::Result<bool> {
                let mut state = app.borrow_mut();
                if action.as_str() != "bold" {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "Unsupported text formatting operation"));
                }
                let selection = selected_range(&ui, state.active()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No active note"))?)?;
                let (start, end) = selection.range();
                let is_all_bold = if start == end {
                    state.typing_style.as_ref().cloned()
                        .or_else(|| state.active().and_then(|n| n.caret_style(selection).ok()))
                        .unwrap_or_default().bold
                } else {
                    (start..end).all(|index| state.active().is_some_and(|n| n.style_at(index).bold))
                };
                apply_editor_format(&mut state, selection, TextStylePatch {
                    bold: Some(!is_all_bold), ..TextStylePatch::default()
                })
            })();
            if outcome.is_ok() {
                let state = app.borrow();
                if let Some(note) = state.active() {
                    update_visual_preview(&ui, note);
                    sync_format_toolbar(&ui, note, state.selection, state.typing_style.as_ref());
                }
                drop(state);
                ui.invoke_restore_text_selection();
            }
            set_status(&ui, outcome.map(|_| ()));
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_apply_text_color(move |color| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let outcome = (|| -> io::Result<bool> {
                let mut state = app.borrow_mut();
                let selection = selected_range(&ui, state.active()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No active note"))?)?;
                // Ink means the current theme's readable foreground, not a
                // hard-coded charcoal override that vanishes in dark mode.
                let patch = if color.as_str() == "#292927" {
                    TextStylePatch { reset_color: true, ..TextStylePatch::default() }
                } else {
                    TextStylePatch { color: Some(color.to_string()), ..TextStylePatch::default() }
                };
                apply_editor_format(&mut state, selection, patch)
            })();
            if outcome.is_ok() {
                let state = app.borrow();
                if let Some(note) = state.active() {
                    update_visual_preview(&ui, note);
                    sync_format_toolbar(&ui, note, state.selection, state.typing_style.as_ref());
                }
                drop(state);
                ui.invoke_restore_text_selection();
            }
            set_status(&ui, outcome.map(|_| ()));
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_undo_rich_edit(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let result = (|| -> io::Result<()> {
                let mut state = app.borrow_mut();
                check_editor_history_fresh(&state)?;
                let App { workspace, text_history, .. } = &mut *state;
                let note = workspace.notes.iter_mut().find(|note| note.id == workspace.active_id)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No active note"))?;
                let changed = text_history.undo_before(note).map_err(io::Error::other)?;
                if changed { state.persist()?; }
                Ok(())
            })();
            if result.is_ok() { update_editor(&ui, &app.borrow()); }
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_redo_rich_edit(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let result = (|| -> io::Result<()> {
                let mut state = app.borrow_mut();
                check_editor_history_fresh(&state)?;
                let App { workspace, text_history, .. } = &mut *state;
                let note = workspace.notes.iter_mut().find(|note| note.id == workspace.active_id)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No active note"))?;
                let changed = text_history.redo(note).map_err(io::Error::other)?;
                if changed { state.persist()?; }
                Ok(())
            })();
            if result.is_ok() { update_editor(&ui, &app.borrow()); }
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_copy_rich_selection(move || {
            let Some(ui) = weak.upgrade() else { return false; };
            if app.borrow().pending_action.is_some() { return false; }
            copy_native_rich_selection(&mut app.borrow_mut(), &ui).unwrap_or(false)
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_cut_rich_selection(move || {
            let Some(ui) = weak.upgrade() else { return false; };
            if app.borrow().pending_action.is_some() { return false; }
            let (selection, note_id) = {
                let state = app.borrow();
                let Some(note) = state.active() else { return false; };
                let Ok(selection) = selected_range(&ui, note) else { return false };
                if selection.is_caret() { return false; }
                (selection, note.id.clone())
            };
            if !copy_native_rich_selection(&mut app.borrow_mut(), &ui).unwrap_or(false) { return false; }
            let later_app = app.clone();
            let later_window = weak.clone();
            slint::Timer::single_shot(Duration::from_millis(0), move || {
                let Some(ui) = later_window.upgrade() else { return; };
                let mut state = later_app.borrow_mut();
                if state.workspace.active_id != note_id { return; }
                let result = cut_or_paste_rich_text(&mut state, selection, None);
                if let Ok(caret) = result.as_ref() { refresh_text_after_clipboard(&ui, &state, *caret); }
                drop(state);
                if result.is_ok() { ui.invoke_restore_text_selection(); }
                set_status(&ui, result.map(|_| ()));
            });
            true
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_paste_rich_selection(move || {
            let Some(ui) = weak.upgrade() else { return false; };
            if app.borrow().pending_action.is_some() { return false; }
            let (fragment, selection, note_id) = {
                let state = app.borrow();
                let Some(fragment) = state.rich_clipboard.as_ref() else { return false; };
                let clipboard_text = arboard::Clipboard::new().and_then(|mut clip| clip.get_text());
                if clipboard_text.ok().as_deref() != Some(fragment.text.as_str()) { return false; }
                let Some(note) = state.active() else { return false; };
                let Ok(selection) = selected_range(&ui, note) else { return false };
                (fragment.clone(), selection, note.id.clone())
            };
            let later_app = app.clone();
            let later_window = weak.clone();
            slint::Timer::single_shot(Duration::from_millis(0), move || {
                let Some(ui) = later_window.upgrade() else { return; };
                let mut state = later_app.borrow_mut();
                if state.workspace.active_id != note_id { return; }
                let result = cut_or_paste_rich_text(&mut state, selection, Some(&fragment));
                if let Ok(caret) = result.as_ref() { refresh_text_after_clipboard(&ui, &state, *caret); }
                drop(state);
                if result.is_ok() { ui.invoke_restore_text_selection(); }
                set_status(&ui, result.map(|_| ()));
            });
            true
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_request_formatted_preview(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            if let Some(note) = app.borrow().active() {
                update_visual_preview(&ui, note);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_window_minimize(move || {
            if let Some(ui) = weak.upgrade() {
                if !ui.get_delete_confirm_open() { ui.window().set_minimized(true); }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_window_toggle_maximize(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_delete_confirm_open() { return; }
                let window = ui.window();
                window.set_maximized(!window.is_maximized());
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_window_close(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_delete_confirm_open() { return; }
                if let Err(error) = ui.window().hide() {
                    eprintln!("Could not close Inkpad window: {error}");
                }
                if let Err(error) = slint::quit_event_loop() {
                    eprintln!("Could not quit Inkpad event loop: {error}");
                }
            }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_set_edit_mode(move |mode| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let mode = mode.as_str();
            if mode == "text" || mode == "draw" {
                app.borrow_mut().edit_mode = mode.to_string();
                ui.set_edit_mode(mode.into());
                if mode == "draw" {
                    if let Some(board) = app.borrow().board.as_ref() {
                        ui.set_canvas_image(board_image(board));
                    }
                }
            }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_insert_image_file(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let chosen = rfd::FileDialog::new()
                .add_filter("Pictures", &["png", "jpg", "jpeg", "webp", "gif"])
                .pick_file();
            if let Some(path) = chosen {
                let result = app.borrow_mut().import_image_file(&path);
                if result.is_ok() { update_all(&ui, &app.borrow()); }
                set_status(&ui, result);
            }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_paste_image(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let result = png_from_clipboard().and_then(|bytes| app.borrow_mut().import_image_bytes(&bytes));
            if result.is_ok() { update_all(&ui, &app.borrow()); }
            set_status(&ui, result);
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_select_media(move |image_id| {
            if let Some(ui) = weak.upgrade() {
                if !ui.get_delete_confirm_open() {
                    ui.set_selected_media_id(image_id);
                }
            }
        });
    }
    // Rapid resize slider/preset clicks coalesce into one disk write after
    // a short pause, avoiding fsync for every physical pointer movement.
    let image_resize_timer = Rc::new(slint::Timer::default());
    {
        let app = app.clone(); let weak = ui.as_weak();
        let timer = image_resize_timer.clone();
        ui.on_resize_media(move |image_id, width| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let id = image_id.to_string();
            let page_id = app.borrow().workspace.active_id.clone();
            let width = width.clamp(10, 100) as u8;
            ui.set_save_status("Updating picture size…".into());
            let app = app.clone(); let weak = weak.clone();
            timer.start(slint::TimerMode::SingleShot, Duration::from_millis(180), move || {
                let Some(ui) = weak.upgrade() else { return; };
                let current_page = app.borrow().workspace.active_id == page_id;
                let result = if current_page {
                    app.borrow_mut().update_media(&id, None, Some(width), None)
                } else {
                    app.borrow().storage.update_image(&page_id, &id, None, Some(width), None).map(|_| ())
                };
                if current_page && result.is_ok() { update_media_rows(&ui, &app.borrow()); }
                set_status(&ui, result);
            });
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_move_media(move |image_id, step| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let index = app.borrow().active()
                .and_then(|note| note.images.iter().find(|image| image.id == image_id.as_str()))
                .map(|image| image.position);
            if let Some(index) = index {
                let new_index = (index as i32 + step).max(0) as usize;
                let result = app.borrow_mut().update_media(image_id.as_str(), None, None, Some(new_index));
                if result.is_ok() { update_media_rows(&ui, &app.borrow()); }
                set_status(&ui, result);
            }
        });
    }
    let image_caption_timer = Rc::new(slint::Timer::default());
    {
        let app = app.clone(); let weak = ui.as_weak();
        let timer = image_caption_timer.clone();
        ui.on_caption_media(move |image_id, caption| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let id = image_id.to_string();
            let caption = caption.to_string();
            let note_id = app.borrow().workspace.active_id.clone();
            ui.set_save_status("Saving picture caption…".into());
            let app = app.clone(); let weak = weak.clone();
            timer.start(slint::TimerMode::SingleShot, Duration::from_millis(420), move || {
                let Some(ui) = weak.upgrade() else { return; };
                let result = if app.borrow().workspace.active_id == note_id {
                    app.borrow_mut().update_media(&id, Some(&caption), None, None)
                } else {
                    app.borrow().storage.update_image(&note_id, &id, Some(&caption), None, None).map(|_| ())
                };
                // Don't recreate the gallery on each keystroke. The caption
                // text input already reflects the user's current edits.
                set_status(&ui, result);
            });
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_delete_media(move |image_id| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            if let Some((title, body, action)) = app.borrow_mut().request_image_delete(image_id.as_str()) {
                ui.set_delete_confirm_title(title.into());
                ui.set_delete_confirm_body(body.into());
                ui.set_confirm_action_label(action.into());
                ui.set_delete_confirm_open(true);
            }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_export_page(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            set_status(&ui, app.borrow().export_complete_page());
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_toggle_pin(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let (result, changed_page) = {
                let mut state = app.borrow_mut();
                let before = state.workspace.active_id.clone();
                if let Some(note) = state.active_mut() { note.pinned = !note.pinned; note.touch(); }
                let result = state.persist();
                let changed = result.is_ok() && before != state.workspace.active_id;
                (result, changed)
            };
            if changed_page { update_all(&ui, &app.borrow()); }
            else { update_rows(&ui, &app.borrow()); }
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_draw_at(move |x, y, phase| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let previous_id = app.borrow().workspace.active_id.clone();
            let result = app.borrow_mut().draw(x, y, phase);
            match result {
                Ok(true) => {
                    let state = app.borrow();
                    if phase == 2 && previous_id != state.workspace.active_id {
                        update_all(&ui, &state);
                    } else {
                        if let Some(board) = state.board.as_ref() { ui.set_canvas_image(board_image(board)); }
                        if phase == 2 {
                            update_rows(&ui, &state);
                            ui.set_save_status("All changes saved on this device".into());
                        }
                    }
                }
                Ok(false) => {}
                Err(error) => set_status(&ui, Err(error)),
            }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_set_brush_size(move |size| {
            if app.borrow().pending_action.is_some() { return; }
            app.borrow_mut().brush_size = size.clamp(1, 40) as u16;
            if let Some(ui) = weak.upgrade() { ui.set_brush_size(size.clamp(1, 40)); }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_set_brush_color(move |color| {
            if app.borrow().pending_action.is_some() { return; }
            app.borrow_mut().brush_color = color.to_string();
            app.borrow_mut().eraser = false;
            if let Some(ui) = weak.upgrade() { ui.set_brush_color(color); ui.set_eraser(false); }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_set_eraser(move |eraser| {
            if app.borrow().pending_action.is_some() { return; }
            app.borrow_mut().eraser = eraser;
            if let Some(ui) = weak.upgrade() { ui.set_eraser(eraser); }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_undo_board(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let result = app.borrow_mut().undo();
            update_all(&ui, &app.borrow()); set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_export_current(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let result = app.borrow().export_active();
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_save_now(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let mut state = app.borrow_mut();
            let selected_before = state.workspace.active_id.clone();
            let result = state.persist();
            if result.is_ok() && selected_before != state.workspace.active_id {
                update_all(&ui, &state);
            }
            drop(state);
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_set_filter(move |value| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            app.borrow_mut().filter = value.to_string();
            ui.set_filter_kind(value);
            update_rows(&ui, &app.borrow());
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_search(move |query| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            app.borrow_mut().query = query.to_string();
            update_rows(&ui, &app.borrow());
        });
    }

    ui.run()?;
    // Flush even if the window was closed during an in-progress brush stroke.
    app.borrow_mut().persist()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn plain_text_input_reconciliation_uses_explicit_pending_caret_style() {
        let mut note = Note::create("note");
        note.body = "hi 🦊!".into();
        let mut typed = Some(TextStyle {
            bold: true, color: Some("#AABBCC".into()), size: Some(28),
            font_family: Some("Serif".into()),
        });
        assert!(apply_text_input_edit(&mut note, "hi 🦊X!".into(), typed.take()).unwrap());
        assert_eq!(note.body, "hi 🦊X!");
        assert_eq!(note.style_at(4).size, Some(28));
        assert!(note.style_at(4).bold);
        assert_eq!(note.style_at(4).font_family.as_deref(), Some("Serif"));
        assert!(note.style_at(3).is_default());
        // TextInput external updates identical to local state must not mutate it.
        let previous = note.rich_text.clone();
        let unchanged = note.body.clone();
        assert!(!apply_text_input_edit(&mut note, unchanged, None).unwrap());
        assert_eq!(note.rich_text, previous);
    }

    #[test]
    fn undo_rejects_remote_unsynced_text_without_touching_media() {
        let root = std::env::temp_dir().join(format!("inkpad-undo-remote-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let mut gui = App::load(Store::in_folder(root.clone()).unwrap()).unwrap();
        let original = gui.active().unwrap().clone();
        gui.text_history.record(&original, EditKind::Typing);
        gui.active_mut().unwrap().set_body_preserving_format("local editor typing".into()).unwrap();
        let after = gui.active().unwrap().clone();
        gui.text_history.commit_after(&after);
        let agent = Store::in_folder(root.clone()).unwrap();
        agent.mutate(|workspace| {
            let note = workspace.notes.iter_mut().find(|note| note.id == original.id).unwrap();
            note.set_body_preserving_format("remote agent changed text".into())
                .map_err(io::Error::other)?;
            Ok(())
        }).unwrap();
        assert_eq!(check_editor_history_fresh(&gui).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(gui.active().unwrap().body, "local editor typing");
        assert_eq!(agent.read_latest().unwrap().notes[0].body, "remote agent changed text");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rich_text_preview_parses_untrusted_literal_body_safely() {
        let mut note = Note::create("note");
        note.body = "Plain **not markup** <script>alert(1)</script> & emoji 猫🦊".into();
        let length = note.body.chars().count();
        note.format_text_range(6, length, TextStylePatch {
            bold: Some(true), color: Some("#B12466".into()),
            ..TextStylePatch::default()
        }).unwrap();
        // Slint's formatter must not interpret literal user content as HTML
        // control markup or silently drop the entire preview.
        let rendered = note_styled_preview(&note);
        let rendered_debug = format!("{rendered:?}");
        assert!(rendered_debug.contains("Plain **not markup** <script>alert(1)</script> & emoji 猫🦊"));
        assert!(rendered_debug.contains("style: Strong"));
        assert!(rendered_debug.contains("style: Color("));
    }

    #[test]
    fn rich_text_agent_format_merges_with_desktop_prefix_edit() {
        let mut base = Note::create("note");
        base.body = "hello world".into();
        let mut local = base.clone();
        local.set_body_preserving_format("Dear hello world".into()).unwrap();
        let mut remote = base.clone();
        remote.format_text_range(6, 11, TextStylePatch {
            bold: Some(true), ..TextStylePatch::default()
        }).unwrap();
        let merged = merge_note(&base, &local, &remote).unwrap();
        assert_eq!(merged.body, "Dear hello world");
        merged.validate_rich_text().unwrap();
        assert!(!merged.style_at(1).bold, "prefixed word must remain unstyled");
        assert!(merged.style_at(11).bold, "original word styling must follow its text");
    }

    #[test]
    fn appearance_theme_persists_and_agent_changes_sync_without_touching_notes() {
        let root = std::env::temp_dir().join(format!("inkpad-theme-sync-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::in_folder(root.clone()).unwrap();
        let mut app = App::load(store).unwrap();
        let before_notes = app.storage.read_latest().unwrap().notes;
        assert_eq!(app.ui_settings.theme, AppearanceTheme::Light);
        app.set_theme("dark").unwrap();
        assert_eq!(UiSettings::load(app.storage.data_path()).unwrap().theme, AppearanceTheme::Dark);
        UiSettings::update(app.storage.data_path(), |s| s.theme = AppearanceTheme::System).unwrap();
        assert!(app.sync_preferences().unwrap());
        assert_eq!(app.ui_settings.theme, AppearanceTheme::System);
        assert_eq!(app.storage.read_latest().unwrap().notes, before_notes);
        assert!(!app.sync_preferences().unwrap());
        drop(app);
        let reopened = App::load(Store::in_folder(root.clone()).unwrap()).unwrap();
        assert_eq!(reopened.ui_settings.theme, AppearanceTheme::System);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn drawing_survives_restart_and_undo_saves() {
        let folder = std::env::temp_dir().join(format!(
            "inkpad-app-smoke-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut first = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        first.create("board").unwrap();
        assert!(first.draw(120, 110, 0).unwrap());
        assert!(first.draw(240, 190, 1).unwrap());
        assert!(first.draw(240, 190, 2).unwrap());
        let saved_id = first.workspace.active_id.clone();
        drop(first);

        let mut reopened = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        assert_eq!(reopened.workspace.active_id, saved_id);
        assert_eq!(reopened.active().unwrap().strokes.len(), 1);
        assert!(reopened.active().unwrap().strokes[0].points.len() >= 2);
        let frame = reopened.board.as_ref().unwrap();
        let mark = ((110 * WIDTH + 120) * 4) as usize;
        assert_ne!(&frame.bytes()[mark..mark + 4], &[255, 255, 255, 255]);
        assert_eq!(&frame.png_bytes().unwrap()[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        reopened.undo().unwrap();
        drop(reopened);

        let after_undo = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        assert_eq!(after_undo.active().unwrap().strokes.len(), 0);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn gui_and_background_agent_changes_merge_without_losing_notes() {
        let folder = std::env::temp_dir().join(format!(
            "inkpad-agent-merge-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut gui = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        let agent_store = Store::in_folder(folder.clone()).unwrap();
        agent_store.mutate(|workspace| {
            let mut note = Note::create("note");
            note.title = "Added by an agent".into();
            note.body = "Background note".into();
            workspace.notes.push(note);
            Ok(())
        }).unwrap();
        gui.active_mut().unwrap().body = "Edited in the GUI".into();
        gui.persist().unwrap();
        let both = agent_store.load().unwrap();
        assert_eq!(both.notes.len(), 2);
        assert!(both.notes.iter().any(|n| n.body == "Background note"));
        assert!(both.notes.iter().any(|n| n.body == "Edited in the GUI"));

        agent_store.mutate(|workspace| {
            workspace.notes.iter_mut().find(|n| n.title == "Added by an agent").unwrap()
                .body = "Updated from the background".into();
            Ok(())
        }).unwrap();
        assert!(gui.sync_external().unwrap());
        assert!(gui.workspace.notes.iter().any(|n| n.body == "Updated from the background"));
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn conflicting_agent_edit_is_detected_not_silently_overwritten() {
        let folder = std::env::temp_dir().join(format!(
            "inkpad-agent-conflict-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut gui = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        gui.active_mut().unwrap().body = "Unsaved human note".into();
        let agent_store = Store::in_folder(folder.clone()).unwrap();
        agent_store.mutate(|workspace| {
            workspace.notes[0].body = "Changed by agent".into();
            Ok(())
        }).unwrap();
        assert_eq!(gui.persist().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(agent_store.load().unwrap().notes[0].body, "Changed by agent");
        assert_eq!(gui.active().unwrap().body, "Unsaved human note");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn one_page_keeps_both_text_and_drawing_when_switching_modes() {
        let folder = std::env::temp_dir().join(format!(
            "inkpad-mixed-page-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        assert_eq!(app.active().unwrap().kind, "note");
        app.active_mut().unwrap().body = "Text for the same drawing page".into();
        app.persist().unwrap();
        app.edit_mode = "draw".into();
        assert!(app.draw(80, 90, 0).unwrap());
        assert!(app.draw(140, 160, 1).unwrap());
        assert!(app.draw(140, 160, 2).unwrap());
        app.edit_mode = "text".into();
        assert_eq!(app.active().unwrap().body, "Text for the same drawing page");
        assert_eq!(app.active().unwrap().strokes.len(), 1);
        drop(app);

        let reopened = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        let note = reopened.active().unwrap();
        assert_eq!(note.kind, "note");
        assert_eq!(note.body, "Text for the same drawing page");
        assert_eq!(note.strokes.len(), 1);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn agent_can_select_page_while_gui_open_without_losing_note_content() {
        let folder = std::env::temp_dir().join(format!(
            "inkpad-agent-select-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut gui = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        let agent_store = Store::in_folder(folder.clone()).unwrap();
        let new_id = agent_store.mutate(|workspace| {
            let mut note = Note::create("note");
            note.title = "Selected by an agent".into();
            let id = note.id.clone();
            workspace.notes.push(note);
            workspace.active_id = id.clone();
            Ok(id)
        }).unwrap();

        assert!(gui.sync_external().unwrap());
        assert_eq!(gui.workspace.active_id, new_id);
        assert_eq!(gui.active().unwrap().title, "Selected by an agent");

        gui.active_mut().unwrap().body = "My desktop edits".into();
        gui.persist().unwrap();
        let latest = agent_store.read_latest().unwrap();
        assert_eq!(latest.active_id, new_id);
        assert_eq!(latest.notes.iter().find(|n| n.id == new_id).unwrap().body, "My desktop edits");
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn unrelated_gui_save_does_not_revert_agent_page_selection() {
        let folder = std::env::temp_dir().join(format!(
            "inkpad-selection-merge-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut gui = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        let agent_store = Store::in_folder(folder.clone()).unwrap();
        gui.active_mut().unwrap().body = "Edited in desktop before agent selected".into();
        let new_id = agent_store.mutate(|workspace| {
            let note = Note::create("note");
            let id = note.id.clone();
            workspace.notes.push(note);
            workspace.active_id = id.clone();
            Ok(id)
        }).unwrap();
        gui.persist().unwrap();
        let stored = agent_store.load().unwrap();
        assert_eq!(stored.active_id, new_id);
        assert!(stored.notes.iter().any(|n| n.body == "Edited in desktop before agent selected"));
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn agent_image_insert_merges_with_unsaved_desktop_text_without_data_loss() {
        use image::ImageEncoder;
        let folder = std::env::temp_dir().join(format!(
            "inkpad-image-concurrent-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut gui = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        let note_id = gui.workspace.active_id.clone();
        gui.active_mut().unwrap().body = "User is still typing".into();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[12, 80, 160, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let agent = Store::in_folder(folder.clone()).unwrap();
        let attachment = agent.insert_image_bytes(&note_id, &png).unwrap();
        gui.persist().unwrap();
        let stored = agent.read_latest().unwrap();
        let note = stored.notes.iter().find(|n| n.id == note_id).unwrap();
        assert_eq!(note.body, "User is still typing");
        assert_eq!(note.images.len(), 1);
        assert_eq!(note.images[0].id, attachment.id);
        assert!(agent.media_path(&attachment).unwrap().exists());
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn sidebar_collapse_is_remembered_across_launches_without_mutating_notes() {
        let folder = std::env::temp_dir().join(format!(
            "inkpad-sidebar-pref-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        let initial_workspace = app.storage.read_latest().unwrap();
        assert!(!app.ui_settings.sidebar_collapsed);
        assert_eq!(app.toggle_sidebar().unwrap(), true);
        assert_eq!(initial_workspace.notes, app.storage.read_latest().unwrap().notes);
        drop(app);
        let mut reopened = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        assert!(reopened.ui_settings.sidebar_collapsed);
        assert!(!reopened.toggle_sidebar().unwrap());
        drop(reopened);
        let restored = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        assert!(!restored.ui_settings.sidebar_collapsed);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn in_app_delete_confirmation_is_explicit_and_targeted() {
        let folder = std::env::temp_dir().join(format!(
            "inkpad-inline-confirm-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        app.create("note").unwrap();
        let selected = app.workspace.active_id.clone();
        let first = app.workspace.notes.iter().find(|n| n.id != selected).unwrap().id.clone();
        let (title, body, button) = app.request_delete(&first).unwrap();
        assert_eq!(title, "Delete this page?");
        assert!(body.contains("cannot be undone"));
        assert_eq!(button, "Delete page");
        app.cancel_action();
        app.confirm_action().unwrap(); // no accidental deletion after cancel
        assert_eq!(app.storage.load().unwrap().notes.len(), 2);
        app.request_delete(&first).unwrap();
        app.confirm_action().unwrap();
        let saved = app.storage.load().unwrap();
        assert_eq!(saved.notes.len(), 1);
        assert_eq!(saved.active_id, selected);
        assert!(saved.notes.iter().any(|n| n.id == selected));
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn clear_drawing_confirmation_preserves_text_and_refuses_wrong_page() {
        let folder = std::env::temp_dir().join(format!(
            "inkpad-inline-clear-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        app.active_mut().unwrap().body = "Keep this text".into();
        app.persist().unwrap();
        app.draw(17, 28, 0).unwrap();
        app.draw(70, 80, 2).unwrap();
        let saved_id = app.workspace.active_id.clone();
        let (title, body, button) = app.request_clear().unwrap();
        assert_eq!(title, "Clear this drawing?");
        assert!(body.contains("text will remain"));
        assert_eq!(button, "Clear drawing");
        app.create("note").unwrap();
        assert_eq!(app.confirm_action().unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(app.storage.load().unwrap().notes.iter().find(|n| n.id == saved_id).unwrap().strokes.len(), 1);
        app.select(&saved_id).unwrap();
        app.request_clear().unwrap();
        app.confirm_action().unwrap();
        let restored = app.storage.load().unwrap();
        let note = restored.notes.iter().find(|n| n.id == saved_id).unwrap();
        assert_eq!(note.strokes.len(), 0);
        assert_eq!(note.body, "Keep this text");
        fs::remove_dir_all(folder).unwrap();
    }
}
