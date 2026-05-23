use leptos::{component, view, ReadSignal, WriteSignal, IntoView, SignalUpdate, SignalGet, SignalSet, event_target_value};

#[component]
pub fn Settings(
    apply_speed_limits: ReadSignal<bool>,
    set_apply_speed_limits: WriteSignal<bool>,
    clip_to_selection: ReadSignal<bool>,
    set_clip_to_selection: WriteSignal<bool>,
    tangent_mode: ReadSignal<bool>,
    set_tangent_mode: WriteSignal<bool>,
    overpass_timeout: ReadSignal<u32>,
    set_overpass_timeout: WriteSignal<u32>,
) -> impl IntoView {
    view! {
        <aside id="settings-panel">
            <header>
                <h3>"Settings"</h3>
            </header>
            <ul>
                <li on:click=move |_| set_apply_speed_limits.update(|v| *v = !*v)
                    title="Use real-world speed limits from OpenStreetMap"
                >
                    <i class=move || if apply_speed_limits.get() { "fa-solid fa-square-check" } else { "fa-regular fa-square" }></i>
                    <span class="label">"Apply speed limits"</span>
                </li>
                <li on:click=move |_| set_clip_to_selection.update(|v| *v = !*v)
                    title="Cut tracks at the selection boundary instead of including full ways"
                >
                    <i class=move || if clip_to_selection.get() { "fa-solid fa-square-check" } else { "fa-regular fa-square" }></i>
                    <span class="label">"Clip to selection"</span>
                </li>
            </ul>
            <nav class="mode-switch"
                title="Point: curves pass through nodes. Tangent: curves fit inside node corners"
            >
                <span class="mode-label">"Track mode"</span>
                <div class="segmented">
                    <button
                        class:active=move || !tangent_mode.get()
                        on:click=move |_| set_tangent_mode.set(false)
                    >"Point"</button>
                    <button
                        class:active=move || tangent_mode.get()
                        on:click=move |_| set_tangent_mode.set(true)
                    >"Tangent"</button>
                </div>
            </nav>
            <nav class="mode-switch"
                title="Overpass API query timeout in seconds"
            >
                <span class="mode-label">"Timeout"</span>
                <div class="timeout-input">
                    <input
                        type="number"
                        min="10"
                        max="300"
                        prop:value=move || overpass_timeout.get().to_string()
                        on:change=move |ev| {
                            if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                set_overpass_timeout.set(v.clamp(10, 300));
                            }
                        }
                    />
                    <span class="unit">"s"</span>
                </div>
            </nav>
        </aside>
    }
}
