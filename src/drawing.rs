//! Persistent editable drawing objects. Legacy freehand strokes remain in
//! `Note::strokes`; the model here is an additional, ordered vector layer.
//! All mutations validate a complete candidate before replacing saved data.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_OBJECTS: usize = 2_000;
pub const MAX_LAYERS: usize = 64;
pub const MAX_POINTS: usize = 256;
const MAX_HISTORY: usize = 48;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn fresh_id(prefix: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{:x}-{sequence:x}", std::process::id())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Point { pub x: f32, pub y: f32 }
impl Point { pub fn new(x: f32, y: f32) -> Self { Self { x, y } } }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DrawShape {
    Rectangle { width: f32, height: f32 },
    RoundedRectangle { width: f32, height: f32, radius: f32 },
    Circle { radius: f32 },
    Ellipse { radius_x: f32, radius_y: f32 },
    Line { to: Point },
    Polygon { points: Vec<Point>, #[serde(default = "default_true")] closed: bool },
    CubicBezier { control1: Point, control2: Point, to: Point },
}

pub fn default_true() -> bool { true }
fn default_opacity() -> f32 { 1.0 }
fn default_scale() -> f32 { 1.0 }
fn default_stroke_width() -> f32 { 3.0 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GradientStop { pub offset: f32, pub color: [u8; 4] }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DrawFill {
    Solid { color: [u8; 4] },
    LinearGradient { start: Point, end: Point, stops: Vec<GradientStop> },
    RadialGradient { center: Point, radius: f32, stops: Vec<GradientStop> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrawStyle {
    #[serde(default = "default_stroke")]
    pub stroke: Option<[u8; 4]>,
    #[serde(default = "default_stroke_width")]
    pub stroke_width: f32,
    #[serde(default)]
    pub fill: Option<DrawFill>,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
}
fn default_stroke() -> Option<[u8; 4]> { Some([36, 41, 45, 255]) }
impl Default for DrawStyle {
    fn default() -> Self {
        Self { stroke: default_stroke(), stroke_width: default_stroke_width(),
            fill: None, opacity: 1.0 }
    }
}
impl DrawStyle {
    pub fn validate(&self) -> Result<(), String> {
        finite_in(self.stroke_width, 0.0, 256.0, "Stroke width")?;
        finite_in(self.opacity, 0.0, 1.0, "Object opacity")?;
        if let Some(fill) = &self.fill {
            match fill {
                DrawFill::Solid { .. } => {}
                DrawFill::LinearGradient { start, end, stops } => {
                    validate_point(*start)?; validate_point(*end)?;
                    if (start.x - end.x).hypot(start.y - end.y) < 0.0001 {
                        return Err("Linear gradient needs distinct endpoints".into());
                    }
                    validate_stops(stops)?;
                }
                DrawFill::RadialGradient { center, radius, stops } => {
                    validate_point(*center)?;
                    finite_in(*radius, 0.001, 8192.0, "Gradient radius")?;
                    validate_stops(stops)?;
                }
            }
        }
        Ok(())
    }
}
fn validate_stops(stops: &[GradientStop]) -> Result<(), String> {
    if stops.len() < 2 || stops.len() > 16 {
        return Err("Gradient needs 2–16 colour stops".into());
    }
    let mut previous = -1.0_f32;
    for stop in stops {
        finite_in(stop.offset, 0.0, 1.0, "Gradient stop")?;
        if stop.offset <= previous {
            return Err("Gradient stops must have increasing offsets".into());
        }
        previous = stop.offset;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    #[serde(default)]
    pub translation: Point,
    #[serde(default = "default_scale")]
    pub scale_x: f32,
    #[serde(default = "default_scale")]
    pub scale_y: f32,
    #[serde(default)]
    pub rotation_deg: f32,
    #[serde(default)]
    pub flip_x: bool,
    #[serde(default)]
    pub flip_y: bool,
}
impl Default for Transform {
    fn default() -> Self {
        Self { translation: Point::default(), scale_x: 1.0, scale_y: 1.0,
            rotation_deg: 0.0, flip_x: false, flip_y: false }
    }
}
impl Transform {
    pub fn validate(&self) -> Result<(), String> {
        validate_point(self.translation)?;
        finite_in(self.scale_x, 0.001, 128.0, "Horizontal scale")?;
        finite_in(self.scale_y, 0.001, 128.0, "Vertical scale")?;
        finite_in(self.rotation_deg, -360_000.0, 360_000.0, "Rotation")
    }
    pub fn apply(&self, p: Point) -> Point {
        let sx = self.scale_x * if self.flip_x { -1.0 } else { 1.0 };
        let sy = self.scale_y * if self.flip_y { -1.0 } else { 1.0 };
        let (sin, cos) = self.rotation_deg.to_radians().sin_cos();
        let x = p.x * sx;
        let y = p.y * sy;
        Point::new(self.translation.x + x * cos - y * sin,
                   self.translation.y + x * sin + y * cos)
    }
    pub fn inverse(&self, p: Point) -> Point {
        let (sin, cos) = self.rotation_deg.to_radians().sin_cos();
        let x = p.x - self.translation.x;
        let y = p.y - self.translation.y;
        Point::new((x * cos + y * sin) / (self.scale_x * if self.flip_x { -1.0 } else { 1.0 }),
                   (-x * sin + y * cos) / (self.scale_y * if self.flip_y { -1.0 } else { 1.0 }))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrawObject {
    pub id: String,
    pub layer_id: String,
    pub shape: DrawShape,
    #[serde(default)]
    pub style: DrawStyle,
    #[serde(default)]
    pub transform: Transform,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
}
impl DrawObject {
    pub fn validate(&self) -> Result<(), String> {
        validate_id(&self.id)?;
        validate_id(&self.layer_id)?;
        if let Some(group) = &self.group_id { validate_id(group)?; }
        self.style.validate()?;
        self.transform.validate()?;
        match &self.shape {
            DrawShape::Rectangle { width, height } |
            DrawShape::RoundedRectangle { width, height, .. } => {
                finite_in(*width, 0.001, 8192.0, "Shape width")?;
                finite_in(*height, 0.001, 8192.0, "Shape height")?;
                if let DrawShape::RoundedRectangle { radius, .. } = &self.shape {
                    finite_in(*radius, 0.0, width.min(*height) / 2.0, "Corner radius")?;
                }
            }
            DrawShape::Circle { radius } => {
                finite_in(*radius, 0.001, 8192.0, "Circle radius")?;
            }
            DrawShape::Ellipse { radius_x, radius_y } => {
                finite_in(*radius_x, 0.001, 8192.0, "Ellipse radius")?;
                finite_in(*radius_y, 0.001, 8192.0, "Ellipse radius")?;
            }
            DrawShape::Line { to } => validate_point(*to)?,
            DrawShape::Polygon { points, .. } => {
                if points.len() < 2 || points.len() > MAX_POINTS {
                    return Err(format!("Polygon needs 2–{MAX_POINTS} points"));
                }
                for p in points { validate_point(*p)?; }
            }
            DrawShape::CubicBezier { control1, control2, to } => {
                validate_point(*control1)?; validate_point(*control2)?; validate_point(*to)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layer {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default)]
    pub locked: bool,
}
impl Layer {
    pub fn validate(&self) -> Result<(), String> {
        validate_id(&self.id)?;
        if self.name.trim().is_empty() || self.name.chars().count() > 80 {
            return Err("Layer name must contain 1–80 characters".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DrawingData {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<Layer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<DrawObject>,
}
pub type DrawingSnapshot = DrawingData;
impl DrawingData {
    pub fn is_empty(&self) -> bool { self.layers.is_empty() && self.objects.is_empty() }
    pub fn validate(&self) -> Result<(), String> {
        if self.layers.len() > MAX_LAYERS { return Err("Too many drawing layers".into()); }
        if self.objects.len() > MAX_OBJECTS { return Err("Too many drawing objects".into()); }
        let mut layer_ids = std::collections::HashSet::new();
        for layer in &self.layers {
            layer.validate()?;
            if !layer_ids.insert(&layer.id) { return Err("Duplicate drawing layer ID".into()); }
        }
        let mut object_ids = std::collections::HashSet::new();
        for object in &self.objects {
            object.validate()?;
            if !layer_ids.contains(&object.layer_id) { return Err("Object references missing drawing layer".into()); }
            if !object_ids.insert(&object.id) { return Err("Duplicate drawing object ID".into()); }
        }
        Ok(())
    }
    fn edit<T>(&mut self, f: impl FnOnce(&mut DrawingData) -> Result<T, String>) -> Result<T, String> {
        let mut candidate = self.clone();
        let result = f(&mut candidate)?;
        candidate.validate()?;
        *self = candidate;
        Ok(result)
    }
    pub fn snapshot(&self) -> DrawingSnapshot { self.clone() }
    pub fn restore_snapshot(&mut self, snap: &DrawingSnapshot) -> Result<(), String> {
        snap.validate()?;
        *self = snap.clone();
        Ok(())
    }
    pub fn add_layer(&mut self, name: &str) -> Result<String, String> {
        let id = fresh_id("layer");
        let name = name.to_string();
        self.edit(|d| {
            d.layers.push(Layer { id: id.clone(), name, visible: true, locked: false });
            Ok(id)
        })
    }
    pub fn move_layer(&mut self, id: &str, position: usize) -> Result<(), String> {
        self.edit(|d| {
            let old = d.layers.iter().position(|l| l.id == id).ok_or("Drawing layer not found")?;
            if position >= d.layers.len() { return Err("Drawing layer position out of range".into()); }
            let layer = d.layers.remove(old);
            d.layers.insert(position, layer);
            Ok(())
        })
    }
    pub fn set_layer_visibility(&mut self, id: &str, visible: bool) -> Result<(), String> {
        self.edit(|d| {
            d.layers.iter_mut().find(|l| l.id == id).ok_or("Drawing layer not found")?.visible = visible;
            Ok(())
        })
    }
    pub fn set_layer_locked(&mut self, id: &str, locked: bool) -> Result<(), String> {
        self.edit(|d| {
            d.layers.iter_mut().find(|l| l.id == id).ok_or("Drawing layer not found")?.locked = locked;
            Ok(())
        })
    }
    pub fn rename_layer(&mut self, id: &str, name: &str) -> Result<(), String> {
        self.edit(|d| {
            d.layers.iter_mut().find(|l| l.id == id).ok_or("Drawing layer not found")?.name = name.to_string();
            Ok(())
        })
    }
    pub fn remove_layer(&mut self, id: &str) -> Result<(), String> {
        self.edit(|d| {
            let index = d.layers.iter().position(|l| l.id == id).ok_or("Drawing layer not found")?;
            if d.layers[index].locked { return Err("Drawing layer is locked".into()); }
            if d.objects.iter().any(|object| object.layer_id == id) {
                return Err("Move or remove objects before deleting their layer".into());
            }
            d.layers.remove(index);
            Ok(())
        })
    }
    pub fn add_object(&mut self, shape: DrawShape, layer_id: Option<&str>) -> Result<String, String> {
        let id = fresh_id("obj");
        self.edit(|d| {
            let layer_id = match layer_id {
                Some(id) => id.to_string(),
                None if d.layers.is_empty() => {
                    let id = fresh_id("layer");
                    d.layers.push(Layer { id: id.clone(), name: "Layer 1".into(), visible: true, locked: false });
                    id
                }
                None => d.layers.last().unwrap().id.clone(),
            };
            let layer = d.layers.iter().find(|l| l.id == layer_id).ok_or("Drawing layer not found")?;
            if layer.locked { return Err("Drawing layer is locked".into()); }
            d.objects.push(DrawObject { id: id.clone(), layer_id, shape,
                style: DrawStyle::default(), transform: Transform::default(), visible: true, group_id: None });
            Ok(id)
        })
    }
    pub fn object(&self, id: &str) -> Option<&DrawObject> { self.objects.iter().find(|o| o.id == id) }
    pub fn update_object(&mut self, id: &str, change: impl FnOnce(&mut DrawObject)) -> Result<(), String> {
        self.edit(|d| {
            let idx = d.objects.iter().position(|o| o.id == id).ok_or("Drawing object not found")?;
            if d.layers.iter().any(|l| l.id == d.objects[idx].layer_id && l.locked) {
                return Err("Drawing layer is locked".into());
            }
            let original_id = d.objects[idx].id.clone();
            change(&mut d.objects[idx]);
            if d.objects[idx].id != original_id {
                return Err("Object ID cannot be changed".into());
            }
            if d.layers.iter().any(|l| l.id == d.objects[idx].layer_id && l.locked) {
                return Err("Target drawing layer is locked".into());
            }
            Ok(())
        })
    }
    pub fn move_object(&mut self, id: &str, dx: f32, dy: f32) -> Result<(), String> {
        self.update_object(id, |o| {
            o.transform.translation.x += dx;
            o.transform.translation.y += dy;
        })
    }
    pub fn resize_object(&mut self, id: &str, sx: f32, sy: f32) -> Result<(), String> {
        self.update_object(id, |o| {
            o.transform.scale_x *= sx;
            o.transform.scale_y *= sy;
        })
    }
    pub fn rotate_object(&mut self, id: &str, degrees: f32) -> Result<(), String> {
        self.update_object(id, |o| o.transform.rotation_deg += degrees)
    }
    pub fn flip_object(&mut self, id: &str, horizontal: bool) -> Result<(), String> {
        self.update_object(id, |o| {
            if horizontal { o.transform.flip_x = !o.transform.flip_x; }
            else { o.transform.flip_y = !o.transform.flip_y; }
        })
    }
    pub fn remove_object(&mut self, id: &str) -> Result<(), String> {
        self.edit(|d| {
            let idx = d.objects.iter().position(|o| o.id == id).ok_or("Drawing object not found")?;
            if d.layers.iter().any(|l| l.id == d.objects[idx].layer_id && l.locked) {
                return Err("Drawing layer is locked".into());
            }
            d.objects.remove(idx);
            Ok(())
        })
    }
    pub fn reorder_object(&mut self, id: &str, new_index: usize) -> Result<(), String> {
        self.edit(|d| {
            let idx = d.objects.iter().position(|o| o.id == id).ok_or("Drawing object not found")?;
            if new_index >= d.objects.len() { return Err("Object order out of range".into()); }
            if d.layers.iter().any(|l| l.id == d.objects[idx].layer_id && l.locked) {
                return Err("Drawing layer is locked".into());
            }
            let obj = d.objects.remove(idx);
            d.objects.insert(new_index, obj);
            Ok(())
        })
    }
    pub fn group_objects(&mut self, ids: &[String]) -> Result<String, String> {
        if ids.len() < 2 || ids.len() > MAX_OBJECTS { return Err("Select 2–2000 objects to group".into()); }
        let group_id = fresh_id("group");
        self.edit(|d| {
            let unique: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
            if unique.len() != ids.len() { return Err("Duplicate group selection".into()); }
            for id in ids {
                let index = d.objects.iter().position(|o| &o.id == id).ok_or("Grouping selection contains missing object")?;
                if d.layers.iter().any(|l| l.id == d.objects[index].layer_id && l.locked) {
                    return Err("Drawing layer is locked".into());
                }
                d.objects[index].group_id = Some(group_id.clone());
            }
            Ok(group_id)
        })
    }
    pub fn ungroup(&mut self, group_id: &str) -> Result<(), String> {
        self.edit(|d| {
            let selected: Vec<usize> = d.objects.iter().enumerate()
                .filter_map(|(i, o)| (o.group_id.as_deref() == Some(group_id)).then_some(i)).collect();
            if selected.is_empty() { return Err("Drawing group not found".into()); }
            for index in selected {
                if d.layers.iter().any(|l| l.id == d.objects[index].layer_id && l.locked) {
                    return Err("Drawing layer is locked".into());
                }
                d.objects[index].group_id = None;
            }
            Ok(())
        })
    }

    fn edit_group(&mut self, group_id: &str,
        change: impl FnOnce(&mut DrawingData, &[usize], Point)) -> Result<(), String> {
        self.edit(|d| {
            let selected: Vec<usize> = d.objects.iter().enumerate()
                .filter_map(|(i, o)| (o.group_id.as_deref() == Some(group_id)).then_some(i))
                .collect();
            if selected.is_empty() { return Err("Drawing group not found".into()); }
            if selected.iter().any(|&i| d.layers.iter().any(|l|
                l.id == d.objects[i].layer_id && l.locked)) {
                return Err("Drawing group contains an object on a locked layer".into());
            }
            let (x, y) = selected.iter().fold((0.0_f32, 0.0_f32), |(x, y), &i| {
                (x + d.objects[i].transform.translation.x, y + d.objects[i].transform.translation.y)
            });
            let center = Point::new(x / selected.len() as f32, y / selected.len() as f32);
            change(d, &selected, center);
            Ok(())
        })
    }

    pub fn move_group(&mut self, group_id: &str, dx: f32, dy: f32) -> Result<(), String> {
        self.edit_group(group_id, |d, selected, _| for &i in selected {
            d.objects[i].transform.translation.x += dx;
            d.objects[i].transform.translation.y += dy;
        })
    }

    pub fn rotate_group(&mut self, group_id: &str, degrees: f32) -> Result<(), String> {
        self.edit_group(group_id, |d, selected, pivot| {
            let (sin,cos) = degrees.to_radians().sin_cos();
            for &i in selected {
                let t = &mut d.objects[i].transform;
                let x = t.translation.x - pivot.x;
                let y = t.translation.y - pivot.y;
                t.translation.x = pivot.x + x*cos - y*sin;
                t.translation.y = pivot.y + x*sin + y*cos;
                t.rotation_deg += degrees;
            }
        })
    }

    pub fn resize_group(&mut self, group_id: &str, sx: f32, sy: f32) -> Result<(), String> {
        self.edit_group(group_id, |d, selected, pivot| for &i in selected {
            let t = &mut d.objects[i].transform;
            t.translation.x = pivot.x + (t.translation.x - pivot.x)*sx;
            t.translation.y = pivot.y + (t.translation.y - pivot.y)*sy;
            t.scale_x *= sx;
            t.scale_y *= sy;
        })
    }

    pub fn flip_group(&mut self, group_id: &str, horizontal: bool) -> Result<(), String> {
        self.edit_group(group_id, |d, selected, pivot| for &i in selected {
            let t = &mut d.objects[i].transform;
            if horizontal {
                t.translation.x = 2.0 * pivot.x - t.translation.x;
                t.flip_x = !t.flip_x;
            } else {
                t.translation.y = 2.0 * pivot.y - t.translation.y;
                t.flip_y = !t.flip_y;
            }
            t.rotation_deg = -t.rotation_deg;
        })
    }
}

/// Bounded, session-only history. Deliberately excluded from workspace JSON.
#[derive(Debug, Default)]
pub struct DrawingHistory {
    undo: Vec<DrawingSnapshot>,
    redo: Vec<DrawingSnapshot>,
    // A cancelled pointer gesture must reinstate the redo branch it replaced
    // when the first edit was recorded. Ordinary undo creates a *new* redo
    // entry instead and therefore cannot safely be used for cancellation.
    redo_before_last_record: Option<Vec<DrawingSnapshot>>,
}
impl DrawingHistory {
    /// Legacy API for clients that apply a validated edit before recording.
    /// Each call registers exactly one gesture. Do not deduplicate identical
    /// snapshots: the GUI may have registered the corresponding undo marker.
    /// Prefer `apply_edit` for operations that can fail or be unchanged.
    pub fn record(&mut self, before: &DrawingData) {
        self.undo.push(before.snapshot());
        if self.undo.len() > MAX_HISTORY { self.undo.remove(0); }
        self.redo_before_last_record = Some(std::mem::take(&mut self.redo));
    }
    /// Record only an actual, valid change. Failed edits and unchanged pointer
    /// movement must leave both undo and redo stacks untouched.
    pub fn record_change(&mut self, before: &DrawingData, after: &DrawingData) -> Result<bool, String> {
        before.validate()?;
        after.validate()?;
        if before == after { return Ok(false); }
        self.record(before);
        Ok(true)
    }
    /// Apply an edit atomically and register one undo step only if it changed
    /// the drawing. A rejected edit cannot mutate the drawing or its history.
    /// The returned boolean is suitable for tracking a pointer gesture.
    pub fn apply_edit<T>(&mut self, current: &mut DrawingData,
        change: impl FnOnce(&mut DrawingData) -> Result<T, String>) -> Result<(T, bool), String> {
        current.validate()?;
        let mut candidate = current.clone();
        let result = change(&mut candidate)?;
        let changed = self.record_change(current, &candidate)?;
        if changed { *current = candidate; }
        Ok((result, changed))
    }
    /// Roll back a pending gesture without creating a redo of its discarded
    /// intermediate state. If the gesture started after Undo, restore that
    /// earlier redo branch. Call only for the most recently recorded gesture.
    pub fn cancel_last(&mut self, current: &mut DrawingData) -> bool {
        let Some(previous_redo) = self.redo_before_last_record.take() else { return false; };
        let Some(before) = self.undo.pop() else {
            self.redo = previous_redo;
            return false;
        };
        *current = before;
        self.redo = previous_redo;
        true
    }
    pub fn undo(&mut self, current: &mut DrawingData) -> bool {
        self.redo_before_last_record = None;
        // The legacy record API can register an unchanged gesture. Skip those
        // snapshots instead of making Undo appear to do nothing.
        while let Some(previous) = self.undo.pop() {
            if previous == *current { continue; }
            self.redo.push(current.snapshot());
            *current = previous;
            return true;
        }
        false
    }
    pub fn redo(&mut self, current: &mut DrawingData) -> bool {
        self.redo_before_last_record = None;
        while let Some(next) = self.redo.pop() {
            if next == *current { continue; }
            self.undo.push(current.snapshot());
            if self.undo.len() > MAX_HISTORY { self.undo.remove(0); }
            *current = next;
            return true;
        }
        false
    }
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.redo_before_last_record = None;
    }
}

fn finite_in(value: f32, lo: f32, hi: f32, name: &str) -> Result<(), String> {
    if !value.is_finite() || value < lo || value > hi {
        return Err(format!("{name} must be finite and between {lo} and {hi}"));
    }
    Ok(())
}
fn validate_point(point: Point) -> Result<(), String> {
    finite_in(point.x, -16_384.0, 16_384.0, "X coordinate")?;
    finite_in(point.y, -16_384.0, 16_384.0, "Y coordinate")
}
fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("Drawing ID must contain 1–128 safe ASCII characters".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_validate_atomically_and_respect_locks() {
        let mut drawing = DrawingData::default();
        let id = drawing.add_object(DrawShape::Rectangle { width: 50.0, height: 40.0 }, None).unwrap();
        let layer = drawing.objects[0].layer_id.clone();
        let before = drawing.snapshot();
        assert!(drawing.resize_object(&id, f32::NAN, 1.0).is_err());
        assert_eq!(drawing, before);
        drawing.set_layer_locked(&layer, true).unwrap();
        assert!(drawing.move_object(&id, 5.0, 0.0).is_err());
        assert_eq!(drawing.objects[0].transform.translation, Point::default());
        drawing.set_layer_locked(&layer, false).unwrap();
        drawing.move_object(&id, 5.0, 9.0).unwrap();
        assert_eq!(drawing.objects[0].transform.translation, Point::new(5.0, 9.0));
        assert!(drawing.remove_layer(&layer).is_err());
    }
    #[test]
    fn shapes_layers_groups_history_and_json_roundtrip() {
        let mut d = DrawingData::default();
        let l = d.add_layer("Foreground").unwrap();
        let a = d.add_object(DrawShape::Ellipse { radius_x: 10.0, radius_y: 12.0 }, Some(&l)).unwrap();
        let b = d.add_object(DrawShape::CubicBezier { control1: Point::new(4.0, 2.0),
            control2: Point::new(6.0, 4.0), to: Point::new(12.0, 0.0) }, Some(&l)).unwrap();
        let group = d.group_objects(&[a.clone(), b]).unwrap();
        assert_eq!(d.object(&a).unwrap().group_id.as_deref(), Some(group.as_str()));
        let first_origin = d.object(&a).unwrap().transform.translation;
        d.move_group(&group, 10.0, 20.0).unwrap();
        assert_eq!(d.object(&a).unwrap().transform.translation,
            Point::new(first_origin.x + 10.0, first_origin.y + 20.0));
        d.rotate_group(&group, 90.0).unwrap();
        d.resize_group(&group, 1.5, 1.5).unwrap();
        d.flip_group(&group, true).unwrap();
        d.validate().unwrap();
        let mut history = DrawingHistory::default();
        history.record(&d);
        d.rotate_object(&a, 35.0).unwrap();
        let encoded = serde_json::to_vec(&d).unwrap();
        let decoded: DrawingData = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, d);
        assert!(history.undo(&mut d));
        assert_eq!(d.object(&a).unwrap().transform.rotation_deg, -90.0);
        assert!(history.redo(&mut d));
        assert_eq!(d, decoded);
        d.ungroup(&group).unwrap();
    }
    #[test]
    fn invalid_gradient_and_duplicate_ids_are_rejected() {
        let mut d = DrawingData::default();
        d.add_object(DrawShape::Line { to: Point::new(4.0, 5.0) }, None).unwrap();
        let mut bad = d.clone();
        bad.objects.push(bad.objects[0].clone());
        assert!(bad.validate().is_err());
        assert!(d.update_object(&d.objects[0].id.clone(), |o| o.style.fill = Some(DrawFill::LinearGradient {
            start: Point::default(), end: Point::new(1.0, 0.0),
            stops: vec![GradientStop { offset: 0.9, color: [0,0,0,255] },
                GradientStop { offset: 0.2, color: [255,255,255,255] }],
        })).is_err());
    }

    #[test]
    fn no_op_and_failed_transactions_keep_drawing_and_redo_intact() {
        let mut drawing = DrawingData::default();
        let id = drawing.add_object(DrawShape::Rectangle { width: 25.0, height: 15.0 }, None).unwrap();
        let mut history = DrawingHistory::default();
        let original = drawing.clone();
        assert_eq!(history.apply_edit(&mut drawing, |d| d.move_object(&id, 10.0, 0.0)),
            Ok(((), true)));
        let moved = drawing.clone();
        assert!(history.undo(&mut drawing));
        assert_eq!(drawing, original);

        // Pointer moved zero distance. No new edit, and Redo must still
        // reapply the earlier committed move.
        assert_eq!(history.apply_edit(&mut drawing, |d| d.move_object(&id, 0.0, 0.0)),
            Ok(((), false)));
        let failed = history.apply_edit(&mut drawing, |d| d.move_object(&id, f32::NAN, 0.0));
        assert!(failed.is_err());
        assert_eq!(drawing, original);
        let error = history.apply_edit(&mut drawing, |d| d.remove_object("missing-object"));
        assert!(error.is_err());
        assert_eq!(drawing, original);
        assert!(!history.undo(&mut drawing));
        assert!(history.redo(&mut drawing));
        assert_eq!(drawing, moved);
        assert!(!history.redo(&mut drawing));
    }

    #[test]
    fn cancelled_gesture_rolls_back_without_redo_and_restores_previous_redo_branch() {
        let mut drawing = DrawingData::default();
        let id = drawing.add_object(DrawShape::Circle { radius: 8.0 }, None).unwrap();
        let original = drawing.clone();
        let mut history = DrawingHistory::default();
        assert!(history.apply_edit(&mut drawing, |d| d.move_object(&id, 5.0, 0.0)).unwrap().1);
        let committed = drawing.clone();
        assert!(history.undo(&mut drawing));
        assert_eq!(drawing, original);

        // A new pointer gesture begins, moves twice, then loses capture.
        history.record(&drawing);
        drawing.move_object(&id, 20.0, 10.0).unwrap();
        drawing.move_object(&id, 3.0, 4.0).unwrap();
        assert!(history.cancel_last(&mut drawing));
        assert_eq!(drawing, original);
        assert!(!history.cancel_last(&mut drawing));
        assert!(!history.undo(&mut drawing));
        assert!(history.redo(&mut drawing));
        assert_eq!(drawing, committed);
        assert!(!history.redo(&mut drawing));
    }

    #[test]
    fn cancelled_new_shape_cannot_reappear_in_redo_and_legacy_noops_are_skipped() {
        let mut drawing = DrawingData::default();
        let mut history = DrawingHistory::default();
        let before = drawing.clone();
        history.record(&before);
        let id = drawing.add_object(DrawShape::Rectangle { width: 1.0, height: 1.0 }, None).unwrap();
        drawing.move_object(&id, 70.0, 90.0).unwrap();
        assert!(history.cancel_last(&mut drawing));
        assert!(drawing.is_empty());
        assert!(!history.redo(&mut drawing));
        assert!(!history.undo(&mut drawing));

        // Legacy clients may record two unchanged events. Undo skips both
        // and reverses the actual preceding edit in one user action.
        let id = drawing.add_object(DrawShape::Ellipse { radius_x: 4.0, radius_y: 5.0 }, None).unwrap();
        let original = drawing.clone();
        history.record(&original);
        drawing.move_object(&id, 10.0, 0.0).unwrap();
        history.record(&drawing);
        history.record(&drawing);
        assert!(history.undo(&mut drawing));
        assert_eq!(drawing, original);
        assert!(history.redo(&mut drawing));
        assert_eq!(drawing.object(&id).unwrap().transform.translation.x, 10.0);
    }

    #[test]
    fn rollback_clears_pending_history_and_rejected_snapshot_is_atomic() {
        let mut drawing = DrawingData::default();
        drawing.add_object(DrawShape::Line { to: Point::new(1.0, 2.0) }, None).unwrap();
        let mut history = DrawingHistory::default();
        let old = drawing.clone();
        let mut invalid = drawing.clone();
        invalid.objects[0].layer_id = "missing".into();
        assert!(history.record_change(&old, &invalid).is_err());
        assert_eq!(drawing, old);
        assert!(!history.undo(&mut drawing));
        assert_eq!(history.record_change(&old, &old), Ok(false));
        assert!(!history.cancel_last(&mut drawing));

        assert!(history.apply_edit(&mut drawing, |d| d.rotate_object(
            &d.objects[0].id.clone(), 45.0)).unwrap().1);
        history.clear();
        assert!(!history.cancel_last(&mut drawing));
        assert!(!history.undo(&mut drawing));
        assert!(!history.redo(&mut drawing));
    }
}
