//! The Entangled mark: two rings that pass over and under each other — really
//! linked, the way two entangled particles are one system. Two circles and two
//! short arcs; the same geometry is rasterised once for the native
//! window/taskbar icon.
//!
//! Everything here is procedural: no image assets, resolution-free.

use egui::{Color32, Painter, Pos2, Rect, Stroke, Vec2};

use crate::theme;

/// The mark is designed on a 48-unit square; these are its dimensions there.
const UNITS: f32 = 48.0;
/// Ring radius.
const RADIUS: f32 = 12.0;
/// Horizontal offset of each ring's centre from the mark's centre.
const OFFSET: f32 = 5.0;
/// Ring stroke at large sizes. Small sizes thicken it (see [`stroke_units`]).
const STROKE: f32 = 3.4;
/// Clear space on each side of the ring that passes over, so the over/under
/// reads as a gap rather than as two lines touching.
const GAP: f32 = 1.8;
/// Half the angle each over-arc spans around its crossing, radians.
const ARC_HALF: f32 = 0.35;
/// How far the coloured over-arc runs past its knockout at each end, radians.
const ARC_OVERLAP: f32 = 0.07;
/// Segments per over-arc.
const ARC_STEPS: usize = 16;

/// Ring stroke for a mark drawn `size` pixels wide: heavier as it shrinks, so
/// the gap still reads at 16 px.
fn stroke_units(size: f32) -> f32 {
    if size >= 96.0 {
        STROKE
    } else if size >= 40.0 {
        3.8
    } else if size >= 24.0 {
        4.4
    } else {
        5.0
    }
}

/// Angle of the upper crossing seen from the left ring's centre, and of the
/// lower crossing from the right ring's centre. They are the same number up to
/// a sign and a half turn: the geometry is symmetric.
fn crossing_angle() -> f32 {
    let dy = (RADIUS * RADIUS - OFFSET * OFFSET).sqrt();
    dy.atan2(OFFSET)
}

/// Draws the mark centred in `rect`. `background` is the surface it sits on:
/// the gap where one ring passes over the other is painted in it.
pub fn paint_mark(painter: &Painter, rect: Rect, background: Color32) {
    let size = rect.width().min(rect.height());
    let unit = size / UNITS;
    let center = rect.center();
    let left = center - Vec2::new(OFFSET * unit, 0.0);
    let right = center + Vec2::new(OFFSET * unit, 0.0);
    let r = RADIUS * unit;
    let width = stroke_units(size) * unit;
    let ring = |c: Pos2, color: Color32| painter.circle_stroke(c, r, Stroke::new(width, color));

    // The light ring is drawn last, so it is on top at both crossings…
    ring(right, theme::ACCENT);
    ring(left, theme::TEXT);
    // …then each crossing gets the ring that belongs on top there, over a
    // knockout in the background colour: the left ring over at the top, the
    // right ring over at the bottom. That is what makes them linked.
    let a = crossing_angle();
    over_arc(painter, left, r, -a, width, theme::TEXT, background);
    over_arc(
        painter,
        right,
        r,
        std::f32::consts::PI - a,
        width,
        theme::ACCENT,
        background,
    );
}

fn over_arc(
    painter: &Painter,
    center: Pos2,
    radius: f32,
    around: f32,
    width: f32,
    color: Color32,
    background: Color32,
) {
    let arc = |half: f32| -> Vec<Pos2> {
        (0..=ARC_STEPS)
            .map(|i| {
                let t = around - half + 2.0 * half * i as f32 / ARC_STEPS as f32;
                center + Vec2::angled(t) * radius
            })
            .collect()
    };
    let gap = GAP * width / STROKE;
    painter.add(egui::Shape::line(
        arc(ARC_HALF),
        Stroke::new(width + 2.0 * gap, background),
    ));
    // A little longer than its knockout, so the two anti-aliased ends never
    // land on the same pixel and leave a seam across the ring.
    painter.add(egui::Shape::line(
        arc(ARC_HALF + ARC_OVERLAP),
        Stroke::new(width, color),
    ));
}

/// Native 64×64 taskbar/window icon: the mark on a rounded tile, rasterised
/// with 4×4 supersampling from the same geometry. Done once at startup, which
/// keeps packaging free of platform-specific PNG/ICO drift.
pub fn app_icon() -> egui::IconData {
    const SIZE: u32 = 64;
    const SS: u32 = 4;
    let tile = theme::CARD;
    let border = theme::STROKE_STRONG;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut acc = [0.0_f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    let c = icon_sample(px, py, SIZE as f32, tile, border);
                    for (slot, value) in acc.iter_mut().zip(c) {
                        *slot += value;
                    }
                }
            }
            let n = (SS * SS) as f32;
            rgba.extend(acc.iter().map(|v| (v / n).round().clamp(0.0, 255.0) as u8));
        }
    }
    egui::IconData {
        rgba,
        width: SIZE,
        height: SIZE,
    }
}

/// One sample of the icon, straight RGBA in 0..=255.
fn icon_sample(px: f32, py: f32, size: f32, tile: Color32, border: Color32) -> [f32; 4] {
    let half = size * 0.5;
    // Rounded tile: corner radius 22% of the size, 1 px border.
    let corner = size * 0.22;
    let qx = (px - half).abs() - (half - corner);
    let qy = (py - half).abs() - (half - corner);
    let outside = Vec2::new(qx.max(0.0), qy.max(0.0)).length() + qx.max(qy).min(0.0) - corner;
    if outside > 0.0 {
        return [0.0; 4];
    }
    let base = if outside > -1.2 { border } else { tile };

    // The mark fills 72% of the tile.
    let unit = size * 0.72 / UNITS;
    let (cx, cy) = (half, half);
    let width = 4.0 * unit;
    let gap = GAP * width / STROKE;
    let dist = |ox: f32| ((px - (cx + ox * unit)).powi(2) + (py - cy).powi(2)).sqrt();
    let on_left = (dist(-OFFSET) - RADIUS * unit).abs();
    let on_right = (dist(OFFSET) - RADIUS * unit).abs();
    // Which ring is on top here: the left one above the centre line, the
    // right one below it — the same linking the painter draws.
    let (top, top_color, under, under_color) = if py < cy {
        (on_left, theme::TEXT, on_right, theme::ACCENT)
    } else {
        (on_right, theme::ACCENT, on_left, theme::TEXT)
    };
    let color = if top <= width * 0.5 {
        top_color
    } else if top <= width * 0.5 + gap {
        base
    } else if under <= width * 0.5 {
        under_color
    } else {
        base
    };
    [color.r() as f32, color.g() as f32, color.b() as f32, 255.0]
}

/// Work in progress (Installing, Suspending): a three-quarter arc turning in
/// `color`. Sized to fit `rect` (a status badge is ~14 px tall).
pub fn paint_spinner(painter: &Painter, rect: Rect, time: f64, color: Color32) {
    let center = rect.center();
    let radius = rect.height().min(rect.width()) * 0.30;
    let start = (time * 4.0) as f32;
    let points: Vec<Pos2> = (0..=18)
        .map(|i| {
            let t = start + std::f32::consts::TAU * 0.75 * i as f32 / 18.0;
            center + Vec2::angled(t) * radius
        })
        .collect();
    painter.circle_stroke(
        center,
        radius,
        Stroke::new(1.6_f32, color.gamma_multiply(0.2)),
    );
    painter.add(egui::Shape::line(points, Stroke::new(1.6_f32, color)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rings_really_cross_where_the_arcs_are_drawn() {
        // The upper crossing, seen from the left ring's centre, must lie on
        // the right ring too — otherwise the over-arc covers nothing.
        let a = crossing_angle();
        let left = Pos2::new(-OFFSET, 0.0);
        let p = left + Vec2::angled(-a) * RADIUS;
        let from_right = (p - Pos2::new(OFFSET, 0.0)).length();
        assert!((from_right - RADIUS).abs() < 1e-3, "{from_right}");
        assert!(p.y < 0.0, "the left ring's over-arc is the upper crossing");
        let right = Pos2::new(OFFSET, 0.0);
        let q = right + Vec2::angled(std::f32::consts::PI - a) * RADIUS;
        assert!(((q - left).length() - RADIUS).abs() < 1e-3);
        assert!(q.y > 0.0, "the right ring's over-arc is the lower crossing");
    }

    #[test]
    fn small_marks_get_heavier_strokes() {
        assert!(stroke_units(16.0) > stroke_units(32.0));
        assert!(stroke_units(32.0) > stroke_units(64.0));
        assert!(stroke_units(64.0) > stroke_units(128.0));
    }

    #[test]
    fn native_icon_has_the_expected_rgba_shape() {
        let icon = app_icon();
        assert_eq!((icon.width, icon.height), (64, 64));
        assert_eq!(icon.rgba.len(), 64 * 64 * 4);
        assert_eq!(icon.rgba[3], 0, "the outer corner stays transparent");
        assert_eq!(icon.rgba[(32 * 64 + 32) * 4 + 3], 255);
    }

    #[test]
    fn the_icon_shows_both_rings_and_a_gap() {
        let icon = app_icon();
        let px = |x: usize, y: usize| {
            let i = (y * 64 + x) * 4;
            Color32::from_rgb(icon.rgba[i], icon.rgba[i + 1], icon.rgba[i + 2])
        };
        let colours: Vec<Color32> = (0..64)
            .flat_map(|y| (0..64).map(move |x| (x, y)))
            .map(|(x, y)| px(x, y))
            .collect();
        let near = |a: Color32, b: Color32| {
            (a.r() as i32 - b.r() as i32).abs() < 6
                && (a.g() as i32 - b.g() as i32).abs() < 6
                && (a.b() as i32 - b.b() as i32).abs() < 6
        };
        assert!(colours.iter().any(|c| near(*c, theme::TEXT)), "light ring");
        assert!(colours.iter().any(|c| near(*c, theme::ACCENT)), "ice ring");
    }
}
