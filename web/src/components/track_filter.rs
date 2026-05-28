use leptos::{component, view, ReadSignal, Callback, IntoView, create_signal, SignalUpdate, Callable, SignalGetUntracked, SignalSet, CollectView, SignalGet};

struct TrackType {
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    default_on: bool,
}

const TRACK_TYPES: &[TrackType] = &[
    TrackType { id: "rail",           icon: "fa-solid fa-train",           label: "Rail",           default_on: true },
    TrackType { id: "light_rail",     icon: "fa-solid fa-train-tram",      label: "Light Rail",     default_on: true },
    TrackType { id: "tram",           icon: "fa-solid fa-train-tram",      label: "Tram",           default_on: true },
    TrackType { id: "subway",         icon: "fa-solid fa-train-subway",    label: "Subway",         default_on: true },
    TrackType { id: "narrow_gauge",   icon: "fa-solid fa-train",           label: "Narrow Gauge",   default_on: true },
    TrackType { id: "monorail",       icon: "fa-solid fa-cable-car",       label: "Monorail",       default_on: true },
    TrackType { id: "funicular",      icon: "fa-solid fa-cable-car",       label: "Funicular",      default_on: true },
    TrackType { id: "preserved",      icon: "fa-solid fa-landmark",        label: "Preserved",      default_on: true },
    TrackType { id: "construction",   icon: "fa-solid fa-helmet-safety",   label: "Construction",   default_on: false },
    TrackType { id: "proposed",       icon: "fa-solid fa-compass-drafting",label: "Proposed",       default_on: false },
    TrackType { id: "miniature",      icon: "fa-solid fa-minimize",        label: "Miniature",      default_on: false },
    TrackType { id: "disused",        icon: "fa-solid fa-pause",           label: "Disused",        default_on: false },
    TrackType { id: "abandoned",      icon: "fa-solid fa-ban",             label: "Abandoned",      default_on: false },
    TrackType { id: "razed",          icon: "fa-solid fa-xmark",           label: "Razed",          default_on: false },
];

pub fn default_enabled_types() -> Vec<String> {
    TRACK_TYPES.iter()
        .filter(|t| t.default_on)
        .map(|t| t.id.to_string())
        .collect()
}

#[component]
pub fn TrackFilter(
    available: ReadSignal<Vec<String>>,
    on_change: Callback<Vec<String>>,
) -> impl IntoView {
    let (enabled, set_enabled) = create_signal::<Vec<String>>(default_enabled_types());

    let toggle = move |type_id: &str| {
        let tid = type_id.to_string();
        set_enabled.update(|v| {
            if v.contains(&tid) {
                v.retain(|t| t != &tid);
            } else {
                v.push(tid);
            }
        });
        on_change.call(enabled.get_untracked());
    };

    let select_all = move |_| {
        let all: Vec<String> = TRACK_TYPES.iter().map(|t| t.id.to_string()).collect();
        set_enabled.set(all.clone());
        on_change.call(all);
    };

    let select_none = move |_| {
        set_enabled.set(vec![]);
        on_change.call(vec![]);
    };

    view! {
        <aside id="track-filter">
            <header>
                <h3>"Track Types"</h3>
                <div class="segmented">
                    <button on:click=select_all>"All"</button>
                    <button on:click=select_none>"None"</button>
                </div>
            </header>
            <ul>
                {TRACK_TYPES.iter().map(|t| {
                    let id = t.id.to_string();
                    let id_dis = id.clone();
                    let id_act = id.clone();
                    let id_chk = id.clone();
                    let icon_class = t.icon;
                    let label = t.label;
                    view! {
                        <li
                            class:disabled=move || !available.get().contains(&id_dis)
                            class:active=move || enabled.get().contains(&id_act) && available.get().contains(&id_act)
                            on:click=move |_| {
                                if available.get_untracked().contains(&id) {
                                    toggle(&id);
                                }
                            }
                        >
                            <i class=icon_class></i>
                            <span class="label">{label}</span>
                            <span class="check">{move || {
                                if available.get().contains(&id_chk) && enabled.get().contains(&id_chk) { "✓" } else { "" }
                            }}</span>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </aside>
    }
}
