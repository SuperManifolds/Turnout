use anyhow::Result;
use std::fmt;
use crate::wire::{PayloadReader, PayloadWriter};
use super::{NrclipRead, NrclipWrite};

#[derive(Debug, Clone)]
pub struct StationGroup {
    pub id: i64,
    pub created_on: Option<i32>,
    pub use_automatic_point: Option<u8>,
    pub position: Option<(f64, f64)>,
    pub name: String,
    pub use_automatic_name: u8,
    pub geo_name_pick: Option<i32>,
    pub tags: Option<Vec<i64>>,
    pub track_ids: Vec<i64>,
    pub building_ids: Option<Vec<i64>>,
    pub extra_ids: Option<Vec<i64>>,
    pub size_factor: Option<f32>,
    pub walk_factor: Option<f32>,
    pub max_platform_pax: Option<u32>,
    pub transfer_overflow_into_hall: Option<u32>,
    pub label_mode: Option<i32>,
    pub scripts: Option<u64>,
}

impl fmt::Display for StationGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Station \"{}\" id={} tracks={}", self.name, self.id, self.track_ids.len())
    }
}

impl NrclipRead for StationGroup {
    fn nrclip_read(r: &mut PayloadReader, ver: u32) -> Result<Self> {
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
            id, created_on, use_automatic_point, position, name, use_automatic_name,
            geo_name_pick, tags, track_ids, building_ids, extra_ids,
            size_factor, walk_factor, max_platform_pax, transfer_overflow_into_hall,
            label_mode, scripts,
        })
    }
}

impl NrclipWrite for StationGroup {
    fn nrclip_write(&self, w: &mut PayloadWriter, ver: u32) {
        w.write_i64z(self.id);
        if ver >= 11 { w.write_i32z(self.created_on.unwrap_or(0)); }
        if ver >= 182 { w.write_raw_u8(self.use_automatic_point.unwrap_or(0)); }
        if ver >= 182 {
            let (px, py) = self.position.unwrap_or((0.0, 0.0));
            w.write_f64(px);
            w.write_f64(py);
        }
        w.write_string(&self.name);
        w.write_raw_u8(self.use_automatic_name);
        if ver >= 57 { w.write_i32z(self.geo_name_pick.unwrap_or(0)); }
        if ver >= 182 { w.write_vec_set_i64(self.tags.as_deref().unwrap_or(&[])); }
        w.write_vec_set_i64(&self.track_ids);
        if ver >= 167 { w.write_vec_set_i64(self.building_ids.as_deref().unwrap_or(&[])); }
        if ver >= 195 { w.write_vec_set_i64(self.extra_ids.as_deref().unwrap_or(&[])); }
        if ver >= 4 { w.write_f32(self.size_factor.unwrap_or(1.0)); }
        if ver >= 163 { w.write_f32(self.walk_factor.unwrap_or(1.0)); }
        if ver >= 165 {
            w.write_varint(self.max_platform_pax.unwrap_or(0) as u64);
            w.write_varint(self.transfer_overflow_into_hall.unwrap_or(0) as u64);
        }
        if ver >= 94 { w.write_i32z(self.label_mode.unwrap_or(0)); }
        if ver >= 208 { w.write_varint(self.scripts.unwrap_or(0)); }
    }
}
