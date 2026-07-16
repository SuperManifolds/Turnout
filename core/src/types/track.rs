use crate::error::Result;
use std::fmt;
use crate::wire::{PayloadReader, PayloadWriter};
use super::{NrclipRead, NrclipWrite};

#[derive(Debug, Clone)]
pub struct Conflict {
    pub mode: i32,
    pub track_id: i64,
    pub lat: i32,
    pub lon: i32,
    pub t_self: f32,
    pub t_other: Option<f32>,
    pub clearance: Option<f32>,
    pub height_delta: Option<f32>,
    pub overlap_dist: f32,
}

/// A track node in a blueprint. 46 fields, version-gated.
#[derive(Debug, Clone)]
pub struct Track {
    // Pre-coordinate
    pub node_id: i64,
    pub node_type: u8,
    pub track_type: i32,
    pub layer: i32,
    pub winding: Option<u8>,
    pub prev_node: i64,
    pub next_node: i64,
    pub group_id: i64,
    // Coordinate block
    pub user_max_speed: Option<f32>,
    pub x: f64,
    pub y: f64,
    pub user_tangent_delta: Option<f32>,
    pub next_spline_t: Option<f32>,
    // Post-coordinate
    pub station_group_id: i64,
    pub blueprint: Option<i32>,
    pub name: Option<String>,
    pub station_platform_auto_name: Option<u8>,
    pub straight: Option<u8>,
    pub tangential: Option<u8>,
    pub limited_shapes: Option<u8>,
    pub conflicts: [Vec<Conflict>; 4],
    pub embedded_signals: Option<Vec<i64>>,
    pub signal_ids: Option<Vec<i64>>,
    pub attached_to_id: i64,
    pub attached_to_t: f64,
    pub attached_to_direction: Option<i32>,
    pub attached_by: Vec<i64>,
    pub building_attached_by: Option<Vec<i64>>,
    pub parallel_to_id: Option<i64>,
    pub parallel_kind: Option<i32>,
    pub parallel_to_t: Option<f32>,
    pub parallel_to_direction: Option<i32>,
    pub parallel_to_offset: Option<f32>,
    pub parallel_to_disp: Option<f32>,
    pub parallel_by: Option<Vec<i64>>,
    pub proximity_diamond: Option<f32>,
}

impl Default for Track {
    fn default() -> Self {
        Track {
            node_id: 0,
            node_type: 1,
            track_type: 0,
            layer: 0,
            winding: Some(1),
            prev_node: 0,
            next_node: 0,
            group_id: 0,
            user_max_speed: Some(0.0),
            x: 0.0,
            y: 0.0,
            user_tangent_delta: Some(0.0),
            next_spline_t: Some(0.5),
            station_group_id: 0,
            blueprint: Some(0),
            name: Some(String::new()),
            station_platform_auto_name: Some(1),
            straight: Some(0),
            tangential: Some(0),
            limited_shapes: Some(0),
            conflicts: Default::default(),
            embedded_signals: None,
            signal_ids: Some(Vec::new()),
            attached_to_id: 0,
            attached_to_t: 0.0,
            attached_to_direction: Some(0),
            attached_by: Vec::new(),
            building_attached_by: Some(Vec::new()),
            parallel_to_id: Some(0),
            parallel_kind: Some(0),
            parallel_to_t: Some(0.0),
            parallel_to_direction: Some(0),
            parallel_to_offset: Some(0.0),
            parallel_to_disp: Some(0.0),
            parallel_by: Some(Vec::new()),
            proximity_diamond: Some(2.0),
        }
    }
}

impl fmt::Display for Track {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Track {} type={} tt={} layer={} prev={} next={} ({:.4}, {:.4})",
            self.node_id, self.node_type, self.track_type, self.layer,
            self.prev_node, self.next_node, self.x, self.y)
    }
}

impl NrclipRead for Track {
    fn nrclip_read(r: &mut PayloadReader, ver: u32) -> Result<Self> {
        // Pre-coordinate fields
        let node_id = r.read_i64z()?;
        let node_type = if ver >= 30 { r.read_raw_u8()? } else { 0 };
        if ver < 30 { r.read_i64z()?; }
        let track_type = if ver >= 30 { r.read_i32z()? } else { 0 };
        if ver < 30 { r.read_i64z()?; }
        let layer = if ver >= 45 { r.read_i32z()? } else { 0 };
        let winding = if ver >= 122 { Some(r.read_raw_u8()?) } else { None };
        let prev_node = r.read_i64z()?;
        let next_node = r.read_i64z()?;
        let group_id = if ver >= 13 { r.read_i64z()? } else { 0 };

        // Coordinate block
        let user_max_speed = if ver >= 72 { Some(r.read_f32()?) } else { None };
        let x = r.read_f64()?;
        let y = r.read_f64()?;
        if (102..=105).contains(&ver) { r.read_f32()?; }
        let user_tangent_delta = if ver >= 102 { Some(r.read_f32()?) } else { None };
        let next_spline_t = if ver >= 141 { Some(r.read_f32()?) } else { None };

        // Post-coordinate
        let station_group_id = r.read_i64z()?;
        let blueprint = if ver >= 108 { Some(r.read_i32z()?) } else { None };
        let (name, station_platform_auto_name) = if ver >= 63 {
            (Some(r.read_string()?), Some(r.read_raw_u8()?))
        } else {
            (None, None)
        };
        if (170..=181).contains(&ver) { r.read_f32()?; }
        if (15..=91).contains(&ver) { r.read_raw_u8()?; }
        let straight = if ver >= 62 { Some(r.read_raw_u8()?) } else { None };
        let tangential = if ver >= 143 { Some(r.read_raw_u8()?) } else { None };
        let limited_shapes = if ver >= 144 { Some(r.read_raw_u8()?) } else { None };

        // 4× conflict vectors
        let conflicts = if ver >= 28 {
            let mut cs: [Vec<Conflict>; 4] = Default::default();
            for cv in &mut cs {
                *cv = read_conflict_vec(r, ver)?;
            }
            cs
        } else {
            Default::default()
        };

        // Signal area
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
                Some(r.read_i64z()? as i32),
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
            node_id, node_type, track_type, layer, winding,
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
}

impl NrclipWrite for Track {
    fn nrclip_write(&self, w: &mut PayloadWriter, ver: u32) {
        w.write_i64z(self.node_id);
        if ver >= 30 { w.write_raw_u8(self.node_type); }
        if ver < 30 { w.write_i64z(0); }
        if ver >= 30 { w.write_i32z(self.track_type); }
        if ver < 30 { w.write_i64z(0); }
        if ver >= 45 { w.write_i32z(self.layer); }
        if ver >= 122 { w.write_raw_u8(self.winding.unwrap_or(0)); }
        w.write_i64z(self.prev_node);
        w.write_i64z(self.next_node);
        if ver >= 13 { w.write_i64z(self.group_id); }

        if ver >= 72 { w.write_f32(self.user_max_speed.unwrap_or(0.0)); }
        w.write_f64(self.x);
        w.write_f64(self.y);
        if (102..=105).contains(&ver) { w.write_f32(0.0); }
        if ver >= 102 { w.write_f32(self.user_tangent_delta.unwrap_or(0.0)); }
        if ver >= 141 { w.write_f32(self.next_spline_t.unwrap_or(0.5)); }

        w.write_i64z(self.station_group_id);
        if ver >= 108 { w.write_i32z(self.blueprint.unwrap_or(0)); }
        if ver >= 63 {
            w.write_string(self.name.as_deref().unwrap_or(""));
            w.write_raw_u8(self.station_platform_auto_name.unwrap_or(0));
        }
        if (170..=181).contains(&ver) { w.write_f32(0.0); }
        if (15..=91).contains(&ver) { w.write_raw_u8(0); }
        if ver >= 62 { w.write_raw_u8(self.straight.unwrap_or(0)); }
        if ver >= 143 { w.write_raw_u8(self.tangential.unwrap_or(0)); }
        if ver >= 144 { w.write_raw_u8(self.limited_shapes.unwrap_or(0)); }

        // Conflicts
        if ver >= 28 {
            for cv in &self.conflicts {
                w.write_varint(cv.len() as u64);
                for c in cv {
                    w.write_i64z(i64::from(c.mode));
                    w.write_i64z(c.track_id);
                    if (28..192).contains(&ver) { w.write_i64z(0); }
                    w.write_i32z(c.lat);
                    w.write_i32z(c.lon);
                    w.write_f32(c.t_self);
                    if ver >= 192 {
                        w.write_f32(c.t_other.unwrap_or(0.0));
                        w.write_f32(c.clearance.unwrap_or(0.0));
                        w.write_f32(c.height_delta.unwrap_or(0.0));
                    }
                    w.write_f32(c.overlap_dist);
                }
            }
        }

        // Signal area
        if (32..=197).contains(&ver) {
            w.write_vec_set_i64(self.embedded_signals.as_deref().unwrap_or(&[]));
        }
        if ver >= 198 {
            w.write_vec_set_i64(self.signal_ids.as_deref().unwrap_or(&[]));
        }

        w.write_i64z(self.attached_to_id);
        w.write_f64(self.attached_to_t);
        if ver >= 30 { w.write_i32z(self.attached_to_direction.unwrap_or(0)); }
        w.write_vec_set_i64(&self.attached_by);
        if ver >= 62 { w.write_vec_set_i64(self.building_attached_by.as_deref().unwrap_or(&[])); }

        if ver >= 33 {
            w.write_i64z(self.parallel_to_id.unwrap_or(0));
            w.write_i64z(i64::from(self.parallel_kind.unwrap_or(0)));
            w.write_f32(self.parallel_to_t.unwrap_or(0.0));
            w.write_i32z(self.parallel_to_direction.unwrap_or(0));
            w.write_f32(self.parallel_to_offset.unwrap_or(0.0));
        }
        if ver >= 60 { w.write_f32(self.parallel_to_disp.unwrap_or(0.0)); }
        if ver >= 33 { w.write_vec_set_i64(self.parallel_by.as_deref().unwrap_or(&[])); }
        if ver >= 192 { w.write_f32(self.proximity_diamond.unwrap_or(0.0)); }
    }
}

fn read_conflict_vec(r: &mut PayloadReader, ver: u32) -> Result<Vec<Conflict>> {
    let count = r.read_varint()? as usize;
    let mut conflicts = Vec::with_capacity(count);
    for _ in 0..count {
        let mode = r.read_i64z()? as i32;
        let track_id = r.read_i64z()?;
        if (28..192).contains(&ver) { r.read_i64z()?; }
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
