//! Deterministic render benchmark for the ORM tile pipeline.
//!
//! Renders a fixed set of representative tiles through maplibre-native's
//! `ImageRenderer` (Metal on macOS, Vulkan on Windows/Linux) and reports
//! throughput + latency as JSON. Source tiles are fetched and cached during a
//! warmup pass, so the timed passes measure the render path, not the network.
//!
//! Sweeps the style's layer count so we can see whether per-render cost scales
//! with the number of style layers (fixed per-render overhead) or with the
//! actual geometry drawn.
//!
//! `orm-bench levels` instead checks the Levels option: it renders one London
//! tile with no level band, with only the underground band and with every band,
//! and exits non-zero unless each render draws more than the one before.

use std::num::NonZeroU32;
use std::time::Instant;

use maplibre_native::{
    GlobalStateValue, Image, ImageRenderer, ImageRendererBuilder, RenderingError, ResourceOptions,
    Tile,
};

const STYLE: &str = include_str!("../../src-tauri/resources/orm/orm.json");
/// The host `build_orm_style.py` bakes into `STYLE` (the app's `STYLE_SOURCE_BASE`).
const STYLE_SOURCE_BASE: &str = "https://openrailwaymap.app";
/// The ORM host the app renders from by default (the app's `DEFAULT_ORM_BASE`).
/// openrailwaymap.app answers maplibre-native's tile requests with 403.
const ORM_BASE: &str = "https://orm.sorlie.io";
/// The bundled SDF glyphs, which the app also loads locally (upstream serves none).
const GLYPHS_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../src-tauri/resources/orm/glyphs"
);
const TILE_SIZE: u32 = 512;
const CACHE_BYTES: u64 = 512 * 1024 * 1024;
const WARMUP_PASSES: usize = 2;
const TIMED_PASSES: usize = 20;

/// The style `global-state` key and band the `levels` check selects
/// (`turnout_core::orm_levels`).
const LEVELS_KEY: &str = "levels";
const UNDERGROUND: &str = "underground";
/// Central London at z14, where Underground lines run in tunnels.
const LEVELS_TILE: (u8, u32, u32) = (14, 8186, 5448);

const POINTS: &[(f64, f64, u8)] = &[
    (51.512, 7.463, 13),
    (51.512, 7.463, 14),
    (51.512, 7.463, 15),
    (52.525, 13.369, 13),
    (52.525, 13.369, 14),
    (52.525, 13.369, 15),
    (48.140, 11.560, 14),
    (48.140, 11.560, 15),
    (47.378, 8.540, 14),
    (47.378, 8.540, 15),
    (50.943, 6.958, 14),
    (50.943, 6.958, 15),
];

fn tile_xy(lat: f64, lon: f64, z: u8) -> (u32, u32) {
    let n = f64::from(1u32 << z);
    let x = (lon + 180.0) / 360.0 * n;
    let lat_rad = lat.to_radians();
    let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x.floor() as u32, y.floor() as u32)
}

/// `STYLE` as the app loads it by default: glyphs from the bundled directory,
/// tiles and sprites from `ORM_BASE`.
fn runtime_style() -> String {
    STYLE
        .replace(
            &format!("{STYLE_SOURCE_BASE}/font/"),
            &format!("file://{GLYPHS_DIR}/"),
        )
        .replace(STYLE_SOURCE_BASE, ORM_BASE)
}

/// Returns the runtime style with its `layers` array truncated to the first `n` entries.
fn style_with_layers(n: usize) -> (String, usize) {
    let mut v: serde_json::Value =
        serde_json::from_str(&runtime_style()).expect("valid style json");
    let total = v["layers"].as_array().map_or(0, Vec::len);
    if let Some(layers) = v["layers"].as_array_mut() {
        layers.truncate(n);
    }
    (v.to_string(), total.min(n))
}

fn tile_renderer() -> ImageRenderer<Tile> {
    let size = NonZeroU32::new(TILE_SIZE).expect("nonzero tile size");
    let opts = ResourceOptions::default()
        .with_cache_path(std::env::temp_dir().join("orm-bench-cache.db"))
        .with_maximum_cache_size(CACHE_BYTES);
    ImageRendererBuilder::new()
        .with_size(size, size)
        .with_resource_options(opts)
        .build_tile_renderer()
        .expect("failed to build tile renderer (no working GPU/Vulkan backend)")
}

fn bench_style(style: &str, tiles: &[(u8, u32, u32)]) -> (f64, f64, f64) {
    let mut renderer = tile_renderer();
    renderer.load_style_from_json_str(style);

    for _ in 0..WARMUP_PASSES {
        for &(z, x, y) in tiles {
            let _ = renderer.render_tile(z, x, y);
        }
    }

    let mut samples: Vec<f64> = Vec::with_capacity(TIMED_PASSES * tiles.len());
    let wall = Instant::now();
    for _ in 0..TIMED_PASSES {
        for &(z, x, y) in tiles {
            let t = Instant::now();
            if renderer.render_tile(z, x, y).is_ok() {
                samples.push(t.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }
    let wall_s = wall.elapsed().as_secs_f64();
    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let p50 = samples[samples.len() / 2];
    (samples.len() as f64 / wall_s, mean, p50)
}

/// The number of pixels a rendered tile draws (alpha above zero).
fn drawn_pixels(img: &Image) -> usize {
    img.as_image()
        .as_raw()
        .chunks_exact(4)
        .filter(|px| px[3] != 0)
        .count()
}

/// Renders `LEVELS_TILE` with the `levels` global-state set to `bands`.
fn render_levels(
    renderer: &mut ImageRenderer<Tile>,
    bands: &[String],
) -> Result<usize, RenderingError> {
    renderer
        .style()
        .set_global_state(LEVELS_KEY, GlobalStateValue::StringList(bands.to_vec()));
    let (z, x, y) = LEVELS_TILE;
    renderer.render_tile(z, x, y).map(|img| drawn_pixels(&img))
}

/// The drawn pixels of `LEVELS_TILE` with no band, with only the underground
/// band and with every band.
fn level_counts(
    renderer: &mut ImageRenderer<Tile>,
    all_bands: &[String],
) -> Result<(usize, usize, usize), RenderingError> {
    let none = render_levels(renderer, &[])?;
    let underground = render_levels(renderer, &[UNDERGROUND.to_owned()])?;
    let all = render_levels(renderer, all_bands)?;
    Ok((none, underground, all))
}

/// Whether the Levels option filters the render: `LEVELS_TILE` with only the
/// underground band draws more than with no band (which still draws the layers
/// the option doesn't gate), and less than with every band (the style's
/// `levels` default).
fn levels_check() -> bool {
    let style: serde_json::Value = serde_json::from_str(STYLE).expect("valid style json");
    let Some(all_bands) = style["state"][LEVELS_KEY]["default"].as_array() else {
        eprintln!("the style declares no `{LEVELS_KEY}` state");
        return false;
    };
    let all_bands: Vec<String> = all_bands
        .iter()
        .filter_map(|b| b.as_str().map(str::to_owned))
        .collect();
    let mut renderer = tile_renderer();
    // global-state can only be set once the style has loaded.
    if let Err(e) = renderer.load_style_from_json_str(runtime_style()).wait() {
        eprintln!("style failed to load: {e}");
        return false;
    }
    let (none, underground, all) = match level_counts(&mut renderer, &all_bands) {
        Ok(counts) => counts,
        Err(e) => {
            eprintln!("render failed: {e:?}");
            return false;
        }
    };
    let (z, x, y) = LEVELS_TILE;
    let ok = none < underground && underground < all;
    println!("{{\"tile\":\"{z}/{x}/{y}\",\"no_bands_px\":{none},\"underground_px\":{underground},\"all_bands_px\":{all},\"ok\":{ok}}}");
    ok
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("levels") {
        if !levels_check() {
            std::process::exit(1);
        }
        return;
    }

    let tiles: Vec<(u8, u32, u32)> = POINTS
        .iter()
        .map(|&(lat, lon, z)| {
            let (x, y) = tile_xy(lat, lon, z);
            (z, x, y)
        })
        .collect();

    // BENCH_LAYERS=<n> runs a single, long fixed-layer pass (for external CPU
    // sampling); otherwise sweep the layer counts.
    // BENCH_LAYERS mode reuses a single renderer for the whole run so shader
    // compilation happens once during warmup and the timed frames measure pure
    // steady-state rendering, like the app's long-lived per-style renderers.
    if let Ok(n) = std::env::var("BENCH_LAYERS").map(|s| s.parse::<usize>().unwrap_or(1)) {
        let secs: u64 = std::env::var("BENCH_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12);
        let (style, actual) = style_with_layers(n);
        let mut renderer = tile_renderer();
        renderer.load_style_from_json_str(&style);

        for _ in 0..WARMUP_PASSES {
            for &(z, x, y) in &tiles {
                let _ = renderer.render_tile(z, x, y);
            }
        }

        let deadline = Instant::now() + std::time::Duration::from_secs(secs);
        let mut samples: Vec<f64> = Vec::new();
        let wall = Instant::now();
        while Instant::now() < deadline {
            for &(z, x, y) in &tiles {
                let t = Instant::now();
                if renderer.render_tile(z, x, y).is_ok() {
                    samples.push(t.elapsed().as_secs_f64() * 1000.0);
                }
            }
        }
        let wall_s = wall.elapsed().as_secs_f64();
        samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        let p50 = samples[samples.len() / 2];
        let tps = samples.len() as f64 / wall_s;

        // Correctness hash (untimed) over the rendered pixels.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &(z, x, y) in &tiles {
            if let Ok(img) = renderer.render_tile(z, x, y) {
                for &b in img.as_image().as_raw() {
                    hash ^= u64::from(b);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
        }
        println!(
            "{{\"layers\":{actual},\"tiles_per_sec\":{tps:.1},\"mean_ms\":{mean:.3},\"p50_ms\":{p50:.3},\"pixel_hash\":\"{hash:016x}\"}}"
        );
        return;
    }

    for n in [1usize, 5, 15, 40, 100, usize::MAX] {
        let (style, actual) = style_with_layers(n);
        let (tps, mean, p50) = bench_style(&style, &tiles);
        println!(
            "{{\"layers\":{actual},\"tiles_per_sec\":{tps:.1},\"mean_ms\":{mean:.3},\"p50_ms\":{p50:.3}}}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_xy_matches_slippy_reference_points() {
        // z0 has a single tile covering the world.
        assert_eq!(tile_xy(51.5, -0.1, 0), (0, 0));
        // z1 splits into a 2×2 grid; the equator/prime-meridian corner is (1, 1).
        assert_eq!(tile_xy(0.0, 0.0, 1), (1, 1));
        // Berlin at z12 (known OSM slippy coords).
        assert_eq!(tile_xy(52.52, 13.405, 12), (2200, 1343));
    }
}
