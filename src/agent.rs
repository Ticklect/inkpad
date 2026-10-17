//! MCP stdio and command-line interface to the Inkpad workspace.
//! Storage mutations use the cross-process transaction lock in `Store`.

use crate::canvas::{Board, HEIGHT, WIDTH};
use crate::store::{DrawShape, DrawStyle, DrawingData, Point, Transform};
use crate::store::drawing::{fresh_id as fresh_drawing_id, DrawObject, Layer, MAX_OBJECTS, MAX_LAYERS};
use crate::settings::{AppearanceTheme, UiSettings};
use crate::store::{Note, Store, Stroke, TextRun, TextStylePatch, Workspace};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fs::OpenOptions;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

const MAX_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_TITLE_CHARS: usize = 200;
const MAX_SEARCH_CHARS: usize = 1000;
const MAX_RESULTS: usize = 200;
const MAX_SEARCH_WITH_BODY_RESULTS: usize = 10;
const MAX_SEARCH_BODY_CHARS: usize = 16_000;
const MAX_STROKE_POINTS: usize = 512;
const MAX_STROKES_PER_NOTE: usize = 2_000;
const MAX_STROKE_JSON_BYTES: usize = 128 * 1024;
const MAX_IMAGE_FILE_BYTES: u64 = 12 * 1024 * 1024;
const MAX_IMAGE_BASE64_BYTES: usize = 1024 * 1024;
const MAX_IMAGE_CAPTION_CHARS: usize = 500;
const MAX_BATCH_FORMAT_OPS: usize = 32;
const MAX_BATCH_NOTE_OPS: usize = 32;
const MAX_BATCH_DRAW_OBJECTS: usize = 64;
const MAX_SCENE_DRAW_OBJECTS: usize = 512;
const MAX_SCENE_DRAW_LAYERS: usize = 16;
const MAX_BATCH_STROKES: usize = 64;
const MAX_FRAGMENT_TEXT_BYTES: usize = 256 * 1024;
const MAX_FRAGMENT_RUNS: usize = 2048;
const MAX_DRAWING_PREVIEW_BYTES: usize = 1024 * 1024;
const DRAWING_HISTORY_LIMIT: usize = 48;

/// Stdio-session history is guarded against another process changing the saved
/// drawing. The snapshots are deliberately not written into workspace.json.
#[derive(Default)]
struct DrawingSession {
    expected: Option<DrawingData>,
    undo: Vec<DrawingData>,
    redo: Vec<DrawingData>,
}
static DRAWING_SESSIONS: OnceLock<Mutex<HashMap<String, DrawingSession>>> = OnceLock::new();
fn drawing_sessions() -> &'static Mutex<HashMap<String, DrawingSession>> {
    DRAWING_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn drawing_session_key(store: &Store, note_id: &str) -> String {
    format!("{}|{note_id}", store.data_path().display())
}

/// At most four process-local PNG frames (<= 1 MiB each). Each lookup starts
/// from Store's fresh saved-note snapshot; the exact persisted strokes and
/// vector data fingerprint makes outside GUI/MCP edits invalidate naturally.
struct CachedDrawingPreview {
    key: String,
    fingerprint: [u8; 32],
    png: Vec<u8>,
}
static DRAWING_PREVIEWS: OnceLock<Mutex<Vec<CachedDrawingPreview>>> = OnceLock::new();
fn drawing_previews() -> &'static Mutex<Vec<CachedDrawingPreview>> {
    DRAWING_PREVIEWS.get_or_init(|| Mutex::new(Vec::with_capacity(4)))
}
fn drawing_preview_png(store: &Store, note: &Note) -> Result<Vec<u8>, String> {
    // Never hash a source filename or last-write timestamp: external writers
    // can change JSON bytes without changing either timestamp or file length.
    let source = serde_json::to_vec(&(&note.strokes, &note.drawing))
        .map_err(|error| format!("Cannot fingerprint saved drawing: {error}"))?;
    let fingerprint = *blake3::hash(&source).as_bytes();
    let key = drawing_session_key(store, &note.id);
    if let Ok(mut cache) = drawing_previews().lock() {
        if let Some(index) = cache.iter().position(|entry|
            entry.key == key && entry.fingerprint == fingerprint) {
            let hit = cache.remove(index);
            let png = hit.png.clone();
            cache.push(hit); // simple LRU: recent entries evict last
            return Ok(png);
        }
    }
    let png = Board::from_note(note).png_bytes()
        .map_err(|error| format!("Drawing render failed: {error}"))?;
    if png.len() > MAX_DRAWING_PREVIEW_BYTES {
        return Err(invalid("Rendered preview exceeds 1 MiB; export the drawing as a PNG file instead"));
    }
    if let Ok(mut cache) = drawing_previews().lock() {
        // Avoid duplicate entries after concurrent identical preview requests.
        cache.retain(|entry| entry.key != key);
        if cache.len() == 4 { cache.remove(0); }
        cache.push(CachedDrawingPreview { key, fingerprint, png: png.clone() });
    }
    Ok(png)
}
fn record_drawing_change(store: &Store, id: &str, before: DrawingData, after: DrawingData) {
    if before == after { return; }
    let Ok(mut sessions) = drawing_sessions().lock() else { return; };
    let session = sessions.entry(drawing_session_key(store, id)).or_default();
    if session.expected.as_ref().is_some_and(|previous| *previous != before) {
        session.undo.clear();
        session.redo.clear();
    }
    session.undo.push(before);
    if session.undo.len() > DRAWING_HISTORY_LIMIT { session.undo.remove(0); }
    session.redo.clear();
    session.expected = Some(after);
}

fn drawing_change(store: &Store, args: &Map<String, Value>,
    change: impl FnOnce(&mut DrawingData) -> Result<Value, String>) -> Result<Value, String> {
    let (by_title, key) = selector(args, "title")?;
    let (result, id, before, after) = mutate(store, |workspace| {
        let index = find_note_index(workspace, by_title, &key)?;
        let note = &mut workspace.notes[index];
        let before = note.drawing.snapshot();
        let result = change(&mut note.drawing)?;
        note.drawing.validate()?;
        let after = note.drawing.snapshot();
        if before != after { note.touch(); }
        Ok((result, note.id.clone(), before, after))
    })?;
    record_drawing_change(store, &id, before, after);
    Ok(result)
}

/// One validated drawing object to be appended in a single Store transaction.
/// The surrounding note and drawing are committed together, after every item
/// has passed its per-object and final drawing integrity checks.
struct BatchDrawObject {
    shape: DrawShape,
    style: DrawStyle,
    transform: Transform,
    layer_id: Option<String>,
}

fn execute_drawing_create_batch(store: &Store, args: &Value) -> Result<Value, String> {
    let args = object_args(args, &["id", "title", "layer_id", "expected_object_count", "objects"])?;
    let objects = args.get("objects").and_then(Value::as_array)
        .ok_or_else(|| invalid("objects must be an array of 1–64 editable shapes"))?;
    if objects.is_empty() || objects.len() > MAX_BATCH_DRAW_OBJECTS {
        return Err(invalid("objects must contain 1–64 editable shapes"));
    }
    let default_layer = optional_string(args, "layer_id")?;
    if default_layer.is_some() { drawing_id(args, "layer_id")?; }
    let expected_object_count = args.get("expected_object_count").map(|value| {
        value.as_u64().filter(|count| *count <= MAX_OBJECTS as u64)
            .map(|count| count as usize)
            .ok_or_else(|| invalid("expected_object_count must be an integer from 0 to 2000"))
    }).transpose()?;
    let prepared = objects.iter().enumerate().map(|(index, value)| {
        let parse = || -> Result<BatchDrawObject, String> {
            let item = object_args(value, &["shape", "style", "transform", "layer_id"])?;
            let shape = draw_shape(item, "shape")?;
            let style = optional_draw_style(item)?.unwrap_or_default();
            let transform = optional_draw_transform(item)?.unwrap_or_default();
            let layer_id = optional_string(item, "layer_id")?.or(default_layer);
            if item.contains_key("layer_id") { drawing_id(item, "layer_id")?; }
            Ok(BatchDrawObject { shape, style, transform, layer_id: layer_id.map(str::to_string) })
        };
        parse().map_err(|error| format!("Object {index}: {error}"))
    }).collect::<Result<Vec<_>, _>>()?;

    drawing_change(store, args, move |drawing| {
        if expected_object_count.is_some_and(|expected| expected != drawing.objects.len()) {
            return Err(invalid("expected_object_count does not match the saved drawing; refresh before adding artwork"));
        }
        if drawing.objects.len().saturating_add(prepared.len()) > MAX_OBJECTS {
            return Err(format!("Cannot exceed {MAX_OBJECTS} editable objects on one page"));
        }
        let mut ids = Vec::with_capacity(prepared.len());
        for (index, item) in prepared.into_iter().enumerate() {
            let add = || -> Result<DrawObject, String> {
                let layer_id = match item.layer_id {
                    Some(id) => id,
                    None if drawing.layers.is_empty() => {
                        let id = fresh_drawing_id("layer");
                        drawing.layers.push(Layer { id: id.clone(), name: "Layer 1".into(), visible: true, locked: false });
                        id
                    }
                    None => drawing.layers.last().expect("drawing contains layer").id.clone(),
                };
                let layer = drawing.layers.iter().find(|layer| layer.id == layer_id)
                    .ok_or_else(|| invalid("Drawing layer not found"))?;
                if layer.locked { return Err(invalid("Drawing layer is locked")); }
                // Use the same shared ID generator as DrawingData::add_object,
                // and also reject collisions with preexisting legacy IDs.
                let mut id = fresh_drawing_id("obj");
                while drawing.objects.iter().any(|object| object.id == id) || ids.iter().any(|other| other == &id) {
                    id = fresh_drawing_id("obj");
                }
                let object = DrawObject { id, layer_id, shape: item.shape,
                    style: item.style, transform: item.transform, visible: true, group_id: None };
                object.validate()?;
                Ok(object)
            };
            let object = add().map_err(|error| format!("Object {index}: {error}"))?;
            ids.push(object.id.clone());
            drawing.objects.push(object);
        }
        Ok(json!({"object_ids":ids,"added_objects":ids.len(),
            "object_count":drawing.objects.len(),"layer_count":drawing.layers.len()}))
    })
}

/// Create a complete multi-layer vector scene without a per-layer or
/// per-object MCP round trip. All layer and object validation occurs before
/// the single workspace commit. A scene is ONE undo step; note text and media
/// are never altered. Layers created by this call use array indexes in object
/// definitions, avoiding a preliminary layer-ID discovery round trip.
fn execute_drawing_create_scene(store: &Store, args: &Value) -> Result<Value, String> {
    let args = object_args(args, &["id", "title", "expected_object_count", "layers", "objects"])?;
    let layers = match args.get("layers") {
        None => &[][..],
        Some(Value::Array(items)) if items.len() <= MAX_SCENE_DRAW_LAYERS => items.as_slice(),
        _ => return Err(invalid("layers must be an array of at most 16 named layers")),
    };
    let mut names = Vec::with_capacity(layers.len());
    for (index, value) in layers.iter().enumerate() {
        let item = object_args(value, &["name"])
            .map_err(|error| format!("Layer {index}: {error}"))?;
        let name = required_string(item, "name")?;
        if name.trim().is_empty() || name.chars().count() > 80 || name.contains('\0') {
            return Err(format!("Layer {index}: name must contain 1–80 characters without NUL"));
        }
        names.push(name.to_owned());
    }
    let objects = args.get("objects").and_then(Value::as_array)
        .filter(|items| !items.is_empty() && items.len() <= MAX_SCENE_DRAW_OBJECTS)
        .ok_or_else(|| invalid("objects must contain 1–512 editable shapes"))?;
    let expected = args.get("expected_object_count").map(|value| {
        value.as_u64().filter(|count| *count <= MAX_OBJECTS as u64)
            .map(|count| count as usize)
            .ok_or_else(|| invalid("expected_object_count must be an integer from 0 to 2000"))
    }).transpose()?;
    let prepared = objects.iter().enumerate().map(|(index, value)| {
        let parse = || -> Result<(BatchDrawObject, Option<usize>), String> {
            let item = object_args(value, &["shape", "style", "transform", "layer_id", "layer_index"])?;
            if item.contains_key("layer_id") && item.contains_key("layer_index") {
                return Err(invalid("Provide only layer_id or layer_index, not both"));
            }
            let layer_index = item.get("layer_index").map(|value| {
                let index = value.as_u64().and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| invalid("layer_index must be a non-negative integer"))?;
                if index >= names.len() { return Err(invalid("layer_index must reference one of this scene's new layers")); }
                Ok(index)
            }).transpose()?;
            let layer_id = optional_string(item, "layer_id")?;
            if layer_id.is_some() { drawing_id(item, "layer_id")?; }
            Ok((BatchDrawObject {
                shape:draw_shape(item, "shape")?,
                style:optional_draw_style(item)?.unwrap_or_default(),
                transform:optional_draw_transform(item)?.unwrap_or_default(),
                layer_id:layer_id.map(str::to_string),
            }, layer_index))
        };
        parse().map_err(|error| format!("Object {index}: {error}"))
    }).collect::<Result<Vec<_>, _>>()?;
    drawing_change(store, args, move |drawing| {
        if expected.is_some_and(|count| count != drawing.objects.len()) {
            return Err(invalid("expected_object_count does not match the saved drawing; refresh before adding artwork"));
        }
        if drawing.layers.len().saturating_add(names.len()) > MAX_LAYERS {
            return Err(invalid("Scene would exceed 64 drawing layers"));
        }
        if drawing.objects.len().saturating_add(prepared.len()) > MAX_OBJECTS {
            return Err(invalid("Scene would exceed 2000 drawing objects"));
        }
        let mut layer_ids = Vec::with_capacity(names.len());
        for name in names {
            let mut id = fresh_drawing_id("layer");
            while drawing.layers.iter().any(|layer| layer.id == id) || layer_ids.contains(&id) {
                id = fresh_drawing_id("layer");
            }
            drawing.layers.push(Layer {id:id.clone(),name,visible:true,locked:false});
            layer_ids.push(id);
        }
        let mut object_ids = Vec::with_capacity(prepared.len());
        for (index, (item, layer_index)) in prepared.into_iter().enumerate() {
            let layer_id = match (layer_index, item.layer_id) {
                (Some(index), None) => layer_ids[index].clone(),
                (None, Some(id)) => id,
                (None, None) if drawing.layers.is_empty() => {
                    let id = fresh_drawing_id("layer");
                    drawing.layers.push(Layer {id:id.clone(),name:"Layer 1".into(),visible:true,locked:false});
                    id
                },
                (None, None) => drawing.layers.last().expect("at least one layer exists").id.clone(),
                (Some(_), Some(_)) => unreachable!("exclusive source validation"),
            };
            let layer = drawing.layers.iter().find(|layer| layer.id == layer_id)
                .ok_or_else(|| format!("Object {index}: referenced drawing layer not found"))?;
            if layer.locked { return Err(format!("Object {index}: drawing layer is locked")); }
            let mut id = fresh_drawing_id("obj");
            while drawing.objects.iter().any(|object| object.id == id) || object_ids.contains(&id) {
                id = fresh_drawing_id("obj");
            }
            let object = DrawObject {id:id.clone(),layer_id,shape:item.shape,
                style:item.style,transform:item.transform,visible:true,group_id:None};
            object.validate().map_err(|error| format!("Object {index}: {error}"))?;
            drawing.objects.push(object);
            object_ids.push(id);
        }
        Ok(json!({"object_ids":object_ids,"created_layer_ids":layer_ids,
            "added_objects":object_ids.len(),"added_layers":layer_ids.len(),
            "object_count":drawing.objects.len(),"layer_count":drawing.layers.len()}))
    })
}

fn drawing_id<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    let id = required_string(args, key)?;
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|ch| ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'_') {
        return Err(format!("{key} must be a valid drawing ID (1–128 ASCII letters, digits, hyphens, underscores)"));
    }
    Ok(id)
}

fn drawing_value<T: serde::de::DeserializeOwned>(args: &Map<String, Value>, key: &str) -> Result<Option<T>, String> {
    args.get(key).map(|value| serde_json::from_value(value.clone())
        .map_err(|e| format!("Invalid {key}: {e}"))).transpose()
}

fn draw_shape(args: &Map<String, Value>, key: &str) -> Result<DrawShape, String> {
    let value = args.get(key).ok_or_else(|| format!("{key} is required"))?;
    let map = value.as_object().ok_or_else(|| format!("{key} must be a shape object"))?;
    let kind = map.get("type").and_then(Value::as_str).ok_or_else(|| invalid("shape.type is required"))?;
    let allowed: &[&str] = match kind {
        "rectangle" => &["type", "width", "height"],
        "rounded_rectangle" => &["type", "width", "height", "radius"],
        "ellipse" => &["type", "radius_x", "radius_y"],
        "circle" => &["type", "radius"],
        "line" => &["type", "to"],
        "polygon" => &["type", "points", "closed"],
        "cubic_bezier" => &["type", "control1", "control2", "to"],
        _ => return Err(invalid("shape.type must be rectangle, rounded_rectangle, ellipse, circle, line, polygon or cubic_bezier")),
    };
    if let Some(unknown) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("Unknown shape property: {unknown}"));
    }
    serde_json::from_value(value.clone()).map_err(|e| format!("Invalid shape: {e}"))
}

fn optional_draw_style(args: &Map<String, Value>) -> Result<Option<DrawStyle>, String> {
    if let Some(value) = args.get("style") {
        let map = value.as_object().ok_or_else(|| invalid("style must be an object"))?;
        if let Some(unknown) = map.keys().find(|key| !["stroke", "stroke_width", "fill", "opacity"].contains(&key.as_str())) {
            return Err(format!("Unknown style property: {unknown}"));
        }
    }
    let style: Option<DrawStyle> = drawing_value(args, "style")?;
    if let Some(style) = &style { style.validate()?; }
    Ok(style)
}

fn optional_draw_transform(args: &Map<String, Value>) -> Result<Option<Transform>, String> {
    if let Some(value) = args.get("transform") {
        let map = value.as_object().ok_or_else(|| invalid("transform must be an object"))?;
        if let Some(unknown) = map.keys().find(|key| !["translation", "scale_x", "scale_y", "rotation_deg", "flip_x", "flip_y"].contains(&key.as_str())) {
            return Err(format!("Unknown transform property: {unknown}"));
        }
    }
    let transform: Option<Transform> = drawing_value(args, "transform")?;
    if let Some(transform) = &transform { transform.validate()?; }
    Ok(transform)
}

fn draw_number(args: &Map<String, Value>, key: &str, min: f64, max: f64) -> Result<f32, String> {
    let value = args.get(key).and_then(Value::as_f64)
        .filter(|x| x.is_finite() && *x >= min && *x <= max)
        .ok_or_else(|| format!("{key} must be a finite number between {min} and {max}"))?;
    Ok(value as f32)
}

fn drawing_capabilities() -> Value {
    json!({
        "agent_transport":"local_stdio_mcp", "agent_drawing_model":"editable_vector_objects_v1",
        "canvas":{"width":WIDTH,"height":HEIGHT,"coordinates":"pixels"},
        "model":{"shapes":["rectangle","rounded_rectangle","ellipse","circle","line","polygon","cubic_bezier"],
            "style":["rgba_stroke","stroke_width","solid_fill","linear_gradient","radial_gradient","opacity"],
            "transforms":["move","resize","rotate","flip","group_move","group_resize","group_rotate","group_flip"],
            "layers":["create","rename","reorder","visibility","lock","delete_empty"],
            "object_management":["create","edit","reorder","delete","group","ungroup"],
            "history":"last 48 MCP drawing edits in this stdio process only; rejects undo/redo after external drawing changes"},
        "mcp":{"drawing_tools":["drawing_list","drawing_create_shape","drawing_create_batch","drawing_create_scene","drawing_edit_shape","drawing_transform_object",
            "drawing_reorder_object","drawing_delete_object","drawing_group_objects","drawing_ungroup_objects",
            "drawing_create_layer","drawing_edit_layer","drawing_move_layer","drawing_delete_layer",
            "drawing_transform_group","drawing_pick_object","drawing_preview","drawing_undo","drawing_redo"],
            "batch_drawing_commands":true,"max_batch_shapes":MAX_BATCH_DRAW_OBJECTS,
            "max_scene_shapes":MAX_SCENE_DRAW_OBJECTS,"max_scene_layers":MAX_SCENE_DRAW_LAYERS,
            "batch_is_atomic":true,"operates_on_saved_workspace":true,"opens_gui":false},
        "gui":{"drawing_model_shared":true,"interactive_acceptance":"requires GUI verification",
            "mcp_cannot_control":["active_tool","editor_view","selection","open_dialogs"]},
        "legacy":{"freehand_strokes_preserved":true,"inline_images_preserved":true,"rich_text_preserved":true,
            "stroke_undo":"undo_last_stroke with explicit confirmation; separate from object undo"},
        "destructive_actions":"Human authorization required. Deletions and drawing history undo/redo require confirm:true."
    })
}

/// In-memory rich fragment for a client-owned clipboard. Never reads or writes
/// the OS clipboard and never stores an undo history beside the workspace.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RichFragment {
    format_version: u8,
    unit: String,
    text: String,
    runs: Vec<TextRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    defaults: Option<FragmentDefaults>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FragmentDefaults {
    size: i32,
    font_family: String,
}

/// Local image paths are access to files outside Inkpad; the MCP client must
/// attest that the user explicitly selected the file. This is not a technical
/// verification of user consent, so the calling assistant must never invent it.
fn image_approval(args: &Map<String, Value>, action: &str) -> Result<(), String> {
    if args.get("user_approved") == Some(&Value::Bool(true)) { Ok(()) }
    else { Err(format!("{action} requires explicit user approval and user_approved:true")) }
}

fn image_extension(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        matches!(ext.to_str().map(str::to_ascii_lowercase).as_deref(), Some("png"|"jpg"|"jpeg"|"gif"|"webp"))
    })
}

/// Strictly allow only a file explicitly supplied by the user: reject relative
/// paths, parent traversal, special files and the notebook's managed data dir.
fn approved_image_path(store: &Store, supplied: &str) -> Result<PathBuf, String> {
    if supplied.is_empty() || supplied.len() > 4096 || supplied.chars().any(char::is_control) {
        return Err(invalid("Image path must be an explicit local path (max 4096 chars)"));
    }
    let path = Path::new(supplied);
    if !path.is_absolute() || path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        return Err(invalid("Image source must be an absolute path without '..' traversal"));
    }
    if !image_extension(path) {
        return Err(invalid("Image source must be .png, .jpg, .jpeg, .gif or .webp"));
    }
    let canonical = path.canonicalize().map_err(|e| format!("Image file cannot be opened: {e}"))?;
    if !canonical.is_file() || !image_extension(&canonical) {
        return Err(invalid("Image source must be a regular image file"));
    }
    let private_dir = store.data_path().parent()
        .ok_or_else(|| invalid("Inkpad storage folder unavailable"))?.canonicalize()
        .map_err(|e| format!("Inkpad storage folder unavailable: {e}"))?;
    if canonical.starts_with(private_dir) {
        return Err(invalid("Do not import from Inkpad's private workspace/media directory"));
    }
    let size = canonical.metadata().map_err(|e| format!("Cannot inspect image size: {e}"))?.len();
    if size == 0 || size > MAX_IMAGE_FILE_BYTES {
        return Err(format!("Image must be between 1 byte and {} MiB", MAX_IMAGE_FILE_BYTES / 1024 / 1024));
    }
    // Cheap magic-number check before Store's strict full decode/re-encode.
    let mut reader = OpenOptions::new().read(true).open(&canonical).map_err(|e| e.to_string())?;
    let mut header = [0u8; 16];
    let count = reader.read(&mut header).map_err(|e| e.to_string())?;
    if !recognised_image(&header[..count]) { return Err(invalid("File does not contain recognised PNG/JPEG/GIF/WebP image data")); }
    Ok(canonical)
}

fn recognised_image(data: &[u8]) -> bool {
    data.starts_with(b"\x89PNG\r\n\x1a\n")
        || data.starts_with(&[0xFF, 0xD8, 0xFF])
        || data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")
        || (data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP")
}

/// Bounded, strict RFC 4648 base64 parser. This avoids another Rust crate and
/// prevents agent-supplied data from allocating unbounded decoded buffers.
fn decode_image_base64(encoded: &str) -> Result<Vec<u8>, String> {
    if encoded.is_empty() || encoded.len() % 4 != 0 || encoded.len() > ((MAX_IMAGE_BASE64_BYTES + 2) / 3) * 4 {
        return Err(invalid("Image base64 must be nonempty, correctly padded and decode to at most 1 MiB"));
    }
    let mut output = Vec::with_capacity(encoded.len() / 4 * 3);
    let blocks = encoded.as_bytes().chunks_exact(4);
    let count = blocks.len();
    for (index, block) in blocks.enumerate() {
        let mut value = [0u8; 4];
        let mut padding = 0;
        for (offset, byte) in block.iter().copied().enumerate() {
            value[offset] = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' if index + 1 == count && offset >= 2 => { padding += 1; 0 },
                _ => return Err(invalid("Invalid image base64 character or padding")),
            };
        }
        if (block[2] == b'=' && block[3] != b'=') || padding > 2 {
            return Err(invalid("Invalid image base64 padding"));
        }
        if (padding == 1 && (value[2] & 0x03) != 0) || (padding == 2 && (value[1] & 0x0f) != 0) {
            return Err(invalid("Invalid nonzero base64 padding bits"));
        }
        output.push((value[0] << 2) | (value[1] >> 4));
        if padding < 2 { output.push((value[1] << 4) | (value[2] >> 2)); }
        if padding == 0 { output.push((value[2] << 6) | value[3]); }
    }
    if output.len() > MAX_IMAGE_BASE64_BYTES || !recognised_image(&output) {
        return Err(invalid("Decoded data is too large or is not a PNG/JPEG/GIF/WebP image"));
    }
    Ok(output)
}

fn image_identity(args: &Map<String, Value>) -> Result<&str, String> {
    let id = required_string(args, "image_id")?;
    if id.is_empty() || id.len() > 160 || id.chars().any(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '_') {
        return Err(invalid("image_id must be an Inkpad image ID, not a file path"));
    }
    Ok(id)
}

fn resolve_note_id(store: &Store, args: &Map<String, Value>) -> Result<String, String> {
    let (by_title, key) = selector(args, "title")?;
    let workspace = load(store)?;
    let index = find_note_index(&workspace, by_title, &key)?;
    Ok(workspace.notes[index].id.clone())
}

fn new_export_destination(store: &Store, supplied: &str, extension: &str) -> Result<PathBuf, String> {
    if supplied.len() > 4096 || supplied.is_empty() || supplied.chars().any(char::is_control) {
        return Err(invalid("Export destination must be a local absolute file path"));
    }
    let path = Path::new(supplied);
    if !path.is_absolute() || path.components().any(|part| matches!(part, std::path::Component::ParentDir)) || path.file_name().is_none() {
        return Err(invalid("Export requires an absolute destination without '..' traversal"));
    }
    if !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case(extension)) {
        return Err(format!("Export must use a .{extension} filename"));
    }
    if path.exists() { return Err(invalid("Export destination already exists; overwriting is not permitted")); }
    let parent = path.parent().ok_or_else(|| invalid("No HTML destination parent"))?
        .canonicalize().map_err(|e| format!("Export destination folder is unavailable: {e}"))?;
    let private_dir = store.data_path().parent().ok_or_else(|| invalid("Private workspace path unavailable"))?
        .canonicalize().map_err(|e| format!("Private workspace path unavailable: {e}"))?;
    if parent.starts_with(private_dir) { return Err(invalid("Do not export inside Inkpad's managed data folder")); }
    Ok(parent.join(path.file_name().unwrap()))
}

fn note_image(store: &Store, note_id: &str, image_id: &str) -> Result<crate::store::MediaBlock, String> {
    store.list_media(note_id).map_err(|e| e.to_string())?.into_iter()
        .find(|image| image.id == image_id).ok_or_else(|| invalid("Image was not found in the selected note"))
}

fn read_image_file_bounded(store: &Store, image: &crate::store::MediaBlock) -> Result<Vec<u8>, String> {
    let path = store.media_path(image).map_err(|e| e.to_string())?;
    let mut file = OpenOptions::new().read(true).open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take((MAX_IMAGE_BASE64_BYTES + 1) as u64).read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_IMAGE_BASE64_BYTES {
        return Err(invalid("This picture is over 1 MiB; use export_image to save it to an approved path instead"));
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(invalid("Stored image is not a valid PNG asset"));
    }
    Ok(bytes)
}

fn cli_base64_arg(value: &str) -> Result<String, String> {
    if let Some(filepath) = value.strip_prefix('@') {
        let path = Path::new(filepath);
        if !path.is_absolute() || path.components().any(|part| matches!(part, std::path::Component::ParentDir))
            || !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("b64") || ext.eq_ignore_ascii_case("txt")) {
            return Err(invalid("Base64 CLI file must be an absolute .b64 or .txt path without traversal"));
        }
        let mut bytes = Vec::new();
        OpenOptions::new().read(true).open(path).map_err(|e| e.to_string())?
            .take(((MAX_IMAGE_BASE64_BYTES + 2) / 3 * 4 + 1) as u64)
            .read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        if bytes.len() > ((MAX_IMAGE_BASE64_BYTES + 2) / 3) * 4 {
            return Err(invalid("Encoded image exceeds the 1 MiB decoded-input limit"));
        }
        String::from_utf8(bytes).map_err(|_| invalid("Encoded image must be UTF-8 base64 text"))
    } else { Ok(value.to_string()) }
}

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
fn read_message<R: BufRead>(reader: &mut R, bytes: &mut Vec<u8>) -> io::Result<Option<Result<Value, String>>> {
    // Most MCP requests are small. Reuse the input allocation across a
    // persistent connection instead of allocating for every round trip.
    bytes.clear();
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
    let mut message_bytes = Vec::with_capacity(8 * 1024);
    while let Some(message) = read_message(&mut input, &mut message_bytes)? {
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
                "instructions":"Headless access to saved Inkpad text, drawing strokes and inline images, including rename, image sizing, placement, captions and self-contained HTML export. Local image import or exporting notes must be explicitly user-approved; image deletion, note deletion, stroke removal and clearing require explicit user authorization plus confirm=true. GUI Text/Draw view mode is transient."
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
    if value.get("mime_type").and_then(Value::as_str) == Some("image/png") {
        if let Some(encoded) = value.get("data_base64").and_then(Value::as_str) {
            let mut metadata = value.clone();
            metadata.as_object_mut().unwrap().remove("data_base64");
            // Standard MCP image content: omit a duplicate ~1 MiB base64 text
            // block, while keeping lightweight IDs/size in structuredContent.
            return json!({"content":[{"type":"image","mimeType":"image/png","data":encoded}],
                "structuredContent":metadata,"isError":false});
        }
    }
    // MCP returns the same machine-readable structuredContent. A compact text
    // mirror avoids a second expensive pretty-print and cuts pipe traffic.
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "null".into());
    json!({"content":[{"type":"text","text":text}],"structuredContent":value,"isError":false})
}

fn tool_definitions() -> &'static Vec<Value> {
    static TOOL_DEFINITIONS: OnceLock<Vec<Value>> = OnceLock::new();
    TOOL_DEFINITIONS.get_or_init(build_tool_definitions)
}

fn build_tool_definitions() -> Vec<Value> {
    let batch_note_operation_schema = json!({"oneOf":[
        {"type":"object","properties":{"op":{"const":"create"},"title":{"type":"string","minLength":1,"maxLength":200},"body":{"type":"string"}},"required":["op"],"additionalProperties":false},
        {"type":"object","properties":{"op":{"const":"append"},"id":{"type":"string"},"title":{"type":"string"},"text":{"type":"string","minLength":1},"expected_body":{"type":"string"}},"required":["op","text"],"additionalProperties":false},
        {"type":"object","properties":{"op":{"const":"rename"},"id":{"type":"string"},"match_title":{"type":"string"},"new_title":{"type":"string","minLength":1,"maxLength":200}},"required":["op","new_title"],"additionalProperties":false},
        {"type":"object","properties":{"op":{"const":"update"},"id":{"type":"string"},"match_title":{"type":"string"},"title":{"type":"string","minLength":1,"maxLength":200},"body":{"type":"string"},"expected_body":{"type":"string"},"confirm":{"type":"boolean","const":true}},"required":["op"],"additionalProperties":false},
        {"type":"object","properties":{"op":{"const":"pin"},"id":{"type":"string"},"title":{"type":"string"},"pinned":{"type":"boolean"}},"required":["op","pinned"],"additionalProperties":false}
    ]});
    let mut tools = vec![
        json!({"name":"list_notes","description":"List Inkpad notes with titles, text previews, drawing counts and IDs; sorted pinned-first and recently updated.","inputSchema":{"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":200}},"additionalProperties":false}}),
        json!({"name":"search_notes","description":"Search titles and full note text, case-insensitively. Default returns compact summaries using the fast in-memory index. Set include_body:true with limit 1–10 to retrieve bounded note text in the same call (body_truncated flags long notes). Experimental sqlite_index:true enables a durable SQLite FTS mirror for benchmarking; it can be slower than the default and adds a first-build cost.","inputSchema":{"type":"object","properties":{"query":{"type":"string","minLength":1},"limit":{"type":"integer","minimum":1,"maximum":200},"include_body":{"type":"boolean"},"sqlite_index":{"type":"boolean"}},"required":["query"],"additionalProperties":false}}),
        json!({"name":"read_note","description":"Read a note by ID or unique title. Set compact:true for text and basic metadata without transferring potentially large drawings, images or formatting; default returns the complete note.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"compact":{"type":"boolean"}},"additionalProperties":false}}),
        json!({"name":"create_note","description":"Create a new Inkpad page with optional title and body. The current desktop page remains selected.","inputSchema":{"type":"object","properties":{"title":{"type":"string"},"body":{"type":"string"}},"additionalProperties":false}}),
        json!({"name":"append_to_note","description":"Append text to an existing page without touching its drawings. Provide an ID or an exact, unique title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"text":{"type":"string","minLength":1}},"required":["text"],"additionalProperties":false}}),
        json!({"name":"update_note","description":"Replace the supplied title and/or body of an existing note. Unspecified fields and all drawing strokes are preserved. Select by ID or exact match_title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"match_title":{"type":"string"},"title":{"type":"string"},"body":{"type":"string"}},"additionalProperties":false}}),
        json!({"name":"rename_note","description":"Rename a note without touching its text or drawing. Select by ID or exact match_title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"match_title":{"type":"string"},"new_title":{"type":"string","minLength":1,"maxLength":200}},"required":["new_title"],"additionalProperties":false}}),
        json!({"name":"set_note_pin","description":"Pin or unpin a note. Set pinned=true or false. Select by ID or exact title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"pinned":{"type":"boolean"}},"required":["pinned"],"additionalProperties":false}}),
        json!({"name":"note_batch","description":"Apply 1–32 create, append, rename, update or pin note changes in request order, atomically under one workspace lock. For append/pin select by id or exact unique title, for rename/update by id or match_title. Replacing a body with update requires explicit human authorization plus confirm:true; optional expected_body on append/update rejects stale text. An invalid operation rolls back the whole batch. Returns one compact result per operation. No delete, clear or undo operation is supported.","inputSchema":{"type":"object","properties":{"operations":{"type":"array","minItems":1,"maxItems":32,"items":batch_note_operation_schema}},"required":["operations"],"additionalProperties":false}}),
        json!({"name":"set_font_size","description":"Set a note's font size in the supported 12–40 pixel range.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"size":{"type":"integer","minimum":12,"maximum":40}},"required":["size"],"additionalProperties":false}}),
        json!({"name":"set_font_choice","description":"Set a note's font family preference to Sans, Serif or Mono.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"choice":{"type":"string","enum":["Sans","Serif","Mono"]}},"required":["choice"],"additionalProperties":false}}),
        json!({"name":"get_active_note","description":"Return the saved active page or null. Set compact:true to retrieve text and basic metadata without large drawing, image, and formatting arrays; default returns the full note.","inputSchema":{"type":"object","properties":{"compact":{"type":"boolean"}},"additionalProperties":false}}),
        json!({"name":"set_active_note","description":"Update the saved active page preference. An already-open GUI may keep its locally selected page; GUI Text/Draw mode is transient and cannot be set here.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"}},"additionalProperties":false}}),
        json!({"name":"export_note","description":"Save a note's UTF-8 plain body text as .txt or its complete 1100x700 drawing (legacy strokes plus editable objects) as PNG at an explicit absolute path. Existing files are never overwritten. Select by ID or exact title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"format":{"type":"string","enum":["txt","png"]},"path":{"type":"string"}},"required":["format","path"],"additionalProperties":false}}),
        json!({"name":"add_stroke","description":"Append one validated drawing stroke to an existing page without altering text. Coordinates are canvas pixels (x:0-1099,y:0-699), 1–512 points, width 1–40, color #RRGGBB, optional eraser.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"points":{"type":"array","minItems":1,"maxItems":512,"items":{"type":"array","minItems":2,"maxItems":2,"items":{"type":"integer","minimum":0}}},"width":{"type":"integer","minimum":1,"maximum":40},"color":{"type":"string","pattern":"^#[0-9a-fA-F]{6}$"},"eraser":{"type":"boolean"}},"required":["points","width"],"additionalProperties":false}}),
        json!({"name":"add_strokes_batch","description":"Paint 1–64 pen or eraser strokes in ONE atomic saved transaction. Each stroke has 1–512 bounded 1100x700 points, width 1–40, optional color #RRGGBB and eraser. Rejects invalid strokes or 2000-stroke page overflow without partial writes. Preserves text, vector objects and attached pictures. Unlike vector batch, undo_last_stroke removes only one stroke at a time.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"strokes":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"object","properties":{"points":{"type":"array","minItems":1,"maxItems":512,"items":{"type":"array","minItems":2,"maxItems":2,"items":{"type":"integer","minimum":0}}},"width":{"type":"integer","minimum":1,"maximum":40},"color":{"type":"string","pattern":"^#[0-9a-fA-F]{6}$"},"eraser":{"type":"boolean"}},"required":["points","width"],"additionalProperties":false}}},"required":["strokes"],"additionalProperties":false}}),
        json!({"name":"undo_last_stroke","description":"Remove one most-recent drawing stroke; destructive operation requiring the user's explicit authorization and confirm=true.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"confirm":{"type":"boolean","const":true}},"required":["confirm"],"additionalProperties":false}}),
        json!({"name":"clear_strokes","description":"Remove ALL drawing strokes from one page, preserving body text. Requires explicit human authorization and confirm=true.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"confirm":{"type":"boolean","const":true}},"required":["confirm"],"additionalProperties":false}}),
        json!({"name":"delete_note","description":"PERMANENTLY delete a note and its drawing strokes. Call only after the human user explicitly authorizes deletion. Requires confirm:true plus note ID or exact, unique title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"confirm":{"type":"boolean","const":true}},"required":["confirm"],"additionalProperties":false}}),
        json!({"name":"get_workspace_info","description":"Show local workspace version, note count, active page ID, and storage file location.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"image_insert_file","description":"Import PNG/JPEG/GIF/WebP into a note from a local user-selected absolute path. This reads the source file: only call if the human explicitly approved that file and set user_approved=true; never guess paths or access secrets. Maximum 12 MiB. Inkpad validates and copies the image into managed storage; original untouched. Use note id or unique exact title.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"path":{"type":"string"},"user_approved":{"type":"boolean","const":true}},"required":["path","user_approved"],"additionalProperties":false}}),
        json!({"name":"image_insert_base64","description":"Import an explicitly user-provided base64-encoded PNG/JPEG/GIF/WebP image into a note (max 1 MiB decoded per MCP request). Data must be image bytes only, not a file path or URL. Require explicit user_approved=true. Does not fetch remote URLs.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"data_base64":{"type":"string"},"user_approved":{"type":"boolean","const":true}},"required":["data_base64","user_approved"],"additionalProperties":false}}),
        json!({"name":"image_list","description":"List the ordered inline image IDs, captions, display width percentages and positions for a note. Returns metadata only, never raw image bytes or private absolute paths.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"}},"additionalProperties":false}}),
        json!({"name":"image_resize","description":"Resize an existing inline note image. Set width_percent to 10–100. Image stays in the note and keeps its original pixels.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"image_id":{"type":"string"},"width_percent":{"type":"integer","minimum":10,"maximum":100}},"required":["image_id","width_percent"],"additionalProperties":false}}),
        json!({"name":"image_move","description":"Move an image to an integer zero-based position within the same note (0 means first). No files are modified; image order is saved.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"image_id":{"type":"string"},"position":{"type":"integer","minimum":0}},"required":["image_id","position"],"additionalProperties":false}}),
        json!({"name":"image_place","description":"Place an existing image between Unicode scalar characters in its note text, without modifying Note.body or rich-text style offsets. Supply char_offset 0..text length and unit=unicode_scalar. Supply null to restore legacy after-text gallery placement. Optional expected_body rejects stale caret positions if another editor changed the text.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"image_id":{"type":"string"},"unit":{"type":"string","const":"unicode_scalar"},"char_offset":{"type":["integer","null"],"minimum":0},"expected_body":{"type":"string"}},"required":["image_id","unit","char_offset"],"additionalProperties":false}}),
        json!({"name":"image_caption","description":"Change an image's caption (up to 500 characters). An empty caption removes the caption. Image bytes and size are preserved.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"image_id":{"type":"string"},"caption":{"type":"string","maxLength":500}},"required":["image_id","caption"],"additionalProperties":false}}),
        json!({"name":"image_delete","description":"Remove the selected image from the note. Only call when the human explicitly authorizes removal of that image; requires confirm:true. Text, strokes and other images remain. The private PNG may be retained for backup recovery rather than erased from disk.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"image_id":{"type":"string"},"confirm":{"type":"boolean","const":true}},"required":["image_id","confirm"],"additionalProperties":false}}),
        json!({"name":"image_export_html","description":"Export a complete self-contained note as HTML with escaped text and inline embedded images at a human-approved absolute .html path. Contains private note content; do not export without user_approved=true. Refuses overwrite and never opens a GUI dialog.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"path":{"type":"string"},"user_approved":{"type":"boolean","const":true}},"required":["path","user_approved"],"additionalProperties":false}}),
        json!({"name":"read_image","description":"Read one selected private Inkpad picture for AI image understanding, returned as an MCP image block (PNG base64, max 1 MiB). Requires explicit user_approved=true from the user requesting picture inspection. For larger images use export_image instead. Never scans files or returns arbitrary paths.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"image_id":{"type":"string"},"user_approved":{"type":"boolean","const":true}},"required":["image_id","user_approved"],"additionalProperties":false}}),
        json!({"name":"export_image","description":"Export one stored picture to an explicit user-approved NEW absolute PNG file, without overwriting any existing file. Requires user_approved=true. Never exports a file that is not an attachment of the selected note; does not upload to any network service.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"image_id":{"type":"string"},"path":{"type":"string"},"user_approved":{"type":"boolean","const":true}},"required":["image_id","path","user_approved"],"additionalProperties":false}}),
        json!({"name":"get_app_settings","description":"Read persisted Inkpad appearance theme (light, dark, system) and sidebar collapsed state. Reads local UI settings only, without opening the window or accessing note content.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"set_app_theme","description":"Persist Inkpad's appearance preference: light, dark, or system. Uses a cross-process settings lock; no GUI window opened. System resolves to OS preference in the GUI. Does not modify notes.","inputSchema":{"type":"object","properties":{"theme":{"type":"string","enum":["light","dark","system"]}},"required":["theme"],"additionalProperties":false}}),
        json!({"name":"set_sidebar_collapsed","description":"Persist the Inkpad sidebar collapsed or expanded state using the cross-process settings lock. Does not open a GUI window or touch notes.","inputSchema":{"type":"object","properties":{"collapsed":{"type":"boolean"}},"required":["collapsed"],"additionalProperties":false}}),
        json!({"name":"get_note_formatting","description":"Read the selected note's sparse rich-text style runs, half-open Unicode scalar ranges, and character count. Style includes bold, foreground color, font size, and font family; unsupported italic/underline/background are not persisted. Returns no image bytes.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"}},"additionalProperties":false}}),
        json!({"name":"format_text_range","description":"Apply one or more attributes to [start,end) Unicode scalar indices in a note body; all unspecified attributes remain unchanged. Supported: bold boolean, color #RRGGBB, size 12-40, font_family Sans/Serif/Mono, and reset_color/reset_size/reset_font_family booleans. Explicit unit='unicode_scalar' required; rejects invalid/overlapping ranges atomically. No GUI focus required.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":1},"unit":{"type":"string","const":"unicode_scalar"},"bold":{"type":"boolean"},"color":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"size":{"type":"integer","minimum":12,"maximum":40},"font_family":{"type":"string","enum":["Sans","Serif","Mono"]},"reset_color":{"type":"boolean"},"reset_size":{"type":"boolean"},"reset_font_family":{"type":"boolean"}},"required":["start","end","unit"],"additionalProperties":false}}),
        json!({"name":"clear_text_formatting","description":"Clear all supported rich-text overrides on [start,end) Unicode scalar indices, leaving body text and other runs untouched. Explicit unit='unicode_scalar' required.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":1},"unit":{"type":"string","const":"unicode_scalar"}},"required":["start","end","unit"],"additionalProperties":false}}),
        json!({"name":"replace_text_range","description":"Replace [start,end) characters in saved note body using Unicode scalar indices; start=end inserts, empty text deletes selection. Preserves and rebases formatting outside the edit, including emoji/combining marks. Explicit unit='unicode_scalar' required; max body 1 MiB. Edits note text, not transient GUI caret/selection.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":0},"unit":{"type":"string","const":"unicode_scalar"},"text":{"type":"string"}},"required":["start","end","unit","text"],"additionalProperties":false}}),
        json!({"name":"format_text_batch","description":"Atomically apply 1–32 ordered rich-text style patches in ONE cross-process transaction. Each operation uses [start,end) Unicode scalar offsets; overlaps are applied sequentially. By default grapheme_safe rejects splitting combining marks or emoji clusters; boundary_policy=scalar opts into exact scalar boundaries. Optional expected_body must match the complete current note body before any change, guarding against stale offsets. Preserves all other note content.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"unit":{"type":"string","const":"unicode_scalar"},"boundary_policy":{"type":"string","enum":["grapheme_safe","scalar"]},"expected_body":{"type":"string"},"operations":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"object","properties":{"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":1},"bold":{"type":"boolean"},"color":{"type":"string","pattern":"^#[0-9a-fA-F]{6}$"},"size":{"type":"integer","minimum":12,"maximum":40},"font_family":{"type":"string","enum":["Sans","Serif","Mono"]},"reset_color":{"type":"boolean"},"reset_size":{"type":"boolean"},"reset_font_family":{"type":"boolean"}},"required":["start","end"],"additionalProperties":false}}},"required":["unit","operations"],"additionalProperties":false}}),
        json!({"name":"copy_rich_fragment","description":"Return a user-selected styled text fragment as structured JSON with relative Unicode scalar runs and source font defaults; never reads/writes the system clipboard or files. Requires note ID/unique title and scalar [start,end) offsets. Default grapheme_safe prevents splitting emoji/combining clusters; boundary_policy=scalar opts into scalar-level selection. Max selected text 256 KiB.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"unit":{"type":"string","const":"unicode_scalar"},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":0},"boundary_policy":{"type":"string","enum":["grapheme_safe","scalar"]}},"required":["unit","start","end"],"additionalProperties":false}}),
        json!({"name":"paste_rich_fragment","description":"Insert/replace a note selection with a previously returned rich fragment, without accessing OS clipboard. Reapplies validated bold, color, font and sizes, preserves unaffected text/styles, and commits atomically under Store lock. Fragment format_version=1, unit=unicode_scalar, text up to 256 KiB, up to 2048 canonical styled runs. Optional expected_text ensures the exact destination selection has not changed. Default grapheme_safe boundary validation; no durable text undo/redo is implied.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"unit":{"type":"string","const":"unicode_scalar"},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":0},"boundary_policy":{"type":"string","enum":["grapheme_safe","scalar"]},"expected_text":{"type":"string"},"fragment":{"type":"object","properties":{"format_version":{"type":"integer","const":1},"unit":{"type":"string","const":"unicode_scalar"},"text":{"type":"string"},"runs":{"type":"array"},"defaults":{"type":"object","properties":{"size":{"type":"integer","minimum":12,"maximum":40},"font_family":{"type":"string","enum":["Sans","Serif","Mono"]}},"required":["size","font_family"],"additionalProperties":false}},"required":["format_version","unit","text","runs"],"additionalProperties":false}},"required":["unit","start","end","fragment"],"additionalProperties":false}}),
    ];
    tools.extend(drawing_tool_definitions());
    tools
}

fn drawing_tool_definitions() -> Vec<Value> {
    let shape = json!({"oneOf":[
        {"type":"object","properties":{"type":{"const":"rectangle"},"width":{"type":"number","exclusiveMinimum":0,"maximum":8192},"height":{"type":"number","exclusiveMinimum":0,"maximum":8192}},"required":["type","width","height"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"rounded_rectangle"},"width":{"type":"number","exclusiveMinimum":0,"maximum":8192},"height":{"type":"number","exclusiveMinimum":0,"maximum":8192},"radius":{"type":"number","minimum":0}},"required":["type","width","height","radius"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"ellipse"},"radius_x":{"type":"number","exclusiveMinimum":0,"maximum":8192},"radius_y":{"type":"number","exclusiveMinimum":0,"maximum":8192}},"required":["type","radius_x","radius_y"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"circle"},"radius":{"type":"number","exclusiveMinimum":0,"maximum":8192}},"required":["type","radius"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"line"},"to":{"$ref":"#/$defs/point"}},"required":["type","to"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"polygon"},"points":{"type":"array","minItems":2,"maxItems":256,"items":{"$ref":"#/$defs/point"}},"closed":{"type":"boolean"}},"required":["type","points"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"cubic_bezier"},"control1":{"$ref":"#/$defs/point"},"control2":{"$ref":"#/$defs/point"},"to":{"$ref":"#/$defs/point"}},"required":["type","control1","control2","to"],"additionalProperties":false}
    ]});
    let point = json!({"type":"object","properties":{"x":{"type":"number","minimum":-16384,"maximum":16384},"y":{"type":"number","minimum":-16384,"maximum":16384}},"required":["x","y"],"additionalProperties":false});
    let color = json!({"type":"array","minItems":4,"maxItems":4,"items":{"type":"integer","minimum":0,"maximum":255}});
    let fill = json!({"oneOf":[
        {"type":"object","properties":{"type":{"const":"solid"},"color":color},"required":["type","color"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"linear_gradient"},"start":point,"end":point,"stops":{"$ref":"#/$defs/stops"}},"required":["type","start","end","stops"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"radial_gradient"},"center":point,"radius":{"type":"number","exclusiveMinimum":0,"maximum":8192},"stops":{"$ref":"#/$defs/stops"}},"required":["type","center","radius","stops"],"additionalProperties":false}
    ]});
    let style = json!({"type":"object","properties":{"stroke":{"oneOf":[color,{"type":"null"}]},"stroke_width":{"type":"number","minimum":0,"maximum":256},"fill":{"oneOf":[fill,{"type":"null"}]},"opacity":{"type":"number","minimum":0,"maximum":1}},"additionalProperties":false});
    let transform = json!({"type":"object","properties":{"translation":point,"scale_x":{"type":"number","minimum":0.001,"maximum":128},"scale_y":{"type":"number","minimum":0.001,"maximum":128},"rotation_deg":{"type":"number","minimum":-360000,"maximum":360000},"flip_x":{"type":"boolean"},"flip_y":{"type":"boolean"}},"additionalProperties":false});
    let defs = json!({"point":point,"stops":{"type":"array","minItems":2,"maxItems":16,"items":{"type":"object","properties":{"offset":{"type":"number","minimum":0,"maximum":1},"color":color},"required":["offset","color"],"additionalProperties":false}}});
    let note = json!({"id":{"type":"string"},"title":{"type":"string"}});
    let object_id = json!({"type":"string","minLength":1,"maxLength":128});
    let layer_id = object_id.clone();
    let mut tools = Vec::new();
    let mut add = |name: &str, description: &str, properties: Value, required: Vec<&str>, definitions: bool| {
        let mut schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
        if definitions { schema["$defs"] = defs.clone(); }
        tools.push(json!({"name":name,"description":description,"inputSchema":schema}));
    };
    add("get_capabilities", "Discover exact capabilities of the local MCP drawing model and current GUI/MCP differences, including supported shapes, styles, transforms, layers and transient history.",json!({}),vec![],false);
    add("drawing_list", "Read editable vector objects and layers in the saved note. Legacy freehand strokes remain separate. No private image bytes are returned.",note.clone(),vec![],false);
    add("drawing_create_shape", "Create one editable shape on the 1100x700 canvas. For detailed artwork prefer drawing_create_scene (up to 512 shapes + named layers in one save) or drawing_create_batch (up to 64). Set local geometry, transform.translation for placement, RGBA stroke and solid/linear/radial gradient fill.",json!({"id":note["id"],"title":note["title"],"shape":shape.clone(),"style":style.clone(),"transform":transform.clone(),"layer_id":layer_id.clone()}),vec!["shape"],true);
    add("drawing_create_scene", "BEST for complex AI artwork: atomically add 1–16 named layers and 1–512 editable vector objects in ONE MCP tool call, one workspace write and one undo step. Each object's layer_index selects its new layer (0-based); existing layer_id also supported. Objects appear in array order and retain stable object IDs. Plan coherent silhouette, palette, lighting and foreground; inspect a drawing_preview after the composition, then add details. Does not replace existing art/text/media. Invalid entries, missing/locked layers, stale expected_object_count or drawing limits roll back the entire scene. MCP message maximum 2 MiB.",json!({"id":note["id"],"title":note["title"],"expected_object_count":{"type":"integer","minimum":0,"maximum":2000},"layers":{"type":"array","maxItems":MAX_SCENE_DRAW_LAYERS,"items":{"type":"object","properties":{"name":{"type":"string","minLength":1,"maxLength":80}},"required":["name"],"additionalProperties":false}},"objects":{"type":"array","minItems":1,"maxItems":MAX_SCENE_DRAW_OBJECTS,"items":{"type":"object","properties":{"shape":shape.clone(),"style":style.clone(),"transform":transform.clone(),"layer_id":layer_id.clone(),"layer_index":{"type":"integer","minimum":0,"maximum":15}},"required":["shape"],"additionalProperties":false}}}),vec!["objects"],true);
    add("drawing_create_batch", "Paint 1–64 editable vector objects in input order with ONE atomic saved transaction, one cross-process lock and one session undo step. Fast for layered illustrations: create named layers first with drawing_create_layer; set each object's layer_id or a shared default layer_id, RGBA stroke/opacity, solid/linear/radial gradient fill and transform.translation. Objects keep unique editable IDs and existing text, images and strokes remain unchanged. An invalid object, missing/locked layer or mismatched expected_object_count aborts the entire batch; response returns object_ids in stacking order. MCP message maximum is 2 MiB.",json!({"id":note["id"],"title":note["title"],"layer_id":layer_id.clone(),"expected_object_count":{"type":"integer","minimum":0,"maximum":2000},"objects":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"object","properties":{"shape":shape,"style":style,"transform":transform,"layer_id":layer_id},"required":["shape"],"additionalProperties":false}}}),vec!["objects"],true);
    add("drawing_edit_shape", "Replace any provided shape geometry, style, transform, visibility or layer association of ONE existing drawing object. Locked layers reject edits. All omitted fields are kept.",json!({"id":note["id"],"title":note["title"],"object_id":object_id,"shape":shape,"style":style,"transform":transform,"visible":{"type":"boolean"},"layer_id":layer_id}),vec!["object_id"],true);
    add("drawing_transform_object", "Apply a relative move (dx/dy), resize (sx/sy), rotate (degrees) or flip (horizontal=true/false) to one editable object. Locked layers reject edits; values are checked.",json!({"id":note["id"],"title":note["title"],"object_id":object_id,"operation":{"type":"string","enum":["move","resize","rotate","flip"]},"dx":{"type":"number","minimum":-16384,"maximum":16384},"dy":{"type":"number","minimum":-16384,"maximum":16384},"sx":{"type":"number","exclusiveMinimum":0,"maximum":128},"sy":{"type":"number","exclusiveMinimum":0,"maximum":128},"degrees":{"type":"number","minimum":-360000,"maximum":360000},"horizontal":{"type":"boolean"}}),vec!["object_id","operation"],false);
    add("drawing_transform_group", "Apply a relative move, resize, rotation or flip to all objects in one named group around their shared transform origin. Locked layers reject the entire operation.",json!({"id":note["id"],"title":note["title"],"group_id":object_id,"operation":{"type":"string","enum":["move","resize","rotate","flip"]},"dx":{"type":"number","minimum":-16384,"maximum":16384},"dy":{"type":"number","minimum":-16384,"maximum":16384},"sx":{"type":"number","exclusiveMinimum":0,"maximum":128},"sy":{"type":"number","exclusiveMinimum":0,"maximum":128},"degrees":{"type":"number","minimum":-360000,"maximum":360000},"horizontal":{"type":"boolean"}}),vec!["group_id","operation"],false);
    add("drawing_reorder_object", "Change one editable object's zero-based stacking position among vector objects.",json!({"id":note["id"],"title":note["title"],"object_id":object_id,"position":{"type":"integer","minimum":0,"maximum":1999}}),vec!["object_id","position"],false);
    add("drawing_delete_object", "Delete the selected vector shape, requiring explicit human authorization and confirm:true. This does not delete legacy strokes or images.",json!({"id":note["id"],"title":note["title"],"object_id":object_id,"confirm":{"type":"boolean","const":true}}),vec!["object_id","confirm"],false);
    add("drawing_group_objects", "Assign one group ID to 2–2000 selected vector objects; group membership is metadata and does not merge geometries.",json!({"id":note["id"],"title":note["title"],"object_ids":{"type":"array","minItems":2,"maxItems":2000,"items":object_id}}),vec!["object_ids"],false);
    add("drawing_ungroup_objects", "Remove group membership for an existing drawing group without deleting shapes.",json!({"id":note["id"],"title":note["title"],"group_id":object_id}),vec!["group_id"],false);
    add("drawing_create_layer", "Create one named editable drawing layer (maximum 64). Existing shapes and legacy strokes remain untouched.",json!({"id":note["id"],"title":note["title"],"name":{"type":"string","minLength":1,"maxLength":80}}),vec!["name"],false);
    add("drawing_edit_layer", "Rename an existing drawing layer or change its visibility/lock. Supply one or more of name, visible or locked.",json!({"id":note["id"],"title":note["title"],"layer_id":layer_id,"name":{"type":"string","minLength":1,"maxLength":80},"visible":{"type":"boolean"},"locked":{"type":"boolean"}}),vec!["layer_id"],false);
    add("drawing_move_layer", "Reorder one drawing layer using a zero-based index; maintains all object associations.",json!({"id":note["id"],"title":note["title"],"layer_id":layer_id,"position":{"type":"integer","minimum":0,"maximum":63}}),vec!["layer_id","position"],false);
    add("drawing_delete_layer", "Remove an EMPTY drawing layer (not a layer containing objects), with explicit human authorization and confirm:true. Rejects locked layers.",json!({"id":note["id"],"title":note["title"],"layer_id":layer_id,"confirm":{"type":"boolean","const":true}}),vec!["layer_id","confirm"],false);
    add("drawing_pick_object", "Inspect the topmost visible, unlocked vector object under a canvas point. Returns object metadata or null. Hit-testing uses the same implementation as the GUI.",json!({"id":note["id"],"title":note["title"],"x":{"type":"number","minimum":-16384,"maximum":16384},"y":{"type":"number","minimum":-16384,"maximum":16384},"tolerance":{"type":"number","minimum":0,"maximum":32}}),vec!["x","y"],false);
    add("drawing_preview", "Render the current saved canvas as an MCP image/png content block, max 1 MiB PNG. Includes legacy freehand strokes and editable objects. Does not create a file or open the GUI.",note.clone(),vec![],false);
    add("drawing_undo", "Undo the last vector drawing MCP edit in this same long-running stdio process. Requires explicit user authorization and confirm:true; refuses if another process changed the drawing.",json!({"id":note["id"],"title":note["title"],"confirm":{"type":"boolean","const":true}}),vec!["confirm"],false);
    add("drawing_redo", "Redo the last vector drawing MCP undo in this same stdio process. Requires explicit user authorization and confirm:true; refuses after external changes.",json!({"id":note["id"],"title":note["title"],"confirm":{"type":"boolean","const":true}}),vec!["confirm"],false);
    tools
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

/// Batch operations borrow validated input, then run in request order inside
/// one Store transaction. A failed later lookup/guard discards every edit.
enum NoteBatchOp<'a> {
    Create { title: &'a str, body: &'a str },
    Append { by_title: bool, selector: String, text: &'a str, expected_body: Option<&'a str> },
    Rename { by_title: bool, selector: String, new_title: &'a str },
    Update { by_title: bool, selector: String, title: Option<&'a str>, body: Option<&'a str>, expected_body: Option<&'a str> },
    Pin { by_title: bool, selector: String, pinned: bool },
}

impl NoteBatchOp<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create",
            Self::Append { .. } => "append",
            Self::Rename { .. } => "rename",
            Self::Update { .. } => "update",
            Self::Pin { .. } => "pin",
        }
    }
}

fn parse_note_batch(args: &Value) -> Result<Vec<NoteBatchOp<'_>>, String> {
    let args = object_args(args, &["operations"])?;
    let operations = args.get("operations").and_then(Value::as_array)
        .ok_or_else(|| invalid("operations must be an array of 1–32 note changes"))?;
    if operations.is_empty() || operations.len() > MAX_BATCH_NOTE_OPS {
        return Err(invalid("operations must contain 1–32 note changes"));
    }
    operations.iter().enumerate().map(|(index, operation)| {
        let parse = || -> Result<NoteBatchOp<'_>, String> {
            let value = operation.as_object().ok_or_else(|| invalid("Operation must be an object"))?;
            match required_string(value, "op")? {
                "create" => {
                    let value = object_args(operation, &["op", "title", "body"])?;
                    let title = optional_string(value, "title")?.unwrap_or("Untitled note");
                    let body = optional_string(value, "body")?.unwrap_or("");
                    validate_title(title)?;
                    validate_body(body)?;
                    Ok(NoteBatchOp::Create { title, body })
                }
                "append" => {
                    let value = object_args(operation, &["op", "id", "title", "text", "expected_body"])?;
                    let (by_title, selector) = selector(value, "title")?;
                    let text = required_string(value, "text")?;
                    validate_body(text)?;
                    if text.is_empty() { return Err(invalid("text cannot be empty")); }
                    let expected_body = optional_string(value, "expected_body")?;
                    if let Some(expected) = expected_body { validate_body(expected)?; }
                    Ok(NoteBatchOp::Append { by_title, selector, text, expected_body })
                }
                "rename" => {
                    let value = object_args(operation, &["op", "id", "match_title", "new_title"])?;
                    let (by_title, selector) = selector(value, "match_title")?;
                    let new_title = required_string(value, "new_title")?;
                    validate_title(new_title)?;
                    Ok(NoteBatchOp::Rename { by_title, selector, new_title })
                }
                "update" => {
                    let value = object_args(operation, &["op", "id", "match_title", "title", "body", "expected_body", "confirm"])?;
                    let (by_title, selector) = selector(value, "match_title")?;
                    let title = optional_string(value, "title")?;
                    let body = optional_string(value, "body")?;
                    if title.is_none() && body.is_none() { return Err(invalid("Provide title and/or body to update")); }
                    if body.is_some() && value.get("confirm") != Some(&Value::Bool(true)) {
                        return Err(invalid("Replacing a note body requires explicit user authorization and confirm:true"));
                    }
                    if body.is_none() && value.contains_key("confirm") {
                        return Err(invalid("confirm is only valid for body replacement"));
                    }
                    if let Some(title) = title { validate_title(title)?; }
                    if let Some(body) = body { validate_body(body)?; }
                    let expected_body = optional_string(value, "expected_body")?;
                    if let Some(expected) = expected_body { validate_body(expected)?; }
                    Ok(NoteBatchOp::Update { by_title, selector, title, body, expected_body })
                }
                "pin" => {
                    let value = object_args(operation, &["op", "id", "title", "pinned"])?;
                    let (by_title, selector) = selector(value, "title")?;
                    let pinned = required_bool(value, "pinned")?;
                    Ok(NoteBatchOp::Pin { by_title, selector, pinned })
                }
                _ => Err(invalid("Unsupported op; use create, append, rename, update or pin")),
            }
        };
        parse().map_err(|error| format!("Operation {index}: {error}"))
    }).collect()
}

fn execute_note_batch(store: &Store, args: &Value) -> Result<Value, String> {
    let operations = parse_note_batch(args)?;
    mutate(store, |workspace| {
        let mut results = Vec::with_capacity(operations.len());
        for (index, operation) in operations.iter().enumerate() {
            let kind = operation.kind();
            let mut change = || -> Result<Value, String> {
                let note = match operation {
                    NoteBatchOp::Create { title, body } => {
                        let mut note = Note::create("note");
                        note.title = (*title).to_string();
                        note.body = (*body).to_string();
                        // In one fast transaction Note::create may get the same
                        // clock tick for multiple notes. Assign collision-safe IDs.
                        let base_id = note.id.clone();
                        let mut suffix = 1usize;
                        while workspace.notes.iter().any(|existing| existing.id == note.id) {
                            note.id = format!("{base_id}-{suffix}");
                            suffix += 1;
                        }
                        if workspace.notes.is_empty() { workspace.active_id = note.id.clone(); }
                        workspace.notes.push(note);
                        workspace.notes.last().expect("just added note")
                    }
                    NoteBatchOp::Append { by_title, selector, text, expected_body } => {
                        let position = find_note_index(workspace, *by_title, selector)?;
                        let note = &mut workspace.notes[position];
                        if expected_body.is_some_and(|expected| note.body != expected) {
                            return Err(invalid("expected_body does not match saved text"));
                        }
                        let separator = if note.body.is_empty() || note.body.ends_with('\n') || text.starts_with('\n') { "" } else { "\n" };
                        let length = note.body.len().checked_add(separator.len())
                            .and_then(|bytes| bytes.checked_add(text.len()))
                            .ok_or_else(|| invalid("Resulting note body exceeds 1 MiB"))?;
                        if length > MAX_BODY_BYTES { return Err(invalid("Resulting note body exceeds 1 MiB")); }
                        let mut edited = note.body.clone();
                        edited.push_str(separator);
                        edited.push_str(text);
                        note.set_body_preserving_format(edited)?;
                        note
                    }
                    NoteBatchOp::Rename { by_title, selector, new_title } => {
                        let position = find_note_index(workspace, *by_title, selector)?;
                        let note = &mut workspace.notes[position];
                        note.title = (*new_title).to_string();
                        note.touch();
                        note
                    }
                    NoteBatchOp::Update { by_title, selector, title, body, expected_body } => {
                        let position = find_note_index(workspace, *by_title, selector)?;
                        let note = &mut workspace.notes[position];
                        if expected_body.is_some_and(|expected| note.body != expected) {
                            return Err(invalid("expected_body does not match saved text"));
                        }
                        if let Some(title) = title { note.title = (*title).to_string(); }
                        if let Some(body) = body { note.set_body_preserving_format((*body).to_string())?; }
                        note.touch();
                        note
                    }
                    NoteBatchOp::Pin { by_title, selector, pinned } => {
                        let position = find_note_index(workspace, *by_title, selector)?;
                        let note = &mut workspace.notes[position];
                        note.pinned = *pinned;
                        note.touch();
                        note
                    }
                };
                Ok(json!({"index":index,"op":kind,"id":note.id,"title":note.title,
                    "pinned":note.pinned,"body_chars":note.body.chars().count()}))
            };
            results.push(change().map_err(|error| format!("Operation {index} ({kind}): {error}"))?);
        }
        Ok(json!({"applied":results.len(),"results":results}))
    })
}

fn ordered_notes<'a>(notes: impl IntoIterator<Item = &'a Note>, limit: usize) -> Vec<&'a Note> {
    // JSON-RPC allows at most 200 summaries. Partition large result sets once
    // and sort only those actually returned. Include original position as the
    // final tie-breaker to preserve the old stable-sort ordering exactly.
    let mut list: Vec<_> = notes.into_iter().enumerate().collect();
    let cmp = |a: &(usize, &Note), b: &(usize, &Note)| {
        b.1.pinned.cmp(&a.1.pinned)
            .then_with(|| b.1.updated.cmp(&a.1.updated))
            .then_with(|| a.1.title.cmp(&b.1.title))
            .then_with(|| a.0.cmp(&b.0))
    };
    if list.len() > limit {
        list.select_nth_unstable_by(limit, cmp);
        list.truncate(limit);
    }
    list.sort_unstable_by(cmp);
    list.into_iter().map(|(_, note)| note).collect()
}

fn preview(body: &str) -> String {
    body.lines().find(|line| !line.trim().is_empty()).unwrap_or("").chars().take(140).collect()
}

fn note_summary(note: &Note) -> Value {
    json!({
        "id":note.id,"title":note.title,"kind":note.kind,
        "preview":preview(&note.body),"stroke_count":note.strokes.len(),
        // Vector-only drawings are still drawings: the old stroke-only flag
        // falsely reported all three detailed mallard paintings as empty.
        "has_drawing":!note.strokes.is_empty() || !note.drawing.objects.is_empty(),
        "drawing_object_count":note.drawing.objects.len(),
        "drawing_layer_count":note.drawing.layers.len(),
        "image_count":note.images.len(),"pinned":note.pinned,
        "font_size":note.font_size,"font_choice":note.font_choice,"updated":note.updated
    })
}

fn compact_note(note: &Note) -> Value {
    json!({"id":note.id,"title":note.title,"body":note.body,
        "kind":note.kind,"pinned":note.pinned,"updated":note.updated,
        "font_size":note.font_size,"font_choice":note.font_choice,
        "image_count":note.images.len(),"stroke_count":note.strokes.len(),
        "drawing_object_count":note.drawing.objects.len(),
        "rich_text_run_count":note.rich_text.len()})
}

fn search_note_with_body(note: &Note) -> Value {
    let mut summary = note_summary(note);
    // Bound the response even when a workspace has large notes. The flag is
    // explicit so callers know when to fetch the remaining text with read_note.
    let body_chars = note.body.chars().count();
    summary["body"] = json!(if body_chars > MAX_SEARCH_BODY_CHARS {
        note.body.chars().take(MAX_SEARCH_BODY_CHARS).collect::<String>()
    } else {
        note.body.clone()
    });
    summary["body_truncated"] = json!(body_chars > MAX_SEARCH_BODY_CHARS);
    summary
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

fn scalar_range(args: &Map<String, Value>, allow_empty: bool) -> Result<(usize, usize), String> {
    if required_string(args, "unit")? != "unicode_scalar" {
        return Err(invalid("unit must be unicode_scalar (Unicode scalar values, not bytes or UTF-16 code units)"));
    }
    let offset = |name: &str| -> Result<usize, String> {
        args.get(name).and_then(Value::as_u64)
            .and_then(|number| usize::try_from(number).ok())
            .ok_or_else(|| format!("{name} must be a nonnegative integer Unicode scalar offset"))
    };
    let (start, end) = (offset("start")?, offset("end")?);
    if start > end || (!allow_empty && start == end) {
        return Err(invalid(if allow_empty { "start must be <= end" } else { "Formatting requires start < end" }));
    }
    Ok((start, end))
}

fn cli_range_args(args: &[String], style: bool) -> Result<Map<String, Value>, String> {
    if args.len() < if style { 5 } else { 4 } {
        return Err(invalid("Provide NOTE_ID START END and any formatting options"));
    }
    let start = args[2].parse::<usize>().map_err(|_| invalid("START must be a nonnegative Unicode scalar index"))?;
    let end = args[3].parse::<usize>().map_err(|_| invalid("END must be a nonnegative Unicode scalar index"))?;
    let mut options = Map::new();
    options.insert("id".into(), json!(args[1]));
    options.insert("start".into(), json!(start));
    options.insert("end".into(), json!(end));
    options.insert("unit".into(), json!("unicode_scalar"));
    if !style { return Ok(options); }
    let mut i = 4;
    let mut seen = std::collections::HashSet::new();
    while i < args.len() {
        let (key, value, consumes): (&str, Value, usize) = match args[i].as_str() {
            "--bold" if i + 1 < args.len() => ("bold", json!(parse_cli_bool(&args[i + 1])?), 2),
            "--color" if i + 1 < args.len() => ("color", json!(&args[i + 1]), 2),
            "--size" if i + 1 < args.len() => {
                let size = args[i + 1].parse::<i32>().map_err(|_| invalid("size must be an integer"))?;
                ("size", json!(size), 2)
            }
            "--family" if i + 1 < args.len() => ("font_family", json!(&args[i + 1]), 2),
            "--reset-color" => ("reset_color", json!(true), 1),
            "--reset-size" => ("reset_size", json!(true), 1),
            "--reset-family" => ("reset_font_family", json!(true), 1),
            _ => return Err(format!("Unknown or incomplete rich-text option: {}", args[i])),
        };
        if !seen.insert(key) { return Err(format!("Duplicate rich-text option {key}")); }
        options.insert(key.to_string(), value);
        i += consumes;
    }
    Ok(options)
}

fn parse_cli_bool(value: &str) -> Result<bool, String> {
    match value { "true" => Ok(true), "false" => Ok(false), _ => Err(invalid("Boolean option must be true or false")) }
}

fn rich_style_patch(args: &Map<String, Value>) -> Result<TextStylePatch, String> {
    let patch_bool = |key: &str| -> Result<Option<bool>, String> {
        if args.contains_key(key) { required_bool(args, key).map(Some) } else { Ok(None) }
    };
    let bold = patch_bool("bold")?;
    let color = optional_string(args, "color")?.map(str::to_string);
    let size = if args.contains_key("size") { Some(required_i32(args, "size", 12, 40)?) } else { None };
    let font_family = optional_string(args, "font_family")?.map(str::to_string);
    let reset_color = patch_bool("reset_color")?.unwrap_or(false);
    let reset_size = patch_bool("reset_size")?.unwrap_or(false);
    let reset_font_family = patch_bool("reset_font_family")?.unwrap_or(false);
    if bold.is_none() && color.is_none() && size.is_none() && font_family.is_none()
        && !(reset_color || reset_size || reset_font_family) {
        return Err(invalid("Specify at least one supported rich-text formatting change"));
    }
    if (reset_color && color.is_some()) || (reset_size && size.is_some()) || (reset_font_family && font_family.is_some()) {
        return Err(invalid("Cannot set and reset the same rich-text attribute in one operation"));
    }
    Ok(TextStylePatch { bold, color, size, font_family, reset_color, reset_size, reset_font_family })
}

/// New operations default to extended grapheme safety, unlike the legacy
/// scalar-range tools whose behavior must remain backwards compatible.
/// Both modes use exact Rust Unicode-scalar offsets for storage.
fn boundary_policy(args: &Map<String, Value>) -> Result<bool, String> {
    match optional_string(args, "boundary_policy")? {
        None | Some("grapheme_safe") => Ok(true),
        Some("scalar") => Ok(false),
        _ => Err(invalid("boundary_policy must be grapheme_safe or scalar")),
    }
}

fn check_grapheme_boundaries(body: &str, start: usize, end: usize, check: bool) -> Result<(), String> {
    if !check { return Ok(()); }
    use unicode_segmentation::UnicodeSegmentation;
    let mut current = 0usize;
    let mut start_ok = start == 0;
    let mut end_ok = end == 0;
    for grapheme in body.graphemes(true) {
        current += grapheme.chars().count();
        if current == start { start_ok = true; }
        if current == end { end_ok = true; }
    }
    if start_ok && end_ok { Ok(()) }
    else { Err(invalid("Selection splits a Unicode grapheme (combining mark, emoji sequence or cluster); use whole grapheme boundaries or explicit boundary_policy:scalar")) }
}

fn scalar_slice(body: &str, start: usize, end: usize) -> Result<&str, String> {
    if start > end { return Err(invalid("start must not exceed end")); }
    let to_byte = |offset: usize| -> Option<usize> {
        body.char_indices().map(|(i, _)| i).chain(std::iter::once(body.len())).nth(offset)
    };
    let begin = to_byte(start).ok_or_else(|| invalid("start is outside the note's Unicode scalar range"))?;
    let finish = to_byte(end).ok_or_else(|| invalid("end is outside the note's Unicode scalar range"))?;
    Ok(&body[begin..finish])
}

fn validate_fragment(fragment: &RichFragment) -> Result<(), String> {
    if fragment.format_version != 1 || fragment.unit != "unicode_scalar" {
        return Err(invalid("Rich fragment requires format_version:1 and unit:unicode_scalar"));
    }
    if fragment.text.len() > MAX_FRAGMENT_TEXT_BYTES || fragment.runs.len() > MAX_FRAGMENT_RUNS {
        return Err(invalid("Rich fragment exceeds the 256 KiB text or 2048 run limit"));
    }
    validate_body(&fragment.text)?;
    crate::store::rich_text::validate_runs(&fragment.text, &fragment.runs)?;
    if let Some(defaults) = &fragment.defaults {
        if !(12..=40).contains(&defaults.size)
            || !matches!(defaults.font_family.as_str(), "Sans" | "Serif" | "Mono") {
            return Err(invalid("Rich fragment defaults must have size 12..40 and font_family Sans/Serif/Mono"));
        }
    }
    Ok(())
}

fn parse_fragment(input: &Value) -> Result<RichFragment, String> {
    // The desktop editor's clipboard uses {version,text,runs}. Accept that
    // versioned interchange format as well as our richer MCP wrapper, so a
    // client can pass a GUI-created fragment without ever reading OS clipboard.
    if input.get("version").is_some() && input.get("format_version").is_none() {
        let native: crate::store::rich_text::StyledFragment =
            serde_json::from_value(input.clone())
                .map_err(|error| format!("Invalid Inkpad styled fragment: {error}"))?;
        native.validate()?;
        let fragment = RichFragment {
            format_version:native.version, unit:"unicode_scalar".into(),
            text:native.text, runs:native.runs,
            defaults:native.defaults.map(|defaults| FragmentDefaults {
                size:defaults.size, font_family:defaults.font_family
            })
        };
        validate_fragment(&fragment)?;
        return Ok(fragment);
    }
    let fragment: RichFragment = serde_json::from_value(input.clone())
        .map_err(|error| format!("Invalid rich fragment schema: {error}"))?;
    validate_fragment(&fragment)?;
    Ok(fragment)
}

fn build_fragment(note: &Note, start: usize, end: usize) -> Result<RichFragment, String> {
    let native = note.copy_styled_range(start, end)?;
    let text = native.text;
    if text.len() > MAX_FRAGMENT_TEXT_BYTES {
        return Err(invalid("Selected fragment exceeds 256 KiB; choose a smaller range"));
    }
    let fragment = RichFragment {
        format_version:native.version, unit:"unicode_scalar".into(), text, runs:native.runs,
        defaults:Some(FragmentDefaults {size:note.font_size, font_family:note.font_choice.clone()})
    };
    validate_fragment(&fragment)?;
    Ok(fragment)
}

fn paste_fragment(note: &mut Note, start: usize, end: usize, fragment: &RichFragment) -> Result<(), String> {
    validate_fragment(fragment)?;
    // Delegate to the SAME validated implementation used by the desktop
    // editor: this copies gaps, explicit runs and inherited source font
    // defaults in one pass rather than repeatedly formatting a large note.
    let native = crate::store::rich_text::StyledFragment {
        version:fragment.format_version,
        text:fragment.text.clone(),
        runs:fragment.runs.clone(),
        defaults:fragment.defaults.as_ref().map(|defaults| crate::store::rich_text::FragmentDefaults {
            size:defaults.size, font_family:defaults.font_family.clone()
        }),
    };
    note.paste_styled_range(start, end, &native)?;
    Ok(())
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

/// CLI JSON input is always an explicit argument, never OS clipboard content.
/// File-backed input avoids Windows PowerShell's native JSON quoting hazards.
fn cli_rich_json(value: &str) -> Result<Value, String> {
    if let Some(filepath) = value.strip_prefix('@') {
        let path = Path::new(filepath);
        if !path.is_absolute()
            || path.components().any(|part| matches!(part, std::path::Component::ParentDir))
            || !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("json")) {
            return Err(invalid("Rich JSON input must be an absolute .json path without traversal"));
        }
        let mut bytes = Vec::new();
        OpenOptions::new().read(true).open(path).map_err(|e| e.to_string())?
            .take((512 * 1024 + 1) as u64).read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 512 * 1024 { return Err(invalid("Rich JSON must be at most 512 KiB")); }
        let bytes = bytes.strip_prefix(&[0xEF,0xBB,0xBF]).unwrap_or(&bytes);
        serde_json::from_slice(bytes).map_err(|e| format!("Invalid rich JSON file: {e}"))
    } else {
        if value.len() > 512 * 1024 { return Err(invalid("Rich JSON must be at most 512 KiB")); }
        serde_json::from_str(value).map_err(|e| format!("Invalid rich JSON: {e}"))
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
        note.drawing.validate()?;
        Board::from_note(note).png_bytes().map_err(|e| format!("PNG rendering failed: {e}"))?
    };
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)
        .map_err(|e| format!("Cannot create export (existing files are never replaced): {e}"))?;
    file.write_all(&output).map_err(|e| format!("Failed writing export: {e}"))?;
    file.sync_all().map_err(|e| format!("Failed syncing export: {e}"))?;
    Ok(json!({"id":note.id,"format":format,"path":path.display().to_string(),"bytes":output.len(),
        "width":if format == "png" {Some(WIDTH)} else {None},
        "height":if format == "png" {Some(HEIGHT)} else {None}}))
}

fn load(store: &Store) -> Result<Arc<Workspace>, String> { store.read_cached().map_err(|e| e.to_string()) }

fn mutate<T>(store: &Store, work: impl FnOnce(&mut Workspace) -> Result<T, String>) -> Result<T, String> {
    // Failed validation aborts without writing an unchanged workspace.
    store.mutate(|workspace| work(workspace).map_err(|message| {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    })).map_err(|error| error.to_string())
}

fn execute_tool(store: &Store, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "note_batch" => execute_note_batch(store, args),
        "get_capabilities" => {
            object_args(args, &[])?;
            Ok(drawing_capabilities())
        }
        "drawing_list" => {
            let args = object_args(args, &["id","title"])?;
            let (by_title, key) = selector(args, "title")?;
            let workspace = load(store)?;
            let note = &workspace.notes[find_note_index(&workspace, by_title, &key)?];
            Ok(json!({"id":note.id,"drawing":note.drawing,"layer_count":note.drawing.layers.len(),
                "object_count":note.drawing.objects.len(),"legacy_stroke_count":note.strokes.len(),
                "canvas":{"width":WIDTH,"height":HEIGHT}}))
        }
        "drawing_create_shape" => {
            let args = object_args(args, &["id","title","shape","style","transform","layer_id"])?;
            let shape = draw_shape(args, "shape")?;
            let style = optional_draw_style(args)?;
            let transform = optional_draw_transform(args)?;
            let layer_id = optional_string(args, "layer_id")?;
            if layer_id.is_some() { drawing_id(args, "layer_id")?; }
            drawing_change(store, args, |drawing| {
                let object_id = drawing.add_object(shape, layer_id)?;
                if style.is_some() || transform.is_some() {
                    drawing.update_object(&object_id, |object| {
                        if let Some(style) = &style { object.style = style.clone(); }
                        if let Some(transform) = &transform { object.transform = transform.clone(); }
                    })?;
                }
                Ok(json!({"object":drawing.object(&object_id),"object_count":drawing.objects.len()}))
            })
        }
        "drawing_create_batch" => execute_drawing_create_batch(store, args),
        "drawing_create_scene" => execute_drawing_create_scene(store, args),
        "drawing_edit_shape" => {
            let args = object_args(args, &["id","title","object_id","shape","style","transform","visible","layer_id"])?;
            let object_id = drawing_id(args, "object_id")?;
            let shape = if args.contains_key("shape") { Some(draw_shape(args, "shape")?) } else { None };
            let style = optional_draw_style(args)?;
            let transform = optional_draw_transform(args)?;
            let visible = args.get("visible").map(|_| required_bool(args, "visible")).transpose()?;
            let layer_id = optional_string(args, "layer_id")?;
            if layer_id.is_some() { drawing_id(args, "layer_id")?; }
            if shape.is_none() && style.is_none() && transform.is_none() && visible.is_none() && layer_id.is_none() {
                return Err(invalid("Provide shape, style, transform, visible or layer_id to edit"));
            }
            drawing_change(store, args, |drawing| {
                drawing.update_object(object_id, |object| {
                    if let Some(shape) = &shape { object.shape = shape.clone(); }
                    if let Some(style) = &style { object.style = style.clone(); }
                    if let Some(transform) = &transform { object.transform = transform.clone(); }
                    if let Some(visible) = visible { object.visible = visible; }
                    if let Some(layer_id) = layer_id { object.layer_id = layer_id.into(); }
                })?;
                Ok(json!({"object":drawing.object(object_id)}))
            })
        }
        "drawing_transform_object" | "drawing_transform_group" => {
            let grouped = name == "drawing_transform_group";
            let args = object_args(args, if grouped {
                &["id","title","group_id","operation","dx","dy","sx","sy","degrees","horizontal"]
            } else {
                &["id","title","object_id","operation","dx","dy","sx","sy","degrees","horizontal"]
            })?;
            let target_id = drawing_id(args, if grouped { "group_id" } else { "object_id" })?;
            let operation = required_string(args, "operation")?;
            let parameters: &[&str] = match operation {
                "move" => &["dx","dy"],
                "resize" => &["sx","sy"],
                "rotate" => &["degrees"],
                "flip" => &["horizontal"],
                _ => return Err(invalid("operation must be move, resize, rotate or flip")),
            };
            if let Some(extra) = ["dx","dy","sx","sy","degrees","horizontal"].iter()
                .find(|key| args.contains_key(**key) && !parameters.contains(key)) {
                return Err(format!("Unexpected transform parameter for {operation}: {extra}"));
            }
            let numbers = match operation {
                "move" => Some((draw_number(args, "dx", -16384.0, 16384.0)?,draw_number(args, "dy", -16384.0, 16384.0)?)),
                "resize" => Some((draw_number(args, "sx", 0.001, 128.0)?,draw_number(args, "sy", 0.001, 128.0)?)),
                "rotate" => Some((draw_number(args, "degrees", -360000.0, 360000.0)?,0.0)),
                _ => None,
            };
            let horizontal = if operation == "flip" { Some(required_bool(args, "horizontal")?) } else { None };
            drawing_change(store, args, |drawing| {
                match (grouped, operation) {
                    (false, "move") => drawing.move_object(target_id, numbers.unwrap().0, numbers.unwrap().1)?,
                    (false, "resize") => drawing.resize_object(target_id, numbers.unwrap().0, numbers.unwrap().1)?,
                    (false, "rotate") => drawing.rotate_object(target_id, numbers.unwrap().0)?,
                    (false, "flip") => drawing.flip_object(target_id, horizontal.unwrap())?,
                    (true, "move") => drawing.move_group(target_id, numbers.unwrap().0, numbers.unwrap().1)?,
                    (true, "resize") => drawing.resize_group(target_id, numbers.unwrap().0, numbers.unwrap().1)?,
                    (true, "rotate") => drawing.rotate_group(target_id, numbers.unwrap().0)?,
                    (true, "flip") => drawing.flip_group(target_id, horizontal.unwrap())?,
                    _ => unreachable!(),
                }
                if grouped { Ok(json!({"group_id":target_id,"operation":operation,"object_count":drawing.objects.iter()
                    .filter(|object| object.group_id.as_deref() == Some(target_id)).count()})) }
                else { Ok(json!({"object":drawing.object(target_id)})) }
            })
        }
        "drawing_reorder_object" => {
            let args = object_args(args, &["id","title","object_id","position"])?;
            let object_id = drawing_id(args, "object_id")?;
            let position = required_i32(args, "position", 0, 1999)? as usize;
            drawing_change(store, args, |drawing| {
                drawing.reorder_object(object_id, position)?;
                Ok(json!({"object_id":object_id,"position":position}))
            })
        }
        "drawing_delete_object" => {
            let args = object_args(args, &["id","title","object_id","confirm"])?;
            require_confirmation(args, "Deleting this drawing object")?;
            let object_id = drawing_id(args, "object_id")?;
            drawing_change(store, args, |drawing| {
                drawing.remove_object(object_id)?;
                Ok(json!({"deleted":true,"object_id":object_id,"object_count":drawing.objects.len()}))
            })
        }
        "drawing_group_objects" => {
            let args = object_args(args, &["id","title","object_ids"])?;
            let ids = args.get("object_ids").and_then(Value::as_array)
                .filter(|list| (2..=2000).contains(&list.len()))
                .ok_or_else(|| invalid("object_ids must contain 2–2000 drawing object IDs"))?;
            let mut object_ids = Vec::with_capacity(ids.len());
            for id in ids {
                let mut identity = Map::new(); identity.insert("object_id".into(), id.clone());
                object_ids.push(drawing_id(&identity, "object_id")?.to_string());
            }
            drawing_change(store, args, |drawing| {
                let group_id = drawing.group_objects(&object_ids)?;
                Ok(json!({"group_id":group_id,"object_ids":object_ids}))
            })
        }
        "drawing_ungroup_objects" => {
            let args = object_args(args, &["id","title","group_id"])?;
            let group_id = drawing_id(args, "group_id")?;
            drawing_change(store, args, |drawing| {
                drawing.ungroup(group_id)?;
                Ok(json!({"group_id":group_id,"ungrouped":true}))
            })
        }
        "drawing_create_layer" => {
            let args = object_args(args, &["id","title","name"])?;
            let name = required_string(args, "name")?;
            if name.trim().is_empty() || name.chars().count() > 80 || name.contains('\0') { return Err(invalid("Layer name must contain 1–80 characters and no NUL")); }
            drawing_change(store, args, |drawing| {
                let layer_id = drawing.add_layer(name)?;
                Ok(json!({"layer_id":layer_id,"layer_count":drawing.layers.len()}))
            })
        }
        "drawing_edit_layer" => {
            let args = object_args(args, &["id","title","layer_id","name","visible","locked"])?;
            let layer_id = drawing_id(args, "layer_id")?;
            let name = optional_string(args, "name")?;
            if let Some(name) = name {
                if name.trim().is_empty() || name.chars().count() > 80 || name.contains('\0') { return Err(invalid("Layer name must contain 1–80 characters and no NUL")); }
            }
            let visible = args.get("visible").map(|_| required_bool(args, "visible")).transpose()?;
            let locked = args.get("locked").map(|_| required_bool(args, "locked")).transpose()?;
            if name.is_none() && visible.is_none() && locked.is_none() { return Err(invalid("Provide name, visible or locked to edit layer")); }
            drawing_change(store, args, |drawing| {
                if let Some(name) = name { drawing.rename_layer(layer_id, name)?; }
                if let Some(visible) = visible { drawing.set_layer_visibility(layer_id, visible)?; }
                if let Some(locked) = locked { drawing.set_layer_locked(layer_id, locked)?; }
                Ok(json!({"layer":drawing.layers.iter().find(|layer| layer.id == layer_id)}))
            })
        }
        "drawing_move_layer" => {
            let args = object_args(args, &["id","title","layer_id","position"])?;
            let layer_id = drawing_id(args, "layer_id")?;
            let position = required_i32(args, "position", 0, 63)? as usize;
            drawing_change(store, args, |drawing| {
                drawing.move_layer(layer_id, position)?;
                Ok(json!({"layer_id":layer_id,"position":position}))
            })
        }
        "drawing_delete_layer" => {
            let args = object_args(args, &["id","title","layer_id","confirm"])?;
            require_confirmation(args, "Deleting this drawing layer")?;
            let layer_id = drawing_id(args, "layer_id")?;
            drawing_change(store, args, |drawing| {
                drawing.remove_layer(layer_id)?;
                Ok(json!({"deleted":true,"layer_id":layer_id,"layer_count":drawing.layers.len()}))
            })
        }
        "drawing_pick_object" => {
            let args = object_args(args, &["id","title","x","y","tolerance"])?;
            let (by_title, key) = selector(args, "title")?;
            let x = draw_number(args, "x", -16384.0, 16384.0)?;
            let y = draw_number(args, "y", -16384.0, 16384.0)?;
            let tolerance = if args.contains_key("tolerance") { draw_number(args, "tolerance", 0.0, 32.0)? } else { 5.0 };
            let workspace = load(store)?;
            let note = &workspace.notes[find_note_index(&workspace, by_title, &key)?];
            note.drawing.validate()?;
            let object_id = crate::canvas::pick_object(&note.drawing, Point::new(x, y), tolerance);
            Ok(json!({"id":note.id,"object":object_id.as_ref().and_then(|id| note.drawing.object(id)),
                "object_id":object_id,"x":x,"y":y,"tolerance":tolerance}))
        }
        "drawing_preview" => {
            let args = object_args(args, &["id","title"])?;
            let (by_title, key) = selector(args, "title")?;
            let workspace = load(store)?;
            let note = &workspace.notes[find_note_index(&workspace, by_title, &key)?];
            note.drawing.validate()?;
            let png = drawing_preview_png(store, note)?;
            Ok(json!({"id":note.id,"mime_type":"image/png","width":WIDTH,"height":HEIGHT,
                "data_base64":base64::engine::general_purpose::STANDARD.encode(&png),"bytes":png.len(),
                "object_count":note.drawing.objects.len(),"legacy_stroke_count":note.strokes.len()}))
        }
        "drawing_undo" | "drawing_redo" => {
            let args = object_args(args, &["id","title","confirm"])?;
            require_confirmation(args, if name == "drawing_undo" { "Undoing a drawing edit" } else { "Redoing a drawing edit" })?;
            let (by_title, key) = selector(args, "title")?;
            let id = {
                let workspace = load(store)?;
                workspace.notes[find_note_index(&workspace, by_title, &key)?].id.clone()
            };
            let mut sessions = drawing_sessions().lock().map_err(|_| invalid("Drawing history lock unavailable"))?;
            let session = sessions.get_mut(&drawing_session_key(store, &id))
                .ok_or_else(|| invalid("No drawing history in this agent session"))?;
            let expected = session.expected.as_ref().ok_or_else(|| invalid("No tracked drawing history"))?.clone();
            let target = if name == "drawing_undo" { session.undo.last() } else { session.redo.last() }
                .ok_or_else(|| invalid("No drawing history in that direction"))?.clone();
            let (id, new_drawing) = mutate(store, |workspace| {
                let index = find_note_index(workspace, false, &id)?;
                let note = &mut workspace.notes[index];
                if note.drawing != expected {
                    return Err(invalid("Drawing changed outside this agent session; undo/redo refused to avoid losing edits"));
                }
                note.drawing.restore_snapshot(&target)?;
                note.touch();
                Ok((note.id.clone(), note.drawing.snapshot()))
            })?;
            if name == "drawing_undo" {
                session.undo.pop();
                session.redo.push(expected);
                if session.redo.len() > DRAWING_HISTORY_LIMIT { session.redo.remove(0); }
            } else {
                session.redo.pop();
                session.undo.push(expected);
                if session.undo.len() > DRAWING_HISTORY_LIMIT { session.undo.remove(0); }
            }
            session.expected = Some(new_drawing.clone());
            Ok(json!({"id":id,"action":name,"object_count":new_drawing.objects.len(),
                "layer_count":new_drawing.layers.len(),"undo_available":!session.undo.is_empty(),
                "redo_available":!session.redo.is_empty()}))
        }
        "list_notes" => {
            let args = object_args(args, &["limit"])?;
            let limit = limit(args)?;
            let workspace = load(store)?;
            let notes: Vec<_> = ordered_notes(&workspace.notes, limit).into_iter().map(note_summary).collect();
            Ok(json!({"total":workspace.notes.len(),"notes":notes}))
        }
        "search_notes" => {
            let args = object_args(args, &["query","limit","include_body","sqlite_index"])?;
            let query = required_string(args, "query")?;
            if query.trim().is_empty() || query.chars().count() > MAX_SEARCH_CHARS {
                return Err(invalid("query must be 1–1000 nonblank characters"));
            }
            let query = query.to_lowercase();
            let limit = limit(args)?;
            let include_body = args.get("include_body")
                .map(|_| required_bool(args, "include_body")).transpose()?.unwrap_or(false);
            if include_body && limit > MAX_SEARCH_WITH_BODY_RESULTS {
                return Err(invalid("include_body requires limit between 1 and 10"));
            }
            // Release measurements showed the existing parsed in-memory index
            // wins on a persistent MCP connection. SQLite remains opt-in until
            // it proves faster in the actual workload (including cold build).
            let use_sqlite = args.get("sqlite_index")
                .map(|_| required_bool(args, "sqlite_index")).transpose()?.unwrap_or(false);
            let (total, notes): (usize, Vec<Value>) = if use_sqlite {
                let found = store.search_indexed(&query).map_err(|error| error.to_string())?;
                let total = found.len();
                let notes = ordered_notes(&found, limit).into_iter().map(|note| {
                    if include_body { search_note_with_body(note) } else { note_summary(note) }
                }).collect();
                (total, notes)
            } else {
                let (workspace, search_index) = store.read_cached_with_search_index()
                    .map_err(|error| error.to_string())?;
                // Filter in source order, then sort only matches. A selective query
                // should not sort thousands of unrelated notes.
                let found: Vec<_> = workspace.notes.iter().zip(search_index.iter())
                    .filter(|(_, (title, body))| title.contains(&query) || body.contains(&query))
                    .map(|(note, _)| note).collect();
                let total = found.len();
                let notes = ordered_notes(found, limit).into_iter().map(|note| {
                    if include_body { search_note_with_body(note) } else { note_summary(note) }
                }).collect();
                (total, notes)
            };
            Ok(json!({"total":total,"notes":notes}))
        }
        "read_note" => {
            let args = object_args(args, &["id","title","compact"])?;
            let (by_title, key) = selector(args, "title")?;
            let compact = args.get("compact").map(|_| required_bool(args, "compact")).transpose()?.unwrap_or(false);
            let workspace = load(store)?;
            let index = find_note_index(&workspace, by_title, &key)?;
            let note = &workspace.notes[index];
            if compact {
                Ok(compact_note(note))
            } else {
                serde_json::to_value(note).map_err(|e| e.to_string())
            }
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
                let mut edited = note.body.clone();
                edited.push_str(separator);
                edited.push_str(text);
                note.set_body_preserving_format(edited).map_err(|error| error.to_string())?;
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
                if let Some(body) = body { note.set_body_preserving_format(body.to_string()).map_err(|error| error.to_string())?; }
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
            let args = object_args(args, &["compact"])?;
            let compact = args.get("compact")
                .map(|_| required_bool(args, "compact")).transpose()?.unwrap_or(false);
            let workspace = load(store)?;
            let note = workspace.notes.iter().find(|note| note.id == workspace.active_id);
            if compact {
                Ok(json!({"active_id":workspace.active_id,"note":note.map(compact_note)}))
            } else {
                Ok(json!({"active_id":workspace.active_id,"note":note}))
            }
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
        "add_strokes_batch" => {
            let args = object_args(args, &["id","title","strokes"])?;
            let (by_title, key) = selector(args, "title")?;
            let items = args.get("strokes").and_then(Value::as_array)
                .filter(|items| !items.is_empty() && items.len() <= MAX_BATCH_STROKES)
                .ok_or_else(|| invalid("strokes must contain 1–64 pen or eraser strokes"))?;
            let strokes: Vec<Stroke> = items.iter().enumerate().map(|(index, item)| {
                object_args(item, &["width","points","color","eraser"])
                    .and_then(parse_stroke).map_err(|error| format!("Stroke {index}: {error}"))
            }).collect::<Result<_,_>>()?;
            let added_points = strokes.iter().map(|stroke| stroke.points.len()).sum::<usize>();
            let added_strokes = strokes.len();
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                if note.strokes.len().saturating_add(strokes.len()) > MAX_STROKES_PER_NOTE {
                    return Err(format!("Cannot add more than {MAX_STROKES_PER_NOTE} strokes to one page"));
                }
                note.strokes.extend(strokes);
                note.touch();
                Ok(json!({"id":note.id,"stroke_count":note.strokes.len(),
                    "added_strokes":added_strokes,"added_points":added_points}))
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
                    workspace.active_id = ordered_notes(&workspace.notes, 1).first().map(|note| note.id.clone()).unwrap_or_default();
                }
                Ok(json!({"deleted":true,"id":deleted.id,"title":deleted.title,"deleted_strokes":deleted.strokes.len()}))
            })
        }
        "image_insert_file" => {
            let args = object_args(args, &["id", "title", "path", "user_approved"])?;
            image_approval(args, "Reading a local image file")?;
            let note_id = resolve_note_id(store, args)?;
            let path = approved_image_path(store, required_string(args, "path")?)?;
            let image = store.insert_image_from_file(&note_id, &path).map_err(|e| e.to_string())?;
            Ok(json!({"note_id":note_id,"image":image,"source_file_unchanged":true}))
        }
        "image_insert_base64" => {
            let args = object_args(args, &["id", "title", "data_base64", "user_approved"])?;
            image_approval(args, "Importing image bytes")?;
            let note_id = resolve_note_id(store, args)?;
            let bytes = decode_image_base64(required_string(args, "data_base64")?)?;
            let image = store.insert_image_bytes(&note_id, &bytes).map_err(|e| e.to_string())?;
            Ok(json!({"note_id":note_id,"image":image,"bytes_imported":bytes.len()}))
        }
        "image_list" => {
            let args = object_args(args, &["id", "title"])?;
            let note_id = resolve_note_id(store, args)?;
            let images = store.list_media(&note_id).map_err(|e| e.to_string())?;
            Ok(json!({"note_id":note_id,"total":images.len(),"images":images}))
        }
        "image_resize" => {
            let args = object_args(args, &["id", "title", "image_id", "width_percent"])?;
            let image_id = image_identity(args)?;
            let width = required_i32(args, "width_percent", 10, 100)? as u8;
            let note_id = resolve_note_id(store, args)?;
            let image = store.update_image(&note_id, image_id, None, Some(width), None).map_err(|e| e.to_string())?;
            Ok(json!({"note_id":note_id,"image":image}))
        }
        "image_move" => {
            let args = object_args(args, &["id", "title", "image_id", "position"])?;
            let image_id = image_identity(args)?;
            let position = args.get("position").and_then(Value::as_u64)
                .and_then(|number| usize::try_from(number).ok()).filter(|number| *number <= 4096)
                .ok_or_else(|| invalid("position must be a zero-based integer (0..4096)"))?;
            let note_id = resolve_note_id(store, args)?;
            let image = store.update_image(&note_id, image_id, None, None, Some(position)).map_err(|e| e.to_string())?;
            Ok(json!({"note_id":note_id,"image":image}))
        }
        "image_place" => {
            let args = object_args(args, &["id", "title", "image_id", "unit",
                "char_offset", "expected_body"])?;
            if required_string(args, "unit")? != "unicode_scalar" {
                return Err(invalid("unit must be unicode_scalar"));
            }
            let image_id = image_identity(args)?;
            let offset = match args.get("char_offset") {
                Some(Value::Null) => None,
                Some(value) => Some(value.as_u64()
                    .and_then(|number| usize::try_from(number).ok())
                    .filter(|number| *number <= MAX_BODY_BYTES)
                    .ok_or_else(|| invalid("char_offset must be a Unicode scalar index or null"))?),
                None => return Err(invalid("char_offset is required; use null to unplace image")),
            };
            let expected_body = optional_string(args, "expected_body")?;
            let note_id = resolve_note_id(store, args)?;
            store.place_image_at_scalar(&note_id, image_id, offset, expected_body)
                .map_err(|error| error.to_string())?;
            Ok(json!({"note_id":note_id,"image_id":image_id,"unit":"unicode_scalar",
                "char_offset":offset}))
        }
        "image_caption" => {
            let args = object_args(args, &["id", "title", "image_id", "caption"])?;
            let image_id = image_identity(args)?;
            let caption = required_string(args, "caption")?;
            if caption.chars().count() > MAX_IMAGE_CAPTION_CHARS || caption.contains('\0') {
                return Err(invalid("Image caption cannot exceed 500 characters or contain NUL"));
            }
            let note_id = resolve_note_id(store, args)?;
            let image = store.update_image(&note_id, image_id, Some(caption), None, None).map_err(|e| e.to_string())?;
            Ok(json!({"note_id":note_id,"image":image}))
        }
        "image_delete" => {
            let args = object_args(args, &["id", "title", "image_id", "confirm"])?;
            require_confirmation(args, "Permanently removing this image")?;
            let image_id = image_identity(args)?;
            let note_id = resolve_note_id(store, args)?;
            store.delete_image(&note_id, image_id).map_err(|e| e.to_string())?;
            Ok(json!({"note_id":note_id,"image_id":image_id,"deleted":true}))
        }
        "image_export_html" => {
            let args = object_args(args, &["id", "title", "path", "user_approved"])?;
            image_approval(args, "Exporting private note content")?;
            let note_id = resolve_note_id(store, args)?;
            let path = new_export_destination(store, required_string(args, "path")?, "html")?;
            store.export_note_html(&note_id, &path).map_err(|e| e.to_string())?;
            let bytes = path.metadata().map_err(|e| e.to_string())?.len();
            Ok(json!({"note_id":note_id,"path":path.display().to_string(),"format":"html","bytes":bytes,"self_contained":true}))
        }
        "read_image" => {
            let args = object_args(args, &["id", "title", "image_id", "user_approved"])?;
            image_approval(args, "Reading private picture content")?;
            let image_id = image_identity(args)?;
            let note_id = resolve_note_id(store, args)?;
            let image = note_image(store, &note_id, image_id)?;
            let bytes = read_image_file_bounded(store, &image)?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(json!({"note_id":note_id,"image_id":image_id,"mime_type":"image/png",
                "bytes":bytes.len(),"data_base64":encoded}))
        }
        "export_image" => {
            let args = object_args(args, &["id", "title", "image_id", "path", "user_approved"])?;
            image_approval(args, "Exporting private picture content")?;
            let image_id = image_identity(args)?;
            let note_id = resolve_note_id(store, args)?;
            let image = note_image(store, &note_id, image_id)?;
            let destination = new_export_destination(store, required_string(args, "path")?, "png")?;
            let input = store.media_path(&image).map_err(|e| e.to_string())?;
            let mut reader = OpenOptions::new().read(true).open(input).map_err(|e| e.to_string())?;
            let mut writer = OpenOptions::new().write(true).create_new(true).open(&destination)
                .map_err(|e| format!("Cannot create new image export (no overwrite): {e}"))?;
            let result = io::copy(&mut reader, &mut writer).and_then(|count| { writer.sync_all()?; Ok(count) });
            let bytes = match result {
                Ok(bytes) => bytes,
                Err(error) => {
                    drop(writer);
                    let _ = std::fs::remove_file(&destination);
                    return Err(format!("Could not finish PNG export: {error}"));
                }
            };
            Ok(json!({"note_id":note_id,"image_id":image_id,"mime_type":"image/png",
                "path":destination.display().to_string(),"bytes":bytes,"exported":true}))
        }
        "get_workspace_info" => {
            object_args(args, &[])?;
            let workspace = load(store)?;
            Ok(json!({"version":workspace.version,"notes_count":workspace.notes.len(),
                "active_id":workspace.active_id,"storage_file":store.data_path().display().to_string()}))
        }
        "get_app_settings" => {
            object_args(args, &[])?;
            let settings = UiSettings::load(store.data_path()).map_err(|e| e.to_string())?;
            serde_json::to_value(settings).map_err(|e| e.to_string())
        }
        "set_app_theme" => {
            let args = object_args(args, &["theme"])?;
            let theme = required_string(args, "theme")?;
            if !["light", "dark", "system"].contains(&theme) {
                return Err(invalid("theme must be light, dark or system"));
            }
            let appearance = serde_json::from_value::<AppearanceTheme>(json!(theme))
                .map_err(|e| format!("Invalid theme: {e}"))?;
            let updated = UiSettings::update(store.data_path(), |settings| settings.theme = appearance)
                .map_err(|e| e.to_string())?;
            serde_json::to_value(updated).map_err(|e| e.to_string())
        }
        "set_sidebar_collapsed" => {
            let args = object_args(args, &["collapsed"])?;
            let collapsed = required_bool(args, "collapsed")?;
            let updated = UiSettings::update(store.data_path(), |settings| settings.sidebar_collapsed = collapsed)
                .map_err(|e| e.to_string())?;
            serde_json::to_value(updated).map_err(|e| e.to_string())
        }
        "get_note_formatting" => {
            let args = object_args(args, &["id","title"])?;
            let (by_title, key) = selector(args, "title")?;
            let workspace = load(store)?;
            let index = find_note_index(&workspace, by_title, &key)?;
            let note = &workspace.notes[index];
            note.validate_rich_text().map_err(|e| e.to_string())?;
            Ok(json!({"id":note.id,"title":note.title,"unit":"unicode_scalar",
                "length":note.body.chars().count(),"runs":note.rich_text,
                "supported_attributes":["bold","color","size","font_family"]}))
        }
        "format_text_range" => {
            let args = object_args(args, &["id","title","start","end","unit","bold","color",
                "size","font_family","reset_color","reset_size","reset_font_family"])?;
            let (by_title, key) = selector(args, "title")?;
            let (start, end) = scalar_range(args, false)?;
            let patch = rich_style_patch(args)?;
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                note.format_text_range(start, end, patch).map_err(|e| e.to_string())?;
                note.touch();
                Ok(json!({"id":note.id,"unit":"unicode_scalar","start":start,"end":end,
                    "length":note.body.chars().count(),"runs":note.rich_text}))
            })
        }
        "clear_text_formatting" => {
            let args = object_args(args, &["id","title","start","end","unit"])?;
            let (by_title, key) = selector(args, "title")?;
            let (start, end) = scalar_range(args, false)?;
            let clear = TextStylePatch { bold:Some(false), reset_color:true, reset_size:true,
                reset_font_family:true, ..Default::default() };
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                note.format_text_range(start, end, clear).map_err(|e| e.to_string())?;
                note.touch();
                Ok(json!({"id":note.id,"unit":"unicode_scalar","start":start,"end":end,
                    "length":note.body.chars().count(),"runs":note.rich_text}))
            })
        }
        "replace_text_range" => {
            let args = object_args(args, &["id","title","start","end","unit","text"])?;
            let (by_title, key) = selector(args, "title")?;
            let (start, end) = scalar_range(args, true)?;
            let text = required_string(args, "text")?;
            validate_body(text)?;
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                note.replace_text_range(start, end, text).map_err(|e| e.to_string())?;
                validate_body(&note.body)?;
                note.touch();
                Ok(json!({"id":note.id,"title":note.title,"unit":"unicode_scalar",
                    "length":note.body.chars().count(),"body":note.body,"runs":note.rich_text}))
            })
        }
        "format_text_batch" => {
            let args = object_args(args, &["id","title","unit","boundary_policy","expected_body","operations"])?;
            if required_string(args, "unit")? != "unicode_scalar" {
                return Err(invalid("unit must be unicode_scalar"));
            }
            let grapheme_safe = boundary_policy(args)?;
            let (by_title, key) = selector(args, "title")?;
            let expected_body = optional_string(args, "expected_body")?.map(str::to_owned);
            let ops = args.get("operations").and_then(Value::as_array)
                .filter(|ops| !ops.is_empty() && ops.len() <= MAX_BATCH_FORMAT_OPS)
                .ok_or_else(|| invalid("operations must have 1 to 32 patches"))?;
            let mut parsed = Vec::with_capacity(ops.len());
            for (i, op) in ops.iter().enumerate() {
                let fields = object_args(op, &["start","end","bold","color","size",
                    "font_family","reset_color","reset_size","reset_font_family"])
                    .map_err(|error| format!("operations[{i}]: {error}"))?;
                let start = fields.get("start").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok())
                    .ok_or_else(|| format!("operations[{i}].start must be nonnegative integer"))?;
                let end = fields.get("end").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok())
                    .ok_or_else(|| format!("operations[{i}].end must be nonnegative integer"))?;
                if start >= end { return Err(format!("operations[{i}] requires start < end")); }
                parsed.push((start, end, rich_style_patch(fields)
                    .map_err(|error| format!("operations[{i}]: {error}"))?));
            }
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                if expected_body.as_deref().is_some_and(|body| body != note.body) {
                    return Err(invalid("Note body changed since expected_body; refresh before applying batch"));
                }
                for (start, end, _) in &parsed {
                    check_grapheme_boundaries(&note.body, *start, *end, grapheme_safe)?;
                }
                for (start, end, patch) in parsed {
                    note.format_text_range(start, end, patch)?;
                }
                Ok(json!({"id":note.id,"unit":"unicode_scalar","applied":ops.len(),
                    "length":note.body.chars().count(),"runs":note.rich_text}))
            })
        }
        "copy_rich_fragment" => {
            let args = object_args(args, &["id","title","unit","start","end","boundary_policy"])?;
            let (start, end) = scalar_range(args, true)?;
            let grapheme_safe = boundary_policy(args)?;
            let (by_title, key) = selector(args, "title")?;
            let workspace = load(store)?;
            let index = find_note_index(&workspace, by_title, &key)?;
            let note = &workspace.notes[index];
            check_grapheme_boundaries(&note.body, start, end, grapheme_safe)?;
            serde_json::to_value(build_fragment(note, start, end)?).map_err(|e| e.to_string())
        }
        "paste_rich_fragment" => {
            let args = object_args(args, &["id","title","unit","start","end",
                "boundary_policy","expected_text","fragment"])?;
            let (start, end) = scalar_range(args, true)?;
            let grapheme_safe = boundary_policy(args)?;
            let (by_title, key) = selector(args, "title")?;
            let expected = optional_string(args, "expected_text")?.map(str::to_owned);
            let fragment = parse_fragment(args.get("fragment")
                .ok_or_else(|| invalid("fragment is required"))?)?;
            mutate(store, |workspace| {
                let index = find_note_index(workspace, by_title, &key)?;
                let note = &mut workspace.notes[index];
                check_grapheme_boundaries(&note.body, start, end, grapheme_safe)?;
                if expected.as_deref().is_some_and(|original| scalar_slice(&note.body,start,end).ok() != Some(original)) {
                    return Err(invalid("Destination selection changed since expected_text; refresh before pasting"));
                }
                let final_bytes = note.body.len()
                    .saturating_sub(scalar_slice(&note.body,start,end)?.len())
                    .saturating_add(fragment.text.len());
                if final_bytes > MAX_BODY_BYTES { return Err(invalid("Pasted note would exceed 1 MiB")); }
                paste_fragment(note, start, end, &fragment)?;
                Ok(json!({"id":note.id,"unit":"unicode_scalar",
                    "inserted":fragment.text.chars().count(),
                    "length":note.body.chars().count(),"runs":note.rich_text}))
            })
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
        "add-image" if args.len() == 4 && args[3] == "--approved" =>
            execute_tool(store, "image_insert_file", &json!({"id":args[1],"path":args[2],"user_approved":true})),
        "add-image-base64" if args.len() == 4 && args[3] == "--approved" =>
            cli_base64_arg(&args[2]).and_then(|encoded| execute_tool(store, "image_insert_base64",
                &json!({"id":args[1],"data_base64":encoded,"user_approved":true}))),
        "images" if args.len() == 2 => execute_tool(store, "image_list", &json!({"id":args[1]})),
        "image-width" if args.len() == 4 => args[3].parse::<i32>().map_err(|_| invalid("Image width must be an integer"))
            .and_then(|width| execute_tool(store, "image_resize", &json!({"id":args[1],"image_id":args[2],"width_percent":width}))),
        "image-move" if args.len() == 4 => args[3].parse::<usize>().map_err(|_| invalid("Image position must be a nonnegative integer"))
            .and_then(|position| execute_tool(store, "image_move", &json!({"id":args[1],"image_id":args[2],"position":position}))),
        "image-caption" if args.len() >= 4 => execute_tool(store, "image_caption", &json!({"id":args[1],"image_id":args[2],"caption":args[3..].join(" ")})),
        "image-delete" if args.len() == 4 && args[3] == "--confirm" =>
            execute_tool(store, "image_delete", &json!({"id":args[1],"image_id":args[2],"confirm":true})),
        "export-html" if args.len() == 4 && args[3] == "--approved" =>
            execute_tool(store, "image_export_html", &json!({"id":args[1],"path":args[2],"user_approved":true})),
        "read-image" if args.len() == 4 && args[3] == "--approved" =>
            execute_tool(store, "read_image", &json!({"id":args[1],"image_id":args[2],"user_approved":true})),
        "export-image" if args.len() == 5 && args[4] == "--approved" =>
            execute_tool(store, "export_image", &json!({"id":args[1],"image_id":args[2],"path":args[3],"user_approved":true})),
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
        "settings" if args.len() == 1 => execute_tool(store, "get_app_settings", &json!({})),
        "theme" if args.len() == 2 => execute_tool(store, "set_app_theme", &json!({"theme":args[1]})),
        "sidebar" if args.len() == 2 => match args[1].as_str() {
            "true" | "collapsed" => execute_tool(store, "set_sidebar_collapsed", &json!({"collapsed":true})),
            "false" | "expanded" => execute_tool(store, "set_sidebar_collapsed", &json!({"collapsed":false})),
            _ => Err(invalid("sidebar must be true/false or collapsed/expanded")),
        },
        "formatting" if args.len() == 2 => execute_tool(store, "get_note_formatting", &json!({"id":args[1]})),
        "format-range" if args.len() >= 5 => cli_range_args(args, true)
            .and_then(|values| execute_tool(store, "format_text_range", &Value::Object(values))),
        "clear-format" if args.len() == 4 => cli_range_args(args, false)
            .and_then(|values| execute_tool(store, "clear_text_formatting", &Value::Object(values))),
        "replace-range" if args.len() >= 5 => cli_range_args(args, false)
            .and_then(|mut values| {
                values.insert("text".into(), json!(args[4..].join(" ")));
                execute_tool(store, "replace_text_range", &Value::Object(values))
            }),
        "format-batch" if args.len() == 3 => cli_rich_json(&args[2])
            .and_then(|value| {
                let fields = object_args(&value, &["operations","expected_body","boundary_policy"])?;
                let mut params = fields.clone();
                params.insert("id".into(), json!(args[1]));
                params.insert("unit".into(), json!("unicode_scalar"));
                execute_tool(store, "format_text_batch", &Value::Object(params))
            }),
        "copy-fragment" if args.len() == 4 || args.len() == 5 && args[4] == "--scalar" => {
            cli_range_args(args, false).and_then(|mut fields| {
                if args.len() == 5 { fields.insert("boundary_policy".into(), json!("scalar")); }
                execute_tool(store, "copy_rich_fragment", &Value::Object(fields))
            })
        },
        "paste-fragment" if args.len() == 5 || args.len() == 6 && args[5] == "--scalar" => {
            cli_range_args(args, false).and_then(|mut fields| {
                if args.len() == 6 { fields.insert("boundary_policy".into(), json!("scalar")); }
                let fragment = cli_rich_json(&args[4])?;
                fields.insert("fragment".into(), fragment);
                execute_tool(store, "paste_rich_fragment", &Value::Object(fields))
            })
        },
        "--help" | "-h" | "help" => {
            println!("Inkpad headless agent\n\nMCP stdio: inkpad-agent.exe [mcp]\nCLI:\n  list | search TEXT | get ID | info | active | activate ID\n  create [--title TITLE] [--body TEXT] | append ID TEXT | update ID [--title TITLE] [--body TEXT]\n  rename ID NEW_TITLE | pin ID | unpin ID | font-size ID 12..40 | font-choice ID Sans|Serif|Mono\n  export-text ID ABS_PATH.txt | export-png ID ABS_PATH.png\n  add-stroke ID WIDTH COLOR X,Y [X,Y ...] [--eraser]\n  add-stroke ID @ABS_PATH.json  (PowerShell-friendly file-backed JSON)\n  undo-stroke ID --confirm | clear-strokes ID --confirm | delete ID --confirm\n  add-image ID ABS_IMAGE_PATH --approved\n  add-image-base64 ID @ABS_PATH.b64 --approved (max 1 MiB decoded)\n  images ID | image-width ID IMAGE_ID 10..100 | image-move ID IMAGE_ID POSITION\n  image-caption ID IMAGE_ID TEXT | image-delete ID IMAGE_ID --confirm\n  export-html ID ABS_PATH.html --approved (self-contained)\n  read-image ID IMAGE_ID --approved (max 1 MiB)\n  export-image ID IMAGE_ID ABS_PATH.png --approved\n  settings | theme light|dark|system | sidebar collapsed|expanded\n  formatting ID | format-range ID START END [--bold true|false] [--color #RRGGBB] [--size 12..40] [--family Sans|Serif|Mono] [--reset-color] [--reset-size] [--reset-family]\n  clear-format ID START END | replace-range ID START END TEXT\n  format-batch ID @ABS_PATH.json  (1..32 ordered operations; optional expected_body)\n  copy-fragment ID START END [--scalar] | paste-fragment ID START END @ABS_PATH.json [--scalar]\n  (START/END are Unicode scalar offsets; new fragment/batch tools default to grapheme safe)\n  (No OS clipboard access and no persisted note-text undo/redo history)\n\nSource files require explicit user approval; destructive actions require explicit user authorization.\nStorage: %LOCALAPPDATA%\\Inkpad\\workspace.json (override with INKPAD_DATA_DIR)\nGUI Text/Draw view mode is transient, not a stored setting.");
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
    fn note_batch_commits_ordered_unicode_edits_and_preserves_note_media_drawing_and_styles() {
        let (store, folder) = store();
        let original = tool(&store, "create_note", json!({"title":"画 / notes","body":"Bonjour 猫"}));
        let id = original["id"].as_str().unwrap();
        let active = load(&store).unwrap().active_id.clone();
        tool(&store, "add_stroke", json!({"id":id,"width":3,"points":[[7,8],[12,13]]}));
        tool(&store, "format_text_range", json!({"id":id,"unit":"unicode_scalar","start":0,"end":2,"bold":true}));
        let image = tool(&store, "image_insert_base64", json!({"id":id,
            "data_base64":base64::engine::general_purpose::STANDARD.encode(tiny_png()),"user_approved":true}));
        let image_id = image["image"]["id"].as_str().unwrap();
        tool(&store, "image_place", json!({"id":id,"image_id":image_id,
            "unit":"unicode_scalar","char_offset":2}));
        let before = tool(&store, "read_note", json!({"id":id}));

        let result = tool(&store, "note_batch", json!({"operations":[
            {"op":"create","title":"🦊","body":"新しい note"},
            {"op":"create","title":"Second","body":"Prior"},
            {"op":"append","id":id,"text":"👩‍🚀 café","expected_body":"Bonjour 猫"},
            {"op":"rename","id":id,"new_title":"画 / renamed"},
            {"op":"pin","title":"画 / renamed","pinned":true},
            {"op":"update","match_title":"Second","body":"After 日","title":"Done","expected_body":"Prior","confirm":true}
        ]}));
        assert_eq!(result["applied"], 6);
        assert_eq!(result["results"].as_array().unwrap().len(), 6);
        for (index, kind) in ["create","create","append","rename","pin","update"].iter().enumerate() {
            assert_eq!(result["results"][index]["index"], index);
            assert_eq!(result["results"][index]["op"], *kind);
        }
        assert_ne!(result["results"][0]["id"], result["results"][1]["id"]);
        assert_ne!(result["results"][0]["id"], id);
        let after = tool(&store, "read_note", json!({"id":id}));
        assert_eq!(after["body"], "Bonjour 猫\n👩‍🚀 café");
        assert_eq!(after["title"], "画 / renamed");
        assert_eq!(after["pinned"], true);
        for preserved in ["strokes","drawing","images","inline_media","rich_text","kind","font_size","font_choice"] {
            assert_eq!(after[preserved], before[preserved], "Batch lost {preserved}");
        }
        assert_eq!(tool(&store, "read_note", json!({"title":"Done"}))["body"], "After 日");
        assert_eq!(load(&store).unwrap().active_id, active);
        assert_eq!(load(&store).unwrap().notes.len(), 4);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn note_batch_late_failure_ambiguous_titles_and_stale_guard_are_fully_atomic() {
        let (store, folder) = store();
        let first = tool(&store, "create_note", json!({"title":"Twin","body":"original 👩‍🚀"}));
        tool(&store, "create_note", json!({"title":"Twin","body":"another"}));
        let id = first["id"].as_str().unwrap();
        let before = std::fs::read(store.data_path()).unwrap();
        let snapshot = serde_json::to_value(&*load(&store).unwrap()).unwrap();
        for operations in [
            json!([
                {"op":"append","id":id,"text":"valid"},
                {"op":"create","title":"Transient"},
                {"op":"rename","id":"missing","new_title":"Nope"}
            ]),
            json!([
                {"op":"create","title":"Transient"},
                {"op":"append","title":"Twin","text":"ambiguous"}
            ]),
            json!([
                {"op":"pin","id":id,"pinned":true},
                {"op":"update","id":id,"body":"Changed","expected_body":"stale","confirm":true}
            ]),
        ] {
            let err = execute_tool(&store, "note_batch", &json!({"operations":operations})).unwrap_err();
            assert!(err.contains("Operation"), "Unidentified batch error: {err}");
            assert_eq!(std::fs::read(store.data_path()).unwrap(), before, "Failed batch wrote disk");
            assert_eq!(serde_json::to_value(&*load(&store).unwrap()).unwrap(), snapshot);
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn note_batch_rejects_unknown_destructive_and_malformed_operations_without_writes() {
        let (store, folder) = store();
        let original = tool(&store, "create_note", json!({"title":"Protected","body":"Keep"}));
        let id = original["id"].as_str().unwrap();
        let before = std::fs::read(store.data_path()).unwrap();
        let invalid = vec![
            json!({"operations":[]}),
            json!({"operations":vec![json!({"op":"create"});33]}),
            json!({"operations":[{"op":"delete","id":id,"confirm":true}]}),
            json!({"operations":[{"op":"clear","id":id,"confirm":true}]}),
            json!({"operations":[{"op":"append","id":id,"text":null}]}),
            json!({"operations":[{"op":"append","id":id,"text":""}]}),
            json!({"operations":[{"op":"pin","id":id,"pinned":"true"}]}),
            json!({"operations":[{"op":"rename","id":id,"new_title":"   "}]}),
            json!({"operations":[{"op":"update","id":id}]}),
            json!({"operations":[{"op":"update","id":id,"body":"Not authorized"}]}),
            json!({"operations":[{"op":"update","id":id,"body":"Not authorized","confirm":false}]}),
            json!({"operations":[{"op":"update","id":id,"title":"Safe rename","confirm":true}]}),
            json!({"operations":[{"op":"create","title":"x","id":"forbidden"}]}),
            json!({"operations":[{"op":"create","title":"Valid"},{"op":"append","id":id,"text":"x","unknown":1}]}),
            json!({"operations":[{"op":"update","id":id,"title":"A","body":1}]}),
            json!({"operations":[{"op":"pin","id":id,"title":"Protected","pinned":true}]}),
            json!({"operations":[{"op":"append","title":"Protected","text":"x","expected_body":5}]}),
            json!({"operations":[{"op":"create","body":"x".repeat(MAX_BODY_BYTES+1)}]}),
            json!({"operations":[{"op":"create"}],"unknown":true}),
        ];
        for request in invalid {
            assert!(execute_tool(&store, "note_batch", &request).is_err(), "Accepted {request}");
            assert_eq!(std::fs::read(store.data_path()).unwrap(), before, "Invalid batch wrote workspace");
        }
        let schema = tool_definitions().iter().find(|tool| tool["name"] == "note_batch").unwrap();
        assert_eq!(schema["inputSchema"]["properties"]["operations"]["maxItems"], 32);
        assert_eq!(schema["inputSchema"]["properties"]["operations"]["items"]["oneOf"].as_array().unwrap().len(), 5);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn list_and_search_identify_vector_only_drawing_without_legacy_strokes() {
        let (store, folder) = store();
        let created = tool(&store, "create_note", json!({"title":"Vector mallard 🦆"}));
        let id = created["id"].as_str().unwrap();
        tool(&store, "drawing_create_batch", json!({"id":id,"objects":[
            {"shape":{"type":"ellipse","radius_x":20,"radius_y":10}},
            {"shape":{"type":"circle","radius":4}}
        ]}));
        let listed = tool(&store, "list_notes", json!({"limit":200}));
        let found = listed["notes"].as_array().unwrap().iter()
            .find(|value| value["id"] == id).unwrap();
        assert_eq!(found["stroke_count"], 0);
        assert_eq!(found["has_drawing"], true);
        assert_eq!(found["drawing_object_count"], 2);
        assert_eq!(found["drawing_layer_count"], 1);
        let searched = tool(&store, "search_notes", json!({"query":"mallard"}));
        let hit = &searched["notes"][0];
        assert_eq!(hit["id"], id);
        assert_eq!(hit["has_drawing"], true);
        assert_eq!(hit["drawing_object_count"], 2);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn drawing_create_scene_atomic_layers_512_shapes_and_one_step_undo() {
        let (store, folder) = store();
        let created = tool(&store, "create_note", json!({"title":"Lake 🦆", "body":"Preserve 🐈"}));
        let id = created["id"].as_str().unwrap();
        let before = tool(&store, "read_note", json!({"id":id}));
        let shapes: Vec<Value> = (0..512).map(|i| json!({
            "shape":{"type":"ellipse","radius_x":2.0,"radius_y":4.0},
            "layer_index":i % 2,
            "transform":{"translation":{"x":i % 100,"y":i / 2}},
            "style":{"stroke":null,"fill":{"type":"solid","color":[15,45,85,255]}}
        })).collect();
        let request = json!({"id":id,"expected_object_count":0,
            "layers":[{"name":"背景 🌅"},{"name":"羽根 🦆"}],"objects":shapes});
        // An invalid 301st object cannot leave behind even the newly created layers.
        let mut broken = request.clone();
        broken["objects"][300]["layer_index"] = json!(9);
        let old_bytes = std::fs::read(store.data_path()).unwrap();
        assert!(execute_tool(&store,"drawing_create_scene",&broken).is_err());
        assert_eq!(std::fs::read(store.data_path()).unwrap(),old_bytes);
        let reply = tool(&store,"drawing_create_scene",request);
        assert_eq!(reply["added_objects"],512);
        assert_eq!(reply["added_layers"],2);
        assert_eq!(reply["object_count"],512);
        assert_eq!(reply["layer_count"],2);
        let ids=reply["object_ids"].as_array().unwrap();
        assert_eq!(ids.len(),512);
        let read=tool(&store,"read_note",json!({"id":id}));
        assert_eq!(read["body"],"Preserve 🐈");
        assert_eq!(read["drawing"]["layers"][0]["name"],"背景 🌅");
        assert_eq!(read["drawing"]["layers"][1]["name"],"羽根 🦆");
        assert_eq!(read["drawing"]["objects"][0]["layer_id"],reply["created_layer_ids"][0]);
        assert_eq!(read["drawing"]["objects"][1]["layer_id"],reply["created_layer_ids"][1]);
        assert_eq!(read["drawing"]["objects"][511]["id"],ids[511]);
        assert!(execute_tool(&store,"drawing_create_scene",&json!({"id":id,
            "expected_object_count":0,"objects":[{"shape":{"type":"circle","radius":5}}]})).is_err());
        assert_eq!(tool(&store,"read_note",json!({"id":id})),read);
        tool(&store,"drawing_undo",json!({"id":id,"confirm":true}));
        assert_eq!(tool(&store,"read_note",json!({"id":id}))["drawing"],before["drawing"]);
        tool(&store,"drawing_redo",json!({"id":id,"confirm":true}));
        assert_eq!(tool(&store,"read_note",json!({"id":id}))["drawing"],read["drawing"]);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn drawing_create_scene_rejects_bad_index_locked_layer_and_extra_field() {
        let (store,folder)=store();
        let id=tool(&store,"create_note",json!({"title":"Do not disturb","body":"safe"}))["id"]
            .as_str().unwrap().to_owned();
        let layer=tool(&store,"drawing_create_layer",json!({"id":id,"name":"Locked"}))["layer_id"]
            .as_str().unwrap().to_owned();
        tool(&store,"drawing_edit_layer",json!({"id":id,"layer_id":layer,"locked":true}));
        let before=std::fs::read(store.data_path()).unwrap();
        let base=json!({"id":id,"layers":[{"name":"New"}],
            "objects":[{"shape":{"type":"rectangle","width":20,"height":20},"layer_index":0}]});
        for bad in [
            json!({"id":id,"objects":[]}),
            json!({"id":id,"layers":[{"name":""}],"objects":base["objects"]}),
            json!({"id":id,"objects":[{"shape":{"type":"circle","radius":5},"layer_index":0}]}),
            json!({"id":id,"objects":[{"shape":{"type":"circle","radius":5},"layer_id":layer}]}),
            json!({"id":id,"objects":[{"shape":{"type":"circle","radius":5},"unexpected":1}]}),
            json!({"id":id,"objects":[{"shape":{"type":"circle","radius":5},"layer_index":0,"layer_id":layer}]}),
        ] {
            assert!(execute_tool(&store,"drawing_create_scene",&bad).is_err(),"Accepted {bad}");
            assert_eq!(std::fs::read(store.data_path()).unwrap(),before);
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn drawing_create_batch_preserves_unicode_media_layers_order_and_one_step_undo() {
        let (store, folder) = store();
        let id = tool(&store, "create_note", json!({"title":"画 🎨 / 雲", "body":"猫👩‍🚀\nExisting text"}))["id"]
            .as_str().unwrap().to_string();
        tool(&store, "add_stroke", json!({"id":id,"width":7,"points":[[10,12],[24,40]]}));
        store.mutate(|workspace| {
            let note = workspace.notes.iter_mut().find(|note| note.id == id).unwrap();
            note.images.push(crate::store::MediaBlock { id:"picture1".into(),
                filename:"picture1.png".into(),caption:"猫 🐈".into(),width_percent:72,position:0 });
            Ok(())
        }).unwrap();
        let bottom = tool(&store,"drawing_create_layer",json!({"id":id,"name":"背面 🌌"}))["layer_id"]
            .as_str().unwrap().to_string();
        let foreground = tool(&store,"drawing_create_layer",json!({"id":id,"name":"前景 🌺"}))["layer_id"]
            .as_str().unwrap().to_string();
        let original = tool(&store, "read_note", json!({"id":id}));
        let gradient = json!({"type":"linear_gradient","start":{"x":0,"y":0},"end":{"x":0,"y":40},
            "stops":[{"offset":0.0,"color":[240,40,90,255]},
                     {"offset":1.0,"color":[40,90,240,255]}]});
        let result = tool(&store,"drawing_create_batch",json!({"id":id,"layer_id":bottom,
            "expected_object_count":0,"objects":[
            {"shape":{"type":"rectangle","width":180,"height":80},
             "style":{"stroke":null,"stroke_width":0,"fill":gradient,"opacity":1},
             "transform":{"translation":{"x":30,"y":40}}},
            {"shape":{"type":"circle","radius":23},"layer_id":foreground,
             "style":{"fill":{"type":"radial_gradient","center":{"x":0,"y":0},"radius":23,
                "stops":[{"offset":0.0,"color":[255,255,255,255]},
                         {"offset":1.0,"color":[24,30,110,255]}]}},
             "transform":{"translation":{"x":78,"y":85}}},
            {"shape":{"type":"cubic_bezier","control1":{"x":15,"y":30},
                "control2":{"x":40,"y":12},"to":{"x":50,"y":50}},
             "layer_id":foreground}
        ]}));
        assert_eq!(result["added_objects"], 3);
        assert_eq!(result["object_count"], 3);
        let ids = result["object_ids"].as_array().unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(Value::is_string));
        assert_ne!(ids[0], ids[1]);
        let saved = tool(&store,"read_note",json!({"id":id}));
        for field in ["body", "title", "images", "strokes", "rich_text", "inline_media"] {
            assert_eq!(saved[field], original[field], "Batch altered {field}");
        }
        assert_eq!(saved["drawing"]["layers"],original["drawing"]["layers"]);
        let objects = saved["drawing"]["objects"].as_array().unwrap();
        assert_eq!(objects.len(), 3);
        for (item, expected) in objects.iter().zip(ids.iter()) { assert_eq!(&item["id"], expected); }
        assert_eq!(objects[0]["layer_id"], bottom);
        assert_eq!(objects[1]["layer_id"], foreground);
        assert_eq!(objects[2]["layer_id"], foreground);
        assert_eq!(objects[0]["style"]["fill"]["type"], "linear_gradient");
        assert_eq!(objects[1]["style"]["fill"]["type"], "radial_gradient");

        // Exactly one undo/redo action for the entire vector batch; note text,
        // images and older pen strokes are never undone with the shape group.
        tool(&store,"drawing_undo",json!({"id":id,"confirm":true}));
        assert_eq!(tool(&store,"read_note",json!({"id":id}))["drawing"],original["drawing"]);
        tool(&store,"drawing_redo",json!({"id":id,"confirm":true}));
        assert_eq!(tool(&store,"read_note",json!({"id":id}))["drawing"],saved["drawing"]);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn drawing_create_batch_rolls_back_invalid_middle_locked_stale_and_overflow() {
        let (store, folder) = store();
        let id = tool(&store,"create_note",json!({"title":"Atomic","body":"Unchanged 🐈"}))["id"]
            .as_str().unwrap().to_string();
        let locked = tool(&store,"drawing_create_layer",json!({"id":id,"name":"Locked"}))["layer_id"]
            .as_str().unwrap().to_string();
        let writable = tool(&store,"drawing_create_layer",json!({"id":id,"name":"Open"}))["layer_id"]
            .as_str().unwrap().to_string();
        tool(&store,"drawing_edit_layer",json!({"id":id,"layer_id":locked,"locked":true}));
        let basic = json!({"shape":{"type":"rectangle","width":30,"height":20},"layer_id":writable});
        let invalid_geometry = json!({"shape":{"type":"circle","radius":-1},"layer_id":writable});
        let invalid_layer = json!({"shape":{"type":"circle","radius":5},"layer_id":locked});
        let test_cases = [
            json!({"id":id,"objects":[basic,invalid_geometry,basic]}),
            json!({"id":id,"objects":[basic,invalid_layer,basic]}),
            json!({"id":id,"objects":[basic,{"shape":{"type":"line","to":{"x":2,"y":5}},"layer_id":"missing-layer"},basic]}),
            json!({"id":id,"expected_object_count":1,"objects":[basic]}),
            json!({"id":id,"objects":[basic,{"shape":{"type":"circle","radius":4},"unexpected":true},basic]}),
            json!({"id":id,"objects":[]}),
            json!({"id":id,"objects":vec![basic.clone();65]}),
        ];
        for (index, request) in test_cases.iter().enumerate() {
            let original_bytes = std::fs::read(store.data_path()).unwrap();
            let original = tool(&store,"read_note",json!({"id":id}));
            let failure = execute_tool(&store,"drawing_create_batch",request).unwrap_err();
            assert!(!failure.is_empty(), "case {index}");
            assert_eq!(std::fs::read(store.data_path()).unwrap(),original_bytes,"case {index} wrote JSON");
            assert_eq!(tool(&store,"read_note",json!({"id":id})),original,"case {index} altered note");
        }
        let schema = tool_definitions().iter().find(|item| item["name"] == "drawing_create_batch").unwrap();
        assert_eq!(schema["inputSchema"]["properties"]["objects"]["maxItems"],64);
        assert_eq!(schema["inputSchema"]["properties"]["objects"]["minItems"],1);
        // Subsequent valid paint works; unsuccessful batches never poison undo.
        let result = tool(&store,"drawing_create_batch",json!({"id":id,"objects":[basic]}));
        assert_eq!(result["object_count"],1);
        tool(&store,"drawing_undo",json!({"id":id,"confirm":true}));
        assert_eq!(tool(&store,"drawing_list",json!({"id":id}))["object_count"],0);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn add_strokes_batch_is_atomic_and_preserves_vector_and_text() {
        let (store, folder) = store();
        let id = tool(&store,"create_note",json!({"title":"Pen 雲","body":"Keep me 👩‍🚀"}))["id"]
            .as_str().unwrap().to_string();
        tool(&store,"drawing_create_batch",json!({"id":id,"objects":[
            {"shape":{"type":"rectangle","width":60,"height":30}}
        ]}));
        let before = tool(&store,"read_note",json!({"id":id}));
        let first = json!({"width":5,"points":[[8,8],[20,20]],"color":"#0055AA"});
        let second = json!({"width":8,"points":[[45,60],[70,80]],"eraser":true});
        let saved = tool(&store,"add_strokes_batch",json!({"id":id,"strokes":[first,second]}));
        assert_eq!(saved["added_strokes"],2);
        assert_eq!(saved["added_points"],4);
        let after = tool(&store,"read_note",json!({"id":id}));
        assert_eq!(after["body"],before["body"]);
        assert_eq!(after["drawing"],before["drawing"]);
        assert_eq!(after["strokes"].as_array().unwrap().len(),2);
        let original_bytes = std::fs::read(store.data_path()).unwrap();
        for bad in [
            json!({"id":id,"strokes":[first,{"width":5,"points":[[2,2],[WIDTH,1]]},second]}),
            json!({"id":id,"strokes":[]}),
            json!({"id":id,"strokes":vec![first.clone();65]}),
            json!({"id":id,"strokes":[first,{"width":1,"points":[[1,1]],"unknown":true}]}),
        ] {
            assert!(execute_tool(&store,"add_strokes_batch",&bad).is_err());
            assert_eq!(std::fs::read(store.data_path()).unwrap(),original_bytes);
        }
        let schema = tool_definitions().iter().find(|tool| tool["name"] == "add_strokes_batch").unwrap();
        assert_eq!(schema["inputSchema"]["properties"]["strokes"]["maxItems"],64);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn cached_drawing_preview_tracks_external_edits_strokes_and_vector_undo() {
        let (store, folder) = store();
        let id = tool(&store,"create_note",json!({"title":"Preview 🐈"}))["id"]
            .as_str().unwrap().to_string();
        let first = tool(&store,"drawing_preview",json!({"id":id}));
        assert_eq!(tool(&store,"drawing_preview",json!({"id":id}))["data_base64"],first["data_base64"]);
        let new_shapes = tool(&store,"drawing_create_batch",json!({"id":id,"objects":[
            {"shape":{"type":"circle","radius":65},
             "style":{"stroke":null,"stroke_width":0,"fill":{"type":"solid","color":[250,50,80,255]}}}
        ]}));
        assert_eq!(new_shapes["added_objects"],1);
        let second = tool(&store,"drawing_preview",json!({"id":id}));
        assert_ne!(second["data_base64"],first["data_base64"]);
        assert_eq!(tool(&store,"drawing_preview",json!({"id":id}))["data_base64"],second["data_base64"]);
        // A different Store instance writes underneath this persistent agent.
        let external = Store::in_folder(folder.clone()).unwrap();
        external.mutate(|workspace| {
            let note = workspace.notes.iter_mut().find(|note| note.id == id).unwrap();
            note.strokes.push(Stroke {width:10,color:[20,60,220,255],eraser:false,
                points:vec![[300,300],[340,340]]});
            note.touch();
            Ok(())
        }).unwrap();
        let third = tool(&store,"drawing_preview",json!({"id":id}));
        assert_ne!(third["data_base64"],second["data_base64"]);
        assert_eq!(third["legacy_stroke_count"],1);
        // Session vector undo cannot discard the independently added ink.
        tool(&store,"drawing_undo",json!({"id":id,"confirm":true}));
        let undone = tool(&store,"drawing_preview",json!({"id":id}));
        assert_ne!(undone["data_base64"],third["data_base64"]);
        assert_ne!(undone["data_base64"],first["data_base64"]);
        assert_eq!(undone["object_count"],0);
        assert_eq!(undone["legacy_stroke_count"],1);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "Synthetic local timing; run explicitly with --ignored"]
    fn drawing_create_batch_benchmark_32_shapes_vs_individual_mcp_tools() {
        fn bench(use_batch: bool) -> (f64, usize) {
            let (store, folder) = store();
            let id = tool(&store,"create_note",json!({"title":"Synthetic artwork"}))["id"]
                .as_str().unwrap().to_string();
            let shapes = (0..32).map(|n| json!({"shape":{"type":"ellipse","radius_x":3,"radius_y":5},
                "transform":{"translation":{"x":n*8,"y":n*6}},
                "style":{"stroke":null,"fill":{"type":"solid","color":[80,140,220,255]}}}))
                .collect::<Vec<_>>();
            let start = Instant::now();
            if use_batch {
                let result = tool(&store,"drawing_create_batch",json!({"id":id,"objects":shapes}));
                assert_eq!(result["added_objects"],32);
            } else {
                for object in &shapes {
                    let mut single = object.as_object().unwrap().clone();
                    single.insert("id".into(), json!(id));
                    tool(&store,"drawing_create_shape",Value::Object(single));
                }
            }
            let elapsed = start.elapsed().as_secs_f64()*1000.0;
            assert_eq!(tool(&store,"drawing_list",json!({"id":id}))["object_count"],32);
            let bytes = std::fs::metadata(store.data_path()).unwrap().len() as usize;
            std::fs::remove_dir_all(folder).unwrap();
            (elapsed,bytes)
        }
        // Read/write benchmarks deliberately exclude network and PNG rendering;
        // they measure the same local MCP execute_tool path and JSON storage.
        let mut singles = Vec::new();
        let mut batches = Vec::new();
        for _ in 0..3 {
            singles.push(bench(false).0);
            batches.push(bench(true).0);
        }
        singles.sort_by(f64::total_cmp);
        batches.sort_by(f64::total_cmp);
        println!("32 editable vectors: individual={:.2} ms, batch={:.2} ms, speedup={:.1}x (median of 3 local runs)",
            singles[1],batches[1],singles[1]/batches[1]);
    }

    #[test]
    fn editable_drawing_mcp_preserves_legacy_data_and_renders_vectors() {
        let (store, folder) = store();
        let note = tool(&store, "create_note", json!({"title":"Vector","body":"Keep 👩‍🚀 text"}));
        let id = note["id"].as_str().unwrap();
        tool(&store, "add_stroke", json!({"id":id,"width":4,"points":[[12,12],[24,24]]}));
        let caps = tool(&store, "get_capabilities", json!({}));
        assert_eq!(caps["mcp"]["batch_drawing_commands"], true);
        assert_eq!(caps["mcp"]["max_batch_shapes"], MAX_BATCH_DRAW_OBJECTS);
        assert!(caps["model"]["shapes"].as_array().unwrap().contains(&json!("cubic_bezier")));
        assert!(execute_tool(&store,"drawing_create_shape",&json!({"id":id,
            "shape":{"type":"rectangle","width":-4,"height":10}})).is_err());
        assert!(execute_tool(&store,"drawing_create_shape",&json!({"id":id,
            "shape":{"type":"rectangle","width":10,"height":10,"unexpected":true}})).is_err());
        let created = tool(&store, "drawing_create_shape", json!({"id":id,
            "shape":{"type":"rounded_rectangle","width":100,"height":80,"radius":12},
            "style":{"stroke":[11,21,31,255],"stroke_width":2,
                "fill":{"type":"solid","color":[212,30,66,255]},"opacity":0.9},
            "transform":{"translation":{"x":100,"y":110}}}));
        let object_id = created["object"]["id"].as_str().unwrap();
        let saved = tool(&store, "read_note", json!({"id":id}));
        assert_eq!(saved["body"], "Keep 👩‍🚀 text");
        assert_eq!(saved["strokes"].as_array().unwrap().len(), 1);
        assert_eq!(saved["drawing"]["objects"][0]["id"], object_id);
        let bad_style = json!({"id":id,"object_id":object_id,"style":{"opacity":5}});
        assert!(execute_tool(&store,"drawing_edit_shape",&bad_style).is_err());
        assert_eq!(tool(&store, "read_note", json!({"id":id})), saved);
        tool(&store, "drawing_edit_shape", json!({"id":id,"object_id":object_id,
            "shape":{"type":"ellipse","radius_x":55,"radius_y":40},"visible":true}));
        let preview = tool(&store, "drawing_preview", json!({"id":id}));
        assert_eq!(preview["mime_type"], "image/png");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(preview["data_base64"].as_str().unwrap()).unwrap();
        assert!(raw.starts_with(b"\x89PNG\r\n\x1a\n"));
        let export = folder.join("vectors.png");
        tool(&store,"export_note",json!({"id":id,"format":"png","path":export.to_str().unwrap()}));
        assert_eq!(std::fs::read(export).unwrap(),raw);
        assert_eq!(tool(&store,"drawing_list",json!({"id":id}))["object_count"],1);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn drawing_layers_history_and_external_change_guard_are_transactional() {
        let (store, folder) = store();
        let id = tool(&store,"create_note",json!({"title":"History"}))["id"].as_str().unwrap().to_string();
        let layer = tool(&store,"drawing_create_layer",json!({"id":id,"name":"Front"}));
        let layer_id = layer["layer_id"].as_str().unwrap();
        let a = tool(&store,"drawing_create_shape",json!({"id":id,"layer_id":layer_id,
            "shape":{"type":"rectangle","width":50,"height":50}}));
        let object_id = a["object"]["id"].as_str().unwrap();
        let b = tool(&store,"drawing_create_shape",json!({"id":id,"layer_id":layer_id,
            "shape":{"type":"line","to":{"x":70,"y":80}}}));
        let second_id = b["object"]["id"].as_str().unwrap();
        let group = tool(&store,"drawing_group_objects",json!({"id":id,"object_ids":[object_id,second_id]}));
        assert!(group["group_id"].is_string());
        tool(&store,"drawing_transform_group",json!({"id":id,"group_id":group["group_id"],
            "operation":"move","dx":3,"dy":5}));
        assert!(execute_tool(&store,"drawing_transform_group",&json!({"id":id,"group_id":group["group_id"],
            "operation":"resize","sx":0,"sy":1})).is_err());
        tool(&store,"drawing_ungroup_objects",json!({"id":id,"group_id":group["group_id"]}));
        tool(&store,"drawing_edit_layer",json!({"id":id,"layer_id":layer_id,"locked":true}));
        let locked_state = tool(&store,"drawing_list",json!({"id":id}));
        assert!(execute_tool(&store,"drawing_transform_object",&json!({"id":id,
            "object_id":object_id,"operation":"move","dx":20,"dy":20})).is_err());
        assert_eq!(tool(&store,"drawing_list",json!({"id":id})),locked_state);
        tool(&store,"drawing_edit_layer",json!({"id":id,"layer_id":layer_id,"locked":false}));
        tool(&store,"drawing_transform_object",json!({"id":id,"object_id":object_id,
            "operation":"move","dx":30,"dy":40}));
        assert!(execute_tool(&store,"drawing_undo",&json!({"id":id})).is_err());
        tool(&store,"drawing_undo",json!({"id":id,"confirm":true}));
        let undone = tool(&store,"drawing_list",json!({"id":id}));
        assert_eq!(undone["drawing"]["objects"][0]["transform"]["translation"]["x"],3.0);
        tool(&store,"drawing_redo",json!({"id":id,"confirm":true}));
        let redone = tool(&store,"drawing_list",json!({"id":id}));
        assert_eq!(redone["drawing"]["objects"][0]["transform"]["translation"]["x"],33.0);
        assert!(execute_tool(&store,"drawing_delete_object",&json!({"id":id,"object_id":object_id})).is_err());
        assert!(execute_tool(&store,"drawing_delete_layer",&json!({"id":id,"layer_id":layer_id,"confirm":true})).is_err());
        let other = Store::in_folder(folder.clone()).unwrap();
        other.mutate(|workspace| {
            let note = workspace.notes.iter_mut().find(|note| note.id == id).unwrap();
            note.drawing.add_object(DrawShape::Ellipse { radius_x: 5.0, radius_y: 5.0 }, None)
                .map_err(io::Error::other)?;
            note.touch();
            Ok(())
        }).unwrap();
        assert!(execute_tool(&store,"drawing_undo",&json!({"id":id,"confirm":true})).is_err());
        assert_eq!(tool(&store,"drawing_list",json!({"id":id}))["object_count"],3);
        std::fs::remove_dir_all(folder).unwrap();
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
        let active = load(&store).unwrap().active_id.clone();
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
        let names: Vec<_> = responses[1]["result"]["tools"].as_array().unwrap().iter()
            .map(|item| item["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 61);
        assert!(names.contains(&"note_batch"));
        assert!(names.contains(&"get_capabilities"));
        assert!(names.contains(&"drawing_preview"));
        assert!(names.contains(&"paste_rich_fragment"));
        assert!(names.contains(&"image_place"));
        assert_eq!(responses[2]["result"]["structuredContent"]["title"], "From MCP");
        assert_eq!(responses[3]["result"]["isError"], true);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn compact_read_returns_text_without_large_drawing_payload() {
        let (store, folder) = store();
        let created = tool(&store, "create_note", json!({"title":"Brief","body":"Hello 猫"}));
        let id = created["id"].as_str().unwrap();
        let full = tool(&store, "read_note", json!({"id":id}));
        let compact = tool(&store, "read_note", json!({"id":id,"compact":true}));
        assert_eq!(compact["body"], full["body"]);
        assert_eq!(compact["title"], full["title"]);
        assert_eq!(compact["image_count"], 0);
        assert!(compact.get("strokes").is_none());
        assert!(compact.get("images").is_none());
        assert!(compact.get("rich_text").is_none());
        assert!(execute_tool(&store, "read_note", &json!({"id":id,"compact":"yes"})).is_err());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn opt_in_sqlite_search_matches_default_and_refreshes_after_external_write() {
        let (store, folder) = store();
        let created = tool(&store, "create_note", json!({"title":"Search title","body":"needle 🐈"}));
        let id = created["id"].as_str().unwrap();
        let query = json!({"query":"needle","limit":5,"include_body":true});
        let default = tool(&store, "search_notes", query.clone());
        let mut indexed_args = query.clone();
        indexed_args["sqlite_index"] = json!(true);
        let indexed = tool(&store, "search_notes", indexed_args.clone());
        assert_eq!(indexed, default);
        let other = Store::in_folder(folder.clone()).unwrap();
        other.mutate(|workspace| {
            let note = workspace.notes.iter_mut().find(|note| note.id == id).unwrap();
            note.body = "fresh 🦊".into();
            note.touch();
            Ok(())
        }).unwrap();
        assert_eq!(tool(&store, "search_notes", indexed_args.clone())["total"], 0);
        indexed_args["query"] = json!("fresh");
        let fresh = tool(&store, "search_notes", indexed_args);
        assert_eq!(fresh["total"], 1);
        assert_eq!(fresh["notes"][0]["body"], "fresh 🦊");
        assert!(execute_tool(&store, "search_notes",
            &json!({"query":"fresh","sqlite_index":"yes"})).is_err());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn search_can_include_bounded_text_and_active_note_can_be_compact() {
        let (store, folder) = store();
        let body = format!("start 猫\n{}MATCH at the end", "é".repeat(MAX_SEARCH_BODY_CHARS));
        let created = tool(&store, "create_note", json!({"title":"Project", "body":body}));
        let id = created["id"].as_str().unwrap();
        let default_search = tool(&store, "search_notes", json!({"query":"MATCH", "limit":1}));
        assert_eq!(default_search["total"], 1);
        assert!(default_search["notes"][0].get("body").is_none());

        let combined = tool(&store, "search_notes", json!({"query":"MATCH", "limit":1,"include_body":true}));
        assert_eq!(combined["notes"][0]["id"], id);
        assert_eq!(combined["notes"][0]["body_truncated"], true);
        let prefix = combined["notes"][0]["body"].as_str().unwrap();
        assert_eq!(prefix.chars().count(), MAX_SEARCH_BODY_CHARS);
        assert!(prefix.starts_with("start 猫"));
        assert!(execute_tool(&store, "search_notes", &json!({"query":"MATCH","limit":11,"include_body":true})).is_err());
        assert!(execute_tool(&store, "search_notes", &json!({"query":"MATCH","limit":1,"include_body":1})).is_err());

        let short = tool(&store, "create_note", json!({"title":"Tiny", "body":"hello 🦊"}));
        let short_id = short["id"].as_str().unwrap();
        let result = tool(&store, "search_notes", json!({"query":"hello", "limit":1,"include_body":true}));
        assert_eq!(result["notes"][0]["body"], "hello 🦊");
        assert_eq!(result["notes"][0]["body_truncated"], false);
        tool(&store, "set_active_note", json!({"id":short_id}));
        let active = tool(&store, "get_active_note", json!({"compact":true}));
        assert_eq!(active["note"]["body"], "hello 🦊");
        assert!(active["note"].get("strokes").is_none());
        assert!(tool(&store, "get_active_note", json!({}))["note"].get("strokes").is_some());
        assert!(execute_tool(&store, "get_active_note", &json!({"compact":1})).is_err());
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
        // Prime the persistent lowercased search index before an external save.
        assert_eq!(tool(&store, "search_notes", json!({"query":"BEFORE"}))["total"], 1);
        // Models another process, such as the GUI, saving after MCP starts.
        let concurrent_gui = Store::in_folder(folder.clone()).unwrap();
        concurrent_gui.mutate(|workspace| {
            let note = workspace.notes.iter_mut().find(|note| note.id == id).unwrap();
            note.body = "after GUI save 🐈".into();
            note.touch();
            Ok(())
        }).unwrap();
        assert_eq!(tool(&store, "read_note", json!({"id":id}))["body"], "after GUI save 🐈");
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
        assert_eq!(serde_json::to_value(&*before).unwrap(), serde_json::to_value(&*after).unwrap());
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

    fn tiny_png() -> Vec<u8> {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&[24, 60, 180, 255]).unwrap();
        }
        png
    }

    #[test]
    fn mcp_image_place_is_additive_scalar_safe_and_rejects_stale_editor_carets() {
        let (store, folder) = store();
        let note = tool(&store, "create_note", json!({"title":"Inline","body":"A🦊猫B"}));
        let id = note["id"].as_str().unwrap();
        let image = store.insert_image_bytes(id, &tiny_png()).unwrap();
        let original = tool(&store, "image_list", json!({"id":id}));
        assert_eq!(original["images"][0]["position"], 0);
        assert!(original["images"][0].get("char_offset").is_none());
        assert!(execute_tool(&store, "image_place", &json!({
            "id":id,"image_id":image.id,"char_offset":2
        })).is_err(), "explicit scalar unit required");
        assert!(execute_tool(&store, "image_place", &json!({
            "id":id,"image_id":image.id,"unit":"unicode_scalar",
            "char_offset":2,"expected_body":"stale"
        })).is_err());
        let placed = tool(&store, "image_place", json!({
            "id":id,"image_id":image.id,"unit":"unicode_scalar",
            "char_offset":2,"expected_body":"A🦊猫B"
        }));
        assert_eq!(placed["char_offset"], 2);
        assert_eq!(tool(&store, "read_note", json!({"id":id}))["body"], "A🦊猫B");
        assert_eq!(store.load().unwrap().notes.iter().find(|note| note.id == id)
            .unwrap().image_anchor(&image.id).unwrap(), Some(2));

        tool(&store, "replace_text_range", json!({
            "id":id,"unit":"unicode_scalar","start":0,"end":0,"text":"💚"
        }));
        assert_eq!(store.load().unwrap().notes.iter().find(|note| note.id == id)
            .unwrap().image_anchor(&image.id).unwrap(), Some(3));
        let cleared = tool(&store, "image_place", json!({
            "id":id,"image_id":image.id,"unit":"unicode_scalar","char_offset":null
        }));
        assert!(cleared["char_offset"].is_null());
        let saved = tool(&store, "read_note", json!({"id":id}));
        assert_eq!(saved["body"], "💚A🦊猫B");
        assert!(saved.get("inline_media").is_none(), "legacy shape restored on unplace");
        assert_eq!(saved["images"][0]["position"], 0);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn image_file_and_base64_roundtrip_order_caption_width_export_and_deletion_safety() {
        use base64::Engine as _;
        let (store, folder) = store();
        let outside = std::env::temp_dir().join(format!("inkpad-agent-image-source-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&outside).unwrap();
        let source = outside.join("source.PNG");
        std::fs::write(&source, tiny_png()).unwrap();
        let note = tool(&store, "create_note", json!({"title":"Pictures","body":"Text <escape> & symbols"}));
        let note_id = note["id"].as_str().unwrap();

        let file = tool(&store, "image_insert_file", json!({"id":note_id,"path":source,"user_approved":true}));
        let file_id = file["image"]["id"].as_str().unwrap();
        let copied = store.media_path(&store.list_media(note_id).unwrap()[0]).unwrap();
        assert!(copied.exists());
        let encoded = base64::engine::general_purpose::STANDARD.encode(tiny_png());
        let inline = tool(&store, "image_insert_base64", json!({"id":note_id,"data_base64":encoded,"user_approved":true}));
        let inline_id = inline["image"]["id"].as_str().unwrap();
        assert_ne!(file_id, inline_id);
        assert_eq!(tool(&store, "image_list", json!({"id":note_id}))["total"], 2);
        assert_eq!(tool(&store, "read_note", json!({"id":note_id}))["images"].as_array().unwrap().len(), 2);
        assert_eq!(tool(&store, "list_notes", json!({}))["notes"].as_array().unwrap()
            .iter().find(|v| v["id"] == note_id).unwrap()["image_count"], 2);

        let resized = tool(&store, "image_resize", json!({"id":note_id,"image_id":inline_id,"width_percent":42}));
        assert_eq!(resized["image"]["width_percent"], 42);
        let moved = tool(&store, "image_move", json!({"id":note_id,"image_id":inline_id,"position":0}));
        assert_eq!(moved["image"]["position"], 0);
        let captioned = tool(&store, "image_caption", json!({"id":note_id,"image_id":inline_id,"caption":"A <b> & quote"}));
        assert_eq!(captioned["image"]["caption"], "A <b> & quote");
        let ordered = tool(&store, "image_list", json!({"id":note_id}));
        assert_eq!(ordered["images"][0]["id"], inline_id);
        assert_eq!(ordered["images"][1]["id"], file_id);

        let html_file = outside.join("export.html");
        let html_export = tool(&store, "image_export_html", json!({"id":note_id,"path":html_file,"user_approved":true}));
        assert_eq!(html_export["self_contained"], true);
        let html = std::fs::read_to_string(&html_file).unwrap();
        assert!(html.contains("Text &lt;escape&gt; &amp; symbols"));
        assert!(html.contains("A &lt;b&gt; &amp; quote"));
        assert!(html.contains("data:image/png;base64,"));
        assert!(!html.contains("file://"));
        assert!(execute_tool(&store, "image_export_html", &json!({"id":note_id,"path":html_file,"user_approved":true})).is_err());

        assert!(execute_tool(&store, "image_delete", &json!({"id":note_id,"image_id":file_id})).is_err());
        assert!(execute_tool(&store, "image_delete", &json!({"id":note_id,"image_id":file_id,"confirm":false})).is_err());
        assert_eq!(tool(&store, "image_list", json!({"id":note_id}))["total"], 2);
        assert_eq!(tool(&store, "image_delete", json!({"id":note_id,"image_id":file_id,"confirm":true}))["deleted"], true);
        assert_eq!(tool(&store, "image_list", json!({"id":note_id}))["total"], 1);
        assert_eq!(std::fs::read(&source).unwrap(), tiny_png());
        std::fs::remove_dir_all(folder).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn images_reject_unapproved_paths_traversal_invalid_bytes_and_ranges() {
        let (store, folder) = store();
        let outside = std::env::temp_dir().join(format!("inkpad-agent-bad-media-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&outside).unwrap();
        let source = outside.join("safe.png");
        let bad_source = outside.join("secret.png");
        std::fs::write(&source, tiny_png()).unwrap();
        std::fs::write(&bad_source, b"not an image").unwrap();
        let note = tool(&store, "create_note", json!({"title":"Guarded"}));
        let note_id = note["id"].as_str().unwrap();
        for request in [
            json!({"id":note_id,"path":source}),
            json!({"id":note_id,"path":source,"user_approved":false}),
            json!({"id":note_id,"path":"..\\secrets.png","user_approved":true}),
            json!({"id":note_id,"path":bad_source,"user_approved":true}),
            json!({"id":note_id,"path":store.data_path(),"user_approved":true}),
        ] {
            assert!(execute_tool(&store, "image_insert_file", &request).is_err(), "Unexpectedly accepted {request}");
        }
        assert!(execute_tool(&store, "image_insert_base64", &json!({"id":note_id,"data_base64":"AAAA"})).is_err());
        for payload in ["AAAA", "A===", "AAA-", "AAAA====", "YWJj"] {
            assert!(decode_image_base64(payload).is_err(), "Unexpectedly accepted {payload}");
        }
        let first = tool(&store, "image_insert_file", json!({"id":note_id,"path":source,"user_approved":true}));
        let image_id = first["image"]["id"].as_str().unwrap();
        for (name, args) in [
            ("image_resize", json!({"id":note_id,"image_id":image_id,"width_percent":9})),
            ("image_resize", json!({"id":note_id,"image_id":image_id,"width_percent":101})),
            ("image_move", json!({"id":note_id,"image_id":image_id,"position":-1})),
            ("image_caption", json!({"id":note_id,"image_id":image_id,"caption":"z".repeat(501)})),
            ("image_delete", json!({"id":note_id,"image_id":"../other.png","confirm":true})),
            ("image_export_html", json!({"id":note_id,"path":outside.join("a.html")})),
            ("image_export_html", json!({"id":note_id,"path":outside.join("../escape.html"),"user_approved":true})),
        ] {
            assert!(execute_tool(&store, name, &args).is_err(), "Unexpectedly accepted {name} {args}");
        }
        assert_eq!(tool(&store, "image_list", json!({"id":note_id}))["total"], 1);
        std::fs::remove_dir_all(folder).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn read_image_and_export_image_protect_private_data_and_avoid_duplicate_mcp_base64() {
        let (store, folder) = store();
        let outside = std::env::temp_dir().join(format!("inkpad-agent-read-picture-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&outside).unwrap();
        let note = tool(&store, "create_note", json!({"title":"Read Picture"}));
        let note_id = note["id"].as_str().unwrap();
        let inserted = store.insert_image_bytes(note_id, &tiny_png()).unwrap();
        let image_id = inserted.id.as_str();
        let source = store.media_path(&inserted).unwrap();
        let stored = std::fs::read(&source).unwrap();
        assert!(execute_tool(&store, "read_image", &json!({"id":note_id,"image_id":image_id})).is_err());
        assert!(execute_tool(&store, "export_image", &json!({"id":note_id,"image_id":image_id,"path":outside.join("photo.png")})).is_err());
        let read = tool(&store, "read_image", json!({"id":note_id,"image_id":image_id,"user_approved":true}));
        assert_eq!(read["bytes"], stored.len());
        let decoded = base64::engine::general_purpose::STANDARD.decode(read["data_base64"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, stored);
        let response = tool_result(read);
        assert_eq!(response["content"][0]["type"], "image");
        assert_eq!(response["content"][0]["mimeType"], "image/png");
        assert!(response["structuredContent"].get("data_base64").is_none());
        let output = outside.join("photo.png");
        let copied = tool(&store, "export_image", json!({"id":note_id,"image_id":image_id,"path":output,"user_approved":true}));
        assert_eq!(copied["bytes"], stored.len());
        assert_eq!(std::fs::read(&output).unwrap(), stored);
        assert!(execute_tool(&store, "export_image", &json!({"id":note_id,"image_id":image_id,"path":output,"user_approved":true})).is_err());
        assert!(execute_tool(&store, "export_image", &json!({"id":note_id,"image_id":image_id,"path":outside.join("bad.jpg"),"user_approved":true})).is_err());
        // The 1 MiB tool cap must hold even if a private asset is unexpectedly
        // replaced with an oversized file on disk (without returning its data).
        std::fs::write(&source, vec![42u8; MAX_IMAGE_BASE64_BYTES + 1]).unwrap();
        let large = execute_tool(&store, "read_image", &json!({"id":note_id,"image_id":image_id,"user_approved":true}));
        assert!(large.unwrap_err().contains("export_image"));
        std::fs::remove_dir_all(folder).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn agent_appearance_and_sidebar_preferences_use_shared_settings_lock() {
        let (store, folder) = store();
        let initial = tool(&store, "get_app_settings", json!({}));
        assert_eq!(initial["theme"], "light");
        assert_eq!(initial["sidebar_collapsed"], false);
        let updated = tool(&store, "set_app_theme", json!({"theme":"dark"}));
        assert_eq!(updated["theme"], "dark");
        assert_eq!(updated["sidebar_collapsed"], false);
        let toggled = tool(&store, "set_sidebar_collapsed", json!({"collapsed":true}));
        assert_eq!(toggled["theme"], "dark");
        assert_eq!(toggled["sidebar_collapsed"], true);
        let settings = UiSettings::load(store.data_path()).unwrap();
        assert_eq!(settings.theme, AppearanceTheme::Dark);
        assert!(settings.sidebar_collapsed);
        assert_eq!(tool(&store, "set_app_theme", json!({"theme":"system"}))["theme"], "system");
        assert_eq!(tool(&store, "set_app_theme", json!({"theme":"light"}))["theme"], "light");
        assert!(execute_tool(&store, "set_app_theme", &json!({"theme":"navy"})).is_err());
        assert!(execute_tool(&store, "set_app_theme", &json!({"theme":true})).is_err());
        assert!(execute_tool(&store, "set_sidebar_collapsed", &json!({"collapsed":"false"})).is_err());
        assert_eq!(tool(&store, "get_app_settings", json!({}))["sidebar_collapsed"], true);
        assert!(!store.data_path().exists(), "UI settings changes must not create/modify note data");
        assert!(UiSettings::path_for(store.data_path()).exists());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn rich_unicode_range_format_replace_clear_and_legacy_edits_preserve_spans() {
        let (store, folder) = store();
        let note = tool(&store, "create_note", json!({"title":"Unicode styles","body":"A👩‍🚀e\u{301}猫Z"}));
        let id = note["id"].as_str().unwrap();
        let requested = json!({"id":id,"unit":"unicode_scalar","start":1,"end":7,
            "bold":true,"color":"#AA33ff","size":24,"font_family":"Serif"});
        let updated = tool(&store, "format_text_range", requested);
        assert_eq!(updated["runs"][0]["start"], 1);
        assert_eq!(updated["runs"][0]["end"], 7);
        assert_eq!(updated["runs"][0]["style"]["bold"], true);
        assert_eq!(updated["runs"][0]["style"]["size"], 24);
        let format = tool(&store, "get_note_formatting", json!({"id":id}));
        assert_eq!(format["unit"], "unicode_scalar");
        assert_eq!(format["length"], 8);

        let replaced = tool(&store, "replace_text_range", json!({"id":id,"unit":"unicode_scalar",
            "start":4,"end":6,"text":"é"}));
        assert_eq!(replaced["body"], "A👩‍🚀é猫Z");
        assert_eq!(replaced["runs"][0]["end"], 6);
        let cleared = tool(&store, "clear_text_formatting", json!({"id":id,"unit":"unicode_scalar",
            "start":1,"end":3}));
        assert_eq!(cleared["runs"][0]["start"], 3);
        assert_eq!(cleared["runs"][0]["end"], 6);
        let appended = tool(&store, "append_to_note", json!({"id":id,"text":"tail"}));
        assert_eq!(appended["body"], "A👩‍🚀é猫Z\ntail");
        let after_append = tool(&store, "get_note_formatting", json!({"id":id}));
        assert_eq!(after_append["runs"][0]["start"], 3);
        assert_eq!(after_append["runs"][0]["end"], 6);
        let body = tool(&store, "read_note", json!({"id":id}))["body"].as_str().unwrap().replace('Z', "Ω");
        let revised = tool(&store, "update_note", json!({"id":id,"body":body}));
        assert_eq!(revised["rich_text"][0]["start"], 3);
        assert_eq!(revised["rich_text"][0]["end"], 6);
        assert!(execute_tool(&store, "format_text_range", &json!({"id":id,"unit":"unicode_scalar",
            "start":1,"end":1,"bold":true})).is_err());
        for malformed in [
            json!({"id":id,"unit":"byte","start":1,"end":2,"bold":true}),
            json!({"id":id,"unit":"unicode_scalar","start":1,"end":99,"bold":true}),
            json!({"id":id,"unit":"unicode_scalar","start":1,"end":2,"color":"red"}),
            json!({"id":id,"unit":"unicode_scalar","start":1,"end":2,"size":50}),
            json!({"id":id,"unit":"unicode_scalar","start":1,"end":2,"font_family":"Comic Sans"}),
            json!({"id":id,"unit":"unicode_scalar","start":1,"end":2,"color":"#AABBCC","reset_color":true}),
            json!({"id":id,"unit":"unicode_scalar","start":1,"end":2,"italic":true}),
            json!({"id":id,"unit":"unicode_scalar","start":1,"end":2}),
        ] {
            assert!(execute_tool(&store, "format_text_range", &malformed).is_err(), "Accepted {malformed}");
        }
        let final_note = tool(&store, "read_note", json!({"id":id}));
        assert_eq!(final_note["rich_text"][0]["start"], 3);
        assert_eq!(final_note["rich_text"][0]["end"], 6);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn atomic_unicode_batch_uses_exact_grapheme_boundaries_and_preserves_other_pages() {
        let (store, folder) = store();
        let created = tool(&store, "create_note", json!({"title":"Batch test","body":"A👩‍🚀e\u{301}猫 Z"}));
        let id = created["id"].as_str().unwrap();
        let other = tool(&store, "create_note", json!({"title":"Unaffected","body":"no changes"}));
        let other_id = other["id"].as_str().unwrap();
        let batch = json!({"id":id,"unit":"unicode_scalar",
            "expected_body":"A👩‍🚀e\u{301}猫 Z",
            "operations":[
                {"start":1,"end":4,"bold":true,"color":"#AABBCC"},
                {"start":4,"end":6,"size":23,"font_family":"Serif"},
                {"start":1,"end":4,"color":"#EE11AA"}
            ]});
        let done = tool(&store,"format_text_batch",batch.clone());
        assert_eq!(done["applied"],3);
        assert_eq!(done["runs"][0]["start"],1);
        assert_eq!(done["runs"][0]["end"],4);
        assert_eq!(done["runs"][0]["style"]["color"],"#EE11AA");
        assert_eq!(done["runs"][1]["start"],4);
        assert_eq!(done["runs"][1]["end"],6);
        assert_eq!(done["runs"][1]["style"]["size"],23);
        let baseline = tool(&store,"read_note",json!({"id":id}));
        let mut invalid = batch.clone();
        invalid["operations"][1]["end"] = json!(999);
        assert!(execute_tool(&store,"format_text_batch",&invalid).is_err());
        assert_eq!(tool(&store,"read_note",json!({"id":id})),baseline,"partial batch never committed");
        let mut stale = batch.clone();
        stale["expected_body"] = json!("outdated");
        assert!(execute_tool(&store,"format_text_batch",&stale).unwrap_err().contains("expected_body"));
        for (lo,hi) in [(2,4),(1,3),(4,5),(5,6)] {
            let request = json!({"id":id,"unit":"unicode_scalar",
                "operations":[{"start":lo,"end":hi,"bold":true}]});
            assert!(execute_tool(&store,"format_text_batch",&request).is_err(),"Split grapheme {lo}..{hi}");
        }
        let scalar = tool(&store,"format_text_batch",json!({"id":id,
            "unit":"unicode_scalar","boundary_policy":"scalar",
            "operations":[{"start":2,"end":3,"bold":false}]}));
        assert_eq!(scalar["applied"],1);
        assert_eq!(tool(&store,"read_note",json!({"id":other_id}))["body"],"no changes");
        let reopened = store.load().unwrap();
        reopened.notes.iter().find(|n| n.id == id).unwrap().validate_rich_text().unwrap();
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn portable_rich_fragment_copies_and_pastes_without_os_clipboard_and_survives_restart() {
        let (store, folder) = store();
        let origin = tool(&store,"create_note",json!({"title":"Source","body":"A👩‍🚀e\u{301}猫Z"}));
        let source_id = origin["id"].as_str().unwrap();
        let destination = tool(&store,"create_note",json!({"title":"Target","body":"X日Y"}));
        let target_id = destination["id"].as_str().unwrap();
        tool(&store,"set_font_size",json!({"id":source_id,"size":26}));
        tool(&store,"set_font_choice",json!({"id":source_id,"choice":"Serif"}));
        tool(&store,"set_font_choice",json!({"id":target_id,"choice":"Mono"}));
        tool(&store,"format_text_batch",json!({"id":source_id,"unit":"unicode_scalar",
            "operations":[{"start":1,"end":4,"bold":true,"color":"#990033"},
                {"start":4,"end":6,"size":32}]}));
        let copied = tool(&store,"copy_rich_fragment",json!({"id":source_id,
            "start":1,"end":6,"unit":"unicode_scalar"}));
        assert_eq!(copied["text"],"👩‍🚀e\u{301}");
        assert_eq!(copied["runs"].as_array().unwrap().len(),2);
        assert_eq!(copied["runs"][0]["start"],0);
        assert_eq!(copied["runs"][0]["end"],3);
        assert_eq!(copied["defaults"]["size"],26);
        assert_eq!(copied["defaults"]["font_family"],"Serif");
        assert!(execute_tool(&store,"copy_rich_fragment",&json!({
            "id":source_id,"start":1,"end":2,"unit":"unicode_scalar"})).is_err());
        assert!(execute_tool(&store,"copy_rich_fragment",&json!({
            "id":source_id,"start":4,"end":5,"unit":"unicode_scalar"})).is_err());
        let original = tool(&store,"read_note",json!({"id":target_id}));
        assert!(execute_tool(&store,"paste_rich_fragment",&json!({"id":target_id,
            "start":1,"end":2,"unit":"unicode_scalar",
            "expected_text":"another character","fragment":copied})).is_err());
        assert_eq!(tool(&store,"read_note",json!({"id":target_id})),original);
        let pasted = tool(&store,"paste_rich_fragment",json!({"id":target_id,
            "start":1,"end":2,"unit":"unicode_scalar",
            "expected_text":"日","fragment":copied}));
        assert_eq!(pasted["inserted"],5);
        let reopened = store.load().unwrap();
        let target = reopened.notes.iter().find(|n| n.id == target_id).unwrap();
        assert_eq!(target.body,"X👩‍🚀e\u{301}Y");
        assert_eq!(target.style_at(1).bold,true);
        assert_eq!(target.style_at(1).color.as_deref(),Some("#990033"));
        assert_eq!(target.style_at(1).font_family.as_deref(),Some("Serif"));
        assert_eq!(target.style_at(1).size,Some(26));
        assert_eq!(target.style_at(4).size,Some(32));
        assert_eq!(target.style_at(0).font_family,None,"Outside pasted range retains defaults");
        target.validate_rich_text().unwrap();
        assert_eq!(reopened.notes.iter().find(|n| n.id == source_id).unwrap().body,"A👩‍🚀e\u{301}猫Z");
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn fragment_rejects_forged_invalid_runs_and_oversized_payload_atomically() {
        let (store, folder) = store();
        let created = tool(&store,"create_note",json!({"title":"Protected","body":"abcd"}));
        let id = created["id"].as_str().unwrap();
        let before = tool(&store,"read_note",json!({"id":id}));
        let valid = json!({"format_version":1,"unit":"unicode_scalar","text":"hey",
            "runs":[{"start":0,"end":3,"style":{"bold":true}}]});
        let malformed = [
            json!({"format_version":2,"unit":"unicode_scalar","text":"hey","runs":[]}),
            json!({"format_version":1,"unit":"utf16","text":"hey","runs":[]}),
            json!({"format_version":1,"unit":"unicode_scalar","text":"hey",
                "runs":[{"start":0,"end":9,"style":{"bold":true}}]}),
            json!({"format_version":1,"unit":"unicode_scalar","text":"hey",
                "runs":[{"start":0,"end":2,"style":{"bold":true}},{"start":1,"end":3,"style":{"color":"#AA33AA"}}]}),
            json!({"format_version":1,"unit":"unicode_scalar","text":"hey",
                "runs":[{"start":0,"end":3,"style":{"size":99}}]}),
            json!({"format_version":1,"unit":"unicode_scalar","text":"hey","runs":[],
                "defaults":{"size":99,"font_family":"Serif"}}),
            json!({"format_version":1,"unit":"unicode_scalar","text":"a".repeat(MAX_FRAGMENT_TEXT_BYTES+1),"runs":[]}),
        ];
        for fragment in malformed {
            assert!(execute_tool(&store,"paste_rich_fragment",&json!({"id":id,"start":2,"end":2,
                "unit":"unicode_scalar","fragment":fragment})).is_err());
        }
        assert!(execute_tool(&store,"paste_rich_fragment",&json!({"id":id,
            "start":99,"end":99,"unit":"unicode_scalar","fragment":valid})).is_err());
        assert_eq!(tool(&store,"read_note",json!({"id":id})),before);
        let native_gui = json!({"version":1,"text":"✔","runs":[
            {"start":0,"end":1,"style":{"bold":true}}
        ],"defaults":{"size":21,"font_family":"Serif"}});
        let pasted = tool(&store,"paste_rich_fragment",json!({"id":id,
            "start":1,"end":2,"unit":"unicode_scalar","expected_text":"b",
            "fragment":native_gui}));
        assert_eq!(pasted["length"],4);
        let styled = tool(&store,"read_note",json!({"id":id}));
        assert_eq!(styled["body"],"a✔cd");
        assert_eq!(styled["rich_text"][0]["style"]["font_family"],"Serif");
        assert_eq!(styled["rich_text"][0]["style"]["size"],21);
        assert_eq!(styled["rich_text"][0]["style"]["bold"],true);
        std::fs::remove_dir_all(folder).unwrap();
    }
}
