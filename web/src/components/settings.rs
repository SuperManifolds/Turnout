use leptos::{component, view, ReadSignal, WriteSignal, IntoView, SignalUpdate, SignalGet};

#[component]
pub fn Settings(
    apply_speed_limits: ReadSignal<bool>,
    set_apply_speed_limits: WriteSignal<bool>,
    clip_to_selection: ReadSignal<bool>,
    set_clip_to_selection: WriteSignal<bool>,
    tangent_mode: ReadSignal<bool>,
    set_tangent_mode: WriteSignal<bool>,
) -> impl IntoView {
    view! {
        <aside id="settings-panel">
            <header>
                <h3>"Settings"</h3>
            </header>
            <ul>
                <li on:click=move |_| set_apply_speed_limits.update(|v| *v = !*v)>
                    <i class=move || if apply_speed_limits.get() { "fa-solid fa-square-check" } else { "fa-regular fa-square" }></i>
                    <span class="label">"Apply speed limits"</span>
                </li>
                <li on:click=move |_| set_clip_to_selection.update(|v| *v = !*v)>
                    <i class=move || if clip_to_selection.get() { "fa-solid fa-square-check" } else { "fa-regular fa-square" }></i>
                    <span class="label">"Clip to selection"</span>
                </li>
                <li on:click=move |_| set_tangent_mode.update(|v| *v = !*v)>
                    <i class=move || if tangent_mode.get() { "fa-solid fa-square-check" } else { "fa-regular fa-square" }></i>
                    <span class="label">"Tangent mode"</span>
                </li>
            </ul>
        </aside>
    }
}
