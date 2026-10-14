use crate::store::Stroke;
use std::io;

pub const WIDTH: u32 = 1100;
pub const HEIGHT: u32 = 700;

/// A single reusable RGBA frame buffer; undo is reconstructed from compact
/// vector strokes rather than retaining multiple full-frame screenshots.
pub struct Board {
    pixels: Vec<u8>,
}

impl Board {
    pub fn new(strokes: &[Stroke]) -> Self {
        let mut board = Self { pixels: vec![255; (WIDTH * HEIGHT * 4) as usize] };
        board.replay(strokes);
        board
    }

    pub fn replay(&mut self, strokes: &[Stroke]) {
        self.pixels.fill(255);
        for stroke in strokes { self.draw_stroke(stroke); }
    }

    pub fn bytes(&self) -> &[u8] { &self.pixels }

    pub fn draw_stroke(&mut self, stroke: &Stroke) {
        let color = if stroke.eraser { [255, 255, 255, 255] } else { stroke.color };
        for pair in stroke.points.windows(2) {
            self.line(pair[0], pair[1], stroke.width, color);
        }
        if let Some(point) = stroke.points.first() {
            self.dot(*point, stroke.width, color);
        }
    }

    pub fn line(&mut self, from: [u16; 2], to: [u16; 2], width: u16, color: [u8; 4]) {
        let dx = to[0] as f32 - from[0] as f32;
        let dy = to[1] as f32 - from[1] as f32;
        let steps = ((dx * dx + dy * dy).sqrt() / ((width as f32) / 4.0).max(1.0)).ceil() as usize;
        for i in 0..=steps.max(1) {
            let t = i as f32 / steps.max(1) as f32;
            self.dot([(from[0] as f32 + dx * t).round() as u16,
                (from[1] as f32 + dy * t).round() as u16], width, color);
        }
    }

    pub fn dot(&mut self, center: [u16; 2], width: u16, color: [u8; 4]) {
        let radius = (width.max(1) as f32 / 2.0).max(0.65);
        let cx = center[0] as i32;
        let cy = center[1] as i32;
        let extent = radius.ceil() as i32;
        for y in (cy - extent).max(0)..=(cy + extent).min(HEIGHT as i32 - 1) {
            for x in (cx - extent).max(0)..=(cx + extent).min(WIDTH as i32 - 1) {
                let dx = x as f32 - cx as f32;
                let dy = y as f32 - cy as f32;
                if dx * dx + dy * dy <= radius * radius {
                    let offset = ((y as u32 * WIDTH + x as u32) * 4) as usize;
                    self.pixels[offset..offset + 4].copy_from_slice(&color);
                }
            }
        }
    }

    pub fn png_bytes(&self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, WIDTH, HEIGHT);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header()?;
            writer.write_image_data(&self.pixels)?;
        }
        Ok(bytes)
    }
}

pub fn parse_color(hex: &str) -> [u8; 4] {
    let value = hex.trim().trim_start_matches('#');
    if value.len() == 6 {
        if let Ok(rgb) = u32::from_str_radix(value, 16) {
            return [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255];
        }
    }
    [36, 41, 45, 255]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strokes_and_eraser_replay() {
        let black = Stroke { color: [1, 2, 3, 255], width: 11, eraser: false, points: vec![[20, 20], [30, 20]] };
        let mut board = Board::new(&[black.clone()]);
        assert_eq!(&board.bytes()[((20 * WIDTH + 25) * 4) as usize..][..4], &[1,2,3,255]);
        board.replay(&[black, Stroke { color: [0,0,0,255], width: 15, eraser: true, points: vec![[25, 20]] }]);
        assert_eq!(&board.bytes()[((20 * WIDTH + 25) * 4) as usize..][..4], &[255,255,255,255]);
        assert_eq!(&board.png_bytes().unwrap()[..8], &[137,80,78,71,13,10,26,10]);
    }
    #[test]
    fn colors_reject_bad_input() {
        assert_eq!(parse_color("#FF0080"), [255,0,128,255]);
        assert_eq!(parse_color("oops"), [36,41,45,255]);
    }
}
