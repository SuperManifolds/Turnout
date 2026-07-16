use crate::error::Result;
use crate::wire::{PayloadReader, PayloadWriter};
use super::{NrclipRead, NrclipWrite};

#[derive(Debug, Clone)]
pub struct DemandRange {
    pub min_distance: i32,
    pub max_distance: i32,
    pub step: i32,
    pub values: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct Demand {
    pub poi_layer_id: u64,
    pub is_mod: Option<u8>,
    pub mod_source: Option<(i64, String)>,
    pub name: String,
    pub time_a: [f32; 168],
    pub time_b: [f32; 168],
    pub distance_ranges: Vec<DemandRange>,
}

impl NrclipRead for Demand {
    fn nrclip_read(r: &mut PayloadReader, ver: u32) -> Result<Self> {
        let poi_layer_id = r.read_varint()?;
        let is_mod = if ver >= 159 { Some(r.read_raw_u8()?) } else { None };
        let mod_source = if ver >= 159 { r.read_optional_mod_source()? } else { None };
        let name = r.read_string()?;
        let mut time_a = [0.0f32; 168];
        for v in &mut time_a { *v = r.read_f32()?; }
        let mut time_b = [0.0f32; 168];
        for v in &mut time_b { *v = r.read_f32()?; }
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
}

impl NrclipWrite for Demand {
    fn nrclip_write(&self, w: &mut PayloadWriter, ver: u32) {
        w.write_varint(self.poi_layer_id);
        if ver >= 159 {
            w.write_raw_u8(self.is_mod.unwrap_or(0));
            w.write_optional_mod_source(&self.mod_source);
        }
        w.write_string(&self.name);
        for &v in &self.time_a { w.write_f32(v); }
        for &v in &self.time_b { w.write_f32(v); }
        w.write_varint(self.distance_ranges.len() as u64);
        for r in &self.distance_ranges {
            w.write_i32z(r.min_distance);
            w.write_i32z(r.max_distance);
            w.write_i32z(r.step);
            w.write_varint(r.values.len() as u64);
            for &v in &r.values { w.write_f32(v); }
        }
    }
}
