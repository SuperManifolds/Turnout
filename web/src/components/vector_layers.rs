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

    let on_generate = move |_| {
        let Some((s, w, n, e)) = bbox.get_untracked() else {
            set_error.set(Some("Select an area on the map first.".to_string()));
            return;
        };
        set_error.set(None);
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
    };

    view! {
        <div class="tile-download-panel">
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
                    view! { <li>{desc}" "<small>"("{name}")"</small></li> }
                }).collect_view();
                view! {
                    <section class="offline-active">
                        <label>"TileJSON URL — paste into NIMBY Rails \u{2192} map sources:"</label>
                        <input type="text" readonly=true prop:value=v.tilejson_url.clone() />
                        <p>{format!("{count} vertical layer(s) in this area:")}</p>
                        <ul class="level-list">{items}</ul>
                        <button on:click=on_stop>"Stop server"</button>
                    </section>
                }
            })}
        </div>
    }
}
