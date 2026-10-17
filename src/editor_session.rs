//! Undo/redo history for the active note's text and formatting.
//!
//! Never holds or serializes private notes outside the running editor. This
//! bounded in-memory history deliberately excludes pictures, strokes and note
//! metadata; those are handled by their existing safe store transactions.
use crate::store::{Note, TextRun};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct TextRevision {
    pub note_id: String,
    pub body: String,
    pub rich_text: Vec<TextRun>,
    pub font_size: i32,
    pub font_choice: String,
}

impl TextRevision {
    pub fn capture(note: &Note) -> Self {
        Self {
            note_id: note.id.clone(),
            body: note.body.clone(),
            rich_text: note.rich_text.clone(),
            font_size: note.font_size,
            font_choice: note.font_choice.clone(),
        }
    }
    pub fn apply(&self, note: &mut Note) -> Result<(), String> {
        if self.note_id != note.id { return Err("Undo history belongs to another note".into()); }
        let mut next = note.clone();
        let original_body = next.body.clone();
        next.body = self.body.clone();
        next.rich_text = self.rich_text.clone();
        next.font_size = self.font_size;
        next.font_choice = self.font_choice.clone();
        // History owns text/formatting, but picture placement is durable and
        // independently editable. Rebase live anchors across the restored text;
        // restoring an old full-note snapshot would resurrect deleted pictures.
        next.rebase_image_placements_for_text_change(&original_body);
        next.validate_rich_text()?;
        next.validate_inline_media()?;
        note.body = next.body;
        note.rich_text = next.rich_text;
        note.inline_media = next.inline_media;
        note.font_size = next.font_size;
        note.font_choice = next.font_choice;
        note.touch();
        Ok(())
    }
    fn approx_bytes(&self) -> usize {
        self.body.len().saturating_add(self.rich_text.len().saturating_mul(90)).saturating_add(128)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind { Typing, Formatting }

/// A bounded undo stack with consecutive-keystroke coalescing. A different
/// note, selection action, or style change breaks typing coalescence.
pub struct TextHistory {
    undo: Vec<TextRevision>,
    redo: Vec<TextRevision>,
    last: Option<(String, EditKind, Instant)>,
    expected: Option<TextRevision>,
}

const MAX_STEPS: usize = 80;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const TYPING_COALESCE: Duration = Duration::from_millis(620);

impl Default for TextHistory {
    fn default() -> Self { Self { undo: vec![], redo: vec![], last: None, expected: None } }
}

impl TextHistory {
    pub fn record(&mut self, note: &Note, kind: EditKind) {
        if self.expected.as_ref().is_some_and(|current| current != &TextRevision::capture(note)) {
            self.discard_on_external_sync();
        }
        let now = Instant::now();
        let coalesce = kind == EditKind::Typing && self.last.as_ref()
            .is_some_and(|(note_id, previous, at)|
                note_id == &note.id && *previous == EditKind::Typing &&
                now.saturating_duration_since(*at) < TYPING_COALESCE);
        if !coalesce {
            self.undo.push(TextRevision::capture(note));
            Self::trim(&mut self.undo);
        }
        self.last = Some((note.id.clone(), kind, now));
        self.redo.clear();
    }

    /// Commit the expected post-edit revision; undo refuses to replay when
    /// a local or agent edit changed the active document in the meantime.
    pub fn commit_after(&mut self, note: &Note) {
        self.expected = Some(TextRevision::capture(note));
    }

    pub fn undo_before(&mut self, note: &mut Note) -> Result<bool, String> {
        let current = TextRevision::capture(note);
        if self.expected.as_ref().is_some_and(|expected| expected != &current) {
            self.discard_on_external_sync();
            return Err("Undo cancelled: note text changed outside this editor history".into());
        }
        let Some(position) = self.undo.iter().rposition(|step| step.note_id == note.id) else {
            return Ok(false);
        };
        let previous = self.undo.remove(position);
        if let Err(error) = previous.apply(note) {
            self.undo.insert(position, previous);
            return Err(error);
        }
        self.redo.push(current);
        Self::trim(&mut self.redo);
        self.last = None;
        self.expected = Some(TextRevision::capture(note));
        Ok(true)
    }

    pub fn redo(&mut self, note: &mut Note) -> Result<bool, String> {
        let current = TextRevision::capture(note);
        if self.expected.as_ref().is_some_and(|expected| expected != &current) {
            self.discard_on_external_sync();
            return Err("Redo cancelled: note text changed outside this editor history".into());
        }
        let Some(position) = self.redo.iter().rposition(|step| step.note_id == note.id) else {
            return Ok(false);
        };
        let next = self.redo.remove(position);
        if let Err(error) = next.apply(note) {
            self.redo.insert(position, next);
            return Err(error);
        }
        self.undo.push(current);
        Self::trim(&mut self.undo);
        self.last = None;
        self.expected = Some(TextRevision::capture(note));
        Ok(true)
    }

    pub fn clear_for_other_note(&mut self, active: &str) {
        self.undo.retain(|revision| revision.note_id == active);
        self.redo.retain(|revision| revision.note_id == active);
        self.last = None;
    }

    pub fn discard_on_external_sync(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.last = None;
        self.expected = None;
    }

    fn trim(stack: &mut Vec<TextRevision>) {
        while stack.len() > MAX_STEPS || stack.iter().map(TextRevision::approx_bytes).sum::<usize>() > MAX_BYTES {
            if stack.len() <= 1 { break; }
            stack.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TextStylePatch;
    #[test]
    fn mixed_unicode_typing_and_formats_undo_redo_atomically() {
        let mut note = Note::create("note");
        note.body = "e\u{301}猫🦊".into();
        let mut history = TextHistory::default();
        history.record(&note, EditKind::Formatting);
        note.format_text_range(2, 4, TextStylePatch {
            bold: Some(true), color: Some("#CC0044".into()), ..Default::default()
        }).unwrap();
        history.record(&note, EditKind::Typing);
        note.replace_text_range(0, 0, "初").unwrap();
        assert!(history.undo_before(&mut note).unwrap());
        assert_eq!(note.body, "e\u{301}猫🦊");
        assert!(note.style_at(2).bold);
        assert!(history.undo_before(&mut note).unwrap());
        assert!(!note.style_at(2).bold);
        assert!(history.redo(&mut note).unwrap());
        assert!(history.redo(&mut note).unwrap());
        assert_eq!(note.body, "初e\u{301}猫🦊");
        assert!(note.style_at(3).bold);
    }
    #[test]
    fn history_is_note_scoped_and_capped() {
        let mut a = Note::create("note");
        let mut b = Note::create("note");
        let mut history = TextHistory::default();
        for n in 0..100 {
            history.record(&a, EditKind::Formatting);
            a.body.push(char::from(b'a' + (n % 26) as u8));
        }
        assert!(history.undo.len() <= MAX_STEPS);
        assert!(!history.undo_before(&mut b).unwrap());
        history.clear_for_other_note(&a.id);
        assert!(history.redo.is_empty());
        b.body.push('x');
        assert_ne!(a.id, b.id);
    }
}
