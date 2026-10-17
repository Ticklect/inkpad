# Inkpad Beta

**Offline Windows notes, drawings and pictures — with a built-in AI agent interface.** Written in Rust and Slint. No account, cloud service or Electron required.

Inkpad includes a desktop app (`Inkpad.exe`) and an [MCP](https://modelcontextprotocol.io/) server (`inkpad-agent.exe`) so compatible AI agents can work with your saved notes **without opening the app**.

## What your AI agent can do

- **Create notes** and list, read, search, rename, update, append to, pin, select or delete them.
- **Search and read together** — return matching note text in one request instead of searching and opening each result separately.
- **Edit multiple notes at once** — perform up to 32 validated create, append, rename, update or pin operations in one atomic batch.
- **Write and format text** — insert or replace selected Unicode text; apply or clear bold, colours, font sizes and fonts; copy/paste styled text fragments; format ranges in batches.
- **Add pictures** — import approved PNG/JPEG/GIF/WebP files or supplied image data, position pictures within a note, resize, reorder, add captions, view or remove them.
- **Read and export pictures** — inspect an attached image or save it as PNG with your approval.
- **Draw on notes** — add freehand strokes, change brush properties, erase, inspect drawings and export a drawing as PNG.
- **Create editable artwork** — make vector shapes, change fills/gradients and strokes, move/resize/rotate/flip objects, reorder or group them, and work with layers, visibility and locks.
- **Inspect and edit drawings** — select objects by position, preview the canvas, and undo/redo supported drawing edits within the current MCP session.
- **Export notes** — save plain text or complete self-contained HTML with formatting, drawings and embedded pictures.
- **Manage preferences** — inspect workspace details, switch the app theme and expand/collapse the sidebar.

Inkpad exposes **58 MCP tools**. Destructive changes and access to local image files require explicit user approval. Full tool arguments, limitations and examples are in [AGENTS.md](AGENTS.md).

## Desktop features

- Notes with rich-text styling, inline pictures, captions and a drawing canvas
- Editable vector objects and freehand drawing
- Search, pinned notes, keyboard shortcuts, light/dark/system themes
- Automatic local saving, backup recovery and cross-process conflict protection

## Download and run

1. Download the **Windows x64 Beta** ZIP from [GitHub Releases](https://github.com/Ticklect/inkpad/releases). It includes **both** `Inkpad.exe` (desktop) and `inkpad-agent.exe` (MCP server). The MCP server is also available there as a separate download.
2. Extract the ZIP and run `Inkpad.exe`. Windows may show a SmartScreen warning for an unsigned Beta executable.
3. In your AI client's MCP settings, add a **local stdio server** with the full path to `inkpad-agent.exe` as the command and `mcp` as its argument. Restart or reconnect the client.

If Windows reports a missing `VCRUNTIME140.dll`, install the [Microsoft Visual C++ Redistributable (x64)](https://aka.ms/vs/17/release/vc_redist.x64.exe).

For clients using an `mcpServers` JSON configuration (such as Claude Desktop), replace the example path with the **actual location** of the downloaded executable:

```json
{
  "mcpServers": {
    "inkpad": {
      "command": "C:\\path\\to\\Inkpad\\inkpad-agent.exe",
      "args": ["mcp"]
    }
  }
}
```

**Chat On Steroids:** add a local MCP server with **Transport: stdio**, **Command: full path to `inkpad-agent.exe`**, **Arguments: `mcp`**. No URL, API key or cloud server is required. The agent and desktop use the same `%LOCALAPPDATA%\Inkpad` notes automatically, even when the desktop is closed. Both programs must run on the PC holding the notes; set `INKPAD_DATA_DIR` on both for a different storage folder. More clients and details: [AGENTS.md](AGENTS.md#mcp-setup).

## Build from source

Requires Rust (MSVC), MSVC Build Tools and the Windows SDK.

```powershell
cargo test
cargo build --release --bins
```

Output: `target\release\Inkpad.exe` (desktop) and `target\release\inkpad-agent.exe` (MCP).

**Beta limitations:** Windows-only; notes are local, not cloud-synced. Drawing canvas size is fixed, and some rich-text presentation remains a separate preview rather than fully WYSIWYG. Back up the **entire** `%LOCALAPPDATA%\Inkpad` folder (including `workspace.json`, `workspace.json.bak` and `media\`) before changing devices or storage.
