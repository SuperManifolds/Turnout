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

/// The single levels the ORM overlay's Levels option selects from, from the
/// lowest to the highest (as in [`LEVEL_LAYERS`]). The style turns a line or
/// platform's level (its `layer`, clamped to -5..5) into one of these strings.
pub const LEVELS: [&str; 11] = ["-5", "-4", "-3", "-2", "-1", "0", "1", "2", "3", "4", "5"];

/// The [`LEVELS`] of each of the [`LEVEL_BANDS`], nearest the ground first.
pub const BAND_LEVELS: [(&str, &[&str]); 3] = [
    (LEVEL_BANDS[0], &["-1", "-2", "-3", "-4", "-5"]),
    (LEVEL_BANDS[1], &["0"]),
    (LEVEL_BANDS[2], &["1", "2", "3", "4", "5"]),
];

/// The ORM style `global-state` key holding the selected levels: any of the
/// [`LEVELS`], plus [`LEVEL_BANDS`] names, which select every level of the band.
/// The style draws a feature when its level or its band is in the list.
pub const LEVELS_KEY: &str = "levels";

/// The zoom from which the Levels option filters lines and platforms; lower
/// zooms draw every level.
pub const LEVELS_MIN_ZOOM: u8 = 8;

/// How many of a band's levels a selection holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandSelection {
    None,
    Partial,
    All,
}

/// The levels of `band`, nearest the ground first; none for an unknown band.
#[must_use]
pub fn band_levels(band: &str) -> &'static [&'static str] {
    BAND_LEVELS
        .iter()
        .find(|(name, _)| *name == band)
        .map_or(&[], |(_, levels)| levels)
}

/// The [`LEVELS`] a `levels` state value selects, in [`LEVELS`] order: its
/// single levels plus every level of the bands it names. Unknown values select
/// nothing.
pub fn expand_bands<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<&'static str> {
    let values: Vec<&str> = values.into_iter().collect();
    LEVELS
        .into_iter()
        .filter(|level| {
            values
                .iter()
                .any(|value| value == level || band_levels(value).contains(level))
        })
        .collect()
}

/// How many of `band`'s levels the selected `levels` hold.
#[must_use]
pub fn band_selection(levels: &[&str], band: &str) -> BandSelection {
    let members = band_levels(band);
    match members
        .iter()
        .filter(|level| levels.contains(level))
        .count()
    {
        0 => BandSelection::None,
        n if n == members.len() => BandSelection::All,
        _ => BandSelection::Partial,
    }
}

/// The selected `levels` with `band` switched: every level of the band off when
/// all of them are on, else all of them on. In [`LEVELS`] order.
#[must_use]
pub fn toggle_band(levels: &[&str], band: &str) -> Vec<&'static str> {
    let members = band_levels(band);
    let all_on = band_selection(levels, band) == BandSelection::All;
    LEVELS
        .into_iter()
        .filter(|level| {
            if members.contains(level) {
                !all_on
            } else {
                levels.contains(level)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_levels_cover_every_level_once() {
        let mut levels: Vec<&str> = BAND_LEVELS
            .iter()
            .flat_map(|(_, levels)| levels.iter().copied())
            .collect();
        levels.sort_by_key(|level| level.parse::<i8>().expect("numeric level"));
        assert_eq!(levels, LEVELS);
    }

    #[test]
    fn expand_bands_turns_old_band_values_into_their_levels() {
        assert_eq!(expand_bands(LEVEL_BANDS), LEVELS);
        assert_eq!(
            expand_bands(["underground"]),
            ["-5", "-4", "-3", "-2", "-1"]
        );
        assert_eq!(
            expand_bands(["elevated", "ground"]),
            ["0", "1", "2", "3", "4", "5"]
        );
        assert!(expand_bands([]).is_empty());
    }

    #[test]
    fn expand_bands_keeps_single_levels_and_drops_unknown_values() {
        assert_eq!(
            expand_bands(["2", "ground", "-3", "tunnel", "9"]),
            ["-3", "0", "2"]
        );
    }

    #[test]
    fn band_selection_counts_the_band_levels() {
        assert_eq!(
            band_selection(&["0", "1"], "underground"),
            BandSelection::None
        );
        assert_eq!(
            band_selection(&["-1", "0"], "underground"),
            BandSelection::Partial
        );
        assert_eq!(band_selection(&LEVELS, "elevated"), BandSelection::All);
        assert_eq!(band_selection(&["0"], "ground"), BandSelection::All);
    }

    #[test]
    fn toggle_band_switches_every_level_of_the_band() {
        assert_eq!(
            toggle_band(&["2", "-1"], "underground"),
            ["-5", "-4", "-3", "-2", "-1", "2"]
        );
        assert_eq!(
            toggle_band(&LEVELS, "underground"),
            ["0", "1", "2", "3", "4", "5"]
        );
        assert_eq!(toggle_band(&["0"], "ground"), Vec::<&str>::new());
    }
}
