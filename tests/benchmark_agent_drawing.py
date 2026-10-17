"""Synthetic end-to-end MCP vector-art benchmark with zero access to real notes.

Usage: py -3 tests/benchmark_agent_drawing.py BASELINE_AGENT_EXE NEW_AGENT_EXE

Measures the wall time of 24 individual shape tools vs one vector batch,
plus preview render time and PNG integrity. Reports machine-specific numbers.
"""

import base64
import hashlib
import json
from pathlib import Path
import statistics
import sys
import tempfile

from benchmark_agent_mcp import MCP


def scene():
    """Identical geometry, gradients, fills and draw order for all builds."""
    result = []
    for i in range(24):
        x = 38 + (i % 8) * 131
        y = 32 + (i // 8) * 206
        c = [40 + (i * 19) % 180, 60 + (i * 37) % 160, 65 + (i * 17) % 150, 255]
        if i % 4 == 0:
            shape = {"type": "circle", "radius": 42}
            fill = {"type": "radial_gradient", "center": {"x": 0, "y": 0},
                    "radius": 55, "stops": [
                        {"offset": 0, "color": [255, 255, 255, 255]},
                        {"offset": 1, "color": c}]}
        elif i % 4 == 1:
            shape = {"type": "rounded_rectangle", "width": 106, "height": 125, "radius": 22}
            fill = {"type": "linear_gradient", "start": {"x": 0, "y": 0},
                    "end": {"x": 106, "y": 125}, "stops": [
                        {"offset": 0, "color": c},
                        {"offset": 1, "color": [245, 248, 255, 255]}]}
        elif i % 4 == 2:
            shape = {"type": "ellipse", "radius_x": 49, "radius_y": 65}
            fill = {"type": "solid", "color": c}
        else:
            shape = {"type": "polygon", "points": [
                {"x": 0, "y": 0}, {"x": 105, "y": 20},
                {"x": 70, "y": 122}, {"x": -15, "y": 90}]}
            fill = {"type": "solid", "color": c}
        result.append({"shape": shape, "style": {
            "fill": fill, "stroke": [22, 38, 62, 255], "stroke_width": 2,
            "opacity": 1.0},
            "transform": {"translation": {"x": x, "y": y}}})
    return result


def trial(exe, output_png=None):
    with tempfile.TemporaryDirectory(prefix="inkpad-ai-art-bench-") as folder:
        mcp = MCP(exe, Path(folder))
        try:
            reply, _ = mcp.tool("create_note", {"title": "Benchmark painting", "body": "🐈 art"})
            note_id = reply["result"]["structuredContent"]["id"]
            shapes = scene()
            names, _ = mcp.call("tools/list", {})
            has_batch = any(tool["name"] == "drawing_create_batch" for tool in names["result"]["tools"])
            sequential_ms = 0.0
            if has_batch:
                # Keep the artifact comparable: the batch creates all 24 objects,
                # in precisely the same order as the pre-upgrade one-by-one API.
                response, creation_ms = mcp.tool("drawing_create_batch", {
                    "id": note_id, "objects": shapes})
                assert len(response["result"]["structuredContent"]["object_ids"]) == len(shapes)
                write_mode = "batch"
            else:
                for spec in shapes:
                    _, elapsed = mcp.tool("drawing_create_shape", {"id": note_id, **spec})
                    sequential_ms += elapsed
                creation_ms = sequential_ms
                write_mode = "sequential"
            preview, render_ms = mcp.tool("drawing_preview", {"id": note_id})
            img = preview["result"]["structuredContent"]
            image_content = preview["result"]["content"][0]
            assert image_content["type"] == "image" and image_content["mimeType"] == "image/png"
            png = base64.b64decode(image_content["data"], validate=True)
            assert png.startswith(b"\x89PNG\r\n\x1a\n")
            assert img["width"] == 1100 and img["height"] == 700
            assert img["object_count"] == len(shapes)
            if output_png is not None:
                output_png.parent.mkdir(parents=True, exist_ok=True)
                output_png.write_bytes(png)
            # Several repeat measurements isolate rendering cost from disk writes.
            repeated = [mcp.tool("drawing_preview", {"id": note_id})[1] for _ in range(4)]
            return {
                "path": str(exe), "creation": write_mode,
                "create_24_ms": round(creation_ms, 2),
                "preview_first_ms": round(render_ms, 2),
                "preview_repeat_median_ms": round(statistics.median(repeated), 2),
                "png_sha256": hashlib.sha256(png).hexdigest(),
                "png_bytes": len(png), "objects": len(shapes),
            }
        finally:
            mcp.close()


if __name__ == "__main__":
    if len(sys.argv) not in (3, 4):
        raise SystemExit(__doc__)
    out_folder = Path(sys.argv[3]).resolve() if len(sys.argv) == 4 else None
    for name, arg in (("baseline", sys.argv[1]), ("candidate", sys.argv[2])):
        path = Path(arg).resolve(strict=True)
        output_png = out_folder / f"{name}.png" if out_folder else None
        print(json.dumps({"run": name, **trial(path, output_png)}), flush=True)
