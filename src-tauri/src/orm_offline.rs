use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use rusqlite::Connection;
use tauri::{Emitter, Manager};

use crate::mbtiles::{DownloadProgress, ProgressEvent};
use crate::tile_server::UnpoisonExt;

const ORM_BASE: &str = "https://openrailwaymap.app";
const ORM_SOURCES: &str = "railway_line_high,railway_text_km,\
    standard_railway_turntables,standard_railway_text_stations,\
    standard_railway_grouped_stations,standard_railway_grouped_station_areas,\
    standard_railway_switch_ref,standard_station_entrances,\
    standard_railway_platforms,standard_railway_platform_edges,\
    standard_railway_stop_positions,standard_railway_line_low,\
    standard_railway_text_stations_low,standard_railway_text_stations_med";

const CONCURRENT_REQUESTS: usize = 6;
const MIN_BATCH_INTERVAL_MS: u64 = 200;
const MAX_RETRIES: u32 = 5;
const INITIAL_RETRY_DELAY_MS: u64 = 500;
const MAX_RETRY_DELAY_MS: u64 = 10_000;
const HTTP_TIMEOUT_SECS: u64 = 15;

/// Download ORM vector tiles + resources for offline use
pub async fn download_orm_offline(
    app: tauri::AppHandle,
    dir: PathBuf,
    south: f64,
    west: f64,
    north: f64,
    east: f64,
    z_min: u8,
    z_max: u8,
    progress: Arc<DownloadProgress>,
) -> Result<PathBuf, String> {
    use turnout_core::geo::latlon_to_tile_xy;

    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create dir: {e}"))?;

    // Phase 1: Download resources (style, sprites, fonts)
    download_resources(&dir).await?;

    // Phase 2: Download MVT tiles to MBTiles
    let mbtiles_path = dir.join("tiles.mbtiles");
    let conn = create_vector_mbtiles(mbtiles_path.to_str().unwrap_or("tiles.mbtiles"), (west, south, east, north))
        .map_err(|e| format!("Failed to create MBTiles: {e}"))?;

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

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent("Turnout/0.2.0 (+https://github.com/SuperManifolds/Turnout)")
        .http2_adaptive_window(true)
        .pool_max_idle_per_host(CONCURRENT_REQUESTS)
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    let conn = std::sync::Mutex::new(conn);

    for chunk in tiles.chunks(CONCURRENT_REQUESTS) {
        if progress.cancelled.load(Ordering::Relaxed) {
            return Err("Download cancelled".into());
        }

        let batch_start = tokio::time::Instant::now();

        let futures: Vec<_> = chunk.iter().map(|&(z, x, y)| {
            let client = client.clone();
            let url = format!("{ORM_BASE}/{ORM_SOURCES}/{z}/{x}/{y}");
            async move {
                (z, x, y, fetch_with_retry(&client, &url).await)
            }
        }).collect();

        let results = futures::future::join_all(futures).await;

        {
            let conn = conn.lock().unpoison();
            for (z, x, y, result) in results {
                match result {
                    FetchResult::Ok(data) => {
                        let tms_y = (1u32 << z) - 1 - y;
                        let hash = tile_hash(&data);
                        let _ = conn.execute(
                            "INSERT OR IGNORE INTO images (tile_id, tile_data) VALUES (?1, ?2)",
                            rusqlite::params![hash, data],
                        );
                        let _ = conn.execute(
                            "INSERT OR REPLACE INTO map (zoom_level, tile_column, tile_row, tile_id) VALUES (?1, ?2, ?3, ?4)",
                            rusqlite::params![z, x, tms_y, hash],
                        );
                        progress.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                        progress.completed.fetch_add(1, Ordering::Relaxed);
                    }
                    FetchResult::NotFound => {
                        progress.completed.fetch_add(1, Ordering::Relaxed);
                    }
                    FetchResult::Throttled | FetchResult::Failed => {
                        progress.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        emit_progress(&app, &progress);

        let elapsed = batch_start.elapsed();
        let min_interval = std::time::Duration::from_millis(MIN_BATCH_INTERVAL_MS);
        if elapsed < min_interval {
            tokio::time::sleep(min_interval - elapsed).await;
        }
    }

    // Phase 3: Create local style JSON pointing at local resources
    create_local_style(&dir);

    Ok(dir)
}

async fn download_resources(dir: &Path) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Turnout/0.2.0")
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;

    let styles_dir = dir.join("styles");
    std::fs::create_dir_all(&styles_dir).map_err(|e| e.to_string())?;
    for style in &["standard", "speed", "signals", "electrification", "track", "operator", "route"] {
        let url = format!("{ORM_BASE}/style/{style}.json");
        if let Ok(bytes) = fetch_bytes(&client, &url).await {
            let _ = std::fs::write(styles_dir.join(format!("{style}.json")), &bytes);
        }
    }

    let sprites_dir = dir.join("sprites");
    std::fs::create_dir_all(&sprites_dir).map_err(|e| e.to_string())?;
    for (name, prefix) in &[("default", "sprite"), ("sdf", "sdf_sprite")] {
        for suffix in &["", "@2x"] {
            if let Ok(bytes) = fetch_bytes(&client, &format!("{ORM_BASE}/{prefix}/symbols{suffix}.json")).await {
                let _ = std::fs::write(sprites_dir.join(format!("{name}{suffix}.json")), &bytes);
            }
            if let Ok(bytes) = fetch_bytes(&client, &format!("{ORM_BASE}/{prefix}/symbols{suffix}.png")).await {
                let _ = std::fs::write(sprites_dir.join(format!("{name}{suffix}.png")), &bytes);
            }
        }
    }

    let fonts_dir = dir.join("fonts");
    for font in &["OpenRailwayMap-Bold", "OpenRailwayMap-Regular", "OpenRailwayMap-Italic", "FiraCode-Bold"] {
        let font_dir = fonts_dir.join(font);
        std::fs::create_dir_all(&font_dir).map_err(|e| e.to_string())?;
        for range_start in (0u32..65536).step_by(256) {
            let range_end = range_start + 255;
            if let Ok(bytes) = fetch_bytes(&client, &format!("{ORM_BASE}/font/{font}/{range_start}-{range_end}.pbf")).await {
                let _ = std::fs::write(font_dir.join(format!("{range_start}-{range_end}.pbf")), &bytes);
            }
        }
    }

    Ok(())
}

fn create_local_style(dir: &Path) {
    let styles_dir = dir.join("styles");
    let sprites_path = dir.join("sprites").to_string_lossy().to_string();
    let fonts_path = dir.join("fonts").to_string_lossy().to_string();

    for style_name in &["standard", "speed", "signals", "electrification", "track", "operator", "route"] {
        let style_path = styles_dir.join(format!("{style_name}.json"));
        let Ok(content) = std::fs::read_to_string(&style_path) else { continue };
        let Ok(mut style) = serde_json::from_str::<serde_json::Value>(&content) else { continue };

        rewrite_sources(&mut style);
        rewrite_sprites(&mut style, &sprites_path);
        rewrite_glyphs(&mut style, &fonts_path);

        let local_path = styles_dir.join(format!("{style_name}_local.json"));
        let _ = std::fs::write(&local_path, serde_json::to_string(&style).unwrap_or_default());
    }
}

fn rewrite_sources(style: &mut serde_json::Value) {
    let Some(sources) = style.get_mut("sources").and_then(|s| s.as_object_mut()) else { return };
    for src in sources.values_mut() {
        if let Some(u) = src.get("url").and_then(|v| v.as_str()).filter(|u| u.starts_with('/')).map(String::from) {
            src["url"] = serde_json::json!(format!("http://127.0.0.1:17855{u}"));
        }
        if let Some(arr) = src.get_mut("tiles").and_then(|v| v.as_array_mut()) {
            for t in arr.iter_mut() {
                if let Some(u) = t.as_str().filter(|u| u.starts_with('/')).map(String::from) {
                    *t = serde_json::json!(format!("http://127.0.0.1:17855{u}"));
                }
            }
        }
    }
}

fn rewrite_sprites(style: &mut serde_json::Value, sprites_path: &str) {
    let Some(arr) = style.get_mut("sprite").and_then(|s| s.as_array_mut()) else { return };
    for s in arr.iter_mut() {
        if s.get("url").and_then(|v| v.as_str()).is_some_and(|u| u.starts_with('/')) {
            s["url"] = serde_json::json!(format!("file://{sprites_path}/default"));
        }
    }
}

fn rewrite_glyphs(style: &mut serde_json::Value, fonts_path: &str) {
    if style.get("glyphs").and_then(|v| v.as_str()).is_some() {
        style["glyphs"] = serde_json::json!(format!("file://{fonts_path}/{{fontstack}}/{{range}}.pbf"));
    }
}

fn create_vector_mbtiles(path: &str, bounds: (f64, f64, f64, f64)) -> rusqlite::Result<Connection> {
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
    let meta = [
        ("name", "OpenRailwayMap Offline".to_string()),
        ("format", "pbf".into()),
        ("type", "overlay".into()),
        ("bounds", format!("{west},{south},{east},{north}")),
    ];
    for (k, v) in &meta {
        conn.execute("INSERT OR REPLACE INTO metadata (name, value) VALUES (?1, ?2)", [k, v.as_str()])?;
    }
    Ok(conn)
}

fn tile_hash(data: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, ()> {
    let resp = client.get(url).send().await.map_err(|_| ())?;
    if !resp.status().is_success() { return Err(()); }
    resp.bytes().await.map(|b| b.to_vec()).map_err(|_| ())
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
                if attempt < MAX_RETRIES {
                    let retry_after = resp.headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(5);
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

fn emit_progress(app: &tauri::AppHandle, progress: &DownloadProgress) {
    let _ = app.emit("tile-download-progress", ProgressEvent {
        total: progress.total.load(Ordering::Relaxed),
        completed: progress.completed.load(Ordering::Relaxed),
        failed: progress.failed.load(Ordering::Relaxed),
        bytes: progress.bytes.load(Ordering::Relaxed),
    });
}

#[tauri::command]
pub async fn download_orm_tiles(
    app: tauri::AppHandle,
    south: f64,
    west: f64,
    north: f64,
    east: f64,
    z_min: u8,
    z_max: u8,
) -> Result<String, String> {
    use tauri_plugin_dialog::DialogExt;

    let dir = app.dialog()
        .file()
        .blocking_pick_folder()
        .ok_or("No folder selected")?
        .to_string();

    let dir = PathBuf::from(dir).join("orm_offline");

    let progress = Arc::new(DownloadProgress {
        total: AtomicU64::new(0),
        completed: AtomicU64::new(0),
        failed: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
        throttled: AtomicBool::new(false),
    });

    {
        let state = app.state::<crate::mbtiles::DownloadState>();
        *state.active.lock().unpoison() = Some(Arc::clone(&progress));
    }

    let result = download_orm_offline(
        app.clone(), dir, south, west, north, east, z_min, z_max, progress,
    ).await;

    {
        let state = app.state::<crate::mbtiles::DownloadState>();
        *state.active.lock().unpoison() = None;
    }

    result.map(|p| p.to_string_lossy().to_string())
}
