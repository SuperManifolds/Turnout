//! Phase C: write population edits back into a `pop400.pmtiles` archive.
//!
//! Flattens the layer stack onto the base density at z10, regenerates the
//! overview pyramid up to z0 by averaging (so lower zooms stay consistent), and
//! rewrites the whole archive — copying unedited tiles byte-for-byte and
//! substituting the changed ones. Tiles are uncompressed 16-bit grayscale PNG,
//! matching the original.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use pmtiles::{AsyncPmTilesReader, Compression, MmapBackend, TileCoord, TileType};
use turnout_core::pop_edit::{downsample_children, PopLayers, EDIT_ZOOM, TILE_PX};

/// World bounds of `pop400` (from the format probe): full longitude, −60°…+80°.
const BOUNDS: (f64, f64, f64, f64) = (-180.0, -60.0, 180.0, 80.0);

type Coord = (u8, u32, u32);

/// Decode a 16-bit grayscale PNG tile to a `TILE_PX`×`TILE_PX` value grid.
fn decode(bytes: &[u8]) -> Option<Vec<u16>> {
    Some(image::load_from_memory(bytes).ok()?.into_luma16().into_raw())
}

/// Encode a `TILE_PX`×`TILE_PX` value grid as a 16-bit grayscale PNG.
fn encode(values: Vec<u16>) -> Option<Vec<u8>> {
    let img = image::ImageBuffer::<image::Luma<u16>, _>::from_raw(TILE_PX, TILE_PX, values)?;
    let mut bytes = Vec::new();
    image::DynamicImage::ImageLuma16(img)
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .ok()?;
    Some(bytes)
}

/// Base density values for a tile, or all-zero if absent.
async fn base_values(reader: &AsyncPmTilesReader<MmapBackend>, z: u8, x: u32, y: u32) -> Vec<u16> {
    let zeros = || vec![0u16; (TILE_PX * TILE_PX) as usize];
    let Ok(coord) = TileCoord::new(z, x, y) else { return zeros() };
    match reader.get_tile(coord).await {
        Ok(Some(bytes)) => decode(&bytes).unwrap_or_else(zeros),
        _ => zeros(),
    }
}

/// Regenerate every changed tile: flatten edits onto z10, then average up each
/// overview zoom. Returns the new PNG bytes keyed by `(z, x, y)`.
async fn regenerate(
    reader: &Arc<AsyncPmTilesReader<MmapBackend>>,
    layers: &PopLayers,
    max_zoom: u8,
) -> HashMap<Coord, Vec<u8>> {
    let r = reader.as_ref();
    let mut new_bytes: HashMap<Coord, Vec<u8>> = HashMap::new();
    let mut values: HashMap<Coord, Vec<u16>> = HashMap::new();

    // z10: composite the layer stack over each base tile. A hidden base changes
    // every tile, so every existing z10 tile joins the dirty set.
    let mut dirty: HashSet<(u32, u32)> = layers.edited_tiles();
    if layers.base_hidden() {
        dirty.extend(z10_base_tiles(reader).await);
    }
    // Write-back bakes paint/import edits; file-backed source layers are view-only
    // for now, so no source tiles are supplied here.
    let no_sources = HashMap::new();
    for &(tx, ty) in &dirty {
        let base = base_values(r, EDIT_ZOOM, tx, ty).await;
        let v = layers.apply_tile(tx, ty, &base, &no_sources);
        if let Some(png) = encode(v.clone()) {
            new_bytes.insert((EDIT_ZOOM, tx, ty), png);
        }
        values.insert((EDIT_ZOOM, tx, ty), v);
    }

    // Overviews z(max-1)..=0: each dirty parent averages its four children.
    for z in (0..max_zoom).rev() {
        let parents: HashSet<(u32, u32)> = dirty.iter().map(|&(x, y)| (x / 2, y / 2)).collect();
        let child_z = z + 1;
        for &(px, py) in &parents {
            let mut child_values: [Option<Vec<u16>>; 4] = [None, None, None, None];
            for (i, (cx, cy)) in
                [(2 * px, 2 * py), (2 * px + 1, 2 * py), (2 * px, 2 * py + 1), (2 * px + 1, 2 * py + 1)]
                    .into_iter()
                    .enumerate()
            {
                let v = match values.get(&(child_z, cx, cy)) {
                    Some(v) => v.clone(),
                    None => base_values(r, child_z, cx, cy).await,
                };
                child_values[i] = Some(v);
            }
            let refs = [
                child_values[0].as_ref(),
                child_values[1].as_ref(),
                child_values[2].as_ref(),
                child_values[3].as_ref(),
            ];
            let parent = downsample_children(refs);
            if let Some(png) = encode(parent.clone()) {
                new_bytes.insert((z, px, py), png);
            }
            values.insert((z, px, py), parent);
        }
        dirty = parents;
    }

    new_bytes
}

/// Every z10 tile present in the base archive — the dirty set when the base is
/// hidden, since then every tile composites to something other than the base.
async fn z10_base_tiles(reader: &Arc<AsyncPmTilesReader<MmapBackend>>) -> HashSet<(u32, u32)> {
    let mut tiles = HashSet::new();
    let mut stream = Arc::clone(reader).entries();
    while let Some(Ok(entry)) = stream.next().await {
        for tid in entry.iter_coords() {
            let coord = TileCoord::from(tid);
            if coord.z() == EDIT_ZOOM {
                tiles.insert((coord.x(), coord.y()));
            }
        }
    }
    tiles
}

type WriteError = Box<dyn std::error::Error + Send + Sync>;

/// Apply `layers` to the archive at `base_path`, writing a full edited copy to
/// `out_path`. Returns the number of tiles written.
pub async fn apply_edits(base_path: &Path, out_path: &Path, layers: &PopLayers) -> Result<u64, WriteError> {
    let reader = Arc::new(AsyncPmTilesReader::new_with_path(base_path).await?);
    let max_zoom = reader.get_header().max_zoom;
    let new_bytes = regenerate(&reader, layers, max_zoom).await;

    let file = std::fs::File::create(out_path)?;
    let mut writer = pmtiles::PmTilesWriter::new(TileType::Png)
        .tile_compression(Compression::None)
        .internal_compression(Compression::None)
        .min_zoom(0)
        .max_zoom(max_zoom)
        .bounds(BOUNDS.0, BOUNDS.1, BOUNDS.2, BOUNDS.3)
        .create(file)?;

    let mut written: HashSet<Coord> = HashSet::new();
    let mut count = 0u64;

    // Copy every base tile, substituting changed ones.
    let mut stream = Arc::clone(&reader).entries();
    while let Some(entry) = stream.next().await {
        let entry = entry?;
        for tid in entry.iter_coords() {
            let coord = TileCoord::from(tid);
            let key = (coord.z(), coord.x(), coord.y());
            let bytes = match new_bytes.get(&key) {
                Some(b) => b.clone(),
                None => match reader.get_tile(coord).await? {
                    Some(b) => b.to_vec(),
                    None => continue,
                },
            };
            writer.add_tile(coord, &bytes)?;
            written.insert(key);
            count += 1;
        }
    }

    // Add edited tiles that did not exist in the base (e.g. painted over water).
    for (&(z, x, y), bytes) in &new_bytes {
        if written.contains(&(z, x, y)) {
            continue;
        }
        if let Ok(coord) = TileCoord::new(z, x, y) {
            writer.add_tile(coord, bytes)?;
            count += 1;
        }
    }

    writer.finalize()?;
    Ok(count)
}
