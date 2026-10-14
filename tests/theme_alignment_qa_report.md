# Inkpad Beta — independent theme and alignment QA

**Scope:** source-driven, headless audit of `ui/main.slint`, `src/main.rs`,
`src/settings.rs` and `src/windows_chrome.rs`, plus the user's reported
alignment/theme screenshot. No GUI, mouse, window, theme registry mutation,
real notebook data, or production source changes. All coordinates below are
**derived from source**, not measured from live screenshot pixels.

## Current status after implementation fixes

**Reverified against updated shared source:** 10 focused headless checks:
**10 PASS, 0 FAIL**. Prime has corrected the native titlebar exclusion to
`RIGHT_APPEARANCE_SELECTOR=185px` plus 132px window chrome and 6px edge,
and added native high-DPI regressions. The theme hit-target test now passes
at 1010/1330/1920/2560px and scales 1/1.25/1.5. UI owner moved
`WindowMoveArea` away from the theme buttons, and the source-driven check
passes. Colour labels now use `InkTheme.muted` and the light subtle
foreground was darkened to `#68645c`; both contrast regression tests pass.
The "Default" colour now clears the explicit text-colour override in Rust.

The last red gate, editor left-anchor alignment, was resolved by the prime:
breadcrumb, note title, Text/Draw, FONT, rich actions and PICTURES now all use
24px left padding (and a consistent 24px right padding where applicable).
Physical GUI acceptance is still pending.

## Original critical root cause — native drag consumed theme button clicks (fixed)

The three appearance buttons have valid Slint `clicked` callbacks and
accessible `KeyboardTarget` hit surfaces. Host wiring also persists
`light`, `dark` or `system` using `UiSettings::update`.

Before prime's fix, the native Win32 winit filter captured `MouseInput::Pressed(Left)` first.
`src/windows_chrome.rs:98-115` calls `titlebar_hit` and returns
`EventResult::PreventDefault` in a broad titlebar rectangle.
`titlebar_hit` reserved only `3*44+6 = 138` logical pixels from the right
edge for controls. It ignores the **185px theme-selector panel** immediately
to the left of those controls. A normal theme-button click is mistaken for
a prospective window drag and never arrives at Slint.

Latest source selector: 185px, inner left/right padding 6px, three 53px buttons,
4px gaps, followed by three 44px window controls. Buttons are at y=5–35;
their centreline y=20 is also inside the native draggable y=6–40.

| Window width (logical px) | Light centre x | Dark centre x | Auto centre x | Native drag x region |
| ---: | ---: | ---: | ---: | --- |
| 1010 minimum | 725.5 | 782.5 | 839.5 | 6 to less than 872 |
| 1330 preferred | 1045.5 | 1102.5 | 1159.5 | 6 to less than 1192 |
| 1920 fullscreen example | 1635.5 | 1692.5 | 1749.5 | 6 to less than 1782 |
| 2560 large example | 2275.5 | 2332.5 | 2389.5 | 6 to less than 2422 |

All 12 button centres are captured by native hit testing, including at
125% and 150% DPI in the scaled geometry test. This directly explains the
user's nonfunctional Light/Dark/Auto buttons. It does **not** implicate the
settings format or system-theme registry detection.

**Required fix (prime's native file):** reserve the actual interactive
right header width, `185 + 3*44 + 6 = 323` logical pixels, in the native
titlebar hit test; ideally share the geometry contract with the UI. Add
Windows titlebar unit assertions that centre clicks for all three theme
buttons at minimum, preferred and maximised widths propagate.

The UI worker separately moved the `WindowMoveArea` to the brand/blank
area. Prime also fixed the native pre-Slint intercept, so both event paths
are now covered by source and headless native geometry tests.

## Alignment — source confirms inconsistent toolbar anchor positions

Within the expanded editor, the sidebar is 278px plus a 1px separator.
The editor content width is therefore 731px at a 1010px window, 1051px at
1330px, 1641px at 1920px and 2281px at 2560px.

For the **original audited UI source**, left padding from the editor's left edge was:

| Editor row | Left inset |
| --- | ---: |
| Breadcrumb/top status | 31px |
| Text/Draw tabs | 24px |
| Font/size | 24px |
| Bold/Undo/Redo and text colours | 24px |
| Pictures toolbar | 26px |

The **7px first-to-last difference** was a reproducible static alignment
discrepancy. Prime aligned the listed row padding to **24px**. The title and
note body have separate content padding and should be assessed independently.

The redesigned editor now places colour swatches on their own line,
which removes the prior single-row competition between long help text,
buttons and colour picker at 1010px. Basic fixed-width minimum calculations
for the font and colour rows fit the 731px expanded-editor width. This
is a **feasibility check**, not a rendered-layout acceptance test.

**Screenshot evidence limit:** the user reported misalignment and dead theme
buttons in screenshots, but an original screenshot image file was not exposed
to this worker. No image coordinates were extracted and no before/after
pixel comparison, DPI/font metrics, or visual score is claimed. The static
insets and hit rectangles above are reproducible evidence consistent with
the report; actual layout acceptance still requires screenshot comparison
and physical pointer testing by the owning GUI engineer.

## Dark/light contrast defects

Contrast ratios from the **original defect snapshot**, calculated with WCAG
relative luminance (the failing colours have subsequently been corrected):

| Element | Foreground vs background | Ratio | Normal-text target |
| --- | --- | ---: | ---: |
| Light normal ink | `#292927` vs `#fbfaf8` | above 4.5:1 | Pass |
| Light muted | `#6d6962` vs `#fbfaf8` | 5.23:1 | Pass |
| Dark muted | `#b5bfcd` vs `#202732` | 8.08:1 | Pass |
| Light subtle | `#948d80` vs `#fbfaf8` | **3.16:1** | Fail |
| Dark PICTURES label | `#686159` vs `#202732` | **2.46:1** | Fail |
| Dark COLOURS label | `#70685c` vs `#202732` | **2.74:1** | Fail |

In the originally audited source, PICTURES/COLOURS used hardcoded light-mode
label colours; the UI owner has now switched both to `InkTheme.muted`.
The light subtle text colour was also improved. Dark mode still has a hardcoded bright
header rule `#e3ded5`, 11.33:1 against dark chrome; this is a visual
consistency concern rather than a WCAG text failure.

The text palette's "Default" button sends `#292927`, which would become
unreadable if saved as an explicit dark-mode foreground. The current Rust
colour callback correctly interprets it as `reset_color: true`, letting
the themed ink render the text. The independent test protects this path.

## Theme flow and remaining UX risks

`ActionButton` contains a full-width/full-height enabled `TouchArea`,
keyboard `FocusScope`, semantic button role, and accessible labels.
The appearance-choice callbacks map Light→`light`, Dark→`dark`,
Auto→`system`. `src/main.rs` loads persisted preferences, binds the
chosen appearance and Windows `AppsUseLightTheme` registry value to Slint,
polls system-mode changes and refreshes settings written by the agent.
`UiSettings::update` locks and saves a separate `ui-settings.json`
without editing notes. An existing `src/main.rs` unit test verifies
theme persistence and cross-process preference refresh with temporary data.

Two residual risks remain: the UI optimistically selects a theme before
the save result is known, so a disk/lock error can show a theme choice that
was not persisted; also no physical-click replay verified the native
input pipeline in this audit.

The user found "STYLE", "INK" and "Range…" unclear. Recent UI work replaced
those with visible "Bold", "Undo", "Redo", "TEXT COLOUR" and an "Advanced"
button. The number-based range fallback is still intentionally advanced;
ordinary mouse selection should be the default editing path. The labelling
change is source verified, **not** visually approved.

## Independent test run

File: `tests/theme_alignment_qa.rs`, compiled without opening the desktop:

```powershell
& "$env:USERPROFILE\.cargo\bin\rustc.exe" --edition=2021 --test tests\theme_alignment_qa.rs -o target\worker6\theme_alignment_qa.exe
& target\worker6\theme_alignment_qa.exe --nocapture
```

The initial nine-test run was **5 passed, 4 failed**, with actual defects
in theme/native hit region, editor alignment, dark draw labels and light
subtle contrast. A tenth test guards theme-aware default text colour.
After source owners fixed native hit testing, contrast and final row padding,
**10 passed, 0 failed**. No GUI interaction or screenshot comparison was
performed, so no UI, UX or GUI quality rating is awarded from these checks.
