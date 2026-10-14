//! Sparse, attributed text runs over the note's canonical UTF-8 `body`.
//!
//! Offsets are **Unicode scalar value indices** (`str::chars()`), not byte
//! offsets, UTF-16 code units or grapheme clusters. End offsets are exclusive.
//! The plain body remains authoritative for search, copy, text export and old
//! workspace readers. An empty run list means an entirely unstyled body.

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

const MIN_SIZE: i32 = 12;
const MAX_SIZE: i32 = 40;
const MAX_CLIPBOARD_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_FRAGMENT_TEXT_BYTES: usize = 1024 * 1024;
const MAX_FRAGMENT_RUNS: usize = 65_536;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextStyle {
    #[serde(default)]
    pub bold: bool,
    /// Explicit foreground color in `#RRGGBB` format, or None for the theme's color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Explicit font size in points; None inherits the note's global size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<i32>,
    /// One of Sans, Serif or Mono; None inherits the note's global family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
}

impl TextStyle {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(color) = &self.color {
            if color.len() != 7 || !color.starts_with('#') || !color[1..].bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("Text color must be #RRGGBB".into());
            }
        }
        if self.size.is_some_and(|size| !(MIN_SIZE..=MAX_SIZE).contains(&size)) {
            return Err("Text size must be from 12 to 40".into());
        }
        if self.font_family.as_deref().is_some_and(|family| !matches!(family, "Sans" | "Serif" | "Mono")) {
            return Err("Text font must be Sans, Serif or Mono".into());
        }
        Ok(())
    }

    pub fn is_default(&self) -> bool {
        !self.bold && self.color.is_none() && self.size.is_none() && self.font_family.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRun {
    pub start: usize,
    pub end: usize,
    pub style: TextStyle,
}

/// Anchor and focus are Unicode scalar indices, retaining selection direction.
/// Convert Slint's `*-position-byte-offset` with `from_utf8_bytes` first.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub struct TextSelection {
    pub anchor: usize,
    pub focus: usize,
}

impl TextSelection {
    pub fn caret(index: usize) -> Self { Self { anchor: index, focus: index } }
    pub fn range(self) -> (usize, usize) {
        (self.anchor.min(self.focus), self.anchor.max(self.focus))
    }
    pub fn is_caret(self) -> bool { self.anchor == self.focus }
    pub fn validate(self, body: &str) -> Result<(), String> {
        if self.anchor > body.chars().count() || self.focus > body.chars().count() {
            return Err("Selection exceeds Unicode text length".into());
        }
        Ok(())
    }
    pub fn from_utf8_bytes(body: &str, anchor_byte: usize, focus_byte: usize) -> Result<Self, String> {
        Ok(Self {
            anchor: utf8_byte_to_scalar(body, anchor_byte)?,
            focus: utf8_byte_to_scalar(body, focus_byte)?,
        })
    }
    /// Strict variant for visible caret movement: both offsets must be full
    /// UAX #29 extended grapheme cluster boundaries. Accepts reverse selection.
    pub fn from_utf8_bytes_grapheme(body: &str, anchor_byte: usize, focus_byte: usize) -> Result<Self, String> {
        let selection = Self::from_utf8_bytes(body, anchor_byte, focus_byte)?;
        selection.validate_graphemes(body)?;
        Ok(selection)
    }
    pub fn validate_graphemes(self, body: &str) -> Result<(), String> {
        self.validate(body)?;
        let (anchor_byte, focus_byte) = self.to_utf8_bytes(body)?;
        if is_grapheme_boundary(body, anchor_byte) && is_grapheme_boundary(body, focus_byte) {
            Ok(())
        } else {
            Err("Selection splits a Unicode grapheme cluster".into())
        }
    }
    pub fn to_utf8_bytes(self, body: &str) -> Result<(usize, usize), String> {
        Ok((scalar_to_utf8_byte(body, self.anchor)?, scalar_to_utf8_byte(body, self.focus)?))
    }
}

/// Strict conversion: no rounding into the middle of an emoji or non-ASCII
/// codepoint. Unicode scalar positions do not promise grapheme boundaries.
pub fn utf8_byte_to_scalar(body: &str, byte_offset: usize) -> Result<usize, String> {
    if !body.is_char_boundary(byte_offset) {
        return Err(format!("Offset {byte_offset} is not a UTF-8 character boundary"));
    }
    Ok(body[..byte_offset].chars().count())
}

pub fn scalar_to_utf8_byte(body: &str, scalar_offset: usize) -> Result<usize, String> {
    char_to_byte(body, scalar_offset)
        .ok_or_else(|| format!("Unicode scalar offset {scalar_offset} exceeds text length"))
}

fn is_grapheme_boundary(body: &str, byte_offset: usize) -> bool {
    byte_offset == body.len() || body.grapheme_indices(true).any(|(offset, _)| offset == byte_offset)
}

/// Single-page, transferable styled text. Run offsets are local to `text`,
/// not to a source note. Versioned separately from the workspace JSON format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyledFragment {
    pub version: u8,
    pub text: String,
    #[serde(default)]
    pub runs: Vec<TextRun>,
    /// Original note's inherited font values, so pasted unstyled spans retain
    /// their effective family and size even in a differently configured note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<FragmentDefaults>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FragmentDefaults {
    pub size: i32,
    pub font_family: String,
}

impl StyledFragment {
    pub const VERSION: u8 = 1;

    pub fn validate(&self) -> Result<(), String> {
        if self.version != Self::VERSION { return Err("Unsupported styled clipboard format version".into()); }
        if self.text.len() > MAX_FRAGMENT_TEXT_BYTES || self.runs.len() > MAX_FRAGMENT_RUNS {
            return Err("Styled clipboard content exceeds supported limits".into());
        }
        if self.text.contains('\0') {
            return Err("Styled clipboard text contains NUL".into());
        }
        if let Some(defaults) = &self.defaults {
            TextStyle { size: Some(defaults.size), font_family: Some(defaults.font_family.clone()),
                ..TextStyle::default() }.validate()?;
        }
        validate_runs(&self.text, &self.runs)
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        if json.len() > MAX_CLIPBOARD_JSON_BYTES {
            return Err("Styled clipboard JSON is too large".into());
        }
        let fragment: Self = serde_json::from_str(json).map_err(|error| format!("Invalid styled clipboard JSON: {error}"))?;
        fragment.validate()?;
        Ok(fragment)
    }

    pub fn to_json(&self) -> Result<String, String> {
        self.validate()?;
        let json = serde_json::to_string(self).map_err(|error| error.to_string())?;
        if json.len() > MAX_CLIPBOARD_JSON_BYTES {
            return Err("Styled clipboard JSON is too large".into());
        }
        Ok(json)
    }
}

/// Immutable snapshot of text and formatting only, safe for deterministic
/// undo and redo without rolling back pictures, drawings or note metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteTextSnapshot {
    pub note_id: String,
    pub body: String,
    pub rich_text: Vec<TextRun>,
}

impl NoteTextSnapshot {
    pub fn validate(&self) -> Result<(), String> { validate_runs(&self.body, &self.rich_text) }
}

/// Partial update for a text selection. An absent property is unchanged.
/// `reset_*` explicitly clears an inherited override; it cannot be combined
/// with setting that same property in a single patch.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextStylePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    #[serde(default)]
    pub reset_color: bool,
    #[serde(default)]
    pub reset_size: bool,
    #[serde(default)]
    pub reset_font_family: bool,
}

impl TextStylePatch {
    fn validate(&self) -> Result<(), String> {
        if self.reset_color && self.color.is_some() || self.reset_size && self.size.is_some()
            || self.reset_font_family && self.font_family.is_some() {
            return Err("A format field cannot be both set and reset".into());
        }
        TextStyle {
            bold: false,
            color: self.color.clone(),
            size: self.size,
            font_family: self.font_family.clone(),
        }.validate()
    }

    fn apply(&self, style: &mut TextStyle) {
        if let Some(bold) = self.bold { style.bold = bold; }
        if self.reset_color { style.color = None; }
        if let Some(color) = &self.color { style.color = Some(color.clone()); }
        if self.reset_size { style.size = None; }
        if let Some(size) = self.size { style.size = Some(size); }
        if self.reset_font_family { style.font_family = None; }
        if let Some(family) = &self.font_family { style.font_family = Some(family.clone()); }
    }
}

/// The serialized runs must be canonical so diffs, backups and three-way merge
/// can compare them deterministically. Unstyled gaps are implicit.
pub fn validate_runs(body: &str, runs: &[TextRun]) -> Result<(), String> {
    let char_len = body.chars().count();
    let mut previous_end = 0;
    let mut previous_style: Option<&TextStyle> = None;
    for run in runs {
        if run.start >= run.end || run.end > char_len || run.start < previous_end {
            return Err("Invalid or overlapping rich-text range".into());
        }
        run.style.validate()?;
        if run.style.is_default() {
            return Err("Default style must be represented by an implicit gap".into());
        }
        if previous_end == run.start && previous_style == Some(&run.style) {
            return Err("Adjacent identical rich-text ranges must be merged".into());
        }
        previous_end = run.end;
        previous_style = Some(&run.style);
    }
    Ok(())
}

fn push_run(runs: &mut Vec<TextRun>, start: usize, end: usize, style: TextStyle) {
    if start == end || style.is_default() { return; }
    if let Some(last) = runs.last_mut() {
        if last.end == start && last.style == style {
            last.end = end;
            return;
        }
    }
    runs.push(TextRun { start, end, style });
}

pub fn style_at(runs: &[TextRun], position: usize) -> TextStyle {
    runs.iter().find(|run| run.start <= position && position < run.end)
        .map(|run| run.style.clone()).unwrap_or_default()
}

/// Style used for typing with no explicit toolbar override. For an insertion
/// this follows the character before the caret (or the first character at 0);
/// for replacing a selection it follows the first selected character.
pub fn caret_style(body: &str, runs: &[TextRun], selection: TextSelection) -> Result<TextStyle, String> {
    validate_runs(body, runs)?;
    selection.validate(body)?;
    let (start, end) = selection.range();
    Ok(if start < end { style_at(runs, start) }
       else if start > 0 { style_at(runs, start - 1) }
       else { style_at(runs, 0) })
}

/// For a selected span, change persisted runs. With a collapsed selection,
/// update transient typing style only; don't create a zero-width run (which
/// cannot be persisted safely). Returns whether the document was changed.
pub fn format_selection(body: &str, runs: &mut Vec<TextRun>, selection: TextSelection,
                        patch: TextStylePatch, typing_style: &mut Option<TextStyle>) -> Result<bool, String> {
    validate_runs(body, runs)?;
    selection.validate(body)?;
    patch.validate()?;
    let (start, end) = selection.range();
    if start == end {
        let mut new_style = typing_style.clone().unwrap_or(caret_style(body, runs, selection)?);
        patch.apply(&mut new_style);
        *typing_style = Some(new_style);
        return Ok(false);
    }
    format_range(body, runs, start, end, patch)?;
    *typing_style = None;
    Ok(true)
}

/// Extract selected text and all intersected attributes, rebased to start 0.
/// A collapsed selection produces an empty, valid unstyled fragment.
pub fn copy_fragment(body: &str, runs: &[TextRun], start: usize, end: usize) -> Result<StyledFragment, String> {
    validate_runs(body, runs)?;
    let (start_byte, end_byte) = checked_range_bytes(body, start, end)?;
    let mut result = Vec::new();
    for run in runs {
        if run.end > start && run.start < end {
            push_run(&mut result, run.start.max(start) - start,
                     run.end.min(end) - start, run.style.clone());
        }
    }
    let fragment = StyledFragment {
        version: StyledFragment::VERSION,
        text: body[start_byte..end_byte].to_string(),
        runs: result,
        defaults: None,
    };
    fragment.validate()?;
    Ok(fragment)
}

/// Paste a validated fragment into `[start,end)` without assuming the source
/// and destination have matching inherited note-wide font preferences.
/// Explicitly unstyled pasted characters stay unstyled, even when surrounded
/// by bold text. The old run suffix and prefix remain intact.
pub fn paste_fragment(body: &mut String, runs: &mut Vec<TextRun>, start: usize, end: usize,
                      fragment: &StyledFragment) -> Result<(), String> {
    validate_runs(body, runs)?;
    fragment.validate()?;
    let (start_byte, end_byte) = checked_range_bytes(body, start, end)?;
    let inserted = fragment.text.chars().count();
    let new_len = body.chars().count().checked_sub(end - start)
        .and_then(|length| length.checked_add(inserted))
        .ok_or_else(|| "Pasted text would exceed indexable length".to_string())?;
    let resulting_bytes = body.len().checked_sub(end_byte - start_byte)
        .and_then(|length| length.checked_add(fragment.text.len()))
        .ok_or_else(|| "Pasted text would exceed indexable length".to_string())?;
    if resulting_bytes > MAX_FRAGMENT_TEXT_BYTES {
        return Err("Pasted note body exceeds supported clipboard text limit".into());
    }
    let mut result = Vec::with_capacity(runs.len().saturating_add(fragment.runs.len()));
    for run in runs.iter().filter(|run| run.start < start) {
        push_run(&mut result, run.start, run.end.min(start), run.style.clone());
    }
    let effective_fragment_runs = resolved_fragment_runs(fragment);
    for run in &effective_fragment_runs {
        push_run(&mut result, start + run.start, start + run.end, run.style.clone());
    }
    for run in runs.iter().filter(|run| run.end > end) {
        push_run(&mut result, shift(run.start.max(end), end - start, inserted),
                 shift(run.end, end - start, inserted), run.style.clone());
    }
    // Reject excessive styling before either body or runs is mutated.
    let candidate_body = {
        let mut candidate = String::with_capacity(resulting_bytes);
        candidate.push_str(&body[..start_byte]);
        candidate.push_str(&fragment.text);
        candidate.push_str(&body[end_byte..]);
        candidate
    };
    debug_assert_eq!(candidate_body.chars().count(), new_len);
    validate_runs(&candidate_body, &result)?;
    *body = candidate_body;
    *runs = result;
    Ok(())
}

fn resolved_fragment_runs(fragment: &StyledFragment) -> Vec<TextRun> {
    let Some(defaults) = fragment.defaults.as_ref() else { return fragment.runs.clone(); };
    let base = TextStyle { size: Some(defaults.size), font_family: Some(defaults.font_family.clone()),
        ..TextStyle::default() };
    let mut runs = Vec::with_capacity(fragment.runs.len().saturating_mul(2).saturating_add(1));
    let mut cursor = 0;
    for run in &fragment.runs {
        push_run(&mut runs, cursor, run.start, base.clone());
        let mut style = run.style.clone();
        if style.size.is_none() { style.size = base.size; }
        if style.font_family.is_none() { style.font_family.clone_from(&base.font_family); }
        push_run(&mut runs, run.start, run.end, style);
        cursor = run.end;
    }
    push_run(&mut runs, cursor, fragment.text.chars().count(), base);
    runs
}

/// Format `[start, end)` measured in Unicode scalars. Does not touch the
/// underlying text or any styling outside the selection. Atomic on failure.
pub fn format_range(body: &str, runs: &mut Vec<TextRun>, start: usize, end: usize,
                    patch: TextStylePatch) -> Result<(), String> {
    validate_runs(body, runs)?;
    patch.validate()?;
    let length = body.chars().count();
    if start >= end || end > length {
        return Err(format!("Text selection must satisfy 0 <= start < end <= {length} (Unicode characters)"));
    }

    let mut boundaries = vec![0, length, start, end];
    for run in runs.iter() { boundaries.extend([run.start, run.end]); }
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut result = Vec::with_capacity(runs.len() + 2);
    let mut index = 0;
    for pair in boundaries.windows(2) {
        let (lo, hi) = (pair[0], pair[1]);
        if lo == hi { continue; }
        while index < runs.len() && runs[index].end <= lo { index += 1; }
        let mut style = if index < runs.len() && runs[index].start <= lo {
            runs[index].style.clone()
        } else { TextStyle::default() };
        if start <= lo && hi <= end { patch.apply(&mut style); }
        push_run(&mut result, lo, hi, style);
    }
    *runs = result;
    Ok(())
}

/// Replace `[start,end)` measured in Unicode scalar values. Insertion at a
/// boundary inherits the left character's style, except at position zero
/// where it takes the first character's style. Replacement inherits the first
/// selected character's style. Runs after the edit shift with their text.
pub fn replace_range(body: &mut String, runs: &mut Vec<TextRun>, start: usize, end: usize,
                     replacement: &str) -> Result<(), String> {
    replace_range_with_style(body, runs, start, end, replacement, None)
}

/// Exact selection replacement with an optional style chosen by the editor
/// for typed characters. `None` preserves the legacy caret inheritance rule.
pub fn replace_range_with_style(body: &mut String, runs: &mut Vec<TextRun>, start: usize, end: usize,
                                replacement: &str, typing_style: Option<TextStyle>) -> Result<(), String> {
    validate_runs(body, runs)?;
    let original_len = body.chars().count();
    if start > end || end > original_len {
        return Err(format!("Text edit must satisfy 0 <= start <= end <= {original_len} (Unicode characters)"));
    }
    let new_len = replacement.chars().count();
    // Prevent impossible range arithmetic before mutating either field.
    original_len.checked_sub(end - start).and_then(|count| count.checked_add(new_len))
        .ok_or_else(|| "Text would exceed indexable length".to_string())?;
    if let Some(style) = typing_style.as_ref() { style.validate()?; }
    let (start_byte, end_byte) = checked_range_bytes(body, start, end)?;
    let inherited = typing_style.unwrap_or_else(|| if start < end { style_at(runs, start) }
        else if start > 0 { style_at(runs, start - 1) }
        else { style_at(runs, 0) });

    let mut result = Vec::with_capacity(runs.len() + 2);
    for run in runs.iter().filter(|run| run.start < start) {
        push_run(&mut result, run.start, run.end.min(start), run.style.clone());
    }
    push_run(&mut result, start, start + new_len, inherited);
    for run in runs.iter().filter(|run| run.end > end) {
        let lo = run.start.max(end);
        let adjusted_lo = shift(lo, end - start, new_len);
        let adjusted_hi = shift(run.end, end - start, new_len);
        push_run(&mut result, adjusted_lo, adjusted_hi, run.style.clone());
    }
    body.replace_range(start_byte..end_byte, replacement);
    *runs = result;
    debug_assert!(validate_runs(body, runs).is_ok());
    Ok(())
}

fn shift(position: usize, removed: usize, inserted: usize) -> usize {
    if inserted >= removed { position + (inserted - removed) }
    else { position - (removed - inserted) }
}

fn char_to_byte(body: &str, position: usize) -> Option<usize> {
    body.char_indices().map(|(index, _)| index).chain(std::iter::once(body.len())).nth(position)
}

fn checked_range_bytes(body: &str, start: usize, end: usize) -> Result<(usize, usize), String> {
    if start > end {
        return Err("Text selection start must not exceed end".into());
    }
    let start_byte = scalar_to_utf8_byte(body, start)?;
    let end_byte = scalar_to_utf8_byte(body, end)?;
    Ok((start_byte, end_byte))
}

/// Update a whole editor buffer without erasing styles around the edit.
/// Preserves the unchanged scalar prefix and suffix and treats the middle as
/// one replacement. Clients with exact editor edit spans can call replace_range
/// directly to preserve styling more precisely.
pub fn reconcile_body(body: &mut String, runs: &mut Vec<TextRun>, edited: String) -> Result<(), String> {
    if *body == edited { return Ok(()); }
    let old_len = body.chars().count();
    let new_len = edited.chars().count();
    let prefix = body.chars().zip(edited.chars()).take_while(|(a, b)| a == b).count();
    let suffix = body.chars().rev().zip(edited.chars().rev())
        .take((old_len - prefix).min(new_len - prefix))
        .take_while(|(a, b)| a == b).count();
    let middle_start = char_to_byte(&edited, prefix).ok_or("Invalid new-text start")?;
    let middle_end = char_to_byte(&edited, new_len - suffix).ok_or("Invalid new-text end")?;
    replace_range(body, runs, prefix, old_len - suffix, &edited[middle_start..middle_end])?;
    debug_assert_eq!(*body, edited);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bold() -> TextStylePatch { TextStylePatch { bold: Some(true), ..Default::default() } }

    #[test]
    fn mixed_formatting_is_canonical_and_splits_then_merges() {
        let body = "one two three";
        let mut runs = vec![];
        format_range(body, &mut runs, 4, 7, bold()).unwrap();
        format_range(body, &mut runs, 0, 3, TextStylePatch {
            color: Some("#bB0099".into()), size: Some(23), font_family: Some("Serif".into()), ..Default::default()
        }).unwrap();
        format_range(body, &mut runs, 6, 11, bold()).unwrap();
        assert_eq!(runs[1], TextRun { start: 4, end: 11, style: TextStyle { bold: true, ..Default::default() } });
        assert_eq!(style_at(&runs, 2).size, Some(23));
        format_range(body, &mut runs, 0, 3, TextStylePatch {
            reset_color: true, reset_size: true, reset_font_family: true, ..Default::default()
        }).unwrap();
        assert_eq!(runs.len(), 1);
        assert!(validate_runs(body, &runs).is_ok());
    }

    #[test]
    fn unicode_indices_do_not_split_emoji_surrogates_or_utf8_bytes() {
        let mut body = "A👩‍🚀e\u{301}猫Z".to_string();
        let mut runs = Vec::new();
        // Unicode scalar offsets: A=0, woman=1, ZWJ=2, rocket=3,
        // e=4, combining accent=5, 猫=6, Z=7.
        format_range(&body, &mut runs, 1, 7, bold()).unwrap();
        replace_range(&mut body, &mut runs, 4, 6, "é").unwrap();
        assert_eq!(body, "A👩‍🚀é猫Z");
        assert_eq!(runs, vec![TextRun { start: 1, end: 6, style: TextStyle { bold: true, ..Default::default() } }]);
        assert!(replace_range(&mut body, &mut runs, 99, 99, "oops").is_err());
    }

    #[test]
    fn deletion_insertion_and_whole_editor_reconciliation_preserve_unedited_runs() {
        let mut body = "abc DEF ghi".to_string();
        let mut runs = Vec::new();
        format_range(&body, &mut runs, 4, 7, bold()).unwrap();
        reconcile_body(&mut body, &mut runs, "abc D🦊EF ghi".into()).unwrap();
        assert_eq!(runs, vec![TextRun { start: 4, end: 8, style: TextStyle { bold: true, ..Default::default() } }]);
        replace_range(&mut body, &mut runs, 4, 8, "").unwrap();
        assert_eq!(body, "abc  ghi");
        assert!(runs.is_empty());
        reconcile_body(&mut body, &mut runs, "".into()).unwrap();
        assert_eq!(body, "");
    }

    #[test]
    fn invalid_format_is_atomic_and_malformed_runs_are_rejected() {
        let body = "hello";
        let mut runs = Vec::new();
        assert!(format_range(body, &mut runs, 0, 4, TextStylePatch { color: Some("red".into()), ..Default::default() }).is_err());
        assert!(runs.is_empty());
        assert!(format_range(body, &mut runs, 0, 4, TextStylePatch { size: Some(55), ..Default::default() }).is_err());
        assert!(format_range(body, &mut runs, 0, 6, bold()).is_err());
        assert!(format_range(body, &mut runs, 1, 1, bold()).is_err());
        assert!(validate_runs(body, &[TextRun { start: 2, end: 6, style: TextStyle { bold: true, ..Default::default() } }]).is_err());
    }

    #[test]
    fn repeated_unicode_edits_leave_text_exact_and_ranges_in_bounds() {
        let mut body = "a👨‍👩‍👧猫z".to_string();
        let mut reference: Vec<char> = body.chars().collect();
        let mut runs = Vec::new();
        format_range(&body, &mut runs, 1, 6, TextStylePatch {
            color: Some("#765432".into()), size: Some(19), ..Default::default()
        }).unwrap();
        for index in 0..150usize {
            let len = reference.len();
            let start = (index * 7 + 3) % (len + 1);
            let end = start + (index * 11 + 1) % (len - start + 1);
            let replacement = match index % 5 {
                0 => "👩‍🚀",
                1 => "",
                2 => "汉",
                3 => "a\u{301}",
                _ => "B",
            };
            reference.splice(start..end, replacement.chars());
            replace_range(&mut body, &mut runs, start, end, replacement).unwrap();
            assert_eq!(body, reference.iter().collect::<String>(), "edit {index}");
            validate_runs(&body, &runs).unwrap();
            if index % 9 == 0 && !reference.is_empty() {
                let sel_start = index % reference.len();
                format_range(&body, &mut runs, sel_start, reference.len(), bold()).unwrap();
            }
        }
    }

    #[test]
    fn slint_byte_offsets_convert_exactly_and_keep_backward_selection() {
        let body = "a👩‍🚀e\u{301}猫\n";
        let chars: Vec<char> = body.chars().collect();
        for index in 0..=chars.len() {
            let byte = scalar_to_utf8_byte(body, index).unwrap();
            assert_eq!(utf8_byte_to_scalar(body, byte).unwrap(), index);
        }
        let selection = TextSelection::from_utf8_bytes(body, body.len(), 1).unwrap();
        assert_eq!(selection.range(), (1, chars.len()));
        assert_eq!(selection.anchor, chars.len());
        assert_eq!(selection.to_utf8_bytes(body).unwrap(), (body.len(), 1));
        assert!(TextSelection::from_utf8_bytes(body, 2, body.len()).is_err());
        assert!(utf8_byte_to_scalar(body, body.len() + 1).is_err());
        assert!(scalar_to_utf8_byte(body, chars.len() + 1).is_err());
    }

    #[test]
    fn caret_formatting_is_transient_but_new_text_keeps_combined_style() {
        let mut note = crate::store::Note::create("note");
        note.set_body_preserving_format("red blue".into()).unwrap();
        note.format_text_range(0, 3, bold()).unwrap();
        assert!(note.caret_style(TextSelection::caret(3)).unwrap().bold);
        assert!(!note.caret_style(TextSelection::caret(4)).unwrap().bold);
        let before_runs = note.rich_text.clone();

        let mut typed_style = None;
        let changed = note.format_selection(TextSelection::caret(4), TextStylePatch {
            color: Some("#EE3344".into()), size: Some(25), ..Default::default()
        }, &mut typed_style).unwrap();
        assert!(!changed);
        assert_eq!(note.rich_text, before_runs);
        assert_eq!(typed_style.as_ref().unwrap().color.as_deref(), Some("#EE3344"));
        let caret = note.replace_selection(TextSelection::caret(4), "猫💡", typed_style.clone()).unwrap();
        assert_eq!(caret, 6);
        assert_eq!(note.body, "red 猫💡blue");
        assert_eq!(note.style_at(5).size, Some(25));
        assert_eq!(note.style_at(5).color.as_deref(), Some("#EE3344"));

        note.format_selection(TextSelection::caret(6), TextStylePatch {
            bold: Some(true), ..Default::default()
        }, &mut typed_style).unwrap();
        assert_eq!(typed_style.as_ref().unwrap().bold, true);
        let caret = note.replace_selection(TextSelection::caret(6), "!", typed_style.clone()).unwrap();
        assert_eq!(caret, 7);
        assert!(note.style_at(6).bold);
        assert_eq!(note.style_at(6).color.as_deref(), Some("#EE3344"));
        note.validate_rich_text().unwrap();
    }

    #[test]
    fn styled_clipboard_roundtrip_retains_format_and_default_gaps() {
        let mut source = crate::store::Note::create("note");
        source.set_body_preserving_format("one 👩‍🚀 two 猫".into()).unwrap();
        source.format_text_range(0, 3, bold()).unwrap();
        source.format_text_range(8, 11, TextStylePatch {
            color: Some("#223344".into()), font_family: Some("Serif".into()), ..Default::default()
        }).unwrap();
        let fragment = source.copy_styled_range(2, source.body.chars().count()).unwrap();
        assert_eq!(fragment.text, "e 👩‍🚀 two 猫");
        assert_eq!(fragment.runs[0].start, 0);
        assert_eq!(fragment.runs[0].end, 1);
        let clipboard_json = fragment.to_json().unwrap();
        let restored = StyledFragment::from_json(&clipboard_json).unwrap();
        assert_eq!(restored, fragment);

        let mut target = crate::store::Note::create("note");
        target.set_body_preserving_format("ABCD".into()).unwrap();
        target.format_text_range(0, 4, bold()).unwrap();
        let caret = target.paste_styled_range(2, 3, &restored).unwrap();
        assert_eq!(caret, 2 + fragment.text.chars().count());
        assert_eq!(target.body, format!("AB{}D", fragment.text));
        assert_eq!(target.style_at(0).bold, true);
        assert_eq!(target.style_at(3).bold, false); // unstyled fragment gap
        assert_eq!(target.style_at(caret).bold, true); // old D suffix
        assert_eq!(target.style_at(2).bold, true); // first fragment char
        assert_eq!(target.style_at(caret - 3).font_family.as_deref(), Some("Serif"));
        target.validate_rich_text().unwrap();
    }

    #[test]
    fn corrupt_clipboard_and_invalid_paste_are_rejected_without_mutation() {
        let mut note = crate::store::Note::create("note");
        note.set_body_preserving_format("safe".into()).unwrap();
        let snapshot = note.snapshot_text();
        for candidate in [
            r#"{"version":2,"text":"hello","runs":[]}"#,
            r#"{"version":1,"text":"a","runs":[{"start":0,"end":2,"style":{"bold":true}}]}"#,
            r#"{"version":1,"text":"a","runs":[],"extra":0}"#,
            r#"{"version":1,"text":"a","runs":[{"start":0,"end":1,"style":{"color":"javascript:x"}}]}"#,
        ] {
            assert!(StyledFragment::from_json(candidate).is_err(), "Accepted {candidate}");
        }
        let fragment = StyledFragment { version: 2, text: "no".into(), runs: vec![], defaults: None };
        assert!(note.paste_styled_range(0, 2, &fragment).is_err());
        assert!(note.paste_styled_range(10, 10, &StyledFragment {
            version: 1, text: "x".into(), runs: vec![], defaults: None
        }).is_err());
        assert_eq!(note.snapshot_text(), snapshot);
    }

    #[test]
    fn text_snapshots_restore_formats_without_touching_other_note_layers() {
        let mut note = crate::store::Note::create("note");
        note.set_body_preserving_format("one 猫".into()).unwrap();
        note.strokes.push(crate::store::Stroke {
            color: [3, 4, 5, 255], width: 4, eraser: false, points: vec![[2, 3]]
        });
        let before = note.snapshot_text();
        let caret = note.replace_selection(TextSelection::caret(5), "🦊", None).unwrap();
        assert_eq!(caret, 6);
        note.format_text_range(4, 6, bold()).unwrap();
        let after = note.snapshot_text();
        note.restore_text_snapshot(&before).unwrap();
        assert_eq!(note.snapshot_text(), before);
        assert_eq!(note.strokes[0].width, 4);
        note.restore_text_snapshot(&after).unwrap();
        assert!(note.style_at(5).bold);
        assert_eq!(note.body, "one 猫🦊");
        assert_eq!(note.strokes[0].width, 4);
    }

    #[test]
    fn snapshots_reject_wrong_note_and_invalid_runs_atomically() {
        let mut note = crate::store::Note::create("note");
        let initial = note.snapshot_text();
        note.set_body_preserving_format("A".into()).unwrap();
        let current = note.snapshot_text();
        let mut invalid = initial.clone();
        invalid.rich_text = vec![TextRun { start: 50, end: 51, style: TextStyle {
            bold: true, ..Default::default()
        } }];
        assert!(note.restore_text_snapshot(&invalid).is_err());
        assert_eq!(note.snapshot_text(), current);
        let mut other = crate::store::Note::create("note");
        assert!(other.restore_text_snapshot(&current).is_err());
        assert_eq!(other.body, "");
    }

    #[test]
    fn strict_grapheme_offsets_reject_incomplete_zwj_combining_and_flags() {
        let body = "a👩‍🚀e\u{301}🇬🇧猫";
        let valid_end = body.len();
        let emoji_start = 1;
        let emoji_end = emoji_start + "👩‍🚀".len();
        assert!(TextSelection::from_utf8_bytes_grapheme(body, emoji_start, emoji_end).is_ok());
        let after_woman = emoji_start + "👩".len();
        assert!(TextSelection::from_utf8_bytes(body, after_woman, valid_end).is_ok());
        assert!(TextSelection::from_utf8_bytes_grapheme(body, after_woman, valid_end).is_err());
        let after_e = emoji_end + "e".len();
        assert!(TextSelection::from_utf8_bytes_grapheme(body, after_e, valid_end).is_err());
        let first_flag = emoji_end + "e\u{301}".len() + "🇬".len();
        assert!(TextSelection::from_utf8_bytes_grapheme(body, first_flag, valid_end).is_err());
        assert!(TextSelection::from_utf8_bytes_grapheme(body, 0, valid_end).is_ok());
    }

    #[test]
    fn pasted_fragment_restores_inherited_font_across_different_note_defaults() {
        let mut source = crate::store::Note::create("note");
        source.font_size = 26;
        source.font_choice = "Serif".into();
        source.set_body_preserving_format("Hi 🐈".into()).unwrap();
        source.format_text_range(3, 4, TextStylePatch {
            bold: Some(true), font_family: Some("Mono".into()), ..Default::default()
        }).unwrap();
        let fragment = source.copy_styled_range(0, 4).unwrap();
        assert_eq!(fragment.defaults.as_ref().unwrap().size, 26);
        assert_eq!(fragment.defaults.as_ref().unwrap().font_family, "Serif");
        let json = fragment.to_json().unwrap();
        let mut target = crate::store::Note::create("note");
        target.font_size = 12;
        target.font_choice = "Sans".into();
        target.set_body_preserving_format("x".into()).unwrap();
        target.paste_styled_range(1, 1, &StyledFragment::from_json(&json).unwrap()).unwrap();
        assert_eq!(target.body, "xHi 🐈");
        assert_eq!(target.style_at(0).font_family, None);
        assert_eq!(target.style_at(1).size, Some(26));
        assert_eq!(target.style_at(1).font_family.as_deref(), Some("Serif"));
        assert_eq!(target.style_at(4).font_family.as_deref(), Some("Mono"));
        assert!(target.style_at(4).bold);
        target.validate_rich_text().unwrap();
    }
}
