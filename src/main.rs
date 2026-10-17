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
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, StyledText, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};
use settings::{AppearanceTheme, UiSettings};
use store::{DrawFill, DrawObject, DrawShape, DrawingHistory, Note, NoteContentBlock, Point, Store, Stroke, StyledFragment, TextSelection, TextStyle, TextStylePatch, Workspace};
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
    draw_tool: String,
    draw_filled: bool,
    draw_fill_color: String,
    draw_opacity: u8,
    selected_draw_layer_id: Option<String>,
    selected_draw_object: Option<String>,
    draw_origin: Option<[u16; 2]>,
    draw_previous: Option<[u16; 2]>,
    draw_gesture_changed: bool,
    drawing_history: DrawingHistory,
    drawing_undo_order: Vec<DrawingEdit>,
    drawing_redo_order: Vec<DrawingEdit>,
    last_canvas_render: Instant,
    ui_settings: UiSettings,
    pending_action: Option<PendingAction>,
    text_history: TextHistory,
    typing_style: Option<TextStyle>,
    selection: TextSelection,
    /// A newly opened page has no meaningful caret location yet. A real
    /// selection event makes image insertion use that point; otherwise attach
    /// at the end of the note instead of unexpectedly before its first word.
    selection_explicit: bool,
    rich_clipboard: Option<StyledFragment>,
}

/// Tracks actual cross-tool edit order. Engine snapshots store vector state;
/// a legacy stroke entry is recorded separately to keep undo predictable
/// when pen and editable shape tools are used on the same page.
#[derive(Clone)]
enum DrawingEdit {
    Vector,
    Stroke(Option<Stroke>),
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
                format!("All strokes and editable shapes on “{title}” will be erased. Your text will remain."),
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
    let anchor_source_body = if local.inline_media != base.inline_media {
        &local.body
    } else if remote.inline_media != base.inline_media {
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
        drawing: merge_field(&base.drawing, &local.drawing, &remote.drawing, "vector drawing")?,
        images: merge_field(&base.images, &local.images, &remote.images, "picture blocks")?,
        inline_media: merge_field(&base.inline_media, &local.inline_media, &remote.inline_media, "picture placement")?,
        updated: local.updated.max(remote.updated),
    };
    if style_source_body != &merged.body {
        // Rebase typography without also moving inline pictures. Their saved
        // anchor offsets can originate from a different concurrent revision.
        let mut styled = merged.clone();
        styled.inline_media.clear();
        styled.body = style_source_body.clone();
        styled.set_body_preserving_format(merged.body.clone()).map_err(|error|
            io::Error::new(io::ErrorKind::WouldBlock, format!("Cannot safely merge formatted note text: {error}")))?;
        merged.rich_text = styled.rich_text;
    }
    if anchor_source_body != &merged.body {
        merged.rebase_image_placements_for_text_change(anchor_source_body);
    }
    merged.validate_rich_text().map_err(|error|
        io::Error::new(io::ErrorKind::InvalidData, format!("Invalid merged text formatting: {error}")))?;
    merged.validate_inline_media().map_err(|error|
        io::Error::new(io::ErrorKind::InvalidData, format!("Invalid merged picture placement: {error}")))?;
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
                         typed: Option<TextStyle>, new_caret: Option<usize>) -> io::Result<bool> {
    if note.body == edited { return Ok(false); }
    let old = note.body.chars().collect::<Vec<_>>();
    let new = edited.chars().collect::<Vec<_>>();
    let old_length = old.len();
    let new_length = new.len();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let common_suffix = old.iter().rev().zip(new.iter().rev())
        .take_while(|(a, b)| a == b).count();
    // Full-buffer TextInput edits have ambiguous splice locations when the
    // surrounding characters repeat ("aaa" -> "aaaa", for instance). Prefer
    // the shortest possible edit nearest the native editor's resulting caret;
    // otherwise formatting can remain on the newly inserted character instead
    // of following the original characters.
    let (start, suffix) = (0..=prefix).map(|candidate| {
        let suffix = common_suffix.min(old_length.min(new_length) - candidate);
        let inserted_end = new_length - suffix;
        let unchanged = candidate + suffix;
        let distance = new_caret.map_or(0, |caret| caret.abs_diff(inserted_end));
        (candidate, suffix, unchanged, distance)
    }).max_by(|a, b| a.2.cmp(&b.2).then_with(|| b.3.cmp(&a.3))
        .then_with(|| a.0.cmp(&b.0)))
        .map(|(start, suffix, _, _)| (start, suffix)).unwrap();
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

/// Conservative transformed hit rectangle for pointer selection and handles.
/// Precise shape editing remains in the vector model; bounds are deliberately
/// slightly generous for reliable mouse selection and touch-sized targets.
fn drawing_bounds(object: &DrawObject) -> (f32, f32, f32, f32) {
    let mut points = match &object.shape {
        DrawShape::Rectangle { width, height } |
        DrawShape::RoundedRectangle { width, height, .. } =>
            vec![Point::new(0.0, 0.0), Point::new(*width, 0.0),
                 Point::new(*width, *height), Point::new(0.0, *height)],
        DrawShape::Ellipse { radius_x, radius_y } =>
            vec![Point::new(-radius_x, -radius_y), Point::new(*radius_x, -*radius_y),
                 Point::new(*radius_x, *radius_y), Point::new(-*radius_x, *radius_y)],
        DrawShape::Circle { radius } =>
            vec![Point::new(-radius, -radius), Point::new(*radius, -*radius),
                 Point::new(*radius, *radius), Point::new(-*radius, *radius)],
        DrawShape::Line { to } => vec![Point::default(), *to],
        DrawShape::Polygon { points, .. } => points.clone(),
        DrawShape::CubicBezier { control1, control2, to } =>
            vec![Point::default(), *control1, *control2, *to],
    };
    if points.is_empty() { points.push(Point::default()); }
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for point in points {
        let p = object.transform.apply(point);
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    (min_x, min_y, max_x, max_y)
}

type PictureKey = (String, String); // (page ID, picture ID)

/// Mirror the media store's 500-Unicode-character caption limit before an
/// invalid value can enter a debounced save queue.
const MAX_PICTURE_CAPTION_CHARS: usize = 500;

fn queue_picture_caption(
    captions: &mut HashMap<PictureKey, String>,
    key: PictureKey,
    caption: &str,
) -> io::Result<()> {
    if caption.chars().take(MAX_PICTURE_CAPTION_CHARS + 1).count() > MAX_PICTURE_CAPTION_CHARS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            "Picture captions are limited to 500 characters"));
    }
    captions.insert(key, caption.to_owned());
    Ok(())
}

/// A completed deletion must also remove edits that were queued while the
/// confirmation sheet was open. Other pages and pictures keep their edits.
fn discard_deleted_picture_edits(
    action: &PendingAction,
    widths: &mut HashMap<PictureKey, u8>,
    captions: &mut HashMap<PictureKey, String>,
) {
    let keep = |(note_id, image_id): &PictureKey| match action {
        PendingAction::Delete { id, .. } => note_id != id,
        PendingAction::DeletePicture { note_id: deleted_note, image_id: deleted_image } =>
            note_id != deleted_note || image_id != deleted_image,
        PendingAction::ClearDrawing { .. } => true,
    };
    widths.retain(|key, _| keep(key));
    captions.retain(|key, _| keep(key));
}

/// Keep pending edits for *every* picture when a debounce timer is restarted.
/// Removal happens only after the store has accepted each individual update.
/// This can also be called on Save, export, and window shutdown, before any
/// debounce timer is due.
fn flush_pending_picture_edits(
    app: &mut App,
    widths: &mut HashMap<PictureKey, u8>,
    captions: &mut HashMap<PictureKey, String>,
) -> io::Result<bool> {
    if widths.is_empty() && captions.is_empty() { return Ok(false); }
    app.persist()?;
    let mut changed = false;
    let result = (|| -> io::Result<()> {
        for key in widths.keys().cloned().collect::<Vec<_>>() {
            let width = widths[&key];
            app.storage.update_image(&key.0, &key.1, None, Some(width), None)?;
            widths.remove(&key);
            changed = true;
        }
        for key in captions.keys().cloned().collect::<Vec<_>>() {
            let caption = &captions[&key];
            app.storage.update_image(&key.0, &key.1, Some(caption), None, None)?;
            captions.remove(&key);
            changed = true;
        }
        Ok(())
    })();
    // Refresh after partial success too, keeping the in-memory notebook in
    // step with each image update that actually reached disk.
    if changed { app.reload_after_media_change()?; }
    result?;
    Ok(changed)
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
            brush_color: "#263238".into(), eraser: false,
            draw_tool: "pen".into(), draw_filled: true, draw_fill_color: "#F0D964".into(),
            draw_opacity: 100, selected_draw_layer_id: None,
            selected_draw_object: None, draw_origin: None,
            draw_previous: None, draw_gesture_changed: false,
            drawing_history: DrawingHistory::default(), ui_settings,
            drawing_undo_order: Vec::new(), drawing_redo_order: Vec::new(),
            last_canvas_render: Instant::now(),
            pending_action: None,
            text_history: TextHistory::default(), typing_style: None,
            selection: TextSelection::caret(0),
            selection_explicit: false,
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
        self.draw_origin = None;
        self.draw_previous = None;
        self.draw_gesture_changed = false;
        self.selected_draw_object = None;
        self.selected_draw_layer_id = self.active().and_then(|note|
            note.drawing.layers.last().map(|layer| layer.id.clone()));
        self.drawing_history.clear();
        self.drawing_undo_order.clear();
        self.drawing_redo_order.clear();
        // Preserve Inkpad's existing "undo last saved pen stroke" behaviour
        // after reopening a note, without allowing an untracked vector edit.
        // Existing strokes are explicitly registered as undoable entries.
        let legacy_strokes = self.active().map_or(0, |note| note.strokes.len().min(48));
        self.drawing_undo_order.extend(
            std::iter::repeat_n(DrawingEdit::Stroke(None), legacy_strokes));
        self.board = self.active().map(Board::from_note);
    }

    fn record_vector_edit(&mut self, before: &store::DrawingData) {
        self.drawing_history.record(before);
        self.drawing_undo_order.push(DrawingEdit::Vector);
        if self.drawing_undo_order.len() > 48 { self.drawing_undo_order.remove(0); }
        self.drawing_redo_order.clear();
    }

    fn record_stroke_edit(&mut self) {
        self.drawing_undo_order.push(DrawingEdit::Stroke(None));
        if self.drawing_undo_order.len() > 48 { self.drawing_undo_order.remove(0); }
        self.drawing_redo_order.clear();
    }

    /// Redraw without discarding a selected shape or session history.
    fn refresh_drawing(&mut self) {
        self.board = self.active().map(Board::from_note);
        self.last_canvas_render = Instant::now();
    }

    fn preview_drawing(&mut self) {
        // A 1100x700 software render becomes expensive with hundreds of paths.
        // Pointer events may arrive far faster than a human-visible frame rate.
        if self.last_canvas_render.elapsed() >= Duration::from_millis(40) {
            self.refresh_drawing();
        }
    }

    fn update_selected_object(&mut self, change: impl FnOnce(&mut DrawObject)) -> io::Result<bool> {
        let Some(id) = self.selected_draw_object.clone() else { return Ok(false); };
        let Some(note) = self.active_mut() else { return Ok(false); };
        let before = note.drawing.snapshot();
        note.drawing.update_object(&id, change).map_err(io::Error::other)?;
        if note.drawing == before { return Ok(false); }
        note.touch();
        self.record_vector_edit(&before);
        self.refresh_drawing();
        self.persist()?;
        Ok(true)
    }

    fn duplicate_selected_object(&mut self) -> io::Result<()> {
        let Some(id) = self.selected_draw_object.clone() else { return Ok(()); };
        let Some(note) = self.active_mut() else { return Ok(()); };
        let original = note.drawing.object(&id)
            .ok_or_else(|| io::Error::other("Selected drawing object no longer exists"))?.clone();
        let before = note.drawing.snapshot();
        let next = note.drawing.add_object(original.shape.clone(), Some(&original.layer_id))
            .map_err(io::Error::other)?;
        note.drawing.update_object(&next, |obj| {
            obj.style = original.style.clone();
            obj.transform = original.transform.clone();
            obj.transform.translation.x += 12.0;
            obj.transform.translation.y += 12.0;
            obj.visible = original.visible;
        }).map_err(io::Error::other)?;
        note.touch();
        self.record_vector_edit(&before);
        self.selected_draw_object = Some(next);
        self.refresh_drawing();
        self.persist()
    }

    fn delete_selected_object(&mut self) -> io::Result<()> {
        let Some(id) = self.selected_draw_object.clone() else { return Ok(()); };
        let Some(note) = self.active_mut() else { return Ok(()); };
        let before = note.drawing.snapshot();
        note.drawing.remove_object(&id).map_err(io::Error::other)?;
        note.touch();
        self.record_vector_edit(&before);
        self.selected_draw_object = None;
        self.refresh_drawing();
        self.persist()
    }

    fn add_drawing_layer(&mut self) -> io::Result<()> {
        let Some(note) = self.active_mut() else { return Ok(()); };
        let before = note.drawing.snapshot();
        let name = format!("Layer {}", note.drawing.layers.len() + 1);
        let new_id = note.drawing.add_layer(&name).map_err(io::Error::other)?;
        note.touch();
        self.record_vector_edit(&before);
        self.selected_draw_layer_id = Some(new_id);
        self.refresh_drawing();
        self.persist()
    }

    fn edit_drawing_layer(
        &mut self,
        change: impl FnOnce(&mut store::DrawingData) -> Result<(), String>,
    ) -> io::Result<()> {
        let Some(note) = self.active_mut() else { return Ok(()); };
        let before = note.drawing.snapshot();
        change(&mut note.drawing).map_err(io::Error::other)?;
        if note.drawing == before { return Ok(()); }
        note.touch();
        self.record_vector_edit(&before);
        self.refresh_drawing();
        self.persist()
    }

    fn create(&mut self, kind: &str) -> io::Result<()> {
        let note = Note::create(kind);
        self.workspace.active_id = note.id.clone();
        self.workspace.notes.push(note);
        self.edit_mode = if kind == "board" { "draw" } else { "text" }.to_string();
        self.selection = TextSelection::caret(0);
        self.selection_explicit = false;
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
            self.selection_explicit = false;
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
        if phase == 3 {
            // Slint pointer capture can be cancelled by focus/window loss.
            // Cancel is NOT a pointer-up: discard the unfinished gesture
            // rather than persisting a stray zero-sized shape or stroke.
            if !self.drawing { return Ok(false); }
            let was_vector = self.draw_tool != "pen" && self.draw_tool != "eraser";
            let had_vector_edit = self.draw_tool != "select" || self.draw_gesture_changed;
            if was_vector && had_vector_edit {
                let mut history = std::mem::take(&mut self.drawing_history);
                if let Some(note) = self.active_mut() {
                    history.cancel_last(&mut note.drawing);
                }
                self.drawing_history = history;
                self.drawing_undo_order.pop();
            } else if !was_vector {
                if let Some(note) = self.active_mut() { note.strokes.pop(); }
                self.drawing_undo_order.pop();
            }
            self.drawing = false;
            self.draw_origin = None;
            self.draw_previous = None;
            self.draw_gesture_changed = false;
            self.selected_draw_object = None;
            self.refresh_drawing();
            return Ok(true);
        }
        let point = [x.clamp(0, WIDTH as i32 - 1) as u16, y.clamp(0, HEIGHT as i32 - 1) as u16];
        if self.draw_tool == "select" {
            match phase {
                0 => {
                    let (px, py) = (point[0] as f32, point[1] as f32);
                    self.selected_draw_object = self.active().and_then(|note|
                        canvas::pick_object(&note.drawing, Point::new(px, py), 7.0));
                    self.drawing = true;
                    self.draw_previous = Some(point);
                    self.draw_gesture_changed = false;
                    return Ok(true);
                }
                1 | 2 if self.drawing => {
                    if let (Some(previous), Some(id)) = (self.draw_previous, self.selected_draw_object.clone()) {
                        if previous != point {
                            let before = self.active().unwrap().drawing.snapshot();
                            if let Some(note) = self.active_mut() {
                                note.drawing.move_object(&id,
                                    point[0] as f32 - previous[0] as f32,
                                    point[1] as f32 - previous[1] as f32).map_err(io::Error::other)?;
                                note.touch();
                            }
                            if !self.draw_gesture_changed { self.record_vector_edit(&before); }
                            self.draw_gesture_changed = true;
                            self.draw_previous = Some(point);
                            if phase == 2 { self.refresh_drawing(); }
                            else { self.preview_drawing(); }
                        }
                    }
                    if phase == 2 {
                        self.drawing = false;
                        if self.draw_gesture_changed { self.persist()?; }
                    }
                    return Ok(true);
                }
                _ => return Ok(false),
            }
        }
        if matches!(self.draw_tool.as_str(), "rectangle" | "ellipse" | "line" |
                   "rounded" | "triangle" | "curve" | "circle") {
            match phase {
                0 => {
                    let shape = match self.draw_tool.as_str() {
                        "ellipse" => DrawShape::Ellipse { radius_x: 0.5, radius_y: 0.5 },
                        "circle" => DrawShape::Circle { radius: 0.5 },
                        "line" => DrawShape::Line { to: Point::new(1.0, 1.0) },
                        "rounded" => DrawShape::RoundedRectangle { width: 1.0, height: 1.0, radius: 0.2 },
                        "triangle" => DrawShape::Polygon {
                            points: vec![Point::new(0.5, 0.0), Point::new(1.0, 1.0),
                                         Point::new(0.0, 1.0)], closed: true },
                        "curve" => DrawShape::CubicBezier {
                            control1: Point::new(0.3, 0.0),
                            control2: Point::new(0.7, 1.0),
                            to: Point::new(1.0, 1.0) },
                        _ => DrawShape::Rectangle { width: 1.0, height: 1.0 },
                    };
                    let before = self.active().unwrap().drawing.snapshot();
                    let stroke = parse_color(&self.brush_color);
                    let fill = parse_color(&self.draw_fill_color);
                    let filled = self.draw_filled;
                    let opacity = self.draw_opacity as f32 / 100.0;
                    let selected_layer = self.selected_draw_layer_id.clone();
                    let id = {
                        let note = self.active_mut().unwrap();
                        let id = note.drawing.add_object(shape, selected_layer.as_deref())
                            .map_err(io::Error::other)?;
                        note.drawing.update_object(&id, |object| {
                            object.transform.translation = Point::new(point[0] as f32, point[1] as f32);
                            object.style.stroke = Some(stroke);
                            object.style.stroke_width = 3.0;
                            object.style.fill = if filled && !matches!(object.shape,
                                DrawShape::Line { .. } | DrawShape::CubicBezier { .. }) {
                                Some(DrawFill::Solid { color: fill })
                            } else { None };
                            object.style.opacity = opacity;
                        }).map_err(io::Error::other)?;
                        note.touch();
                        id
                    };
                    self.record_vector_edit(&before);
                    self.selected_draw_object = Some(id);
                    self.draw_origin = Some(point);
                    self.drawing = true;
                    self.refresh_drawing();
                    return Ok(true);
                }
                1 | 2 if self.drawing => {
                    if let (Some(start), Some(id)) = (self.draw_origin, self.selected_draw_object.clone()) {
                        let (dx, dy) = (point[0] as f32 - start[0] as f32,
                                        point[1] as f32 - start[1] as f32);
                        let tool = self.draw_tool.clone();
                        if let Some(note) = self.active_mut() {
                            note.drawing.update_object(&id, |object| {
                                match tool.as_str() {
                                    "ellipse" => {
                                        object.shape = DrawShape::Ellipse {
                                            radius_x: (dx.abs() / 2.0).max(0.5),
                                            radius_y: (dy.abs() / 2.0).max(0.5),
                                        };
                                        object.transform.translation = Point::new(
                                            start[0].min(point[0]) as f32 + (dx.abs() / 2.0).max(0.5),
                                            start[1].min(point[1]) as f32 + (dy.abs() / 2.0).max(0.5));
                                    }
                                    "circle" => {
                                        let radius = ((dx * dx + dy * dy).sqrt() / 2.0).max(0.5);
                                        object.shape = DrawShape::Circle { radius };
                                        object.transform.translation = Point::new(
                                            (start[0] as f32 + point[0] as f32) / 2.0,
                                            (start[1] as f32 + point[1] as f32) / 2.0);
                                    }
                                    "line" => {
                                        object.shape = DrawShape::Line { to: Point::new(dx, dy) };
                                        object.transform.translation = Point::new(start[0] as f32, start[1] as f32);
                                    }
                                    "curve" => {
                                        object.shape = DrawShape::CubicBezier {
                                            control1: Point::new(dx / 3.0, 0.0),
                                            control2: Point::new(dx * 2.0 / 3.0, dy),
                                            to: Point::new(dx, dy),
                                        };
                                        object.transform.translation = Point::new(start[0] as f32, start[1] as f32);
                                    }
                                    "triangle" | "rounded" => {
                                        let w = dx.abs().max(1.0);
                                        let h = dy.abs().max(1.0);
                                        object.shape = if tool == "triangle" {
                                            DrawShape::Polygon {
                                                points: vec![Point::new(w / 2.0, 0.0),
                                                             Point::new(w, h), Point::new(0.0, h)],
                                                closed: true,
                                            }
                                        } else {
                                            DrawShape::RoundedRectangle {
                                                width: w, height: h,
                                                radius: (w.min(h) * 0.15).min(50.0),
                                            }
                                        };
                                        object.transform.translation = Point::new(
                                            start[0].min(point[0]) as f32,
                                            start[1].min(point[1]) as f32);
                                    }
                                    _ => {
                                        object.shape = DrawShape::Rectangle {
                                            width: dx.abs().max(1.0), height: dy.abs().max(1.0),
                                        };
                                        object.transform.translation = Point::new(
                                            start[0].min(point[0]) as f32,
                                            start[1].min(point[1]) as f32);
                                    }
                                }
                            }).map_err(io::Error::other)?;
                            note.touch();
                        }
                        if phase == 2 { self.refresh_drawing(); }
                        else { self.preview_drawing(); }
                    }
                    if phase == 2 {
                        self.drawing = false;
                        self.draw_origin = None;
                        self.persist()?;
                    }
                    return Ok(true);
                }
                _ => return Ok(false),
            }
        }
        if self.draw_tool != "pen" && self.draw_tool != "eraser" { return Ok(false); }
        match phase {
            0 => {
                self.drawing = true;
                self.record_stroke_edit();
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
                    self.refresh_drawing();
                    self.persist()?;
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn undo(&mut self) -> io::Result<()> {
        self.drawing = false;
        let edit = self.drawing_undo_order.pop();
        match edit {
            Some(DrawingEdit::Vector) => {
                let mut history = std::mem::take(&mut self.drawing_history);
                let result = if let Some(note) = self.active_mut() {
                    history.undo(&mut note.drawing)
                } else { false };
                self.drawing_history = history;
                if !result { return Err(io::Error::other("No vector snapshot to undo")); }
                self.drawing_redo_order.push(DrawingEdit::Vector);
            }
            Some(DrawingEdit::Stroke(_)) => {
                let stroke = self.active_mut().and_then(|note| note.strokes.pop());
                if let Some(stroke) = stroke {
                    self.drawing_redo_order.push(DrawingEdit::Stroke(Some(stroke)));
                }
            }
            None => return Ok(()),
        }
        self.selected_draw_object = None;
        if let Some(note) = self.active_mut() { note.touch(); }
        self.refresh_drawing();
        self.persist()
    }

    fn redo_drawing(&mut self) -> io::Result<()> {
        let Some(edit) = self.drawing_redo_order.pop() else { return Ok(()); };
        match edit {
            DrawingEdit::Vector => {
                let mut history = std::mem::take(&mut self.drawing_history);
                let result = if let Some(note) = self.active_mut() {
                    history.redo(&mut note.drawing)
                } else { false };
                self.drawing_history = history;
                if !result { return Err(io::Error::other("No vector snapshot to redo")); }
                self.drawing_undo_order.push(DrawingEdit::Vector);
            }
            DrawingEdit::Stroke(Some(stroke)) => {
                if let Some(note) = self.active_mut() { note.strokes.push(stroke); }
                self.drawing_undo_order.push(DrawingEdit::Stroke(None));
            }
            DrawingEdit::Stroke(None) => return Err(io::Error::other("Stroke redo snapshot missing")),
        }
        self.selected_draw_object = None;
        if let Some(note) = self.active_mut() { note.touch(); }
        self.refresh_drawing();
        self.persist()
    }

    fn clear(&mut self) -> io::Result<()> {
        self.drawing = false;
        if let Some(note) = self.active_mut() {
            note.strokes.clear();
            note.drawing.objects.clear();
            note.touch();
        }
        self.drawing_history.clear();
        self.drawing_undo_order.clear();
        self.drawing_redo_order.clear();
        self.selected_draw_object = None;
        self.refresh_drawing();
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
        let (expected_body, anchor) = self.image_insertion_point()?;
        let inserted = self.storage.insert_image_from_file(&id, path)?;
        let placement = self.storage.place_image_at_scalar(&id, &inserted.id,
            Some(anchor), Some(&expected_body));
        let refresh = self.reload_after_media_change();
        self.edit_mode = "text".to_string();
        refresh?;
        placement?;
        Ok(())
    }

    fn import_image_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.persist()?;
        let id = self.workspace.active_id.clone();
        let (expected_body, anchor) = self.image_insertion_point()?;
        let inserted = self.storage.insert_image_bytes(&id, bytes)?;
        let placement = self.storage.place_image_at_scalar(&id, &inserted.id,
            Some(anchor), Some(&expected_body));
        let refresh = self.reload_after_media_change();
        self.edit_mode = "text".to_string();
        refresh?;
        placement?;
        Ok(())
    }

    fn image_insertion_point(&self) -> io::Result<(String, usize)> {
        let note = self.active().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound,
            "Select a page before inserting a picture"))?;
        let scalar_len = note.body.chars().count();
        let anchor = if self.selection_explicit {
            self.selection.focus.min(scalar_len)
        } else {
            scalar_len
        };
        Ok((note.body.clone(), anchor))
    }

    fn update_media(&mut self, id: &str, caption: Option<&str>, width: Option<u8>,
        position: Option<usize>) -> io::Result<()> {
        self.persist()?;
        let note_id = self.workspace.active_id.clone();
        self.storage.update_image(&note_id, id, caption, width, position)?;
        self.reload_after_media_change()
    }

    fn place_media_at_caret(&mut self, image_id: &str) -> io::Result<()> {
        if !self.selection_explicit {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                "Click in the writing area where this picture should appear first"));
        }
        self.persist()?;
        let note_id = self.workspace.active_id.clone();
        let (body, offset) = self.image_insertion_point()?;
        self.storage.place_image_at_scalar(&note_id, image_id, Some(offset), Some(&body))?;
        self.reload_after_media_change()
    }

    fn export_complete_page(&mut self) -> io::Result<()> {
        let Some(note) = self.active() else { return Ok(()); };
        let title: String = note.title.chars().map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c }
        }).take(65).collect();
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Web page with embedded pictures", &["html"])
            .set_file_name(format!("{}.html", if title.is_empty() { "Inkpad page" } else { &title }))
            .save_file() else { return Ok(()); };
        let id = note.id.clone();
        self.export_complete_page_to(&id, &path)
    }

    fn export_complete_page_to(&mut self, note_id: &str, path: &Path) -> io::Result<()> {
        // HTML export reads the on-disk workspace, while the visible editor
        // may still be inside its 380 ms autosave delay.
        self.persist()?;
        self.storage.export_note_html(note_id, path)
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

/// Show the text surrounding a body search hit instead of the first line.
/// Lowercasing can expand a Unicode character (e.g. İ -> i + combining dot),
/// so map the match back to the original body's character positions before
/// clipping. Never use offsets from the lowercased copy to slice the original.
fn body_match_snippet(body: &str, query: &str) -> Option<String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() { return None; }
    let match_byte = body.to_lowercase().find(&query)?;
    let mut lower_byte = 0;
    let mut match_char = 0usize;
    for ch in body.chars() {
        let char_bytes = ch.to_lowercase().map(char::len_utf8).sum::<usize>();
        if lower_byte + char_bytes > match_byte { break; }
        lower_byte += char_bytes;
        match_char += 1;
    }

    const CONTEXT_BEFORE: usize = 18;
    const MAX_SNIPPET_CHARS: usize = 72;
    let start = match_char.saturating_sub(CONTEXT_BEFORE);
    let excerpt: String = body.chars().skip(start).take(MAX_SNIPPET_CHARS).collect();
    let normalized = excerpt.split_whitespace().collect::<Vec<_>>().join(" ");
    let clipped_after = body.chars().skip(start + MAX_SNIPPET_CHARS).next().is_some();
    Some(format!("{}{}{}", if start > 0 { "… " } else { "" }, normalized,
        if clipped_after { " …" } else { "" }))
}

fn note_row_subtitle(note: &Note, query: &str) -> String {
    if let Some(snippet) = body_match_snippet(&note.body, query) {
        return snippet;
    }
    if !note.images.is_empty() {
        format!("{} picture{} · {}", note.images.len(), if note.images.len() == 1 { "" } else { "s" },
            note.body.lines().find(|line| !line.trim().is_empty()).unwrap_or("Photo page"))
            .chars().take(70).collect::<String>()
    } else if note.kind == "board" {
        let strokes = note.strokes.len();
        let shapes = note.drawing.objects.len();
        format!("Sketch · {strokes} stroke{} · {shapes} shape{}",
            if strokes == 1 { "" } else { "s" }, if shapes == 1 { "" } else { "s" })
    } else {
        let preview = note.body.lines().find(|line| !line.trim().is_empty()).unwrap_or("Empty note");
        preview.chars().take(58).collect()
    }
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
        subtitle: note_row_subtitle(note, &query).into(),
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

/// Produce a separate, read-only display span for the composed document.
/// The *single* editable TextInput remains authoritative for caret movement,
/// native selection, copy/paste and undo. A rendered span must never become an
/// independent text buffer that could silently overwrite a saved note.
fn note_visual_slice(note: &Note, start: usize, end: usize) -> Note {
    let mut part = note.clone();
    let start_byte = note.body.char_indices().nth(start)
        .map(|(at, _)| at).unwrap_or(note.body.len());
    let end_byte = note.body.char_indices().nth(end)
        .map(|(at, _)| at).unwrap_or(note.body.len());
    part.body = note.body.get(start_byte..end_byte).unwrap_or("").to_string();
    part.rich_text = note.rich_text.iter().filter_map(|run| {
        let lo = run.start.max(start);
        let hi = run.end.min(end);
        (lo < hi).then(|| store::TextRun {
            start: lo - start, end: hi - start, style: run.style.clone()
        })
    }).collect();
    part.images.clear();
    part.inline_media.clear();
    part
}

fn document_paragraph_model(note: &Note) -> (ModelRc<RichPreviewParagraph>, bool) {
    let Some(paragraphs) = visual_preview::build(note) else {
        return (ModelRc::from(Rc::new(VecModel::<RichPreviewParagraph>::from(vec![]))), false);
    };
    let rows = paragraphs.into_iter().map(|paragraph| {
        let spans = paragraph.spans.into_iter().map(|span| {
            let (has_foreground, foreground) = span.foreground
                .map(|rgb| (true, slint::Color::from_rgb_u8(rgb[0], rgb[1], rgb[2])))
                .unwrap_or((false, slint::Color::default()));
            RichPreviewSpan {
                text: span.layout_text().into(),
                size: span.font_size,
                family: span.family.into(),
                bold: span.bold,
                foreground,
                has_foreground,
                trailing_gap: span.text.chars().last().is_some_and(char::is_whitespace),
            }
        }).collect::<Vec<_>>();
        RichPreviewParagraph { spans: ModelRc::from(Rc::new(VecModel::from(spans))) }
    }).collect::<Vec<_>>();
    (ModelRc::from(Rc::new(VecModel::from(rows))), true)
}

/// Render the persisted text/image order in the same writing workspace.
/// Picture previews reuse the existing bounded thumbnail model and never read
/// full-resolution PNGs into the Slint scene. Legacy photos remain after text.
fn update_document_blocks(ui: &MainWindow, app: &App) {
    let Some(note) = app.active() else {
        ui.set_has_rich_formatting(false);
        ui.set_document_blocks(ModelRc::from(Rc::new(VecModel::<DocumentBlockRow>::from(vec![]))));
        return;
    };
    ui.set_has_rich_formatting(!note.rich_text.is_empty());
    let media_model = ui.get_media_rows();
    let media: HashMap<String, MediaRow> = (0..media_model.row_count())
        .filter_map(|index| media_model.row_data(index))
        .map(|row| (row.id.to_string(), row)).collect();
    let order = match note.content_blocks() {
        Ok(order) => order,
        Err(error) => {
            eprintln!("Inkpad document composition unavailable: {error}");
            vec![NoteContentBlock::Text { start: 0, end: note.body.chars().count() }]
        }
    };
    let blank_paragraphs = || ModelRc::from(Rc::new(VecModel::<RichPreviewParagraph>::from(vec![])));
    let blank_style = || StyledText::from_plain_text("");
    let rows = order.into_iter().map(|block| match block {
        NoteContentBlock::Text { start, end } => {
            let part = note_visual_slice(note, start, end);
            let (rich_paragraphs, rich_exact) = document_paragraph_model(&part);
            DocumentBlockRow {
                kind: "text".into(), text: note_styled_preview(&part),
                image_id: "".into(), caption: "".into(), width_percent: 100,
                position: 0, preview: Image::default(), can_move_up: false,
                can_move_down: false, rich_paragraphs, rich_exact,
            }
        }
        NoteContentBlock::Image { image_id } => {
            let row = media.get(&image_id);
            DocumentBlockRow {
                kind: "image".into(), text: blank_style(),
                image_id: image_id.into(),
                caption: row.map_or("".into(), |it| it.caption.clone()),
                width_percent: row.map_or(65, |it| it.width_percent),
                position: row.map_or(0, |it| it.position),
                preview: row.map_or_else(Image::default, |it| it.preview.clone()),
                can_move_up: row.is_some_and(|it| it.position > 0),
                can_move_down: row.is_some_and(|it| (it.position as usize) + 1 < media.len()),
                rich_paragraphs: blank_paragraphs(), rich_exact: false,
            }
        }
    }).collect::<Vec<_>>();
    ui.set_document_blocks(ModelRc::from(Rc::new(VecModel::from(rows))));
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
                    text: span.layout_text().into(),
                    size: span.font_size,
                    family: span.family.into(),
                    bold: span.bold,
                    foreground,
                    has_foreground,
                    trailing_gap: span.text.chars().last().is_some_and(char::is_whitespace),
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

fn update_drawing_ui(ui: &MainWindow, app: &App) {
    ui.set_draw_tool(app.draw_tool.clone().into());
    ui.set_can_undo_drawing(!app.drawing_undo_order.is_empty());
    ui.set_can_redo_drawing(!app.drawing_redo_order.is_empty());
    ui.set_draw_filled(app.draw_filled);
    ui.set_draw_fill_color(app.draw_fill_color.clone().into());
    ui.set_draw_opacity(app.draw_opacity as i32);
    ui.set_eraser(app.eraser);
    ui.set_brush_color(app.brush_color.clone().into());
    ui.set_brush_size(app.brush_size as i32);
    let drawing = app.active().map(|note| &note.drawing);
    ui.set_drawing_object_count(drawing.map_or(0, |d| d.objects.len() as i32));
    ui.set_drawing_layer_count(drawing.map_or(0, |d| d.layers.len() as i32));
    let layers = drawing.map(|d| d.layers.iter().map(|layer| DrawingLayerRow {
        id: layer.id.clone().into(),
        name: layer.name.clone().into(),
        visible: layer.visible,
        locked: layer.locked,
    }).collect::<Vec<_>>()).unwrap_or_default();
    ui.set_drawing_layers(ModelRc::from(Rc::new(VecModel::from(layers))));
    let selected_layer = app.selected_draw_layer_id.as_deref().and_then(|id|
        drawing.and_then(|d| d.layers.iter().find(|l| l.id == id)));
    ui.set_selected_draw_layer(selected_layer.map_or("", |l| l.id.as_str()).into());
    ui.set_selected_draw_layer_name(selected_layer.map_or("", |l| l.name.as_str()).into());
    let selected = app.selected_draw_object.as_deref().and_then(|id|
        drawing.and_then(|d| d.object(id)));
    if let Some(object) = selected {
        let (x0, y0, x1, y1) = drawing_bounds(object);
        ui.set_selected_draw_object(object.id.clone().into());
        ui.set_selected_draw_x(x0.max(0.0).min(WIDTH as f32) as i32);
        ui.set_selected_draw_y(y0.max(0.0).min(HEIGHT as f32) as i32);
        ui.set_selected_draw_width((x1-x0).max(1.0).min(WIDTH as f32) as i32);
        ui.set_selected_draw_height((y1-y0).max(1.0).min(HEIGHT as f32) as i32);
    } else {
        ui.set_selected_draw_object(SharedString::default());
    }
}

fn update_editor(ui: &MainWindow, app: &App) {
    if let Some(note) = app.active() {
        ui.set_has_note(true);
        ui.set_active_pinned(note.pinned);
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
        ui.set_active_pinned(false);
        ui.set_note_title(SharedString::default());
        ui.set_note_body(SharedString::default());
        ui.set_note_styled_preview(StyledText::from_plain_text(""));
        ui.set_selection_bold(false);
        ui.set_text_color("#292927".into());
        ui.set_active_kind("none".into());
    }
    ui.set_edit_mode(app.edit_mode.clone().into());
    if let Some(board) = app.board.as_ref() { ui.set_canvas_image(board_image(board)); }
    update_drawing_ui(ui, app);
    update_media_rows(ui, app);
    update_document_blocks(ui, app);
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

/// Refresh saved photo metadata in existing repeater rows. Recreating the
/// gallery on a caption timer destroys the focused caption editor/caret.
fn sync_visible_picture_rows(ui: &MainWindow, app: &App) {
    let Some(note) = app.active() else { return; };
    let rows = ui.get_media_rows();
    for index in 0..rows.row_count() {
        let Some(mut row) = rows.row_data(index) else { continue; };
        let Some(saved) = note.images.iter().find(|image| image.id == row.id.as_str()) else {
            continue;
        };
        if row.width_percent != saved.width_percent as i32 ||
            row.caption.as_str() != saved.caption
        {
            row.width_percent = saved.width_percent as i32;
            row.caption = saved.caption.clone().into();
            row.position = saved.position as i32;
            rows.set_row_data(index, row);
        }
    }
    // The embedded document is another view of the SAME saved image model.
    // Change existing repeater rows in place so a focused caption LineEdit
    // does not disappear when a debounced resize or caption save finishes.
    let documents = ui.get_document_blocks();
    for index in 0..documents.row_count() {
        let Some(mut row) = documents.row_data(index) else { continue; };
        if row.kind.as_str() != "image" { continue; }
        let Some(saved) = note.images.iter().find(|image| image.id == row.image_id.as_str()) else { continue; };
        if row.width_percent != saved.width_percent as i32 ||
            row.caption.as_str() != saved.caption ||
            row.position != saved.position as i32
        {
            row.width_percent = saved.width_percent as i32;
            row.caption = saved.caption.clone().into();
            row.position = saved.position as i32;
            documents.set_row_data(index, row);
        }
    }
}

/// Restore the last accepted caption after an overlong edit. An existing
/// pending (valid) caption takes precedence over the previously saved value.
fn restore_rejected_picture_caption(ui: &MainWindow, image_id: &str, caption: &str) {
    let rows = ui.get_media_rows();
    for index in 0..rows.row_count() {
        let Some(mut row) = rows.row_data(index) else { continue; };
        if row.id.as_str() == image_id {
            row.caption = caption.into();
            rows.set_row_data(index, row);
            break;
        }
    }
    let blocks = ui.get_document_blocks();
    for index in 0..blocks.row_count() {
        let Some(mut row) = blocks.row_data(index) else { continue; };
        if row.image_id.as_str() == image_id {
            row.caption = caption.into();
            blocks.set_row_data(index, row);
            break;
        }
    }
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
                if result.is_ok() {
                    update_all(&ui, &app.borrow());
                    ui.set_pictures_expanded(true);
                }
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
                if result.is_ok() {
                    update_all(&ui, &app.borrow());
                    ui.set_pictures_expanded(true);
                }
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
    let pending_picture_widths: Rc<RefCell<HashMap<PictureKey, u8>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let pending_picture_captions: Rc<RefCell<HashMap<PictureKey, String>>> =
        Rc::new(RefCell::new(HashMap::new()));
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
            // Drop the mutable RefCell borrow before reading settings below.
            // Matching directly on borrow_mut() keeps its temporary alive
            // through the match arm and panics when appearance is updated.
            let preferences_result = { app.borrow_mut().sync_preferences() };
            match preferences_result {
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
                let result: io::Result<()> = ($action)(&mut app.borrow_mut(), value, &ui);
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
                        if changed_page {
                            update_editor(&ui, &state);
                        } else if result.is_ok() {
                            update_document_blocks(&ui, &state);
                        }
                        drop(state);
                        set_status(&ui, result);
                    }
                });
            });
        }};
    }

    bind_change!(on_rename_note, |app: &mut App, text: SharedString, _ui: &MainWindow| {
        if let Some(note) = app.active_mut() {
            note.title = text.to_string(); note.touch();
        }
        Ok(())
    });
    bind_change!(on_edit_body, |app: &mut App, text: SharedString, ui: &MainWindow| {
        let edited = text.to_string();
        let caret = ui.get_selection_caret_byte();
        let caret_hint = if caret < 0 { None } else {
            TextSelection::from_utf8_bytes(&edited, caret as usize, caret as usize)
                .ok().map(|selection| selection.focus)
        };
        if let Some(before) = app.active().cloned() {
            if before.body != edited {
                let typed = app.typing_style.clone();
                app.text_history.record(&before, EditKind::Typing);
                if let Some(note) = app.active_mut() {
                    apply_text_input_edit(note, edited, typed, caret_hint)?;
                }
                if let Some(after) = app.active().cloned() { app.text_history.commit_after(&after); }
            }
        }
        Ok(())
    });
    bind_change!(on_change_font_size, |app: &mut App, size: i32, _ui: &MainWindow| {
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
    bind_change!(on_change_font_choice, |app: &mut App, choice: SharedString, _ui: &MainWindow| {
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
        let widths = pending_picture_widths.clone();
        let captions = pending_picture_captions.clone();
        ui.on_confirm_delete(move || {
            let Some(ui) = weak.upgrade() else { return; };
            let pending_action = app.borrow().pending_action.clone();
            let result = app.borrow_mut().confirm_action();
            ui.set_delete_confirm_open(false);
            if result.is_ok() {
                if let Some(action) = pending_action.as_ref() {
                    discard_deleted_picture_edits(action, &mut widths.borrow_mut(),
                        &mut captions.borrow_mut());
                }
                update_all(&ui, &app.borrow());
            }
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
            state.selection_explicit = true;
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
                update_document_blocks(&ui, &state);
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
                update_document_blocks(&ui, &state);
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
                if result.is_ok() {
                    update_all(&ui, &app.borrow());
                    ui.set_pictures_expanded(true);
                }
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
                if result.is_ok() {
                    update_all(&ui, &app.borrow());
                    ui.set_pictures_expanded(true);
                }
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
        let widths = pending_picture_widths.clone();
        ui.on_resize_media(move |image_id, width| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let key = (app.borrow().workspace.active_id.clone(), image_id.to_string());
            let width = width.clamp(10, 100) as u8;
            widths.borrow_mut().insert(key, width);
            ui.set_save_status("Updating picture size…".into());
            let app = app.clone(); let weak = weak.clone();
            let widths = widths.clone();
            timer.start(slint::TimerMode::SingleShot, Duration::from_millis(180), move || {
                let Some(ui) = weak.upgrade() else { return; };
                let result = flush_pending_picture_edits(&mut app.borrow_mut(),
                    &mut widths.borrow_mut(), &mut HashMap::new());
                // Update existing rows without replacing the caption editor
                // while the user has a pending caption change.
                if result.as_ref().is_ok_and(|changed| *changed) {
                    sync_visible_picture_rows(&ui, &app.borrow());
                }
                set_status(&ui, result.map(|_| ()));
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
                if result.is_ok() {
                    update_media_rows(&ui, &app.borrow());
                    update_document_blocks(&ui, &app.borrow());
                }
                set_status(&ui, result);
            }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_place_media_at_caret(move |image_id| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let result = app.borrow_mut().place_media_at_caret(image_id.as_str());
            if result.is_ok() {
                update_media_rows(&ui, &app.borrow());
                update_document_blocks(&ui, &app.borrow());
            }
            set_status(&ui, result);
        });
    }
    let image_caption_timer = Rc::new(slint::Timer::default());
    {
        let app = app.clone(); let weak = ui.as_weak();
        let timer = image_caption_timer.clone();
        let captions = pending_picture_captions.clone();
        ui.on_caption_media(move |image_id, caption| {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let key = (app.borrow().workspace.active_id.clone(), image_id.to_string());
            if let Err(error) = queue_picture_caption(&mut captions.borrow_mut(),
                key.clone(), caption.as_str())
            {
                let last_accepted = captions.borrow().get(&key).cloned().or_else(|| {
                    app.borrow().active().and_then(|note| {
                        note.images.iter().find(|image| image.id == image_id.as_str())
                            .map(|image| image.caption.clone())
                    })
                });
                if let Some(previous) = last_accepted {
                    restore_rejected_picture_caption(&ui, image_id.as_str(), &previous);
                }
                ui.set_save_status(format!("Caption not saved: {error}. Extra text rejected.").into());
                return;
            }
            ui.set_save_status("Saving picture caption…".into());
            let app = app.clone(); let weak = weak.clone();
            let captions = captions.clone();
            timer.start(slint::TimerMode::SingleShot, Duration::from_millis(420), move || {
                let Some(ui) = weak.upgrade() else { return; };
                let result = flush_pending_picture_edits(&mut app.borrow_mut(),
                    &mut HashMap::new(), &mut captions.borrow_mut());
                if result.as_ref().is_ok_and(|changed| *changed) {
                    sync_visible_picture_rows(&ui, &app.borrow());
                }
                set_status(&ui, result.map(|_| ()));
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
        let widths = pending_picture_widths.clone();
        let captions = pending_picture_captions.clone();
        ui.on_export_page(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let result = (|| -> io::Result<()> {
                flush_pending_picture_edits(&mut app.borrow_mut(),
                    &mut widths.borrow_mut(), &mut captions.borrow_mut())?;
                app.borrow_mut().export_complete_page()
            })();
            set_status(&ui, result);
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
            else {
                update_rows(&ui, &app.borrow());
                ui.set_active_pinned(app.borrow().active().is_some_and(|note| note.pinned));
            }
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
                        update_drawing_ui(&ui, &state);
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
        ui.on_change_draw_tool(move |tool| {
            if app.borrow().pending_action.is_some() { return; }
            let name = tool.to_string();
            if !matches!(name.as_str(), "select"|"pen"|"eraser"|"rectangle"|"ellipse"|
                "line"|"triangle"|"rounded"|"curve"|"circle") { return; }
            let mut state = app.borrow_mut();
            state.draw_tool = name.clone();
            state.eraser = name == "eraser";
            state.drawing = false;
            if let Some(ui) = weak.upgrade() { update_drawing_ui(&ui, &state); }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_change_draw_fill_color(move |value| {
            let name = value.to_string();
            let valid = name.len() == 7 && name.starts_with('#') &&
                name[1..].bytes().all(|c| c.is_ascii_hexdigit());
            let Some(ui) = weak.upgrade() else { return; };
            if !valid {
                set_status(&ui, Err(io::Error::new(io::ErrorKind::InvalidInput,
                    "Fill colour must be #RRGGBB")));
                return;
            }
            let mut state = app.borrow_mut();
            state.draw_fill_color = name.clone();
            let filled = state.draw_filled;
            let rgba = parse_color(&name);
            let result = state.update_selected_object(|object| {
                object.style.fill = if filled { Some(DrawFill::Solid { color: rgba }) } else { None };
            });
            update_editor(&ui, &state);
            set_status(&ui, result.map(|_| ()));
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_change_draw_filled(move |filled| {
            let Some(ui) = weak.upgrade() else { return; };
            let mut state = app.borrow_mut();
            state.draw_filled = filled;
            let rgba = parse_color(&state.draw_fill_color);
            let result = state.update_selected_object(|object| {
                object.style.fill = if filled { Some(DrawFill::Solid { color: rgba }) } else { None };
            });
            update_drawing_ui(&ui, &state);
            if let Some(board) = state.board.as_ref() { ui.set_canvas_image(board_image(board)); }
            set_status(&ui, result.map(|_| ()));
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_change_draw_opacity(move |percent| {
            let Some(ui) = weak.upgrade() else { return; };
            let mut state = app.borrow_mut();
            state.draw_opacity = percent.clamp(10, 100) as u8;
            let opacity = state.draw_opacity as f32 / 100.0;
            let result = state.update_selected_object(|object| object.style.opacity = opacity);
            update_drawing_ui(&ui, &state);
            if let Some(board) = state.board.as_ref() { ui.set_canvas_image(board_image(board)); }
            set_status(&ui, result.map(|_| ()));
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_move_selected_draw_object(move |dx, dy| {
            let Some(ui) = weak.upgrade() else { return; };
            let mut state = app.borrow_mut();
            let Some(id) = state.selected_draw_object.clone() else { return; };
            let result = state.update_selected_object(|object| {
                if object.id == id {
                    object.transform.translation.x += dx as f32;
                    object.transform.translation.y += dy as f32;
                }
            });
            update_drawing_ui(&ui, &state);
            if let Some(board) = state.board.as_ref() { ui.set_canvas_image(board_image(board)); }
            set_status(&ui, result.map(|_| ()));
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_delete_selected_draw_object(move || {
            let Some(ui) = weak.upgrade() else { return; };
            let result = app.borrow_mut().delete_selected_object();
            update_all(&ui, &app.borrow());
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_duplicate_selected_draw_object(move || {
            let Some(ui) = weak.upgrade() else { return; };
            let result = app.borrow_mut().duplicate_selected_object();
            update_all(&ui, &app.borrow());
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_add_draw_layer(move || {
            let Some(ui) = weak.upgrade() else { return; };
            let result = app.borrow_mut().add_drawing_layer();
            update_all(&ui, &app.borrow());
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_select_draw_layer(move |id| {
            let id = id.to_string();
            let Some(ui) = weak.upgrade() else { return; };
            let mut state = app.borrow_mut();
            if state.active().is_some_and(|n| n.drawing.layers.iter().any(|l| l.id == id)) {
                state.selected_draw_layer_id = Some(id);
                update_drawing_ui(&ui, &state);
            }
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_rename_draw_layer(move |id, name| {
            let Some(ui) = weak.upgrade() else { return; };
            let result = app.borrow_mut().edit_drawing_layer(|drawing|
                drawing.rename_layer(&id, &name));
            update_all(&ui, &app.borrow());
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_set_draw_layer_visible(move |id, visible| {
            let Some(ui) = weak.upgrade() else { return; };
            let result = app.borrow_mut().edit_drawing_layer(|drawing|
                drawing.set_layer_visibility(&id, visible));
            update_all(&ui, &app.borrow());
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_set_draw_layer_locked(move |id, locked| {
            let Some(ui) = weak.upgrade() else { return; };
            let result = app.borrow_mut().edit_drawing_layer(|drawing|
                drawing.set_layer_locked(&id, locked));
            update_all(&ui, &app.borrow());
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_move_draw_layer(move |id, delta| {
            let Some(ui) = weak.upgrade() else { return; };
            let id = id.to_string();
            let result = app.borrow_mut().edit_drawing_layer(|drawing| {
                let old = drawing.layers.iter().position(|l| l.id == id)
                    .ok_or_else(|| "Drawing layer no longer exists".to_string())?;
                let next = (old as i32 + delta).clamp(0,
                    drawing.layers.len().saturating_sub(1) as i32) as usize;
                if next == old { return Ok(()); }
                drawing.move_layer(&id, next)
            });
            update_all(&ui, &app.borrow());
            set_status(&ui, result);
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_transform_selected_draw_object(move |command| {
            let Some(ui) = weak.upgrade() else { return; };
            let command = command.to_string();
            let mut state = app.borrow_mut();
            let result = state.update_selected_object(|object| match command.as_str() {
                "larger" => {
                    object.transform.scale_x *= 1.1;
                    object.transform.scale_y *= 1.1;
                }
                "smaller" => {
                    object.transform.scale_x *= 0.9;
                    object.transform.scale_y *= 0.9;
                }
                "rotate" => object.transform.rotation_deg += 15.0,
                "flip" => object.transform.flip_x = !object.transform.flip_x,
                _ => {},
            });
            update_drawing_ui(&ui, &state);
            if let Some(board) = state.board.as_ref() { ui.set_canvas_image(board_image(board)); }
            set_status(&ui, result.map(|_| ()));
        });
    }
    {
        let app = app.clone(); let weak = ui.as_weak();
        ui.on_redo_board(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let result = app.borrow_mut().redo_drawing();
            update_all(&ui, &app.borrow());
            set_status(&ui, result);
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
        let widths = pending_picture_widths.clone();
        let captions = pending_picture_captions.clone();
        ui.on_save_now(move || {
            let Some(ui) = weak.upgrade() else { return; };
            if app.borrow().pending_action.is_some() { return; }
            let mut state = app.borrow_mut();
            let selected_before = state.workspace.active_id.clone();
            let result = state.persist().and_then(|_| {
                flush_pending_picture_edits(&mut state, &mut widths.borrow_mut(),
                    &mut captions.borrow_mut()).map(|_| ())
            });
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
    let mut state = app.borrow_mut();
    state.persist()?;
    flush_pending_picture_edits(&mut state, &mut pending_picture_widths.borrow_mut(),
        &mut pending_picture_captions.borrow_mut())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn search_sidebar_preview_shows_deep_body_hit_not_first_line() {
        let mut note = Note::create("Travel journal");
        note.body = format!("First line with no useful context\n{}\nAfter a long gap, the\tNEEDLE\n appears here.",
            "unrelated details ".repeat(35));
        let subtitle = note_row_subtitle(&note, "needle");
        assert!(subtitle.contains("NEEDLE"), "search hit missing: {subtitle}");
        assert!(!subtitle.contains("First line"), "incorrect first-line preview: {subtitle}");
        assert!(subtitle.starts_with("… "), "deep match should show clipping: {subtitle}");
        assert!(!subtitle.contains('\n') && !subtitle.contains('\t'));
        assert!(!subtitle.contains("  "), "whitespace was not normalized: {subtitle}");
        assert!(subtitle.chars().count() <= 76);
    }

    #[test]
    fn search_sidebar_preview_preserves_unicode_and_safe_character_boundaries() {
        let mut note = Note::create("Unicode");
        note.body = format!("{}\n  Café 🦊 İstanbul\t follows", "猫🦊".repeat(45));
        let accented = note_row_subtitle(&note, "CAFÉ");
        assert!(accented.contains("Café 🦊"), "accents/emoji missing: {accented}");
        assert!(accented.starts_with("… "));
        assert!(accented.chars().count() <= 76);
        // Turkish uppercase dotted I expands to two lowercase scalars.
        // The position in the lowercased body cannot be used as a byte
        // offset into the original UTF-8 text.
        let dotted_i = note_row_subtitle(&note, "i\u{307}stanbul");
        assert!(dotted_i.contains("İstanbul"), "Unicode case expansion lost: {dotted_i}");
        assert!(!dotted_i.contains('\n') && !dotted_i.contains('\t'));
    }

    #[test]
    fn search_sidebar_empty_or_unmatched_query_keeps_picture_and_sketch_summaries() {
        let mut image_note = Note::create("Photos");
        image_note.body = "A first line\nThe hidden word".into();
        image_note.images.push(store::MediaBlock {
            id: "img".into(), filename: "img.png".into(),
            caption: String::new(), width_percent: 65, position: 0,
        });
        let original_photo_summary = "1 picture · A first line";
        assert_eq!(note_row_subtitle(&image_note, ""), original_photo_summary);
        assert_eq!(note_row_subtitle(&image_note, "   "), original_photo_summary);
        assert_eq!(note_row_subtitle(&image_note, "Photos"), original_photo_summary);
        assert!(note_row_subtitle(&image_note, "hidden").contains("hidden word"));

        let mut board = Note::create("Sketch");
        board.kind = "board".into();
        board.body = "First line\nSecret drawing label".into();
        assert_eq!(note_row_subtitle(&board, ""), "Sketch · 0 strokes · 0 shapes");
        assert_eq!(note_row_subtitle(&board, "absent"), "Sketch · 0 strokes · 0 shapes");
        assert!(note_row_subtitle(&board, "secret").contains("Secret drawing label"));
    }

    #[test]
    fn plain_text_input_reconciliation_uses_explicit_pending_caret_style() {
        let mut note = Note::create("note");
        note.body = "hi 🦊!".into();
        let mut typed = Some(TextStyle {
            bold: true, color: Some("#AABBCC".into()), size: Some(28),
            font_family: Some("Serif".into()),
        });
        assert!(apply_text_input_edit(&mut note, "hi 🦊X!".into(), typed.take(), Some(5)).unwrap());
        assert_eq!(note.body, "hi 🦊X!");
        assert_eq!(note.style_at(4).size, Some(28));
        assert!(note.style_at(4).bold);
        assert_eq!(note.style_at(4).font_family.as_deref(), Some("Serif"));
        assert!(note.style_at(3).is_default());
        // TextInput external updates identical to local state must not mutate it.
        let previous = note.rich_text.clone();
        let unchanged = note.body.clone();
        assert!(!apply_text_input_edit(&mut note, unchanged, None, Some(5)).unwrap());
        assert_eq!(note.rich_text, previous);
    }

    #[test]
    fn repeated_characters_use_actual_editor_caret_for_rich_text_splice() {
        let mut prepended = Note::create("note");
        prepended.body = "aaa".into();
        prepended.format_text_range(0, 1, TextStylePatch {
            bold: Some(true), ..Default::default()
        }).unwrap();
        apply_text_input_edit(&mut prepended, "aaaa".into(), Some(TextStyle::default()), Some(1)).unwrap();
        assert_eq!(prepended.body, "aaaa");
        assert!(!prepended.style_at(0).bold, "new leading 'a' must not inherit old formatting");
        assert!(prepended.style_at(1).bold, "bold old leading 'a' must shift right");

        let mut appended = Note::create("note");
        appended.body = "aaa".into();
        appended.format_text_range(2, 3, TextStylePatch {
            bold: Some(true), ..Default::default()
        }).unwrap();
        apply_text_input_edit(&mut appended, "aaaa".into(), Some(TextStyle::default()), Some(4)).unwrap();
        assert!(appended.style_at(2).bold, "appending must preserve original final 'a' style");
        assert!(!appended.style_at(3).bold);

        let mut deleted = Note::create("note");
        deleted.body = "aaaa".into();
        deleted.format_text_range(3, 4, TextStylePatch {
            bold: Some(true), ..Default::default()
        }).unwrap();
        apply_text_input_edit(&mut deleted, "aaa".into(), None, Some(0)).unwrap();
        assert_eq!(deleted.body, "aaa");
        assert!(deleted.style_at(2).bold, "backspace/delete at beginning should shift old end style");
    }

    #[test]
    fn picture_debounce_keeps_both_images_and_flushes_before_close() {
        use image::ImageEncoder;
        let folder = std::env::temp_dir().join(format!("inkpad-picture-debounce-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        let note_id = app.workspace.active_id.clone();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[10, 40, 60, 255], 1, 1, image::ExtendedColorType::Rgba8).unwrap();
        let first = app.storage.insert_image_bytes(&note_id, &png).unwrap();
        let second = app.storage.insert_image_bytes(&note_id, &png).unwrap();
        app.reload_after_media_change().unwrap();

        // Each successive text/slider event replaces its own picture key,
        // while preserving edits for every other photo in the same notebook.
        let mut widths = HashMap::new();
        let mut captions = HashMap::new();
        widths.insert((note_id.clone(), first.id.clone()), 30);
        widths.insert((note_id.clone(), second.id.clone()), 60);
        widths.insert((note_id.clone(), first.id.clone()), 45);
        captions.insert((note_id.clone(), first.id.clone()), "first draft".into());
        captions.insert((note_id.clone(), second.id.clone()), "second caption".into());
        captions.insert((note_id.clone(), first.id.clone()), "first final".into());
        assert!(flush_pending_picture_edits(&mut app, &mut widths, &mut captions).unwrap());
        assert!(widths.is_empty() && captions.is_empty());
        let stored = app.storage.read_latest().unwrap();
        let page = stored.notes.iter().find(|n| n.id == note_id).unwrap();
        let a = page.images.iter().find(|img| img.id == first.id).unwrap();
        let b = page.images.iter().find(|img| img.id == second.id).unwrap();
        assert_eq!((a.width_percent, a.caption.as_str()), (45, "first final"));
        assert_eq!((b.width_percent, b.caption.as_str()), (60, "second caption"));

        // A user can delete a picture while an edit debounce is outstanding.
        // Confirmed deletion discards only that picture's queued changes;
        // unrelated pictures must still save instead of hitting NotFound.
        widths.insert((note_id.clone(), first.id.clone()), 20);
        widths.insert((note_id.clone(), second.id.clone()), 80);
        captions.insert((note_id.clone(), first.id.clone()), "deleted caption".into());
        captions.insert((note_id.clone(), second.id.clone()), "kept caption".into());
        app.storage.delete_image(&note_id, &first.id).unwrap();
        app.reload_after_media_change().unwrap();
        discard_deleted_picture_edits(&PendingAction::DeletePicture {
            note_id: note_id.clone(), image_id: first.id.clone(),
        }, &mut widths, &mut captions);
        assert!(!widths.contains_key(&(note_id.clone(), first.id.clone())));
        assert!(!captions.contains_key(&(note_id.clone(), first.id.clone())));
        assert!(flush_pending_picture_edits(&mut app, &mut widths, &mut captions).unwrap());
        assert!(widths.is_empty() && captions.is_empty());
        let stored = app.storage.read_latest().unwrap();
        let remaining = &stored.notes.iter().find(|n| n.id == note_id).unwrap().images;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, second.id);
        assert_eq!((remaining[0].width_percent, remaining[0].caption.as_str()),
            (80, "kept caption"));

        // A rejected 501-character edit must not replace the valid queued
        // caption or poison later Save/export/close flushes. Count Unicode
        // scalar characters, matching Store::update_image validation.
        let key = (note_id.clone(), second.id.clone());
        let accepted = "🦆".repeat(500);
        assert!(queue_picture_caption(&mut captions, key.clone(), &accepted).is_ok());
        let rejected = format!("{accepted}!");
        assert!(queue_picture_caption(&mut captions, key.clone(), &rejected).is_err());
        assert_eq!(captions.get(&key), Some(&accepted));
        assert!(flush_pending_picture_edits(&mut app, &mut widths, &mut captions).unwrap());
        assert!(captions.is_empty());
        let stored = app.storage.read_latest().unwrap();
        let caption = &stored.notes.iter().find(|n| n.id == note_id).unwrap().images[0].caption;
        assert_eq!(caption, &accepted);

        assert!(queue_picture_caption(&mut captions, key.clone(), &rejected).is_err());
        assert!(captions.is_empty(), "a new overlong caption must not enter the save queue");
        assert!(!flush_pending_picture_edits(&mut app, &mut widths, &mut captions).unwrap());
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn html_export_uses_unsaved_current_note_and_does_not_override_remote_conflicts() {
        let folder = std::env::temp_dir().join(format!("inkpad-html-fresh-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        let note_id = app.workspace.active_id.clone();
        app.active_mut().unwrap().body = "Unflushed <fresh> Unicode 🦊".into();
        app.active_mut().unwrap().title = "New local title".into();
        let target = folder.join("fresh.html");
        app.export_complete_page_to(&note_id, &target).unwrap();
        let html = fs::read_to_string(&target).unwrap();
        assert!(html.contains("Unflushed &lt;fresh&gt; Unicode 🦊"));
        assert!(html.contains("New local title"));
        let remote = Store::in_folder(folder.clone()).unwrap();
        remote.mutate(|ws| {
            ws.notes.iter_mut().find(|n| n.id == note_id).unwrap().body = "remote edit".into();
            Ok(())
        }).unwrap();
        app.active_mut().unwrap().body = "conflicting local edit".into();
        let rejected = app.export_complete_page_to(&note_id, &target).unwrap_err();
        assert_eq!(rejected.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::read_to_string(&target).unwrap(), html,
            "failed export must not overwrite previous valid exported copy");
        fs::remove_dir_all(folder).unwrap();
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

    #[test]
    fn editable_shape_gesture_select_move_duplicate_undo_redo_and_restart() {
        let folder = std::env::temp_dir().join(format!("inkpad-vector-gui-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        app.draw_tool = "rectangle".into();
        assert!(app.draw(100, 100, 0).unwrap());
        assert!(app.draw(250, 190, 1).unwrap());
        assert!(app.draw(250, 190, 2).unwrap());
        let id = app.active().unwrap().drawing.objects[0].id.clone();
        assert_eq!(app.active().unwrap().drawing.layers.len(), 1);
        assert_eq!(app.active().unwrap().drawing.objects.len(), 1);
        let original_position = app.active().unwrap().drawing.object(&id).unwrap().transform.translation;

        app.draw_tool = "select".into();
        assert!(app.draw(150, 150, 0).unwrap());
        assert_eq!(app.selected_draw_object.as_deref(), Some(id.as_str()));
        assert!(app.draw(170, 160, 1).unwrap());
        assert!(app.draw(170, 160, 2).unwrap());
        let moved = app.active().unwrap().drawing.object(&id).unwrap().transform.translation;
        assert_eq!(moved.x, original_position.x + 20.0);
        assert_eq!(moved.y, original_position.y + 10.0);

        app.duplicate_selected_object().unwrap();
        assert_eq!(app.active().unwrap().drawing.objects.len(), 2);
        app.undo().unwrap();
        assert_eq!(app.active().unwrap().drawing.objects.len(), 1);
        app.redo_drawing().unwrap();
        assert_eq!(app.active().unwrap().drawing.objects.len(), 2);
        drop(app);
        let reopened = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        assert_eq!(reopened.active().unwrap().drawing.objects.len(), 2);
        assert_eq!(reopened.active().unwrap().drawing.object(&id).unwrap().transform.translation, moved);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn gui_agent_three_way_merge_keeps_drawing_and_text_but_rejects_same_field_conflicts() {
        let base = Note::create("note");
        let mut text_change = base.clone();
        text_change.set_body_preserving_format("Writing while agent draws".into()).unwrap();
        let mut vector_change = base.clone();
        vector_change.drawing.add_object(DrawShape::Circle { radius: 22.0 }, None).unwrap();
        let merged = merge_note(&base, &text_change, &vector_change).unwrap();
        assert_eq!(merged.body, "Writing while agent draws");
        assert_eq!(merged.drawing.objects.len(), 1);
        let mut second_vector_change = base.clone();
        second_vector_change.drawing.add_object(DrawShape::Rectangle { width: 33.0, height: 44.0 }, None).unwrap();
        assert_eq!(merge_note(&base, &second_vector_change, &vector_change)
            .unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn drawing_undo_redo_respects_pen_shape_pen_sequence() {
        let folder = std::env::temp_dir().join(format!("inkpad-undo-mixed-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        app.draw(5, 5, 0).unwrap();
        app.draw(20, 20, 2).unwrap();
        app.draw_tool = "rectangle".into();
        app.draw(100, 100, 0).unwrap();
        app.draw(160, 150, 2).unwrap();
        app.draw_tool = "pen".into();
        app.draw(40, 40, 0).unwrap();
        app.draw(65, 65, 2).unwrap();
        assert_eq!(app.active().unwrap().strokes.len(), 2);
        assert_eq!(app.active().unwrap().drawing.objects.len(), 1);
        app.undo().unwrap(); // Latest pen stroke only
        assert_eq!(app.active().unwrap().strokes.len(), 1);
        assert_eq!(app.active().unwrap().drawing.objects.len(), 1);
        app.undo().unwrap(); // Shape
        assert_eq!(app.active().unwrap().strokes.len(), 1);
        assert!(app.active().unwrap().drawing.objects.is_empty());
        app.redo_drawing().unwrap(); // Shape restored
        app.redo_drawing().unwrap(); // Pen restored
        assert_eq!(app.active().unwrap().strokes.len(), 2);
        assert_eq!(app.active().unwrap().drawing.objects.len(), 1);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn pointer_cancel_reverts_unfinished_shape_and_stroke_without_saving() {
        let folder = std::env::temp_dir().join(format!("inkpad-pointer-cancel-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        let initial = app.storage.read_latest().unwrap();
        app.draw_tool = "rectangle".into();
        app.draw(200, 200, 0).unwrap();
        app.draw(300, 260, 1).unwrap();
        app.draw(300, 260, 3).unwrap();
        assert!(app.active().unwrap().drawing.objects.is_empty());
        assert!(app.drawing_undo_order.is_empty());
        app.draw_tool = "pen".into();
        app.draw(20, 30, 0).unwrap();
        app.draw(50, 60, 1).unwrap();
        app.draw(50, 60, 3).unwrap();
        assert!(app.active().unwrap().strokes.is_empty());
        assert_eq!(app.storage.read_latest().unwrap().notes, initial.notes);
        app.undo().unwrap();
        assert!(app.active().unwrap().strokes.is_empty());
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn reopening_legacy_note_retains_undo_last_stroke_without_untracked_deletes() {
        let folder = std::env::temp_dir().join(format!("inkpad-legacy-drawing-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        app.draw(10, 15, 0).unwrap();
        app.draw(50, 65, 2).unwrap();
        drop(app);
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        assert_eq!(app.drawing_undo_order.len(), 1);
        app.undo().unwrap();
        assert!(app.active().unwrap().strokes.is_empty());
        assert!(app.drawing_undo_order.is_empty());
        app.undo().unwrap(); // No further history: must remain a no-op.
        assert!(app.active().unwrap().strokes.is_empty());
        app.redo_drawing().unwrap();
        assert_eq!(app.active().unwrap().strokes.len(), 1);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn inserted_photo_uses_explicit_unicode_caret_and_survives_reopen_export() {
        use image::ImageEncoder;
        let folder = std::env::temp_dir().join(format!("inkpad-inline-writing-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        let body = "BEFORE 🚀\nAFTER 猫".to_string();
        app.active_mut().unwrap().set_body_preserving_format(body.clone()).unwrap();
        app.persist().unwrap();
        let boundary = "BEFORE 🚀\n".chars().count();
        app.selection = TextSelection::caret(boundary);
        app.selection_explicit = true;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[12, 56, 90, 255], 1, 1, image::ExtendedColorType::Rgba8).unwrap();
        app.import_image_bytes(&png).unwrap();
        let note = app.active().unwrap();
        assert_eq!(note.body, body, "placing an image must never insert placeholder text");
        assert_eq!(note.images.len(), 1);
        assert_eq!(note.image_anchor(&note.images[0].id).unwrap(), Some(boundary));
        let id = note.id.clone();
        drop(app);

        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        app.select(&id).unwrap();
        assert_eq!(app.active().unwrap().inline_media[0].char_offset, boundary);
        let html = folder.join("inline-order.html");
        app.export_complete_page_to(&id, &html).unwrap();
        let rendered = fs::read_to_string(&html).unwrap();
        let before = rendered.find("BEFORE").unwrap();
        let image = rendered.find("data:image/png;base64,").unwrap();
        let after = rendered.find("AFTER").unwrap();
        assert!(before < image && image < after, "export should reflect picture's text caret");
        let image_id = app.active().unwrap().images[0].id.clone();
        let no_cursor = app.place_media_at_caret(&image_id).unwrap_err();
        assert_eq!(no_cursor.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(app.active().unwrap().inline_media[0].char_offset, boundary);
        app.selection = TextSelection::caret(0);
        app.selection_explicit = true;
        app.place_media_at_caret(&image_id).unwrap();
        assert_eq!(app.active().unwrap().image_anchor(&image_id).unwrap(), Some(0));
        let moved_html = folder.join("moved-inline-order.html");
        app.export_complete_page_to(&id, &moved_html).unwrap();
        let moved = fs::read_to_string(moved_html).unwrap();
        assert!(moved.find("data:image/png;base64,").unwrap() < moved.find("BEFORE").unwrap());
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn photo_insertion_defaults_to_end_and_text_undo_rebases_anchor() {
        use image::ImageEncoder;
        let folder = std::env::temp_dir().join(format!("inkpad-inline-undo-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        app.active_mut().unwrap().set_body_preserving_format("猫 🚀".into()).unwrap();
        app.persist().unwrap();
        assert_eq!(app.image_insertion_point().unwrap().1, 3);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[12, 34, 56, 255], 1, 1, image::ExtendedColorType::Rgba8).unwrap();
        app.import_image_bytes(&png).unwrap();
        assert_eq!(app.active().unwrap().inline_media[0].char_offset, 3);
        let before = app.active().unwrap().clone();
        app.text_history.record(&before, EditKind::Typing);
        app.active_mut().unwrap().replace_text_range(0, 0, "Hi ").unwrap();
        let after = app.active().unwrap().clone();
        app.text_history.commit_after(&after);
        assert_eq!(after.inline_media[0].char_offset, 6);
        let App { workspace, text_history, .. } = &mut app;
        let note = workspace.notes.iter_mut().find(|n| n.id == workspace.active_id).unwrap();
        assert!(text_history.undo_before(note).unwrap());
        assert_eq!(note.inline_media[0].char_offset, 3);
        assert!(text_history.redo(note).unwrap());
        assert_eq!(note.inline_media[0].char_offset, 6);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn concurrent_note_text_and_picture_placement_rebase_without_duplication() {
        let mut base = Note::create("note");
        base.body = "Start 🦊 End".into();
        base.images.push(store::MediaBlock {
            id: "photoA".into(), filename: "photoA.png".into(),
            caption: "Test".into(), width_percent: 65, position: 0,
        });
        let mut local = base.clone();
        local.replace_text_range(0, 0, "Intro ").unwrap();
        let mut remote = base.clone();
        remote.set_image_anchor("photoA", Some(6)).unwrap();
        let merged = merge_note(&base, &local, &remote).unwrap();
        assert_eq!(merged.body, "Intro Start 🦊 End");
        assert_eq!(merged.images.len(), 1);
        assert_eq!(merged.image_anchor("photoA").unwrap(), Some(12));
        assert_eq!(merged.content_blocks().unwrap().len(), 3);
        let mut other = remote.clone();
        other.set_image_anchor("photoA", Some(3)).unwrap();
        assert!(merge_note(&base, &remote, &other).is_err(),
            "two concurrent edits to picture placement must not silently win");
    }

    #[test]
    fn gui_layer_selection_controls_shape_assignment_lock_and_visibility() {
        let folder = std::env::temp_dir().join(format!("inkpad-layer-gui-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let mut app = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        app.add_drawing_layer().unwrap();
        let layer = app.selected_draw_layer_id.clone().unwrap();
        app.draw_tool = "circle".into();
        app.draw(100, 100, 0).unwrap();
        app.draw(160, 160, 2).unwrap();
        assert_eq!(app.active().unwrap().drawing.objects[0].layer_id, layer);
        app.edit_drawing_layer(|d| d.rename_layer(&layer, "Duck wings")).unwrap();
        app.edit_drawing_layer(|d| d.set_layer_visibility(&layer, false)).unwrap();
        assert_eq!(app.active().unwrap().drawing.layers[0].name, "Duck wings");
        assert!(!app.active().unwrap().drawing.layers[0].visible);
        app.edit_drawing_layer(|d| d.set_layer_locked(&layer, true)).unwrap();
        let before = app.storage.read_latest().unwrap();
        assert!(app.draw(200, 200, 0).is_err(), "A locked layer must reject new shapes");
        assert_eq!(app.storage.read_latest().unwrap().notes, before.notes);
        app.edit_drawing_layer(|d| d.set_layer_locked(&layer, false)).unwrap();
        app.edit_drawing_layer(|d| d.set_layer_visibility(&layer, true)).unwrap();
        drop(app);
        let reopened = App::load(Store::in_folder(folder.clone()).unwrap()).unwrap();
        assert_eq!(reopened.active().unwrap().drawing.objects.len(), 1);
        assert!(reopened.active().unwrap().drawing.layers[0].visible);
        assert_eq!(reopened.active().unwrap().drawing.layers[0].name, "Duck wings");
        fs::remove_dir_all(folder).unwrap();
    }
}
