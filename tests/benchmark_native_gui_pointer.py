"""Measure actual Win32 pointer interactions against a disposable Inkpad GUI.

Usage: python tests/benchmark_native_gui_pointer.py PID WORKSPACE_DIR X Y
Requires the Inkpad process to be launched with INKPAD_DATA_DIR=WORKSPACE_DIR,
Draw mode and Select active, and X/Y inside a visible test object's bounds.
Does not start or modify the installed application. Avoid using while the
computer owner is actively controlling the mouse.
"""

import ctypes
from ctypes import wintypes
import json
from pathlib import Path
import psutil
import statistics
import sys
import time

MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
user32 = ctypes.windll.user32
user32.SetCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
user32.GetCursorPos.argtypes = [ctypes.POINTER(wintypes.POINT)]
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]


def current_scene(directory):
    workspace = json.loads((directory / "workspace.json").read_text("utf-8"))
    active = next(note for note in workspace["notes"]
                  if note["id"] == workspace["active_id"])
    return active["drawing"]["objects"][-1]["transform"]["translation"]


def main():
    if len(sys.argv) != 5:
        raise SystemExit(__doc__)
    pid, directory, x, y = int(sys.argv[1]), Path(sys.argv[2]).resolve(), int(sys.argv[3]), int(sys.argv[4])
    assert directory.is_dir() and (directory / "workspace.json").is_file()
    process = psutil.Process(pid)
    assert process.name().casefold() == "inkpad.exe"
    hwnd = process.create_time()  # checked before interaction
    del hwnd
    # Read the actual process's top-level Inkpad window, not a guessed handle.
    wins = []
    enum_proc = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)

    @enum_proc
    def callback(hwnd, extra):
        win_pid = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(win_pid))
        if win_pid.value == pid and user32.IsWindowVisible(hwnd):
            title = ctypes.create_unicode_buffer(256)
            user32.GetWindowTextW(hwnd, title, len(title))
            if title.value == "Inkpad Beta":
                wins.append(hwnd)
        return True

    user32.EnumWindows(callback, 0)
    assert len(wins) == 1, wins
    hwnd = wins[0]
    old_cursor = wintypes.POINT()
    assert user32.GetCursorPos(ctypes.byref(old_cursor))
    before = current_scene(directory)
    prior_cpu = sum(process.cpu_times()[:2])
    prior_rss = process.memory_info().rss
    timings = []
    try:
        user32.ShowWindow(hwnd, 9)
        assert user32.SetForegroundWindow(hwnd)
        time.sleep(.18)
        assert user32.GetForegroundWindow() == hwnd
        user32.SetCursorPos(x, y)
        time.sleep(.08)
        user32.mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
        time.sleep(.03)
        for index in range(1, 13):
            start = time.perf_counter_ns()
            user32.SetCursorPos(x + index * 2, y - index * 2)
            # 60Hz dispatch, rather than a tight synthetic event burst.
            time.sleep(.016)
            timings.append((time.perf_counter_ns() - start) / 1e6)
        user32.mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
        time.sleep(.3)
    finally:
        user32.SetCursorPos(old_cursor.x, old_cursor.y)
    after = current_scene(directory)
    cpu_delta = max(0.0, 1000 * (sum(process.cpu_times()[:2]) - prior_cpu))
    timings.sort()
    result = {
        "pid": pid, "window_hwnd": hwnd, "real_win32_input": True,
        "object_before": before, "object_after": after,
        "object_changed": before != after,
        "pointer_dispatch_p50_ms": round(statistics.median(timings), 3),
        "pointer_dispatch_p95_ms": round(timings[int(.95 * (len(timings)-1))], 3),
        "cpu_ms_including_render_and_save": round(cpu_delta, 3),
        "rss_before_mib": round(prior_rss / (2**20), 3),
        "rss_after_mib": round(process.memory_info().rss / (2**20), 3),
    }
    print(json.dumps(result))
    if not result["object_changed"]:
        raise SystemExit("FAIL: actual pointer input did not move the test object")


if __name__ == "__main__":
    main()
