//! First-launch guided tour. Walks the user through the core Turnout features by
//! opening each real panel and spotlighting it with an explanatory callout, and
//! ends on a mandatory acknowledgement that Turnout issues go to the Turnout
//! developer rather than the NIMBY Rails developer.

use std::time::Duration;

use leptos::{
    component, view, CollectView, IntoView, ReadSignal, WriteSignal, Show, For,
    create_signal, create_effect, event_target_checked, on_cleanup, set_timeout,
    spawn_local, web_sys, SignalGet, SignalGetUntracked, SignalSet,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Where to report Turnout problems, surfaced on the final step.
const REPORT_ISSUES_URL: &str = "https://github.com/SuperManifolds/Turnout/issues";
/// Discord DM link for the developer (@supermanifolds, "Alex" in the NIMBY Rails
/// Discord). Discord deep-links by numeric user ID, not username.
const REPORT_DISCORD_URL: &str = "https://discord.com/users/385879184276193290";

/// Breathing room around a spotlighted element, and the gap between the element
/// and its callout, in CSS pixels.
const SPOTLIGHT_PAD: f64 = 6.0;
const CALLOUT_GAP: f64 = 14.0;

/// Delays after opening a panel before measuring its element, covering the
/// panel's mount plus any slide/opacity transition. Two passes catch both fast
/// and animated reveals.
const MEASURE_DELAY_EARLY: Duration = Duration::from_millis(120);
const MEASURE_DELAY_LATE: Duration = Duration::from_millis(360);

/// Which live panel a step needs open. The tour keeps exactly one open at a time
/// so the spotlight always frames a clean target.
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    None,
    Blueprints,
    Overlays,
    RailLayers,
}

/// Where the callout sits relative to the spotlighted element.
#[derive(Clone, Copy)]
enum Placement {
    Center,
    Above,
    Left,
    Right,
}

struct Step {
    /// CSS selector to spotlight, or `None` for a centered card.
    target: Option<&'static str>,
    panel: Panel,
    placement: Placement,
    icon: &'static str,
    title: &'static str,
    body: &'static [&'static str],
}

const STEPS: &[Step] = &[
    Step {
        target: None,
        panel: Panel::None,
        placement: Placement::Center,
        icon: "fa-solid fa-train-tram",
        title: "Welcome to Turnout",
        body: &[
            "Turnout imports real-world railways from OpenRailwayMap into NIMBY Rails as ready-to-use blueprints, and overlays live rail data on the map while you plan.",
            "This short tour points out the main features. You can replay it any time from Settings.",
        ],
    },
    Step {
        target: Some("#map-controls"),
        panel: Panel::None,
        placement: Placement::Above,
        icon: "fa-solid fa-vector-square",
        title: "Import real railways",
        body: &[
            "Pan and zoom to a place, click Select Area, then drag a box on the map. Turnout pulls the railways inside it from OpenRailwayMap.",
            "Once an area is selected, more tools appear here: Import Tracks turns it into a blueprint, plus Download Tiles and Rail Layers.",
        ],
    },
    Step {
        target: Some("#layer-switcher"),
        panel: Panel::None,
        placement: Placement::Left,
        icon: "fa-solid fa-layer-group",
        title: "Live rail overlay",
        body: &[
            "These buttons switch the OpenRailwayMap overlay style on the map — Infrastructure, Speed, Signals, Electrification and more.",
            "It's a reference layer to plan against. It isn't imported into the game; it just helps you see what's really there.",
        ],
    },
    Step {
        target: Some("#overlay-drawer button[title='Copy TileJSON URL']"),
        panel: Panel::Overlays,
        placement: Placement::Right,
        icon: "fa-solid fa-map",
        title: "Custom overlays",
        body: &[
            "This drawer manages map overlays. At the top is the live OpenRailwayMap overlay — the highlighted button copies its tile URL.",
            "To see the rail overlay inside NIMBY Rails, copy that URL and add it as a Map Source in the game (Map sources → Add → paste the URL).",
            "You can also add your own overlays here — WMS, WMTS, XYZ, ArcGIS, Apple Maps, or local MBTiles and KML — and toggle each layer's visibility and opacity.",
        ],
    },
    Step {
        target: Some("#blueprint-drawer"),
        panel: Panel::Blueprints,
        placement: Placement::Left,
        icon: "fa-solid fa-clipboard-list",
        title: "Your blueprints",
        body: &[
            "Every import is saved here as a NIMBY Rails blueprint, dropped straight into your mods folder.",
            "Click a blueprint to fly to it, or open its folder, rename, or delete it. In-game, load it from the blueprint browser.",
        ],
    },
    Step {
        target: Some(".rail-layers-panel"),
        panel: Panel::RailLayers,
        placement: Placement::Left,
        icon: "fa-solid fa-train-subway",
        title: "Rail layers in-game",
        body: &[
            "Rail Layers streams OpenRailwayMap into NIMBY Rails itself, split into underground, ground, and elevated levels you can toggle in the game.",
            "It needs the free \"ORM Vertical Layers\" Steam Workshop mod for the styling — this panel links to it. Select an area first, then Generate.",
        ],
    },
    Step {
        target: None,
        panel: Panel::None,
        placement: Placement::Center,
        icon: "fa-brands fa-steam",
        title: "Auto-launch with NIMBY Rails",
        body: &[
            "The live overlays are served from a local server that only runs while Turnout is open, so Turnout has to be running before NIMBY loads them. Closing Turnout's window now keeps it in the tray / menu bar, so the server stays up.",
            "To make Steam start Turnout automatically whenever you launch NIMBY Rails, add this to the game's Launch Options — Steam \u{2192} NIMBY Rails \u{2192} Properties \u{2192} Launch Options:",
        ],
    },
    Step {
        target: None,
        panel: Panel::None,
        placement: Placement::Center,
        icon: "fa-solid fa-circle-exclamation",
        title: "Reporting problems",
        body: &[
            "Turnout is not made by the NIMBY Rails developer. If something is wrong with a blueprint's tracks, the rail overlay, or the Rail Layers mod, it's a Turnout issue.",
            "Please report those to me, the Turnout developer — the game's developer can't help with them.",
            "Open an issue on GitHub, or message me on Discord: @supermanifolds (I'm \"Alex\" in the NIMBY Rails Discord).",
        ],
    },
];

/// The tour reads the DOM directly to place the spotlight, so its geometry is a
/// plain viewport rectangle rather than a reactive value.
#[derive(Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn measure(selector: &str) -> Option<Rect> {
    let el = web_sys::window()?.document()?.query_selector(selector).ok()??;
    let r = el.get_bounding_client_rect();
    if r.width() <= 0.0 && r.height() <= 0.0 {
        return None;
    }
    Some(Rect { x: r.x(), y: r.y(), w: r.width(), h: r.height() })
}

fn copy_to_clipboard(text: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().clipboard().write_text(text);
    }
}

fn spotlight_style(rect: Option<Rect>) -> String {
    match rect {
        Some(r) => format!(
            "display:block;top:{}px;left:{}px;width:{}px;height:{}px",
            r.y - SPOTLIGHT_PAD,
            r.x - SPOTLIGHT_PAD,
            r.w + SPOTLIGHT_PAD * 2.0,
            r.h + SPOTLIGHT_PAD * 2.0,
        ),
        None => "display:none".to_string(),
    }
}

fn callout_style(rect: Option<Rect>, placement: Placement) -> String {
    let Some(r) = rect else { return String::new() };
    match placement {
        Placement::Center => String::new(),
        // Anchor the callout's bottom edge just above the target; the CSS
        // translate lifts it by its own height so a bottom toolbar's callout
        // stays on-screen.
        Placement::Above => format!(
            "top:{}px;left:{}px;transform:translateY(-100%)",
            r.y - CALLOUT_GAP,
            r.x.max(CALLOUT_GAP),
        ),
        // Anchor the callout's right edge just left of the target; the CSS
        // translate keeps it fully on-screen for right-hand panels.
        Placement::Left => format!(
            "top:{}px;left:{}px;transform:translateX(-100%)",
            r.y.max(CALLOUT_GAP),
            r.x - CALLOUT_GAP,
        ),
        // Just right of the target, for a left-docked panel.
        Placement::Right => format!(
            "top:{}px;left:{}px",
            r.y.max(CALLOUT_GAP),
            r.x + r.w + CALLOUT_GAP,
        ),
    }
}

#[component]
pub fn Tutorial(
    set_active: WriteSignal<bool>,
    replay: ReadSignal<bool>,
    set_drawer_open: WriteSignal<bool>,
    set_overlay_open: WriteSignal<bool>,
    set_show_vector_layers: WriteSignal<bool>,
) -> impl IntoView {
    let (step, set_step) = create_signal(0usize);
    let (rect, set_rect) = create_signal::<Option<Rect>>(None);
    let (accepted, set_accepted) = create_signal(false);
    let (launch_setup, set_launch_setup) =
        create_signal::<Option<crate::tauri::NimbyLaunchSetup>>(None);

    // Fetch the Steam launch-options string once; the auto-launch step shows it.
    spawn_local(async move {
        set_launch_setup.set(crate::tauri::nimby_launch_setup().await);
    });

    let last = STEPS.len() - 1;
    // The auto-launch step sits just before the final "Reporting problems" step.
    let autolaunch = last - 1;

    // Keep exactly the panel the current step needs open.
    let apply_panel = move |panel: Panel| {
        set_drawer_open.set(panel == Panel::Blueprints);
        set_overlay_open.set(panel == Panel::Overlays);
        set_show_vector_layers.set(panel == Panel::RailLayers);
    };

    let remeasure = move || {
        if let Some(sel) = STEPS[step.get_untracked()].target
            && let Some(r) = measure(sel)
        {
            set_rect.set(Some(r));
        }
    };

    // On each step: open its panel, clear the stale spotlight, then measure the
    // new target once it has mounted and settled.
    create_effect(move |_| {
        let cur = &STEPS[step.get()];
        apply_panel(cur.panel);
        set_rect.set(None);
        if cur.target.is_some() {
            set_timeout(remeasure, MEASURE_DELAY_EARLY);
            set_timeout(remeasure, MEASURE_DELAY_LATE);
        }
    });

    // Reposition the spotlight when the window resizes; drop the listener when
    // the tour closes so a replay doesn't stack stale callbacks.
    if let Some(window) = web_sys::window() {
        let closure = Closure::<dyn Fn()>::new(remeasure);
        let _ = window.add_event_listener_with_callback("resize", closure.as_ref().unchecked_ref());
        on_cleanup(move || {
            if let Some(w) = web_sys::window() {
                let _ = w.remove_event_listener_with_callback("resize", closure.as_ref().unchecked_ref());
            }
        });
    }

    let finish = move || {
        apply_panel(Panel::None);
        set_active.set(false);
        spawn_local(async {
            crate::components::app_settings::mark_tutorial_completed().await;
        });
    };

    let next = move |_| {
        let s = step.get_untracked();
        if s < last {
            set_step.set(s + 1);
        }
    };
    let back = move |_| {
        let s = step.get_untracked();
        if s > 0 {
            set_step.set(s - 1);
        }
    };

    let open_url = move |url: &'static str| {
        spawn_local(async move {
            let _ = crate::tauri::open_external_url(url).await;
        });
    };

    view! {
        <div class="tour-blocker" class:dim=move || rect.get().is_none()></div>
        <div class="tour-spotlight" style=move || spotlight_style(rect.get())></div>
        <aside
            class="tour-callout"
            class:center=move || rect.get().is_none()
            style=move || callout_style(rect.get(), STEPS[step.get()].placement)
        >
            <Show when=move || replay.get()>
                <button class="tour-close" title="Close" on:click=move |_| finish()>
                    <i class="fa-solid fa-xmark"></i>
                </button>
            </Show>
            <header>
                <i class=move || STEPS[step.get()].icon></i>
                <h2>{move || STEPS[step.get()].title}</h2>
            </header>
            {move || STEPS[step.get()].body.iter().map(|p| view! { <p>{*p}</p> }).collect_view()}

            <Show when=move || step.get() == autolaunch>
                {move || match launch_setup.get() {
                    Some(setup) => match setup.launch_options {
                        Some(opts) => {
                            let to_copy = opts.clone();
                            let detected = setup.nimby_detected;
                            view! {
                                <div class="tour-autolaunch">
                                    <pre class="tour-code">{opts}</pre>
                                    <div class="tour-report">
                                        <button on:click=move |_| copy_to_clipboard(&to_copy)>
                                            <i class="fa-regular fa-copy"></i>
                                            " Copy launch options"
                                        </button>
                                    </div>
                                    <p class="tour-hint">{
                                        if detected {
                                            "NIMBY Rails found in your Steam library."
                                        } else {
                                            "Couldn't find NIMBY Rails in your Steam libraries — it still works once the game is installed."
                                        }
                                    }</p>
                                </div>
                            }.into_any()
                        }
                        None => view! {
                            <p class="tour-hint">"NIMBY Rails runs on Windows and Linux, so this Steam setup applies there. On macOS, just keep Turnout running — closing its window leaves it in the menu bar."</p>
                        }.into_any(),
                    },
                    None => view! { <p class="tour-hint">"Preparing\u{2026}"</p> }.into_any(),
                }}
            </Show>

            <Show when=move || step.get() == last>
                <div class="tour-report">
                    <button on:click=move |_| open_url(REPORT_ISSUES_URL)>
                        <i class="fa-brands fa-github"></i>
                        " Open GitHub Issues"
                    </button>
                    <button on:click=move |_| open_url(REPORT_DISCORD_URL)>
                        <i class="fa-brands fa-discord"></i>
                        " Message me on Discord"
                    </button>
                </div>
                <label class="tour-accept">
                    <input
                        type="checkbox"
                        prop:checked=accepted
                        on:change=move |ev| set_accepted.set(event_target_checked(&ev))
                    />
                    <span>"I understand that issues with Turnout's blueprinted tracks and layers go to the Turnout developer, not the NIMBY Rails developer."</span>
                </label>
            </Show>

            <footer>
                <span class="tour-progress">
                    <For
                        each=move || 0..STEPS.len()
                        key=|i| *i
                        children=move |i| view! {
                            <span class="tour-dot" class:active=move || step.get() == i></span>
                        }
                    />
                </span>
                <nav>
                    <button on:click=back disabled=move || step.get() == 0>"Back"</button>
                    <Show
                        when=move || step.get() == last
                        fallback=move || view! { <button class="primary" on:click=next>"Next"</button> }
                    >
                        <button
                            class="primary"
                            disabled=move || !accepted.get()
                            on:click=move |_| finish()
                        >"Finish"</button>
                    </Show>
                </nav>
            </footer>
        </aside>
    }
}
