use leptos::{
    component, view, Callable, Callback, CollectView, IntoView, Show, SignalGet,
    SignalGetUntracked, SignalSet, create_effect, create_signal, spawn_local,
};

use crate::tauri;

/// Panel that starts the local CartoMetro proxy and hands back a per-city TileJSON
/// URL to paste into NIMBY Rails as a map source.
#[component]
pub fn CartoMetro(on_close: Callback<()>) -> impl IntoView {
    let (cities, set_cities) = create_signal::<Vec<tauri::CartoCity>>(Vec::new());
    let (loading, set_loading) = create_signal(true);
    let (error, set_error) = create_signal::<Option<String>>(None);
    let (filter, set_filter) = create_signal(String::new());
    let (selected, set_selected) = create_signal::<Option<tauri::CartoCity>>(None);
    let (copied, set_copied) = create_signal(false);

    // Start the proxy and load the city catalog when the panel opens.
    create_effect(move |_| {
        spawn_local(async move {
            match tauri::start_cartometro().await {
                Ok(info) => set_cities.set(info.cities),
                Err(e) => set_error.set(Some(e)),
            }
            set_loading.set(false);
        });
    });

    let copy_url = move |_| {
        if let Some(c) = selected.get_untracked() {
            if let Some(win) = web_sys::window() {
                let _ = win.navigator().clipboard().write_text(&c.tilejson_url);
            }
            set_copied.set(true);
        }
    };

    let filtered = move || {
        let needle = filter.get().to_lowercase();
        cities
            .get()
            .into_iter()
            .filter(|c| needle.is_empty() || c.name.to_lowercase().contains(&needle))
            .collect::<Vec<_>>()
    };

    view! {
        <div class="tile-download-panel cartometro-panel">
            <header>
                <h3>"CartoMetro Maps"</h3>
                <button class="icon-btn" on:click=move |_| on_close.call(()) title="Close">
                    <i class="fa-solid fa-xmark"></i>
                </button>
            </header>

            <p class="hint">
                "Adds a CartoMetro transit map for a city as a NIMBY Rails map source. Pick a city, then add its URL as a map source."
            </p>
            <p class="hint">
                <i class="fa-solid fa-circle-info"></i>
                " Maps are © cartometro.com — served from their site for personal use."
            </p>

            {move || error.get().map(|e| view! { <p class="error-text">{e}</p> })}

            <Show when=move || loading.get()>
                <p class="hint">"Starting the CartoMetro proxy…"</p>
            </Show>

            <Show when=move || !loading.get() && error.get().is_none()>
                <fieldset>
                    <label>"City"</label>
                    <input
                        type="text"
                        placeholder="Filter cities…"
                        prop:value=move || filter.get()
                        on:input=move |ev| set_filter.set(leptos::event_target_value(&ev))
                    />
                    <ul class="cartometro-city-list">
                        {move || filtered().into_iter().map(|c| {
                            let is_sel = selected.get().map(|s| s.slug) == Some(c.slug.clone());
                            let pick = c.clone();
                            view! {
                                <li
                                    class:active=is_sel
                                    on:click=move |_| { set_selected.set(Some(pick.clone())); set_copied.set(false); }
                                >
                                    {c.name.clone()}
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                </fieldset>

                {move || selected.get().map(|c| view! {
                    <fieldset>
                        <label>"TileJSON URL — add as a map source in NIMBY Rails"</label>
                        <div class="url-copy">
                            <input type="text" readonly=true prop:value=c.tilejson_url.clone() />
                            <button class="icon-btn" on:click=copy_url title="Copy URL">
                                <i class="fa-solid fa-copy"></i>
                            </button>
                        </div>
                        {move || copied.get().then(|| view! {
                            <small class="success-text">"Copied to clipboard"</small>
                        })}
                        <p class="hint">{format!("Zoom {}–{}", c.min_zoom, c.max_zoom)}</p>
                    </fieldset>
                })}
            </Show>
        </div>
    }
}
