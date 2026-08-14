use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::error::{CommandError, CommandResult};
use crate::server_core::{self, UnpoisonExt};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use lru::LruCache;
use tiny_skia::Pixmap;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;
use turnout_core::kml::OverlayData;
use turnout_core::pop_edit::{
    brush_delta, brush_pixels, region_bounds, Blend, BrushMode, LayerInfo, PopLayers, TILE_PX,
};
use turnout_core::pop_import::GridAccumulator;

/// Outcome of importing external population data as a layer.
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PopImportResult {
    /// Id of the new import layer.
    pub id: u32,
    /// Number of z10 pixels the source covered.
    pub covered_pixels: u64,
    /// Total people read from the source (before value-space scaling).
    pub source_total: f64,
    /// The scale applied to match the source to the base density.
    pub applied_scale: f64,
    /// The updated layer stack (top-first).
    pub layers: Vec<LayerInfo>,
}

mod fetch;
mod render;

use fetch::{
    RemoteReq, decode_remote_bytes, decoded_to_pixmap, fetch_arcgis_tile, fetch_wms_tile,
    fetch_xyz_tile, get_remote_cached, mbtiles_conn, put_remote_cached, read_mbtiles_tile,
};
use render::render_tile;

pub(crate) use fetch::xyz_to_quadkey;

const TILE_SIZE: u32 = 256;
const RENDER_CACHE_CAPACITY: usize = 512;
const REMOTE_CACHE_CAPACITY: usize = 2048;
/// Max cached open `MBTiles` read connections; bounds file descriptors when many
/// distinct `MBTiles` layers are added. Evicting closes the connection once unused.
const MBTILES_CONN_CACHE: usize = 16;
/// Max cached open population-map readers. One per distinct `pop400.pmtiles`
/// path; in practice a single file, but bounded like the `MBTiles` cache.
const POP_READER_CACHE: usize = 4;
/// Upper bound on a region operation's pixel count, so a huge selection can't
/// stall the server iterating tens of millions of pixels (~a 230 km box).
const MAX_REGION_PIXELS: u64 = 8_000_000;
/// Cap on z10 tiles a single import may materialize (~0.36 MB each): a guard so a
/// too-large import fails with a message instead of exhausting memory. ~30k tiles
/// ≈ 11 GB covers the continental US (≈12k) with headroom.
const MAX_IMPORT_TILES: usize = 30_000;
/// Target covered-pixel samples when estimating base mean for value matching.
const MEAN_SAMPLE_TARGET: u64 = 20_000;
/// Cap on base tiles decoded while estimating that mean.
const MAX_MEAN_TILES: usize = 400;
pub const PREFERRED_PORT: u16 = 17853;
const MAX_ZOOM: u8 = 22;
const HTTP_TIMEOUT_SECS: u64 = 15;
/// Substring unique to Apple's satellite tile host, used to tell a satellite Apple
/// layer from a standard-map one when a layer is added.
pub const APPLE_SAT_MARKER: &str = "sat-cdn";

const WEB_MERCATOR_EXTENT: (f64, f64, f64, f64) = (-180.0, -85.051_129, 180.0, 85.051_129);


pub(crate) struct DecodedImage {
    pixmap: Pixmap,
    north: f64,
    south: f64,
    east: f64,
    west: f64,
    rotation: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LayerKind {
    Kmz, Shp, GeoJson, Wms, ArcGis, Xyz, Wmts, Apple, Bing, MbTiles, Pop,
}

/// A layer's data source: the single, self-describing, serializable definition of
/// where a layer's tiles come from. This is both the live description and the
/// persisted form — there is no parallel "saved" type. Runtime-only data (parsed
/// file geometry + rasterized ground overlays for file layers, and Apple's live
/// tokenized URL) is NOT here; it lives in [`TileState::runtime`], keyed by layer
/// id, so the definition stays serializable and free of secrets.
#[derive(Clone)]
pub enum SourceDef {
    Kmz { path: Option<String> },
    Shp { path: Option<String> },
    GeoJson { path: Option<String> },
    Wms { base_url: String, layer_name: String },
    ArcGis { base_url: String, service_name: String },
    Xyz { url_template: String },
    Wmts { url_template: String },
    /// Apple imagery. Only whether it is satellite is persisted (via `SavedSource`);
    /// the tokenized URL is runtime state in [`TileState::runtime`].
    Apple { sat: bool },
    Bing { url_template: String },
    MbTiles { path: String, max_zoom: u8 },
    /// NIMBY Rails' population map (`pop400.pmtiles`), rendered as a colorized
    /// density heatmap.
    Pop { path: String },
}

impl SourceDef {
    pub fn kind(&self) -> LayerKind {
        match self {
            SourceDef::Kmz { .. } => LayerKind::Kmz,
            SourceDef::Shp { .. } => LayerKind::Shp,
            SourceDef::GeoJson { .. } => LayerKind::GeoJson,
            SourceDef::Wms { .. } => LayerKind::Wms,
            SourceDef::ArcGis { .. } => LayerKind::ArcGis,
            SourceDef::Xyz { .. } => LayerKind::Xyz,
            SourceDef::Wmts { .. } => LayerKind::Wmts,
            SourceDef::Apple { .. } => LayerKind::Apple,
            SourceDef::Bing { .. } => LayerKind::Bing,
            SourceDef::MbTiles { .. } => LayerKind::MbTiles,
            SourceDef::Pop { .. } => LayerKind::Pop,
        }
    }

    /// True for the file-backed kinds, whose geometry is rendered locally from
    /// [`RuntimeData::File`] rather than fetched as remote raster tiles.
    fn is_file(&self) -> bool {
        matches!(self, SourceDef::Kmz { .. } | SourceDef::Shp { .. } | SourceDef::GeoJson { .. })
    }
}

/// Runtime-only companion to a [`Layer`], held in [`TileState::runtime`] keyed by
/// layer id. Never persisted: it is rebuilt on restore (re-parse the file, or
/// rebuild Apple's URL from credentials).
pub enum RuntimeData {
    /// Parsed geometry + rasterized ground overlays for a file layer.
    File { data: OverlayData, images: Vec<DecodedImage> },
    /// The live tokenized Apple tile URL.
    Apple { url_template: String },
}

/// A layer: its data-source definition plus display state. The persisted form
/// lives in `overlay::SavedSource`, mapped 1:1 from `source` on save.
#[derive(Clone)]
pub struct Layer {
    pub id: u32,
    pub name: String,
    pub bbox: (f64, f64, f64, f64),
    pub visible: bool,
    pub opacity: f32,
    pub source: SourceDef,
}

impl Layer {
    pub fn kind(&self) -> LayerKind {
        self.source.kind()
    }
}

type DecodedTile = (Vec<u8>, u32, u32);
/// Decoded base z10 tiles for a region, keyed by tile: `(width, values)`.
type RegionBase = HashMap<(u32, u32), (u32, Vec<u16>)>;
type RemoteCache = LruCache<(u32, u8, u32, u32), DecodedTile>;

pub struct TileState {
    pub(crate) layers: RwLock<Vec<Layer>>,
    /// Runtime-only per-layer data (parsed file geometry / rasterized overlays, and
    /// Apple's live URL), keyed by layer id. Rebuilt on restore; never persisted.
    runtime: RwLock<HashMap<u32, RuntimeData>>,
    /// Composited PNG tiles keyed by `(z, x, y)`. Single-stripe (this server sees
    /// modest traffic); `Bytes` so a hit shares the buffer instead of copying.
    render_cache: server_core::TileCache<(u8, u32, u32)>,
    remote_cache: Mutex<RemoteCache>,
    pub(crate) error_layers: Mutex<std::collections::HashSet<u32>>,
    next_id: Mutex<u32>,
    port: u16,
    http: reqwest::Client,
    /// Path-keyed read-only `MBTiles` connections, reused across tile requests to
    /// avoid reopening the `SQLite` file on every fetch.
    mbtiles_conns: Mutex<LruCache<String, Arc<Mutex<rusqlite::Connection>>>>,
    /// Path-keyed memory-mapped population-map readers, reused across requests to
    /// avoid re-opening the 1.3 GB `pop400.pmtiles` on every tile.
    pop_readers: Mutex<LruCache<String, Arc<crate::pop_tiles::PopReader>>>,
    /// Live population edit layers overlaid on the base density when rendering the
    /// population layer. Empty on every server except the built-in population one.
    pub(crate) pop_layers: RwLock<PopLayers>,
    /// Paths of file-backed population source layers, keyed by their `pop_layers`
    /// layer id (a baked census/`WorldPop` `PMTiles` the renderer memory-maps).
    pop_sources: Mutex<HashMap<u32, String>>,
}

pub struct ServerHandle {
    pub port: u16,
    pub state: Arc<TileState>,
    pub shutdown_tx: watch::Sender<bool>,
}

impl ServerHandle {
    /// Adds a KMZ/Shp/GeoJson layer, returning its new id, or `None` when the file
    /// carries no drawable geometry (no bounding box) and nothing was added.
    pub fn add_kmz_layer(&self, data: OverlayData, path: Option<String>, kind: LayerKind) -> Option<u32> {
        let bbox = data.bbox()?;
        let name = data.name.clone().unwrap_or_else(|| "Overlay".to_string());
        let images = decode_images(&data);
        let id = self.next_id();
        let source = match kind {
            LayerKind::Shp => SourceDef::Shp { path },
            LayerKind::GeoJson => SourceDef::GeoJson { path },
            _ => SourceDef::Kmz { path },
        };
        self.state.runtime.write().unpoison().insert(id, RuntimeData::File { data, images });
        self.push_layer(Layer { id, name, bbox, visible: true, opacity: 1.0, source });
        Some(id)
    }

    pub fn add_wms_layer(&self, base_url: String, layer_name: String, display_name: String) -> u32 {
        let id = self.next_id();
        self.push_layer(Layer {
            id, name: display_name, bbox: WEB_MERCATOR_EXTENT, visible: true, opacity: 1.0,
            source: SourceDef::Wms { base_url, layer_name },
        });
        id
    }

    pub fn add_xyz_layer_with_kind(&self, url_template: String, display_name: String, kind: LayerKind) -> u32 {
        let id = self.next_id();
        let source = match kind {
            LayerKind::Wmts => SourceDef::Wmts { url_template },
            LayerKind::Bing => SourceDef::Bing { url_template },
            LayerKind::Apple => {
                let sat = url_template.contains(APPLE_SAT_MARKER);
                self.state.runtime.write().unpoison().insert(id, RuntimeData::Apple { url_template });
                SourceDef::Apple { sat }
            }
            _ => SourceDef::Xyz { url_template },
        };
        self.push_layer(Layer {
            id, name: display_name, bbox: WEB_MERCATOR_EXTENT, visible: true, opacity: 1.0, source,
        });
        id
    }

    pub fn add_arcgis_layer(&self, base_url: String, service_name: String, display_name: String) -> u32 {
        let id = self.next_id();
        self.push_layer(Layer {
            id, name: display_name, bbox: WEB_MERCATOR_EXTENT, visible: true, opacity: 1.0,
            source: SourceDef::ArcGis { base_url, service_name },
        });
        id
    }

    pub fn add_mbtiles_layer(&self, path: String, display_name: String) -> CommandResult<u32> {
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ).map_err(|e| CommandError::Io(format!("Failed to open MBTiles: {e}")))?;

        let bbox = conn.query_row(
            "SELECT value FROM metadata WHERE name = 'bounds'",
            [],
            |row| row.get::<_, String>(0),
        ).ok().and_then(|s| {
            let parts: Vec<f64> = s.split(',').filter_map(|p| p.parse().ok()).collect();
            if parts.len() == 4 { Some((parts[0], parts[1], parts[2], parts[3])) } else { None }
        }).unwrap_or(WEB_MERCATOR_EXTENT);

        let max_zoom = conn.query_row(
            "SELECT MAX(zoom_level) FROM tiles", [],
            |row| row.get::<_, u8>(0),
        ).unwrap_or(22);

        let id = self.next_id();
        self.push_layer(Layer {
            id, name: display_name, bbox, visible: true, opacity: 1.0,
            source: SourceDef::MbTiles { path, max_zoom },
        });
        Ok(id)
    }

    /// Add the population-density layer, backed by `path` (`pop400.pmtiles`).
    pub fn add_pop_layer(&self, path: String, display_name: String) -> u32 {
        let id = self.next_id();
        self.push_layer(Layer {
            id, name: display_name, bbox: WEB_MERCATOR_EXTENT, visible: true, opacity: 1.0,
            source: SourceDef::Pop { path },
        });
        id
    }

    /// Path of this server's population layer, if any (the built-in population
    /// server holds exactly one).
    fn pop_layer_path(&self) -> Option<String> {
        self.state.layers.read().unpoison().iter().find_map(|l| match &l.source {
            SourceDef::Pop { path } => Some(path.clone()),
            _ => None,
        })
    }

    /// Paint a brush *stroke* (a sequence of lon/lat points) onto the active edit
    /// layer as additive deltas. Overlapping dabs accumulate. When `clip` (a
    /// lon/lat selection bbox) is set, only pixels inside it are affected — the
    /// selection constrains the tool like a Photoshop marquee. Invalidates cache.
    pub fn pop_brush(
        &self,
        points: &[(f64, f64)],
        radius_m: f64,
        strength: u32,
        mode: BrushMode,
        clip: Option<(f64, f64, f64, f64)>,
    ) {
        let rect = clip.map(|(w, s, e, n)| region_bounds(w, s, e, n));

        // Take the strongest weight each pixel sees across the whole stroke rather
        // than summing every overlapping dab — so one stroke deposits at most
        // `strength` per pixel (build-up happens across separate strokes). Summing
        // dabs made even a low strength saturate almost immediately.
        let mut per_pixel: HashMap<(u32, u32, u16, u16), f64> = HashMap::new();
        for &(lon, lat) in points {
            for (p, weight) in brush_pixels(lon, lat, radius_m) {
                if let Some((gx0, gy0, gx1, gy1)) = rect {
                    let gx = p.tile_x * TILE_PX + u32::from(p.px);
                    let gy = p.tile_y * TILE_PX + u32::from(p.py);
                    if gx < gx0 || gx > gx1 || gy < gy0 || gy > gy1 {
                        continue;
                    }
                }
                let slot = per_pixel.entry((p.tile_x, p.tile_y, p.px, p.py)).or_insert(0.0);
                *slot = slot.max(weight);
            }
        }

        let mut layers = self.state.pop_layers.write().unpoison();
        for ((tx, ty, px, py), weight) in per_pixel {
            layers.add_delta(tx, ty, px, py, brush_delta(weight, strength, mode));
        }
        drop(layers);
        self.clear_cache();
    }

    /// The edit-layer stack (top-first).
    pub fn pop_list_layers(&self) -> Vec<LayerInfo> {
        self.state.pop_layers.read().unpoison().list()
    }

    /// Mutate the layer stack via `f`, then invalidate the render cache.
    fn with_pop_layers(&self, f: impl FnOnce(&mut PopLayers)) {
        f(&mut self.state.pop_layers.write().unpoison());
        self.clear_cache();
    }

    pub fn pop_add_layer(&self) {
        self.state.pop_layers.write().unpoison().add_layer();
    }

    /// Add a file-backed source layer reading the baked `PMTiles` at `path`. Fails
    /// if the archive can't be opened. Returns the updated stack.
    pub async fn pop_add_source_layer(
        &self,
        path: String,
        name: String,
        blend: Blend,
    ) -> Result<Vec<LayerInfo>, String> {
        // Validate the archive opens (and warm the reader cache).
        pop_reader(&self.state, &path).await.ok_or_else(|| format!("Could not open {path}"))?;
        let id = self.state.pop_layers.write().unpoison().add_source_layer(name, blend);
        self.state.pop_sources.lock().unpoison().insert(id, path);
        self.clear_cache();
        Ok(self.pop_list_layers())
    }

    pub fn pop_remove_layer(&self, id: u32) {
        self.with_pop_layers(|l| l.remove_layer(id));
        self.state.pop_sources.lock().unpoison().remove(&id);
    }

    pub fn pop_rename_layer(&self, id: u32, name: String) {
        self.state.pop_layers.write().unpoison().rename_layer(id, name);
    }

    pub fn pop_set_layer_visible(&self, id: u32, visible: bool) {
        self.with_pop_layers(|l| l.set_layer_visible(id, visible));
    }

    pub fn pop_set_layer_blend(&self, id: u32, blend: Blend) {
        self.with_pop_layers(|l| l.set_layer_blend(id, blend));
    }

    pub fn pop_set_active_layer(&self, id: u32) {
        self.state.pop_layers.write().unpoison().set_active(id);
    }

    pub fn pop_move_layer(&self, id: u32, up: bool) {
        self.with_pop_layers(|l| l.move_layer(id, up));
    }

    /// Discard all population edits (all layers) and invalidate the cache.
    pub fn clear_pop_edits(&self) {
        self.with_pop_layers(PopLayers::clear_all);
    }

    /// A snapshot of the edit-layer stack, for writing edits back to disk.
    pub fn pop_layers_snapshot(&self) -> PopLayers {
        self.state.pop_layers.read().unpoison().clone()
    }

    /// Whether any layer currently holds an edit.
    pub fn pop_has_edits(&self) -> bool {
        !self.state.pop_layers.read().unpoison().is_empty()
    }

    /// Drop cached population readers so the next tile re-opens the file — used
    /// after the archive on disk is replaced (apply / restore).
    pub fn evict_pop_readers(&self) {
        self.state.pop_readers.lock().unpoison().clear();
        self.clear_cache();
    }

    /// Decode every base z10 tile overlapping the lon/lat region, keyed by tile,
    /// alongside the inclusive global-pixel rect the region covers.
    async fn pop_region_base(
        &self,
        reader: &crate::pop_tiles::PopReader,
        west: f64,
        south: f64,
        east: f64,
        north: f64,
    ) -> (RegionBase, (u32, u32, u32, u32)) {
        let rect = region_bounds(west, south, east, north);
        let (gx0, gy0, gx1, gy1) = rect;
        let mut base = HashMap::new();
        for ty in (gy0 / TILE_PX)..=(gy1 / TILE_PX) {
            for tx in (gx0 / TILE_PX)..=(gx1 / TILE_PX) {
                base.insert((tx, ty), crate::pop_tiles::base_values(reader, tx, ty).await);
            }
        }
        (base, rect)
    }

    /// Mean base density over a bounded sample of the pixels an import covers, for
    /// value-space matching. A nationwide import covers ~10^9 pixels, so this
    /// strides through them and decodes at most [`MAX_MEAN_TILES`] base tiles —
    /// a mean estimate needs no more.
    async fn pop_base_mean_over(
        &self,
        reader: &crate::pop_tiles::PopReader,
        acc: &GridAccumulator,
    ) -> f64 {
        let stride = (acc.covered_pixels() / MEAN_SAMPLE_TARGET).max(1) as usize;
        let mut tiles: HashMap<(u32, u32), (u32, Vec<u16>)> = HashMap::new();
        let (mut sum, mut n) = (0.0f64, 0u64);
        for (gx, gy, _) in acc.iter().step_by(stride) {
            let (tx, ty) = (gx / TILE_PX, gy / TILE_PX);
            let (px, py) = (gx % TILE_PX, gy % TILE_PX);
            if !tiles.contains_key(&(tx, ty)) {
                if tiles.len() >= MAX_MEAN_TILES {
                    continue; // enough tiles sampled; skip decoding more
                }
                tiles.insert((tx, ty), crate::pop_tiles::base_values(reader, tx, ty).await);
            }
            let (w, values) = &tiles[&(tx, ty)];
            sum += f64::from(values.get((py * w + px) as usize).copied().unwrap_or(0));
            n += 1;
        }
        if n == 0 { 0.0 } else { sum / n as f64 }
    }

    /// Value-space match a filled accumulator to the base, add it as an import
    /// layer, and return the summary. Shared by every import front-end.
    async fn finish_import(
        &self,
        acc: GridAccumulator,
        name: String,
        blend: Blend,
        user_scale: f64,
    ) -> Result<PopImportResult, String> {
        if acc.is_empty() {
            return Err("The source covered none of the map.".into());
        }
        if acc.tiles_touched() > MAX_IMPORT_TILES {
            return Err(format!(
                "That import covers {} map tiles (~{:.1} GB) — too large. Select a smaller area.",
                acc.tiles_touched(),
                acc.tiles_touched() as f64 * 0.36 / 1000.0
            ));
        }
        let source_total = acc.total();
        let covered_pixels = acc.covered_pixels();
        let import_mean = acc.mean();

        let base_mean = match self.pop_layer_path() {
            Some(path) => match pop_reader(&self.state, &path).await {
                Some(reader) => self.pop_base_mean_over(&reader, &acc).await,
                None => 0.0,
            },
            None => 0.0,
        };
        let applied_scale = turnout_core::pop_import::match_scale(base_mean, import_mean, user_scale);

        let tiles = acc.into_tiles(applied_scale);
        let id = {
            let mut layers = self.state.pop_layers.write().unpoison();
            layers.add_import_layer(name, blend, tiles)
        };
        self.clear_cache();

        Ok(PopImportResult { id, covered_pixels, source_total, applied_scale, layers: self.pop_list_layers() })
    }

    /// Import a shapefile's polygons (each carrying `field` people) as a new import
    /// layer, value-space matched to the base and scaled by `user_scale`.
    pub async fn pop_import_shapefile(
        &self,
        shp_path: String,
        field: String,
        name: String,
        blend: Blend,
        user_scale: f64,
    ) -> Result<PopImportResult, String> {
        let acc = tokio::task::spawn_blocking(move || {
            let polys = turnout_core::shapefile_reader::read_valued_polygons(
                std::path::Path::new(&shp_path),
                &field,
            )
            .map_err(|e| e.to_string())?;
            let mut acc = GridAccumulator::new();
            for (geom, value) in &polys {
                turnout_core::pop_import::rasterize_geometry(geom, *value, &mut acc);
            }
            Ok::<_, String>(acc)
        })
        .await
        .map_err(|e| e.to_string())??;

        self.finish_import(acc, name, blend, user_scale).await
    }

    /// Import a geographic (EPSG:4326) `GeoTIFF` raster within `bbox`
    /// (`west, south, east, north`) as a new import layer.
    pub async fn pop_import_geotiff(
        &self,
        path: String,
        name: String,
        blend: Blend,
        user_scale: f64,
        bbox: (f64, f64, f64, f64),
    ) -> Result<PopImportResult, String> {
        let acc = tokio::task::spawn_blocking(move || {
            turnout_core::pop_geotiff::accumulate_geotiff(std::path::Path::new(&path), bbox)
        })
        .await
        .map_err(|e| e.to_string())??;

        self.finish_import(acc, name, blend, user_scale).await
    }

    /// Summed effective density (base with edits applied) over the region.
    pub async fn pop_region_total(&self, west: f64, south: f64, east: f64, north: f64) -> f64 {
        let Some(path) = self.pop_layer_path() else { return 0.0 };
        let Some(reader) = pop_reader(&self.state, &path).await else { return 0.0 };
        if region_pixel_count(region_bounds(west, south, east, north)) > MAX_REGION_PIXELS {
            return 0.0;
        }
        let (base, rect) = self.pop_region_base(&reader, west, south, east, north).await;
        let layers = self.state.pop_layers.read().unpoison();
        region_sum(&base, &layers, rect)
    }

    /// Set the region's total density to `target` by scaling every pixel by the
    /// same factor (`flat` = distribute `target` uniformly instead). Records the
    /// results as edits and invalidates the cache.
    pub async fn pop_set_region(&self, west: f64, south: f64, east: f64, north: f64, target: f64, flat: bool) {
        let Some(path) = self.pop_layer_path() else { return };
        let Some(reader) = pop_reader(&self.state, &path).await else { return };
        let rect = region_bounds(west, south, east, north);
        let count = region_pixel_count(rect);
        if count == 0 || count > MAX_REGION_PIXELS {
            return;
        }
        let (base, rect) = self.pop_region_base(&reader, west, south, east, north).await;
        let (gx0, gy0, gx1, gy1) = rect;

        let mut layers = self.state.pop_layers.write().unpoison();
        let current = region_sum(&base, &layers, rect);
        let uniform = target / count as f64;
        let factor = if current > 0.0 { target / current } else { 0.0 };

        // Set the active layer's delta so the composite effective density hits the
        // per-pixel target, relative to the base and the other visible layers.
        for gy in gy0..=gy1 {
            for gx in gx0..=gx1 {
                let (tx, ty) = (gx / TILE_PX, gy / TILE_PX);
                let (px, py) = ((gx % TILE_PX) as u16, (gy % TILE_PX) as u16);
                let base_v = base_at(&base, tx, ty, px, py);
                let target_eff = if flat || current <= 0.0 {
                    uniform
                } else {
                    f64::from(layers.effective(base_v, tx, ty, px, py)) * factor
                };
                let target_eff = target_eff.round().clamp(0.0, f64::from(u16::MAX)) as i64;
                // The active-layer delta lifts everything beneath it up to the target.
                let beneath = i64::from(layers.effective_excluding_active(base_v, tx, ty, px, py));
                let delta = target_eff - beneath;
                layers.set_delta(tx, ty, px, py, delta.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32);
            }
        }
        drop(layers);
        self.clear_cache();
    }

    /// Append a layer and invalidate the render cache.
    fn push_layer(&self, layer: Layer) {
        self.state.layers.write().unpoison().push(layer);
        self.clear_cache();
    }

    pub fn remove_layer(&self, id: u32) -> bool {
        let mut layers = self.state.layers.write().unpoison();
        let before = layers.len();
        layers.retain(|l| l.id != id);
        let removed = layers.len() < before;
        drop(layers);
        if removed {
            self.state.runtime.write().unpoison().remove(&id);
            self.evict_remote_cache(id);
            self.clear_cache();
        }
        removed
    }

    /// Remove a layer and hand back its definition plus any runtime data, so the
    /// caller can move both to another group's server.
    pub fn take_layer(&self, id: u32) -> Option<(Layer, Option<RuntimeData>)> {
        let mut layers = self.state.layers.write().unpoison();
        let idx = layers.iter().position(|l| l.id == id)?;
        let layer = layers.remove(idx);
        drop(layers);
        let runtime = self.state.runtime.write().unpoison().remove(&id);
        self.clear_cache();
        Some((layer, runtime))
    }

    pub fn move_layer_up(&self, id: u32) -> bool {
        let mut layers = self.state.layers.write().unpoison();
        let Some(idx) = layers.iter().position(|l| l.id == id) else { return false };
        if idx == 0 { return false; }
        layers.swap(idx, idx - 1);
        drop(layers);
        self.clear_cache();
        true
    }

    pub fn move_layer_down(&self, id: u32) -> bool {
        let mut layers = self.state.layers.write().unpoison();
        let Some(idx) = layers.iter().position(|l| l.id == id) else { return false };
        if idx + 1 >= layers.len() { return false; }
        layers.swap(idx, idx + 1);
        drop(layers);
        self.clear_cache();
        true
    }

    pub fn insert_layer(&self, layer: Layer, runtime: Option<RuntimeData>) {
        let mut next = self.state.next_id.lock().unpoison();
        if layer.id >= *next {
            *next = layer.id + 1;
        }
        drop(next);
        if let Some(rt) = runtime {
            self.state.runtime.write().unpoison().insert(layer.id, rt);
        }
        self.state.layers.write().unpoison().push(layer);
        self.clear_cache();
    }

    /// Set an Apple layer's live tokenized URL. The URL is runtime-only (never
    /// persisted), so it is stored in the runtime cache rather than the definition.
    pub fn update_apple_url(&self, id: u32, url_template: String) {
        self.state.runtime.write().unpoison().insert(id, RuntimeData::Apple { url_template });
        self.clear_cache();
        self.evict_remote_cache(id);
    }

    pub fn rename_layer(&self, id: u32, name: String) {
        let mut layers = self.state.layers.write().unpoison();
        if let Some(layer) = layers.iter_mut().find(|l| l.id == id) {
            layer.name = name;
        }
    }

    pub fn set_layer_visible(&self, id: u32, visible: bool) -> bool {
        let mut layers = self.state.layers.write().unpoison();
        if let Some(layer) = layers.iter_mut().find(|l| l.id == id) {
            layer.visible = visible;
            drop(layers);
            self.clear_cache();
            true
        } else {
            false
        }
    }

    pub fn set_all_visible(&self, visible: bool) {
        let mut layers = self.state.layers.write().unpoison();
        for layer in layers.iter_mut() {
            layer.visible = visible;
        }
        drop(layers);
        self.clear_cache();
    }

    pub fn set_layer_opacity(&self, id: u32, opacity: f32) -> bool {
        let mut layers = self.state.layers.write().unpoison();
        if let Some(layer) = layers.iter_mut().find(|l| l.id == id) {
            layer.opacity = opacity.clamp(0.0, 1.0);
            drop(layers);
            self.clear_cache();
            true
        } else {
            false
        }
    }

    pub fn layer_count(&self) -> usize {
        self.state.layers.read().unpoison().len()
    }

    fn next_id(&self) -> u32 {
        let mut next = self.state.next_id.lock().unpoison();
        let id = *next;
        *next += 1;
        id
    }

    pub fn clear_cache(&self) {
        self.state.render_cache.clear();
    }

    pub fn evict_remote_cache(&self, layer_id: u32) {
        let mut cache = self.state.remote_cache.lock().unpoison();
        let keys: Vec<_> = cache.iter()
            .filter(|((lid, ..), _)| *lid == layer_id)
            .map(|(k, _)| *k)
            .collect();
        for key in keys {
            cache.pop(&key);
        }
    }
}

pub async fn start(port_hint: u16) -> Result<ServerHandle, Box<dyn std::error::Error + Send + Sync>> {
    let listener = if port_hint > 0 {
        match TcpListener::bind(format!("127.0.0.1:{port_hint}")).await {
            Ok(l) => l,
            Err(_) => TcpListener::bind("127.0.0.1:0").await?,
        }
    } else {
        TcpListener::bind("127.0.0.1:0").await?
    };
    let port = listener.local_addr()?.port();

    let state = Arc::new(TileState {
        layers: RwLock::new(Vec::new()),
        runtime: RwLock::new(HashMap::new()),
        render_cache: server_core::TileCache::new(RENDER_CACHE_CAPACITY, 1),
        remote_cache: server_core::lru_cache(REMOTE_CACHE_CAPACITY),
        error_layers: Mutex::new(std::collections::HashSet::new()),
        next_id: Mutex::new(0),
        port,
        http: reqwest::Client::builder()
            .user_agent(server_core::USER_AGENT)
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .connect_timeout(server_core::CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default(),
        mbtiles_conns: server_core::lru_cache(MBTILES_CONN_CACHE),
        pop_readers: server_core::lru_cache(POP_READER_CACHE),
        pop_layers: RwLock::new(PopLayers::default()),
        pop_sources: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/tilejson.json", get(serve_tilejson))
        .route("/{z}/{x}/{y}", get(serve_tile))
        .layer(CorsLayer::permissive())
        .with_state(Arc::clone(&state));

    let shutdown_tx = server_core::spawn_with_shutdown(listener, app);

    Ok(ServerHandle { port, state, shutdown_tx })
}

fn decode_images(data: &OverlayData) -> Vec<DecodedImage> {
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


async fn serve_tilejson(
    State(state): State<Arc<TileState>>,
) -> impl IntoResponse {
    let port = state.port;
    let tile_url = format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}");

    let (min_zoom, max_zoom) = (0, MAX_ZOOM);

    let bounds = {
        let layers = state.layers.read().unpoison();
        if layers.is_empty() {
            [-180.0, -85.051_129, 180.0, 85.051_129]
        } else {
            let mut w = f64::MAX;
            let mut s = f64::MAX;
            let mut e = f64::MIN;
            let mut n = f64::MIN;
            for l in layers.iter() {
                w = w.min(l.bbox.0);
                s = s.min(l.bbox.1);
                e = e.max(l.bbox.2);
                n = n.max(l.bbox.3);
            }
            [w, s, e, n]
        }
    };

    let center_lon = (bounds[0] + bounds[2]) / 2.0;
    let center_lat = (bounds[1] + bounds[3]) / 2.0;

    let json = serde_json::json!({
        "tilejson": "3.0.0",
        "tiles": [tile_url],
        "minzoom": min_zoom,
        "maxzoom": max_zoom,
        "bounds": bounds,
        "center": [center_lon, center_lat, 10],
        "format": "png",
        "scheme": "xyz",
    });

    (
        StatusCode::OK,
        [("content-type", "application/json")],
        json.to_string().into_bytes(),
    )
}

async fn serve_tile(
    Path((z, x, y)): Path<(u8, u32, u32)>,
    State(state): State<Arc<TileState>>,
) -> impl IntoResponse {
    let max_coord = 1u32 << z.min(MAX_ZOOM);
    if z > MAX_ZOOM || x >= max_coord || y >= max_coord {
        return (StatusCode::BAD_REQUEST, [("content-type", "text/plain")], Bytes::from_static(b"invalid tile coordinates"));
    }

    if let Some(png) = state.render_cache.get(&(z, x, y)) {
        return (StatusCode::OK, [("content-type", "image/png")], png);
    }

    // Population layers, collected here and rendered after the async fetches
    // below (their reader is opened async, unlike the sync gather).
    let mut pop_layers: Vec<(u32, String)> = Vec::new();
    let (remote_requests, mbtiles_tiles): (Vec<(u32, RemoteReq)>, HashMap<u32, Pixmap>) = {
        let layers = state.layers.read().unpoison();
        let runtime = state.runtime.read().unpoison();
        let mut reqs = Vec::new();
        let mut local = HashMap::new();
        let tms_y = turnout_core::geo::tms_y(z, y);
        for l in layers.iter() {
            if !l.visible { continue; }
            match &l.source {
                SourceDef::Kmz { .. } | SourceDef::Shp { .. } | SourceDef::GeoJson { .. } => {}
                SourceDef::Pop { path } => pop_layers.push((l.id, path.clone())),
                SourceDef::Wms { base_url, layer_name } =>
                    reqs.push((l.id, RemoteReq::Wms(base_url.clone(), layer_name.clone()))),
                SourceDef::ArcGis { base_url, service_name } =>
                    reqs.push((l.id, RemoteReq::ArcGis(base_url.clone(), service_name.clone()))),
                SourceDef::Xyz { url_template }
                | SourceDef::Wmts { url_template }
                | SourceDef::Bing { url_template } =>
                    reqs.push((l.id, RemoteReq::Xyz(url_template.clone()))),
                SourceDef::Apple { .. } => {
                    if let Some(RuntimeData::Apple { url_template }) = runtime.get(&l.id) {
                        reqs.push((l.id, RemoteReq::Xyz(url_template.clone())));
                    }
                }
                SourceDef::MbTiles { path, max_zoom } => {
                    if let Some(cached) = get_remote_cached(&state, l.id, z, x, y) {
                        if let Some(pm) = decoded_to_pixmap(&cached) {
                            local.insert(l.id, pm);
                        }
                    } else if let Some(conn) = mbtiles_conn(&state, path) {
                        let pm = read_mbtiles_tile(&conn.lock().unpoison(), z, x, tms_y, *max_zoom);
                        if let Some(pm) = pm {
                            let decoded = (pm.data().to_vec(), pm.width(), pm.height());
                            put_remote_cached(&state, l.id, z, x, y, decoded);
                            local.insert(l.id, pm);
                        }
                    }
                }
            }
        }
        (reqs, local)
    };

    let fetches = remote_requests.iter().map(|(id, req)| {
        let client = &state.http;
        let id = *id;
        let state_ref = &state;
        async move {
            if let Some(cached) = get_remote_cached(state_ref, id, z, x, y) {
                return (id, decoded_to_pixmap(&cached));
            }
            let bytes = match req {
                RemoteReq::Wms(url, layer) => fetch_wms_tile(client, url, layer, z.into(), x, y).await,
                RemoteReq::ArcGis(url, svc) => fetch_arcgis_tile(client, url, svc, z.into(), x, y).await,
                RemoteReq::Xyz(tpl) => fetch_xyz_tile(client, tpl, z.into(), x, y).await,
            };
            if let Some(b) = bytes {
                state_ref.error_layers.lock().unpoison().remove(&id);
                if let Some(decoded) = decode_remote_bytes(&b) {
                    let pixmap = decoded_to_pixmap(&decoded);
                    put_remote_cached(state_ref, id, z, x, y, decoded);
                    (id, pixmap)
                } else {
                    (id, None)
                }
            } else {
                state_ref.error_layers.lock().unpoison().insert(id);
                (id, None)
            }
        }
    });
    let fetch_results: Vec<(u32, Option<Pixmap>)> = futures::future::join_all(fetches)
        .await
        .into_iter()
        .collect();
    let any_failed = fetch_results.iter().any(|(_, pm)| pm.is_none());
    let mut remote_tiles: HashMap<u32, Pixmap> = fetch_results
        .into_iter()
        .filter_map(|(id, pm)| pm.map(|p| (id, p)))
        .collect();
    remote_tiles.extend(mbtiles_tiles);

    // Open readers for the visible file-backed source layers (census, WorldPop…)
    // once, so pop_pixmap can composite them with the base on demand.
    let source_paths: Vec<(u32, String)> = {
        let ids = state.pop_layers.read().unpoison().visible_source_ids();
        let paths = state.pop_sources.lock().unpoison();
        ids.into_iter().filter_map(|id| paths.get(&id).map(|p| (id, p.clone()))).collect()
    };
    let mut source_readers: HashMap<u32, Arc<crate::pop_tiles::PopReader>> = HashMap::new();
    for (id, path) in source_paths {
        if let Some(reader) = pop_reader(&state, &path).await {
            source_readers.insert(id, reader);
        }
    }

    for (id, path) in pop_layers {
        if let Some(reader) = pop_reader(&state, &path).await
            && let Some(pm) =
                crate::pop_tiles::pop_pixmap(&reader, z, x, y, TILE_SIZE, &state.pop_layers, &source_readers).await
        {
            remote_tiles.insert(id, pm);
        }
    }

    let png = Bytes::from(render_tile(&state, &remote_tiles, z.into(), x, y));

    if !any_failed {
        state.render_cache.put((z, x, y), png.clone());
    }

    (StatusCode::OK, [("content-type", "image/png")], png)
}

/// Number of pixels in an inclusive global-pixel rect.
fn region_pixel_count((gx0, gy0, gx1, gy1): (u32, u32, u32, u32)) -> u64 {
    u64::from(gx1 - gx0 + 1) * u64::from(gy1 - gy0 + 1)
}

/// Decoded base density at a pixel, or `0` if the tile is absent.
fn base_at(base: &RegionBase, tile_x: u32, tile_y: u32, px: u16, py: u16) -> u16 {
    base.get(&(tile_x, tile_y))
        .and_then(|(w, v)| v.get((u32::from(py) * w + u32::from(px)) as usize).copied())
        .unwrap_or(0)
}

/// Sum effective density (base + composite layer deltas) over the region rect.
fn region_sum(base: &RegionBase, layers: &PopLayers, rect: (u32, u32, u32, u32)) -> f64 {
    let (gx0, gy0, gx1, gy1) = rect;
    let mut total = 0.0;
    for gy in gy0..=gy1 {
        for gx in gx0..=gx1 {
            let (tx, ty) = (gx / TILE_PX, gy / TILE_PX);
            let (px, py) = ((gx % TILE_PX) as u16, (gy % TILE_PX) as u16);
            total += f64::from(layers.effective(base_at(base, tx, ty, px, py), tx, ty, px, py));
        }
    }
    total
}

/// Shared, cached memory-mapped reader for a population archive, opened on first
/// use. The cache holds an `Arc`, so eviction only drops this map's reference; an
/// in-flight request keeps its own until done.
async fn pop_reader(state: &TileState, path: &str) -> Option<Arc<crate::pop_tiles::PopReader>> {
    if let Some(reader) = state.pop_readers.lock().unpoison().get(path) {
        return Some(Arc::clone(reader));
    }
    let reader = Arc::new(crate::pop_tiles::open(path).await.ok()?);
    state.pop_readers.lock().unpoison().put(path.to_string(), Arc::clone(&reader));
    Some(reader)
}

