//! Shared geometry and coordinate utilities.

const EARTH_RADIUS: f64 = 6_378_137.0;

/// Convert lat/lon (degrees) to Web Mercator (EPSG:3857) meters.
#[must_use]
pub fn latlon_to_mercator(lat: f64, lon: f64) -> (f64, f64) {
    let x = lon.to_radians() * EARTH_RADIUS;
    let y = (lat.to_radians() / 2.0 + std::f64::consts::FRAC_PI_4).tan().ln() * EARTH_RADIUS;
    (x, y)
}

/// Convert Web Mercator meters to lat/lon (degrees).
#[must_use]
pub fn mercator_to_latlon(x: f64, y: f64) -> (f64, f64) {
    let lat = (y / EARTH_RADIUS).sinh().atan().to_degrees();
    let lon = (x / EARTH_RADIUS).to_degrees();
    (lat, lon)
}

/// Inverse Mercator Y to latitude in radians.
#[must_use]
pub fn merc_y_to_lat_rad(merc_y: f64) -> f64 {
    (merc_y / EARTH_RADIUS).sinh().atan()
}

/// Compute the ground-meter offset `(dx, dy)` from center to node such that the
/// game's spherical great-circle destination formula reconstructs the correct
/// position. `dx = dist * sin(bearing)`, `dy = dist * cos(bearing)`.
/// All inputs in radians.
#[must_use]
pub fn inverse_geodesic(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> (f64, f64) {
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;

    // Spherical distance (haversine)
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    let dist = EARTH_RADIUS * c;

    if dist < 0.001 {
        return (0.0, 0.0);
    }

    // Forward bearing from center to node
    let y_comp = lat2.cos() * dlon.sin();
    let x_comp = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
    let bearing = y_comp.atan2(x_comp);

    (dist * bearing.sin(), dist * bearing.cos())
}

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
