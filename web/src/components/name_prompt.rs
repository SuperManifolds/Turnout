use leptos::*;

#[component]
pub fn NamePrompt(
    #[prop(into)] default_name: String,
    #[prop(into)] on_confirm: Callback<String>,
    #[prop(into)] on_cancel: Callback<()>,
) -> impl IntoView {
    let (name, set_name) = create_signal(default_name);

    let on_submit = move |ev: web_sys::SubmitEvent| {
        ev.prevent_default();
        let n = name.get_untracked().trim().to_string();
        if !n.is_empty() {
            on_confirm.call(sanitize_name(&n));
        }
    };

    view! {
        <div id="modal-overlay" on:click=move |_| on_cancel.call(())>
            <form id="name-prompt" on:submit=on_submit on:click=move |ev| ev.stop_propagation()>
                <h2>"Blueprint Name"</h2>
                <input
                    type="text"
                    prop:value=name
                    on:input=move |ev| set_name.set(event_target_value(&ev))
                    autofocus=true
                    placeholder="e.g. bielefeld_hbf"
                />
                <nav>
                    <button type="button" on:click=move |_| on_cancel.call(())>"Cancel"</button>
                    <button type="submit" class="primary">"Import"</button>
                </nav>
            </form>
        </div>
    }
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_lowercase()
}
