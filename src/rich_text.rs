//! Sparse, attributed text runs over the note's canonical UTF-8 `body`.
//!
//! Offsets are **Unicode scalar value indices** (`str::chars()`), not byte
//! offsets, UTF-16 code units or grapheme clusters. End offsets are exclusive.
//! The plain body remains authoritative for search, copy, text export and old
//! workspace readers. An empty run list means an entirely unstyled body.

use serde::{Deserialize, Serialize};

const MIN_SIZE: i32 = 12;
const MAX_SIZE: i32 = 40;

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
    validate_runs(body, runs)?;
    let original_len = body.chars().count();
    if start > end || end > original_len {
        return Err(format!("Text edit must satisfy 0 <= start <= end <= {original_len} (Unicode characters)"));
    }
    let new_len = replacement.chars().count();
    // Prevent impossible range arithmetic before mutating either field.
    original_len.checked_sub(end - start).and_then(|count| count.checked_add(new_len))
        .ok_or_else(|| "Text would exceed indexable length".to_string())?;
    let start_byte = char_to_byte(body, start).ok_or("Invalid start character index")?;
    let end_byte = char_to_byte(body, end).ok_or("Invalid end character index")?;
    let inherited = if start < end { style_at(runs, start) }
        else if start > 0 { style_at(runs, start - 1) }
        else { style_at(runs, 0) };

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
}
