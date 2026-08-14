//! `GeoTIFF` import front-end (gather/scatter onto the z10 grid).
//!
//! Reads a georeferenced raster (`WorldPop`, GHS-POP, GPW, Meta HRSL — anything
//! in geographic lon/lat, EPSG:4326) and scatters each source pixel's value into
//! a [`GridAccumulator`]: map the pixel's centre to lon/lat via the file's affine
//! geo-transform, then to a z10 global pixel, and add the value there. Scatter
//! (rather than per-target sampling) conserves totals across any resolution ratio
//! and needs only the forward transform. Units are reconciled later by
//! value-space matching, so a count raster and a density raster both work.
//!
//! Only geographic (lon/lat) rasters are supported in this pass; a projected file
//! (e.g. GHS-POP's Mollweide variant) is rejected with a message pointing at the
//! WGS84 version.

use std::io::{Read, Seek};

use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

use crate::pop_edit::lonlat_to_global_px;
use crate::pop_import::GridAccumulator;

/// `GeoTIFF` `GeoKey` ids we read from the `GeoKeyDirectoryTag`.
const GT_MODEL_TYPE_KEY: u16 = 1024;
const GEOGRAPHIC_TYPE_KEY: u16 = 2048;
const MODEL_TYPE_GEOGRAPHIC: u16 = 2;

/// An affine map from pixel `(col, row)` to model coordinates `(x, y)`:
/// `x = a·col + b·row + c`, `y = d·col + e·row + f`. For a geographic raster the
/// model coordinates are `(lon, lat)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoTransform {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl GeoTransform {
    /// From `ModelPixelScaleTag` `(sx, sy, sz)` + `ModelTiepointTag`
    /// `(i, j, k, x, y, z)`: a north-up transform where `y` decreases with `row`.
    #[must_use]
    pub fn from_pixel_scale(scale: [f64; 3], tiepoint: [f64; 6]) -> Self {
        let (sx, sy) = (scale[0], scale[1]);
        let (i, j, x0, y0) = (tiepoint[0], tiepoint[1], tiepoint[3], tiepoint[4]);
        Self {
            a: sx,
            b: 0.0,
            c: x0 - i * sx,
            d: 0.0,
            e: -sy,
            f: y0 + j * sy,
        }
    }

    /// From a `ModelTransformationTag` 4×4 row-major matrix (the affine 2-D part).
    #[must_use]
    pub fn from_matrix(m: [f64; 16]) -> Self {
        Self {
            a: m[0],
            b: m[1],
            c: m[3],
            d: m[4],
            e: m[5],
            f: m[7],
        }
    }

    /// Model coordinates at pixel `(col, row)`.
    #[must_use]
    pub fn pixel_to_model(&self, col: f64, row: f64) -> (f64, f64) {
        (
            self.a * col + self.b * row + self.c,
            self.d * col + self.e * row + self.f,
        )
    }

    /// Pixel `(col, row)` for model coordinates `(x, y)` — inverse of the affine.
    /// Returns `None` for a degenerate (non-invertible) transform.
    #[must_use]
    pub fn model_to_pixel(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let det = self.a * self.e - self.b * self.d;
        if det.abs() < f64::EPSILON {
            return None;
        }
        let (px, py) = (x - self.c, y - self.f);
        Some((
            (self.e * px - self.b * py) / det,
            (-self.d * px + self.a * py) / det,
        ))
    }
}

/// A finite, non-negative population sample; sentinels (nodata, negatives) drop.
fn keep(v: f64) -> bool {
    v.is_finite() && v > 0.0 && v < 1e12
}

fn sample_at(data: &DecodingResult, idx: usize) -> Option<f64> {
    match data {
        DecodingResult::U8(v) => v.get(idx).map(|&x| f64::from(x)),
        DecodingResult::U16(v) => v.get(idx).map(|&x| f64::from(x)),
        DecodingResult::U32(v) => v.get(idx).map(|&x| f64::from(x)),
        DecodingResult::U64(v) => v.get(idx).map(|&x| x as f64),
        DecodingResult::F32(v) => v.get(idx).map(|&x| f64::from(x)),
        DecodingResult::F64(v) => v.get(idx).copied(),
        DecodingResult::I8(v) => v.get(idx).map(|&x| f64::from(x)),
        DecodingResult::I16(v) => v.get(idx).map(|&x| f64::from(x)),
        DecodingResult::I32(v) => v.get(idx).map(|&x| f64::from(x)),
        _ => None,
    }
}

/// Scatter one source pixel `(col, row)` carrying `value` into `acc`, mapping its
/// centre through `transform` (model = lon/lat). `carry` diffuses the sub-1
/// remainder across cells so fractional counts (`WorldPop` stores <1 person per
/// cell in sparse areas) accumulate into whole people instead of rounding away.
pub fn scatter_pixel(
    transform: &GeoTransform,
    col: u32,
    row: u32,
    value: f64,
    acc: &mut GridAccumulator,
    carry: &mut f64,
) {
    if !keep(value) {
        return;
    }
    let (lon, lat) = transform.pixel_to_model(f64::from(col) + 0.5, f64::from(row) + 0.5);
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
        return;
    }
    *carry += value;
    let dep = carry.floor();
    *carry -= dep;
    if dep > 0.0 {
        let (gx, gy) = lonlat_to_global_px(lon, lat);
        acc.add(gx as u32, gy as u32, dep);
    }
}

/// Read the geo-transform from a decoder's tags, preferring a full transformation
/// matrix, else pixel-scale + tiepoint.
fn read_transform<R: Read + Seek>(dec: &mut Decoder<R>) -> Result<GeoTransform, String> {
    if let Ok(m) = dec.get_tag_f64_vec(Tag::ModelTransformationTag)
        && m.len() >= 16
    {
        let mut arr = [0.0; 16];
        arr.copy_from_slice(&m[..16]);
        return Ok(GeoTransform::from_matrix(arr));
    }
    let scale = dec
        .get_tag_f64_vec(Tag::ModelPixelScaleTag)
        .map_err(|_| "GeoTIFF has no ModelPixelScale or ModelTransformation tag".to_string())?;
    let tie = dec
        .get_tag_f64_vec(Tag::ModelTiepointTag)
        .map_err(|_| "GeoTIFF has no ModelTiepoint tag".to_string())?;
    if scale.len() < 3 || tie.len() < 6 {
        return Err("GeoTIFF geo-transform tags are malformed".into());
    }
    Ok(GeoTransform::from_pixel_scale(
        [scale[0], scale[1], scale[2]],
        [tie[0], tie[1], tie[2], tie[3], tie[4], tie[5]],
    ))
}

/// Whether the raster's `GeoKeys` declare a geographic (lon/lat) coordinate system.
/// A missing directory is treated as geographic (the common WGS84 case) and left
/// to the lon/lat range guard in [`scatter_pixel`].
fn is_geographic<R: Read + Seek>(dec: &mut Decoder<R>) -> bool {
    let Ok(keys) = dec.get_tag_u16_vec(Tag::GeoKeyDirectoryTag) else {
        return true;
    };
    if keys.len() < 4 {
        return true;
    }
    let count = keys[3] as usize;
    let mut model_type = None;
    let mut has_geographic = false;
    for k in 0..count {
        let base = 4 + k * 4;
        let Some(entry) = keys.get(base..base + 4) else {
            break;
        };
        let (key_id, location, value) = (entry[0], entry[1], entry[3]);
        if location != 0 {
            continue; // value stored elsewhere; not needed for these keys
        }
        match key_id {
            GT_MODEL_TYPE_KEY => model_type = Some(value),
            GEOGRAPHIC_TYPE_KEY => has_geographic = true,
            _ => {}
        }
    }
    match model_type {
        Some(v) => v == MODEL_TYPE_GEOGRAPHIC,
        None => has_geographic,
    }
}

/// Whole-world bbox: pass to [`accumulate_geotiff_into`] to bake a raster's full
/// extent (the corners clamp to the raster's own bounds).
pub const WORLD_BBOX: (f64, f64, f64, f64) = (-180.0, -85.0, 180.0, 85.0);

/// Scatter a geographic `GeoTIFF`'s pixels within the lon/lat `bbox`
/// `(west, south, east, north)` into a new [`GridAccumulator`]. Reads only the
/// chunks overlapping the requested window.
pub fn accumulate_geotiff(
    path: &std::path::Path,
    bbox: (f64, f64, f64, f64),
) -> Result<GridAccumulator, String> {
    let mut acc = GridAccumulator::new();
    accumulate_geotiff_into(path, bbox, &mut acc)?;
    Ok(acc)
}

/// Scatter a geographic `GeoTIFF`'s pixels within `bbox` into an existing
/// accumulator, so several sources can be baked into one grid.
pub fn accumulate_geotiff_into(
    path: &std::path::Path,
    bbox: (f64, f64, f64, f64),
    acc: &mut GridAccumulator,
) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut dec = Decoder::new(std::io::BufReader::new(file)).map_err(|e| e.to_string())?;
    let (width, height) = dec.dimensions().map_err(|e| e.to_string())?;

    if !is_geographic(&mut dec) {
        return Err("This GeoTIFF is projected. Please use its WGS84 / EPSG:4326 version.".into());
    }
    let transform = read_transform(&mut dec)?;

    // Source pixel window covering the import bbox corners.
    let (w, s, e, n) = bbox;
    let corners = [(w, n), (e, n), (w, s), (e, s)];
    let (mut c0, mut r0, mut c1, mut r1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (x, y) in corners {
        let Some((col, row)) = transform.model_to_pixel(x, y) else {
            return Err("GeoTIFF has a degenerate geo-transform".into());
        };
        c0 = c0.min(col);
        r0 = r0.min(row);
        c1 = c1.max(col);
        r1 = r1.max(row);
    }
    let clampc = |v: f64| v.floor().clamp(0.0, f64::from(width - 1)) as u32;
    let clampr = |v: f64| v.floor().clamp(0.0, f64::from(height - 1)) as u32;
    let (c0, c1) = (clampc(c0), clampc(c1));
    let (r0, r1) = (clampr(r0), clampr(r1));

    let (cw, ch) = dec.chunk_dimensions();
    if cw == 0 || ch == 0 {
        return Err("GeoTIFF reports zero-sized chunks".into());
    }
    let chunks_across = width.div_ceil(cw);

    let mut carry = 0.0f64;
    for chunk_row in (r0 / ch)..=(r1 / ch) {
        for chunk_col in (c0 / cw)..=(c1 / cw) {
            let idx = chunk_row * chunks_across + chunk_col;
            let (dw, dh) = dec.chunk_data_dimensions(idx);
            let data = dec.read_chunk(idx).map_err(|e| e.to_string())?;
            for ly in 0..dh {
                let gy = chunk_row * ch + ly;
                if gy < r0 || gy > r1 {
                    continue;
                }
                for lx in 0..dw {
                    let gx = chunk_col * cw + lx;
                    if gx < c0 || gx > c1 {
                        continue;
                    }
                    if let Some(v) = sample_at(&data, (ly * dw + lx) as usize) {
                        scatter_pixel(&transform, gx, gy, v, acc, &mut carry);
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_scale_transform_maps_corners() {
        // A 0.1° grid with top-left tiepoint at (lon -10, lat 50), pixel (0,0).
        let t = GeoTransform::from_pixel_scale([0.1, 0.1, 0.0], [0.0, 0.0, 0.0, -10.0, 50.0, 0.0]);
        assert_eq!(t.pixel_to_model(0.0, 0.0), (-10.0, 50.0));
        // One pixel right and down: lon +0.1, lat -0.1.
        let (lon, lat) = t.pixel_to_model(1.0, 1.0);
        assert!((lon - -9.9).abs() < 1e-9 && (lat - 49.9).abs() < 1e-9);
    }

    #[test]
    fn model_to_pixel_inverts() {
        let t = GeoTransform::from_pixel_scale([0.05, 0.05, 0.0], [0.0, 0.0, 0.0, 20.0, -5.0, 0.0]);
        for &(col, row) in &[(0.0, 0.0), (10.0, 7.0), (100.5, 42.25)] {
            let (x, y) = t.pixel_to_model(col, row);
            let (c2, r2) = t.model_to_pixel(x, y).expect("invertible");
            assert!((col - c2).abs() < 1e-6 && (row - r2).abs() < 1e-6);
        }
    }

    #[test]
    fn matrix_transform_reads_affine_part() {
        #[rustfmt::skip]
        let m = [
            0.2, 0.0, 0.0, -30.0,
            0.0, -0.2, 0.0, 60.0,
            0.0, 0.0, 0.0, 0.0,
            0.0, 0.0, 0.0, 1.0,
        ];
        let t = GeoTransform::from_matrix(m);
        assert_eq!(t.pixel_to_model(0.0, 0.0), (-30.0, 60.0));
        assert_eq!(t.pixel_to_model(1.0, 0.0), (-29.8, 60.0));
    }

    #[test]
    fn scatter_deposits_into_z10_and_drops_nodata() {
        let t = GeoTransform::from_pixel_scale([0.1, 0.1, 0.0], [0.0, 0.0, 0.0, 10.0, 1.0, 0.0]);
        let mut acc = GridAccumulator::new();
        let mut carry = 0.0;
        scatter_pixel(&t, 0, 0, 250.0, &mut acc, &mut carry);
        scatter_pixel(&t, 1, 1, -99999.0, &mut acc, &mut carry); // nodata sentinel dropped
        scatter_pixel(&t, 2, 2, 0.0, &mut acc, &mut carry); // zero dropped
        assert_eq!(acc.covered_pixels(), 1);
        assert!((acc.mean() - 250.0).abs() < 1e-9);
    }

    #[test]
    fn scatter_ignores_out_of_range_lonlat() {
        // A transform that pushes a pixel outside valid lon/lat is skipped.
        let t = GeoTransform::from_pixel_scale([1.0, 1.0, 0.0], [0.0, 0.0, 0.0, 200.0, 5.0, 0.0]);
        let mut acc = GridAccumulator::new();
        let mut carry = 0.0;
        scatter_pixel(&t, 0, 0, 100.0, &mut acc, &mut carry); // lon 200.5 → out of range
        assert!(acc.is_empty());
    }

    #[test]
    fn scatter_conserves_fractional_counts() {
        // WorldPop stores <1 person per cell in sparse areas; naive per-cell
        // rounding would drop them all. Error diffusion must keep (about) the sum.
        let t = GeoTransform::from_pixel_scale([0.01, 0.01, 0.0], [0.0, 0.0, 0.0, 10.0, 1.0, 0.0]);
        let mut acc = GridAccumulator::new();
        let mut carry = 0.0;
        for i in 0..1000 {
            scatter_pixel(&t, i % 40, i / 40, 0.3, &mut acc, &mut carry); // 1000 * 0.3 = 300
        }
        assert!((acc.total() - 300.0).abs() <= 1.0, "lost fractional people: {}", acc.total());
    }
}
