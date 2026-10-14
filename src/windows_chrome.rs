//! Native Win32 drag for the frameless Inkpad titlebar (Slint 1.18 / winit 0.30).
//!
//! `WindowMoveArea` alone is unreliable with a frameless maximized window on
//! Windows, and can also fail to initiate normal dragging on some machines.
//! This module installs one public winit event filter, rather than subclassing
//! the HWND or synthesizing raw mouse events. The *system* owns the subsequent
//! drag/snap interaction. Presses without sufficient movement never restore.
//!
//! Host wiring, after creating `MainWindow` (on the Slint UI thread):
//! ```ignore
//! #[cfg(windows)]
//! windows_chrome::install(ui.window(), {
//!     let weak = ui.as_weak();
//!     move || weak.upgrade().is_some_and(|ui| !ui.get_delete_confirm_open())
//! });
//! ```
//! Keep the native `WindowMoveArea` for platforms other than Windows. The
//! current layout uses a 40 logical-pixel header, 6px resize border, and three
//! 44px chrome buttons on the right. This filter respects those hit regions.

#![cfg(target_os = "windows")]

use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
use std::cell::RefCell;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, GetWindowRect, IsZoomed, SetWindowPos,
    ShowWindow, SM_CXDRAG, SM_CYDRAG, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE,
};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::keyboard::Key;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

const HEADER_HEIGHT: f64 = 40.0;
const RESIZE_EDGE: f64 = 6.0;
const RIGHT_CHROME_BUTTONS: f64 = 3.0 * 44.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScreenPoint { x: i32, y: i32 }

#[derive(Clone, Copy, Debug)]
struct Pressed {
    origin: ScreenPoint,
    /// Fraction of the maximized client width where the pointer was pressed.
    horizontal_fraction: f64,
    /// Keep the pointer at approximately the same titlebar height on restore.
    titlebar_y: i32,
    started_maximized: bool,
}

#[derive(Default)]
struct Tracker { pressed: Option<Pressed>, ctrl_down: bool }

/// Installs the Windows-only titlebar drag fix into Slint's *existing* winit
/// window. `can_drag` should return false while a destructive modal is open.
/// Call once, after constructing the window; never install multiple event
/// filters on the same Slint window because later registrations replace prior
/// `on_winit_window_event` callbacks.
pub fn install(
    window: &slint::Window,
    can_drag: impl Fn() -> bool + 'static,
    on_dropped_file: impl Fn(std::path::PathBuf) + 'static,
    on_image_paste: impl Fn() -> bool + 'static,
) {
    let state = RefCell::new(Tracker::default());
    window.on_winit_window_event(move |slint_window, event| {
        let Some(action) = slint_window.with_winit_window(|native| {
            let Some(hwnd) = native_hwnd(native) else { return EventResult::Propagate; };
            let mut tracker = state.borrow_mut();
            if !(can_drag)() {
                tracker.pressed = None;
                return EventResult::Propagate;
            }
            let scale = native.scale_factor();
            match event {
                WindowEvent::ModifiersChanged(modifiers) => {
                    tracker.ctrl_down = modifiers.state().control_key();
                    EventResult::Propagate
                }
                WindowEvent::DroppedFile(path) => {
                    // Native Windows Explorer file drop, no manual desktop
                    // automation. The caller handles type/size validation.
                    on_dropped_file(path.clone());
                    EventResult::PreventDefault
                }
                WindowEvent::KeyboardInput { event, .. }
                    if tracker.ctrl_down
                        && event.state == ElementState::Pressed
                        && !event.repeat
                        && matches!(&event.logical_key, Key::Character(s) if s.eq_ignore_ascii_case("v")) =>
                {
                    // An actual picture overrides Ctrl+V text paste; plain
                    // clipboard text propagates normally to TextInput.
                    if on_image_paste() { EventResult::PreventDefault }
                    else { EventResult::Propagate }
                }
                WindowEvent::MouseInput { state: ElementState::Pressed,
                    button: MouseButton::Left, .. } => {
                    let Some(screen) = cursor_position() else { return EventResult::Propagate; };
                    let mut client = POINT { x: screen.x, y: screen.y };
                    if unsafe { ScreenToClient(hwnd, &mut client) } == 0 {
                        return EventResult::Propagate;
                    }
                    let size = native.inner_size();
                    if titlebar_hit(client.x, client.y, size.width, scale) {
                        tracker.pressed = Some(Pressed {
                            origin: screen,
                            horizontal_fraction: (client.x as f64 / size.width.max(1) as f64).clamp(0.0, 1.0),
                            titlebar_y: client.y,
                            started_maximized: unsafe { IsZoomed(hwnd) } != 0,
                        });
                        // Do not forward the press to WindowMoveArea; that
                        // path could race our system-level drag operation.
                        return EventResult::PreventDefault;
                    }
                    tracker.pressed = None;
                    EventResult::Propagate
                }
                WindowEvent::MouseInput { state: ElementState::Released,
                    button: MouseButton::Left, .. } => {
                    if tracker.pressed.take().is_some() { EventResult::PreventDefault }
                    else { EventResult::Propagate }
                }
                WindowEvent::CursorMoved { .. } => {
                    let Some(pressed) = tracker.pressed else { return EventResult::Propagate; };
                    let Some(now) = cursor_position() else { return EventResult::PreventDefault; };
                    if !past_drag_threshold(pressed.origin, now,
                        unsafe { GetSystemMetrics(SM_CXDRAG) }, unsafe { GetSystemMetrics(SM_CYDRAG) }) {
                        return EventResult::PreventDefault;
                    }
                    // Clear before calling into Windows: restoring the window
                    // may synchronously dispatch additional window events.
                    tracker.pressed = None;
                    if pressed.started_maximized && unsafe { IsZoomed(hwnd) } != 0 {
                        let _ = restore_under_cursor(hwnd, pressed, now);
                    }
                    // Winit sends ReleaseCapture + WM_NCLBUTTONDOWN(HTCAPTION)
                    // through native Win32, preserving Aero Snap and monitors.
                    let _ = native.drag_window();
                    EventResult::PreventDefault
                }
                WindowEvent::Focused(false) | WindowEvent::CursorLeft { .. } => {
                    tracker.pressed = None;
                    EventResult::Propagate
                }
                _ => EventResult::Propagate,
            }
        }) else { return EventResult::Propagate; };
        action
    });
}

fn native_hwnd(window: &winit::window::Window) -> Option<HWND> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get() as HWND),
        _ => None,
    }
}

fn cursor_position() -> Option<ScreenPoint> {
    let mut point = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut point) } != 0 {
        Some(ScreenPoint { x: point.x, y: point.y })
    } else { None }
}

/// Explicitly excludes resize edges and all three control buttons.
fn titlebar_hit(x: i32, y: i32, physical_width: u32, scale: f64) -> bool {
    if !scale.is_finite() || scale <= 0.0 { return false; }
    let edge = (RESIZE_EDGE * scale).ceil();
    let max_x = physical_width as f64 - (RIGHT_CHROME_BUTTONS + RESIZE_EDGE) * scale;
    let xx = x as f64;
    let yy = y as f64;
    xx >= edge && xx < max_x && yy >= edge && yy < HEADER_HEIGHT * scale
}

/// Honour system drag tolerance so a titlebar click never restores/maximizes.
fn past_drag_threshold(from: ScreenPoint, to: ScreenPoint, cx: i32, cy: i32) -> bool {
    let horizontal = (i64::from(to.x) - i64::from(from.x)).abs();
    let vertical = (i64::from(to.y) - i64::from(from.y)).abs();
    horizontal >= i64::from(cx.max(4)) || vertical >= i64::from(cy.max(4))
}

/// Computes the restored window's upper-left corner. The cursor stays at its
/// original fractional position across the restored titlebar (browser style),
/// not at the left edge or the old maximized window's origin.
fn restored_origin(cursor: ScreenPoint, restored_width: i32,
    fraction: f64, titlebar_y: i32) -> ScreenPoint {
    let width = restored_width.max(1) as f64;
    ScreenPoint {
        x: cursor.x.saturating_sub((fraction.clamp(0.0, 1.0) * width).round() as i32),
        y: cursor.y.saturating_sub(titlebar_y.max(0)),
    }
}

/// Restore and reposition synchronously before starting winit's system drag.
/// Win32 workspaces support negative screen coordinates on left/top monitors.
/// Failure leaves the previous window intact and lets winit attempt dragging.
fn restore_under_cursor(hwnd: HWND, press: Pressed, cursor: ScreenPoint) -> bool {
    unsafe {
        ShowWindow(hwnd, SW_RESTORE);
        let mut restored = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if GetWindowRect(hwnd, &mut restored) == 0 { return false; }
        let width = restored.right.saturating_sub(restored.left);
        if width < 100 { return false; }
        let position = restored_origin(cursor, width, press.horizontal_fraction, press.titlebar_y);
        SetWindowPos(hwnd, std::ptr::null_mut(), position.x, position.y, 0, 0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_restore_before_system_drag_threshold() {
        let start = ScreenPoint { x: 1900, y: 24 };
        for delta in [ScreenPoint { x: 1901, y: 25 }, ScreenPoint { x: 1903, y: 27 }] {
            assert!(!past_drag_threshold(start, delta, 4, 4));
        }
        assert!(past_drag_threshold(start, ScreenPoint { x: 1904, y: 24 }, 4, 4));
        assert!(past_drag_threshold(start, ScreenPoint { x: 1900, y: 20 }, 4, 4));
        assert!(past_drag_threshold(start, ScreenPoint { x: 1905, y: 24 }, 0, 0));
    }

    #[test]
    fn buttons_and_resize_edges_never_trigger_drag() {
        assert!(titlebar_hit(300, 18, 1920, 1.0));
        assert!(!titlebar_hit(300, 4, 1920, 1.0));
        assert!(!titlebar_hit(5, 20, 1920, 1.0));
        assert!(!titlebar_hit(1810, 20, 1920, 1.0));
        assert!(!titlebar_hit(300, 40, 1920, 1.0));
        assert!(titlebar_hit(375, 25, 2400, 1.25));
        assert!(!titlebar_hit(2300, 25, 2400, 1.25));
    }

    #[test]
    fn restored_window_anchors_under_cursor_on_multiple_monitors_and_dpi() {
        assert_eq!(restored_origin(ScreenPoint { x: 1440, y: 85 }, 1100, 0.75, 24),
            ScreenPoint { x: 615, y: 61 });
        assert_eq!(restored_origin(ScreenPoint { x: -620, y: 380 }, 1330, 0.5, 30),
            ScreenPoint { x: -1285, y: 350 });
        assert_eq!(restored_origin(ScreenPoint { x: 2700, y: 120 }, 1650, 0.65, 50),
            ScreenPoint { x: 1627, y: 70 });
    }
}
