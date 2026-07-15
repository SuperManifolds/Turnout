use leptos::{wasm_bindgen, component, view, IntoView, ReadSignal, WriteSignal, create_signal, SignalGet, SignalGetUntracked, SignalSet, SignalUpdate, spawn_local, Show, CollectView};
use wasm_bindgen::prelude::*;

use crate::tauri::BlueprintInfo;

#[derive(Clone, Copy, PartialEq)]
enum SortBy { Date, Name, Nodes }

#[wasm_bindgen]
extern "C" {
    fn map_fly_to(lng: f64, lat: f64, zoom: f64);
}

fn format_datetime(unix_secs: u64) -> String {
    let js_date = js_sys::Date::new(&JsValue::from_f64(unix_secs as f64 * 1000.0));
    let year = js_date.get_full_year();
    let month = js_date.get_month() + 1;
    let day = js_date.get_date();
    let hour = js_date.get_hours();
    let min = js_date.get_minutes();
    format!("{year}-{month:02}-{day:02} {hour:02}:{min:02}")
}

fn display_name(info: &BlueprintInfo) -> String {
    if info.clip_name.is_empty() {
        info.folder_name.clone()
    } else {
        info.clip_name.clone()
    }
}

#[component]
pub fn BlueprintDrawer(
    open: ReadSignal<bool>,
    set_open: WriteSignal<bool>,
) -> impl IntoView {
    let (blueprints, set_blueprints) = create_signal::<Vec<BlueprintInfo>>(Vec::new());
    let (search, set_search) = create_signal(String::new());
    let (sort_by, set_sort_by) = create_signal(SortBy::Date);
    let (loading, set_loading) = create_signal(false);
    let (confirm_delete, set_confirm_delete) = create_signal::<Option<String>>(None);
    let (editing, set_editing) = create_signal::<Option<String>>(None);
    let (edit_value, set_edit_value) = create_signal(String::new());
    let (error_msg, set_error_msg) = create_signal::<Option<String>>(None);

    let refresh = move || {
        set_loading.set(true);
        set_error_msg.set(None);
        spawn_local(async move {
            match crate::tauri::list_blueprints().await {
                Ok(mut list) => {
                    // Generate thumbnails for items that need them
                    for bp in &mut list {
                        if !bp.has_thumbnail
                            && crate::tauri::generate_thumbnail(&bp.folder_name, bp.clip_index).await.is_ok()
                        {
                            bp.has_thumbnail = true;
                        }
                    }
                    set_blueprints.set(list);
                }
                Err(e) => set_error_msg.set(Some(e)),
            }
            set_loading.set(false);
        });
    };

    // Refresh when drawer opens
    leptos::create_effect(move |prev_open: Option<bool>| {
        let is_open = open.get();
        if is_open && prev_open != Some(true) {
            refresh();
        }
        is_open
    });

    // Listen for filesystem changes from the backend watcher.
    crate::tauri::listen_to_events(&["blueprints-changed"], move |_| {
        if open.get_untracked() {
            refresh();
        }
    });

    let on_fly_to = move |bp: BlueprintInfo| {
        map_fly_to(bp.center_lon, bp.center_lat, 12.0);
        set_open.set(false);
    };

    let on_open_folder = move |folder_name: String| {
        spawn_local(async move {
            let _ = crate::tauri::open_blueprint_folder(&folder_name).await;
        });
    };

    let on_delete_confirm = move |folder_name: String| {
        set_confirm_delete.set(None);
        spawn_local(async move {
            match crate::tauri::delete_blueprint(&folder_name).await {
                Ok(()) => {
                    set_blueprints.update(|list| list.retain(|b| b.folder_name != *folder_name));
                }
                Err(e) => set_error_msg.set(Some(e)),
            }
        });
    };

    let on_rename_start = move |bp: &BlueprintInfo| {
        set_editing.set(Some(bp.folder_name.clone()));
        set_edit_value.set(display_name(bp));
    };

    let on_rename_confirm = move || {
        let Some(ref old_name) = editing.get_untracked() else { return };
        let old_name = old_name.clone();
        let new_name = edit_value.get_untracked().trim().to_string();
        if new_name.is_empty() || new_name == old_name {
            set_editing.set(None);
            return;
        }
        set_editing.set(None);
        spawn_local(async move {
            match crate::tauri::rename_blueprint(&old_name, &new_name).await {
                Ok(()) => {
                    set_blueprints.update(|list| {
                        for bp in list.iter_mut().filter(|b| b.folder_name == old_name) {
                            bp.folder_name.clone_from(&new_name);
                            bp.clip_name.clone_from(&new_name);
                            bp.has_thumbnail = false;
                        }
                    });
                }
                Err(e) => set_error_msg.set(Some(e)),
            }
        });
    };

    let on_rename_cancel = move || {
        set_editing.set(None);
    };

    view! {
        <Show when=move || open.get()>
            <aside id="blueprint-drawer">
                <header>
                    <h3>"Blueprints"</h3>
                    <div class="segmented">
                        <button class:active=move || sort_by.get() == SortBy::Date on:click=move |_| set_sort_by.set(SortBy::Date)>"Date"</button>
                        <button class:active=move || sort_by.get() == SortBy::Name on:click=move |_| set_sort_by.set(SortBy::Name)>"Name"</button>
                        <button class:active=move || sort_by.get() == SortBy::Nodes on:click=move |_| set_sort_by.set(SortBy::Nodes)>"Nodes"</button>
                    </div>
                </header>

                <input
                    class="drawer-search"
                    type="text"
                    placeholder="Search..."
                    prop:value=move || search.get()
                    on:input=move |ev| set_search.set(leptos::event_target_value(&ev))
                />

                <Show when=move || error_msg.get().is_some()>
                    <p class="error">{move || error_msg.get().unwrap_or_default()}</p>
                </Show>

                <Show when=move || loading.get()>
                    <p class="loading">"Loading..."</p>
                </Show>

                <Show when=move || !loading.get() && blueprints.get().is_empty()>
                    <p class="empty">"No blueprints found"</p>
                </Show>

                <ul>
                    {move || {
                        let query = search.get().to_lowercase();
                        let sort = sort_by.get();
                        let mut list: Vec<BlueprintInfo> = blueprints.get().into_iter()
                            .filter(|bp| query.is_empty() || display_name(bp).to_lowercase().contains(&query) || bp.folder_name.to_lowercase().contains(&query))
                            .collect();
                        match sort {
                            SortBy::Date => list.sort_by_key(|b| std::cmp::Reverse(b.modified)),
                            SortBy::Name => list.sort_by_key(|b| display_name(b).to_lowercase()),
                            SortBy::Nodes => list.sort_by_key(|b| std::cmp::Reverse(b.track_count)),
                        }
                        list.into_iter().map(|bp| {
                        let folder = bp.folder_name.clone();
                        let folder_fly = bp.clone();
                        let folder_open = folder.clone();
                        let folder_del = folder.clone();
                        let folder_rename = bp.clone();
                        let is_editing = {
                            let folder = folder.clone();
                            move || editing.get().as_deref() == Some(&folder)
                        };
                        let is_confirm = {
                            let folder = folder.clone();
                            move || confirm_delete.get().as_deref() == Some(&folder)
                        };
                        let name = display_name(&bp);
                        let meta = format_datetime(bp.modified);

                        let thumb_folder = folder.clone();
                        let thumb_clip_index = bp.clip_index;
                        let has_thumb = bp.has_thumbnail;

                        view! {
                            <li class="blueprint-item">
                                <div class="blueprint-thumb" on:click=move |_| on_fly_to(folder_fly.clone())>
                                    <Show when=move || has_thumb
                                        fallback=|| view! { <div class="thumb-placeholder"></div> }
                                    >
                                        <BlueprintThumbnail folder_name=thumb_folder.clone() clip_index=thumb_clip_index />
                                    </Show>
                                </div>
                                <div class="blueprint-info">
                                    <Show when=is_editing
                                        fallback={
                                            let name = name.clone();
                                            let folder_rename = folder_rename.clone();
                                            move || {
                                                let fr = folder_rename.clone();
                                                view! {
                                                    <span class="blueprint-name" on:dblclick=move |_| on_rename_start(&fr)>{name.clone()}</span>
                                                }
                                            }
                                        }
                                    >
                                        <input
                                            class="rename-input"
                                            prop:value=move || edit_value.get()
                                            on:input=move |ev| set_edit_value.set(leptos::event_target_value(&ev))
                                            on:keydown=move |ev| {
                                                if ev.key() == "Enter" { on_rename_confirm(); }
                                                if ev.key() == "Escape" { on_rename_cancel(); }
                                            }
                                            on:blur=move |_| on_rename_confirm()
                                            autofocus=true
                                        />
                                    </Show>
                                    <span class="blueprint-meta">{meta}</span>
                                </div>
                                <nav class="blueprint-actions">
                                    <Show when=is_confirm
                                        fallback={
                                            let folder_open = folder_open.clone();
                                            let folder_del = folder_del.clone();
                                            move || {
                                                let fo = folder_open.clone();
                                                let fd = folder_del.clone();
                                                view! {
                                                    <button class="icon-btn" title="Open folder" on:click=move |_| on_open_folder(fo.clone())>
                                                        <i class="fa-solid fa-folder-open"></i>
                                                    </button>
                                                    <button class="icon-btn danger" title="Delete" on:click=move |_| set_confirm_delete.set(Some(fd.clone()))>
                                                        <i class="fa-solid fa-trash"></i>
                                                    </button>
                                                }
                                            }
                                        }
                                    >
                                        {
                                            let folder_del_confirm = folder_del.clone();
                                            view! {
                                                <button class="icon-btn danger" title="Confirm delete" on:click=move |_| on_delete_confirm(folder_del_confirm.clone())>
                                                    <i class="fa-solid fa-check"></i>
                                                </button>
                                                <button class="icon-btn" title="Cancel" on:click=move |_| set_confirm_delete.set(None)>
                                                    <i class="fa-solid fa-xmark"></i>
                                                </button>
                                            }
                                        }
                                    </Show>
                                </nav>
                            </li>
                        }
                    }).collect_view()
                    }}
                </ul>
            </aside>
        </Show>
    }
}

/// Renders a thumbnail image from a base64 data URL returned by the backend.
#[component]
fn BlueprintThumbnail(folder_name: String, clip_index: usize) -> impl IntoView {
    let (src, set_src) = create_signal(String::new());
    spawn_local(async move {
        if let Ok(data_url) = crate::tauri::generate_thumbnail(&folder_name, clip_index).await {
            set_src.set(data_url);
        }
    });

    view! {
        <Show when=move || !src.get().is_empty()
            fallback=|| view! { <div class="thumb-placeholder"></div> }
        >
            <img src=move || src.get() alt="thumbnail" />
        </Show>
    }
}
