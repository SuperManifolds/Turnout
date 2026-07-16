use crate::error::Result;
use crate::wire::{PayloadReader, PayloadWriter};
use super::{NrclipRead, NrclipWrite};

#[derive(Debug, Clone)]
pub struct Signal {
    pub id: i64,
    pub kind_enum: Option<u8>,
    pub name: Option<String>,
    pub kind: i32,
    pub signal_textures_hash: Option<u64>,
    pub pos_track_id: i64,
    pub pos_t: f64,
    pub dir_a: Option<i8>,
    pub dir_b: Option<i8>,
    pub side: i32,
    pub size: Option<i32>,
    pub rotate: Option<i32>,
    pub custom_alert_wait: Option<u8>,
    pub alert_wait: Option<i32>,
    pub match_block_facing: Option<u8>,
    pub check_beyond_stops: Option<u8>,
    pub filter: Option<i32>,
    pub filter_exception_tags: Option<Vec<u64>>,
    pub scripts: Option<u64>,
}

impl NrclipRead for Signal {
    fn nrclip_read(r: &mut PayloadReader, ver: u32) -> Result<Self> {
        let id = r.read_i64z()?;
        let kind_enum = if ver >= 202 { Some(r.read_raw_u8()?) } else { None };
        let name = if ver >= 202 { Some(r.read_string()?) } else { None };
        let kind = if ver >= 32 { r.read_i32z()? } else { 0 };
        let signal_textures_hash = if ver >= 211 { Some(r.read_varint()?) } else { None };
        let pos_track_id = if ver >= 32 { r.read_i64z()? } else { 0 };
        let pos_t = if ver >= 32 { r.read_f64()? } else { 0.0 };
        if (32..205).contains(&ver) { r.read_i32z()?; }
        let dir_a = if ver >= 205 { Some(r.read_raw_u8()? as i8) } else { None };
        let dir_b = if ver >= 205 { Some(r.read_raw_u8()? as i8) } else { None };
        let side = if ver >= 32 { r.read_i32z()? } else { 0 };
        let size = if ver >= 214 { Some(r.read_i32z()?) } else { None };
        let rotate = if ver >= 214 { Some(r.read_i32z()?) } else { None };
        if (44..=50).contains(&ver) { r.read_raw_u8()?; }
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
}

impl NrclipWrite for Signal {
    fn nrclip_write(&self, w: &mut PayloadWriter, ver: u32) {
        w.write_i64z(self.id);
        if ver >= 202 { w.write_raw_u8(self.kind_enum.unwrap_or(0)); }
        if ver >= 202 { w.write_string(self.name.as_deref().unwrap_or("")); }
        if ver >= 32 { w.write_i32z(self.kind); }
        if ver >= 211 { w.write_varint(self.signal_textures_hash.unwrap_or(0)); }
        if ver >= 32 { w.write_i64z(self.pos_track_id); }
        if ver >= 32 { w.write_f64(self.pos_t); }
        if (32..205).contains(&ver) { w.write_i32z(0); }
        if ver >= 205 {
            w.write_raw_u8(self.dir_a.unwrap_or(0) as u8);
            w.write_raw_u8(self.dir_b.unwrap_or(0) as u8);
        }
        if ver >= 32 { w.write_i32z(self.side); }
        if ver >= 214 { w.write_i32z(self.size.unwrap_or(0)); }
        if ver >= 214 { w.write_i32z(self.rotate.unwrap_or(0)); }
        if (44..=50).contains(&ver) { w.write_raw_u8(0); }
        if ver >= 55 {
            w.write_raw_u8(self.custom_alert_wait.unwrap_or(0));
            w.write_i32z(self.alert_wait.unwrap_or(0));
        }
        if ver >= 37 { w.write_raw_u8(self.match_block_facing.unwrap_or(0)); }
        if ver >= 194 { w.write_raw_u8(self.check_beyond_stops.unwrap_or(0)); }
        if ver >= 53 {
            w.write_i32z(self.filter.unwrap_or(0));
            let tags = self.filter_exception_tags.as_deref().unwrap_or(&[]);
            w.write_varint(tags.len() as u64);
            for &t in tags { w.write_varint(t); }
        }
        if ver >= 204 { w.write_varint(self.scripts.unwrap_or(0)); }
    }
}
