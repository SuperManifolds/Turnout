use leptos::{
    component, create_effect, create_signal, event_target_value, spawn_local, view, wasm_bindgen,
    web_sys, CollectView, IntoView, ReadSignal, Show, SignalGet, SignalSet, WriteSignal,
};
use wasm_bindgen::prelude::*;

use crate::tauri;

/// Map source id for the built-in population heatmap.
const POPULATION_SOURCE_ID: &str = "population";
/// Heatmap opacity so the base map reads through.
const POPULATION_OPACITY: f64 = 0.8;
/// Brush size (on-screen radius, px) and per-dab strength (people) bounds.
const BRUSH_RADIUS_MIN: f64 = 5.0;
const BRUSH_RADIUS_MAX: f64 = 120.0;
const BRUSH_STRENGTH_MIN: u32 = 1;
const BRUSH_STRENGTH_MAX: u32 = 500;

#[wasm_bindgen]
extern "C" {
    fn map_add_overlay_layer(id: &str, url: &str, opacity: f64);
    fn map_remove_overlay_layer(id: &str);
    fn map_refresh_population();
}

/// A file's stem (last path segment without extension), for a default layer name.
fn file_stem(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem).to_string()
}

/// Guess which DBF field holds population, so the import form pre-selects it.
fn guess_pop_field(fields: &[String]) -> String {
    fields
        .iter()
        .find(|f| {
            let u = f.to_uppercase();
            u == "P1_001N" || u.contains("POP") || u.starts_with("P001") || u.contains("TOTAL")
        })
        .or_else(|| fields.first())
        .cloned()
        .unwrap_or_default()
}

/// The active population-editing tool. Drives both the map interaction (paint vs
/// region-select) and the cursor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PopTool {
    None,
    Brush,
    Erase,
    Select,
}

impl PopTool {
    /// CSS cursor for this tool while it is active.
    #[must_use]
    pub fn cursor(self) -> &'static str {
        match self {
            PopTool::None => "",
            PopTool::Brush | PopTool::Select => "crosshair",
            PopTool::Erase => "cell",
        }
    }

    /// True for the paint tools (as opposed to region-select).
    #[must_use]
    pub fn is_paint(self) -> bool {
        matches!(self, PopTool::Brush | PopTool::Erase)
    }
}

#[component]
pub fn Population(
    open: ReadSignal<bool>,
    set_open: WriteSignal<bool>,
    tool: ReadSignal<PopTool>,
    set_tool: WriteSignal<PopTool>,
    brush_radius: ReadSignal<f64>,
    set_brush_radius: WriteSignal<f64>,
    brush_strength: ReadSignal<u32>,
    set_brush_strength: WriteSignal<u32>,
    selection_bbox: ReadSignal<Option<(f64, f64, f64, f64)>>,
) -> impl IntoView {
    let (layers_list, set_layers_list) = create_signal::<Vec<tauri::PopLayerInfo>>(Vec::new());
    let (apply_state, set_apply_state) = create_signal(tauri::PopApplyStatus::default());
    let (region_total, set_region_total) = create_signal(0.0_f64);
    let (region_target, set_region_target) = create_signal(String::new());
    let (toast, set_toast) = create_signal::<Option<String>>(None);

    // Import flow: a picked source awaiting field/mode/scale confirmation.
    // `import_kind` is "shp" (needs a field) or "tif" (needs a selected area).
    let (import_path, set_import_path) = create_signal::<Option<String>>(None);
    let (import_kind, set_import_kind) = create_signal("shp".to_string());
    let (import_fields, set_import_fields) = create_signal::<Vec<String>>(Vec::new());
    let (import_field, set_import_field) = create_signal(String::new());
    let (import_name, set_import_name) = create_signal(String::new());
    let (import_blend, set_import_blend) = create_signal("normal".to_string());
    let (import_scale, set_import_scale) = create_signal(1.0_f64);
    let (importing, set_importing) = create_signal(false);

    let show_toast = move |msg: String| set_toast.set(Some(msg));

    let refresh_status = move || {
        spawn_local(async move {
            set_layers_list.set(tauri::pop_layers().await);
            set_apply_state.set(tauri::pop_apply_status().await);
        });
    };

    // Opening the editor shows the heatmap and selects the Brush tool; closing it
    // hides the heatmap and drops the active tool.
    create_effect(move |_| {
        if open.get() {
            spawn_local(async move {
                match tauri::add_population_layer().await {
                    Ok(url) => {
                        map_add_overlay_layer(POPULATION_SOURCE_ID, &url, POPULATION_OPACITY);
                        set_layers_list.set(tauri::pop_layers().await);
                        set_apply_state.set(tauri::pop_apply_status().await);
                    }
                    Err(e) => set_toast.set(Some(e)),
                }
            });
            if tool.get() == PopTool::None {
                set_tool.set(PopTool::Brush);
            }
        } else {
            map_remove_overlay_layer(POPULATION_SOURCE_ID);
            set_tool.set(PopTool::None);
            spawn_local(async move { tauri::remove_population_layer().await; });
        }
    });

    // Refresh the selection total whenever the region or tool changes.
    create_effect(move |_| {
        let bbox = selection_bbox.get();
        if tool.get() != PopTool::Select {
            return;
        }
        let Some((s, w, n, e)) = bbox else {
            set_region_total.set(0.0);
            return;
        };
        spawn_local(async move { set_region_total.set(tauri::pop_region_total(w, s, e, n).await); });
    });

    let apply_region = move |flat: bool| {
        let Some((s, w, n, e)) = selection_bbox.get() else { return };
        let Ok(target) = region_target.get().trim().parse::<f64>() else { return };
        spawn_local(async move {
            tauri::pop_set_region(w, s, e, n, target, flat).await;
            map_refresh_population();
            set_region_total.set(tauri::pop_region_total(w, s, e, n).await);
            set_apply_state.set(tauri::pop_apply_status().await);
        });
    };

    let on_add_layer = move |_| {
        spawn_local(async move { set_layers_list.set(tauri::pop_add_layer().await); });
    };
    let on_select_layer = move |id: u32| {
        spawn_local(async move { set_layers_list.set(tauri::pop_set_active_layer(id).await); });
    };
    let on_toggle_layer = move |id: u32, visible: bool| {
        spawn_local(async move {
            set_layers_list.set(tauri::pop_set_layer_visible(id, !visible).await);
            map_refresh_population();
        });
    };
    let on_remove_layer = move |id: u32| {
        spawn_local(async move {
            set_layers_list.set(tauri::pop_remove_layer(id).await);
            map_refresh_population();
        });
    };
    let on_move_layer = move |id: u32, up: bool| {
        spawn_local(async move { set_layers_list.set(tauri::pop_move_layer(id, up).await); });
    };
    let on_rename_layer = move |id: u32, name: String| {
        spawn_local(async move { set_layers_list.set(tauri::pop_rename_layer(id, &name).await); });
    };
    let on_clear_edits = move |_| {
        spawn_local(async move {
            tauri::pop_clear_edits().await;
            map_refresh_population();
        });
    };
    let on_toggle_blend = move |id: u32, blend: String| {
        let next = if blend == "add" { "normal" } else { "add" };
        spawn_local(async move {
            set_layers_list.set(tauri::pop_set_layer_blend(id, next).await);
            map_refresh_population();
        });
    };

    // Pick a shapefile, then show the import form pre-filled with its fields.
    let on_import_click = move |_| {
        spawn_local(async move {
            let Some(path) = tauri::pick_shapefile().await else { return };
            match tauri::pop_shapefile_fields(&path).await {
                Ok(fields) if !fields.is_empty() => {
                    set_import_field.set(guess_pop_field(&fields));
                    set_import_fields.set(fields);
                    set_import_name.set(file_stem(&path));
                    set_import_kind.set("shp".to_string());
                    set_import_path.set(Some(path));
                }
                Ok(_) => show_toast("That shapefile has no attribute fields.".to_string()),
                Err(e) => show_toast(e),
            }
        });
    };
    // Pick a GeoTIFF; it imports into the selected area, so no field is needed.
    let on_import_tiff = move |_| {
        spawn_local(async move {
            let Some(path) = tauri::pick_geotiff().await else { return };
            set_import_name.set(file_stem(&path));
            set_import_kind.set("tif".to_string());
            set_import_path.set(Some(path));
        });
    };
    // Add a pre-baked PMTiles (e.g. the census bake) as a file-backed layer —
    // memory-mapped, no in-app rasterization.
    let on_add_source = move |_| {
        spawn_local(async move {
            let Some(path) = tauri::pick_pmtiles().await else { return };
            let name = file_stem(&path);
            let layers = tauri::pop_add_source_layer(&path, &name, "normal").await;
            if layers.is_empty() {
                set_toast.set(Some("Could not open that PMTiles file.".to_string()));
            } else {
                set_layers_list.set(layers);
                map_refresh_population();
            }
        });
    };
    let cancel_import = move |_| set_import_path.set(None);
    let do_import = move |_| {
        let Some(path) = import_path.get() else { return };
        let name = import_name.get();
        let name = if name.trim().is_empty() { "Imported".to_string() } else { name };
        let (blend, scale, kind) = (import_blend.get(), import_scale.get(), import_kind.get());

        let finish = move |res: Result<tauri::PopImportResult, String>| {
            spawn_local(async move {
                match res {
                    Ok(res) => {
                        set_layers_list.set(res.layers);
                        set_apply_state.set(tauri::pop_apply_status().await);
                        show_toast(format!(
                            "Imported {} people across {} cells (matched ×{:.3})",
                            res.source_total.round() as u64,
                            res.covered_pixels,
                            res.applied_scale
                        ));
                        map_refresh_population();
                        set_import_path.set(None);
                    }
                    Err(e) => show_toast(e),
                }
                set_importing.set(false);
            });
        };

        if kind == "tif" {
            let Some((s, w, n, e)) = selection_bbox.get() else {
                show_toast("Select an area first — GeoTIFF import fills the selection.".to_string());
                return;
            };
            set_importing.set(true);
            spawn_local(async move {
                finish(tauri::pop_import_geotiff(&path, &name, &blend, scale, (w, s, e, n)).await);
            });
        } else {
            let field = import_field.get();
            if field.is_empty() {
                return;
            }
            set_importing.set(true);
            spawn_local(async move {
                finish(tauri::pop_import_shapefile(&path, &field, &name, &blend, scale).await);
            });
        }
    };

    let on_apply = move |_| {
        spawn_local(async move {
            match tauri::pop_apply().await {
                Ok(n) => {
                    show_toast(format!("Applied to game — {n} tiles written"));
                    refresh_status();
                    map_refresh_population();
                }
                Err(e) => show_toast(e),
            }
        });
    };
    let on_restore = move |_| {
        spawn_local(async move {
            match tauri::pop_restore_original().await {
                Ok(()) => {
                    show_toast("Restored the original population map".to_string());
                    refresh_status();
                    map_refresh_population();
                }
                Err(e) => show_toast(e),
            }
        });
    };

    let tool_button = move |t: PopTool, icon: &'static str, label: &'static str| {
        view! {
            <button class="tool-btn" class:active=move || tool.get() == t
                title=label
                on:click=move |_| set_tool.set(if tool.get() == t { PopTool::None } else { t })>
                <i class=icon></i>
                <span>{label}</span>
            </button>
        }
    };

    view! {
        <Show when=move || open.get()>
            <aside id="population-panel">
                <header>
                    <h3>"Population"</h3>
                    <button class="close-btn" title="Close" on:click=move |_| set_open.set(false)>
                        <i class="fa-solid fa-xmark"></i>
                    </button>
                </header>

                <section class="pop-tools">
                    {tool_button(PopTool::Brush, "fa-solid fa-paintbrush", "Brush")}
                    {tool_button(PopTool::Erase, "fa-solid fa-eraser", "Erase")}
                    {tool_button(PopTool::Select, "fa-solid fa-vector-square", "Select")}
                </section>

                <Show when=move || tool.get().is_paint()>
                    <section class="pop-brush-settings">
                        <label>"Size"
                            <input type="range" min=BRUSH_RADIUS_MIN max=BRUSH_RADIUS_MAX step="1"
                                prop:value=move || brush_radius.get()
                                on:input=move |ev| set_brush_radius.set(
                                    event_target_value(&ev).parse().unwrap_or(BRUSH_RADIUS_MIN))
                            />
                        </label>
                        <label>"Strength"
                            <input type="range" min=BRUSH_STRENGTH_MIN max=BRUSH_STRENGTH_MAX step="1"
                                prop:value=move || brush_strength.get()
                                on:input=move |ev| set_brush_strength.set(
                                    event_target_value(&ev).parse().unwrap_or(BRUSH_STRENGTH_MIN))
                            />
                        </label>
                    </section>
                </Show>

                <Show when=move || tool.get() == PopTool::Select && selection_bbox.get().is_some()>
                    <section class="pop-region">
                        <span class="pop-total">
                            {move || format!("Selected total: {}", region_total.get().round() as u64)}
                        </span>
                        <div class="pop-region-set">
                            <input type="number" min="0" placeholder="Target total"
                                prop:value=move || region_target.get()
                                on:input=move |ev| set_region_target.set(event_target_value(&ev))
                            />
                            <button class="mode-btn" title="Scale existing density to the target"
                                on:click=move |_| apply_region(false)>"Scale"</button>
                            <button class="mode-btn" title="Fill the area evenly to the target"
                                on:click=move |_| apply_region(true)>"Fill"</button>
                        </div>
                    </section>
                </Show>

                <section class="pop-layers">
                    <div class="pop-layers-header">
                        <span>"Layers"</span>
                        <div>
                            <button class="icon-btn" title="Import a shapefile (e.g. US Census)" on:click=on_import_click>
                                <i class="fa-solid fa-file-import"></i>
                            </button>
                            <button class="icon-btn" title="Import a GeoTIFF into the selected area (e.g. WorldPop)" on:click=on_import_tiff>
                                <i class="fa-solid fa-image"></i>
                            </button>
                            <button class="icon-btn" title="Add a baked data layer (PMTiles, e.g. US census)" on:click=on_add_source>
                                <i class="fa-solid fa-database"></i>
                            </button>
                            <button class="icon-btn" title="Add layer" on:click=on_add_layer>
                                <i class="fa-solid fa-plus"></i>
                            </button>
                            <button class="icon-btn" title="Clear all edits" on:click=on_clear_edits>
                                <i class="fa-solid fa-broom"></i>
                            </button>
                        </div>
                    </div>

                    <Show when=move || import_path.get().is_some()>
                        <div class="pop-import">
                            <Show when=move || import_kind.get() == "shp">
                                <label>"Population field"
                                    <select prop:value=move || import_field.get()
                                        on:change=move |ev| set_import_field.set(event_target_value(&ev))>
                                        {move || import_fields.get().into_iter().map(|f| {
                                            view! { <option value=f.clone()>{f}</option> }
                                        }).collect_view()}
                                    </select>
                                </label>
                            </Show>
                            <Show when=move || import_kind.get() == "tif">
                                <p class="pop-import-hint">
                                    {move || if selection_bbox.get().is_some() {
                                        "Imports into the selected area.".to_string()
                                    } else {
                                        "Select an area first — GeoTIFF imports fill the selection.".to_string()
                                    }}
                                </p>
                            </Show>
                            <label>"Layer name"
                                <input type="text" prop:value=move || import_name.get()
                                    on:input=move |ev| set_import_name.set(event_target_value(&ev)) />
                            </label>
                            <label>"Mode"
                                <select prop:value=move || import_blend.get()
                                    on:change=move |ev| set_import_blend.set(event_target_value(&ev))>
                                    <option value="normal">"Replace base"</option>
                                    <option value="add">"Add on top"</option>
                                </select>
                            </label>
                            <label>"Scale"
                                <input type="number" min="0" step="0.1"
                                    prop:value=move || import_scale.get()
                                    on:input=move |ev| set_import_scale.set(
                                        event_target_value(&ev).parse().unwrap_or(1.0)) />
                            </label>
                            <div class="pop-import-actions">
                                <button class="mode-btn primary" disabled=move || importing.get()
                                    on:click=do_import>
                                    {move || if importing.get() { "Importing…" } else { "Import" }}
                                </button>
                                <button class="mode-btn" on:click=cancel_import>"Cancel"</button>
                            </div>
                        </div>
                    </Show>
                    <ul class="pop-layer-list">
                        {move || layers_list.get().into_iter().map(|l| {
                            let (id, visible) = (l.id, l.visible);
                            let is_base = l.kind == "base";
                            let has_blend = l.kind == "import" || l.kind == "source";
                            let blend = l.blend.clone();

                            let name_el = if is_base {
                                view! { <span class="layer-name base-name">{l.name.clone()}</span> }.into_view()
                            } else {
                                view! {
                                    <input class="layer-name" type="text" prop:value=l.name.clone()
                                        on:click=move |ev: web_sys::MouseEvent| ev.stop_propagation()
                                        on:change=move |ev| on_rename_layer(id, event_target_value(&ev)) />
                                }.into_view()
                            };
                            let blend_btn = if has_blend {
                                let (b, label) = (blend.clone(), if blend == "add" { "＋" } else { "▣" });
                                let title = if blend == "add" { "Add on top (click to Replace)" } else { "Replace base (click to Add)" };
                                view! {
                                    <button class="icon-btn blend-toggle" title=title
                                        on:click=move |ev: web_sys::MouseEvent| { ev.stop_propagation(); on_toggle_blend(id, b.clone()); }>
                                        {label}
                                    </button>
                                }.into_view()
                            } else {
                                ().into_view()
                            };
                            let controls = if is_base {
                                ().into_view()
                            } else {
                                view! {
                                    <button class="icon-btn" title="Move up"
                                        on:click=move |ev: web_sys::MouseEvent| { ev.stop_propagation(); on_move_layer(id, true); }>
                                        <i class="fa-solid fa-chevron-up"></i>
                                    </button>
                                    <button class="icon-btn" title="Move down"
                                        on:click=move |ev: web_sys::MouseEvent| { ev.stop_propagation(); on_move_layer(id, false); }>
                                        <i class="fa-solid fa-chevron-down"></i>
                                    </button>
                                    <button class="icon-btn" title="Delete layer"
                                        on:click=move |ev: web_sys::MouseEvent| { ev.stop_propagation(); on_remove_layer(id); }>
                                        <i class="fa-solid fa-trash"></i>
                                    </button>
                                }.into_view()
                            };
                            view! {
                                <li class="pop-layer" class:active=l.active class:base-layer=is_base
                                    on:click=move |_| on_select_layer(id)>
                                    <button class="icon-btn visibility-toggle" title="Toggle visibility"
                                        on:click=move |ev: web_sys::MouseEvent| { ev.stop_propagation(); on_toggle_layer(id, visible); }>
                                        <i class=if visible { "fa-solid fa-eye" } else { "fa-solid fa-eye-slash" }></i>
                                    </button>
                                    {name_el}
                                    {blend_btn}
                                    {controls}
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                </section>

                <section class="pop-apply">
                    <button class="mode-btn primary" title="Write these edits into the game's map file"
                        disabled=move || !apply_state.get().has_edits
                        on:click=on_apply>"Apply to game"</button>
                    <Show when=move || apply_state.get().has_backup>
                        <button class="mode-btn" title="Restore the original population map"
                            on:click=on_restore>"Restore original"</button>
                    </Show>
                </section>

                <Show when=move || toast.get().is_some()>
                    <p class="pop-toast">{move || toast.get().unwrap_or_default()}</p>
                </Show>
            </aside>
        </Show>
    }
}
