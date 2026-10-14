//! Durable, bounded local picture attachments for Inkpad pages.
//!
//! Metadata belongs in workspace.json, while decoded and validated PNG assets
//! live beneath a private `media/` folder beside that workspace. The stored
//! filenames are opaque generated IDs, never user-provided paths.

use crate::canvas::Board;
use crate::store::{MediaBlock, Note, Store, Workspace};
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
    let pixels = decoded.to_rgba8();
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
        let workspace = self.read_latest()?;
        let note = workspace.notes.iter().find(|note| note.id == note_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Note not found"))?;
        let mut items = note.images.clone();
        items.sort_by_key(|item| item.position);
        Ok(items)
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
        let backup_file = self.data_path().with_extension("json.bak");
        let backup = match fs::read(backup_file) {
            Ok(bytes) => match serde_json::from_slice::<Workspace>(&bytes) {
                Ok(backup) => Some(backup),
                Err(_) => return Ok(0), // preserve assets if recovery state unknown
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let mut referenced = HashSet::new();
        for note in &workspace.notes {
            for item in &note.images { referenced.insert(item.filename.clone()); }
        }
        if let Some(previous) = backup {
            for note in &previous.notes {
                for item in &note.images { referenced.insert(item.filename.clone()); }
            }
        }
        let root = self.data_path().with_file_name("media");
        if !root.exists() { return Ok(0); }
        if fs::symlink_metadata(&root)?.file_type().is_symlink() {
            return Err(bad("Unsafe media directory"));
        }
        let previews = root.join("previews");
        let mut removed = 0;
        for item in fs::read_dir(&root)? {
            let item = item?;
            let info = item.metadata()?;
            if !info.is_file() || item.file_type()?.is_symlink() { continue; }
            let name = item.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".png") else { continue; };
            if validate_id(id).is_err() || referenced.contains(&name) { continue; }
            fs::remove_file(item.path())?;
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
        html.push_str("<div class=\"body\">"); html.push_str(&escape_html(&note.body)); html.push_str("</div>");
        let mut images = note.images.clone();
        images.sort_by_key(|item| item.position);
        for image in &images {
            let png = fs::read(self.media_path(image)?)?;
            html.push_str(&format!("<figure><img style=\"width:{}%;height:auto\" alt=\"{}\" src=\"data:image/png;base64,{}\">",
                image.width_percent.clamp(10, 100), escape_html(&image.caption),
                base64::engine::general_purpose::STANDARD.encode(png)));
            if !image.caption.is_empty() {
                html.push_str("<figcaption>"); html.push_str(&escape_html(&image.caption)); html.push_str("</figcaption>");
            }
            html.push_str("</figure>");
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

fn escape_html(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;")
        .replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(store.media_path(&MediaBlock { filename: "../evil.png".into(), ..first }).is_err());
        fs::remove_dir_all(folder).unwrap();
    }
}
