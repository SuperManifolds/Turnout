use leptos::{component, view, Callback, IntoView, create_signal, create_node_ref, html, create_effect, web_sys, SignalGet, SignalGetUntracked, SignalSet, Callable, event_target_value, spawn_local, Show};

#[component]
pub fn NamePrompt(
    #[prop(into)] default_name: String,
    #[prop(into)] on_confirm: Callback<String>,
    #[prop(into)] on_cancel: Callback<()>,
) -> impl IntoView {
    let (name, set_name) = create_signal(default_name);
    let (exists, set_exists) = create_signal(false);
    let input_ref = create_node_ref::<html::Input>();

    // Auto-select the default text so typing replaces it
    create_effect(move |_| {
        if let Some(input) = input_ref.get() {
            let el: &web_sys::HtmlInputElement = &input;
            let _ = el.focus();
            let () = el.select();
        }
    });

    let do_confirm = move || {
        let n = name.get_untracked().trim().to_string();
        if !n.is_empty() {
            on_confirm.call(sanitize_name(&n));
        }
    };

    let on_submit = move |ev: web_sys::SubmitEvent| {
        ev.prevent_default();
        let n = sanitize_name(&name.get_untracked());
        if n.is_empty() { return; }
        spawn_local(async move {
            if crate::tauri::blueprint_exists(&n).await {
                set_exists.set(true);
            } else {
                do_confirm();
            }
        });
    };

    view! {
        <div id="modal-overlay" on:click=move |_| on_cancel.call(())>
            <form id="name-prompt" on:submit=on_submit on:click=move |ev| ev.stop_propagation()>
                <h2>"Blueprint Name"</h2>
                <input
                    type="text"
                    node_ref=input_ref
                    prop:value=name
                    on:input=move |ev| {
                        set_name.set(event_target_value(&ev));
                        set_exists.set(false);
                    }
                    placeholder="e.g. bielefeld_hbf"
                />
                <Show when=move || exists.get()>
                    <p class="warning">"A blueprint with this name already exists. Overwrite?"</p>
                </Show>
                <nav>
                    <button type="button" on:click=move |_| on_cancel.call(())>"Cancel"</button>
                    <Show when=move || exists.get()
                        fallback=move || view! { <button type="submit" class="primary">"Import"</button> }
                    >
                        <button type="button" class="primary" on:click=move |_| do_confirm()>"Overwrite"</button>
                    </Show>
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
