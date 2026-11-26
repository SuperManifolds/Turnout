use leptos::{
    component, create_effect, create_rw_signal, create_signal, ev, spawn_local, view, wasm_bindgen,
    window_event_listener, CollectView, IntoView, RwSignal, Show, SignalGet, SignalSet, SignalUpdate,
    SignalWith, SignalWithUntracked, WriteSignal,
};
use serde::Serialize;
use serde_json::{Map, Value};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

#[wasm_bindgen]
extern "C" {
    fn map_refresh_orm();
    fn map_on_theme_change(callback: &Closure<dyn Fn(bool)>);
}

/// Poll `orm_disabled_reason` a few times after mount. The preflight reason is set
/// before the frontend loads, but a runtime warm-up failure is recorded lazily by a
/// render worker and can land just after the first query.
const ORM_REASON_POLLS: u32 = 4;
const ORM_REASON_POLL_MS: i32 = 1500;

/// Element id of the toolbar, used to tell clicks inside it from outside ones.
const TOOLBAR_ID: &str = "orm-options";

/// Popover id of the display settings (the categories use their option key).
const DISPLAY: &str = "display";
const DISPLAY_ICON: &str = "fa-solid fa-sliders";
const DISPLAY_LABEL: &str = "Display";

/// The option value that switches a category off.
const NONE: &str = "none";

/// The current ORM option values (the style's `global-state`), keyed by option.
type Options = Map<String, Value>;

/// `(value, label)` pair for a choice.
type Choice = (&'static str, &'static str);

/// How a category picks its value.
enum Pick {
    /// One value from the list (a string option).
    One(&'static [Choice]),
    /// Any subset of the list (a string-array option).
    Many(&'static [Choice]),
}

/// One toolbar button: an option key and the choices its popover offers.
struct Category {
    key: &'static str,
    label: &'static str,
    icon: &'static str,
    pick: Pick,
}

/// Mirrors the openrailwaymap.app toolbar, in its order.
const CATEGORIES: &[Category] = &[
    Category {
        key: "tracks",
        label: "Tracks",
        icon: "fa-solid fa-train",
        pick: Pick::One(&[
            ("usage", "Usage"),
            ("speed", "Speed"),
            ("train_protection", "Train protection"),
            ("voltage_frequency", "Voltage & frequency"),
            ("maximum_current", "Maximum current"),
            ("power", "Electrical power"),
            ("gauge", "Track gauge"),
            ("loading_gauge", "Loading gauge"),
            ("track_class", "Track class"),
            ("axle_load", "Axle load"),
            ("metre_load", "Metre load"),
            ("operator", "Operator"),
            ("owner", "Owner"),
            ("routes", "Routes"),
            ("passenger_lines", "Number of tracks"),
            ("radio", "Radio"),
            ("plain", "Plain"),
        ]),
    },
    Category {
        key: "stations",
        label: "Stations",
        icon: "fa-solid fa-location-dot",
        pick: Pick::One(&[("station", "Modality"), ("operator", "Operator"), ("owner", "Owner"), (NONE, "None")]),
    },
    Category {
        key: "platforms",
        label: "Platforms",
        icon: "fa-solid fa-grip-lines",
        pick: Pick::One(&[("plain", "Plain"), ("height", "Height"), (NONE, "None")]),
    },
    Category {
        key: "switches",
        label: "Switches",
        icon: "fa-solid fa-code-branch",
        pick: Pick::One(&[("plain", "Plain"), (NONE, "None")]),
    },
    Category {
        key: "signals",
        label: "Signals",
        icon: "fa-solid fa-traffic-light",
        pick: Pick::Many(&[
            ("main", "Main"),
            ("distant", "Distant"),
            ("speed", "Speed"),
            ("train_protection", "Train protection"),
            ("electricity", "Electricity"),
            ("station", "Station"),
            ("radio", "Radio"),
            ("shunting", "Shunting"),
            ("other", "Other"),
        ]),
    },
    Category { key: "catenaries", label: "Catenaries", icon: "fa-solid fa-bolt", pick: Pick::One(OWNABLE) },
    Category { key: "boxes", label: "Signal boxes", icon: "fa-solid fa-house", pick: Pick::One(OWNABLE) },
    Category { key: "substations", label: "Substations", icon: "fa-solid fa-plug", pick: Pick::One(OWNABLE) },
    Category {
        key: "pois",
        label: "Points of interest",
        icon: "fa-solid fa-star",
        pick: Pick::Many(&[
            ("radio", "Radio"),
            ("facility", "Facility"),
            ("equipment", "Equipment"),
            ("operator", "Operator"),
            ("vacancy_detection", "Vacancy detection"),
            ("electrical_equipment", "Electrical equipment"),
            ("level_crossing", "Level crossing"),
            ("train_protection", "Train protection"),
        ]),
    },
];

/// Shared value set for catenaries, signal boxes and substations.
const OWNABLE: &[Choice] = &[("plain", "Plain"), ("operator", "Operator"), ("owner", "Owner"), (NONE, "None")];

/// A literal option value in a display setting.
#[derive(Clone, Copy)]
enum Literal {
    Bool(bool),
    Str(&'static str),
}

impl Literal {
    fn to_value(self) -> Value {
        match self {
            Self::Bool(b) => Value::Bool(b),
            Self::Str(s) => Value::from(s),
        }
    }
}

/// A display choice: its label and the option values it sets.
type Preset = (&'static str, &'static [(&'static str, Literal)]);

/// The display popover's sections: settings that set one or more options at once.
const DISPLAY_SETTINGS: &[(&str, &[Preset])] = &[
    (
        "Future infrastructure",
        &[
            (
                "None",
                &[
                    ("showConstructionInfrastructure", Literal::Bool(false)),
                    ("showProposedInfrastructure", Literal::Bool(false)),
                ],
            ),
            (
                "Under construction",
                &[
                    ("showConstructionInfrastructure", Literal::Bool(true)),
                    ("showProposedInfrastructure", Literal::Bool(false)),
                ],
            ),
            (
                "Under construction and proposed",
                &[
                    ("showConstructionInfrastructure", Literal::Bool(true)),
                    ("showProposedInfrastructure", Literal::Bool(true)),
                ],
            ),
        ],
    ),
    (
        "Historical infrastructure",
        &[
            (
                "None",
                &[
                    ("showAbandonedInfrastructure", Literal::Bool(false)),
                    ("showRazedInfrastructure", Literal::Bool(false)),
                ],
            ),
            (
                "OpenStreetMap",
                &[
                    ("showAbandonedInfrastructure", Literal::Bool(true)),
                    ("showRazedInfrastructure", Literal::Bool(true)),
                ],
            ),
        ],
    ),
    (
        "Station labels",
        &[
            ("Reference", &[("stationLowZoomLabel", Literal::Str("label"))]),
            ("Name", &[("stationLowZoomLabel", Literal::Str("name"))]),
        ],
    ),
];

fn is_selected(options: &Options, key: &str, value: &str) -> bool {
    match options.get(key) {
        Some(Value::String(s)) => s == value,
        Some(Value::Array(items)) => items.iter().any(|v| v.as_str() == Some(value)),
        _ => false,
    }
}

/// Whether a category draws nothing (set to "none", or an empty selection).
fn is_off(options: &Options, key: &str) -> bool {
    match options.get(key) {
        Some(Value::String(s)) => s == NONE,
        Some(Value::Array(items)) => items.is_empty(),
        _ => false,
    }
}

fn preset_active(options: &Options, values: &[(&str, Literal)]) -> bool {
    values.iter().all(|(key, lit)| options.get(*key) == Some(&lit.to_value()))
}

/// `key`'s array with `value` added or removed.
fn toggled(options: &Options, key: &str, value: &str) -> Value {
    let mut items: Vec<Value> = options.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    if let Some(pos) = items.iter().position(|v| v.as_str() == Some(value)) {
        items.remove(pos);
    } else {
        items.push(Value::from(value));
    }
    Value::Array(items)
}

/// Sets option values that differ from the current ones, pushes them to the
/// renderer and reloads the overlay. Unchanged values cause no reload.
fn apply(options: RwSignal<Options>, changes: Vec<(&'static str, Value)>) {
    let changed: Vec<(&'static str, Value)> = options.with_untracked(|current| {
        changes.into_iter().filter(|(key, value)| current.get(*key) != Some(value)).collect()
    });
    if changed.is_empty() {
        return;
    }
    options.update(|current| {
        for (key, value) in &changed {
            current.insert((*key).to_owned(), value.clone());
        }
    });
    spawn_local(async move {
        for (key, value) in changed {
            let result = match value.serialize(&serde_wasm_bindgen::Serializer::json_compatible()) {
                Ok(js) => crate::tauri::set_orm_option(key, js).await,
                Err(e) => Err(e.to_string()),
            };
            if let Err(e) = result {
                web_sys::console::warn_1(&format!("could not set ORM option {key}: {e}").into());
            }
        }
        map_refresh_orm();
    });
}

/// Loads the saved option values, then keeps the overlay theme on the map's theme.
fn load_options(options: RwSignal<Options>) {
    spawn_local(async move {
        let saved = crate::tauri::get_orm_options()
            .await
            .and_then(|js| serde_wasm_bindgen::from_value::<Options>(js).map_err(|e| e.to_string()));
        match saved {
            Ok(saved) => options.set(saved),
            Err(e) => web_sys::console::warn_1(&format!("could not load ORM options: {e}").into()),
        }
        // Registered after the load so the saved theme cannot overwrite the map's.
        let follow_theme = Closure::<dyn Fn(bool)>::new(move |dark: bool| {
            apply(options, vec![("theme", Value::from(if dark { "dark" } else { "light" }))]);
        });
        map_on_theme_change(&follow_theme);
        follow_theme.forget();
    });
}

fn is_inside_toolbar(event: &web_sys::MouseEvent) -> bool {
    event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        .and_then(|element| element.closest(&format!("#{TOOLBAR_ID}")).ok().flatten())
        .is_some()
}

/// Records the runtime reason ORM rendering is unavailable, if any.
fn poll_disabled_reason(set_reason: WriteSignal<Option<String>>) {
    spawn_local(async move {
        for attempt in 0..ORM_REASON_POLLS {
            if let Some(reason) = crate::tauri::orm_disabled_reason().await {
                set_reason.set(Some(reason));
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
}

/// A popover list item that shows whether its choice is selected.
fn choice_item(
    name: &'static str,
    role: &'static str,
    selected: impl Fn() -> bool + Copy + 'static,
    on_pick: impl Fn() + 'static,
) -> impl IntoView {
    view! {
        <li>
            <button
                role=role
                aria-checked=move || selected().to_string()
                class:selected=selected
                on:click=move |_| on_pick()
            >
                {name}
            </button>
        </li>
    }
}

fn choice_list(
    category: &'static Category,
    options: RwSignal<Options>,
    open: RwSignal<Option<&'static str>>,
) -> impl IntoView {
    let key = category.key;
    let (choices, many) = match category.pick {
        Pick::One(choices) => (choices, false),
        Pick::Many(choices) => (choices, true),
    };
    let role = if many { "menuitemcheckbox" } else { "menuitemradio" };
    view! {
        <ul>
            {choices.iter().map(|&(value, name)| {
                let selected = move || options.with(|o| is_selected(o, key, value));
                choice_item(name, role, selected, move || {
                    if many {
                        let next = options.with_untracked(|o| toggled(o, key, value));
                        apply(options, vec![(key, next)]);
                    } else {
                        apply(options, vec![(key, Value::from(value))]);
                        open.set(None);
                    }
                })
            }).collect_view()}
        </ul>
    }
}

fn display_sections(options: RwSignal<Options>) -> impl IntoView {
    DISPLAY_SETTINGS
        .iter()
        .map(|&(title, presets)| {
            view! {
                <section>
                    <h3>{title}</h3>
                    <ul>
                        {presets.iter().map(|&(name, values)| {
                            let selected = move || options.with(|o| preset_active(o, values));
                            choice_item(name, "menuitemradio", selected, move || {
                                apply(options, values.iter().map(|(key, lit)| (*key, lit.to_value())).collect());
                            })
                        }).collect_view()}
                    </ul>
                </section>
            }
        })
        .collect_view()
}

/// One toolbar button plus its popover, titled by `header` when its content has no
/// headings of its own.
fn tool(
    id: &'static str,
    label: &'static str,
    icon: &'static str,
    off: impl Fn() -> bool + 'static,
    open: RwSignal<Option<&'static str>>,
    header: Option<&'static str>,
    popover: impl IntoView + 'static,
) -> impl IntoView {
    let is_open = move || open.get() == Some(id);
    view! {
        <li class="orm-tool">
            <button
                title=label
                aria-label=label
                aria-haspopup="menu"
                aria-expanded=move || is_open().to_string()
                class:off=off
                class:open=is_open
                on:click=move |_| open.update(|o| *o = if *o == Some(id) { None } else { Some(id) })
            >
                <i class=icon></i>
            </button>
            <section class="orm-popover" role="menu" aria-label=label hidden=move || !is_open()>
                {header.map(|text| view! { <header>{text}</header> })}
                {popover}
            </section>
        </li>
    }
}

/// The rail overlay toolbar: one button per `OpenRailwayMap` option category, each
/// opening a popover of its values, like the openrailwaymap.app toolbar.
#[component]
pub fn LayerSwitcher(set_orm_tracks: WriteSignal<String>) -> impl IntoView {
    let options = create_rw_signal(Options::new());
    let open = create_rw_signal::<Option<&'static str>>(None);
    load_options(options);

    // The overlay drawer labels the layer with the current tracks mode.
    create_effect(move |_| {
        if let Some(tracks) = options.with(|o| o.get("tracks").and_then(Value::as_str).map(str::to_owned)) {
            set_orm_tracks.set(tracks);
        }
    });

    // Any click outside the toolbar, or Escape, closes the open popover.
    let _ = window_event_listener(ev::click, move |e| {
        if !is_inside_toolbar(&e) {
            open.set(None);
        }
    });
    let _ = window_event_listener(ev::keydown, move |e| {
        if e.key() == "Escape" {
            open.set(None);
        }
    });

    // When ORM rendering couldn't start (e.g. no working Vulkan runtime), tell the
    // user why the rail overlay does nothing rather than leaving it silently blank.
    let (disabled_reason, set_disabled_reason) = create_signal::<Option<String>>(None);
    poll_disabled_reason(set_disabled_reason);

    view! {
        <nav id=TOOLBAR_ID aria-label="Rail overlay options">
            <ul class="orm-toolbar">
                {CATEGORIES.iter().map(|category| {
                    let key = category.key;
                    tool(
                        key,
                        category.label,
                        category.icon,
                        move || options.with(|o| is_off(o, key)),
                        open,
                        Some(category.label),
                        choice_list(category, options, open),
                    )
                }).collect_view()}
                {tool(DISPLAY, DISPLAY_LABEL, DISPLAY_ICON, || false, open, None, display_sections(options))}
            </ul>
        </nav>
        <Show when=move || disabled_reason.get().is_some()>
            <p id="orm-unavailable" title=move || disabled_reason.get().unwrap_or_default()>
                {move || disabled_reason.get().unwrap_or_default()}
            </p>
        </Show>
    }
}
