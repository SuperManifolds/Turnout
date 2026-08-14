//! Non-destructive population edit model: a Photoshop-like stack of **layers**
//! composited over the base `pop400` density. The base itself is the bottom layer
//! and can be hidden like any other. Above it sit two kinds of edit layer:
//!
//! - **Paint** layers hold sparse signed per-pixel *deltas* (the brush / region
//!   tools) and always *add* to whatever is beneath them.
//! - **Import** layers hold a dense *absolute* density field with a coverage mask
//!   (external data — census, `WorldPop`, …) and combine via a per-layer [`Blend`]:
//!   `Add` deposits on top, `Normal` replaces the value where the layer covers.
//!
//! The effective density at a pixel is the stack composited bottom-to-top, then
//! clamped to `u16`. Because the base is never mutated and every layer is
//! independently toggleable, any contribution can be reverted exactly.
//!
//! Edits are stored at [`EDIT_ZOOM`] — the finest level, where each pixel is a
//! ground cell — so one source of truth drives the live preview and the
//! write-back that regenerates coarser overview zooms by averaging.
//!
//! Geometry matches the standard Web-Mercator XYZ tiling the archive uses (see
//! the `pop400-pmtiles-format` notes): 1024 tiles per axis at z10, 422 px each.

use std::collections::{HashMap, HashSet};
use std::f64::consts::PI;

/// Zoom level edits are stored at (the archive's max zoom).
pub const EDIT_ZOOM: u8 = 10;
/// Pixels per tile edge in `pop400` tiles.
pub const TILE_PX: u32 = 422;
/// Tiles per axis at [`EDIT_ZOOM`] (`2^EDIT_ZOOM`).
pub const TILES_PER_AXIS: u32 = 1 << EDIT_ZOOM;
/// Total pixels per axis of the global z10 grid.
pub const AXIS_PX: u32 = TILES_PER_AXIS * TILE_PX;

/// How a brush stroke changes density.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrushMode {
    /// Add to the active layer's delta at each covered pixel (with soft falloff).
    Add,
    /// Subtract from the active layer's delta at each covered pixel.
    Remove,
}

/// A single edited pixel: its z10 tile and in-tile coordinate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRef {
    pub tile_x: u32,
    pub tile_y: u32,
    pub px: u16,
    pub py: u16,
}

/// How an [`Import`](Content::Import) layer's absolute field combines with the
/// stack beneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Blend {
    /// Deposit the layer's value on top of what is beneath.
    Add,
    /// Replace the value where the layer covers (paint-through elsewhere).
    Normal,
}

/// What a layer contributes to the composite.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LayerKind {
    /// The immutable `pop400` base density (its pixels are injected by the
    /// renderer; this layer only carries a visibility flag).
    Base,
    /// Sparse signed deltas from the brush / region tools.
    Paint,
    /// A dense absolute density field imported from external data.
    Import,
    /// A file-backed density raster (a baked `PMTiles`); its pixels are injected
    /// by the renderer from that file, so nationwide data costs no memory.
    Source,
}

/// Serializable summary of one layer for the UI (top-of-stack first).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerInfo {
    pub id: u32,
    pub name: String,
    pub visible: bool,
    pub active: bool,
    pub kind: LayerKind,
    pub blend: Blend,
}

fn pack(px: u16, py: u16) -> u32 {
    (u32::from(px) << 16) | u32::from(py)
}

fn pixel_idx(px: u16, py: u16) -> usize {
    u32::from(py) as usize * TILE_PX as usize + u32::from(px) as usize
}

fn clamp_u16(v: i64) -> u16 {
    v.clamp(0, i64::from(u16::MAX)) as u16
}

/// A dense absolute density field for one z10 tile, with a coverage mask so a
/// partial import (a pixel the source did not cover) falls through to the layers
/// beneath rather than reading as zero.
#[derive(Clone)]
pub struct TileRaster {
    values: Vec<u16>,
    /// One bit per pixel, set when the source covered it.
    mask: Vec<u64>,
}

impl Default for TileRaster {
    fn default() -> Self {
        let n = (TILE_PX * TILE_PX) as usize;
        Self {
            values: vec![0; n],
            mask: vec![0u64; n.div_ceil(64)],
        }
    }
}

impl TileRaster {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark pixel `idx` as covered with density `v`.
    pub fn set(&mut self, idx: usize, v: u16) {
        self.values[idx] = v;
        self.mask[idx / 64] |= 1u64 << (idx % 64);
    }

    /// Density at pixel `idx`, or `None` where the source did not cover it.
    #[must_use]
    pub fn get(&self, idx: usize) -> Option<u16> {
        if self.mask[idx / 64] & (1u64 << (idx % 64)) == 0 {
            return None;
        }
        Some(self.values[idx])
    }
}

/// What a layer holds. `Base` and `Source` carry no data in core — their pixels
/// come from a decoded archive at composite time — while `Paint` and `Import` own
/// their edits.
#[derive(Clone)]
enum Content {
    Base,
    /// z10 tile → (packed in-tile pixel → signed delta).
    Paint(HashMap<(u32, u32), HashMap<u32, i32>>),
    /// z10 tile → dense absolute field.
    Import(HashMap<(u32, u32), TileRaster>),
    /// A file-backed raster: the renderer injects its tile values by layer id.
    Source,
}

#[derive(Clone)]
struct Layer {
    id: u32,
    name: String,
    visible: bool,
    blend: Blend,
    content: Content,
}

impl Layer {
    fn kind(&self) -> LayerKind {
        match self.content {
            Content::Base => LayerKind::Base,
            Content::Paint(_) => LayerKind::Paint,
            Content::Import(_) => LayerKind::Import,
            Content::Source => LayerKind::Source,
        }
    }
}

/// A stack of layers over the base density. Index `0` is the bottom of the stack
/// (the base), the last element is the top; edits target the active layer.
#[derive(Clone)]
pub struct PopLayers {
    layers: Vec<Layer>,
    active: u32,
    next_id: u32,
}

impl Default for PopLayers {
    fn default() -> Self {
        Self {
            layers: vec![
                Layer {
                    id: 1,
                    name: "Base population".to_string(),
                    visible: true,
                    blend: Blend::Normal,
                    content: Content::Base,
                },
                Layer {
                    id: 2,
                    name: "Layer 1".to_string(),
                    visible: true,
                    blend: Blend::Add,
                    content: Content::Paint(HashMap::new()),
                },
            ],
            active: 2,
            next_id: 3,
        }
    }
}

impl PopLayers {
    /// Layers top-of-stack first, each flagged with visibility, kind, blend, and
    /// whether it is the active (edited) layer.
    #[must_use]
    pub fn list(&self) -> Vec<LayerInfo> {
        self.layers
            .iter()
            .rev()
            .map(|l| LayerInfo {
                id: l.id,
                name: l.name.clone(),
                visible: l.visible,
                active: l.id == self.active,
                kind: l.kind(),
                blend: l.blend,
            })
            .collect()
    }

    /// True when the stack composites to exactly the base — no edits, imports, or
    /// source layers, and the base still visible — so there is nothing to write.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.base_hidden()
            && self.layers.iter().all(|l| match &l.content {
                Content::Base => true,
                Content::Paint(m) => m.values().all(HashMap::is_empty),
                Content::Import(m) => m.is_empty(),
                Content::Source => !l.visible,
            })
    }

    fn index_of(&self, id: u32) -> Option<usize> {
        self.layers.iter().position(|l| l.id == id)
    }

    /// The active layer's paint deltas, creating a new paint layer on top first
    /// if the active layer cannot hold deltas (base or import selected).
    fn active_paint_mut(&mut self) -> &mut HashMap<(u32, u32), HashMap<u32, i32>> {
        let active_is_paint = self
            .index_of(self.active)
            .is_some_and(|i| matches!(self.layers[i].content, Content::Paint(_)));
        if !active_is_paint {
            self.add_layer();
        }
        let idx = self.index_of(self.active).unwrap_or(self.layers.len() - 1);
        match &mut self.layers[idx].content {
            Content::Paint(m) => m,
            _ => unreachable!("add_layer always pushes a paint layer"),
        }
    }

    /// Add a new empty paint layer on top of the stack and make it active.
    pub fn add_layer(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.layers.push(Layer {
            id,
            name: format!("Layer {}", id - 1),
            visible: true,
            blend: Blend::Add,
            content: Content::Paint(HashMap::new()),
        });
        self.active = id;
        id
    }

    /// Add a file-backed source layer (its raster lives in a `PMTiles` the
    /// renderer supplies by id) on top of the stack, and make it active. Returns
    /// its id.
    pub fn add_source_layer(&mut self, name: String, blend: Blend) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.layers.push(Layer {
            id,
            name,
            visible: true,
            blend,
            content: Content::Source,
        });
        self.active = id;
        id
    }

    /// Ids of visible source layers, bottom-to-top, so the renderer can fetch each
    /// one's tile from its `PMTiles` before compositing.
    #[must_use]
    pub fn visible_source_ids(&self) -> Vec<u32> {
        self.layers
            .iter()
            .filter(|l| l.visible && matches!(l.content, Content::Source))
            .map(|l| l.id)
            .collect()
    }

    /// Add an import layer holding an absolute density field, on top of the stack,
    /// and make it active. Returns its id.
    pub fn add_import_layer(
        &mut self,
        name: String,
        blend: Blend,
        tiles: HashMap<(u32, u32), TileRaster>,
    ) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.layers.push(Layer {
            id,
            name,
            visible: true,
            blend,
            content: Content::Import(tiles),
        });
        self.active = id;
        id
    }

    /// Remove a layer. The base layer is never removed; the last edit layer is
    /// cleared rather than removed so the stack keeps at least a base + one layer.
    pub fn remove_layer(&mut self, id: u32) {
        let Some(idx) = self.index_of(id) else { return };
        if matches!(self.layers[idx].content, Content::Base) {
            return;
        }
        let edit_layers = self
            .layers
            .iter()
            .filter(|l| !matches!(l.content, Content::Base))
            .count();
        if edit_layers <= 1 {
            self.layers[idx].content = Content::Paint(HashMap::new());
            self.layers[idx].blend = Blend::Add;
            self.active = id;
            return;
        }
        self.layers.remove(idx);
        if self.active == id {
            self.active = self.layers.last().map_or(0, |l| l.id);
        }
    }

    pub fn rename_layer(&mut self, id: u32, name: String) {
        if let Some(idx) = self.index_of(id) {
            self.layers[idx].name = name;
        }
    }

    pub fn set_layer_visible(&mut self, id: u32, visible: bool) {
        if let Some(idx) = self.index_of(id) {
            self.layers[idx].visible = visible;
        }
    }

    pub fn set_layer_blend(&mut self, id: u32, blend: Blend) {
        if let Some(idx) = self.index_of(id) {
            self.layers[idx].blend = blend;
        }
    }

    pub fn set_active(&mut self, id: u32) {
        if self.index_of(id).is_some() {
            self.active = id;
        }
    }

    /// Move a layer up (toward the top) or down within the stack. The base layer
    /// stays pinned at the bottom.
    pub fn move_layer(&mut self, id: u32, up: bool) {
        let Some(idx) = self.index_of(id) else { return };
        if matches!(self.layers[idx].content, Content::Base) {
            return;
        }
        let swap = if up { idx + 1 } else { idx.wrapping_sub(1) };
        let swap_ok = if up {
            swap < self.layers.len()
        } else {
            idx > 0
        };
        // Never let an edit layer sink below the base at index 0.
        if swap_ok && !matches!(self.layers[swap].content, Content::Base) {
            self.layers.swap(idx, swap);
        }
    }

    pub fn clear_all(&mut self) {
        for l in &mut self.layers {
            match &mut l.content {
                Content::Paint(m) => m.clear(),
                Content::Import(m) => m.clear(),
                Content::Base => l.visible = true,
                Content::Source => {}
            }
        }
    }

    /// Add `delta` to the active paint layer at a pixel (the brush).
    pub fn add_delta(&mut self, tile_x: u32, tile_y: u32, px: u16, py: u16, delta: i32) {
        let entry = self
            .active_paint_mut()
            .entry((tile_x, tile_y))
            .or_default()
            .entry(pack(px, py))
            .or_insert(0);
        *entry = entry.saturating_add(delta);
    }

    /// Set the active paint layer's delta at a pixel to an exact value (region
    /// tools).
    pub fn set_delta(&mut self, tile_x: u32, tile_y: u32, px: u16, py: u16, delta: i32) {
        self.active_paint_mut()
            .entry((tile_x, tile_y))
            .or_default()
            .insert(pack(px, py), delta);
    }

    /// True when a base layer exists but is hidden — the whole map differs from
    /// the archive, so a write-back must regenerate every tile.
    #[must_use]
    pub fn base_hidden(&self) -> bool {
        self.layers
            .iter()
            .any(|l| matches!(l.content, Content::Base) && !l.visible)
    }

    /// Composite the visible stack at one pixel, given the decoded `base` value,
    /// optionally skipping the active layer (region tools set the active layer
    /// relative to what is beneath it).
    fn compose(&self, base: u16, tx: u32, ty: u32, px: u16, py: u16, skip_active: bool) -> i64 {
        let key = pack(px, py);
        let idx = pixel_idx(px, py);
        let mut acc: i64 = 0;
        for l in &self.layers {
            if !l.visible || (skip_active && l.id == self.active) {
                continue;
            }
            match &l.content {
                Content::Base => acc = i64::from(base),
                Content::Paint(m) => {
                    if let Some(d) = m.get(&(tx, ty)).and_then(|t| t.get(&key)) {
                        acc += i64::from(*d);
                    }
                }
                Content::Import(m) => {
                    if let Some(v) = m.get(&(tx, ty)).and_then(|t| t.get(idx)) {
                        match l.blend {
                            Blend::Add => acc += i64::from(v),
                            Blend::Normal => acc = i64::from(v),
                        }
                    }
                }
                // File-backed source layers are density references; the per-pixel
                // path (region readout/fill) does not read their archive.
                Content::Source => {}
            }
        }
        acc
    }

    /// Effective density at a pixel: the composited stack, clamped to `u16`.
    #[must_use]
    pub fn effective(&self, base: u16, tile_x: u32, tile_y: u32, px: u16, py: u16) -> u16 {
        clamp_u16(self.compose(base, tile_x, tile_y, px, py, false))
    }

    /// Effective density with the active layer excluded — the value the region
    /// tool must add its active-layer delta on top of to reach a target.
    #[must_use]
    pub fn effective_excluding_active(
        &self,
        base: u16,
        tile_x: u32,
        tile_y: u32,
        px: u16,
        py: u16,
    ) -> u16 {
        clamp_u16(self.compose(base, tile_x, tile_y, px, py, true))
    }

    /// Every z10 tile any visible edit (paint or import) layer touches, so the
    /// write-back knows which tiles and overview ancestors to regenerate. (Source
    /// layers cover unknown extents; the writer expands the dirty set for those.)
    #[must_use]
    pub fn edited_tiles(&self) -> HashSet<(u32, u32)> {
        let mut tiles = HashSet::new();
        for l in self.layers.iter().filter(|l| l.visible) {
            match &l.content {
                Content::Base | Content::Source => {}
                Content::Paint(m) => tiles.extend(m.keys().copied()),
                Content::Import(m) => tiles.extend(m.keys().copied()),
            }
        }
        tiles
    }

    /// The visible stack's contributions to one z10 tile, bottom-to-top, owning
    /// only the data overlapping this tile so a render thread can composite
    /// off-lock. `Source` ops carry the layer id; the caller injects that layer's
    /// decoded tile into [`composite_tile`]. When `include_edits` is false (a
    /// zoomed-out overview render), paint/import edits are omitted — they exist
    /// only at [`EDIT_ZOOM`] — leaving just base + source. Returns `None` for the
    /// fast path: base visible, no source, and nothing touches the tile.
    #[must_use]
    pub fn tile_ops(&self, tile_x: u32, tile_y: u32, include_edits: bool) -> Option<Vec<TileOp>> {
        let has_source = self
            .layers
            .iter()
            .any(|l| l.visible && matches!(l.content, Content::Source));
        let edits_touch = include_edits
            && self.layers.iter().any(|l| {
                l.visible
                    && match &l.content {
                        Content::Paint(m) => {
                            m.get(&(tile_x, tile_y)).is_some_and(|t| !t.is_empty())
                        }
                        Content::Import(m) => m.contains_key(&(tile_x, tile_y)),
                        _ => false,
                    }
            });
        if !edits_touch && !has_source && !self.base_hidden() {
            return None;
        }
        let ops = self
            .layers
            .iter()
            .filter(|l| l.visible)
            .filter_map(|l| match &l.content {
                Content::Base => Some(TileOp::Base),
                Content::Source => Some(TileOp::Source {
                    id: l.id,
                    blend: l.blend,
                }),
                Content::Paint(m) if include_edits => m
                    .get(&(tile_x, tile_y))
                    .filter(|t| !t.is_empty())
                    .map(|t| TileOp::Paint(t.clone())),
                Content::Import(m) if include_edits => {
                    m.get(&(tile_x, tile_y)).map(|r| TileOp::Import {
                        blend: l.blend,
                        raster: r.clone(),
                    })
                }
                _ => None,
            })
            .collect();
        Some(ops)
    }

    /// Composite the stack over a decoded base tile, returning the effective
    /// density grid (row-major, `TILE_PX`×`TILE_PX`). `sources` supplies each
    /// visible source layer's decoded tile by id. `base` is the decoded `pop400`
    /// tile; pixels past its end are treated as `0`.
    #[must_use]
    pub fn apply_tile(
        &self,
        tile_x: u32,
        tile_y: u32,
        base: &[u16],
        sources: &HashMap<u32, Vec<u16>>,
    ) -> Vec<u16> {
        match self.tile_ops(tile_x, tile_y, true) {
            None => {
                let n = (TILE_PX * TILE_PX) as usize;
                let mut out = vec![0u16; n];
                let k = base.len().min(n);
                out[..k].copy_from_slice(&base[..k]);
                out
            }
            Some(ops) => composite_tile(base, &ops, sources),
        }
    }
}

/// One visible layer's contribution to a single z10 tile, owning its data (see
/// [`PopLayers::tile_ops`]).
pub enum TileOp {
    Base,
    Paint(HashMap<u32, i32>),
    Import {
        blend: Blend,
        raster: TileRaster,
    },
    /// A file-backed source layer; its tile is supplied to [`composite_tile`] in
    /// the `sources` map keyed by this `id`.
    Source {
        id: u32,
        blend: Blend,
    },
}

/// Composite a decoded base tile through `ops` (bottom-to-top), returning the
/// effective density grid, row-major `TILE_PX`×`TILE_PX`. `sources` supplies each
/// `TileOp::Source` layer's decoded tile by id (absent → that layer contributes
/// nothing here). `base` shorter than the grid reads as `0` past its end.
#[must_use]
pub fn composite_tile(base: &[u16], ops: &[TileOp], sources: &HashMap<u32, Vec<u16>>) -> Vec<u16> {
    let w = TILE_PX as usize;
    let n = w * w;
    let mut out = vec![0u16; n];
    for (idx, slot) in out.iter_mut().enumerate() {
        let key = pack((idx % w) as u16, (idx / w) as u16);
        let mut acc: i64 = 0;
        for op in ops {
            match op {
                TileOp::Base => acc = i64::from(base.get(idx).copied().unwrap_or(0)),
                TileOp::Source { id, blend } => {
                    if let Some(v) = sources.get(id).and_then(|g| g.get(idx)).copied() {
                        match blend {
                            Blend::Add => acc += i64::from(v),
                            Blend::Normal => acc = i64::from(v),
                        }
                    }
                }
                TileOp::Paint(m) => {
                    if let Some(d) = m.get(&key) {
                        acc += i64::from(*d);
                    }
                }
                TileOp::Import { blend, raster } => {
                    if let Some(v) = raster.get(idx) {
                        match blend {
                            Blend::Add => acc += i64::from(v),
                            Blend::Normal => acc = i64::from(v),
                        }
                    }
                }
            }
        }
        *slot = clamp_u16(acc);
    }
    out
}

/// Convert lon/lat to fractional global-pixel coordinates in the z10 grid.
#[must_use]
pub fn lonlat_to_global_px(lon: f64, lat: f64) -> (f64, f64) {
    let axis = f64::from(AXIS_PX);
    let x = (lon + 180.0) / 360.0 * axis;
    let lat_rad = lat.to_radians();
    let y = (1.0 - lat_rad.tan().asinh() / PI) / 2.0 * axis;
    (x, y)
}

/// Inclusive global-pixel rectangle `(gx_min, gy_min, gx_max, gy_max)` covering a
/// lon/lat bbox, clamped to the grid. Used to sum or set population over a region.
#[must_use]
pub fn region_bounds(west: f64, south: f64, east: f64, north: f64) -> (u32, u32, u32, u32) {
    let (x0, _) = lonlat_to_global_px(west, 0.0);
    let (x1, _) = lonlat_to_global_px(east, 0.0);
    // North is the smaller pixel-y (top of the grid).
    let (_, y_top) = lonlat_to_global_px(0.0, north);
    let (_, y_bot) = lonlat_to_global_px(0.0, south);
    let clamp = |v: f64| v.clamp(0.0, f64::from(AXIS_PX - 1)) as u32;
    (
        clamp(x0.min(x1)),
        clamp(y_top.min(y_bot)),
        clamp(x0.max(x1)),
        clamp(y_top.max(y_bot)),
    )
}

/// Ground metres per z10 pixel at `lat` (Web-Mercator scale factor). Lets the UI
/// specify a brush radius in metres and have it cover a consistent real area.
#[must_use]
pub fn meters_per_pixel(lat: f64) -> f64 {
    const EQUATOR_M: f64 = 40_075_016.686;
    EQUATOR_M * lat.to_radians().cos() / f64::from(AXIS_PX)
}

/// Split a global-pixel coordinate into its z10 tile and in-tile pixel.
fn split(global: u32) -> (u32, u16) {
    (global / TILE_PX, (global % TILE_PX) as u16)
}

/// The pixels a circular brush covers, each with a `0.0..=1.0` falloff weight
/// (1 at the centre, 0 at the edge). `center` is lon/lat; `radius_m` is metres.
#[must_use]
pub fn brush_pixels(lon: f64, lat: f64, radius_m: f64) -> Vec<(PixelRef, f64)> {
    let (cx, cy) = lonlat_to_global_px(lon, lat);
    let radius_px = (radius_m / meters_per_pixel(lat)).max(0.5);
    let r = radius_px.ceil() as i64;
    let (cxi, cyi) = (cx as i64, cy as i64);

    let mut out = Vec::new();
    for gy in (cyi - r)..=(cyi + r) {
        if gy < 0 || gy >= i64::from(AXIS_PX) {
            continue;
        }
        for gx in (cxi - r)..=(cxi + r) {
            if gx < 0 || gx >= i64::from(AXIS_PX) {
                continue;
            }
            let dist = (((gx - cxi).pow(2) + (gy - cyi).pow(2)) as f64).sqrt();
            if dist > radius_px {
                continue;
            }
            let weight = 1.0 - dist / radius_px; // linear falloff
            let (tile_x, px) = split(gx as u32);
            let (tile_y, py) = split(gy as u32);
            out.push((
                PixelRef {
                    tile_x,
                    tile_y,
                    px,
                    py,
                },
                weight,
            ));
        }
    }
    out
}

/// Regenerate an overview (parent) tile from its four z10-style children by 2×2
/// box-averaging. Children are `[top_left, top_right, bottom_left, bottom_right]`,
/// each a `TILE_PX`×`TILE_PX` density grid; the parent is the same size and
/// covers 4× the area, so total population is conserved (see the format notes:
/// overviews are mean-pooled). A `None` child is treated as all-zero.
#[must_use]
pub fn downsample_children(children: [Option<&Vec<u16>>; 4]) -> Vec<u16> {
    let w = TILE_PX;
    let sample = |x: u32, y: u32| -> u32 {
        let idx = usize::from(x >= w) + 2 * usize::from(y >= w); // TL,TR,BL,BR
        children[idx].map_or(0, |c| {
            let (lx, ly) = (x % w, y % w);
            c.get((ly * w + lx) as usize).copied().map_or(0, u32::from)
        })
    };
    let mut parent = vec![0u16; (w * w) as usize];
    for py in 0..w {
        for px in 0..w {
            let (sx, sy) = (2 * px, 2 * py);
            let sum =
                sample(sx, sy) + sample(sx + 1, sy) + sample(sx, sy + 1) + sample(sx + 1, sy + 1);
            parent[(py * w + px) as usize] = (sum / 4) as u16;
        }
    }
    parent
}

/// Signed delta a brush touch contributes to the active layer, given falloff
/// `weight`, `strength`, and `mode`.
#[must_use]
pub fn brush_delta(weight: f64, strength: u32, mode: BrushMode) -> i32 {
    let mag = (f64::from(strength) * weight).round() as i32;
    match mode {
        BrushMode::Add => mag,
        BrushMode::Remove => -mag,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_base_plus_active_paint_layer() {
        let l = PopLayers::default();
        let list = l.list(); // top-of-stack first
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].kind, LayerKind::Paint);
        assert!(list[0].active && list[0].visible);
        assert_eq!(list[1].kind, LayerKind::Base);
        assert!(list[1].visible && !list[1].active);
        assert!(l.is_empty());
    }

    #[test]
    fn deltas_compose_over_base_and_clamp() {
        let mut l = PopLayers::default();
        l.add_delta(1, 2, 3, 4, 50);
        assert_eq!(l.effective(100, 1, 2, 3, 4), 150);
        // A second paint layer stacks additively.
        l.add_layer();
        l.add_delta(1, 2, 3, 4, 25);
        assert_eq!(l.effective(100, 1, 2, 3, 4), 175);
        // Hiding the top layer reverts exactly its contribution.
        let top = l.list()[0].id;
        l.set_layer_visible(top, false);
        assert_eq!(l.effective(100, 1, 2, 3, 4), 150);
        // A large negative composite clamps at zero.
        l.set_layer_visible(top, true);
        l.add_delta(1, 2, 3, 4, -1000);
        assert_eq!(l.effective(100, 1, 2, 3, 4), 0);
    }

    #[test]
    fn hiding_base_drops_it_from_the_composite() {
        let mut l = PopLayers::default();
        l.add_delta(0, 0, 1, 1, 40);
        assert_eq!(l.effective(100, 0, 0, 1, 1), 140);
        assert!(!l.base_hidden());
        let base = l.list()[1].id;
        l.set_layer_visible(base, false);
        // With the base hidden only the paint delta remains.
        assert_eq!(l.effective(100, 0, 0, 1, 1), 40);
        assert!(l.base_hidden());
        assert!(!l.is_empty());
    }

    #[test]
    fn apply_tile_composites_the_stack_over_base() {
        let mut l = PopLayers::default();
        l.add_delta(0, 0, 2, 1, 7); // idx = 1*TILE_PX + 2
        let base = vec![100u16; (TILE_PX * TILE_PX) as usize];
        let out = l.apply_tile(0, 0, &base, &HashMap::new());
        let idx = pixel_idx(2, 1);
        assert_eq!(out[idx], 107);
        assert_eq!(out[0], 100); // untouched pixel stays at base
    }

    #[test]
    fn source_layer_composites_from_injected_tile() {
        let mut l = PopLayers::default();
        let id = l.add_source_layer("census".into(), Blend::Add);
        let base = vec![100u16; (TILE_PX * TILE_PX) as usize];
        let mut src = vec![0u16; (TILE_PX * TILE_PX) as usize];
        src[pixel_idx(4, 4)] = 900;
        let sources = HashMap::from([(id, src)]);
        let out = l.apply_tile(0, 0, &base, &sources);
        assert_eq!(out[pixel_idx(4, 4)], 1000); // 100 base + 900 source (Add)
        assert_eq!(out[0], 100); // source zero elsewhere → base
                                 // Region/per-pixel path ignores source layers by design.
        assert_eq!(l.effective(100, 0, 0, 4, 4), 100);
    }

    #[test]
    fn import_layer_add_and_normal_blend() {
        let mut base_tiles = HashMap::new();
        let mut raster = TileRaster::new();
        raster.set(pixel_idx(3, 3), 500);
        base_tiles.insert((0, 0), raster);

        let mut add = PopLayers::default();
        add.add_import_layer("census".into(), Blend::Add, base_tiles.clone());
        assert_eq!(add.effective(100, 0, 0, 3, 3), 600); // 100 base + 500
                                                         // A pixel the import did not cover falls through to the base.
        assert_eq!(add.effective(100, 0, 0, 0, 0), 100);

        let mut normal = PopLayers::default();
        normal.add_import_layer("census".into(), Blend::Normal, base_tiles);
        assert_eq!(normal.effective(100, 0, 0, 3, 3), 500); // replaced
        assert_eq!(normal.effective(100, 0, 0, 0, 0), 100); // uncovered → base
    }

    #[test]
    fn base_is_never_removed_and_stays_at_bottom() {
        let mut l = PopLayers::default();
        let base_id = l.list()[1].id;
        l.remove_layer(base_id);
        assert!(l.list().iter().any(|i| i.kind == LayerKind::Base));
        // The base cannot be moved above an edit layer.
        l.move_layer(base_id, true);
        assert_eq!(
            l.list().last().expect("stack is non-empty").kind,
            LayerKind::Base
        );
    }

    #[test]
    fn remove_and_reorder_edit_layers() {
        let mut l = PopLayers::default();
        let first = l.list()[0].id; // the default paint layer
        let second = l.add_layer();
        assert_eq!(l.list()[0].id, second); // top-of-stack first
        l.move_layer(first, true);
        assert_eq!(l.list()[0].id, first);
        l.remove_layer(second);
        assert!(l.list().iter().all(|i| i.id != second));
        // Removing the last edit layer clears it rather than emptying the stack.
        l.add_delta(0, 0, 0, 0, 99);
        let last = l.list()[0].id;
        l.remove_layer(last);
        assert_eq!(l.list().len(), 2); // base + one cleared paint layer
        assert!(l.is_empty());
    }

    #[test]
    fn region_target_uses_stack_beneath_active() {
        let mut l = PopLayers::default();
        l.add_delta(0, 0, 5, 5, 30); // default paint layer
        l.add_layer(); // active empty layer on top
                       // Everything beneath the active layer: base(100) + 30.
        assert_eq!(l.effective_excluding_active(100, 0, 0, 5, 5), 130);
        assert_eq!(l.effective(100, 0, 0, 5, 5), 130);
    }

    #[test]
    fn null_island_is_grid_center() {
        let (gx, gy) = lonlat_to_global_px(0.0, 0.0);
        assert!((gx - f64::from(AXIS_PX) / 2.0).abs() < 1.0);
        assert!((gy - f64::from(AXIS_PX) / 2.0).abs() < 1.0);
        assert_eq!((gx as u32) / TILE_PX, TILES_PER_AXIS / 2);
    }

    #[test]
    fn global_px_round_trips_a_tile_center() {
        // A point inside z10 tile (523, 493) must map back into that tile.
        let lon = (523.5 / f64::from(TILES_PER_AXIS)) * 360.0 - 180.0;
        let n = f64::from(TILES_PER_AXIS);
        let lat = ((PI * (1.0 - 2.0 * 493.5 / n)).sinh()).atan().to_degrees();
        let (gx, gy) = lonlat_to_global_px(lon, lat);
        assert_eq!((gx as u32) / TILE_PX, 523);
        assert_eq!((gy as u32) / TILE_PX, 493);
    }

    #[test]
    fn region_bounds_orders_and_clamps() {
        let (gx0, gy0, gx1, gy1) = region_bounds(3.0, 6.0, 4.0, 7.0);
        assert!(gx0 < gx1 && gy0 < gy1);
        assert!(gx1 < AXIS_PX && gy1 < AXIS_PX);
        // A degenerate/out-of-order bbox still yields an ordered, in-range rect.
        let (ax0, ay0, ax1, ay1) = region_bounds(200.0, -100.0, -200.0, 100.0);
        assert!(ax0 <= ax1 && ay0 <= ay1 && ax1 < AXIS_PX && ay1 < AXIS_PX);
    }

    #[test]
    fn meters_per_pixel_shrinks_toward_poles() {
        assert!(meters_per_pixel(0.0) > meters_per_pixel(60.0));
        assert!(meters_per_pixel(0.0) > 50.0 && meters_per_pixel(0.0) < 150.0);
    }

    #[test]
    fn brush_covers_a_disc_and_is_weighted() {
        let px = brush_pixels(3.4, 6.5, meters_per_pixel(6.5) * 3.0);
        assert!(!px.is_empty());
        // Centre pixel has the highest weight; all weights are in range.
        assert!(px.iter().all(|(_, w)| (0.0..=1.0).contains(w)));
        assert!(px.iter().any(|(_, w)| *w > 0.9));
    }

    #[test]
    fn downsample_conserves_and_averages() {
        let w = TILE_PX as usize;
        let uniform = vec![100u16; w * w];
        // Four identical uniform children average to the same uniform value.
        let parent = downsample_children([
            Some(&uniform),
            Some(&uniform),
            Some(&uniform),
            Some(&uniform),
        ]);
        assert!(parent.iter().all(|&v| v == 100));
        // A missing child pulls the average down (three of four quadrants zero).
        let one = downsample_children([Some(&uniform), None, None, None]);
        assert!(one.iter().all(|&v| v <= 100));
        assert!(one.contains(&100)); // top-left quadrant stays 100
        assert!(one.contains(&0)); // other quadrants are zero
    }

    #[test]
    fn brush_delta_signs_and_scales() {
        assert_eq!(brush_delta(1.0, 50, BrushMode::Add), 50);
        assert_eq!(brush_delta(1.0, 50, BrushMode::Remove), -50);
        assert_eq!(brush_delta(0.5, 40, BrushMode::Add), 20);
    }
}
