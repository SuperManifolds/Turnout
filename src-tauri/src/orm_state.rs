//! Resolves GL-JS-5 `global-state` expressions in the online ORM styles.
//!
//! The new `OpenRailwayMap` style is one style driven by a `state` block: a track
//! mode plus per-option toggles (stations, signals, POIs, platforms, lifecycle
//! filters, ...), and every detail layer filters/styles itself from that state via
//! `["global-state", "<key>"]`. maplibre-native cannot evaluate global-state, so
//! Turnout substitutes the current option values into the style before handing it
//! to mbgl. This is how both the detail layers and the per-option UI work on native.

use serde_json::{json, Map, Value};

/// Recursively replaces every `["global-state", "<key>"]` expression with the value
/// for `<key>` in `state` (null when absent). Array and object values are wrapped as
/// `["literal", …]` so they stay valid where an expression is expected; scalars are
/// substituted bare (a bare string/number/bool/null is already a valid literal).
pub fn resolve(value: &mut Value, state: &Map<String, Value>) {
    match value {
        Value::Array(arr) => {
            if arr.len() == 2
                && arr[0] == Value::String("global-state".to_owned())
                && let Some(key) = arr[1].as_str()
            {
                *value = wrap_literal(state.get(key).cloned().unwrap_or(Value::Null));
                return;
            }
            for item in arr.iter_mut() {
                resolve(item, state);
            }
        }
        Value::Object(obj) => {
            for v in obj.values_mut() {
                resolve(v, state);
            }
        }
        _ => {}
    }
}

fn wrap_literal(value: Value) -> Value {
    match value {
        array_or_object @ (Value::Array(_) | Value::Object(_)) => json!(["literal", array_or_object]),
        scalar => scalar,
    }
}

/// The option values the new `OpenRailwayMap` UI defaults to (its style `state`
/// block). Used until the Turnout UI overrides individual options.
pub fn default_state() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("tracks".into(), json!("usage"));
    m.insert("stations".into(), json!("station"));
    m.insert("stationLowZoomLabel".into(), json!("label"));
    m.insert("platforms".into(), json!("plain"));
    m.insert("switches".into(), json!("plain"));
    m.insert("signals".into(), json!([]));
    m.insert(
        "pois".into(),
        json!(["radio", "facility", "equipment", "level_crossing", "train_protection"]),
    );
    m.insert("boxes".into(), json!("none"));
    m.insert("catenaries".into(), json!("none"));
    m.insert("substations".into(), json!("none"));
    m.insert("showConstructionInfrastructure".into(), json!(true));
    m.insert("showProposedInfrastructure".into(), json!(true));
    m.insert("showAbandonedInfrastructure".into(), json!(false));
    m.insert("showRazedInfrastructure".into(), json!(false));
    m.insert("openHistoricalMap".into(), json!(true));
    m.insert("hillshade".into(), json!(false));
    m.insert("pitched".into(), json!(false));
    m.insert("bearing".into(), Value::Null);
    m.insert("date".into(), json!(2026));
    m.insert("allDates".into(), json!(false));
    m.insert("theme".into(), json!("light"));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_state_substitutes_bare() {
        let state = default_state();
        let mut v = json!(["==", ["global-state", "pitched"], false]);
        resolve(&mut v, &state);
        assert_eq!(v, json!(["==", false, false]));
    }

    #[test]
    fn array_state_wraps_in_literal() {
        let state = default_state();
        // signals default is [], which must become ["literal", []] in an expression.
        let mut v = json!(["in", ["get", "category0"], ["global-state", "signals"]]);
        resolve(&mut v, &state);
        assert_eq!(v, json!(["in", ["get", "category0"], ["literal", []]]));
        // pois default is a non-empty list, also wrapped.
        let mut p = json!(["global-state", "pois"]);
        resolve(&mut p, &state);
        assert_eq!(
            p,
            json!(["literal", ["radio", "facility", "equipment", "level_crossing", "train_protection"]])
        );
    }

    #[test]
    fn unknown_key_resolves_to_null() {
        let state = Map::new();
        let mut v = json!(["global-state", "not_a_real_option"]);
        resolve(&mut v, &state);
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn resolves_nested_in_a_layer() {
        let state = default_state();
        let mut layer = json!({
            "id": "x",
            "filter": ["all", ["==", ["global-state", "pitched"], false],
                       ["in", ["get", "c"], ["global-state", "signals"]]],
            "layout": {"visibility": "visible"}
        });
        resolve(&mut layer, &state);
        assert_eq!(
            layer["filter"],
            json!(["all", ["==", false, false], ["in", ["get", "c"], ["literal", []]]])
        );
    }
}
