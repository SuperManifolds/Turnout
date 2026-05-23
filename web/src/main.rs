use leptos::*;

mod components;

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    let (available_types, set_available_types) = create_signal::<Vec<String>>(vec![]);
    let (enabled_types, set_enabled_types) = create_signal(
        components::track_filter::default_enabled_types()
    );
    let (has_selection, set_has_selection) = create_signal(false);
    let (apply_speed_limits, set_apply_speed_limits) = create_signal(true);

    let on_filter_change = Callback::new(move |types: Vec<String>| {
        set_enabled_types.set(types);
    });

    view! {
        <main>
            <section id="map-container">
                <components::Map
                    set_available_types=set_available_types
                    enabled_types=enabled_types
                    set_has_selection=set_has_selection
                    apply_speed_limits=apply_speed_limits
                />
                <components::Search />
                <components::LayerSwitcher />
                <Show when=move || has_selection.get()>
                    <div id="sidebar">
                        <components::TrackFilter
                            available=available_types.into()
                            on_change=on_filter_change
                        />
                        <components::Settings
                            apply_speed_limits=apply_speed_limits
                            set_apply_speed_limits=set_apply_speed_limits
                        />
                    </div>
                </Show>
            </section>
        </main>
    }
}
