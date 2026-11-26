//! Overlay persistence: the on-disk schema (`SavedGroup`/`SavedLayer`/`SavedSource`),
//! saving the live groups to the store, loading them back, and rebuilding one
//! layer on restore. The live tile-server state is the source of truth; this maps
//! it 1:1 to a serializable form (and back) and never stores short-lived secrets.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::server_core::UnpoisonExt;
use crate::tile_server::{self, LayerKind, SourceDef};

use super::{parse_overlay_file, OverlayState};

const STORE_KEY: &str = "overlay_groups";
/// Substring unique to Apple's satellite tile host, used to tell a satellite Apple
/// layer from a standard-map one when recovering `sat` from a legacy URL.
const APPLE_SAT_MARKER: &str = "sat-cdn";

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct SavedGroup {
    // Persist the group id so its tile-server port (PREFERRED_PORT + id) is
    // stable across restarts. Optional for backward compatibility with stores
    // written before ids were saved (those restore in sequential order).
    #[serde(default)]
    pub(super) id: Option<u32>,
    pub(super) name: String,
    pub(super) layers: Vec<SavedLayer>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct SavedLayer {
    name: String,
    visible: bool,
    opacity: f32,
    #[serde(flatten)]
    source: SavedSource,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum SavedSource {
    Kmz {
        path: String,
    },
    Shp {
        path: String,
    },
    GeoJson {
        path: String,
    },
    Wms {
        wms_url: String,
        wms_layer: String,
    },
    ArcGis {
        arcgis_url: String,
        arcgis_service: String,
    },
    Xyz {
        xyz_url: String,
    },
    Wmts {
        xyz_url: String,
    },
    Apple {
        /// Satellite (vs standard map) imagery. The short-lived Apple access token
        /// is deliberately NOT persisted — the URL is rebuilt from the stored Apple
        /// credentials on restore, so the store never holds a stale/secret token.
        #[serde(default)]
        sat: bool,
        /// Back-compat: older stores persisted the full tokenized URL. Read to
        /// recover `sat` and used as a fallback; never written by current saves.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        xyz_url: Option<String>,
    },
    Bing {
        xyz_url: String,
    },
    MbTiles {
        path: String,
    },
}

/// Assign a concrete id to each restored group. A saved id is reused when it is
/// still free (keeping its tile-server port stable across restarts); a missing
/// id (legacy store) or a collision falls back to the lowest free id, which
/// preserves the saved order.
pub(super) fn allocate_group_ids(saved_ids: &[Option<u32>]) -> Vec<u32> {
    let mut used: HashSet<u32> = HashSet::new();
    saved_ids
        .iter()
        .map(|&preferred| {
            let id = match preferred {
                Some(id) if !used.contains(&id) => id,
                // The lowest free id is at most used.len() (pigeonhole over the
                // used-so-far set), so this bounded search always succeeds.
                _ => (0..=used.len() as u32)
                    .find(|id| !used.contains(id))
                    .expect("a free id exists"),
            };
            used.insert(id);
            id
        })
        .collect()
}

/// The layer kind for a restored XYZ-family source. The kind must round-trip so
/// that kind-specific behaviour (notably Apple token refresh) still applies to
/// restored layers.
fn kind_for_saved_source(source: &SavedSource) -> LayerKind {
    match source {
        SavedSource::Wmts { .. } => LayerKind::Wmts,
        SavedSource::Apple { .. } => LayerKind::Apple,
        SavedSource::Bing { .. } => LayerKind::Bing,
        _ => LayerKind::Xyz,
    }
}

/// The stored Apple credentials used to rebuild restored Apple layer URLs, since
/// the short-lived token is intentionally not persisted with the layer.
pub(super) struct AppleCreds {
    pub(super) access_key: Option<String>,
    pub(super) map_version: Option<String>,
    pub(super) sat_version: Option<String>,
}

impl AppleCreds {
    /// A tile URL for the given imagery, or `None` when the key or the relevant
    /// version is missing.
    fn url(&self, sat: bool) -> Option<String> {
        let key = self.access_key.as_deref()?;
        let ver = if sat {
            self.sat_version.as_deref()
        } else {
            self.map_version.as_deref()
        }?;
        Some(turnout_core::geo::apple_tile_url(key, ver, sat))
    }
}

pub(super) fn restore_layer(
    handle: &tile_server::ServerHandle,
    layer: &SavedLayer,
    apple: &AppleCreds,
) {
    // Capture the id the add actually assigned so the follow-up name/visibility/
    // opacity apply to THIS layer. A `.last()` guess would silently target the
    // previous layer whenever an add no-ops (e.g. a file whose geometry vanished
    // between sessions), corrupting the sibling.
    let id = match &layer.source {
        SavedSource::Kmz { path } | SavedSource::Shp { path } | SavedSource::GeoJson { path } => {
            let kind = match &layer.source {
                SavedSource::Shp { .. } => LayerKind::Shp,
                SavedSource::GeoJson { .. } => LayerKind::GeoJson,
                _ => LayerKind::Kmz,
            };
            let Ok(data) = parse_overlay_file(path, kind) else {
                tracing::warn!("restore: failed to parse {path}");
                return;
            };
            let Some(id) = handle.add_kmz_layer(data, Some(path.clone()), kind) else {
                tracing::warn!("restore: {path} has no geometry to display");
                return;
            };
            id
        }
        SavedSource::Wms { wms_url, wms_layer } => {
            handle.add_wms_layer(wms_url.clone(), wms_layer.clone(), layer.name.clone())
        }
        SavedSource::ArcGis {
            arcgis_url,
            arcgis_service,
        } => handle.add_arcgis_layer(
            arcgis_url.clone(),
            arcgis_service.clone(),
            layer.name.clone(),
        ),
        SavedSource::Apple { sat, xyz_url } => {
            let is_sat = *sat
                || xyz_url
                    .as_deref()
                    .is_some_and(|u| u.contains(APPLE_SAT_MARKER));
            // Rebuild the URL from the stored credentials (the freshest known key);
            // the token refresher rewrites it again once it fetches a live token.
            // Fall back to a legacy saved URL, then to a token-less marker URL that
            // is still correctly classified — so the layer is kept (and filled in by
            // the next refresh) rather than dropped when credentials are absent.
            let url = apple
                .url(is_sat)
                .or_else(|| xyz_url.clone())
                .unwrap_or_else(|| turnout_core::geo::apple_tile_url("", "", is_sat));
            handle.add_xyz_layer_with_kind(url, layer.name.clone(), LayerKind::Apple)
        }
        SavedSource::Xyz { xyz_url }
        | SavedSource::Wmts { xyz_url }
        | SavedSource::Bing { xyz_url } => {
            // Preserve the specific kind so kind-specific behaviour still applies.
            handle.add_xyz_layer_with_kind(
                xyz_url.clone(),
                layer.name.clone(),
                kind_for_saved_source(&layer.source),
            )
        }
        SavedSource::MbTiles { path } => {
            match handle.add_mbtiles_layer(path.clone(), layer.name.clone()) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!("restore: failed to open MBTiles {path}: {e}");
                    return;
                }
            }
        }
    };

    handle.rename_layer(id, layer.name.clone());
    handle.set_layer_visible(id, layer.visible);
    handle.set_layer_opacity(id, layer.opacity);
}

pub(super) fn save_groups(app: &tauri::AppHandle) {
    use tauri::Manager;
    use tauri_plugin_store::StoreExt;

    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();
    let saved: Vec<SavedGroup> = groups
        .iter()
        .map(|g| {
            let layers = g.handle.state.layers.read().unpoison();
            SavedGroup {
                id: Some(g.id),
                name: g.name.clone(),
                layers: layers
                    .iter()
                    .filter_map(|l| {
                        // 1:1 map from the live source to its persisted form.
                        let source = match &l.source {
                            SourceDef::Kmz { path: Some(p) } => {
                                SavedSource::Kmz { path: p.clone() }
                            }
                            SourceDef::Shp { path: Some(p) } => {
                                SavedSource::Shp { path: p.clone() }
                            }
                            SourceDef::GeoJson { path: Some(p) } => {
                                SavedSource::GeoJson { path: p.clone() }
                            }
                            // File layers without a path can't be re-parsed on
                            // restore; population is a built-in, non-persistent
                            // layer served from its own handle. Both are skipped.
                            SourceDef::Kmz { path: None }
                            | SourceDef::Shp { path: None }
                            | SourceDef::GeoJson { path: None }
                            | SourceDef::Pop { .. } => return None,
                            SourceDef::Wms {
                                base_url,
                                layer_name,
                            } => SavedSource::Wms {
                                wms_url: base_url.clone(),
                                wms_layer: layer_name.clone(),
                            },
                            SourceDef::ArcGis {
                                base_url,
                                service_name,
                            } => SavedSource::ArcGis {
                                arcgis_url: base_url.clone(),
                                arcgis_service: service_name.clone(),
                            },
                            SourceDef::Xyz { url_template } => SavedSource::Xyz {
                                xyz_url: url_template.clone(),
                            },
                            SourceDef::Wmts { url_template } => SavedSource::Wmts {
                                xyz_url: url_template.clone(),
                            },
                            SourceDef::Bing { url_template } => SavedSource::Bing {
                                xyz_url: url_template.clone(),
                            },
                            // Persist only which imagery it is, never the tokenized URL.
                            SourceDef::Apple { sat } => SavedSource::Apple {
                                sat: *sat,
                                xyz_url: None,
                            },
                            SourceDef::MbTiles { path, .. } => {
                                SavedSource::MbTiles { path: path.clone() }
                            }
                        };
                        Some(SavedLayer {
                            name: l.name.clone(),
                            visible: l.visible,
                            opacity: l.opacity,
                            source,
                        })
                    })
                    .collect(),
            }
        })
        .collect();
    drop(groups);

    match app.store("settings.json") {
        Ok(store) => {
            store.set(STORE_KEY, serde_json::json!(saved));
            if let Err(e) = store.save() {
                tracing::error!("failed to persist overlays: {e}");
            }
        }
        Err(e) => tracing::error!("failed to open store to persist overlays: {e}"),
    }
}

pub(super) fn load_saved(app: &tauri::AppHandle) -> Vec<SavedGroup> {
    use tauri_plugin_store::StoreExt;

    let Ok(store) = app.store("settings.json") else {
        return Vec::new();
    };
    store
        .get(STORE_KEY)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contiguous_ids_are_preserved() {
        assert_eq!(
            allocate_group_ids(&[Some(0), Some(1), Some(2)]),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn gap_after_removal_keeps_ids_and_ports() {
        // Group 1 was removed in a prior session; the survivor must keep id 2
        // (and therefore its port), not slide down to 1.
        assert_eq!(allocate_group_ids(&[Some(0), Some(2)]), vec![0, 2]);
    }

    #[test]
    fn reorder_keeps_each_groups_id() {
        assert_eq!(
            allocate_group_ids(&[Some(2), Some(0), Some(1)]),
            vec![2, 0, 1]
        );
    }

    #[test]
    fn legacy_store_without_ids_falls_back_to_sequential() {
        assert_eq!(allocate_group_ids(&[None, None, None]), vec![0, 1, 2]);
    }

    #[test]
    fn duplicate_saved_ids_are_disambiguated() {
        assert_eq!(allocate_group_ids(&[Some(1), Some(1)]), vec![1, 0]);
    }

    #[test]
    fn mixed_legacy_and_saved_ids_avoid_collision() {
        assert_eq!(allocate_group_ids(&[Some(2), None, None]), vec![2, 0, 1]);
    }

    #[test]
    fn saved_group_json_round_trips_stably() {
        // Locks the on-disk overlay schema: a group with a mix of source kinds must
        // serialize and deserialize back to the identical JSON. A field rename or a
        // variant change that broke this would silently drop users' overlays.
        let group = SavedGroup {
            id: Some(2),
            name: "Reference".to_string(),
            layers: vec![
                SavedLayer {
                    name: "kmz".into(),
                    visible: true,
                    opacity: 1.0,
                    source: SavedSource::Kmz {
                        path: "/a/b.kmz".into(),
                    },
                },
                SavedLayer {
                    name: "wms".into(),
                    visible: false,
                    opacity: 0.5,
                    source: SavedSource::Wms {
                        wms_url: "http://w".into(),
                        wms_layer: "L".into(),
                    },
                },
                SavedLayer {
                    name: "sat".into(),
                    visible: true,
                    opacity: 0.8,
                    source: SavedSource::Apple {
                        sat: true,
                        xyz_url: None,
                    },
                },
                SavedLayer {
                    name: "mb".into(),
                    visible: true,
                    opacity: 1.0,
                    source: SavedSource::MbTiles {
                        path: "/t.mbtiles".into(),
                    },
                },
            ],
        };
        let json = serde_json::to_string(&group).expect("serialize");
        let back: SavedGroup = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(serde_json::to_string(&back).expect("re-serialize"), json);
    }

    #[test]
    fn restored_xyz_family_keeps_its_kind() {
        let url = || "https://example.test/{z}/{x}/{y}".to_string();
        assert_eq!(
            kind_for_saved_source(&SavedSource::Apple {
                sat: false,
                xyz_url: None
            }),
            LayerKind::Apple
        );
        assert_eq!(
            kind_for_saved_source(&SavedSource::Bing { xyz_url: url() }),
            LayerKind::Bing
        );
        assert_eq!(
            kind_for_saved_source(&SavedSource::Wmts { xyz_url: url() }),
            LayerKind::Wmts
        );
        assert_eq!(
            kind_for_saved_source(&SavedSource::Xyz { xyz_url: url() }),
            LayerKind::Xyz
        );
    }

    #[test]
    fn legacy_apple_store_deserializes_with_url_and_sat_default() {
        // Old stores persisted the full tokenized URL under `xyz_url` (enum
        // rename_all renames variants, not their fields); it must still load
        // (sat defaults false, url retained for fallback / sat recovery).
        let legacy =
            r#"{"kind":"apple","xyz_url":"https://sat-cdn.apple-mapkit.com/tile?accessKey=abc"}"#;
        let src: SavedSource = serde_json::from_str(legacy).expect("legacy apple loads");
        match src {
            SavedSource::Apple { sat, xyz_url } => {
                assert!(!sat);
                assert!(xyz_url
                    .as_deref()
                    .is_some_and(|u| u.contains(APPLE_SAT_MARKER)));
            }
            _ => panic!("expected Apple"),
        }
    }

    #[test]
    fn apple_creds_build_url_only_with_key_and_version() {
        let creds = AppleCreds {
            access_key: Some("k".into()),
            map_version: Some("9".into()),
            sat_version: None,
        };
        assert!(creds.url(false).is_some_and(|u| u.contains("access")));
        assert!(creds.url(true).is_none(), "no sat version -> no sat url");
        let none = AppleCreds {
            access_key: None,
            map_version: Some("9".into()),
            sat_version: None,
        };
        assert!(none.url(false).is_none(), "no key -> no url");
    }
}
