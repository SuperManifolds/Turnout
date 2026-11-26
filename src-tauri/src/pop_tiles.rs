//! Reads NIMBY Rails' population map (`pop400.pmtiles`) and turns each tile into
//! a colorized [`Pixmap`] the tile server can composite over the base map.
//!
//! The archive stores 16-bit grayscale density (see the `pop400-pmtiles-format`
//! notes); this module memory-maps it, decodes a tile, runs each density through
//! [`turnout_core::pop_color`], and premultiplies for `tiny-skia`. Tiles above
//! the archive's max zoom are overzoomed from the deepest stored ancestor so the
//! overlay stays visible at city detail.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use pmtiles::{AsyncPmTilesReader, MmapBackend, PmtResult, TileCoord};
use tiny_skia::Pixmap;

use turnout_core::pop_edit::{composite_tile, PopLayers, TileOp, EDIT_ZOOM, TILE_PX};
use turnout_core::pop_color;

use crate::server_core::UnpoisonExt;

/// Memory-mapped reader over `pop400.pmtiles`. Cheap to clone-share (`Arc`), so
/// the tile server keeps one per file path.
pub type PopReader = AsyncPmTilesReader<MmapBackend, pmtiles::NoCache>;

/// Open a population archive with the memory-mapped backend.
pub async fn open(path: &str) -> PmtResult<PopReader> {
    AsyncPmTilesReader::new_with_path(path).await
}

/// Colorized tile for `(z, x, y)` at `size`×`size` px, or `None` if the tile
/// (or its overzoom ancestor) is absent. Live `edits` are overlaid on the base
/// density. Output is premultiplied RGBA, matching `tiny-skia` compositing.
pub async fn pop_pixmap(
    reader: &PopReader,
    z: u8,
    x: u32,
    y: u32,
    size: u32,
    edits: &RwLock<PopLayers>,
    source_readers: &HashMap<u32, Arc<PopReader>>,
) -> Option<Pixmap> {
    let max_zoom = reader.get_header().max_zoom;

    // Below max zoom: read the tile directly and use its whole extent. Above:
    // read the deepest ancestor and sample the sub-region this tile covers.
    let dz = z.saturating_sub(max_zoom);
    let src_z = z.min(max_zoom);
    let (src_x, src_y) = (x >> dz, y >> dz);
    let span = 1u32 << dz; // ancestor subdivisions per axis
    let (sub_x, sub_y) = (x & (span - 1), y & (span - 1));

    let coord = TileCoord::new(src_z, src_x, src_y).ok()?;
    let bytes = reader.get_tile(coord).await.ok()??;

    // Composite the layer stack. Paint/import edits only exist at EDIT_ZOOM, but
    // base + file-backed source layers (which have their own overviews) composite
    // at every zoom. `None` means the base tile is used unchanged (fast path).
    let ops: Option<Vec<TileOp>> =
        edits.read().unpoison().tile_ops(src_x, src_y, src_z == EDIT_ZOOM);

    // Read each visible source layer's tile at the same coord (absent → skipped).
    let mut source_png: Vec<(u32, Vec<u8>)> = Vec::new();
    if let Some(ops) = &ops {
        for op in ops {
            if let TileOp::Source { id, .. } = op
                && let Some(sr) = source_readers.get(id)
                && let Ok(Some(b)) = sr.get_tile(coord).await
            {
                source_png.push((*id, b.to_vec()));
            }
        }
    }

    tokio::task::spawn_blocking(move || {
        colorize_region(&bytes, size, span, sub_x, sub_y, ops.as_deref(), &source_png)
    })
    .await
    .ok()?
}

/// Decode the base z10 tile at `(tile_x, tile_y)` to raw density values, as
/// `(width, values)`. An absent tile (ocean / unpopulated) reads as all-zero, so
/// the brush can add population where there was none.
pub async fn base_values(reader: &PopReader, tile_x: u32, tile_y: u32) -> (u32, Vec<u16>) {
    let empty = || (TILE_PX, vec![0u16; (TILE_PX * TILE_PX) as usize]);
    let Ok(coord) = TileCoord::new(EDIT_ZOOM, tile_x, tile_y) else { return empty() };
    let Ok(Some(bytes)) = reader.get_tile(coord).await else { return empty() };
    tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&bytes).ok()?.into_luma16();
        Some((img.width(), img.into_raw()))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_else(empty)
}

/// Decode a 16-bit grayscale PNG tile, composite the layer-stack `ops` over it
/// (when present), and colorize the `1/span` sub-region at `(sub_x, sub_y)` into a
/// premultiplied-RGBA [`Pixmap`] of `size`×`size`.
fn colorize_region(
    bytes: &[u8],
    size: u32,
    span: u32,
    sub_x: u32,
    sub_y: u32,
    ops: Option<&[TileOp]>,
    source_png: &[(u32, Vec<u8>)],
) -> Option<Pixmap> {
    let img = image::load_from_memory(bytes).ok()?.into_luma16();
    let (w, h) = (img.width(), img.height());
    let mut values = img.into_raw();
    // Compositing is defined on the full tile grid; only apply it when the decoded
    // tile matches (`TILE_PX` square), otherwise render the raw base.
    if let Some(ops) = ops.filter(|_| w == TILE_PX && h == TILE_PX) {
        let sources: HashMap<u32, Vec<u16>> = source_png
            .iter()
            .filter_map(|(id, png)| {
                let g = image::load_from_memory(png).ok()?.into_luma16().into_raw();
                (g.len() == values.len()).then_some((*id, g))
            })
            .collect();
        values = composite_tile(&values, ops, &sources);
    }

    // Source origin and extent (in source px) of the sub-region to sample.
    let region_w = f64::from(w) / f64::from(span);
    let region_h = f64::from(h) / f64::from(span);
    let origin_x = f64::from(sub_x) * region_w;
    let origin_y = f64::from(sub_y) * region_h;

    let mut buf = vec![0u8; (size * size * 4) as usize];
    for oy in 0..size {
        let sy = (origin_y + (f64::from(oy) + 0.5) * region_h / f64::from(size)) as u32;
        let sy = sy.min(h - 1);
        for ox in 0..size {
            let sx = (origin_x + (f64::from(ox) + 0.5) * region_w / f64::from(size)) as u32;
            let sx = sx.min(w - 1);
            let value = values[(sy * w + sx) as usize];
            let [r, g, b, a] = pop_color::color(value);
            let i = ((oy * size + ox) * 4) as usize;
            // Premultiply: tiny-skia's Pixmap buffer is premultiplied RGBA.
            let pm = |c: u8| ((u16::from(c) * u16::from(a)) / 255) as u8;
            buf[i] = pm(r);
            buf[i + 1] = pm(g);
            buf[i + 2] = pm(b);
            buf[i + 3] = a;
        }
    }
    Pixmap::from_vec(buf, tiny_skia::IntSize::from_wh(size, size)?)
}
