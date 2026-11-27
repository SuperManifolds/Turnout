//! Vertical levels of the `OpenRailwayMap` levels source.

/// `TileJSON` of the live levels source on orm.sorlie.io. NIMBY Rails players add
/// it as a map source and style it with the "ORM Vertical Layers" Workshop mod.
pub const LEVELS_TILEJSON_URL: &str = "https://orm.sorlie.io/levels.json";

/// The MVT layer names served by orm.sorlie.io `railway_levels` and styled by
/// the "ORM Vertical Layers" Workshop mod, from the lowest level to the highest.
pub const LEVEL_LAYERS: [&str; 11] = [
    "Underground5",
    "Underground4",
    "Underground3",
    "Underground2",
    "Underground1",
    "Ground",
    "Elevated1",
    "Elevated2",
    "Elevated3",
    "Elevated4",
    "Elevated5",
];

/// The level bands the ORM overlay's Levels option selects from: a line or
/// platform below ground level, at it, or above it.
pub const LEVEL_BANDS: [&str; 3] = ["underground", "ground", "elevated"];

/// The ORM style `global-state` key holding the selected [`LEVEL_BANDS`].
pub const LEVELS_KEY: &str = "levels";
