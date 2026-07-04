use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use rusqlite::Connection;
use tauri::{Emitter, Manager};

use crate::tile_server::UnpoisonExt;

const CONCURRENT_REQUESTS: usize = 24;
const HTTP_TIMEOUT_SECS: u64 = 15;
const MAX_RETRIES: u32 = 5;
const INITIAL_RETRY_DELAY_MS: u64 = 500;
const MAX_RETRY_DELAY_MS: u64 = 10_000;
const PROGRESS_INTERVAL: usize = 48;
const SUBDOMAINS: &[&str] = &["a", "b", "c", "d"];

pub struct DownloadProgress {
    pub total: AtomicU64,
    pub completed: AtomicU64,
    pub failed: AtomicU64,
    pub bytes: AtomicU64,
    pub cancelled: AtomicBool,
    pub throttled: AtomicBool,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub total: u64,
    pub completed: u64,
    pub failed: u64,
    pub bytes: u64,
}

enum FetchResult {
    Ok(Vec<u8>),
    NotFound,
    Failed,
    Throttled,
}

async fn fetch_with_retry(client: &reqwest::Client, url: &str) -> FetchResult {
    let mut delay = std::time::Duration::from_millis(INITIAL_RETRY_DELAY_MS);
    for attempt in 0..=MAX_RETRIES {
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                return resp.bytes().await
                    .map_or(FetchResult::Failed, |b| FetchResult::Ok(b.to_vec()));
            }
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                return FetchResult::NotFound;
            }
            Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                let retry_after = resp.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(5);
                if attempt < MAX_RETRIES {
                    tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
                    continue;
                }
                return FetchResult::Throttled;
            }
            _ if attempt < MAX_RETRIES => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_millis(MAX_RETRY_DELAY_MS));
            }
            _ => return FetchResult::Failed,
        }
    }
    FetchResult::Failed
}

fn expand_url(template: &str, z: u8, x: u32, y: u32, tile_index: usize) -> String {
    let url = template
        .replace("{z}", &z.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string())
        .replace("{q}", &crate::tile_server::xyz_to_quadkey(u32::from(z), x, y));
    if url.contains("{s}") {
        url.replace("{s}", SUBDOMAINS[tile_index % SUBDOMAINS.len()])
    } else {
        url
    }
}

pub fn create_mbtiles(path: &str, name: &str, format: &str, bounds: (f64, f64, f64, f64)) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS images (
            tile_id TEXT NOT NULL PRIMARY KEY,
            tile_data BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS map (
            zoom_level INTEGER NOT NULL,
            tile_column INTEGER NOT NULL,
            tile_row INTEGER NOT NULL,
            tile_id TEXT,
            PRIMARY KEY (zoom_level, tile_column, tile_row)
        );
        CREATE VIEW IF NOT EXISTS tiles AS
            SELECT map.zoom_level, map.tile_column, map.tile_row, images.tile_data
            FROM map JOIN images ON map.tile_id = images.tile_id;
        CREATE TABLE IF NOT EXISTS metadata (
            name TEXT NOT NULL,
            value TEXT NOT NULL,
            PRIMARY KEY (name)
        );"
    )?;
    let (west, south, east, north) = bounds;
    let center_lon = (west + east) / 2.0;
    let center_lat = (south + north) / 2.0;
    let meta = [
        ("name", name.to_string()),
        ("format", format.into()),
        ("type", "overlay".into()),
        ("bounds", format!("{west},{south},{east},{north}")),
        ("center", format!("{center_lon},{center_lat}")),
    ];
    for (k, v) in &meta {
        conn.execute("INSERT OR REPLACE INTO metadata (name, value) VALUES (?1, ?2)", [k, v.as_str()])?;
    }
    Ok(conn)
}

fn mbtiles_y(z: u8, y: u32) -> u32 {
    (1u32 << z) - 1 - y
}

fn detect_format(data: &[u8]) -> &'static str {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if data.starts_with(&[0xFF, 0xD8]) {
        "jpg"
    } else {
        "png"
    }
}

fn tile_hash(data: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn insert_tile(conn: &Connection, z: u8, x: u32, tms_y: u32, data: &[u8]) {
    let hash = tile_hash(data);
    let _ = conn.execute(
        "INSERT OR IGNORE INTO images (tile_id, tile_data) VALUES (?1, ?2)",
        rusqlite::params![hash, data],
    );
    let _ = conn.execute(
        "INSERT OR REPLACE INTO map (zoom_level, tile_column, tile_row, tile_id) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![z, x, tms_y, hash],
    );
}

pub async fn download_tiles(
    app: tauri::AppHandle,
    url_template: String,
    path: String,
    name: String,
    south: f64,
    west: f64,
    north: f64,
    east: f64,
    z_min: u8,
    z_max: u8,
    progress: Arc<DownloadProgress>,
) -> Result<(), String> {
    use turnout_core::geo::latlon_to_tile_xy;

    let mut tiles: Vec<(u8, u32, u32)> = Vec::new();
    for z in z_min..=z_max {
        let (x_min, y_min) = latlon_to_tile_xy(north, west, z);
        let (x_max, y_max) = latlon_to_tile_xy(south, east, z);
        for x in x_min..=x_max {
            for y in y_min..=y_max {
                tiles.push((z, x, y));
            }
        }
    }

    progress.total.store(tiles.len() as u64, Ordering::Relaxed);
    emit_progress(&app, &progress);

    let format_detected = std::sync::Mutex::new(false);
    let conn = create_mbtiles(&path, &name, "png", (west, south, east, north))
        .map_err(|e| format!("Failed to create MBTiles: {e}"))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent("Turnout/0.2.0 (+https://github.com/SuperManifolds/Turnout)")
        .http2_adaptive_window(true)
        .pool_max_idle_per_host(CONCURRENT_REQUESTS)
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    let conn = std::sync::Mutex::new(conn);
    let mut throttle_delay = std::time::Duration::ZERO;

    for (chunk_idx, chunk) in tiles.chunks(CONCURRENT_REQUESTS).enumerate() {
        if progress.cancelled.load(Ordering::Relaxed) {
            return Err("Download cancelled".into());
        }

        if !throttle_delay.is_zero() {
            tokio::time::sleep(throttle_delay).await;
        }

        let base_idx = chunk_idx * CONCURRENT_REQUESTS;
        let futures: Vec<_> = chunk.iter().enumerate().map(|(i, &(z, x, y))| {
            let client = client.clone();
            let url = expand_url(&url_template, z, x, y, base_idx + i);
            async move {
                (z, x, y, fetch_with_retry(&client, &url).await)
            }
        }).collect();

        let results = futures::future::join_all(futures).await;

        let mut got_throttled = false;
        {
            let conn = conn.lock().unpoison();
            for (z, x, y, result) in results {
                match result {
                    FetchResult::Ok(data) => {
                        if !*format_detected.lock().unpoison() {
                            let fmt = detect_format(&data);
                            let _ = conn.execute(
                                "INSERT OR REPLACE INTO metadata (name, value) VALUES ('format', ?1)",
                                [fmt],
                            );
                            *format_detected.lock().unpoison() = true;
                        }
                        progress.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                        insert_tile(&conn, z, x, mbtiles_y(z, y), &data);
                        progress.completed.fetch_add(1, Ordering::Relaxed);
                    }
                    FetchResult::NotFound => {
                        progress.completed.fetch_add(1, Ordering::Relaxed);
                    }
                    FetchResult::Throttled => {
                        got_throttled = true;
                        progress.failed.fetch_add(1, Ordering::Relaxed);
                    }
                    FetchResult::Failed => {
                        progress.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        if got_throttled {
            throttle_delay = (throttle_delay * 2)
                .max(std::time::Duration::from_secs(2))
                .min(std::time::Duration::from_secs(30));
            progress.throttled.store(true, Ordering::Relaxed);
        } else if !throttle_delay.is_zero() {
            throttle_delay /= 2;
            if throttle_delay < std::time::Duration::from_millis(100) {
                throttle_delay = std::time::Duration::ZERO;
                progress.throttled.store(false, Ordering::Relaxed);
            }
        }

        if chunk_idx % (PROGRESS_INTERVAL / CONCURRENT_REQUESTS).max(1) == 0 {
            emit_progress(&app, &progress);
        }
    }

    emit_progress(&app, &progress);
    Ok(())
}

pub struct DownloadState {
    pub active: std::sync::Mutex<Option<Arc<DownloadProgress>>>,
}

impl DownloadState {
    pub fn new() -> Self {
        Self { active: std::sync::Mutex::new(None) }
    }
}

#[tauri::command]
pub fn count_tiles(south: f64, west: f64, north: f64, east: f64, z_min: u8, z_max: u8) -> u64 {
    turnout_core::geo::count_tiles_in_bbox(south, west, north, east, z_min, z_max)
}

#[tauri::command]
pub async fn start_tile_download(
    app: tauri::AppHandle,
    url: String,
    name: String,
    south: f64,
    west: f64,
    north: f64,
    east: f64,
    z_min: u8,
    z_max: u8,
) -> Result<String, String> {
    use tauri_plugin_dialog::DialogExt;

    let path = app.dialog()
        .file()
        .add_filter("MBTiles", &["mbtiles"])
        .set_file_name(format!("{name}.mbtiles"))
        .blocking_save_file()
        .ok_or("No save location selected")?
        .to_string();

    let progress = Arc::new(DownloadProgress {
        total: AtomicU64::new(0),
        completed: AtomicU64::new(0),
        failed: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        throttled: AtomicBool::new(false),
    });

    {
        let state = app.state::<DownloadState>();
        *state.active.lock().unpoison() = Some(Arc::clone(&progress));
    }

    let result = download_tiles(
        app.clone(), url, path.clone(), name,
        south, west, north, east, z_min, z_max, progress,
    ).await;

    {
        let state = app.state::<DownloadState>();
        *state.active.lock().unpoison() = None;
    }

    result?;
    Ok(path)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn cancel_tile_download(app: tauri::AppHandle) {
    let state = app.state::<DownloadState>();
    if let Some(progress) = state.active.lock().unpoison().as_ref() {
        progress.cancelled.store(true, Ordering::Relaxed);
    }
}

fn emit_progress(app: &tauri::AppHandle, progress: &DownloadProgress) {
    let _ = app.emit("tile-download-progress", ProgressEvent {
        total: progress.total.load(Ordering::Relaxed),
        completed: progress.completed.load(Ordering::Relaxed),
        failed: progress.failed.load(Ordering::Relaxed),
        bytes: progress.bytes.load(Ordering::Relaxed),
    });
}
