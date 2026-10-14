# Inkpad Beta

A lightweight, local-first Windows notebook and drawing board made with **Rust + Slint**. A calm macOS-inspired interface, without Electron, accounts, or internet requirements. **Beta software — not a 2.0 or stable release.**

## Features

- **Collapsible vertical sidebar:** browse pages, search, pin, and delete. Use the header collapse/expand control or **Ctrl+B**; Inkpad remembers the choice in `ui-settings.json` separately from saved notes.
- **Mac-inspired custom chrome:** frameless unified titlebar with window drag, native edge resizing and minimize/maximize/close controls. Inkpad includes a branded, multi-resolution executable/window/taskbar icon.
- **In-app confirmation:** deleting pages or clearing drawings uses the app's own confirmation sheet rather than a Windows Yes/No message box. Destructive actions still require confirmation.
- **Text + drawing in one page:** select **Text** or **Draw** to edit either medium on the same page. Existing old drawing boards are preserved and can also contain text.
- **Text:** multiline writing with explicit readable dark text, per-page font size (12–40), and Sans / Serif / Mono font styles.
- **Drawing:** 1100 × 700 canvas with freehand strokes, brush colours, eraser, brush size, undo and clear (with confirmation). Strokes remain editable after restart.
- **Automatic local saves:** `%LOCALAPPDATA%\Inkpad\workspace.json` stores all text and strokes; `workspace.json.bak` keeps a previous version. Typing saves after a short pause and again on close.
- **Rapid background agent checks:** `inkpad-agent.exe` provides headless MCP tools and a JSON CLI for listing, searching, reading, creating and updating notes. Persistent stdio MCP avoids repeated process startup; it never needs a desktop window; see [AGENTS.md](AGENTS.md).
- **Safe multi-process edits:** Windows file locks, merging of unrelated changes, periodic desktop refresh and explicit conflict detection protect against overwriting agent or human work.
- **Export:** export the current Text view as `.txt`, or the Draw view as `.png`.
- **Accessible controls:** keyboard-focusable buttons, labelled sidebar pages, Esc to dismiss confirmation, and input controls.

## Run / build on Windows

Install [Rust MSVC toolchain](https://rustup.rs/), MSVC Build Tools, and the Windows SDK resource compiler (`rc.exe`) for the embedded icon. Then in this folder:

```powershell
cargo run --bin inkpad
cargo test
cargo build --release
```

The finished desktop executable is `target\release\inkpad.exe`. The optional MCP/CLI executable is `target\release\inkpad-agent.exe`. They use the same notes folder, even when only the agent is running. Set `INKPAD_DATA_DIR` to change this folder consistently for both processes. The portable build lives at `dist\Inkpad-Beta-Pending-Drag-Test\Inkpad.exe`.

## Data and limitations

The app keeps data entirely on your computer. Back up the `%LOCALAPPDATA%\Inkpad` folder to protect against disk loss. UI preferences are stored separately from `workspace.json` so they cannot inadvertently overwrite note changes. Notes have page-level font choices but **do not yet support rich text per selection**, attachments, syncing between computers or infinite drawing canvas. The drawing editor uses a fixed 1100 × 700 canvas and exports PNG at that resolution.

This is a beta. The first launch creates a welcome page which you can edit or delete. Older notes and sketch-only pages continue to load; their text and strokes are not erased by the migration.
