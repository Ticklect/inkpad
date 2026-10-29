"""Reproducible image-heavy notebook MCP benchmark.

Usage: python tests/benchmark_image_heavy.py path/to/inkpad-agent.exe
Builds a 2,200-note synthetic notebook and six attached 600x400 PNGs in
temporary directories. Measures p50/p95 read, search, import, placement,
image list, HTML export, CPU and RSS. Does not read or modify personal notes.
"""
import json
import os
from pathlib import Path
import random
import statistics
import sys
import tempfile
import time

from PIL import Image
import psutil
from benchmark_agent_mcp import MCP, create_workspace


def distribution(values):
    values = sorted(values)
    return {"p50":round(statistics.median(values),3),
            "p95":round(values[int(.95*(len(values)-1))],3)}


def benchmark(exe):
    with tempfile.TemporaryDirectory(prefix="inkpad-image-heavy-workspace-") as data:
        with tempfile.TemporaryDirectory(prefix="inkpad-image-heavy-assets-") as assets:
            folder, source_dir = Path(data), Path(assets)
            size = create_workspace(exe, folder)
            rand = random.Random(11019)
            pixels = bytes(rand.getrandbits(8) for _ in range(600*400*3))
            image_path = source_dir/"synthetic-test-only.png"
            Image.frombytes("RGB",(600,400),pixels).save(image_path,format="PNG")
            client = MCP(exe, folder)
            process = psutil.Process(client.proc.pid)
            start_cpu = sum(process.cpu_times()[:2])
            note_id = "speed-00001"
            metrics = {"read_ms":[], "search_ms":[], "image_insert_ms":[],
                       "image_place_ms":[], "image_list_ms":[]}
            try:
                for _ in range(12):
                    _,elapsed = client.tool("read_note",{"id":note_id,"compact":True})
                    metrics["read_ms"].append(elapsed)
                    _,elapsed = client.tool("search_notes",{"query":"rare-tail-marker","limit":2})
                    metrics["search_ms"].append(elapsed)
                for index in range(6):
                    response, elapsed = client.tool("image_insert_file",{
                        "id":note_id,"path":str(image_path),"user_approved":True})
                    metrics["image_insert_ms"].append(elapsed)
                    payload = response["result"]["structuredContent"]
                    image_id = payload.get("image",{}).get("id")
                    if image_id is None:
                        raise RuntimeError(f"Missing image id in reply: {payload}")
                    _,elapsed = client.tool("image_place",{
                        "id":note_id,"image_id":image_id,
                        "unit":"unicode_scalar","char_offset":index*25})
                    metrics["image_place_ms"].append(elapsed)
                for _ in range(12):
                    response,elapsed = client.tool("image_list",{"id":note_id})
                    metrics["image_list_ms"].append(elapsed)
                    assert len(response["result"]["structuredContent"]["images"])==6
                html = source_dir/"export.html"
                _,export_ms = client.tool("image_export_html",{
                    "id":note_id,"path":str(html),"user_approved":True})
                assert html.is_file() and html.stat().st_size > image_path.stat().st_size
                report = {name:distribution(values) for name,values in metrics.items()}
                report.update({
                    "notes":2200, "workspace_mb":round(size/(1024**2),3),
                    "images":6, "source_png_bytes":image_path.stat().st_size,
                    "html_export_ms":round(export_ms,3),
                    "html_export_bytes":html.stat().st_size,
                    "process_cpu_ms":round((sum(process.cpu_times()[:2])-start_cpu)*1000,3),
                    "rss_after_mib":round(process.memory_info().rss/(1024**2),3)})
                return report
            finally:
                client.close()


if __name__=="__main__":
    if len(sys.argv)!=2:
        raise SystemExit(__doc__)
    print(json.dumps({"exe":str(Path(sys.argv[1]).resolve(strict=True)),
                      **benchmark(Path(sys.argv[1]).resolve(strict=True))}),flush=True)
