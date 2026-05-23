use leptos::{wasm_bindgen, component, view, web_sys, IntoView, create_signal, store_value, SignalSet, spawn_local, event_target_value, SignalGetUntracked, Show, SignalGet, CollectView};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

const NOMINATIM_URL: &str = "https://nominatim.openstreetmap.org/search";
const DEBOUNCE_MS: i32 = 300;

#[wasm_bindgen]
extern "C" {
    fn map_fly_to(lng: f64, lat: f64, zoom: f64);
}

#[derive(Clone, serde::Deserialize)]
struct NominatimResult {
    lat: String,
    lon: String,
    display_name: String,
}

async fn geocode(query: &str) -> Result<Vec<NominatimResult>, String> {
    let url = format!("{NOMINATIM_URL}?q={}&format=json&limit=5", urlencoding(query));
    let window = web_sys::window().ok_or("no window")?;
    let resp = JsFuture::from(window.fetch_with_str(&url))
        .await
        .map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp.into();
    let text = JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let json = text.as_string().ok_or("no text")?;
    serde_json::from_str(&json).map_err(|e| e.to_string())
}

use crate::utils::urlencoding;

fn fly_to_result(r: &NominatimResult) {
    if let (Ok(lat), Ok(lng)) = (r.lat.parse::<f64>(), r.lon.parse::<f64>()) {
        map_fly_to(lng, lat, 14.0);
    }
}

#[component]
pub fn Search() -> impl IntoView {
    let (query, set_query) = create_signal(String::new());
    let (results, set_results) = create_signal::<Vec<NominatimResult>>(vec![]);
    let (show_results, set_show_results) = create_signal(false);
    let debounce_handle = store_value::<Option<i32>>(None);

    let do_search = move |q: String| {
        if q.trim().len() < 2 {
            set_results.set(vec![]);
            set_show_results.set(false);
            return;
        }
        spawn_local(async move {
            match geocode(&q).await {
                Ok(r) => {
                    set_show_results.set(!r.is_empty());
                    set_results.set(r);
                }
                Err(e) => {
                    web_sys::console::error_1(&format!("Geocode error: {e}").into());
                }
            }
        });
    };

    let on_input = move |ev: leptos::ev::Event| {
        let val = event_target_value(&ev);
        set_query.set(val.clone());

        // Debounce: cancel previous timer, start new one
        if let Some(handle) = debounce_handle.get_value() {
            web_sys::window()
                .expect("window")
                .clear_timeout_with_handle(handle);
        }
        let cb = Closure::once(move || do_search(val));
        let handle = web_sys::window()
            .expect("window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                DEBOUNCE_MS,
            )
            .unwrap_or(0);
        cb.forget();
        debounce_handle.set_value(Some(handle));
    };

    let on_submit = move |ev: web_sys::SubmitEvent| {
        ev.prevent_default();
        let r = results.get_untracked();
        if let Some(first) = r.first() {
            fly_to_result(first);
            set_show_results.set(false);
        } else {
            // No results yet — trigger immediate search
            let q = query.get_untracked();
            if q.trim().len() >= 2 {
                let q2 = q.clone();
                spawn_local(async move {
                    if let Ok(r) = geocode(&q2).await {
                        if let Some(first) = r.first() {
                            fly_to_result(first);
                        }
                        set_results.set(r);
                    }
                });
            }
        }
    };

    view! {
        <form id="search-form" on:submit=on_submit>
            <input
                type="text"
                placeholder="Search location..."
                prop:value=query
                on:input=on_input
                on:focus=move |_| {
                    if !results.get_untracked().is_empty() {
                        set_show_results.set(true);
                    }
                }
                on:blur=move |_| {
                    // Delay hide to allow click on results
                    let set_show = set_show_results;
                    let cb = Closure::once(move || set_show.set(false));
                    let _ = web_sys::window().expect("window")
                        .set_timeout_with_callback_and_timeout_and_arguments_0(
                            cb.as_ref().unchecked_ref(), 200);
                    cb.forget();
                }
            />
            <Show when=move || show_results.get() && !results.get().is_empty()>
                <ul id="search-results">
                    {move || results.get().into_iter().map(|r| {
                        let r2 = r.clone();
                        view! {
                            <li on:mousedown=move |_| {
                                fly_to_result(&r2);
                                set_show_results.set(false);
                                set_query.set(r2.display_name.clone());
                            }>
                                {&r.display_name}
                            </li>
                        }
                    }).collect_view()}
                </ul>
            </Show>
        </form>
    }
}
