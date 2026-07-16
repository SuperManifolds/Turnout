//! Tile rasterization: composite the visible file layers' parsed geometry and
//! ground overlays (and pre-fetched remote tiles) into a single PNG with `tiny-skia`.

use std::collections::HashMap;

use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};
use turnout_core::geo::{latlon_to_tile_pixel, tile_bounds};
use turnout_core::kml::{Geometry, OverlayData, Style};

use crate::server_core::UnpoisonExt;

use super::{DecodedImage, RuntimeData, TileState, TILE_SIZE};

const DEFAULT_LINE_COLOR: [u8; 4] = [255, 100, 0, 200];
const DEFAULT_LINE_WIDTH: f32 = 2.0;
const DEFAULT_FILL_COLOR: [u8; 4] = [255, 100, 0, 80];
const POINT_RADIUS: f32 = 5.0;
const POINT_COLOR: [u8; 4] = [255, 60, 0, 220];
const MIN_OVERLAY_PIXEL_SIZE: f32 = 0.01;
const ROTATION_EPSILON: f64 = 0.001;
const MAX_RENDER_DEPTH: usize = 10;

/// The geographic bounds and identity of the tile being rendered, with a helper to
/// project a lat/lon into tile-local pixel coordinates.
struct TileCtx {
    z: u32,
    x: u32,
    y: u32,
    w: f64,
    s: f64,
    e: f64,
    n: f64,
}

impl TileCtx {
    fn new(z: u32, x: u32, y: u32) -> Self {
        let (w, s, e, n) = tile_bounds(z, x, y);
        Self { z, x, y, w, s, e, n }
    }

    fn pixel(&self, lat: f64, lon: f64) -> (f32, f32) {
        latlon_to_tile_pixel(lat, lon, self.z, self.x, self.y)
    }
}

pub(super) fn render_tile(state: &TileState, remote_tiles: &HashMap<u32, Pixmap>, z: u32, x: u32, y: u32) -> Vec<u8> {
    let mut pixmap = Pixmap::new(TILE_SIZE, TILE_SIZE).expect("256x256 pixmap");
    let ctx = TileCtx::new(z, x, y);

    {
        let layers = state.layers.read().unpoison();
        let runtime = state.runtime.read().unpoison();
        for layer in layers.iter() {
            if !layer.visible {
                continue;
            }
            let opacity = layer.opacity;
            if layer.source.is_file() {
                // Locally rendered from the layer's parsed geometry + overlays.
                if let Some(RuntimeData::File { data, images }) = runtime.get(&layer.id) {
                    render_ground_overlays(&mut pixmap, images, opacity, &ctx);
                    render_geometry(&mut pixmap, data, opacity, &ctx);
                }
            } else if let Some(remote_pixmap) = remote_tiles.get(&layer.id) {
                let paint = PixmapPaint { opacity, ..PixmapPaint::default() };
                pixmap.draw_pixmap(
                    0, 0,
                    remote_pixmap.as_ref(),
                    &paint,
                    Transform::identity(),
                    None,
                );
            }
        }
    }

    pixmap.encode_png().unwrap_or_default()
}

fn render_ground_overlays(
    pixmap: &mut Pixmap,
    images: &[DecodedImage],
    opacity: f32,
    ctx: &TileCtx,
) {
    let paint = PixmapPaint { opacity, ..PixmapPaint::default() };

    for ov in images {
        if ov.east < ctx.w || ov.west > ctx.e || ov.north < ctx.s || ov.south > ctx.n {
            continue;
        }

        let (px_left, py_top) = ctx.pixel(ov.north, ov.west);
        let (px_right, py_bottom) = ctx.pixel(ov.south, ov.east);

        let dest_w = px_right - px_left;
        let dest_h = py_bottom - py_top;
        if dest_w.abs() < MIN_OVERLAY_PIXEL_SIZE || dest_h.abs() < MIN_OVERLAY_PIXEL_SIZE {
            continue;
        }

        let sx = dest_w / ov.pixmap.width() as f32;
        let sy = dest_h / ov.pixmap.height() as f32;

        let transform = if ov.rotation.abs() > ROTATION_EPSILON {
            let cx = (px_left + px_right) / 2.0;
            let cy = (py_top + py_bottom) / 2.0;
            Transform::from_translate(cx, cy)
                .pre_concat(Transform::from_rotate(-ov.rotation as f32))
                .pre_concat(Transform::from_translate(-cx, -cy))
                .pre_concat(Transform::from_translate(px_left, py_top))
                .pre_concat(Transform::from_scale(sx, sy))
        } else {
            Transform::from_translate(px_left, py_top).pre_concat(Transform::from_scale(sx, sy))
        };

        pixmap.draw_pixmap(0, 0, ov.pixmap.as_ref(), &paint, transform, None);
    }
}

fn apply_opacity(rgba: [u8; 4], opacity: f32) -> [u8; 4] {
    [rgba[0], rgba[1], rgba[2], (f32::from(rgba[3]) * opacity).clamp(0.0, 255.0) as u8]
}

fn render_geometry(
    pixmap: &mut Pixmap,
    data: &OverlayData,
    opacity: f32,
    ctx: &TileCtx,
) {
    let margin = (ctx.e - ctx.w) * 0.1;
    let expanded = TileCtx {
        w: ctx.w - margin,
        s: ctx.s - margin,
        e: ctx.e + margin,
        n: ctx.n + margin,
        ..*ctx
    };

    for pm in &data.placemarks {
        let style = data.resolve_style(pm);
        render_single_geometry(pixmap, &pm.geometry, opacity, &expanded, &style);
    }
}

fn render_single_geometry(
    pixmap: &mut Pixmap,
    geom: &Geometry,
    opacity: f32,
    ctx: &TileCtx,
    style: &Style,
) {
    render_geometry_depth(pixmap, geom, opacity, ctx, style, 0);
}

fn render_geometry_depth(
    pixmap: &mut Pixmap,
    geom: &Geometry,
    opacity: f32,
    ctx: &TileCtx,
    style: &Style,
    depth: usize,
) {
    if depth > MAX_RENDER_DEPTH { return; }
    match geom {
        Geometry::Point { lon, lat } => {
            if *lon >= ctx.w && *lon <= ctx.e && *lat >= ctx.s && *lat <= ctx.n {
                render_point(pixmap, *lat, *lon, opacity, ctx, style);
            }
        }
        Geometry::LineString { coords } => {
            if coords_intersect(coords, ctx.w, ctx.s, ctx.e, ctx.n) {
                render_linestring(pixmap, coords, opacity, ctx, style);
            }
        }
        Geometry::Polygon { outer, inner } => {
            if coords_intersect(outer, ctx.w, ctx.s, ctx.e, ctx.n) {
                render_polygon(pixmap, outer, inner, opacity, ctx, style);
            }
        }
        Geometry::Multi(geoms) => {
            for g in geoms {
                render_geometry_depth(pixmap, g, opacity, ctx, style, depth + 1);
            }
        }
    }
}

fn coords_intersect(coords: &[(f64, f64)], w: f64, s: f64, e: f64, n: f64) -> bool {
    if coords.iter().any(|(lon, lat)| *lon >= w && *lon <= e && *lat >= s && *lat <= n) {
        return true;
    }
    coords.windows(2).any(|seg| {
        turnout_core::geo::segment_rect_intersect(
            seg[0].0, seg[0].1, seg[1].0, seg[1].1, w, s, e, n,
        ).is_some()
    })
}

fn render_point(pixmap: &mut Pixmap, lat: f64, lon: f64, opacity: f32, ctx: &TileCtx, style: &Style) {
    let (px, py) = ctx.pixel(lat, lon);

    let rgba = apply_opacity(style.line_color.unwrap_or(POINT_COLOR), opacity);
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
    paint.anti_alias = true;

    let mut pb = PathBuilder::new();
    pb.push_circle(px, py, POINT_RADIUS);
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn render_linestring(
    pixmap: &mut Pixmap,
    coords: &[(f64, f64)],
    opacity: f32,
    ctx: &TileCtx,
    style: &Style,
) {
    if coords.len() < 2 {
        return;
    }

    let mut pb = PathBuilder::new();
    let (px, py) = ctx.pixel(coords[0].1, coords[0].0);
    pb.move_to(px, py);
    for &(lon, lat) in &coords[1..] {
        let (px, py) = ctx.pixel(lat, lon);
        pb.line_to(px, py);
    }

    let Some(path) = pb.finish() else { return };

    let rgba = apply_opacity(style.line_color.unwrap_or(DEFAULT_LINE_COLOR), opacity);
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
    paint.anti_alias = true;

    let stroke = Stroke {
        width: style.line_width.unwrap_or(DEFAULT_LINE_WIDTH),
        ..Stroke::default()
    };

    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn render_polygon(
    pixmap: &mut Pixmap,
    outer: &[(f64, f64)],
    inner: &[Vec<(f64, f64)>],
    opacity: f32,
    ctx: &TileCtx,
    style: &Style,
) {
    if outer.len() < 3 {
        return;
    }

    let mut pb = PathBuilder::new();
    let (px, py) = ctx.pixel(outer[0].1, outer[0].0);
    pb.move_to(px, py);
    for &(lon, lat) in &outer[1..] {
        let (px, py) = ctx.pixel(lat, lon);
        pb.line_to(px, py);
    }
    pb.close();

    for ring in inner {
        if ring.len() < 3 {
            continue;
        }
        let (px, py) = ctx.pixel(ring[0].1, ring[0].0);
        pb.move_to(px, py);
        for &(lon, lat) in &ring[1..] {
            let (px, py) = ctx.pixel(lat, lon);
            pb.line_to(px, py);
        }
        pb.close();
    }

    let Some(path) = pb.finish() else { return };

    let should_fill = style.poly_fill.unwrap_or(true);
    let should_outline = style.poly_outline.unwrap_or(true);

    if should_fill {
        let rgba = apply_opacity(style.fill_color.unwrap_or(DEFAULT_FILL_COLOR), opacity);
        let mut paint = Paint::default();
        paint.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
        paint.anti_alias = true;
        pixmap.fill_path(&path, &paint, FillRule::EvenOdd, Transform::identity(), None);
    }

    if should_outline {
        let rgba = apply_opacity(style.line_color.unwrap_or(DEFAULT_LINE_COLOR), opacity);
        let mut paint = Paint::default();
        paint.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
        paint.anti_alias = true;

        let stroke = Stroke {
            width: style.line_width.unwrap_or(DEFAULT_LINE_WIDTH),
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}
