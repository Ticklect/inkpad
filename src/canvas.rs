use crate::store::{Stroke, Note};
use crate::store::drawing::{DrawingData, DrawObject, DrawShape, DrawFill, PathCommand, Point, GradientStop, Transform};
use std::io;

pub const WIDTH: u32 = 1100;
pub const HEIGHT: u32 = 700;

/// A single reusable RGBA frame buffer; undo is reconstructed from compact
/// vector strokes rather than retaining multiple full-frame screenshots.
#[derive(Clone)]
pub struct Board {
    pixels: Vec<u8>,
}

impl Board {
    pub fn new(strokes: &[Stroke]) -> Self {
        let mut board = Self { pixels: vec![255; (WIDTH * HEIGHT * 4) as usize] };
        board.replay(strokes);
        board
    }

    /// Full saved note preview: legacy freehand strokes plus editable vectors.
    /// Kept separate from `new` for callers that only draw a live brush stroke.
    pub fn from_note(note: &Note) -> Self {
        Self::with_drawing(&note.strokes, &note.drawing)
    }

    pub fn with_drawing(strokes: &[Stroke], drawing: &DrawingData) -> Self {
        let mut board = Self::new(strokes);
        board.draw_drawing(drawing);
        board
    }

    pub fn draw_drawing(&mut self, drawing: &DrawingData) {
        // Reject malformed saved data before rendering. This is also checked by
        // Store during file load and save, but protects direct Board callers.
        if drawing.validate().is_err() { return; }
        // Layers determine the draw order; objects remain ordered within each layer.
        for layer in &drawing.layers {
            if layer.visible {
                for object in &drawing.objects {
                    if object.layer_id == layer.id && object.visible {
                        self.draw_object(object);
                    }
                }
            }
        }
    }

    pub fn draw_object(&mut self, object: &DrawObject) {
        if object.validate().is_err() { return; }
        // Preserve the exact legacy pixel pipeline for existing saved objects.
        // Compound outlines, clipping and shadows opt into the new renderer.
        if matches!(object.shape, DrawShape::Path { .. }) || object.clip_shape.is_some() ||
            object.style.shadow.is_some() {
            self.draw_compound_object(object);
            return;
        }
        let (path, closed) = flattened_path_for_scale(&object.shape,
            object.transform.scale_x.max(object.transform.scale_y));
        if path.len() < 2 { return; }
        let transformed: Vec<Point> = path.iter().map(|p| object.transform.apply(*p)).collect();
        let scaled_width = object.style.stroke_width *
            object.transform.scale_x.max(object.transform.scale_y);
        let pad = scaled_width / 2.0 + 1.5;
        let left = transformed.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
        let right = transformed.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max);
        let top = transformed.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
        let bottom = transformed.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max);
        if left - pad >= WIDTH as f32 || right + pad < 0.0 ||
            top - pad >= HEIGHT as f32 || bottom + pad < 0.0 { return; }
        let x0 = ((left - pad).floor() as i32).clamp(0, WIDTH as i32 - 1);
        let x1 = ((right + pad).ceil() as i32).clamp(0, WIDTH as i32 - 1);
        let y0 = ((top - pad).floor() as i32).clamp(0, HEIGHT as i32 - 1);
        let y1 = ((bottom + pad).ceil() as i32).clamp(0, HEIGHT as i32 - 1);
        let fill = if closed { object.style.fill.as_ref() } else { None };
        let stroke = object.style.stroke.filter(|_| object.style.stroke_width > 0.0);
        // Shape validation has already run. Alpha-zero colours are invisible;
        // this shortcut matters for AI drawings with many hidden paint objects.
        if object.style.opacity == 0.0 ||
            (fill.is_none_or(|fill| fill_is_transparent(fill)) &&
             stroke.is_none_or(|color| color[3] == 0)) { return; }

        // Supersampling still uses the same four locations and blending as the
        // original renderer. Cache polygon intersections for the two y-samples
        // of each scanline instead of rescanning/dividing every edge four times
        // per pixel. The parity of crossings strictly to the right is the
        // exact even-odd rule used by point_in_polygon.
        let mut top_crossings = Vec::with_capacity(8);
        let mut bottom_crossings = Vec::with_capacity(8);
        // Segment bounds also let us skip almost every ellipse/Bezier edge
        // when testing a pixel far from the outline.
        let segments = if stroke.is_some() {
            prepare_segments(&transformed, closed)
        } else { Vec::new() };
        let mut row_segments = Vec::with_capacity(segments.len().min(16));
        let fill_sampler = FillSampler::new(&object.transform);
        let stroke_radius = scaled_width / 2.0;
        let stroke_radius_sq = scaled_width * scaled_width / 4.0;
        let margin = stroke_radius + 0.5;
        for y in y0..=y1 {
            if fill.is_some() {
                polygon_scanline_crossings(&transformed, y as f32 - 0.25, &mut top_crossings);
                polygon_scanline_crossings(&transformed, y as f32 + 0.25, &mut bottom_crossings);
            }
            row_segments.clear();
            if stroke.is_some() {
                let y = y as f32;
                row_segments.extend(segments.iter().filter(|segment|
                    y >= segment.min_y - margin && y <= segment.max_y + margin));
            }
            // The bounds of the whole shape can be much wider than the pixels
            // touched on a single scanline (especially curves and thin lines).
            // Crossings bound every filled sample; active segment bounds cover
            // every stroked sample. The extra pixel keeps subpixel edges exact.
            let mut row_left = f32::INFINITY;
            let mut row_right = f32::NEG_INFINITY;
            if fill.is_some() {
                for crossings in [&top_crossings, &bottom_crossings] {
                    if let (Some(&left), Some(&right)) = (crossings.first(), crossings.last()) {
                        row_left = row_left.min(left);
                        row_right = row_right.max(right);
                    }
                }
            }
            for segment in &row_segments {
                row_left = row_left.min(segment.min_x - margin);
                row_right = row_right.max(segment.max_x + margin);
            }
            if !row_left.is_finite() { continue; }
            let start_x = ((row_left - 1.0).floor() as i32).clamp(x0, x1);
            let end_x = ((row_right + 1.0).ceil() as i32).clamp(x0, x1);
            if start_x > end_x { continue; }
            for x in start_x..=end_x {
                // Four subsamples smooth edges and make thin diagonal lines visible.
                let samples = [(-0.25,-0.25),(0.25,-0.25),(-0.25,0.25),(0.25,0.25)];
                let mut fill_hits = 0;
                let mut stroke_hits = 0;
                for (ox, oy) in samples {
                    let p = Point::new(x as f32 + ox, y as f32 + oy);
                    if fill.is_some() && inside_crossings(p.x,
                        if oy < 0.0 { &top_crossings } else { &bottom_crossings }) {
                        fill_hits += 1;
                    }
                    if stroke.is_some() && row_segments.iter().any(|segment|
                        p.x >= segment.min_x - margin && p.x <= segment.max_x + margin &&
                        segment_distance_sq(p, segment) <= stroke_radius_sq) {
                        stroke_hits += 1;
                    }
                }
                if fill_hits == 0 && stroke_hits == 0 { continue; }
                let offset = ((y as u32 * WIDTH + x as u32) * 4) as usize;
                if let Some(fill) = fill {
                    if fill_hits > 0 {
                        let color = fill_sampler.sample(fill, x, y);
                        self.blend(offset, color, object.style.opacity * (fill_hits as f32 / 4.0));
                    }
                }
                if let Some(color) = stroke {
                    if stroke_hits > 0 {
                        self.blend(offset, color, object.style.opacity * (stroke_hits as f32 / 4.0));
                    }
                }
            }
        }
    }

    fn blend(&mut self, offset: usize, color: [u8; 4], coverage: f32) {
        let alpha = (color[3] as f32 / 255.0) * coverage.clamp(0.0, 1.0);
        if alpha <= 0.0 { return; }
        for (channel, component) in color.iter().take(3).enumerate() {
            self.pixels[offset + channel] =
                (*component as f32 * alpha + self.pixels[offset + channel] as f32 * (1.0-alpha))
                    .round().clamp(0.0, 255.0) as u8;
        }
        // Board starts as opaque white, so the final alpha remains opaque.
        self.pixels[offset + 3] = 255;
    }

    fn draw_compound_object(&mut self, object: &DrawObject) {
        let scale = object.transform.scale_x.max(object.transform.scale_y);
        let contours = transformed_contours(&object.shape, &object.transform, scale);
        let clip = object.clip_shape.as_ref()
            .map(|shape| transformed_contours(shape, &object.transform, scale));
        let fill = object.style.fill.as_ref();
        let stroke = object.style.stroke.filter(|color|
            color[3] != 0 && object.style.stroke_width > 0.0);
        let shadow_enabled = object.style.shadow.as_ref().is_some_and(|s| s.color[3] > 0);
        if object.style.opacity <= 0.0 || (fill.is_none_or(fill_is_transparent) && stroke.is_none()) {
            return;
        }
        let segments: Vec<PreparedSegment> = if stroke.is_some() {
            contours.iter().flat_map(|c| prepare_segments(&c.points, c.closed)).collect()
        } else { Vec::new() };
        let width = object.style.stroke_width * scale;
        let pad = if stroke.is_some() { width / 2.0 + 1.5 } else { 1.5 };
        let Some((x0,y0,x1,y1)) = contour_bounds(&contours, pad) else { return; };
        let clipped = clip.is_some();
        let fill_sampler = FillSampler::new(&object.transform);
        let mut fill_top = Vec::new();
        let mut fill_bottom = Vec::new();
        let mut clip_top = Vec::new();
        let mut clip_bottom = Vec::new();
        // A local column index prevents compound curves with hundreds of
        // control points from checking every segment at every pixel.
        const COLUMN_PIXELS: i32 = 32;
        let column_count = ((x1-x0)/COLUMN_PIXELS+1) as usize;
        let mut stroke_columns: Vec<Vec<&PreparedSegment>> = vec![Vec::new();column_count];
        let mut alpha = if shadow_enabled {
            vec![0u8; ((x1-x0+1)*(y1-y0+1)) as usize]
        } else { Vec::new() };
        // For a shadowed shape, preserve the exact four-sample coverage from
        // the alpha pass for foreground blending after the shadow is painted.
        // Four fill and four stroke samples fit into six bits per pixel.
        // This avoids flattening and rasterizing clipped paths a second time.
        let mut foreground_hits = if shadow_enabled {
            vec![0u8; alpha.len()]
        } else { Vec::new() };
        for y in y0..=y1 {
            if fill.is_some() {
                crossings_contours(&contours, y as f32 - 0.25, &mut fill_top);
                crossings_contours(&contours, y as f32 + 0.25, &mut fill_bottom);
            }
            if let Some(ref clip) = clip {
                crossings_contours(clip, y as f32 - 0.25, &mut clip_top);
                crossings_contours(clip, y as f32 + 0.25, &mut clip_bottom);
            }
            for bucket in &mut stroke_columns { bucket.clear(); }
            let mut row_left = f32::INFINITY;
            let mut row_right = f32::NEG_INFINITY;
            if fill.is_some() {
                for crossings in [&fill_top, &fill_bottom] {
                    if let (Some(&left), Some(&right)) = (crossings.first(), crossings.last()) {
                        row_left = row_left.min(left);
                        row_right = row_right.max(right);
                    }
                }
            }
            if stroke.is_some() {
                let margin = width/2.0+0.5;
                for segment in segments.iter().filter(|s|
                    y as f32 >= s.min_y-margin && y as f32 <= s.max_y+margin) {
                    let left = (segment.min_x-margin).floor().max(x0 as f32);
                    let right = (segment.max_x+margin).ceil().min(x1 as f32);
                    if left > right { continue; }
                    row_left = row_left.min(left);
                    row_right = row_right.max(right);
                    let a = ((left as i32-x0)/COLUMN_PIXELS) as usize;
                    let b = ((right as i32-x0)/COLUMN_PIXELS) as usize;
                    for bucket in &mut stroke_columns[a..=b] { bucket.push(segment); }
                }
            }
            if clipped {
                let mut clip_left = f32::INFINITY;
                let mut clip_right = f32::NEG_INFINITY;
                for crossings in [&clip_top, &clip_bottom] {
                    if let (Some(&left), Some(&right)) = (crossings.first(), crossings.last()) {
                        clip_left = clip_left.min(left);
                        clip_right = clip_right.max(right);
                    }
                }
                row_left = row_left.max(clip_left);
                row_right = row_right.min(clip_right);
            }
            if !row_left.is_finite() || !row_right.is_finite() || row_left > row_right { continue; }
            let start_x = ((row_left - 1.0).floor() as i32).clamp(x0, x1);
            let end_x = ((row_right + 1.0).ceil() as i32).clamp(x0, x1);
            if start_x > end_x { continue; }
            for x in start_x..=end_x {
                let column = &stroke_columns[((x-x0)/COLUMN_PIXELS) as usize];
                if fill.is_none() && column.is_empty() { continue; }
                let mut fills = 0;
                let mut strokes = 0;
                for (ox,oy) in [(-0.25,-0.25),(0.25,-0.25),(-0.25,0.25),(0.25,0.25)] {
                    let p = Point::new(x as f32+ox, y as f32+oy);
                    if clipped && !inside_crossings(p.x,
                        if oy < 0.0 { &clip_top } else { &clip_bottom }) { continue; }
                    if fill.is_some() && inside_crossings(p.x,
                        if oy < 0.0 { &fill_top } else { &fill_bottom }) { fills += 1; }
                    if stroke.is_some() && column.iter().any(|s| {
                        let margin = width/2.0+0.5;
                        p.x >= s.min_x-margin && p.x <= s.max_x+margin &&
                        segment_distance_sq(p,s) <= width*width/4.0
                    }) { strokes += 1; }
                }
                if fills == 0 && strokes == 0 { continue; }
                let offset = ((y as u32 * WIDTH + x as u32) * 4) as usize;
                if !alpha.is_empty() {
                    let index = ((y-y0)*(x1-x0+1)+(x-x0)) as usize;
                    foreground_hits[index] = (fills as u8) | ((strokes as u8) << 3);
                    let fill_a = if let Some(f) = fill {
                        fill_sampler.sample(f,x,y)[3] as f32 * fills as f32 / 4.0
                    } else { 0.0 };
                    let stroke_a = stroke.map(|c| c[3] as f32 * strokes as f32 / 4.0).unwrap_or(0.0);
                    // Union alpha before the blur; transparent fill does not cast shadow.
                    alpha[index] =
                        (255.0 * (1.0-(1.0-fill_a/255.0)*(1.0-stroke_a/255.0))) as u8;
                }
                if let Some(f) = fill.filter(|_| !shadow_enabled) {
                    if fills > 0 {
                        self.blend(offset, fill_sampler.sample(f,x,y), object.style.opacity * fills as f32/4.0);
                    }
                }
                if let Some(c) = stroke.filter(|_| !shadow_enabled) {
                    if strokes > 0 { self.blend(offset,c,object.style.opacity*strokes as f32/4.0); }
                }
            }
        }
        if shadow_enabled {
            if let Some(shadow) = &object.style.shadow {
                self.draw_soft_shadow(&alpha, (x0,y0,x1,y1), object, shadow);
            }
            // Preserve foreground order: original software path painted the
            // blurred shadow, then fill, then stroke at each pixel. Replaying
            // saved coverage keeps float math, AA and layer compositing intact.
            let row_len = (x1-x0+1) as usize;
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let hit = foreground_hits[(y-y0) as usize*row_len + (x-x0) as usize];
                    if hit == 0 { continue; }
                    let offset = ((y as u32 * WIDTH + x as u32) * 4) as usize;
                    let fills = (hit & 7) as f32;
                    let strokes = (hit >> 3) as f32;
                    if let Some(f) = fill {
                        if fills > 0.0 {
                            self.blend(offset, fill_sampler.sample(f,x,y),
                                object.style.opacity*fills/4.0);
                        }
                    }
                    if let Some(c) = stroke {
                        if strokes > 0.0 {
                            self.blend(offset,c,object.style.opacity*strokes/4.0);
                        }
                    }
                }
            }
        }
    }

    fn draw_soft_shadow(&mut self, alpha: &[u8], bounds: (i32,i32,i32,i32),
        object: &DrawObject, shadow: &crate::store::drawing::DrawShadow) {
        if shadow.color[3] == 0 { return; }
        let (x0,y0,x1,y1) = bounds;
        let scale = object.transform.scale_x.max(object.transform.scale_y);
        // Blur size is bounded independently of geometry scale, preventing
        // enormous temporaries for malformed or adversarial artwork.
        let radius = (shadow.blur_radius * scale).ceil().min(48.0) as i32;
        let centre = object.transform.apply(Point::default());
        let target = object.transform.apply(shadow.offset);
        let dx = (target.x - centre.x).round() as i32;
        let dy = (target.y - centre.y).round() as i32;
        let left = (x0.saturating_add(dx).saturating_sub(radius*2)).clamp(0,WIDTH as i32-1);
        let right = (x1.saturating_add(dx).saturating_add(radius*2)).clamp(0,WIDTH as i32-1);
        let top = (y0.saturating_add(dy).saturating_sub(radius*2)).clamp(0,HEIGHT as i32-1);
        let bottom = (y1.saturating_add(dy).saturating_add(radius*2)).clamp(0,HEIGHT as i32-1);
        if left > right || top > bottom { return; }
        let w = (right-left+1) as usize;
        let h = (bottom-top+1) as usize;
        let mut buffer = vec![0u8;w*h];
        let source_width = (x1-x0+1) as usize;
        for y in top..=bottom {
            let sy = y.saturating_sub(dy).saturating_sub(y0);
            if sy < 0 || sy > y1-y0 { continue; }
            for x in left..=right {
                let sx = x.saturating_sub(dx).saturating_sub(x0);
                if sx >= 0 && sx <= x1-x0 {
                    buffer[(y-top) as usize*w+(x-left) as usize] =
                        alpha[sy as usize*source_width+sx as usize];
                }
            }
        }
        if radius > 0 {
            // Three small, separable box blurs approximate a smooth Gaussian.
            // Each pass is linear in pixels, independent of the blur radius.
            let r = ((radius+1)/2) as usize;
            for _ in 0..3 { buffer = box_blur(&buffer,w,h,r); }
        }
        for y in top..=bottom {
            for x in left..=right {
                let coverage = buffer[(y-top) as usize*w+(x-left) as usize] as f32 / 255.0;
                if coverage > 0.0 {
                    self.blend(((y as u32*WIDTH+x as u32)*4) as usize, shadow.color,
                        coverage*object.style.opacity);
                }
            }
        }
    }

    pub fn replay(&mut self, strokes: &[Stroke]) {
        self.pixels.fill(255);
        for stroke in strokes { self.draw_stroke(stroke); }
    }

    pub fn bytes(&self) -> &[u8] { &self.pixels }

    /// Replace the frame in place, retaining the allocated backing buffer.
    /// Both frames are always the fixed 1100×700 RGBA layout.
    pub fn copy_pixels_from(&mut self, other: &Board) {
        self.pixels.copy_from_slice(&other.pixels);
    }

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
                    self.blend(offset, color, 1.0);
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

/// Capture the inverse transform once per shape, rather than evaluating
/// sin/cos for every gradient-covered pixel. Keep the arithmetic order of
/// Transform::inverse so gradient colours remain byte-for-byte identical.
struct FillSampler {
    translation: Point,
    sin: f32,
    cos: f32,
    sx: f32,
    sy: f32,
}

impl FillSampler {
    fn new(transform: &Transform) -> Self {
        let (sin, cos) = transform.rotation_deg.to_radians().sin_cos();
        Self { translation: transform.translation, sin, cos,
            sx: transform.scale_x * if transform.flip_x { -1.0 } else { 1.0 },
            sy: transform.scale_y * if transform.flip_y { -1.0 } else { 1.0 } }
    }

    fn sample(&self, fill: &DrawFill, x: i32, y: i32) -> [u8;4] {
        if let DrawFill::Solid { color } = fill { return *color; }
        let x = x as f32 - self.translation.x;
        let y = y as f32 - self.translation.y;
        sample_fill(fill, Point::new((x*self.cos + y*self.sin) / self.sx,
            (-x*self.sin + y*self.cos) / self.sy))
    }
}

struct PreparedSegment {
    start: Point,
    dx: f32,
    dy: f32,
    length_sq: f32,
    min_x: f32,
    max_x: f32,
    min_y: f32,
    max_y: f32,
}

#[derive(Debug)]
struct Contour { points: Vec<Point>, closed: bool }

fn path_contours(commands: &[PathCommand], scale: f32) -> Vec<Contour> {
    let mut contours = Vec::new();
    let mut points: Vec<Point> = Vec::new();
    let mut current = Point::default();
    for command in commands {
        match command {
            PathCommand::MoveTo { to } => {
                if points.len() >= 2 { contours.push(Contour { points: std::mem::take(&mut points), closed:false }); }
                points.clear();
                points.push(*to);
                current = *to;
            }
            PathCommand::LineTo { to } => { points.push(*to); current = *to; }
            PathCommand::CubicTo { control1, control2, to } => {
                // Bound adaptive subdivision to <=64 vertices per cubic command.
                // Curves with control handles much longer than their chords get
                // more samples; a 256-command path stays under 16,385 points.
                let control_length = current.x.hypot(current.y)
                    .max(control1.x.hypot(control1.y))
                    .max(control2.x.hypot(control2.y)).max(to.x.hypot(to.y));
                let complexity = ((current.x-control1.x).hypot(current.y-control1.y) +
                    (control1.x-control2.x).hypot(control1.y-control2.y) +
                    (control2.x-to.x).hypot(control2.y-to.y) -
                    (current.x-to.x).hypot(current.y-to.y)).max(0.0);
                let steps = ((complexity*scale/2.0).sqrt()*4.0).ceil() as usize;
                let steps = steps.max(if control_length*scale > 1200.0 { 24 } else { 8 }).min(64);
                let from = current;
                for i in 1..=steps {
                    let t = i as f32/steps as f32;
                    let u = 1.0-t;
                    points.push(Point::new(
                        u*u*u*from.x+3.0*u*u*t*control1.x+3.0*u*t*t*control2.x+t*t*t*to.x,
                        u*u*u*from.y+3.0*u*u*t*control1.y+3.0*u*t*t*control2.y+t*t*t*to.y));
                }
                current = *to;
            }
            PathCommand::QuadTo { control, to } => {
                let complexity = (current.x-control.x).hypot(current.y-control.y) +
                    (control.x-to.x).hypot(control.y-to.y) -
                    (current.x-to.x).hypot(current.y-to.y);
                let steps = (((complexity*scale/2.0).max(0.0)).sqrt()*4.0).ceil() as usize;
                let steps = steps.clamp(8,48);
                let from = current;
                for i in 1..=steps {
                    let t=i as f32/steps as f32;
                    let u=1.0-t;
                    points.push(Point::new(u*u*from.x+2.0*u*t*control.x+t*t*to.x,
                                           u*u*from.y+2.0*u*t*control.y+t*t*to.y));
                }
                current = *to;
            }
            PathCommand::Close => {
                if points.len() >= 2 { contours.push(Contour { points: std::mem::take(&mut points), closed:true }); }
                points.clear();
            }
        }
    }
    if points.len() >= 2 { contours.push(Contour { points, closed:false }); }
    contours
}

fn transformed_contours(shape: &DrawShape, transform: &crate::store::drawing::Transform,
    scale: f32) -> Vec<Contour> {
    let mut result = match shape {
        DrawShape::Path { commands } => path_contours(commands,scale),
        _ => {
            let (points,closed) = flattened_path_for_scale(shape,scale);
            vec![Contour { points, closed }]
        }
    };
    for contour in &mut result {
        for p in &mut contour.points { *p = transform.apply(*p); }
    }
    result
}

fn contour_bounds(contours: &[Contour], pad: f32) -> Option<(i32,i32,i32,i32)> {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for contour in contours { for p in &contour.points {
        min_x = min_x.min(p.x); min_y = min_y.min(p.y);
        max_x = max_x.max(p.x); max_y = max_y.max(p.y);
    }}
    if !min_x.is_finite() || min_x-pad>=WIDTH as f32 || max_x+pad<0.0 ||
        min_y-pad>=HEIGHT as f32 || max_y+pad<0.0 { return None; }
    Some((((min_x-pad).floor() as i32).clamp(0,WIDTH as i32-1),
          ((min_y-pad).floor() as i32).clamp(0,HEIGHT as i32-1),
          ((max_x+pad).ceil() as i32).clamp(0,WIDTH as i32-1),
          ((max_y+pad).ceil() as i32).clamp(0,HEIGHT as i32-1)))
}

fn crossings_contours(contours: &[Contour], y: f32, crossings: &mut Vec<f32>) {
    crossings.clear();
    for contour in contours {
        let path = &contour.points;
        for i in 0..path.len() {
            // Open subpaths are implicitly closed for fill and clip, but not stroke.
            let a = path[i];
            let b = path[(i+1)%path.len()];
            if (a.y>y) != (b.y>y) {
                crossings.push((b.x-a.x)*(y-a.y)/(b.y-a.y)+a.x);
            }
        }
    }
    crossings.sort_unstable_by(f32::total_cmp);
}

fn box_blur(input: &[u8], w: usize, h: usize, radius: usize) -> Vec<u8> {
    if radius == 0 { return input.to_vec(); }
    let mut horizontal = vec![0u8;w*h];
    let size = (2*radius+1) as i32;
    for y in 0..h {
        let mut sum = 0i32;
        for x in 0..w {
            if x == 0 {
                for k in 0..=radius.min(w.saturating_sub(1)) { sum += input[y*w+k] as i32; }
            } else {
                if x+radius < w { sum += input[y*w+x+radius] as i32; }
                if x>radius { sum -= input[y*w+x-radius-1] as i32; }
            }
            horizontal[y*w+x] = (sum/size) as u8;
        }
    }
    let mut output = vec![0u8;w*h];
    for x in 0..w {
        let mut sum = 0i32;
        for y in 0..h {
            if y == 0 {
                for k in 0..=radius.min(h.saturating_sub(1)) { sum += horizontal[k*w+x] as i32; }
            } else {
                if y+radius < h { sum += horizontal[(y+radius)*w+x] as i32; }
                if y>radius { sum -= horizontal[(y-radius-1)*w+x] as i32; }
            }
            output[y*w+x] = (sum/size) as u8;
        }
    }
    output
}

fn prepare_segments(path: &[Point], closed: bool) -> Vec<PreparedSegment> {
    let count = if closed { path.len() } else { path.len() - 1 };
    (0..count).map(|i| {
        let a = path[i];
        let b = path[(i+1) % path.len()];
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        PreparedSegment { start:a, dx,dy,length_sq:dx*dx+dy*dy,
            min_x:a.x.min(b.x), max_x:a.x.max(b.x),
            min_y:a.y.min(b.y), max_y:a.y.max(b.y) }
    }).collect()
}

fn segment_distance_sq(point: Point, segment: &PreparedSegment) -> f32 {
    let t = if segment.length_sq > 0.000001 {
        ((point.x-segment.start.x)*segment.dx +
         (point.y-segment.start.y)*segment.dy) / segment.length_sq
    } else { 0.0 }.clamp(0.0, 1.0);
    let px = point.x-segment.start.x - t*segment.dx;
    let py = point.y-segment.start.y - t*segment.dy;
    px*px + py*py
}

fn polygon_scanline_crossings(path: &[Point], y: f32, crossings: &mut Vec<f32>) {
    crossings.clear();
    for i in 0..path.len() {
        let a = path[i];
        let b = path[(i+1)%path.len()];
        if (a.y > y) != (b.y > y) {
            crossings.push((b.x-a.x)*(y-a.y)/(b.y-a.y)+a.x);
        }
    }
    crossings.sort_unstable_by(f32::total_cmp);
}

fn inside_crossings(x: f32, crossings: &[f32]) -> bool {
    (crossings.len() - crossings.partition_point(|crossing| *crossing <= x)) % 2 != 0
}

fn fill_is_transparent(fill: &DrawFill) -> bool {
    match fill {
        DrawFill::Solid { color } => color[3] == 0,
        DrawFill::LinearGradient { stops, .. } | DrawFill::RadialGradient { stops, .. } |
        DrawFill::EllipticalGradient { stops, .. } =>
            stops.iter().all(|stop| stop.color[3] == 0),
    }
}

fn flattened_path(shape: &DrawShape) -> (Vec<Point>, bool) {
    match shape {
        DrawShape::Rectangle { width, height } => (vec![
            Point::new(0.0,0.0), Point::new(*width,0.0),
            Point::new(*width,*height), Point::new(0.0,*height)], true),
        DrawShape::RoundedRectangle { width, height, radius } => {
            let mut path = Vec::with_capacity(32);
            for (cx,cy,start) in [
                (width-radius, *radius, -90.0_f32),
                (width-radius, height-radius, 0.0),
                (*radius, height-radius, 90.0),
                (*radius, *radius, 180.0),
            ] {
                for i in 0..=8 {
                    let angle = (start + i as f32 * 90.0 / 8.0).to_radians();
                    path.push(Point::new(cx + radius * angle.cos(), cy + radius * angle.sin()));
                }
            }
            (path, true)
        }
        DrawShape::Circle { radius } => (ellipse_path(*radius, *radius), true),
        DrawShape::Ellipse { radius_x, radius_y } =>
            (ellipse_path(*radius_x, *radius_y), true),
        DrawShape::Line { to } => (vec![Point::default(), *to], false),
        DrawShape::Polygon { points, closed } => (points.clone(), *closed),
        DrawShape::CubicBezier { control1, control2, to } => {
            let mut path = Vec::with_capacity(65);
            for i in 0..=64 {
                let t = i as f32 / 64.0;
                let u = 1.0 - t;
                path.push(Point::new(
                    3.0*u*u*t*control1.x + 3.0*u*t*t*control2.x + t*t*t*to.x,
                    3.0*u*u*t*control1.y + 3.0*u*t*t*control2.y + t*t*t*to.y,
                ));
            }
            (path, false)
        }
        DrawShape::Path { commands } => path_contours(commands,1.0).into_iter()
            .next().map(|p| (p.points,p.closed)).unwrap_or_default(),
    }
}

/// Most saved drawings keep their original tessellation and exact PNG pixels.
/// Exceptionally large curves need more vertices: the original eight rounded
/// corner steps or 96 ellipse edges can become visibly faceted when an agent
/// creates a large zoomed shape. Clamp subdivision to bound CPU and memory.
fn flattened_path_for_scale(shape: &DrawShape, scale: f32) -> (Vec<Point>, bool) {
    const MAX_CURVE_STEPS: usize = 512;
    match shape {
        DrawShape::Circle { radius } if radius * scale > 500.0 => {
            let steps = ellipse_steps(radius * scale).min(MAX_CURVE_STEPS);
            (ellipse_path_steps(*radius, *radius, steps), true)
        }
        DrawShape::Ellipse { radius_x, radius_y }
            if radius_x.max(*radius_y) * scale > 500.0 => {
            let steps = ellipse_steps(radius_x.max(*radius_y) * scale).min(MAX_CURVE_STEPS);
            (ellipse_path_steps(*radius_x, *radius_y, steps), true)
        }
        DrawShape::RoundedRectangle { width, height, radius } if radius * scale > 150.0 => {
            let steps = ((std::f32::consts::FRAC_PI_2 *
                (radius * scale / 2.0).sqrt()).ceil() as usize).clamp(8, 64);
            let mut path = Vec::with_capacity(4 * (steps + 1));
            for (cx,cy,start) in [
                (width-radius, *radius, -90.0_f32),
                (width-radius, height-radius, 0.0),
                (*radius, height-radius, 90.0),
                (*radius, *radius, 180.0),
            ] {
                for i in 0..=steps {
                    let angle = (start + i as f32 * 90.0 / steps as f32).to_radians();
                    path.push(Point::new(cx + radius * angle.cos(), cy + radius * angle.sin()));
                }
            }
            (path, true)
        }
        DrawShape::CubicBezier { control1, control2, to } => {
            let maximum = control1.x.abs().max(control1.y.abs())
                .max(control2.x.abs()).max(control2.y.abs()).max(to.x.abs()).max(to.y.abs()) * scale;
            if maximum <= 800.0 { return flattened_path(shape); }
            let steps = ((maximum / 800.0).sqrt() * 64.0).ceil() as usize;
            let steps = steps.clamp(64, MAX_CURVE_STEPS);
            let mut path = Vec::with_capacity(steps+1);
            for i in 0..=steps {
                let t = i as f32 / steps as f32;
                let u = 1.0 - t;
                path.push(Point::new(
                    3.0*u*u*t*control1.x + 3.0*u*t*t*control2.x + t*t*t*to.x,
                    3.0*u*u*t*control1.y + 3.0*u*t*t*control2.y + t*t*t*to.y,
                ));
            }
            (path, false)
        }
        _ => flattened_path(shape),
    }
}

fn ellipse_steps(screen_radius: f32) -> usize {
    // Circle sagitta approx PI^2 * radius / (2 * segments^2).
    // Aim for <=0.2 screen pixels, keeping the original 96-edge minimum.
    (std::f32::consts::PI * (screen_radius / 0.4).sqrt()).ceil() as usize
}

fn ellipse_path(radius_x: f32, radius_y: f32) -> Vec<Point> {
    ellipse_path_steps(radius_x, radius_y, 96)
}

fn ellipse_path_steps(radius_x: f32, radius_y: f32, steps: usize) -> Vec<Point> {
    (0..steps).map(|i| {
        let angle = i as f32 * std::f32::consts::TAU / steps as f32;
        Point::new(radius_x * angle.cos(), radius_y * angle.sin())
    }).collect()
}

/// Returns the frontmost editable visible object at a canvas point. Locked
/// layers are intentionally skipped so pointer selection cannot edit them.
pub fn pick_object(drawing: &DrawingData, at: Point, tolerance: f32) -> Option<String> {
    if !at.x.is_finite() || !at.y.is_finite() || !tolerance.is_finite() ||
        !(0.0..=32.0).contains(&tolerance) { return None; }
    for layer in drawing.layers.iter().rev().filter(|l| l.visible && !l.locked) {
        for object in drawing.objects.iter().rev().filter(|o| {
            o.layer_id == layer.id && o.visible && o.style.opacity > 0.0
        }) {
            if object.validate().is_err() { continue; }
            if matches!(object.shape, DrawShape::Path { .. }) || object.clip_shape.is_some() {
                let scale = object.transform.scale_x.max(object.transform.scale_y);
                let contours = transformed_contours(&object.shape,&object.transform,scale);
                if let Some(clip_shape) = &object.clip_shape {
                    let clip = transformed_contours(clip_shape,&object.transform,scale);
                    if !point_in_contours(at,&clip) { continue; }
                }
                let fill_hit = object.style.fill.as_ref().is_some_and(|f|
                    sample_fill(f,object.transform.inverse(at))[3] > 0 &&
                    point_in_contours(at,&contours));
                let threshold = object.style.stroke_width*scale/2.0+tolerance;
                let stroke_hit = object.style.stroke.is_some_and(|color| color[3]>0) &&
                    object.style.stroke_width>0.0 && contours.iter().any(|c|
                        path_distance_sq(at,&c.points,c.closed)<=threshold*threshold);
                if fill_hit || stroke_hit { return Some(object.id.clone()); }
                continue;
            }
            let (path, closed) = flattened_path_for_scale(&object.shape,
                object.transform.scale_x.max(object.transform.scale_y));
            let transformed: Vec<Point> = path.iter().map(|p| object.transform.apply(*p)).collect();
            let fill_hit = closed && object.style.fill.as_ref().is_some_and(|fill|
                sample_fill(fill, object.transform.inverse(at))[3] > 0) &&
                point_in_polygon(at, &transformed);
            let stroke_hit = object.style.stroke.is_some_and(|stroke| stroke[3] > 0) &&
                object.style.stroke_width > 0.0 &&
                path_distance_sq(at, &transformed, closed) <=
                    (object.style.stroke_width *
                        object.transform.scale_x.max(object.transform.scale_y) / 2.0 +
                        tolerance).powi(2);
            if fill_hit || stroke_hit { return Some(object.id.clone()); }
        }
    }
    None
}

fn point_in_polygon(point: Point, path: &[Point]) -> bool {
    let mut inside = false;
    for i in 0..path.len() {
        let a = path[i];
        let b = path[(i+1)%path.len()];
        if (a.y > point.y) != (b.y > point.y) &&
            point.x < (b.x - a.x) * (point.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
    }
    inside
}

fn point_in_contours(point: Point, contours: &[Contour]) -> bool {
    contours.iter().filter(|c| point_in_polygon(point,&c.points)).count()%2 != 0
}

fn path_distance_sq(point: Point, path: &[Point], closed: bool) -> f32 {
    let segment_count = if closed { path.len() } else { path.len()-1 };
    let mut min_dist = f32::INFINITY;
    for i in 0..segment_count {
        let a = path[i];
        let b = path[(i+1)%path.len()];
        let dx = b.x-a.x;
        let dy = b.y-a.y;
        let length_sq = dx*dx+dy*dy;
        let t = if length_sq > 0.000001 {
            ((point.x-a.x)*dx+(point.y-a.y)*dy) / length_sq
        } else { 0.0 }.clamp(0.0, 1.0);
        let px = point.x - a.x - t*dx;
        let py = point.y - a.y - t*dy;
        min_dist = min_dist.min(px*px+py*py);
    }
    min_dist
}

fn sample_fill(fill: &DrawFill, point: Point) -> [u8; 4] {
    match fill {
        DrawFill::Solid { color } => *color,
        DrawFill::LinearGradient { start, end, stops } => {
            let vx = end.x - start.x; let vy = end.y - start.y;
            let offset = ((point.x-start.x)*vx + (point.y-start.y)*vy) / (vx*vx+vy*vy);
            sample_stops(stops, offset)
        }
        DrawFill::RadialGradient { center, radius, stops } => {
            sample_stops(stops, (point.x-center.x).hypot(point.y-center.y)/radius)
        }
        DrawFill::EllipticalGradient { center, radius_x, radius_y, stops } => {
            sample_stops(stops, ((point.x-center.x)/radius_x).hypot((point.y-center.y)/radius_y))
        }
    }
}

fn sample_stops(stops: &[GradientStop], offset: f32) -> [u8; 4] {
    if offset <= stops[0].offset { return stops[0].color; }
    for pair in stops.windows(2) {
        if offset <= pair[1].offset {
            let t = ((offset-pair[0].offset) / (pair[1].offset-pair[0].offset)).clamp(0.0,1.0);
            let mut result = [0;4];
            for (i, value) in result.iter_mut().enumerate() {
                *value = (pair[0].color[i] as f32 * (1.0-t) +
                    pair[1].color[i] as f32 * t).round() as u8;
            }
            return result;
        }
    }
    stops.last().unwrap().color
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
    use crate::store::drawing::{DrawShape, DrawFill, DrawShadow, PathCommand, DrawingHistory, GradientStop, DrawStyle, Transform, Layer};
    use std::time::Instant;

    fn px(board:&Board,x:u32,y:u32)->[u8;4] {
        let offset=((y*WIDTH+x)*4) as usize;
        board.bytes()[offset..offset+4].try_into().unwrap()
    }

    fn rectangle_path(outer:(f32,f32,f32,f32), inner:Option<(f32,f32,f32,f32)>) -> DrawShape {
        fn contour(a:&mut Vec<PathCommand>,rect:(f32,f32,f32,f32)) {
            let (x0,y0,x1,y1)=rect;
            a.extend([PathCommand::MoveTo {to:Point::new(x0,y0)},
                PathCommand::LineTo {to:Point::new(x1,y0)},
                PathCommand::LineTo {to:Point::new(x1,y1)},
                PathCommand::LineTo {to:Point::new(x0,y1)},PathCommand::Close]);
        }
        let mut commands=Vec::new();
        contour(&mut commands,outer);
        if let Some(inner)=inner { contour(&mut commands,inner); }
        DrawShape::Path {commands}
    }

    #[test]
    fn compound_even_odd_hole_and_open_subpaths_pick_like_raster() {
        let mut d=DrawingData::default();
        let shape=rectangle_path((0.0,0.0,100.0,100.0),Some((35.0,35.0,65.0,65.0)));
        let id=d.add_object(shape,None).unwrap();
        d.update_object(&id,|o| {
            o.transform.translation=Point::new(80.0,70.0);
            o.style.stroke=None;
            o.style.fill=Some(DrawFill::Solid {color:[12,90,120,255]});
        }).unwrap();
        let board=Board::with_drawing(&[],&d);
        assert_eq!(px(&board,110,100),[12,90,120,255]);
        assert_eq!(px(&board,130,120),[255;4]);
        assert_eq!(pick_object(&d,Point::new(110.0,100.0),0.0),Some(id.clone()));
        assert_eq!(pick_object(&d,Point::new(130.0,120.0),0.0),None);
        // Distinct open contour has no closing stroke while its fill is still closed.
        d.update_object(&id,|o| {
            o.shape=DrawShape::Path {commands:vec![PathCommand::MoveTo {to:Point::new(0.0,0.0)},
                PathCommand::LineTo {to:Point::new(100.0,0.0)},
                PathCommand::LineTo {to:Point::new(100.0,100.0)}]};
            o.style.fill=None;
            o.style.stroke=Some([20,20,20,255]);
            o.style.stroke_width=2.0;
        }).unwrap();
        let open_board=Board::with_drawing(&[],&d);
        assert_eq!(px(&open_board,110,70),[20,20,20,255]);
        assert_eq!(px(&open_board,128,119),[255;4]);
        assert_eq!(pick_object(&d,Point::new(128.0,119.0),0.0),None);
    }

    #[test]
    fn clip_is_in_object_local_coordinates_with_even_odd_holes() {
        let mut d=DrawingData::default();
        let id=d.add_object(DrawShape::Rectangle {width:120.0,height:120.0},None).unwrap();
        d.update_object(&id,|o| {
            o.clip_shape=Some(rectangle_path((0.0,0.0,100.0,100.0),
                Some((30.0,30.0,70.0,70.0))));
            o.transform.translation=Point::new(215.0,145.0);
            o.style.stroke=None;
            o.style.fill=Some(DrawFill::Solid {color:[0,100,200,255]});
        }).unwrap();
        let b=Board::with_drawing(&[],&d);
        assert_eq!(px(&b,230,163),[0,100,200,255]);
        assert_eq!(px(&b,270,200),[255;4]);
        assert_eq!(px(&b,322,163),[255;4]);
        assert_eq!(pick_object(&d,Point::new(230.0,163.0),0.0),Some(id.clone()));
        assert_eq!(pick_object(&d,Point::new(270.0,200.0),0.0),None);
        assert_eq!(pick_object(&d,Point::new(322.0,163.0),0.0),None);
    }

    #[test]
    fn soft_shadow_is_behind_fill_and_transparent_shadow_is_noop() {
        let mut d=DrawingData::default();
        let id=d.add_object(rectangle_path((0.0,0.0,50.0,50.0),None),None).unwrap();
        d.update_object(&id,|o| {
            o.transform.translation=Point::new(300.0,150.0);
            o.style.stroke=None;
            o.style.fill=Some(DrawFill::Solid {color:[220,40,30,255]});
            o.style.shadow=Some(DrawShadow {offset:Point::new(12.0,9.0),
                blur_radius:7.0,color:[20,30,40,210]});
        }).unwrap();
        let b=Board::with_drawing(&[],&d);
        assert_eq!(px(&b,323,173),[220,40,30,255],"shape stays in front of shadow");
        let fringed=px(&b,365,194);
        assert!(fringed[0]<255 && fringed[0]>20,"shadow fringe must be soft: {fringed:?}");
        assert_eq!(px(&b,385,214),[255;4]);
        assert_eq!(pick_object(&d,Point::new(365.0,194.0),0.0),None,
            "shadow is decoration, not selectable geometry");
        d.update_object(&id,|o| o.style.shadow.as_mut().unwrap().color[3]=0).unwrap();
        let transparent=Board::with_drawing(&[],&d);
        assert_eq!(px(&transparent,323,173),[220,40,30,255]);
        assert_eq!(px(&transparent,365,194),[255;4]);
    }

    #[test]
    fn elliptical_radial_gradient_respects_separate_axes_and_path_cap() {
        let mut d=DrawingData::default();
        let id=d.add_object(rectangle_path((-100.0,-45.0,100.0,45.0),None),None).unwrap();
        d.update_object(&id,|o| {
            o.transform.translation=Point::new(460.0,235.0);
            o.style.stroke=None;
            o.style.fill=Some(DrawFill::EllipticalGradient {center:Point::default(),
                radius_x:90.0,radius_y:30.0,stops:vec![
                    GradientStop {offset:0.0,color:[255,0,0,255]},
                    GradientStop {offset:1.0,color:[0,0,255,255]}]});
        }).unwrap();
        let board=Board::with_drawing(&[],&d);
        let centre=px(&board,460,235);
        let axis_x=px(&board,505,235);
        let axis_y=px(&board,460,250);
        assert!(centre[0]>240 && centre[2]<15);
        assert!((axis_x[0] as i32 - axis_y[0] as i32).abs()<3);
        let mut cmds=vec![PathCommand::MoveTo {to:Point::new(5.0,5.0)}];
        for i in 1..256 {cmds.push(PathCommand::QuadTo {
            control:Point::new(5.0+i as f32*0.1,15.0),
            to:Point::new(5.0+i as f32*0.1,5.0)});}
        let bounded=path_contours(&cmds,128.0);
        assert!(bounded.iter().map(|c|c.points.len()).sum::<usize>()<=256*64+1);
    }

    #[test]
    fn compound_curve_and_blur_render_work_is_bounded() {
        let mut commands=vec![PathCommand::MoveTo {to:Point::new(50.0,120.0)}];
        for i in 0..80 {
            let x=50.0+i as f32*9.0;
            commands.push(PathCommand::CubicTo {control1:Point::new(x+2.0,105.0),
                control2:Point::new(x+6.0,185.0),to:Point::new(x+9.0,120.0)});
        }
        let mut d=DrawingData::default();
        let id=d.add_object(DrawShape::Path {commands},None).unwrap();
        d.update_object(&id,|o| {
            o.style.fill=None;
            o.style.stroke=Some([34,59,90,255]);
            o.style.stroke_width=3.0;
            o.style.shadow=Some(DrawShadow {offset:Point::new(4.0,4.0),
                blur_radius:12.0,color:[0,0,0,120]});
        }).unwrap();
        let start=Instant::now();
        let result=Board::with_drawing(&[],&d);
        assert_ne!(px(&result,50,120),[255;4]);
        // Catch quadratic-per-pixel regressions without enforcing tight timing
        // on slower CI machines. 80 curves are spread across a wide canvas.
        assert!(start.elapsed().as_secs_f32()<10.0,"compound path rendering regressed");
    }

    // Keep the pre-optimization rasterizer as an independent, byte-level
    // reference. This also makes benchmark numbers directly comparable when
    // changes to clipping, sampling or curves are proposed later.
    fn reference_draw_object(board: &mut Board, object: &DrawObject) {
        if object.validate().is_err() { return; }
        let (path, closed) = flattened_path(&object.shape);
        if path.len() < 2 { return; }
        let transformed: Vec<Point> = path.iter().map(|point| object.transform.apply(*point)).collect();
        let scaled_width = object.style.stroke_width *
            object.transform.scale_x.max(object.transform.scale_y);
        let pad = scaled_width / 2.0 + 1.5;
        let left = transformed.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
        let right = transformed.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max);
        let top = transformed.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
        let bottom = transformed.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max);
        if left - pad >= WIDTH as f32 || right + pad < 0.0 ||
            top - pad >= HEIGHT as f32 || bottom + pad < 0.0 { return; }
        let x0 = ((left - pad).floor() as i32).clamp(0, WIDTH as i32 - 1);
        let x1 = ((right + pad).ceil() as i32).clamp(0, WIDTH as i32 - 1);
        let y0 = ((top - pad).floor() as i32).clamp(0, HEIGHT as i32 - 1);
        let y1 = ((bottom + pad).ceil() as i32).clamp(0, HEIGHT as i32 - 1);
        let fill = if closed { object.style.fill.as_ref() } else { None };
        let stroke = object.style.stroke.filter(|_| object.style.stroke_width > 0.0);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let samples = [(-0.25,-0.25),(0.25,-0.25),(-0.25,0.25),(0.25,0.25)];
                let mut fill_hits = 0;
                let mut stroke_hits = 0;
                for (ox, oy) in samples {
                    let p = Point::new(x as f32 + ox, y as f32 + oy);
                    if fill.is_some() && point_in_polygon(p, &transformed) { fill_hits += 1; }
                    if stroke.is_some() && path_distance_sq(p, &transformed, closed) <=
                        scaled_width * scaled_width / 4.0 { stroke_hits += 1; }
                }
                if fill_hits == 0 && stroke_hits == 0 { continue; }
                let offset = ((y as u32 * WIDTH + x as u32) * 4) as usize;
                if let Some(fill) = fill {
                    if fill_hits > 0 {
                        let p = object.transform.inverse(Point::new(x as f32, y as f32));
                        let color = sample_fill(fill, p);
                        board.blend(offset, color, object.style.opacity * (fill_hits as f32 / 4.0));
                    }
                }
                if let Some(color) = stroke {
                    if stroke_hits > 0 {
                        board.blend(offset, color, object.style.opacity * (stroke_hits as f32 / 4.0));
                    }
                }
            }
        }
    }

    fn synthetic_vector_scene(count: usize) -> DrawingData {
        let layers = vec![Layer { id: "layer-bench".into(), name: "Benchmark".into(),
            visible: true, locked: false }];
        let objects = (0..count).map(|i| {
            let x = (i % 12) as f32 * 83.0 + 46.0;
            // Reuse on-screen positions for dense 100-512 shape scenes: objects
            // beyond the initial grid still exercise blending and rasterization.
            let y = ((i / 12) % 7) as f32 * 94.0 + 49.0;
            let shape = match i % 4 {
                0 => DrawShape::Ellipse { radius_x: 29.0, radius_y: 35.0 },
                1 => DrawShape::RoundedRectangle { width: 64.0, height: 48.0, radius: 14.0 },
                2 => DrawShape::CubicBezier { control1:Point::new(16.0, 51.0),
                    control2:Point::new(52.0, -30.0), to:Point::new(76.0, 21.0) },
                _ => DrawShape::Polygon { closed:true, points: vec![Point::new(-30.0, -32.0),
                    Point::new(30.0,-28.0),Point::new(39.0,8.0),Point::new(0.0,36.0),Point::new(-33.0,19.0)] },
            };
            let gradient = DrawFill::LinearGradient { start:Point::new(-29.0, -20.0),
                end:Point::new(40.0,33.0), stops:vec![
                    GradientStop {offset:0.0, color:[25,85,155,210]},
                    GradientStop {offset:1.0, color:[218,42,83,240]}] };
            DrawObject { id:format!("shape-{i}"),layer_id:"layer-bench".into(),shape,clip_shape:None,
                visible:true,group_id:None,
                style: DrawStyle { stroke:Some([16,33,61,230]),stroke_width:3.0,
                    fill: if i%4==2 {None} else {Some(gradient)}, opacity:0.9, shadow:None },
                transform: Transform { translation:Point::new(x,y), rotation_deg:(i%7) as f32 * 11.0,
                    ..Transform::default() } }
        }).collect();
        DrawingData { layers, objects }
    }

    /// Deterministic stress art: overlapping editable curves, clipped fills,
    /// shadows and gradients; no saved user notes or external fixtures.
    fn dense_compound_scene(count: usize, layer_count: usize) -> DrawingData {
        let mut drawing = synthetic_vector_scene(count);
        drawing.layers = (0..layer_count).map(|i| Layer {
            id: format!("layer-{i}"), name: format!("Layer {i}"),
            visible: true, locked: false,
        }).collect();
        for (i, object) in drawing.objects.iter_mut().enumerate() {
            object.layer_id = format!("layer-{}", (i*17)%layer_count);
            object.shape = DrawShape::Path { commands: vec![
                PathCommand::MoveTo {to: Point::new(-27.0,-16.0)},
                PathCommand::CubicTo {control1:Point::new(-8.0,-32.0),
                    control2:Point::new(32.0,-29.0),to:Point::new(34.0,-8.0)},
                PathCommand::CubicTo {control1:Point::new(43.0,12.0),
                    control2:Point::new(7.0,38.0),to:Point::new(-22.0,24.0)},
                PathCommand::CubicTo {control1:Point::new(-37.0,13.0),
                    control2:Point::new(-39.0,-12.0),to:Point::new(-27.0,-16.0)},
                PathCommand::Close,
            ] };
            if i%3 != 0 {
                object.clip_shape = Some(DrawShape::Ellipse {
                    radius_x:24.0 + (i%3) as f32, radius_y:26.0,
                });
            }
            if i%2 == 0 {
                object.style.shadow = Some(DrawShadow { offset:Point::new(4.0,-3.0),
                    blur_radius:5.0,color:[14,25,44,120] });
            }
        }
        drawing.validate().unwrap();
        drawing
    }

    fn timed_render_samples(scene: &DrawingData, n: usize) -> (f64, f64, String) {
        let mut times = Vec::with_capacity(n);
        let mut hash = String::new();
        for sample in 0..n {
            let begin = Instant::now();
            let board = std::hint::black_box(Board::with_drawing(&[], scene));
            times.push(begin.elapsed().as_secs_f64()*1000.0);
            if sample == 0 { hash = blake3::hash(board.bytes()).to_hex().to_string(); }
        }
        times.sort_by(f64::total_cmp);
        (times[n/2], times[((n*95).div_ceil(100)).saturating_sub(1)], hash)
    }

    #[test]
    #[ignore = "opt-in realistic synthetic dense vector performance benchmark"]
    fn dense_compound_repeated_preview_benchmark() {
        for count in [24, 100, 256, 512] {
            let scene = dense_compound_scene(count, 1);
            let (p50,p95,hash) = timed_render_samples(&scene, 5);
            println!("dense_scene count={count} layers=1 p50_ms={p50:.3} p95_ms={p95:.3} rgba_blake3={hash}");
        }
        let mut layered = dense_compound_scene(512, 64);
        let (p50,p95,hash) = timed_render_samples(&layered, 5);
        println!("dense_scene count=512 layers=64 p50_ms={p50:.3} p95_ms={p95:.3} rgba_blake3={hash}");
        for frame in 0..5 {
            layered.objects[511].transform.translation.x += 1.0;
            let begin = Instant::now();
            let board = std::hint::black_box(Board::with_drawing(&[], &layered));
            println!("dense_drag count=512 layers=64 frame={frame} elapsed_ms={:.3} rgba_blake3={}",
                begin.elapsed().as_secs_f64()*1000.0,
                blake3::hash(board.bytes()).to_hex());
        }
    }

    #[test]
    fn dense_path_clip_shadow_matches_original_rgba_golden() {
        let scene = dense_compound_scene(24, 1);
        assert_eq!(blake3::hash(Board::with_drawing(&[], &scene).bytes()).to_hex().as_str(),
            "8374ae178dd1fa6dd623b6e1380b2c2816d37ccb4fa2f71a0489734e4a1d1e49");
        let mut drag = dense_compound_scene(512,64);
        drag.objects[511].transform.translation.x += 1.0;
        assert_eq!(blake3::hash(Board::with_drawing(&[], &drag).bytes()).to_hex().as_str(),
            "c667fa74304ab7ace71c98ccb87c908696f4180927feef207995b9eaec25034d");
    }

    #[test]
    fn optimized_vector_pixels_match_reference_for_layered_agent_scene() {
        let scene = synthetic_vector_scene(56);
        scene.validate().unwrap();
        let mut reference = Board::new(&[]);
        for object in &scene.objects { reference_draw_object(&mut reference, object); }
        let optimized = Board::with_drawing(&[], &scene);
        assert_eq!(optimized.bytes(), reference.bytes(),
            "Vector raster optimization changed antialiasing, gradients or layer compositing");
    }

    #[test]
    fn optimized_vector_pixels_match_legacy_for_clipped_flipped_transparent_and_thin_shapes() {
        let mut scene = synthetic_vector_scene(12);
        for (i, object) in scene.objects.iter_mut().enumerate() {
            match i {
                0 => object.transform.translation = Point::new(-25.0, -10.0),
                1 => object.transform.translation = Point::new(WIDTH as f32-20.0, HEIGHT as f32-10.0),
                2 => {object.transform.flip_x = true; object.transform.flip_y = true;},
                3 => {object.style.stroke_width = 0.0; object.style.stroke = None;},
                4 => object.style.stroke_width = 0.25,
                5 => object.style.opacity = 0.0,
                6 => object.style.stroke = Some([255,10,0,0]),
                7 => object.style.fill = Some(DrawFill::Solid {color:[1,2,3,0]}),
                8 => object.style.fill = Some(DrawFill::RadialGradient {
                    center:Point::new(0.0,0.0),radius:45.0,
                    stops:vec![GradientStop{offset:0.0,color:[240,50,90,0]},
                        GradientStop{offset:1.0,color:[10,150,200,0]}]}),
                9 => object.transform.rotation_deg = 179.0,
                10 => {object.transform.scale_x = 0.75; object.transform.scale_y = 1.3;},
                11 => object.style.stroke_width = 0.001,
                _ => {}
            }
        }
        scene.validate().unwrap();
        let mut reference = Board::new(&[]);
        for object in &scene.objects { reference_draw_object(&mut reference, object); }
        assert_eq!(Board::with_drawing(&[], &scene).bytes(), reference.bytes());
    }

    #[test]
    fn cached_gradient_inverse_matches_original_transform_for_flips_and_rotations() {
        let fill = DrawFill::EllipticalGradient {
            center: Point::new(12.5, -8.0), radius_x: 62.0, radius_y: 35.0,
            stops: vec![GradientStop {offset:0.0,color:[12,42,210,91]},
                GradientStop {offset:0.41,color:[190,60,80,240]},
                GradientStop {offset:1.0,color:[250,250,10,255]}],
        };
        for rotation_deg in [0.0, 23.0, 90.0, 179.0, -315.0] {
            for (flip_x, flip_y) in [(false,false),(true,false),(false,true),(true,true)] {
                let transform = Transform {translation:Point::new(87.5,-31.0),
                    scale_x:0.77,scale_y:1.31,rotation_deg,flip_x,flip_y};
                let sampler = FillSampler::new(&transform);
                for (x,y) in [(0,0),(95,30),(131,240),(1048,699),(-23,741)] {
                    assert_eq!(sampler.sample(&fill,x,y),
                        sample_fill(&fill,transform.inverse(Point::new(x as f32,y as f32))),
                        "gradient changed at ({x},{y}) for {transform:?}");
                }
            }
        }
    }

    #[test]
    fn large_agent_curves_adapt_to_subpixel_geometry_without_changing_small_shapes() {
        let small = DrawShape::Ellipse {radius_x:40.0,radius_y:30.0};
        assert_eq!(flattened_path_for_scale(&small,1.0), flattened_path(&small));
        let circle = DrawShape::Circle {radius:1500.0};
        let (base, _) = flattened_path(&circle);
        let (refined, _) = flattened_path_for_scale(&circle, 1.0);
        assert!(refined.len() > base.len() && refined.len() <= 512);
        let maximum_chord_error = |path: &[Point]| {
            path.iter().enumerate().map(|(i,a)| {
                let b = path[(i+1) % path.len()];
                let x = (a.x + b.x) * 0.5;
                let y = (a.y + b.y) * 0.5;
                1500.0 - x.hypot(y)
            }).fold(0.0_f32, f32::max)
        };
        assert!(maximum_chord_error(&base) > 0.7);
        assert!(maximum_chord_error(&refined) < 0.3);
        let corner = DrawShape::RoundedRectangle { width:640.0, height:640.0, radius:300.0 };
        assert!(flattened_path_for_scale(&corner,1.0).0.len() > flattened_path(&corner).0.len());
        let curve = DrawShape::CubicBezier {control1:Point::new(1800.0,-500.0),
            control2:Point::new(-1200.0,2100.0),to:Point::new(500.0,800.0)};
        assert!(flattened_path_for_scale(&curve,1.0).0.len() > flattened_path(&curve).0.len());
    }

    #[test]
    #[ignore = "explicit opt-in synthetic render benchmark; no user files"]
    fn agent_vector_render_benchmark() {
        for count in [24, 100, 256, 512] {
            let legacy = synthetic_vector_scene(count);
            let mut mixed = legacy.clone();
            for (i, object) in mixed.objects.iter_mut().enumerate() {
                match i % 6 {
                    0 | 3 => object.shape = DrawShape::Path { commands: vec![
                        PathCommand::MoveTo { to: Point::new(-30.0,-20.0) },
                        PathCommand::CubicTo { control1:Point::new(-5.0,-60.0),
                            control2:Point::new(40.0,25.0),to:Point::new(30.0,30.0) },
                        PathCommand::LineTo {to:Point::new(-30.0,30.0)}, PathCommand::Close] },
                    1 => object.clip_shape = Some(DrawShape::Ellipse {radius_x:26.0,radius_y:23.0}),
                    2 => object.style.shadow = Some(DrawShadow { offset:Point::new(5.0,7.0),
                        blur_radius:4.0,color:[20,35,50,90] }),
                    _ => {}
                }
            }
            for (name, scene) in [("legacy", &legacy), ("mixed", &mixed)] {
                scene.validate().unwrap();
                let mut best = f64::MAX;
                let mut hash = String::new();
                for _ in 0..3 {
                    let start = Instant::now();
                    let board = std::hint::black_box(Board::with_drawing(&[], scene));
                    best = best.min(start.elapsed().as_secs_f64()*1000.0);
                    hash = blake3::hash(board.bytes()).to_hex().to_string();
                }
                println!("vector_render {name} {count}: best_of_3_ms={best:.3} rgba_blake3={hash}");
            }
        }
    }
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
    fn copy_pixels_keeps_frame_storage_and_exact_rgba() {
        let mut destination = Board::new(&[]);
        let backing = destination.bytes().as_ptr();
        let source = Board::new(&[Stroke {color:[10,60,180,220],width:9,
            eraser:false,points:vec![[10,10],[950,680]]}]);
        destination.copy_pixels_from(&source);
        assert_eq!(destination.bytes().as_ptr(),backing);
        assert_eq!(destination.bytes(),source.bytes());
    }
    #[test]
    fn colors_reject_bad_input() {
        assert_eq!(parse_color("#FF0080"), [255,0,128,255]);
        assert_eq!(parse_color("oops"), [36,41,45,255]);
    }
    #[test]
    fn legacy_partial_alpha_composites_over_opaque_board() {
        let board = Board::new(&[Stroke { color:[0,0,0,128],width:8,eraser:false,
            points:vec![[10,10]] }]);
        assert_eq!(&board.bytes()[((10*WIDTH+10)*4) as usize..][..4],
            &[127,127,127,255]);
    }
    #[test]
    fn editable_shapes_composite_over_legacy_strokes_and_preserve_alpha() {
        let legacy = Stroke { color:[0,0,0,255], width:10, eraser:false, points:vec![[50,50]] };
        let mut d = DrawingData::default();
        let id = d.add_object(DrawShape::Rectangle {width:80.0,height:80.0}, None).unwrap();
        d.move_object(&id, 40.0, 40.0).unwrap();
        d.update_object(&id, |o| {
            o.style.stroke = None;
            o.style.fill = Some(DrawFill::Solid {color:[255,0,0,128]});
        }).unwrap();
        let board = Board::with_drawing(&[legacy], &d);
        let center = &board.bytes()[((50*WIDTH+50)*4) as usize..][..4];
        assert_eq!(center, &[128,0,0,255]);
        let filled = &board.bytes()[((60*WIDTH+60)*4) as usize..][..4];
        assert_eq!(filled, &[255,127,127,255]);
        assert_eq!(&board.png_bytes().unwrap()[..8], &[137,80,78,71,13,10,26,10]);
        let mut hidden = d.clone();
        hidden.set_layer_visibility(&hidden.layers[0].id.clone(), false).unwrap();
        let hidden_board = Board::with_drawing(&[Stroke {color:[0,0,0,255],width:10,eraser:false,
            points:vec![[50,50]]}], &hidden);
        assert_eq!(&hidden_board.bytes()[((60*WIDTH+60)*4) as usize..][..4], &[255,255,255,255]);
    }
    #[test]
    fn rotated_gradient_ellipse_and_bezier_rasterize() {
        let mut d = DrawingData::default();
        let id = d.add_object(DrawShape::Ellipse { radius_x:40.0, radius_y:20.0 }, None).unwrap();
        d.move_object(&id, 100.0, 100.0).unwrap();
        d.rotate_object(&id, 90.0).unwrap();
        d.update_object(&id, |o| {
            o.style.stroke = None;
            o.style.fill = Some(DrawFill::LinearGradient {
                start:Point::new(-40.0,0.0), end:Point::new(40.0,0.0),
                stops:vec![GradientStop {offset:0.0,color:[0,0,0,255]},
                    GradientStop {offset:1.0,color:[255,0,0,255]}],
            });
        }).unwrap();
        let curve = d.add_object(DrawShape::CubicBezier { control1:Point::new(10.0,40.0),
            control2:Point::new(40.0,40.0),to:Point::new(50.0,0.0) }, None).unwrap();
        d.move_object(&curve, 180.0, 100.0).unwrap();
        let board = Board::with_drawing(&[], &d);
        let sample = &board.bytes()[((100*WIDTH+100)*4) as usize..][..4];
        assert!(sample[0] > 80 && sample[0] < 180);
        assert_ne!(&board.bytes()[((130*WIDTH+100)*4) as usize..][..4], &[255,255,255,255]);
        assert_ne!(&board.bytes()[((130*WIDTH+208)*4) as usize..][..4], &[255,255,255,255]);
    }
    #[test]
    fn radial_fill_and_layer_order_visibility_and_lock_are_respected() {
        let mut d = DrawingData::default();
        let background = d.add_layer("Background").unwrap();
        let foreground = d.add_layer("Foreground").unwrap();
        let circle = d.add_object(DrawShape::Ellipse {radius_x:40.0,radius_y:40.0}, Some(&background)).unwrap();
        d.move_object(&circle, 70.0,70.0).unwrap();
        d.update_object(&circle, |o| {
            o.style.stroke = None;
            o.style.fill = Some(DrawFill::RadialGradient {
                center:Point::default(), radius:40.0,
                stops:vec![GradientStop {offset:0.0,color:[250,0,0,255]},
                    GradientStop {offset:1.0,color:[0,0,250,255]}],
            });
        }).unwrap();
        let square = d.add_object(DrawShape::Rectangle {width:20.0,height:20.0},Some(&foreground)).unwrap();
        d.move_object(&square, 60.0,60.0).unwrap();
        d.update_object(&square, |o| {
            o.style.stroke = None;
            o.style.fill = Some(DrawFill::Solid {color:[0,200,0,255]});
        }).unwrap();
        let composite = Board::with_drawing(&[],&d);
        assert_eq!(&composite.bytes()[((70*WIDTH+70)*4) as usize..][..4], &[0,200,0,255]);
        d.set_layer_visibility(&foreground, false).unwrap();
        let underneath = Board::with_drawing(&[], &d);
        let pixel = &underneath.bytes()[((70*WIDTH+70)*4) as usize..][..4];
        assert!(pixel[0] > 200 && pixel[2] < 40);
        d.set_layer_visibility(&foreground, true).unwrap();
        d.move_layer(&background, 1).unwrap();
        let reordered = Board::with_drawing(&[], &d);
        let pixel = &reordered.bytes()[((70*WIDTH+70)*4) as usize..][..4];
        assert!(pixel[0] > 200 && pixel[1] < 40);
        d.set_layer_locked(&background, true).unwrap();
        assert!(d.move_object(&circle, 1.0,1.0).is_err());
        assert_eq!(pick_object(&d, Point::new(70.0,70.0), 0.0), Some(square));
        d.set_layer_visibility(&foreground, false).unwrap();
        assert_eq!(pick_object(&d, Point::new(70.0,70.0), 0.0), None);
    }

    #[test]
    fn cancelled_pointer_preview_restores_pixels_hit_targets_and_clean_redo() {
        let legacy = Stroke { color: [30,40,50,255], width: 12,
            eraser: false, points: vec![[20,25], [100,25]] };
        let mut drawing = DrawingData::default();
        let unchanged = Board::with_drawing(&[legacy.clone()], &drawing);
        let original_pixels = unchanged.bytes().to_vec();
        let mut history = DrawingHistory::default();
        let before = drawing.snapshot();
        history.record(&before);
        let id = drawing.add_object(DrawShape::Rectangle {width:2.0,height:2.0}, None).unwrap();
        drawing.move_object(&id, 60.0, 50.0).unwrap();
        drawing.update_object(&id, |object| {
            object.shape = DrawShape::Rectangle {width:90.0,height:60.0};
            object.style.fill = Some(DrawFill::Solid {color:[180,30,30,255]});
        }).unwrap();
        let preview = Board::with_drawing(&[legacy.clone()], &drawing);
        assert_ne!(preview.bytes(), original_pixels);
        assert_eq!(pick_object(&drawing, Point::new(70.0,60.0), 1.0),
            Some(id));

        // Losing pointer capture discards the shape rather than treating
        // cancellation as an Undo that can later revive an unfinished preview.
        assert!(history.cancel_last(&mut drawing));
        assert_eq!(Board::with_drawing(&[legacy], &drawing).bytes(), original_pixels);
        assert_eq!(pick_object(&drawing, Point::new(70.0,60.0), 1.0), None);
        assert!(!history.undo(&mut drawing));
        assert!(!history.redo(&mut drawing));
    }

    #[test]
    fn transparent_vector_fill_and_stroke_are_not_mouse_targets() {
        let mut drawing = DrawingData::default();
        let id = drawing.add_object(DrawShape::Rectangle {width:40.0,height:30.0}, None).unwrap();
        drawing.move_object(&id, 50.0, 50.0).unwrap();
        drawing.update_object(&id, |object| {
            object.style.stroke = Some([0,0,0,0]);
            object.style.fill = Some(DrawFill::Solid {color:[255,0,0,0]});
        }).unwrap();
        assert_eq!(pick_object(&drawing, Point::new(60.0,60.0), 2.0), None);
        assert_eq!(pick_object(&drawing, Point::new(50.0,60.0), 2.0), None);
        drawing.update_object(&id, |object|
            object.style.fill = Some(DrawFill::Solid {color:[255,0,0,100]})).unwrap();
        assert_eq!(pick_object(&drawing, Point::new(60.0,60.0), 2.0),
            Some(id.clone()));
        drawing.update_object(&id, |object| object.style.opacity = 0.0).unwrap();
        assert_eq!(pick_object(&drawing, Point::new(60.0,60.0), 2.0), None);
    }
}
