use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, RwLock};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use lru::LruCache;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;
use turnout_core::geo::{latlon_to_tile_pixel, tile_bounds};
use turnout_core::kml::{Geometry, KmzData, Style};

const TILE_SIZE: u32 = 256;
const CACHE_CAPACITY: usize = 512;

const DEFAULT_LINE_COLOR: [u8; 4] = [255, 100, 0, 200];
const DEFAULT_LINE_WIDTH: f32 = 2.0;
const DEFAULT_FILL_COLOR: [u8; 4] = [255, 100, 0, 80];
const POINT_RADIUS: f32 = 5.0;
const POINT_COLOR: [u8; 4] = [255, 60, 0, 220];

struct DecodedImage {
    pixmap: Pixmap,
    north: f64,
    south: f64,
    east: f64,
    west: f64,
    rotation: f64,
}

pub struct Layer {
    pub id: u32,
    pub name: String,
    pub bbox: (f64, f64, f64, f64),
    data: KmzData,
    images: Vec<DecodedImage>,
}

pub struct TileState {
    pub(crate) layers: RwLock<Vec<Layer>>,
    cache: Mutex<LruCache<(u8, u32, u32), Vec<u8>>>,
    next_id: Mutex<u32>,
}

pub struct ServerHandle {
    pub port: u16,
    pub state: Arc<TileState>,
    pub shutdown_tx: watch::Sender<bool>,
}

impl ServerHandle {
    pub fn add_layer(&self, data: KmzData) -> bool {
        let Some(bbox) = data.bbox() else { return false };
        let name = data.name.clone().unwrap_or_else(|| "Overlay".to_string());
        let images = decode_images(&data);

        let id = {
            let mut next = self.state.next_id.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = *next;
            *next += 1;
            id
        };

        {
            let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
            layers.push(Layer { id, name, bbox, data, images });
        }

        self.clear_cache();
        true
    }

    pub fn remove_layer(&self, id: u32) -> bool {
        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = layers.len();
        layers.retain(|l| l.id != id);
        let removed = layers.len() < before;
        drop(layers);
        if removed {
            self.clear_cache();
        }
        removed
    }

    pub fn layer_count(&self) -> usize {
        self.state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }

    fn clear_cache(&self) {
        let mut cache = self.state.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.clear();
    }
}

pub async fn start() -> Result<ServerHandle, Box<dyn std::error::Error + Send + Sync>> {
    let state = Arc::new(TileState {
        layers: RwLock::new(Vec::new()),
        cache: Mutex::new(LruCache::new(
            NonZeroUsize::new(CACHE_CAPACITY).expect("nonzero"),
        )),
        next_id: Mutex::new(0),
    });

    let app = Router::new()
        .route("/{z}/{x}/{y}", get(serve_tile))
        .layer(CorsLayer::permissive())
        .with_state(Arc::clone(&state));

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await
            .ok();
    });

    Ok(ServerHandle { port, state, shutdown_tx })
}

fn decode_images(data: &KmzData) -> Vec<DecodedImage> {
    data.ground_overlays
        .iter()
        .filter_map(|go| {
            let img_bytes = data.images.get(&go.href).or_else(|| {
                let basename = go.href.rsplit('/').next()?;
                data.images
                    .iter()
                    .find(|(k, _)| k.ends_with(basename))
                    .map(|(_, v)| v)
            })?;

            let dyn_img = image::load_from_memory(img_bytes).ok()?;
            let rgba = dyn_img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            let pixmap = Pixmap::from_vec(rgba.into_raw(), tiny_skia::IntSize::from_wh(w, h)?)?;

            Some(DecodedImage {
                pixmap,
                north: go.north,
                south: go.south,
                east: go.east,
                west: go.west,
                rotation: go.rotation,
            })
        })
        .collect()
}

async fn serve_tile(
    Path((z, x, y)): Path<(u8, u32, u32)>,
    State(state): State<Arc<TileState>>,
) -> impl IntoResponse {
    {
        let mut cache = state.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(png) = cache.get(&(z, x, y)) {
            return (StatusCode::OK, [("content-type", "image/png")], png.clone());
        }
    }

    let png = render_tile(&state, z.into(), x, y);

    {
        let mut cache = state.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.put((z, x, y), png.clone());
    }

    (StatusCode::OK, [("content-type", "image/png")], png)
}

fn render_tile(state: &TileState, z: u32, x: u32, y: u32) -> Vec<u8> {
    let mut pixmap = Pixmap::new(TILE_SIZE, TILE_SIZE).expect("256x256 pixmap");
    let (tile_w, tile_s, tile_e, tile_n) = tile_bounds(z, x, y);

    let layers = state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    for layer in layers.iter() {
        render_ground_overlays(&mut pixmap, &layer.images, z, x, y, tile_w, tile_s, tile_e, tile_n);
        render_geometry(&mut pixmap, &layer.data, z, x, y, tile_w, tile_s, tile_e, tile_n);
    }

    pixmap.encode_png().unwrap_or_default()
}

fn render_ground_overlays(
    pixmap: &mut Pixmap,
    images: &[DecodedImage],
    z: u32, x: u32, y: u32,
    tile_w: f64, tile_s: f64, tile_e: f64, tile_n: f64,
) {
    for ov in images {
        if ov.east < tile_w || ov.west > tile_e || ov.north < tile_s || ov.south > tile_n {
            continue;
        }

        let (px_left, py_top) = latlon_to_tile_pixel(ov.north, ov.west, z, x, y);
        let (px_right, py_bottom) = latlon_to_tile_pixel(ov.south, ov.east, z, x, y);

        let dest_w = px_right - px_left;
        let dest_h = py_bottom - py_top;
        if dest_w.abs() < 0.01 || dest_h.abs() < 0.01 {
            continue;
        }

        let sx = dest_w / ov.pixmap.width() as f32;
        let sy = dest_h / ov.pixmap.height() as f32;

        let transform = if ov.rotation.abs() > 0.001 {
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

        pixmap.draw_pixmap(
            0, 0,
            ov.pixmap.as_ref(),
            &PixmapPaint::default(),
            transform,
            None,
        );
    }
}

fn render_geometry(
    pixmap: &mut Pixmap,
    data: &KmzData,
    z: u32, x: u32, y: u32,
    tile_w: f64, tile_s: f64, tile_e: f64, tile_n: f64,
) {
    let margin = (tile_e - tile_w) * 0.1;
    let ew = tile_w - margin;
    let ee = tile_e + margin;
    let es = tile_s - margin;
    let en = tile_n + margin;

    for pm in &data.placemarks {
        let style = data.resolve_style(pm);
        render_single_geometry(pixmap, &pm.geometry, z, x, y, ew, es, ee, en, &style);
    }
}

fn render_single_geometry(
    pixmap: &mut Pixmap,
    geom: &Geometry,
    z: u32, x: u32, y: u32,
    ew: f64, es: f64, ee: f64, en: f64,
    style: &Style,
) {
    match geom {
        Geometry::Point { lon, lat } => {
            if *lon >= ew && *lon <= ee && *lat >= es && *lat <= en {
                render_point(pixmap, *lat, *lon, z, x, y, style);
            }
        }
        Geometry::LineString { coords } => {
            if coords_intersect(coords, ew, es, ee, en) {
                render_linestring(pixmap, coords, z, x, y, style);
            }
        }
        Geometry::Polygon { outer, inner } => {
            if coords_intersect(outer, ew, es, ee, en) {
                render_polygon(pixmap, outer, inner, z, x, y, style);
            }
        }
        Geometry::Multi(geoms) => {
            for g in geoms {
                render_single_geometry(pixmap, g, z, x, y, ew, es, ee, en, style);
            }
        }
    }
}

fn coords_intersect(coords: &[(f64, f64)], w: f64, s: f64, e: f64, n: f64) -> bool {
    coords.iter().any(|(lon, lat)| *lon >= w && *lon <= e && *lat >= s && *lat <= n)
}

fn render_point(pixmap: &mut Pixmap, lat: f64, lon: f64, z: u32, x: u32, y: u32, style: &Style) {
    let (px, py) = latlon_to_tile_pixel(lat, lon, z, x, y);

    let rgba = style.line_color.unwrap_or(POINT_COLOR);
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
    z: u32, x: u32, y: u32,
    style: &Style,
) {
    if coords.len() < 2 {
        return;
    }

    let mut pb = PathBuilder::new();
    let (px, py) = latlon_to_tile_pixel(coords[0].1, coords[0].0, z, x, y);
    pb.move_to(px, py);
    for &(lon, lat) in &coords[1..] {
        let (px, py) = latlon_to_tile_pixel(lat, lon, z, x, y);
        pb.line_to(px, py);
    }

    let Some(path) = pb.finish() else { return };

    let rgba = style.line_color.unwrap_or(DEFAULT_LINE_COLOR);
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
    z: u32, x: u32, y: u32,
    style: &Style,
) {
    if outer.len() < 3 {
        return;
    }

    let mut pb = PathBuilder::new();
    let (px, py) = latlon_to_tile_pixel(outer[0].1, outer[0].0, z, x, y);
    pb.move_to(px, py);
    for &(lon, lat) in &outer[1..] {
        let (px, py) = latlon_to_tile_pixel(lat, lon, z, x, y);
        pb.line_to(px, py);
    }
    pb.close();

    for ring in inner {
        if ring.len() < 3 {
            continue;
        }
        let (px, py) = latlon_to_tile_pixel(ring[0].1, ring[0].0, z, x, y);
        pb.move_to(px, py);
        for &(lon, lat) in &ring[1..] {
            let (px, py) = latlon_to_tile_pixel(lat, lon, z, x, y);
            pb.line_to(px, py);
        }
        pb.close();
    }

    let Some(path) = pb.finish() else { return };

    let should_fill = style.poly_fill.unwrap_or(true);
    let should_outline = style.poly_outline.unwrap_or(true);

    if should_fill {
        let rgba = style.fill_color.unwrap_or(DEFAULT_FILL_COLOR);
        let mut paint = Paint::default();
        paint.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
        paint.anti_alias = true;
        pixmap.fill_path(&path, &paint, FillRule::EvenOdd, Transform::identity(), None);
    }

    if should_outline {
        let rgba = style.line_color.unwrap_or(DEFAULT_LINE_COLOR);
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
