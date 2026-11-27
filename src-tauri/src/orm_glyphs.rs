//! Self-hosted SDF glyphs for the online ORM styles.
//!
//! `OpenRailwayMap` moved from serving SDF glyph PBFs (`/font/{fontstack}/{range}.pbf`)
//! to client-side TTF `font-faces`, so that endpoint now 404s. maplibre-native needs
//! SDF glyph PBFs, so we bundle them and serve the online styles a `file://` glyphs
//! URL pointing at a copy materialized next to the render cache. Mirrors how the
//! offline styles already reference local fonts.
//!
//! Coverage is U+0020-052F (ranges 0 through 1280-1535), matching the unicode-range
//! upstream's `font-faces` declare. The glyphs are generated from the same Noto Sans
//! and Fira Code faces upstream ships; regenerate with the scripted fontnik step.

use std::path::{Path, PathBuf};

/// `(fontstack, range, bytes)`. The fontstack names match the styles' `text-font`.
const BUNDLED: &[(&str, &str, &[u8])] = &[
    (
        "OpenRailwayMap-Regular",
        "0-255",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Regular/0-255.pbf"),
    ),
    (
        "OpenRailwayMap-Regular",
        "256-511",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Regular/256-511.pbf"),
    ),
    (
        "OpenRailwayMap-Regular",
        "512-767",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Regular/512-767.pbf"),
    ),
    (
        "OpenRailwayMap-Regular",
        "768-1023",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Regular/768-1023.pbf"),
    ),
    (
        "OpenRailwayMap-Regular",
        "1024-1279",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Regular/1024-1279.pbf"),
    ),
    (
        "OpenRailwayMap-Regular",
        "1280-1535",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Regular/1280-1535.pbf"),
    ),
    (
        "OpenRailwayMap-Bold",
        "0-255",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Bold/0-255.pbf"),
    ),
    (
        "OpenRailwayMap-Bold",
        "256-511",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Bold/256-511.pbf"),
    ),
    (
        "OpenRailwayMap-Bold",
        "512-767",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Bold/512-767.pbf"),
    ),
    (
        "OpenRailwayMap-Bold",
        "768-1023",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Bold/768-1023.pbf"),
    ),
    (
        "OpenRailwayMap-Bold",
        "1024-1279",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Bold/1024-1279.pbf"),
    ),
    (
        "OpenRailwayMap-Bold",
        "1280-1535",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Bold/1280-1535.pbf"),
    ),
    (
        "OpenRailwayMap-Italic",
        "0-255",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Italic/0-255.pbf"),
    ),
    (
        "OpenRailwayMap-Italic",
        "256-511",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Italic/256-511.pbf"),
    ),
    (
        "OpenRailwayMap-Italic",
        "512-767",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Italic/512-767.pbf"),
    ),
    (
        "OpenRailwayMap-Italic",
        "768-1023",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Italic/768-1023.pbf"),
    ),
    (
        "OpenRailwayMap-Italic",
        "1024-1279",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Italic/1024-1279.pbf"),
    ),
    (
        "OpenRailwayMap-Italic",
        "1280-1535",
        include_bytes!("../resources/orm/glyphs/OpenRailwayMap-Italic/1280-1535.pbf"),
    ),
    (
        "FiraCode-Bold",
        "0-255",
        include_bytes!("../resources/orm/glyphs/FiraCode-Bold/0-255.pbf"),
    ),
    (
        "FiraCode-Bold",
        "256-511",
        include_bytes!("../resources/orm/glyphs/FiraCode-Bold/256-511.pbf"),
    ),
    (
        "FiraCode-Bold",
        "512-767",
        include_bytes!("../resources/orm/glyphs/FiraCode-Bold/512-767.pbf"),
    ),
    (
        "FiraCode-Bold",
        "768-1023",
        include_bytes!("../resources/orm/glyphs/FiraCode-Bold/768-1023.pbf"),
    ),
    (
        "FiraCode-Bold",
        "1024-1279",
        include_bytes!("../resources/orm/glyphs/FiraCode-Bold/1024-1279.pbf"),
    ),
    (
        "FiraCode-Bold",
        "1280-1535",
        include_bytes!("../resources/orm/glyphs/FiraCode-Bold/1280-1535.pbf"),
    ),
];

/// Writes the bundled glyphs under `<cache_dir>/glyphs/<fontstack>/<range>.pbf` and
/// returns that `glyphs` directory, for the online styles' `file://` glyphs URL.
/// Overwrites each file so an app update ships fresh glyphs.
pub fn materialize(cache_dir: &Path) -> std::io::Result<PathBuf> {
    let root = cache_dir.join("glyphs");
    for (stack, range, bytes) in BUNDLED {
        let dir = root.join(stack);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{range}.pbf")), bytes)?;
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `text-font` with no bundled glyphs leaves maplibre-native waiting forever on
    /// the missing glyph file, so every tile with that label hangs.
    #[test]
    fn every_style_font_has_bundled_glyphs() {
        for (name, json) in crate::orm_tiles::STYLES {
            let style: serde_json::Value = serde_json::from_str(json).expect("embedded style parses");
            for layer in style["layers"].as_array().expect("layers") {
                let Some(fonts) = layer["layout"]["text-font"].as_array() else { continue };
                for font in fonts {
                    let font = font.as_str().expect("text-font is a literal list");
                    assert!(
                        BUNDLED.iter().any(|(stack, _, _)| *stack == font),
                        "{name} layer {} uses unbundled font {font}",
                        layer["id"]
                    );
                }
            }
        }
    }
}
