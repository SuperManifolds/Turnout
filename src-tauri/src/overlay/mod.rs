use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use crate::arcgis;
use crate::error::{CommandError, CommandResult};
use crate::server_core::UnpoisonExt;
use crate::settings;
use crate::tile_server::{self, LayerKind, SourceDef};
use crate::wms;
use crate::wmts;

mod persist;

use persist::{allocate_group_ids, load_saved, restore_layer, save_groups, AppleCreds};

pub struct OverlayState {
    groups: Mutex<Vec<TileGroup>>,
    next_group_id: Mutex<u32>,
    /// The built-in population overlay's tile server, present only while the layer
    /// is toggled on. Kept out of `groups` so it never shows in the overlay list
    /// or persists — it is an always-available, off-by-default layer like ORM.
    population: Mutex<Option<Arc<tile_server::ServerHandle>>>,
}

struct TileGroup {
    id: u32,
    name: String,
    handle: tile_server::ServerHandle,
}


#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LayerInfo {
    pub id: u32,
    pub name: String,
    pub kind: LayerKind,
    pub visible: bool,
    pub opacity: f32,
    pub bbox: [f64; 4],
    pub has_errors: bool,
    pub source_url: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GroupInfo {
    pub id: u32,
    pub name: String,
    pub tile_url: String,
    pub tilejson_url: String,
    pub layers: Vec<LayerInfo>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OverlayStatus {
    pub groups: Vec<GroupInfo>,
}

impl OverlayState {
    pub fn new() -> Self {
        Self {
            groups: Mutex::new(Vec::new()),
            next_group_id: Mutex::new(0),
            population: Mutex::new(None),
        }
    }

    fn next_group_id(&self) -> u32 {
        let mut id = self.next_group_id.lock().unpoison();
        let val = *id;
        *id += 1;
        val
    }
}

fn port_for_id(id: u32) -> u16 {
    tile_server::PREFERRED_PORT.saturating_add(id as u16)
}

/// Fixed preferred port for the built-in population overlay's tile server. Set
/// clear of the per-group range (`PREFERRED_PORT + group_id`); falls back to an
/// ephemeral port if taken.
const POPULATION_PORT: u16 = 17969;


// --- Tauri commands ---

#[tauri::command]
pub async fn pick_kmz_file(app: tauri::AppHandle) -> Option<String> {
    let path = app
        .dialog()
        .file()
        .add_filter("Overlay files", &["kmz", "kml", "shp", "geojson", "json", "mbtiles"])
        .blocking_pick_file()?;
    Some(path.to_string())
}

#[tauri::command]
pub async fn pick_shapefile(app: tauri::AppHandle) -> Option<String> {
    let path = app
        .dialog()
        .file()
        .add_filter("Shapefile", &["shp"])
        .blocking_pick_file()?;
    Some(path.to_string())
}

#[tauri::command]
pub async fn pick_geotiff(app: tauri::AppHandle) -> Option<String> {
    let path = app
        .dialog()
        .file()
        .add_filter("GeoTIFF", &["tif", "tiff"])
        .blocking_pick_file()?;
    Some(path.to_string())
}

#[tauri::command]
pub async fn pick_pmtiles(app: tauri::AppHandle) -> Option<String> {
    let path = app
        .dialog()
        .file()
        .add_filter("PMTiles", &["pmtiles"])
        .blocking_pick_file()?;
    Some(path.to_string())
}

#[tauri::command]
pub async fn create_group(app: tauri::AppHandle, name: String) -> CommandResult<OverlayStatus> {
    let state = app.state::<OverlayState>();
    let group_id = state.next_group_id();
    let port = port_for_id(group_id);

    let handle = tile_server::start(port)
        .await
        .map_err(|e| CommandError::Server(format!("Failed to start tile server: {e}")))?;

    let mut groups = state.groups.lock().unpoison();
    groups.push(TileGroup { id: group_id, name, handle });
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn remove_group(app: tauri::AppHandle, group_id: u32) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let mut groups = state.groups.lock().unpoison();
    if let Some(idx) = groups.iter().position(|g| g.id == group_id) {
        let group = groups.remove(idx);
        let _ = group.handle.shutdown_tx.send(true);
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn reorder_group(app: tauri::AppHandle, group_id: u32, direction: String) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let mut groups = state.groups.lock().unpoison();
    if let Some(idx) = groups.iter().position(|g| g.id == group_id) {
        match direction.as_str() {
            "up" if idx > 0 => { groups.swap(idx, idx - 1); }
            "down" if idx + 1 < groups.len() => { groups.swap(idx, idx + 1); }
            _ => {}
        }
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn rename_group(app: tauri::AppHandle, group_id: u32, name: String) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let mut groups = state.groups.lock().unpoison();
    if let Some(group) = groups.iter_mut().find(|g| g.id == group_id) {
        group.name = name;
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
pub async fn add_overlay(
    app: tauri::AppHandle,
    path: String,
    group_id: Option<u32>,
) -> CommandResult<OverlayStatus> {
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if ext == "mbtiles" {
        let display_name = std::path::Path::new(&path)
            .file_stem()
            .map_or_else(|| "MBTiles".into(), |s| s.to_string_lossy().to_string());
        return add_mbtiles_layer(app, path, display_name, group_id).await;
    }

    let kind = kind_for_extension(&path);
    let mut data = parse_overlay_file(&path, kind)?;

    if data.bbox().is_none() {
        return Err(CommandError::Invalid("File contains no geometry or overlays".into()));
    }

    if data.name.is_none() {
        data.name = std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string());
    }

    let state = app.state::<OverlayState>();
    ensure_group_exists(&state, &app, group_id).await?;

    let groups = state.groups.lock().unpoison();
    let group = find_group(&groups, group_id)?;
    // bbox was validated above, so the add succeeds; the id isn't needed here.
    let _ = group.handle.add_kmz_layer(data, Some(path), kind);
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
pub async fn fetch_wms_layers(url: String) -> Result<Vec<wms::WmsLayerInfo>, String> {
    wms::get_capabilities(&url).await
}

#[tauri::command]
pub async fn add_wms_layer(
    app: tauri::AppHandle,
    url: String,
    layer_name: String,
    display_name: String,
    group_id: Option<u32>,
) -> CommandResult<OverlayStatus> {
    let state = app.state::<OverlayState>();
    ensure_group_exists(&state, &app, group_id).await?;

    let groups = state.groups.lock().unpoison();
    let group = find_group(&groups, group_id)?;
    group.handle.add_wms_layer(url, layer_name, display_name);
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
pub async fn fetch_arcgis_services(url: String) -> Result<Vec<arcgis::ArcGisServiceInfo>, String> {
    arcgis::list_services(&url).await
}

#[tauri::command]
pub async fn add_arcgis_layer(
    app: tauri::AppHandle,
    url: String,
    service_name: String,
    display_name: String,
    group_id: Option<u32>,
) -> CommandResult<OverlayStatus> {
    let state = app.state::<OverlayState>();
    ensure_group_exists(&state, &app, group_id).await?;

    let groups = state.groups.lock().unpoison();
    let group = find_group(&groups, group_id)?;
    group.handle.add_arcgis_layer(url, service_name, display_name);
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
pub async fn fetch_wmts_layers(url: String) -> Result<Vec<wmts::WmtsLayerInfo>, String> {
    wmts::get_capabilities(&url).await
}

#[tauri::command]
pub async fn add_xyz_layer(
    app: tauri::AppHandle,
    url_template: String,
    display_name: String,
    group_id: Option<u32>,
    kind: Option<String>,
) -> CommandResult<OverlayStatus> {
    let state = app.state::<OverlayState>();
    ensure_group_exists(&state, &app, group_id).await?;

    let layer_kind = match kind.as_deref() {
        Some("wmts") => LayerKind::Wmts,
        Some("apple") => LayerKind::Apple,
        Some("bing") => LayerKind::Bing,
        _ => LayerKind::Xyz,
    };
    let groups = state.groups.lock().unpoison();
    let group = find_group(&groups, group_id)?;
    group.handle.add_xyz_layer_with_kind(url_template, display_name, layer_kind);
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
pub async fn add_mbtiles_layer(
    app: tauri::AppHandle,
    path: String,
    display_name: String,
    group_id: Option<u32>,
) -> CommandResult<OverlayStatus> {
    let state = app.state::<OverlayState>();
    ensure_group_exists(&state, &app, group_id).await?;

    let groups = state.groups.lock().unpoison();
    let group = find_group(&groups, group_id)?;
    group.handle.add_mbtiles_layer(path, display_name)?;
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

/// Turn on the built-in population overlay, returning its tile-URL template for
/// the frontend to add as a raster source. Idempotent: if already on, returns the
/// existing URL. Auto-locates `pop400.pmtiles` in the NIMBY Rails install.
#[tauri::command]
pub async fn add_population_layer(app: tauri::AppHandle) -> CommandResult<String> {
    let state = app.state::<OverlayState>();

    if let Some(handle) = state.population.lock().unpoison().as_ref() {
        return Ok(population_tile_url(handle.port));
    }

    let path = crate::blueprint::pop400_path(&app)
        .ok_or_else(|| {
            CommandError::NotFound(
                "Could not find NIMBY Rails' population map. Set the game folder in Settings."
                    .into(),
            )
        })?
        .to_string_lossy()
        .to_string();

    let handle = tile_server::start(POPULATION_PORT)
        .await
        .map_err(|e| CommandError::Server(format!("Failed to start population tile server: {e}")))?;
    handle.add_pop_layer(path, "Population density".to_string());
    let url = population_tile_url(handle.port);
    *state.population.lock().unpoison() = Some(Arc::new(handle));
    Ok(url)
}

/// The live population server handle, cloned out of the mutex so callers can
/// `await` on it without holding the lock across suspension points.
fn population_handle(state: &OverlayState) -> Option<Arc<tile_server::ServerHandle>> {
    state.population.lock().unpoison().clone()
}

/// Paint a brush stroke onto the active edit layer over a sequence of
/// `(lon, lat)` points. No-op when the overlay is off.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_brush(
    app: tauri::AppHandle,
    points: Vec<(f64, f64)>,
    radius_m: f64,
    strength: u32,
    mode: String,
    clip: Option<(f64, f64, f64, f64)>,
) {
    let state = app.state::<OverlayState>();
    let Some(handle) = population_handle(&state) else { return };
    let mode = if mode == "remove" {
        turnout_core::pop_edit::BrushMode::Remove
    } else {
        turnout_core::pop_edit::BrushMode::Add
    };
    handle.pop_brush(&points, radius_m, strength, mode, clip);
}

/// The population edit-layer stack (top-first), or empty when the overlay is off.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_layers(app: tauri::AppHandle) -> Vec<turnout_core::pop_edit::LayerInfo> {
    let state = app.state::<OverlayState>();
    population_handle(&state).map(|h| h.pop_list_layers()).unwrap_or_default()
}

/// Add a new empty edit layer on top and make it active. Returns the new stack.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_add_layer(app: tauri::AppHandle) -> Vec<turnout_core::pop_edit::LayerInfo> {
    let state = app.state::<OverlayState>();
    let Some(handle) = population_handle(&state) else { return Vec::new() };
    handle.pop_add_layer();
    handle.pop_list_layers()
}

/// Layer stack mutations, each returning the updated stack for the UI.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_remove_layer(app: tauri::AppHandle, id: u32) -> Vec<turnout_core::pop_edit::LayerInfo> {
    let state = app.state::<OverlayState>();
    let Some(handle) = population_handle(&state) else { return Vec::new() };
    handle.pop_remove_layer(id);
    handle.pop_list_layers()
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_rename_layer(app: tauri::AppHandle, id: u32, name: String) -> Vec<turnout_core::pop_edit::LayerInfo> {
    let state = app.state::<OverlayState>();
    let Some(handle) = population_handle(&state) else { return Vec::new() };
    handle.pop_rename_layer(id, name);
    handle.pop_list_layers()
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_set_layer_visible(app: tauri::AppHandle, id: u32, visible: bool) -> Vec<turnout_core::pop_edit::LayerInfo> {
    let state = app.state::<OverlayState>();
    let Some(handle) = population_handle(&state) else { return Vec::new() };
    handle.pop_set_layer_visible(id, visible);
    handle.pop_list_layers()
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_set_layer_blend(app: tauri::AppHandle, id: u32, blend: turnout_core::pop_edit::Blend) -> Vec<turnout_core::pop_edit::LayerInfo> {
    let state = app.state::<OverlayState>();
    let Some(handle) = population_handle(&state) else { return Vec::new() };
    handle.pop_set_layer_blend(id, blend);
    handle.pop_list_layers()
}

/// The numeric-capable DBF field names of a shapefile, so the UI can let the user
/// pick which attribute holds the population count.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_shapefile_fields(path: String) -> CommandResult<Vec<String>> {
    turnout_core::shapefile_reader::field_names(std::path::Path::new(&path))
        .map_err(|e| CommandError::Io(e.to_string()))
}

/// Import a shapefile's polygons as a new population layer. `field` is the DBF
/// attribute holding the per-polygon people count; `blend` is "add" or "normal";
/// `scale` is a manual multiplier on top of value-space matching.
#[tauri::command]
pub async fn pop_import_shapefile(
    app: tauri::AppHandle,
    path: String,
    field: String,
    name: String,
    blend: turnout_core::pop_edit::Blend,
    scale: f64,
) -> CommandResult<tile_server::PopImportResult> {
    let state = app.state::<OverlayState>();
    let handle = population_handle(&state)
        .ok_or_else(|| CommandError::NotFound("Turn on the population layer first.".into()))?;
    handle
        .pop_import_shapefile(path, field, name, blend, scale)
        .await
        .map_err(CommandError::Io)
}

/// Add a baked `PMTiles` (e.g. a census bake) as a file-backed source layer —
/// memory-mapped, read on demand, composited with the base.
#[tauri::command]
pub async fn pop_add_source_layer(
    app: tauri::AppHandle,
    path: String,
    name: String,
    blend: turnout_core::pop_edit::Blend,
) -> CommandResult<Vec<turnout_core::pop_edit::LayerInfo>> {
    let state = app.state::<OverlayState>();
    let handle = population_handle(&state)
        .ok_or_else(|| CommandError::NotFound("Turn on the population layer first.".into()))?;
    handle.pop_add_source_layer(path, name, blend).await.map_err(CommandError::Io)
}

/// Import a geographic `GeoTIFF` raster within a lon/lat bbox as a new layer.
#[tauri::command]
pub async fn pop_import_geotiff(
    app: tauri::AppHandle,
    path: String,
    name: String,
    blend: turnout_core::pop_edit::Blend,
    scale: f64,
    bbox: (f64, f64, f64, f64),
) -> CommandResult<tile_server::PopImportResult> {
    let state = app.state::<OverlayState>();
    let handle = population_handle(&state)
        .ok_or_else(|| CommandError::NotFound("Turn on the population layer first.".into()))?;
    handle
        .pop_import_geotiff(path, name, blend, scale, bbox)
        .await
        .map_err(CommandError::Io)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_set_active_layer(app: tauri::AppHandle, id: u32) -> Vec<turnout_core::pop_edit::LayerInfo> {
    let state = app.state::<OverlayState>();
    let Some(handle) = population_handle(&state) else { return Vec::new() };
    handle.pop_set_active_layer(id);
    handle.pop_list_layers()
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_move_layer(app: tauri::AppHandle, id: u32, up: bool) -> Vec<turnout_core::pop_edit::LayerInfo> {
    let state = app.state::<OverlayState>();
    let Some(handle) = population_handle(&state) else { return Vec::new() };
    handle.pop_move_layer(id, up);
    handle.pop_list_layers()
}

/// Discard all in-progress population edits (clears every layer).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_clear_edits(app: tauri::AppHandle) {
    let state = app.state::<OverlayState>();
    if let Some(handle) = population_handle(&state) {
        handle.clear_pop_edits();
    }
}

/// The pristine-original backup of the population map, kept in Turnout's app data
/// so a game update or Steam file-verify can't destroy it.
fn pop_backup_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    Some(app.path().app_data_dir().ok()?.join("pop400-backup.pmtiles"))
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PopApplyStatus {
    /// A pristine backup exists, so the original can always be restored.
    pub has_backup: bool,
    /// There are unsaved edits in the layer stack.
    pub has_edits: bool,
}

/// Write the current edits into the game's `pop400.pmtiles`, backing up the
/// pristine original first. Returns the number of tiles written.
#[tauri::command]
pub async fn pop_apply(app: tauri::AppHandle) -> CommandResult<u64> {
    let game = crate::blueprint::pop400_path(&app)
        .ok_or_else(|| CommandError::NotFound("Could not find the game's population map. Set the game folder in Settings.".into()))?;
    let backup = pop_backup_path(&app)
        .ok_or_else(|| CommandError::Io("No app-data directory available".into()))?;

    let layers = {
        let state = app.state::<OverlayState>();
        let handle = population_handle(&state)
            .ok_or_else(|| CommandError::NotFound("Turn the Population overlay on before applying.".into()))?;
        if !handle.pop_has_edits() {
            return Err(CommandError::NotFound("No population edits to apply.".into()));
        }
        handle.pop_layers_snapshot()
    };

    // Back up the pristine original once, before it is ever overwritten.
    if let Some(dir) = backup.parent() {
        std::fs::create_dir_all(dir).map_err(|e| CommandError::Io(e.to_string()))?;
    }
    if !backup.exists() {
        std::fs::copy(&game, &backup).map_err(|e| CommandError::Io(format!("Backup failed: {e}")))?;
    }

    // Write to a temp file next to the target, then atomically swap it in. The
    // pmtiles writer isn't `Send`, so the whole async write runs on a blocking
    // thread that drives it with the current runtime.
    let tmp = game.with_extension("pmtiles.turnout-tmp");
    let (game_w, tmp_w) = (game.clone(), tmp.clone());
    let write = tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(crate::pop_write::apply_edits(&game_w, &tmp_w, &layers))
    })
    .await
    .map_err(|e| CommandError::Io(e.to_string()))?;
    let count = match write {
        Ok(count) => count,
        Err(e) => {
            // Don't leave the partial temp behind for the next apply to trip over.
            let _ = std::fs::remove_file(&tmp);
            return Err(CommandError::Io(format!("Write failed: {e}")));
        }
    };

    // Drop the tile server's cached mmap readers on the pre-swap file *before* the
    // rename (and again after), so no reader lingers on the old inode serving
    // stale tiles across the swap.
    let handle = population_handle(&app.state::<OverlayState>());
    if let Some(h) = &handle {
        h.evict_pop_readers();
    }
    std::fs::rename(&tmp, &game).map_err(|e| CommandError::Io(format!("Swap-in failed: {e}")))?;
    // Edits are baked into the file now: reset the session and re-read the base.
    if let Some(h) = &handle {
        h.clear_pop_edits();
        h.evict_pop_readers();
    }
    Ok(count)
}

/// Cheap sanity check that a backup file is a plausible `pmtiles` archive before
/// we copy it over the live game file: the `PMTiles` v3 magic (`"PMTiles"`) and a
/// size past the header. Catches an empty or truncated backup (e.g. the process
/// died during the initial copy) that would otherwise clobber a good game file.
fn backup_is_valid(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() < 128 {
        return false;
    }
    let mut magic = [0u8; 7];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && &magic == b"PMTiles"
}

/// Restore the pristine original population map from the backup.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_restore_original(app: tauri::AppHandle) -> CommandResult<()> {
    let game = crate::blueprint::pop400_path(&app)
        .ok_or_else(|| CommandError::NotFound("Could not find the game's population map.".into()))?;
    let backup = pop_backup_path(&app).filter(|p| p.exists())
        .ok_or_else(|| CommandError::NotFound("No backup to restore.".into()))?;
    if !backup_is_valid(&backup) {
        return Err(CommandError::Io(
            "The backup looks incomplete or corrupt; not restoring over the game file.".into(),
        ));
    }
    std::fs::copy(&backup, &game).map_err(|e| CommandError::Io(format!("Restore failed: {e}")))?;
    if let Some(handle) = population_handle(&app.state::<OverlayState>()) {
        handle.clear_pop_edits();
        handle.evict_pop_readers();
    }
    Ok(())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn pop_apply_status(app: tauri::AppHandle) -> PopApplyStatus {
    let has_backup = pop_backup_path(&app).is_some_and(|p| p.exists());
    let has_edits = population_handle(&app.state::<OverlayState>()).is_some_and(|h| h.pop_has_edits());
    PopApplyStatus { has_backup, has_edits }
}

/// Total effective population density within a lon/lat region. `0` when the
/// overlay is off or the region is too large.
#[tauri::command]
pub async fn pop_region_total(
    app: tauri::AppHandle,
    west: f64,
    south: f64,
    east: f64,
    north: f64,
) -> f64 {
    let state = app.state::<OverlayState>();
    let Some(handle) = population_handle(&state) else { return 0.0 };
    handle.pop_region_total(west, south, east, north).await
}

/// Set a region's population to `target` — scaling existing density, or `flat`
/// uniform fill.
#[tauri::command]
pub async fn pop_set_region(
    app: tauri::AppHandle,
    west: f64,
    south: f64,
    east: f64,
    north: f64,
    target: f64,
    flat: bool,
) {
    let state = app.state::<OverlayState>();
    let Some(handle) = population_handle(&state) else { return };
    handle.pop_set_region(west, south, east, north, target, flat).await;
}

/// Turn off the built-in population overlay, shutting down its tile server.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn remove_population_layer(app: tauri::AppHandle) {
    let state = app.state::<OverlayState>();
    if let Some(handle) = state.population.lock().unpoison().take() {
        let _ = handle.shutdown_tx.send(true);
    }
}

fn population_tile_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}")
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn move_layer(
    app: tauri::AppHandle,
    layer_id: u32,
    from_group_id: u32,
    to_group_id: u32,
) -> CommandResult<OverlayStatus> {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();

    let from = groups.iter().find(|g| g.id == from_group_id)
        .ok_or_else(|| CommandError::NotFound("Source group not found".into()))?;
    let to = groups.iter().find(|g| g.id == to_group_id)
        .ok_or_else(|| CommandError::NotFound("Destination group not found".into()))?;

    let (layer, runtime) = from.handle.take_layer(layer_id)
        .ok_or_else(|| CommandError::NotFound("Layer not found in source group".into()))?;
    to.handle.insert_layer(layer, runtime);

    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn remove_overlay(app: tauri::AppHandle, group_id: u32, layer_id: u32) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let mut groups = state.groups.lock().unpoison();
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        group.handle.remove_layer(layer_id);
        if group.handle.layer_count() == 0 {
            let idx = groups.iter().position(|g| g.id == group_id).expect("just found");
            let removed = groups.remove(idx);
            let _ = removed.handle.shutdown_tx.send(true);
        }
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn rename_layer(app: tauri::AppHandle, group_id: u32, layer_id: u32, name: String) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        group.handle.rename_layer(layer_id, name);
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_group_visible(app: tauri::AppHandle, group_id: u32, visible: bool) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        group.handle.set_all_visible(visible);
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn reorder_layer(app: tauri::AppHandle, group_id: u32, layer_id: u32, direction: String) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        match direction.as_str() {
            "up" => { group.handle.move_layer_up(layer_id); }
            "down" => { group.handle.move_layer_down(layer_id); }
            _ => {}
        }
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_layer_visible(app: tauri::AppHandle, group_id: u32, layer_id: u32, visible: bool) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        group.handle.set_layer_visible(layer_id, visible);
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_layer_opacity(app: tauri::AppHandle, group_id: u32, layer_id: u32, opacity: f32) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        group.handle.set_layer_opacity(layer_id, opacity);
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn update_apple_urls(
    app: tauri::AppHandle,
    access_key: String,
    map_version: Option<String>,
    sat_version: Option<String>,
) -> OverlayStatus {
    apply_apple_credentials(&app, &access_key, map_version.as_deref(), sat_version.as_deref())
}

/// Rewrites every live Apple layer's tile URL with a fresh access key and per-kind
/// version. Shared by the manual `update_apple_urls` command and the automatic
/// token refresher. It does NOT persist: the token is not part of the saved layer
/// (only the `sat` flag is), so there is nothing new to write — and the refresher
/// runs at startup before the overlays are restored, so a save here would persist
/// the still-empty group set over the saved overlays and wipe them.
pub(crate) fn apply_apple_credentials(
    app: &tauri::AppHandle,
    access_key: &str,
    map_version: Option<&str>,
    sat_version: Option<&str>,
) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();
    for group in groups.iter() {
        let layers = group.handle.state.layers.read().unpoison();
        // Read whether each Apple layer is satellite straight from its definition —
        // no URL parsing, since the source records it.
        let apple_layers: Vec<(u32, bool)> = layers.iter().filter_map(|l| match &l.source {
            tile_server::SourceDef::Apple { sat } => Some((l.id, *sat)),
            _ => None,
        }).collect();
        drop(layers);

        for (id, is_sat) in apple_layers {
            let ver = if is_sat { sat_version } else { map_version };
            let Some(ver) = ver else { continue };
            let url = turnout_core::geo::apple_tile_url(access_key, ver, is_sat);
            group.handle.update_apple_url(id, url);
        }
    }
    let status = build_status(&groups);
    drop(groups);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_overlay_status(app: tauri::AppHandle) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();
    build_status(&groups)
}

#[tauri::command]
pub async fn restore_overlays(app: tauri::AppHandle) -> OverlayStatus {
    let state = app.state::<OverlayState>();

    {
        let mut groups = state.groups.lock().unpoison();
        for g in groups.drain(..) {
            let _ = g.handle.shutdown_tx.send(true);
        }
    }

    let saved = load_saved(&app);
    if saved.is_empty() {
        return OverlayStatus { groups: Vec::new() };
    }

    let ids = allocate_group_ids(&saved.iter().map(|g| g.id).collect::<Vec<_>>());

    // Reserve ids above every restored one BEFORE binding any server, so a
    // concurrent `add_overlay`/`create_group` during the awaits below cannot hand
    // out a colliding id — and therefore a colliding port — for a new group.
    if let Some(max_id) = ids.iter().max() {
        let mut next = state.next_group_id.lock().unpoison();
        *next = (*next).max(max_id + 1);
    }

    let settings = settings::load(&app);
    let apple = AppleCreds {
        access_key: settings.apple_access_key,
        map_version: settings.apple_map_version,
        sat_version: settings.apple_sat_version,
    };

    for (saved_group, &group_id) in saved.iter().zip(&ids) {
        let port = port_for_id(group_id);
        let Ok(handle) = tile_server::start(port).await else { continue };

        for layer in &saved_group.layers {
            restore_layer(&handle, layer, &apple);
        }
        let mut groups = state.groups.lock().unpoison();
        groups.push(TileGroup {
            id: group_id,
            name: saved_group.name.clone(),
            handle,
        });
    }

    let groups = state.groups.lock().unpoison();
    build_status(&groups)
}

// --- Helpers ---

async fn ensure_group_exists(
    state: &OverlayState,
    _app: &tauri::AppHandle,
    group_id: Option<u32>,
) -> CommandResult<()> {
    let has_target = {
        let groups = state.groups.lock().unpoison();
        match group_id {
            Some(id) => groups.iter().any(|g| g.id == id),
            None => !groups.is_empty(),
        }
    };

    if has_target {
        return Ok(());
    }

    let gid = state.next_group_id();
    let port = port_for_id(gid);

    let handle = tile_server::start(port)
        .await
        .map_err(|e| CommandError::Server(format!("Failed to start tile server: {e}")))?;
    let mut groups = state.groups.lock().unpoison();
    if group_id.is_none() && groups.is_empty() || group_id.is_some() && !groups.iter().any(|g| g.id == group_id.expect("checked")) {
        groups.push(TileGroup { id: gid, name: "Default".to_string(), handle });
    } else {
        let _ = handle.shutdown_tx.send(true);
    }
    Ok(())
}

fn parse_overlay_file(path: &str, kind: LayerKind) -> CommandResult<turnout_core::kml::OverlayData> {
    match kind {
        LayerKind::Shp => turnout_core::shapefile_reader::parse_shapefile(std::path::Path::new(path))
            .map_err(|e| CommandError::Parse(e.to_string())),
        LayerKind::GeoJson => {
            let text = std::fs::read_to_string(path).map_err(|e| CommandError::Io(e.to_string()))?;
            turnout_core::geojson_reader::parse_geojson(&text).map_err(|e| CommandError::Parse(e.to_string()))
        }
        _ => {
            let bytes = std::fs::read(path).map_err(|e| CommandError::Io(e.to_string()))?;
            turnout_core::kml::parse_kmz(&bytes).map_err(|e| CommandError::Parse(e.to_string()))
        }
    }
}

fn kind_for_extension(path: &str) -> LayerKind {
    match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "shp" => LayerKind::Shp,
        "geojson" | "json" => LayerKind::GeoJson,
        _ => LayerKind::Kmz,
    }
}




fn find_group(groups: &[TileGroup], group_id: Option<u32>) -> CommandResult<&TileGroup> {
    match group_id {
        Some(id) => groups.iter().find(|g| g.id == id).ok_or_else(|| CommandError::NotFound("Group not found".into())),
        None => groups.first().ok_or_else(|| CommandError::NotFound("No groups exist".into())),
    }
}

fn build_status(groups: &[TileGroup]) -> OverlayStatus {
    OverlayStatus {
        groups: groups.iter().map(|g| {
            let layers = g.handle.state.layers.read().unpoison();
            let errors = g.handle.state.error_layers.lock().unpoison();
            GroupInfo {
                id: g.id,
                name: g.name.clone(),
                tile_url: format!("http://127.0.0.1:{}/{{z}}/{{x}}/{{y}}", g.handle.port),
                tilejson_url: format!("http://127.0.0.1:{}/tilejson.json", g.handle.port),
                layers: layers.iter().map(|l| {
                    let source_url = match &l.source {
                        SourceDef::Xyz { url_template }
                        | SourceDef::Wmts { url_template }
                        | SourceDef::Bing { url_template } => Some(url_template.clone()),
                        SourceDef::Wms { base_url, layer_name } => Some(format!(
                            "{base_url}?SERVICE=WMS&VERSION=1.1.1&REQUEST=GetMap&LAYERS={layer_name}\
                             &SRS=EPSG:3857&FORMAT=image/png&WIDTH=256&HEIGHT=256&BBOX={{bbox}}"
                        )),
                        _ => None,
                    };
                    LayerInfo {
                        id: l.id,
                        name: l.name.clone(),
                        kind: l.kind(),
                        visible: l.visible,
                        opacity: l.opacity,
                        bbox: [l.bbox.1, l.bbox.0, l.bbox.3, l.bbox.2],
                        has_errors: errors.contains(&l.id),
                        source_url,
                    }
                }).collect(),
            }
        }).collect(),
    }
}




#[cfg(test)]
mod tests {
    use super::backup_is_valid;
    use std::io::Write;

    #[test]
    fn backup_validity_checks_magic_and_size() {
        let dir = std::env::temp_dir();
        let uniq = std::process::id();

        // Valid: the PMTiles magic followed by enough bytes to clear the header.
        let good = dir.join(format!("turnout-backup-good-{uniq}.pmtiles"));
        {
            let mut f = std::fs::File::create(&good).expect("create good");
            f.write_all(b"PMTiles").expect("write magic");
            f.write_all(&[0u8; 200]).expect("write padding");
        }
        assert!(backup_is_valid(&good));

        // Truncated: right magic but smaller than a header — reject.
        let small = dir.join(format!("turnout-backup-small-{uniq}.pmtiles"));
        std::fs::write(&small, b"PMTiles").expect("write small");
        assert!(!backup_is_valid(&small));

        // Wrong magic — reject.
        let bad = dir.join(format!("turnout-backup-bad-{uniq}.pmtiles"));
        std::fs::write(&bad, vec![0u8; 300]).expect("write bad");
        assert!(!backup_is_valid(&bad));

        // Missing file — reject.
        assert!(!backup_is_valid(&dir.join(format!("turnout-backup-missing-{uniq}.pmtiles"))));

        let _ = std::fs::remove_file(&good);
        let _ = std::fs::remove_file(&small);
        let _ = std::fs::remove_file(&bad);
    }
}
