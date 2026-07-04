use std::num::NonZeroUsize;
use std::sync::Mutex;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use lru::LruCache;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tower_http::cors::CorsLayer;

use crate::tile_server::UnpoisonExt;

const PREFERRED_PORT: u16 = 17854;
const TILE_CACHE_CAPACITY: usize = 8192;
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
    style: String,
    z: u8,
    x: u32,
    y: u32,
    tx: oneshot::Sender<Option<Vec<u8>>>,
}

type TileCache = LruCache<(String, u8, u32, u32), Vec<u8>>;

struct OrmTileState {
    cache: Mutex<TileCache>,
    render_tx: mpsc::Sender<RenderRequest>,
}

pub struct OrmHandle {
    _shutdown_tx: watch::Sender<bool>,
}

pub fn start_blocking() -> Result<OrmHandle, Box<dyn std::error::Error + Send + Sync>> {
    let (render_tx, render_rx) = mpsc::channel::<RenderRequest>(256);
    let render_rx = std::sync::Arc::new(tokio::sync::Mutex::new(render_rx));

    // Spawn render worker on a dedicated thread
    // Note: maplibre_native is !Send — each renderer must stay on its thread
    // Using a single worker since Metal may require main-thread-like context
    let rx_clone = std::sync::Arc::clone(&render_rx);
    std::thread::Builder::new()
        .name("orm-render".into())
        .spawn(move || {
            render_worker(0, rx_clone);
        })?;

    let state = std::sync::Arc::new(OrmTileState {
        cache: Mutex::new(LruCache::new(
            NonZeroUsize::new(TILE_CACHE_CAPACITY).expect("nonzero"),
        )),
        render_tx,
    });

    // Start HTTP server on a tokio runtime
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state2 = std::sync::Arc::clone(&state);

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
    state: std::sync::Arc<OrmTileState>,
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
    eprintln!("ORM tile renderer at http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}");

    let router = Router::new()
        .route("/{style}/{z}/{x}/{y}", get(serve_tile))
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
    rx: std::sync::Arc<tokio::sync::Mutex<mpsc::Receiver<RenderRequest>>>,
) {
    use maplibre_native::{ImageRendererBuilder, CameraUpdate, LatLng};
    use std::collections::HashMap;
    use std::num::NonZeroU32;

    eprintln!("[ORM worker {id}] Starting...");

    let size = NonZeroU32::new(512).expect("nonzero");
    let mut renderers: HashMap<String, maplibre_native::ImageRenderer<maplibre_native::Static>> = HashMap::new();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("worker runtime");

    rt.block_on(async {
        loop {
            let req = {
                let mut rx = rx.lock().await;
                rx.recv().await
            };
            let Some(req) = req else { break };

            // Get or create renderer for this style
            let renderer = renderers.entry(req.style.clone()).or_insert_with(|| {
                eprintln!("[ORM worker {id}] Loading style '{}'...", req.style);
                let mut r = ImageRendererBuilder::new()
                    .with_size(size, size)
                    .build_static_renderer();
                r.load_style_from_json_str(style_json(&req.style));
                std::thread::sleep(std::time::Duration::from_secs(2));
                eprintln!("[ORM worker {id}] Style '{}' ready", req.style);
                r
            });

            let (west, south, east, north) = turnout_core::geo::tile_bounds(u32::from(req.z), req.x, req.y);
            let center_lat = (south + north) / 2.0;
            let center_lon = (west + east) / 2.0;

            let camera = CameraUpdate::new()
                .center(LatLng { lat: center_lat, lng: center_lon })
                .zoom(f64::from(req.z));

            match renderer.render_static(&camera) {
                Ok(image) => {
                    let img = image.as_image();
                    let mut png_buf = Vec::new();
                    let encoder = image::codecs::png::PngEncoder::new(&mut png_buf);
                    if img.write_with_encoder(encoder).is_ok() {
                        let _ = req.tx.send(Some(png_buf));
                    } else {
                        let _ = req.tx.send(None);
                    }
                }
                Err(e) => {
                    eprintln!("[ORM worker {id}] Render error z={} x={} y={}: {e}", req.z, req.x, req.y);
                    let _ = req.tx.send(None);
                }
            }
        }
    });
}

async fn serve_tile(
    Path((style, z, x, y)): Path<(String, u8, u32, u32)>,
    State(state): State<std::sync::Arc<OrmTileState>>,
) -> impl IntoResponse {
    let cache_key = (style.clone(), z, x, y);
    {
        let mut cache = state.cache.lock().unpoison();
        if let Some(png) = cache.get(&cache_key) {
            return (StatusCode::OK, [("content-type", "image/png")], png.clone());
        }
    }

    let (tx, rx) = oneshot::channel();
    if state.render_tx.send(RenderRequest { style: style.clone(), z, x, y, tx }).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, [("content-type", "text/plain")], b"render queue full".to_vec());
    }

    // Wait for result
    match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
        Ok(Ok(Some(png))) => {
            state.cache.lock().unpoison().put(cache_key, png.clone());
            (StatusCode::OK, [("content-type", "image/png")], png)
        }
        Ok(Ok(None)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, [("content-type", "text/plain")], b"render failed".to_vec())
        }
        _ => {
            (StatusCode::GATEWAY_TIMEOUT, [("content-type", "text/plain")], b"render timeout".to_vec())
        }
    }
}
