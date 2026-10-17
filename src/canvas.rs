use crate::store::{Stroke, Note};
use crate::store::drawing::{DrawingData, DrawObject, DrawShape, DrawFill, Point, GradientStop};
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
            for x in x0..=x1 {
                if fill.is_none() && row_segments.is_empty() { break; }
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
                        let p = object.transform.inverse(Point::new(x as f32, y as f32));
                        let color = sample_fill(fill, p);
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
        DrawFill::LinearGradient { stops, .. } | DrawFill::RadialGradient { stops, .. } =>
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
    use crate::store::drawing::{DrawShape, DrawFill, DrawingHistory, GradientStop, DrawStyle, Transform, Layer};
    use std::time::Instant;

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
            let y = (i / 12) as f32 * 94.0 + 49.0;
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
            DrawObject { id:format!("shape-{i}"),layer_id:"layer-bench".into(),shape,
                visible:true,group_id:None,
                style: DrawStyle { stroke:Some([16,33,61,230]),stroke_width:3.0,
                    fill: if i%4==2 {None} else {Some(gradient)}, opacity:0.9 },
                transform: Transform { translation:Point::new(x,y), rotation_deg:(i%7) as f32 * 11.0,
                    ..Transform::default() } }
        }).collect();
        DrawingData { layers, objects }
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
        for count in [32, 96] {
            let scene = synthetic_vector_scene(count);
            let mut reference = Board::new(&[]);
            let begin = Instant::now();
            for object in &scene.objects { reference_draw_object(&mut reference, object); }
            let baseline = begin.elapsed();
            let begin = Instant::now();
            let optimized = Board::with_drawing(&[], &scene);
            let after = begin.elapsed();
            assert_eq!(reference.bytes(), optimized.bytes());
            println!("agent_vector_render {count} objects: baseline_ms={:.3} optimized_ms={:.3} speedup={:.2}x",
                baseline.as_secs_f64()*1000.0, after.as_secs_f64()*1000.0,
                baseline.as_secs_f64()/after.as_secs_f64());
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
