# Inkpad Beta — independent selection-editor QA (headless)

**Scope:** follow-up after commit `ca42f66` on 2026-10-14. The reviewer owns only
`tests/selection_editor_qa.rs`, this report and updates to existing
`tests/rich_text_qa.ps1`/`tests/rich_text_qa_report.md`. No production
Rust/Slint files were edited by this reviewer. All tests are headless; no
application window, focus, physical input, display automation or clipboard was
opened or controlled.

## Commands

```powershell
cd D:\chat\inkpad
cargo test --offline --test selection_editor_qa
cargo build --offline --bin inkpad-agent
powershell -NoProfile -ExecutionPolicy Bypass -File tests\rich_text_qa.ps1 `
  -AgentExe D:\chat\inkpad\target\debug\inkpad-agent.exe `
  -RequireRich -RequireStyledHtml -RequireSelectionMcp
powershell -NoProfile -ExecutionPolicy Bypass -File tests\media_qa.ps1 `
  -AgentExe D:\chat\inkpad\target\debug\inkpad-agent.exe
cargo test --offline --bin inkpad-agent
cargo test --offline --bin inkpad
```

The PowerShell integration scripts generate fresh GUID-scoped `INKPAD_DATA_DIR`
folders in `dist`, launch only hidden MCP stdio processes and preserve fixture
evidence. The Rust selection tests use production `rich_text`,
`editor_session`, `store` and media modules directly and never open the GUI.

## What was independently challenged

- **Native selection conversion:** reversals, entire note selection,
  multibyte emoji, ZWJ sequences, Devanagari, combining diacritics,
  out-of-range bytes and mid-codepoint bytes. Production
  `TextSelection::from_utf8_bytes` must preserve direction and reject
  invalid UTF-8 boundaries.
- **Style inheritance:** applying bold/colour/size/family at a collapsed caret
  must not create a zero-length stored run; inserted text receives the
  explicit transient typing style without applying it to untouched neighbours.
- **Copied rich content:** styled JSON/fragment roundtrips, relative offsets,
  source font defaults across differently configured notes, invalid size,
  forged style range, unknown fields and invalid format versions.
- **Multiple edits:** a deterministic 240-operation Unicode edit-and-format
  oracle independently checks every styled scalar after each operation,
  including deletions, replaced selections, emoji and combining marks.
- **Undo/redo:** several text edits and format edits undo/redo in correct
  order without rolling back media; an external text edit must invalidate the
  in-memory history rather than silently overwrite the newer content.
- **MCP parity:** `format_text_batch`, `copy_rich_fragment` and
  `paste_rich_fragment` with grapheme-safe defaults, scalar opt-in,
  stale selection guards, atomic bad payload rejection and preserved source
  font defaults; prior 15 rich/theme/media scenarios remain in the suite.
- **Read-only per-font preview:** new native preview's pure Rust paragraph/span
  builder reconstructs multiline Unicode exactly (including a trailing blank
  paragraph), preserves selected 31px Georgia/bold/#A1B2C3 and inherited
  Segoe UI defaults; refuses partial grapheme styling, Hebrew/Arabic bidi and
  notes above the 1 MiB preview budget rather than misrendering them.
- **Theme, restart, media and backup:** existing integration tests cover theme
  settings races, malformed primary recovery, untouched images and draws,
  media garbage collection and HTML roundtrip.

## Slint and GUI limitations — source review, not physical acceptance

Installed Slint **1.18.1**'s `TextInput` builtin supplies
`cursor-position-byte-offset` and `anchor-position-byte-offset` and
`set-selection-offsets`. The bundled compiler labels the offset properties
internal and undocumented, exposed for testing, and defines the offsets as
**UTF-8 bytes** (not Unicode scalar or UTF-16 offsets). The application now
connects them to a Rust scalar conversion helper. It must keep rejecting
mid-codepoint bytes and preserve reverse selections through toolbar focus
changes. Continued use of undocumented properties remains an upgrade risk.

The app keeps **plain TextInput for editing**. Its separate formatted preview
now uses per-range Slint `Text` items inside `FlexboxLayout` to represent
font-family/size/weight/colour, with fallback to native `StyledText` for
bidi or split-grapheme content. The pure layout data passed the headless tests,
but **actual word wrapping, kerning, clipping, 125% DPI and visual appearance
are not verified** without native screenshots. Source review also found conventional **Ctrl+B** is assigned
to sidebar toggle while bold uses **Ctrl+Alt+B**; this conflicts with common
rich-text editor expectations. A literal pointer/keyboard interaction test is
necessary to verify shortcut precedence, focus restoration, text copy/paste,
undo key presses and held font-size button repeats.

The frameless custom-window native drag implementation contains explicit
maximized restore and drag-threshold logic, but without Windows physical
pointer testing its observed behavior remains unverified. The UI's min-width
is **1010 logical pixels**, so at simulated 125% scale its minimum physical
window is approximately 1263px wide; forcing a 1010px *physical* window at
125% is below the advertised supported geometry. Static dimensions and
successful Slint compilation cannot establish visual layout quality.

## Quality claim boundary

Passing isolated headless logic and integration tests supports a positive
assessment of the storage/editor algorithms and agent interface only. It
**does not certify 9/10 GUI/UI/UX**, selected text styling during actual
mouse drags, rich rendered font-family/size fidelity, pointer hitboxes,
real DPI changes or the visual design. No GUI numerical score will be issued
until screenshots and interaction evidence are permitted.

## Current result

Latest 2026-10-14 headless source validation: **PASS**.

| Suite | Result | Scope |
| --- | --- | --- |
| `cargo test --test selection_editor_qa` | **40/40 PASS** | 12 independently authored Unicode/selection/clipboard/history/preview tests plus 28 included production module tests |
| `cargo test --bin inkpad-agent` | **46/46 PASS** | Headless agent/unit domain |
| `cargo test --bin inkpad` | **49/49 PASS** | Binary domain unit tests including visual preview; no GUI launched |
| `rich_text_qa.ps1 -RequireRich -RequireStyledHtml -RequireSelectionMcp` | **17/17 PASS** | Two hidden MCP stdio processes with isolated storage, rich selection fragments and settings |
| `media_qa.ps1` | **14/14 PASS** | Media security, lifecycle and backup in a separate isolated workspace |

The stale-undo regression originally reproduced a real lost-update defect:
`TextHistory::undo_before` applied an earlier revision after a remote agent had
changed the same note. The prime corrected this by recording an expected
post-edit `TextRevision`, calling `commit_after` after updates and rejecting
an undo/redo whose current note no longer matches. The independent regression
now passes and confirms neither the external text nor picture metadata changes.

The latest batch-format/portable-rich-copy test also passes: atomic rejection
of invalid format patches and stale text, no mutation on malformed fragments,
grapheme-safe refusal to split ZWJ emoji or a combining sequence, source note
font-family/size defaults retained when pasted into a different note, and
no use of the Windows clipboard. The **17** integration scenarios and the
**14** media scenarios are unique suites, but the separate Rust compilation
targets include some of the same production embedded tests; do not add the
totals and call them all independent.

Latest isolated evidence folders:
`dist/rich-text-qa-runs/bff18681a6744d439f4a2b9a13063f47` and
`dist/media-qa-runs/92283c4e3ca347619939d3695df2e1a8`.

**Unresolved for actual experience:** conventional Ctrl+B is bound to sidebar
toggle while bold is Control+Alt+B, live selection-focus restoration is not
physically verified, text typing/copy/paste with a real keyboard has not been
tested, per-range fonts/sizes still do not render **in the editable input**
(but now have a dedicated read-only preview), and native
maximized-drag behavior and 125% visual layout remain outside authorized QA.
The 360 ms/75 ms StepButton repeat logic was reviewed statically, not operated.

**Backend/editor algorithms:** evidence supports a positive assessment within
the tested headless scope. **Overall UI/UX/GUI 9/10 certification: NOT
AVAILABLE** without screenshots, native pointer/keyboard acceptance and
rendered mixed-font proof. No numerical GUI rating is assigned.
