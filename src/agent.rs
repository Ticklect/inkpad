//! MCP stdio and command-line interface to the Inkpad workspace.
//! Storage mutations use the cross-process transaction lock in `Store`.

use crate::canvas::{Board, HEIGHT, WIDTH};
use crate::store::{Note, Store, Stroke, Workspace};
use serde_json::{json, Map, Value};
use std::fs::OpenOptions;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;

const MAX_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_TITLE_CHARS: usize = 200;
const MAX_SEARCH_CHARS: usize = 1000;
const MAX_RESULTS: usize = 200;
const MAX_STROKE_POINTS: usize = 512;
const MAX_STROKES_PER_NOTE: usize = 2_000;
const MAX_STROKE_JSON_BYTES: usize = 128 * 1024;

pub fn run() -> io::Result<()> {
    let store = Store::from_system()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("mcp") => serve_mcp(&store, io::stdin().lock(), io::stdout().lock()),
        _ => run_cli(&store, &args),
    }
}

/// Parse one bounded line at a time. A malformed or oversized client message
/// cannot crash the server or make it allocate arbitrarily large buffers.
fn read_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Result<Value, String>>> {
    let mut bytes = Vec::new();
    let mut overflow = false;
    let mut saw_data = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            if !saw_data { return Ok(None); }
            break;
        }
        saw_data = true;
        let n = buf.iter().position(|&c| c == b'\n').map(|i| i + 1).unwrap_or(buf.len());
        let end_line = buf[n - 1] == b'\n';
        if !overflow && bytes.len().saturating_add(n) <= MAX_MESSAGE_BYTES {
            bytes.extend_from_slice(&buf[..n]);
        } else {
            overflow = true;
        }
        reader.consume(n);
        if end_line { break; }
    }
    if overflow { return Ok(Some(Err("JSON-RPC message exceeds 2 MiB".into()))); }
    Ok(Some(serde_json::from_slice(&bytes).map_err(|error| error.to_string())))
}

fn serve_mcp<R: BufRead, W: Write>(store: &Store, mut input: R, mut output: W) -> io::Result<()> {
    while let Some(message) = read_message(&mut input)? {
        let response = match message {
            Ok(request) => handle_rpc(store, request),
            Err(error) => Some(rpc_error(Value::Null, -32700, &format!("Parse error: {error}"))),
        };
        if let Some(value) = response {
            serde_json::to_writer(&mut output, &value)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message}})
}

fn handle_rpc(store: &Store, request: Value) -> Option<Value> {
    let Some(object) = request.as_object() else {
        return Some(rpc_error(Value::Null, -32600, "Invalid JSON-RPC request"));
    };
    let id = object.get("id").cloned();
    let response_id = id.clone().unwrap_or(Value::Null);
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || object.get("method").and_then(Value::as_str).is_none()
        || id.as_ref().is_some_and(|id| !id.is_null() && !id.is_string() && !id.is_number())
    {
        return Some(rpc_error(response_id, -32600, "Invalid JSON-RPC request"));
    }
    // JSON-RPC notifications have no ID and must never produce a reply.
    let id = id?;
    let method = object["method"].as_str().unwrap_or_default();
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    let result: Result<Value, String> = match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = match requested {
                "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25" => requested,
                _ => "2025-06-18",
            };
            Ok(json!({
                "protocolVersion":version,
                "capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"inkpad-agent","version":env!("CARGO_PKG_VERSION")},
                "instructions":"Headless access to saved Inkpad text, formatting, pins, active-page preference and drawing strokes. Deletion, undo and clearing drawings require explicit user authorization and confirm=true. Text/Draw GUI view mode is not stored."
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools":tool_definitions()})),
        "prompts/list" | "resources/list" | "resources/templates/list" => Ok(json!({"prompts":[],"resources":[],"resourceTemplates":[]})),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(rpc_error(id, -32602, "tools/call requires a tool name"));
            };
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let result = execute_tool(store, name, &args);
            return Some(json!({
                "jsonrpc":"2.0", "id":id,
                "result":match result {
                    Ok(value) => tool_result(value),
                    Err(error) => json!({"content":[{"type":"text","text":error}],"isError":true})
                }
            }));
        }
        _ => return Some(rpc_error(id, -32601, "Method not found")),
    };
    Some(match result {
        Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
        Err(error) => rpc_error(id, -32602, &error),
    })
}

fn tool_result(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".into());
    json!({"content":[{"type":"text","text":text}],"structuredContent":value,"isError":false})
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({"name":"list_notes","description":"List Inkpad notes with titles, text previews, drawing counts and IDs; sorted pinned-first and recently updated.","inputSchema":{"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":200}},"additionalProperties":false}}),
        json!({"name":"search_notes","description":"Search note titles and full text content, case-insensitively. Returns note IDs and drawing counts.","inputSchema":{"type":"object","properties":{"query":{"type":"string","minLength":1},"limit":{"type":"integer","minimum":1,"maximum":200}},"required":["query"],"additionalProperties":false}}),
        json!({"name":"read_note","description":"Read a complete note including text, formatting, and all drawing strokes. Provide an ID or an exact, unique title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"}},"additionalProperties":false}}),
        json!({"name":"create_note","description":"Create a new Inkpad page with optional title and body. The current desktop page remains selected.","inputSchema":{"type":"object","properties":{"title":{"type":"string"},"body":{"type":"string"}},"additionalProperties":false}}),
        json!({"name":"append_to_note","description":"Append text to an existing page without touching its drawings. Provide an ID or an exact, unique title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"text":{"type":"string","minLength":1}},"required":["text"],"additionalProperties":false}}),
        json!({"name":"update_note","description":"Replace the supplied title and/or body of an existing note. Unspecified fields and all drawing strokes are preserved. Select by ID or exact match_title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"match_title":{"type":"string"},"title":{"type":"string"},"body":{"type":"string"}},"additionalProperties":false}}),
        json!({"name":"rename_note","description":"Rename a note without touching its text or drawing. Select by ID or exact match_title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"match_title":{"type":"string"},"new_title":{"type":"string","minLength":1,"maxLength":200}},"required":["new_title"],"additionalProperties":false}}),
        json!({"name":"set_note_pin","description":"Pin or unpin a note. Set pinned=true or false. Select by ID or exact title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"pinned":{"type":"boolean"}},"required":["pinned"],"additionalProperties":false}}),
        json!({"name":"set_font_size","description":"Set a note's font size in the supported 12–40 pixel range.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"size":{"type":"integer","minimum":12,"maximum":40}},"required":["size"],"additionalProperties":false}}),
        json!({"name":"set_font_choice","description":"Set a note's font family preference to Sans, Serif or Mono.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"choice":{"type":"string","enum":["Sans","Serif","Mono"]}},"required":["choice"],"additionalProperties":false}}),
        json!({"name":"get_active_note","description":"Return the complete saved active page, including text and drawing strokes, or null if no page is selected.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"set_active_note","description":"Update the saved active page preference. An already-open GUI may keep its locally selected page; GUI Text/Draw mode is transient and cannot be set here.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"}},"additionalProperties":false}}),
        json!({"name":"export_note","description":"Save a note's plain UTF-8 text as .txt or its drawing strokes as a 1100x700 .png file at an explicit absolute path. Never overwrites an existing file. Select by ID or exact title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"format":{"type":"string","enum":["txt","png"]},"path":{"type":"string"}},"required":["format","path"],"additionalProperties":false}}),
        json!({"name":"add_stroke","description":"Append one validated drawing stroke to an existing page without altering text. Coordinates are canvas pixels (x:0-1099,y:0-699), 1–512 points, width 1–40, color #RRGGBB, optional eraser.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"points":{"type":"array","minItems":1,"maxItems":512,"items":{"type":"array","minItems":2,"maxItems":2,"items":{"type":"integer","minimum":0}}},"width":{"type":"integer","minimum":1,"maximum":40},"color":{"type":"string","pattern":"^#[0-9a-fA-F]{6}$"},"eraser":{"type":"boolean"}},"required":["points","width"],"additionalProperties":false}}),
        json!({"name":"undo_last_stroke","description":"Remove one most-recent drawing stroke; destructive operation requiring the user's explicit authorization and confirm=true.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"confirm":{"type":"boolean","const":true}},"required":["confirm"],"additionalProperties":false}}),
        json!({"name":"clear_strokes","description":"Remove ALL drawing strokes from one page, preserving body text. Requires explicit human authorization and confirm=true.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"confirm":{"type":"boolean","const":true}},"required":["confirm"],"additionalProperties":false}}),
        json!({"name":"delete_note","description":"PERMANENTLY delete a note and its drawing strokes. Call only after the human user explicitly authorizes deletion. Requires confirm:true plus note ID or exact, unique title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"confirm":{"type":"boolean","const":true}},"required":["confirm"],"additionalProperties":false}}),
        json!({"name":"get_workspace_info","description":"Show local workspace version, note count, active page ID, and storage file location.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
    ]
}

fn invalid(message: impl Into<String>) -> String { message.into() }

fn object_args<'a>(args: &'a Value, allowed: &[&str]) -> Result<&'a Map<String, Value>, String> {
    let object = args.as_object().ok_or_else(|| invalid("Arguments must be a JSON object"))?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("Unknown argument: {key}"));
    }
    Ok(object)
}

fn optional_string<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(value) => value.as_str().map(Some).ok_or_else(|| format!("{key} must be a string")),
    }
}

fn required_string<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    optional_string(args, key)?.ok_or_else(|| format!("{key} is required"))
}

fn validate_title(title: &str) -> Result<(), String> {
    if title.trim().is_empty() { return Err(invalid("Title cannot be blank")); }
    if title.chars().count() > MAX_TITLE_CHARS { return Err(invalid("Title is too long (max 200 characters)")); }
    if title.chars().any(char::is_control) { return Err(invalid("Title cannot include control characters")); }
    Ok(())
}

fn validate_body(body: &str) -> Result<(), String> {
    if body.len() > MAX_BODY_BYTES { return Err(invalid("Text exceeds 1 MiB")); }
    if body.contains('\0') { return Err(invalid("Text cannot include null characters")); }
    Ok(())
}

fn selector(args: &Map<String, Value>, by_title: &str) -> Result<(bool, String), String> {
    let id = optional_string(args, "id")?;
    let title = optional_string(args, by_title)?;
    match (id, title) {
        (Some(id), None) if !id.trim().is_empty() && id.len() <= 256 => Ok((false, id.to_string())),
        (None, Some(title)) => { validate_title(title)?; Ok((true, title.to_string())) }
        (None, None) => Err(format!("Provide id or {by_title}")),
        (Some(_), Some(_)) => Err(format!("Provide only one of id or {by_title}")),
        _ => Err(invalid("Invalid note ID")),
    }
}

fn find_note_index(workspace: &Workspace, by_title: bool, selector: &str) -> Result<usize, String> {
    if !by_title {
        return workspace.notes.iter().position(|note| note.id == selector)
            .ok_or_else(|| format!("Note ID '{selector}' was not found"));
    }
    let mut matches = workspace.notes.iter().enumerate().filter(|(_, note)| note.title == selector);
    let first = matches.next().ok_or_else(|| format!("No note with exact title '{selector}'"))?.0;
    if matches.next().is_some() { return Err(format!("Multiple notes have title '{selector}'; use the note ID")); }
    Ok(first)
}

fn ordered_notes(notes: &[Note]) -> Vec<&Note> {
    let mut list: Vec<&Note> = notes.iter().collect();
    sort_notes(&mut list);
    list
}

fn sort_notes(list: &mut Vec<&Note>) {
    list.sort_by(|a, b| b.pinned.cmp(&a.pinned).then_with(|| b.updated.cmp(&a.updated)).then_with(|| a.title.cmp(&b.title)));
}

fn preview(body: &str) -> String {
    body.lines().find(|line| !line.trim().is_empty()).unwrap_or("").chars().take(140).collect()
}

fn note_summary(note: &Note) -> Value {
    json!({
        "id":note.id,"title":note.title,"kind":note.kind,
        "preview":preview(&note.body),"stroke_count":note.strokes.len(),
        "has_drawing":!note.strokes.is_empty(),"pinned":note.pinned,
        "font_size":note.font_size,"font_choice":note.font_choice,"updated":note.updated
    })
}

fn limit(args: &Map<String, Value>) -> Result<usize, String> {
    match args.get("limit") {
        None => Ok(100),
        Some(value) => match value.as_u64() {
            Some(n) if (1..=MAX_RESULTS as u64).contains(&n) => Ok(n as usize),
            _ => Err(invalid("limit must be an integer between 1 and 200")),
        },
    }
}

fn required_bool(args: &Map<String, Value>, key: &str) -> Result<bool, String> {
    args.get(key).and_then(Value::as_bool).ok_or_else(|| format!("{key} must be true or false"))
}

fn required_i32(args: &Map<String, Value>, key: &str, min: i32, max: i32) -> Result<i32, String> {
    let n = args.get(key).and_then(Value::as_i64)
        .filter(|n| (*n >= min as i64) && (*n <= max as i64));
    n.map(|n| n as i32).ok_or_else(|| format!("{key} must be an integer between {min} and {max}"))
}

fn require_confirmation(args: &Map<String, Value>, purpose: &str) -> Result<(), String> {
    if args.get("confirm") == Some(&Value::Bool(true)) { Ok(()) }
    else { Err(format!("{purpose} requires the user's explicit authorization and confirm:true")) }
}

fn parse_stroke(args: &Map<String, Value>) -> Result<Stroke, String> {
    let width = required_i32(args, "width", 1, 40)? as u16;
    let eraser = match args.get("eraser") {
        None => false,
        Some(_) => required_bool(args, "eraser")?,
    };
    let color = match optional_string(args, "color")? {
        None => [38, 50, 56, 255],
        Some(color) if color.len() == 7 && color.starts_with('#') && color[1..].bytes().all(|b| b.is_ascii_hexdigit()) => {
            let red = u8::from_str_radix(&color[1..3], 16).map_err(|e| e.to_string())?;
            let green = u8::from_str_radix(&color[3..5], 16).map_err(|e| e.to_string())?;
            let blue = u8::from_str_radix(&color[5..7], 16).map_err(|e| e.to_string())?;
            [red, green, blue, 255]
        }
        _ => return Err(invalid("color must be exactly #RRGGBB")),
    };
    let points = args.get("points").and_then(Value::as_array)
        .filter(|points| !points.is_empty() && points.len() <= MAX_STROKE_POINTS)
        .ok_or_else(|| format!("points must contain 1–{MAX_STROKE_POINTS} x/y coordinate pairs"))?;
    let mut parsed = Vec::with_capacity(points.len());
    for (i, point) in points.iter().enumerate() {
        let pair = point.as_array().filter(|pair| pair.len() == 2)
            .ok_or_else(|| format!("points[{i}] must be [x,y]"))?;
        let x = pair[0].as_u64().filter(|x| *x < WIDTH as u64)
            .ok_or_else(|| format!("points[{i}] x must be 0–{}", WIDTH - 1))?;
        let y = pair[1].as_u64().filter(|y| *y < HEIGHT as u64)
            .ok_or_else(|| format!("points[{i}] y must be 0–{}", HEIGHT - 1))?;
        parsed.push([x as u16, y as u16]);
    }
    Ok(Stroke { width, color, eraser, points: parsed })
}

fn parse_cli_stroke(args: &[String]) -> Result<Map<String, Value>, String> {
    // PowerShell strips unescaped JSON quotes from native executable arguments.
    // Accept: add-stroke ID WIDTH '#RRGGBB' X,Y [X,Y ...] [--eraser].
    if args.len() < 5 { return Err(invalid("Stroke syntax: add-stroke ID WIDTH COLOR X,Y [X,Y ...] [--eraser]")); }
    let width = args[2].parse::<i32>().map_err(|_| invalid("width must be an integer from 1 to 40"))?;
    let mut pair_end = args.len();
    let eraser = args.last().is_some_and(|last| last == "--eraser");
    if eraser { pair_end -= 1; }
    if pair_end <= 4 || pair_end - 4 > MAX_STROKE_POINTS {
        return Err(invalid("Provide 1–512 X,Y coordinate pairs"));
    }
    let mut points = Vec::with_capacity(pair_end - 4);
    for pair in &args[4..pair_end] {
        let (x,y) = pair.split_once(',').ok_or_else(|| invalid("Coordinates must be X,Y"))?;
        let x = x.parse::<i64>().map_err(|_| invalid("X coordinates must be integers"))?;
        let y = y.parse::<i64>().map_err(|_| invalid("Y coordinates must be integers"))?;
        points.push(json!([x,y]));
    }
    let mut result = Map::new();
    result.insert("id".into(), json!(args[1]));
    result.insert("width".into(), json!(width));
    result.insert("color".into(), json!(args[3]));
    result.insert("points".into(), json!(points));
    result.insert("eraser".into(), json!(eraser));
    Ok(result)
}

fn cli_stroke_json(value: &str) -> Result<Value, String> {
    if let Some(file_path) = value.strip_prefix('@') {
        let path = Path::new(file_path);
        if !path.is_absolute() || !path.is_file() || !path.extension().is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("json")) {
            return Err(invalid("@file must point to an existing absolute .json file"));
        }
        let mut reader = OpenOptions::new().read(true).open(path)
            .map_err(|e| format!("Cannot open stroke JSON file: {e}"))?
            .take((MAX_STROKE_JSON_BYTES + 1) as u64);
        let mut contents = Vec::new();
        reader.read_to_end(&mut contents).map_err(|e| format!("Cannot read stroke JSON file: {e}"))?;
        if contents.len() > MAX_STROKE_JSON_BYTES {
            return Err(format!("Stroke JSON file must not exceed {MAX_STROKE_JSON_BYTES} bytes"));
        }
        // Windows PowerShell 5.1 commonly writes UTF-8 with a BOM.
        let payload = contents.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&contents);
        serde_json::from_slice(payload).map_err(|e| format!("Invalid stroke JSON file: {e}"))
    } else {
        serde_json::from_str(value).map_err(|e| format!("Invalid JSON stroke: {e}"))
    }
}

fn export_note(note: &Note, format: &str, destination: &str) -> Result<Value, String> {
    let suffix = match format { "txt" => "txt", "png" => "png", _ => return Err(invalid("format must be txt or png")) };
    let path = Path::new(destination);
    if destination.is_empty() || !path.is_absolute() || path.file_name().is_none() {
        return Err(invalid("path must be an absolute path to a file"));
    }
    if !path.extension().is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case(suffix)) {
        return Err(format!("Export path must have a .{suffix} extension"));
    }
    // Render fully before opening the path. This keeps invalid/huge stroke
    // data from creating a partial file and avoids holding the workspace lock.
    let output = if format == "txt" {
        note.body.as_bytes().to_vec()
    } else {
        if note.strokes.len() > 20_000 || note.strokes.iter().map(|s| s.points.len()).sum::<usize>() > 250_000 {
            return Err(invalid("Drawing is too large to safely render in a headless export"));
        }
        Board::new(&note.strokes).png_bytes().map_err(|e| format!("PNG rendering failed: {e}"))?
    };
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)
        .map_err(|e| format!("Cannot create export (existing files are never replaced): {e}"))?;
    file.write_all(&output).map_err(|e| format!("Failed writing export: {e}"))?;
    file.sync_all().map_err(|e| format!("Failed syncing export: {e}"))?;
    Ok(json!({"id":note.id,"format":format,"path":path.display().to_string(),"bytes":output.len(),
        "width":if format == "png" {Some(WIDTH)} else {None},
        "height":if format == "png" {Some(HEIGHT)} else {None}}))
}

fn load(store: &Store) -> Result<Workspace, String> { store.read_latest().map_err(|e| e.to_string()) }

fn mutate<T>(store: &Store, work: impl FnOnce(&mut Workspace) -> Result<T, String>) -> Result<T, String> {
    // Failed validation aborts without writing an unchanged workspace.
    store.mutate(|workspace| work(workspace).map_err(|message| {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    })).map_err(|error| error.to_string())
}

fn execute_tool(store: &Store, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "list_notes" => {
            let args = object_args(args, &["limit"])?;
            let limit = limit(args)?;
            let workspace = load(store)?;
            let notes: Vec<_> = ordered_notes(&workspace.notes).into_iter().take(limit).map(note_summary).collect();
            Ok(json!({"total":workspace.notes.len(),"notes":notes}))
        }
        "search_notes" => {
            let args = object_args(args, &["query","limit"])?;
            let query = required_string(args, "query")?;
            if query.trim().is_empty() || query.chars().count() > MAX_SEARCH_CHARS {
                return Err(invalid("query must be 1–1000 nonblank characters"));
            }
            let query = query.to_lowercase();
            let limit = limit(args)?;
            let workspace = load(store)?;
            // Filter in source order, then sort only matches. A selective query
            // should not spend time sorting thousands of unrelated notes.
            // Stable sorting retains the original tie order used by list_notes.
            let mut found: Vec<_> = workspace.notes.iter().filter(|note| {
                note.title.to_lowercase().contains(&query) || note.body.to_lowercase().contains(&query)
            }).collect();
            sort_notes(&mut found);
            let total = found.len();
            let notes: Vec<_> = found.into_iter().take(limit).map(note_summary).collect();
            Ok(json!({"total":total,"notes":notes}))
        }
        "read_note" => {
            let args = object_args(args, &["id","title"])?;
            let (by_title, key) = selector(args, "title")?;
            let workspace = load(store)?;
            let index = find_note_index(&workspace, by_title, &key)?;
            serde_json::to_value(&workspace.notes[index]).map_err(|e| e.to_string())
        }
        "create_note" => {
            let args = object_args(args, &["title","body"])?;
            let title = optional_string(args, "title")?.unwrap_or("Untitled note");
            validate_title(title)?;
            let body = optional_string(args, "body")?.unwrap_or("");
            validate_body(body)?;
            mutate(store, |workspace| {
                let mut note = Note::create("note");
                note.title = title.into();
                note.body = body.into();
                // Avoid changing the page that someone is editing in the GUI.
                if workspace.notes.is_empty() { workspace.active_id = note.id.clone(); }
                while workspace.notes.iter().any(|other| other.id == note.id) { note = Note::create("note"); }
                workspace.notes.push(note.clone());
                serde_json::to_value(note).map_err(|e| e.to_string())
            })
        }
        "append_to_note" => {
            let args = object_args(args, &["id","title","text"])?;
            let (by_title, key) = selector(args, "title")?;
            let text = required_string(args, "text")?;
            validate_body(text)?;
            if text.is_empty() { return Err(invalid("text cannot be empty")); }
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                let separator = if note.body.is_empty() || note.body.ends_with('\n') || text.starts_with('\n') { "" } else { "\n" };
                if note.body.len() + separator.len() + text.len() > MAX_BODY_BYTES {
                    return Err(invalid("Resulting note body exceeds 1 MiB"));
                }
                note.body.push_str(separator);
                note.body.push_str(text);
                note.touch();
                Ok(json!({"id":note.id,"title":note.title,"body":note.body,"stroke_count":note.strokes.len()}))
            })
        }
        "update_note" => {
            let args = object_args(args, &["id","match_title","title","body"])?;
            let (by_title, key) = selector(args, "match_title")?;
            let title = optional_string(args, "title")?;
            let body = optional_string(args, "body")?;
            if title.is_none() && body.is_none() { return Err(invalid("Provide title and/or body to update")); }
            if let Some(title) = title { validate_title(title)?; }
            if let Some(body) = body { validate_body(body)?; }
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                if let Some(title) = title { note.title = title.into(); }
                if let Some(body) = body { note.body = body.into(); }
                note.touch();
                serde_json::to_value(note).map_err(|e| e.to_string())
            })
        }
        "rename_note" => {
            let args = object_args(args, &["id","match_title","new_title"])?;
            let (by_title, key) = selector(args, "match_title")?;
            let new_title = required_string(args, "new_title")?;
            validate_title(new_title)?;
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                note.title = new_title.into();
                note.touch();
                Ok(note_summary(note))
            })
        }
        "set_note_pin" => {
            let args = object_args(args, &["id","title","pinned"])?;
            let (by_title, key) = selector(args, "title")?;
            let pinned = required_bool(args, "pinned")?;
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                note.pinned = pinned;
                note.touch();
                Ok(note_summary(note))
            })
        }
        "set_font_size" => {
            let args = object_args(args, &["id","title","size"])?;
            let (by_title, key) = selector(args, "title")?;
            let size = required_i32(args, "size", 12, 40)?;
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                note.font_size = size;
                note.touch();
                Ok(note_summary(note))
            })
        }
        "set_font_choice" => {
            let args = object_args(args, &["id","title","choice"])?;
            let (by_title, key) = selector(args, "title")?;
            let choice = required_string(args, "choice")?;
            if !["Sans", "Serif", "Mono"].contains(&choice) {
                return Err(invalid("choice must be Sans, Serif or Mono"));
            }
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                note.font_choice = choice.into();
                note.touch();
                Ok(note_summary(note))
            })
        }
        "get_active_note" => {
            object_args(args, &[])?;
            let workspace = load(store)?;
            let note = workspace.notes.iter().find(|note| note.id == workspace.active_id);
            Ok(json!({"active_id":workspace.active_id,"note":note}))
        }
        "set_active_note" => {
            let args = object_args(args, &["id","title"])?;
            let (by_title, key) = selector(args, "title")?;
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                workspace.active_id = workspace.notes[index].id.clone();
                Ok(json!({"active_id":workspace.active_id,"title":workspace.notes[index].title,
                    "saved":true,"gui_mode_changed":false}))
            })
        }
        "export_note" => {
            let args = object_args(args, &["id","title","format","path"])?;
            let (by_title, key) = selector(args, "title")?;
            let format = required_string(args, "format")?;
            let path = required_string(args, "path")?;
            let workspace = load(store)?;
            let index = find_note_index(&workspace, by_title, &key)?;
            export_note(&workspace.notes[index], format, path)
        }
        "add_stroke" => {
            let args = object_args(args, &["id","title","width","points","color","eraser"])?;
            let (by_title, key) = selector(args, "title")?;
            let stroke = parse_stroke(args)?;
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                if note.strokes.len() >= MAX_STROKES_PER_NOTE {
                    return Err(format!("Cannot add more than {MAX_STROKES_PER_NOTE} strokes to one page"));
                }
                let added_points = stroke.points.len();
                note.strokes.push(stroke);
                note.touch();
                Ok(json!({"id":note.id,"stroke_count":note.strokes.len(),"added_points":added_points}))
            })
        }
        "undo_last_stroke" => {
            let args = object_args(args, &["id","title","confirm"])?;
            require_confirmation(args, "Removing the last drawing stroke")?;
            let (by_title, key) = selector(args, "title")?;
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                if note.strokes.pop().is_none() { return Err(invalid("No drawing strokes to undo")); }
                note.touch();
                Ok(json!({"id":note.id,"stroke_count":note.strokes.len(),"undone":true}))
            })
        }
        "clear_strokes" => {
            let args = object_args(args, &["id","title","confirm"])?;
            require_confirmation(args, "Clearing drawing strokes")?;
            let (by_title, key) = selector(args, "title")?;
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                let removed = note.strokes.len();
                note.strokes.clear();
                note.touch();
                Ok(json!({"id":note.id,"stroke_count":0,"removed_strokes":removed}))
            })
        }
        "delete_note" => {
            let args = object_args(args, &["id","title","confirm"])?;
            if args.get("confirm") != Some(&Value::Bool(true)) {
                return Err(invalid("Deletion requires explicit user authorization and confirm:true"));
            }
            let (by_title, key) = selector(args, "title")?;
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let deleted = workspace.notes.remove(index);
                if workspace.active_id == deleted.id {
                    workspace.active_id = ordered_notes(&workspace.notes).first().map(|note| note.id.clone()).unwrap_or_default();
                }
                Ok(json!({"deleted":true,"id":deleted.id,"title":deleted.title,"deleted_strokes":deleted.strokes.len()}))
            })
        }
        "get_workspace_info" => {
            object_args(args, &[])?;
            let workspace = load(store)?;
            Ok(json!({"version":workspace.version,"notes_count":workspace.notes.len(),
                "active_id":workspace.active_id,"storage_file":store.data_path().display().to_string()}))
        }
        _ => Err(format!("Unknown tool '{name}'")),
    }
}

fn run_cli(store: &Store, args: &[String]) -> io::Result<()> {
    let get = |index: usize| -> Result<&str, String> {
        args.get(index).map(String::as_str).ok_or_else(|| format!("Missing argument for {}", args[0]))
    };
    let flags = |offset: usize| -> Result<Map<String, Value>, String> {
        let mut map = Map::new();
        let mut rest = args[offset..].iter();
        while let Some(flag) = rest.next() {
            if !["--title", "--body"].contains(&flag.as_str()) { return Err(format!("Unknown option {flag}")); }
            let value = rest.next().ok_or_else(|| format!("Missing value for {flag}"))?;
            if map.insert(flag.trim_start_matches('-').into(), json!(value)).is_some() {
                return Err(format!("Duplicate option {flag}"));
            }
        }
        Ok(map)
    };
    let value = match args[0].as_str() {
        "list" if args.len() == 1 => execute_tool(store, "list_notes", &json!({})),
        "search" if args.len() >= 2 => execute_tool(store, "search_notes", &json!({"query":args[1..].join(" ")})),
        "get" if args.len() == 2 => get(1).and_then(|id| execute_tool(store, "read_note", &json!({"id":id}))),
        "create" => flags(1).and_then(|args| execute_tool(store, "create_note", &Value::Object(args))),
        "append" if args.len() >= 3 => get(1).and_then(|id| execute_tool(store, "append_to_note", &json!({"id":id,"text":args[2..].join(" ")}))),
        "update" if args.len() >= 4 => flags(2).and_then(|mut args| {
            args.insert("id".into(), json!(get(1)?));
            execute_tool(store, "update_note", &Value::Object(args))
        }),
        "rename" if args.len() == 3 => execute_tool(store, "rename_note", &json!({"id":args[1],"new_title":args[2]})),
        "pin" if args.len() == 2 => execute_tool(store, "set_note_pin", &json!({"id":args[1],"pinned":true})),
        "unpin" if args.len() == 2 => execute_tool(store, "set_note_pin", &json!({"id":args[1],"pinned":false})),
        "font-size" if args.len() == 3 => args[2].parse::<i32>().map_err(|_| invalid("font size must be a number"))
            .and_then(|size| execute_tool(store, "set_font_size", &json!({"id":args[1],"size":size}))),
        "font-choice" if args.len() == 3 => execute_tool(store, "set_font_choice", &json!({"id":args[1],"choice":args[2]})),
        "active" if args.len() == 1 => execute_tool(store, "get_active_note", &json!({})),
        "activate" if args.len() == 2 => execute_tool(store, "set_active_note", &json!({"id":args[1]})),
        "export-text" if args.len() == 3 => execute_tool(store, "export_note", &json!({"id":args[1],"format":"txt","path":args[2]})),
        "export-png" if args.len() == 3 => execute_tool(store, "export_note", &json!({"id":args[1],"format":"png","path":args[2]})),
        "add-stroke" if args.len() == 3 => cli_stroke_json(&args[2])
            .and_then(|stroke| {
                let mut fields = object_args(&stroke, &["width","points","color","eraser"])?.clone();
                fields.insert("id".into(), json!(args[1]));
                execute_tool(store, "add_stroke", &Value::Object(fields))
            }),
        "add-stroke" if args.len() >= 5 => parse_cli_stroke(args)
            .and_then(|fields| execute_tool(store, "add_stroke", &Value::Object(fields))),
        "undo-stroke" if args.len() == 3 && args[2] == "--confirm" => execute_tool(store, "undo_last_stroke", &json!({"id":args[1],"confirm":true})),
        "clear-strokes" if args.len() == 3 && args[2] == "--confirm" => execute_tool(store, "clear_strokes", &json!({"id":args[1],"confirm":true})),
        "delete" if args.len() == 3 && args[2] == "--confirm" => get(1).and_then(|id| execute_tool(store, "delete_note", &json!({"id":id,"confirm":true}))),
        "info" if args.len() == 1 => execute_tool(store, "get_workspace_info", &json!({})),
        "--help" | "-h" | "help" => {
            println!("Inkpad headless agent\n\nMCP stdio: inkpad-agent.exe [mcp]\nCLI:\n  list | search TEXT | get ID | info | active | activate ID\n  create [--title TITLE] [--body TEXT] | append ID TEXT | update ID [--title TITLE] [--body TEXT]\n  rename ID NEW_TITLE | pin ID | unpin ID | font-size ID 12..40 | font-choice ID Sans|Serif|Mono\n  export-text ID ABS_PATH.txt | export-png ID ABS_PATH.png\n  add-stroke ID WIDTH COLOR X,Y [X,Y ...] [--eraser]\n  add-stroke ID @ABS_PATH.json  (PowerShell-friendly file-backed JSON)\n  add-stroke ID STROKE_JSON (for correctly escaped inline JSON)\n  undo-stroke ID --confirm | clear-strokes ID --confirm | delete ID --confirm\n\nStorage: %LOCALAPPDATA%\\Inkpad\\workspace.json (override with INKPAD_DATA_DIR)\nNote: GUI Text/Draw view mode is transient, not a stored setting.");
            return Ok(());
        }
        _ => Err(invalid("Invalid command; run inkpad-agent --help for usage")),
    };
    match value {
        Ok(value) => { println!("{}", serde_json::to_string_pretty(&value)?); Ok(()) }
        Err(error) => Err(io::Error::new(io::ErrorKind::InvalidInput, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Stroke;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn store() -> (Store, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("inkpad-agent-test-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        (Store::in_folder(path.clone()).unwrap(), path)
    }

    fn tool(store: &Store, name: &str, args: Value) -> Value {
        execute_tool(store, name, &args).unwrap()
    }

    #[test]
    fn create_search_read_append_update_and_delete_preserve_strokes() {
        let (store, folder) = store();
        store.mutate(|workspace| {
            let mut legacy = Note::create("board");
            legacy.title = "Drawing".into();
            legacy.body = "original text".into();
            legacy.strokes.push(Stroke {color:[20,30,40,255], width:7, eraser:false, points:vec![[2,4],[5,7]]});
            workspace.active_id = legacy.id.clone();
            workspace.notes.push(legacy);
            Ok(())
        }).unwrap();
        let active = load(&store).unwrap().active_id;
        let created = tool(&store, "create_note", json!({"title":"My diary","body":"rain today"}));
        let id = created["id"].as_str().unwrap();
        assert_eq!(load(&store).unwrap().active_id, active);
        assert_eq!(tool(&store, "search_notes", json!({"query":"RAIN"}))["total"], 1);
        assert_eq!(tool(&store, "list_notes", json!({}))["total"], 3);
        assert_eq!(tool(&store, "read_note", json!({"id":id}))["body"], "rain today");
        let appended = tool(&store, "append_to_note", json!({"title":"My diary","text":"blue sky"}));
        assert_eq!(appended["body"], "rain today\nblue sky");
        let updated = tool(&store, "update_note", json!({"match_title":"Drawing","title":"Artwork","body":"new text"}));
        assert_eq!(updated["strokes"][0]["points"][1][0], 5);
        assert_eq!(updated["kind"], "board");
        assert_eq!(tool(&store, "read_note", json!({"title":"Artwork"}))["strokes"][0]["width"], 7);
        let before = load(&store).unwrap().notes.len();
        assert!(execute_tool(&store, "delete_note", &json!({"id":id})).is_err());
        assert!(execute_tool(&store, "delete_note", &json!({"id":id,"confirm":false})).is_err());
        assert_eq!(load(&store).unwrap().notes.len(), before);
        assert_eq!(tool(&store, "delete_note", json!({"id":id,"confirm":true}))["deleted"], true);
        assert_eq!(load(&store).unwrap().active_id, active);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn ambiguous_titles_are_rejected_and_invalid_input_cannot_change_storage() {
        let (store, folder) = store();
        tool(&store, "create_note", json!({"title":"Repeated"}));
        tool(&store, "create_note", json!({"title":"Repeated"}));
        let original = load(&store).unwrap().notes.len();
        assert!(execute_tool(&store, "append_to_note", &json!({"title":"Repeated","text":"x"})).is_err());
        assert!(execute_tool(&store, "delete_note", &json!({"title":"Repeated","confirm":true})).is_err());
        assert!(execute_tool(&store, "create_note", &json!({"title":"\n"})).is_err());
        assert!(execute_tool(&store, "update_note", &json!({"id":"missing","body":"x"})).is_err());
        assert!(execute_tool(&store, "create_note", &json!({"unknown":"foo"})).is_err());
        assert_eq!(load(&store).unwrap().notes.len(), original);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn jsonrpc_handshake_tool_discovery_calls_and_notifications() {
        let (store, folder) = store();
        let input = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"create_note","arguments":{"title":"From MCP","body":"hello"}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"delete_note","arguments":{"title":"From MCP"}}}),
        ];
        let bytes = input.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n") + "\n";
        let mut output = Vec::new();
        serve_mcp(&store, io::Cursor::new(bytes), &mut output).unwrap();
        let responses: Vec<Value> = String::from_utf8(output).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(responses.len(), 4);
        assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(responses[1]["result"]["tools"].as_array().unwrap().len(), 18);
        assert_eq!(responses[2]["result"]["structuredContent"]["title"], "From MCP");
        assert_eq!(responses[3]["result"]["isError"], true);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn oversized_or_malformed_messages_return_parse_errors_and_server_continues() {
        let (store, folder) = store();
        let mut input = b"not json\n".to_vec();
        input.extend(std::iter::repeat_n(b'a', MAX_MESSAGE_BYTES + 1));
        input.extend_from_slice(b"\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n");
        let mut output = Vec::new();
        serve_mcp(&store, io::Cursor::new(input), &mut output).unwrap();
        let lines: Vec<Value> = String::from_utf8(output).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["error"]["code"], -32700);
        assert_eq!(lines[1]["error"]["code"], -32700);
        assert_eq!(lines[2]["result"], json!({}));
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn persistent_session_reads_new_writes_from_another_process_store() {
        let (store, folder) = store();
        let note = tool(&store, "create_note", json!({"title":"Shared note","body":"before"}));
        let id = note["id"].as_str().unwrap();
        // Models another process, such as the GUI, saving after MCP starts.
        let concurrent_gui = Store::in_folder(folder.clone()).unwrap();
        concurrent_gui.mutate(|workspace| {
            let note = workspace.notes.iter_mut().find(|note| note.id == id).unwrap();
            note.body = "after GUI save".into();
            note.touch();
            Ok(())
        }).unwrap();
        assert_eq!(tool(&store, "read_note", json!({"id":id}))["body"], "after GUI save");
        assert_eq!(tool(&store, "search_notes", json!({"query":"after GUI"}))["total"], 1);
        assert_eq!(tool(&store, "search_notes", json!({"query":"before"}))["total"], 0);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn selective_search_large_workspace_is_correct_and_bounded() {
        let (store, folder) = store();
        store.mutate(|workspace| {
            workspace.notes.clear();
            // Approx. 3 MiB of body text plus thousands of note metadata records.
            for i in 0..1_600 {
                let mut note = Note::create("note");
                note.id = format!("perf-note-{i:05}");
                note.title = format!("Project {i:04}");
                note.body = format!("{} {}", "Project release status and milestones. ".repeat(55),
                    if i == 1_599 { "rare-tail-marker" } else { "ordinary" });
                note.updated = i as u64;
                note.pinned = i % 9 == 0;
                workspace.notes.push(note);
            }
            workspace.active_id = workspace.notes[0].id.clone();
            Ok(())
        }).unwrap();

        // Measure the actual stored-data path (lock, parse, filter, sort).
        // Deliberately generous release/debug-independent bound: detect order
        // of magnitude regressions without flaky millisecond timing gates.
        let start = Instant::now();
        for _ in 0..6 {
            let hit = tool(&store, "search_notes", json!({"query":"RARE-TAIL-MARKER"}));
            assert_eq!(hit["total"], 1);
            assert_eq!(hit["notes"][0]["id"], "perf-note-01599");
            let miss = tool(&store, "search_notes", json!({"query":"no-matches-anywhere"}));
            assert_eq!(miss["total"], 0);
            let common = tool(&store, "search_notes", json!({"query":"project","limit":3}));
            assert_eq!(common["total"], 1_600);
            assert_eq!(common["notes"].as_array().unwrap().len(), 3);
            assert_eq!(common["notes"][0]["id"], "perf-note-01593"); // newest pinned first
        }
        let elapsed = start.elapsed();
        eprintln!("Inkpad agent: 18 selective/common searches across 1600 notes completed in {elapsed:?}");
        assert!(elapsed < Duration::from_secs(15), "Large-workspace searches regressed: {elapsed:?}");
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn rename_pin_fonts_and_active_page_changes_preserve_existing_note_content() {
        let (store, folder) = store();
        let first = tool(&store, "create_note", json!({"title":"First","body":"Important text"}));
        let second = tool(&store, "create_note", json!({"title":"Second","body":"Separate page"}));
        let first_id = first["id"].as_str().unwrap();
        let second_id = second["id"].as_str().unwrap();

        let renamed = tool(&store, "rename_note", json!({"id":first_id,"new_title":"Renamed"}));
        assert_eq!(renamed["title"], "Renamed");
        let pinned = tool(&store, "set_note_pin", json!({"title":"Renamed","pinned":true}));
        assert_eq!(pinned["pinned"], true);
        assert_eq!(tool(&store, "list_notes", json!({}))["notes"][0]["id"], first_id);
        assert_eq!(tool(&store, "set_note_pin", json!({"id":first_id,"pinned":false}))["pinned"], false);
        assert_eq!(tool(&store, "set_font_size", json!({"id":first_id,"size":40}))["font_size"], 40);
        assert_eq!(tool(&store, "set_font_size", json!({"id":first_id,"size":12}))["font_size"], 12);
        for choice in ["Serif", "Mono", "Sans"] {
            assert_eq!(tool(&store, "set_font_choice", json!({"id":first_id,"choice":choice}))["font_choice"], choice);
        }
        assert_eq!(tool(&store, "get_active_note", json!({}))["note"]["title"], "Welcome to Inkpad");
        assert_eq!(tool(&store, "set_active_note", json!({"title":"Renamed"}))["active_id"], first_id);
        let active = tool(&store, "get_active_note", json!({}));
        assert_eq!(active["note"]["id"], first_id);
        assert_eq!(active["note"]["body"], "Important text");
        assert_eq!(active["note"]["font_size"], 12);
        assert_eq!(active["note"]["font_choice"], "Sans");
        assert_eq!(active["note"]["strokes"], json!([]));
        assert_eq!(tool(&store, "set_active_note", json!({"id":second_id}))["active_id"], second_id);
        assert_eq!(tool(&store, "get_workspace_info", json!({}))["active_id"], second_id);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn malformed_management_arguments_are_rejected_without_persisting() {
        let (store, folder) = store();
        let before = load(&store).unwrap();
        let id = before.notes[0].id.clone();
        for (name, args) in [
            ("rename_note", json!({"id":id,"new_title":"  "})),
            ("rename_note", json!({"id":id,"new_title":"valid","unknown":true})),
            ("set_note_pin", json!({"id":id,"pinned":"true"})),
            ("set_font_size", json!({"id":id,"size":11})),
            ("set_font_size", json!({"id":id,"size":41})),
            ("set_font_size", json!({"id":id,"size":14.5})),
            ("set_font_choice", json!({"id":id,"choice":"Comic Sans"})),
            ("get_active_note", json!({"id":id})),
            ("set_active_note", json!({"id":"missing"})),
            ("set_active_note", json!({"id":id,"title":"Welcome to Inkpad"})),
        ] {
            assert!(execute_tool(&store, name, &args).is_err(), "Should reject {name} {args}");
        }
        let after = load(&store).unwrap();
        assert_eq!(serde_json::to_value(before).unwrap(), serde_json::to_value(after).unwrap());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn validated_stroke_add_undo_clear_preserves_text_and_enforces_confirmation() {
        let (store, folder) = store();
        let created = tool(&store, "create_note", json!({"title":"Sketch","body":"Preserved text"}));
        let id = created["id"].as_str().unwrap();
        let stroke = json!({"id":id,"width":8,"color":"#23A1bf","points":[[15,16],[50,55]],"eraser":false});
        let result = tool(&store, "add_stroke", stroke);
        assert_eq!(result["stroke_count"], 1);
        let original = tool(&store, "read_note", json!({"id":id}));
        assert_eq!(original["strokes"][0]["color"], json!([35,161,191,255]));
        assert_eq!(original["strokes"][0]["points"], json!([[15,16],[50,55]]));
        assert_eq!(original["body"], "Preserved text");
        assert_eq!(tool(&store, "add_stroke", json!({"id":id,"width":4,"eraser":true,"points":[[15,16]]}))["stroke_count"], 2);
        for (name, params) in [
            ("undo_last_stroke", json!({"id":id})),
            ("clear_strokes", json!({"id":id,"confirm":false})),
            ("clear_strokes", json!({"id":id,"confirm":"true"})),
        ] {
            assert!(execute_tool(&store, name, &params).is_err(), "Should reject unconfirmed {name}");
        }
        assert_eq!(tool(&store, "read_note", json!({"id":id}))["strokes"].as_array().unwrap().len(), 2);
        assert_eq!(tool(&store, "undo_last_stroke", json!({"id":id,"confirm":true}))["stroke_count"], 1);
        assert_eq!(tool(&store, "clear_strokes", json!({"id":id,"confirm":true}))["removed_strokes"], 1);
        let final_note = tool(&store, "read_note", json!({"id":id}));
        assert_eq!(final_note["body"], "Preserved text");
        assert_eq!(final_note["strokes"], json!([]));
        assert!(execute_tool(&store, "undo_last_stroke", &json!({"id":id,"confirm":true})).is_err());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn drawing_rejects_bad_points_width_color_type_and_length_without_mutation() {
        let (store, folder) = store();
        let created = tool(&store, "create_note", json!({"title":"Bounds"}));
        let id = created["id"].as_str().unwrap();
        for patch in [
            json!({"width":0,"points":[[0,0]]}),
            json!({"width":41,"points":[[0,0]]}),
            json!({"width":2,"points":[]}),
            json!({"width":2,"points":[[WIDTH,0]]}),
            json!({"width":2,"points":[[0,HEIGHT]]}),
            json!({"width":2,"points":[[-1,0]]}),
            json!({"width":2,"points":[[1.2,1]]}),
            json!({"width":2,"points":[[1,2,3]]}),
            json!({"width":2,"points":[[1,"0"]]}),
            json!({"width":2,"points":[[0,0]],"color":"blue"}),
            json!({"width":2,"points":[[0,0]],"eraser":"false"}),
            json!({"width":2,"points":[[0,0]],"strange":true}),
            json!({"width":2,"points":vec![json!([0,0]);MAX_STROKE_POINTS + 1]}),
        ] {
            let mut obj = patch.as_object().unwrap().clone();
            obj.insert("id".into(), json!(id));
            assert!(execute_tool(&store, "add_stroke", &Value::Object(obj)).is_err(), "Accepted invalid {patch}");
        }
        assert!(tool(&store, "read_note", json!({"id":id}))["strokes"].as_array().unwrap().is_empty());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn exports_text_and_png_without_overwriting_files_or_changing_notes() {
        let (store, folder) = store();
        let created = tool(&store, "create_note", json!({"title":"Exported","body":"Hi 🌎\nNext line"}));
        let id = created["id"].as_str().unwrap();
        tool(&store, "add_stroke", json!({"id":id,"width":4,"points":[[30,40],[31,41]]}));
        let prior = tool(&store, "read_note", json!({"id":id}));
        let txt = folder.join("note.txt");
        let png = folder.join("sketch.png");
        let txt_path = txt.display().to_string();
        let png_path = png.display().to_string();
        let written = tool(&store, "export_note", json!({"id":id,"format":"txt","path":txt_path}));
        assert_eq!(written["format"], "txt");
        assert_eq!(std::fs::read_to_string(&txt).unwrap(), "Hi 🌎\nNext line");
        let drawn = tool(&store, "export_note", json!({"id":id,"format":"png","path":png_path}));
        assert_eq!(drawn["width"], WIDTH);
        assert_eq!(drawn["height"], HEIGHT);
        assert_eq!(&std::fs::read(&png).unwrap()[..8], &[137,80,78,71,13,10,26,10]);
        for (format, path) in [("txt", &txt_path), ("png", &png_path)] {
            assert!(execute_tool(&store, "export_note", &json!({"id":id,"format":format,"path":path})).is_err());
        }
        assert!(execute_tool(&store, "export_note", &json!({"id":id,"format":"txt","path":png_path})).is_err());
        assert!(execute_tool(&store, "export_note", &json!({"id":id,"format":"html","path":txt_path})).is_err());
        assert!(execute_tool(&store, "export_note", &json!({"id":id,"format":"txt","path":"relative.txt"})).is_err());
        assert_eq!(tool(&store, "read_note", json!({"id":id})), prior);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn cli_stroke_supports_safe_file_input_and_powershell_friendly_coordinates() {
        let (store, folder) = store();
        let note = tool(&store, "create_note", json!({"title":"CLI Drawing"}));
        let id = note["id"].as_str().unwrap();
        let json_file = folder.join("stroke.json");
        std::fs::write(&json_file, br##"{"width":5,"color":"#2288BB","points":[[7,8],[15,16]]}"##).unwrap();
        let from_file = cli_stroke_json(&format!("@{}", json_file.display())).unwrap();
        let mut object = from_file.as_object().unwrap().clone();
        object.insert("id".into(), json!(id));
        assert_eq!(execute_tool(&store, "add_stroke", &Value::Object(object)).unwrap()["stroke_count"], 1);
        let args = vec!["add-stroke",id,"7","#AABBCC","20,30","100,120","--eraser"]
            .into_iter().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(execute_tool(&store, "add_stroke", &Value::Object(parse_cli_stroke(&args).unwrap())).unwrap()["stroke_count"], 2);
        let saved = tool(&store, "read_note", json!({"id":id}));
        assert_eq!(saved["strokes"][1]["eraser"], true);
        assert_eq!(saved["strokes"][1]["points"], json!([[20,30],[100,120]]));
        assert!(parse_cli_stroke(&vec!["add-stroke",id,"7","#AABBCC","bad-coordinate"].into_iter().map(str::to_string).collect::<Vec<_>>()).is_err());
        assert!(cli_stroke_json("@relative.json").is_err());
        let huge = folder.join("too-large.json");
        std::fs::write(&huge, vec![b' '; MAX_STROKE_JSON_BYTES + 1]).unwrap();
        assert!(cli_stroke_json(&format!("@{}", huge.display())).is_err());
        std::fs::write(&json_file, b"\xef\xbb\xbf{\"width\":6,\"color\":\"#001122\",\"points\":[[3,4]]}").unwrap();
        assert_eq!(cli_stroke_json(&format!("@{}", json_file.display())).unwrap()["width"], 6);
        std::fs::remove_dir_all(folder).unwrap();
    }
}
