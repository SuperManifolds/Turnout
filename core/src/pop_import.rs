//! Shared backend for importing external population data as an [`Import`] layer.
//!
//! Every source — a `GeoTIFF` density raster (`WorldPop`, GHS-POP, …) or vector
//! polygons with a count attribute (US Census) — funnels through one path:
//!
//! 1. A front-end fills a [`GridAccumulator`] with per-z10-pixel values. A
//!    *gather* front-end (density raster) samples the source once per target
//!    pixel; a *scatter* front-end (counts) distributes each source feature's
//!    people across the pixels it covers, and the accumulator sums them. Because
//!    the base stores population *per z10 pixel-cell*, accumulated people-per-
//!    pixel is already in the base's unit — up to the game's unknown calibration
//!    constant, which value-space matching absorbs.
//! 2. The caller value-space matches the accumulator to the base over the overlap
//!    (see [`match_scale`]) — the base samples come from the archive, so that
//!    step lives with the reader — and bakes the resulting scale into
//!    [`GridAccumulator::into_tiles`], yielding the layer's [`TileRaster`]s.
//!
//! [`Import`]: crate::pop_edit
//! [`TileRaster`]: crate::pop_edit::TileRaster

use std::collections::HashMap;
use std::f64::consts::PI;

use crate::kml::Geometry;
use crate::pop_edit::{lonlat_to_global_px, TileRaster, AXIS_PX, TILE_PX};

/// Cap on the pixel bbox a single polygon may scan. Census blocks/tracts are far
/// smaller; a polygon larger than this (a whole state) falls back to depositing
/// its people at its centroid so import cost stays bounded.
const MAX_POLY_PIXELS: u64 = 4_000_000;

/// Inverse of [`crate::pop_edit::lonlat_to_global_px`]: a fractional z10 global
/// pixel back to lon/lat, so a gather front-end can sample its source at the
/// ground point each target pixel covers.
#[must_use]
pub fn global_px_to_lonlat(gx: f64, gy: f64) -> (f64, f64) {
    let axis = f64::from(AXIS_PX);
    let lon = gx / axis * 360.0 - 180.0;
    let lat = (PI * (1.0 - 2.0 * gy / axis)).sinh().atan().to_degrees();
    (lon, lat)
}

/// A dense population grid for one z10 tile: `u16` people per pixel plus a
/// coverage bitmask. Population per z10 cell is far below `u16::MAX`.
struct TileAccum {
    values: Vec<u16>,
    mask: Vec<u64>,
}

impl Default for TileAccum {
    fn default() -> Self {
        let n = (TILE_PX * TILE_PX) as usize;
        Self {
            values: vec![0; n],
            mask: vec![0u64; n.div_ceil(64)],
        }
    }
}

impl TileAccum {
    fn mark_and_add(&mut self, idx: usize, add: u16, covered: &mut u64) {
        let (word, bit) = (idx / 64, 1u64 << (idx % 64));
        if self.mask[word] & bit == 0 {
            self.mask[word] |= bit;
            *covered += 1;
        }
        self.values[idx] = self.values[idx].saturating_add(add);
    }
}

/// Accumulates source population onto the z10 grid, stored **per z10 tile** so
/// memory is bounded by the tiles actually touched. A global per-pixel map would
/// be ~10^9 entries for a country-sized import; this holds only ~356 KB per
/// touched tile.
#[derive(Default)]
pub struct GridAccumulator {
    tiles: HashMap<(u32, u32), TileAccum>,
    covered: u64,
}

impl GridAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `value` people to global z10 pixel `(gx, gy)` (rounded, saturating).
    pub fn add(&mut self, gx: u32, gy: u32, value: f64) {
        if gx >= AXIS_PX || gy >= AXIS_PX || value <= 0.0 {
            return;
        }
        let idx = ((gy % TILE_PX) * TILE_PX + (gx % TILE_PX)) as usize;
        let add = value.round().clamp(0.0, f64::from(u16::MAX)) as u16;
        self.tiles
            .entry((gx / TILE_PX, gy / TILE_PX))
            .or_default()
            .mark_and_add(idx, add, &mut self.covered);
    }

    /// Deposit `per`-pixel population across the horizontal run `gx0..=gx1` on row
    /// `gy`, using error diffusion (via the caller's persistent `carry`) so a
    /// sub-1 `per` — a sparse rural county spread over many pixels — deposits whole
    /// people instead of rounding to zero, and the running total is conserved
    /// across all of a polygon's rows. Every pixel is marked covered even where it
    /// deposits zero, so a `Normal`-blend import replaces the base there. One
    /// hashmap lookup per tile-segment rather than per pixel.
    pub fn add_run(&mut self, gy: u32, gx0: u32, gx1: u32, per: f64, carry: &mut f64) {
        if gy >= AXIS_PX || gx1 < gx0 || per <= 0.0 {
            return;
        }
        let (gx1, py) = (gx1.min(AXIS_PX - 1), gy % TILE_PX);
        let mut gx = gx0;
        while gx <= gx1 {
            let tx = gx / TILE_PX;
            let seg_end = gx1.min((tx + 1) * TILE_PX - 1);
            let tile = self.tiles.entry((tx, gy / TILE_PX)).or_default();
            for x in gx..=seg_end {
                *carry += per;
                let dep = carry.floor();
                *carry -= dep;
                let idx = (py * TILE_PX + x % TILE_PX) as usize;
                tile.mark_and_add(idx, dep.min(f64::from(u16::MAX)) as u16, &mut self.covered);
            }
            gx = seg_end + 1;
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.covered == 0
    }

    /// Number of z10 pixels marked covered.
    #[must_use]
    pub fn covered_pixels(&self) -> u64 {
        self.covered
    }

    /// Number of z10 tiles holding data (its memory footprint / cap check).
    #[must_use]
    pub fn tiles_touched(&self) -> usize {
        self.tiles.len()
    }

    /// Total people across covered pixels.
    #[must_use]
    pub fn total(&self) -> f64 {
        self.tiles
            .values()
            .map(|t| t.values.iter().map(|&v| f64::from(v)).sum::<f64>())
            .sum()
    }

    /// Mean people over covered pixels (`0` if empty).
    #[must_use]
    pub fn mean(&self) -> f64 {
        if self.covered == 0 {
            0.0
        } else {
            self.total() / self.covered as f64
        }
    }

    /// Iterate covered pixels as `(gx, gy, value)`.
    pub fn iter(&self) -> impl Iterator<Item = (u32, u32, f64)> + '_ {
        self.tiles.iter().flat_map(|(&(tx, ty), tile)| {
            (0..(TILE_PX * TILE_PX)).filter_map(move |idx| {
                let i = idx as usize;
                (tile.mask[i / 64] & (1u64 << (i % 64)) != 0).then(|| {
                    (
                        tx * TILE_PX + idx % TILE_PX,
                        ty * TILE_PX + idx / TILE_PX,
                        f64::from(tile.values[i]),
                    )
                })
            })
        })
    }

    /// Consume into raw per-z10-tile people grids (`TILE_PX`×`TILE_PX`, row-major,
    /// uncovered pixels are `0`). For the offline bake, which encodes these tiles
    /// straight to PNG without value-space scaling.
    #[must_use]
    pub fn into_grids(self) -> HashMap<(u32, u32), Vec<u16>> {
        self.tiles
            .into_iter()
            .map(|(coord, tile)| (coord, tile.values))
            .collect()
    }

    /// Bake `scale` into every value and pack into per-z10-tile [`TileRaster`]s
    /// for an [`crate::pop_edit::PopLayers`] import layer. Covered pixels stay
    /// covered even at zero, so a `Normal` blend replaces them.
    #[must_use]
    pub fn into_tiles(self, scale: f64) -> HashMap<(u32, u32), TileRaster> {
        self.tiles
            .into_iter()
            .map(|(coord, tile)| {
                let mut raster = TileRaster::new();
                for idx in 0..(TILE_PX * TILE_PX) as usize {
                    if tile.mask[idx / 64] & (1u64 << (idx % 64)) != 0 {
                        let v = (f64::from(tile.values[idx]) * scale)
                            .round()
                            .clamp(0.0, f64::from(u16::MAX)) as u16;
                        raster.set(idx, v);
                    }
                }
                (coord, raster)
            })
            .collect()
    }
}

/// Rasterize a polygon (`outer` ring, `inner` holes, vertices in lon/lat),
/// distributing `value` people uniformly across the z10 pixels its area covers —
/// the areal-weighting census imports assume. Uses a scanline fill (crossings per
/// row, even-odd across all rings so holes subtract). A polygon smaller than one
/// pixel, or one too large to scan, deposits its people at a single
/// representative pixel so the total is conserved.
pub fn rasterize_polygon(
    outer: &[(f64, f64)],
    inner: &[Vec<(f64, f64)>],
    value: f64,
    acc: &mut GridAccumulator,
) {
    if outer.len() < 3 || value <= 0.0 {
        return;
    }
    let to_px = |ring: &[(f64, f64)]| -> Vec<(f64, f64)> {
        ring.iter()
            .map(|&(lon, lat)| lonlat_to_global_px(lon, lat))
            .collect()
    };
    let px_outer = to_px(outer);
    let px_inner: Vec<Vec<(f64, f64)>> = inner.iter().map(|r| to_px(r)).collect();

    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &(x, y) in &px_outer {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    let clamp = |v: f64| v.clamp(0.0, f64::from(AXIS_PX - 1));
    let (x0, y0) = (clamp(min_x.floor()) as u32, clamp(min_y.floor()) as u32);
    let (x1, y1) = (clamp(max_x.ceil()) as u32, clamp(max_y.ceil()) as u32);

    let centroid_fallback = |acc: &mut GridAccumulator| {
        let (cx, cy) = centroid(&px_outer);
        acc.add(cx as u32, cy as u32, value);
    };

    // A polygon larger than the cap (a huge western/Alaska county) is filled at a
    // coarser stride so the work stays bounded — but still distributed over its
    // area, never lumped into one pixel (which would saturate `u16` and drop a
    // populous county's millions).
    let span = u64::from(x1 - x0 + 1) * u64::from(y1 - y0 + 1);
    let stride = if span <= MAX_POLY_PIXELS {
        1
    } else {
        ((span as f64 / MAX_POLY_PIXELS as f64).sqrt().ceil() as u32).max(2)
    };

    let rings: Vec<&[(f64, f64)]> = std::iter::once(px_outer.as_slice())
        .chain(px_inner.iter().map(Vec::as_slice))
        .collect();

    // Pass 1: count the pixels we will actually deposit into (respecting stride).
    let mut count: u64 = 0;
    let mut gy = y0;
    while gy <= y1 {
        for (a, b) in row_spans(&rings, f64::from(gy) + 0.5, x0, x1) {
            count += u64::from((b - a) / stride + 1);
        }
        gy += stride;
    }
    if count == 0 {
        centroid_fallback(acc);
        return;
    }

    // Pass 2: deposit an equal per-pixel share, carrying the sub-pixel remainder
    // across the whole polygon so the total is conserved.
    let per = value / count as f64;
    let mut carry = 0.0f64;
    let mut gy = y0;
    while gy <= y1 {
        for (a, b) in row_spans(&rings, f64::from(gy) + 0.5, x0, x1) {
            if stride == 1 {
                acc.add_run(gy, a, b, per, &mut carry);
            } else {
                let mut gx = a;
                while gx <= b {
                    carry += per;
                    let dep = carry.floor();
                    carry -= dep;
                    acc.add(gx, gy, dep);
                    gx += stride;
                }
            }
        }
        gy += stride;
    }
}

/// Rasterize any polygonal [`Geometry`] (a `Polygon` or a `Multi` of them),
/// splitting `value` across a multi-polygon's parts by vertex count so the total
/// is conserved. Non-polygon geometries contribute nothing.
pub fn rasterize_geometry(geometry: &Geometry, value: f64, acc: &mut GridAccumulator) {
    match geometry {
        Geometry::Polygon { outer, inner } => rasterize_polygon(outer, inner, value, acc),
        Geometry::Multi(parts) => {
            let polys: Vec<&Geometry> = parts
                .iter()
                .filter(|g| matches!(g, Geometry::Polygon { .. }))
                .collect();
            let total_verts: usize = polys
                .iter()
                .map(|g| match g {
                    Geometry::Polygon { outer, .. } => outer.len(),
                    _ => 0,
                })
                .sum();
            if total_verts == 0 {
                return;
            }
            for g in polys {
                if let Geometry::Polygon { outer, inner } = g {
                    let share = value * outer.len() as f64 / total_verts as f64;
                    rasterize_polygon(outer, inner, share, acc);
                }
            }
        }
        _ => {}
    }
}

/// Covered pixel-column spans `(gx_start, gx_end)` on the scanline `yc`, clamped
/// to `[x0, x1]`. Even-odd across every ring's edges (outer + holes) so a hole's
/// crossings toggle coverage back off. `gx` is covered when its centre `gx + 0.5`
/// lies between a pair of sorted crossings.
fn row_spans(rings: &[&[(f64, f64)]], yc: f64, x0: u32, x1: u32) -> Vec<(u32, u32)> {
    let mut xs: Vec<f64> = Vec::new();
    for ring in rings {
        let n = ring.len();
        if n < 2 {
            continue;
        }
        let mut j = n - 1;
        for i in 0..n {
            let (xi, yi) = ring[i];
            let (xj, yj) = ring[j];
            if (yi > yc) != (yj > yc) {
                xs.push((xj - xi) * (yc - yi) / (yj - yi) + xi);
            }
            j = i;
        }
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let mut spans = Vec::new();
    let (lo, hi) = (f64::from(x0), f64::from(x1));
    for pair in xs.chunks_exact(2) {
        let start = (pair[0] - 0.5).ceil().max(lo);
        let end = (pair[1] - 0.5).floor().min(hi);
        if end >= start {
            spans.push((start as u32, end as u32));
        }
    }
    spans
}

/// Vertex-average centroid of a ring, in the same coordinate space.
fn centroid(ring: &[(f64, f64)]) -> (f64, f64) {
    let n = ring.len() as f64;
    let (sx, sy) = ring
        .iter()
        .fold((0.0, 0.0), |(sx, sy), &(x, y)| (sx + x, sy + y));
    (sx / n, sy / n)
}

/// Value-space match: the scale that maps the imported mean onto the base mean,
/// times a manual `user_scale`. Returns `user_scale` alone when either mean is
/// negligible (e.g. importing into unpopulated base, where there is nothing to
/// match against), so the caller keeps the source values at their own level.
#[must_use]
pub fn match_scale(base_mean: f64, import_mean: f64, user_scale: f64) -> f64 {
    const EPS: f64 = 1e-6;
    if base_mean <= EPS || import_mean <= EPS {
        return user_scale;
    }
    (base_mean / import_mean) * user_scale
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_px_inverts_lonlat_round_trip() {
        for &(lon, lat) in &[(0.0, 0.0), (3.4, 6.5), (-122.4, 37.8), (139.7, 35.7)] {
            let (gx, gy) = crate::pop_edit::lonlat_to_global_px(lon, lat);
            let (lon2, lat2) = global_px_to_lonlat(gx, gy);
            assert!((lon - lon2).abs() < 1e-6, "lon {lon} != {lon2}");
            assert!((lat - lat2).abs() < 1e-6, "lat {lat} != {lat2}");
        }
    }

    #[test]
    fn accumulator_sums_and_ignores_out_of_grid() {
        let mut acc = GridAccumulator::new();
        acc.add(10, 20, 100.0);
        acc.add(10, 20, 50.0); // same pixel sums
        acc.add(AXIS_PX, 0, 999.0); // out of grid, ignored
        acc.add(5, 5, 0.0); // zero, ignored
        assert_eq!(acc.covered_pixels(), 1);
        assert_eq!(acc.mean(), 150.0);
    }

    #[test]
    fn into_tiles_scales_and_places_pixels() {
        let mut acc = GridAccumulator::new();
        // A pixel two tiles in, at in-tile (3, 4).
        let (gx, gy) = (2 * TILE_PX + 3, 5 * TILE_PX + 4);
        acc.add(gx, gy, 200.0);
        let tiles = acc.into_tiles(0.5); // 200 * 0.5 = 100
        let raster = tiles.get(&(2, 5)).expect("tile present");
        let idx = (4 * TILE_PX + 3) as usize;
        assert_eq!(raster.get(idx), Some(100));
        // A pixel the source never touched is not covered.
        assert_eq!(raster.get(0), None);
    }

    /// A small lon/lat square around a point, `half` degrees to a side.
    fn square(lon: f64, lat: f64, half: f64) -> Vec<(f64, f64)> {
        vec![
            (lon - half, lat - half),
            (lon + half, lat - half),
            (lon + half, lat + half),
            (lon - half, lat + half),
            (lon - half, lat - half),
        ]
    }

    #[test]
    fn rasterize_conserves_total_over_covered_pixels() {
        let mut acc = GridAccumulator::new();
        // ~0.2° square near the equator spans many z10 pixels.
        rasterize_polygon(&square(10.0, 0.0, 0.1), &[], 100_000.0, &mut acc);
        assert!(acc.covered_pixels() > 1);
        // Error diffusion conserves the total to within the final sub-pixel carry.
        assert!(
            (acc.total() - 100_000.0).abs() <= 1.0,
            "total not conserved: {}",
            acc.total()
        );
    }

    #[test]
    fn low_density_polygon_keeps_people_instead_of_rounding_to_zero() {
        // A large area with few people: naive per-pixel rounding would drop them
        // all to zero; error diffusion must keep (about) all of them.
        let mut acc = GridAccumulator::new();
        rasterize_polygon(&square(10.0, 0.0, 0.1), &[], 300.0, &mut acc);
        assert!(
            (acc.total() - 300.0).abs() <= 1.0,
            "lost people: {}",
            acc.total()
        );
    }

    #[test]
    fn tiny_polygon_falls_back_to_one_pixel() {
        let mut acc = GridAccumulator::new();
        // A sub-pixel square (no pixel center inside) still deposits its people.
        rasterize_polygon(&square(10.0, 0.0, 1e-5), &[], 42.0, &mut acc);
        assert_eq!(acc.covered_pixels(), 1);
        assert!((acc.mean() - 42.0).abs() < 1e-6);
    }

    #[test]
    fn hole_excludes_interior_pixels() {
        let mut solid = GridAccumulator::new();
        rasterize_polygon(&square(10.0, 0.0, 0.2), &[], 100_000.0, &mut solid);
        let mut holed = GridAccumulator::new();
        rasterize_polygon(
            &square(10.0, 0.0, 0.2),
            &[square(10.0, 0.0, 0.1)],
            100_000.0,
            &mut holed,
        );
        assert!(holed.covered_pixels() < solid.covered_pixels());
        // Total is still conserved across the (fewer) covered pixels.
        assert!(
            (holed.total() - 100_000.0).abs() <= 1.0,
            "total: {}",
            holed.total()
        );
    }

    #[test]
    fn multi_polygon_splits_value_across_parts() {
        let geom = Geometry::Multi(vec![
            Geometry::Polygon {
                outer: square(10.0, 0.0, 0.1),
                inner: vec![],
            },
            Geometry::Polygon {
                outer: square(20.0, 0.0, 0.1),
                inner: vec![],
            },
        ]);
        let mut acc = GridAccumulator::new();
        rasterize_geometry(&geom, 500_000.0, &mut acc);
        assert!(
            (acc.total() - 500_000.0).abs() <= 2.0,
            "total: {}",
            acc.total()
        );
    }

    #[test]
    fn match_scale_maps_means_and_guards_zeros() {
        // Import mean 400 → base mean 100 means halve-ish (0.25), ×2 user = 0.5.
        assert!((match_scale(100.0, 400.0, 2.0) - 0.5).abs() < 1e-9);
        // Empty base (nothing to match): keep user scale alone.
        assert_eq!(match_scale(0.0, 400.0, 3.0), 3.0);
        assert_eq!(match_scale(100.0, 0.0, 3.0), 3.0);
    }
}
