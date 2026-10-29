"""Real Win32 60Hz canvas dragging with file-commit acknowledgement.

Usage: python tests/benchmark_gui_sustained.py PID TEMP_WORKSPACE X Y [GESTURES [IMAGE_WIDTH IMAGE_HEIGHT]]
Draw mode and Select must be active on the disposable active page.
The last object is a red target at initial canvas (700,350). X/Y are its
screen coordinates as measured using Windows UI Automation. One full
mouse-down/drag/mouse-up gesture is sent each iteration; each must persist.
The live cursor is restored. No installed application or personal notes used.
"""

import ctypes
from ctypes import wintypes
import json
from pathlib import Path
import psutil
import statistics
import sys
import time

user32 = ctypes.windll.user32
user32.SetCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
user32.GetCursorPos.argtypes = [ctypes.POINTER(wintypes.POINT)]
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]


def get_target(folder):
    payload = json.loads((folder / "workspace.json").read_text(encoding="utf-8"))
    note = next(n for n in payload["notes"] if n["id"] == payload["active_id"])
    obj = note["drawing"]["objects"][-1]
    return obj["id"], obj["transform"]["translation"]


def main():
    if len(sys.argv) not in (5, 6, 8):
        raise SystemExit(__doc__)
    pid, folder, origin_x, origin_y = (int(sys.argv[1]), Path(sys.argv[2]).resolve(),
                                      int(sys.argv[3]), int(sys.argv[4]))
    gestures = int(sys.argv[5]) if len(sys.argv) == 6 else 6
    if len(sys.argv) == 8:
        gestures = int(sys.argv[5])
    canvas_width = float(sys.argv[6]) if len(sys.argv) == 8 else 689.0
    canvas_height = float(sys.argv[7]) if len(sys.argv) == 8 else 438.0
    if canvas_width <= 0 or canvas_height <= 0:
        raise ValueError("Invalid native image bounds")
    assert 1 <= gestures <= 60
    process = psutil.Process(pid)
    assert process.name().lower().startswith("inkpad")
    enum_proc = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    windows = []

    @enum_proc
    def callback(hwnd, ignored):
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        title = ctypes.create_unicode_buffer(256)
        user32.GetWindowTextW(hwnd, title, 256)
        if owner.value == pid and title.value == "Inkpad Beta":
            windows.append(hwnd)
        return True

    user32.EnumWindows(callback, 0)
    assert len(windows) == 1
    hwnd = windows[0]
    assert (folder / "workspace.json").exists()
    def ensure_foreground(stage, gesture):
        foreground = user32.GetForegroundWindow()
        if foreground != hwnd:
            raise RuntimeError(f"FOREGROUND_STOLEN stage={stage} gesture={gesture} "
                               f"test_hwnd={hwnd} actual_foreground_hwnd={foreground}")
    target_id, first = get_target(folder)
    cursor = wintypes.POINT()
    user32.GetCursorPos(ctypes.byref(cursor))
    cpu_samples, memory_samples, commit_samples = [], [], []
    rss_initial = process.memory_info().rss
    rss_peak = rss_initial
    target_moves = []
    try:
        user32.ShowWindow(hwnd, 9)
        if not user32.SetForegroundWindow(hwnd) or user32.GetForegroundWindow() != hwnd:
            user32.BringWindowToTop(hwnd)
            time.sleep(.15)
            if not user32.SetForegroundWindow(hwnd) and user32.GetForegroundWindow() != hwnd:
                raise RuntimeError("WINDOW_FOCUS_FAILURE: user32 could not focus the isolated test window")
        for gesture in range(gestures):
            ensure_foreground("before_drag", gesture)
            current_id, before = get_target(folder)
            assert current_id == target_id
            direction = 1 if gesture % 2 == 0 else -1
            # Map the saved object centre using the ACTUAL accessibility
            # canvas dimensions. The old hardcoded 689x438 assumption fails
            # after window resizes and display scaling changes.
            start_x = round(origin_x + (before["x"] - first["x"]) * canvas_width / 1100)
            start_y = round(origin_y + (before["y"] - first["y"]) * canvas_height / 700)
            user32.SetCursorPos(start_x, start_y)
            # Wait until Slint has processed the mouse relocation and button
            # press before sending motion. High-rate synthetic transitions can
            # otherwise be coalesced when the GUI thread is busy, yielding a
            # misleading drag distance or missing the target altogether.
            time.sleep(.10)
            rss_before = process.memory_info().rss
            cpu_before = sum(process.cpu_times()[:2])
            user32.mouse_event(0x0002, 0, 0, 0, 0)
            time.sleep(.08)
            ensure_foreground("after_mouse_down", gesture)
            # Stay INSIDE the red target's ~15px screen radius. The previous
            # 24px x/y diagonal exceeded that radius, and may select artwork
            # underneath when Slint coalesces mouse-down/motion events.
            for step in range(1, 13):
                ensure_foreground("move", gesture)
                user32.SetCursorPos(start_x + step * direction, start_y)
                time.sleep(.016)
            commit_start = time.perf_counter()
            user32.mouse_event(0x0004, 0, 0, 0, 0)
            ensure_foreground("after_mouse_up", gesture)
            changed = None
            while time.perf_counter() - commit_start < 5:
                try:
                    _, current = get_target(folder)
                    if current != before:
                        changed = current
                        break
                except (ValueError, OSError):
                    pass
                time.sleep(.004)
            if changed is None:
                try:
                    from PIL import ImageGrab
                    diagnostic = folder / f"failed-gesture-{gesture:02}.png"
                    ImageGrab.grab(window=hwnd).save(diagnostic)
                    print(json.dumps({"failed_gesture": gesture,
                        "expected_start": before,
                        "persisted_after_timeout": get_target(folder)[1],
                        "gui_diagnostic": str(diagnostic),
                        "rss_mib": round(process.memory_info().rss / 1048576, 3)}), flush=True)
                except Exception as diagnostic_error:
                    print(f"GUI diagnostic capture failed: {diagnostic_error}", flush=True)
                raise RuntimeError(f"No persisted Win32 gesture after 5s at step {gesture}")
            delta_x = changed["x"] - before["x"]
            delta_y = changed["y"] - before["y"]
            if delta_x * direction <= 0 or abs(delta_y) > 2:
                raise RuntimeError(f"WRONG_TARGET_OR_MOTION: gesture={gesture}, delta=({delta_x},{delta_y}), direction={direction}")
            elapsed_commit = (time.perf_counter() - commit_start)*1000
            cpu_ms = (sum(process.cpu_times()[:2])-cpu_before)*1000
            rss_after = process.memory_info().rss
            rss_peak = max(rss_peak, rss_after)
            target_moves.append({"before":before,"after":changed})
            cpu_samples.append(cpu_ms)
            memory_samples.append((rss_after-rss_before)/(1024**2))
            commit_samples.append(elapsed_commit)
            time.sleep(.04)
    finally:
        user32.SetCursorPos(cursor.x,cursor.y)
    # Distinguish a permanent process-memory rise from memory released only
    # after Slint's pending frames and save callbacks have gone idle. No new
    # pointer events are injected during this measurement.
    rss_immediate = process.memory_info().rss
    time.sleep(1.5)
    rss_idle = process.memory_info().rss
    private_idle = getattr(process.memory_info(), "private", 0)
    def stats(samples):
        values = sorted(samples)
        return {"p50":round(statistics.median(values),3),
                "p95":round(values[int(.95*(len(values)-1))],3)}
    final_workspace = json.loads((folder / "workspace.json").read_text("utf-8"))
    active_note = next(n for n in final_workspace["notes"] if n["id"] == final_workspace["active_id"])
    print(json.dumps({"gui": "actual Win32 mouse events into Slint", "pid":pid,
        "objects":len(active_note.get("drawing",{}).get("objects",[])),
        "gestures":gestures, "cpu_ms_per_gesture":stats(cpu_samples),
        "mouse_up_to_durable_commit_ms":stats(commit_samples),
        "rss_delta_mib_per_gesture":stats(memory_samples),
        "rss_initial_mib":round(rss_initial/(2**20),3),
        "rss_peak_mib":round(rss_peak/(2**20),3),
        "rss_immediate_mib":round(rss_immediate/(2**20),3),
        "rss_after_idle_mib":round(rss_idle/(2**20),3),
        "private_after_idle_mib":round(private_idle/(2**20),3),
        "rss_final_mib":round(process.memory_info().rss/(2**20),3),
        "object_moves":target_moves}, separators=(",",":")))


if __name__ == "__main__":
    main()
