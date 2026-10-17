//! Independent source-driven theme, titlebar hit target, colour and alignment QA.
//!
//! These tests do not launch Inkpad or read notes. Red assertions intentionally
//! expose source defects. The geometry model is in logical Slint pixels with a
//! matching scale applied to native Win32 coordinates.

const UI: &str = include_str!("../ui/main.slint");
const CHROME: &str = include_str!("../src/windows_chrome.rs");
const MAIN: &str = include_str!("../src/main.rs");
const SETTINGS: &str = include_str!("../src/settings.rs");

fn after<'a>(src: &'a str, token: &str) -> &'a str {
    &src[src.find(token).unwrap_or_else(|| panic!("missing source anchor {token:?}")) + token.len()..]
}

fn px_field(src: &str, field: &str) -> f64 {
    let value = after(src, field).trim_start();
    value.split("px").next().unwrap().trim().parse::<f64>()
        .unwrap_or_else(|_| panic!("could not parse {field} in {value:?}"))
}

fn f64_constant(src: &str, key: &str) -> f64 {
    let prefix = format!("const {key}: f64 = ");
    let expression = after(src, &prefix).split(';').next().unwrap();
    expression.split('*').map(|part| {
        part.trim().parse::<f64>().unwrap_or_else(|_| panic!("bad f64 constant {key}: {expression}"))
    }).product()
}

fn native_right_reservation() -> f64 {
    let expr = after(CHROME, "let max_x =").split(';').next().unwrap();
    // The native boundary must remain expressible as W -(names+names)*scale.
    // If the implementation changes, update this independent model after
    // reviewing the actual hit testing rather than silently skipping QA.
    let names = expr.split("-").nth(1)
        .expect("native boundary must explicitly subtract a right-side exclusion")
        .split('(').nth(1).expect("right-side reservation must be a parenthesized sum")
        .split(')').next().unwrap();
    names.split('+').map(|name| f64_constant(CHROME, name.trim())).sum()
}

fn theme_geometry() -> (f64, f64, f64, f64) {
    let p = UI.find("label: \"Light\"").expect("Light theme button");
    let rect = UI[..p].rfind("Rectangle {").expect("theme rectangle");
    let theme_section = &UI[rect..p];
    let theme_width = px_field(theme_section, "width:");
    let button_start = UI[..p].rfind("ActionButton {").expect("theme button");
    let button_width = px_field(&UI[button_start..p], "width:");
    let inner_layout = &UI[rect..button_start];
    let padding_left = if inner_layout.contains("padding-left:") {
        px_field(after(inner_layout, "HorizontalLayout {"), "padding-left:")
    } else { 0. };
    let gap = px_field(after(inner_layout, "HorizontalLayout {"), "spacing:");
    (theme_width, button_width, padding_left, gap)
}

fn theme_centres(window_width: f64) -> [f64; 3] {
    let (selector_width, button_width, pad_left, gap) = theme_geometry();
    let chrome_controls = 3. * 44.;
    let left = window_width - chrome_controls - selector_width + pad_left;
    [
        left + button_width / 2.,
        left + button_width * 1.5 + gap,
        left + button_width * 2.5 + gap * 2.,
    ]
}

fn matching_brace_close(src: &str, opening: usize) -> usize {
    let mut depth = 0i32;
    for (index, byte) in src.as_bytes().iter().enumerate().skip(opening) {
        if *byte == b'{' { depth += 1; }
        if *byte == b'}' {
            depth -= 1;
            if depth == 0 { return index; }
        }
    }
    panic!("unclosed source block");
}

fn field_before_marker(marker: &str, field: &str) -> f64 {
    let location = UI.find(marker).unwrap_or_else(|| panic!("missing marker {marker}"));
    let position = UI[..location].rfind(field).unwrap_or_else(|| panic!("missing {field} near {marker}"));
    px_field(&UI[position..], field)
}

fn brightness(hex: &str) -> f64 {
    let bytes = (0..3).map(|k| u8::from_str_radix(&hex[1 + k * 2..3 + k * 2], 16).unwrap());
    let components: Vec<f64> = bytes.map(|v| {
        let s = v as f64 / 255.;
        if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    }).collect();
    0.2126 * components[0] + 0.7152 * components[1] + 0.0722 * components[2]
}

fn contrast(a: &str, b: &str) -> f64 {
    let (l1, l2) = (brightness(a), brightness(b));
    (l1.max(l2) + 0.05) / (l1.min(l2) + 0.05)
}

fn dark_ink_for_label(label: &str) -> String {
    let marker = format!("label: \"{label}\"");
    let value = after(after(UI, &marker), "ink:").trim_start();
    if value.starts_with("root.muted") || value.starts_with("InkTheme.muted") {
        return "#b5bfcd".into();
    }
    assert!(value.starts_with('#'), "unexpected colour expression for {label}: {value}");
    value[..7].to_ascii_lowercase()
}

fn light_subtle_ink() -> String {
    let value = after(UI, "out property <color> subtle: dark ?");
    let pair = value.split(';').next().unwrap();
    pair.split(':').nth(1).unwrap().trim().to_string()
}

#[test]
fn theme_button_centres_must_never_be_eaten_by_native_drag() {
    let native_right = native_right_reservation();
    let (selector_width, _, _, _) = theme_geometry();
    for width in [1010., 1330., 1920., 2560.] {
        for scale in [1., 1.25, 1.5] {
            let physical_width = width * scale;
            let native_hit_end = physical_width - native_right * scale;
            for (index, x) in theme_centres(width).into_iter().enumerate() {
                let physical_x = x * scale;
                assert!(physical_x >= native_hit_end,
                    "theme button {} centre x={physical_x:.1} intercepted by native drag x<{native_hit_end:.1}, window={width}px scale={scale}; selector={selector_width}px reserved={native_right}px", ["Light", "Dark", "Auto"][index]);
            }
        }
    }
}

#[test]
fn slint_window_move_area_must_exclude_theme_controls() {
    let open = UI.find("WindowMoveArea {").expect("native titlebar still draggable");
    let close = matching_brace_close(UI, UI[open..].find('{').unwrap() + open);
    let light = UI.find("label: \"Light\"").unwrap();
    let dark = UI.find("label: \"Dark\"").unwrap();
    let auto = UI.find("label: \"Auto\"").unwrap();
    assert!(light > close && dark > close && auto > close,
        "theme buttons must be siblings of WindowMoveArea, not descendants that can become drag gestures");
}

#[test]
fn theme_buttons_are_pointer_and_keyboard_accessible() {
    let action_button = &UI[UI.find("component ActionButton ").unwrap()..
        UI.find("component PencilMark ").unwrap()];
    assert!(action_button.contains("accessible-role: button"));
    assert!(action_button.contains("forward-focus: hit;"));
    assert!(action_button.contains("width: parent.width;") && action_button.contains("height: parent.height;"),
        "whole visual button must have a pointer hit target");
    for (visible, stored) in [("Light", "light"), ("Dark", "dark"), ("Auto", "system")] {
        let marker = format!("label: \"{visible}\"");
        let section = &UI[UI.find(&marker).unwrap()..];
        assert!(section.contains(&format!("root.change-appearance-theme(\"{stored}\")")));
        assert!(section.contains(&format!("root.appearance-theme == \"{stored}\"")));
    }
}

#[test]
fn theme_storage_and_system_follow_pipeline_is_connected() {
    assert!(SETTINGS.contains("pub enum AppearanceTheme"));
    for choice in ["Light", "Dark", "System"] {
        assert!(SETTINGS.contains(choice));
    }
    assert!(SETTINGS.contains("ui-settings.json"));
    assert!(SETTINGS.contains("pub fn update") && SETTINGS.contains("acquire_lock"));
    assert!(MAIN.contains("ui.on_change_appearance_theme("));
    assert!(MAIN.contains("app.borrow_mut().set_theme(theme.as_str())"));
    assert!(MAIN.contains("UiSettings::update(self.storage.data_path()"));
    assert!(MAIN.contains("ui.set_appearance_theme(state.ui_settings.theme.as_str().into())"));
    assert!(MAIN.contains("ui.set_system_dark(system_prefers_dark())"));
    assert!(MAIN.contains("if ui.get_appearance_theme().as_str() == \"system\""));
    assert!(MAIN.contains("AppsUseLightTheme"));
    assert!(UI.contains("changed appearance-theme =>") && UI.contains("changed system-dark =>"));
    assert!(UI.contains("InkTheme.dark = root.appearance-theme == \"dark\""));
}

#[test]
fn default_text_colour_resolves_to_theme_ink_in_dark_mode() {
    // The first palette swatch uses #292927 as a sentinel for "default".
    // Persisting that RGB as an explicit foreground makes preview text nearly
    // invisible against dark paper; Rust must remove the style override.
    assert!(UI.contains("{name: \"Default\", code: \"#292927\""));
    let handler = after(MAIN, "ui.on_apply_text_color(");
    let branch = after(handler, "if color.as_str() == \"#292927\"");
    let before_else = branch.split("} else {").next().unwrap();
    assert!(before_else.contains("reset_color: true"),
        "Default swatch must clear explicit foreground so InkTheme.ink drives the preview");
}

#[test]
fn stacked_editor_toolbar_insets_should_align() {
    // These strips share the same main editor x-axis. A 10px drift between
    // labels, tabs, format controls, and image tools is user-visible.
    let labels = [
        ("breadcrumb", "text: \"ALL NOTES\""),
        ("text-draw-tabs", "label: \"Text\""),
        ("font-toolbar", "label: \"FONT\""),
        ("rich-toolbar", "label: \"Bold\""),
        ("pictures-toolbar", "label: \"IMAGES\""),
    ];
    let positions: Vec<(&str, f64)> = labels.into_iter()
        .map(|(label, marker)| (label, field_before_marker(marker, "padding-left:")))
        .collect();
    let minimum = positions.iter().map(|(_, x)| *x).fold(f64::INFINITY, f64::min);
    let maximum = positions.iter().map(|(_, x)| *x).fold(f64::NEG_INFINITY, f64::max);
    assert!(maximum - minimum <= 2.,
        "stacked editor left anchors drift {:.0}px: {positions:?}; align using shared inset", maximum - minimum);
}

#[test]
fn desktop_widths_have_editor_room_for_fixed_controls() {
    assert!(UI.contains("min-width: 1010px"));
    assert!(UI.contains("preferred-width: 1330px"));
    let (selector_width, button_width, pad_left, gap) = theme_geometry();
    assert!(pad_left + button_width * 3. + gap * 2. <= selector_width);
    let side = 278.;
    let divider = 1.;
    // FONT row: 523 fixed + 13*5 gaps + 48 padding.
    // Colour row: 88 label + 6*32 swatches + 170 help text + 8*8 gaps
    // + 48 padding. Stretching help text can elide as a last resort.
    let font_min = 523. + 13. * 5. + 48.;
    let style_min = 88. + 6. * 32. + 170. + 8. * 8. + 48.;
    for width in [1010., 1330., 1920., 2560.] {
        let editor = width - side - divider;
        assert!(editor >= font_min && editor >= style_min,
            "toolbar fixed elements overflow at width={width} expanded sidebar editor={editor}");
    }
}

#[test]
fn primary_palette_labels_pass_normal_text_contrast() {
    for (fg, bg, label) in [
        ("#eef1f6", "#202732", "dark ink"),
        ("#b5bfcd", "#202732", "dark muted"),
        ("#292927", "#fbfaf8", "light ink"),
        ("#6d6962", "#fbfaf8", "light muted"),
    ] {
        assert!(contrast(fg, bg) >= 4.5, "{label}: contrast {:.2}", contrast(fg, bg));
    }
}

#[test]
fn fixed_draw_toolbar_labels_must_be_readable_in_dark_theme() {
    for label in ["IMAGES", "COLOURS"] {
        let fg = dark_ink_for_label(label);
        let ratio = contrast(&fg, "#202732");
        assert!(ratio >= 4.5,
            "{label} foreground {fg} is unreadable on dark toolbar: ratio {ratio:.2}");
    }
}

#[test]
fn subtle_light_copy_must_pass_normal_text_contrast() {
    let fg = light_subtle_ink();
    let ratio = contrast(&fg, "#fbfaf8");
    assert!(ratio >= 4.5, "subtle light copy ratio {ratio:.2} < 4.5 for normal-sized copy");
}
