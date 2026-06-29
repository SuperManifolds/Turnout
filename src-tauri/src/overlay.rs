use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use crate::arcgis;
use crate::tile_server::{self, LayerSource};
use crate::wms;

const STORE_KEY: &str = "overlay_layers";

pub struct OverlayState {
    server: Mutex<Option<tile_server::ServerHandle>>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SavedLayer {
    kind: String,
    name: String,
    visible: bool,
    opacity: f32,
    path: Option<String>,
    wms_url: Option<String>,
    wms_layer: Option<String>,
    arcgis_url: Option<String>,
    arcgis_service: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LayerInfo {
    pub id: u32,
    pub name: String,
    pub kind: &'static str,
    pub visible: bool,
    pub opacity: f32,
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

    fn is_started(&self) -> bool {
        let guard = self.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.is_some()
    }
}

pub fn restore_layers(app: &tauri::AppHandle) {
    let saved = load_saved(app);
    if saved.is_empty() {
        return;
    }

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<OverlayState>();

        let handle = match tile_server::start().await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("Failed to start tile server for restore: {e}");
                return;
            }
        };

        for layer in &saved {
            match layer.kind.as_str() {
                "kmz" => {
                    if let Some(ref path) = layer.path {
                        match std::fs::read(path) {
                            Ok(bytes) => match turnout_core::kml::parse_kmz(&bytes) {
                                Ok(mut data) => {
                                    if data.name.is_none() {
                                        data.name = Some(layer.name.clone());
                                    }
                                    handle.add_kmz_layer(data, Some(path.clone()));
                                }
                                Err(e) => eprintln!("Failed to parse {path}: {e}"),
                            },
                            Err(e) => eprintln!("Failed to read {path}: {e}"),
                        }
                    }
                }
                "wms" => {
                    if let (Some(url), Some(wms_layer)) = (&layer.wms_url, &layer.wms_layer) {
                        handle.add_wms_layer(url.clone(), wms_layer.clone(), layer.name.clone());
                    }
                }
                "arcgis" => {
                    if let (Some(url), Some(svc)) = (&layer.arcgis_url, &layer.arcgis_service) {
                        handle.add_arcgis_layer(url.clone(), svc.clone(), layer.name.clone());
                    }
                }
                _ => {}
            }

            let layers = handle.state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(last) = layers.last() {
                let id = last.id;
                drop(layers);
                handle.set_layer_visible(id, layer.visible);
                handle.set_layer_opacity(id, layer.opacity);
            }
        }

        let mut guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(handle);
    });
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
    start_if_needed(state.inner()).await?;

    let guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = guard.as_ref().expect("just started");
    if !server.add_kmz_layer(data, Some(path)) {
        return Err("KMZ contains no geometry or overlays".into());
    }

    let status = build_status(server);
    drop(guard);
    save_layers(&app);
    Ok(status)
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
    start_if_needed(state.inner()).await?;

    let guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = guard.as_ref().expect("just started");
    server.add_wms_layer(url, layer_name, display_name);

    let status = build_status(server);
    drop(guard);
    save_layers(&app);
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
) -> Result<OverlayStatus, String> {
    let state = app.state::<OverlayState>();
    start_if_needed(state.inner()).await?;

    let guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = guard.as_ref().expect("just started");
    server.add_arcgis_layer(url, service_name, display_name);

    let status = build_status(server);
    drop(guard);
    save_layers(&app);
    Ok(status)
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
        drop(guard);
        save_layers(&app);
        return None;
    }

    let status = build_status(server);
    drop(guard);
    save_layers(&app);
    Some(status)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_layer_visible(app: tauri::AppHandle, id: u32, visible: bool) -> Option<OverlayStatus> {
    let state = app.state::<OverlayState>();
    let guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = guard.as_ref()?;
    server.set_layer_visible(id, visible);
    let status = build_status(server);
    drop(guard);
    save_layers(&app);
    Some(status)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_layer_opacity(app: tauri::AppHandle, id: u32, opacity: f32) -> Option<OverlayStatus> {
    let state = app.state::<OverlayState>();
    let guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = guard.as_ref()?;
    server.set_layer_opacity(id, opacity);
    let status = build_status(server);
    drop(guard);
    save_layers(&app);
    Some(status)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_overlay_status(app: tauri::AppHandle) -> Option<OverlayStatus> {
    let state = app.state::<OverlayState>();
    let guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.as_ref().map(build_status)
}

async fn start_if_needed(state: &OverlayState) -> Result<(), String> {
    if state.is_started() {
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
                visible: l.visible,
                opacity: l.opacity,
                bbox: [l.bbox.1, l.bbox.0, l.bbox.3, l.bbox.2],
            })
            .collect(),
    }
}

fn save_layers(app: &tauri::AppHandle) {
    use tauri_plugin_store::StoreExt;

    let state = app.state::<OverlayState>();
    let guard = state.server.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let saved: Vec<SavedLayer> = guard.as_ref().map_or_else(Vec::new, |server| {
        let layers = server.state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        layers
            .iter()
            .map(|l| {
                let (path, wms_url, wms_layer, arcgis_url, arcgis_service) = match &l.source {
                    LayerSource::Kmz { path, .. } => (path.clone(), None, None, None, None),
                    LayerSource::Wms { base_url, layer_name } => {
                        (None, Some(base_url.clone()), Some(layer_name.clone()), None, None)
                    }
                    LayerSource::ArcGis { base_url, service_name } => {
                        (None, None, None, Some(base_url.clone()), Some(service_name.clone()))
                    }
                };
                SavedLayer {
                    kind: l.kind.to_string(),
                    name: l.name.clone(),
                    visible: l.visible,
                    opacity: l.opacity,
                    path,
                    wms_url,
                    wms_layer,
                    arcgis_url,
                    arcgis_service,
                }
            })
            .collect()
    });
    drop(guard);

    if let Ok(store) = app.store("settings.json") {
        store.set(STORE_KEY, serde_json::json!(saved));
        let _ = store.save();
    }
}

fn load_saved(app: &tauri::AppHandle) -> Vec<SavedLayer> {
    use tauri_plugin_store::StoreExt;

    let Ok(store) = app.store("settings.json") else {
        return Vec::new();
    };
    store
        .get(STORE_KEY)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}
