#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod canvas;
mod instance;
mod settings;
mod store;
#[cfg(windows)]
mod windows_chrome;

use canvas::{parse_color, Board, HEIGHT, WIDTH};
use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use std::cell::RefCell;
use std::fs;
use std::io;
use std::rc::Rc;
use std::time::{Duration, SystemTime};
use settings::UiSettings;
use store::{Note, Store, Stroke, Workspace};

slint::include_modules!();

struct App {
    storage: Store,
    workspace: Workspace,
    baseline: Workspace,
    board: Option<Board>,
    drawing: bool,
    edit_mode: String,
    last_seen_file: Option<(SystemTime, u64)>,
    filter: String,
    query: String,
    brush_size: u16,
    brush_color: String,
    eraser: bool,
    ui_settings: UiSettings,
    pending_action: Option<PendingAction>,
}

#[derive(Debug, Clone)]
enum PendingAction {
    Delete { id: String, title: String },
    ClearDrawing { id: String, title: String },
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
        }
    }
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
            storage, baseline: workspace.clone(), workspace, board: None, drawing: false, edit_mode,
            filter: "all".into(), query: String::new(), brush_size: 6,
            brush_color: "#263238".into(), eraser: false, ui_settings,
            pending_action: None,
        };
        app.reload_board();
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
                        if latest.notes[pos] != *former {
                            return Err(io::Error::new(io::ErrorKind::WouldBlock,
                                format!("An agent also edited '{}'. Your unsaved changes remain in the window; resolve this conflict before closing.", note.title)));
                        }
                    } else {
                        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Note ID conflict"));
                    }
                    latest.notes[pos] = note.clone();
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
        let next = !self.ui_settings.sidebar_collapsed;
        let changed = UiSettings { sidebar_collapsed: next };
        changed.save(self.storage.data_path())?;
        self.ui_settings = changed;
        Ok(next)
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
        self.reload_board();
        self.persist()
    }

    fn select(&mut self, id: &str) -> io::Result<()> {
        if self.workspace.notes.iter().any(|note| note.id == id) {
            self.workspace.active_id = id.into();
            self.edit_mode = if self.active().is_some_and(|n| n.kind == "board") { "draw" } else { "text" }.into();
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
}

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
        subtitle: if note.kind == "board" {
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

fn update_editor(ui: &MainWindow, app: &App) {
    if let Some(note) = app.active() {
        ui.set_has_note(true);
        ui.set_note_title(note.title.clone().into());
        ui.set_note_body(note.body.clone().into());
        ui.set_font_size(note.font_size);
        ui.set_font_choice(note.font_choice.clone().into());
        ui.set_active_kind(note.kind.clone().into());
    } else {
        ui.set_has_note(false);
        ui.set_note_title(SharedString::default());
        ui.set_note_body(SharedString::default());
        ui.set_active_kind("none".into());
    }
    ui.set_edit_mode(app.edit_mode.clone().into());
    if let Some(board) = app.board.as_ref() { ui.set_canvas_image(board_image(board)); }
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
                    ui.set_font_size(note.font_size);
                    ui.set_font_choice(note.font_choice.clone().into());
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
        if let Some(note) = app.active_mut() {
            note.body = text.to_string(); note.touch();
        }
        Ok(())
    });
    bind_change!(on_change_font_size, |app: &mut App, size: i32| {
        if let Some(note) = app.active_mut() { note.font_size = size.clamp(12, 40); note.touch(); }
        Ok(())
    });
    bind_change!(on_change_font_choice, |app: &mut App, choice: SharedString| {
        if let Some(note) = app.active_mut() {
            let choice = choice.to_string();
            if ["Sans", "Serif", "Mono"].contains(&choice.as_str()) { note.font_choice = choice; note.touch(); }
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
