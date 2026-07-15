use leptos::{wasm_bindgen, component, view, IntoView, ReadSignal, WriteSignal, create_signal, SignalGet, SignalGetUntracked, SignalSet, spawn_local, Show, CollectView};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::tauri;

const TOAST_DURATION_MS: i32 = 4000;

#[wasm_bindgen]
extern "C" {
    fn map_add_overlay_layer(id: &str, url: &str, opacity: f64);
    fn map_remove_overlay_layer(id: &str);
    fn map_fit_bounds(west: f64, south: f64, east: f64, north: f64);
    fn map_set_orm_style(style_name: &str);
    fn map_hide_orm();
}

/// Human-readable name for an ORM style id, for the built-in layer label.
fn orm_mode_label(style: &str) -> &'static str {
    match style {
        "speed" => "Speed",
        "signals" => "Signals",
        "electrification" => "Electrification",
        "track" => "Track",
        "operator" => "Operator",
        "route" => "Route",
        _ => "Infrastructure",
    }
}

fn source_id(group_id: u32) -> String {
    format!("overlay-{group_id}")
}

fn sync_map_layers(status: &tauri::OverlayStatus, prev_group_ids: &[u32], force_refresh: bool) {
    let new_ids: Vec<u32> = status.groups.iter().map(|g| g.id).collect();

    for id in prev_group_ids {
        if !new_ids.contains(id) {
            map_remove_overlay_layer(&source_id(*id));
        }
    }

    for g in &status.groups {
        if !prev_group_ids.contains(&g.id) {
            map_add_overlay_layer(&source_id(g.id), &g.tile_url, 1.0);
        } else if force_refresh {
            map_remove_overlay_layer(&source_id(g.id));
            map_add_overlay_layer(&source_id(g.id), &g.tile_url, 1.0);
        }
    }
}

fn layer_icon(kind: &str) -> &'static str {
    match kind {
        "wms" => "fa-solid fa-globe",
        "wmts" => "fa-solid fa-map",
        "arcGis" => "fa-solid fa-server",
        "shp" => "fa-solid fa-shapes",
        "geoJson" => "fa-solid fa-code",
        "xyz" => "fa-solid fa-link",
        "apple" => "fa-solid fa-map-location-dot",
        "bing" => "fa-solid fa-satellite",
        _ => "fa-solid fa-layer-group",
    }
}

fn is_remote(kind: &str) -> bool {
    matches!(kind, "wms" | "arcGis" | "xyz" | "wmts" | "apple" | "bing")
}

#[derive(Clone, Copy, PartialEq)]
enum ServiceForm { None, Wms, Wmts, ArcGis, Xyz, CartoMetro }

#[derive(Clone)]
struct ServiceEntry {
    name: String,
    display: String,
}

#[component]
pub fn OverlayDrawer(
    open: ReadSignal<bool>,
    set_open: WriteSignal<bool>,
    orm_style: ReadSignal<String>,
    orm_visible: ReadSignal<bool>,
    set_orm_visible: WriteSignal<bool>,
) -> impl IntoView {
    let (status, set_status) = create_signal(tauri::OverlayStatus { groups: Vec::new() });
    let (loading, set_loading) = create_signal(false);
    let (toast, set_toast) = create_signal::<Option<String>>(None);
    let (copied_group, set_copied_group) = create_signal::<Option<u32>>(None);
    let (orm_copied, set_orm_copied) = create_signal(false);
    let (orm_port, set_orm_port) = create_signal::<Option<u16>>(None);

    let (active_form, set_active_form) = create_signal(ServiceForm::None);
    let (target_group, set_target_group) = create_signal::<Option<u32>>(None);
    let (service_url, set_service_url) = create_signal(String::new());
    let (service_loading, set_service_loading) = create_signal(false);
    let (service_entries, set_service_entries) = create_signal::<Vec<ServiceEntry>>(Vec::new());

    let (menu_open, set_menu_open) = create_signal(false);
    let (move_menu_layer, set_move_menu_layer) = create_signal::<Option<(u32, u32)>>(None);
    let (apple_creds, set_apple_creds) = create_signal::<Option<(String, Option<String>, Option<String>)>>(None);

    let load_apple_creds = move || {
        spawn_local(async move {
            let settings = crate::components::app_settings::load_settings().await;
            if let Some(ref key) = settings.apple_access_key
                && !key.is_empty()
                && (settings.apple_map_version.is_some() || settings.apple_sat_version.is_some())
            {
                let s = tauri::update_apple_urls(
                    key,
                    settings.apple_map_version.as_deref(),
                    settings.apple_sat_version.as_deref(),
                ).await;
                if !s.groups.is_empty() {
                    let prev: Vec<u32> = status.get_untracked().groups.iter().map(|g| g.id).collect();
                    sync_map_layers(&s, &prev, true);
                    set_status.set(s);
                }
                set_apple_creds.set(Some((
                    key.clone(),
                    settings.apple_map_version,
                    settings.apple_sat_version,
                )));
            } else {
                set_apple_creds.set(None);
            }
        });
    };

    load_apple_creds();

    // Reload creds both on a manual settings change and when the background
    // token refresher stores a fresh key, so newly added Apple layers never
    // pick up a stale access key.
    tauri::listen_to_events(&["settings-changed", "apple-token-refreshed"], move |_| load_apple_creds());

    let group_ids = move || status.get_untracked().groups.iter().map(|g| g.id).collect::<Vec<_>>();

    let show_toast = move |msg: String| {
        set_toast.set(Some(msg));
        spawn_local(async move {
            let _ = wasm_bindgen_futures::JsFuture::from(
                js_sys::Promise::new(&mut |resolve, _| {
                    if let Some(w) = web_sys::window() {
                        let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, TOAST_DURATION_MS);
                    }
                }),
            ).await;
            set_toast.set(None);
        });
    };

    let apply_status = move |new_status: tauri::OverlayStatus| {
        let prev = group_ids();
        sync_map_layers(&new_status, &prev, true);
        set_status.set(new_status);
    };

    let apply_status_no_refresh = move |new_status: tauri::OverlayStatus| {
        let prev = group_ids();
        sync_map_layers(&new_status, &prev, false);
        set_status.set(new_status);
    };

    spawn_local(async move {
        let s = tauri::restore_overlays().await;
        if !s.groups.is_empty() {
            apply_status(s);
        }
    });

    let on_add_file = move |group_id: Option<u32>| {
        set_menu_open.set(false);
        spawn_local(async move {
            let Some(path) = tauri::pick_kmz_file().await else { return };
            set_loading.set(true);
            match tauri::add_overlay(&path, group_id).await {
                Ok(s) => {
                    if let Some(last_group) = s.groups.last()
                        && let Some(last_layer) = last_group.layers.last()
                    {
                        let [south, west, north, east] = last_layer.bbox;
                        map_fit_bounds(west, south, east, north);
                    }
                    apply_status(s);
                }
                Err(e) => show_toast(e),
            }
            set_loading.set(false);
        });
    };

    let on_new_group = move |_| {
        set_menu_open.set(false);
        spawn_local(async move {
            match tauri::create_group("New group").await {
                Ok(s) => apply_status(s),
                Err(e) => show_toast(e),
            }
        });
    };

    let on_remove_group = move |gid: u32| {
        spawn_local(async move { apply_status(tauri::remove_group(gid).await); });
    };

    let on_reorder_group = move |gid: u32, dir: &'static str| {
        spawn_local(async move { apply_status_no_refresh(tauri::reorder_group(gid, dir).await); });
    };

    let on_rename_group = move |gid: u32, name: String| {
        spawn_local(async move { apply_status_no_refresh(tauri::rename_group(gid, &name).await); });
    };

    let on_group_visible = move |gid: u32, visible: bool| {
        spawn_local(async move { apply_status(tauri::set_group_visible(gid, visible).await); });
    };

    let on_remove_layer = move |gid: u32, lid: u32| {
        spawn_local(async move { apply_status(tauri::remove_overlay(gid, lid).await); });
    };

    let on_rename_layer = move |gid: u32, lid: u32, name: String| {
        spawn_local(async move { apply_status_no_refresh(tauri::rename_layer(gid, lid, &name).await); });
    };

    let on_zoom_layer = move |bbox: [f64; 4]| {
        let [south, west, north, east] = bbox;
        map_fit_bounds(west, south, east, north);
    };

    let on_toggle_visible = move |gid: u32, lid: u32, visible: bool| {
        spawn_local(async move {
            apply_status(tauri::set_layer_visible(gid, lid, visible).await);
        });
    };

    let on_layer_opacity = move |gid: u32, lid: u32, val: f32| {
        spawn_local(async move {
            apply_status(tauri::set_layer_opacity(gid, lid, val).await);
        });
    };

    let on_reorder = move |gid: u32, lid: u32, dir: &'static str| {
        spawn_local(async move {
            apply_status(tauri::reorder_layer(gid, lid, dir).await);
        });
    };

    let on_move_layer = move |lid: u32, from_gid: u32, to_gid: u32| {
        set_move_menu_layer.set(None);
        spawn_local(async move {
            match tauri::move_layer(lid, from_gid, to_gid).await {
                Ok(s) => apply_status(s),
                Err(e) => show_toast(e),
            }
        });
    };

    let on_copy_url = move |gid: u32, url: String| {
        spawn_local(async move {
            let Some(window) = web_sys::window() else { return };
            let clipboard = window.navigator().clipboard();
            let _ = wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&url)).await;
            set_copied_group.set(Some(gid));
            let _ = wasm_bindgen_futures::JsFuture::from(
                js_sys::Promise::new(&mut |resolve, _| {
                    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 2000);
                }),
            ).await;
            set_copied_group.set(None);
        });
    };

    // Resolve the ORM tile server port once so the copy button can build endpoint
    // URLs; the toggle itself works without it.
    spawn_local(async move {
        if let Ok(port) = tauri::get_orm_port().await {
            set_orm_port.set(Some(port));
        }
    });

    let on_toggle_orm = move |_| {
        let visible = !orm_visible.get();
        set_orm_visible.set(visible);
        if visible {
            map_set_orm_style(&orm_style.get());
        } else {
            map_hide_orm();
        }
    };

    let on_copy_orm = move |_| {
        let Some(port) = orm_port.get() else { return };
        let url = format!("http://127.0.0.1:{port}/{}/tilejson.json", orm_style.get());
        spawn_local(async move {
            let Some(window) = web_sys::window() else { return };
            let _ = wasm_bindgen_futures::JsFuture::from(
                window.navigator().clipboard().write_text(&url),
            ).await;
            set_orm_copied.set(true);
            let _ = wasm_bindgen_futures::JsFuture::from(
                js_sys::Promise::new(&mut |resolve, _| {
                    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 2000);
                }),
            ).await;
            set_orm_copied.set(false);
        });
    };

    let open_service_form = move |form: ServiceForm, gid: Option<u32>| {
        set_menu_open.set(false);
        if active_form.get() == form && target_group.get() == gid {
            set_active_form.set(ServiceForm::None);
        } else {
            set_active_form.set(form);
            set_target_group.set(gid);
            set_service_url.set(String::new());
            set_service_entries.set(Vec::new());
            // CartoMetro has a fixed catalogue rather than a URL to fetch — load it
            // (starting the proxy) as soon as the form opens.
            if form == ServiceForm::CartoMetro {
                set_service_loading.set(true);
                spawn_local(async move {
                    match tauri::start_cartometro().await {
                        Ok(info) => set_service_entries.set(
                            info.cities.into_iter()
                                .map(|c| ServiceEntry { name: c.tile_url, display: c.name })
                                .collect(),
                        ),
                        Err(e) => show_toast(e),
                    }
                    set_service_loading.set(false);
                });
            }
        }
    };

    let do_fetch = move || {
        let url = service_url.get();
        if url.trim().is_empty() { return; }
        let form = active_form.get();
        let gid = target_group.get();

        if form == ServiceForm::Xyz {
            let name = url.split('/').find(|s| s.contains('.')).unwrap_or(&url).to_string();
            set_service_loading.set(true);
            spawn_local(async move {
                match tauri::add_xyz_layer(&url, &name, gid).await {
                    Ok(s) => {
                        apply_status(s);
                        set_active_form.set(ServiceForm::None);
                        set_service_url.set(String::new());
                    }
                    Err(e) => show_toast(e),
                }
                set_service_loading.set(false);
            });
            return;
        }

        set_service_loading.set(true);
        set_service_entries.set(Vec::new());
        spawn_local(async move {
            let result = match form {
                ServiceForm::Wms => tauri::fetch_wms_layers(&url).await.map(|layers|
                    layers.into_iter().map(|l| ServiceEntry { name: l.name, display: l.title }).collect()),
                ServiceForm::Wmts => tauri::fetch_wmts_layers(&url).await.map(|layers|
                    layers.into_iter().map(|l| ServiceEntry { name: l.tile_url, display: l.title }).collect()),
                ServiceForm::ArcGis => tauri::fetch_arcgis_services(&url).await.map(|services|
                    services.into_iter().map(|s| ServiceEntry { name: s.name.clone(), display: s.name }).collect()),
                _ => return,
            };
            match result {
                Ok(entries) => set_service_entries.set(entries),
                Err(e) => show_toast(e),
            }
            set_service_loading.set(false);
        });
    };

    let on_service_select = move |name: String, display: String| {
        let url = service_url.get();
        let form = active_form.get();
        let gid = target_group.get();
        set_service_loading.set(true);
        spawn_local(async move {
            let result = match form {
                ServiceForm::Wms => tauri::add_wms_layer(&url, &name, &display, gid).await,
                ServiceForm::Wmts => tauri::add_xyz_layer_with_kind(&name, &display, gid, Some("wmts")).await,
                ServiceForm::ArcGis => tauri::add_arcgis_layer(&url, &name, &display, gid).await,
                // For CartoMetro the entry's `name` is the local tile-URL template.
                ServiceForm::CartoMetro => tauri::add_xyz_layer(&name, &format!("CartoMetro {display}"), gid).await,
                ServiceForm::None | ServiceForm::Xyz => return,
            };
            match result {
                Ok(s) => {
                    apply_status(s);
                    set_active_form.set(ServiceForm::None);
                    set_service_url.set(String::new());
                    set_service_entries.set(Vec::new());
                }
                Err(e) => show_toast(e),
            }
            set_service_loading.set(false);
        });
    };

    let on_add_apple = move |satellite: bool| {
        set_menu_open.set(false);
        let Some((key, map_ver, sat_ver)) = apple_creds.get() else { return };
        let gid = target_group.get();
        let (tile_url, name) = if satellite {
            let Some(ver) = sat_ver else { return };
            (turnout_core::geo::apple_tile_url(&key, &ver, true), "Apple Satellite")
        } else {
            let Some(ver) = map_ver else { return };
            (turnout_core::geo::apple_tile_url(&key, &ver, false), "Apple Maps")
        };
        set_service_loading.set(true);
        spawn_local(async move {
            match tauri::add_xyz_layer_with_kind(&tile_url, name, gid, Some("apple")).await {
                Ok(s) => apply_status(s),
                Err(e) => show_toast(e),
            }
            set_service_loading.set(false);
        });
    };

    let on_add_bing = move |tile_type: &'static str, name: &'static str| {
        set_menu_open.set(false);
        let gid = target_group.get();
        let tile_url = format!(
            "https://ecn.t0.tiles.virtualearth.net/tiles/{tile_type}{{q}}?g=1&mkt=en"
        );
        spawn_local(async move {
            match tauri::add_xyz_layer_with_kind(&tile_url, name, gid, Some("bing")).await {
                Ok(s) => apply_status(s),
                Err(e) => show_toast(e),
            }
        });
    };

    let on_url_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Enter" { do_fetch(); }
    };

    view! {
        <Show when=move || open.get()>
            <aside id="overlay-drawer">
                <header>
                    <h3>"Overlays"</h3>
                    <button class="close-btn" on:click=move |_| set_open.set(false) title="Close">
                        <i class="fa-solid fa-xmark"></i>
                    </button>
                </header>

                <section class="overlay-actions">
                    <div class="add-menu">
                        <button on:click=move |_| set_menu_open.set(!menu_open.get()) disabled=move || loading.get()>
                            <i class="fa-solid fa-plus"></i>
                            {move || if loading.get() { " Loading\u{2026}" } else { " Add" }}
                        </button>
                        <Show when=move || menu_open.get()>
                            <ul class="add-menu-list">
                                <li on:click=move |_| on_add_file(None)>
                                    <i class="fa-solid fa-file"></i>" File overlay"
                                </li>
                                <li on:click=move |_| open_service_form(ServiceForm::Wms, None)>
                                    <i class="fa-solid fa-globe"></i>" WMS server"
                                </li>
                                <li on:click=move |_| open_service_form(ServiceForm::Wmts, None)>
                                    <i class="fa-solid fa-map"></i>" WMTS server"
                                </li>
                                <li on:click=move |_| open_service_form(ServiceForm::ArcGis, None)>
                                    <i class="fa-solid fa-server"></i>" ArcGIS MapServer"
                                </li>
                                <li on:click=move |_| open_service_form(ServiceForm::Xyz, None)>
                                    <i class="fa-solid fa-link"></i>" XYZ tile URL"
                                </li>
                                <li on:click=move |_| open_service_form(ServiceForm::CartoMetro, None)>
                                    <i class="fa-solid fa-train-subway"></i>" CartoMetro"
                                </li>
                                <li on:click=move |_| on_add_bing("a", "Bing Aerial")>
                                    <i class="fa-solid fa-satellite"></i>" Bing Aerial"
                                </li>
                                <li on:click=move |_| on_add_bing("r", "Bing Roads")>
                                    <i class="fa-solid fa-road"></i>" Bing Roads"
                                </li>
                                {move || {
                                    let creds = apple_creds.get();
                                    let has_map = creds.as_ref().is_some_and(|(_, mv, _)| mv.is_some());
                                    let has_sat = creds.as_ref().is_some_and(|(_, _, sv)| sv.is_some());
                                    view! {
                                        {if has_map { Some(view! {
                                            <li on:click=move |_| on_add_apple(false)>
                                                <i class="fa-solid fa-map-location-dot"></i>" Apple Maps"
                                            </li>
                                        }) } else { None }}
                                        {if has_sat { Some(view! {
                                            <li on:click=move |_| on_add_apple(true)>
                                                <i class="fa-solid fa-satellite"></i>" Apple Satellite"
                                            </li>
                                        }) } else { None }}
                                    }
                                }}
                            </ul>
                        </Show>
                    </div>
                    <button on:click=on_new_group>
                        <i class="fa-solid fa-folder-plus"></i>" Group"
                    </button>
                </section>

                <Show when=move || active_form.get() != ServiceForm::None>
                    <section class="service-form">
                        <input
                            type="text"
                            placeholder=move || match active_form.get() {
                                ServiceForm::Wms => "WMS server URL",
                                ServiceForm::Wmts => "WMTS server URL",
                                ServiceForm::ArcGis => "ArcGIS services URL",
                                ServiceForm::Xyz => "https://example.com/{z}/{x}/{y}.png",
                                ServiceForm::CartoMetro => "Filter cities\u{2026}",
                                ServiceForm::None => "",
                            }
                            prop:value=move || service_url.get()
                            on:input=move |ev| set_service_url.set(leptos::event_target_value(&ev))
                            on:keydown=on_url_keydown
                        />
                        <Show when=move || active_form.get() != ServiceForm::CartoMetro>
                            <button on:click=move |_| do_fetch() disabled=move || service_loading.get() || service_url.get().trim().is_empty()>
                                {move || if service_loading.get() { "Loading\u{2026}" } else if active_form.get() == ServiceForm::Xyz { "Add" } else { "Fetch" }}
                            </button>
                        </Show>
                    </section>
                    <Show when=move || !service_entries.get().is_empty()>
                        <ul class="service-list">
                            {move || {
                                let needle = service_url.get().to_lowercase();
                                let filtering = active_form.get() == ServiceForm::CartoMetro && !needle.is_empty();
                                service_entries.get().iter()
                                    .filter(|e| !filtering || e.display.to_lowercase().contains(&needle))
                                    .map(|e| {
                                        let name = e.name.clone();
                                        let display = e.display.clone();
                                        let label = display.clone();
                                        view! {
                                            <li on:click=move |_| on_service_select(name.clone(), display.clone())>
                                                {label}
                                            </li>
                                        }
                                    }).collect_view()
                            }}
                        </ul>
                    </Show>
                </Show>

                <Show when=move || status.get().groups.is_empty() && !loading.get() && active_form.get() == ServiceForm::None>
                    <p class="empty">"No overlay groups"</p>
                </Show>

                <div class="groups-list">
                    <section class="group-section">
                        <ul class="group-layers">
                            <li class="overlay-item">
                                <button class="icon-btn visibility-toggle"
                                    on:click=on_toggle_orm
                                    title=move || if orm_visible.get() { "Hide" } else { "Show" }
                                >
                                    <i class=move || if orm_visible.get() { "fa-solid fa-eye" } else { "fa-solid fa-eye-slash" }></i>
                                </button>
                                <i class="fa-solid fa-train-tram overlay-icon"></i>
                                <input class="layer-name" type="text" readonly=true
                                    prop:value=move || format!("OpenRailwayMap \u{00b7} {}", orm_mode_label(&orm_style.get()))
                                />
                                <button class="icon-btn" title="Copy TileJSON URL" on:click=on_copy_orm>
                                    <i class=move || if orm_copied.get() { "fa-solid fa-check" } else { "fa-solid fa-copy" }></i>
                                </button>
                            </li>
                        </ul>
                    </section>
                    {move || status.get().groups.iter().map(|g| {
                        let gid = g.id;
                        let gname = g.name.clone();
                        let tilejson_url_copy = g.tilejson_url.clone();
                        let layers = g.layers.clone();
                        let layer_count = layers.len();
                        let group_idx = status.get().groups.iter().position(|og| og.id == gid).unwrap_or(0);
                        let is_first_group = group_idx == 0;
                        let is_last_group = group_idx + 1 >= status.get().groups.len();
                        let all_visible = layers.iter().all(|l| l.visible);
                        let all_group_ids: Vec<(u32, String)> = status.get().groups.iter()
                            .filter(|og| og.id != gid)
                            .map(|og| (og.id, og.name.clone()))
                            .collect();

                        view! {
                            <section class="group-section">
                                <header class="group-header">
                                    <button class="icon-btn visibility-toggle"
                                        on:click=move |_| on_group_visible(gid, !all_visible)
                                        title=if all_visible { "Hide all" } else { "Show all" }
                                    >
                                        <i class=if all_visible { "fa-solid fa-eye" } else { "fa-solid fa-eye-slash" }></i>
                                    </button>
                                    <i class="fa-solid fa-pencil group-rename-hint"></i>
                                    <input
                                        class="group-name"
                                        type="text"
                                        value=gname
                                        on:change=move |ev: web_sys::Event| {
                                            let Some(target) = ev.target() else { return };
                                            let input: web_sys::HtmlInputElement = target.unchecked_into();
                                            on_rename_group(gid, input.value());
                                        }
                                    />
                                    <button class="icon-btn reorder-btn" title="Move up" disabled=is_first_group
                                        on:click=move |_| on_reorder_group(gid, "up")
                                    ><i class="fa-solid fa-chevron-up"></i></button>
                                    <button class="icon-btn reorder-btn" title="Move down" disabled=is_last_group
                                        on:click=move |_| on_reorder_group(gid, "down")
                                    ><i class="fa-solid fa-chevron-down"></i></button>
                                    <button class="icon-btn" on:click=move |_| on_copy_url(gid, tilejson_url_copy.clone())
                                        title="Copy TileJSON URL"
                                    >
                                        <i class=move || if copied_group.get() == Some(gid) { "fa-solid fa-check" } else { "fa-solid fa-copy" }></i>
                                    </button>
                                    <button class="icon-btn danger" on:click=move |_| on_remove_group(gid) title="Delete group">
                                        <i class="fa-solid fa-trash"></i>
                                    </button>
                                </header>
                                <ul class="group-layers">
                                    {layers.iter().enumerate().map(|(idx, l)| {
                                        let lid = l.id;
                                        let name = l.name.clone();
                                        let visible = l.visible;
                                        let layer_opacity = l.opacity;
                                        let icon = layer_icon(&l.kind);
                                        let remote = is_remote(&l.kind);
                                        let has_errors = l.has_errors;
                                        let bbox = l.bbox;
                                        let is_remote_bbox = bbox == [-85.051_129_f64, -180.0, 85.051_129, 180.0];
                                        let is_first = idx == 0;
                                        let is_last = idx + 1 == layer_count;
                                        let has_other_groups = !all_group_ids.is_empty();
                                        let move_targets: Vec<(u32, String)> = all_group_ids.clone();
                                        view! {
                                            <li class="overlay-item">
                                                <button class="icon-btn visibility-toggle"
                                                    on:click=move |_| on_toggle_visible(gid, lid, !visible)
                                                    title=if visible { "Hide" } else { "Show" }
                                                >
                                                    <i class=if visible { "fa-solid fa-eye" } else { "fa-solid fa-eye-slash" }></i>
                                                </button>
                                                <i class=format!("{icon} overlay-icon{}{}", if remote { " remote-icon" } else { "" }, if has_errors { " error-icon" } else { "" })></i>
                                                <input
                                                    class="layer-name"
                                                    type="text"
                                                    value=name
                                                    on:change=move |ev: web_sys::Event| {
                                                        let Some(target) = ev.target() else { return };
                                                        let input: web_sys::HtmlInputElement = target.unchecked_into();
                                                        on_rename_layer(gid, lid, input.value());
                                                    }
                                                />
                                                {if is_remote_bbox {
                                                    None
                                                } else {
                                                    Some(view! {
                                                        <button class="icon-btn" title="Zoom to" on:click=move |_| on_zoom_layer(bbox)>
                                                            <i class="fa-solid fa-crosshairs"></i>
                                                        </button>
                                                    })
                                                }}
                                                <nav class="layer-buttons">
                                                    <button class="icon-btn reorder-btn" title="Move up"
                                                        disabled=is_first
                                                        on:click=move |_| on_reorder(gid, lid, "up")
                                                    >
                                                        <i class="fa-solid fa-chevron-up"></i>
                                                    </button>
                                                    <button class="icon-btn reorder-btn" title="Move down"
                                                        disabled=is_last
                                                        on:click=move |_| on_reorder(gid, lid, "down")
                                                    >
                                                        <i class="fa-solid fa-chevron-down"></i>
                                                    </button>
                                                    {if has_other_groups {
                                                        Some(view! {
                                                            <div class="move-menu">
                                                                <button class="icon-btn" title="Move to..."
                                                                    on:click=move |_| {
                                                                        let cur = move_menu_layer.get();
                                                                        if cur == Some((gid, lid)) {
                                                                            set_move_menu_layer.set(None);
                                                                        } else {
                                                                            set_move_menu_layer.set(Some((gid, lid)));
                                                                        }
                                                                    }
                                                                >
                                                                    <i class="fa-solid fa-arrow-right-arrow-left"></i>
                                                                </button>
                                                                <Show when=move || move_menu_layer.get() == Some((gid, lid))>
                                                                    <ul class="move-menu-list">
                                                                        {move_targets.iter().map(|(to_gid, to_name)| {
                                                                            let to_gid = *to_gid;
                                                                            let label = to_name.clone();
                                                                            view! {
                                                                                <li on:click=move |_| on_move_layer(lid, gid, to_gid)>{label}</li>
                                                                            }
                                                                        }).collect_view()}
                                                                    </ul>
                                                                </Show>
                                                            </div>
                                                        })
                                                    } else {
                                                        None
                                                    }}
                                                    <button class="icon-btn danger" title="Remove" on:click=move |_| on_remove_layer(gid, lid)>
                                                        <i class="fa-solid fa-trash"></i>
                                                    </button>
                                                </nav>
                                                <span class="opacity-row">
                                                    <input
                                                        type="range"
                                                        class="layer-opacity"
                                                        min="0" max="1" step="0.05"
                                                        prop:value=layer_opacity.to_string()
                                                        on:change=move |ev: web_sys::Event| {
                                                            let Some(target) = ev.target() else { return };
                                                            let input: web_sys::HtmlInputElement = target.unchecked_into();
                                                            let val: f32 = input.value().parse().unwrap_or(1.0);
                                                            on_layer_opacity(gid, lid, val);
                                                        }
                                                        title="Layer opacity"
                                                    />
                                                    <span class="opacity-value">{format!("{}%", (layer_opacity * 100.0) as u32)}</span>
                                                </span>
                                            </li>
                                        }
                                    }).collect_view()}
                                </ul>
                            </section>
                        }
                    }).collect_view()}
                </div>
            </aside>
        </Show>

        <Show when=move || toast.get().is_some()>
            <div class="overlay-toast">
                <i class="fa-solid fa-circle-exclamation"></i>
                {move || toast.get().unwrap_or_default()}
            </div>
        </Show>
    }
}
