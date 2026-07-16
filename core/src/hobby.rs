//! Track curve computation matching Nimby Rails' actual algorithm.
//! NOT standard Hobby splines — uses local tangent computation
//! (weighted bisector) instead of global tridiagonal solver.
//!
//! Reverse engineered from NIMBYRails.exe:
//!   Tangent: RVA 0xBA490/0xBA5C0 (local bisector)
//!   Rho:     RVA 0xBD8E0 (golden-ratio based velocity function)

/// Hobby's rho velocity parameter: `(3 - √5) / 2` ≈ 0.382
const DELTA: f64 = (3.0 - 2.236_067_977_499_79) / 2.0; // (3-√5)/2
const EPSILON: f64 = 1e-10;

/// A cubic Bézier segment: start, control1, control2, end
pub struct BezierSegment {
    pub p0: (f64, f64),
    pub c0: (f64, f64),
    pub c1: (f64, f64),
    pub p1: (f64, f64),
}

/// Compute track curves through a sequence of points using the game's
/// actual algorithm: local bisector tangents + Hobby rho velocity function.
#[must_use]
pub fn hobby_spline(points: &[(f64, f64)]) -> Vec<BezierSegment> {
    hobby_spline_with_tangents(points, None, None)
}

/// Compute a spline with overridden tangent angles at specific endpoints.
/// `start_tangent`: if Some, override the tangent at the first point.
/// `end_tangent`: if Some, override the tangent at the last point.
#[must_use]
pub fn hobby_spline_with_tangents(
    points: &[(f64, f64)],
    start_tangent: Option<f64>,
    end_tangent: Option<f64>,
) -> Vec<BezierSegment> {
    if points.len() < 2 {
        return Vec::new();
    }
    if points.len() == 2 {
        return two_point_segment(points, start_tangent);
    }

    let n = points.len();

    // Compute chord vectors and lengths
    let (chords, chord_lens) = compute_chords(points);

    // Compute tangent angle at each node
    let tangent_angles = compute_tangent_angles(&chords, n, start_tangent, end_tangent);

    // Generate Bézier segments from tangents
    generate_segments(points, &chords, &chord_lens, &tangent_angles)
}

/// Get the tangent direction (unit vector) at parameter t along a spline.
/// t is in [0, `total_length`] mapped to segment index + local t.
/// For `attached_to_t` lookup: t ∈ [0, 1] maps across the entire chain.
#[must_use]
pub fn spline_direction_at(segments: &[BezierSegment], t: f64) -> (f64, f64) {
    if segments.is_empty() {
        return (1.0, 0.0);
    }
    // Map t ∈ [0, 1] to segment index + local parameter
    let n = segments.len() as f64;
    let global = (t * n).max(0.0);
    let seg_idx = (global as usize).min(segments.len() - 1);
    let local_t = (global - seg_idx as f64).clamp(0.0, 1.0);

    // Cubic Bézier derivative: B'(t) = 3(1-t)²(c0-p0) + 6(1-t)t(c1-c0) + 3t²(p1-c1)
    let seg = &segments[seg_idx];
    let mt = 1.0 - local_t;
    let dx = 3.0 * mt * mt * (seg.c0.0 - seg.p0.0)
           + 6.0 * mt * local_t * (seg.c1.0 - seg.c0.0)
           + 3.0 * local_t * local_t * (seg.p1.0 - seg.c1.0);
    let dy = 3.0 * mt * mt * (seg.c0.1 - seg.p0.1)
           + 6.0 * mt * local_t * (seg.c1.1 - seg.c0.1)
           + 3.0 * local_t * local_t * (seg.p1.1 - seg.c1.1);
    let len = (dx * dx + dy * dy).sqrt().max(EPSILON);
    (dx / len, dy / len)
}

/// Sample a cubic Bézier at parameter t ∈ [0, 1]
#[must_use]
pub fn bezier_point(seg: &BezierSegment, t: f64) -> (f64, f64) {
    let t2 = t * t;
    let t3 = t2 * t;
    let mt = 1.0 - t;
    let mt2 = mt * mt;
    let mt3 = mt2 * mt;
    (
        mt3 * seg.p0.0 + 3.0 * mt2 * t * seg.c0.0 + 3.0 * mt * t2 * seg.c1.0 + t3 * seg.p1.0,
        mt3 * seg.p0.1 + 3.0 * mt2 * t * seg.c0.1 + 3.0 * mt * t2 * seg.c1.1 + t3 * seg.p1.1,
    )
}

// ══════════════════════════════════════════════════════════════════════
// Internal helpers
// ══════════════════════════════════════════════════════════════════════

fn two_point_segment(points: &[(f64, f64)], start_tangent: Option<f64>) -> Vec<BezierSegment> {
    let (x0, y0) = points[0];
    let (x1, y1) = points[1];
    let angle = start_tangent.unwrap_or_else(|| (y1 - y0).atan2(x1 - x0));
    let d = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt() / 3.0;
    let (sa, ca) = angle.sin_cos();
    vec![BezierSegment {
        p0: (x0, y0),
        c0: (x0 + ca * d, y0 + sa * d),
        c1: (x1 - ca * d, y1 - sa * d),
        p1: (x1, y1),
    }]
}

fn compute_chords(points: &[(f64, f64)]) -> (Vec<(f64, f64)>, Vec<f64>) {
    let mut chords = Vec::with_capacity(points.len() - 1);
    let mut chord_lens = Vec::with_capacity(points.len() - 1);
    for i in 0..points.len() - 1 {
        let dx = points[i + 1].0 - points[i].0;
        let dy = points[i + 1].1 - points[i].1;
        let len = (dx * dx + dy * dy).sqrt().max(EPSILON);
        chords.push((dx, dy));
        chord_lens.push(len);
    }
    (chords, chord_lens)
}

fn compute_tangent_angles(
    chords: &[(f64, f64)],
    n: usize,
    start_tangent: Option<f64>,
    end_tangent: Option<f64>,
) -> Vec<f64> {
    let mut angles = Vec::with_capacity(n);
    for i in 0..n {
        let angle = if i == 0 {
            start_tangent.unwrap_or_else(|| chords[0].1.atan2(chords[0].0))
        } else if i == n - 1 {
            end_tangent.unwrap_or_else(|| chords[n - 2].1.atan2(chords[n - 2].0))
        } else {
            // Interior: bisector of incoming/outgoing chord directions
            // (matching game at RVA 0xBA5C0)
            let in_angle = chords[i - 1].1.atan2(chords[i - 1].0);
            let out_angle = chords[i].1.atan2(chords[i].0);
            let turn = normalize_angle(out_angle - in_angle);
            in_angle + turn / 2.0
        };
        angles.push(angle);
    }
    angles
}

fn generate_segments(
    points: &[(f64, f64)],
    chords: &[(f64, f64)],
    chord_lens: &[f64],
    tangent_angles: &[f64],
) -> Vec<BezierSegment> {
    let n = points.len();
    let mut segments = Vec::with_capacity(n - 1);
    for i in 0..n - 1 {
        let chord_angle = chords[i].1.atan2(chords[i].0);
        let d = chord_lens[i];
        let alpha = normalize_angle(tangent_angles[i] - chord_angle);
        let beta = normalize_angle(chord_angle - tangent_angles[i + 1]);
        let (rho_a, rho_b) = hobby_rho(alpha, beta);
        let c0_dir = rotate_unit(chord_angle + alpha);
        let c1_dir = rotate_unit(chord_angle - beta);
        segments.push(BezierSegment {
            p0: points[i],
            c0: (points[i].0 + rho_a * d * c0_dir.0, points[i].1 + rho_a * d * c0_dir.1),
            c1: (points[i + 1].0 - rho_b * d * c1_dir.0, points[i + 1].1 - rho_b * d * c1_dir.1),
            p1: points[i + 1],
        });
    }
    segments
}

/// Game's rho velocity function (RVA 0xBD8E0).
fn hobby_rho(alpha: f64, beta: f64) -> (f64, f64) {
    let sa = alpha.sin();
    let sb = beta.sin();
    let ca = alpha.cos();
    let cb = beta.cos();

    let a = sa - sb / 16.0;
    let b = sb - sa / 16.0;
    let c = ca - cb;
    let f = std::f64::consts::SQRT_2 * a * b * c;

    let denom_a = 3.0 * (1.0 + (1.0 - DELTA) * ca + DELTA * cb);
    let denom_b = 3.0 * (1.0 + (1.0 - DELTA) * cb + DELTA * ca);

    let rho_a = if denom_a.abs() > EPSILON { (2.0 + f) / denom_a } else { 1.0 / 3.0 };
    let rho_b = if denom_b.abs() > EPSILON { (2.0 - f) / denom_b } else { 1.0 / 3.0 };

    (rho_a.max(0.0), rho_b.max(0.0))
}

fn normalize_angle(mut a: f64) -> f64 {
    while a > std::f64::consts::PI { a -= std::f64::consts::TAU; }
    while a < -std::f64::consts::PI { a += std::f64::consts::TAU; }
    a
}

fn rotate_unit(angle: f64) -> (f64, f64) {
    (angle.cos(), angle.sin())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool { (a - b).abs() < 1e-9 }

    #[test]
    fn fewer_than_two_points_yields_no_segments() {
        assert!(hobby_spline(&[]).is_empty());
        assert!(hobby_spline(&[(0.0, 0.0)]).is_empty());
    }

    #[test]
    fn n_points_yield_n_minus_one_segments_through_the_knots() {
        let pts = [(0.0, 0.0), (1.0, 2.0), (3.0, 1.0), (4.0, 4.0)];
        let segs = hobby_spline(&pts);
        assert_eq!(segs.len(), pts.len() - 1);
        // The curve interpolates every input point: each segment starts/ends on a knot.
        for (i, seg) in segs.iter().enumerate() {
            assert!(close(seg.p0.0, pts[i].0) && close(seg.p0.1, pts[i].1), "seg {i} start");
            assert!(close(seg.p1.0, pts[i + 1].0) && close(seg.p1.1, pts[i + 1].1), "seg {i} end");
        }
    }

    #[test]
    fn bezier_point_endpoints_match_the_segment() {
        let seg = &hobby_spline(&[(0.0, 0.0), (5.0, 3.0)])[0];
        assert_eq!(bezier_point(seg, 0.0), seg.p0);
        assert_eq!(bezier_point(seg, 1.0), seg.p1);
    }

    #[test]
    fn collinear_points_stay_on_the_line() {
        // A straight horizontal run must produce a curve that never leaves y=0.
        let pts = [(0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (3.0, 0.0)];
        let segs = hobby_spline(&pts);
        for seg in &segs {
            for step in 0..=10 {
                let (_x, y) = bezier_point(seg, f64::from(step) / 10.0);
                assert!(y.abs() < 1e-9, "point left the line: y={y}");
            }
        }
        // ...and the tangent points straight along +x.
        let (dx, dy) = spline_direction_at(&segs, 0.5);
        assert!(dx > 0.99 && dy.abs() < 1e-6, "direction ({dx},{dy})");
    }

    #[test]
    fn direction_of_empty_spline_is_defined() {
        assert_eq!(spline_direction_at(&[], 0.5), (1.0, 0.0));
    }
}
