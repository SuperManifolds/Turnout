use leptos::*;

mod components;

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    view! {
        <main>
            <section id="map-container">
                <components::Map />
                <components::Search />
                <components::LayerSwitcher />
            </section>
        </main>
    }
}
