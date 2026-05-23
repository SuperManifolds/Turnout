//! Track curve computation matching Nimby Rails' actual algorithm.
//! NOT standard Hobby splines — uses local tangent computation
//! (weighted bisector) instead of global tridiagonal solver.
//!
//! Reverse engineered from NIMBYRails.exe:
//!   Tangent: RVA 0xBA490/0xBA5C0 (local bisector)
//!   Rho:     RVA 0xBD8E0 (golden-ratio based velocity function)

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
pub fn hobby_spline(points: &[(f64, f64)], _omega: f64) -> Vec<BezierSegment> {
    if points.len() < 2 {
        return Vec::new();
    }
    if points.len() == 2 {
        // Straight line — control points at 1/3 and 2/3
        let (x0, y0) = points[0];
        let (x1, y1) = points[1];
        return vec![BezierSegment {
            p0: (x0, y0),
            c0: (x0 + (x1 - x0) / 3.0, y0 + (y1 - y0) / 3.0),
            c1: (x0 + 2.0 * (x1 - x0) / 3.0, y0 + 2.0 * (y1 - y0) / 3.0),
            p1: (x1, y1),
        }];
    }

    let n = points.len();

    // Compute chord vectors and lengths
    let mut chords: Vec<(f64, f64)> = Vec::with_capacity(n - 1);
    let mut chord_lens: Vec<f64> = Vec::with_capacity(n - 1);
    for i in 0..n - 1 {
        let dx = points[i + 1].0 - points[i].0;
        let dy = points[i + 1].1 - points[i].1;
        let len = (dx * dx + dy * dy).sqrt().max(1e-10);
        chords.push((dx, dy));
        chord_lens.push(len);
    }

    // Compute tangent angle at each node using weighted bisector
    let mut tangent_angles: Vec<f64> = Vec::with_capacity(n);
    for i in 0..n {
        if i == 0 {
            // First node: tangent = chord direction
            tangent_angles.push(chords[0].1.atan2(chords[0].0));
        } else if i == n - 1 {
            // Last node: tangent = last chord direction
            tangent_angles.push(chords[n - 2].1.atan2(chords[n - 2].0));
        } else {
            // Interior node: bisector of incoming/outgoing chord directions
            // (matching game at RVA 0xBA5C0 — direction is angle midpoint,
            // the 1/(2*cos(angle/2)) weight only affects magnitude)
            let in_angle = chords[i - 1].1.atan2(chords[i - 1].0);
            let out_angle = chords[i].1.atan2(chords[i].0);
            let mut turn = out_angle - in_angle;
            while turn > std::f64::consts::PI { turn -= 2.0 * std::f64::consts::PI; }
            while turn < -std::f64::consts::PI { turn += 2.0 * std::f64::consts::PI; }
            tangent_angles.push(in_angle + turn / 2.0);
        }
    }

    // Generate Bézier segments
    let delta = (3.0 - 5.0_f64.sqrt()) / 2.0; // ≈ 0.38197

    let mut segments = Vec::with_capacity(n - 1);
    for i in 0..n - 1 {
        let chord_angle = chords[i].1.atan2(chords[i].0);
        let d = chord_lens[i];

        // Alpha: angle from chord to tangent at start node
        let alpha = normalize_angle(tangent_angles[i] - chord_angle);
        // Beta: angle from chord to tangent at end node (negated)
        let beta = normalize_angle(chord_angle - tangent_angles[i + 1]);

        // Hobby rho velocity function (game's version with delta = (3-√5)/2)
        let (rho_a, rho_b) = rho(alpha, beta, delta);

        // Control points
        let c0_dir = rotate_unit(chord_angle + alpha);
        let c1_dir = rotate_unit(chord_angle - beta);

        let c0 = (
            points[i].0 + rho_a * d * c0_dir.0,
            points[i].1 + rho_a * d * c0_dir.1,
        );
        let c1 = (
            points[i + 1].0 - rho_b * d * c1_dir.0,
            points[i + 1].1 - rho_b * d * c1_dir.1,
        );

        segments.push(BezierSegment {
            p0: points[i],
            c0,
            c1,
            p1: points[i + 1],
        });
    }

    segments
}

/// Game's rho velocity function (RVA 0xBD8E0).
/// Uses delta = (3-√5)/2 ≈ 0.38197 (golden ratio related).
fn rho(alpha: f64, beta: f64, delta: f64) -> (f64, f64) {
    let sa = alpha.sin();
    let sb = beta.sin();
    let ca = alpha.cos();
    let cb = beta.cos();

    let a = sa - sb / 16.0;
    let b = sb - sa / 16.0;
    let c = ca - cb;
    let f = std::f64::consts::SQRT_2 * a * b * c;

    let denom_a = 3.0 * (1.0 + (1.0 - delta) * ca + delta * cb);
    let denom_b = 3.0 * (1.0 + (1.0 - delta) * cb + delta * ca);

    let rho_a = if denom_a.abs() > 1e-10 { (2.0 + f) / denom_a } else { 1.0 / 3.0 };
    let rho_b = if denom_b.abs() > 1e-10 { (2.0 - f) / denom_b } else { 1.0 / 3.0 };

    (rho_a.max(0.0), rho_b.max(0.0))
}

fn normalize_angle(mut a: f64) -> f64 {
    while a > std::f64::consts::PI { a -= 2.0 * std::f64::consts::PI; }
    while a < -std::f64::consts::PI { a += 2.0 * std::f64::consts::PI; }
    a
}

fn rotate_unit(angle: f64) -> (f64, f64) {
    (angle.cos(), angle.sin())
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
    let len = (dx * dx + dy * dy).sqrt().max(1e-10);
    (dx / len, dy / len)
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
        let (x0, y0) = points[0];
        let (x1, y1) = points[1];
        let angle = match start_tangent {
            Some(a) => a,
            None => (y1 - y0).atan2(x1 - x0),
        };
        let d = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt() / 3.0;
        let (sa, ca) = angle.sin_cos();
        return vec![BezierSegment {
            p0: (x0, y0),
            c0: (x0 + ca * d, y0 + sa * d),
            c1: (x1 - ca * d, y1 - sa * d),
            p1: (x1, y1),
        }];
    }

    let n = points.len();
    let mut chords: Vec<(f64, f64)> = Vec::with_capacity(n - 1);
    let mut chord_lens: Vec<f64> = Vec::with_capacity(n - 1);
    for i in 0..n - 1 {
        let dx = points[i + 1].0 - points[i].0;
        let dy = points[i + 1].1 - points[i].1;
        let len = (dx * dx + dy * dy).sqrt().max(1e-10);
        chords.push((dx, dy));
        chord_lens.push(len);
    }

    let mut tangent_angles: Vec<f64> = Vec::with_capacity(n);
    for i in 0..n {
        if i == 0 {
            tangent_angles.push(start_tangent.unwrap_or_else(|| chords[0].1.atan2(chords[0].0)));
        } else if i == n - 1 {
            tangent_angles.push(end_tangent.unwrap_or_else(|| chords[n - 2].1.atan2(chords[n - 2].0)));
        } else {
            let in_angle = chords[i - 1].1.atan2(chords[i - 1].0);
            let out_angle = chords[i].1.atan2(chords[i].0);
            let mut turn = out_angle - in_angle;
            while turn > std::f64::consts::PI { turn -= 2.0 * std::f64::consts::PI; }
            while turn < -std::f64::consts::PI { turn += 2.0 * std::f64::consts::PI; }
            tangent_angles.push(in_angle + turn / 2.0);
        }
    }

    let delta = (3.0 - 5.0_f64.sqrt()) / 2.0;
    let mut segments = Vec::with_capacity(n - 1);
    for i in 0..n - 1 {
        let chord_angle = chords[i].1.atan2(chords[i].0);
        let d = chord_lens[i];
        let alpha = normalize_angle(tangent_angles[i] - chord_angle);
        let beta = normalize_angle(chord_angle - tangent_angles[i + 1]);
        let (rho_a, rho_b) = rho(alpha, beta, delta);
        let c0_dir = rotate_unit(chord_angle + alpha);
        let c1_dir = rotate_unit(chord_angle - beta);
        segments.push(BezierSegment {
            p0: points[i],
            c0: (points[i].0 + rho_a * d * c0_dir.0, points[i].1 + rho_a * d * c0_dir.1),
            c1: (points[i+1].0 - rho_b * d * c1_dir.0, points[i+1].1 - rho_b * d * c1_dir.1),
            p1: points[i + 1],
        });
    }
    segments
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
