/// Hobby spline implementation for point-mode track rendering.
/// Given a sequence of points, produces cubic Bézier control points.
///
/// Based on Jake Low's implementation (ISC license):
/// https://www.jakelow.com/blog/hobby-curves/hobby.js
/// Which implements the algorithm from:
/// Hobby, J.D., "Smooth, Easy to Compute Interpolating Splines", 1986

/// A cubic Bézier segment: start, control1, control2, end
pub struct BezierSegment {
    pub p0: (f64, f64),
    pub c0: (f64, f64),
    pub c1: (f64, f64),
    pub p1: (f64, f64),
}

/// Compute Hobby spline through a sequence of points.
/// omega (0.0-1.0) controls endpoint curl (0 = natural, 1 = straight).
/// Returns cubic Bézier segments connecting consecutive points.
pub fn hobby_spline(points: &[(f64, f64)], omega: f64) -> Vec<BezierSegment> {
    if points.len() < 2 {
        return Vec::new();
    }
    if points.len() == 2 {
        // Straight line
        let (x0, y0) = points[0];
        let (x1, y1) = points[1];
        let cx = (x0 + x1) / 2.0;
        let cy = (y0 + y1) / 2.0;
        return vec![BezierSegment {
            p0: points[0], c0: (cx, cy), c1: (cx, cy), p1: points[1],
        }];
    }

    let n = points.len() - 1;

    // Chords and their lengths
    let mut chords: Vec<(f64, f64)> = Vec::with_capacity(n);
    let mut d: Vec<f64> = Vec::with_capacity(n);
    for i in 0..n {
        let cx = points[i + 1].0 - points[i].0;
        let cy = points[i + 1].1 - points[i].1;
        let len = (cx * cx + cy * cy).sqrt();
        chords.push((cx, cy));
        d.push(len.max(1e-10)); // avoid zero-length
    }

    // Turning angles (gamma) at each interior point
    let mut gamma = vec![0.0f64; n + 1];
    for i in 1..n {
        gamma[i] = angle_between(chords[i - 1], chords[i]);
    }

    // Set up tridiagonal system (Jackowski formula 38)
    let mut a_diag = vec![0.0; n + 1];
    let mut b_diag = vec![0.0; n + 1];
    let mut c_diag = vec![0.0; n + 1];
    let mut d_rhs = vec![0.0; n + 1];

    b_diag[0] = 2.0 + omega;
    c_diag[0] = 2.0 * omega + 1.0;
    d_rhs[0] = -c_diag[0] * gamma[1];

    for i in 1..n {
        a_diag[i] = 1.0 / d[i - 1];
        b_diag[i] = (2.0 * d[i - 1] + 2.0 * d[i]) / (d[i - 1] * d[i]);
        c_diag[i] = 1.0 / d[i];
        d_rhs[i] = -(2.0 * gamma[i] * d[i] + gamma[i + 1] * d[i - 1]) / (d[i - 1] * d[i]);
    }

    a_diag[n] = 2.0 * omega + 1.0;
    b_diag[n] = 2.0 + omega;
    d_rhs[n] = 0.0;

    // Solve with Thomas algorithm → alpha angles
    let alpha = thomas(&a_diag, &b_diag, &c_diag, &d_rhs);

    // Compute beta angles
    let mut beta = vec![0.0; n];
    for i in 0..n - 1 {
        beta[i] = -gamma[i + 1] - alpha[i + 1];
    }
    beta[n - 1] = -alpha[n];

    // Compute Bézier control points
    let mut segments = Vec::with_capacity(n);
    for i in 0..n {
        let a_len = rho(alpha[i], beta[i]) * d[i] / 3.0;
        let b_len = rho(beta[i], alpha[i]) * d[i] / 3.0;

        let chord_norm = normalize(chords[i]);
        let c0_dir = rotate(chord_norm, alpha[i]);
        let c1_dir = rotate(chord_norm, -beta[i]);

        let c0 = (
            points[i].0 + c0_dir.0 * a_len,
            points[i].1 + c0_dir.1 * a_len,
        );
        let c1 = (
            points[i + 1].0 - c1_dir.0 * b_len,
            points[i + 1].1 - c1_dir.1 * b_len,
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

/// Velocity function (Jackowski formula 28)
fn rho(alpha: f64, beta: f64) -> f64 {
    let c = 2.0 / 3.0;
    2.0 / (1.0 + c * beta.cos() + (1.0 - c) * alpha.cos())
}

/// Signed angle between two 2D vectors
fn angle_between(a: (f64, f64), b: (f64, f64)) -> f64 {
    let cross = a.0 * b.1 - a.1 * b.0;
    let dot = a.0 * b.0 + a.1 * b.1;
    cross.atan2(dot)
}

fn normalize(v: (f64, f64)) -> (f64, f64) {
    let len = (v.0 * v.0 + v.1 * v.1).sqrt().max(1e-10);
    (v.0 / len, v.1 / len)
}

fn rotate(v: (f64, f64), angle: f64) -> (f64, f64) {
    let (s, c) = angle.sin_cos();
    (v.0 * c - v.1 * s, v.0 * s + v.1 * c)
}

/// Thomas algorithm for tridiagonal matrix
fn thomas(a: &[f64], b: &[f64], c: &[f64], d: &[f64]) -> Vec<f64> {
    let n = b.len() - 1;
    let mut cp = vec![0.0; n + 1];
    let mut dp = vec![0.0; n + 1];

    cp[0] = c[0] / b[0];
    dp[0] = d[0] / b[0];

    for i in 1..=n {
        let denom = b[i] - cp[i - 1] * a[i];
        cp[i] = c[i] / denom;
        dp[i] = (d[i] - dp[i - 1] * a[i]) / denom;
    }

    let mut x = vec![0.0; n + 1];
    x[n] = dp[n];
    for i in (0..n).rev() {
        x[i] = dp[i] - cp[i] * x[i + 1];
    }
    x
}

/// Sample a cubic Bézier at parameter t ∈ [0, 1]
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
