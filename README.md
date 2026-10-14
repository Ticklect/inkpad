# Inkpad Beta

A lightweight, local-first Windows notebook and drawing board made with **Rust + Slint**. A calm macOS-inspired interface, without Electron, accounts, or internet requirements. **Beta software — not a 2.0 or stable release.**

## Features

- **Collapsible vertical sidebar:** browse pages, search, pin, and delete. Use the header collapse/expand control or **Ctrl+B**; Inkpad remembers the choice in `ui-settings.json` separately from saved notes.
- **Mac-inspired custom chrome:** frameless unified titlebar with window drag, native edge resizing and minimize/maximize/close controls. Inkpad includes a branded, multi-resolution executable/window/taskbar icon.
- **In-app confirmation:** deleting pages or clearing drawings uses the app's own confirmation sheet rather than a Windows Yes/No message box. Destructive actions still require confirmation.
- **Text + drawing in one page:** select **Text** or **Draw** to edit either medium on the same page. Existing old drawing boards are preserved and can also contain text.
- **Pictures in a page:** paste an image with **Ctrl+V**, use **Paste image** or **Add picture**, or drag a PNG/JPEG/GIF/WebP file from Explorer onto Inkpad. Imported pictures stay with their page after restart.
- **Picture sizing and layout:** picture blocks have 25%/50%/75%/100% presets, a **continuous 10–100% width slider**, responsive vertical space, captions, move up/down and confirmed removal. Previews preserve image proportions.
- **Text:** multiline writing with explicit readable dark text, per-page font size (12–40), and Sans / Serif / Mono font styles.
- **Drawing:** 1100 × 700 canvas with freehand strokes, **18 brush colours**, eraser, brush size, undo and clear (with confirmation). Strokes remain editable after restart.
- **Automatic local saves:** `%LOCALAPPDATA%\Inkpad\workspace.json` stores all text and strokes; `workspace.json.bak` keeps a previous version. Typing saves after a short pause and again on close.
- **Rapid background agent checks:** `inkpad-agent.exe` provides **38 headless MCP tools** and a JSON CLI for notes, drawings, picture attachments and exports. Agents can insert, read, resize, reorder, caption, delete and export pictures with approval safeguards. Persistent stdio MCP requires no Inkpad desktop window; see [AGENTS.md](AGENTS.md).
- **Safe multi-process edits:** Windows file locks, merging of unrelated changes, periodic desktop refresh and explicit conflict detection protect against overwriting agent or human work.
- **Export:** export the current Text view as `.txt`, Draw view as `.png`, or the **complete page as self-contained `.html`** containing text, ordered pictures and drawings. Picture bytes are embedded, so no external asset folder is required to open the export.
- **Accessible controls:** keyboard-focusable buttons, labelled sidebar pages, Esc to dismiss confirmation, and input controls.

## Run / build on Windows

Install [Rust MSVC toolchain](https://rustup.rs/), MSVC Build Tools, and the Windows SDK resource compiler (`rc.exe`) for the embedded icon. Then in this folder:

```powershell
cargo run --bin inkpad
cargo test
cargo build --release
```

The finished desktop executable is `target\release\inkpad.exe`. The optional MCP/CLI executable is `target\release\inkpad-agent.exe`. They use the same notes folder, even when only the agent is running. Set `INKPAD_DATA_DIR` to change this folder consistently for both processes. The portable build lives at `dist\Inkpad-Beta-Images-Staged\Inkpad.exe`.

## Appearance, formatting and agent parity

- The titlebar includes **Light / Dark / System** appearance choices. The selection persists to `ui-settings.json` independently of notebook content and is protected against concurrent agent/GUI updates.
- **Hold + or -** on the font and drawing brush controls for continuous bounded adjustments. Keyboard activation still changes once per key press.
- Genuine attributed Unicode character ranges support bold, text colour (`#RRGGBB`), 12-40pt custom range sizes and Sans/Serif/Mono style overrides; styled spans survive saves, restarts and headless edits. Previously saved plain notes remain readable.
- **Natural text selection:** Slint 1.18 TextInput reports mouse/keyboard selection UTF-8 byte offsets, converted without breaking Unicode. Bold and colour operate directly on the selected range; font size/family can also target the selection. **Ctrl+Alt+B** toggles bold while **Ctrl+B** still collapses the sidebar. Manual scalar start/end fields remain behind an optional advanced disclosure. Styled typing, rich copy/cut/paste with native plain-text fallback, and bounded session undo/redo are available. Uncommitted undo is rejected if the saved note was changed externally.
- **Formatted preview:** short/ordinary left-to-right notes render mixed fonts, sizes, bold and colours via Slint 1.18 wrapping Flexbox elements. Complex bidirectional text, formatting that splits a grapheme, or exceptionally long pages fall back to the native StyledText renderer (emphasis/colour only). The preview is read-only and its line wrapping can differ from the editable plain TextInput. This is **not yet a fully attributed WYSIWYG editor**; accurate physical acceptance testing remains pending.
- Chat On Steroids uses **38 MCP tools**: the previous 35 plus atomic format batches and portable rich copy/paste fragments. Agents can edit Unicode text and formatting without the GUI; OS clipboard reads and clicks are never needed.
- Preserve the single existing portable release folder. Development release builds live in Cargo's existing `target/release`; never overwrite an Inkpad GUI or MCP agent executable while its process is running. Notebook data is never a release artifact.
## Data and limitations

The app keeps data entirely on your computer. Back up the **entire** `%LOCALAPPDATA%\Inkpad` folder, including `media/` image files, `workspace.json` and `workspace.json.bak`. Picture files are stored locally as validated PNGs with opaque generated filenames; JSON stores captions, display widths and order. A removed picture's file is retained only while the previous-save backup still references it, then automatically reclaimed after backup rotation. Imports are limited to 12 MiB and 16 megapixels, with up to 100 pictures per page; animated GIFs are imported as a still image.

UI preferences live separately from note content. Notes preserve attributed styles and allow real mouse-selected formatting through the editable text view. The formatted view remains read-only, and the app does not synchronize between computers or provide an infinite drawing canvas. Picture blocks appear below the note's main text, not between words in a paragraph. The drawing editor uses a fixed 1100 × 700 canvas and exports PNG at that resolution. Clipboard paste, OS drag/drop and image layout have been implemented and headlessly built, but physical on-screen interaction still needs a separate acceptance check when the user allows window interaction.

This is a beta. The first launch creates a welcome page which you can edit or delete. Older notes and sketch-only pages continue to load; their text and strokes are not erased by the migration.
