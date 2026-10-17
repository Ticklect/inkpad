# Art workflow for Inkpad headless MCP

Use this reference **only for drawing/painting/illustration requests**. Inkpad artwork is saved as editable vector shapes and optional legacy ink strokes on a fixed **1100×700** canvas. The notebook's body text and attached pictures are separate. The MCP server is local; never open the desktop or use screen automation for ordinary note operations.

## Compose before making tool calls

1. Read the user's desired subject/style; find or create the intended note, then `drawing_list`. Do not replace existing objects to create a new illustration unless the user authorizes doing so. Build on free space or create a new note.
2. Set a focal area with 20–40 px breathing room. Pick a restrained palette: usually 3–5 dominant colours and 1–2 accents. Plan a common light direction, clear foreground and legible silhouette. For backgrounds prefer broad low-contrast filled objects and gradients rather than many strokes.
3. Prefer verified `drawing_create_scene` for a complete composition. It adds up to **16 named layers and 512 shapes** in **one transaction**, keeping separate stable editable object IDs. Each object chooses its new layer by zero-based `layer_index`. Name layers by visual purpose (backdrop, distance, subject, foreground, highlights); shadows belong below their objects and details above them.
4. For subsequent adjustments or a server without `drawing_create_scene`, use verified `drawing_create_batch` for at most 64 shapes per call. Use `drawing_create_layer` before batches only when necessary. Supply `expected_object_count` from `drawing_list` to protect against stale plans. If batches are missing, use `drawing_create_shape` singles and report the limitation; never invent endpoints or rebuild the app mid-drawing.
5. For expressive line/hand-painted details, use **verified** `add_strokes_batch` with at most 64 strokes per call, each up to 512 integer canvas points and width 1–40, or single `add_stroke` if that is all the live executable supports. Prefer vector outlines/Bezier curves for smooth, editable focal edges.

## Valid example: three editable objects in one scene

This is a **tool argument example**, not a request to create a real note. Replace `NOTE_ID` with a verified ID; add `expected_object_count` only after obtaining the current count.

```json
{
  "id": "NOTE_ID",
  "layers": [{"name":"Backdrop"}, {"name":"Foreground"}],
  "objects": [
    {
      "layer_index": 0,
      "shape": {"type": "rectangle", "width": 1100, "height": 700},
      "style": {
        "stroke": null,
        "stroke_width": 0,
        "fill": {
          "type": "linear_gradient",
          "start": {"x": 0, "y": 0},
          "end": {"x": 0, "y": 700},
          "stops": [
            {"offset": 0, "color": [22, 37, 76, 255]},
            {"offset": 1, "color": [99, 97, 139, 255]}
          ]
        }
      }
    },
    {
      "layer_index": 0,
      "shape": {"type": "circle", "radius": 85},
      "transform": {"translation": {"x": 850, "y": 175}},
      "style": {
        "stroke": null,
        "stroke_width": 0,
        "fill": {"type": "solid", "color": [255, 215, 146, 235]}
      }
    },
    {
      "layer_index": 1,
      "shape": {"type": "rounded_rectangle", "width": 920, "height": 190, "radius": 32},
      "transform": {"translation": {"x": 90, "y": 460}},
      "style": {
        "stroke": [25, 36, 61, 255],
        "stroke_width": 3,
        "fill": {"type": "solid", "color": [29, 45, 67, 255]}
      }
    }
  ]
}
```

For paint-like accents, a sample **`add_strokes_batch` argument** (only if advertised by current `tools/list`):

```json
{
  "id": "NOTE_ID",
  "strokes": [
    {"points": [[160, 300], [190, 282], [220, 278], [253, 290]], "width": 4, "color": "#FFE4A8", "eraser": false},
    {"points": [[170, 318], [204, 308], [238, 315]], "width": 2, "color": "#B3C7DB", "eraser": false}
  ]
}
```

Call the first example with `drawing_create_scene` if advertised. Calls **append**, not replace, existing art. Shapes remain separately editable and return stable IDs. Shapes use local geometry: a circle without `transform.translation` is centred at (0,0), and a rectangle starts there. RGBA `[r,g,b,a]` is 0–255; gradients have 2–16 strictly increasing offset stops in `[0,1]`.

## Staged visual QA

- After the backdrop and large silhouette, call `drawing_preview`, inspect the image block and check composition, clipping, horizon/ground planes and negative space. If no image-capable viewer is available, do not claim a visual assessment.
- After form and shadows, inspect shape overlap, stroke weights, lighting consistency and gradient placement. Check small-scale readability: silhouettes should remain recognizable without thin detail.
- After highlights and details, preview again. Fix only verified problems using `drawing_edit_shape`, `drawing_transform_object`, `drawing_reorder_object` or new additive shapes. Avoid a wholesale redraw that discards text, pictures or pre-existing art.
- Keep image quality practical: crisp silhouette, coherent palette, dimensional shadows, clear focal point and uncluttered balance. Use low-contrast gradients for atmosphere; reserve strong contrast and sharp highlights for the subject.
- `drawing_preview` returns a **bounded PNG (max 1 MiB)**, not a desktop screenshot or saved file. It depicts drawings, not note body text. `export_note` with `format:"png"` writes a fresh user-requested destination if actual file delivery is needed.
- Limits: 1100×700 canvas; 2000 vector objects, 64 layers, 256 polygon points; 2000 ink strokes; 512 shapes and 16 new layers per scene; 64 creations per smaller batch. Verify actual tool presence because older connectors and binaries are missing scene/batch APIs. Never use deletion/undo/clear as a styling shortcut without explicit user consent.

Do not scan source files, rebuild Inkpad, generate desktop UI interactions or touch private workspace data during ordinary art creation.
