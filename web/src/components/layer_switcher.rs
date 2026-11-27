use leptos::{
    component, create_effect, create_rw_signal, create_signal, ev, spawn_local, view, wasm_bindgen,
    window_event_listener, CollectView, IntoView, RwSignal, Show, SignalGet, SignalSet, SignalUpdate,
    SignalWith, SignalWithUntracked, WriteSignal,
};
use serde::Serialize;
use serde_json::{Map, Value};
use turnout_core::orm_levels::{LEVELS_KEY, LEVEL_BANDS};
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

/// `orm_controls` setting values (see `app_settings::ORM_CONTROLS`).
const CONTROLS_TOOLBAR: &str = "toolbar";
const CONTROLS_PRESETS: &str = "presets";
const CONTROLS_BOTH: &str = "both";

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

/// The vertical levels lines and platforms draw at. A Turnout option (not on
/// openrailwaymap.app), so it sits apart from [`CATEGORIES`].
const LEVELS: Category = Category {
    key: LEVELS_KEY,
    label: "Levels",
    icon: "fa-solid fa-layer-group",
    pick: Pick::Many(&[(LEVEL_BANDS[0], "Underground"), (LEVEL_BANDS[1], "Ground"), (LEVEL_BANDS[2], "Elevated")]),
};
/// The style filters by level from zoom 8 only; lower zooms draw every level.
const LEVELS_HINT: &str = "Applies from zoom 8";

/// Shared value set for catenaries, signal boxes and substations.
const OWNABLE: &[Choice] = &[("plain", "Plain"), ("operator", "Operator"), ("owner", "Owner"), (NONE, "None")];

/// A literal option value set by a preset.
#[derive(Clone, Copy)]
enum Literal {
    Bool(bool),
    Str(&'static str),
    List(&'static [&'static str]),
}

impl Literal {
    fn to_value(self) -> Value {
        match self {
            Self::Bool(b) => Value::Bool(b),
            Self::Str(s) => Value::from(s),
            Self::List(items) => Value::from(items.to_vec()),
        }
    }
}

/// A preset: its label and the option values it sets.
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

/// The detail options with everything switched off. A classic preset starts from
/// this, so picking one replaces the previous view instead of adding to it.
const DETAILS_OFF: &[(&str, Literal)] = &[
    ("stations", Literal::Str(NONE)),
    ("platforms", Literal::Str(NONE)),
    ("switches", Literal::Str(NONE)),
    ("signals", Literal::List(&[])),
    ("catenaries", Literal::Str(NONE)),
    ("boxes", Literal::Str(NONE)),
    ("substations", Literal::Str(NONE)),
    ("pois", Literal::List(&[])),
];

/// The styles Turnout offered before the toolbar, as the track mode and details
/// each one drew (on top of [`DETAILS_OFF`]).
const CLASSIC_PRESETS: &[Preset] = &[
    (
        "Infrastructure",
        &[
            ("tracks", Literal::Str("usage")),
            ("stations", Literal::Str("station")),
            ("platforms", Literal::Str("plain")),
            ("switches", Literal::Str("plain")),
            ("pois", Literal::List(&["radio", "facility", "equipment", "level_crossing", "train_protection"])),
        ],
    ),
    ("Speed", &[("tracks", Literal::Str("speed")), ("signals", Literal::List(&["speed"]))]),
    (
        "Signals",
        &[
            ("tracks", Literal::Str("train_protection")),
            (
                "signals",
                Literal::List(&["main", "distant", "train_protection", "station", "radio", "shunting", "other"]),
            ),
            ("boxes", Literal::Str("plain")),
            ("pois", Literal::List(&["train_protection"])),
        ],
    ),
    (
        "Electrification",
        &[
            ("tracks", Literal::Str("voltage_frequency")),
            ("signals", Literal::List(&["electricity"])),
            ("catenaries", Literal::Str("plain")),
            ("substations", Literal::Str("plain")),
        ],
    ),
    ("Track", &[("tracks", Literal::Str("gauge"))]),
    (
        "Operator",
        &[
            ("tracks", Literal::Str("operator")),
            ("stations", Literal::Str("operator")),
            ("boxes", Literal::Str("operator")),
        ],
    ),
    ("Route", &[("tracks", Literal::Str("routes")), ("stations", Literal::Str("station"))]),
];

/// Every option value a classic preset sets: [`DETAILS_OFF`] with its overrides.
fn classic_values(overrides: &[(&'static str, Literal)]) -> Vec<(&'static str, Value)> {
    DETAILS_OFF
        .iter()
        .filter(|(key, _)| !overrides.iter().any(|(k, _)| k == key))
        .chain(overrides)
        .map(|(key, lit)| (*key, lit.to_value()))
        .collect()
}

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

fn classic_active(options: &Options, overrides: &[(&'static str, Literal)]) -> bool {
    classic_values(overrides).iter().all(|(key, value)| options.get(*key) == Some(value))
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

/// Tracks the rail overlay controls setting, live across settings saves.
fn follow_controls_setting(set_controls: WriteSignal<String>) {
    spawn_local(async move {
        set_controls.set(crate::components::app_settings::load_settings().await.orm_controls);
    });
    crate::tauri::listen_to_events(&["settings-changed"], move |payload| {
        if let Some(controls) = js_sys::Reflect::get(&payload, &"orm_controls".into())
            .ok()
            .and_then(|v| v.as_string())
        {
            set_controls.set(controls);
        }
    });
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
/// headings of its own. The button's tooltip is its label, with `hint` below it.
fn tool(
    id: &'static str,
    label: &'static str,
    hint: Option<&'static str>,
    icon: &'static str,
    off: impl Fn() -> bool + 'static,
    open: RwSignal<Option<&'static str>>,
    header: Option<&'static str>,
    popover: impl IntoView + 'static,
) -> impl IntoView {
    let is_open = move || open.get() == Some(id);
    let title = hint.map_or_else(|| label.to_owned(), |hint| format!("{label}\n{hint}"));
    view! {
        <li class="orm-tool">
            <button
                title=title
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

/// A category's toolbar button and its choices.
fn category_tool(
    category: &'static Category,
    hint: Option<&'static str>,
    options: RwSignal<Options>,
    open: RwSignal<Option<&'static str>>,
) -> impl IntoView {
    let key = category.key;
    tool(
        key,
        category.label,
        hint,
        category.icon,
        move || options.with(|o| is_off(o, key)),
        open,
        Some(category.label),
        choice_list(category, options, open),
    )
}

/// The full toolbar: one button per option category, the levels and the display
/// settings.
fn toolbar(options: RwSignal<Options>, open: RwSignal<Option<&'static str>>) -> impl IntoView {
    view! {
        <ul class="orm-toolbar">
            {CATEGORIES.iter().map(|category| category_tool(category, None, options, open)).collect_view()}
            {category_tool(&LEVELS, Some(LEVELS_HINT), options, open)}
            {tool(DISPLAY, DISPLAY_LABEL, None, DISPLAY_ICON, || false, open, None, display_sections(options))}
        </ul>
    }
}

/// The classic row of style presets.
fn preset_bar(options: RwSignal<Options>) -> impl IntoView {
    view! {
        <ul class="orm-presets">
            {CLASSIC_PRESETS.iter().map(|&(name, overrides)| {
                let active = move || options.with(|o| classic_active(o, overrides));
                view! {
                    <li>
                        <button
                            aria-pressed=move || active().to_string()
                            class:active=active
                            on:click=move |_| apply(options, classic_values(overrides))
                        >
                            {name}
                        </button>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
}

/// The rail overlay controls: the openrailwaymap.app-style toolbar, the classic
/// style presets, or both, per the `orm_controls` setting.
#[component]
pub fn LayerSwitcher(set_orm_tracks: WriteSignal<String>) -> impl IntoView {
    let options = create_rw_signal(Options::new());
    let open = create_rw_signal::<Option<&'static str>>(None);
    let (controls, set_controls) = create_signal(String::from(CONTROLS_TOOLBAR));
    load_options(options);
    follow_controls_setting(set_controls);
    let show_presets = move || controls.with(|c| c == CONTROLS_PRESETS || c == CONTROLS_BOTH);
    let show_toolbar = move || controls.with(|c| c != CONTROLS_PRESETS);

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
            <Show when=show_presets>{preset_bar(options)}</Show>
            <Show when=show_toolbar>{toolbar(options, open)}</Show>
            <Show when=move || disabled_reason.get().is_some()>
                <p id="orm-unavailable" title=move || disabled_reason.get().unwrap_or_default()>
                    {move || disabled_reason.get().unwrap_or_default()}
                </p>
            </Show>
        </nav>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_presets_set_every_detail_once() {
        for &(name, overrides) in CLASSIC_PRESETS {
            let values = classic_values(overrides);
            let mut keys: Vec<&str> = values.iter().map(|(key, _)| *key).collect();
            keys.sort_unstable();
            let mut expected: Vec<&str> = DETAILS_OFF.iter().map(|(key, _)| *key).chain(["tracks"]).collect();
            expected.sort_unstable();
            assert_eq!(keys, expected, "{name}");
        }
    }

    #[test]
    fn classic_preset_overrides_win_over_details_off() {
        let (_, speed) = CLASSIC_PRESETS.iter().find(|(name, _)| *name == "Speed").expect("Speed preset");
        let values = classic_values(speed);
        let signals = values.iter().find(|(key, _)| *key == "signals").map(|(_, v)| v.clone());
        assert_eq!(signals, Some(serde_json::json!(["speed"])));
        let options: Options = values.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        assert!(classic_active(&options, speed));
    }
}
