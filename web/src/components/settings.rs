use leptos::*;

#[component]
pub fn Settings(
    apply_speed_limits: ReadSignal<bool>,
    set_apply_speed_limits: WriteSignal<bool>,
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
            </ul>
        </aside>
    }
}
