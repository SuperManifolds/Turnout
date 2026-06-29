use std::sync::Mutex;

use serde::Serialize;
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use crate::tile_server;
use crate::wms;

pub struct OverlayState {
    server: Mutex<Option<tile_server::ServerHandle>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LayerInfo {
    pub id: u32,
    pub name: String,
    pub kind: &'static str,
    pub bbox: [f64; 4],
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OverlayStatus {
    pub tile_url: String,
    pub layers: Vec<LayerInfo>,
}

impl OverlayState {
    pub fn new() -> Self {
        Self {
            server: Mutex::new(None),
        }
    }

    fn ensure_started(&self) -> bool {
        let guard = self.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.is_some()
    }
}

#[tauri::command]
pub async fn pick_kmz_file(app: tauri::AppHandle) -> Option<String> {
    let path = app
        .dialog()
        .file()
        .add_filter("KMZ / KML", &["kmz", "kml"])
        .blocking_pick_file()?;
    Some(path.to_string())
}

#[tauri::command]
pub async fn add_overlay(app: tauri::AppHandle, path: String) -> Result<OverlayStatus, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("Failed to read file: {e}"))?;
    let mut data = turnout_core::kml::parse_kmz(&bytes).map_err(|e| format!("Parse error: {e}"))?;

    if data.bbox().is_none() {
        return Err("KMZ contains no geometry or overlays".into());
    }

    if data.name.is_none() {
        data.name = std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string());
    }

    let state = app.state::<OverlayState>();
    start_if_needed(&state, &app).await?;

    let guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = guard.as_ref().expect("just started");
    if !server.add_kmz_layer(data) {
        return Err("KMZ contains no geometry or overlays".into());
    }

    Ok(build_status(server))
}

#[tauri::command]
pub async fn fetch_wms_layers(url: String) -> Result<Vec<wms::WmsLayerInfo>, String> {
    let layers = wms::get_capabilities(&url).await?;
    for l in &layers {
        eprintln!("WMS layer: name={:?} title={:?}", l.name, l.title);
    }
    Ok(layers)
}

#[tauri::command]
pub async fn add_wms_layer(
    app: tauri::AppHandle,
    url: String,
    layer_name: String,
    display_name: String,
) -> Result<OverlayStatus, String> {
    let state = app.state::<OverlayState>();
    start_if_needed(&state, &app).await?;

    let guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = guard.as_ref().expect("just started");
    server.add_wms_layer(url, layer_name, display_name);

    Ok(build_status(server))
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn remove_overlay(app: tauri::AppHandle, id: u32) -> Option<OverlayStatus> {
    let state = app.state::<OverlayState>();
    let mut guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    let server = guard.as_ref()?;
    server.remove_layer(id);

    if server.layer_count() == 0 {
        if let Some(s) = guard.take() {
            let _ = s.shutdown_tx.send(true);
        }
        return None;
    }

    Some(build_status(server))
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_overlay_status(app: tauri::AppHandle) -> Option<OverlayStatus> {
    let state = app.state::<OverlayState>();
    let guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.as_ref().map(build_status)
}

async fn start_if_needed(
    state: &OverlayState,
    _app: &tauri::AppHandle,
) -> Result<(), String> {
    if state.ensure_started() {
        return Ok(());
    }

    let handle = tile_server::start()
        .await
        .map_err(|e| format!("Failed to start tile server: {e}"))?;
    let mut guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(handle);
    Ok(())
}

fn build_status(server: &tile_server::ServerHandle) -> OverlayStatus {
    let layers = server.state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    OverlayStatus {
        tile_url: format!("http://127.0.0.1:{}/{{z}}/{{x}}/{{y}}", server.port),
        layers: layers
            .iter()
            .map(|l| LayerInfo {
                id: l.id,
                name: l.name.clone(),
                kind: l.kind,
                bbox: [l.bbox.1, l.bbox.0, l.bbox.3, l.bbox.2],
            })
            .collect(),
    }
}
