use leptos::{
    component, view, Callable, Callback, CollectView, IntoView, ReadSignal, Show, SignalGet,
    SignalGetUntracked, SignalSet, create_signal, spawn_local,
};

use crate::tauri;

/// Panel that builds a per-vertical-layer railway MVT source from the selected
/// area and hands back the TileJSON URL to paste into NIMBY Rails.
#[component]
pub fn VectorLayers(
    bbox: ReadSignal<Option<(f64, f64, f64, f64)>>,
    on_close: Callback<()>,
) -> impl IntoView {
    let (info, set_info) = create_signal::<Option<tauri::VectorLayersInfo>>(None);
    let (loading, set_loading) = create_signal(false);
    let (error, set_error) = create_signal::<Option<String>>(None);
    let (copied, set_copied) = create_signal(false);

    let on_generate = move |_| {
        let Some((s, w, n, e)) = bbox.get_untracked() else {
            set_error.set(Some("Select an area on the map first.".to_string()));
            return;
        };
        set_error.set(None);
        set_copied.set(false);
        set_loading.set(true);
        spawn_local(async move {
            let timeout = crate::components::app_settings::load_settings().await.overpass_timeout;
            match tauri::start_orm_vector_layers(s, w, n, e, timeout).await {
                Ok(v) => set_info.set(Some(v)),
                Err(err) => set_error.set(Some(err)),
            }
            set_loading.set(false);
        });
    };

    let on_stop = move |_| {
        spawn_local(async move { tauri::stop_orm_vector_layers().await });
        set_info.set(None);
        set_copied.set(false);
    };

    let open_workshop = move |_| {
        spawn_local(async move {
            let _ = tauri::open_workshop_mod().await;
        });
    };

    let copy_url = move |_| {
        if let Some(v) = info.get_untracked() {
            if let Some(win) = web_sys::window() {
                let _ = win.navigator().clipboard().write_text(&v.tilejson_url);
            }
            set_copied.set(true);
        }
    };

    view! {
        <div class="tile-download-panel rail-layers-panel">
            <header>
                <h3>"Rail Layers for NIMBY Rails"</h3>
                <button class="icon-btn" on:click=move |_| on_close.call(()) title="Close">
                    <i class="fa-solid fa-xmark"></i>
                </button>
            </header>

            <p class="hint">
                "Builds a vector-tile source of railways in the selected area, split by vertical layer. Add the URL to NIMBY Rails as a map source and enable the "
                <strong>"ORM Vertical Layers"</strong>
                " mod to colour by type and toggle heights."
            </p>

            <button class="workshop-link" on:click=open_workshop>
                <i class="fa-brands fa-steam"></i>
                "Get the mod on Steam Workshop"
            </button>

            {move || error.get().map(|e| view! { <p class="error-text">{e}</p> })}

            <Show when=move || info.get().is_none()>
                <button
                    class="primary"
                    on:click=on_generate
                    disabled=move || loading.get() || bbox.get().is_none()
                >
                    {move || if loading.get() { "Fetching railways…" } else { "Generate rail layers" }}
                </button>
            </Show>

            {move || info.get().map(|v| {
                let count = v.levels.len();
                let items = v.levels.iter().map(|l| {
                    let name = l.layer_name.clone();
                    let desc = l.description.clone();
                    view! {
                        <li>
                            <span class="level-name">{name}</span>
                            <span class="level-desc">{desc}</span>
                        </li>
                    }
                }).collect_view();
                view! {
                    <fieldset>
                        <label>"TileJSON URL — add as a map source in NIMBY Rails"</label>
                        <div class="url-copy">
                            <input type="text" readonly=true prop:value=v.tilejson_url.clone() />
                            <button class="icon-btn" on:click=copy_url title="Copy URL">
                                <i class="fa-solid fa-copy"></i>
                            </button>
                        </div>
                        {move || copied.get().then(|| view! {
                            <small class="success-text">"Copied to clipboard"</small>
                        })}
                    </fieldset>
                    <fieldset>
                        <label>{format!("{count} vertical layer(s) — toggle each in-game")}</label>
                        <ul class="rail-level-list">{items}</ul>
                    </fieldset>
                    <button class="stop-btn" on:click=on_stop>"Stop server"</button>
                }
            })}
        </div>
    }
}
