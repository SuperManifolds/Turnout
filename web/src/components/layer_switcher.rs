use leptos::{wasm_bindgen, component, view, IntoView, create_signal, SignalSet, CollectView, SignalGet};
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
pub fn LayerSwitcher() -> impl IntoView {
    let (active, set_active) = create_signal("standard".to_string());

    let on_change = move |style: &str| {
        let s = style.to_string();
        set_active.set(s.clone());
        map_set_orm_style(&s);
    };

    view! {
        <nav id="layer-switcher">
            {ORM_STYLES.iter().map(|&(id, label)| {
                let id_active = id.to_string();
                let id_click = id.to_string();
                view! {
                    <button
                        class:active=move || active.get() == id_active
                        on:click=move |_| on_change(&id_click)
                    >
                        {label}
                    </button>
                }
            }).collect_view()}
        </nav>
    }
}
