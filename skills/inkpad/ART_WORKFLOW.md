# Art workflow for Inkpad headless MCP

Use this reference **only for drawing/painting/illustration requests**. Inkpad artwork is saved as editable vector shapes and optional legacy ink strokes on a fixed **1100×700** canvas. The notebook's body text and attached pictures are separate. The MCP server is local; never open the desktop or use screen automation for ordinary note operations.

## Compose before making tool calls

1. Read the user's desired subject/style; find or create the intended note, then `drawing_list`. Do not replace existing objects to create a new illustration unless the user authorizes doing so. Build on free space or create a new note.
2. **First plan the complete image at thumbnail scale**: focal subject bounding rectangle and direction, horizon, light source, relative sky/water areas, distant shore depth, reflection under the light source and water-contact wake, and useful foreground framing. A technically detailed subject against two flat gradients is not a finished realistic scene. Then plan anatomy as connected landmarks and curve tangents (skull crown → bill root → throat → breast apex → keel/waterline → rump → back → nape → crown). Make one anatomically credible closed `path` via the current schema; a connected path that imitates a round head and oval hull still fails. Add bill, eye, tail and internal wing as separate **attached** forms; clip colour fields where supported. Inspect an untextured silhouette before fine feathers but preserve the broader composition. External duck reference photos are **not required**.
3. Prefer verified `drawing_create_scene` for a complete composition. It adds up to **16 named layers and 512 shapes** in **one transaction**, keeping separate stable editable object IDs. Each object chooses its new layer by zero-based `layer_index`. Name layers by visual purpose (backdrop, distance, subject, foreground, highlights); shadows belong below their objects and details above them.
4. For subsequent adjustments, read IDs from `drawing_list` and **edit those existing shapes**, rather than appending overlapping replacements. New source builds of `drawing_create_scene` accept `edits:[{"object_id":"ID","shape":{...}},{"object_id":"OTHER_ID","position":{"x":200,"y":300}}]`, including edits-only requests with no `objects`, up to 64 per one atomic commit and undo; `expected_object_count` guards against stale plans. This is only available if the exact live scene schema advertises `edits`; otherwise use `drawing_edit_shape` per object. For additional new shapes use verified `drawing_create_batch` for at most 64 shapes. Never invent endpoints or rebuild the app mid-drawing.
5. Make each shape a meaningful surface: main contour, joined neck/breast colour region, wing plane, coverts, primary feathers, reflected light or cast shadow. **Connect shared anatomical edges, not detached ellipse overlaps.** For side-profile mallards, beak attaches at front crown near eye level, crown narrows into nape, white collar wraps the real throat, chestnut breast slopes into the keel (never a detached long oval), and folded wing occupies the flank with a rooted leading edge. Feather tiers start at actual wing root and overlap **fore-to-aft / root-to-tip** with changing scale and colour, not crisscross white scratches. Optionally import `vector_geometry.py` from this skill folder: `cubic_path(landmarks)` gives a smooth editable closed path and `wing_covert_row(top_rail,lower_rail,count=8)` gives individual editable curved feathers. You must still choose credible landmarks and correctly oriented upper/lower rails. When clipping, confirm the actual saved parent wing path spans the complete feather region; an earlier test accidentally used a much smaller wing subshape and cropped most feathers. Use coherent light, water contact and broken reflections. Add pen marks only when they describe form.

**Feature/position gate:** `path` commands (`move_to`, `line_to`, `cubic_to`, `quad_to`, `close`), object `clip_shape`, `style.shadow`, `elliptical_gradient` and **object `position:{x,y}`** exist in the source/optimized candidate, **not necessarily in the installed/connected agent**. Check `get_capabilities.model` flags and the **exact tool input schema** before sending any. For `position`, require `model.absolute_position:true` plus `position` in the invoked tool; otherwise use `transform.translation`. For circles/ellipses always place their **centre** explicitly, and for rectangles/rounded rectangles their **top-left**; without either, their local origin remains (0,0). On create/scene/batch, `position` and `transform` are mutually exclusive; on `drawing_edit_shape`, `position` also preserves existing rotation and scale. Use full `transform` for new scaled/rotated objects. Never attempt to position an object via gradient coordinates: gradient `start`/`end`/`center` stay **object-local**, unchanged by movement.

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
      "position": {"x": 850, "y": 175},
      "style": {
        "stroke": null,
        "stroke_width": 0,
        "fill": {"type": "solid", "color": [255, 215, 146, 235]}
      }
    },
    {
      "layer_index": 1,
      "shape": {"type": "rounded_rectangle", "width": 920, "height": 190, "radius": 32},
      "position": {"x": 90, "y": 460},
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

**Use the example above only if `drawing_create_scene` is live and `get_capabilities.model.absolute_position:true` and its per-object schema advertises `position`.** Otherwise replace each `"position":{"x":X,"y":Y}` with `"transform":{"translation":{"x":X,"y":Y}}` (do not send both). Rectangle origin/top-left is `position`; circle and ellipse centre is `position`. To put a lake rectangle at y=382, set its geometry placement to `{x:0,y:382}`; to centre a sun at (855,238), set *its own* position to `{x:855,y:238}`. **Gradient coordinates describe only colour distribution in local space; never move them as a proxy for object placement**, nor rebase them when the object moves. Batches **append** stable editable objects, they do not overwrite old silhouettes. RGBA channels are 0–255, gradient stops 2–16 with increasing offsets in `[0,1]`.

## Staged visual QA

- Keep quality checks **bounded and event-driven**, never per object or on routine note operations. For most illustrations, render **one composition checkpoint** after the major forms and **one final preview** after details. A genuinely complex scene may justify another preview, but do not render after every layer, shape or stroke.
- At the earliest realistic-subject checkpoint, **inspect the real PNG** (including at thumbnail size) for focal balance as well as believable landmark spacing, connected head–nape–neck–breast, wing root and natural body proportions. A path or high object count alone does not prove anatomy. At final preview, check **both** regional detail and whole-scene quality: varied atmospheric distance, foreground framing, feather direction, light continuity, water contact, reflected forms and broken sun glints where plausible. Do not let the subject consume all effort while the environment stays empty. No arbitrary requirement to find external duck photos.
- If inspection reveals a defect, perform **one bounded targeted refinement** using `drawing_edit_shape` on the returned *original* object ID, reposition/reorder it or add dependent shapes. Do **not** stack a new silhouette on top of the flawed old silhouette and leave both saved; inspect `drawing_list` to confirm the old shape was corrected and preview the result. Removal of existing objects remains an explicitly authorized delete action. Never change just the previously sent JSON/script and assume the saved note updated.
- Quality follows contour continuity, credible proportions, clear material differences, dimensional shadows and coherent illumination. Restrict accents to where they reveal anatomy or atmosphere; decorative texture and object totals are never quality metrics.
- `drawing_preview` returns a **bounded PNG (max 1 MiB)**, not a desktop screenshot or saved file. It depicts drawings, not note body text. **After the final edit, issue a fresh preview and visually inspect it; only then export** using `export_note` `format:"png"` to a fresh user-approved destination. Never send an earlier partial PNG as the finished artwork, or claim an export matches the final edit if the new preview has not been checked.
- Benchmark/image-history archives are **explicit, offline and after-the-fact**: record model, elapsed time, scene call count, object count, image hashes and visual observations. Never auto-export private notes, upload them, or add archive work to read/search/edit calls.
- Limits: 1100×700 canvas; 2000 vector objects, 64 layers, 256 polygon points; 2000 ink strokes; 512 shapes and 16 new layers per scene; 64 creations per smaller batch. Verify actual tool presence because older connectors and binaries are missing scene/batch APIs. Never use deletion/undo/clear as a styling shortcut without explicit user consent.

Do not scan source files, rebuild Inkpad, generate desktop UI interactions or touch private workspace data during ordinary art creation.
