//! Read-only visual typography layout data for Slint's wrapping Flexbox.
//!
//! Every token keeps its exact font, size, weight and foreground; the Slint
//! layout engine performs actual font-metric wrapping at the current width/DPI
//! rather than guessed character counts. No note content is ever changed.
//! Complex bidi or spans splitting a grapheme stay on the native StyledText
//! fallback until a future editable, fully attributed renderer is available.
use crate::store::{Note, TextStyle};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, PartialEq)]
pub struct VisualSpan {
    pub text: String,
    pub font_size: i32,
    pub family: String,
    pub bold: bool,
    pub foreground: Option<[u8; 3]>,
}

impl VisualSpan {
    /// Slint Text's preferred width can exclude trailing ordinary spaces.
    /// Each word is a separate Flexbox child, so that makes words appear
    /// joined together even though the saved text still contains spaces.
    /// A non-breaking space has a measured glyph width and remains visible.
    /// Keep `text` untouched; this conversion is only for presentation.
    pub fn layout_text(&self) -> String {
        self.text.replace(' ', "\u{00a0}")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VisualParagraph {
    pub spans: Vec<VisualSpan>,
}

const MAX_SPANS: usize = 5000;
const MAX_PREVIEW_BYTES: usize = 1024 * 1024;

fn rgb(color: Option<&str>) -> Option<[u8; 3]> {
    let color = color?;
    if color.len() != 7 || !color.starts_with('#') { return None; }
    let hex = |at| u8::from_str_radix(&color[at..at + 2], 16).ok();
    Some([hex(1)?, hex(3)?, hex(5)?])
}

fn native_font(family: Option<&str>) -> String {
    match family {
        Some("Serif") => "Georgia",
        Some("Mono") => "Consolas",
        _ => "Segoe UI",
    }.into()
}

fn push_word(target: &mut VisualParagraph, text: &mut String,
             style: &TextStyle, note: &Note) {
    if text.is_empty() { return; }
    target.spans.push(VisualSpan {
        text: std::mem::take(text),
        font_size: style.size.unwrap_or(note.font_size).clamp(12, 40),
        family: native_font(style.font_family.as_deref().or(Some(&note.font_choice))),
        bold: style.bold,
        foreground: rgb(style.color.as_deref()),
    });
}

/// None indicates a case best rendered by StyledText's native script shaping.
pub fn build(note: &Note) -> Option<Vec<VisualParagraph>> {
    if note.body.len() > MAX_PREVIEW_BYTES || note.validate_rich_text().is_err() { return None; }
    // A single native text widget preserves ordinary unformatted whitespace,
    // paragraph flow and selection-like shaping more accurately than Flexbox.
    if note.rich_text.is_empty() || note.body.contains('\t') { return None; }
    // Flexbox lays out independent words. Directional embedding of Arabic or
    // Hebrew across such items changes bidi ordering, so do not misrender it.
    if note.body.chars().any(|ch| ('\u{0590}'..='\u{08FF}').contains(&ch)
        || ('\u{FB1D}'..='\u{FDFF}').contains(&ch)) {
        return None;
    }
    let mut paras = Vec::new();
    let mut current = VisualParagraph { spans: vec![] };
    let mut text = String::new();
    let mut current_style: Option<TextStyle> = None;
    let mut position = 0usize;
    let mut span_count = 0usize;
    for grapheme in note.body.graphemes(true) {
        let count = grapheme.chars().count();
        let style = note.style_at(position);
        // If formatting divides a grapheme, shaped placement across Text
        // children would be invalid. Keep safe whole-paragraph fallback.
        if (position + 1..position + count).any(|index| note.style_at(index) != style) {
            return None;
        }
        position += count;
        if matches!(grapheme, "\n" | "\r\n" | "\r") {
            push_word(&mut current, &mut text,
                current_style.as_ref().unwrap_or(&style), note);
            paras.push(current);
            current = VisualParagraph { spans: vec![] };
            current_style = None;
            continue;
        }
        if current_style.as_ref().is_some_and(|old| old != &style) {
            push_word(&mut current, &mut text,
                current_style.as_ref().expect("prior style"), note);
            span_count += 1;
        }
        current_style = Some(style.clone());
        text.push_str(grapheme);
        if grapheme.chars().all(char::is_whitespace) {
            push_word(&mut current, &mut text, &style, note);
            span_count += 1;
        }
        if span_count > MAX_SPANS { return None; }
    }
    push_word(&mut current, &mut text,
        current_style.as_ref().unwrap_or(&TextStyle::default()), note);
    paras.push(current);
    if paras.iter().map(|p| p.spans.len()).sum::<usize>() > MAX_SPANS { return None; }
    Some(paras)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TextStylePatch;
    #[test]
    fn separate_style_tokens_keep_font_family_size_and_literal_unicode() {
        let mut note = Note::create("note");
        note.body = "My blue word\nItalic-like size 😀 end".into();
        note.format_text_range(3, 7, TextStylePatch {
            bold: Some(true), size: Some(28), font_family: Some("Serif".into()),
            color: Some("#AABBCC".into()), ..Default::default()
        }).unwrap();
        let paragraphs = build(&note).unwrap();
        assert_eq!(paragraphs.len(), 2);
        assert_eq!(paragraphs[0].spans.iter().map(|s| s.text.as_str()).collect::<String>(), "My blue word");
        let blue = paragraphs[0].spans.iter().find(|s| s.text.contains("blue")).unwrap();
        assert_eq!(blue.font_size, 28);
        assert_eq!(blue.family, "Georgia");
        assert_eq!(blue.foreground, Some([0xAA,0xBB,0xCC]));
        assert!(blue.bold);
        assert_eq!(paragraphs[1].spans.iter().map(|s| s.text.as_str()).collect::<String>(), "Italic-like size 😀 end");
    }
    #[test]
    fn complex_bidi_and_mid_grapheme_style_fall_back() {
        let mut rtl = Note::create("note");
        rtl.body = "سلام 👋".into();
        assert!(build(&rtl).is_none());
        let mut combining = Note::create("note");
        combining.body = "ae\u{301}z".into();
        combining.format_text_range(2, 3, TextStylePatch {
            bold: Some(true), ..Default::default()
        }).unwrap();
        assert!(build(&combining).is_none());
    }

    #[test]
    fn layout_spaces_remain_measurable_without_changing_the_saved_text() {
        let mut note = Note::create("note");
        note.body = "Feature idea: Show recommended YouTube channels that can be added to the channels bar, based on the user's previous channel searches.\nUI improvement: Remove the strip at the top of the YouTube Downloader Desktop Beta window (see attached screenshot).".into();
        note.format_text_range(0, 12, TextStylePatch {
            bold: Some(true), ..Default::default()
        }).unwrap();

        let paragraphs = build(&note).unwrap();
        assert_eq!(paragraphs.len(), 2);
        for (rendered, source) in paragraphs.iter().zip(note.body.split('\n')) {
            let raw: String = rendered.spans.iter().map(|span| span.text.as_str()).collect();
            assert_eq!(raw, source, "the model must never alter the note");
            let laid_out: String = rendered.spans.iter().map(VisualSpan::layout_text).collect();
            assert_eq!(laid_out.replace('\u{00a0}', " "), source);
            assert!(!laid_out.contains(' '), "regular spaces can collapse in the Flexbox");
            assert!(laid_out.contains('\u{00a0}'), "word gaps need measurable width");
        }
    }

    #[test]
    fn plain_text_and_tabs_use_the_lossless_native_renderer() {
        let mut note = Note::create("note");
        note.body = "A  B\n  C".into();
        assert!(build(&note).is_none());
        note.format_text_range(0, 1, TextStylePatch { bold: Some(true), ..Default::default() }).unwrap();
        assert!(build(&note).is_some());
        note.body.push('\t');
        assert!(build(&note).is_none());
    }
}
