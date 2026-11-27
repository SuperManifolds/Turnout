use leptos::{
    component, create_effect, create_rw_signal, create_signal, ev, spawn_local, view, wasm_bindgen,
    window_event_listener, CollectView, IntoView, RwSignal, Show, SignalGet, SignalSet, SignalUpdate,
    SignalWith, SignalWithUntracked, View, WriteSignal,
};
use serde::Serialize;
use serde_json::{Map, Value};
use turnout_core::orm_levels::{
    band_levels, band_selection, expand_bands, toggle_band, BandSelection, LEVELS_KEY, LEVELS_MIN_ZOOM,
    LEVEL_BANDS,
};
use turnout_core::orm_presets::{classic_values, Literal, Preset, CLASSIC_PRESETS, NONE};
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

/// The icons of a collapsed and an expanded band row in the Levels popover.
const BAND_COLLAPSED_ICON: &str = "fa-solid fa-chevron-right";
const BAND_EXPANDED_ICON: &str = "fa-solid fa-chevron-down";

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
    /// Any subset of the levels (the Levels string-array option), by band: each
    /// `(band, label)` row switches every level of the band and expands to one
    /// row per level, nearest the ground first ("Underground 1"). A band of one
    /// level is a single row.
    Bands(&'static [Choice]),
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
    pick: Pick::Bands(&[(LEVEL_BANDS[0], "Underground"), (LEVEL_BANDS[1], "Ground"), (LEVEL_BANDS[2], "Elevated")]),
};
/// The Levels tooltip hint: the style filters by level from [`LEVELS_MIN_ZOOM`]
/// only, and lower zooms draw every level.
fn levels_zoom_hint() -> String {
    format!("Applies from zoom {LEVELS_MIN_ZOOM}")
}

/// Shared value set for catenaries, signal boxes and substations.
const OWNABLE: &[Choice] = &[("plain", "Plain"), ("operator", "Operator"), ("owner", "Owner"), (NONE, "None")];

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

/// The strings of `key`'s string-array option.
fn selected_values<'a>(options: &'a Options, key: &str) -> Vec<&'a str> {
    options
        .get(key)
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |items| items.iter().filter_map(Value::as_str).collect())
}

/// `saved` with the Levels option's band names (saved by older versions)
/// expanded to the levels they select, so each level row shows its state.
fn with_expanded_levels(mut saved: Options) -> Options {
    if saved.get(LEVELS_KEY).is_some_and(Value::is_array) {
        let levels = expand_bands(selected_values(&saved, LEVELS_KEY));
        saved.insert(LEVELS_KEY.to_owned(), Value::from(levels));
    }
    saved
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
            Ok(saved) => options.set(with_expanded_levels(saved)),
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
    name: impl IntoView + 'static,
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

/// Adds `value` to or removes it from `key`'s string-array option.
fn toggle_choice(options: RwSignal<Options>, key: &'static str, value: &str) {
    let next = options.with_untracked(|o| toggled(o, key, value));
    apply(options, vec![(key, next)]);
}

fn choice_list(
    category: &'static Category,
    options: RwSignal<Options>,
    open: RwSignal<Option<&'static str>>,
) -> View {
    let key = category.key;
    let (choices, many) = match category.pick {
        Pick::One(choices) => (choices, false),
        Pick::Many(choices) => (choices, true),
        Pick::Bands(bands) => return band_list(key, bands, options).into_view(),
    };
    let role = if many { "menuitemcheckbox" } else { "menuitemradio" };
    view! {
        <ul>
            {choices.iter().map(|&(value, name)| {
                let selected = move || options.with(|o| is_selected(o, key, value));
                choice_item(name, role, selected, move || {
                    if many {
                        toggle_choice(options, key, value);
                    } else {
                        apply(options, vec![(key, Value::from(value))]);
                        open.set(None);
                    }
                })
            }).collect_view()}
        </ul>
    }
    .into_view()
}

/// The `aria-checked` value of a band row.
fn band_checked(selection: BandSelection) -> &'static str {
    match selection {
        BandSelection::None => "false",
        BandSelection::Partial => "mixed",
        BandSelection::All => "true",
    }
}

/// The rows of a [`Pick::Bands`] option: one per band, each expanding to its levels.
fn band_list(key: &'static str, bands: &'static [Choice], options: RwSignal<Options>) -> impl IntoView {
    view! {
        <ul>
            {bands.iter().map(|&(band, label)| band_item(key, band, label, options)).collect_view()}
        </ul>
    }
}

/// A band row that switches all of `band`'s levels, shown as partly checked when
/// only some are on, with a button that expands it to a row per level.
fn band_item(
    key: &'static str,
    band: &'static str,
    label: &'static str,
    options: RwSignal<Options>,
) -> impl IntoView {
    let levels = band_levels(band);
    let selection = move || options.with(|o| band_selection(&selected_values(o, key), band));
    let toggle = move |_| {
        let next = options.with_untracked(|o| toggle_band(&selected_values(o, key), band));
        apply(options, vec![(key, Value::from(next))]);
    };
    let expanded = create_rw_signal(false);
    let expander = (levels.len() > 1).then(|| {
        view! {
            <button
                class="orm-expand"
                aria-label=format!("{label} levels")
                aria-expanded=move || expanded.get().to_string()
                on:click=move |_| expanded.update(|e| *e = !*e)
            >
                <i class=move || if expanded.get() { BAND_EXPANDED_ICON } else { BAND_COLLAPSED_ICON }></i>
            </button>
        }
    });
    let level_rows = (levels.len() > 1).then(|| {
        view! {
            <ul hidden=move || !expanded.get()>
                {levels.iter().zip(1..).map(|(&level, distance)| {
                    let selected = move || options.with(|o| is_selected(o, key, level));
                    choice_item(format!("{label} {distance}"), "menuitemcheckbox", selected, move || {
                        toggle_choice(options, key, level);
                    })
                }).collect_view()}
            </ul>
        }
    });
    view! {
        <li class="orm-band">
            {expander}
            <button
                role="menuitemcheckbox"
                aria-checked=move || band_checked(selection())
                class:selected=move || selection() == BandSelection::All
                on:click=toggle
            >
                {label}
            </button>
            {level_rows}
        </li>
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
    hint: Option<String>,
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
    hint: Option<String>,
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
            {category_tool(&LEVELS, Some(levels_zoom_hint()), options, open)}
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
