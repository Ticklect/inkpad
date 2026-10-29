r"""Isolated advanced-vector feather compositing / clipping / shadow QA.

Usage (from D:\chat\inkpad):
    py -3 tests/vector_illustration_structural_qa.py
    py -3 tests/vector_illustration_structural_qa.py --exe D:\absolute\inkpad-agent.exe

Uses the tested packaged Inkpad MCP stdio binary, but NEVER a user notebook.
MCP's INKPAD_DATA_DIR is a unique TemporaryDirectory. Only NEW artifacts under
target/vector-structural-qa-20261018 are retained; creation is exclusive.
No existing artifacts, Inkpad sources, skills or user settings are modified.

The controls test *renderer output*, not merely accepting MCP arguments:
  1. even-odd intersecting compound paths (deliberate cutout);
  2. rotated / scaled, clipped feather with a partial-alpha gradient and hole;
  3. layered crossing feather with per-pixel source-over compositing oracle;
  4. same feather with an offset alpha-correct shadow behind it.
Every scene is one MCP drawing_create_scene call. PNG previews must exactly
match the separately exported PNGs. Failures retain their reference evidence.
"""

import argparse
import base64
from datetime import datetime, timezone
import hashlib
import json
import math
from pathlib import Path
import statistics
import sys
import tempfile
import uuid

from benchmark_agent_mcp import MCP
from advanced_vector_mcp_smoke import png_pixels

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_EXE = ROOT / "dist" / "Inkpad-Beta-Vector-Placement-QA-Candidate-20261018" / "inkpad-agent.exe"
OUTPUT_ROOT = ROOT / "target" / "vector-structural-qa-20261018"
CANVAS = (1100, 700)
BASE = (44, 97, 128)
SECOND = (239, 176, 69)
SHADOW = (12, 18, 48, 200)
ANGLE = 12.0
SCALE = 1.1
ORIGIN = (520, 305)


def pt(x, y):
    return {"x": x, "y": y}


def m(x, y):
    return {"type": "move_to", "to": pt(x, y)}


def l(x, y):
    return {"type": "line_to", "to": pt(x, y)}


def c(ax, ay, bx, by, x, y):
    return {"type": "cubic_to", "control1": pt(ax, ay),
            "control2": pt(bx, by), "to": pt(x, y)}


def z():
    return {"type": "close"}


def path(*segments):
    return {"type": "path", "commands": list(segments)}


def colour(rgb, alpha=255):
    return {"type": "solid", "color": [*rgb, alpha]}


def style(fill, opacity=1.0, shadow=None):
    result = {"fill": fill, "opacity": opacity, "stroke": None, "stroke_width": 0}
    if shadow:
        result["shadow"] = shadow
    return result


def pos(x, y):
    return {"translation": pt(x, y)}


def feather_outline():
    # Directionally curved asymmetric feather: broad root / tapered tip.
    return path(m(-120, -28), c(-38, -69, 67, -48, 145, -1),
                c(69, 40, -48, 52, -120, -28), z())


def feather_mask():
    # Cut into the main silhouette and make a rectangular negative-space
    # notch: both contours must participate in even-odd fill/clip crossings.
    return path(m(-115, -25), c(-37, -48, 46, -35, 119, -2),
                c(70, 26, -43, 35, -115, -25), z(),
                m(-25, -11), l(12, -11), l(12, 11), l(-25, 11), z())


def feather_fill():
    # Gradient geometry is LOCAL to the feather, not canvas-space.
    return {"type": "linear_gradient", "start": pt(-120, 0),
            "end": pt(145, 0), "stops": [
                {"offset": 0, "color": [232, 80, 38, 180]},
                {"offset": 1, "color": [66, 45, 213, 65]}]}


def transform():
    return {"translation": pt(*ORIGIN), "scale_x": SCALE,
            "scale_y": SCALE, "rotation_deg": ANGLE}


def world(local, origin=ORIGIN, angle=ANGLE, scale=SCALE):
    t = math.radians(angle)
    x, y = local
    return (round(origin[0] + scale * (x * math.cos(t) - y * math.sin(t))),
            round(origin[1] + scale * (x * math.sin(t) + y * math.cos(t))))


def inverse(world_xy, origin=ORIGIN, angle=ANGLE, scale=SCALE):
    t = math.radians(angle)
    x, y = ((world_xy[0] - origin[0]), (world_xy[1] - origin[1]))
    return ((x * math.cos(t) + y * math.sin(t)) / scale,
            (-x * math.sin(t) + y * math.cos(t)) / scale)


def main_feather(shadow=False):
    value = {"shape": feather_outline(), "clip_shape": feather_mask(),
             "transform": transform(), "layer_index": 1,
             "style": style(feather_fill(), opacity=.65,
                 shadow={"offset": pt(0, 35), "blur_radius": 0,
                         "color": list(SHADOW)} if shadow else None)}
    return value


def secondary_feather():
    # The tip overlaps the middle of the first feather in the render.
    return {"shape": path(m(-110, -23), c(-51, -45, 52, -43, 126, 0),
                          c(46, 32, -40, 36, -110, -23), z()),
            "clip_shape": path(m(-102, -19), c(-40, -36, 52, -34, 117, 0),
                               c(42, 28, -35, 30, -102, -19), z()),
            "transform": {"translation": pt(555, 317), "scale_x": .95,
                          "scale_y": .95, "rotation_deg": -8},
            "layer_index": 2,
            "style": style(colour(SECOND, 140), opacity=.72)}


def underpainting():
    return {"shape": {"type": "rectangle", "width": 530, "height": 320},
            "position": pt(325, 150), "layer_index": 0,
            "style": style(colour(BASE))}


def feather_scene(secondary=False, shadow=False):
    layers = [{"name": "01 - single-piece underpainting"},
              {"name": "02 - translated gradient flight feather"}]
    objs = [underpainting(), main_feather(shadow=shadow)]
    if secondary:
        layers.append({"name": "03 - overlapping translucent covert"})
        objs.append(secondary_feather())
    # The layer index, not literal drawing-object insertion order,
    # defines overlap. The top layer is intentionally put first.
    objs.reverse()
    return {"layers": layers, "objects": objs}


def evenodd_scene(shadow=False):
    background = {"shape": {"type": "rectangle", "width": 380,
                             "height": 140}, "position": pt(103, 468),
                  "layer_index": 0,
                  "style": style(colour(BASE))}
    intersecting = path(
        m(0, 0), l(186, 0), l(186, 90), l(0, 90), z(),
        m(82, 0), l(278, 0), l(278, 90), l(82, 90), z())
    top = {"shape": intersecting, "position": pt(135, 492),
           "layer_index": 1, "style": style(
               colour((226, 53, 52), 174), .7,
               {"offset": pt(0,20), "blur_radius": 0,
                "color": [12,18,48,200]} if shadow else None)}
    return {"layers": [{"name": "Under feather"},
                       {"name": "Overlapping compound contours"}],
            "objects": [top, background]}


def hash_bytes(raw):
    return hashlib.sha256(raw).hexdigest()


def write_new(path_, raw):
    with path_.open("xb") as f:
        f.write(raw)
    return path_


def blend(dst, rgb, alpha):
    return tuple(int(math.floor(a * (1 - alpha) + b * alpha + 0.5))
                 for a, b in zip(dst, rgb))


def near(actual, expected, limit=3, desc=""):
    delta = max(abs(a-b) for a, b in zip(actual, expected))
    if delta > limit:
        raise AssertionError(f"{desc}: actual RGB {actual}, expected {expected}, max delta {delta}>{limit}")


def inspect_schema(client):
    reply, elapsed = client.call("tools/list", {})
    tools = {v["name"]: v for v in reply["result"]["tools"]}
    mandatory = ("create_note", "get_capabilities", "drawing_create_scene",
                 "drawing_list", "drawing_preview", "export_note")
    if any(n not in tools for n in mandatory):
        raise RuntimeError(f"Missing MCP tools: {[n for n in mandatory if n not in tools]}")
    caps, _ = client.tool("get_capabilities")
    caps = caps["result"]["structuredContent"]
    if tuple(caps["canvas"][k] for k in ("width", "height")) != CANVAS:
        raise RuntimeError("Unexpected actual MCP canvas dimensions")
    model = caps["model"]
    if not (model["clip_shape"] and {"shadow", "linear_gradient"}.issubset(model["style"])):
        raise RuntimeError("Candidate does not advertise required clip, gradient and shadow features")
    schema = tools["drawing_create_scene"]["inputSchema"]
    props = schema["properties"]["objects"]["items"]["properties"]
    if not {"clip_shape", "transform", "style", "layer_index", "position"}.issubset(props):
        raise RuntimeError("Scene tool schema lacks advanced vector object properties")
    variants = {x["properties"]["type"]["const"] for x in props["shape"]["oneOf"]}
    if not {"path", "rectangle"}.issubset(variants):
        raise RuntimeError("Actual tool schema lacks path/rectangle variants")
    return len(tools), round(elapsed, 3)


def render_scene(client, spec, title, scratch, metrics):
    created, _ = client.tool("create_note", {"title": "Disposable structural QA: " + title})
    note_id = created["result"]["structuredContent"]["id"]
    metrics["scene_calls"] += 1
    result, elapsed = client.tool("drawing_create_scene",
                                  {"id": note_id, "expected_object_count": 0, **spec})
    info = result["result"]["structuredContent"]
    assert info["added_objects"] == len(spec["objects"]), (title, info)
    assert info["added_layers"] == len(spec["layers"]), (title, info)
    metrics["scene_times_ms"][title] = round(elapsed, 3)
    check, _ = client.tool("drawing_list", {"id": note_id})
    assert check["result"]["structuredContent"]["object_count"] == len(spec["objects"])
    assert check["result"]["structuredContent"]["layer_count"] == len(spec["layers"])
    preview, elapsed = client.tool("drawing_preview", {"id": note_id})
    block = next(c for c in preview["result"]["content"] if c.get("type") == "image")
    raw = base64.b64decode(block["data"], validate=True)
    meta = preview["result"]["structuredContent"]
    assert (meta["width"], meta["height"]) == CANVAS
    assert meta["object_count"] == len(spec["objects"])
    metrics["preview_times_ms"][title] = round(elapsed, 3)
    export_file = scratch / (title + "-export.png")
    assert not export_file.exists()
    client.tool("export_note", {"id": note_id, "format": "png",
                                "path": str(export_file.resolve())})
    exported = export_file.read_bytes()
    if exported != raw:
        raise AssertionError(f"{title}: saved export differs from actual MCP preview")
    write_new(scratch / (title + "-preview.png"), raw)
    nonwhite, sample = png_pixels(raw)
    return {"sha256": hash_bytes(raw), "png_bytes": len(raw),
            "nonwhite_pixels": nonwhite, "object_count": len(spec["objects"]),
            "layer_count": len(spec["layers"]), "sample": sample}


def check_evenodd(rendered):
    p = rendered["sample"]
    # Two overlapping contours obey SVG-style even-odd parity: the intersection
    # is empty, and either non-overlapped lobe gets precisely one alpha blend.
    alpha = 174 / 255 * .7
    painted = blend(BASE, (226, 53, 52), alpha)
    points = {"first_lobe": (160, 520), "overlap_cutout": (260, 520),
              "second_lobe": (360, 520), "outside": (470, 595)}
    checks = {name: p(*xy) for name, xy in points.items()}
    near(checks["first_lobe"], painted, 2, "even-odd first lobe")
    near(checks["overlap_cutout"], BASE, 0, "even-odd overlapping cutout")
    near(checks["second_lobe"], painted, 2, "even-odd second lobe")
    near(checks["outside"], BASE, 0, "opaque underpainting outside feather")
    return checks


def check_evenodd_shadow(rendered):
    p = rendered["sample"]
    # The even-odd intersection is also absent from the SHADOW alpha mask.
    # y=600 is 18 px below the visible contours; with local shadow offset +20
    # it samples a solid part of the feather at source y=580.
    outer = (160,600)
    hole = (260,600)
    other = (360,600)
    alpha = (174/255) * .7 * (200/255)
    expected = blend(BASE, (12,18,48),alpha)
    samples = {"first_shadow":p(*outer),"overlap_shadow_hole":p(*hole),
               "second_shadow":p(*other)}
    near(samples["first_shadow"],expected,2,"overlap contour shadow first lobe")
    near(samples["overlap_shadow_hole"],BASE,0,"overlap contour shadow must retain hole")
    near(samples["second_shadow"],expected,2,"overlap contour shadow second lobe")
    return samples


def gradient_sample(local_x):
    t = max(0.0, min(1.0, (local_x + 120) / 265))
    rgba = [int(math.floor(a * (1-t) + b*t + .5))
            for a,b in zip((232,80,38,180), (66,45,213,65))]
    return tuple(rgba[:3]), rgba[3] / 255 * .65


def check_transformed_cutouts(rendered):
    p = rendered["sample"]
    samples = {label: world(local) for label, local in (
        ("inboard", (-73, 0)), ("tip", (65, 0)), ("hole", (-7, 0)),
        ("clipped_side", (130, 0)), ("empty_origin", (-120, -130)))}
    output = {}
    for label, xy in samples.items():
        actual = p(*xy)
        if label in ("inboard", "tip"):
            local_x, _ = inverse(xy)
            ink, alpha = gradient_sample(local_x)
            expected = blend(BASE, ink, alpha)
            near(actual, expected, 3, label + " translated clipped gradient / effective alpha")
        elif label == "empty_origin":
            # Outside the rectangular teal underpainting, not just the clip.
            near(actual, (255,255,255), 0, label + " empty canvas")
        else:
            near(actual, BASE, 0, label + " negative clipping / even-odd hole")
        output[label] = {"xy": xy, "rgb": actual}
    return output


def check_crossing_feathers(single, only_top, combined):
    base = BASE
    first = single["sample"]
    top = only_top["sample"]
    both = combined["sample"]
    # The secondary object is constant-colour but semi-transparent. Derive its
    # empirical coverage from the CONTROL rendered without feather one, then
    # independently predict the full layered scene pixel by pixel. This tests
    # order, edge coverage and premultiplication rather than a single token
    # 'nonwhite' assertion. The regions are directional curved feather paths.
    overlap = []
    tested = 0
    worst = 0
    for y in range(254, 376, 4):
        for x in range(413, 694, 4):
            a, b, actual = first(x,y), top(x,y), both(x,y)
            first_diff = max(abs(c-d) for c,d in zip(a,base))
            top_diff = max(abs(c-d) for c,d in zip(b,base))
            if first_diff <= 9 or top_diff <= 9:
                continue
            alpha = max(0., min(1., (b[0]-base[0])/(SECOND[0]-base[0])))
            predicted = blend(a, SECOND, alpha)
            delta = max(abs(c-d) for c,d in zip(actual,predicted))
            worst = max(worst, delta)
            tested += 1
            if len(overlap) < 8:
                overlap.append({"xy":[x,y],"main":a,"top_alone":b,
                                "actual":actual,"expected":predicted,"delta":delta})
    if tested < 12:
        raise AssertionError(f"Insufficient crossing feather pixels: {tested}")
    if worst > 4:
        raise AssertionError(f"Directional feather source-over mismatch: max delta {worst}")
    return {"overlap_pixels_tested":tested,"worst_channel_error":worst,
            "sample_pairs":overlap}


def check_shadow(single, shadowed):
    p = shadowed["sample"]
    ref = single["sample"]
    out = {}
    # A local +35px shadow offset, blur 0, should darken the bare underlayer
    # behind the feather while respecting source gradient alpha and opacity.
    for label, xlocal in (("root_shadow", -72), ("mid_shadow", 65)):
        target = world((xlocal,35))
        before = ref(*target)
        after = p(*target)
        near(before, BASE, 0, label + " target must be outside silhouette")
        source_xy = world((xlocal,0))
        local_at_source = inverse(source_xy)
        _, effective_alpha = gradient_sample(local_at_source[0])
        expected = blend(BASE, SHADOW[:3], effective_alpha * SHADOW[3]/255)
        near(after, expected, 8, label + " alpha-correct offset shadow")
        out[label] = {"xy":list(target),"actual":after,"expected":expected}
    return out


def run(exe):
    if not exe.is_file():
        raise FileNotFoundError(exe)
    OUTPUT_ROOT.mkdir(parents=True, exist_ok=True)
    name = datetime.now(timezone.utc).strftime("run-%Y%m%dT%H%M%SZ-") + uuid.uuid4().hex[:10]
    scratch = OUTPUT_ROOT / name
    scratch.mkdir(exist_ok=False)
    metrics = {"scene_calls":0,"scene_times_ms":{},"preview_times_ms":{}}
    result = {"candidate":str(exe),"candidate_sha256":hash_bytes(exe.read_bytes()),
              "workspace":"disposable isolated temp INKPAD_DATA_DIR",
              "scratch":str(scratch)}
    try:
        with tempfile.TemporaryDirectory(prefix="inkpad-structural-qa-") as workspace:
            client = MCP(exe, Path(workspace))
            try:
                num_tools, list_ms = inspect_schema(client)
                result["mcp_tools"] = num_tools
                metrics["list_ms"] = list_ms
                evenodd = render_scene(client,evenodd_scene(),"evenodd",scratch,metrics)
                evenodd_shadow = render_scene(client,evenodd_scene(shadow=True),
                                              "evenodd-shadow",scratch,metrics)
                single = render_scene(client,feather_scene(),"feather-one",scratch,metrics)
                # Two-feather and shadow variants are separate notes to provide
                # precise compositing controls with no mutation after export.
                top = render_scene(client,
                    {"layers":[{"name":"opaque underpaint"},{"name":"empty"},{"name":"covert"}],
                     "objects":[underpainting(),secondary_feather()]},
                    "feather-two-control",scratch,metrics)
                combo = render_scene(client,feather_scene(secondary=True),
                                     "feather-layered",scratch,metrics)
                shadow = render_scene(client,feather_scene(shadow=True),
                                      "feather-shadow",scratch,metrics)
                result["even_odd_pixels"] = check_evenodd(evenodd)
                result["even_odd_shadow_pixels"] = check_evenodd_shadow(evenodd_shadow)
                result["transformed_gradient_clip_pixels"] = check_transformed_cutouts(single)
                result["feather_overlap_oracle"] = check_crossing_feathers(single,top,combo)
                result["shadow_alpha_oracle"] = check_shadow(single,shadow)
                result["pngs"] = {key:{"sha256":val["sha256"],
                                        "bytes":val["png_bytes"],
                                        "nonwhite":val["nonwhite_pixels"],
                                        "objects":val["object_count"]}
                                  for key,val in (("evenodd",evenodd),
                                                  ("evenodd_shadow",evenodd_shadow),
                                                  ("main",single),
                                                  ("second_control",top),("layered",combo),
                                                  ("shadow",shadow))}
            finally:
                client.close()
        result["metrics"] = metrics
        result["status"]="PASS"
        write_new(scratch/"report.json",json.dumps(result,indent=2,ensure_ascii=False).encode("utf-8"))
        return result
    except Exception as exc:
        result["status"]="FAIL"
        result["error"]={"type":type(exc).__name__,"message":str(exc)}
        result["metrics"] = metrics
        write_new(scratch/"failure.json",json.dumps(result,indent=2,ensure_ascii=False).encode("utf-8"))
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe",type=Path,default=DEFAULT_EXE)
    opts=parser.parse_args()
    exe = opts.exe.expanduser().resolve(strict=True)
    print(json.dumps(run(exe),ensure_ascii=False),flush=True)


if __name__ == "__main__":
    main()
