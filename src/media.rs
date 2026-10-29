//! Durable, bounded local picture attachments for Inkpad pages.
//!
//! Metadata belongs in workspace.json, while decoded and validated PNG assets
//! live beneath a private `media/` folder beside that workspace. The stored
//! filenames are opaque generated IDs, never user-provided paths.

use crate::canvas::Board;
use crate::store::{MediaBlock, Note, NoteContentBlock, Store, TextStyle, Workspace};
use base64::Engine as _;
use image::codecs::png::PngEncoder;
use image::{GenericImageView, ImageEncoder, ImageFormat, ImageReader};
use std::fs::{self, OpenOptions};
use std::collections::HashSet;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_INPUT_BYTES: usize = 12 * 1024 * 1024;
const MAX_ENCODED_BYTES: usize = 32 * 1024 * 1024;
const MAX_PIXELS: u64 = 16_000_000;
const MAX_IMAGES_PER_NOTE: usize = 100;
const MAX_CAPTION_CHARS: usize = 500;

fn bad(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn mime_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn validate_id(id: &str) -> io::Result<()> {
    if id.is_empty() || id.len() > 80 || !id.bytes().all(|ch|
        ch.is_ascii_hexdigit() || ch == b'-') {
        return Err(bad("Invalid internal image identifier"));
    }
    Ok(())
}

fn normalize_to_png(bytes: &[u8]) -> io::Result<Vec<u8>> {
    if bytes.is_empty() || bytes.len() > MAX_INPUT_BYTES {
        return Err(bad("Image must contain 1 byte to 12 MiB"));
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format().map_err(|err| mime_error(err.to_string()))?;
    match reader.format() {
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP) => {}
        _ => return Err(bad("Use PNG, JPEG, GIF or WebP images")),
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(7000);
    limits.max_image_height = Some(7000);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|err| mime_error(format!("Invalid or oversized image: {err}")))?;
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 || (width as u64) * (height as u64) > MAX_PIXELS {
        return Err(bad("Image dimensions exceed Inkpad's 16-megapixel limit"));
    }
    // Reuse the decoded RGBA buffer when the source already has that format.
    // `to_rgba8` clones even an existing full-size RGBA image.
    let pixels = decoded.into_rgba8();
    let mut png = Vec::new();
    PngEncoder::new(&mut png).write_image(
        pixels.as_raw(), width, height, image::ExtendedColorType::Rgba8,
    ).map_err(|err| mime_error(err.to_string()))?;
    if png.len() > MAX_ENCODED_BYTES {
        return Err(bad("Decoded image exceeds Inkpad's 32 MiB asset limit"));
    }
    Ok(png)
}

fn fresh_id() -> String {
    let ticks = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("{ticks:x}-{:x}", std::process::id())
}

fn note_mut<'a>(workspace: &'a mut Workspace, id: &str) -> io::Result<&'a mut Note> {
    workspace.notes.iter_mut().find(|note| note.id == id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Note not found"))
}

fn checked_caption(value: &str) -> io::Result<String> {
    if value.chars().count() > MAX_CAPTION_CHARS { return Err(bad("Caption is too long")); }
    Ok(value.to_string())
}

fn normalize_positions(images: &mut [MediaBlock]) {
    images.sort_by_key(|image| image.position);
    renumber_positions(images);
}

fn renumber_positions(images: &mut [MediaBlock]) {
    for (index, image) in images.iter_mut().enumerate() {
        image.position = index;
    }
}

impl Store {
    fn media_dir(&self) -> io::Result<PathBuf> {
        let base = self.data_path().parent().ok_or_else(|| bad("Invalid data folder"))?;
        let dir = base.join("media");
        fs::create_dir_all(&dir)?;
        let metadata = fs::symlink_metadata(&dir)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied,
                "Media directory must not be a symbolic link"));
        }
        Ok(dir)
    }

    pub fn media_path(&self, image: &MediaBlock) -> io::Result<PathBuf> {
        validate_id(&image.id)?;
        if image.filename != format!("{}.png", image.id) {
            return Err(bad("Unsafe image filename in saved notebook"));
        }
        let folder = self.media_dir()?;
        let path = folder.join(&image.filename);
        if path.exists() && fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied,
                "Image file must not be a symbolic link"));
        }
        Ok(path)
    }

    /// Decode a bounded preview rather than placing full-resolution images
    /// into Slint's live UI model. Generated only on the first view.
    pub fn media_preview_path(&self, image: &MediaBlock) -> io::Result<PathBuf> {
        let source = self.media_path(image)?;
        let previews = self.media_dir()?.join("previews");
        fs::create_dir_all(&previews)?;
        if fs::symlink_metadata(&previews)?.file_type().is_symlink() {
            return Err(bad("Unsafe preview directory"));
        }
        let preview = previews.join(&image.filename);
        if preview.exists() {
            if fs::symlink_metadata(&preview)?.file_type().is_symlink() {
                return Err(bad("Unsafe preview filename"));
            }
            return Ok(preview);
        }
        let bitmap = image::open(&source).map_err(|error| mime_error(error.to_string()))?;
        let thumb = bitmap.thumbnail(720, 720);
        let temporary = previews.join(format!("{}.tmp", image.id));
        thumb.save_with_format(&temporary, ImageFormat::Png)
            .map_err(|error| mime_error(error.to_string()))?;
        if fs::rename(&temporary, &preview).is_err() {
            // Parallel GUI/agent preview render may have created the same
            // preview first. Never overwrite that verified existing file.
            let _ = fs::remove_file(temporary);
            if !preview.is_file() { return Err(bad("Could not create image preview")); }
        }
        Ok(preview)
    }

    pub fn insert_image_from_file(&self, note_id: &str, path: &Path) -> io::Result<MediaBlock> {
        // Only read normal local files, not directories, device streams or
        // files whose declared size exceeds our bounded decoder input.
        let info = fs::metadata(path)?;
        if !info.is_file() || info.len() > MAX_INPUT_BYTES as u64 {
            return Err(bad("Choose a local image smaller than 12 MiB"));
        }
        let mut input = fs::File::open(path)?;
        let mut bytes = Vec::with_capacity(info.len() as usize);
        (&mut input).take(MAX_INPUT_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        self.insert_image_bytes(note_id, &bytes)
    }

    pub fn insert_image_bytes(&self, note_id: &str, bytes: &[u8]) -> io::Result<MediaBlock> {
        let png = normalize_to_png(bytes)?;
        let id = fresh_id();
        let block = MediaBlock {
            id: id.clone(), filename: format!("{id}.png"),
            caption: String::new(), width_percent: 65, position: 0,
        };
        let output = self.media_path(&block)?;
        let staged = output.with_extension("upload");
        let mut file = OpenOptions::new().write(true).create_new(true).open(&staged)?;
        if let Err(err) = file.write_all(&png).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&staged);
            return Err(err);
        }
        drop(file);
        let outcome = self.mutate(|workspace| {
            let note = note_mut(workspace, note_id)?;
            if note.images.len() >= MAX_IMAGES_PER_NOTE { return Err(bad("Maximum 100 images per page")); }
            // Atomically publish the image's final filename while holding
            // the cross-process workspace lock. Garbage collection ignores
            // staged uploads and cannot delete an incomplete import.
            fs::rename(&staged, &output)?;
            let mut item = block.clone();
            item.position = note.images.len();
            note.images.push(item.clone());
            note.touch();
            Ok(item)
        });
        if outcome.is_err() {
            let _ = fs::remove_file(&staged);
            // A failed metadata write may leave the generated PNG visible;
            // only our fresh unique opaque filename can be rolled back.
            let _ = fs::remove_file(&output);
        }
        outcome
    }

    pub fn list_media(&self, note_id: &str) -> io::Result<Vec<MediaBlock>> {
        // Image metadata is frequently queried after every placement or
        // reorder. Reuse the same byte-verified, externally invalidated
        // notebook snapshot as MCP note/search reads instead of parsing a
        // large unchanged JSON workspace for each image-list request.
        let workspace = self.read_cached()?;
        let note = workspace.notes.iter().find(|note| note.id == note_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Note not found"))?;
        let mut items = note.images.clone();
        items.sort_by_key(|item| item.position);
        Ok(items)
    }

    /// Move an existing picture to a Unicode-scalar boundary in the original
    /// note text. None restores the legacy after-text gallery placement.
    /// expected_body prevents applying a stale caret to concurrently edited
    /// text; the entire change occurs under the workspace lock.
    pub fn place_image_at_scalar(
        &self, note_id: &str, image_id: &str, char_offset: Option<usize>,
        expected_body: Option<&str>,
    ) -> io::Result<Option<usize>> {
        self.mutate(|workspace| {
            let note = note_mut(workspace, note_id)?;
            if expected_body.is_some_and(|expected| expected != note.body) {
                return Err(io::Error::new(io::ErrorKind::WouldBlock,
                    "Note text changed; refresh the editor before moving this picture"));
            }
            if !note.images.iter().any(|image| image.id == image_id) {
                return Err(io::Error::new(io::ErrorKind::NotFound, "Image not found"));
            }
            note.set_image_anchor(image_id, char_offset).map_err(bad)?;
            Ok(char_offset)
        })
    }

    pub fn update_image(&self, note_id: &str, image_id: &str, caption: Option<&str>,
        width_percent: Option<u8>, position: Option<usize>) -> io::Result<MediaBlock> {
        if let Some(width) = width_percent {
            if !(10..=100).contains(&width) { return Err(bad("Image width must be 10–100%")); }
        }
        let new_caption = caption.map(checked_caption).transpose()?;
        self.mutate(|workspace| {
            let note = note_mut(workspace, note_id)?;
            normalize_positions(&mut note.images);
            let index = note.images.iter().position(|item| item.id == image_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Image not found"))?;
            if let Some(text) = &new_caption { note.images[index].caption = text.clone(); }
            if let Some(width) = width_percent { note.images[index].width_percent = width; }
            if let Some(wanted) = position {
                let item = note.images.remove(index);
                note.images.insert(wanted.min(note.images.len()), item);
            }
            renumber_positions(&mut note.images);
            note.touch();
            Ok(note.images.iter().find(|item| item.id == image_id).unwrap().clone())
        })
    }

    pub fn delete_image(&self, note_id: &str, image_id: &str) -> io::Result<()> {
        self.mutate(|workspace| {
            let note = note_mut(workspace, note_id)?;
            let old_len = note.images.len();
            note.images.retain(|block| block.id != image_id);
            if old_len == note.images.len() {
                return Err(io::Error::new(io::ErrorKind::NotFound, "Image not found"));
            }
            note.inline_media.retain(|placement| placement.image_id != image_id);
            normalize_positions(&mut note.images);
            note.touch();
            Ok(())
        })?;
        // The backup written by this mutation may still reference the image.
        // It is reclaimed after a subsequent successful workspace mutation
        // rotates the backup and removes its final reference.
        Ok(())
    }

    /// Public maintenance entrypoint used at startup. The same exclusive
    /// workspace lock guards concurrent inserts, deletions and mark/sweep.
    pub fn prune_unused_media(&self) -> io::Result<usize> {
        let _lock = self.lock_workspace()?;
        let workspace = self.read_unlocked()?;
        self.prune_unused_media_locked(&workspace)
    }

    pub(crate) fn prune_unused_media_locked(&self, workspace: &Workspace) -> io::Result<usize> {
        // A text-only notebook has no media directory. Return before parsing
        // the previous full workspace just to discover there is nothing to
        // prune; this runs after every note mutation, including text edits.
        let root = self.data_path().with_file_name("media");
        if !root.exists() { return Ok(0); }
        if fs::symlink_metadata(&root)?.file_type().is_symlink() {
            return Err(bad("Unsafe media directory"));
        }
        let mut referenced = HashSet::new();
        for note in &workspace.notes {
            for item in &note.images { referenced.insert(item.filename.clone()); }
        }
        // Most edits leave all on-disk images referenced by the current
        // workspace. Scan first so a large previous-save backup is parsed
        // only when there is actually a candidate for removal.
        let mut candidates = Vec::new();
        for item in fs::read_dir(&root)? {
            let item = item?;
            if !item.file_type()?.is_file() { continue; }
            let name = item.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".png") else { continue; };
            if validate_id(id).is_err() || referenced.contains(&name) { continue; }
            candidates.push((name, item.path()));
        }
        if candidates.is_empty() { return Ok(0); }

        // Preserve any candidate still referenced by the recovery backup.
        // An unreadable backup must keep all possible recovery assets.
        let backup_file = self.data_path().with_extension("json.bak");
        match fs::read(backup_file) {
            Ok(bytes) => match serde_json::from_slice::<Workspace>(&bytes) {
                Ok(previous) => {
                    for note in &previous.notes {
                        for item in &note.images { referenced.insert(item.filename.clone()); }
                    }
                }
                Err(_) => return Ok(0),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let previews = root.join("previews");
        let mut removed = 0;
        for (name, path) in candidates {
            if referenced.contains(&name) { continue; }
            fs::remove_file(path)?;
            if previews.exists() {
                let preview = previews.join(&name);
                if preview.is_file() && !fs::symlink_metadata(&preview)?.file_type().is_symlink() {
                    let _ = fs::remove_file(preview);
                }
            }
            removed += 1;
        }
        Ok(removed)
    }

    pub fn export_note_html(&self, note_id: &str, path: &Path) -> io::Result<()> {
        let workspace = self.read_latest()?;
        let note = workspace.notes.iter().find(|item| item.id == note_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Note not found"))?;
        let mut html = String::from(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
            <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
            <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; style-src 'unsafe-inline'\">\
            <title>Inkpad export</title><style>body{font:17px system-ui;max-width:900px;margin:2rem auto;padding:0 24px;color:#252525;background:#fff}\
            .body{white-space:pre-wrap;line-height:1.6}figure{margin:24px 0}figure img{height:auto;max-width:100%;border-radius:8px}\
            figcaption{color:#656565;font-size:14px;margin-top:7px}h1{font-size:2rem}</style></head><body>"
        );
        html.push_str("<h1>"); html.push_str(&escape_html(&note.title)); html.push_str("</h1>");
        // Global note typography is inherited by the unstyled gaps. Whitelist
        // legacy font names so saved workspace fields never become raw CSS.
        let base_family = safe_font_family(&note.font_choice);
        let base_size = if (12..=40).contains(&note.font_size) { note.font_size } else { 17 };
        html.push_str(&format!(
            "<div class=\"body\" style=\"font-family:{base_family};font-size:{base_size}px\">"
        ));
        if note.inline_media.is_empty() {
            // Keep the legacy export shape byte-for-byte when no anchors were
            // supplied. Old workspaces still export their gallery after text.
            html.push_str(&render_formatted_body(note)?);
        } else {
            let blocks = note.content_blocks().map_err(mime_error)?;
            // Index the scalar text once; large styled documents otherwise
            // repeatedly scan the same UTF-8 prefix for each inline figure.
            let characters = note.body.chars().collect::<Vec<_>>();
            for block in blocks {
                match block {
                    NoteContentBlock::Text { start, end } =>
                        html.push_str(&render_formatted_body_slice(note, &characters, start, end)?),
                    NoteContentBlock::Image { image_id } => {
                        let image = note.images.iter().find(|item| item.id == image_id)
                            .ok_or_else(|| mime_error("Inline image placement references missing picture"))?;
                        write_html_figure(&mut html, self, image)?;
                    }
                }
            }
        }
        html.push_str("</div>");
        if note.inline_media.is_empty() {
            let mut images = note.images.clone();
            images.sort_by_key(|item| item.position);
            for image in &images { write_html_figure(&mut html, self, image)?; }
        }
        if !note.strokes.is_empty() {
            let png = Board::new(&note.strokes).png_bytes()?;
            html.push_str(&format!("<figure><img alt=\"Inkpad drawing\" width=\"100%\" src=\"data:image/png;base64,{}\"></figure>",
                base64::engine::general_purpose::STANDARD.encode(png)));
        }
        html.push_str("</body></html>");
        // Reject overwrite and source path aliasing; exports are explicit files.
        let mut output = OpenOptions::new().create_new(true).write(true).open(path)?;
        output.write_all(html.as_bytes())?;
        output.sync_all()
    }
}

fn write_html_figure(html: &mut String, store: &Store, image: &MediaBlock) -> io::Result<()> {
    let png = fs::read(store.media_path(image)?)?;
    html.push_str(&format!("<figure><img style=\"width:{}%;height:auto\" alt=\"{}\" src=\"data:image/png;base64,{}\">",
        image.width_percent.clamp(10, 100), escape_html(&image.caption),
        base64::engine::general_purpose::STANDARD.encode(png)));
    if !image.caption.is_empty() {
        html.push_str("<figcaption>");
        html.push_str(&escape_html(&image.caption));
        html.push_str("</figcaption>");
    }
    html.push_str("</figure>");
    Ok(())
}

fn escape_html(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;")
        .replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// CSS font families are selected exclusively from fixed declarations. Never
/// interpolate a saved font name (including names from legacy workspaces).
fn safe_font_family(name: &str) -> &'static str {
    match name {
        "Serif" | "Georgia" => "Georgia,serif",
        "Mono" | "Consolas" => "Consolas,monospace",
        _ => "'Segoe UI',Arial,sans-serif",
    }
}

fn escaped_char(html: &mut String, ch: char) {
    match ch {
        '&' => html.push_str("&amp;"),
        '<' => html.push_str("&lt;"),
        '>' => html.push_str("&gt;"),
        '"' => html.push_str("&quot;"),
        '\'' => html.push_str("&#39;"),
        _ => html.push(ch),
    }
}

/// Only the four supported style attributes can reach an HTML style value;
/// the shared rich-text validator rejects malicious colours, sizes and fonts.
fn safe_style_css(style: &TextStyle) -> io::Result<String> {
    style.validate().map_err(mime_error)?;
    let mut css = String::new();
    if style.bold { css.push_str("font-weight:700;"); }
    if let Some(color) = &style.color {
        css.push_str("color:");
        css.push_str(color);
        css.push(';');
    }
    if let Some(size) = style.size {
        css.push_str(&format!("font-size:{size}px;"));
    }
    if let Some(family) = &style.font_family {
        css.push_str("font-family:");
        css.push_str(safe_font_family(family));
        css.push(';');
    }
    Ok(css)
}

/// Rich runs use half-open Unicode scalar offsets. Walk the body once so
/// multi-byte emoji and combining characters cannot shift or split the text.
fn render_formatted_body(note: &Note) -> io::Result<String> {
    note.validate_rich_text().map_err(mime_error)?;
    let mut html = String::with_capacity(note.body.len());
    let mut characters = note.body.chars();
    let mut position = 0;
    for run in &note.rich_text {
        for ch in characters.by_ref().take(run.start - position) {
            escaped_char(&mut html, ch);
        }
        let css = safe_style_css(&run.style)?;
        html.push_str("<span style=\"");
        html.push_str(&css);
        html.push_str("\">");
        for ch in characters.by_ref().take(run.end - run.start) {
            escaped_char(&mut html, ch);
        }
        html.push_str("</span>");
        position = run.end;
    }
    for ch in characters { escaped_char(&mut html, ch); }
    Ok(html)
}

/// Render a scalar text slice while preserving styles that cross a picture.
/// A rich run straddling an image is safely reopened after the figure without
/// shifting any stored formatting offsets.
fn render_formatted_body_slice(note: &Note, characters: &[char],
    start: usize, end: usize) -> io::Result<String> {
    note.validate_rich_text().map_err(mime_error)?;
    if start > end || end > characters.len() {
        return Err(mime_error("Invalid inline text slice"));
    }
    fn append_slice(out: &mut String, characters: &[char], start: usize, end: usize) {
        for ch in &characters[start..end] { escaped_char(out, *ch); }
    }
    let mut html = String::new();
    let mut cursor = start;
    for run in &note.rich_text {
        if run.end <= start { continue; }
        if run.start >= end { break; }
        let lo = run.start.max(start);
        let hi = run.end.min(end);
        if cursor < lo { append_slice(&mut html, characters, cursor, lo); }
        html.push_str("<span style=\"");
        html.push_str(&safe_style_css(&run.style)?);
        html.push_str("\">");
        append_slice(&mut html, characters, lo, hi);
        html.push_str("</span>");
        cursor = hi;
    }
    if cursor < end { append_slice(&mut html, characters, cursor, end); }
    Ok(html)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Stroke, TextStylePatch};

    /// Synthetic local-only benchmark for large workspaces with at least one
    /// persisted image. Run explicitly via `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn large_notebook_media_prune_benchmark() {
        use std::time::Instant;
        let folder = std::env::temp_dir().join(format!("inkpad-media-prune-bench-{}", fresh_id()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let mut workspace = store.load().unwrap();
        workspace.notes.clear();
        let filler = "Large saved text and drawing notes. ".repeat(95);
        for index in 0..2200 {
            let mut note = Note::create("note");
            note.id = format!("bench-{index}");
            note.title = format!("Notebook page {index}");
            note.body = filler.clone();
            if index == 0 {
                note.images.push(MediaBlock {
                    id: "aabbcc".into(), filename: "aabbcc.png".into(),
                    caption: String::new(), width_percent: 65, position: 0,
                });
            }
            workspace.notes.push(note);
        }
        let media = workspace.notes[0].images[0].clone();
        fs::write(store.media_path(&media).unwrap(), b"png-placeholder").unwrap();
        store.save(&workspace).unwrap();
        store.save(&workspace).unwrap();
        let mut prune_times = Vec::new();
        let mut mutation_times = Vec::new();
        for iteration in 0..14 {
            let start = Instant::now();
            assert_eq!(store.prune_unused_media_locked(&workspace).unwrap(), 0);
            prune_times.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            store.mutate(|notebook| {
                notebook.notes[0].title = format!("Notebook page {iteration}");
                Ok(())
            }).unwrap();
            mutation_times.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        prune_times.sort_by(f64::total_cmp);
        mutation_times.sort_by(f64::total_cmp);
        println!("Media prune benchmark: {:.2} MiB JSON, {} notes, prune median {:.3} ms, mutation median {:.3} ms",
            fs::metadata(store.data_path()).unwrap().len() as f64 / 1048576.0,
            workspace.notes.len(), prune_times[7], mutation_times[7]);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn inline_html_places_pictures_between_escaped_rich_text_and_preserves_legacy_order() {
        let folder = std::env::temp_dir().join(format!("inkpad-inline-html-{}", fresh_id()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let note_id = store.load().unwrap().notes[0].id.clone();
        store.mutate(|workspace| {
            let note = &mut workspace.notes[0];
            note.set_body_preserving_format("A猫🦊B <script> & end".into())
                .map_err(mime_error)?;
            note.format_text_range(0, 4, TextStylePatch {
                bold:Some(true), ..Default::default()
            }).map_err(mime_error)?;
            Ok(())
        }).unwrap();
        let mut source = Vec::new();
        PngEncoder::new(&mut source).write_image(&[100, 90, 20, 255], 1, 1,
            image::ExtendedColorType::Rgba8).unwrap();
        let first = store.insert_image_bytes(&note_id, &source).unwrap();
        let second = store.insert_image_bytes(&note_id, &source).unwrap();
        let text = store.load().unwrap().notes[0].body.clone();
        assert!(store.place_image_at_scalar(&note_id, &first.id, Some(2),
            Some("stale text")).is_err());
        assert_eq!(store.load().unwrap().notes[0].image_anchor(&first.id).unwrap(), None);
        assert!(store.place_image_at_scalar(&note_id, &first.id, Some(100),
            Some(&text)).is_err());
        store.place_image_at_scalar(&note_id, &first.id, Some(2), Some(&text)).unwrap();
        let before = store.load().unwrap();
        let output = folder.join("inline.html");
        store.export_note_html(&note_id, &output).unwrap();
        let html = fs::read_to_string(&output).unwrap();
        let rich_before = "<span style=\"font-weight:700;\">A猫</span>";
        let rich_after = "<span style=\"font-weight:700;\">🦊B</span>";
        let before_pos = html.find(rich_before).unwrap();
        let image_pos = html.find("src=\"data:image/png;base64,").unwrap();
        let after_pos = html.find(rich_after).unwrap();
        let second_pos = html.rfind("src=\"data:image/png;base64,").unwrap();
        assert!(before_pos < image_pos && image_pos < after_pos && after_pos < second_pos);
        assert!(html.contains("&lt;script&gt; &amp; end"));
        assert_eq!(html.matches("src=\"data:image/png;base64,").count(), 2);
        assert_eq!(store.load().unwrap().notes[0], before.notes[0],
            "export must not mutate text, images or anchors");

        store.delete_image(&note_id, &first.id).unwrap();
        assert!(store.load().unwrap().notes[0].inline_media.is_empty());
        let legacy_output = folder.join("legacy.html");
        store.export_note_html(&note_id, &legacy_output).unwrap();
        let legacy_html = fs::read_to_string(&legacy_output).unwrap();
        assert!(legacy_html.find("end</div>").unwrap() <
            legacy_html.find("src=\"data:image/png;base64,").unwrap());
        assert_eq!(store.list_media(&note_id).unwrap()[0].id, second.id);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn html_export_preserves_unicode_rich_text_images_drawing_and_safe_css() {
        let folder = std::env::temp_dir().join(format!("inkpad-rich-html-{}", fresh_id()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let note_id = store.load().unwrap().notes[0].id.clone();
        let text = "A👩‍🚀e\u{301}猫<script>alert(\"x\")</script>\n& end";
        // Positions 1..4 select a three-scalar emoji, 4..6 a base plus
        // combining mark, and 6..7 the multibyte Han character.
        assert_eq!(text.chars().nth(1), Some('👩'));
        assert_eq!(text.chars().nth(6), Some('猫'));
        store.mutate(|workspace| {
            let note = &mut workspace.notes[0];
            note.title = "<img src=x onerror=alert('oops')>".into();
            note.body = text.into();
            note.font_size = 19;
            note.font_choice = "Serif".into();
            note.format_text_range(1, 4, TextStylePatch {
                bold: Some(true), color: Some("#A1B2C3".into()), size: Some(23),
                font_family: Some("Serif".into()), ..Default::default()
            }).map_err(mime_error)?;
            note.format_text_range(4, 6, TextStylePatch {
                size: Some(31), font_family: Some("Mono".into()), ..Default::default()
            }).map_err(mime_error)?;
            note.format_text_range(6, 7, TextStylePatch {
                bold: Some(true), ..Default::default()
            }).map_err(mime_error)?;
            note.strokes.push(Stroke {
                color: [9, 99, 199, 255], width: 3, eraser: false,
                points: vec![[10, 20], [30, 50]],
            });
            Ok(())
        }).unwrap();

        let mut source = Vec::new();
        PngEncoder::new(&mut source).write_image(&[200, 100, 50, 255], 1, 1,
            image::ExtendedColorType::Rgba8).unwrap();
        let image = store.insert_image_bytes(&note_id, &source).unwrap();
        store.update_image(&note_id, &image.id, Some("<svg onload=alert(1)> &"),
            Some(42), None).unwrap();
        let before = store.load().unwrap();
        let output = folder.join("complete-note.html");
        store.export_note_html(&note_id, &output).unwrap();
        let html = fs::read_to_string(&output).unwrap();

        assert!(html.contains("default-src 'none'; img-src data:; style-src 'unsafe-inline'"));
        assert!(html.contains("<div class=\"body\" style=\"font-family:Georgia,serif;font-size:19px\">"));
        assert!(html.contains("A<span style=\"font-weight:700;color:#A1B2C3;font-size:23px;font-family:Georgia,serif;\">👩‍🚀</span>"));
        assert!(html.contains("<span style=\"font-size:31px;font-family:Consolas,monospace;\">e\u{301}</span>"));
        assert!(html.contains("<span style=\"font-weight:700;\">猫</span>"));
        assert!(html.contains("&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;\n&amp; end"));
        assert!(html.contains("&lt;img src=x onerror=alert(&#39;oops&#39;)&gt;"));
        assert!(html.contains("&lt;svg onload=alert(1)&gt; &amp;"));
        assert!(html.contains("style=\"width:42%;height:auto\""));
        assert_eq!(html.matches("src=\"data:image/png;base64,").count(), 2);
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<svg "));
        assert!(!html.contains("file://"));
        assert!(!html.contains(&folder.to_string_lossy().to_string()));
        assert_eq!(store.export_note_html(&note_id, &output).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists);
        assert_eq!(html, fs::read_to_string(&output).unwrap());
        assert_eq!(store.load().unwrap().notes[0], before.notes[0]);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn html_export_uses_legacy_plain_text_and_rejects_unsafe_styles() {
        let mut legacy = Note::create("note");
        legacy.body = "Before <br> & after 👋".into();
        legacy.font_choice = "evil;font-family:Comic Sans".into();
        legacy.font_size = 10000;
        let rendered = render_formatted_body(&legacy).unwrap();
        assert_eq!(rendered, "Before &lt;br&gt; &amp; after 👋");
        assert_eq!(safe_font_family(&legacy.font_choice), "'Segoe UI',Arial,sans-serif");
        assert!(safe_style_css(&TextStyle {
            color: Some("red;background:url(javascript:alert(1))".into()),
            ..Default::default()
        }).is_err());
        assert!(safe_style_css(&TextStyle {
            font_family: Some("Serif;background:red".into()),
            ..Default::default()
        }).is_err());
        assert!(safe_style_css(&TextStyle {
            size: Some(10000), ..Default::default()
        }).is_err());
    }

    #[test]
    fn jpeg_gif_webp_and_png_are_imported_as_bounded_png() {
        let pixel = image::DynamicImage::new_rgba8(2, 2);
        for source_format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Gif, ImageFormat::WebP] {
            let mut encoded = Cursor::new(Vec::new());
            pixel.write_to(&mut encoded, source_format).unwrap();
            let normalized = normalize_to_png(encoded.get_ref()).unwrap();
            assert_eq!(&normalized[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
            let decoded = image::load_from_memory(&normalized).unwrap();
            assert_eq!(decoded.dimensions(), (2, 2));
        }
    }

    #[test]
    fn image_import_resize_move_export_and_legacy_compatibility() {
        let folder = std::env::temp_dir().join(format!("inkpad-media-{}-{}", std::process::id(), fresh_id()));
        let store = Store::in_folder(folder.clone()).unwrap();
        let notes = store.load().unwrap();
        let note = &notes.notes[0];
        assert!(note.images.is_empty());
        let mut source = Vec::new();
        let pixel = [0xff, 0x55, 0x22, 0xff];
        PngEncoder::new(&mut source).write_image(&pixel, 1, 1, image::ExtendedColorType::Rgba8).unwrap();
        let first = store.insert_image_bytes(&note.id, &source).unwrap();
        let second = store.insert_image_bytes(&note.id, &source).unwrap();
        assert_ne!(first.id, second.id);
        let result = store.update_image(&note.id, &second.id, Some("A <b> & quote"), Some(40), Some(0)).unwrap();
        assert_eq!(result.position, 0);
        assert_eq!(result.width_percent, 40);
        assert_eq!(store.list_media(&note.id).unwrap()[0].id, second.id);
        assert!(store.update_image(&note.id, &first.id, None, Some(2), None).is_err());
        assert!(store.insert_image_bytes(&note.id, b"<svg><script/></svg>").is_err());
        let output = folder.join("note.html");
        store.export_note_html(&note.id, &output).unwrap();
        let html = fs::read_to_string(output).unwrap();
        assert!(html.contains("data:image/png;base64,"));
        assert!(html.contains("A &lt;b&gt; &amp; quote"));
        assert!(!html.contains(&folder.to_string_lossy().to_string()));
        assert_eq!(store.load().unwrap().notes[0].images.len(), 2);
        store.delete_image(&note.id, &first.id).unwrap();
        assert_eq!(store.list_media(&note.id).unwrap().len(), 1);
        let deleted_path = store.media_path(&first).unwrap();
        // The previous snapshot still has the deleted image until a new
        // successful mutation rotates the backup.
        assert!(deleted_path.exists());
        store.update_image(&note.id, &second.id, Some("Rotate backup"), None, None).unwrap();
        assert!(!deleted_path.exists(), "orphan media should be reclaimed after backup rotation");
        let cached = store.list_media(&note.id).unwrap();
        assert_eq!(cached[0].caption, "Rotate backup");
        let json = fs::read_to_string(store.data_path()).unwrap();
        assert!(json.contains("Rotate backup"));
        // A second process can change the JSON without changing its size.
        // Image metadata caches must observe the exact new bytes immediately.
        fs::write(store.data_path(), json.replace("Rotate backup", "Mutate backup")).unwrap();
        assert_eq!(store.list_media(&note.id).unwrap()[0].caption, "Mutate backup");
        assert!(store.media_path(&MediaBlock { filename: "../evil.png".into(), ..first }).is_err());
        fs::remove_dir_all(folder).unwrap();
    }
}
