# Inkpad Beta — independent headless rich-text QA

**Scope:** 2026-10-14; production source reviewed read-only. Only `tests/rich_text_qa.ps1` and this QA report are authored by the independent reviewer. No GUI/window, browser, focus, mouse, clipboard or physical input was used.

## Reproduce

From `D:\chat\inkpad`, build the **headless agent** (not the desktop) and run:

```powershell
cargo build --offline --bin inkpad-agent
powershell -NoProfile -ExecutionPolicy Bypass -File tests\rich_text_qa.ps1 -AgentExe D:\chat\inkpad\target\debug\inkpad-agent.exe -RequireRich -RequireStyledHtml -RequireSelectionMcp
```

The test creates a fresh GUID-named `INKPAD_DATA_DIR` beneath `dist\rich-text-qa-runs`; it never reads the user's normal Inkpad directory. It starts only two hidden, persistent MCP stdio agent processes, plus a third isolated legacy-compatibility session, and closes its own processes at completion. It retains fixture evidence.

## Observed results

**17/17 headless scenarios PASS**, run under current debug agent, with Unicode, format, settings, media and corruption exercised across real MCP requests (strict `-RequireRich -RequireStyledHtml -RequireSelectionMcp`):

| Contract | Evidence |
| --- | --- |
| Unicode text | RTL, Devanagari, CJK, Cyrillic, emoji ZWJ, combining mark and newline survive two agent processes |
| Style intervals | Overlapping bold/foreground/size/family runs compose canonically; repeated identical formatting does not duplicate runs |
| Validation | Wrong offset unit, empty/out-of-bounds intervals, invalid colour/size, unsupported italic rejected without partial mutation |
| Editing | Combining-accent replacement, emoji insertion, clearing subranges and range rebasing preserve text and surviving spans |
| Media parity | WebP picture, drawing, image width and Unicode caption survive unrelated rich formatting and MCP edits |
| Theme | Light/dark/system preference, sidebar setting, simultaneous cross-process updates and invalid value rejection |
| Concurrency | 20 interleaved appends plus five format/append overlaps from two independent MCP processes preserve all markers |
| Persistence | Restart, previous-save backup, malformed JSON, invalid saved rich interval and old workspace lacking rich fields |
| Settings recovery | Broken settings primary recovers its valid backup; double-corrupt files reject updates without overwriting evidence |
| HTML fidelity | Self-contained HTML includes escaped body, embedded PNG, and per-range bold, foreground colour, font size and family |
| Selection MCP parity | Atomic batch formatting, grapheme-safe copied/pasted Unicode with source font defaults, stale selection guards and no OS clipboard access |

The latest independent Rust unit suites also ran headlessly: **agent 46/46 PASS** and **desktop domain 49/49 PASS**. These are source/domain tests, not an assertion of native GUI behavior.

The separate existing `tests/media_qa.ps1` integration suite was also rerun against the same headless agent: **14/14 PASS** (valid/invalid image import, MIME validation, approval/path safety, HTML/CSP, image metadata, decoding limits, cross-process writes, restart and backup-aware garbage collection). The combined independent integration evidence is **31/31 headless scenarios PASS**; the suites each use their own GUID-isolated notebook.

## Remaining verified gaps and limitations

1. **Formatted preview now has a more complete read-only rendering path.** A new `src/visual_preview.rs` produces per-range font-size/family/weight/colour spans, with Unicode-preserving tests. `src/main.rs::append_styled_segment` remains as native StyledText fallback for bidi and split grapheme content and does not translate size/family in that fallback. Actual visual spacing and line wrapping are not certified without screenshots.
2. **Selection became available using Slint's undocumented testing properties.** The new editor adapter reads native selection UTF-8 bytes and converts them to Unicode-scalar ranges; manual numeric offsets are now behind an advanced disclosure. This is a source-level improvement, not verified live mouse selection. TextInput still displays uniform styling while the separate StyledText Preview shows mixed bold/colour. Per-range size/family are not displayed in that preview.
3. **Repeat-press behavior is not certified.** The static `StepButton` uses 360 ms initial delay, then 75 ms repeat while held. This cannot be verified by headless tests, and no GUI interaction is authorized.
4. **Live GUI↔MCP behavior remains untested.** Two independent MCP processes do exercise the cross-process storage and settings locks. They do not prove caret state, viewport layout, theme animation, or conflict recovery in an open GUI.

**Resolved during review:** `UiSettings::load` now acquires the settings lock, uses a private unlocked reader inside locked updates, and recovers from a valid `ui-settings.json.bak`. The earlier temporary default-theme read race is addressed in the current source. New headless tests verify backup recovery. The HTML exporter now serializes rich runs into escaped spans with validated CSS, and the strict standalone integration test verifies all four style attributes.

## Evidence-based quality judgment

**Headless storage and MCP rich-text correctness: 8.5/10, high confidence within the tested flows.** The range representation is canonical and deterministic, Unicode edits have defensible semantics, durability safeguards recover corrupt data, exports retain rich styles, and multi-agent edits did not lose changes. This does not certify exhaustive fuzzing, power-loss atomicity, or manual user experience.

**Full rich-text user experience: no numerical score issued.** Native selection, styling preview fidelity, keyboard repeat and responsive GUI layout have not been exercised under the user's no-GUI restriction. The remaining preview rendering and live interaction gaps prevent a claim of complete rich-text fidelity.
