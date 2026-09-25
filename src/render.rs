//! Baseline-aware stroke layout and tiny-skia rasterization.
use crate::parser::Node;
use serde::Deserialize;
use std::{collections::HashMap, fmt, path::Path};
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Stroke, Transform};

#[derive(Debug, Deserialize)]
pub struct StrokeFile {
    schema: String,
    version: u32,
    #[serde(default, rename = "inkWidth")]
    ink_width: Option<f32>,
    glyphs: Vec<Glyph>,
}
#[derive(Debug, Deserialize)]
struct Glyph {
    key: String,
    status: String,
    bbox: Option<Bounds>,
    #[serde(default)]
    strokes: Vec<Vec<Point>>,
    #[serde(default)]
    variants: Vec<Variant>,
}
#[derive(Debug, Deserialize)]
struct Variant {
    #[serde(default)]
    baseline: f32,
    bbox: Option<Bounds>,
    strokes: Vec<Vec<Point>>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bounds {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}
#[derive(Debug, Deserialize)]
struct Point {
    x: f32,
    y: f32,
    p: Option<f32>,
}

#[derive(Debug)]
pub struct RenderError(pub String);
impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for RenderError {}
type Result<T> = std::result::Result<T, RenderError>;

pub struct Handwriting {
    glyphs: HashMap<String, Glyph>,
    ink: InkWeight,
}
impl Handwriting {
    pub fn load(path: &Path) -> Result<Self> {
        let size = std::fs::metadata(path)
            .map_err(|e| RenderError(format!("{}: {e}", path.display())))?
            .len();
        if size > 50_000_000 {
            return Err(RenderError("stroke file exceeds 50 MB".into()));
        }
        let contents = std::fs::read_to_string(path)
            .map_err(|e| RenderError(format!("{}: {e}", path.display())))?;
        let file: StrokeFile = serde_json::from_str(&contents)
            .map_err(|e| RenderError(format!("stroke JSON: {e}")))?;
        if file.schema != "aspectwrite.handwriting" || ![1, 2].contains(&file.version) {
            return Err(RenderError(
                "expected aspectwrite.handwriting stroke file version 1 or 2".into(),
            ));
        }
        let mut glyphs = HashMap::new();
        let mut pressures = Vec::new();
        for glyph in &file.glyphs {
            for stroke in glyph.strokes.iter().chain(
                glyph
                    .variants
                    .iter()
                    .flat_map(|variant| variant.strokes.iter()),
            ) {
                pressures.extend(stroke.iter().filter_map(|point| point.p));
            }
        }
        if let Some(ink_width) = file.ink_width
            && (!ink_width.is_finite() || ink_width <= 0.0)
        {
            return Err(RenderError(format!(
                "inkWidth must be a positive number of pixels, got {ink_width}"
            )));
        }
        let ink = InkWeight::new(&pressures, file.ink_width.unwrap_or(INK_WIDTH));
        for glyph in file.glyphs {
            if glyphs.insert(glyph.key.clone(), glyph).is_some() {
                return Err(RenderError("duplicate glyph key".into()));
            }
        }
        Ok(Self { glyphs, ink })
    }
    fn lookup(&self, key: &str) -> Result<&Glyph> {
        let key = match key {
            "\\to" | "\\longrightarrow" | "\\xrightarrow" => "\\rightarrow",
            "\\longleftarrow" | "\\xleftarrow" => "\\leftarrow",
            "\\xleftrightarrow" => "\\leftrightarrow",
            "\\Longrightarrow" => "\\Rightarrow",
            "\\ast" => "*",
            "\\prime" => "'",
            _ => key,
        };
        let g = self.glyphs.get(key).ok_or_else(|| {
            RenderError(format!(
                "missing handwriting glyph {key}; collect it or choose an expression not using it"
            ))
        })?;
        if g.status != "complete"
            || (g.variants.is_empty() && (g.bbox.is_none() || g.strokes.is_empty()))
        {
            return Err(RenderError(format!(
                "handwriting glyph {key} has no complete stroke sample"
            )));
        }
        Ok(g)
    }

    fn recorded_parenthesis_width(&self, key: &str) -> Option<f32> {
        if key != "(" && key != ")" {
            return None;
        }
        let glyph = self.lookup(key).ok()?;
        glyph
            .variants
            .iter()
            .filter_map(|v| v.bbox.as_ref())
            .chain(glyph.bbox.iter())
            .map(|bb| (bb.max_x - bb.min_x) * UNIT)
            .reduce(f32::max)
    }
}

const UNIT: f32 = 0.48; // 145 source-pixel capital -> 70 output pixels
const GAP: f32 = 4.5;
const TEXT_GAP: f32 = 6.5;
const DIGIT_GAP: f32 = 9.0;
const SUBSCRIPT_INK_GAP: f32 = 7.0;
const ENTRY_STROKE_START_WEIGHT: f32 = 0.47;
const ENTRY_STROKE_TIP_WEIGHT: f32 = 0.15;
const ENTRY_STROKE_JOIN_LENGTH: f32 = 1.0;
const L_ENTRY_STROKE_TAPER_LENGTH: f32 = 9.0;
const ONE_ENTRY_STROKE_TAPER_LENGTH: f32 = 25.0;
const DECIMAL_POINT_GAP: f32 = 8.0;
const ALIGNED_ROW_GAP: f32 = 36.0;
const FRACTION_EXTRA_WIDTH: f32 = 30.0;
const FRACTION_EXTRA_WIDTH_VARIATION: f32 = 8.0;
const PARENTHESIS_WIDTH_VARIATION: f32 = 0.18;
const PARENTHESIS_LEAN: f32 = 2.0;
const PARENTHESIS_BOW_VARIATION: f32 = 3.5;
// Geometry may shrink for scripts and fractions, but ink width never does.
const INK_WIDTH: f32 = 2.3;

// Pens and browsers report pressure on different scales, so an absolute
// pressure-to-width curve is wrong for most stroke files: one profile's usual
// pressure can sit below another's lightest. Each profile is instead mapped
// onto one fixed width band, using its own pressure spread, which gives every
// author the same contrast without making anyone's light strokes invisible.
const MIN_WEIGHT_FACTOR: f32 = 0.60;
const MAX_WEIGHT_FACTOR: f32 = 1.40;
const MIN_PRESSURE_SPAN: f32 = 0.02;
const CALIBRATION_LOW_QUANTILE: f32 = 0.10;
const CALIBRATION_MID_QUANTILE: f32 = 0.50;
const CALIBRATION_HIGH_QUANTILE: f32 = 0.90;

/// Maps recorded pressure onto an ink width for one stroke file.
struct InkWeight {
    low: f32,
    mid: f32,
    high: f32,
    base: f32,
}

impl InkWeight {
    /// `pressures` is every pressure in the profile; `base` is the width for
    /// ordinary pressure, either the profile's `inkWidth` or `INK_WIDTH`.
    fn new(pressures: &[f32], base: f32) -> Self {
        if pressures.is_empty() {
            // No pressure anywhere: one uniform width, whatever the base is.
            return Self {
                low: 0.0,
                mid: 0.0,
                high: 0.0,
                base,
            };
        }
        let mut sorted = pressures.to_vec();
        sorted.sort_by(f32::total_cmp);
        let at = |quantile: f32| {
            let index = ((sorted.len() - 1) as f32 * quantile).round() as usize;
            sorted[index]
        };
        Self {
            low: at(CALIBRATION_LOW_QUANTILE),
            mid: at(CALIBRATION_MID_QUANTILE),
            high: at(CALIBRATION_HIGH_QUANTILE),
            base,
        }
    }

    fn width(&self, pressure: f32) -> f32 {
        if self.high - self.low < MIN_PRESSURE_SPAN {
            // A flat profile carries no pressure signal to calibrate against.
            return self.base;
        }
        // Anchor the profile's own light, ordinary, and heavy pressure on the
        // band edges and centre, so every author gets the same weight range and
        // the same weight at ordinary pressure regardless of their pen.
        let t = if pressure <= self.mid {
            let lower = self.mid - self.low;
            if lower < MIN_PRESSURE_SPAN {
                0.5
            } else {
                0.5 * ((pressure - self.low) / lower).clamp(0.0, 1.0)
            }
        } else {
            let upper = self.high - self.mid;
            if upper < MIN_PRESSURE_SPAN {
                0.5
            } else {
                0.5 + 0.5 * ((pressure - self.mid) / upper).clamp(0.0, 1.0)
            }
        };
        self.base * (MIN_WEIGHT_FACTOR + (MAX_WEIGHT_FACTOR - MIN_WEIGHT_FACTOR) * t)
    }
}
#[derive(Clone)]
struct Mark {
    points: Vec<(f32, f32)>,
    pressures: Option<Vec<f32>>,
    parenthesis: bool,
    /// Number of generated entry points before the captured downstroke.
    entry_tail_points: usize,
    entry_taper_length: f32,
}
#[derive(Clone)]
struct Box2 {
    width: f32,
    above: f32,
    below: f32,
    /// Depth below this box's own baseline of the deepest subscript baseline,
    /// zero when the box holds no subscripts. Fractions use it to put the bar
    /// under the writing line, so descenders cross the bar while subscripts
    /// rest on it.
    script_drop: f32,
    marks: Vec<Mark>,
    large: bool,
}
impl Box2 {
    fn empty() -> Self {
        Self {
            width: 0.0,
            above: 0.0,
            below: 0.0,
            script_drop: 0.0,
            marks: vec![],
            large: false,
        }
    }
    fn translated(mut self, x: f32, y: f32) -> Self {
        for mark in &mut self.marks {
            for p in &mut mark.points {
                p.0 += x;
                p.1 += y;
            }
        }
        self
    }
    fn scaled(mut self, factor: f32) -> Self {
        self.width *= factor;
        self.above *= factor;
        self.below *= factor;
        self.script_drop *= factor;
        for mark in &mut self.marks {
            for p in &mut mark.points {
                p.0 *= factor;
                p.1 *= factor;
            }
        }
        self
    }
    fn scaled_vertically(mut self, factor: f32) -> Self {
        self.above *= factor;
        self.below *= factor;
        self.script_drop *= factor;
        for mark in &mut self.marks {
            for point in &mut mark.points {
                point.1 *= factor;
            }
        }
        self
    }
    fn add(&mut self, other: Self) {
        self.marks.extend(other.marks);
    }
    fn line(&mut self, points: Vec<(f32, f32)>) {
        self.marks.push(Mark {
            points,
            pressures: None,
            parenthesis: false,
            entry_tail_points: 0,
            entry_taper_length: 0.0,
        });
    }
    fn parenthesis(&mut self, points: Vec<(f32, f32)>) {
        let last = (points.len() - 1) as f32;
        let pressures = (0..points.len())
            .map(|i| 0.15 + 0.38 * (std::f32::consts::PI * i as f32 / last).sin())
            .collect();
        self.marks.push(Mark {
            points,
            pressures: Some(pressures),
            parenthesis: true,
            entry_tail_points: 0,
            entry_taper_length: 0.0,
        });
    }
}

fn body_bounds(box2: &Box2) -> Option<(f32, f32)> {
    let points = box2.marks.iter().flat_map(|mark| &mark.points);
    let body: Vec<_> = points
        .clone()
        .filter(|(_, y)| (-65.0..=-5.0).contains(y))
        .map(|(x, _)| *x)
        .collect();
    let xs: Vec<_> = if body.is_empty() {
        points.map(|(x, _)| *x).collect()
    } else {
        body
    };
    Some((
        xs.iter().copied().reduce(f32::min)?,
        xs.iter().copied().reduce(f32::max)?,
    ))
}

/// A stroke in local glyph coordinates: laid-out points plus its optional
/// pressure profile.
type RawStroke = (Vec<(f32, f32)>, Option<Vec<f32>>);

fn ink_extent(box2: &Box2, axis: impl Fn(&(f32, f32)) -> f32) -> Option<(f32, f32)> {
    let mut low = f32::INFINITY;
    let mut high = f32::NEG_INFINITY;
    for point in box2.marks.iter().flat_map(|mark| &mark.points) {
        low = low.min(axis(point));
        high = high.max(axis(point));
    }
    (low <= high).then_some((low, high))
}

fn point_segment_distance_squared(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = if dx * dx + dy * dy > 0.0 {
        ((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)
    } else {
        0.0
    }
    .clamp(0.0, 1.0);
    (p.0 - a.0 - t * dx).powi(2) + (p.1 - a.1 - t * dy).powi(2)
}

fn segment_distance_squared(a: (f32, f32), b: (f32, f32), c: (f32, f32), d: (f32, f32)) -> f32 {
    let cross = |p: (f32, f32), q: (f32, f32), r: (f32, f32)| {
        (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
    };
    if cross(a, b, c) * cross(a, b, d) <= 0.0
        && cross(c, d, a) * cross(c, d, b) <= 0.0
        && a.0.min(b.0) <= c.0.max(d.0)
        && c.0.min(d.0) <= a.0.max(b.0)
        && a.1.min(b.1) <= c.1.max(d.1)
        && c.1.min(d.1) <= a.1.max(b.1)
    {
        return 0.0;
    }
    point_segment_distance_squared(a, c, d)
        .min(point_segment_distance_squared(b, c, d))
        .min(point_segment_distance_squared(c, a, b))
        .min(point_segment_distance_squared(d, a, b))
}

fn ink_is_too_close(a: &Box2, b: &Box2, offset: f32, clearance: f32) -> bool {
    a.marks.iter().any(|left| {
        left.points
            .iter()
            .zip(left.points.iter().skip(1).chain(left.points.last()))
            .any(|(&start, &end)| {
                b.marks.iter().any(|right| {
                    right
                        .points
                        .iter()
                        .zip(right.points.iter().skip(1).chain(right.points.last()))
                        .any(|(&first, &last)| {
                            let first = (first.0 + offset, first.1);
                            let last = (last.0 + offset, last.1);
                            segment_distance_squared(start, end, first, last)
                                < clearance * clearance
                        })
                })
            })
    })
}

// Subtle deterministic asymmetry: neighboring brackets don't look stamped,
// while repeated renders of the same expression produce identical pixels.
fn vary_parenthesis(mark: &mut Mark) {
    if !mark.parenthesis || mark.points.len() < 3 {
        return;
    }
    let (x, y) = mark.points[0];
    let belly = 1.25 * (x * 0.129 + y * 0.217).sin();
    let skew = 0.95 * (x * 0.071 + y * 0.17).cos();
    let rise = 0.65 * ((x + y) * 0.09).sin();
    let last = (mark.points.len() - 1) as f32;
    for (index, point) in mark.points.iter_mut().enumerate() {
        let t = index as f32 / last;
        let envelope = (std::f32::consts::PI * t).sin();
        point.0 += envelope * (belly + skew * (2.0 * t - 1.0));
        point.1 += envelope * rise;
    }
}

// Per-instance variation is derived only from the seed, the glyph key and the
// occurrence count. It must never depend on wall-clock time or on HashMap
// iteration order, which Rust randomizes per process, or a fixed seed would
// stop producing byte-identical output.
const MAX_INSTANCE_ROTATION: f32 = 1.6 * std::f32::consts::PI / 180.0;
const MAX_INSTANCE_SCALE: f32 = 0.045;
const MAX_INSTANCE_BASELINE_OFFSET: f32 = 3.0;
const MIN_INSTANCE_PRESSURE: f32 = 0.78;
const MAX_INSTANCE_PRESSURE: f32 = 1.28;

fn mix64(mut bits: u64) -> u64 {
    bits ^= bits >> 30;
    bits = bits.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    bits ^= bits >> 27;
    bits = bits.wrapping_mul(0x94d0_49bb_1331_11eb);
    bits ^= bits >> 31;
    bits
}

fn instance_bits(seed: u64, key: &str, index: usize) -> u64 {
    let mut bits = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        bits ^= u64::from(*byte);
        bits = bits.wrapping_mul(0x0000_0100_0000_01b3);
    }
    bits ^= seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    bits ^= (index as u64)
        .wrapping_add(1)
        .wrapping_mul(0x2545_f491_4f6c_dd1d);
    mix64(bits)
}

fn unit_interval(bits: u64) -> f32 {
    (bits >> 40) as f32 / (1u64 << 24) as f32
}

fn unit_signed(bits: u64) -> f32 {
    unit_interval(bits) * 2.0 - 1.0
}

/// A slow wobble so a line of glyphs does not sit on a perfect baseline.
fn baseline_drift(seed: u64, placed: usize) -> f32 {
    let phase = (seed % 997) as f32 * 0.017;
    let index = placed as f32;
    1.8 * (index * 0.23 + phase).sin() + 0.9 * (index * 0.41 + phase * 1.7).sin()
}

struct InstanceVariation {
    scale_x: f32,
    scale_y: f32,
    rotation: f32,
    baseline_offset: f32,
    pressure_scale: f32,
}

impl InstanceVariation {
    fn for_instance(seed: u64, key: &str, index: usize) -> Self {
        let bits = instance_bits(seed, key, index);
        let mix = |amount: u32| mix64(bits.rotate_left(amount));
        Self {
            scale_x: 1.0 + MAX_INSTANCE_SCALE * unit_signed(mix(17)),
            scale_y: 1.0 + MAX_INSTANCE_SCALE * 1.2 * unit_signed(mix(29)),
            rotation: MAX_INSTANCE_ROTATION * unit_signed(mix(41)),
            baseline_offset: MAX_INSTANCE_BASELINE_OFFSET * unit_signed(mix(53)),
            pressure_scale: MIN_INSTANCE_PRESSURE
                + (MAX_INSTANCE_PRESSURE - MIN_INSTANCE_PRESSURE) * unit_interval(mix(7)),
        }
    }

    /// Rotate and scale about the glyph's baseline midpoint so it pivots like a
    /// written letter rather than a stamped one.
    fn point(&self, (x, y): (f32, f32), anchor_x: f32, drift: f32) -> (f32, f32) {
        let (x, y) = (x - anchor_x, y);
        let (sin_r, cos_r) = self.rotation.sin_cos();
        let (x, y) = (x * cos_r - y * sin_r, x * sin_r + y * cos_r);
        (
            x * self.scale_x + anchor_x,
            y * self.scale_y + self.baseline_offset + drift,
        )
    }
}

fn is_delimiter(key: &str) -> bool {
    matches!(key, "(" | ")" | "[" | "]" | "|")
}

const HAND_DRAWN_LINE_SEGMENTS: usize = 8;
const MAX_LINE_WAVE: f32 = 0.9;
const MAX_LINE_TILT: f32 = 0.9;

/// How far a hand-drawn bar can wander from its centre line at its worst point:
/// both wave amplitudes plus half the tilt.
const MAX_BAR_WOBBLE: f32 = MAX_LINE_WAVE * 1.5 + MAX_LINE_TILT / 2.0;

/// A hand-drawn straight line: a slight wave and tilt that fade to nothing at
/// the endpoints, so the line still spans exactly the same two points.
fn hand_drawn_line(
    seed: u64,
    placed: usize,
    key: &str,
    from: (f32, f32),
    to: (f32, f32),
) -> Vec<(f32, f32)> {
    let bits = instance_bits(seed, key, placed);
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let length = dx.hypot(dy).max(1.0);
    let normal = (-dy / length, dx / length);
    let phase_a = unit_interval(bits) * std::f32::consts::TAU;
    let phase_b = unit_interval(mix64(bits.rotate_left(23))) * std::f32::consts::TAU;
    let tilt = MAX_LINE_TILT * unit_signed(mix64(bits.rotate_left(37)));
    let root_segment = key.starts_with("\\sqrt");
    let root_wobble_scale = (length / 65.0).clamp(0.15, 1.0);
    (0..=HAND_DRAWN_LINE_SEGMENTS)
        .map(|index| {
            let t = index as f32 / HAND_DRAWN_LINE_SEGMENTS as f32;
            let envelope = (std::f32::consts::PI * t).sin();
            let wave = MAX_LINE_WAVE * (t * 5.3 + phase_a).sin()
                + 0.5 * MAX_LINE_WAVE * (t * 9.1 + phase_b).sin();
            // Short radical segments should not have the same-sized wobble as
            // an overline. Anchor their joins so they meet without small kinks.
            let deviation = if root_segment {
                envelope * (wave + (t - 0.5) * tilt) * root_wobble_scale
            } else {
                envelope * wave + (t - 0.5) * tilt
            };
            (
                from.0 + dx * t + normal.0 * deviation,
                from.1 + dy * t + normal.1 * deviation,
            )
        })
        .collect()
}

/// Largest distance a stroke may start below its own top and still count as a
/// free stem end whose lead-in can be replaced.
const ENTRY_MAX_START_BELOW_TOP: f32 = 10.0;
/// Largest horizontal travel of the replaced lead-in: a longer sweep along the
/// writing is a real entry stroke and must be left intact.
const ENTRY_MAX_HORIZONTAL_REACH: f32 = 4.0;

/// The point where a synthetic entry may join a captured stroke: past a short
/// lead-in, where the stroke has settled into its downstroke. `None` when the
/// stroke is not a free stem end.
fn entry_tail_join(mark: &Mark, glyph_top: f32, glyph_height: f32) -> Option<usize> {
    let &first = mark.points.first()?;
    let stroke_top = mark
        .points
        .iter()
        .map(|p| p.1)
        .fold(f32::INFINITY, f32::min);
    let stroke_bottom = mark
        .points
        .iter()
        .map(|p| p.1)
        .fold(f32::NEG_INFINITY, f32::max);
    if glyph_height < 35.0 || stroke_bottom - stroke_top < glyph_height * 0.6 {
        return None;
    }
    // A stroke that starts far below the top is not a stem end; leave it alone.
    if first.1 - glyph_top > ENTRY_MAX_START_BELOW_TOP {
        return None;
    }
    let first_down_index = mark.points.iter().position(|p| p.1 - first.1 >= 3.0)?;
    // A tiny upward curl at the start is replaced; join just below its peak,
    // where the captured line has settled into its downstroke.
    let (peak_index, _) = mark.points[..=first_down_index].iter().enumerate().fold(
        (0, f32::INFINITY),
        |(best, top), (index, point)| {
            if point.1 <= top + 0.15 {
                (index, point.1)
            } else {
                (best, top)
            }
        },
    );
    let join_index = if peak_index == 0 {
        0
    } else {
        mark.points[peak_index..]
            .iter()
            .position(|p| p.1 - mark.points[peak_index].1 >= 2.0)
            .map_or(peak_index, |offset| peak_index + offset)
    };
    let start = mark.points[join_index];
    // A prefix that sweeps along the writing is a real entry stroke, not a stub.
    if (start.0 - first.0).abs() > ENTRY_MAX_HORIZONTAL_REACH {
        return None;
    }
    // The stroke must continue downward after the join.
    mark.points[join_index + 1..]
        .iter()
        .find(|p| p.1 - start.1 >= 3.0)?;
    Some(join_index)
}

/// A brief entry stroke for a free stem end. Replaces a short isolated lead-in
/// when the captured stroke has one.
fn add_entry_tail(
    mark: &mut Mark,
    bits: u64,
    glyph_top: f32,
    glyph_height: f32,
    digit_one: bool,
) -> bool {
    let Some(join_index) = entry_tail_join(mark, glyph_top, glyph_height) else {
        return false;
    };
    let start = mark.points[join_index];
    let downstroke = mark
        .points
        .get(join_index + 1..)
        .and_then(|points| points.iter().find(|p| p.1 - start.1 >= 3.0))
        .copied()
        .unwrap_or(start);
    let (dx, dy) = (downstroke.0 - start.0, downstroke.1 - start.1);
    let distance = dx.hypot(dy);
    let length = if digit_one { 2.8 } else { 4.2 }
        + if digit_one { 1.1 } else { 1.3 } * unit_interval(mix64(bits.rotate_left(29)));
    let tip_drop = if digit_one { -0.2 } else { -1.0 }
        + if digit_one { 3.2 } else { 4.0 } * unit_interval(mix64(bits.rotate_left(13)));
    let tip = (start.0 - length, start.1 + tip_drop);
    let control_a = (
        tip.0 + length * 0.45,
        tip.1 - 0.8 - tip_drop.max(0.0) * 0.65,
    );
    // Follow the direction of the captured downstroke at the join instead of
    // adding a short corner that looks pasted onto the letter.
    let control_b = (start.0 - 2.0 * dx / distance, start.1 - 2.0 * dy / distance);
    let entry: Vec<_> = (0..4)
        .map(|step| {
            let t = step as f32 / 4.0;
            let u = 1.0 - t;
            (
                u * u * u * tip.0
                    + 3.0 * u * u * t * control_a.0
                    + 3.0 * u * t * t * control_b.0
                    + t * t * t * start.0,
                u * u * u * tip.1
                    + 3.0 * u * u * t * control_a.1
                    + 3.0 * u * t * t * control_b.1
                    + t * t * t * start.1,
            )
        })
        .collect();
    if mark.pressures.is_none() {
        mark.pressures = Some(vec![0.5; mark.points.len()]);
    }
    if let Some(pressures) = &mut mark.pressures {
        let start_pressure = pressures[join_index];
        pressures.splice(0..join_index, [start_pressure; 4]);
    }
    mark.points.splice(0..join_index, entry);
    mark.entry_tail_points = 4;
    mark.entry_taper_length = if digit_one {
        ONE_ENTRY_STROKE_TAPER_LENGTH
    } else {
        L_ENTRY_STROKE_TAPER_LENGTH
    };
    true
}

/// Bias an adjacent entry toward the previous pen lift, without drawing a
/// line all the way to it. Preserve the tangent at the recorded downstroke.
fn adjust_entry_for_previous_stroke(box2: &mut Box2, previous_end: (f32, f32), x: f32) {
    let Some(mark) = box2
        .marks
        .iter_mut()
        .find(|mark| mark.entry_tail_points > 0)
    else {
        return;
    };
    let count = mark.entry_tail_points;
    let join = mark.points[count];
    let horizontal = x + join.0 - previous_end.0;
    let vertical = previous_end.1 - join.1;
    if !(0.0..=32.0).contains(&horizontal) || !(10.0..=100.0).contains(&vertical) {
        return;
    }
    let desired_drop = (vertical * 0.235).clamp(4.0, 16.0);
    let adjustment = desired_drop - (mark.points[0].1 - join.1);
    for (i, point) in mark.points[..count].iter_mut().enumerate() {
        let remaining = 1.0 - i as f32 / count as f32;
        point.1 += adjustment * remaining * remaining;
    }
    if let Some((top, bottom)) = ink_extent(box2, |point| point.1) {
        box2.above = (-top).max(0.0);
        box2.below = bottom.max(0.0);
    }
}

pub fn png(node: &Node, handwriting: &Handwriting) -> Result<Vec<u8>> {
    png_with_seed(node, handwriting, 0)
}

pub fn png_with_seed(node: &Node, handwriting: &Handwriting, seed: u64) -> Result<Vec<u8>> {
    png_with_seed_scaled(node, handwriting, seed, 1)
}

/// Rasterize the same stroke layout at a larger output resolution.
pub fn png_with_seed_scaled(
    node: &Node,
    handwriting: &Handwriting,
    seed: u64,
    scale: u32,
) -> Result<Vec<u8>> {
    if !(1..=16).contains(&scale) {
        return Err(RenderError("output scale must be between 1 and 16".into()));
    }
    let scale = scale as f32;
    let mut engine = Layout {
        hand: handwriting,
        seed,
        occurrences: HashMap::new(),
        variation: true,
        placed: 0,
    };
    let layout = engine.layout(node)?;
    let ink = &handwriting.ink;
    let margin = 22.0;
    let w = ((layout.width + margin * 2.0) * scale).ceil().max(1.0);
    let h = ((layout.above + layout.below + margin * 2.0) * scale)
        .ceil()
        .max(1.0);
    if !w.is_finite() || !h.is_finite() || w > 16000.0 || h > 16000.0 || w * h > 40_000_000.0 {
        return Err(RenderError("image dimensions exceed safe limit".into()));
    }
    let mut pixmap = Pixmap::new(w as u32, h as u32)
        .ok_or_else(|| RenderError("failed to create image".into()))?;
    pixmap.fill(Color::WHITE);
    for mut mark in layout.marks {
        if mark.points.is_empty() {
            continue;
        }
        vary_parenthesis(&mut mark);
        if mark.entry_tail_points > 0 {
            // A captured downstroke may begin with one long sampled segment.
            // Split it near the join so the light entry does not stay thin for
            // the whole segment and then jump to the next segment's width.
            let mut distance = 0.0;
            for index in mark.entry_tail_points..mark.points.len() - 1 {
                let from = mark.points[index];
                let to = mark.points[index + 1];
                let length = (to.0 - from.0).hypot(to.1 - from.1);
                if distance + length > ENTRY_STROKE_JOIN_LENGTH {
                    let t = (ENTRY_STROKE_JOIN_LENGTH - distance) / length;
                    if (0.01..0.99).contains(&t) {
                        mark.points.insert(
                            index + 1,
                            (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t),
                        );
                        if let Some(pressures) = &mut mark.pressures {
                            let from = pressures[index];
                            pressures.insert(index + 1, from + (pressures[index + 1] - from) * t);
                        }
                    }
                    break;
                }
                distance += length;
            }
        }
        let mut builder = PathBuilder::new();
        if mark.points.len() == 1 {
            let (x, y) = mark.points[0];
            let width = mark
                .pressures
                .as_ref()
                .map_or(ink.base, |p| ink.width(p[0]));
            builder.push_circle(
                (margin + x) * scale,
                (margin + layout.above + y) * scale,
                width * scale / 2.0,
            );
            if let Some(path) = builder.finish() {
                let mut paint = Paint::default();
                paint.set_color(Color::BLACK);
                paint.anti_alias = true;
                pixmap.fill_path(
                    &path,
                    &paint,
                    tiny_skia::FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        } else if let Some(pressures) = &mark.pressures {
            let mut paint = Paint::default();
            paint.set_color(Color::BLACK);
            paint.anti_alias = true;
            let mut captured_distance = 0.0;
            let mut rendered_entry_segments = 0;
            if mark.entry_tail_points > 0 {
                let mut last = mark.entry_tail_points;
                while last + 1 < mark.points.len() && captured_distance < 0.75 {
                    let from = mark.points[last];
                    let to = mark.points[last + 1];
                    captured_distance += (to.0 - from.0).hypot(to.1 - from.1);
                    last += 1;
                }
                let join_width = ink.width(pressures[mark.entry_tail_points]);
                // Taper the entry itself: its tip is the thinnest point, and the
                // width climbs to the join weight at the recorded downstroke.
                for index in 0..last {
                    let ((x1, y1), (x2, y2)) = (mark.points[index], mark.points[index + 1]);
                    let factor = if index < mark.entry_tail_points {
                        let progress = (index as f32 + 1.0) / mark.entry_tail_points as f32;
                        ENTRY_STROKE_TIP_WEIGHT
                            + (ENTRY_STROKE_START_WEIGHT - ENTRY_STROKE_TIP_WEIGHT) * progress
                    } else {
                        ENTRY_STROKE_START_WEIGHT
                    };
                    let mut builder = PathBuilder::new();
                    builder.move_to((margin + x1) * scale, (margin + layout.above + y1) * scale);
                    builder.line_to((margin + x2) * scale, (margin + layout.above + y2) * scale);
                    if let Some(path) = builder.finish() {
                        let stroke = Stroke {
                            width: join_width * factor * scale,
                            line_cap: tiny_skia::LineCap::Round,
                            ..Stroke::default()
                        };
                        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                    }
                }
                rendered_entry_segments = last;
            }
            for (i, (segment, values)) in
                mark.points.windows(2).zip(pressures.windows(2)).enumerate()
            {
                if i < rendered_entry_segments {
                    continue;
                }
                let ((x1, y1), (x2, y2)) = (segment[0], segment[1]);
                let segment_length = (x2 - x1).hypot(y2 - y1);
                let sections =
                    if mark.entry_tail_points > 0 && captured_distance < mark.entry_taper_length {
                        (segment_length / 1.5).ceil().max(1.0) as usize
                    } else {
                        1
                    };
                for section in 0..sections {
                    let from_t = section as f32 / sections as f32;
                    let to_t = (section + 1) as f32 / sections as f32;
                    let midpoint = captured_distance + segment_length * (from_t + to_t) * 0.5;
                    let mut builder = PathBuilder::new();
                    builder.move_to(
                        (margin + x1 + (x2 - x1) * from_t) * scale,
                        (margin + layout.above + y1 + (y2 - y1) * from_t) * scale,
                    );
                    builder.line_to(
                        (margin + x1 + (x2 - x1) * to_t) * scale,
                        (margin + layout.above + y1 + (y2 - y1) * to_t) * scale,
                    );
                    if let Some(path) = builder.finish() {
                        let pressure = values[0] + (values[1] - values[0]) * (from_t + to_t) * 0.5;
                        let width = if mark.entry_tail_points == 0 {
                            ink.width(pressure)
                        } else {
                            let progress = (midpoint / mark.entry_taper_length).clamp(0.0, 1.0);
                            let gradual_progress = progress * progress * (3.0 - 2.0 * progress);
                            let entry_width = ink.width(pressures[mark.entry_tail_points])
                                * ENTRY_STROKE_START_WEIGHT;
                            entry_width + (ink.width(pressure) - entry_width) * gradual_progress
                        };
                        let stroke = Stroke {
                            width: width * scale,
                            line_cap: tiny_skia::LineCap::Round,
                            ..Stroke::default()
                        };
                        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                    }
                }
                captured_distance += segment_length;
            }
        } else {
            let (x, y) = mark.points[0];
            builder.move_to((margin + x) * scale, (margin + layout.above + y) * scale);
            for (x, y) in mark.points.into_iter().skip(1) {
                builder.line_to((margin + x) * scale, (margin + layout.above + y) * scale);
            }
            if let Some(path) = builder.finish() {
                let stroke = Stroke {
                    width: ink.base * scale,
                    line_cap: tiny_skia::LineCap::Round,
                    line_join: tiny_skia::LineJoin::Round,
                    ..Stroke::default()
                };
                let mut paint = Paint::default();
                paint.set_color(Color::BLACK);
                paint.anti_alias = true;
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
    }
    pixmap
        .encode_png()
        .map_err(|e| RenderError(format!("PNG encoding failed: {e}")))
}

struct Layout<'a> {
    hand: &'a Handwriting,
    seed: u64,
    occurrences: HashMap<String, usize>,
    variation: bool,
    placed: usize,
}
impl Layout<'_> {
    fn digit_glyph(node: &Node) -> bool {
        matches!(node, Node::Glyph(key) if key.len() == 1 && key.as_bytes()[0].is_ascii_digit())
    }

    fn decimal_point_at(nodes: &[Node], index: usize) -> bool {
        index > 0
            && matches!(nodes.get(index), Some(Node::Glyph(key)) if key == ".")
            && nodes.get(index - 1).is_some_and(Self::digit_glyph)
            && nodes.get(index + 1).is_some_and(Self::digit_glyph)
    }

    /// Relations and fraction bars belong on the user's math axis, not the
    /// writing baseline. Measure it from the center of their '=' sample.
    fn math_axis(&self) -> f32 {
        self.hand
            .glyphs
            .get("=")
            .and_then(|g| g.bbox.as_ref())
            .map_or(34.0, |bb| {
                ((bb.min_y + bb.max_y) * 0.5 * UNIT).clamp(16.0, 60.0)
            })
    }

    fn operator_padding(node: &Node, previous: Option<&Node>) -> f32 {
        match node {
            Node::Arrow(..) => 19.0,
            Node::Glyph(key) => match key.as_str() {
                "=" | "<" | ">" | "\\ne" | "\\equiv" | "\\approx" | "\\simeq"
                | "\\sim" | "\\propto" | "\\le" | "\\ge" | "\\ll" | "\\gg"
                | "\\in" | "\\notin" | "\\mid" | "\\parallel"
                | "\\to" | "\\rightarrow" | "\\longrightarrow" | "\\leftarrow"
                | "\\longleftarrow" | "\\leftrightarrow" | "\\Rightarrow"
                | "\\Longrightarrow" | "\\Leftarrow" | "\\Leftrightarrow"
                | "\\mapsto" | "\\uparrow" | "\\downarrow"
                | "\\rightleftharpoons" => 16.0,
                "+" | "*" | "/" | "\\times" | "\\cdot" | "\\div"
                | "\\pm" | "\\mp" | "\\ast" | "\\oplus" | "\\otimes" => 12.0,
                "-" if previous.is_some_and(|p| !matches!(p, Node::Glyph(g) if matches!(g.as_str(), "=" | "+" | "-" | "<" | ">"))) => 12.0,
                _ => 0.0,
            },
            _ => 0.0,
        }
    }

    fn delimiter_gap(previous: Option<&Node>, next: &Node) -> f32 {
        match (previous, next) {
            (Some(Node::Delimited(..)), Node::Delimited(..)) => 20.0,
            (Some(Node::Glyph(a)), Node::Glyph(b))
                if matches!(a.as_str(), ")" | "]" | "|")
                    && matches!(b.as_str(), "(" | "[" | "|") =>
            {
                10.0
            }
            _ => 0.0,
        }
    }

    /// Monotonic count of glyphs placed so far, in layout order.
    fn next_placement(&mut self) -> usize {
        let placed = self.placed;
        self.placed += 1;
        placed
    }

    fn glyph(&mut self, key: &str) -> Result<Box2> {
        // Use collected parentheses when available; other literal delimiters
        // (and parentheses in older profiles) remain procedural.
        if ["(", ")", "[", "]", "|"].contains(&key)
            && self.hand.recorded_parenthesis_width(key).is_none()
        {
            let mut out = Box2 {
                width: 22.0,
                above: 49.0,
                below: 19.0,
                script_drop: 0.0,
                marks: vec![],
                large: false,
            };
            self.delim(
                &mut out,
                key,
                if key == "(" || key == "[" { 13.0 } else { 5.0 },
                49.0,
                19.0,
                key == "(" || key == "[",
            )?;
            return Ok(out);
        }
        if key == "\\cdots" || key == "\\ldots" {
            let dot = self.glyph(".")?;
            let mut out = Box2::empty();
            out.width = 45.0;
            out.above = if key == "\\cdots" { 31.0 } else { dot.above };
            out.below = dot.below;
            for i in 0..3 {
                out.add(
                    dot.clone()
                        .translated(i as f32 * 17.0, if key == "\\cdots" { -31.0 } else { 0.0 }),
                );
            }
            return Ok(out);
        }
        if key == "\\iint" || key == "\\iiint" {
            let single = self.glyph("\\int")?;
            let n = if key == "\\iint" { 2 } else { 3 };
            let mut out = Box2 {
                width: single.width + (n - 1) as f32 * (single.width * 0.65),
                above: single.above,
                below: single.below,
                script_drop: 0.0,
                marks: vec![],
                large: true,
            };
            for i in 0..n {
                out.add(
                    single
                        .clone()
                        .translated(i as f32 * single.width * 0.65, 0.0),
                );
            }
            return Ok(out);
        }
        let g = self.hand.lookup(key)?;
        let occurrence = {
            let index = self.occurrences.entry(key.to_owned()).or_default();
            let value = *index;
            *index += 1;
            value
        };
        let chosen = if g.variants.is_empty() {
            None
        } else {
            Some(
                &g.variants
                    [(occurrence + (self.seed as usize % g.variants.len())) % g.variants.len()],
            )
        };
        let bb = chosen
            .and_then(|v| v.bbox.as_ref())
            .or(g.bbox.as_ref())
            .ok_or_else(|| RenderError(format!("missing bounding box for glyph {key}")))?;
        let strokes = chosen.map_or(g.strokes.as_slice(), |v| v.strokes.as_slice());
        if chosen.is_some_and(|v| v.baseline != 0.0) || strokes.is_empty() {
            return Err(RenderError(format!("invalid variant for glyph {key}")));
        }
        if ![bb.min_x, bb.max_x, bb.min_y, bb.max_y]
            .iter()
            .all(|v| v.is_finite())
            || bb.max_x < bb.min_x
            || bb.max_y < bb.min_y
        {
            return Err(RenderError(format!("invalid bounding box for glyph {key}")));
        }
        // Delimiters are stretched to a computed height and their width feeds
        // the surrounding slot math, so they stay unjittered.
        let variation = if self.variation && !is_delimiter(key) {
            Some(InstanceVariation::for_instance(self.seed, key, occurrence))
        } else {
            None
        };
        let placed = self.next_placement();
        let drift = if self.variation {
            baseline_drift(self.seed, placed)
        } else {
            0.0
        };

        let mut raw: Vec<RawStroke> = Vec::new();
        let mut ink_min_x = f32::INFINITY;
        let mut ink_max_x = f32::NEG_INFINITY;
        for stroke in strokes {
            if stroke.is_empty() {
                continue;
            }
            let mut points = Vec::with_capacity(stroke.len());
            for point in stroke {
                let (x, y) = ((point.x - bb.min_x) * UNIT, -point.y * UNIT);
                if !x.is_finite() || !y.is_finite() {
                    return Err(RenderError(format!("invalid stroke point in {key}")));
                }
                ink_min_x = ink_min_x.min(x);
                ink_max_x = ink_max_x.max(x);
                points.push((x, y));
            }
            raw.push((points, stroke.iter().map(|p| p.p).collect()));
        }
        // Rotate and scale about the baseline midpoint of the ink.
        let anchor_x = if ink_min_x.is_finite() && ink_max_x.is_finite() {
            (ink_min_x + ink_max_x) * 0.5
        } else {
            0.0
        };
        let mut out = Box2 {
            width: ((bb.max_x - bb.min_x) * UNIT).max(3.0) + GAP,
            above: (bb.max_y * UNIT).max(0.0),
            below: (-bb.min_y * UNIT).max(0.0),
            script_drop: 0.0,
            marks: vec![],
            large: matches!(key, "\\int" | "\\oint" | "\\sum" | "\\prod"),
        };
        for (mut points, mut pressures) in raw {
            if let Some(variation) = &variation {
                for point in &mut points {
                    *point = variation.point(*point, anchor_x, drift);
                }
                for value in pressures.iter_mut().flatten() {
                    *value = (*value * variation.pressure_scale).clamp(0.0, 1.0);
                }
            }
            out.marks.push(Mark {
                points,
                pressures,
                parenthesis: false,
                entry_tail_points: 0,
                entry_taper_length: 0.0,
            });
        }
        let mut tail_added = false;
        // Uppercase L: its top is a free stem end, so a short entry hook can be
        // added. Other letters are left alone.
        let eligible = matches!(key, "L");
        if self.variation && eligible {
            let tail_key = format!("{key}-tail");
            let bits = instance_bits(self.seed, &tail_key, occurrence);
            let digit_one = false;
            let chance = 0.60;
            if unit_interval(mix64(bits.rotate_left(17))) < chance
                && let Some((top, bottom)) = ink_extent(&out, |point| point.1)
            {
                for mark in &mut out.marks {
                    if add_entry_tail(mark, bits, top, bottom - top, digit_one) {
                        tail_added = true;
                        break;
                    }
                }
            }
        }
        // Measure the drawn ink so jittered glyphs keep correct spacing and are
        // never clipped by the image bounds.
        if let Some((min_x, max_x)) = ink_extent(&out, |point| point.0) {
            if tail_added && min_x < 0.0 {
                for point in out.marks.iter_mut().flat_map(|mark| &mut mark.points) {
                    point.0 -= min_x;
                }
            }
            out.width = (max_x - min_x).max(3.0) + GAP;
        }
        if let Some((min_y, max_y)) = ink_extent(&out, |point| point.1) {
            out.above = (-min_y).max(0.0);
            out.below = max_y.max(0.0);
        }
        Ok(out)
    }
    fn layout(&mut self, n: &Node) -> Result<Box2> {
        match n {
            Node::Glyph(key) => self.glyph(key),
            Node::Text(text) => self.text(text),
            Node::Space(w) => Ok(Box2 {
                width: *w,
                ..Box2::empty()
            }),
            Node::Row(nodes) => {
                let mut out = Box2::empty();
                for (i, node) in nodes.iter().enumerate() {
                    let mut b = self.layout(node)?;
                    let previous = i.checked_sub(1).map(|j| &nodes[j]);
                    let pad = Self::operator_padding(node, previous);
                    let group_gap = Self::delimiter_gap(previous, node);
                    let digit_gap = match (previous, node) {
                        (Some(Node::Glyph(a)), Node::Glyph(b))
                            if a.len() == 1
                                && b.len() == 1
                                && a.as_bytes()[0].is_ascii_digit()
                                && b.as_bytes()[0].is_ascii_digit() =>
                        {
                            DIGIT_GAP
                        }
                        _ => 0.0,
                    };
                    let decimal_gap = if Self::decimal_point_at(nodes, i)
                        || (i > 0 && Self::decimal_point_at(nodes, i - 1))
                    {
                        DECIMAL_POINT_GAP
                    } else {
                        0.0
                    };
                    let x = out.width + pad + group_gap + digit_gap + decimal_gap;
                    if matches!(node, Node::Glyph(key) if matches!(key.as_str(), "L" | "l" | "1"))
                        && matches!(previous, Some(Node::Glyph(key))
                            if (key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric())
                                || key == "\\mu")
                        && let Some(previous_end) = out.marks.last().and_then(|m| m.points.last())
                    {
                        adjust_entry_for_previous_stroke(&mut b, *previous_end, x);
                    }
                    out.above = out.above.max(b.above);
                    out.below = out.below.max(b.below);
                    out.script_drop = out.script_drop.max(b.script_drop);
                    out.width = x + b.width + pad;
                    out.add(b.translated(x, 0.0));
                }
                Ok(out)
            }
            Node::Fraction(a, b) => {
                let top = self.layout(a)?.scaled(0.55);
                let bottom = self.layout(b)?.scaled(0.55);
                let extra_width = if self.variation {
                    FRACTION_EXTRA_WIDTH_VARIATION
                        * unit_signed(instance_bits(self.seed, "\\frac-width", self.placed))
                } else {
                    0.0
                };
                let width = top.width.max(bottom.width) + FRACTION_EXTRA_WIDTH + extra_width;
                let axis = -self.math_axis();
                // The bar is the writing line for the numerator: put the
                // numerator's own baseline, or its deepest subscript baseline,
                // just above the bar. A subscript then reads as written on the
                // line, while its descender and any letter descender cross the
                // bar the way they do when writing by hand.
                //
                // The denominator is placed by its ink top rather than its
                // writing line, so the same distance reads as cramped; it keeps
                // more air under the bar.
                const FRACTION_NUMERATOR_GAP: f32 = INK_WIDTH / 2.0 + MAX_BAR_WOBBLE + 0.5;
                const FRACTION_DENOMINATOR_GAP: f32 = FRACTION_NUMERATOR_GAP + 2.0;
                let ty = axis - FRACTION_NUMERATOR_GAP - top.script_drop;
                let by = axis + FRACTION_DENOMINATOR_GAP + bottom.above;
                // A crossing descender can reach below the denominator, so the
                // box has to grow to hold it instead of clipping it.
                let numerator_low = ty + top.below;
                let mut out = Box2 {
                    width,
                    above: (-ty + top.above).max(0.0),
                    below: (by + bottom.below).max(numerator_low).max(0.0),
                    script_drop: 0.0,
                    marks: vec![],
                    large: false,
                };
                let tx = (width - top.width) / 2.0;
                let bx = (width - bottom.width) / 2.0;
                out.add(top.translated(tx, ty));
                out.add(bottom.translated(bx, by));
                let bar = if self.variation {
                    hand_drawn_line(
                        self.seed,
                        self.next_placement(),
                        "\\fracbar",
                        (2.0, axis),
                        (width - 2.0, axis),
                    )
                } else {
                    vec![(2.0, axis), (width - 2.0, axis)]
                };
                out.line(bar);
                Ok(out)
            }
            Node::Root(index, body) => {
                let body = self.layout(body)?;
                let cap = body.above.max(56.0) + 9.0;
                let body_script_drop = body.script_drop;
                let mut out = Box2 {
                    width: body.width + 30.0,
                    above: cap + 4.0,
                    below: body.below.max(13.0),
                    script_drop: 0.0,
                    marks: vec![],
                    large: false,
                };
                out.script_drop = out.script_drop.max(body_script_drop);
                out.add(body.translated(26.0, 0.0));
                let placed = self.next_placement();
                let bits = instance_bits(self.seed, "\\sqrt-bend", placed);
                let (approach_x, bend_x, bend_y) = if self.variation {
                    (
                        9.0 + 1.8 * unit_signed(bits),
                        15.0 + 4.0 * unit_signed(mix64(bits.rotate_left(17))),
                        10.0 + 1.6 * unit_signed(mix64(bits.rotate_left(31))),
                    )
                } else {
                    (9.0, 15.0, 10.0)
                };
                let corners = [
                    (1.0, -cap * 0.45),
                    (approach_x, -cap * 0.25),
                    (bend_x, bend_y),
                    (27.0, -cap),
                    (out.width - 3.0, -cap),
                ];
                let stroke = if self.variation {
                    let mut stroke = Vec::new();
                    let rounded =
                        unit_interval(instance_bits(self.seed, "\\sqrt-round", placed)) < 0.5;
                    let radius = 10.0 + 4.0 * unit_interval(mix64(bits.rotate_left(43)));
                    let toward = |from: (f32, f32), to: (f32, f32)| {
                        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
                        let fraction = radius / dx.hypot(dy);
                        (from.0 + dx * fraction, from.1 + dy * fraction)
                    };
                    let before = toward(corners[2], corners[1]);
                    let after = toward(corners[2], corners[3]);
                    let segments = [
                        (corners[0], corners[1], "\\sqrt-entry"),
                        (
                            corners[1],
                            if rounded { before } else { corners[2] },
                            "\\sqrt-down",
                        ),
                        (
                            if rounded { after } else { corners[2] },
                            corners[3],
                            "\\sqrt-up",
                        ),
                        (corners[3], corners[4], "\\sqrtbar"),
                    ];
                    for (i, &(from, to, key)) in segments.iter().enumerate() {
                        if i == 2 && rounded {
                            for step in 1..=6 {
                                let t = step as f32 / 6.0;
                                let u = 1.0 - t;
                                stroke.push((
                                    u * u * before.0 + 2.0 * u * t * corners[2].0 + t * t * after.0,
                                    u * u * before.1 + 2.0 * u * t * corners[2].1 + t * t * after.1,
                                ));
                            }
                        }
                        stroke.extend(
                            hand_drawn_line(self.seed, placed, key, from, to)
                                .into_iter()
                                .skip(usize::from(i > 0)),
                        );
                    }
                    stroke
                } else {
                    corners.to_vec()
                };
                out.line(stroke);
                if let Some(index) = index {
                    let small = self.layout(index)?.scaled(0.48);
                    out.above = out.above.max(cap + small.above + 4.0);
                    let x = -small.width * 0.25;
                    out.add(small.translated(x, -cap * 0.75));
                }
                Ok(out)
            }
            Node::Script { base, sub, sup } => {
                let integral = matches!(base.as_ref(), Node::Glyph(key)
                    if matches!(key.as_str(), "\\int" | "\\oint" | "\\iint" | "\\iiint"));
                let base = self.layout(base)?;
                let sub = sub
                    .as_deref()
                    .map(|n| {
                        self.layout(n).map(|b| {
                            b.scaled(if integral { 0.40 } else { 0.50 })
                                .scaled_vertically(0.82)
                        })
                    })
                    .transpose()?;
                let sup = sup
                    .as_deref()
                    .map(|n| {
                        self.layout(n).map(|b| {
                            b.scaled(if integral { 0.44 } else { 0.55 })
                                .scaled_vertically(0.85)
                        })
                    })
                    .transpose()?;
                if base.large {
                    let width = base
                        .width
                        .max(sup.as_ref().map_or(0.0, |b| b.width))
                        .max(sub.as_ref().map_or(0.0, |b| b.width))
                        + 6.0;
                    // Keep script ink outside the integral's actual ink
                    // bounds, not merely beyond its baseline.
                    let sy = -base.above - 10.0 - sup.as_ref().map_or(0.0, |b| b.below);
                    let uy = base.below + 10.0 + sub.as_ref().map_or(0.0, |b| b.above);
                    let mut out = Box2 {
                        width,
                        above: base.above,
                        below: base.below,
                        script_drop: 0.0,
                        marks: vec![],
                        large: false,
                    };
                    let x = (width - base.width) / 2.0;
                    out.add(base.translated(x, 0.0));
                    if let Some(b) = sup {
                        out.above = out.above.max(-sy + b.above);
                        let x = (width - b.width) / 2.0;
                        out.add(b.translated(x, sy));
                    }
                    if let Some(b) = sub {
                        out.below = out.below.max(uy + b.below);
                        let x = (width - b.width) / 2.0;
                        out.add(b.translated(x, uy));
                    }
                    return Ok(out);
                }
                let dx = base.width - 2.0;
                let sy = -base.above.max(34.0) + 10.0;
                let uy = base.below.max(8.0) + 6.0;
                // A sampled letter can reach right to its box edge, and a
                // subscript digit can begin at its left edge. Reserve a gap
                // between the actual strokes rather than relying on box padding.
                let sub_x = sub.as_ref().map_or(dx, |b| {
                    match (ink_extent(&base, |p| p.0), ink_extent(b, |p| p.0)) {
                        (Some((_, base_right)), Some((sub_left, _))) => {
                            dx.max(base_right - sub_left + SUBSCRIPT_INK_GAP)
                        }
                        _ => dx,
                    }
                });
                let script_right = sup
                    .as_ref()
                    .map_or(dx, |b| dx + b.width)
                    .max(sub.as_ref().map_or(dx, |b| sub_x + b.width));
                let base_script_drop = base.script_drop;
                let mut out = Box2 {
                    width: script_right + 5.0,
                    above: base.above,
                    below: base.below,
                    script_drop: base_script_drop,
                    marks: vec![],
                    large: false,
                };
                out.add(base);
                if let Some(b) = sup {
                    out.above = out.above.max(-sy + b.above);
                    out.add(b.translated(dx, sy));
                }
                if let Some(b) = sub {
                    out.below = out.below.max(uy + b.below);
                    out.script_drop = out.script_drop.max(uy + b.script_drop);
                    out.add(b.translated(sub_x, uy));
                }
                Ok(out)
            }
            Node::Accent(name, n) => {
                let body = self.layout(n)?;
                let body_script_drop = body.script_drop;
                let w = body.width;
                let under = body.below + 6.0;
                let y = -body.above - 7.0;
                let mut out = Box2 {
                    width: w,
                    above: body.above + 19.0,
                    below: body.below,
                    script_drop: 0.0,
                    marks: vec![],
                    large: false,
                };
                out.script_drop = out.script_drop.max(body_script_drop);
                out.add(body);
                match name.as_str() {
                    "hat" | "widehat" => {
                        out.line(vec![(3.0, y), (w / 2.0, y - 10.0), (w - 3.0, y)])
                    }
                    "tilde" | "widetilde" => out.line(vec![
                        (3.0, y - 3.0),
                        (w * 0.3, y - 9.0),
                        (w * 0.7, y),
                        (w - 3.0, y - 6.0),
                    ]),
                    "vec" => {
                        out.line(vec![
                            (3.0, y - 4.0),
                            (w - 4.0, y - 4.0),
                            (w - 12.0, y - 10.0),
                        ]);
                        out.line(vec![(w - 4.0, y - 4.0), (w - 12.0, y + 2.0)]);
                    }
                    "dot" | "ddot" => {
                        for x in if name == "dot" {
                            vec![w / 2.0]
                        } else {
                            vec![w * 0.35, w * 0.65]
                        } {
                            out.line(vec![(x, y - 4.0)]);
                        }
                    }
                    "underline" => {
                        out.line(vec![(2.0, under), (w - 2.0, under)]);
                        out.below += 9.0;
                    }
                    _ => out.line(vec![(2.0, y - 4.0), (w - 2.0, y - 4.0)]),
                }
                Ok(out)
            }
            Node::Delimited(left, n, right) => {
                let b = self.layout(n)?;
                let above = b.above.max(42.0) + 5.0;
                let below = b.below.max(16.0) + 5.0;
                let left_slot = self
                    .hand
                    .recorded_parenthesis_width(left)
                    .map_or(20.0, |w| (w + 6.0 + PARENTHESIS_BOW_VARIATION).max(20.0));
                let right_slot = self
                    .hand
                    .recorded_parenthesis_width(right)
                    .map_or(20.0, |w| (w + 6.0 + PARENTHESIS_BOW_VARIATION).max(20.0));
                let mut out = Box2 {
                    width: b.width + left_slot + right_slot,
                    above,
                    below,
                    script_drop: 0.0,
                    marks: vec![],
                    large: false,
                };
                let right_x = left_slot + b.width + 6.0;
                out.add(b.translated(left_slot, 0.0));
                self.delim(&mut out, left, left_slot - 6.0, above, below, true)?;
                self.delim(&mut out, right, right_x, above, below, false)?;
                Ok(out)
            }
            Node::Arrow(key, label) => {
                let b = self.layout(label)?.scaled(0.60);
                let arrow_key = match key.as_str() {
                    "\\xrightarrow" => "\\rightarrow",
                    "\\xleftarrow" => "\\leftarrow",
                    _ => "\\leftrightarrow",
                };
                let arrow = self.glyph(arrow_key)?;
                let width = arrow.width.max(b.width + 14.0);
                let y = -arrow.above - b.below - 8.0;
                let mut out = Box2 {
                    width,
                    above: (-y + b.above).max(arrow.above),
                    below: arrow.below,
                    script_drop: 0.0,
                    marks: vec![],
                    large: false,
                };
                let arrow_x = (width - arrow.width) / 2.0;
                let label_x = (width - b.width) / 2.0;
                out.add(arrow.translated(arrow_x, 0.0));
                out.add(b.translated(label_x, y));
                Ok(out)
            }
            Node::Aligned(rows) => {
                let layouts = rows
                    .iter()
                    .map(|(a, b)| {
                        Ok((
                            self.layout(a)?,
                            b.as_ref().map(|b| self.layout(b)).transpose()?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let left_w = layouts.iter().map(|(a, _)| a.width).fold(0.0, f32::max);
                let right_w = layouts
                    .iter()
                    .filter_map(|(_, b)| b.as_ref().map(|b| b.width))
                    .fold(0.0, f32::max);
                let mut out = Box2::empty();
                let mut cursor = 0.0;
                for (i, (a, b)) in layouts.into_iter().enumerate() {
                    let above = a.above.max(b.as_ref().map_or(0.0, |b| b.above));
                    let below = a.below.max(b.as_ref().map_or(0.0, |b| b.below));
                    cursor += if i == 0 {
                        above
                    } else {
                        above + ALIGNED_ROW_GAP
                    };
                    let baseline = cursor;
                    let x = left_w - a.width;
                    out.add(a.translated(x, baseline));
                    if let Some(b) = b {
                        out.add(b.translated(left_w + 12.0, baseline));
                    }
                    cursor += below;
                }
                out.width = left_w + if right_w > 0.0 { 12.0 + right_w } else { 0.0 };
                out.above = 0.0;
                out.below = cursor;
                Ok(out)
            }
        }
    }
    fn text(&mut self, text: &str) -> Result<Box2> {
        let mut out = Box2::empty();
        let mut word: Vec<(Box2, f32)> = Vec::new();
        let mut previous_char = None;
        let chars: Vec<char> = text.chars().collect();
        for (i, ch) in chars.iter().copied().enumerate() {
            let b = if ch == ' ' {
                Box2 {
                    width: 24.0,
                    ..Box2::empty()
                }
            } else if ch == '%' {
                self.glyph("\\%")?
            } else {
                self.glyph(&ch.to_string())?
            };
            out.above = out.above.max(b.above);
            out.below = out.below.max(b.below);
            let mut x = out.width;
            if ch.is_ascii_digit() && previous_char.is_some_and(|c: char| c.is_ascii_digit()) {
                x += DIGIT_GAP;
            }
            if (ch == '.'
                && i > 0
                && chars[i - 1].is_ascii_digit()
                && chars.get(i + 1).is_some_and(char::is_ascii_digit))
                || (ch.is_ascii_digit()
                    && i >= 2
                    && chars[i - 1] == '.'
                    && chars[i - 2].is_ascii_digit())
            {
                x += DECIMAL_POINT_GAP;
            }
            if ch.is_ascii_alphabetic()
                && let Some((prior, prior_x)) = word.last()
                && let (Some((_, prior_right)), Some((current_left, _))) =
                    (body_bounds(prior), body_bounds(&b))
            {
                x = prior_x + prior_right + TEXT_GAP - current_left;
                // Check the full strokes as well as the letter bodies: an
                // overhanging stroke can otherwise touch any earlier letter.
                while word.iter().any(|(earlier, earlier_x)| {
                    earlier_x + earlier.width + TEXT_GAP >= x
                        && ink_is_too_close(earlier, &b, x - earlier_x, TEXT_GAP)
                }) {
                    x += 0.5;
                }
            }
            out.width = out.width.max(x + b.width);
            if ch.is_ascii_alphabetic() {
                word.push((b.clone(), x));
            } else {
                word.clear();
            }
            out.add(b.translated(x, 0.0));
            previous_char = Some(ch);
        }
        Ok(out)
    }
    fn delim(
        &mut self,
        out: &mut Box2,
        token: &str,
        x: f32,
        up: f32,
        down: f32,
        opening: bool,
    ) -> Result<()> {
        if token == "." {
            return Ok(());
        }
        let top = -up;
        let bot = down;
        if let Some(width) = self.hand.recorded_parenthesis_width(token) {
            let mut sample = self.glyph(token)?;
            let min_y = sample
                .marks
                .iter()
                .flat_map(|m| &m.points)
                .map(|p| p.1)
                .fold(f32::INFINITY, f32::min);
            let max_y = sample
                .marks
                .iter()
                .flat_map(|m| &m.points)
                .map(|p| p.1)
                .fold(f32::NEG_INFINITY, f32::max);
            if max_y - min_y < 1.0 {
                return Err(RenderError(format!(
                    "parenthesis {token} needs a stroke with vertical height"
                )));
            }
            let start_x = if opening { x - width } else { x };
            // Keep collected parentheses within their reserved slots while
            // allowing each occurrence a little more/less curvature and lean.
            let bits = instance_bits(self.seed, token, self.placed);
            let width_scale = if self.variation {
                1.0 + PARENTHESIS_WIDTH_VARIATION * unit_signed(bits)
            } else {
                1.0
            };
            let lean = if self.variation {
                PARENTHESIS_LEAN * unit_signed(mix64(bits.rotate_left(29)))
            } else {
                0.0
            };
            let bow = if self.variation {
                PARENTHESIS_BOW_VARIATION * unit_signed(mix64(bits.rotate_left(11)))
            } else {
                0.0
            };
            for mark in &mut sample.marks {
                for point in &mut mark.points {
                    let t = (point.1 - min_y) / (max_y - min_y);
                    point.0 = start_x
                        + width * 0.5
                        + (point.0 - width * 0.5) * width_scale
                        + (t - 0.5) * lean
                        + if opening { -1.0 } else { 1.0 } * bow * (std::f32::consts::PI * t).sin();
                    point.1 = top + t * (bot - top);
                }
            }
            out.add(sample);
            return Ok(());
        }
        let mid = (top + bot) / 2.0;
        let kind = match token {
            "(" | ")" => "round",
            "[" | "]" => "square",
            "|" | "\\lvert" | "\\rvert" => "bar",
            "\\langle" | "\\rangle" => "angle",
            _ => "brace",
        };
        match kind {
            "round" => {
                let mut pts = Vec::new();
                let amplitude = ((up + down) * 0.14).clamp(8.0, 15.0);
                for i in 0..=20 {
                    let t = i as f32 / 20.0;
                    let y = top + (bot - top) * t;
                    let bulge = (std::f32::consts::PI * t).sin() * amplitude;
                    pts.push((x + if opening { -bulge } else { bulge }, y));
                }
                out.parenthesis(pts);
            }
            "square" => {
                let d = if opening { -7.0 } else { 7.0 };
                out.line(vec![(x, top), (x + d, top), (x + d, bot), (x, bot)]);
            }
            "bar" => out.line(vec![(x, top), (x, bot)]),
            "angle" => out.line(vec![
                (x, top),
                (x + if opening { -8.0 } else { 8.0 }, mid),
                (x, bot),
            ]),
            _ => {
                let d = if opening { -8.0 } else { 8.0 };
                out.line(vec![
                    (x, top),
                    (x + d, top + 5.0),
                    (x + d, mid - 5.0),
                    (x, mid),
                    (x + d, mid + 5.0),
                    (x + d, bot - 5.0),
                    (x, bot),
                ]);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn fixture() -> Handwriting {
        let sample = |key: &str, max_y: f32, min_y: f32| {
            serde_json::json!({
                "key": key, "status": "complete",
                "bbox": {"minX": 0.0, "maxX": 45.0, "minY": min_y, "maxY": max_y},
                "strokes": [[{"x": 0.0, "y": max_y}, {"x": 45.0, "y": min_y}]]
            })
        };
        let file: StrokeFile = serde_json::from_value(serde_json::json!({
            "schema": "aspectwrite.handwriting", "version": 1,
            "glyphs": [sample("=", 87.0, 55.0), sample("x", 65.0, 0.0),
                       sample("1", 120.0, 0.0), sample("T", 125.0, 0.0),
                       sample("\\int", 130.0, -5.0)]
        }))
        .unwrap();
        Handwriting {
            glyphs: file
                .glyphs
                .into_iter()
                .map(|g| (g.key.clone(), g))
                .collect(),
            ink: InkWeight::new(&[], INK_WIDTH),
        }
    }

    #[test]
    fn fraction_bar_tracks_equal_sign_math_axis() {
        let hand = fixture();
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        let fraction = layout.layout(&parse(r"\frac{x}{x}").unwrap()).unwrap();
        let bar = fraction
            .marks
            .iter()
            .find(|m| m.points.first().is_some_and(|p| p.0 == 2.0))
            .unwrap();
        let equals = layout.glyph("=").unwrap();
        let equals_center = (equals.marks[0].points[0].1 + equals.marks[0].points[1].1) / 2.0;
        assert!((bar.points[0].1 - equals_center).abs() < 0.1);
        let full_digit = layout.glyph("1").unwrap();
        let small_fraction = layout.layout(&parse(r"\frac{1}{1}").unwrap()).unwrap();
        let ratio =
            (small_fraction.above + small_fraction.below) / (full_digit.above + full_digit.below);
        assert!(
            (1.0..1.4).contains(&ratio),
            "fraction/full digit height: {ratio}"
        );
        // Without variation the bar is a plain two-point line.
        assert_eq!(bar.points.len(), 2);
    }

    #[test]
    fn fraction_bars_are_hand_drawn_when_variation_is_on() {
        let hand = fixture();
        let bar = |seed| {
            let mut layout = varied(&hand, seed);
            let fraction = layout.layout(&parse(r"\frac{1}{1}").unwrap()).unwrap();
            fraction
                .marks
                .iter()
                .find(|mark| mark.points.len() == HAND_DRAWN_LINE_SEGMENTS + 1)
                .map(|mark| mark.points.clone())
                .expect("a wavy fraction bar")
        };
        let first = bar(0);
        assert_eq!(first, bar(0), "the bar must be reproducible per seed");
        assert_ne!(first, bar(1), "the bar must differ between seeds");

        let (from, to) = (first[0], first[first.len() - 1]);
        assert!((from.0 - 2.0).abs() < 0.01, "the bar must start at 2.0");
        let mut largest = 0.0f32;
        for (index, point) in first.iter().enumerate() {
            let t = index as f32 / (first.len() - 1) as f32;
            let chord = from.1 + (to.1 - from.1) * t;
            largest = largest.max((point.1 - chord).abs());
        }
        assert!(largest > 0.2, "the bar is still straight: {largest}");
        assert!(
            largest <= MAX_LINE_WAVE * 1.5,
            "the bar waves too much: {largest}"
        );
    }

    #[test]
    fn captured_l_entry_tails_are_occasional_tapered_and_repeatable() {
        let mut hand = fixture();
        hand.glyphs.insert(
            "L".into(),
            serde_json::from_value(serde_json::json!({
                "key":"L", "status":"complete",
                "bbox":{"minX":0,"maxX":45,"minY":0,"maxY":120},
                "strokes":[[
                    {"x":5,"y":120,"p":0.6}, {"x":4,"y":90,"p":0.7},
                    {"x":8,"y":10,"p":0.8}, {"x":45,"y":0,"p":0.5}
                ]]
            }))
            .unwrap(),
        );
        let expression = parse("LLLLLLLL").unwrap();
        let plain = Layout {
            hand: &hand,
            seed: 7,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        }
        .layout(&expression)
        .unwrap();
        assert!(plain.marks.iter().all(|mark| mark.points.len() == 4));
        let drawn = varied(&hand, 7).layout(&expression).unwrap();
        let repeated = varied(&hand, 7).layout(&expression).unwrap();
        let mut tail_count = 0;
        let mut tip_drops = Vec::new();
        for (mark, again) in drawn.marks.iter().zip(&repeated.marks) {
            assert_eq!(mark.points, again.points);
            assert_eq!(mark.pressures, again.pressures);
            if mark.points.len() == 8 {
                tail_count += 1;
                let pressures = mark.pressures.as_ref().unwrap();
                assert!(
                    pressures[..5]
                        .windows(2)
                        .all(|pair| (pair[0] - pair[1]).abs() < 0.001)
                );
                assert!(mark.points[0].0 < mark.points[4].0);
                assert!((4.2..5.6).contains(&(mark.points[4].0 - mark.points[0].0)));
                assert!(mark.points[3].1 < mark.points[4].1);
                assert_eq!(mark.entry_tail_points, 4);
                tip_drops.push(mark.points[0].1 - mark.points[4].1);
            } else {
                assert_eq!(mark.points.len(), 4);
            }
        }
        assert!((1..drawn.marks.len()).contains(&tail_count));
        assert!(tip_drops.iter().any(|&drop| drop < -0.1));
        assert!(tip_drops.iter().any(|&drop| drop > 1.5));
        // A stroke that starts well below its top is not a free stem end.
        let mut low_start = Mark {
            points: vec![(4.0, 12.0), (3.0, 8.0), (4.0, 20.0), (30.0, 60.0)],
            pressures: None,
            parenthesis: false,
            entry_tail_points: 0,
            entry_taper_length: 0.0,
        };
        assert!(!add_entry_tail(&mut low_start, 0, 0.0, 60.0, false));
        assert_eq!(low_start.points.len(), 4);
        let mut curled_start = Mark {
            points: vec![
                (0.0, 0.0),
                (-0.3, -1.0),
                (-0.7, -1.0),
                (-1.0, -0.3),
                (-1.0, 2.6),
                (-1.3, 8.8),
                (-2.0, 28.0),
                (30.0, 60.0),
            ],
            pressures: None,
            parenthesis: false,
            entry_tail_points: 0,
            entry_taper_length: 0.0,
        };
        assert!(add_entry_tail(&mut curled_start, 0, -1.0, 61.0, false));
        assert!((curled_start.points[4].1 - 2.6).abs() < 0.01);
        assert!(curled_start.points[5].1 > curled_start.points[4].1);
        assert_eq!(
            curled_start.pressures.unwrap().len(),
            curled_start.points.len()
        );
        // A profile whose letter already begins with a long written lead-in must
        // keep it: adding or replacing part of it would double the entry.
        let mut long_lead_in = Mark {
            points: vec![
                (0.0, -1.0),
                (2.0, -1.5),
                (5.0, -1.0),
                (8.0, 0.0),
                (10.0, 6.0),
                (10.0, 60.0),
            ],
            pressures: None,
            parenthesis: false,
            entry_tail_points: 0,
            entry_taper_length: 0.0,
        };
        let original = long_lead_in.points.clone();
        assert!(!add_entry_tail(&mut long_lead_in, 0, -1.5, 61.0, false));
        assert_eq!(long_lead_in.points, original);
    }

    #[test]
    fn adjacent_entry_points_downward_without_extending_to_previous_stroke() {
        let mut box2 = Box2 {
            width: 20.0,
            above: 60.0,
            below: 0.0,
            script_drop: 0.0,
            marks: vec![Mark {
                points: vec![
                    (-5.0, -59.0),
                    (-4.0, -61.0),
                    (-2.0, -62.0),
                    (-1.0, -61.0),
                    (0.0, -60.0),
                    (0.0, -57.0),
                    (0.0, 0.0),
                ],
                pressures: None,
                parenthesis: false,
                entry_tail_points: 4,
                entry_taper_length: ONE_ENTRY_STROKE_TAPER_LENGTH,
            }],
            large: false,
        };
        let join = box2.marks[0].points[4];
        adjust_entry_for_previous_stroke(&mut box2, (0.0, 0.0), 10.0);
        let tip_drop = box2.marks[0].points[0].1 - join.1;
        assert!((10.0..20.0).contains(&tip_drop));
        assert_eq!(box2.marks[0].points[4], join);
        assert!(box2.marks[0].points[1].1 + 61.0 < box2.marks[0].points[0].1 + 59.0);
    }

    #[test]
    fn root_bends_and_overlines_vary_without_reaching_the_body() {
        let hand = fixture();
        let expression = parse(r"\sqrt{x}").unwrap();
        let mut bends = Vec::new();
        let mut rounded_count = 0;
        for seed in 0..24 {
            let drawn = varied(&hand, seed).layout(&expression).unwrap();
            let repeated = varied(&hand, seed).layout(&expression).unwrap();
            assert_eq!(drawn.marks[1].points, repeated.marks[1].points);
            let root = &drawn.marks[1].points;
            assert!(root.len() > 5);
            rounded_count += usize::from(root.len() > 33);
            bends.push(root[16].0);
            let entry_deviation = root[..=HAND_DRAWN_LINE_SEGMENTS]
                .iter()
                .map(|&p| {
                    point_segment_distance_squared(p, root[0], root[HAND_DRAWN_LINE_SEGMENTS])
                        .sqrt()
                })
                .fold(0.0, f32::max);
            assert!(entry_deviation < 0.8, "seed {seed}: root entry is jagged");
            let body_top = drawn.marks[0]
                .points
                .iter()
                .map(|p| p.1)
                .fold(f32::INFINITY, f32::min);
            let overline_bottom = root[root.len() - HAND_DRAWN_LINE_SEGMENTS..]
                .iter()
                .map(|p| p.1)
                .fold(f32::NEG_INFINITY, f32::max);
            assert!(body_top - overline_bottom > 4.0);
        }
        let range = bends.iter().copied().fold(f32::NEG_INFINITY, f32::max)
            - bends.iter().copied().fold(f32::INFINITY, f32::min);
        assert!(range > 2.0, "root bends hardly vary: {range}");
        assert!(
            (1..24).contains(&rounded_count),
            "root corners are always the same shape"
        );
    }

    #[test]
    fn fraction_overhang_varies_but_clears_its_contents() {
        let hand = fixture();
        let mut widths = Vec::new();
        for seed in 0..32 {
            let fraction = varied(&hand, seed)
                .layout(&parse(r"\frac{1}{1}").unwrap())
                .unwrap();
            let bar = fraction.marks.last().unwrap();
            let bar_left = bar.points.first().unwrap().0;
            let bar_right = bar.points.last().unwrap().0;
            let content: Vec<_> = fraction.marks[..fraction.marks.len() - 1]
                .iter()
                .flat_map(|mark| mark.points.iter().map(|p| p.0))
                .collect();
            let content_left = content.iter().copied().fold(f32::INFINITY, f32::min);
            let content_right = content.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            assert!(
                content_left - bar_left >= 5.0,
                "seed {seed}: left bar too short"
            );
            assert!(
                bar_right - content_right >= 5.0,
                "seed {seed}: right bar too short"
            );
            assert!(bar_right <= fraction.width, "seed {seed}: bar exceeds box");
            widths.push(bar_right - bar_left);
        }
        let width_range = widths.iter().copied().fold(f32::NEG_INFINITY, f32::max)
            - widths.iter().copied().fold(f32::INFINITY, f32::min);
        assert!(width_range > 3.0, "fraction bars all have the same length");
    }

    #[test]
    fn aligned_rows_have_room_between_full_height_digits() {
        let hand = fixture();
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        let digit = layout.glyph("1").unwrap();
        let rows = layout
            .layout(&parse(r"\begin{aligned}&1\\&1\end{aligned}").unwrap())
            .unwrap();
        let first_y = rows.marks[0].points[0].1;
        let second_y = rows.marks[1].points[0].1;
        assert!((second_y - first_y - digit.above - digit.below - ALIGNED_ROW_GAP).abs() < 0.01);
    }

    #[test]
    fn fraction_bar_is_the_numerator_writing_line() {
        let mut hand = fixture();
        let glyph: Glyph = serde_json::from_value(serde_json::json!({
            "key":"g", "status":"complete",
            "bbox":{"minX":0,"maxX":45,"minY":-55,"maxY":65},
            "strokes":[[{"x":0,"y":65},{"x":45,"y":-55}]]
        }))
        .unwrap();
        hand.glyphs.insert("g".into(), glyph);

        // A subscript sits on the bar; a letter descender crosses it.
        let cases = [(r"\frac{x_{1}}{1}", false), (r"\frac{g}{1}", true)];
        for (expression, crosses) in cases {
            for seed in 0..32 {
                let mut layout = varied(&hand, seed);
                let fraction = layout.layout(&parse(expression).unwrap()).unwrap();
                let bar_index = fraction
                    .marks
                    .iter()
                    .position(|mark| mark.points.len() == HAND_DRAWN_LINE_SEGMENTS + 1)
                    .expect("hand-drawn fraction bar");
                let numerator_mark_count = if expression.contains("x_{1}") { 2 } else { 1 };
                let numerator_bottom = fraction.marks[..numerator_mark_count]
                    .iter()
                    .flat_map(|mark| &mark.points)
                    .map(|point| point.1)
                    .fold(f32::NEG_INFINITY, f32::max);
                let bar = &fraction.marks[bar_index].points;
                let bar_top = bar
                    .iter()
                    .map(|point| point.1)
                    .fold(f32::INFINITY, f32::min);
                let bar_centre = bar.iter().map(|point| point.1).sum::<f32>() / bar.len() as f32;
                // White space between the numerator ink and the bar ink, both
                // measured at the bar's own worst-case wobble.
                let visible_gap = bar_top - numerator_bottom - INK_WIDTH / 2.0;
                if crosses {
                    assert!(
                        numerator_bottom > bar_centre,
                        "{expression}, seed {seed}: the descender stops {:.2}px short of the bar",
                        bar_centre - numerator_bottom
                    );
                } else {
                    assert!(
                        visible_gap > 0.0,
                        "{expression}, seed {seed}: the bar slices the subscript, gap = {visible_gap}"
                    );
                    assert!(
                        visible_gap < 2.5,
                        "{expression}, seed {seed}: subscript floats {visible_gap}px above the bar"
                    );
                }
                // The denominator is placed by its ink top, so it keeps more
                // air under the bar than the numerator's writing line does.
                let denominator_top = fraction.marks[numerator_mark_count..bar_index]
                    .iter()
                    .flat_map(|mark| &mark.points)
                    .map(|point| point.1)
                    .fold(f32::INFINITY, f32::min);
                let bar_bottom = bar
                    .iter()
                    .map(|point| point.1)
                    .fold(f32::NEG_INFINITY, f32::max);
                let denominator_gap = denominator_top - bar_bottom - INK_WIDTH / 2.0;
                assert!(
                    denominator_gap > 2.0,
                    "{expression}, seed {seed}: denominator is cramped, gap = {denominator_gap}"
                );
            }
        }
    }

    #[test]
    fn consecutive_digits_have_extra_spacing_without_changing_letters() {
        let hand = fixture();
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        let digit_width = layout.glyph("1").unwrap().width;
        let letter_width = layout.glyph("x").unwrap().width;
        assert!(
            (layout.layout(&parse("11").unwrap()).unwrap().width - 2.0 * digit_width - DIGIT_GAP)
                .abs()
                < 0.01
        );
        assert!(
            (layout.layout(&parse("xx").unwrap()).unwrap().width - 2.0 * letter_width).abs() < 0.01
        );
    }

    #[test]
    fn decimal_points_have_extra_space_only_inside_numbers() {
        let mut hand = fixture();
        hand.glyphs.insert(
            ".".into(),
            serde_json::from_value(serde_json::json!({
                "key":".", "status":"complete",
                "bbox":{"minX":0,"maxX":5,"minY":0,"maxY":5},
                "strokes":[[{"x":0,"y":5},{"x":5,"y":0}]]
            }))
            .unwrap(),
        );
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        let digit_width = layout.glyph("1").unwrap().width;
        let letter_width = layout.glyph("x").unwrap().width;
        let period_width = layout.glyph(".").unwrap().width;
        let number_width = 2.0 * digit_width + period_width + 2.0 * DECIMAL_POINT_GAP;
        assert!((layout.layout(&parse("1.1").unwrap()).unwrap().width - number_width).abs() < 0.01);
        assert!((layout.text("1.1").unwrap().width - number_width).abs() < 0.01);
        // A prose period between letters is not a decimal point.
        assert!(
            (layout.text("x.x").unwrap().width - (2.0 * letter_width + period_width)).abs() < 0.01
        );
    }

    #[test]
    fn both_scripts_are_small_and_subscripts_are_tucked_under_the_base() {
        let hand = fixture();
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        let scripts = layout.layout(&parse(r"x_{1}^{1}").unwrap()).unwrap();
        // Both scripts use the same digit sample. Subscripts should be
        // slightly smaller than superscripts and near the base.
        let widths: Vec<_> = scripts.marks[1..]
            .iter()
            .map(|m| (m.points[1].0 - m.points[0].0).abs())
            .collect();
        let heights: Vec<_> = scripts.marks[1..]
            .iter()
            .map(|m| (m.points[1].1 - m.points[0].1).abs())
            .collect();
        let (sup, sub) = if scripts.marks[1].points[0].1 < scripts.marks[2].points[0].1 {
            (0, 1)
        } else {
            (1, 0)
        };
        assert!(widths[sub] < widths[sup]);
        assert!(heights[sub] < heights[sup]);
        assert!(heights[sub] / widths[sub] < heights[sup] / widths[sup]);
        let subscript_baseline = scripts.marks[sub + 1].points[1].1;
        assert!((10.0..18.0).contains(&subscript_baseline));
    }

    #[test]
    fn subscript_ink_clears_the_base_across_variants() {
        let hand = fixture();
        for seed in 0..32 {
            let drawn = varied(&hand, seed)
                .layout(&parse(r"x_{1}^{1}").unwrap())
                .unwrap();
            let base_right = drawn.marks[0]
                .points
                .iter()
                .map(|p| p.0)
                .fold(f32::NEG_INFINITY, f32::max);
            let sub_left = drawn.marks[2]
                .points
                .iter()
                .map(|p| p.0)
                .fold(f32::INFINITY, f32::min);
            assert!(
                sub_left - base_right >= SUBSCRIPT_INK_GAP - 0.01,
                "seed {seed}: subscript is too close to its base"
            );
        }
    }

    #[test]
    fn relation_and_reaction_spacing() {
        assert_eq!(
            Layout::operator_padding(&Node::Glyph("=".into()), None),
            16.0
        );
        assert_eq!(
            Layout::operator_padding(&Node::Glyph("\\rightarrow".into()), None),
            16.0
        );
        assert_eq!(
            Layout::operator_padding(
                &Node::Arrow("\\xrightarrow".into(), Box::new(Node::Row(vec![]))),
                None
            ),
            19.0
        );
    }

    #[test]
    fn neighboring_parentheses_have_visible_ink_gap() {
        let hand = fixture();
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        for expression in [r"\left(x\right)\left(x\right)", ")("] {
            let result = layout.layout(&parse(expression).unwrap()).unwrap();
            let curves: Vec<_> = result
                .marks
                .iter()
                .filter(|m| m.points.len() == 21)
                .collect();
            assert!(curves.len() >= 2);
            // For scalable groups: left, right, left, right. For literal
            // delimiters: right, left. In either case, compare the inner pair.
            let (right, left) = if curves.len() == 4 {
                (curves[1], curves[2])
            } else {
                (curves[0], curves[1])
            };
            let max_right = right
                .points
                .iter()
                .map(|p| p.0)
                .fold(f32::NEG_INFINITY, f32::max);
            let min_left = left
                .points
                .iter()
                .map(|p| p.0)
                .fold(f32::INFINITY, f32::min);
            assert!(
                min_left - max_right >= 18.0,
                "{expression}: gap = {}",
                min_left - max_right
            );
        }
    }

    #[test]
    fn neighboring_square_brackets_have_visible_ink_gap() {
        let hand = fixture();
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        for expression in [r"\left[x\right]\left[x\right]", "]["] {
            let result = layout.layout(&parse(expression).unwrap()).unwrap();
            let brackets: Vec<_> = result
                .marks
                .iter()
                .filter(|mark| mark.points.len() == 4)
                .collect();
            let (right, left) = if expression.starts_with(r"\left") {
                assert_eq!(brackets.len(), 4, "{expression}");
                (brackets[1], brackets[2])
            } else {
                assert_eq!(brackets.len(), 2, "{expression}");
                (brackets[0], brackets[1])
            };
            let max_right = right
                .points
                .iter()
                .map(|point| point.0)
                .fold(f32::NEG_INFINITY, f32::max);
            let min_left = left
                .points
                .iter()
                .map(|point| point.0)
                .fold(f32::INFINITY, f32::min);
            assert!(
                min_left - max_right >= 18.0,
                "{expression}: gap = {}",
                min_left - max_right
            );
        }
    }

    #[test]
    fn parenthesis_variation_is_subtle_distinct_and_repeatable() {
        let hand = fixture();
        let result = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        }
        .layout(&parse(r"\left(x\right)\left(x\right)").unwrap())
        .unwrap();
        let mut curves = result.marks.iter().filter(|m| m.parenthesis);
        let mut first = curves.next().unwrap().clone();
        curves.next(); // closing parenthesis
        let mut second = curves.next().unwrap().clone();
        let original = first.clone();
        let second_original = second.clone();
        let mut repeat = first.clone();
        vary_parenthesis(&mut first);
        vary_parenthesis(&mut second);
        vary_parenthesis(&mut repeat);
        assert_eq!(first.points, repeat.points);
        assert_eq!(first.points[0], original.points[0]);
        assert_eq!(first.points.last(), original.points.last());
        let middle = first.points.len() / 2;
        let first_shift = first.points[middle].0 - original.points[middle].0;
        let second_shift = second.points[middle].0 - second_original.points[middle].0;
        assert!((first_shift - second_shift).abs() > 0.01);
        assert!(first_shift.abs() <= 2.2);
        assert!(second_shift.abs() <= 2.2);
    }

    #[test]
    fn taller_parentheses_curve_more_and_have_tapered_ends() {
        let hand = fixture();
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        let plain = layout.layout(&parse(r"\left(1\right)").unwrap()).unwrap();
        let fraction = layout
            .layout(&parse(r"\left(\frac{1}{1}\right)").unwrap())
            .unwrap();
        let small = plain.marks.iter().find(|m| m.parenthesis).unwrap();
        let large = fraction.marks.iter().find(|m| m.parenthesis).unwrap();
        assert!(large.points[0].1 < small.points[0].1);
        assert!(large.points[0].0 - large.points[10].0 > small.points[0].0 - small.points[10].0);
        let widths = large.pressures.as_ref().unwrap();
        let ink = InkWeight {
            low: 0.1,
            mid: 0.5,
            high: 0.9,
            base: INK_WIDTH,
        };
        assert!(ink.width(widths[0]) < ink.width(widths[10]));
        assert!(ink.width(widths[20]) < ink.width(widths[10]));
    }

    #[test]
    fn collected_parentheses_keep_their_horizontal_shape_at_different_heights() {
        let mut hand = fixture();
        for key in ["(", ")"] {
            let glyph: Glyph = serde_json::from_value(serde_json::json!({
                "key": key, "status": "complete",
                "variants": [{
                    "baseline": 0,
                    "bbox": {"minX": 10, "maxX": 50, "minY": -20, "maxY": 100},
                    "strokes": [[
                        {"x": 50, "y": 100, "p": 0.1},
                        {"x": 10, "y": 40, "p": 0.7},
                        {"x": 50, "y": -20, "p": 0.1}
                    ]]
                }]
            }))
            .unwrap();
            hand.glyphs.insert(key.into(), glyph);
        }
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        let normal = layout.layout(&parse(r"\left(1\right)").unwrap()).unwrap();
        let tall = layout
            .layout(&parse(r"\left(\frac{1}{1}\right)").unwrap())
            .unwrap();
        fn opening(marks: &[Mark]) -> &Mark {
            marks
                .iter()
                .find(|m| m.pressures.as_ref().is_some_and(|p| p[1] == 0.7))
                .unwrap()
        }
        let normal_mark = opening(&normal.marks);
        let tall_mark = opening(&tall.marks);
        assert_eq!(normal_mark.points.len(), 3);
        assert!(!normal_mark.parenthesis);
        assert!(
            (normal_mark.points[0].0
                - normal_mark.points[1].0
                - (tall_mark.points[0].0 - tall_mark.points[1].0))
                .abs()
                < 0.01
        );
        assert!(
            tall_mark.points[2].1 - tall_mark.points[0].1
                > normal_mark.points[2].1 - normal_mark.points[0].1
        );
        assert_eq!(normal_mark.pressures.as_ref().unwrap(), &[0.1, 0.7, 0.1]);
        assert_eq!(layout.glyph("(").unwrap().marks[0].points.len(), 3);
    }

    #[test]
    fn integral_limits_clear_ink_and_prose_has_word_gaps() {
        let hand = fixture();
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        let integral = layout.glyph("\\int").unwrap();
        let limits = layout.layout(&parse(r"\int_{T_1}^{T_1}").unwrap()).unwrap();
        assert!(limits.above > integral.above + 10.0);
        assert!(limits.below > integral.below + 10.0);
        let simple_limits = layout.layout(&parse(r"\int_{T}^{T}").unwrap()).unwrap();
        let full_t = layout.glyph("T").unwrap().marks[0].points[1].0;
        for bound in &simple_limits.marks[1..] {
            let bound_width = bound.points[1].0 - bound.points[0].0;
            assert!(
                bound_width < full_t * 0.45,
                "integral bound too large: {bound_width}"
            );
        }
        let one_word = layout.text("xx").unwrap();
        let two_words = layout.text("x x").unwrap();
        assert!(two_words.width > one_word.width + 10.0);
    }

    #[test]
    fn text_spacing_ignores_descender_overhang_without_touching_ink() {
        let mut hand = fixture();
        for (key, strokes) in [
            (
                "a",
                serde_json::json!([[{"x":0,"y":50},{"x":20,"y":40}], [{"x":50,"y":-60}]]),
            ),
            (
                "b",
                serde_json::json!([[{"x":0,"y":140}], [{"x":30,"y":40},{"x":50,"y":50}]]),
            ),
        ] {
            let glyph: Glyph = serde_json::from_value(serde_json::json!({
                "key":key, "status":"complete",
                "bbox":{"minX":0,"maxX":50,"minY":-60,"maxY":140},
                "strokes":strokes
            }))
            .unwrap();
            hand.glyphs.insert(key.into(), glyph);
        }
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        let text = layout.text("ab").unwrap();
        let first = text.marks[0].points[1].0;
        let second = text.marks[3].points[0].0;
        // These body points are 20 source pixels apart within each glyph;
        // distant ascenders/descenders must not leave a huge word gap.
        assert!((second - first - TEXT_GAP).abs() < 0.6);
    }

    #[test]
    fn crossing_strokes_are_detected_between_sampled_points() {
        let a = (0.0, 0.0);
        let b = (10.0, 10.0);
        let c = (0.0, 10.0);
        let d = (10.0, 0.0);
        assert_eq!(segment_distance_squared(a, b, c, d), 0.0);
        assert!(segment_distance_squared(a, b, (20.0, 0.0), (30.0, 0.0)) > 0.0);
    }

    #[test]
    fn pressure_is_preserved_for_ink_and_optional_for_old_profiles() {
        let mut hand = fixture();
        let glyph = hand.glyphs.get_mut("x").unwrap();
        glyph.strokes[0][0].p = Some(0.1);
        glyph.strokes[0][1].p = Some(0.9);
        let mut layout = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        assert_eq!(
            layout.glyph("x").unwrap().marks[0].pressures,
            Some(vec![0.1, 0.9])
        );
        let ink = InkWeight {
            low: 0.1,
            mid: 0.5,
            high: 0.9,
            base: INK_WIDTH,
        };
        assert!((ink.width(0.1) - INK_WIDTH * MIN_WEIGHT_FACTOR).abs() < 0.001);
        assert!((ink.width(0.5) - INK_WIDTH).abs() < 0.001);
        assert!((ink.width(0.9) - INK_WIDTH * MAX_WEIGHT_FACTOR).abs() < 0.001);
        assert!(ink.width(0.9) > ink.width(0.5));
        assert!(ink.width(0.5) > ink.width(0.1));
        // Pressures outside the profile's own range clamp to the band edges.
        assert!((ink.width(0.0) - ink.width(0.1)).abs() < 0.001);
        assert!((ink.width(1.0) - ink.width(0.9)).abs() < 0.001);
        assert_eq!(layout.glyph("T").unwrap().marks[0].pressures, None);
    }

    #[test]
    fn ink_weight_is_calibrated_to_each_profile() {
        // The same relative spread reported on two different pressure scales
        // must produce the same widths. This is the whole point of the
        // calibration: one profile's ordinary pressure can sit below another's
        // lightest, and an absolute curve cannot serve both.
        let ramp = |start: f32| -> Vec<f32> {
            (0..=100).map(|i| start + 0.30 * i as f32 / 100.0).collect()
        };
        let light = InkWeight::new(&ramp(0.05), INK_WIDTH);
        let heavy = InkWeight::new(&ramp(0.45), INK_WIDTH);
        // Light, ordinary, and heavy pressure agree across both scales.
        assert!((light.width(0.08) - heavy.width(0.48)).abs() < 0.001);
        assert!((light.width(0.20) - heavy.width(0.60)).abs() < 0.001);
        assert!((light.width(0.32) - heavy.width(0.72)).abs() < 0.001);
        // Ordinary pressure lands exactly on the base weight for both.
        assert!((light.width(0.20) - INK_WIDTH).abs() < 0.001);
        assert!((heavy.width(0.60) - INK_WIDTH).abs() < 0.001);
        // And each profile still spans the full band.
        assert!((light.width(0.08) - INK_WIDTH * MIN_WEIGHT_FACTOR).abs() < 0.001);
        assert!((heavy.width(0.72) - INK_WIDTH * MAX_WEIGHT_FACTOR).abs() < 0.001);

        // A profile with no pressure spread has nothing to calibrate against.
        let flat = InkWeight::new(&[0.5; 200], INK_WIDTH);
        assert!((flat.width(0.1) - INK_WIDTH).abs() < 0.001);
        assert!((flat.width(0.9) - INK_WIDTH).abs() < 0.001);

        // A profile with no pressures at all matches a flat one.
        let none = InkWeight::new(&[], INK_WIDTH);
        assert!((none.width(0.0) - INK_WIDTH).abs() < 0.001);

        // The base weight scales the whole band.
        let doubled = InkWeight::new(&ramp(0.05), INK_WIDTH * 2.0);
        assert!((doubled.width(0.2) - 2.0 * light.width(0.2)).abs() < 0.001);
    }

    #[test]
    fn ink_width_field_sets_the_base_weight_and_is_validated() {
        let directory = std::env::temp_dir();
        let write = |name: &str, ink_width: &str| {
            let path = directory.join(format!("aspectwrite-ink-{}.json", name));
            std::fs::write(
                &path,
                format!(
                    r#"{{"schema":"aspectwrite.handwriting","version":2,{ink_width}
                        "glyphs":[{{"key":"a","status":"complete",
                        "bbox":{{"minX":0,"maxX":40,"minY":0,"maxY":90}},
                        "strokes":[[{{"x":0,"y":90,"p":0.2}},{{"x":40,"y":0,"p":0.8}}]]}}]}}"#
                ),
            )
            .unwrap();
            let loaded = Handwriting::load(&path);
            (path, loaded)
        };

        let (path, loaded) = write("default", "");
        let hand = loaded.unwrap();
        assert!((hand.ink.base - INK_WIDTH).abs() < 0.001);
        std::fs::remove_file(path).unwrap();

        let (path, loaded) = write("custom", r#""inkWidth": 4.0,"#);
        let hand = loaded.unwrap();
        assert!((hand.ink.base - 4.0).abs() < 0.001);
        assert!((hand.ink.width(0.2) - 4.0 * MIN_WEIGHT_FACTOR).abs() < 0.001);
        std::fs::remove_file(path).unwrap();

        let (path, loaded) = write("invalid", r#""inkWidth": -1,"#);
        assert!(loaded.is_err());
        std::fs::remove_file(path).unwrap();
    }

    fn varied<'a>(hand: &'a Handwriting, seed: u64) -> Layout<'a> {
        Layout {
            hand,
            seed,
            occurrences: HashMap::new(),
            variation: true,
            placed: 0,
        }
    }

    #[test]
    fn instance_variation_is_reproducible_and_distinct_per_occurrence() {
        let hand = fixture();
        let points = |seed| {
            varied(&hand, seed).glyph("x").unwrap().marks[0]
                .points
                .clone()
        };
        // A fixed seed must reproduce exactly; a different seed must not.
        assert_eq!(points(0), points(0));
        assert_ne!(points(0), points(1));
        // Repeating a glyph inside one expression must vary it.
        let mut layout = varied(&hand, 0);
        let first = layout.glyph("x").unwrap().marks[0].points.clone();
        let second = layout.glyph("x").unwrap().marks[0].points.clone();
        assert_ne!(first, second);
        // With variation off the two occurrences are identical.
        let mut plain = Layout {
            hand: &hand,
            seed: 0,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        assert_eq!(
            plain.glyph("x").unwrap().marks[0].points,
            plain.glyph("x").unwrap().marks[0].points
        );
    }

    #[test]
    fn instance_variation_stays_subtle() {
        let hand = fixture();
        let base = {
            let mut plain = Layout {
                hand: &hand,
                seed: 0,
                occurrences: HashMap::new(),
                variation: false,
                placed: 0,
            };
            plain.glyph("x").unwrap().marks[0].points.clone()
        };
        let mut moved = 0;
        let mut largest = 0.0f32;
        for seed in 0..64 {
            let varied_points = varied(&hand, seed).glyph("x").unwrap().marks[0]
                .points
                .clone();
            for (before, after) in base.iter().zip(&varied_points) {
                let (dx, dy) = ((after.0 - before.0).abs(), (after.1 - before.1).abs());
                assert!(dx <= 5.0, "horizontal jitter {dx} is too large");
                assert!(dy <= 10.0, "vertical jitter {dy} is too large");
                if dx > 0.01 || dy > 0.01 {
                    moved += 1;
                }
                largest = largest.max(dx).max(dy);
            }
        }
        assert!(moved > 0, "variation never moved any point");
        // A jitter this small is invisible on a real glyph, which is how an
        // earlier version of this feature shipped without any visible effect.
        assert!(
            largest >= 1.5,
            "variation is too small to notice: largest displacement {largest}"
        );
    }

    #[test]
    fn collected_parentheses_vary_without_touching_their_contents() {
        let mut hand = fixture();
        for key in ["(", ")"] {
            let glyph: Glyph = serde_json::from_value(serde_json::json!({
                "key": key, "status": "complete",
                "variants": [{
                    "baseline": 0,
                    "bbox": {"minX": 10, "maxX": 50, "minY": -20, "maxY": 100},
                    "strokes": [[{"x": 50, "y": 100}, {"x": 10, "y": 40}, {"x": 50, "y": -20}]]
                }]
            }))
            .unwrap();
            hand.glyphs.insert(key.into(), glyph);
        }
        let recorded = &hand.glyphs.get("(").unwrap().variants[0].strokes[0];
        let mut widths = Vec::new();
        for seed in 0..32 {
            let drawn = varied(&hand, seed)
                .layout(&parse(r"\left(1\right)").unwrap())
                .unwrap();
            let body = &drawn.marks[0];
            let opening = &drawn.marks[1];
            let closing = &drawn.marks[2];
            let (max_open, min_body) = (
                opening
                    .points
                    .iter()
                    .map(|p| p.0)
                    .fold(f32::NEG_INFINITY, f32::max),
                body.points
                    .iter()
                    .map(|p| p.0)
                    .fold(f32::INFINITY, f32::min),
            );
            let (max_body, min_close) = (
                body.points
                    .iter()
                    .map(|p| p.0)
                    .fold(f32::NEG_INFINITY, f32::max),
                closing
                    .points
                    .iter()
                    .map(|p| p.0)
                    .fold(f32::INFINITY, f32::min),
            );
            assert!(
                min_body - max_open >= 2.0,
                "seed {seed}: opening touches content"
            );
            assert!(
                min_close - max_body >= 2.0,
                "seed {seed}: closing touches content"
            );
            widths.push(
                opening
                    .points
                    .iter()
                    .map(|p| p.0)
                    .fold(f32::NEG_INFINITY, f32::max)
                    - opening
                        .points
                        .iter()
                        .map(|p| p.0)
                        .fold(f32::INFINITY, f32::min),
            );

            // Vertical stretching is still uniform; the new variation is
            // horizontal only and keeps the recorded gesture recognizable.
            let mut stretch: Option<f32> = None;
            for (pair, original) in opening.points.windows(2).zip(recorded.windows(2)) {
                let dy = pair[1].1 - pair[0].1;
                let original_dy = -(original[1].y - original[0].y) * UNIT;
                if original_dy.abs() > 0.01 {
                    let ratio = dy / original_dy;
                    if let Some(previous) = stretch {
                        assert!((ratio - previous).abs() < 0.01);
                    }
                    stretch = Some(ratio);
                }
            }
            assert!(stretch.is_some());
        }
        let span = widths.iter().copied().fold(f32::NEG_INFINITY, f32::max)
            - widths.iter().copied().fold(f32::INFINITY, f32::min);
        assert!(
            span > 2.0,
            "collected parentheses barely vary: width range {span}"
        );
    }

    #[test]
    fn pressure_scaling_varies_weight_between_occurrences() {
        let mut hand = fixture();
        let glyph: Glyph = serde_json::from_value(serde_json::json!({
            "key": "p", "status": "complete",
            "bbox": {"minX": 0, "maxX": 45, "minY": 0, "maxY": 65},
            "strokes": [[{"x": 0, "y": 65, "p": 0.5}, {"x": 45, "y": 0, "p": 0.5}]]
        }))
        .unwrap();
        hand.glyphs.insert("p".into(), glyph);

        let mut layout = varied(&hand, 5);
        let first = layout.glyph("p").unwrap().marks[0]
            .pressures
            .clone()
            .unwrap();
        let second = layout.glyph("p").unwrap().marks[0]
            .pressures
            .clone()
            .unwrap();
        assert_ne!(first, second, "weight should vary between occurrences");
        for value in first.iter().chain(second.iter()) {
            let factor = value / 0.5;
            assert!(
                (MIN_INSTANCE_PRESSURE - 0.001..=MAX_INSTANCE_PRESSURE + 0.001).contains(&factor),
                "pressure factor {factor} is outside the declared range"
            );
        }

        let mut plain = Layout {
            hand: &hand,
            seed: 5,
            occurrences: HashMap::new(),
            variation: false,
            placed: 0,
        };
        assert_eq!(
            plain.glyph("p").unwrap().marks[0].pressures,
            Some(vec![0.5, 0.5])
        );
    }
}
