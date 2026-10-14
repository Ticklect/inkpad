# Inkpad Windows icon and frameless window notes

`inkpad.ico` is the native Windows executable icon and contains independent PNG-backed image entries at **16, 24, 32, 48, 64 and 256 pixels**. `inkpad-icon-preview.png` is the original high-resolution artwork used by Slint's `MainWindow.icon` binding. Both use the same golden notepad and diagonal pencil identity as the Inkpad sidebar.

On Windows, `build.rs` verifies the ICO sizes and PNG payloads, compiles `inkpad.rc` using the installed Windows SDK `rc.exe`, and links the resulting `.res` file into **inkpad.exe only**. No additional Cargo dependency or extra runtime file is necessary. The headless `inkpad-agent.exe` remains a Console-subsystem binary so MCP stdin/stdout works correctly.

The root Slint window can use:

```slint
icon: @image-url("../assets/inkpad-icon-preview.png");
no-frame: true;
resize-border-width: 6px;
```

In Slint 1.18's winit backend, an undecorated, resizable window's `resize-border-width` is passed to native winit cursor and resize handlers. The backend invokes `drag_resize_window` at the window's eight edges and corners. A `WindowMoveArea` provides native window movement over its draggable header region. Custom close, minimize and maximize buttons can use `slint::Window::hide()`, `set_minimized(true)` and `set_maximized(!is_maximized())` respectively, with the application's existing shutdown/save behavior respected.

**Borderless-window tradeoffs:** Test resizing at 100%, 125%, 150% and 200% scaling, keyboard focus on custom window controls, maximize restoration, screen-edge snap, and fullscreen. Native system menus and Windows 11 snap-layout hover affordances may differ from standard titlebars. Avoid Windows `WM_NCHITTEST` subclasses unless a demonstrated issue remains; the Slint backend already implements resize handling.

Validation on Windows (14 October 2026): both release executables built offline and 31 tests passed. Windows extracted a branded icon from `inkpad.exe`. A separate isolated GUI process returned a nonzero branded `WM_GETICON` result for `ICON_SMALL` and `ICON_SMALL2`; the sample color matched the source artwork. A visual Windows taskbar screenshot remains a separate manual check.
