//! Shared geometry utilities.

/// Find where a line segment (from `inside` toward `outside`) first crosses a rectangle.
/// Coordinates are generic (lat/lon, x/y, lon/lat — caller decides).
/// Returns the intersection point closest to the inside point, or None.
#[must_use]
pub fn segment_rect_intersect(
    in_x: f64, in_y: f64, out_x: f64, out_y: f64,
    min_x: f64, min_y: f64, max_x: f64, max_y: f64,
) -> Option<(f64, f64)> {
    let dx = out_x - in_x;
    let dy = out_y - in_y;
    let mut best_t = f64::MAX;
    let mut best_pt = (0.0, 0.0);

    // min_y edge
    if dy.abs() > 1e-12 {
        let t = (min_y - in_y) / dy;
        if t > 0.0 && t < best_t {
            let x = in_x + t * dx;
            if x >= min_x && x <= max_x { best_t = t; best_pt = (x, min_y); }
        }
    }
    // max_y edge
    if dy.abs() > 1e-12 {
        let t = (max_y - in_y) / dy;
        if t > 0.0 && t < best_t {
            let x = in_x + t * dx;
            if x >= min_x && x <= max_x { best_t = t; best_pt = (x, max_y); }
        }
    }
    // min_x edge
    if dx.abs() > 1e-12 {
        let t = (min_x - in_x) / dx;
        if t > 0.0 && t < best_t {
            let y = in_y + t * dy;
            if y >= min_y && y <= max_y { best_t = t; best_pt = (min_x, y); }
        }
    }
    // max_x edge
    if dx.abs() > 1e-12 {
        let t = (max_x - in_x) / dx;
        if t > 0.0 && t < best_t {
            let y = in_y + t * dy;
            if y >= min_y && y <= max_y { best_t = t; best_pt = (max_x, y); }
        }
    }

    if best_t < f64::MAX { Some(best_pt) } else { None }
}
