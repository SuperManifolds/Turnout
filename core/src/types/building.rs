use crate::error::Result;
use std::fmt;
use crate::wire::{PayloadReader, PayloadWriter};
use super::{NrclipRead, NrclipWrite};

#[derive(Debug, Clone)]
pub struct BuildingPoi {
    pub name: String,
    pub font_size: i32,
    pub max_zoom: i32,
    pub fill_background: u8,
    pub demand_curve: Option<u64>,
    pub population: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Building {
    pub id: i64,
    pub kind_idx: i32,
    pub kind_decal_idx: Option<i32>,
    pub owner: i64,
    pub created_on: i32,
    pub layer: i32,
    pub draw_layer: Option<i32>,
    pub blueprint: u8,
    pub x: f64,
    pub y: f64,
    pub rotation_sin: f32,
    pub rotation_cos: f32,
    pub size_x: f32,
    pub size_y: f32,
    pub color: u32,
    pub decal_color: Option<u32>,
    pub poi: Option<BuildingPoi>,
    pub attached_to_track_id: i64,
    pub start_t: f32,
    pub end_t: f32,
    pub bottom_side: f32,
    pub top_side: f32,
    pub attached_curved: u8,
}

impl fmt::Display for Building {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let angle = self.rotation_sin.atan2(self.rotation_cos).to_degrees();
        write!(f, "Building id={} kind={} ({:.2}, {:.2}) angle={:.1}°",
            self.id, self.kind_idx, self.x, self.y, angle)
    }
}

impl NrclipRead for Building {
    fn nrclip_read(r: &mut PayloadReader, ver: u32) -> Result<Self> {
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
}

impl NrclipWrite for Building {
    fn nrclip_write(&self, w: &mut PayloadWriter, ver: u32) {
        w.write_i64z(self.id);
        w.write_i32z(self.kind_idx);
        if ver >= 69 { w.write_i32z(self.kind_decal_idx.unwrap_or(0)); }
        w.write_i64z(self.owner);
        w.write_i32z(self.created_on);
        w.write_i32z(self.layer);
        if ver >= 63 { w.write_i32z(self.draw_layer.unwrap_or(0)); }
        w.write_raw_u8(self.blueprint);
        w.write_f64(self.x);
        w.write_f64(self.y);
        w.write_f32(self.rotation_sin);
        w.write_f32(self.rotation_cos);
        w.write_f32(self.size_x);
        w.write_f32(self.size_y);
        w.write_varint(u64::from(self.color));
        if ver >= 69 { w.write_varint(u64::from(self.decal_color.unwrap_or(0))); }

        if ver >= 67 {
            match &self.poi {
                Some(poi) => {
                    w.write_raw_u8(1);
                    w.write_string(&poi.name);
                    w.write_i32z(poi.font_size);
                    if ver >= 68 { w.write_i32z(poi.max_zoom); }
                    w.write_raw_u8(poi.fill_background);
                    if ver >= 158 {
                        match poi.demand_curve {
                            Some(v) => { w.write_raw_u8(1); w.write_varint(v); }
                            None => w.write_raw_u8(0),
                        }
                        w.write_varint(u64::from(poi.population.unwrap_or(0)));
                    }
                }
                None => w.write_raw_u8(0),
            }
        }

        w.write_i64z(self.attached_to_track_id);
        w.write_f32(self.start_t);
        w.write_f32(self.end_t);
        w.write_f32(self.bottom_side);
        w.write_f32(self.top_side);
        w.write_raw_u8(self.attached_curved);
    }
}
