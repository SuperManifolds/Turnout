//! Default values for the ORM style's `global-state` options.
//!
//! The `OpenRailwayMap` style is one style driven by a `state` block: a track mode
//! plus per-option toggles (stations, signals, POIs, platforms, lifecycle filters,
//! ...), and every layer filters/styles itself via `["global-state", "<key>"]`.
//! maplibre-native evaluates global-state, so Turnout sets option values on the
//! renderer's style instead of rewriting it. The style's `state` block declares each
//! option with its default, which is where the UI starts.

use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct StyleState {
    #[serde(default)]
    state: Map<String, Value>,
}

/// The default value of every option the style's `state` block declares.
pub fn default_state(style_json: &str) -> Map<String, Value> {
    let style: StyleState = match serde_json::from_str(style_json) {
        Ok(style) => style,
        Err(e) => {
            tracing::error!("could not read the ORM style's option defaults: {e}");
            return Map::new();
        }
    };
    style
        .state
        .into_iter()
        .filter_map(|(key, decl)| Some((key, decl.get("default")?.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embedded_style_declares_the_option_defaults() {
        let state = default_state(crate::orm_tiles::STYLES[0].1);
        assert_eq!(state.get("tracks"), Some(&json!("usage")));
        assert_eq!(state.get("platforms"), Some(&json!("plain")));
        assert_eq!(state.get("boxes"), Some(&json!("none")));
        assert_eq!(state.get("signals"), Some(&json!([])));
        // Turnout serves no OpenHistoricalMap source.
        assert_eq!(state.get("openHistoricalMap"), Some(&json!(false)));
    }

    #[test]
    fn reads_each_declared_default() {
        let state =
            default_state(r#"{"state":{"tracks":{"default":"speed"},"date":{"default":1990}}}"#);
        assert_eq!(state.get("tracks"), Some(&json!("speed")));
        assert_eq!(state.get("date"), Some(&json!(1990)));
    }

    #[test]
    fn a_style_without_state_has_no_defaults() {
        assert!(default_state(r#"{"layers":[]}"#).is_empty());
    }
}
