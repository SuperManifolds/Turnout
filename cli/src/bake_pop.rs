//! One-time offline bake: rasterize census polygons or scatter a geographic
//! `GeoTIFF` onto the game's z10 grid and write a compact `PMTiles` the app
//! memory-maps as a file-backed population layer.
//!
//! Usage:
//!   `bake_pop` <out.pmtiles> <field> <input>...
//!
//! Inputs are dispatched by extension: `.shp` reads the `<field>` population
//! attribute and rasterizes polygons; `.tif`/`.tiff` scatters a geographic
//! (EPSG:4326) `GeoTIFF` (`<field>` is ignored). Examples:
//!   `bake_pop us_pop.pmtiles POP20 tl_2020_*_tabblock20.shp`  (2020 TIGER blocks)
//!   `bake_pop nor_pop.pmtiles - nor_ppp_2020.tif`             (`WorldPop` Norway)
//!
//! Tiles are 16-bit grayscale PNG (people per z10 pixel), overview zooms z9..z0
//! are mean-pooled (matching `pop400`), and the archive is gzip-compressed
//! internally so output stays small.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result};
use pmtiles::{Compression, PmTilesWriter, TileCoord, TileId, TileType};
use turnout_core::pop_edit::{downsample_children, EDIT_ZOOM, TILE_PX};
use turnout_core::pop_geotiff::{accumulate_geotiff_into, WORLD_BBOX};
use turnout_core::pop_import::{rasterize_geometry, GridAccumulator};
use turnout_core::shapefile_reader::read_valued_polygons;

/// `pop400` world bounds, reused so the layer aligns with the base.
const BOUNDS: (f64, f64, f64, f64) = (-180.0, -60.0, 180.0, 80.0);

type Grids = HashMap<(u32, u32), Vec<u16>>;

fn encode(values: &[u16]) -> Result<Vec<u8>> {
    let img =
        image::ImageBuffer::<image::Luma<u16>, _>::from_raw(TILE_PX, TILE_PX, values.to_vec())
            .context("tile buffer size mismatch")?;
    let mut bytes = Vec::new();
    image::DynamicImage::ImageLuma16(img)
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)?;
    Ok(bytes)
}

/// Mean-pool one zoom into its parent zoom (parent pixel = average of 4 children),
/// conserving total population as `pop400`'s pyramid does.
fn overview(child: &Grids) -> Grids {
    let parents: HashSet<(u32, u32)> = child.keys().map(|&(x, y)| (x / 2, y / 2)).collect();
    parents
        .into_iter()
        .map(|(px, py)| {
            let get = |cx, cy| child.get(&(cx, cy));
            let refs = [
                get(2 * px, 2 * py),
                get(2 * px + 1, 2 * py),
                get(2 * px, 2 * py + 1),
                get(2 * px + 1, 2 * py + 1),
            ];
            ((px, py), downsample_children(refs))
        })
        .collect()
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .context("usage: bake_pop <out.pmtiles> <field> <shp>...")?;
    let field = args
        .next()
        .context("missing <field> (e.g. POP20, or - for GeoTIFF)")?;
    let inputs: Vec<String> = args.collect();
    anyhow::ensure!(!inputs.is_empty(), "no input files given");

    // 1. Fill one shared z10 accumulator from every input (dispatch by extension).
    let mut acc = GridAccumulator::new();
    for input in &inputs {
        let ext = Path::new(input)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if ext == "tif" || ext == "tiff" {
            accumulate_geotiff_into(Path::new(input), WORLD_BBOX, &mut acc)
                .map_err(|e| anyhow::anyhow!("reading GeoTIFF {input}: {e}"))?;
        } else {
            let polys = read_valued_polygons(Path::new(input), &field)
                .with_context(|| format!("reading {input}"))?;
            for (geom, value) in &polys {
                rasterize_geometry(geom, *value, &mut acc);
            }
        }
        eprintln!(
            "{input}: running total {:.0} people, {} tiles",
            acc.total(),
            acc.tiles_touched()
        );
    }
    anyhow::ensure!(!acc.is_empty(), "no population rasterized");
    let total = acc.total();

    // 2. Build the pyramid: z10 from the accumulator, then average up to z0.
    let mut levels: HashMap<u8, Grids> = HashMap::new();
    levels.insert(EDIT_ZOOM, acc.into_grids());
    for z in (0..EDIT_ZOOM).rev() {
        let parent = overview(&levels[&(z + 1)]);
        levels.insert(z, parent);
    }

    // 3. Encode + write, tiles in ascending tile-id order (clustered).
    let mut coords: Vec<(u8, u32, u32)> = levels
        .iter()
        .flat_map(|(&z, g)| g.keys().map(move |&(x, y)| (z, x, y)))
        .collect();
    coords.sort_by_key(|&(z, x, y)| {
        TileCoord::new(z, x, y).map_or(u64::MAX, |c| u64::from(TileId::from(c)))
    });

    let file = std::fs::File::create(&out).with_context(|| format!("creating {out}"))?;
    let mut writer = PmTilesWriter::new(TileType::Png)
        .tile_compression(Compression::None)
        .internal_compression(Compression::Gzip)
        .min_zoom(0)
        .max_zoom(EDIT_ZOOM)
        .bounds(BOUNDS.0, BOUNDS.1, BOUNDS.2, BOUNDS.3)
        .create(file)?;

    let mut written = 0u64;
    for (z, x, y) in coords {
        let Some(values) = levels.get(&z).and_then(|g| g.get(&(x, y))) else {
            continue;
        };
        let coord = TileCoord::new(z, x, y).context("invalid tile coord")?;
        writer.add_tile(coord, &encode(values)?)?;
        written += 1;
    }
    writer.finalize()?;

    let size = std::fs::metadata(&out).map_or(0, |m| m.len());
    eprintln!(
        "wrote {out}: {written} tiles, {total:.0} people, {:.1} MB",
        size as f64 / 1e6
    );
    Ok(())
}
