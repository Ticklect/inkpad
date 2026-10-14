//! Independent, no-GUI character-formatting regression tests.
//! These execute the production Unicode text engine without opening Slint,
//! touching the user's Inkpad files, accessing clipboard or controlling input.
//! The editor/selection adapter's integration tests are added once finalized.
#![allow(dead_code)]

#[path = "../src/store.rs"]
mod store;
#[path = "../src/media.rs"]
mod media;
#[path = "../src/canvas.rs"]
mod canvas;
#[path = "../src/editor_session.rs"]
mod editor_session;
#[path = "../src/visual_preview.rs"]
mod visual_preview;

use store::rich_text;
use rich_text::{TextRun, TextStyle, TextStylePatch};

fn scalar_offset_to_byte(text: &str, scalar: usize) -> Option<usize> {
    text.char_indices().map(|(offset, _)| offset)
        .chain(std::iter::once(text.len())).nth(scalar)
}

fn reference_style_at(model: &[TextStyle], scalar: usize) -> TextStyle {
    model.get(scalar).cloned().unwrap_or_default()
}

fn assert_canonical(body: &str, runs: &[TextRun], model: &[TextStyle]) {
    rich_text::validate_runs(body, runs).expect("production rich spans are not canonical");
    assert_eq!(body.chars().count(), model.len(), "reference/body mismatch");
    for index in 0..model.len() {
        assert_eq!(rich_text::style_at(runs, index), model[index], "wrong span style at Unicode scalar {index}");
    }
}

#[test]
fn utf8_bytes_and_scalar_positions_differ_for_multiscript_and_emoji_sequences() {
    let body = "A👩‍🚀e\u{301}क्‍ष猫\nZ";
    let boundaries: Vec<_> = body.char_indices().map(|(offset, _)| offset)
        .chain(std::iter::once(body.len())).collect();
    assert_eq!(boundaries.len(), body.chars().count() + 1);
    assert!(boundaries.iter().any(|&offset| offset != boundaries.iter().position(|&b| b == offset).unwrap()));
    for (scalar, &byte) in boundaries.iter().enumerate() {
        assert_eq!(scalar_offset_to_byte(body, scalar), Some(byte));
        assert!(body.is_char_boundary(byte));
        let prefix = body.get(..byte).expect("UTF-8 cut at character boundary");
        assert_eq!(prefix.chars().count(), scalar);
    }
    assert_eq!(scalar_offset_to_byte(body, boundaries.len()), None);
    for byte in 0..body.len() {
        if !body.is_char_boundary(byte) {
            assert!(body.get(..byte).is_none(), "a middle-of-code-point selection must not be converted or sliced");
        }
    }
}

#[test]
fn selection_reversal_and_unicode_character_bounds_preserve_exact_selected_text() {
    let body = "👩‍🚀 Cafe\u{301} 漢字 🏳️‍🌈";
    let length = body.chars().count();
    for a in 0..=length {
        for b in 0..=length {
            let (start, end) = (a.min(b), a.max(b));
            let lo = scalar_offset_to_byte(body, start).unwrap();
            let hi = scalar_offset_to_byte(body, end).unwrap();
            let selected = &body[lo..hi];
            assert_eq!(selected.chars().count(), end - start);
            let mut ranges = Vec::new();
            if start != end {
                rich_text::format_range(body, &mut ranges, start, end,
                    TextStylePatch { bold: Some(true), ..Default::default() }).unwrap();
                for i in 0..length {
                    assert_eq!(rich_text::style_at(&ranges, i).bold, start <= i && i < end);
                }
            }
            rich_text::validate_runs(body, &ranges).unwrap();
        }
    }
}

#[test]
fn multi_step_unicode_cut_paste_and_typing_inheritance_keep_style_and_text() {
    let mut body = "A👩‍🚀e\u{301}猫 END".to_owned();
    let mut runs = Vec::new();
    rich_text::format_range(&body, &mut runs, 1, 6, TextStylePatch {
        bold: Some(true), color: Some("#FA1020".into()),
        size: Some(28), font_family: Some("Mono".into()), ..Default::default()
    }).unwrap();
    let style = rich_text::style_at(&runs, 2);
    assert_eq!(style.size, Some(28));
    // Copy/paste can insert Unicode scalars at a collapsed selection inside
    // a styled run: the insertion must inherit the preceding attributes.
    rich_text::replace_range(&mut body, &mut runs, 4, 4, "🦊").unwrap();
    assert_eq!(body, "A👩‍🚀🦊e\u{301}猫 END");
    assert_eq!(rich_text::style_at(&runs, 4), style);
    // Replacing decomposed e+acute with precomposed é is a real editor action
    // that shortens the Unicode scalar length without changing glyph count.
    rich_text::replace_range(&mut body, &mut runs, 5, 7, "é").unwrap();
    assert_eq!(body, "A👩‍🚀🦊é猫 END");
    assert_eq!(rich_text::style_at(&runs, 5), style);
    // Inserting at the opposite edge then deleting the selection must leave
    // the outside unaffected and spans valid.
    rich_text::replace_range(&mut body, &mut runs, 0, 0, "前").unwrap();
    assert_eq!(body, "前A👩‍🚀🦊é猫 END");
    assert!(!rich_text::style_at(&runs, 0).bold);
    rich_text::replace_range(&mut body, &mut runs, 2, 7, "").unwrap();
    assert_eq!(body, "前A猫 END");
    rich_text::validate_runs(&body, &runs).unwrap();
    assert!(!rich_text::style_at(&runs, 3).bold);
}

#[test]
fn deterministic_property_oracle_catches_style_loss_over_many_interleaved_edits() {
    let alphabet = ["A", "🦊", "e\u{301}", "猫", "क्ष", "\n", "💙"];
    let mut seed = 0x8b18491f_u64;
    let mut rand = |max: usize| -> usize {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed as usize) % max
    };
    let mut body = "A👩‍🚀e\u{301}猫".to_owned();
    let mut spans = Vec::<TextRun>::new();
    let mut model = vec![TextStyle::default(); body.chars().count()];
    for _step in 0..240 {
        let count = model.len();
        if count > 0 && rand(4) == 0 {
            let start = rand(count);
            let end = start + 1 + rand(count - start);
            let bold = rand(2) == 0;
            let style = TextStylePatch { bold: Some(bold), ..Default::default() };
            rich_text::format_range(&body, &mut spans, start, end, style).unwrap();
            for cell in &mut model[start..end] { cell.bold = bold; }
        } else {
            let start = rand(count + 1);
            let end = start + rand(count - start + 1);
            let replacement = alphabet[rand(alphabet.len())];
            let inherit = if start < end { reference_style_at(&model, start) }
                else if start > 0 { reference_style_at(&model, start - 1) }
                else { reference_style_at(&model, 0) };
            let byte_start = scalar_offset_to_byte(&body, start).unwrap();
            let byte_end = scalar_offset_to_byte(&body, end).unwrap();
            let mut expected = body.clone();
            expected.replace_range(byte_start..byte_end, replacement);
            let new_style = std::iter::repeat_n(inherit, replacement.chars().count());
            model.splice(start..end, new_style);
            rich_text::replace_range(&mut body, &mut spans, start, end, replacement).unwrap();
            assert_eq!(body, expected, "Unicode body lost a character after replacement");
        }
        assert_canonical(&body, &spans, &model);
    }
}

#[test]
fn invalid_ranges_must_not_touch_text_or_style_even_after_many_edits() {
    let mut body = "A👩‍🚀e\u{301}猫".to_string();
    let mut runs = Vec::new();
    rich_text::format_range(&body, &mut runs, 1, 4, TextStylePatch {
        bold: Some(true), color: Some("#012345".into()), ..Default::default()
    }).unwrap();
    let baseline = (body.clone(), runs.clone());
    let length = body.chars().count();
    assert!(rich_text::replace_range(&mut body, &mut runs, length + 1, length + 1, "a").is_err());
    assert!(rich_text::replace_range(&mut body, &mut runs, 5, 4, "a").is_err());
    assert!(rich_text::format_range(&body, &mut runs, 2, 2, TextStylePatch { bold: Some(false), ..Default::default() }).is_err());
    assert!(rich_text::format_range(&body, &mut runs, 0, length + 1, TextStylePatch { bold: Some(false), ..Default::default() }).is_err());
    assert!(rich_text::format_range(&body, &mut runs, 0, 1, TextStylePatch { size: Some(500), ..Default::default() }).is_err());
    assert_eq!((body, runs), baseline, "a rejected range changed committed text or style");
}

#[test]
fn real_text_selection_adapter_roundtrips_utf8_and_reversed_mouse_drag() {
    let body = "A👩‍🚀e\u{301}猫🏳️‍🌈z";
    let boundaries: Vec<_> = body.char_indices().map(|(b, _)| b)
        .chain(std::iter::once(body.len())).collect();
    for a in 0..boundaries.len() {
        for b in 0..boundaries.len() {
            let selected = rich_text::TextSelection::from_utf8_bytes(body, boundaries[a], boundaries[b])
                .expect("valid UTF-8 selection rejected");
            assert_eq!((selected.anchor, selected.focus), (a, b), "selection direction lost");
            assert_eq!(selected.range(), (a.min(b), a.max(b)));
            assert_eq!(selected.to_utf8_bytes(body).unwrap(), (boundaries[a], boundaries[b]));
        }
    }
    for byte in 0..body.len() {
        if !body.is_char_boundary(byte) {
            assert!(rich_text::TextSelection::from_utf8_bytes(body, 0, byte).is_err());
            assert!(rich_text::TextSelection::from_utf8_bytes(body, byte, body.len()).is_err());
        }
    }
    assert!(rich_text::TextSelection::from_utf8_bytes(body, 0, body.len() + 10).is_err());
    assert!(rich_text::TextSelection::caret(boundaries.len()).validate(body).is_err());
}

#[test]
fn collapsed_selection_changes_future_typing_without_zero_length_ghost_spans() {
    let mut note = store::Note::create("note");
    note.body = "🦊hello".into();
    let original = note.body.clone();
    let mut pending = None;
    let changed = note.format_selection(rich_text::TextSelection::caret(1), TextStylePatch {
        bold: Some(true), color: Some("#FF3399".into()),
        size: Some(31), font_family: Some("Serif".into()), ..Default::default()
    }, &mut pending).unwrap();
    assert!(!changed, "typing toggle alone must not mutate saved content");
    assert_eq!(note.body, original);
    assert!(note.rich_text.is_empty());
    assert!(pending.as_ref().unwrap().bold);
    let cursor = note.replace_selection(rich_text::TextSelection::caret(1), "👩‍🚀", pending).unwrap();
    assert_eq!(cursor, 4, "new caret is Unicode-scalar, not UTF8 byte or grapheme count");
    assert_eq!(note.body, "🦊👩‍🚀hello");
    for index in 1..4 {
        let style = note.style_at(index);
        assert_eq!(style.color.as_deref(), Some("#FF3399"));
        assert_eq!(style.size, Some(31));
        assert_eq!(style.font_family.as_deref(), Some("Serif"));
        assert!(style.bold);
    }
    assert!(!note.style_at(4).bold, "typed style must not bleed into unselected suffix");
}

#[test]
fn styled_unicode_clipboard_roundtrip_respects_selection_and_ignores_malicious_payloads() {
    let mut from = store::Note::create("note");
    from.body = "a👨‍👩‍👦b e\u{301}猫".into();
    let count = from.body.chars().count();
    from.format_text_range(1, count - 1, TextStylePatch {
        bold: Some(true), color: Some("#5511AB".into()),
        size: Some(26), font_family: Some("Mono".into()), ..Default::default()
    }).unwrap();
    let fragment = from.copy_styled_range(1, count - 1).unwrap();
    assert_eq!(fragment.text.chars().count(), count - 2);
    let roundtrip = rich_text::StyledFragment::from_json(&fragment.to_json().unwrap()).unwrap();
    assert_eq!(fragment, roundtrip, "copied rich styles changed on JSON interchange");
    let mut destination = store::Note::create("note");
    destination.body = "⬅️👉".into();
    let original_destination_scalars = destination.body.chars().count();
    destination.format_text_range(0, destination.body.chars().count(),
        TextStylePatch { bold: Some(true), ..Default::default() }).unwrap();
    let caret = destination.paste_styled_range(2, 2, &roundtrip).unwrap();
    assert_eq!(caret, 2 + roundtrip.text.chars().count());
    assert_eq!(destination.body.chars().count(), original_destination_scalars + roundtrip.text.chars().count());
    assert!(destination.style_at(2).bold);
    assert_eq!(destination.style_at(2).size, Some(26));
    assert_eq!(destination.style_at(2).font_family.as_deref(), Some("Mono"));
    rich_text::validate_runs(&destination.body, &destination.rich_text).unwrap();
    assert!(rich_text::StyledFragment::from_json(r#"{"version":1,"text":"a","runs":[{"start":0,"end":8,"style":{"bold":true}}]}"#).is_err());
    assert!(rich_text::StyledFragment::from_json(r#"{"version":2,"text":"a","runs":[]}"#).is_err());
    assert!(rich_text::StyledFragment::from_json(r#"{"version":1,"text":"a","runs":[],"secret":"x"}"#).is_err());
}

#[test]
fn editor_history_undo_redo_preserves_media_and_blocks_stale_remote_edits() {
    use editor_session::{EditKind, TextHistory};
    let mut note = store::Note::create("note");
    note.body = "A猫👩‍🚀".into();
    note.images.push(store::MediaBlock { id: "qa-picture".into(), filename: "qa-picture.png".into(),
        caption: "kept".into(), width_percent: 65, position: 0 });
    let mut history = TextHistory::default();
    history.record(&note, EditKind::Formatting);
    note.format_text_range(1, 3, TextStylePatch {bold:Some(true), ..Default::default()}).unwrap();
    history.commit_after(&note);
    history.record(&note, EditKind::Typing);
    note.replace_text_range(0, 0, "🦊").unwrap();
    history.commit_after(&note);
    let last = note.snapshot_text();
    assert!(history.undo_before(&mut note).unwrap());
    assert_eq!(note.body, "A猫👩‍🚀");
    assert!(note.style_at(1).bold);
    assert!(history.undo_before(&mut note).unwrap());
    assert!(!note.style_at(1).bold);
    assert!(history.redo(&mut note).unwrap());
    assert!(history.redo(&mut note).unwrap());
    assert_eq!(note.snapshot_text(), last);
    assert_eq!(note.images.len(), 1, "undo rolled back media references");
    // External edit invalidates replay. The history layer must not erase a
    // remote edit merely because the GUI's periodic sync timer has not run.
    note.replace_text_range(0, 0, "REMOTE").unwrap();
    let current = note.snapshot_text();
    assert!(history.undo_before(&mut note).is_err(),
        "stale undo silently overwrote a newer edit; demand fresh revision conflict check");
    assert_eq!(note.snapshot_text(), current);
}

#[test]
fn alternate_editor_history_coalesces_typing_then_undo_redo_replays_formatting() {
    use editor_session::{EditKind, TextHistory};
    let mut note = store::Note::create("note");
    note.body = "é猫".into();
    let mut stack = TextHistory::default();
    stack.record(&note, EditKind::Typing);
    note.set_body_preserving_format("é猫A".into()).unwrap();
    stack.commit_after(&note);
    stack.record(&note, EditKind::Typing);
    note.set_body_preserving_format("é猫AB".into()).unwrap();
    stack.commit_after(&note);
    stack.record(&note, EditKind::Formatting);
    note.format_text_range(2, 4, TextStylePatch {bold:Some(true), ..Default::default()}).unwrap();
    stack.commit_after(&note);
    assert!(stack.undo_before(&mut note).unwrap());
    assert_eq!(note.body, "é猫AB");
    assert!(!note.style_at(2).bold);
    assert!(stack.undo_before(&mut note).unwrap());
    assert_eq!(note.body, "é猫");
    assert!(stack.redo(&mut note).unwrap());
    assert_eq!(note.body, "é猫AB");
    assert!(stack.redo(&mut note).unwrap());
    assert!(note.style_at(2).bold);
    // A newly recorded edit after undo invalidates the prior redo branch.
    assert!(stack.undo_before(&mut note).unwrap());
    stack.record(&note, EditKind::Typing);
    note.set_body_preserving_format("é猫💙".into()).unwrap();
    stack.commit_after(&note);
    assert!(!stack.redo(&mut note).unwrap());
}

#[test]
fn visual_preview_preserves_exact_unicode_text_and_all_four_font_attributes() {
    let mut note = store::Note::create("note");
    note.body = "Alpha 🦊 猫\nSecond 👩‍🚀 Line\n".into();
    note.font_size = 19;
    note.font_choice = "Sans".into();
    note.format_text_range(6, 9, TextStylePatch {
        bold: Some(true), color: Some("#A1B2C3".into()), size: Some(31),
        font_family: Some("Serif".into()), ..Default::default()
    }).unwrap();
    let paragraphs = visual_preview::build(&note).expect("normal LTR preview should be available");
    assert_eq!(paragraphs.len(), 3, "trailing newline must retain final blank paragraph");
    let recovered = paragraphs.iter().map(|p| p.spans.iter().map(|s| s.text.as_str())
        .collect::<String>()).collect::<Vec<_>>().join("\n");
    assert_eq!(recovered, note.body, "visual segmentation changed the original Unicode note");
    let styled = paragraphs[0].spans.iter().find(|span| span.text.contains('🦊')).expect("missing glyph");
    assert_eq!(styled.font_size, 31);
    assert_eq!(styled.family, "Georgia");
    assert!(styled.bold);
    assert_eq!(styled.foreground, Some([0xA1, 0xB2, 0xC3]));
    let inherited = paragraphs[1].spans.iter().find(|span| span.text.contains("Second")).unwrap();
    assert_eq!(inherited.font_size, 19);
    assert_eq!(inherited.family, "Segoe UI");
    assert_eq!(inherited.foreground, None, "unstyled text must use current theme ink");
}

#[test]
fn visual_preview_refuses_bidi_or_split_grapheme_and_bounds_memory() {
    let mut note = store::Note::create("note");
    note.body = "עברית العربية and English".into();
    assert!(visual_preview::build(&note).is_none(), "bidi must use native renderer fallback");
    note.body = "Ae\u{301}猫".into();
    note.format_text_range(2, 3, TextStylePatch {bold: Some(true), ..Default::default()}).unwrap();
    assert!(visual_preview::build(&note).is_none(), "split grapheme must not render as independent text spans");
    note.body = "x".repeat(1024 * 1024 + 1);
    note.rich_text.clear();
    assert!(visual_preview::build(&note).is_none(), "huge preview must be bounded");
}
