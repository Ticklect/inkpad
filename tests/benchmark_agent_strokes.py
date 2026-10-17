"""Compare Inkpad AI pen/eraser throughput and rendered ink with isolated notes.

Usage: py -3 tests/benchmark_agent_strokes.py BASELINE_EXE NEW_EXE [OUTPUT_FOLDER]
"""

import base64
import hashlib
import json
import math
from pathlib import Path
import statistics
import sys
import tempfile

from benchmark_agent_mcp import MCP


def paths():
    strokes = []
    for i in range(32):
        pts = []
        for j in range(96):
            x = 16 + j * 11
            y = int(80 + i * 16 + math.sin(j * 0.12 + i * 0.21) * (13 + i % 5))
            pts.append([x, y])
        palette = ("#425AA8", "#A95374", "#478A79", "#A46A34")
        strokes.append({"points": pts, "width": 3 + i % 6,
                        "color": palette[i % len(palette)]})
    return strokes


def trial(exe, output_png=None):
    with tempfile.TemporaryDirectory(prefix="inkpad-ai-pen-bench-") as folder:
        client = MCP(exe, Path(folder))
        try:
            created, _ = client.tool("create_note", {"title": "Synthetic lines", "body": ""})
            note_id = created["result"]["structuredContent"]["id"]
            discovered, _ = client.call("tools/list", {})
            names = {t["name"] for t in discovered["result"]["tools"]}
            strokes = paths()
            if "add_strokes_batch" in names:
                response, draw_ms = client.tool("add_strokes_batch", {"id": note_id, "strokes": strokes})
                mode = "batch"
                assert response["result"]["structuredContent"].get("stroke_count") == len(strokes)
            else:
                mode = "sequential"
                draw_ms = 0
                for stroke in strokes:
                    _, ms = client.tool("add_stroke", {"id": note_id, **stroke})
                    draw_ms += ms
            preview, first_ms = client.tool("drawing_preview", {"id": note_id})
            content = preview["result"]["content"][0]
            assert content["mimeType"] == "image/png"
            png = base64.b64decode(content["data"], validate=True)
            meta = preview["result"]["structuredContent"]
            assert meta["legacy_stroke_count"] == len(strokes)
            if output_png is not None:
                output_png.parent.mkdir(parents=True, exist_ok=True)
                output_png.write_bytes(png)
            repeats = [client.tool("drawing_preview", {"id": note_id})[1] for _ in range(4)]
            return {"create_32_ms": round(draw_ms, 2), "creation": mode,
                    "preview_first_ms": round(first_ms, 2),
                    "preview_repeat_median_ms": round(statistics.median(repeats), 2),
                    "png_sha256": hashlib.sha256(png).hexdigest(),
                    "png_bytes": len(png)}
        finally:
            client.close()


if __name__ == "__main__":
    if len(sys.argv) not in (3, 4):
        raise SystemExit(__doc__)
    output = Path(sys.argv[3]).resolve() if len(sys.argv) == 4 else None
    for name, exe in (("baseline", sys.argv[1]), ("candidate", sys.argv[2])):
        png_path = output / f"strokes-{name}.png" if output else None
        print(json.dumps({"run": name, **trial(Path(exe).resolve(strict=True), png_path)}), flush=True)
