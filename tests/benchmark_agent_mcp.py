"""Compare Inkpad MCP stdio round trips on synthetic local notes.

Usage: python tests/benchmark_agent_mcp.py OLD_AGENT_EXE NEW_AGENT_EXE
Each binary gets an isolated disposable workspace. This never reads real notes.
"""

import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import time


NOTE_COUNT = 2200
SAMPLES = 25


def run_cli(exe, folder, *args):
    env = os.environ.copy()
    env["INKPAD_DATA_DIR"] = str(folder)
    subprocess.run([str(exe), *args], env=env, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


def create_workspace(exe, folder):
    run_cli(exe, folder, "list")
    path = folder / "workspace.json"
    workspace = json.loads(path.read_text(encoding="utf-8"))
    original = workspace["notes"][0]
    notes = []
    for i in range(NOTE_COUNT):
        note = original.copy()
        note.update(id=f"speed-{i:05}", title=f"Project {i:04}",
                    body=("Project release milestones and unicode 猫. " * 72)
                    + ("rare-tail-marker" if i == NOTE_COUNT - 1 else "ordinary"),
                    pinned=(i % 9 == 0), updated=i)
        notes.append(note)
    workspace["notes"] = notes
    workspace["active_id"] = notes[0]["id"]
    path.write_text(json.dumps(workspace, ensure_ascii=False), encoding="utf-8")
    return path.stat().st_size


class MCP:
    def __init__(self, exe, folder):
        env = os.environ.copy()
        env["INKPAD_DATA_DIR"] = str(folder)
        flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
        self.proc = subprocess.Popen([str(exe), "mcp"], env=env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, text=True, encoding="utf-8", bufsize=1,
                                     creationflags=flags)
        self.counter = 0
        reply, _ = self.call("initialize", {"protocolVersion": "2025-06-18"})
        assert reply["result"]["protocolVersion"] == "2025-06-18"

    def call(self, method, params):
        self.counter += 1
        request = {"jsonrpc": "2.0", "id": self.counter, "method": method, "params": params}
        start = time.perf_counter_ns()
        self.proc.stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        elapsed_ms = (time.perf_counter_ns() - start) / 1e6
        if not line:
            raise RuntimeError(f"MCP server terminated: code {self.proc.poll()}")
        reply = json.loads(line)
        assert reply.get("id") == self.counter, reply
        assert "error" not in reply and not reply.get("result", {}).get("isError", False), reply
        return reply, elapsed_ms

    def tool(self, name, args=None):
        return self.call("tools/call", {"name": name, "arguments": args or {}})

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=10)
        assert self.proc.returncode == 0


def measure(client, operation, count=SAMPLES):
    operation()  # warm cache / lazy initialization
    samples = [operation()[1] for _ in range(count)]
    samples.sort()
    return round(statistics.median(samples), 2), round(samples[int(0.95 * (len(samples) - 1))], 2)


def benchmark(exe):
    with tempfile.TemporaryDirectory(prefix="inkpad-mcp-bench-") as temp:
        folder = Path(temp)
        size = create_workspace(exe, folder)
        client = MCP(exe, folder)
        try:
            result = {"workspace_mb": round(size / (1024 ** 2), 2)}
            result["discovery_ms"] = measure(client, lambda: client.call("tools/list", {}), 10)
            result["search_hit_ms"] = measure(client, lambda: client.tool("search_notes", {"query": "RARE-TAIL-MARKER", "limit": 5}))
            result["search_miss_ms"] = measure(client, lambda: client.tool("search_notes", {"query": "no-matches-anywhere", "limit": 5}))
            def search_then_read():
                found, search_ms = client.tool("search_notes", {"query": "rare-tail-marker", "limit": 1})
                matches = found["result"]["structuredContent"]["notes"]
                assert len(matches) == 1
                note, read_ms = client.tool("read_note", {"id": matches[0]["id"], "compact": True})
                assert "rare-tail-marker" in note["result"]["structuredContent"]["body"]
                return note, search_ms + read_ms

            def search_with_body():
                found, elapsed = client.tool("search_notes", {"query": "rare-tail-marker", "limit": 1, "include_body": True})
                matches = found["result"]["structuredContent"]["notes"]
                assert len(matches) == 1 and "rare-tail-marker" in matches[0]["body"]
                assert matches[0]["body_truncated"] is False
                return found, elapsed

            result["search_then_read_ms"] = measure(client, search_then_read)
            result["search_with_body_ms"] = measure(client, search_with_body)
            result["list_ms"] = measure(client, lambda: client.tool("list_notes", {"limit": 100}))
            result["append_ms"] = measure(client, lambda: client.tool("append_to_note", {"id": "speed-00001", "text": "bench"}), 8)
            ops = [{"op": "append", "id": "speed-00002", "text": f"step-{i}"} for i in range(8)]

            def sequential_eight():
                elapsed_total = 0
                for operation in ops:
                    _, elapsed = client.tool("append_to_note", {
                        "id": operation["id"], "text": operation["text"]})
                    elapsed_total += elapsed
                return None, elapsed_total

            result["eight_appends_sequential_ms"] = measure(client, sequential_eight, 6)
            discovery, _ = client.call("tools/list", {})
            if any(tool["name"] == "note_batch" for tool in discovery["result"]["tools"]):
                result["eight_appends_batch_ms"] = measure(
                    client, lambda: client.tool("note_batch", {"operations": ops}), 6)
            # Prove that a persistent MCP process sees another process's update.
            run_cli(exe, folder, "append", "speed-02199", "external-save-check")
            check, _ = client.tool("search_notes", {"query": "external-save-check"})
            assert check["result"]["structuredContent"]["total"] == 1
            return result
        finally:
            client.close()


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: benchmark_agent_mcp.py OLD_AGENT_EXE NEW_AGENT_EXE")
    for label, arg in (("before", sys.argv[1]), ("after", sys.argv[2])):
        exe = Path(arg).resolve(strict=True)
        print(json.dumps({"binary": label, "path": str(exe), **benchmark(exe)}), flush=True)


if __name__ == "__main__":
    main()
