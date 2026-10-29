"""Reproducible internal MCP scene performance benchmark, without touching user notes.

Usage: python tests/benchmark_scene_scaling.py <inkpad-agent.exe> [repetitions]
Create 24, 100, 256 and 512 objects with the same 8-layer seeded artwork.
Each trial creates its own temporary INKPAD_DATA_DIR and agent stdio process.
"""

import hashlib
import json
from pathlib import Path
import statistics
import sys
import tempfile

from benchmark_agent_mcp import MCP
from benchmark_agent_scene import build_scene
try:
    import psutil
except ImportError:
    psutil = None


def sample_scene(count):
    names, objects = build_scene()
    # Interleave layers rather than taking the first N objects from one layer.
    indexes = [(offset % 8) * 64 + offset // 8 for offset in range(count)]
    return [{"name": name} for name in names], [objects[index] for index in indexes]


def trial(exe, count):
    layers, objects = sample_scene(count)
    with tempfile.TemporaryDirectory(prefix="inkpad-scene-scaling-") as folder:
        client = MCP(exe, Path(folder))
        try:
            process = psutil.Process(client.proc.pid) if psutil else None
            cpu_ms = lambda: 1000 * sum(process.cpu_times()[:2]) if process else 0.0
            created, _ = client.tool("create_note", {"title": "Synthetic profile", "body": "Unicode 🦆"})
            note_id = created["result"]["structuredContent"]["id"]
            cpu_before = cpu_ms()
            response, create_ms = client.tool("drawing_create_scene", {
                "id": note_id, "expected_object_count": 0,
                "layers": layers, "objects": objects,
            })
            create_cpu_ms = cpu_ms() - cpu_before
            create_rss_mib = process.memory_info().rss / (1024 ** 2) if process else 0
            payload = response["result"]["structuredContent"]
            assert payload["added_objects"] == count
            assert payload["added_layers"] == len(layers)
            cpu_before = cpu_ms()
            first, preview_ms = client.tool("drawing_preview", {"id": note_id})
            preview_cpu_ms = cpu_ms() - cpu_before
            preview_rss_mib = process.memory_info().rss / (1024 ** 2) if process else 0
            second, cached_preview_ms = client.tool("drawing_preview", {"id": note_id})
            a = first["result"]["content"][0]["data"]
            b = second["result"]["content"][0]["data"]
            assert a == b
            # Check that one undo reverses the entire scene, then redo restores it.
            _, undo_ms = client.tool("drawing_undo", {"id": note_id, "confirm": True})
            reverted, _ = client.tool("drawing_list", {"id": note_id})
            assert len(reverted["result"]["structuredContent"]["drawing"].get("objects", [])) == 0
            _, redo_ms = client.tool("drawing_redo", {"id": note_id, "confirm": True})
            restored, _ = client.tool("drawing_preview", {"id": note_id})
            assert restored["result"]["content"][0]["data"] == a
            return {"create_ms": create_ms, "preview_ms": preview_ms,
                    "create_cpu_ms": create_cpu_ms, "preview_cpu_ms": preview_cpu_ms,
                    "create_rss_mib": create_rss_mib, "preview_rss_mib": preview_rss_mib,
                    "cached_preview_ms": cached_preview_ms,
                    "undo_ms": undo_ms, "redo_ms": redo_ms,
                    "png_sha256": hashlib.sha256(a.encode("ascii")).hexdigest()}
        finally:
            client.close()


def main():
    if len(sys.argv) not in (2, 3):
        raise SystemExit(__doc__)
    exe = Path(sys.argv[1]).resolve(strict=True)
    repetitions = int(sys.argv[2]) if len(sys.argv) == 3 else 5
    assert 1 <= repetitions <= 30
    for count in (24, 100, 256, 512):
        samples = [trial(exe, count) for _ in range(repetitions)]
        hashes = {sample["png_sha256"] for sample in samples}
        assert len(hashes) == 1, "Rendered scene varied between repetitions"
        report = {"object_count": count, "repetitions": repetitions,
                  "png_sha256_of_base64": hashes.pop(),
                  **{key: round(statistics.median(sample[key] for sample in samples), 3)
                     for key in ("create_ms", "create_cpu_ms", "create_rss_mib",
                                 "preview_ms", "preview_cpu_ms", "preview_rss_mib",
                                 "cached_preview_ms", "undo_ms", "redo_ms")}}
        print(json.dumps(report), flush=True)


if __name__ == "__main__":
    main()
