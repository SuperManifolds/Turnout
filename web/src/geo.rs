//! Pure geometry helpers for the area-selection bounding box: the `GeoJSON` the map
//! renders, resize-handle placement, and handle-drag math. No UI or map FFI here —
//! the map component feeds these values into the render bridge.

/// A resize handle on the selection box, named by compass direction.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    NW, N, NE, E, SE, S, SW, W,
}

impl Handle {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "nw" => Some(Self::NW), "n" => Some(Self::N), "ne" => Some(Self::NE),
            "e" => Some(Self::E), "se" => Some(Self::SE), "s" => Some(Self::S),
            "sw" => Some(Self::SW), "w" => Some(Self::W), _ => None,
        }
    }

    /// The CSS cursor for dragging this handle.
    pub fn cursor(self) -> &'static str {
        match self {
            Self::NW | Self::SE => "nwse-resize",
            Self::NE | Self::SW => "nesw-resize",
            Self::N | Self::S => "ns-resize",
            Self::E | Self::W => "ew-resize",
        }
    }
}

/// A `Polygon` feature for the selection box outline/fill.
pub fn bbox_geojson(w: f64, s: f64, e: f64, n: f64) -> String {
    format!(
        r#"{{"type":"Feature","geometry":{{"type":"Polygon","coordinates":[[[{w},{s}],[{e},{s}],[{e},{n}],[{w},{n}],[{w},{s}]]]}}}}"#
    )
}

/// A `FeatureCollection` of the eight resize handles, each tagged with its
/// `handle` direction so a click can resolve which one was grabbed.
pub fn handles_geojson(w: f64, s: f64, e: f64, n: f64) -> String {
    let mx = (w + e) / 2.0;
    let my = (s + n) / 2.0;
    let pts = [
        (w, n, "nw"), (mx, n, "n"), (e, n, "ne"), (e, my, "e"),
        (e, s, "se"), (mx, s, "s"), (w, s, "sw"), (w, my, "w"),
    ];
    let features: Vec<String> = pts.iter().map(|(lng, lat, h)| {
        format!(
            r#"{{"type":"Feature","properties":{{"handle":"{h}"}},"geometry":{{"type":"Point","coordinates":[{lng},{lat}]}}}}"#
        )
    }).collect();
    format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))
}

/// An empty `FeatureCollection`, used to clear a rendered source.
pub fn empty_geojson() -> &'static str {
    r#"{"type":"FeatureCollection","features":[]}"#
}

/// Move the dragged handle to `(lng, lat)` and return the normalized
/// `(south, west, north, east)` box, keeping min < max on each axis.
pub fn apply_handle_drag(handle: Handle, bbox: (f64, f64, f64, f64), lng: f64, lat: f64) -> (f64, f64, f64, f64) {
    let (mut s, mut w, mut n, mut e) = bbox;
    match handle {
        Handle::NW => { n = lat; w = lng; }
        Handle::N  => { n = lat; }
        Handle::NE => { n = lat; e = lng; }
        Handle::E  => { e = lng; }
        Handle::SE => { s = lat; e = lng; }
        Handle::S  => { s = lat; }
        Handle::SW => { s = lat; w = lng; }
        Handle::W  => { w = lng; }
    }
    if s > n { std::mem::swap(&mut s, &mut n); }
    if w > e { std::mem::swap(&mut w, &mut e); }
    (s, w, n, e)
}

/// The status-bar label for a selected box.
pub fn format_bbox(s: f64, w: f64, n: f64, e: f64) -> String {
    format!("Selected: {s:.4}°, {w:.4}° to {n:.4}°, {e:.4}°")
}
