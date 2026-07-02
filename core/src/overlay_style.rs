use crate::kml::{Geometry, Style};

#[must_use]
pub fn parse_css_color(s: &str) -> Option<[u8; 4]> {
    let s = s.strip_prefix('#')?;
    match s.len() {
        3 => {
            let r = u8::from_str_radix(&s[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&s[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&s[2..3], 16).ok()? * 17;
            Some([r, g, b, 255])
        }
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some([r, g, b, 255])
        }
        8 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            let a = u8::from_str_radix(&s[6..8], 16).ok()?;
            Some([r, g, b, a])
        }
        _ => None,
    }
}

pub struct DefaultColors {
    pub line: [u8; 4],
    pub fill: [u8; 4],
    pub point: [u8; 4],
    pub line_width: f32,
    pub polygon_outline_width: f32,
}

#[must_use]
pub fn default_style_id(prefix: &str, geom: &Geometry) -> String {
    let suffix = match geom {
        Geometry::Point { .. } => "point",
        Geometry::LineString { .. } => "line",
        Geometry::Polygon { .. } => "polygon",
        Geometry::Multi(gs) => return gs.first().map_or(format!("{prefix}_default_line"), |g| default_style_id(prefix, g)),
    };
    format!("{prefix}_default_{suffix}")
}

#[must_use]
pub fn default_style_for(geom: &Geometry, colors: &DefaultColors) -> Style {
    match geom {
        Geometry::Point { .. } => Style {
            line_color: Some(colors.point),
            line_width: Some(colors.line_width),
            ..Style::default()
        },
        Geometry::LineString { .. } => Style {
            line_color: Some(colors.line),
            line_width: Some(colors.line_width),
            ..Style::default()
        },
        Geometry::Polygon { .. } => Style {
            fill_color: Some(colors.fill),
            line_color: Some(colors.line),
            line_width: Some(colors.polygon_outline_width),
            poly_fill: Some(true),
            poly_outline: Some(true),
        },
        Geometry::Multi(gs) => gs.first().map_or(Style::default(), |g| default_style_for(g, colors)),
    }
}
