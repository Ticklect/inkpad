"""Isolated cross-process Inkpad storage regression.

Usage: py -3 tests/mcp_concurrency_integrity.py NEW_AGENT_EXE [LEGACY_AGENT_EXE]
Does not access the user's actual Inkpad workspace.
"""

from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from benchmark_agent_mcp import MCP


def append_once(exe, folder, note_id, value):
    env = os.environ.copy()
    env["INKPAD_DATA_DIR"] = str(folder)
    proc = subprocess.run(
        [str(exe), "append", note_id, value],
        env=env, capture_output=True, text=True, encoding="utf-8",
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        timeout=30,
    )
    if proc.returncode:
        raise AssertionError(f"Concurrent append failed ({proc.returncode}): {proc.stderr}")


def main():
    if len(sys.argv) not in (2, 3):
        raise SystemExit("usage: mcp_concurrency_integrity.py NEW_AGENT_EXE [LEGACY_AGENT_EXE]")
    exe = Path(sys.argv[1]).resolve(strict=True)
    legacy_exe = Path(sys.argv[2]).resolve(strict=True) if len(sys.argv) == 3 else None
    with tempfile.TemporaryDirectory(prefix="inkpad-mcp-concurrency-") as folder_name:
        folder = Path(folder_name)
        legacy_note_id = None
        if legacy_exe is not None:
            legacy_client = MCP(legacy_exe, folder)
            try:
                prior, _ = legacy_client.tool("create_note", {"title": "Legacy 🐈", "body": "before migration"})
                legacy_note_id = prior["result"]["structuredContent"]["id"]
                legacy_client.tool("append_to_note", {"id": legacy_note_id, "text": "second snapshot"})
            finally:
                legacy_client.close()
        client = MCP(exe, folder)
        try:
            if legacy_note_id is not None:
                restored, _ = client.tool("read_note", {"id": legacy_note_id, "compact": True})
                assert restored["result"]["structuredContent"]["body"] == "before migration\nsecond snapshot"
            created, _ = client.tool("create_note", {"title": "Concurrency test", "body": "begin 🐈"})
            note_id = created["result"]["structuredContent"]["id"]
            values = [f"token-{i:02d}-猫" for i in range(12)]
            # Each independent process takes the same cross-process workspace
            # lock. Lost updates or stale caches would drop one or more tokens.
            with ThreadPoolExecutor(max_workers=4) as pool:
                list(pool.map(lambda value: append_once(exe, folder, note_id, value), values))
            read, _ = client.tool("read_note", {"id": note_id, "compact": True})
            body = read["result"]["structuredContent"]["body"]
            assert body.startswith("begin 🐈")
            assert all(body.count(value) == 1 for value in values)
            found, _ = client.tool("search_notes", {"query": values[-1], "limit": 1, "include_body": True})
            matches = found["result"]["structuredContent"]["notes"]
            assert len(matches) == 1 and matches[0]["id"] == note_id
            assert matches[0]["body_truncated"] is False
            assert (folder / "workspace.json").exists(), "Legacy workspace source must remain recoverable"
            if legacy_note_id is not None:
                assert (folder / "workspace.json.bak").exists(), "Existing legacy recovery backup must survive migration"
            print(json.dumps({"result": "PASS", "concurrent_writers": 4, "appends": len(values),
                              "all_updates_visible": True, "legacy_migration": legacy_note_id is not None}))
        finally:
            client.close()


if __name__ == "__main__":
    main()
