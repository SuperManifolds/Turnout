#![allow(dead_code)]

use anyhow::{Context, Result};
use std::fmt;

// ══════════════════════════════════════════════════════════════════════
// Wire primitives — matching the game's serde::Deserializer vtable
// ══════════════════════════════════════════════════════════════════════

pub struct PayloadReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PayloadReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub fn position(&self) -> usize {
        self.pos
    }
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// LEB128 unsigned varint — vtable[3] with size=4 or size=8.
    pub fn read_varint(&mut self) -> Result<u64> {
        let start = self.pos;
        let mut result: u64 = 0;
        let mut shift = 0u32;
        loop {
            if self.pos >= self.data.len() {
                anyhow::bail!("EOF reading varint at {}", start);
            }
            let b = self.data[self.pos];
            self.pos += 1;
            result |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                anyhow::bail!("varint overflow at {}", start);
            }
        }
    }

    /// Zigzag-decoded signed i64 — the game uses zigzag flag=0 for all signed fields.
    pub fn read_i64z(&mut self) -> Result<i64> {
        let v = self.read_varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    /// Zigzag-decoded signed i32.
    pub fn read_i32z(&mut self) -> Result<i32> {
        self.read_i64z().map(|v| v as i32)
    }

    /// Raw u8 — vtable[3] with size=1 (raw memcpy, NOT varint).
    pub fn read_raw_u8(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            anyhow::bail!("EOF reading u8 at {}", self.pos);
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    /// Raw f32 LE — vtable[4].
    pub fn read_f32(&mut self) -> Result<f32> {
        if self.pos + 4 > self.data.len() {
            anyhow::bail!("EOF reading f32 at {}", self.pos);
        }
        let v = f32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Ok(v)
    }

    /// Raw f64 LE — vtable[5].
    pub fn read_f64(&mut self) -> Result<f64> {
        if self.pos + 8 > self.data.len() {
            anyhow::bail!("EOF reading f64 at {}", self.pos);
        }
        let v = f64::from_le_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        Ok(v)
    }

    /// String: varint(length) + length raw bytes.
    /// The game serializes each char via read_int(size=1), which is a raw byte read.
    /// Returns valid UTF-8; the game only stores UTF-8 strings.
    pub fn read_string(&mut self) -> Result<String> {
        let len = self.read_varint()? as usize;
        if self.pos + len > self.data.len() {
            anyhow::bail!("EOF reading string({}) at {}", len, self.pos);
        }
        let bytes = &self.data[self.pos..self.pos + len];
        self.pos += len;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| anyhow::anyhow!("invalid UTF-8 string at {}: {}", self.pos - len, e))
    }

    /// Tags: nimby::model::Tags = sorted vec_set<i64>.
    /// Wire: varint(count) + count × zigzag i64.
    pub fn read_tags(&mut self) -> Result<Vec<i64>> {
        let count = self.read_varint()? as usize;
        let mut tags = Vec::with_capacity(count);
        for _ in 0..count {
            tags.push(self.read_i64z()?);
        }
        Ok(tags)
    }

    /// vec_set<i64>: varint(count) + count × zigzag i64.
    /// Used at Track+0x140, +0x3F8, +0x430, +0x488, StationGroup track_ids, etc.
    pub fn read_vec_set_i64(&mut self) -> Result<Vec<i64>> {
        self.read_tags() // same wire format
    }

    /// vec<i64> unsigned: varint(count) + count × unsigned i64.
    pub fn read_vec_u64(&mut self) -> Result<Vec<u64>> {
        let count = self.read_varint()? as usize;
        let mut v = Vec::with_capacity(count);
        for _ in 0..count {
            v.push(self.read_varint()?);
        }
        Ok(v)
    }

    /// pair<ModSource, string>: i64z workshop_id + string path.
    pub fn read_mod_source_pair(&mut self) -> Result<(i64, String)> {
        let workshop_id = self.read_i64z()?;
        let path = self.read_string()?;
        Ok((workshop_id, path))
    }

    /// ModRelFile: pair<ModSource, string> + string name.
    /// Used in TrackKind::Horizon textures.
    pub fn read_mod_rel_file(&mut self) -> Result<ModRelFile> {
        let (workshop_id, path) = self.read_mod_source_pair()?;
        let name = self.read_string()?;
        Ok(ModRelFile { workshop_id, path, name })
    }

    /// optional<pair<ModSource,string>>: reads a bool flag, then conditionally the pair.
    pub fn read_optional_mod_source(&mut self) -> Result<Option<(i64, String)>> {
        let flag = self.read_raw_u8()?;
        if flag != 0 {
            let source = self.read_i64z()?;
            let path = self.read_string()?;
            Ok(Some((source, path)))
        } else {
            Ok(None)
        }
    }
}

// ══════════════════════════════════════════════════════════════════════
// Data structures — matching the game's C++ structs
// ══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct ModRelFile {
    pub workshop_id: i64,
    pub path: String,
    pub name: String,
}

// ──────────── Collection / Clip header ────────────

#[derive(Debug)]
pub struct Collection {
    pub id_a: u64,
    pub id_b: u64,
    pub mod_source: Option<(i64, String)>,
    pub name: String,
    pub clips: Vec<Clip>,
}

#[derive(Debug)]
pub struct Clip {
    pub guid: String,
    pub clip_id: u64,
    pub center_x: f64,
    pub center_y: f64,
    pub tracks: Vec<Track>,
    pub signals: Vec<Signal>,
    pub station_groups: Vec<StationGroup>,
    pub buildings: Vec<Building>,
    pub track_kinds: Vec<(i32, TrackKind)>,
    pub building_kinds: Vec<(i32, BuildingKind)>,
    pub demands: Vec<(u64, Demand)>,
    pub mod_metas: Vec<ModMeta>,
}

// ──────────── Track ────────────

/// A track node in a blueprint. Field layout from binary disassembly of
/// Clip Track deserializer at RVA 0x4FD90. Names from serde descriptor tables.
#[derive(Debug, Clone)]
pub struct Track {
    // Pre-coordinate fields
    pub node_id: i64,                        // +0x00 i64z (v>=1)
    pub node_type: u8,                       // +0x20 raw u8 (v>=30)
    pub track_type: i32,                     // +0x24 i32z (v>=30), index into TrackKind map
    pub max_speed: i32,                      // +0x28 i32z (v>=45)
    pub winding: Option<u8>,                 // +0x2C raw u8 (v>=122), curve winding direction
    pub prev_node: i64,                      // +0x08 i64z (v>=1), zigzag(-1) = null
    pub next_node: i64,                      // +0x10 i64z (v>=1)
    pub group_id: i64,                       // +0x18 i64z (v>=13)
    // Coordinate block
    pub user_max_speed: Option<f32>,         // +0x84 f32 (v>=72), custom speed limit
    pub x: f64,                              // +0x30 f64 (v>=1)
    pub y: f64,                              // +0x38 f64 (v>=1)
    pub user_tangent_delta: Option<f32>,     // +0x78 f32 (v>=102), tangent angle delta (0.0 or 2π)
    pub next_spline_t: Option<f32>,          // +0x7C f32 (v>=141), spline t parameter, default 0.5
    // Post-coordinate fields
    pub station_group_id: i64,               // +0xD0 i64z (v>=1), ref to StationGroup
    pub blueprint: Option<i32>,               // +0xA0 i32z (v>=108), creation origin enum
    pub name: Option<String>,                // +0xA8 string (v>=63)
    pub station_platform_auto_name: Option<u8>,// +0xC8 raw u8 (v>=63)
    pub straight: Option<u8>,                // +0xD8 raw u8 (v>=62), keep-straight constraint
    pub tangential: Option<u8>,              // +0xDA raw u8 (v>=143), tangential constraint
    pub limited_shapes: Option<u8>,          // +0xDB raw u8 (v>=144), shape mode limited/full
    pub conflicts: [Vec<Conflict>; 4],       // +0xE0/F8/110/128 (v>=28)
    /// Embedded signals at v32-197 via sub_5A750. Replaced by clip-level
    /// vec<Signal> at v>=198. For encoding, only v>=198 (signal_ids) is needed.
    /// At v<192 the decoded values here include conflict element data consumed
    /// as varints — NOT valid signal IDs. At v192-197 these ARE valid IDs.
    /// This field is DECODE-ONLY and should not be used for encoding new files.
    pub embedded_signals: Option<Vec<i64>>,  // +0x178 (v32-197 only, decode-only)
    pub signal_ids: Option<Vec<i64>>,        // +0x140 vec_set (v>=198, replaces embedded signals)
    pub attached_to_id: i64,                 // +0x3E0 i64z (v>=1), constraint parent track
    pub attached_to_t: f64,                  // +0x3E8 f64 (v>=1), t along parent track
    pub attached_to_direction: Option<i32>,   // +0x3F0 i32z (v>=30)
    pub attached_by: Vec<i64>,               // +0x3F8 vec_set (v>=1), reverse index for attached_to_id
    pub building_attached_by: Option<Vec<i64>>, // +0x430 vec_set (v>=62), objects attached to this track
    pub parallel_to_id: Option<i64>,         // +0x468 i64z (v>=33), parallel constraint parent
    pub parallel_kind: Option<i32>,          // +0x470 i64z→i32 truncated (v>=33)
    pub parallel_to_t: Option<f32>,          // +0x474 f32 (v>=33)
    pub parallel_to_direction: Option<i32>,  // +0x478 i32z (v>=33)
    pub parallel_to_offset: Option<f32>,     // +0x47C f32 (v>=33), offset distance
    pub parallel_to_disp: Option<f32>,       // +0x480 f32 (v>=60), display displacement
    pub parallel_by: Option<Vec<i64>>,        // +0x488 vec_set (v>=33), reverse index for parallel_to_id
    pub proximity_diamond: Option<f32>,      // +0x4D0 f32 (v>=192), overlap distance
}

impl fmt::Display for Track {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Track {} type={} tt={} speed={} prev={} next={} ({:.4}, {:.4})",
            self.node_id, self.node_type, self.track_type, self.max_speed,
            self.prev_node, self.next_node, self.x, self.y)
    }
}

// ──────────── Conflict ────────────

/// Track overlap/crossing conflict element.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub mode: i32,                   // conflict subtype enum
    pub track_id: i64,               // other track involved
    pub lat: i32,                    // LL32 geographic latitude
    pub lon: i32,                    // LL32 geographic longitude
    pub t_self: f32,                 // spline t on this track
    pub t_other: Option<f32>,        // spline t on other track (v>=192)
    pub clearance: Option<f32>,      // min clearance distance (v>=192)
    pub height_delta: Option<f32>,   // elevation difference (v>=192)
    pub overlap_dist: f32,           // overlap/separation distance
}

// ──────────── Signal ────────────

/// Signal placement. Serde descriptor table at RVA 0xA4D200.
#[derive(Debug, Clone)]
pub struct Signal {
    pub id: i64,
    pub kind_enum: Option<u8>,               // +0x08 (v>=202), SignalKind enum
    pub name: Option<String>,                // +0x10 (v>=202)
    pub kind: i32,                           // +0x30 (v>=32), signal type index
    pub signal_textures_hash: Option<u64>,   // +0x38 (v>=211), appearance hash
    pub pos_track_id: i64,                   // +0x40 (v>=32), track this signal is on
    pub pos_t: f64,                          // +0x48 (v>=32), t parameter (0-1) along track
    pub dir_a: Option<i8>,                   // +0x50 (v>=205), direction (-1/0/+1)
    pub dir_b: Option<i8>,                   // +0x51 (v>=205)
    pub side: i32,                           // +0x58 (v>=32), display side (0/1/2)
    pub size: Option<i32>,                   // +0x5C (v>=214), visual size category (mod 5)
    pub rotate: Option<i32>,                 // +0x60 (v>=214), visual rotation (mod 4)
    pub custom_alert_wait: Option<u8>,       // +0x64 (v>=55), enable custom alert wait
    pub alert_wait: Option<i32>,             // +0x68 (v>=55), alert wait time
    pub match_block_facing: Option<u8>,      // +0x6C (v>=37), script field
    pub check_beyond_stops: Option<u8>,      // +0x6D (v>=194), script field
    pub filter: Option<i32>,                 // +0x70 (v>=53), signal filter mode
    pub filter_exception_tags: Option<Vec<u64>>, // +0x78 (v>=53)
    pub scripts: Option<u64>,               // +0x90 (v>=204), ScriptInstances count
}

// ──────────── StationGroup ────────────

#[derive(Debug, Clone)]
pub struct StationGroup {
    pub id: i64,
    pub created_on: Option<i32>,               // +0x08 (v>=11), creation timestamp
    pub use_automatic_point: Option<u8>,        // +0x0C (v>=182), auto geo point placement
    pub position: Option<(f64, f64)>,           // +0x10/18 (v>=182)
    pub name: String,
    pub use_automatic_name: u8,                 // +0x40, auto station naming flag
    pub geo_name_pick: Option<i32>,             // +0x44 (v>=57), selected geographic name index
    pub tags: Option<Vec<i64>>,                 // v>=182
    pub track_ids: Vec<i64>,
    pub building_ids: Option<Vec<i64>>,         // v>=167
    pub extra_ids: Option<Vec<i64>>,            // v>=195
    pub size_factor: Option<f32>,               // +0x388 (v>=4), population area size multiplier
    pub walk_factor: Option<f32>,               // +0x38C (v>=163), walking distance factor
    pub max_platform_pax: Option<u32>,          // +0x390 (v>=165)
    pub transfer_overflow_into_hall: Option<u32>,// +0x394 (v>=165), max pax overflow capacity
    pub label_mode: Option<i32>,                // +0x398 (v>=94), display style enum
    pub scripts: Option<u64>,                   // v>=208
}

impl fmt::Display for StationGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Station \"{}\" id={} tracks={}", self.name, self.id, self.track_ids.len())
    }
}

// ──────────── Building ────────────

#[derive(Debug, Clone)]
pub struct Building {
    pub id: i64,
    pub kind_idx: i32,                       // +0x08, index into BuildingKind table
    pub kind_decal_idx: Option<i32>,         // +0x0C (v>=69), decal variant
    pub owner: i64,                          // +0x10, owner entity ref
    pub created_on: i32,                     // +0x18, creation timestamp
    pub layer: i32,                          // +0x1C, height layer (underground/ground/elevated)
    pub draw_layer: Option<i32>,             // +0x20 (v>=63), rendering draw order
    pub blueprint: u8,                       // +0x24, placed as part of a blueprint
    pub x: f64,
    pub y: f64,
    pub rotation_sin: f32,
    pub rotation_cos: f32,
    pub size_x: f32,                         // +0x40, building footprint width
    pub size_y: f32,                         // +0x44, building footprint depth
    pub color: u32,                          // +0x48, building color
    pub decal_color: Option<u32>,            // +0x4C (v>=69)
    pub poi: Option<BuildingPoi>,            // v>=67
    pub attached_to_track_id: i64,           // +0xA8, track this building is on
    pub start_t: f32,                        // +0xB0, start t (0 to 1) along track
    pub end_t: f32,                          // +0xB4, end t (0 to 1) along track
    pub bottom_side: f32,                    // +0xB8, bottom side offset (meters)
    pub top_side: f32,                       // +0xBC, top side offset (meters)
    pub attached_curved: u8,                // +0xC0, building follows track curve
}

/// Building point-of-interest / sign sub-struct. Optional — guarded by a u8 has_value flag.
/// Wire format from binary disassembly of sub_65C90 / sub_5DEF0.
#[derive(Debug, Clone)]
pub struct BuildingPoi {
    pub name: String,                      // string (v>=67)
    pub font_size: i32,                    // i32z (v>=67), 0=Small/1=Medium/2=Large, default 1
    pub max_zoom: i32,                     // i32z (v>=68), max zoom level, default 10
    pub fill_background: u8,               // u8 (v>=67), checkbox flag, default 0
    pub demand_curve: Option<u64>,         // optional<u64> (v>=158), demand curve ID ref
    pub population: Option<u32>,           // u32 (v>=158), pax count, default 0
}

impl fmt::Display for Building {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let angle = self.rotation_sin.atan2(self.rotation_cos).to_degrees();
        write!(f, "Building id={} kind={} ({:.2}, {:.2}) angle={:.1}°",
            self.id, self.kind_idx, self.x, self.y, angle)
    }
}

// ──────────── TrackKind ────────────

#[derive(Debug, Clone)]
pub struct TrackKindHorizon {
    pub speed_class: i32,
    pub gauge: f64,
    pub height: f64,
    pub max_speed: f64,
    pub width_a: f64,
    pub width_b: f64,
    pub spacing: f64,
    pub offset_a: f64,
    pub offset_b: f64,
    pub visual_distance: i64,
    pub flags: [u8; 5],
    pub textures: Vec<TrackTexture>,
}

#[derive(Debug, Clone)]
pub struct TrackTexture {
    pub speed_class: i32,
    pub files: [ModRelFile; 4],
}

#[derive(Debug, Clone)]
pub struct TrackKind {
    pub display_name: String,
    pub speed_class_flag: u8,
    pub speed_class: i32,
    pub internal_name: String,
    pub secondary_name: String,
    pub horizons: [TrackKindHorizon; 3],
}

impl fmt::Display for TrackKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TrackKind \"{}\" ({})", self.display_name, self.internal_name)
    }
}

// ──────────── BuildingKind ────────────

#[derive(Debug, Clone)]
pub struct BuildingKind {
    pub display_name: String,
    pub speed_class_flag: u8,
    pub speed_class: i32,
    pub internal_name: String,
    pub secondary_name: String,
    pub tags: Vec<i64>,
    pub size_x: f32,
    pub size_y: f32,
    pub curved: Option<u8>,          // +0x88 (v>=65), building_constraint_curved
    pub recolor: u8,                 // +0x89, can be recolored by player
    pub is_poi: Option<u8>,          // +0x8A (v>=67), point of interest / sign
    pub has_default_size: Option<u8>,// +0x8B (v>=69), sizing flag
    pub decal_count: Option<u8>,     // +0x8C (v>=69), number of decals
    pub border_x: i32,              // +0x90, 9-patch horizontal border width (read twice)
    pub lod_x: f32,
    pub lod_y: f32,
    pub sentinel: u32,
    pub offset_neg: f32,
    pub offset_pos: f32,
    pub scripts: Vec<String>,
    pub rule_x: i32,                 // +0xC4, horizontal repeat(1)/stretch(0)
    pub rule_y: i32,                 // +0xC8, vertical repeat(1)/stretch(0)
    pub partial_repeat_x: Option<u8>,// +0xCC (v>=63), allow fractional horizontal tile
    pub partial_repeat_y: Option<u8>,// +0xCD (v>=63), allow fractional vertical tile
    pub default_draw_layer: i32,     // +0xD0, 0=base/1=floor/2=wall/3=roof
    pub texture: ModRelFile,
    pub model_path: String,
}

impl fmt::Display for BuildingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BuildingKind \"{}\" ({})", self.display_name, self.internal_name)
    }
}

// ──────────── Demand ────────────

#[derive(Debug, Clone)]
pub struct Demand {
    pub poi_layer_id: u64,           // +0x00, links to a geographic POI data layer
    pub is_mod: Option<u8>,          // +0x08 (v>=159), demand curve from a mod
    pub mod_source: Option<(i64, String)>,
    pub name: String,
    pub time_a: [f32; 168],
    pub time_b: [f32; 168],
    pub distance_ranges: Vec<DemandRange>,
}

#[derive(Debug, Clone)]
pub struct DemandRange {
    pub min_distance: i32,
    pub max_distance: i32,
    pub step: i32,
    pub values: Vec<f32>,
}

// ──────────── ModMeta ────────────

/// Mod metadata. Describes a Steam Workshop mod or built-in content pack.
#[derive(Debug, Clone)]
pub struct ModMeta {
    pub source_id: i64,
    pub source_path: String,
    pub folder: String,
    pub display_name: String,
    pub author: String,
    pub description: String,
    pub version: String,
    pub tag: String,
    pub provides: Vec<i64>,
    pub content_items: Vec<(i32, String, String)>, // v>=117
    pub content_loaded: u8,              // whether mod content has been validated
    pub has_local_data: u8,              // whether mod has local filesystem data
}

impl fmt::Display for ModMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mod src={} \"{}\" path=\"{}\"",
            self.source_id, self.display_name, self.source_path)
    }
}

// ══════════════════════════════════════════════════════════════════════
// Parsers — version-aware, matching binary decompilation
// ══════════════════════════════════════════════════════════════════════

pub fn parse_collections(r: &mut PayloadReader<'_>, ver: u32) -> Result<Vec<Collection>> {
    let count = r.read_varint()? as usize;
    let mut collections = Vec::with_capacity(count);
    for _ in 0..count {
        collections.push(parse_collection(r, ver)?);
    }
    Ok(collections)
}

fn parse_collection(r: &mut PayloadReader<'_>, ver: u32) -> Result<Collection> {
    let (id_a, id_b) = if ver >= 71 {
        (r.read_varint()?, r.read_varint()?)
    } else {
        (0, 0)
    };
    let mod_source = if ver >= 71 {
        r.read_optional_mod_source()?
    } else {
        None
    };
    let name = if ver >= 66 { r.read_string()? } else { String::new() };
    let clip_count = if ver >= 66 { r.read_varint()? as usize } else { 0 };
    let mut clips = Vec::with_capacity(clip_count);
    for _ in 0..clip_count {
        clips.push(parse_clip(r, ver)?);
    }
    Ok(Collection { id_a, id_b, mod_source, name, clips })
}

fn parse_clip(r: &mut PayloadReader<'_>, ver: u32) -> Result<Clip> {
    let guid = if ver >= 66 { r.read_string()? } else { String::new() };
    let clip_id = if ver >= 66 { r.read_varint()? } else { 0 };
    let (center_x, center_y) = if ver >= 147 {
        (r.read_f64()?, r.read_f64()?)
    } else {
        (0.0, 0.0)
    };

    let tracks = if ver >= 66 { parse_tracks(r, ver).with_context(|| format!("tracks at {}", r.position()))? } else { vec![] };
    // Section order from binary: signals → stations → buildings → track_kinds → building_kinds → demands → mods
    let signals = if ver >= 198 { parse_signals(r, ver).with_context(|| format!("signals at {}", r.position()))? } else { vec![] };
    let station_groups = if ver >= 66 { parse_station_groups(r, ver).with_context(|| format!("stations at {}", r.position()))? } else { vec![] };
    let buildings = if ver >= 66 { parse_buildings(r, ver).with_context(|| format!("buildings at {}", r.position()))? } else { vec![] };
    let track_kinds = if ver >= 66 { parse_track_kind_map(r, ver).with_context(|| format!("track_kinds at {}", r.position()))? } else { vec![] };
    let building_kinds = if ver >= 66 { parse_building_kind_map(r, ver).with_context(|| format!("building_kinds at {}", r.position()))? } else { vec![] };
    let demands = if ver >= 158 { parse_demand_map(r, ver).with_context(|| format!("demands at {}", r.position()))? } else { vec![] };
    let mod_metas = if ver >= 66 { parse_mod_metas(r, ver).with_context(|| format!("mods at {}", r.position()))? } else { vec![] };

    Ok(Clip {
        guid, clip_id, center_x, center_y,
        tracks, signals, station_groups, buildings,
        track_kinds, building_kinds, demands, mod_metas,
    })
}

// ──────────── Track parser ────────────

fn parse_tracks(r: &mut PayloadReader<'_>, ver: u32) -> Result<Vec<Track>> {
    let count = r.read_varint()? as usize;
    let mut tracks = Vec::with_capacity(count);
    for _ in 0..count {
        tracks.push(parse_track(r, ver)?);
    }
    Ok(tracks)
}

fn parse_track(r: &mut PayloadReader<'_>, ver: u32) -> Result<Track> {
    // Pre-coordinate fields
    let node_id = r.read_i64z()?;
    let node_type = if ver >= 30 { r.read_raw_u8()? } else { 0 };
    if ver < 30 { r.read_i64z()?; } // v1-29 migration
    let track_type = if ver >= 30 { r.read_i32z()? } else { 0 };
    if ver < 30 { r.read_i64z()?; } // v1-29 migration
    let max_speed = if ver >= 45 { r.read_i32z()? } else { 0 };
    let winding = if ver >= 122 { Some(r.read_raw_u8()?) } else { None };
    let prev_node = r.read_i64z()?;
    let next_node = r.read_i64z()?;
    let group_id = if ver >= 13 { r.read_i64z()? } else { 0 };

    // Coordinate block
    let user_max_speed = if ver >= 72 { Some(r.read_f32()?) } else { None };
    let x = r.read_f64()?;
    let y = r.read_f64()?;
    if (102..=105).contains(&ver) { r.read_f32()?; } // v102-105 migration
    let user_tangent_delta = if ver >= 102 { Some(r.read_f32()?) } else { None };
    let next_spline_t = if ver >= 141 { Some(r.read_f32()?) } else { None };

    // Post-coordinate fields — exact layout from binary disassembly (RVA 0x4FD90)
    let station_group_id = r.read_i64z()?;
    let blueprint = if ver >= 108 { Some(r.read_i32z()?) } else { None };
    let (name, station_platform_auto_name) = if ver >= 63 {
        (Some(r.read_string()?), Some(r.read_raw_u8()?))
    } else {
        (None, None)
    };
    if (170..=181).contains(&ver) { r.read_f32()?; } // F20 migration
    if (15..=91).contains(&ver) { r.read_raw_u8()?; } // F21 migration
    let straight = if ver >= 62 { Some(r.read_raw_u8()?) } else { None };
    let tangential = if ver >= 143 { Some(r.read_raw_u8()?) } else { None };
    let limited_shapes = if ver >= 144 { Some(r.read_raw_u8()?) } else { None };

    // 4× conflict vectors via sub_65620 (v>=28)
    let conflicts = if ver >= 28 {
        let mut cs: [Vec<Conflict>; 4] = Default::default();
        for cv in &mut cs {
            *cv = parse_conflict_vec(r, ver)?;
        }
        cs
    } else {
        Default::default()
    };

    // Signal/vec_140 — mutually exclusive (upper-bounded v32-197 vs v>=198)
    let embedded_signals = if (32..=197).contains(&ver) {
        Some(r.read_vec_set_i64()?)
    } else {
        None
    };
    let signal_ids = if ver >= 198 {
        Some(r.read_vec_set_i64()?)
    } else {
        None
    };

    let attached_to_id = r.read_i64z()?;
    let attached_to_t = r.read_f64()?;
    let attached_to_direction = if ver >= 30 { Some(r.read_i32z()?) } else { None };
    let attached_by = r.read_vec_set_i64()?;
    let building_attached_by = if ver >= 62 { Some(r.read_vec_set_i64()?) } else { None };

    let (parallel_to_id, parallel_kind, parallel_to_t, parallel_to_direction, parallel_to_offset) = if ver >= 33 {
        (
            Some(r.read_i64z()?),
            Some(r.read_i64z()? as i32),  // reads i64, truncates to i32
            Some(r.read_f32()?),
            Some(r.read_i32z()?),
            Some(r.read_f32()?),
        )
    } else {
        (None, None, None, None, None)
    };
    let parallel_to_disp = if ver >= 60 { Some(r.read_f32()?) } else { None };
    let parallel_by = if ver >= 33 { Some(r.read_vec_set_i64()?) } else { None };
    let proximity_diamond = if ver >= 192 { Some(r.read_f32()?) } else { None };

    Ok(Track {
        node_id, node_type, track_type, max_speed, winding,
        prev_node, next_node, group_id,
        user_max_speed, x, y, user_tangent_delta, next_spline_t,
        station_group_id, blueprint, name, station_platform_auto_name,
        straight, tangential, limited_shapes,
        conflicts, embedded_signals, signal_ids,
        attached_to_id, attached_to_t, attached_to_direction,
        attached_by, building_attached_by,
        parallel_to_id, parallel_kind, parallel_to_t, parallel_to_direction, parallel_to_offset,
        parallel_to_disp, parallel_by, proximity_diamond,
    })
}

fn parse_conflict_vec(r: &mut PayloadReader<'_>, ver: u32) -> Result<Vec<Conflict>> {
    let count = r.read_varint()? as usize;
    let mut conflicts = Vec::with_capacity(count);
    for _ in 0..count {
        let mode = r.read_i64z()? as i32;
        let track_id = r.read_i64z()?;
        if (28..192).contains(&ver) { r.read_i64z()?; } // removed field (v28-191 inclusive, Rust range [28,192) is correct)
        let lat = r.read_i32z()?;
        let lon = r.read_i32z()?;
        let t_self = r.read_f32()?;
        let (t_other, clearance, height_delta) = if ver >= 192 {
            (Some(r.read_f32()?), Some(r.read_f32()?), Some(r.read_f32()?))
        } else {
            (None, None, None)
        };
        let overlap_dist = r.read_f32()?;
        conflicts.push(Conflict { mode, track_id, lat, lon, t_self, t_other, clearance, height_delta, overlap_dist });
    }
    Ok(conflicts)
}

// ──────────── Signal parser ────────────

fn parse_signals(r: &mut PayloadReader<'_>, ver: u32) -> Result<Vec<Signal>> {
    let count = r.read_varint()? as usize;
    let mut signals = Vec::with_capacity(count);
    for _ in 0..count {
        signals.push(parse_signal(r, ver)?);
    }
    Ok(signals)
}

fn parse_signal(r: &mut PayloadReader<'_>, ver: u32) -> Result<Signal> {
    let id = r.read_i64z()?;
    let kind_enum = if ver >= 202 { Some(r.read_raw_u8()?) } else { None };
    let name = if ver >= 202 { Some(r.read_string()?) } else { None };
    let kind = if ver >= 32 { r.read_i32z()? } else { 0 };
    let signal_textures_hash = if ver >= 211 { Some(r.read_varint()?) } else { None };
    let pos_track_id = if ver >= 32 { r.read_i64z()? } else { 0 };
    let pos_t = if ver >= 32 { r.read_f64()? } else { 0.0 };
    if (32..205).contains(&ver) { r.read_i32z()?; } // legacy migration
    let dir_a = if ver >= 205 { Some(r.read_raw_u8()? as i8) } else { None };
    let dir_b = if ver >= 205 { Some(r.read_raw_u8()? as i8) } else { None };
    let side = if ver >= 32 { r.read_i32z()? } else { 0 };
    let size = if ver >= 214 { Some(r.read_i32z()?) } else { None };
    let rotate = if ver >= 214 { Some(r.read_i32z()?) } else { None };
    if (44..=50).contains(&ver) { r.read_raw_u8()?; } // legacy discard
    let custom_alert_wait = if ver >= 55 { Some(r.read_raw_u8()?) } else { None };
    let alert_wait = if ver >= 55 { Some(r.read_i32z()?) } else { None };
    let match_block_facing = if ver >= 37 { Some(r.read_raw_u8()?) } else { None };
    let check_beyond_stops = if ver >= 194 { Some(r.read_raw_u8()?) } else { None };
    let filter = if ver >= 53 { Some(r.read_i32z()?) } else { None };
    let filter_exception_tags = if ver >= 53 { Some(r.read_vec_u64()?) } else { None };
    let scripts = if ver >= 204 { Some(r.read_varint()?) } else { None };

    Ok(Signal {
        id, kind_enum, name, kind, signal_textures_hash, pos_track_id, pos_t,
        dir_a, dir_b, side, size, rotate,
        custom_alert_wait, alert_wait, match_block_facing, check_beyond_stops,
        filter, filter_exception_tags, scripts,
    })
}

// ──────────── StationGroup parser ────────────

fn parse_station_groups(r: &mut PayloadReader<'_>, ver: u32) -> Result<Vec<StationGroup>> {
    let count = r.read_varint()? as usize;
    let mut groups = Vec::with_capacity(count);
    for _ in 0..count {
        groups.push(parse_station_group(r, ver)?);
    }
    Ok(groups)
}

fn parse_station_group(r: &mut PayloadReader<'_>, ver: u32) -> Result<StationGroup> {
    let id = r.read_i64z()?;
    let created_on = if ver >= 11 { Some(r.read_i32z()?) } else { None };
    let use_automatic_point = if ver >= 182 { Some(r.read_raw_u8()?) } else { None };
    let position = if ver >= 182 {
        Some((r.read_f64()?, r.read_f64()?))
    } else {
        None
    };
    let name = r.read_string()?;
    let use_automatic_name = r.read_raw_u8()?;
    let geo_name_pick = if ver >= 57 { Some(r.read_i32z()?) } else { None };
    let tags = if ver >= 182 { Some(r.read_tags()?) } else { None };
    let track_ids = r.read_vec_set_i64()?;
    let building_ids = if ver >= 167 { Some(r.read_vec_set_i64()?) } else { None };
    let extra_ids = if ver >= 195 { Some(r.read_vec_set_i64()?) } else { None };
    let size_factor = if ver >= 4 { Some(r.read_f32()?) } else { None };
    let walk_factor = if ver >= 163 { Some(r.read_f32()?) } else { None };
    let max_platform_pax = if ver >= 165 { Some(r.read_varint()? as u32) } else { None };
    let transfer_overflow_into_hall = if ver >= 165 { Some(r.read_varint()? as u32) } else { None };
    let label_mode = if ver >= 94 { Some(r.read_i32z()?) } else { None };
    let scripts = if ver >= 208 { Some(r.read_varint()?) } else { None };

    Ok(StationGroup {
        id, created_on, use_automatic_point, position, name, use_automatic_name, geo_name_pick,
        tags, track_ids, building_ids, extra_ids,
        size_factor, walk_factor, max_platform_pax, transfer_overflow_into_hall, label_mode, scripts,
    })
}

// ──────────── Building parser ────────────

fn parse_buildings(r: &mut PayloadReader<'_>, ver: u32) -> Result<Vec<Building>> {
    let count = r.read_varint()? as usize;
    let mut buildings = Vec::with_capacity(count);
    for _ in 0..count {
        buildings.push(parse_building(r, ver)?);
    }
    Ok(buildings)
}

fn parse_building(r: &mut PayloadReader<'_>, ver: u32) -> Result<Building> {
    let id = r.read_i64z()?;
    let kind_idx = r.read_i32z()?;
    let kind_decal_idx = if ver >= 69 { Some(r.read_i32z()?) } else { None };
    let owner = r.read_i64z()?;
    let created_on = r.read_i32z()?;
    let layer = r.read_i32z()?;
    let draw_layer = if ver >= 63 { Some(r.read_i32z()?) } else { None };
    let blueprint = r.read_raw_u8()?;
    let x = r.read_f64()?;
    let y = r.read_f64()?;
    let rotation_sin = r.read_f32()?;
    let rotation_cos = r.read_f32()?;
    let size_x = r.read_f32()?;
    let size_y = r.read_f32()?;
    let color = r.read_varint()? as u32;
    let decal_color = if ver >= 69 { Some(r.read_varint()? as u32) } else { None };

    // Building::POI (v>=67): optional sub-struct via sub_65C90
    let poi = if ver >= 67 {
        let has_value = r.read_raw_u8()? != 0;
        if has_value {
            let name = r.read_string()?;
            let font_size = r.read_i32z()?;
            let max_zoom = if ver >= 68 { r.read_i32z()? } else { 10 };
            let fill_background = r.read_raw_u8()?;
            let demand_curve = if ver >= 158 {
                let has = r.read_raw_u8()? != 0;
                if has { Some(r.read_varint()?) } else { None }
            } else {
                None
            };
            let population = if ver >= 158 { Some(r.read_varint()? as u32) } else { None };
            Some(BuildingPoi { name, font_size, max_zoom, fill_background, demand_curve, population })
        } else {
            None
        }
    } else {
        None
    };

    let attached_to_track_id = r.read_i64z()?;
    let start_t = r.read_f32()?;
    let end_t = r.read_f32()?;
    let bottom_side = r.read_f32()?;
    let top_side = r.read_f32()?;
    let attached_curved = r.read_raw_u8()?;

    Ok(Building {
        id, kind_idx, kind_decal_idx, owner, created_on, layer, draw_layer, blueprint,
        x, y, rotation_sin, rotation_cos, size_x, size_y,
        color, decal_color, poi, attached_to_track_id,
        start_t, end_t, bottom_side, top_side, attached_curved,
    })
}

// ──────────── TrackKind parser ────────────

fn parse_track_kind_map(r: &mut PayloadReader<'_>, ver: u32) -> Result<Vec<(i32, TrackKind)>> {
    let count = r.read_varint()? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let key = r.read_i32z()?;
        let value = parse_track_kind(r, ver)?;
        entries.push((key, value));
    }
    Ok(entries)
}

fn parse_track_kind(r: &mut PayloadReader<'_>, ver: u32) -> Result<TrackKind> {
    let display_name = r.read_string()?;
    let speed_class_flag = r.read_raw_u8()?;
    let speed_class = r.read_i32z()?;
    let internal_name = r.read_string()?;
    let secondary_name = r.read_string()?;

    let mut horizons_vec = Vec::with_capacity(3);
    for _ in 0..3 {
        horizons_vec.push(parse_horizon(r, ver)?);
    }
    let horizons: [TrackKindHorizon; 3] = horizons_vec.try_into().unwrap();

    Ok(TrackKind { display_name, speed_class_flag, speed_class, internal_name, secondary_name, horizons })
}

/// All Horizon fields are gated at v>=48 in the binary. Since TrackKind itself
/// is only serialized at v>=66, all fields are always present in practice.
fn parse_horizon(r: &mut PayloadReader<'_>, _ver: u32) -> Result<TrackKindHorizon> {
    let speed_class = r.read_i32z()?;
    let gauge = r.read_f64()?;
    let height = r.read_f64()?;
    let max_speed = r.read_f64()?;
    let width_a = r.read_f64()?;
    let width_b = r.read_f64()?;
    let spacing = r.read_f64()?;
    let offset_a = r.read_f64()?;
    let offset_b = r.read_f64()?;
    let visual_distance = r.read_i64z()?;
    let mut flags = [0u8; 5];
    for f in &mut flags { *f = r.read_raw_u8()?; }

    let mut textures = Vec::with_capacity(6);
    for _ in 0..6 {
        let tex_speed = r.read_i32z()?;
        let mut files = Vec::with_capacity(4);
        for _ in 0..4 {
            files.push(r.read_mod_rel_file()?);
        }
        let files: [ModRelFile; 4] = files.try_into().unwrap();
        textures.push(TrackTexture { speed_class: tex_speed, files });
    }

    Ok(TrackKindHorizon {
        speed_class, gauge, height, max_speed, width_a, width_b,
        spacing, offset_a, offset_b, visual_distance, flags, textures,
    })
}

// ──────────── BuildingKind parser ────────────

fn parse_building_kind_map(r: &mut PayloadReader<'_>, ver: u32) -> Result<Vec<(i32, BuildingKind)>> {
    let count = r.read_varint()? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let key = r.read_i32z()?;
        let value = parse_building_kind(r, ver)?;
        entries.push((key, value));
    }
    Ok(entries)
}

fn parse_building_kind(r: &mut PayloadReader<'_>, ver: u32) -> Result<BuildingKind> {
    let display_name = r.read_string()?;
    let speed_class_flag = r.read_raw_u8()?;
    let speed_class = r.read_i32z()?;
    let internal_name = r.read_string()?;
    let secondary_name = r.read_string()?;
    // v==62 only: discarded i32z
    if ver == 62 { r.read_i32z()?; }
    let tags = r.read_tags()?;
    let size_x = r.read_f32()?;
    let size_y = r.read_f32()?;
    // v62-181: discarded u8
    if (62..=181).contains(&ver) { r.read_raw_u8()?; }
    // v167-181: discarded u8
    if (167..=181).contains(&ver) { r.read_raw_u8()?; }
    let curved = if ver >= 65 { Some(r.read_raw_u8()?) } else { None };
    let recolor = r.read_raw_u8()?;
    let is_poi = if ver >= 67 { Some(r.read_raw_u8()?) } else { None };
    let (has_default_size, decal_count) = if ver >= 69 {
        (Some(r.read_raw_u8()?), Some(r.read_raw_u8()?))
    } else {
        (None, None)
    };
    let border_x = r.read_i32z()?;
    let _border_x_again = r.read_i32z()?; // read twice (game quirk)
    let (lod_x, lod_y, sentinel, offset_neg, offset_pos) = if ver >= 69 {
        (r.read_f32()?, r.read_f32()?, r.read_varint()? as u32, r.read_f32()?, r.read_f32()?)
    } else {
        (5.0, 5.0, 0xFFFFFFFF, -2.5, 2.5)
    };
    // flag_8C re-read at v>=69
    if ver >= 69 { r.read_raw_u8()?; }
    // vec<string> scripts (v>=69)
    let scripts = if ver >= 69 {
        let c = r.read_varint()? as usize;
        let mut s = Vec::with_capacity(c);
        for _ in 0..c { s.push(r.read_string()?); }
        s
    } else {
        vec![]
    };
    let rule_x = r.read_i32z()?;
    let rule_y = r.read_i32z()?;
    let partial_repeat_x = if ver >= 63 { Some(r.read_raw_u8()?) } else { None };
    let partial_repeat_y = if ver >= 63 { Some(r.read_raw_u8()?) } else { None };
    let default_draw_layer = r.read_i32z()?;
    let (tex_workshop_id, tex_path) = r.read_mod_source_pair()?;
    let model_path = r.read_string()?;
    let texture = ModRelFile { workshop_id: tex_workshop_id, path: tex_path, name: String::new() };
    // v==62 only: discarded extra pair + string
    if ver == 62 { r.read_mod_source_pair()?; r.read_string()?; }

    Ok(BuildingKind {
        display_name, speed_class_flag, speed_class, internal_name, secondary_name,
        tags, size_x, size_y, curved, recolor, is_poi, has_default_size, decal_count,
        border_x, lod_x, lod_y, sentinel,
        offset_neg, offset_pos, scripts, rule_x, rule_y, partial_repeat_x, partial_repeat_y,
        default_draw_layer, texture, model_path,
    })
}

// ──────────── Demand parser ────────────

fn parse_demand_map(r: &mut PayloadReader<'_>, ver: u32) -> Result<Vec<(u64, Demand)>> {
    let count = r.read_varint()? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let key = r.read_varint()?;
        let value = parse_demand(r, ver)?;
        entries.push((key, value));
    }
    Ok(entries)
}

fn parse_demand(r: &mut PayloadReader<'_>, ver: u32) -> Result<Demand> {
    let poi_layer_id = r.read_varint()?;
    let is_mod = if ver >= 159 { Some(r.read_raw_u8()?) } else { None };
    let mod_source = if ver >= 159 { r.read_optional_mod_source()? } else { None };
    let name = r.read_string()?;
    let mut time_a = [0.0f32; 168];
    for v in &mut time_a { *v = r.read_f32()?; }
    let mut time_b = [0.0f32; 168];
    for v in &mut time_b { *v = r.read_f32()?; }
    // Distance ranges
    let range_count = r.read_varint()? as usize;
    let mut distance_ranges = Vec::with_capacity(range_count);
    for _ in 0..range_count {
        let min_distance = r.read_i32z()?;
        let max_distance = r.read_i32z()?;
        let step = r.read_i32z()?;
        let val_count = r.read_varint()? as usize;
        let mut values = Vec::with_capacity(val_count);
        for _ in 0..val_count { values.push(r.read_f32()?); }
        distance_ranges.push(DemandRange { min_distance, max_distance, step, values });
    }
    Ok(Demand { poi_layer_id, is_mod, mod_source, name, time_a, time_b, distance_ranges })
}

// ──────────── ModMeta parser ────────────

fn parse_mod_metas(r: &mut PayloadReader<'_>, ver: u32) -> Result<Vec<ModMeta>> {
    let count = r.read_varint()? as usize;
    let mut metas = Vec::with_capacity(count);
    for _ in 0..count {
        metas.push(parse_mod_meta(r, ver)?);
    }
    Ok(metas)
}

fn parse_mod_meta(r: &mut PayloadReader<'_>, ver: u32) -> Result<ModMeta> {
    let source_id = r.read_i64z()?;
    let source_path = r.read_string()?;
    let folder = r.read_string()?;
    let display_name = r.read_string()?;
    let author = r.read_string()?;
    let description = r.read_string()?;
    let version = r.read_string()?;
    let tag = r.read_string()?;
    let provides = r.read_vec_set_i64()?;
    let content_items = if ver >= 117 {
        let c = r.read_varint()? as usize;
        let mut items = Vec::with_capacity(c);
        for _ in 0..c {
            let key_type = r.read_i32z()?;
            let key_name = r.read_string()?;
            let value_name = r.read_string()?;
            items.push((key_type, key_name, value_name));
        }
        items
    } else {
        vec![]
    };
    let content_loaded = r.read_raw_u8()?;
    let has_local_data = r.read_raw_u8()?;
    Ok(ModMeta { source_id, source_path, folder, display_name, author, description, version, tag, provides, content_items, content_loaded, has_local_data })
}

// ══════════════════════════════════════════════════════════════════════
// Top-level entry points
// ══════════════════════════════════════════════════════════════════════

/// Parse a decompressed .nrclip payload into structured data.
pub fn parse_payload(data: &[u8], model_version: u32) -> Result<Vec<Collection>> {
    let mut r = PayloadReader::new(data);
    let collections = parse_collections(&mut r, model_version)?;
    if r.remaining() > 0 {
        anyhow::bail!(
            "{} trailing bytes at offset {} (payload size {})",
            r.remaining(), r.position(), data.len()
        );
    }
    Ok(collections)
}
