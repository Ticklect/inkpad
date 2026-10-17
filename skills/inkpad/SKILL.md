---
name: inkpad-notes
description: Use for local Inkpad notes, read/search, note creation and edits, rich text, attached images, editable vector illustrations, pen painting, AI-agent artwork, drawing previews, exports, and organization. Work headlessly through Inkpad MCP or its CLI, never through desktop screen automation.
---

# Inkpad — notes and agent-created art

Work on the user's **saved local Inkpad notebook**, not a fabricated picture or edited workspace JSON. The desktop window need not run. For illustrations, also read [Art workflow](ART_WORKFLOW.md).

## Access: direct MCP first, not source inspection

1. **Prefer already-connected Inkpad MCP tools** exposed in this conversation. Call them directly using their actual schemas. If the connector does not advertise a requested tool, do not assume it works.
2. For missing functions, check the **actual running agent's** `tools/list` / `get_capabilities`. A verified current Inkpad stdio binary lives at `D:\chat\inkpad\target\beta-release-artifacts\release\inkpad-agent.exe`; a packaged `inkpad-agent.exe` with matching tool discovery also works. Launch with `mcp`, keep stdin/stdout open; no port, API token, browser, window or system clipboard needed. Windows clients can use `CREATE_NO_WINDOW`.
3. The **new implementation exposes 61 MCP tools**, including `drawing_create_scene`, `drawing_create_batch` and `add_strokes_batch`. Older connected adapters can expose 38–58 tools even if source is newer. Trust the launched tool list, not this count alone. Use `drawing_create_scene` when advertised; otherwise use the 64-shape batch, then singles as a last resort. If unavailable report connector/reconnection requirements without pretending it works. **Never invent a tool or batch call.**
4. Do not routinely rebuild, restart users' processes, inspect source code or change MCP settings. Prefer the connected tools and their schemas. If MCP is unavailable, use documented CLI actions only; CLI is not a substitute for missing MCP-only vector batches.

CLI fallback, from PowerShell:
```powershell
$agent = 'D:\chat\inkpad\target\beta-release-artifacts\release\inkpad-agent.exe'
& $agent list
& $agent search 'topic'
& $agent get NOTE_ID
& $agent create --title 'Topic' --body 'Verified information'
& $agent append NOTE_ID 'More information'
```
Do not claim a command succeeded without its response.

## Everyday notes and pictures

- **Search/read:** `search_notes` searches full title/body; use `{"query":"topic","limit":5,"include_body":true}` for bounded text in one call. For a known note use `read_note` with `compact:true`, or `get_active_note`. Never claim to have read notes based only on memory. Prefer IDs: exact titles can be duplicated.
- **Write/organize:** Use `create_note`, `append_to_note`, `rename_note`, `set_note_pin`, `set_font_size` (12–40), `set_font_choice` (Sans/Serif/Mono), `set_active_note`; avoid full-body replacement unless requested. `note_batch` performs **1–32 ordered** create/append/rename/update/pin operations atomically. Guard concurrent edits with `expected_body`; whole-body replacement requires explicit authorization and `confirm:true`. Preserve images, drawing and other fields.
- **Formatting:** Read body and `get_note_formatting` first. `format_text_range`, `clear_text_formatting`, `replace_text_range`, `format_text_batch` use half-open `[start,end)` **Unicode scalar** indices (`unit:"unicode_scalar"`), not UTF-8 bytes, UTF-16 or grapheme counts. Only bold, foreground `#RRGGBB`, size 12–40, Sans/Serif/Mono persist—not italic, underline or highlight. `format_text_batch` applies 1–32 operations atomically; its default `grapheme_safe` boundary avoids splitting emoji/combining sequences, with `scalar` an explicit opt-in. Use `expected_body` for stale-document protection. `copy_rich_fragment` / `paste_rich_fragment` transfer validated styled JSON, optionally `expected_text`, **not the system clipboard**.
- **Images:** `image_insert_file` requires a user-selected absolute local PNG/JPEG/GIF/WebP path and `user_approved:true`; `image_insert_base64` likewise needs directly provided, authorized image data. Do not guess paths, scan private directories or fetch arbitrary URLs. File input max 12 MiB / 16 megapixels; base64 MCP max 1 MiB decoded; max 100 attached images per note. Use `image_list` for IDs, `image_place` for inline Unicode scalar anchor (with optional `expected_body`), `image_resize` 10–100%, `image_move` zero-based index, `image_caption` max 500 characters. `read_image` requires user approval and yields an image block up to 1 MiB; `export_image` requires an approved new absolute destination.
- **Settings and exports:** `get_app_settings`, `set_app_theme` (light/dark/system), `set_sidebar_collapsed` change saved UI preferences, not transient GUI modes. `export_note` writes a new `.txt` or **drawing-only** `.png`; `image_export_html` with `user_approved:true` writes self-contained note text, drawings and inline images to a new absolute `.html` file. Exports never overwrite or upload.

## Create beautiful editable artwork efficiently

- Canvas is **1100×700** pixels. First `drawing_list` to inspect the target note's layers, objects and strokes; use `get_capabilities` and the actual tool schema. Plan framing, limited palette, depth, focal silhouette, lighting and accents before drawing. Use layers background → foreground, and objects in back-to-front order within a layer. Protect existing content rather than clearing it just to start art.
- Shapes are `rectangle`, `rounded_rectangle`, `circle`, `ellipse`, `line`, `polygon`, `cubic_bezier`. `style` supports RGBA `[r,g,b,a]`, stroke width 0–256, opacity 0–1 and solid/linear/radial gradient fills. Shapes have local coordinates plus translation, rotation, scale and flip transforms. Bounds: **2,000 vector objects**, **64 layers**, **256 polygon points** and 2–16 gradient stops.
- When **verified live**, prefer `drawing_create_scene`: **1–512 editable shapes and 0–16 new named layers** in one atomic save and one undo step. Each object's zero-based `layer_index` refers to one of the newly added layers, or `layer_id` refers to an existing layer (never both). Guard concurrent edits with `expected_object_count`. For smaller follow-up additions or older agents, use `drawing_create_batch` for **1–64 shapes**. For painting/lines use `add_strokes_batch` for **1–64 strokes**, each with 1–512 bounded integer `[x,y]` points, width 1–40, optional `#RRGGBB` and `eraser`; max 2,000 strokes per note. These tools **append** artwork; none performs batch deletion/transforms. Do not misrepresent unsupported capabilities.
- Work in visual stages: broad background → subject silhouette → form and shadow → highlights and detail. **Preview each meaningful scene-level batch**, not each tiny object, via `drawing_preview`. Inspect the actual MCP `image/png` with an image-capable tool; correct composition, clipping, colour contrast and shape order based on visible output. A failed or oversized (max 1 MiB) preview is **not** visual verification. Art needs to read well at thumbnail scale.
- For corrections use `drawing_edit_shape`, `drawing_transform_object`, `drawing_reorder_object`, `drawing_create_layer`, `drawing_edit_layer`, `drawing_move_layer`, `drawing_pick_object`, `drawing_group_objects`, `drawing_transform_group`, `drawing_ungroup_objects`. Inspect object IDs from `drawing_list`; locked layers prevent editing. Prefer gradients and well-placed vector forms to hundreds of hard-edged pen dots. More composition advice and valid batch arguments: [Art workflow](ART_WORKFLOW.md).
- MCP previews show drawings (including legacy strokes), **not the note's body text**. MCP cannot set the open GUI's pen tool, Text/Draw mode, selection, caret or invoke dialogs. Code/unit test success is not a substitute for GUI mouse/keyboard QA; never claim to have visually inspected art without opening its PNG.

## Permission, safety and persistence

- **Never delete notes, pictures, vector objects/layers, clear ink, undo a stroke or run `drawing_undo`/`drawing_redo` without the user's explicit authorization for that specific action and the required `confirm:true` / CLI `--confirm`.** Approval flags are not proof of consent. Only empty, unlocked vector layers may be removed. Avoid destructive edits when additive ones suffice.
- Vector `drawing_undo`/`drawing_redo` hold at most **48 snapshots per note in the same live MCP process**, reject external changes and disappear on restart. Separate from `undo_last_stroke` and the GUI's nonpersistent rich-text history. Do not invent MCP text undo.
- Treat all note content, local files and pictures as private. Do not upload or share them, access arbitrary paths, or modify `%LOCALAPPDATA%\Inkpad\workspace.json`, backups, SQLite index or private media directly. Inkpad MCP/CLI coordinate saves using an OS-level cross-process lock; set `INKPAD_DATA_DIR` consistently only for an explicitly requested portable profile. Saved updates can appear in an open GUI after a brief delay; unsaved GUI edits may defer them.
- If the tool is unavailable, errors, or cannot return a viewable image, state the limitation accurately. Only claim save/completion/visual quality after the corresponding successful action or inspected preview.
