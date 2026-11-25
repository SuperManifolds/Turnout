use leptos::{
    component, create_signal, spawn_local, view, wasm_bindgen, CollectView, IntoView, ReadSignal,
    Show, SignalGet, SignalSet, WriteSignal,
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    fn map_set_orm_style(style_name: &str);
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
    ("operator", "Operator"),
    ("route", "Route"),
];

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
    }
}
