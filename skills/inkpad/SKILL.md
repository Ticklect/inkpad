---
name: inkpad-notes
description: Use when a user asks to remember, note down, save, search, check, read, rename, pin, format, draw, paste/import pictures, resize or reorder images, caption them, export complete notes or organise something in their local Inkpad notebook. All agent operations are headless; never open the Inkpad window or use screen automation.
---

# Inkpad local notes — headless agent skill

When the user says **"note that down in Inkpad"** or asks to **check their notes**, use the Inkpad headless agent binary. It can run without the desktop window or a background web server.

## Command

Use an appropriate local command-execution tool:

`D:\chat\inkpad\target\release\inkpad-agent.exe`

For a packaged version, confirm the package's executable has the same updated `tools/list` before substituting its path; earlier Beta distributions may contain an older agent without picture or vector-drawing tools. **Editing source does not update a running MCP process or the release executable.** Build/restart the appropriate agent before claiming newly added commands are available.

The binary emits JSON to standard output. From PowerShell:

```powershell
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' list
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' search 'topic'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' get NOTE_ID
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' create --title 'Topic' --body 'Actual facts and context to remember'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' append NOTE_ID 'Additional information'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' rename NOTE_ID 'New title'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' pin NOTE_ID
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' font-size NOTE_ID 19
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' font-choice NOTE_ID Serif
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' active
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' activate NOTE_ID
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' export-text NOTE_ID 'D:\Notes\export.txt'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' export-png NOTE_ID 'D:\Notes\sketch.png'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' add-image NOTE_ID 'D:\Photos\holiday.jpg' --approved
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' images NOTE_ID
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' image-width NOTE_ID IMAGE_ID 10
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' image-move NOTE_ID IMAGE_ID 0
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' image-caption NOTE_ID IMAGE_ID 'Holiday photo'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' image-delete NOTE_ID IMAGE_ID --confirm
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' read-image NOTE_ID IMAGE_ID --approved
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' export-image NOTE_ID IMAGE_ID 'D:\Notes\picture.png' --approved
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' settings
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' theme system
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' sidebar expanded
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' formatting NOTE_ID
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' format-range NOTE_ID 0 5 --bold true --color '#AA3377' --size 22 --family Serif
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' clear-format NOTE_ID 0 5
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' replace-range NOTE_ID 0 5 'New text'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' format-batch NOTE_ID '@D:\Notes\format-operations.json'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' copy-fragment NOTE_ID 1 7
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' paste-fragment NOTE_ID 5 5 '@D:\Notes\rich-fragment.json'
& 'D:\chat\inkpad\target\release\inkpad-agent.exe' export-html NOTE_ID 'D:\Notes\complete.html' --approved
```

If no command tool is available, connect the Inkpad MCP stdio server as documented in [AGENTS.md](../../AGENTS.md). The current Beta exposes **58 MCP tools** covering notes, indexed and combined search, atomic note batches, formatting, pictures, vector drawings, settings and exports. Check `tools/list` against the **actual launched executable**, since older Beta builds may expose fewer tools. For full stroke JSON in Windows PowerShell 5.1, prefer `add-stroke NOTE_ID @D:\Notes\stroke.json` (use a string variable) or the numeric point syntax detailed in AGENTS.md.

## Editable vector drawings

- For **"draw a rectangle/circle/arrow/diagram in my note"**, find the note ID first. Use `get_capabilities` to discover supported vector shapes and use `drawing_create_shape` for each single shape. The current model supports `rectangle`, `rounded_rectangle`, `ellipse`, `circle`, `line`, `polygon` and `cubic_bezier`. Example JSON arguments: `{"id":"NOTE_ID","shape":{"type":"circle","radius":36},"transform":{"translation":{"x":90,"y":90}},"style":{"stroke":[32,36,43,255],"stroke_width":2,"fill":{"type":"solid","color":[255,200,0,255]}}}`. Objects are added to the note's saved vector drawing and get stable IDs in the result.
- Use `drawing_list` to obtain existing object and layer IDs. Edit geometry or appearance with `drawing_edit_shape` (which replaces only the specified shape/style/transform/visibility/layer fields). For a relative move, resize, rotate or flip, use `drawing_transform_object` with `object_id` and `operation`. `move` needs `dx,dy`; `resize` needs `sx,sy`; `rotate` needs `degrees`; `flip` needs `horizontal:true` or `false`.
- For layers, use `drawing_create_layer`, `drawing_edit_layer` (name, visible, locked), `drawing_move_layer` (zero-based position), and `drawing_delete_layer` (empty unlocked layer only, explicitly authorized, `confirm:true`). Objects on locked layers reject edits. `drawing_reorder_object` changes one object's stacking position; use `drawing_group_objects` to group two or more IDs, `drawing_transform_group` to change the shared group, and `drawing_ungroup_objects` to restore independent group membership. These are individual MCP operations; **do not request or invent batch drawing commands**.
- Use `drawing_pick_object` to inspect the topmost visible and unlocked object at one canvas position, or `drawing_preview` to receive a PNG view of the complete saved drawing as a standard MCP image block. Preview is bounded to 1 MiB PNG, and `export_note` with `format:"png"` writes the drawing to a fresh absolute destination file if explicitly requested. The canvas is **1100×700** pixels. Legacy pen strokes, text and attached images remain stored separately and are preserved by shape tools.
- For deleting a vector object, use `drawing_delete_object` with explicit human authorization and `confirm:true`. The `drawing_undo` and `drawing_redo` tools likewise require `confirm:true` and actual user authorization; they operate only on vector edits tracked in the **same ongoing MCP stdio session** (max 48 snapshots per note), reject external drawing changes, and cannot recover actions after the MCP process restarts. They are separate from confirmed legacy `undo_last_stroke` and from the GUI's transient rich-text undo history.
- The desktop GUI exposes basic pen/eraser/selection and simple shapes, with more drawing tools in an expandable **Advanced** area. MCP supports some drawing properties (for example gradients, layer locks and programmatic groups) that do not necessarily have a GUI control yet. MCP cannot switch the user's active tool, change selection or invoke an open dialog. Read `D:\chat\inkpad\docs\DRAWING_CAPABILITIES.md` for the GUI versus MCP feature matrix; do not claim the UI was visually verified without a real window test.
- **Testing/verification boundary:** source compilation and unit tests do not prove mouse hit targets, compact layout, drawing performance or live GUI/MCP synchronization. Check the specific executable's `tools/list`, exercise the relevant operation, and inspect the returned PNG with an image-capable tool before saying an AI has visually checked its drawing. Until physical native-window QA is completed, treat 8+/10 GUI quality as unverified. For an interrupted drawing gesture, GUI pointer cancellation should roll back the in-progress edit rather than introducing an undo/redo step.

## Adding and managing pictures

- If the user says **"add this picture to Inkpad"**, obtain the actual image file path provided/selected by the user. Call `image_insert_file` with note `id`, absolute `path` and `user_approved:true` **only when that exact source is authorized**. For already user-provided base64 PNG/JPEG/GIF/WebP, use `image_insert_base64`; no remote URL fetching and no background folder scanning. Never set `user_approved:true` for a guessed path.
- The source image stays untouched; Inkpad validates and stores a normalized PNG under its private media directory. Input files are limited to 12 MiB and 16 megapixels, and a page supports up to 100 images. Base64 via MCP is limited to 1 MiB decoded per call. The CLI can read encoded text from `@ABS_PATH.b64` with `--approved`.
- Use `image_list` to get image IDs before moving, captioning, resizing or removing pictures. `image_resize` uses `width_percent` from **10 to 100**. `image_move` uses zero-based `position`; 0 means first picture. Captions can contain up to **500 characters**.
- For **"what's in this Inkpad picture?"**, use `read_image` only for the exact selected note attachment, with user approval. MCP receives a normal image content block, max 1 MiB PNG. If larger, use `export_image` to a user-approved new `.png` path; do not claim that exporting alone makes the assistant visually understand the image.
- Before `image_delete` identify the exact image and require explicit human authorization, then pass `confirm:true`. Deleting the attachment removes its note reference, but backup recovery may keep the PNG file temporarily; do not claim secure destruction of its bytes.
- For a portable copy containing **all text, inline pictures and drawings**, use `image_export_html` to a user-approved absolute `.html` path with `user_approved:true`. This embeds images into the HTML. It never overwrites a file and does not upload/share it.

## How to behave

- For **"note that down"**, summarise the context accurately, choose a concise descriptive title, and create or append an Inkpad note. Do not invent facts. Respond only after checking command success.
- For **"check my notes"**, run list or search and then get the relevant note; do not claim to have checked based on memory alone.
- Prefer appending to an existing exact note by ID rather than replacing its whole text.
- For rename, pin/unpin, font/size changes, active page selection, and exports, use the named action rather than editing the JSON file.
- For a user-requested appearance change, read `get_app_settings` first as needed; use `set_app_theme` (`light`, `dark`, or `system`) or `set_sidebar_collapsed` for persisted preferences. These commands do not control which transient editor panel is visible and do not edit notes.
- For **"make these words bold / change their colour / font"**, first read the note body and formatting; count Unicode **scalar** positions (`str::chars()`, not byte/UTF-16/grapheme offsets) and call `format_text_range` with half-open `[start,end)` and `unit:"unicode_scalar"`. Use `get_note_formatting` to verify. Only bold, foreground `#RRGGBB`, size 12–40 and font family Sans/Serif/Mono are supported. Never claim italic, underline or highlight/background colour support.
- For **"remove this formatting"**, use `clear_text_formatting`; for a precise replacement, use `replace_text_range`, which keeps surrounding styles. `append_to_note` / `update_note` also preserve span data. Selection/caret and temporary GUI view modes are not remotely changed by headless operations.
- For **multi-range formatting**, use `format_text_batch` with 1–32 ordered operations and `expected_body` to detect stale document text; the entire batch commits or none does. New operations use **UAX extended grapheme-safe** selection by default, even though offsets remain Unicode scalar indices; specify `boundary_policy:"scalar"` only for deliberate partial grapheme edits.
- For **"copy these formatted words"**, call `copy_rich_fragment` on the exact selected Unicode range; it returns styled JSON (text, relative runs, source font defaults) to the AI/client. For **"paste the copied rich text here"**, pass that returned JSON to `paste_rich_fragment`, preferably with `expected_text` matching the selected target. Neither tool accesses the operating-system clipboard. The fragment text must be at most 256 KiB and the style runs must be valid/canonical.
- The desktop's rich-text undo/redo history is bounded **session memory only**. It is not saved to the notebook, so do **not** claim durable undo/redo or call a non-existent MCP undo/redo tool. `undo_last_stroke` remains a different, explicitly confirmed action for drawing strokes only.
- For picture inserts, size, order, caption and removals, use the image MCP tools or matching CLI commands. Note title/body changes must preserve the `images` collection and other unsupplied fields.
- Never manipulate files inside Inkpad's data folder directly; always use the headless CLI or MCP tools so concurrent changes are coordinated safely.
- **Never delete a note or picture, clear drawings or undo a saved stroke** unless the user specifically requests it. These destructive actions require `--confirm` (CLI) or `confirm:true` (MCP).
- Treat the user's notes as sensitive. Do not paste them into public services or send them to a network endpoint.
- The Inkpad GUI need not be running. If it is open, saved external edits automatically appear in its sidebar after a short refresh.
- A page can contain text and drawing strokes simultaneously; the GUI's Text/Draw viewing mode is temporary and is not a remotely controlled persisted setting. Do not claim an agent changed that visual mode.
- If command execution fails, say so; do not claim the note was saved.

The notebook defaults to `%LOCALAPPDATA%\Inkpad\workspace.json` with an OS-level cross-process lock. For portable profiles, set `INKPAD_DATA_DIR` consistently.
