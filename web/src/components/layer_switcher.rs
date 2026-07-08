use leptos::{wasm_bindgen, component, view, IntoView, ReadSignal, WriteSignal, SignalSet, CollectView, SignalGet};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    fn map_set_orm_style(style_name: &str);
}

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

    view! {
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
