use leptos::{
    component, create_signal, event_target_checked, spawn_local, view, wasm_bindgen, CollectView,
    IntoView, ReadSignal, Show, SignalGet, SignalSet, WriteSignal,
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    fn map_set_orm_style(style_name: &str);
    fn map_refresh_orm();
}

/// Poll `orm_disabled_reason` a few times after mount. The preflight reason is set
/// before the frontend loads, but a runtime warm-up failure is recorded lazily by a
/// render worker and can land just after the first query.
const ORM_REASON_POLLS: u32 = 4;
const ORM_REASON_POLL_MS: i32 = 1500;

const ORM_STYLES: &[(&str, &str)] = &[
    ("standard", "Infrastructure"),
    ("speed", "Speed"),
    ("signals", "Signals"),
    ("electrification", "Electrification"),
    ("track", "Track"),
    ("loading_gauge", "Loading gauge"),
    ("axle_load", "Axle load"),
    ("operator", "Operator"),
    ("route", "Route"),
];

/// Builds an option's on/off value.
type ValueFn = fn(bool) -> JsValue;
/// A display toggle: state key, label, value builder, and its reactive checked state.
type Toggle = (
    &'static str,
    &'static str,
    ValueFn,
    ReadSignal<bool>,
    WriteSignal<bool>,
);
/// A toggle before its signals are created: key, label, default, value builder.
type ToggleSpec = (&'static str, &'static str, bool, ValueFn);

/// The POI categories the new ORM UI shows by default; the toggle picks all or none.
const POI_CATEGORIES: &[&str] =
    &["radio", "facility", "equipment", "level_crossing", "train_protection"];

/// Builds the option value for a "plain"/"none" visibility toggle.
fn mode_value(on: bool) -> JsValue {
    JsValue::from_str(if on { "plain" } else { "none" })
}

/// Builds the `pois` option value: the full category list when on, empty when off.
fn pois_value(on: bool) -> JsValue {
    let array = js_sys::Array::new();
    if on {
        for category in POI_CATEGORIES {
            array.push(&JsValue::from_str(category));
        }
    }
    array.into()
}

/// Pushes one ORM option to the backend and reloads the overlay to show the result.
fn apply_option(key: &'static str, value: JsValue) {
    spawn_local(async move {
        let _ = crate::tauri::set_orm_option(key, value).await;
        map_refresh_orm();
    });
}

#[component]
pub fn LayerSwitcher(
    orm_style: ReadSignal<String>,
    set_orm_style: WriteSignal<String>,
    orm_visible: ReadSignal<bool>,
) -> impl IntoView {
    let on_change = move |style: &str| {
        set_orm_style.set(style.to_string());
        // Only push to the map when the overlay is shown; otherwise the choice
        // takes effect when it is toggled back on from the layers list.
        if orm_visible.get() {
            map_set_orm_style(style);
        }
    };

    // When ORM rendering couldn't start (e.g. no working Vulkan runtime), tell the
    // user why the rail overlay does nothing rather than leaving it silently blank.
    let (disabled_reason, set_disabled_reason) = create_signal::<Option<String>>(None);
    spawn_local(async move {
        for attempt in 0..ORM_REASON_POLLS {
            if let Some(reason) = crate::tauri::orm_disabled_reason().await {
                set_disabled_reason.set(Some(reason));
                return;
            }
            if attempt + 1 < ORM_REASON_POLLS
                && let Some(window) = web_sys::window()
            {
                let _ = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::new(&mut |resolve, _| {
                    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                        &resolve,
                        ORM_REASON_POLL_MS,
                    );
                }))
                .await;
            }
        }
    });

    // Per-option display toggles, matching the new OpenRailwayMap UI. Each carries its
    // state key, a default (the upstream default), and a builder for its on/off value.
    let specs: Vec<ToggleSpec> = vec![
        ("platforms", "Platforms", true, mode_value as ValueFn),
        ("switches", "Switches", true, mode_value),
        ("boxes", "Signal boxes", false, mode_value),
        ("catenaries", "Catenary", false, mode_value),
        ("substations", "Substations", false, mode_value),
        ("pois", "Points of interest", true, pois_value),
    ];
    let toggles: Vec<Toggle> = specs
        .into_iter()
        .map(|(key, label, default_on, value)| {
            let (checked, set_checked) = create_signal(default_on);
            (key, label, value, checked, set_checked)
        })
        .collect();

    // Sync the toggle states with any persisted overrides the backend applied.
    let setters: Vec<(&'static str, bool, WriteSignal<bool>)> =
        toggles.iter().map(|&(key, _, _, _, set)| (key, key == "pois", set)).collect();
    spawn_local(async move {
        let Ok(options) = crate::tauri::get_orm_options().await else {
            return;
        };
        for (key, is_list, set_checked) in setters {
            let Ok(value) = js_sys::Reflect::get(&options, &JsValue::from_str(key)) else {
                continue;
            };
            let on = if is_list {
                js_sys::Array::from(&value).length() > 0
            } else {
                value.as_string().as_deref() != Some("none")
            };
            set_checked.set(on);
        }
    });

    view! {
        <Show when=move || disabled_reason.get().is_some()>
            <p id="orm-unavailable" title=move || disabled_reason.get().unwrap_or_default()>
                {move || disabled_reason.get().unwrap_or_default()}
            </p>
        </Show>
        <nav id="layer-switcher">
            {ORM_STYLES.iter().map(|&(id, label)| {
                let id_active = id.to_string();
                let id_click = id.to_string();
                view! {
                    <button
                        class:active=move || orm_style.get() == id_active
                        on:click=move |_| on_change(&id_click)
                    >
                        {label}
                    </button>
                }
            }).collect_view()}
        </nav>
        <section id="orm-options">
            {toggles.into_iter().map(|(key, label, value, checked, set_checked)| {
                view! {
                    <label>
                        <input
                            type="checkbox"
                            prop:checked=move || checked.get()
                            on:change=move |ev| {
                                let on = event_target_checked(&ev);
                                set_checked.set(on);
                                apply_option(key, value(on));
                            }
                        />
                        {label}
                    </label>
                }
            }).collect_view()}
        </section>
    }
}
