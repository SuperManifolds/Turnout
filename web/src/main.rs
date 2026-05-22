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
            <header>
                <h1>"Nimby Gen"</h1>
                <p>"Import OpenRailwayMap tracks into Nimby Rails blueprints"</p>
            </header>
            <section id="map-container">
                <components::Map />
            </section>
        </main>
    }
}
