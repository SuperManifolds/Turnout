use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use rusqlite::Connection;
use serde_json::{Map, Value};
use tauri::{Emitter, Manager};

use crate::mbtiles::{DownloadProgress, ProgressEvent};
use crate::tile_server::UnpoisonExt;

/// Union of every distinct MVT source layer referenced by the composite
/// `openrailwaymap.app/<layers>/{z}/{x}/{y}` tile URLs across all seven bundled
/// styles. Downloaded together into a single combined-blob `MBTiles`.
const ORM_LAYERS: &[&str] = &[
    "electrification_catenary",
    "electrification_railway_line_low",
    "electrification_railway_symbols",
    "electrification_signals",
    "electrification_substation",
    "operator_railway_line_low",
    "operator_railway_symbols",
    "railway_line_high",
    "railway_text_km",
    "route_railway_line_low",
    "signals_railway_line_low",
    "signals_railway_line_low_construction",
    "signals_railway_signals",
    "signals_signal_boxes",
    "speed_railway_line_low",
    "speed_railway_signals",
    "standard_railway_grouped_station_areas",
    "standard_railway_grouped_stations",
    "standard_railway_line_low",
    "standard_railway_platform_edges",
    "standard_railway_platforms",
    "standard_railway_stop_positions",
    "standard_railway_switch_ref",
    "standard_railway_symbols",
    "standard_railway_text_stations",
    "standard_railway_text_stations_low",
    "standard_railway_text_stations_med",
    "standard_railway_turntables",
    "standard_station_entrances",
    "track_railway_line_low",
];

/// Zoom-gated layer groups are individually cheap (≤8 layers, like the online
/// renderer's sources), so vector requests run as wide as raster and without a
/// fixed pacing floor — `fetch_with_retry` still backs off on HTTP 429.
/// Lowest zoom fetched offline. `OpenRailwayMap`'s `*_railway_line_low` endpoints
/// (the only sources below z7) hang server-side, and a sub-z7 tile spans whole
/// continents anyway — useless for an area-scoped offline bundle. z7+ is served by
/// the healthy `railway_line_high` source.
const ORM_MIN_ZOOM: u8 = 7;
const CONCURRENT_REQUESTS: usize = 64;
/// Upper bound on layers packed into a single composite tile request. At z7+ (the
/// only zooms fetched) every in-range layer — at most ~20 — fits in one request,
/// so each tile is a single fetch. The 30-layer composite that timed out did so
/// only because it included the dead `*_railway_line_low` layers, which zoom-gating
/// now excludes; the healthy z7+ layers compose fine in one request.
const MAX_LAYERS_PER_REQUEST: usize = 32;
/// Tiles the writer thread accumulates before committing a transaction. A commit
/// also fires once `WRITER_COMMIT_INTERVAL_MS` elapses so a slow trickle still
/// flushes promptly. Kept well above `CONCURRENT_REQUESTS` so a full window's worth
/// of stores amortizes into one journal flush.
const WRITER_COMMIT_TILES: usize = 512;
const WRITER_COMMIT_INTERVAL_MS: u64 = 1_000;
/// Bound on tiles queued to the writer thread, back-pressuring the fetch stream if
/// sqlite falls behind so in-flight memory stays bounded without a batch barrier.
const WRITER_QUEUE_CAP: usize = 2_048;
/// Progress is emitted from the fetch loop on whichever of these comes first, so the
/// UI advances smoothly instead of jumping once per transaction.
const PROGRESS_EMIT_TILES: usize = 64;
const PROGRESS_EMIT_INTERVAL_MS: u64 = 250;
const MAX_RETRIES: u32 = 5;
const INITIAL_RETRY_DELAY_MS: u64 = 500;
const MAX_RETRY_DELAY_MS: u64 = 10_000;
const HTTP_TIMEOUT_SECS: u64 = 15;
const CONNECT_TIMEOUT_SECS: u64 = 5;
/// Shared 429 backoff bounds. A throttled response bumps a delay every request
/// consults before sending; sustained success decays it back toward zero.
const THROTTLE_MIN_MS: u64 = 2_000;
const THROTTLE_MAX_MS: u64 = 30_000;
const THROTTLE_FLOOR_MS: u64 = 100;

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

    let orm_base = crate::settings::resolve_orm_base(crate::settings::load(&app).orm_base_url.as_deref());

    // HTTP/1.1 with a warm connection pool, NOT HTTP/2: the tile endpoint is
    // latency-bound, and a pool of independent connections parallelizes far better
    // than HTTP/2 multiplexing over a single connection (measured ~1.5x faster, and
    // the gap widens with latency).
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .user_agent("Turnout/0.2.0 (+https://github.com/SuperManifolds/Turnout)")
        .http1_only()
        .pool_max_idle_per_host(CONCURRENT_REQUESTS)
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    let z_min = z_min.max(ORM_MIN_ZOOM);
    let mut tiles: Vec<(u8, u32, u32)> = Vec::new();
    // Per-zoom tile bbox `(z, x_min, x_max, y_min, y_max)`, retained so the resume
    // scan can query only this job's tile range instead of the whole archive.
    let mut zoom_ranges: Vec<(u8, u32, u32, u32, u32)> = Vec::new();
    for z in z_min..=z_max {
        let (x_min, y_min) = latlon_to_tile_xy(north, west, z);
        let (x_max, y_max) = latlon_to_tile_xy(south, east, z);
        zoom_ranges.push((z, x_min, x_max, y_min, y_max));
        for x in x_min..=x_max {
            for y in y_min..=y_max {
                tiles.push((z, x, y));
            }
        }
    }

    // Resources (sprites + glyph ranges) are counted into the same progress total
    // so the bar advances from the first file rather than sitting at 0/0 while
    // ~1000 glyph requests run.
    let resources = resource_list(&dir, &orm_base).map_err(|e| format!("Failed to prepare resources: {e}"))?;
    progress.total.store((tiles.len() + resources.len()) as u64, Ordering::Relaxed);
    emit_progress(&app, &progress);

    // Phase 1: sprites + fonts (concurrent, reports progress)
    download_resources(&app, &client, &resources, &progress).await?;

    // Phase 2: Download MVT tiles to MBTiles
    let mbtiles_path = dir.join("tiles.mbtiles");
    let conn = create_vector_mbtiles(
        mbtiles_path.to_str().unwrap_or("tiles.mbtiles"),
        (west, south, east, north),
        z_min,
        z_max,
    )
    .map_err(|e| format!("Failed to create MBTiles: {e}"))?;

    // Resume support: drop tiles already stored (e.g. from an aborted run). They
    // still count toward `total`, so pre-credit them as completed to keep the bar
    // honest. `map.tile_row` is TMS, so compare against each tile's flipped row.
    // The scan is scoped to this job's tile range and run off the async thread.
    let (conn, existing) = tokio::task::spawn_blocking(move || {
        let existing = existing_tile_rows(&conn, &zoom_ranges);
        (conn, existing)
    })
    .await
    .map_err(|e| format!("Resume scan failed: {e}"))?;
    if !existing.is_empty() {
        let before = tiles.len();
        tiles.retain(|&(z, x, y)| !existing.contains(&(z, x, (1u32 << z) - 1 - y)));
        let already = (before - tiles.len()) as u64;
        progress.completed.fetch_add(already, Ordering::Relaxed);
        emit_progress(&app, &progress);
    }

    // Each zoom level fetches only the layers valid there, in small groups, so no
    // single request carries the timeout-prone 30-layer composite. Plans are keyed
    // by zoom and computed once rather than per tile.
    let ranges = derive_layer_zoom_ranges();
    let plans: HashMap<u8, Vec<String>> = (z_min..=z_max)
        .map(|z| (z, zoom_group_plan(z, z_max, &ranges)))
        .collect();

    let throttle = Arc::new(Throttle::new());

    // A dedicated writer thread owns the connection and groups incoming tiles into
    // transactions, so sqlite commits overlap fetching instead of stalling the
    // request window at a per-batch barrier.
    let (tx, rx) = std::sync::mpsc::sync_channel::<StoreItem>(WRITER_QUEUE_CAP);
    let writer_progress = Arc::clone(&progress);
    let writer = std::thread::spawn(move || run_writer(conn, &rx, &writer_progress));

    // Tiles whose zoom has no applicable layers are never fetched; credit them so
    // the bar reaches `total`.
    for &(z, x, y) in &tiles {
        if plans.get(&z).is_none_or(Vec::is_empty) {
            let _ = tx.send(StoreItem { z, x, y, groups: Vec::new() });
        }
    }

    // One request unit per (tile, layer group). Results stream back out of order;
    // multi-group tiles are held in `partial` until all their groups arrive, then
    // handed to the writer. At z7+ every tile is a single group and skips buffering.
    let units: Vec<(usize, usize, String)> = tiles
        .iter()
        .enumerate()
        .flat_map(|(ti, &(z, x, y))| {
            let orm_base = orm_base.clone();
            plans
                .get(&z)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(move |(gi, group)| (ti, gi, format!("{orm_base}/{group}/{z}/{x}/{y}")))
        })
        .collect();

    // Keep `CONCURRENT_REQUESTS` fetches continuously in flight. The pause gate lives
    // inside each request future, so a pause stops issuing new requests promptly while
    // in-flight ones drain; a slow tile occupies one slot rather than stalling a batch.
    let mut stream = futures::stream::iter(units)
        .map(|(ti, gi, url)| {
            let client = client.clone();
            let throttle = Arc::clone(&throttle);
            let progress = Arc::clone(&progress);
            async move {
                progress.wait_while_paused().await;
                (ti, gi, fetch_with_retry(&client, &url, &throttle).await)
            }
        })
        .buffer_unordered(CONCURRENT_REQUESTS);

    let mut partial: HashMap<usize, Vec<Option<FetchResult>>> = HashMap::new();
    let mut since_emit = 0usize;
    let mut last_emit = Instant::now();
    let cancelled = loop {
        let Some((ti, gi, result)) = stream.next().await else { break false };
        if progress.cancelled.load(Ordering::Relaxed) {
            break true;
        }
        let (z, x, y) = tiles[ti];
        let group_count = plans.get(&z).map_or(0, Vec::len);
        let done = if group_count <= 1 {
            Some(vec![Some(result)])
        } else {
            let slot = partial.entry(ti).or_insert_with(|| (0..group_count).map(|_| None).collect());
            slot[gi] = Some(result);
            if slot.iter().all(Option::is_some) {
                partial.remove(&ti)
            } else {
                None
            }
        };
        if let Some(groups) = done {
            if tx.send(StoreItem { z, x, y, groups }).is_err() {
                break true;
            }
            since_emit += 1;
        }
        if since_emit >= PROGRESS_EMIT_TILES
            || last_emit.elapsed() >= Duration::from_millis(PROGRESS_EMIT_INTERVAL_MS)
        {
            emit_progress(&app, &progress);
            since_emit = 0;
            last_emit = Instant::now();
        }
    };

    // Stop fetching, then close the channel so the writer flushes its open transaction.
    drop(stream);
    drop(tx);
    let conn = writer.join().map_err(|_| "Tile writer thread panicked".to_string())?;
    finalize_vector_mbtiles(&conn).map_err(|e| format!("Failed to finalize MBTiles: {e}"))?;
    emit_progress(&app, &progress);

    if cancelled {
        return Err("Download cancelled".into());
    }

    // Phase 3: Generate self-contained offline styles pointing at local resources
    generate_offline_styles(&dir)?;

    Ok(dir)
}

/// A completed tile handed to the writer thread. `groups` are the per-layer-group
/// fetch outcomes in request order; empty means the tile had no applicable layers.
struct StoreItem {
    z: u8,
    x: u32,
    y: u32,
    groups: Vec<Option<FetchResult>>,
}

/// Owns the connection and drains stored tiles into transactions, committing every
/// `WRITER_COMMIT_TILES` tiles or `WRITER_COMMIT_INTERVAL_MS`, whichever comes first,
/// so a single journal flush amortizes many inserts. Returns the connection so the
/// caller can finalize it.
fn run_writer(
    conn: Connection,
    rx: &std::sync::mpsc::Receiver<StoreItem>,
    progress: &DownloadProgress,
) -> Connection {
    let interval = Duration::from_millis(WRITER_COMMIT_INTERVAL_MS);
    let mut in_txn = false;
    let mut since_commit = 0usize;
    let mut txn_start = Instant::now();
    loop {
        match rx.recv_timeout(interval) {
            Ok(item) => {
                if !in_txn {
                    let _ = conn.execute_batch("BEGIN");
                    in_txn = true;
                    txn_start = Instant::now();
                }
                store_tile(&conn, progress, item.z, item.x, item.y, &item.groups);
                since_commit += 1;
                if since_commit >= WRITER_COMMIT_TILES || txn_start.elapsed() >= interval {
                    let _ = conn.execute_batch("COMMIT");
                    in_txn = false;
                    since_commit = 0;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if in_txn {
                    let _ = conn.execute_batch("COMMIT");
                    in_txn = false;
                    since_commit = 0;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if in_txn {
        let _ = conn.execute_batch("COMMIT");
    }
    conn
}

const FONTS: &[&str] =
    &["OpenRailwayMap-Bold", "OpenRailwayMap-Regular", "OpenRailwayMap-Italic", "FiraCode-Bold"];

/// Builds the (url, destination) list of sprite and glyph resources to download,
/// creating the target directories. Glyph ranges that the server lacks return 404
/// and are skipped at fetch time; the whole 0..65535 space is attempted because
/// coverage varies per font.
fn resource_list(dir: &Path, orm_base: &str) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut items = Vec::new();

    let sprites_dir = dir.join("sprites");
    std::fs::create_dir_all(&sprites_dir)?;
    for (name, prefix) in [("default", "sprite"), ("sdf", "sdf_sprite")] {
        for suffix in ["", "@2x"] {
            for ext in ["json", "png"] {
                items.push((
                    format!("{orm_base}/{prefix}/symbols{suffix}.{ext}"),
                    sprites_dir.join(format!("{name}{suffix}.{ext}")),
                ));
            }
        }
    }

    let fonts_dir = dir.join("fonts");
    for font in FONTS {
        let font_dir = fonts_dir.join(font);
        std::fs::create_dir_all(&font_dir)?;
        for range_start in (0u32..65536).step_by(256) {
            let range = format!("{range_start}-{}", range_start + 255);
            items.push((
                format!("{orm_base}/font/{font}/{range}.pbf"),
                font_dir.join(format!("{range}.pbf")),
            ));
        }
    }

    Ok(items)
}

/// Downloads sprite and glyph resources concurrently, writing each to its
/// destination and advancing `progress` (missing glyph ranges count as completed,
/// not failed — their absence is expected).
async fn download_resources(
    app: &tauri::AppHandle,
    client: &reqwest::Client,
    resources: &[(String, PathBuf)],
    progress: &DownloadProgress,
) -> Result<(), String> {
    for chunk in resources.chunks(CONCURRENT_REQUESTS) {
        if progress.cancelled.load(Ordering::Relaxed) {
            return Err("Download cancelled".into());
        }

        let futures = chunk.iter().map(|(url, dest)| async move {
            if let Ok(bytes) = fetch_bytes(client, url).await {
                let _ = std::fs::write(dest, &bytes);
                bytes.len() as u64
            } else {
                0
            }
        });
        let sizes = futures::future::join_all(futures).await;

        let bytes: u64 = sizes.iter().sum();
        progress.bytes.fetch_add(bytes, Ordering::Relaxed);
        progress.completed.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        emit_progress(app, progress);
    }
    Ok(())
}

/// Rewrites the bundled styles into fully self-contained offline styles that
/// reference the local `MBTiles`, sprites and fonts, and writes them to
/// `<dir>/styles/{name}_offline.json`.
fn generate_offline_styles(dir: &Path) -> Result<(), String> {
    let styles_dir = dir.join("styles");
    std::fs::create_dir_all(&styles_dir).map_err(|e| e.to_string())?;

    let dir_str = dir.to_string_lossy();
    let orm_source = serde_json::json!({
        "type": "vector",
        "url": format!("mbtiles://{dir_str}/tiles.mbtiles"),
    });
    let sprite = serde_json::json!([
        { "id": "default", "url": format!("file://{dir_str}/sprites/default") },
        { "id": "sdf", "url": format!("file://{dir_str}/sprites/sdf") },
    ]);
    let glyphs = Value::String(format!("file://{dir_str}/fonts/{{fontstack}}/{{range}}.pbf"));

    for (name, raw) in crate::orm_tiles::STYLES {
        let mut style: Value = serde_json::from_str(raw).map_err(|e| format!("{name}: {e}"))?;
        rewrite_offline_style(&mut style, &orm_source, &sprite, &glyphs);
        let out = styles_dir.join(format!("{name}_offline.json"));
        let json = serde_json::to_string(&style).map_err(|e| format!("{name}: {e}"))?;
        std::fs::write(out, json).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

fn rewrite_offline_style(style: &mut Value, orm_source: &Value, sprite: &Value, glyphs: &Value) {
    let (replaced, dropped) = rebuild_sources(style, orm_source);
    rewrite_layers(style, &replaced, &dropped);
    style["sprite"] = sprite.clone();
    style["glyphs"] = glyphs.clone();
}

/// True for vector sources whose tiles are served from `openrailwaymap.app`.
fn is_orm_vector_source(src: &Value) -> bool {
    src.get("type").and_then(Value::as_str) == Some("vector")
        && src
            .get("tiles")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .is_some_and(|t| t.contains("openrailwaymap.app"))
}

/// Extracts the comma-separated layer path (the segment between
/// `openrailwaymap.app/` and `/{z}`) from an ORM vector source's tile template.
fn orm_layer_segment(src: &Value) -> Option<&str> {
    let tmpl = src.get("tiles")?.as_array()?.first()?.as_str()?;
    let after_host = tmpl.split_once("openrailwaymap.app/")?.1;
    Some(after_host.split_once("/{z}")?.0)
}

/// Unions two optional max-zooms where `None` means "unbounded above" and wins.
fn union_max(a: Option<u8>, b: Option<u8>) -> Option<u8> {
    match (a, b) {
        (None, _) | (_, None) => None,
        (Some(x), Some(y)) => Some(x.max(y)),
    }
}

/// Derives each source layer's zoom range from the bundled styles. A layer present
/// in several sources gets the union of ranges (min of minzooms, max of maxzooms,
/// where an unbounded `None` maxzoom wins). `None` max is resolved to the requested
/// `z_max` at plan time. Mirrors how the online renderer zoom-gates its sources.
fn derive_layer_zoom_ranges() -> HashMap<String, (u8, Option<u8>)> {
    let mut ranges: HashMap<String, (u8, Option<u8>)> = HashMap::new();
    for (_, raw) in crate::orm_tiles::STYLES {
        let Ok(style) = serde_json::from_str::<Value>(raw) else { continue };
        let Some(sources) = style.get("sources").and_then(Value::as_object) else { continue };
        for src in sources.values().filter(|s| is_orm_vector_source(s)) {
            let Some(segment) = orm_layer_segment(src) else { continue };
            let min = src.get("minzoom").and_then(Value::as_u64).unwrap_or(0) as u8;
            let max = src.get("maxzoom").and_then(Value::as_u64).map(|v| v as u8);
            for layer in segment.split(',') {
                ranges
                    .entry(layer.to_string())
                    .and_modify(|(mn, mx)| {
                        *mn = (*mn).min(min);
                        *mx = union_max(*mx, max);
                    })
                    .or_insert((min, max));
            }
        }
    }
    debug_assert_eq!(
        ranges.keys().map(String::as_str).collect::<HashSet<_>>(),
        ORM_LAYERS.iter().copied().collect::<HashSet<_>>(),
        "derived ORM layer set diverged from ORM_LAYERS"
    );
    ranges
}

/// Sorted, chunked layer groups to request for a tile at zoom `z`. Each group is a
/// comma-joined list of at most `MAX_LAYERS_PER_REQUEST` layers valid at `z`. An
/// empty result means no layer applies and the tile is skipped entirely.
fn zoom_group_plan(z: u8, z_max: u8, ranges: &HashMap<String, (u8, Option<u8>)>) -> Vec<String> {
    let mut layers: Vec<&str> = ranges
        .iter()
        .filter(|(_, range)| {
            let (min, max) = **range;
            min <= z && z <= max.unwrap_or(z_max)
        })
        .map(|(layer, _)| layer.as_str())
        .collect();
    layers.sort_unstable();
    layers.chunks(MAX_LAYERS_PER_REQUEST).map(|group| group.join(",")).collect()
}

/// Concatenates a tile's fetched layer-group bodies in group order — a valid MVT
/// merge, since a serialized `Tile` is `repeated Layer`, so joined messages combine
/// their layers — and inserts the combined blob, advancing `progress`. A tile with
/// no applicable layers or an all-empty result is counted complete without storing;
/// a tile with any group that failed after retries is counted failed.
fn store_tile(
    conn: &Connection,
    progress: &DownloadProgress,
    z: u8,
    x: u32,
    y: u32,
    groups: &[Option<FetchResult>],
) {
    let mut blob = Vec::new();
    for group in groups {
        match group {
            Some(FetchResult::Ok(data)) => blob.extend_from_slice(data),
            Some(FetchResult::NotFound) => {}
            _ => {
                progress.failed.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }
    if !blob.is_empty() {
        let tms_y = (1u32 << z) - 1 - y;
        let hash = tile_hash(&blob);
        if let Ok(mut stmt) =
            conn.prepare_cached("INSERT OR IGNORE INTO images (tile_id, tile_data) VALUES (?1, ?2)")
        {
            let _ = stmt.execute(rusqlite::params![hash, blob]);
        }
        if let Ok(mut stmt) = conn.prepare_cached(
            "INSERT OR REPLACE INTO map (zoom_level, tile_column, tile_row, tile_id) VALUES (?1, ?2, ?3, ?4)",
        ) {
            let _ = stmt.execute(rusqlite::params![z, x, tms_y, hash]);
        }
        progress.bytes.fetch_add(blob.len() as u64, Ordering::Relaxed);
    }
    progress.completed.fetch_add(1, Ordering::Relaxed);
}

/// Collapses every ORM vector source into a single `orm` source, keeps geojson
/// sources, and drops network-only sources (e.g. `openhistoricalmap`, `dem`).
/// Returns the set of replaced source names and the set of dropped source names.
fn rebuild_sources(style: &mut Value, orm_source: &Value) -> (HashSet<String>, HashSet<String>) {
    let mut replaced = HashSet::new();
    let mut dropped = HashSet::new();
    let Some(sources) = style.get_mut("sources").and_then(Value::as_object_mut) else {
        return (replaced, dropped);
    };

    let mut rebuilt = Map::new();
    for (name, src) in sources.iter() {
        match src.get("type").and_then(Value::as_str) {
            Some("geojson") => {
                rebuilt.insert(name.clone(), src.clone());
            }
            _ if is_orm_vector_source(src) => {
                replaced.insert(name.clone());
            }
            _ => {
                dropped.insert(name.clone());
            }
        }
    }
    rebuilt.insert("orm".into(), orm_source.clone());
    *sources = rebuilt;
    (replaced, dropped)
}

/// Points layers at the combined `orm` source and removes layers that referenced
/// dropped network-only sources.
fn rewrite_layers(style: &mut Value, replaced: &HashSet<String>, dropped: &HashSet<String>) {
    let Some(layers) = style.get_mut("layers").and_then(Value::as_array_mut) else { return };
    layers.retain(|l| {
        l.get("source")
            .and_then(Value::as_str)
            .is_none_or(|s| !dropped.contains(s))
    });
    for layer in layers.iter_mut() {
        let is_replaced = layer
            .get("source")
            .and_then(Value::as_str)
            .is_some_and(|s| replaced.contains(s));
        if is_replaced {
            layer["source"] = Value::String("orm".into());
        }
    }
}

/// Reads the `(zoom, column, TMS row)` keys of already-stored tiles that fall inside
/// this job's per-zoom tile range so a resumed download can skip them. Scoping the
/// query to the current bbox avoids scanning the whole `map` table. `map.tile_row` is
/// TMS, so each range's y bounds are flipped to `[(1<<z)-1-y_max, (1<<z)-1-y_min]`.
/// Returns an empty set on any query error (fresh or unreadable db) — the download
/// then simply re-fetches everything.
fn existing_tile_rows(conn: &Connection, ranges: &[(u8, u32, u32, u32, u32)]) -> HashSet<(u8, u32, u32)> {
    let mut set = HashSet::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT zoom_level, tile_column, tile_row FROM map
         WHERE zoom_level = ?1 AND tile_column BETWEEN ?2 AND ?3 AND tile_row BETWEEN ?4 AND ?5",
    ) else {
        return set;
    };
    for &(z, x_min, x_max, y_min, y_max) in ranges {
        let tms_lo = (1u32 << z) - 1 - y_max;
        let tms_hi = (1u32 << z) - 1 - y_min;
        let Ok(rows) = stmt.query_map(rusqlite::params![z, x_min, x_max, tms_lo, tms_hi], |r| {
            Ok((r.get::<_, u8>(0)?, r.get::<_, u32>(1)?, r.get::<_, u32>(2)?))
        }) else {
            continue;
        };
        set.extend(rows.flatten());
    }
    set
}

fn create_vector_mbtiles(
    path: &str,
    bounds: (f64, f64, f64, f64),
    z_min: u8,
    z_max: u8,
) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    // WAL + relaxed sync lets the writer commit without an fsync per transaction;
    // `finalize_vector_mbtiles` checkpoints and reverts to a self-contained file.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
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
    let vector_layers: Vec<Value> = ORM_LAYERS
        .iter()
        .map(|l| serde_json::json!({ "id": l, "fields": {} }))
        .collect();
    let json_meta = serde_json::json!({ "vector_layers": vector_layers }).to_string();
    let meta = [
        ("name", "OpenRailwayMap Offline".to_string()),
        ("format", "pbf".into()),
        ("type", "overlay".into()),
        ("bounds", format!("{west},{south},{east},{north}")),
        ("minzoom", z_min.to_string()),
        ("maxzoom", z_max.to_string()),
        ("json", json_meta),
    ];
    for (k, v) in &meta {
        conn.execute("INSERT OR REPLACE INTO metadata (name, value) VALUES (?1, ?2)", [k, v.as_str()])?;
    }
    Ok(conn)
}

/// Checkpoints the WAL into the main db and reverts to a rollback journal so the
/// finished `.mbtiles` is a single self-contained file with no `-wal`/`-shm`
/// sidecars — read-only consumers open it without needing shared-memory access.
fn finalize_vector_mbtiles(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
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

/// Shared, adaptive 429 backoff consulted by every request. A single throttled
/// response slows the whole fetch window instead of each slot sleeping in isolation
/// (which silently collapses concurrency); sustained success decays the delay back
/// toward zero. Decay is wall-clock gated so a burst of quick successes can't undo a
/// throttle before the server has recovered.
struct Throttle {
    start: Instant,
    delay_ms: AtomicU64,
    decay_after_ms: AtomicU64,
}

impl Throttle {
    fn new() -> Self {
        Self { start: Instant::now(), delay_ms: AtomicU64::new(0), decay_after_ms: AtomicU64::new(0) }
    }

    fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Sleeps the current shared delay before a request is sent.
    async fn wait(&self) {
        let delay = self.delay_ms.load(Ordering::Relaxed);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    }

    /// Raises the shared delay on a 429, honoring the server's `retry-after` as a
    /// floor and holding it for at least that long before any decay.
    fn on_throttled(&self, retry_after_secs: u64) {
        let current = self.delay_ms.load(Ordering::Relaxed);
        let bumped = (current.max(THROTTLE_MIN_MS) * 2)
            .max(retry_after_secs.saturating_mul(1_000))
            .min(THROTTLE_MAX_MS);
        self.delay_ms.store(bumped, Ordering::Relaxed);
        self.decay_after_ms.store(self.elapsed_ms() + bumped, Ordering::Relaxed);
    }

    /// Decays the shared delay once the hold window has elapsed, so throughput ramps
    /// back up after the server stops throttling.
    fn on_success(&self) {
        let current = self.delay_ms.load(Ordering::Relaxed);
        if current == 0 || self.elapsed_ms() < self.decay_after_ms.load(Ordering::Relaxed) {
            return;
        }
        let next = current / 2;
        let next = if next < THROTTLE_FLOOR_MS { 0 } else { next };
        self.delay_ms.store(next, Ordering::Relaxed);
        self.decay_after_ms.store(self.elapsed_ms() + next.max(THROTTLE_FLOOR_MS), Ordering::Relaxed);
    }
}

async fn fetch_with_retry(client: &reqwest::Client, url: &str, throttle: &Throttle) -> FetchResult {
    let mut delay = Duration::from_millis(INITIAL_RETRY_DELAY_MS);
    for attempt in 0..=MAX_RETRIES {
        throttle.wait().await;
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                throttle.on_success();
                return resp.bytes().await
                    .map_or(FetchResult::Failed, |b| FetchResult::Ok(b.to_vec()));
            }
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                throttle.on_success();
                return FetchResult::NotFound;
            }
            Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                let retry_after = resp.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(5);
                throttle.on_throttled(retry_after);
                if attempt < MAX_RETRIES {
                    continue;
                }
                return FetchResult::Throttled;
            }
            _ if attempt < MAX_RETRIES => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(MAX_RETRY_DELAY_MS));
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

    let progress = Arc::new(DownloadProgress::new());

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
