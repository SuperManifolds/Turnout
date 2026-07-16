use crate::error::Result;
use std::fmt;
use crate::wire::{PayloadReader, PayloadWriter};
use super::{NrclipRead, NrclipWrite, ModRelFile};

// ══════════════════════════════════════════════════════════════════════
// TrackKind
// ══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct TrackTexture {
    pub speed_class: i32,
    pub files: [ModRelFile; 4],
}

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

impl NrclipRead for TrackKind {
    fn nrclip_read(r: &mut PayloadReader, ver: u32) -> Result<Self> {
        let display_name = r.read_string()?;
        let speed_class_flag = r.read_raw_u8()?;
        let speed_class = r.read_i32z()?;
        let internal_name = r.read_string()?;
        let secondary_name = r.read_string()?;

        let mut horizons_vec = Vec::with_capacity(3);
        for _ in 0..3 {
            horizons_vec.push(TrackKindHorizon::nrclip_read(r, ver)?);
        }
        let horizons: [TrackKindHorizon; 3] = horizons_vec.try_into().expect("3 horizons");

        Ok(TrackKind { display_name, speed_class_flag, speed_class, internal_name, secondary_name, horizons })
    }
}

impl NrclipWrite for TrackKind {
    fn nrclip_write(&self, w: &mut PayloadWriter, ver: u32) {
        w.write_string(&self.display_name);
        w.write_raw_u8(self.speed_class_flag);
        w.write_i32z(self.speed_class);
        w.write_string(&self.internal_name);
        w.write_string(&self.secondary_name);
        for h in &self.horizons {
            h.nrclip_write(w, ver);
        }
    }
}

impl NrclipRead for TrackKindHorizon {
    fn nrclip_read(r: &mut PayloadReader, _ver: u32) -> Result<Self> {
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
            let files: [ModRelFile; 4] = files.try_into().expect("4 texture files");
            textures.push(TrackTexture { speed_class: tex_speed, files });
        }
        Ok(TrackKindHorizon {
            speed_class, gauge, height, max_speed, width_a, width_b,
            spacing, offset_a, offset_b, visual_distance, flags, textures,
        })
    }
}

impl NrclipWrite for TrackKindHorizon {
    fn nrclip_write(&self, w: &mut PayloadWriter, _ver: u32) {
        w.write_i32z(self.speed_class);
        w.write_f64(self.gauge);
        w.write_f64(self.height);
        w.write_f64(self.max_speed);
        w.write_f64(self.width_a);
        w.write_f64(self.width_b);
        w.write_f64(self.spacing);
        w.write_f64(self.offset_a);
        w.write_f64(self.offset_b);
        w.write_i64z(self.visual_distance);
        for &f in &self.flags { w.write_raw_u8(f); }
        for tex in &self.textures {
            w.write_i32z(tex.speed_class);
            for file in &tex.files {
                w.write_mod_rel_file(file.workshop_id, &file.path, &file.name);
            }
        }
    }
}

// ══════════════════════════════════════════════════════════════════════
// BuildingKind
// ══════════════════════════════════════════════════════════════════════

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
    pub curved: Option<u8>,
    pub recolor: u8,
    pub is_poi: Option<u8>,
    pub has_default_size: Option<u8>,
    pub decal_count: Option<u8>,
    pub border_x: i32,
    pub lod_x: f32,
    pub lod_y: f32,
    pub sentinel: u32,
    pub offset_neg: f32,
    pub offset_pos: f32,
    pub scripts: Vec<String>,
    pub rule_x: i32,
    pub rule_y: i32,
    pub partial_repeat_x: Option<u8>,
    pub partial_repeat_y: Option<u8>,
    pub default_draw_layer: i32,
    pub texture: ModRelFile,
    pub model_path: String,
}

impl fmt::Display for BuildingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BuildingKind \"{}\" ({})", self.display_name, self.internal_name)
    }
}

impl NrclipRead for BuildingKind {
    fn nrclip_read(r: &mut PayloadReader, ver: u32) -> Result<Self> {
        let display_name = r.read_string()?;
        let speed_class_flag = r.read_raw_u8()?;
        let speed_class = r.read_i32z()?;
        let internal_name = r.read_string()?;
        let secondary_name = r.read_string()?;
        if ver == 62 { r.read_i32z()?; }
        let tags = r.read_tags()?;
        let size_x = r.read_f32()?;
        let size_y = r.read_f32()?;
        if (62..=181).contains(&ver) { r.read_raw_u8()?; }
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
        let _border_x_again = r.read_i32z()?;
        let (lod_x, lod_y, sentinel, offset_neg, offset_pos) = if ver >= 69 {
            (r.read_f32()?, r.read_f32()?, r.read_varint()? as u32, r.read_f32()?, r.read_f32()?)
        } else {
            (5.0, 5.0, 0xFFFF_FFFF, -2.5, 2.5)
        };
        if ver >= 69 { r.read_raw_u8()?; }
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
        if ver == 62 { r.read_mod_source_pair()?; r.read_string()?; }

        Ok(BuildingKind {
            display_name, speed_class_flag, speed_class, internal_name, secondary_name,
            tags, size_x, size_y, curved, recolor, is_poi, has_default_size, decal_count,
            border_x, lod_x, lod_y, sentinel,
            offset_neg, offset_pos, scripts, rule_x, rule_y, partial_repeat_x, partial_repeat_y,
            default_draw_layer, texture, model_path,
        })
    }
}

impl NrclipWrite for BuildingKind {
    fn nrclip_write(&self, w: &mut PayloadWriter, ver: u32) {
        w.write_string(&self.display_name);
        w.write_raw_u8(self.speed_class_flag);
        w.write_i32z(self.speed_class);
        w.write_string(&self.internal_name);
        w.write_string(&self.secondary_name);
        if ver == 62 { w.write_i32z(0); }
        w.write_vec_set_i64(&self.tags);
        w.write_f32(self.size_x);
        w.write_f32(self.size_y);
        if (62..=181).contains(&ver) { w.write_raw_u8(0); }
        if (167..=181).contains(&ver) { w.write_raw_u8(0); }
        if ver >= 65 { w.write_raw_u8(self.curved.unwrap_or(0)); }
        w.write_raw_u8(self.recolor);
        if ver >= 67 { w.write_raw_u8(self.is_poi.unwrap_or(0)); }
        if ver >= 69 {
            w.write_raw_u8(self.has_default_size.unwrap_or(0));
            w.write_raw_u8(self.decal_count.unwrap_or(0));
        }
        w.write_i32z(self.border_x);
        w.write_i32z(self.border_x); // read twice (game quirk)
        if ver >= 69 {
            w.write_f32(self.lod_x);
            w.write_f32(self.lod_y);
            w.write_varint(u64::from(self.sentinel));
            w.write_f32(self.offset_neg);
            w.write_f32(self.offset_pos);
            w.write_raw_u8(self.decal_count.unwrap_or(0)); // re-read (game quirk)
            w.write_varint(self.scripts.len() as u64);
            for s in &self.scripts { w.write_string(s); }
        }
        w.write_i32z(self.rule_x);
        w.write_i32z(self.rule_y);
        if ver >= 63 {
            w.write_raw_u8(self.partial_repeat_x.unwrap_or(1));
            w.write_raw_u8(self.partial_repeat_y.unwrap_or(1));
        }
        w.write_i32z(self.default_draw_layer);
        w.write_mod_source_pair(self.texture.workshop_id, &self.texture.path);
        w.write_string(&self.model_path);
        if ver == 62 { w.write_mod_source_pair(0, ""); w.write_string(""); }
    }
}
