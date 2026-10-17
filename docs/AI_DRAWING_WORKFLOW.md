# Drawing polished artwork with Inkpad MCP

Inkpad's canvas is **1100 × 700 pixels**. The agent creates real editable vector
objects, not a screenshot or a generated bitmap. Use the local `inkpad-agent.exe`
MCP server; no Inkpad window, browser or clipboard is required.

## Fast workflow

1. Find or create a note, then use `drawing_list` to inspect existing layers,
   objects and any artwork that must be preserved.
2. Plan the composition as a small set of regions: background, distant objects,
   subject, foreground and highlights. Give each region a restrained palette.
3. **Prefer `drawing_create_scene`** to create up to 16 named layers and 512
   editable objects together in a single validated commit. Refer to each new
   layer by zero-based `layer_index` in the corresponding object. Keep shadows
   and highlights close to the subjects they belong to. For smaller additions,
   use `drawing_create_batch` (1–64 shapes); older servers may require this
   fallback. Supply `expected_object_count` when another editor might change
   the note before the save, so a stale composition is rejected.
4. Build a coherent scene with large silhouettes before adding fine feathers,
   reeds, water glints and other repetitive objects. Minimize unnecessary
   `drawing_create_shape` calls; do not optimize for object count alone.
5. Call `drawing_preview` **after each meaningful scene-level batch**, not after
   every single shape. Inspect the returned MCP `image/png` and correct the
   artwork based on what is actually visible. Do not claim to have inspected
   artwork without opening the preview.

The schema in MCP `tools/list` is authoritative. `drawing_create_scene` adds
new layers in array order and shapes in draw order, preserving existing data.
It uses the same `shape`, `style` and `transform` fields as
`drawing_create_shape`, plus `layer_index` for a newly created layer or
`layer_id` for an existing layer (never both). Returned object and layer IDs
are stable and can be edited later. 512 shapes is a per-call ceiling, while
the note-wide limit remains 2000 objects and 64 layers.

## Visual quality guidelines

- **Clear silhouette:** first define your subject with 2–5 readable large
  shapes. Details cannot rescue an unclear overall composition.
- **Layered depth:** distant items use lighter/lower-contrast tones and less
  detail; nearer elements use stronger values and sharper edges.
- **Light direction:** use a consistent shadow side. A subtle translucent,
  slightly offset ellipse or rounded rectangle often works as a soft shadow.
- **Use gradients deliberately:** two or three colour stops in a linear
  gradient suit skies and large surfaces. A radial gradient is useful for
  glows and rounded highlights. All colours are `[R,G,B,A]` integers (0–255).
- **Stroke restraint:** outer silhouettes can use 2–4 px dark strokes; internal
  highlights often need no stroke. Use consistent stroke widths throughout.
- **Shape economy:** circles/ellipses for smooth forms, rounded rectangles for
  cards and structures, polygons for layered silhouettes, cubic Bézier curves
  for custom flowing paths. Use transformations instead of rebuilding shapes.
- **Canvas framing:** reserve 20–40 pixels of breathing room around important
  subjects. Avoid clipping the focal point at the edge unless intentional.
- **Keep it editable:** named layers, vector groups and stable IDs make later
  revisions easier than one giant opaque raster image.

## Example: one well-styled scene batch

Supply a real note `id`. This example places a background, a glow and a
foreground surface on the same editable layer:

```json
{
  "id": "NOTE_ID",
  "layers": [{"name": "Sunset sky and backdrop"}],
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
            {"offset": 0, "color": [24, 39, 79, 255]},
            {"offset": 1, "color": [118, 92, 138, 255]}
          ]
        }
      }
    },
    {
      "layer_index": 0,
      "shape": {"type": "circle", "radius": 94},
      "transform": {"translation": {"x": 815, "y": 186}},
      "style": {
        "stroke": null,
        "stroke_width": 0,
        "fill": {
          "type": "radial_gradient",
          "center": {"x": 0, "y": 0},
          "radius": 94,
          "stops": [
            {"offset": 0, "color": [255, 230, 177, 240]},
            {"offset": 1, "color": [255, 190, 145, 140]}
          ]
        }
      }
    },
    {
      "layer_index": 0,
      "shape": {"type": "rounded_rectangle", "width": 920, "height": 190, "radius": 32},
      "transform": {"translation": {"x": 90, "y": 458}},
      "style": {
        "stroke": [31, 35, 62, 220],
        "stroke_width": 3,
        "fill": {"type": "solid", "color": [24, 38, 57, 255]},
        "opacity": 0.96
      }
    }
  ]
}
```

Gradients use **object-local coordinates**, then transform with the object.
The scene preview is a rasterized PNG, but the saved shapes remain editable.
With 64 objects per call and the application's 2,000-object safety limit,
large compositions should be split into carefully planned, bounded batches.
