"""Small, dependency-free *editable vector* geometry helpers for Inkpad MCP.

These helpers construct paths, never raster pictures or hardcoded duck anatomy.
The artist/agent supplies world-space anatomical landmarks and wing rails.
Curves remain ordinary separately editable Inkpad `path` shapes.

Not an SVG importer; the output uses Inkpad's move_to/cubic_to/close schema.
"""
from __future__ import annotations

import math
from typing import Sequence


def _point(value):
    if isinstance(value, dict):
        x, y = value["x"], value["y"]
    else:
        x, y = value
    x, y = float(x), float(y)
    if not math.isfinite(x) or not math.isfinite(y) or abs(x) > 16384 or abs(y) > 16384:
        raise ValueError("Non-finite/out-of-bounds vector landmark")
    return (x, y)


def _json(point):
    return {"x": round(point[0], 4), "y": round(point[1], 4)}


def _add(a, b):
    return (a[0] + b[0], a[1] + b[1])


def _sub(a, b):
    return (a[0] - b[0], a[1] - b[1])


def _mul(a, factor):
    return (a[0] * factor, a[1] * factor)


def _length(a):
    return math.hypot(a[0], a[1])


def cubic_path(points: Sequence, *, closed=True, smoothness=0.55, corners=()):
    """Produce continuous cubic geometry from connected landmark locations.

    Coordinates are already in canvas space; omit object transform/position.
    For real anatomy, place correct landmarks FIRST: smoothing cannot repair a
    circular skull, detached neck, incorrect wing root or exaggerated keel.
    `corners` may mark bill-root/tail breaks that must retain sharper angles.
    """
    pts = [_point(v) for v in points]
    count = len(pts)
    if count < (3 if closed else 2) or count > 128:
        raise ValueError("Need 3..128 closed or 2..128 open contour landmarks")
    if not 0 <= smoothness <= 1:
        raise ValueError("smoothness must be 0..1")
    if any(_length(_sub(pts[i], pts[(i + 1) % count])) < 0.001
           for i in range(count if closed else count - 1)):
        raise ValueError("Adjacent landmarks must differ")
    if any(not isinstance(i, int) or i < 0 or i >= count for i in corners):
        raise ValueError("Corner index out of range")
    corners = set(corners)
    def tangent(i):
        if i in corners or (not closed and i in (0, count-1)):
            return (0.0, 0.0)
        before, here, after = pts[(i-1) % count], pts[i], pts[(i+1) % count]
        leg0, leg1 = _length(_sub(here, before)), _length(_sub(after, here))
        span = _sub(after, before)
        span_length = _length(span)
        if span_length < 0.001:
            return (0.0, 0.0)
        # Limiting by the shorter adjacent segment prevents broad overshoots
        # when head/nape/wing landmarks are unevenly spaced.
        return _mul(span, smoothness * min(leg0, leg1) / span_length)
    commands = [{"type": "move_to", "to": _json(pts[0])}]
    for i in range(count if closed else count - 1):
        j = (i+1) % count
        c1 = _add(pts[i], _mul(tangent(i), 1/3))
        c2 = _sub(pts[j], _mul(tangent(j), 1/3))
        commands.append({"type":"cubic_to","control1":_json(c1),
                         "control2":_json(c2),"to":_json(pts[j])})
    if closed:
        commands.append({"type":"close"})
    return {"type":"path","commands":commands}


def cubic_bezier(rail: Sequence, t: float):
    """Point on a cubic rail. Rails define actual anatomical top/bottom forms."""
    if len(rail) != 4 or not 0 <= t <= 1:
        raise ValueError("A rail needs four points and t in [0,1]")
    a,b,c,d = [_point(p) for p in rail]
    u = 1-t
    return (a[0]*u**3 + 3*b[0]*u*u*t + 3*c[0]*u*t*t + d[0]*t**3,
            a[1]*u**3 + 3*b[1]*u*u*t + 3*c[1]*u*t*t + d[1]*t**3)


def feather_path(root, tip, *, width=15.0, bend=0.05, tip_width=0.0):
    """One attached tapered feather surface aligned from root to tip.

    Width/bend are local to the supplied root/tip direction. With zero bend
    it has tangent-aligned sides and a clean pointed tip rather than a pasted
    ellipse. Use each returned editable path with a parent `clip_shape` if it
    must never protrude through the wing outline.
    """
    root, tip = _point(root), _point(tip)
    vector = _sub(tip, root)
    length = _length(vector)
    if length <= 1 or not (0 < width <= length * 1.5):
        raise ValueError("Feather must have nonzero length and sensible root width")
    if not -0.75 <= bend <= 0.75 or not 0 <= tip_width < width:
        raise ValueError("Invalid feather bend or tip width")
    forward = _mul(vector, 1/length)
    normal = (-forward[1], forward[0])
    root_l = _sub(root, _mul(normal,width/2))
    root_r = _add(root, _mul(normal,width/2))
    tip_l = _sub(tip, _mul(normal,tip_width/2))
    tip_r = _add(tip, _mul(normal,tip_width/2))
    bow = _mul(normal,length*bend)
    c1l = _add(_add(root_l, _mul(vector,0.30)),bow)
    c2l = _sub(_add(tip_l,bow),_mul(vector,0.31))
    c1r = _sub(_add(tip_r,bow),_mul(vector,0.31))
    c2r = _add(_add(root_r,_mul(vector,0.30)),bow)
    # Shape the tip into a curved covert (not the sawtooth wedge produced by
    # joining two straight tips), whilst keeping its root attached to the wing.
    cap_depth = min(width*0.4,length*0.15)
    cap_l = _add(tip_l,_mul(forward,cap_depth))
    cap_r = _add(tip_r,_mul(forward,cap_depth))
    return {"type":"path","commands":[
        {"type":"move_to","to":_json(root_l)},
        {"type":"cubic_to","control1":_json(c1l),"control2":_json(c2l),"to":_json(tip_l)},
        {"type":"cubic_to","control1":_json(cap_l),"control2":_json(cap_r),"to":_json(tip_r)},
        {"type":"cubic_to","control1":_json(c1r),"control2":_json(c2r),"to":_json(root_r)},
        {"type":"close"}]}


def wing_covert_row(top_rail, lower_rail, *, count=8, overlap=1.18, bend=0.05,
                    wing_root=None, tip_shift=0.0):
    """Generate a row of physically directed feather surfaces.

    Top and lower rails run *both* from near the wing shoulder toward rump.
    Their correspondence controls the actual flow; feathers follow the rows,
    not arbitrary crisscross marks. Each path can be individually edited.
    """
    if not isinstance(count,int) or count < 2 or count > 48:
        raise ValueError("covert row must have 2..48 feathers")
    if not 0.5 <= overlap <= 2.5:
        raise ValueError("Invalid feather overlap ratio")
    if not -0.8 <= tip_shift <= 0.8:
        raise ValueError("Feather tip rail shift must stay within -0.8..0.8")
    if len(top_rail) != 4 or len(lower_rail) != 4:
        raise ValueError("Top/lower feather rails each need four Bézier control points")
    # Both rails must travel from wing shoulder to rump. Reversing a lower
    # rail formerly produced crossing, backward-pointing 'feathers' that were
    # still accepted and drawn as legitimate editable vector objects.
    top_forward = _sub(cubic_bezier(top_rail, 1), cubic_bezier(top_rail, 0))
    lower_forward = _sub(cubic_bezier(lower_rail, 1), cubic_bezier(lower_rail, 0))
    if _length(top_forward) < 1 or _length(lower_forward) < 1:
        raise ValueError("Wing rails must advance from shoulder to rump")
    if (top_forward[0]*lower_forward[0]+top_forward[1]*lower_forward[1]) <= (
        0.20*_length(top_forward)*_length(lower_forward)
    ):
        raise ValueError("Top and lower wing rails must flow in the same shoulder-to-rump direction")
    if wing_root is not None:
        root = _point(wing_root)
        first = cubic_bezier(top_rail,0)
        last = cubic_bezier(top_rail,1)
        if _length(_sub(root,first))>_length(_sub(root,last)):
            raise ValueError("Top rail starts away from wing root; reverse both rails shoulder-to-rump")
    paths=[]
    roots=[cubic_bezier(top_rail,i/(count-1)) for i in range(count)]
    # Enforce a coherent row, rather than accepting individual rail folds
    # that can reverse feather order halfway through a folded wing.
    for index in range(1, count):
        advance = _sub(roots[index],roots[index-1])
        if advance[0]*top_forward[0]+advance[1]*top_forward[1] <= 0:
            raise ValueError("Top rail folds backward and reverses feather root ordering")
    for i,root in enumerate(roots):
        tip=cubic_bezier(lower_rail,max(0,min(1,(i+0.45+tip_shift)/count)))
        if _length(_sub(tip,root)) <= 1:
            raise ValueError("Wing feather tip overlaps its root")
        # The root-to-tip vector should point toward the lower rail's side
        # consistently. Flipped lower rails create visually crossed coverts.
        root_side=_sub(cubic_bezier(lower_rail,i/(count-1)),root)
        direction=_sub(tip,root)
        if _length(root_side)>1 and (
            direction[0]*root_side[0]+direction[1]*root_side[1]
        ) < -0.1*_length(direction)*_length(root_side):
            raise ValueError("Feather points away from lower wing rail")
        pitch=(_length(_sub(roots[min(i+1,count-1)],root)) if i==0 else
               _length(_sub(root,roots[i-1])))
        length=_length(_sub(tip,root))
        width=max(1.0,min(length*.85, pitch*overlap))
        paths.append(feather_path(root,tip,width=width,bend=bend,
                                  tip_width=min(width*.42,length*.25)))
    return paths


def silhouette_landmark_warnings(landmarks, *, facing="right"):
    """Detect common *structural* profile mistakes before rasterizing details.

    This diagnoses an artist-supplied bird profile; it never generates a
    particular duck or imposes a reference photo. Diagnostics are advisory,
    since perspective and stylistic exaggeration can be intentional.
    World-space canvas Y increases downwards.

    Required landmarks: crown, bill_root, throat, nape, breast_apex,
    belly, back, wing_root, rump and water_contact.
    """
    required = ("crown", "bill_root", "throat", "nape", "breast_apex",
                "belly", "back", "wing_root", "rump", "water_contact")
    missing = [key for key in required if key not in landmarks]
    if missing:
        raise ValueError("Missing anatomical landmarks: " + ", ".join(missing))
    if facing not in ("right", "left"):
        raise ValueError("facing must be left or right")
    p = {key: _point(landmarks[key]) for key in required}
    xsign = 1 if facing == "right" else -1
    front = lambda a,b: (p[a][0]-p[b][0])*xsign
    warnings=[]
    body_span=abs(p["breast_apex"][0]-p["rump"][0])
    if body_span<60:
        warnings.append("Body too short in profile: check rump-to-breast landmarks")
    if front("bill_root","nape") <= 5:
        warnings.append("Bill root should project ahead of posterior nape")
    if p["crown"][1]>=p["throat"][1]-8:
        warnings.append("Crown/throat landmarks collapse skull height")
    if p["nape"][1]>=p["water_contact"][1]-20:
        warnings.append("Nape descends nearly to water contact")
    if abs(p["breast_apex"][0]-p["water_contact"][0])<5 and p["breast_apex"][1] >= p["water_contact"][1]-12:
        warnings.append("Breast hangs directly to waterline without lateral taper")
    if p["wing_root"][1]>p["belly"][1]-6:
        warnings.append("Wing root should sit above belly, not along the keel")
    if p["back"][1]>p["belly"][1]-30:
        warnings.append("Back/belly depth collapsed into an ellipse-like flat hull")
    if p["water_contact"][1]<p["belly"][1]-35:
        warnings.append("Contact shadow lies high above belly rather than at lake surface")
    return warnings
