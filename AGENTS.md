# Inkpad agent access (headless)

Inkpad stores text, formatting, pin state and drawing strokes together in local `workspace.json` records. Agents can read, search, create, rename, append, update, pin, format, export and draw on pages without opening the desktop app or using screen control. The MCP server runs through standard input/output, with **no network listener**, API token, or always-running background service.

## Build and executable

From `D:\chat\inkpad` run:

```powershell
cargo build --release --bin inkpad-agent
```

The agent executable is **`D:\chat\inkpad\target\release\inkpad-agent.exe`** when building from source, or **`D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe`** in the ready-to-run Beta package. Run `inkpad-agent.exe mcp` to start its JSON-RPC/MCP stdio server (MCP mode is also the default). A client should launch one process and keep its stdin/stdout pipes open. Inkpad desktop does not need to be running.

**Windows launch behavior:** `inkpad-agent.exe` is deliberately a **console-subsystem** executable so the MCP client's redirected standard input, output and error streams work reliably. The MCP client should start it as a **stdio child process**, with stdin/stdout/stderr connected to pipes. If that client otherwise shows a brief console flash, its Windows process launcher should pass `CREATE_NO_WINDOW` (`0x08000000`) to `CreateProcessW` (for example, Python `subprocess.Popen(..., creationflags=subprocess.CREATE_NO_WINDOW, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)`). The flag suppresses the console window without switching to the Windows GUI subsystem or breaking MCP pipe communication. Clients that already launch stdio servers hidden need no extra setting. Do not use `start`, `cmd /c start`, or another process-detaching launcher, because MCP needs the child's pipes.

By default, workspace data lives at `%LOCALAPPDATA%\Inkpad\workspace.json`. If using portable storage, set `INKPAD_DATA_DIR` to the same directory for **both** the desktop app and the agent. The application stores backup data in `workspace.json.bak` and coordinates cross-process updates with a local file lock.

## MCP setup

**Chat On Steroids:** Add a local MCP server using the **stdio** transport in your MCP server settings. Set executable/command to `D:\chat\inkpad\target\release\inkpad-agent.exe` and arguments to `mcp`. Enable the server for the AI session where you want note access. If your client offers a hidden/background launch option for MCP stdio child processes, enable it. No browser, window capture or Control API is necessary.

**Codex:** Put this in your Codex MCP configuration (`%USERPROFILE%\.codex\config.toml` on Windows):

```toml
[mcp_servers.inkpad]
command = 'D:\chat\inkpad\target\release\inkpad-agent.exe'
args = ["mcp"]
```

**Claude Desktop:** Add an MCP server to `claude_desktop_config.json` (commonly under `%APPDATA%\Claude`):

```json
{
  "mcpServers": {
    "inkpad": {
      "command": "D:\\chat\\inkpad\\target\\release\\inkpad-agent.exe",
      "args": ["mcp"]
    }
  }
}
```

If you already have other MCP servers, merge the `inkpad` entry into the existing `mcpServers` object. Restart/reconnect the MCP client after configuration changes.

## Fast headless reads

For repeated checks such as “find my shopping note” or “what did I write about X?”, **keep the MCP stdio process connected**. It avoids the cost of launching a new Windows process for every tool call. No Inkpad window, screen capture, network service, or background polling is needed. The process stays idle until a request arrives.

An agent reads saved notes immediately while the Inkpad desktop window is open. Every read gets a fresh workspace snapshot through the same cross-process storage lock that protects writes, including when the MCP process has already been running for a long time. **There is no potentially stale in-memory note cache.** A simultaneous save can briefly delay a read while the file lock is held. The agent cannot see text that is still being typed in the GUI **until it has been saved**; desktop text saves are debounced by roughly 380 ms after editing. The GUI checks file metadata for external changes about every **350 ms**, then reloads when necessary; it can postpone reloading while the user has unsaved edits to prevent cursor jumps or lost text.

Measured on Windows, 14 October 2026, using the optimized release executable, warm OS filesystem cache, 14 separately launched CLI processes per operation and 25 round trips through one persistent MCP stdio process per operation. Times are local wall-clock milliseconds, median (95th percentile); not guaranteed response-time limits.

| Saved workspace | Fresh CLI `list` | Fresh CLI `search` | Persistent MCP search |
| --- | --- | --- | --- |
| 2,200 notes, 6.87 MB | 35.55 ms (51.90 ms) | 35.52 ms (52.69 ms) | 12.72 ms (13.79 ms), one match; 12.88 ms (15.03 ms), no matches |
| GUI open, small workspace, 548 bytes | 22.97 ms (28.45 ms) | 23.70 ms (37.70 ms) | 0.43 ms (0.55 ms), no matches |

For the 2,200-note workload, filtering before sorting reduced selective MCP search median from **14.09 to 12.72 ms** (~10%) and no-match search median from **15.15 to 12.88 ms** (~15%). Cold-process timing did not materially improve. Larger saved workspaces take longer because each request reads and parses the complete JSON file under the lock. The measurements used a warm OS cache and are sensitive to machine load. Avoid spawning a new agent for each search if an MCP connection is already available.

Regression coverage: `cargo test --offline --bin inkpad-agent` includes a 1,600-note search correctness/latency test and a test proving that a long-running agent sees a subsequent save from an independent store instance. The timing assertion deliberately catches severe slowdowns rather than enforcing a fragile millisecond threshold on slower machines.

## Available tools

| Tool | What it does |
| --- | --- |
| `list_notes` | Lists titles, IDs, preview text, formatting, pinned state and drawing stroke counts. Optional `limit` (1–200). |
| `search_notes` | Finds case-insensitive matches in titles and **full body text**. Requires `query`; optional `limit`. |
| `read_note` | Reads complete text, metadata, and the saved drawing `strokes`. Select by `id` or exact `title`. |
| `create_note` | Creates a page using optional `title` and `body`, leaving the current GUI page selected. |
| `append_to_note` | Appends `text` to a page selected by `id` or exact `title`; inserts a newline between existing and new text when required. |
| `update_note` | Replaces supplied `title` and/or `body`; select by `id` or `match_title`. Preserves drawings and unspecified fields. |
| `rename_note` | Sets `new_title` for `id` or exact `match_title`; preserves text, styles and drawings. |
| `set_note_pin` | Pins or unpins a page with `pinned: true` or `false`. Select by `id` or exact `title`. |
| `set_font_size` | Sets `size` to an integer from 12 to 40. |
| `set_font_choice` | Sets `choice` to `Sans`, `Serif` or `Mono`. |
| `get_active_note` | Reads the saved active page and returns its text, formatting and complete strokes, or null if none. |
| `set_active_note` | Saves which page should be active, selected by `id` or exact `title`. May change the open GUI's page when safe. Does not change the transient Text/Draw view mode. |
| `export_note` | Exports `.txt` (UTF-8 body) or `.png` (1100×700 drawing) with `format` (`txt`/`png`) and an explicit absolute `path`. Existing files are never overwritten, and page data is untouched. |
| `add_stroke` | Adds a validated stroke: `points` (1–512 `[x,y]` pairs with x=0–1099 and y=0–699), `width` (1–40), optional `color` (`#RRGGBB`) and `eraser` boolean. Maximum 2,000 added strokes per page. |
| `undo_last_stroke` | Removes the final stroke; `confirm: true` and prior human authorization required. Does not erase text. |
| `clear_strokes` | Clears a page's drawings with `confirm: true` and prior human authorization; text remains. |
| `delete_note` | Deletes a page and its drawing strokes. Must receive a human user's explicit deletion request and `confirm: true`; select by `id` or exact `title`. |
| `get_workspace_info` | Returns local file location, workspace version, number of pages and active page ID. |

IDs are the safest way to target existing notes; exact titles may be duplicated, in which case the server rejects the request rather than guessing. Read a page before overwriting its whole body, and prefer `append_to_note` for normal note-taking requests. The agent preserves legacy `board` pages and drawing stroke data during text and formatting edits. Only explicitly authorized `undo_last_stroke`, `clear_strokes` or `delete_note` calls remove drawings. The `confirm` argument is a safeguard; it does not independently verify consent, so the calling AI must only supply it after the user has actually authorized the corresponding destructive action.

**GUI state:** An Inkpad page can hold text and drawings simultaneously. Text/Draw is a temporary editor view choice held in the desktop application's memory, **not** a saved property. Headless calls can edit both layers and the saved active page. They cannot remotely change which editor mode is on screen, invoke dialogs, or manipulate open selection/caret state. When the GUI has unsaved changes, its conflict safeguards can delay applying external updates; `set_active_note` confirms the saved preference, not guaranteed immediate visible switching in every GUI state. The GUI and the agent coordinate durable writes through the same lock.

**Export rules:** The target directory must already exist. `export_note` never opens a Save As dialog and refuses a path that already exists. It requires a full local destination path with the requested `.txt` or `.png` extension. For `.png`, it renders the existing strokes using the same software drawing renderer as Inkpad's GUI; body text is not rendered into that image. `txt` contains the note's body only. Extremely large or malformed saved strokes may be rejected instead of exhausting CPU or memory.

## Optional command-line interface

You can script small tasks directly without MCP:

```powershell
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' list
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' search 'shopping'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' create --title 'Shopping' --body 'Milk'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' get NOTE_ID
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' append NOTE_ID 'Bread'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' update NOTE_ID --title 'Weekly shopping'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' rename NOTE_ID 'To buy'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' pin NOTE_ID
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' unpin NOTE_ID
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' font-size NOTE_ID 20
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' font-choice NOTE_ID Serif
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' active
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' activate NOTE_ID
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' export-text NOTE_ID 'D:\Notes\export.txt'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' export-png NOTE_ID 'D:\Notes\drawing.png'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' add-stroke NOTE_ID 6 '#0066CC' 10,20 60,80
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' undo-stroke NOTE_ID --confirm
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' clear-strokes NOTE_ID --confirm
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' delete NOTE_ID --confirm
```

JSON outputs appear on stdout. Errors appear on stderr with a nonzero exit code. For MCP clients, every stdout line is a JSON-RPC 2.0 message; diagnostics are never sent to stdout.

For full stroke JSON from PowerShell, write it to a UTF-8 `.json` file and pass the file prefixed by `@` (quoted through a variable). This avoids PowerShell 5.1 removing double quotes inside JSON arguments:

```powershell
$jsonFile = 'D:\Notes\stroke.json' # Example contents: {"width":6,"color":"#0066CC","points":[[10,20],[60,80]]}
$strokeArg = '@' + $jsonFile
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' add-stroke NOTE_ID $strokeArg
```

The JSON file must be an existing, absolute `.json` path and at most 128 KiB; stroke values must still pass the normal width, coordinate, colour and point-count validation. Direct `add-stroke NOTE_ID WIDTH COLOR X,Y ...` also works in PowerShell and avoids JSON argument escaping entirely.
