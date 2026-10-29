"""Positioning / gradient / scene atomicity regression via actual Inkpad MCP stdio.

Examples (from the Inkpad repository):
    py -3 tests/advanced_vector_positioning_regression.py \
        target/advanced-vector-landmark-package-verify/Inkpad-Beta-Advanced-Vectors-20261018/inkpad-agent.exe
    py -3 tests/advanced_vector_positioning_regression.py PACKAGED_AGENT_EXE \
        --old-executable target/bench-baseline/inkpad-agent.exe

ONLY touches: isolated TemporaryDirectory INKPAD_DATA_DIR and newly named
artifacts under target/advanced-vector-positioning-qa-20261018. Never reads
or modifies actual Inkpad notes or application settings. Artifacts are
write-once, named with UTC timestamp + random token (no overwrite).
"""

import argparse
import base64
import copy
import hashlib
import json
from pathlib import Path
import statistics
import sys
import tempfile
import time
import uuid
from datetime import datetime, timezone

from benchmark_agent_mcp import MCP
from advanced_vector_mcp_smoke import png_pixels, call_expect_rejection

CANVAS = (1100, 700)
ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / "target" / "advanced-vector-positioning-qa-20261018"


def point(x, y):
    return {"x": x, "y": y}


def solid(rgb):
    return {"type": "solid", "color": [*rgb, 255]}


def gradient(kind, center):
    a = {"offset": 0, "color": [255, 240, 49, 255]}
    b = {"offset": 1, "color": [116, 34, 93, 255]}
    if kind == "elliptical_gradient":
        return {"type": kind, "center": center, "radius_x": 52,
                "radius_y": 52, "stops": [a, b]}
    return {"type": "radial_gradient", "center": center, "radius": 52,
            "stops": [a, b]}


def obj(shape, layer, fill, *, at, clip=None, shadow=None):
    item = {"shape": shape, "layer_index": layer,
            "transform": {"translation": point(*at)},
            "style": {"stroke": None, "stroke_width": 0, "opacity": 1,
                      "fill": fill}}
    if clip is not None:
        item["clip_shape"] = clip
    if shadow is not None:
        item["style"]["shadow"] = shadow
    return item


def sample_scene(*, with_clip, with_shadow, with_ellipse):
    """Four off-origin placements and deliberate layer-order inversion.

    The historical failure was confusing a gradient's center (local sample
    coordinates) with transform.translation (screen-space placement). Here
    every drawable explicitly specifies translation, even when gradient
    center is (0,0); no sun-sized circle should appear at canvas (0,0).
    """
    background = obj({"type": "rectangle", "width": 200, "height": 130}, 0,
                     solid([31, 149, 139]), at=(410, 310))
    circle_fill = gradient("elliptical_gradient" if with_ellipse else "radial_gradient", point(0, 0))
    circle = obj({"type": "circle", "radius": 52}, 1, circle_fill, at=(860, 220),
                 shadow={"offset": point(10, 10), "blur_radius": 8,
                         "color": [42, 44, 108, 215]} if with_shadow else None)
    rectangle = obj({"type": "rectangle", "width": 160, "height": 110}, 1,
                    {"type": "linear_gradient", "start": point(0, 0),
                     "end": point(160, 0), "stops": [
                         {"offset": 0, "color": [244, 45, 39, 255]},
                         {"offset": 1, "color": [37, 64, 241, 255]}]}, at=(150, 140))
    clip = {"type": "path", "commands": [
        {"type": "move_to", "to": point(45, 25)},
        {"type": "line_to", "to": point(140, 25)},
        {"type": "line_to", "to": point(140, 105)},
        {"type": "line_to", "to": point(45, 105)},
        {"type": "close"}]}
    top = obj({"type": "rectangle", "width": 200, "height": 130}, 2,
              solid([237, 53, 177]), at=(410, 310), clip=clip if with_clip else None)
    # Objects array deliberately puts the *lowest* layer last. Rendering
    # must still respect layer_index, never submission order.
    return {"layers": [{"name": "Base / underpainting"},
                       {"name": "Translated gradients"},
                       {"name": "Clipped glaze"}],
            "objects": [circle, rectangle, top, background]}


def require_features(tools, caps):
    necessary = ("create_note", "drawing_create_scene", "drawing_list",
                 "drawing_preview", "export_note", "get_capabilities")
    for name in necessary:
        if name not in tools:
            raise RuntimeError(f"Packaged agent lacks required MCP tool: {name}")
    schema = tools["drawing_create_scene"]["inputSchema"]
    props = schema["properties"]
    objects = props["objects"]
    # Newer Inkpad agents permit edits-only scenes with no new shapes.
    # Earlier agents require at least one creation. Both must still reject
    # an entirely empty scene via their runtime validator.
    assert objects.get("minItems", 0) in (0, 1) and objects["maxItems"] >= 4, objects
    if objects.get("minItems", 0) == 0:
        assert "edits" in props, "No minimum on objects requires edits-only support"
    assert props["layers"]["maxItems"] >= 3
    item = objects["items"]["properties"]
    assert {"shape", "transform", "style", "layer_index"}.issubset(item)
    variants = {x["properties"]["type"]["const"]
                for x in item["shape"]["oneOf"]}
    assert {"circle", "rectangle", "path"}.issubset(variants), variants
    model = caps["model"]
    features = {
        "clip_shape": bool(model.get("clip_shape")) and "clip_shape" in item,
        "shadow": "shadow" in model["style"] and "shadow" in item["style"]["properties"],
        "elliptical_gradient": "elliptical_gradient" in model["style"],
    }
    if not all(features.values()):
        raise RuntimeError(f"Packaged advanced agent is missing regression features: {features}")
    return features, schema


def content(reply):
    return reply["result"]["structuredContent"]


def rgba_png(reply):
    result = content(reply)
    assert (result["width"], result["height"]) == CANVAS
    image_block = next(item for item in reply["result"]["content"]
                       if item["type"] == "image")
    assert image_block["mimeType"] == "image/png"
    raw = base64.b64decode(image_block["data"], validate=True)
    assert raw[:8] == b"\x89PNG\r\n\x1a\n"
    return raw


def within(actual, expected, tolerance, explanation):
    deviation = max(abs(a-b) for a,b in zip(actual, expected))
    assert deviation <= tolerance, f"{explanation}: actual {actual} expected {expected} ±{tolerance}"


def verify_pixels(pixel):
    """Assert real material colours at world locations and white at origin."""
    samples = {name: pixel(*xy) for name,xy in {
        "canvas_origin": (8, 8),
        "circle_legacy_origin": (47, 48),
        "circle_center": (860, 220),
        "circle_edge_interior": (895, 220),
        "circle_shadow_side": (920, 226),
        "rectangle_red": (164, 185),
        "rectangle_blue": (296, 185),
        "rectangle_missing_origin": (72, 52),
        "clip_inside": (470, 355),
        "clip_outside_on_base": (423, 335),
        "outside_base": (635, 374)}.items()}
    for sample in ("canvas_origin", "circle_legacy_origin", "rectangle_missing_origin", "outside_base"):
        within(samples[sample], (255,255,255), 0, f"unexpected paint near {sample}")
    # At local origin the circle's elliptical gradient must start yellow.
    within(samples["circle_center"], (255,240,49), 5,
           "circle translated center / local gradient origin mismatch")
    edge = samples["circle_edge_interior"]
    assert edge[1] < 200 and edge[2] > 50, f"circle gradient did not darken: {edge}"
    assert samples["rectangle_red"][0] > 180 and samples["rectangle_red"][2] < 115, samples
    assert samples["rectangle_blue"][2] > 175 and samples["rectangle_blue"][0] < 140, samples
    within(samples["clip_inside"], (237,53,177), 5,
           "top clipped magenta not visible over base (layer order or clipping)")
    within(samples["clip_outside_on_base"], (31,149,139), 5,
           "clip shape leaked into base underlay or layer order was ignored")
    shadow = samples["circle_shadow_side"]
    assert shadow != (255,255,255) and min(shadow) < 248, f"soft shadow missing: {shadow}"
    return samples


def save_new(folder, name, data):
    dest = folder / name
    # Exclusive creation prevents any accidental overwrite of previous QA.
    with dest.open("xb") as handle:
        handle.write(data)
    return dest


def rejection(client, data, folder, note_id, tag):
    before = (folder / "workspace.json").read_bytes()
    response = call_expect_rejection(client, "drawing_create_scene", data)
    after = (folder / "workspace.json").read_bytes()
    assert before == after, f"Rejected {tag} corrupted saved workspace"
    return {"case": tag,
            "message": (response.get("error", {}).get("message") or
                        str(response.get("result", {}).get("content", "")))[:300],
            "workspace_sha256": hashlib.sha256(after).hexdigest()}


def verify_legacy(exe, artifacts, label):
    """Optional legacy raster check for two existing shapes only."""
    with tempfile.TemporaryDirectory(prefix="inkpad-positioning-legacy-") as tmp:
        client = MCP(exe, Path(tmp))
        try:
            found, _ = client.call("tools/list", {})
            tool_names = {tool["name"] for tool in found["result"]["tools"]}
            if not {"drawing_create_shape", "drawing_preview"}.issubset(tool_names):
                return {"skipped": True, "reason": "old binary predates editable shape tools"}
            note, _ = client.tool("create_note", {"title": "Isolated legacy raster"})
            note_id = content(note)["id"]
            # Identical shapes and transforms on the old and new renderers.
            objects = [
                obj({"type": "rectangle", "width": 160, "height": 110}, 0,
                    {"type": "linear_gradient", "start": point(0,0), "end": point(160,0),
                     "stops": [{"offset": 0, "color": [244,45,39,255]},
                               {"offset": 1, "color": [37,64,241,255]}]},at=(150,140)),
                obj({"type": "circle", "radius": 52}, 0,
                    {"type": "radial_gradient", "center": point(0,0), "radius": 52,
                     "stops": [{"offset": 0, "color": [255,240,49,255]},
                               {"offset": 1, "color": [116,34,93,255]}]},at=(860,220))]
            total = 0.0
            for item in objects:
                shape = {k: v for k,v in item.items() if k != "layer_index"}
                _, elapsed = client.tool("drawing_create_shape", {"id": note_id, **shape})
                total += elapsed
            preview, preview_ms = client.tool("drawing_preview", {"id": note_id})
            png = rgba_png(preview)
            filename = f"{label}-legacy-shapes.png"
            save_new(artifacts, filename, png)
            _, pixel = png_pixels(png)
            within(pixel(8,8), (255,255,255),0,"legacy raster painted at origin")
            return {"png_sha256": hashlib.sha256(png).hexdigest(),
                    "png_bytes":len(png),"create_ms":round(total,2),
                    "preview_ms":round(preview_ms,2)}
        finally:
            client.close()


def verify_position_shorthand(client, tools, folder, artifacts):
    """Exercise the new geometric placement API in scene/batch/single/edit.

    Older packaged agents remain valid test baselines. Detect new schemas,
    never send new arguments to older binaries, and state the exact exclusion.
    """
    names = ("drawing_create_scene", "drawing_create_shape",
             "drawing_create_batch", "drawing_edit_shape")
    absent = []
    for name in names:
        spec = tools[name]["inputSchema"]["properties"]
        shape_props = spec["objects"]["items"]["properties"] if name in (
            "drawing_create_scene", "drawing_create_batch") else spec
        if "position" not in shape_props:
            absent.append(name)
    if absent:
        return {"supported":False,"skipped":True,
                "reason":"pre-position API packaged binary", "missing_from":absent}

    note,_ = client.tool("create_note",{"title":"Explicit sun and lake positioning / 云"})
    id_ = content(note)["id"]
    sun = {"layer_index":1,"shape":{"type":"circle","radius":68},
           "position":point(855,238),
           "style":{"stroke":None,"stroke_width":0,
                    "fill":{"type":"radial_gradient","center":point(0,0),
                            "radius":68,"stops":[
                                {"offset":0,"color":[251,225,64,255]},
                                {"offset":1,"color":[224,90,49,255]}]}}}
    lake = {"layer_index":0,"shape":{"type":"rectangle","width":1100,"height":200},
            "position":point(0,382),
            "style":{"stroke":None,"stroke_width":0,
                     "fill":{"type":"linear_gradient", "start":point(0,0),
                             "end":point(0,200),"stops":[
                                {"offset":0,"color":[94,183,209,255]},
                                {"offset":1,"color":[33,72,136,255]}]}}}
    # Backward compatible omission of position/transform: local path geometry
    # already includes its intended on-canvas coordinates.
    badge = {"layer_index":1,"shape":{"type":"path","commands":[
                {"type":"move_to","to":point(325,110)},
                {"type":"line_to","to":point(392,110)},
                {"type":"line_to","to":point(392,165)},
                {"type":"line_to","to":point(325,165)},
                {"type":"close"}]},
             "style":{"stroke":None,"stroke_width":0,"fill":solid([62,194,83])}}
    created, scene_ms = client.tool("drawing_create_scene",{
        "id":id_,"expected_object_count":0,
        "layers":[{"name":"Water"},{"name":"Sun and local geometry"}],
        "objects":[sun,lake,badge]})
    scene = content(created)
    assert scene["added_objects"] == 3 and scene["added_layers"] == 2,scene
    listing,_ = client.tool("drawing_list",{"id":id_})
    objects = content(listing)["drawing"]["objects"]
    assert [o["transform"]["translation"] for o in objects] == [
        point(855,238),point(0,382),point(0,0)]
    assert objects[0]["style"]["fill"]["center"] == point(0,0)
    assert objects[1]["style"]["fill"]["start"] == point(0,0)

    batch,_batch_ms = client.tool("drawing_create_batch",{
        "id":id_,"expected_object_count":3,"objects":[{
            "shape":{"type":"circle","radius":16},"position":point(520,125),
            "style":{"stroke":None,"fill":solid([236,72,143])}}]})
    assert content(batch)["object_count"] == 4
    single,_single_ms = client.tool("drawing_create_shape",{
        "id":id_,"shape":{"type":"rectangle","width":90,"height":60},
        "position":point(120,440),"style":{"stroke":None,"fill":solid([53,198,60])}})
    oid = content(single)["object"]["id"]
    assert content(single)["object_count"] == 5
    edited,_edit_ms = client.tool("drawing_edit_shape",{
        "id":id_,"object_id":oid,"position":point(160,475)})
    assert content(edited)["object"]["transform"]["translation"] == point(160,475)
    assert content(edited)["object"]["style"]["fill"] == solid([53,198,60])

    rollback = []
    conflicting_scene = {"id":id_,"expected_object_count":5,
        "layers":[{"name":"should never commit"}],
        "objects":[{"layer_index":0,"shape":{"type":"circle","radius":4},
                    "position":point(20,20)},
                   {"layer_index":0,"shape":{"type":"circle","radius":5},
                    "position":point(70,80),"transform":{"translation":point(50,60)}}]}
    rollback.append(rejection(client,conflicting_scene,folder,id_,"scene_position_transform_conflict"))
    out_of_range = copy.deepcopy(conflicting_scene)
    out_of_range["objects"][1].pop("transform")
    out_of_range["objects"][1]["position"] = point(19999,72)
    rollback.append(rejection(client,out_of_range,folder,id_,"scene_position_out_of_range"))

    def reject_other_tool(name, arguments, tag):
        before = (folder/"workspace.json").read_bytes()
        reply=call_expect_rejection(client,name,arguments)
        assert (folder/"workspace.json").read_bytes()==before, f"{tag} modified workspace"
        return {"case":tag,"rejected":True,
                "message":str(reply.get("result",{}).get("content","") or reply.get("error",{}))[:240]}

    rollback.append(reject_other_tool("drawing_create_batch",{
        "id":id_,"objects":[{"shape":{"type":"circle","radius":4},
                              "position":point(20,20)},
                             {"shape":{"type":"rectangle","width":10,"height":10},
                              "position":point(70,80),"transform":{"translation":point(50,60)}}]},
        "batch_position_transform_conflict"))
    rollback.append(reject_other_tool("drawing_create_shape",{
        "id":id_,"shape":{"type":"circle","radius":8},
        "position":point(800,230),"transform":{"translation":point(850,240)}},
        "single_position_transform_conflict"))
    rollback.append(reject_other_tool("drawing_edit_shape",{
        "id":id_,"object_id":oid,"position":point(850,230),
        "transform":{"translation":point(820,210)}},"edit_position_transform_conflict"))

    after,_=client.tool("drawing_list",{"id":id_})
    assert content(after)["object_count"]==5
    preview,preview_ms=client.tool("drawing_preview",{"id":id_})
    png=rgba_png(preview)
    nonwhite,pixel=png_pixels(png)
    samples={key:pixel(*coords) for key,coords in {
        "canvas_origin":(8,8),"former_sun_origin":(40,40),
        "sun_center":(855,238),"sun_edge":(903,238),
        "lake_start":(15,390),"lake_bottom":(15,554),
        "above_lake":(15,365),"legacy_path":(355,134),
        "batch_shape":(520,125),"edited_single":(175,490),
        "former_single":(130,455)}.items()}
    for key in ("canvas_origin","former_sun_origin","above_lake"):
        within(samples[key],(255,255,255),0,key)
    within(samples["sun_center"],(251,225,64),5,"position sun at 855,238 with local radial gradient")
    assert samples["sun_edge"][1] < samples["sun_center"][1]-45,samples
    assert samples["lake_start"][2] > samples["lake_bottom"][2]+20,samples
    assert samples["lake_start"][1] > samples["lake_bottom"][1]+45,samples
    within(samples["legacy_path"],(62,194,83),5,"absent position path regression")
    within(samples["batch_shape"],(236,72,143),5,"batch position regression")
    within(samples["edited_single"],(53,198,60),5,"edit position regression")
    assert samples["former_single"][1] < 198, "old single position was not cleared"
    save_new(artifacts,"position-shorthand-preview.png",png)
    dest=artifacts/"position-shorthand-export.png"
    client.tool("export_note",{"id":id_,"format":"png","path":str(dest.resolve())})
    assert dest.read_bytes()==png
    return {"supported":True,"tested_tools":list(names),"rollback":rollback,
        "objects":5,"layers":2,"samples":{key:list(value) for key,value in samples.items()},
        "png_sha256":hashlib.sha256(png).hexdigest(),"png_bytes":len(png),
        "nonwhite_pixels":nonwhite,
        "timings_ms":{"scene":round(scene_ms,2),"batch":round(_batch_ms,2),
                      "single":round(_single_ms,2),"edit":round(_edit_ms,2),
                      "preview":round(preview_ms,2)}}


def test_packaged(exe, artifacts):
    with tempfile.TemporaryDirectory(prefix="inkpad-positioning-regression-") as tmp:
        folder = Path(tmp)
        client = MCP(exe, folder)
        try:
            discovery, list_ms = client.call("tools/list", {})
            tools = {t["name"]: t for t in discovery["result"]["tools"]}
            cap_result, cap_ms = client.tool("get_capabilities")
            caps = content(cap_result)
            assert (caps["canvas"]["width"], caps["canvas"]["height"]) == CANVAS
            features, schema = require_features(tools, caps)
            note, note_ms = client.tool("create_note", {
                "title": "Positioning gradient regression · 雲 / 🔴",
                "body": "Isolated regression fixture; real notes untouched"})
            note_id = content(note)["id"]
            fixture = sample_scene(with_clip=True, with_shadow=True, with_ellipse=True)
            request = {"id": note_id, "expected_object_count": 0, **fixture}
            response, create_ms = client.tool("drawing_create_scene", request)
            result = content(response)
            assert result["added_objects"] == 4 and result["added_layers"] == 3, result
            assert len(set(result["object_ids"])) == 4
            listed, read_ms = client.tool("drawing_list", {"id": note_id})
            state = content(listed)
            assert (state["object_count"],state["layer_count"]) == (4,3)
            assert [o["id"] for o in state["drawing"]["objects"]] == result["object_ids"]
            assert [o["transform"]["translation"] for o in state["drawing"]["objects"]] == [
                o["transform"]["translation"] for o in fixture["objects"]]
            assert state["drawing"]["objects"][0]["style"]["fill"]["center"] == point(0,0)
            assert state["drawing"]["objects"][0]["style"]["fill"]["type"] == "elliptical_gradient"
            assert state["drawing"]["objects"][2]["clip_shape"]["type"] == "path"
            assert state["drawing"]["objects"][0]["style"]["shadow"] is not None

            errors = []
            # Malformed second operation; if the other two inserted first the
            # batch still must not write anything.
            malformed = {"id": note_id, "layers": [{"name": "should not exist"}],
                         "objects": [{"layer_index":0,"shape":{"type":"circle","radius":9}},
                                     {"layer_index":0,"shape":{"type":"rectangle","width":-3,"height":20}},
                                     {"layer_index":0,"shape":{"type":"circle","radius":12}}]}
            errors.append(rejection(client, malformed,folder,note_id,"invalid_middle_shape"))
            # Stale expected count before any write must also leave byte-identical workspace.
            stale = {"id": note_id,"expected_object_count":0,
                     "layers": [{"name": "stale-layer"}],
                     "objects": [{"layer_index":0,"shape":{"type":"circle","radius":9}}]}
            errors.append(rejection(client, stale,folder,note_id,"stale_expected_object_count"))
            # Validated geometry but layer does not exist -> reject during the
            # transaction after earlier layer/object candidates, still atomic.
            late = {"id":note_id, "expected_object_count":4,
                    "layers":[{"name":"not-committed"}],
                    "objects":[{"layer_index":0,"shape":{"type":"circle","radius":12}},
                               {"layer_id":"does-not-exist","shape":{"type":"circle","radius":8}}]}
            errors.append(rejection(client,late,folder,note_id,"invalid_late_layer_reference"))

            preview, first_ms = client.tool("drawing_preview", {"id": note_id})
            png = rgba_png(preview)
            assert content(preview)["object_count"] == 4
            nonwhite,pixel = png_pixels(png)
            samples = verify_pixels(pixel)
            assert nonwhite > 20000,nonwhite
            preview_path = save_new(artifacts,"scene-positioned-preview.png",png)
            export_path = artifacts/"scene-positioned-export.png"
            export_result, export_ms = client.tool("export_note",{
                "id":note_id,"format":"png","path":str(export_path.resolve())})
            exported = export_path.read_bytes()
            assert exported.startswith(b"\x89PNG\r\n\x1a\n")
            assert exported == png, "PNG export differs from MCP preview in unchanged scene"
            _ = content(export_result)
            repeat = []
            for _ in range(3):
                cached,_ms = client.tool("drawing_preview", {"id":note_id})
                repeat.append(_ms)
                assert rgba_png(cached) == png, "preview changed without edits"

            # A separate note containing the exact legacy circle/rectangle
            # geometry gives byte-stable comparison across old/new renderers.
            legacy_result = verify_legacy(exe,artifacts,"packaged")
            position_api = verify_position_shorthand(client,tools,folder,artifacts)
            manifest = {
                "status":"PASS", "tool_count":len(tools),"capabilities":features,
                "exe":str(exe),"exe_sha256":hashlib.sha256(exe.read_bytes()).hexdigest(),
                "scene_objects":4,"scene_layers":3,"atomic_rejections":errors,
                "pixels":{k:list(v) for k,v in samples.items()},
                "nonwhite_pixels":nonwhite,
                "preview_sha256":hashlib.sha256(png).hexdigest(),
                "export_sha256":hashlib.sha256(exported).hexdigest(),
                "png_bytes":len(png),"png_size":list(CANVAS),
                "timings_ms":{"tools_list":round(list_ms,2),"capabilities":round(cap_ms,2),
                              "note_create":round(note_ms,2),"scene_create":round(create_ms,2),
                              "drawing_list":round(read_ms,2),"preview_first":round(first_ms,2),
                              "preview_cache_median":round(statistics.median(repeat),2),
                              "export":round(export_ms,2)},
                "legacy_shapes":legacy_result,"position_api":position_api,
                "artifacts":[preview_path.name,export_path.name,"packaged-legacy-shapes.png"]}
            return manifest
        finally:
            client.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable",type=Path,help="Advanced packaged Inkpad MCP executable")
    parser.add_argument("--old-executable",type=Path,help="Optional existing legacy Inkpad MCP agent for raster comparison")
    args = parser.parse_args()
    exe = args.executable.expanduser().resolve(strict=True)
    old = args.old_executable.expanduser().resolve(strict=True) if args.old_executable else None
    if not exe.is_file():
        raise ValueError(f"Not a file: {exe}")
    ARTIFACTS.mkdir(parents=True,exist_ok=True)
    run_name = datetime.now(timezone.utc).strftime("positioning-%Y%m%dT%H%M%SZ-") + uuid.uuid4().hex[:10]
    run = ARTIFACTS/run_name
    run.mkdir(exist_ok=False)
    # Every attempt retains new artifacts and an explicit PASS/FAIL result.
    try:
        result = test_packaged(exe,run)
        if old is not None:
            result["old_exe"] = str(old)
            result["old_legacy_shapes"] = verify_legacy(old,run,"old")
            if "png_sha256" in result["old_legacy_shapes"]:
                result["legacy_png_pixel_identical"] = (result["legacy_shapes"]["png_sha256"]
                                                       == result["old_legacy_shapes"]["png_sha256"])
        save_new(run,"report.json",json.dumps(result,indent=2,ensure_ascii=False).encode("utf-8"))
        print(json.dumps({"artifact_dir":str(run), **result},ensure_ascii=False),flush=True)
    except Exception as err:
        result = {"status":"FAIL","exception":type(err).__name__,"message":str(err),
                  "binary":str(exe)}
        save_new(run,"failure.json",json.dumps(result,indent=2).encode("utf-8"))
        print(json.dumps({"artifact_dir":str(run), **result}),file=sys.stderr,flush=True)
        raise


if __name__ == "__main__":
    main()
