"""Isolated real-stdio MCP regression/benchmark for in-place multi-object art revisions.

Run:
 py -3 tests/scene_atomic_revision_mcp.py target/advanced-vector-release-build/release/inkpad-agent.exe
Never reads or modifies the user's real Inkpad notebook.
"""
import base64
import hashlib
import json
import statistics
import sys
import tempfile
import time
from pathlib import Path
from benchmark_agent_mcp import MCP
from advanced_vector_mcp_smoke import call_expect_rejection


def content(reply):
    return reply["result"]["structuredContent"]


def png(reply):
    return base64.b64decode(next(x for x in reply["result"]["content"]
                                 if x["type"] == "image")["data"])


def main(exe: Path):
    with tempfile.TemporaryDirectory(prefix="inkpad-scene-edit-") as tmp:
        client = MCP(exe.resolve(strict=True), Path(tmp))
        try:
            schema, _ = client.call("tools/list", {})
            scene_schema = next(x for x in schema["result"]["tools"]
                                if x["name"] == "drawing_create_scene")["inputSchema"]
            assert "edits" in scene_schema["properties"]
            noteids = []
            allids = []
            objects = []
            for i in range(24):
                objects.append({"shape":{"type":"ellipse","radius_x":13,"radius_y":5},
                                "position":{"x":62+i*40,"y":340+i%3*30},
                                "style":{"stroke":None,"fill":{"type":"solid",
                                    "color":[115+i*2,160,185,255]}}})
            for title in ("24 individual corrections","One atomic correction"):
                note,_ = client.tool("create_note",{"title":title})
                nid = content(note)["id"]
                created,_ = client.tool("drawing_create_scene",{
                    "id":nid,"expected_object_count":0,
                    "layers":[{"name":"Wing feather tiles"}],
                    "objects":[dict(o,layer_index=0) for o in objects]})
                assert content(created)["object_count"] == 24
                noteids.append(nid)
                allids.append(content(created)["object_ids"])
            individual=[]
            for i,oid in enumerate(allids[0]):
                r,elapsed=client.tool("drawing_edit_shape",{
                    "id":noteids[0],"object_id":oid,
                    "position":{"x":62+i*40,"y":370+i%3*29}})
                assert content(r)["object"]["id"] == oid
                individual.append(elapsed)
            scene,_=client.call("tools/list",{})
            assert "edits" in scene_schema["properties"]
            corrections = [{"object_id":oid,"position":{"x":62+i*40,"y":370+i%3*29}}
                           for i,oid in enumerate(allids[1])]
            updated,batch_ms=client.tool("drawing_create_scene",{
                "id":noteids[1],"expected_object_count":24,
                "edits":corrections})
            assert content(updated)["edited_objects"] == 24
            assert content(updated)["added_objects"] == 0
            assert content(updated)["edited_object_ids"] == allids[1]
            first,_=client.tool("drawing_list",{"id":noteids[0]})
            second,_=client.tool("drawing_list",{"id":noteids[1]})
            assert content(first)["object_count"] == content(second)["object_count"] == 24
            # Rendering proves the two styles of correction leave the same
            # actual illustration, despite different stable random object IDs.
            preview_a,_=client.tool("drawing_preview",{"id":noteids[0]})
            preview_b,_=client.tool("drawing_preview",{"id":noteids[1]})
            sha=hashlib.sha256(png(preview_a)).hexdigest()
            assert png(preview_a)==png(preview_b)
            before=(Path(tmp)/"workspace.json").read_bytes()
            call_expect_rejection(client,"drawing_create_scene",{
                "id":noteids[1],"expected_object_count":24,
                "edits":[{"object_id":allids[1][0],"visible":False},
                         {"object_id":"missing-id","visible":False}]})
            assert (Path(tmp)/"workspace.json").read_bytes()==before
            # Final preview from saved post-edit note exactly matches a fresh
            # lossless PNG export. No workspace.json scripting or guessing.
            with tempfile.TemporaryDirectory(prefix="inkpad-scene-export-") as exported:
                target=Path(exported)/"independent-export.png"
                client.tool("export_note",{"id":noteids[1],"format":"png","path":str(target)})
                assert png(preview_b)==target.read_bytes()
            client.tool("drawing_undo",{"id":noteids[1],"confirm":True})
            reverted,_=client.tool("drawing_list",{"id":noteids[1]})
            assert content(reverted)["drawing"]["objects"][0]["transform"]["translation"] == {
                "x":62.0,"y":340.0}
            client.tool("drawing_redo",{"id":noteids[1],"confirm":True})
            redone,_=client.tool("drawing_preview",{"id":noteids[1]})
            assert png(redone)==png(preview_b)
            report={"result":"PASS","objects":24,"layers":1,
                    "individual_edit_calls":24,"atomic_edit_calls":1,
                    "individual_mcp_ms_total":round(sum(individual),3),
                    "atomic_mcp_ms":round(batch_ms,3),
                    "measured_speedup":round(sum(individual)/batch_ms,2),
                    "pixel_identical_preview_sha256":sha,
                    "verified_undo_step":True,
                    "atomic_late_failure_rollback":True,
                    "export_matches_preview":True,"exe":str(exe)}
            print(json.dumps(report))
        finally:
            client.close()


if __name__=="__main__":
    if len(sys.argv)!=2:
        raise SystemExit("usage: python tests/scene_atomic_revision_mcp.py AGENT_EXE")
    main(Path(sys.argv[1]))
