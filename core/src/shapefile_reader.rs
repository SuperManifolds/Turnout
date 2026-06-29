use anyhow::{Context, Result};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::HashMap;
use std::path::Path;

use crate::geo::mercator_to_latlon;
use crate::kml::{Geometry, KmzData, Placemark, Style};

const DEFAULT_LINE_COLOR: [u8; 4] = [255, 140, 0, 200];
const DEFAULT_FILL_COLOR: [u8; 4] = [70, 130, 180, 100];
const DEFAULT_POINT_COLOR: [u8; 4] = [220, 50, 50, 220];
const DEFAULT_LINE_WIDTH: f32 = 2.0;

struct StyleRule {
    filter: Option<(String, String)>,
    style: Style,
}

enum Crs {
    Wgs84,
    WebMercator,
}

pub fn parse_shapefile(shp_path: &Path) -> Result<KmzData> {
    let dir = shp_path.parent().unwrap_or(Path::new("."));
    let stem = shp_path.file_stem().context("no file stem")?.to_string_lossy();

    let crs = detect_crs(&dir.join(format!("{stem}.prj")));
    let rules = load_style_rules(dir, &stem);

    let mut reader = shapefile::Reader::from_path(shp_path)
        .with_context(|| format!("failed to open {}", shp_path.display()))?;

    let mut placemarks = Vec::new();
    let mut styles: HashMap<String, Style> = HashMap::new();

    for (i, result) in reader.iter_shapes_and_records().enumerate() {
        let (shape, record) = result.with_context(|| format!("record {i}"))?;

        let geometry = shape_to_geometry(&shape, &crs);
        let Some(geometry) = geometry else { continue };

        let name = first_text_field(&record);

        let style_id = if rules.is_empty() {
            let default_id = default_style_id(&geometry);
            if !styles.contains_key(&default_id) {
                styles.insert(default_id.clone(), default_style_for(&geometry));
            }
            default_id
        } else {
            let matched = match_style(&rules, &record);
            let id = format!("_shp_{i}");
            styles.insert(id.clone(), matched);
            id
        };

        placemarks.push(Placemark {
            name,
            style_url: Some(format!("#{style_id}")),
            geometry,
        });
    }

    Ok(KmzData {
        name: Some(stem.to_string()),
        placemarks,
        ground_overlays: Vec::new(),
        styles,
        images: HashMap::new(),
    })
}

fn detect_crs(prj_path: &Path) -> Crs {
    let Ok(wkt) = std::fs::read_to_string(prj_path) else {
        return Crs::Wgs84;
    };
    let upper = wkt.to_uppercase();
    if upper.contains("WEB_MERCATOR")
        || upper.contains("PSEUDO_MERCATOR")
        || upper.contains("3857")
        || upper.contains("900913")
        || upper.contains("102100")
    {
        Crs::WebMercator
    } else {
        Crs::Wgs84
    }
}

fn project(x: f64, y: f64, crs: &Crs) -> (f64, f64) {
    match crs {
        Crs::Wgs84 => (x, y),
        Crs::WebMercator => {
            let (lat, lon) = mercator_to_latlon(x, y);
            (lon, lat)
        }
    }
}

fn shape_to_geometry(shape: &shapefile::Shape, crs: &Crs) -> Option<Geometry> {
    match shape {
        shapefile::Shape::Point(p) => {
            let (lon, lat) = project(p.x, p.y, crs);
            Some(Geometry::Point { lon, lat })
        }
        shapefile::Shape::PointZ(p) => {
            let (lon, lat) = project(p.x, p.y, crs);
            Some(Geometry::Point { lon, lat })
        }
        shapefile::Shape::PointM(p) => {
            let (lon, lat) = project(p.x, p.y, crs);
            Some(Geometry::Point { lon, lat })
        }
        shapefile::Shape::Polyline(pl) => parts_to_lines(pl.parts(), crs),
        shapefile::Shape::PolylineZ(pl) => partsz_to_lines(pl.parts(), crs),
        shapefile::Shape::PolylineM(pl) => partsm_to_lines(pl.parts(), crs),
        shapefile::Shape::Polygon(pg) => rings_to_polygon(pg.rings(), crs),
        shapefile::Shape::PolygonZ(pg) => ringsz_to_polygon(pg.rings(), crs),
        shapefile::Shape::PolygonM(pg) => ringsm_to_polygon(pg.rings(), crs),
        shapefile::Shape::Multipoint(mp) => {
            let points: Vec<Geometry> = mp.points().iter().map(|p| {
                let (lon, lat) = project(p.x, p.y, crs);
                Geometry::Point { lon, lat }
            }).collect();
            if points.is_empty() { None } else { Some(Geometry::Multi(points)) }
        }
        shapefile::Shape::MultipointZ(mp) => {
            let points: Vec<Geometry> = mp.points().iter().map(|p| {
                let (lon, lat) = project(p.x, p.y, crs);
                Geometry::Point { lon, lat }
            }).collect();
            if points.is_empty() { None } else { Some(Geometry::Multi(points)) }
        }
        _ => None,
    }
}

fn points_to_coords(points: &[shapefile::Point], crs: &Crs) -> Vec<(f64, f64)> {
    points.iter().map(|p| project(p.x, p.y, crs)).collect()
}

fn parts_to_lines(parts: &[Vec<shapefile::Point>], crs: &Crs) -> Option<Geometry> {
    let lines: Vec<Geometry> = parts
        .iter()
        .filter(|part| part.len() >= 2)
        .map(|part| Geometry::LineString { coords: points_to_coords(part, crs) })
        .collect();
    match lines.len() {
        0 => None,
        1 => Some(lines.into_iter().next().expect("checked")),
        _ => Some(Geometry::Multi(lines)),
    }
}

fn partsz_to_lines(parts: &[Vec<shapefile::PointZ>], crs: &Crs) -> Option<Geometry> {
    let lines: Vec<Geometry> = parts
        .iter()
        .filter(|part| part.len() >= 2)
        .map(|part| Geometry::LineString {
            coords: part.iter().map(|p| project(p.x, p.y, crs)).collect(),
        })
        .collect();
    match lines.len() {
        0 => None,
        1 => Some(lines.into_iter().next().expect("checked")),
        _ => Some(Geometry::Multi(lines)),
    }
}

fn partsm_to_lines(parts: &[Vec<shapefile::PointM>], crs: &Crs) -> Option<Geometry> {
    let lines: Vec<Geometry> = parts
        .iter()
        .filter(|part| part.len() >= 2)
        .map(|part| Geometry::LineString {
            coords: part.iter().map(|p| project(p.x, p.y, crs)).collect(),
        })
        .collect();
    match lines.len() {
        0 => None,
        1 => Some(lines.into_iter().next().expect("checked")),
        _ => Some(Geometry::Multi(lines)),
    }
}

trait HasXY { fn x(&self) -> f64; fn y(&self) -> f64; }
impl HasXY for shapefile::Point { fn x(&self) -> f64 { self.x } fn y(&self) -> f64 { self.y } }
impl HasXY for shapefile::PointZ { fn x(&self) -> f64 { self.x } fn y(&self) -> f64 { self.y } }
impl HasXY for shapefile::PointM { fn x(&self) -> f64 { self.x } fn y(&self) -> f64 { self.y } }

fn generic_rings_to_polygon<P: HasXY>(rings: &[shapefile::PolygonRing<P>], crs: &Crs) -> Option<Geometry> {
    let mut outers: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut inners: Vec<Vec<(f64, f64)>> = Vec::new();
    for ring in rings {
        let coords: Vec<(f64, f64)> = ring.points().iter().map(|p| project(p.x(), p.y(), crs)).collect();
        match ring {
            shapefile::PolygonRing::Outer(_) => outers.push(coords),
            shapefile::PolygonRing::Inner(_) => inners.push(coords),
        }
    }
    if outers.is_empty() {
        return None;
    }
    if outers.len() == 1 {
        return Some(Geometry::Polygon { outer: outers.remove(0), inner: inners });
    }
    Some(Geometry::Multi(
        outers.into_iter().map(|outer| Geometry::Polygon { outer, inner: Vec::new() }).collect(),
    ))
}

fn rings_to_polygon(rings: &[shapefile::PolygonRing<shapefile::Point>], crs: &Crs) -> Option<Geometry> {
    generic_rings_to_polygon(rings, crs)
}

fn ringsz_to_polygon(rings: &[shapefile::PolygonRing<shapefile::PointZ>], crs: &Crs) -> Option<Geometry> {
    generic_rings_to_polygon(rings, crs)
}

fn ringsm_to_polygon(rings: &[shapefile::PolygonRing<shapefile::PointM>], crs: &Crs) -> Option<Geometry> {
    generic_rings_to_polygon(rings, crs)
}

fn first_text_field(record: &shapefile::dbase::Record) -> Option<String> {
    for value in record.as_ref().values() {
        if let shapefile::dbase::FieldValue::Character(Some(s)) = value {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn default_style_id(geom: &Geometry) -> String {
    match geom {
        Geometry::Point { .. } => "_shp_default_point".to_string(),
        Geometry::LineString { .. } => "_shp_default_line".to_string(),
        Geometry::Polygon { .. } => "_shp_default_polygon".to_string(),
        Geometry::Multi(gs) => gs.first().map_or("_shp_default_line".to_string(), default_style_id),
    }
}

fn default_style_for(geom: &Geometry) -> Style {
    match geom {
        Geometry::Point { .. } => Style {
            line_color: Some(DEFAULT_POINT_COLOR),
            line_width: Some(DEFAULT_LINE_WIDTH),
            ..Style::default()
        },
        Geometry::LineString { .. } => Style {
            line_color: Some(DEFAULT_LINE_COLOR),
            line_width: Some(DEFAULT_LINE_WIDTH),
            ..Style::default()
        },
        Geometry::Polygon { .. } => Style {
            fill_color: Some(DEFAULT_FILL_COLOR),
            line_color: Some(DEFAULT_LINE_COLOR),
            line_width: Some(1.0),
            poly_fill: Some(true),
            poly_outline: Some(true),
        },
        Geometry::Multi(gs) => gs.first().map_or(Style::default(), default_style_for),
    }
}

fn match_style(rules: &[StyleRule], record: &shapefile::dbase::Record) -> Style {
    for rule in rules {
        if let Some((prop, value)) = &rule.filter
            && let Some(field_val) = record_field_as_string(record, prop)
            && field_val == *value
        {
            return rule.style.clone();
        }
    }
    rules
        .iter()
        .rev()
        .find(|r| r.filter.is_none())
        .map_or(Style::default(), |r| r.style.clone())
}

fn record_field_as_string(record: &shapefile::dbase::Record, field_name: &str) -> Option<String> {
    let value = record.get(field_name)?;
    match value {
        shapefile::dbase::FieldValue::Character(Some(s)) => Some(s.trim().to_string()),
        shapefile::dbase::FieldValue::Numeric(Some(n)) => Some(n.to_string()),
        shapefile::dbase::FieldValue::Integer(n) => Some(n.to_string()),
        shapefile::dbase::FieldValue::Float(Some(n)) => Some(n.to_string()),
        _ => None,
    }
}

// --- Style file loading ---

fn load_style_rules(dir: &Path, stem: &str) -> Vec<StyleRule> {
    let sld_path = dir.join(format!("{stem}.sld"));
    if sld_path.exists()
        && let Ok(xml) = std::fs::read_to_string(&sld_path)
    {
        let rules = parse_sld(&xml);
        if !rules.is_empty() {
            return rules;
        }
    }

    let qml_path = dir.join(format!("{stem}.qml"));
    if qml_path.exists()
        && let Ok(xml) = std::fs::read_to_string(&qml_path)
    {
        let rules = parse_qml(&xml);
        if !rules.is_empty() {
            return rules;
        }
    }

    Vec::new()
}

// --- SLD parsing ---

fn parse_sld(xml: &str) -> Vec<StyleRule> {
    let mut reader = Reader::from_str(xml);
    let mut rules = Vec::new();
    let mut text_buf = String::new();

    let mut in_rule = false;
    let mut in_filter = false;
    let mut in_stroke = false;
    let mut in_fill = false;
    let mut cur_prop_name: Option<String> = None;
    let mut cur_literal: Option<String> = None;
    let mut cur_style = Style::default();
    let mut cur_css_param: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if tag == "Rule" {
                    in_rule = true;
                    cur_style = Style::default();
                    cur_prop_name = None;
                    cur_literal = None;
                } else if tag == "Filter" {
                    in_filter = true;
                } else if tag == "Stroke" {
                    in_stroke = true;
                } else if tag == "Fill" && in_rule {
                    in_fill = true;
                } else if tag == "CssParameter" || tag == "SvgParameter" {
                    cur_css_param = e.attributes().filter_map(Result::ok).find_map(|a| {
                        if a.key.as_ref() == b"name" {
                            Some(String::from_utf8_lossy(&a.value).to_string())
                        } else {
                            None
                        }
                    });
                }
                text_buf.clear();
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                let text = text_buf.trim().to_string();

                match tag.as_str() {
                    "PropertyName" if in_filter => cur_prop_name = Some(text),
                    "Literal" if in_filter => cur_literal = Some(text),
                    "Filter" => in_filter = false,
                    "Stroke" => in_stroke = false,
                    "Fill" => in_fill = false,
                    "CssParameter" | "SvgParameter" => {
                        if let Some(ref param) = cur_css_param {
                            apply_css_param(&mut cur_style, param, &text, in_stroke, in_fill);
                        }
                        cur_css_param = None;
                    }
                    "Rule" => {
                        let filter = match (&cur_prop_name, &cur_literal) {
                            (Some(p), Some(v)) => Some((p.clone(), v.clone())),
                            _ => None,
                        };
                        rules.push(StyleRule { filter, style: cur_style.clone() });
                        in_rule = false;
                    }
                    _ => {}
                }
                text_buf.clear();
            }
            Ok(Event::Text(ref e)) => {
                if let Ok(s) = e.unescape() {
                    text_buf.push_str(&s);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    rules
}

fn apply_css_param(style: &mut Style, name: &str, value: &str, in_stroke: bool, in_fill: bool) {
    match name {
        "stroke" if in_stroke => style.line_color = parse_css_color(value),
        "stroke-width" if in_stroke => style.line_width = value.parse().ok(),
        "fill" if in_fill => {
            style.fill_color = parse_css_color(value);
            style.poly_fill = Some(true);
        }
        "fill-opacity" if in_fill => {
            if let (Some(c), Ok(opacity)) = (&mut style.fill_color, value.parse::<f32>()) {
                c[3] = (opacity * 255.0) as u8;
            }
        }
        "stroke-opacity" if in_stroke => {
            if let (Some(c), Ok(opacity)) = (&mut style.line_color, value.parse::<f32>()) {
                c[3] = (opacity * 255.0) as u8;
            }
        }
        _ => {}
    }
}

// --- QML parsing ---

fn parse_qml(xml: &str) -> Vec<StyleRule> {
    let mut reader = Reader::from_str(xml);
    let mut text_buf = String::new();

    let mut attr_name: Option<String> = None;
    let mut renderer_type: Option<String> = None;
    let mut categories: Vec<(String, String)> = Vec::new();
    let mut symbols: HashMap<String, Style> = HashMap::new();

    let mut cur_symbol_name: Option<String> = None;
    let mut cur_symbol_style = Style::default();
    let mut in_symbol = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e) | Event::Empty(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                let attrs: HashMap<String, String> = e
                    .attributes()
                    .filter_map(Result::ok)
                    .map(|a| (
                        String::from_utf8_lossy(a.key.as_ref()).to_string(),
                        String::from_utf8_lossy(&a.value).to_string(),
                    ))
                    .collect();

                if tag == "renderer-v2" {
                    renderer_type = attrs.get("type").cloned();
                    attr_name = attrs.get("attr").cloned();
                } else if tag == "category" {
                    if let (Some(value), Some(symbol)) = (attrs.get("value"), attrs.get("symbol")) {
                        categories.push((value.clone(), symbol.clone()));
                    }
                } else if tag == "symbol" {
                    in_symbol = true;
                    cur_symbol_name = attrs.get("name").cloned();
                    cur_symbol_style = Style::default();
                } else if tag == "prop" && in_symbol
                    && let (Some(k), Some(v)) = (attrs.get("k"), attrs.get("v"))
                {
                    apply_qml_prop(&mut cur_symbol_style, k, v);
                }

                text_buf.clear();
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if tag == "symbol" {
                    if let Some(ref name) = cur_symbol_name {
                        symbols.insert(name.clone(), cur_symbol_style.clone());
                    }
                    in_symbol = false;
                }
                text_buf.clear();
            }
            Ok(Event::Text(ref e)) => {
                if let Ok(s) = e.unescape() {
                    text_buf.push_str(&s);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    match renderer_type.as_deref() {
        Some("singleSymbol") => {
            symbols.values().next().map_or(Vec::new(), |s| {
                vec![StyleRule { filter: None, style: s.clone() }]
            })
        }
        Some("categorizedSymbol") => {
            let attr = attr_name.unwrap_or_default();
            categories
                .iter()
                .filter_map(|(value, sym_name)| {
                    let style = symbols.get(sym_name)?;
                    Some(StyleRule {
                        filter: Some((attr.clone(), value.clone())),
                        style: style.clone(),
                    })
                })
                .collect()
        }
        _ => {
            symbols.values().next().map_or(Vec::new(), |s| {
                vec![StyleRule { filter: None, style: s.clone() }]
            })
        }
    }
}

fn apply_qml_prop(style: &mut Style, key: &str, value: &str) {
    match key {
        "line_color" | "outline_color" => style.line_color = parse_qml_color(value),
        "color" => {
            style.fill_color = parse_qml_color(value);
            style.poly_fill = Some(true);
        }
        "line_width" | "outline_width" => style.line_width = value.parse().ok(),
        _ => {}
    }
}

// --- Color parsing ---

fn parse_css_color(s: &str) -> Option<[u8; 4]> {
    let s = s.strip_prefix('#')?;
    match s.len() {
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

fn parse_qml_color(s: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = s.split(',').collect();
    match parts.len() {
        3 => Some([
            parts[0].trim().parse().ok()?,
            parts[1].trim().parse().ok()?,
            parts[2].trim().parse().ok()?,
            255,
        ]),
        4 => Some([
            parts[0].trim().parse().ok()?,
            parts[1].trim().parse().ok()?,
            parts[2].trim().parse().ok()?,
            parts[3].trim().parse().ok()?,
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_css_color() {
        assert_eq!(parse_css_color("#ff0000"), Some([255, 0, 0, 255]));
        assert_eq!(parse_css_color("#00ff0080"), Some([0, 255, 0, 128]));
        assert_eq!(parse_css_color("invalid"), None);
    }

    #[test]
    fn test_parse_qml_color() {
        assert_eq!(parse_qml_color("255,0,0,128"), Some([255, 0, 0, 128]));
        assert_eq!(parse_qml_color("0, 255, 0"), Some([0, 255, 0, 255]));
    }

    #[test]
    fn test_parse_sld() {
        let sld = r#"<?xml version="1.0" encoding="UTF-8"?>
<StyledLayerDescriptor version="1.0.0">
  <NamedLayer><UserStyle><FeatureTypeStyle>
    <Rule>
      <Filter><PropertyIsEqualTo>
        <PropertyName>TYPE</PropertyName><Literal>highway</Literal>
      </PropertyIsEqualTo></Filter>
      <LineSymbolizer><Stroke>
        <CssParameter name="stroke">#ff0000</CssParameter>
        <CssParameter name="stroke-width">3</CssParameter>
      </Stroke></LineSymbolizer>
    </Rule>
    <Rule>
      <PolygonSymbolizer>
        <Fill><CssParameter name="fill">#00ff00</CssParameter></Fill>
      </PolygonSymbolizer>
    </Rule>
  </FeatureTypeStyle></UserStyle></NamedLayer>
</StyledLayerDescriptor>"#;
        let rules = parse_sld(sld);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].filter, Some(("TYPE".into(), "highway".into())));
        assert_eq!(rules[0].style.line_color, Some([255, 0, 0, 255]));
        assert_eq!(rules[0].style.line_width, Some(3.0));
        assert!(rules[1].filter.is_none());
        assert_eq!(rules[1].style.fill_color, Some([0, 255, 0, 255]));
    }

    #[test]
    fn test_parse_qml_categorized() {
        let qml = r#"<qgis>
  <renderer-v2 type="categorizedSymbol" attr="CLASS">
    <categories>
      <category value="road" symbol="0"/>
      <category value="rail" symbol="1"/>
    </categories>
    <symbols>
      <symbol name="0" type="line">
        <layer class="SimpleLine">
          <prop k="line_color" v="255,0,0,255"/>
          <prop k="line_width" v="2"/>
        </layer>
      </symbol>
      <symbol name="1" type="line">
        <layer class="SimpleLine">
          <prop k="line_color" v="0,0,255,255"/>
          <prop k="line_width" v="1.5"/>
        </layer>
      </symbol>
    </symbols>
  </renderer-v2>
</qgis>"#;
        let rules = parse_qml(qml);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].filter, Some(("CLASS".into(), "road".into())));
        assert_eq!(rules[0].style.line_color, Some([255, 0, 0, 255]));
        assert_eq!(rules[1].filter, Some(("CLASS".into(), "rail".into())));
        assert_eq!(rules[1].style.line_color, Some([0, 0, 255, 255]));
    }
}
