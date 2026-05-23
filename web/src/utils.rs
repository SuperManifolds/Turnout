pub fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "+".to_string(),
            c if c.is_alphanumeric() || "-_.~".contains(c) => c.to_string(),
            c => format!("%{:02X}", c as u32),
        })
        .collect()
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

pub fn load_bool(key: &str, default: bool) -> bool {
    storage()
        .and_then(|s| s.get_item(key).ok()?)
        .map_or(default, |v| v == "true")
}

pub fn save_bool(key: &str, value: bool) {
    if let Some(s) = storage() {
        let _ = s.set_item(key, if value { "true" } else { "false" });
    }
}

pub fn load_string_vec(key: &str) -> Option<Vec<String>> {
    let json = storage()?.get_item(key).ok()??;
    serde_json::from_str(&json).ok()
}

pub fn save_string_vec(key: &str, values: &[String]) {
    if let Some(s) = storage()
        && let Ok(json) = serde_json::to_string(values) {
            let _ = s.set_item(key, &json);
    }
}
