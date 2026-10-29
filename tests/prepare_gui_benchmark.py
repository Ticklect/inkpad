"""Build a disposable dense GUI artwork workspace; never accesses real notes.

Usage: python tests/prepare_gui_benchmark.py INKPAD_AGENT_EXE TARGET_DIRECTORY [OBJECTS]
The caller must set INKPAD_DATA_DIR to TARGET_DIRECTORY when launching Inkpad.exe.
"""
import json
import os
from pathlib import Path
import sys
from benchmark_agent_mcp import MCP
from benchmark_agent_scene import build_scene


def main():
    if len(sys.argv) not in (3, 4):
        raise SystemExit(__doc__)
    exe = Path(sys.argv[1]).resolve(strict=True)
    folder = Path(sys.argv[2]).resolve()
    count = int(sys.argv[3]) if len(sys.argv) == 4 else 256
    if count not in (24, 100, 256, 512):
        raise ValueError("scene must have 24, 100, 256 or 512 objects")
    if folder.exists() and any(folder.iterdir()):
        raise FileExistsError("target folder must be empty, to protect user content")
    folder.mkdir(parents=True, exist_ok=True)
    names, scene_objects = build_scene()
    client = MCP(exe, folder)
    try:
        note, _ = client.tool("create_note", {
            "title": f"Disposable GUI performance test — {count} objects",
            "body": "All contents are synthetic; select Draw to move the red target.",
        })
        id_ = note["result"]["structuredContent"]["id"]
        selection = [(offset % 8) * 64 + offset // 8 for offset in range(count - 1)]
        objects = [scene_objects[index] for index in selection]
        # A dedicated TOPMOST layer is essential for a pointer benchmark. The
        # previous red target shared layer 7 with artwork which could occlude its
        # hit-test region even when the red circle looked visible in a screenshot.
        objects.append({
            "layer_index": 8,
            "shape": {"type": "circle", "radius": 25},
            "transform": {"translation": {"x": 700, "y": 350}},
            "style": {"stroke": None, "stroke_width": 0,
                      "fill": {"type": "solid", "color": [245, 40, 35, 255]}}})
        created, elapsed = client.tool("drawing_create_scene", {
            "id": id_, "layers": [{"name":name} for name in names] + [{"name":"Drag target (top)"}],
            "objects": objects, "expected_object_count": 0})
        client.tool("set_active_note", {"id": id_})
        assert created["result"]["structuredContent"]["added_objects"] == count
        print(json.dumps({"workspace": str(folder), "note_id": id_,
                          "object_count": count, "create_ms": round(elapsed, 3)}))
    finally:
        client.close()
    extra_notes = int(os.environ.get("INKPAD_GUI_EXTRA_NOTES", "0"))
    if not 0 <= extra_notes <= 5000:
        raise ValueError("INKPAD_GUI_EXTRA_NOTES must be between 0 and 5000")
    if extra_notes:
        path = folder / "workspace.json"
        workspace = json.loads(path.read_text(encoding="utf-8"))
        template = next(n for n in workspace["notes"] if n["id"] != id_)
        for i in range(extra_notes):
            note = template.copy()
            note.update(id=f"gui-stress-{i:05}", title=f"Stress note {i}",
                        body="Synthetic unicode 猫 notebook text. " * 80,
                        pinned=bool(i % 9 == 0), updated=i)
            workspace["notes"].append(note)
        path.write_text(json.dumps(workspace, ensure_ascii=False, separators=(",", ":")), encoding="utf-8")
        print(json.dumps({"extra_notes":extra_notes,"workspace_mib":round(path.stat().st_size/1048576,3)}))


if __name__ == "__main__":
    main()
