# Inkpad Beta performance investigation — 29 October 2026

This report concerns disposable synthetic test notebooks. All scene MCP
benchmarks set `INKPAD_DATA_DIR` to a fresh temporary directory and never
open or mutate personal Inkpad notes. Original working-tree edits were retained.

## Reproduction

Windows, Rust release profile (`opt-level = "s"`, thin LTO), warm filesystem
cache, five independent samples per scene size. Run in `D:\chat\inkpad`:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" build --release --bin inkpad-agent --offline --jobs 1
python tests\benchmark_scene_scaling.py target\release\inkpad-agent.exe 5
python tests\scene_atomic_revision_mcp.py target\release\inkpad-agent.exe
```

`tests/benchmark_scene_scaling.py` creates 24, 100, 256, and 512 editable
objects across eight layers. It uses fixed-seed gradients, polygons, circles,
ellipses, and rounded rectangles. Each sample creates a fresh note, creates
the whole scene in one MCP call, renders it twice, undoes it in one step, and
redoes it; identical PNG base64 output is required within each group.

## Before: source checkout prior to this optimisation

Captured as `tools/perf_baseline/inkpad-agent-before.exe` before code edits.
Medians are end-to-end persistent MCP wall times in milliseconds:

| Objects | Create | First PNG | Cached PNG | Undo | Redo |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 24 | 13.997 | 14.064 | 1.070 | 4.462 | 4.314 |
| 100 | 16.550 | 25.680 | 1.758 | 4.899 | 4.718 |
| 256 | 20.215 | 54.565 | 2.950 | 5.891 | 5.577 |
| 512 | 36.477 | 105.631 | 4.464 | 7.415 | 7.718 |

These measurements include OS file locks and a durable save for create/edit.
CPU measurement with `psutil` is also available in the harness, but Windows
reports process CPU in roughly 15.625 ms quanta; treat the wall time medians as
the more useful measurements for individual small operations. The 512-object
first preview consumed approximately 109 ms of reported child-process CPU in
a separate run and used approximately 8.1 MiB process resident memory.

## After: combined release build

Same fixed-seed inputs, five independent trial workspaces per size. The source
was rebuilt with `--release --offline --jobs 1`, then timed end-to-end over
one persistent MCP stdio process per trial. All four PNG outputs exactly match
the corresponding baseline hashes, and the entire scene is still a single
undo/redo step.

| Objects | Create before → after | Create speedup | First PNG before → after | PNG speedup | Cached PNG after |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 24 | 13.997 → 9.797 ms | 1.43× | 14.064 → 9.326 ms | 1.51× | 0.989 ms |
| 100 | 16.550 → 14.085 ms | 1.18× | 25.680 → 15.513 ms | 1.66× | 1.142 ms |
| 256 | 20.215 → 13.495 ms | 1.50× | 54.565 → 28.581 ms | 1.91× | 2.415 ms |
| 512 | 36.477 → 20.091 ms | 1.82× | 105.631 → 54.230 ms | 1.95× | 2.883 ms |

Process RSS during the 512-object preview was about 8.2 MiB. A second run of
the baseline with CPU instrumentation produced approximately 109.4 ms process
CPU for its first 512-object PNG, versus approximately 62.5 ms for the combined
after build; Windows CPU-time granularity makes these approximate.

The renderer also passed an independent in-process synthetic test with dense
on-screen objects, paths, clips and shadows: all eight full-frame RGBA BLAKE3
hashes matched the pre-optimisation renderer. That separate DEBUG build's
best-of-three timings for 512 legacy objects were 1747.141 → 1321.059 ms,
and mixed clipped/path/shadow objects were 1728.392 → 1515.639 ms. Debug
numbers are not release estimates; use the release MCP table above for typical
agent workloads.

## Media garbage collection and notebook save

Isolated 2,200-note / 7.53 MiB workspace with a referenced media PNG.
Direct `Store` benchmark, 14 iterations, median times:

| Operation | Before | After | Speedup |
| --- | ---: | ---: | ---: |
| `prune_unused_media_locked` | 13.405 ms | 0.279 ms | 48.0× |
| Full `Store::mutate` including durable save | 138.332 ms | 91.027 ms | 1.52× |

The optimisation scans live media files before parsing the backup JSON. When
there are no orphan candidates, it returns immediately. If any candidate
exists, the previous backup is still parsed and its references still protect
recoverable attachments. Decode also consumes existing RGBA image memory
instead of always cloning it (`into_rgba8` in place of `to_rgba8`).

The worker reported 5 focused media tests and 16 storage tests passing using
the compiled release harness. The ignored large-notebook benchmark is run
separately using:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test --offline --release --jobs 1 --bin inkpad-agent large_notebook_media_prune_benchmark -- --ignored --nocapture
```

## Additional changes and validation

* Scene creation and batches use constant-time ID collision checks for groups
  of at least 16 objects, retaining the old small-operation path.
* Intermediate vector-pointer events continue applying every geometry edit,
  preserving selection, undo and mouse-up accuracy. Slint frame uploads are
  skipped when the existing 40 ms preview throttle has not refreshed pixels;
  pen/eraser samples and frame uploads remain untouched.
* Renderer changes bound scanline work from actual per-row crossings and cache
  per-object gradient inverse-rotation values, with identical RGBA and PNG
  output across the eight independent dense render fixtures and four MCP
  scene counts.

Python integration checks passed for `scene_atomic_revision_mcp.py`,
`vector_geometry_qa.py` and `advanced_vector_mcp_smoke.py`; those cover stale
edits, atomic late rollback, stable revisions, undo/redo, compound paths,
clipping, shadows, gradients, and PNG export/preview byte equality.
`advanced_vector_positioning_regression.py` also passed with the preserved
baseline executable: legacy scene PNG bytes were identical, and explicit
position and transform errors left the workspace unchanged.

Full Rust regression results with the combined source:

* `cargo test --offline --release --jobs 1 --bin inkpad-agent`: **102 passed,
  0 failed, 4 ignored benchmarks**.
* `cargo test --offline --jobs 1 --bin inkpad`: **103 passed, 0 failed,
  3 ignored benchmarks**. An earlier attempt caught a GUI pointer regression
  in a proposed geometry throttle; that throttle was removed. Only redundant
  Slint image uploads remain throttled, and all 103 GUI tests passed afterward.
* `cargo build --release --bin inkpad --offline --jobs 1`: successful; optimized
  desktop binary at `target/release/inkpad.exe`. This executable was built and
  tested at the unit level, but no physical mouse/UI frame-rate benchmark was
  performed. No installed or packaged Inkpad executable was replaced.

## Remaining performance opportunities

The first preview of a dense 512-object scene still costs about 54 ms in this
fixture. The drawing renderer traverses objects by layer and calculates
compound-path crossings per row. Revision-keyed raster caches may improve
repeated previews but require careful invalidation after stroke, transform,
layer visibility, image export or external edits. GUI vector pointer movement
currently validates and clones full drawing data on each sample. Changing that
path would require a new gesture-state design and dedicated interaction tests;
an attempted event skip was rejected by existing regression coverage.

Full notebook saves remain around 91 ms for the large fixture because the
program intentionally reparses JSON, saves a durable temporary file, retains
the recovery backup and syncs data to disk. Removing any of those durability
properties merely to improve a benchmark would violate Inkpad's storage
contract.

During the *first* pass no measured operation achieved 100×. The second pass
below measured a larger isolated per-object movement speedup; neither figure
should be confused with a 100× end-to-end application improvement.

## Second engineering pass — native GUI, vector editing and image reads

This section extends the first-pass baseline. Sources already modified by
earlier work, including `AGENTS.md`, the vector renderer, notebooks, art files
and existing skill instructions, were preserved. The installed application
and personal workspaces were never opened or replaced. Benchmarks use
`target/perf-gui-*` with `INKPAD_DATA_DIR` set explicitly for each child process.

### In-memory single-object edits (release, 160 moves per scene)

The original `move_object` delegated to `update_object`, which cloned *every*
object and validated the whole drawing. The new translation path finds the
selected object, checks its locked layer and validates its new coordinates
before changing anything. Undo still records one snapshot per gesture and
pointer events remain immediate. Separate shape-update edits now clone the
single affected object, commit only after full-scene validation and restore
the original on error.

| Scene size | Previous move p50 / p95 | New move p50 / p95 | p50 improvement |
| ---: | ---: | ---: | ---: |
| 24 | 0.00740 / 0.00980 ms | 0.00010 / 0.00010 ms | 74× |
| 100 | 0.03690 / 0.03790 ms | 0.00030 / 0.00050 ms | 123× |
| 256 | 0.09360 / 0.10640 ms | 0.00050 / 0.00060 ms | 187× |
| 512 | 0.17990 / 0.20110 ms | 0.00110 / 0.00120 ms | 164× |

These are *per-call engine measurements*, not full paint or save latency.
The 512-object `update_object` path also improved from 0.20230 ms p50 /
0.23660 ms p95 to 0.12180 ms p50 / 0.14790 ms p95 by avoiding an entire
scene clone; this path retains full-scene validation.

To reproduce the two ignored benchmarks (release optimizations matter):

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test --release --offline --jobs 1 --bin inkpad-agent pointer_move_scaling_benchmark -- --ignored --nocapture
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test --release --offline --jobs 1 --bin inkpad-agent pointer_shape_scaling_benchmark -- --ignored --nocapture
```

### Actual Win32 GUI drag and durable commit

The test launches the standalone release desktop binary against an isolated
MCP-created notebook with 24, 100, 256 or 512 editable objects across eight
layers and a red selectable marker. Real Windows UI Automation invokes
Draw/Select; a hardware-style Windows mouse stream moves the marker through
12 steps at 60 Hz per gesture, alternates direction and verifies actual saved
coordinates in `workspace.json`. Each measurement reports six complete
gestures (small sample; p95 should be considered indicative). It measures
process CPU in 15.625 ms Windows quanta and mouse-up to observed durable save.

An initial run after adding the prefix cache but before selecting a cache
threshold produced these results:

| Objects | CPU p50 before → after | Commit p50 before → after | Commit p95 before → after |
| ---: | ---: | ---: | ---: |
| 24 | 31.25 → 54.69 ms | 44.32 → 49.04 ms | 46.23 → 50.41 ms |
| 100 | 62.50 → 62.50 ms | 26.75 → 27.29 ms | 46.06 → 46.00 ms |
| 256 | 117.19 → 31.25 ms | 55.32 → 26.68 ms | 59.02 → 29.24 ms |
| 512 | 179.69 → 109.38 ms | 69.75 → 31.75 ms | 84.96 → 44.75 ms |

The initial cache was slower for small scenes, so the final code enables
cached raster prefixes only when at least 128 *visible objects precede the
selected object in actual drawing order*. Earlier selections and small
documents continue using the original renderer. The cache is invalidated on
page reload, complete redraw and pointer release. A dedicated GUI Rust
regression compares every RGBA byte with a complete rerender after several
movements of selected objects near the beginning and end of a dense layered
scene, including gradients, transparency, clipping and shadows.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File tests\benchmark_gui_sustained.ps1
```

This script builds uniquely named disposable workspaces, exercises the
preserved previous-release GUI binary and current GUI binary, checks real
on-disk movement and closes its own test processes. No installer is touched.

### Image-heavy notebook

An independent synthetic notebook contains 2,200 notes (7.04 MiB JSON) and
six 600×400 PNG images (721,096 bytes each source), with Unicode text.
Before extending the exact-byte read cache to image-list metadata, the
combined release MCP measurements were:

| Operation | p50 | p95 |
| --- | ---: | ---: |
| Compact note read | 2.215 ms | 2.887 ms |
| Search | 2.564 ms | 3.746 ms |
| Image insert | 68.968 ms | 71.982 ms |
| Image placement | 48.171 ms | 49.939 ms |
| Image metadata list | 11.757 ms | 14.373 ms |
| HTML export | 39.493 ms | — |

The agent used about 23.0 MiB RSS after the image-heavy operations; total
measured CPU was approximately 750 ms. An image metadata read now uses
`Store::read_cached`, which holds a validated snapshot *only while the full
primary JSON bytes match*. Every external edit, including equal-size changes,
invalidates it. The previous backup-recovery path is retained on corruption.

```powershell
python tests\benchmark_image_heavy.py target\release\inkpad-agent.exe
```

These results do not imply a 100× overall application speedup. Image imports,
durable notebook writes and the software renderer remain real costs. The
selected-object cache is a gesture-local optimization; it is not persisted
across undo, page changes, external modifications or an interrupted gesture.

### Final GUI build: native input comparison (29 October 2026)

After fixing the mouse-up redraw when the pointer stops at the same position,
removing the redundant undo history clone and requiring at least 128 *preceding
visible* objects to enable the raster cache, the final release executable was
tested again against the same preserved before-build executable. Raw output:
`target/perf-gui-20261029T065642-results.json`.

| Objects | CPU p50 before → after | CPU p95 before → after | Save p50 before → after | Save p95 before → after |
| ---: | ---: | ---: | ---: | ---: |
| 24 | 78.13 → 62.50 ms | 78.13 → 109.38 ms | 48.35 → 50.87 ms | 51.22 → 51.28 ms |
| 100 | 54.69 → 23.44 ms | 78.13 → 46.88 ms | 14.31 → 45.01 ms | 45.45 → 50.81 ms |
| 256 | 101.56 → 93.75 ms | 125.00 → 109.38 ms | 52.02 → 28.82 ms | 73.82 → 33.50 ms |
| 512 | 156.25 → 70.31 ms | 156.25 → 78.13 ms | 74.49 → 53.07 ms | 93.50 → 54.49 ms |

These are real Windows mouse events, verified against the saved coordinates of
the selected test object after every drag. Six gestures is a small sample.
Timings for 24/100 objects are noisy and do not uniformly improve; avoid
claiming universal performance gains on small drawings.

A further **30-gesture sustained 512-object** run is available at
`target/perf-gui-20261029T065827-results.json`. The same input moves the
marker alternately left/right and verifies durable JSON after every mouse-up:

| Metric | Before | After | Change |
| --- | ---: | ---: | ---: |
| Gesture CPU p50 | 171.875 ms | 109.375 ms | 1.57× faster |
| Gesture CPU p95 | 218.750 ms | 140.625 ms | 1.56× faster |
| Mouse-up to durable commit p50 | 97.676 ms | 48.419 ms | 2.02× faster |
| Mouse-up to durable commit p95 | 113.216 ms | 57.687 ms | 1.96× faster |
| Process RSS initial | 120.883 MiB | 120.496 MiB | Similar |
| Process RSS peak | 142.840 MiB | 142.555 MiB | Similar |
| Process RSS final | 142.836 MiB | 142.555 MiB | Similar |

The approximately 22 MiB increase in RSS during repeated GUI operations is
**not reduced** by these changes. These numbers are Windows process RSS, not
instrumented total allocation counts. A longer allocator profile would be
needed to establish whether the memory high-water mark is a leak, a reusable
pixel buffer, or allocator retention. The observed test does not show runaway
growth or establish long-term stability.

### Final image-heavy before/after and external-edit verification

The two release binaries used the same generator and separate disposable
2,200-note notebooks, each with six 600×400 PNGs. Each ran twelve repeated
read/search/list requests and six image insert/placement operations.

| Operation | Before p50 / p95 | After p50 / p95 |
| --- | ---: | ---: |
| Compact note read | 2.428 / 3.274 ms | 2.374 / 3.618 ms |
| Search | 3.064 / 3.950 ms | 2.995 / 4.212 ms |
| Image import | 88.431 / 91.770 ms | 73.584 / 92.047 ms |
| Image placement | 63.194 / 66.510 ms | 52.148 / 53.074 ms |
| Image metadata list | 12.180 / 14.299 ms | 5.980 / 6.696 ms |
| HTML export, single run | 50.065 ms | 56.566 ms |
| RSS after workload | 23.559 MiB | 23.816 MiB |

Image list metadata is 2.04× faster at p50 and 2.14× faster at p95 in this
workload. HTML export was slower in this single sample; further profiling is
needed before advertising any HTML export improvement. The existing Rust
media test was extended to replace a caption with a **same-length external
edit**, then checks that the cached image list immediately sees the new value.

### Final preserved scene-engine regression

The last five-repetition release MCP run (identical fixed-seed 24/100/256/512
artwork, isolated workspaces, atomic undo/redo and stable PNG base64 SHA-256)
returned the following medians; the earlier renderer optimizations remain:

| Objects | Scene creation | First PNG | Cached PNG | Undo | Redo |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 24 | 11.344 ms | 13.494 ms | 1.059 ms | 4.113 ms | 3.687 ms |
| 100 | 12.056 ms | 15.870 ms | 1.531 ms | 4.071 ms | 3.686 ms |
| 256 | 16.742 ms | 31.318 ms | 2.387 ms | 4.971 ms | 4.496 ms |
| 512 | 19.926 ms | 54.212 ms | 3.229 ms | 5.555 ms | 5.021 ms |

### Final test status and scope

* `cargo test --offline --jobs 1 --bin inkpad`: **104 passed,
  0 failed, 5 ignored**. Includes selected drag prefix-vs-full RGBA byte
  comparisons, pointer mouse-up preview correctness, drawing history and
  layer controls.
* `cargo test --release --offline --jobs 1 --bin inkpad-agent`: **102 passed,
  0 failed, 6 ignored**. Includes same-size external primary JSON edit and
  media backup recovery, malformed vector edits and scene atomicity.
* `scene_atomic_revision_mcp.py`: **PASS**, 52.14× fewer cumulative MCP
  latency for the 24-object batched edit compared with 24 individual edits.
* `advanced_vector_mcp_smoke.py`, `vector_geometry_qa.py` and
  `advanced_vector_positioning_regression.py`: **PASS**. Verified paths,
  gradients, clips, shadows, PNG export matching preview, invalid-edit
  rollback and legacy export SHA-256 equality with the first baseline binary.
* Windows `--release --offline --jobs 1` builds for `inkpad.exe` and
  `inkpad-agent.exe` completed successfully. `git diff --check` is clean;
  repository line-ending warnings are informational.

The installed distribution and existing personal notebooks were not changed.
`target/release/inkpad.exe` and `target/release/inkpad-agent.exe` are standalone
newly built files. The long-term image cache and full-scene clone during
complex group operations remain plausible next bottlenecks; the validated
gesture cache is deliberately short-lived and has no cross-revision validity.

## Third pass — targeted remaining costs

The previous release desktop executable was preserved independently before
further changes at `tools/perf_baseline/inkpad-before-thirdpass.exe`. All tests
continue using uniquely named temporary workspaces; the installed Inkpad app
and user notes remain untouched.

### Expensive compound paths with clipping and blurred shadows

Independent graphics profiling found that a shadowed compound path was
flattened/transformed/clipped and supersampled again after rendering its
shadow. The renderer now retains bounded, temporary per-pixel fill/stroke
sample coverage for the shadow operation and composites identical fill and
stroke pixels without rerunning the same path geometry. This cache lives only
within one render and cannot become stale across scene revisions.

The following are debug synthetic wall-clock median/p95 timings (ms) from
the same deterministic 24, 100, 256 and 512 object fixtures before and after
the change; these are *not* release GUI or MCP numbers:

| Objects | Before p50/p95 | After p50/p95 | Median gain |
| ---: | ---: | ---: | ---: |
| 24 | 63.926/65.499 | 55.161/56.472 | 1.16× |
| 100 | 279.565/330.753 | 232.540/235.883 | 1.20× |
| 256 | 812.780/892.657 | 641.261/757.281 | 1.27× |
| 512 | 1700.031/1756.092 | 1345.212/1469.661 | 1.26× |
| 512 objects, 64 layers | 1720.020/1749.005 | 1390.159/1521.103 | 1.24× |

Five changing 512-object drag frames improved from 1757.284 to 1478.104 ms
median (1.19×), with variability. Identical full RGBA BLAKE3 hashes were
verified for all five static scenes and all five changing drag frames. Layer
traversal by itself contributed only around 20 ms (about 1%) of the dense
debug scene cost; a layer traversal rewrite was not justified.

### Removing extra full-frame allocations and notebook clones

`Board::copy_pixels_from` copies a fixed-size RGBA frame into its existing
allocation. Cached selection drags reuse the `App.board` pixel vector rather
than cloning a fresh 3,080,000-byte buffer for every rendered frame. Ordinary
`refresh_drawing()` now also replays strokes and redraws editable objects into
the existing buffer rather than allocating another 3.08 MB vector. The GUI
RGBA-equality regression additionally checks that the buffer pointer remains
unchanged across both full and cached redraws. Slint's image upload is still
a separate buffer; this change does not claim to eliminate that upload.

The GUI `persist()` previously cloned both the entire local notebook and its
comparison baseline before attempting a transactional save. It now borrows
both immutably during the existing cross-process lock/merge, preserving exact
three-way conflict protection and leaving the on-screen copy intact if the
write fails. Once the merge succeeds, the app still retains independent local
and baseline workspaces for future conflict detection. This removes two full
notebook clones per GUI save without sacrificing crash recovery or locking.

An opt-in `large_notebook_gui_snapshot_benchmark` constructs 2,200 notes
and isolates the former two-clone overhead; these measurements represent
removed allocation work rather than a claim about total fsync duration.

### Large-notebook GUI automation limitation

The previous 100-object save-latency regression is not consistently observed
across earlier six-gesture runs: the prior baseline varies between about
14 and 27 ms median for the same object count, depending on GUI/OS load. A
new 2,200-note (6.60 MiB) GUI fixture exposed an independent benchmark
problem: after invoking UI Automation's Select button, Win32 pointer events
sometimes failed to move the target (failure at either the first gesture or
the twelfth). This is a test failure, not a valid latency sample. The harness
now requires the GUI's visible `Objects: ... select & drag` status before
starting each measured run; a failed assertion is reported, not counted as a
performance improvement. Large-notebook save latency must remain unconfirmed
until this test can complete reliably; no production bug was inferred from
the failed automation alone.

### Third-pass final measurements and verification

The finished combined source was compiled with
`cargo build --release --offline --jobs 1 --bin inkpad --bin inkpad-agent`
and produced `target/release/inkpad.exe` (29 October, 07:18) and
`target/release/inkpad-agent.exe` (07:13). Both are newer than their affected
source files. No installed release was replaced.

Five new repetitions of the unchanged fixed-seed MCP scene benchmark against
the combined agent binary produced the following release medians. Every count
matched the prior PNG SHA-256, the whole scene remained one atomic undo/redo
step, and there were no user-workspace reads:

| Scene objects | Create | First PNG | Cached PNG | Undo | Redo |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 24 | 7.986 ms | 7.483 ms | 0.579 ms | 2.571 ms | 2.488 ms |
| 100 | 8.969 ms | 12.759 ms | 1.026 ms | 2.856 ms | 2.787 ms |
| 256 | 10.026 ms | 26.072 ms | 1.720 ms | 3.277 ms | 3.291 ms |
| 512 | 17.887 ms | 46.650 ms | 2.546 ms | 4.549 ms | 4.457 ms |

The previous agent's 512-object first PNG median was 54.212 ms. The new one
is 46.650 ms (1.16× within these separate release runs), but the independent
same-process dense-complex-path debug benchmark above is the stronger evidence
for the targeted shadow coverage optimisation. Changes in machine contention
and filesystem cache can move MCP wall times, particularly for the small
scenes. PNG hashes, rather than inferred visual similarity, establish output
fidelity.

The targeted 2,200-note synthetic GUI snapshot benchmark (6.542 MiB JSON,
31 samples, debug build) measured **4.699 ms p50 / 10.789 ms p95** for the two
whole-workspace clones removed from `App::persist`. These are CPU/allocation
overhead measurements, **not** a verified 4.7 ms reduction in the complete
durable-save wall time. The rest of the save still parses, merges, serializes,
fsyncs, copies a backup and replaces the primary atomically.

The new graphics and buffer code passed **106 desktop Rust tests (0 failed,
7 ignored benchmarks)**, including untouched-buffer-pointer identity and
pixel-equality cases for both cached and complete redraws. **102 release
agent Rust tests (0 failed, 6 ignored)** passed. Re-run Python integration
checks for complex paths, clips, shadows, gradients and positioning passed;
`advanced_vector_positioning_regression.py` verified identical legacy PNG
SHA-256 against the preserved first baseline executable and atomic rejection
without JSON modification. `git diff --check` passed (Windows LF/CRLF warnings
only).

### GUI latency, process memory and caution about small scenes

Native Windows input experiments against the preserved second-pass desktop
binary were additionally attempted at 24/100/256/512 objects. The third-pass
short-sample results **do not establish a uniform GUI speedup**. One complete
12-gesture 24-object pair showed median gesture CPU increasing from 46.875
to 78.125 ms, and median mouse-up-to-save increasing from 8.031 to
10.472 ms. A separate complete six-gesture 100-object pair showed CPU
decreasing 125.0 to 54.688 ms but save increasing 11.293 to 16.220 ms.
Six-gesture 256-object CPU increased 62.5 to 78.125 ms, while the 512-object
pair increased 78.125 to 93.75 ms. Other earlier runs varied materially.
These results warrant caution; they cannot justify claiming a third-pass
overall GUI improvement. The original second-pass sustained 512-object gains
remain separately documented above.

Several longer Win32 automation runs were **invalidated** by a missed drag,
even though an accessibility query showed Select mode and the test marker
was visible. Misses occurred in both preserved and current executables.
One attempt also failed to obtain foreground-window focus. The test harness
now saves `failed-gesture-*.png` to the disposable fixture for diagnosis and
requires an explicit GUI Select status; unsuccessful runs are never included
as latency samples. The evidence does not isolate a persistent Inkpad bug,
but sustained GUI input reliability still needs independent user-driven
validation on an otherwise idle desktop. The 100-object save-time regression
cannot be confidently assigned to the new code given this measurement noise.

Native idle-RSS samples remained around 135 MiB at 24/100 objects and
137–140 MiB with 512 objects; holding the app idle for 1.5 seconds did not
fully return RSS to its startup value. Reusing the 3.08 MB board allocation
avoids allocation *frequency* but did not establish elimination of the
previously observed ~22 MiB process high-water growth. Rust allocator or
Slint/GPU retention must be profiled separately before calling it a leak or
claiming a memory-usage improvement. No evidence supports 100× faster full
GUI painting, full durable saves, or memory reclamation.

The integrated feature set, atomic edits, exact object IDs, revision guards,
undo history, rich Unicode text, image references, gradients, shadows, clips,
legacy ink and PNG export were retained. All existing working-tree and art
files were preserved. The next warranted investigation would be a native
per-frame CPU/heap profile with a stable desktop input harness, plus a
separately controlled disk/fsync trace for large notebook saves; repeating
the noisy short GUI measurements without that instrumentation would not be
productive.

## Fourth pass — durable JSON, allocation pressure and native input diagnostics

### New evidence: streaming and syncing a large notebook

`Store::write_unlocked_with_primary_validity` formerly allocated a full output
`Vec<u8>` before writing it to the durable temporary file. A 20-trial,
alternating-order release benchmark compared that path to streaming compact
JSON through a 128 KiB `BufWriter`. Both wrote **identical** 9,205,949-byte
JSON from the same 2,200-note synthetic Unicode workspace and called
`File::sync_all`.

| Serialization and synced temp write | Original Vec | Streamed writer |
| --- | ---: | ---: |
| p50 | 32.123 ms | 29.001 ms |
| p95 | 61.403 ms | 35.222 ms |
| Temporary output buffer | at least 9,205,949 bytes | 131,072 bytes |

The streamed writer was **1.11× faster p50 and 1.74× faster p95** for that
stage. It avoids a second full-size temporary output allocation. This is
*not* a demonstrated 1.74× gain for the complete notebook save, which still
includes validating rich text, reading/parsing primary JSON, merge handling,
copying a recovery backup, file locking and atomic replacement. All those
durability steps remain in place.

Reproduce the serializer comparison with:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test --offline --release --jobs 1 --bin inkpad-agent compare_atomic_json_serializers_benchmark -- --ignored --nocapture
```

### Native GUI measurement investigation

The earlier test target shared layer 7 with overlapping artwork, and the
mouse driver assumed the canvas was always 689×438 physical pixels. The
fixture now uses a dedicated ninth, topmost layer for the test circle;
screen-to-canvas conversion uses actual UI Automation bounds. The drag stays
within the circle, failed gestures record screenshots, and direction/vertical
movement is checked before a sample counts as valid. Foreground-window
failures remain explicit and excluded from latency statistics.

One run with the corrected top-layer fixture still recorded a displacement
of (+2,-8) logical drawing units during a commanded horizontal movement.
That establishes **an unresolved input-mapping/event-order discrepancy**;
it does not establish whether it originates in Win32 automated input timing,
Slint `TouchArea` event coordinates or Inkpad's handler. Additional invalid
gestures cannot be used as proof of either a performance regression or speedup.
The 100-object GUI save regression remains inconclusive rather than a
confirmed production defect.

### Sustained-session memory attribution

The earlier approximately 22 MiB RSS high-water increase remains unexplained
by direct allocation stacks. Inkpad intentionally retains up to 48 undo
snapshots, a 3.08 MB board buffer, a temporary cached render prefix while
dragging, and Slint render/display memory. Such bounded retention could
explain some of the high water, but no heap-profile evidence distinguishes
allocator retention from a leak in the remaining bytes. Streaming JSON now
avoids about 9.2 MB of *transient* allocation in this specific fixture, which
must not be reported as a measured 9.2 MB reduction in process RSS.

### Additional sidebar and GUI investigation

Profiling the GUI source revealed a separate cost: each completed drag in
selection mode called `update_rows`, which sorted all visible notes, cloned
titles and previews, built a new 2,200-row Slint model and replaced the
previous model even though moving an object changes none of the sidebar's
visible fields. A new 51-sample debug benchmark using 2,200 synthetic Unicode
notes measured the pure `sidebar_rows()` build at **12.0405 ms p50 and
14.8846 ms p95**, without including the subsequent Slint model upload.

The drawing callback now skips the sidebar rebuild when a Select gesture
finishes with the same `Note.updated` timestamp. The timestamp is recorded in
seconds; the first drag following a time change still refreshes the sidebar,
preserving the pinned/last-edited sort. Pen, eraser and geometry creation still
refresh so stroke/shape counts stay accurate. The new unit regression checks
all these cases. This removes approximately 12 ms of pure model-building work
*per skipped rebuild* in the measured 2,200-note fixture; a full native GUI
wall-time speedup has not been established.

Reproduce the isolated model cost with:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test --offline --jobs 1 --bin inkpad large_notebook_sidebar_model_benchmark -- --ignored --nocapture
```

An actual Windows 100-object GUI comparison was repeated in both binary
orders. First, `before → after` gave **21.593 → 46.482 ms** median mouse-up
to durable commit (six gestures). Reversing the order with twelve gestures
gave **after 21.214 ms, before 37.952 ms**. The direction of the supposed
regression reversed with test order, strongly indicating uncontrolled timing
variation; no reproducible 100-object save regression can be confirmed from
these short runs. A successful 512-object six-gesture comparison yielded
CPU p50 **93.75 → 62.5 ms** while durable-commit p50 stayed essentially
unchanged (**46.203 → 46.311 ms**), but six measurements are too few to
generalise to sustained editing performance.

The 2,200-note GUI automation attempt failed before the first drag. Its
diagnostic `failed-gesture-00.png` captured the Windows desktop and other
applications rather than Inkpad, giving concrete evidence of foreground
interference or invalid window capture during the test. The Python driver
now checks the actual foreground HWND before, during and after every drag;
focus losses are classified as `FOREGROUND_STOLEN`, not measured latency.
Previously reported missed drags and off-axis input remain untrustworthy
as application defect reports without a controlled exclusive desktop session.

One isolated 512-object fixture serialized its vector drawing to 213,459
JSON bytes; forty-eight equivalent serialized snapshots would occupy about
9.77 MiB even before accounting for Rust object-layout overhead. Inkpad's
undo history is bounded to 48 snapshots, providing a plausible source of
bounded memory high water. It does not by itself establish where the entire
~22 MiB observed RSS rise went. No verified leak fix or sustained-memory
reduction is claimed.

The final combined Rust source passed **107 desktop tests (0 failed, 9
ignored)** and **104 agent tests (0 failed, 8 ignored)**. The Windows release
builds completed: `target/release/inkpad.exe` and
`target/release/inkpad-agent.exe`. The final image-heavy MCP workflow on a
2,200-note workspace passed: image insert p50 **67.507 ms**, image placement
p50 **48.675 ms**, image metadata list p50 **6.501 ms** and HTML export
**45.752 ms** in one run; compare with the previous baseline cautiously
because the six-image import/export workload is sensitive to OS I/O.
Atomic drawing revisions, single-step batching, unchanged PNG preview/export
hashes and crash-recovery properties passed the existing Rust and Python
regressions. The installed Inkpad distribution and personal notebooks were
not changed.

## Final build reconciliation — 29 October 2026

The disagreement between an earlier **"desktop still linking"** update and
the later **"both builds complete"** report is resolved. The former was an
intermediate observation. The actual filesystem contains two valid Windows
PE release executables with timestamps later than the source code included
in them:

| Release executable | UTC build timestamp | SHA-256 |
| --- | --- | --- |
| `target/release/inkpad-agent.exe` | 07:43:17 | `9852a279773d9056271e23f1db0ff37021fd3a30e644c10456a10f5dd0e9df77` |
| `target/release/inkpad.exe` | 07:49:45 | `52c865f4c8083c2454c577f892c5bcbab6fb93499e8739b2912871b80d38bd95` |

`src/store.rs` was last changed at 07:36:29 UTC and `src/main.rs` at
07:45:21 UTC. Both binaries have the expected `MZ` Windows executable
header. The recorded release build command exited successfully. There is no
remaining Inkpad compilation; the active Cargo/rustc processes were checked
and belong to the separate YouTube Downloader project. The live Inkpad MCP
agent is still the pre-existing distribution executable under
`dist/Inkpad-Beta-Images-Staged/`. Neither that installed copy nor personal
notebooks was replaced.

**Final validation:** The combined source passed 107 desktop Rust tests and
104 agent Rust tests with zero failures, excluding intentional ignored
benchmarks. `git diff --check` passed. After the final serializer change,
the release agent also passed the Python `advanced_vector_mcp_smoke.py`,
`advanced_vector_positioning_regression.py` and `vector_geometry_qa.py`
integration suites. They verified complex path commands, gradients,
clipping, shadows, preview/export PNG SHA-256 equality, legacy output
pixel identity against `tools/perf_baseline/inkpad-agent-before.exe`, and
atomic invalid-edit rollback with unchanged workspace hashes. These suites
used isolated temporary workspaces and test artifacts.

**Interpretation of performance evidence:** The existing 48× media cleanup,
1.82× scene creation, 1.95× release rendering, 164× isolated movement and
1.26× dense complex-path debug rendering improvements are specific to the
benchmarked operations. The later 1.11× median / 1.74× p95 streaming JSON
result covers serialization and the synced temporary file write of a
9.2 MB notebook, not the entire durable-save transaction. Skipping an
unchanged sidebar rebuild avoids a measured 12.04 ms median of model
construction in a 2,200-note debug fixture; a matching end-to-end GUI
speedup has not been established.

The native GUI comparisons produced mixed short samples. In particular,
100-object mouse-up-to-save results reversed direction when the binary
testing order was reversed. Additional runs were invalidated by missed
drags and foreground interference. No uniform GUI speedup or confirmed
100-object save defect can be claimed. The approximately 22 MiB sustained
RSS high-water increase remains unattributed: bounded undo snapshots,
retained raster buffers and allocator/framework behavior are hypotheses,
not verified explanations. There is no proof of a memory leak or a memory
reclamation improvement. This closes the authorized mission without
repeating already established benchmarks or changing the installed app.
