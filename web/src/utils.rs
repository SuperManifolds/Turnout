use std::fmt::Write;

pub fn urlencoding(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' => result.push('+'),
            c if c.is_alphanumeric() || "-_.~".contains(c) => result.push(c),
            c => { let _ = write!(result, "%{:02X}", c as u32); }
        }
    }
    result
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

pub fn load_u32(key: &str, default: u32) -> u32 {
    storage()
        .and_then(|s| s.get_item(key).ok()?)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub fn save_u32(key: &str, value: u32) {
    if let Some(s) = storage() {
        let _ = s.set_item(key, &value.to_string());
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
