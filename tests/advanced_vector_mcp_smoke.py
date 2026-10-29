"""End-to-end advanced Inkpad MCP artwork smoke, without accessing real notes.

Usage:
    py -3 tests/advanced_vector_mcp_smoke.py PATH_TO_inkpad-agent.exe
    py -3 tests/advanced_vector_mcp_smoke.py PATH_TO_inkpad-agent.exe --output C:\\art\\mallard-new.png

Uses the persistent stdio MCP helper from benchmark_agent_mcp.py. Its ONLY
Inkpad workspace is a newly created TemporaryDirectory passed through
INKPAD_DATA_DIR. Does not invoke the desktop app, scan files or reuse art.
PNG output is entirely opt-in, must have an existing parent directory, and
is created with exclusive mode (never overwrites an existing file).
"""

import argparse
import base64
import copy
import hashlib
import json
import os
from pathlib import Path
import struct
import sys
import tempfile
import time
import zlib

from benchmark_agent_mcp import MCP


PNG_MAGIC = b"\x89PNG\r\n\x1a\n"
CANVAS = (1100, 700)


def pt(x, y):
    return {"x": x, "y": y}


def M(x, y):
    return {"type": "move_to", "to": pt(x, y)}


def L(x, y):
    return {"type": "line_to", "to": pt(x, y)}


def C(x1, y1, x2, y2, x, y):
    return {"type": "cubic_to", "control1": pt(x1, y1),
            "control2": pt(x2, y2), "to": pt(x, y)}


def Q(cx, cy, x, y):
    return {"type": "quad_to", "control": pt(cx, cy), "to": pt(x, y)}


def Z():
    return {"type": "close"}


def path(*commands):
    return {"type": "path", "commands": list(commands)}


def solid(r, g, b, a=255):
    return {"type": "solid", "color": [r, g, b, a]}


def stops(*colors):
    return [{"offset": i / (len(colors) - 1), "color": list(color)}
            for i, color in enumerate(colors)]


def linear(x0, y0, x1, y1, *colors):
    return {"type": "linear_gradient", "start": pt(x0, y0), "end": pt(x1, y1),
            "stops": stops(*colors)}


def elliptical(cx, cy, rx, ry, *colors):
    return {"type": "elliptical_gradient", "center": pt(cx, cy),
            "radius_x": rx, "radius_y": ry, "stops": stops(*colors)}


def paint(shape, layer, fill=None, stroke=None, width=0, *, clip=None, shadow=None,
          opacity=1.0):
    style = {"stroke": stroke, "stroke_width": width, "opacity": opacity}
    if fill is not None:
        style["fill"] = fill
    if shadow is not None:
        style["shadow"] = shadow
    obj = {"layer_index": layer, "shape": shape, "style": style}
    if clip is not None:
        obj["clip_shape"] = clip
    return obj


def duck_body():
    """One connected torso contour, from pointed tail to breast and waterline."""
    return path(M(315, 405), C(383, 371, 418, 361, 483, 366),
                C(560, 367, 626, 388, 683, 424),
                C(729, 443, 737, 478, 719, 500),
                C(686, 537, 624, 549, 535, 544),
                C(429, 543, 354, 512, 323, 477),
                C(305, 456, 300, 426, 315, 405), Z())


def primary_wing():
    return path(M(409, 397), C(479, 372, 560, 382, 630, 419),
                C(665, 440, 671, 466, 651, 482),
                C(603, 509, 546, 508, 483, 481),
                Q(443, 460, 409, 397), Z())


def scene(enable_clip=True, enable_shadow=True, enable_elliptical=True):
    """Fresh original side-on mallard illustration, entirely editable paths.

    Layer and object ordering deliberately follow feather anatomy: surrounding
    reeds/water, tail behind body, breast under continuous upright neck, wing
    over body, coverts over flight feathers, speculum under feather bars.
    """
    layers = [{"name": value} for value in (
        "01 · Mist and morning", "02 · Marsh behind", "03 · Water and reflection",
        "04 · Tail and silhouette", "05 · Neck and breast", "06 · Folded wing",
        "07 · Plumage texture", "08 · Face and bill", "09 · Foreground water")]
    objects = []

    # Pale blue morning atmosphere and water; gradients are literal canvas
    # coordinates (all scene objects have identity transforms).
    objects.append(paint({"type": "rectangle", "width": 1100, "height": 700}, 0,
                         linear(0, 0, 0, 700, (204, 226, 231, 255),
                                (241, 236, 212, 255), (186, 210, 204, 255))))
    objects.append(paint(path(M(0, 445), C(170, 431, 275, 440, 424, 446),
                              C(605, 460, 807, 438, 1100, 453),
                              L(1100, 700), L(0, 700), Z()), 2,
                         linear(0, 449, 0, 700, (139, 191, 185, 255),
                                (80, 139, 148, 255), (51, 106, 131, 255))))
    # Distant marsh banks are quiet organic shapes rather than repeated ovals.
    objects.append(paint(path(M(0, 447), C(118, 431, 169, 447, 233, 453),
                              Q(275, 465, 312, 452), L(312, 481),
                              Q(170, 460, 0, 479), Z()), 1, solid(104, 142, 113)))
    objects.append(paint(path(M(790, 455), Q(917, 420, 1100, 437),
                              L(1100, 479), Q(982, 452, 790, 477), Z()),
                         1, solid(114, 146, 110)))
    # Distant reed stems have tapered, uninterrupted directional gestures.
    for origin_x, bend, extent in ((95, -38, 301), (129, 13, 334),
                                  (180, 41, 353), (934, -29, 338), (991, 31, 320)):
        objects.append(paint(path(M(origin_x, 460),
                                  Q(origin_x + bend, 402, origin_x + bend, extent)),
                             1, stroke=[89, 114, 83, 185], width=3))

    # The swimming body's broad contact shadow lies underneath all anatomy.
    objects.append(paint(path(M(323, 534), C(451, 516, 627, 519, 746, 536),
                              Q(675, 555, 471, 567), Q(372, 558, 323, 534), Z()),
                         2, solid(38, 99, 118, 88)))

    # Separate swept back tail feather tips radiate from the rump.
    objects.append(paint(path(M(368, 412), C(331, 400, 293, 382, 260, 354),
                              C(274, 387, 303, 429, 356, 444), Z()),
                         3, solid(73, 72, 63)))
    objects.append(paint(path(M(364, 426), C(307, 422, 286, 408, 244, 389),
                              Q(280, 450, 360, 456), Z()),
                         3, solid(118, 101, 75)))
    objects.append(paint(path(M(350, 435), Q(294, 429, 277, 418),
                              C(303, 455, 328, 468, 367, 474), Z()),
                         3, solid(155, 131, 95)))

    body_fill = (elliptical(501, 410, 297, 197, (198, 160, 107, 255),
                           (144, 105, 68, 255), (91, 74, 61, 255))
                 if enable_elliptical else solid(161, 122, 81))
    body_shadow = ({"offset": pt(7, 12), "blur_radius": 14,
                    "color": [37, 59, 61, 110]} if enable_shadow else None)
    objects.append(paint(duck_body(), 3, body_fill,
                         stroke=[86, 69, 61, 255], width=2,
                         shadow=body_shadow))

    # A single curved chest connects visual torso to neck, avoiding the
    # characteristic pasted-on-circle duck error.
    objects.append(paint(path(M(614, 410), C(655, 397, 704, 415, 725, 449),
                              C(749, 487, 729, 505, 693, 522),
                              C(666, 524, 632, 487, 614, 410), Z()), 4,
                         linear(615, 403, 734, 513, (147, 100, 73, 255),
                                (111, 73, 66, 255))))
    # Slender continuous upright green neck and round skull are ONE shape.
    neck = path(M(631, 414), C(649, 382, 648, 354, 632, 319),
                C(619, 289, 613, 256, 626, 233),
                C(642, 203, 687, 193, 717, 207),
                C(751, 223, 753, 258, 738, 285),
                C(730, 302, 711, 323, 709, 343),
                C(709, 374, 723, 401, 731, 431),
                C(690, 435, 657, 430, 631, 414), Z())
    neck_fill = (elliptical(682, 251, 83, 143, (51, 137, 119, 255),
                            (26, 91, 80, 255), (18, 56, 59, 255))
                 if enable_elliptical else solid(36, 103, 87))
    objects.append(paint(neck, 4, neck_fill,
                         stroke=[27, 69, 64, 255], width=2,
                         shadow={"offset": pt(2, 6), "blur_radius": 6,
                                 "color": [23, 56, 59, 75]} if enable_shadow else None))
    # Iridescent cheek following cranial contour, transparent feather sheen.
    objects.append(paint(path(M(648, 237), C(657, 214, 691, 213, 714, 226),
                              Q(689, 219, 669, 248),
                              C(651, 273, 651, 299, 668, 318),
                              Q(633, 294, 648, 237), Z()), 4,
                         solid(64, 163, 143, 120)))
    # White separating neck collar is a tapered contour, not a detached ring.
    objects.append(paint(path(M(632, 322), C(661, 341, 695, 337, 712, 319),
                              L(714, 336), C(681, 353, 658, 356, 641, 345), Z()),
                         4, solid(243, 239, 219)))
    objects.append(paint(path(M(643, 348), Q(680, 356, 713, 340),
                              L(717, 359), Q(683, 376, 649, 367), Z()),
                         4, solid(55, 73, 68)))

    # Folding wing sits anatomically above flank and beneath rear scapulars.
    wing_fill = (elliptical(494, 408, 170, 108, (180, 145, 104, 255),
                           (123, 94, 74, 255), (77, 70, 68, 255))
                 if enable_elliptical else solid(151, 114, 84))
    objects.append(paint(primary_wing(), 5, wing_fill,
                         stroke=[88, 74, 67, 255], width=2))

    # Long cobalt speculum and iridescent secondaries. Its soft edge masks to
    # the wing silhouette; same local coordinates for shape and clip_shape.
    speculum = path(M(460, 440), C(503, 423, 557, 438, 622, 466),
                    C(599, 492, 541, 499, 492, 471), Z())
    objects.append(paint(speculum, 6, linear(465, 430, 624, 486,
                                             (38, 76, 140, 255),
                                             (63, 88, 177, 255),
                                             (31, 52, 112, 255)),
                         clip=primary_wing() if enable_clip else None))
    for yshift in (0, 9):
        objects.append(paint(path(M(471, 436 + yshift),
                                  C(512, 423 + yshift, 564, 440 + yshift,
                                    618, 458 + yshift)), 6,
                             stroke=[238, 231, 217, 238] if not yshift
                             else [38, 55, 81, 210], width=3))

    # Distinct feather groups follow the direction of the wing, not a grid.
    for dx, dy, scale in ((0, 0, 1.0), (24, 10, .95), (48, 18, .81),
                          (73, 29, .66), (96, 36, .55)):
        x = 431 + dx
        y = 403 + dy
        objects.append(paint(path(M(x, y), C(x + 35 * scale, y - 7,
                                       x + 59 * scale, y + 1,
                                       x + 82 * scale, y + 17),
                                  Q(x + 44 * scale, y + 13,
                                    x + 11 * scale, y + 23), Z()),
                             6, solid(211 - int(dx * .5), 177 - int(dx * .42),
                                      130 - int(dx * .22), 247),
                             stroke=[115, 91, 71, 150], width=.8))

    # Clipped directional highlights make the large body a coherent surface.
    band = path(M(325, 427), C(435, 408, 587, 436, 699, 497),
                L(720, 530), C(560, 459, 426, 434, 318, 457), Z())
    objects.append(paint(band, 6, solid(236, 207, 169, 40),
                         clip=duck_body() if enable_clip else None))

    # Bill is joined just forward of eye line; upper and lower lobes separated.
    objects.append(paint(path(M(731, 252), C(756, 249, 774, 255, 814, 274),
                              Q(830, 283, 801, 296),
                              C(775, 298, 748, 293, 730, 283), Z()), 7,
                         linear(738, 255, 818, 294, (243, 196, 74, 255),
                                (215, 147, 49, 255)),
                         stroke=[101, 91, 53, 255], width=2))
    objects.append(paint(path(M(742, 281), Q(773, 289, 813, 282),
                              Q(800, 301, 759, 299), Z()), 7,
                         solid(169, 112, 50)))
    objects.append(paint(path(M(748, 261), Q(766, 261, 783, 267)),
                         7, stroke=[255, 220, 127, 255], width=2))
    objects.append(paint(path(M(777, 273), Q(780, 272, 781, 276),
                              Q(777, 280, 775, 276), Z()),
                         7, solid(94, 78, 49)))
    # Eye is a filled ring with a true negative-space cutout; even-odd winding
    # reveals underlying head, followed by pupil and a single catchlight.
    eye_ring = path(M(704, 248), C(704, 236, 716, 232, 724, 239),
                    C(732, 246, 728, 257, 717, 259),
                    Q(705, 262, 704, 248), Z(),
                    M(708, 246), Q(716, 235, 723, 245),
                    Q(724, 255, 716, 256), Q(708, 254, 708, 246), Z())
    objects.append(paint(eye_ring, 7, solid(216, 177, 94),
                         stroke=[42, 55, 49, 255], width=.8))
    objects.append(paint(path(M(711, 243), C(717, 239, 724, 243, 724, 250),
                              C(723, 257, 714, 257, 711, 252), Z()),
                         7, solid(24, 35, 33)))
    objects.append(paint(path(M(715, 243), Q(718, 241, 720, 244),
                              Q(718, 247, 715, 243), Z()),
                         7, solid(243, 248, 238)))

    # Foreground wakes are slender outlined curved contours; include the
    # required quadratic paths as natural water movements.
    for x, y, span in ((255, 555, 141), (702, 570, 210),
                       (398, 601, 283), (773, 611, 136)):
        objects.append(paint(path(M(x, y), Q(x + span / 2, y - 15,
                                      x + span, y + 2)), 8,
                             stroke=[219, 239, 227, 175], width=3))
    # A compound water ripple has a genuine elliptical hole in its fill.
    objects.append(paint(path(M(338, 584), C(418, 562, 623, 558, 703, 581),
                              C(626, 613, 421, 614, 338, 584), Z(),
                              M(370, 584), C(450, 574, 591, 571, 672, 583),
                              C(574, 603, 468, 604, 370, 584), Z()),
                         8, solid(225, 240, 220, 68)))
    # Stems beyond the near right edge frame rather than obscure the duck.
    for x, tip_x, tip_y in ((1027, 995, 426), (1067, 1068, 388)):
        objects.append(paint(path(M(x, 700), Q(tip_x + 12, 525, tip_x, tip_y)),
                             8, stroke=[49, 93, 86, 225], width=5))

    # Keep object extent and payload bounded so one stdio request stays below
    # the agent's 2 MiB framing ceiling. There are only a few dozen objects.
    assert len(objects) < 160
    return {"layers": layers, "objects": objects}


def feature_support(capabilities, schema):
    """Require path/scene support, conditionally exercise newer embellishments."""
    model = capabilities.get("model", {})
    features = {
        "clip_shape": bool(model.get("clip_shape", False)),
        "shadow": "shadow" in model.get("style", []),
        "elliptical_gradient": "elliptical_gradient" in model.get("style", []),
    }
    if "path" not in model.get("shapes", []):
        raise RuntimeError("Agent lacks editable shape.type=path; build updated Inkpad MCP")
    mandatory = {"move_to", "line_to", "cubic_to", "quad_to", "close"}
    observed = set(model.get("path_commands", []))
    if not mandatory.issubset(observed):
        raise RuntimeError(f"Missing path commands: {sorted(mandatory - observed)}")
    props = schema["inputSchema"]["properties"]["objects"]["items"]["properties"]
    if not {"shape", "style", "layer_index"}.issubset(props):
        raise RuntimeError("Scene input schema missing shape/style/layer_index")
    if not any(v.get("properties", {}).get("type", {}).get("const") == "path"
               for v in props["shape"].get("oneOf", [])):
        raise RuntimeError("Scene schema does not advertise the path variant")
    features["clip_shape"] &= "clip_shape" in props
    style_props = props["style"]["properties"]
    features["shadow"] &= "shadow" in style_props
    fill_options = style_props["fill"]["oneOf"]
    features["elliptical_gradient"] &= any(
        item.get("properties", {}).get("type", {}).get("const") == "elliptical_gradient"
        for option in fill_options for item in option.get("oneOf", [option]))
    return features


def call_expect_rejection(client, tool_name, arguments):
    """MCP helper asserts success; send raw JSON-RPC for an expected error."""
    client.counter += 1
    request = {"jsonrpc": "2.0", "id": client.counter, "method": "tools/call",
               "params": {"name": tool_name, "arguments": arguments}}
    client.proc.stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
    client.proc.stdin.flush()
    line = client.proc.stdout.readline()
    if not line:
        raise RuntimeError("Server terminated during negative-case validation")
    result = json.loads(line)
    if result.get("id") != client.counter:
        raise AssertionError("Mismatched error response ID")
    rejected = "error" in result or result.get("result", {}).get("isError") is True
    if not rejected:
        raise AssertionError("Malformed scene was accepted instead of atomically rejected")
    return result


def png_pixels(blob):
    """Decode a normal 8-bit RGB/RGBA PNG using only Python stdlib.

    Applies PNG's five scanline filters so a genuine sampled raster, rather
    than merely a valid PNG header, establishes that paint was rendered.
    """
    assert blob.startswith(PNG_MAGIC), "Invalid PNG signature"
    pos = len(PNG_MAGIC)
    parts = []
    width = height = color_type = depth = None
    while pos + 12 <= len(blob):
        size = struct.unpack_from(">I", blob, pos)[0]
        kind = blob[pos + 4:pos + 8]
        data = blob[pos + 8:pos + 8 + size]
        checksum = struct.unpack_from(">I", blob, pos + 8 + size)[0]
        assert zlib.crc32(kind + data) & 0xFFFFFFFF == checksum, "Bad PNG chunk CRC"
        pos += size + 12
        if kind == b"IHDR":
            width, height, depth, color_type, compression, filtering, interlace = struct.unpack(
                ">IIBBBBB", data)
            assert (compression, filtering, interlace) == (0, 0, 0)
        elif kind == b"IDAT":
            parts.append(data)
        elif kind == b"IEND":
            break
    assert (width, height) == CANVAS, (width, height)
    assert depth == 8 and color_type in (2, 6), (depth, color_type)
    bpp = 4 if color_type == 6 else 3
    stride = width * bpp
    raw = zlib.decompress(b"".join(parts))
    assert len(raw) == height * (1 + stride), "PNG scanlines have wrong length"
    rows = []
    prev = bytearray(stride)
    position = 0
    for _ in range(height):
        mode = raw[position]
        row = bytearray(raw[position + 1:position + 1 + stride])
        position += 1 + stride
        for j in range(stride):
            left = row[j - bpp] if j >= bpp else 0
            above = prev[j]
            upper_left = prev[j - bpp] if j >= bpp else 0
            if mode == 0:
                predictor = 0
            elif mode == 1:
                predictor = left
            elif mode == 2:
                predictor = above
            elif mode == 3:
                predictor = (left + above) // 2
            elif mode == 4:
                p = left + above - upper_left
                distances = (abs(p - left), abs(p - above), abs(p - upper_left))
                predictor = (left, above, upper_left)[distances.index(min(distances))]
            else:
                raise AssertionError(f"Unsupported PNG filter {mode}")
            row[j] = (row[j] + predictor) & 255
        rows.append(bytes(row))
        prev = row

    def pixel(x, y):
        values = rows[y][x * bpp:x * bpp + bpp]
        return tuple(values[:3])

    nonwhite = sum(
        any(row[k + n] < 246 for n in range(3))
        for row in rows for k in range(0, stride, bpp))
    return nonwhite, pixel


def run(executable, output):
    if output is not None:
        if output.suffix.lower() != ".png":
            raise ValueError("Explicit --output must end with .png")
        if not output.parent.is_dir():
            raise ValueError("Output parent directory must already exist")
        if output.exists() or output.is_symlink():
            raise FileExistsError(f"Refusing to overwrite existing output: {output}")

    with tempfile.TemporaryDirectory(prefix="inkpad-advanced-vector-mcp-") as directory:
        workspace = Path(directory)
        started = time.perf_counter()
        client = MCP(executable, workspace)
        try:
            discovery, _ = client.call("tools/list", {})
            tools = {tool["name"]: tool for tool in discovery["result"]["tools"]}
            for required in ("create_note", "drawing_create_scene", "drawing_list",
                             "drawing_preview", "get_capabilities"):
                if required not in tools:
                    raise RuntimeError(f"MCP missing {required}; compile updated inkpad-agent")
            caps_reply, _ = client.tool("get_capabilities", {})
            caps = caps_reply["result"]["structuredContent"]
            assert tuple(caps["canvas"][k] for k in ("width", "height")) == CANVAS
            features = feature_support(caps, tools["drawing_create_scene"])
            drawing = scene(enable_clip=features["clip_shape"],
                            enable_shadow=features["shadow"],
                            enable_elliptical=features["elliptical_gradient"])
            title = "Isolated path study · mallard 🦆 / 橙光"
            created, _ = client.tool("create_note", {"title": title, "body": "Disposable MCP art test 🍃"})
            note_id = created["result"]["structuredContent"]["id"]

            call_args = {"id": note_id, "expected_object_count": 0, **drawing}
            scene_reply, scene_ms = client.tool("drawing_create_scene", call_args)
            scene_result = scene_reply["result"]["structuredContent"]
            expected_objects = len(drawing["objects"])
            expected_layers = len(drawing["layers"])
            assert scene_result["added_objects"] == expected_objects
            assert scene_result["added_layers"] == expected_layers
            ids = scene_result["object_ids"]
            assert len(ids) == len(set(ids)) == expected_objects
            listed, _ = client.tool("drawing_list", {"id": note_id})
            detail = listed["result"]["structuredContent"]
            assert detail["object_count"] == expected_objects
            assert detail["layer_count"] == expected_layers
            assert len(detail["drawing"]["objects"]) == expected_objects
            assert [o["id"] for o in detail["drawing"]["objects"]] == ids
            assert [layer["name"] for layer in detail["drawing"]["layers"]] == [
                layer["name"] for layer in drawing["layers"]]

            # A malformed SECOND object must not retain the first object/layer
            # or touch even one byte of the saved, existing workspace.
            storage = workspace / "workspace.json"
            before = storage.read_bytes()
            shape = drawing["objects"][0]["shape"]
            invalid = {"id": note_id, "expected_object_count": expected_objects,
                       "layers": [{"name": "Must never be saved"}], "objects": [
                           {"layer_index": 0, "shape": shape},
                           {"layer_index": 0, "shape": path(L(5, 5), M(10, 10), Z())},
                           {"layer_index": 0, "shape": drawing["objects"][2]["shape"]}]}
            call_expect_rejection(client, "drawing_create_scene", invalid)
            assert storage.read_bytes() == before, "Invalid middle operation wrote workspace"

            # Also reject the last object's nonsensical layer index atomically.
            invalid["objects"][1] = copy.deepcopy(drawing["objects"][1])
            invalid["objects"][1]["layer_index"] = 0
            invalid["objects"][2]["layer_index"] = 98
            call_expect_rejection(client, "drawing_create_scene", invalid)
            assert storage.read_bytes() == before, "Invalid late layer wrote workspace"

            # This passes parser-level validation; the missing but well-formed
            # layer ID fails AFTER earlier candidates were appended inside the
            # Store transaction. The full workspace must still roll back.
            invalid["objects"][2].pop("layer_index")
            invalid["objects"][2]["layer_id"] = "missing-layer-smoke"
            call_expect_rejection(client, "drawing_create_scene", invalid)
            assert storage.read_bytes() == before, "Rejected in-transaction layer wrote workspace"

            after, _ = client.tool("drawing_list", {"id": note_id})
            assert after["result"]["structuredContent"]["drawing"] == detail["drawing"]

            preview, preview_ms = client.tool("drawing_preview", {"id": note_id})
            rendered = preview["result"]["structuredContent"]
            block = next((c for c in preview["result"]["content"]
                          if c.get("type") == "image"), None)
            assert block is not None and block["mimeType"] == "image/png"
            png = base64.b64decode(block["data"], validate=True)
            assert png.startswith(PNG_MAGIC)
            assert (rendered["width"], rendered["height"]) == CANVAS
            assert rendered["object_count"] == expected_objects
            count, pixel = png_pixels(png)
            assert count > CANVAS[0] * CANVAS[1] // 4, "Image appears blank"
            samples = {k: pixel(*xy) for k, xy in {
                "sky": (80, 110), "water": (120, 634),
                "mallard_head": (676, 251), "mallard_body": (527, 510),
                "mallard_bill": (777, 277)}.items()}
            assert len(set(samples.values())) >= 3, f"No distinct material colours: {samples}"

            # Verify optional advanced metadata was actually persisted by MCP.
            serialized = detail["drawing"]["objects"]
            assert any(obj["shape"]["type"] == "path" for obj in serialized)
            assert any(len(obj["shape"].get("commands", [])) >= 10 for obj in serialized)
            if features["clip_shape"]:
                assert any(obj.get("clip_shape") for obj in serialized)
            if features["shadow"]:
                assert any((obj.get("style") or {}).get("shadow") for obj in serialized)
            if features["elliptical_gradient"]:
                assert any(((obj.get("style") or {}).get("fill") or {}).get("type") == "elliptical_gradient"
                           for obj in serialized)

            # No source-file write; artifact appears only when caller explicitly
            # supplies a new path. 'xb' refuses overwrite even if file appeared
            # after our initial existence check.
            if output is not None:
                with output.open("xb") as handle:
                    handle.write(png)
            return {
                "status": "pass", "exe": str(executable),
                "workspace": "isolated temporary directory (removed)",
                "layers": expected_layers, "objects": expected_objects,
                "path_commands": ["move_to", "line_to", "cubic_to", "quad_to", "close"],
                "features_exercised": features, "atomic_rejections": 3,
                "png_size": len(png), "png_sha256": hashlib.sha256(png).hexdigest(),
                "png_dimensions": CANVAS, "nonwhite_pixels": count,
                "samples_rgb": samples, "scene_ms": round(scene_ms, 2),
                "preview_ms": round(preview_ms, 2),
                "elapsed_ms": round((time.perf_counter() - started) * 1000, 2),
                "output": str(output) if output is not None else None,
            }
        finally:
            client.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("executable", type=Path, help="Path to tested inkpad-agent executable")
    parser.add_argument("--output", type=Path, help="Optional NEW PNG output (never overwrites)")
    args = parser.parse_args()
    try:
        executable = args.executable.expanduser().resolve(strict=True)
        if not executable.is_file():
            raise ValueError(f"Not an executable file: {executable}")
        if args.output is not None and args.output.expanduser().is_symlink():
            raise FileExistsError("Refusing symlink output; provide a new regular PNG filename")
        output = args.output.expanduser().resolve() if args.output else None
        print(json.dumps(run(executable, output), ensure_ascii=False), flush=True)
    except (OSError, ValueError, RuntimeError, AssertionError, KeyError, TypeError) as error:
        print(json.dumps({"status": "fail", "error": str(error),
                          "error_type": type(error).__name__}, ensure_ascii=False),
              file=sys.stderr, flush=True)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
