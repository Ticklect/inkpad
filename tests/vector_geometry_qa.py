"""Test reusable editable Inkpad anatomy/feather geometry, and render it via real MCP.

Usage: py -3 tests/vector_geometry_qa.py path/to/inkpad-agent.exe
Only writes new files under target/vector-geometry-qa-20261018 and a new
temp INKPAD_DATA_DIR. Does not access the real notebook or historical art.
"""
import base64
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import uuid

from benchmark_agent_mcp import MCP
from advanced_vector_mcp_smoke import png_pixels

ROOT=Path(__file__).resolve().parent.parent
module_path=ROOT/"skills"/"inkpad"/"vector_geometry.py"
spec=importlib.util.spec_from_file_location("inkpad_vector_geometry",module_path)
geo=importlib.util.module_from_spec(spec)
spec.loader.exec_module(geo)


def error_case(fn):
    try: fn()
    except ValueError: return
    raise AssertionError("Invalid geometry accepted")


def run(exe):
    # Controlled geometry unit regressions before entering the MCP session.
    pts=[(210,440),(270,388),(410,370),(470,422),(411,475),(260,490)]
    outline=geo.cubic_path(pts)
    commands=outline["commands"]
    assert commands[0]["type"]=="move_to"
    assert commands[-1]["type"]=="close"
    assert len(commands)==len(pts)+2
    for c in commands:
        for k in ("control1","control2","to"):
            if k in c:
                assert -16384 <= c[k]["x"] <= 16384
                assert -16384 <= c[k]["y"] <= 16384
    open_path=geo.cubic_path(pts[:3],closed=False,corners=(1,))
    assert open_path["commands"][-1]["type"]=="cubic_to"
    assert open_path["commands"][1]["to"]=={"x":270.0,"y":388.0}
    error_case(lambda: geo.cubic_path([(0,0),(0,0),(3,4)]))
    error_case(lambda: geo.cubic_path([(0,0),(1,float("nan")),(3,4)]))
    error_case(lambda: geo.feather_path((0,0),(0,0)))
    error_case(lambda: geo.wing_covert_row([(0,0)]*4,[(0,0)]*4,count=100))
    error_case(lambda: geo.wing_covert_row(
        [(300,350),(420,320),(540,320),(660,365)],
        [(660,440),(540,450),(420,450),(300,415)],count=8))
    error_case(lambda: geo.wing_covert_row(
        [(300,350),(420,320),(540,320),(660,365)],
        [(300,350),(420,320),(540,320),(660,365)],count=8))
    error_case(lambda: geo.wing_covert_row(
        [(300,350),(420,320),(540,320),(660,365)],
        [(300,415),(420,450),(540,450),(660,440)],count=8,wing_root=(700,355)))
    healthy={"crown":(690,225),"bill_root":(734,255),"throat":(690,320),
             "nape":(655,300),"breast_apex":(705,411),"belly":(560,480),
             "back":(535,368),"wing_root":(595,383),"rump":(340,411),
             "water_contact":(550,497)}
    assert not geo.silhouette_landmark_warnings(healthy,facing="right")
    flawed=dict(healthy,breast_apex=(550,497),wing_root=(590,480))
    detected=geo.silhouette_landmark_warnings(flawed)
    assert len(detected)>=2 and any("Breast" in issue for issue in detected)
    error_case(lambda: geo.silhouette_landmark_warnings({"crown":(0,0)}))
    wing=geo.cubic_path([(330,411),(390,374),(554,360),(747,388),
                         (777,414),(736,458),(541,475),(364,456)],smoothness=0.64)
    roots=[(396,389),(474,368),(601,381),(699,408)]
    tips=[(415,444),(494,458),(614,455),(714,426)]
    ranks=geo.wing_covert_row(roots,tips,count=10,overlap=1.25,bend=0.07)
    assert len(ranks)==10 and all(p["type"]=="path" for p in ranks)
    assert all(p["commands"][-1]["type"]=="close" for p in ranks)
    out=ROOT/"target"/"vector-geometry-qa-20261018"
    out.mkdir(parents=True,exist_ok=True)
    basename=datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ-")+uuid.uuid4().hex[:8]
    run_dir=out/basename
    run_dir.mkdir(exist_ok=False)
    with tempfile.TemporaryDirectory(prefix="inkpad-connected-geometry-") as temp:
        mcp=MCP(exe.resolve(strict=True),Path(temp))
        try:
            listed,_=mcp.call("tools/list",{})
            names={x["name"] for x in listed["result"]["tools"]}
            assert {"drawing_create_scene","drawing_preview","export_note","drawing_list"}.issubset(names)
            note,_=mcp.tool("create_note",{"title":"Synthetic coherent wing - geometry QA"})
            note_id=note["result"]["structuredContent"]["id"]
            scene=[
                {"layer_index":0,"shape":{"type":"rectangle","width":1100,"height":700},
                 "style":{"stroke":None,"fill":{"type":"solid","color":[218,233,237,255]}}},
                {"layer_index":1,"shape":wing,
                 "style":{"stroke":[73,78,73,255],"stroke_width":1.0,
                     "fill":{"type":"linear_gradient","start":{"x":300,"y":365},
                             "end":{"x":500,"y":484},"stops":[
                                {"offset":0,"color":[173,162,134,255]},
                                {"offset":1,"color":[94,104,100,255]}]}}},
            ]
            for i,feather in enumerate(ranks):
                col=[197-7*i,191-6*i,164-4*i,242]
                scene.append({"layer_index":2,"shape":feather,"clip_shape":wing,
                              "style":{"stroke":None,"opacity":0.95,
                                       "fill":{"type":"solid","color":col}}})
            result,scene_ms=mcp.tool("drawing_create_scene",{
                "id":note_id,"expected_object_count":0,
                "layers":[{"name":"White background"},{"name":"Wing volume"},{"name":"Anchored coverts"}],
                "objects":scene})
            assert result["result"]["structuredContent"]["object_count"]==len(scene)
            actual,_=mcp.tool("drawing_list",{"id":note_id})
            saved=actual["result"]["structuredContent"]
            assert saved["object_count"]==12 and saved["layer_count"]==3
            preview,preview_ms=mcp.tool("drawing_preview",{"id":note_id})
            block=next(v for v in preview["result"]["content"] if v["type"]=="image")
            img=base64.b64decode(block["data"])
            coloured, pixel=png_pixels(img)
            assert coloured > 16000
            assert pixel(5,5)==(218,233,237)
            assert pixel(500,407)!=(218,233,237)
            export=run_dir/"coherent-wing-feather-geometry.png"
            mcp.tool("export_note",{"id":note_id,"format":"png","path":str(export)})
            assert export.read_bytes()==img
            manifest={"result":"PASS","note_id":note_id,"objects":12,"layers":3,
                "wing_feather_count":len(ranks),"scene_ms":round(scene_ms,3),
                "preview_ms":round(preview_ms,3),"preview_export_equal":True,
                "sha256":hashlib.sha256(img).hexdigest(),"path":str(export)}
            with (run_dir/"report.json").open("x",encoding="utf8") as f:
                json.dump(manifest,f,indent=2)
            print(json.dumps(manifest))
        finally:
            mcp.close()

if __name__=="__main__":
    if len(sys.argv)!=2:
        raise SystemExit("usage: vector_geometry_qa.py AGENT_EXE")
    run(Path(sys.argv[1]))
