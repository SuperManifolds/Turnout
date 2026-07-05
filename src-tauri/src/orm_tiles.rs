use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use lru::LruCache;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tower_http::cors::CorsLayer;

use crate::tile_server::UnpoisonExt;

const PREFERRED_PORT: u16 = 17854;
const TILE_CACHE_CAPACITY: usize = 8192;
const WORKER_COUNT: usize = 4;
const STYLES: &[(&str, &str)] = &[
    ("standard", include_str!("../resources/orm/standard.json")),
    ("speed", include_str!("../resources/orm/speed.json")),
    ("signals", include_str!("../resources/orm/signals.json")),
    ("electrification", include_str!("../resources/orm/electrification.json")),
    ("track", include_str!("../resources/orm/track.json")),
    ("operator", include_str!("../resources/orm/operator.json")),
    ("route", include_str!("../resources/orm/route.json")),
];

fn style_json(name: &str) -> &'static str {
    STYLES.iter().find(|(n, _)| *n == name).map_or(STYLES[0].1, |(_, s)| s)
}

struct RenderRequest {
    style: Arc<str>,
    z: u8,
    x: u32,
    y: u32,
    tx: oneshot::Sender<Option<Bytes>>,
}

type TileCache = LruCache<(Arc<str>, u8, u32, u32), Bytes>;

struct OrmTileState {
    cache: Mutex<TileCache>,
    dispatch_tx: std::sync::mpsc::Sender<RenderRequest>,
}

pub struct OrmHandle {
    _shutdown_tx: watch::Sender<bool>,
}

pub fn start_blocking() -> Result<OrmHandle, Box<dyn std::error::Error + Send + Sync>> {
    let (dispatch_tx, dispatch_rx) = std::sync::mpsc::channel::<RenderRequest>();
    let dispatch_rx = Arc::new(Mutex::new(dispatch_rx));

    let cache_dir = dirs_next::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("turnout")
        .join("orm_cache");
    let _ = std::fs::create_dir_all(&cache_dir);

    for i in 0..WORKER_COUNT {
        let rx = Arc::clone(&dispatch_rx);
        let cache_path = cache_dir.join(format!("worker_{i}.db"));
        std::thread::Builder::new()
            .name(format!("orm-render-{i}"))
            .spawn(move || {
                render_worker(i, rx, &cache_path);
            })?;
    }

    let state = Arc::new(OrmTileState {
        cache: Mutex::new(LruCache::new(
            NonZeroUsize::new(TILE_CACHE_CAPACITY).expect("nonzero"),
        )),
        dispatch_tx,
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state2 = Arc::clone(&state);

    std::thread::Builder::new()
        .name("orm-http".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(run_server(state2, shutdown_rx));
        })?;

    Ok(OrmHandle { _shutdown_tx: shutdown_tx })
}

async fn run_server(
    state: Arc<OrmTileState>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let listener = match TcpListener::bind(format!("127.0.0.1:{PREFERRED_PORT}")).await {
        Ok(l) => l,
        Err(_) => match TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ORM tiles: failed to bind: {e}");
                return;
            }
        },
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    eprintln!("ORM tile renderer at http://127.0.0.1:{port}/{{style}}/{{z}}/{{y}}/{{x}}.png");

    let router = Router::new()
        .route("/tilejson.json", get(serve_tilejson))
        .route("/{style}/tilejson.json", get(serve_style_tilejson))
        .route("/{style}/{z}/{y}/{x_png}", get(serve_tile))
        .layer(CorsLayer::permissive())
        .with_state(state);

    axum::serve(listener, router)
        .with_graceful_shutdown(async move { let _ = shutdown_rx.changed().await; })
        .await
        .ok();
}

#[allow(clippy::needless_pass_by_value)]
fn render_worker(
    id: usize,
    rx: Arc<Mutex<std::sync::mpsc::Receiver<RenderRequest>>>,
    cache_path: &std::path::Path,
) {
    let cache_path = cache_path.to_path_buf();
    let rx_clone = Arc::clone(&rx);

    loop {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render_worker_inner(id, &rx_clone, &cache_path);
        }));
        match result {
            Ok(()) => {
                eprintln!("[ORM worker {id}] Exited cleanly");
                break;
            }
            Err(e) => {
                let msg = if let Some(s) = e.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                eprintln!("[ORM worker {id}] PANIC: {msg} — restarting...");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
}

fn render_worker_inner(
    id: usize,
    rx: &Arc<Mutex<std::sync::mpsc::Receiver<RenderRequest>>>,
    cache_path: &std::path::Path,
) {
    use maplibre_native::{ImageRendererBuilder, ResourceOptions};
    use std::num::NonZeroU32;

    eprintln!("[ORM worker {id}] Starting...");

    let tile_size = NonZeroU32::new(512).expect("nonzero");
    let cache_path = cache_path.to_path_buf();
    let mut render_count: u64 = 0;
    let mut renderer: Option<maplibre_native::ImageRenderer<maplibre_native::Tile>> = None;
    let mut current_style: Option<Arc<str>> = None;

    loop {
        eprintln!("[ORM worker {id}] Waiting for request...");
        let req = {
            let rx = rx.lock().unpoison();
            match rx.recv() {
                Ok(req) => req,
                Err(_) => return,
            }
        };
        eprintln!("[ORM worker {id}] Got request z={} x={} y={} style={}", req.z, req.x, req.y, req.style);

        if current_style.as_ref() != Some(&req.style) || renderer.is_none() {
            eprintln!("[ORM worker {id}] Loading style '{}'...", req.style);
            let resource_opts = ResourceOptions::default()
                .with_cache_path(cache_path.clone());
            let mut r = ImageRendererBuilder::new()
                .with_size(tile_size, tile_size)
                .with_resource_options(resource_opts)
                .build_tile_renderer();
            r.load_style_from_json_str(style_json(&req.style));
            eprintln!("[ORM worker {id}] Style '{}' loaded", req.style);
            renderer = Some(r);
            current_style = Some(Arc::clone(&req.style));
        }

        let r = renderer.as_mut().expect("just created");
        let start = Instant::now();

        let (rz, rx_coord, ry) = if req.z > 19 {
            let dz = req.z - 19;
            (19, req.x >> dz, req.y >> dz)
        } else {
            (req.z, req.x, req.y)
        };

        match r.render_tile(rz, rx_coord, ry) {
            Ok(image) => {
                let render_ms = start.elapsed().as_millis();
                let img = image.as_image();
                let mut png_buf = Vec::with_capacity(48 * 1024);
                let encoder = image::codecs::png::PngEncoder::new_with_quality(
                    &mut png_buf,
                    image::codecs::png::CompressionType::Fast,
                    image::codecs::png::FilterType::NoFilter,
                );
                if img.write_with_encoder(encoder).is_ok() {
                    let _ = req.tx.send(Some(Bytes::from(png_buf)));
                } else {
                    let _ = req.tx.send(None);
                }
                render_count += 1;
                if render_count.is_multiple_of(50) || render_ms > 2000 {
                    eprintln!("[ORM worker {id}] Rendered {render_count} tiles (last: {render_ms}ms)");
                }
            }
            Err(e) => {
                eprintln!("[ORM worker {id}] Failed z={rz} x={rx_coord} y={ry}: {e} ({}ms)", start.elapsed().as_millis());
                let _ = req.tx.send(None);
                renderer = None;
                current_style = None;
            }
        }
    }
}

async fn serve_tilejson(
    State(_state): State<Arc<OrmTileState>>,
) -> impl IntoResponse {
    serve_tilejson_for("standard")
}

async fn serve_style_tilejson(
    Path(style): Path<String>,
    State(_state): State<Arc<OrmTileState>>,
) -> impl IntoResponse {
    serve_tilejson_for(&style)
}

fn serve_tilejson_for(style: &str) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let json = serde_json::json!({
        "tilejson": "3.0.0",
        "name": format!("OpenRailwayMap {style}"),
        "tiles": [format!("http://127.0.0.1:{PREFERRED_PORT}/{style}/{{z}}/{{y}}/{{x}}.png")],
        "minzoom": 0,
        "maxzoom": 19,
        "format": "png",
        "bounds": [-180.0, -85.051_129, 180.0, 85.051_129],
    });
    (StatusCode::OK, [("content-type", "application/json")], serde_json::to_string(&json).unwrap_or_default())
}

async fn serve_tile(
    Path((style, z, y, x_png)): Path<(String, u8, u32, String)>,
    State(state): State<Arc<OrmTileState>>,
) -> (StatusCode, [(&'static str, &'static str); 1], Bytes) {
    let style: Arc<str> = Arc::from(style.as_str());
    let x: u32 = x_png.strip_suffix(".png").unwrap_or(&x_png)
        .parse().unwrap_or(0);

    {
        let mut cache = state.cache.lock().unpoison();
        if let Some(png) = cache.get(&(Arc::clone(&style), z, x, y)) {
            return (StatusCode::OK, [("content-type", "image/png")], png.clone());
        }
    }

    let (tx, rx) = oneshot::channel();
    if state.dispatch_tx.send(RenderRequest {
        style: Arc::clone(&style), z, x, y, tx
    }).is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, [("content-type", "text/plain")], Bytes::from_static(b"workers unavailable"));
    }

    match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
        Ok(Ok(Some(png))) => {
            state.cache.lock().unpoison().put((style, z, x, y), png.clone());
            (StatusCode::OK, [("content-type", "image/png")], png)
        }
        Ok(Ok(None)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, [("content-type", "text/plain")], Bytes::from_static(b"render failed"))
        }
        _ => {
            eprintln!("[ORM http] TIMEOUT z={z} x={x} y={y}");
            (StatusCode::GATEWAY_TIMEOUT, [("content-type", "text/plain")], Bytes::from_static(b"render timeout"))
        }
    }
}
