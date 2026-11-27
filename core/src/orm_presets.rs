//! Presets of the ORM overlay's `global-state` options: the classic per-mode
//! presets the preset bar offers and the tile server's legacy per-mode URLs
//! render.

use serde_json::{Map, Value};

/// The option value that switches a category off.
pub const NONE: &str = "none";

/// A literal option value set by a preset.
#[derive(Clone, Copy)]
pub enum Literal {
    Bool(bool),
    Str(&'static str),
    List(&'static [&'static str]),
}

impl Literal {
    #[must_use]
    pub fn to_value(self) -> Value {
        match self {
            Self::Bool(b) => Value::Bool(b),
            Self::Str(s) => Value::from(s),
            Self::List(items) => Value::from(items.to_vec()),
        }
    }
}

/// A preset: its label and the option values it sets.
pub type Preset = (&'static str, &'static [(&'static str, Literal)]);

/// The detail options with everything switched off. A classic preset starts from
/// this, so picking one replaces the previous view instead of adding to it.
pub const DETAILS_OFF: &[(&str, Literal)] = &[
    ("stations", Literal::Str(NONE)),
    ("platforms", Literal::Str(NONE)),
    ("switches", Literal::Str(NONE)),
    ("signals", Literal::List(&[])),
    ("catenaries", Literal::Str(NONE)),
    ("boxes", Literal::Str(NONE)),
    ("substations", Literal::Str(NONE)),
    ("pois", Literal::List(&[])),
];

/// The classic per-mode presets the preset bar offers and the tile server's
/// legacy per-mode URLs render, as the track mode and details each one
/// draws (on top of [`DETAILS_OFF`]).
pub const CLASSIC_PRESETS: &[Preset] = &[
    (
        "Infrastructure",
        &[
            ("tracks", Literal::Str("usage")),
            ("stations", Literal::Str("station")),
            ("platforms", Literal::Str("plain")),
            ("switches", Literal::Str("plain")),
            (
                "pois",
                Literal::List(&[
                    "radio",
                    "facility",
                    "equipment",
                    "level_crossing",
                    "train_protection",
                ]),
            ),
        ],
    ),
    (
        "Speed",
        &[
            ("tracks", Literal::Str("speed")),
            ("signals", Literal::List(&["speed"])),
        ],
    ),
    (
        "Signals",
        &[
            ("tracks", Literal::Str("train_protection")),
            (
                "signals",
                Literal::List(&[
                    "main",
                    "distant",
                    "train_protection",
                    "station",
                    "radio",
                    "shunting",
                    "other",
                ]),
            ),
            ("boxes", Literal::Str("plain")),
            ("pois", Literal::List(&["train_protection"])),
        ],
    ),
    (
        "Electrification",
        &[
            ("tracks", Literal::Str("voltage_frequency")),
            ("signals", Literal::List(&["electricity"])),
            ("catenaries", Literal::Str("plain")),
            ("substations", Literal::Str("plain")),
        ],
    ),
    ("Track", &[("tracks", Literal::Str("gauge"))]),
    (
        "Operator",
        &[
            ("tracks", Literal::Str("operator")),
            ("stations", Literal::Str("operator")),
            ("boxes", Literal::Str("operator")),
        ],
    ),
    (
        "Route",
        &[
            ("tracks", Literal::Str("routes")),
            ("stations", Literal::Str("station")),
        ],
    ),
];

/// Every option value a classic preset sets: [`DETAILS_OFF`] with its overrides.
#[must_use]
pub fn classic_values(overrides: &[(&'static str, Literal)]) -> Vec<(&'static str, Value)> {
    DETAILS_OFF
        .iter()
        .filter(|(key, _)| !overrides.iter().any(|(k, _)| k == key))
        .chain(overrides)
        .map(|(key, lit)| (*key, lit.to_value()))
        .collect()
}

/// The option values of the [`CLASSIC_PRESETS`] entry labelled `name`, or `None`
/// when no preset has that label.
#[must_use]
pub fn classic_state(name: &str) -> Option<Map<String, Value>> {
    let (_, overrides) = CLASSIC_PRESETS.iter().find(|(label, _)| *label == name)?;
    Some(
        classic_values(overrides)
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classic_presets_set_every_detail_once() {
        for &(name, overrides) in CLASSIC_PRESETS {
            let values = classic_values(overrides);
            let mut keys: Vec<&str> = values.iter().map(|(key, _)| *key).collect();
            keys.sort_unstable();
            let mut expected: Vec<&str> = DETAILS_OFF
                .iter()
                .map(|(key, _)| *key)
                .chain(["tracks"])
                .collect();
            expected.sort_unstable();
            assert_eq!(keys, expected, "{name}");
        }
    }

    #[test]
    fn classic_preset_overrides_win_over_details_off() {
        let (_, speed) = CLASSIC_PRESETS
            .iter()
            .find(|(name, _)| *name == "Speed")
            .expect("Speed preset");
        let values = classic_values(speed);
        let signals = values
            .iter()
            .find(|(key, _)| *key == "signals")
            .map(|(_, v)| v.clone());
        assert_eq!(signals, Some(json!(["speed"])));
        let stations = values
            .iter()
            .find(|(key, _)| *key == "stations")
            .map(|(_, v)| v.clone());
        assert_eq!(stations, Some(json!(NONE)));
    }

    #[test]
    fn classic_state_holds_the_preset_values() {
        let state = classic_state("Signals").expect("Signals preset");
        assert_eq!(state.len(), DETAILS_OFF.len() + 1);
        assert_eq!(state.get("tracks"), Some(&json!("train_protection")));
        assert_eq!(state.get("boxes"), Some(&json!("plain")));
        assert_eq!(state.get("platforms"), Some(&json!(NONE)));
    }

    #[test]
    fn classic_state_of_an_unknown_preset_is_none() {
        assert!(classic_state("Satellite").is_none());
    }
}
