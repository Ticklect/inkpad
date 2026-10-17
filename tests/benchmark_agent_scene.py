"""Benchmark drawing a multi-layer AI scene over MCP (no real notes touched).

Usage: py -3 tests/benchmark_agent_scene.py OLD_60_TOOL_EXE NEW_SCENE_EXE

Both agents receive exactly the same 512 shapes across eight editable layers.
The old agent uses eight layer calls and eight 64-shape batch calls; the new
agent uses one drawing_create_scene. Both PNGs must have identical bytes.
"""

import base64
import hashlib
import json
from pathlib import Path
import random
import sys
import tempfile

from benchmark_agent_mcp import MCP


def build_scene():
    rand = random.Random(170_061)
    names = ["Sky", "Mountains", "Lake", "Shadows", "Body", "Head", "Reeds", "Highlights"]
    objects = []
    for layer_index in range(8):
        for index in range(64):
            r = 7 + rand.randrange(25)
            x = rand.randrange(1095)
            y = rand.randrange(690)
            c = [30 + rand.randrange(190), 35 + rand.randrange(165), 40 + rand.randrange(160), 210]
            variant = index % 4
            if variant == 0:
                shape = {"type":"circle","radius":r}
            elif variant == 1:
                shape = {"type":"ellipse","radius_x":r+17,"radius_y":r}
            elif variant == 2:
                shape = {"type":"rounded_rectangle","width":r*4,"height":r*2,"radius":r//3}
            else:
                shape = {"type":"polygon","points":[
                    {"x":0,"y":0},{"x":r*3,"y":-r//2},{"x":r*4,"y":r*2},
                    {"x":0,"y":r*3}]}
            fill = {"type":"solid","color":c}
            if index % 13 == 0:
                fill = {"type":"linear_gradient", "start":{"x":0,"y":0},
                        "end":{"x":r*4,"y":r*2},"stops":[
                            {"offset":0,"color":[255,240,210,190]},
                            {"offset":1,"color":c}]}
            objects.append({"shape":shape,"layer_index":layer_index,
                "style":{"stroke":None,"fill":fill,"stroke_width":0},
                "transform":{"translation":{"x":x,"y":y}}})
    return names, objects


def trial(exe):
    names, objects = build_scene()
    with tempfile.TemporaryDirectory(prefix="inkpad-scene-mcp-") as folder:
        c = MCP(exe, Path(folder))
        try:
            created, _ = c.tool("create_note", {"title":"Synthetic 512-object painting", "body":"Keep Unicode: 🦆"})
            note_id = created["result"]["structuredContent"]["id"]
            catalog, _ = c.call("tools/list", {})
            tools = {t["name"] for t in catalog["result"]["tools"]}
            elapsed = 0.0
            tool_calls = 0
            if "drawing_create_scene" in tools:
                result, ms = c.tool("drawing_create_scene", {"id":note_id,"expected_object_count":0,
                    "layers":[{"name":name} for name in names],"objects":objects})
                assert result["result"]["structuredContent"]["added_objects"] == 512
                elapsed += ms
                tool_calls += 1
                mode = "scene"
            else:
                layer_ids = []
                for name in names:
                    reply, ms = c.tool("drawing_create_layer", {"id":note_id,"name":name})
                    layer_ids.append(reply["result"]["structuredContent"]["layer_id"])
                    elapsed += ms
                    tool_calls += 1
                for layer in range(8):
                    group = [{**o, "layer_id":layer_ids[layer], "layer_index":None} for o in objects[layer*64:(layer+1)*64]]
                    for obj in group: obj.pop("layer_index",None)
                    reply, ms = c.tool("drawing_create_batch", {"id":note_id, "objects":group})
                    assert reply["result"]["structuredContent"]["added_objects"] == 64
                    elapsed += ms
                    tool_calls += 1
                mode = "batches"
            preview, render_ms = c.tool("drawing_preview", {"id":note_id})
            payload = preview["result"]["content"][0]
            assert payload["type"] == "image"
            png = base64.b64decode(payload["data"])
            assert preview["result"]["structuredContent"]["object_count"] == 512
            return {"mode":mode,"tool_count":len(tools),"creation_calls":tool_calls,
                "creation_ms":round(elapsed,2),"preview_ms":round(render_ms,2),
                "png_sha256":hashlib.sha256(png).hexdigest(),"png_bytes":len(png)}
        finally:
            c.close()


if __name__ == "__main__":
    if len(sys.argv) != 3: raise SystemExit(__doc__)
    outcomes = []
    for name, path in (("old",sys.argv[1]),("new",sys.argv[2])):
        result = trial(Path(path).resolve(strict=True))
        outcomes.append(result)
        print(json.dumps({"run":name,**result}),flush=True)
    assert outcomes[0]["png_sha256"] == outcomes[1]["png_sha256"], "Scene PNG changed"
    print("PASS: both 512-object scene previews are byte-for-byte identical",flush=True)
