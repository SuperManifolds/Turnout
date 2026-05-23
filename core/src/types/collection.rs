use anyhow::{Context, Result};
use crate::wire::{PayloadReader, PayloadWriter};
use super::{Track, Signal, StationGroup, Building, TrackKind, BuildingKind, Demand, ModMeta, NrclipRead, NrclipWrite};

#[derive(Debug)]
pub struct Collection {
    pub id_a: u64,
    pub id_b: u64,
    pub mod_source: Option<(i64, String)>,
    pub name: String,
    pub clips: Vec<Clip>,
}

impl Default for Collection {
    fn default() -> Self {
        Collection {
            id_a: 0x0012_3456_7890,
            id_b: 0x0009_8765_4321,
            mod_source: None,
            name: String::new(),
            clips: Vec::new(),
        }
    }
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

impl Default for Clip {
    fn default() -> Self {
        Clip {
            guid: String::new(),
            clip_id: 0,
            center_x: 0.0,
            center_y: 0.0,
            tracks: Vec::new(),
            signals: Vec::new(),
            station_groups: Vec::new(),
            buildings: Vec::new(),
            track_kinds: Vec::new(),
            building_kinds: Vec::new(),
            demands: Vec::new(),
            mod_metas: Vec::new(),
        }
    }
}

impl NrclipRead for Collection {
    fn nrclip_read(r: &mut PayloadReader, ver: u32) -> Result<Self> {
        let (id_a, id_b) = if ver >= 71 {
            (r.read_varint()?, r.read_varint()?)
        } else {
            (0, 0)
        };
        let mod_source = if ver >= 71 { r.read_optional_mod_source()? } else { None };
        let name = if ver >= 66 { r.read_string()? } else { String::new() };
        let clip_count = if ver >= 66 { r.read_varint()? as usize } else { 0 };
        let mut clips = Vec::with_capacity(clip_count);
        for _ in 0..clip_count {
            clips.push(Clip::nrclip_read(r, ver)?);
        }
        Ok(Collection { id_a, id_b, mod_source, name, clips })
    }
}

impl NrclipWrite for Collection {
    fn nrclip_write(&self, w: &mut PayloadWriter, ver: u32) {
        if ver >= 71 {
            w.write_varint(self.id_a);
            w.write_varint(self.id_b);
            w.write_optional_mod_source(&self.mod_source);
        }
        if ver >= 66 { w.write_string(&self.name); }
        if ver >= 66 { w.write_varint(self.clips.len() as u64); }
        for clip in &self.clips {
            clip.nrclip_write(w, ver);
        }
    }
}

impl NrclipRead for Clip {
    fn nrclip_read(r: &mut PayloadReader, ver: u32) -> Result<Self> {
        let guid = if ver >= 66 { r.read_string()? } else { String::new() };
        let clip_id = if ver >= 66 { r.read_varint()? } else { 0 };
        let (center_x, center_y) = if ver >= 147 {
            (r.read_f64()?, r.read_f64()?)
        } else {
            (0.0, 0.0)
        };

        // Tracks
        let tracks = if ver >= 66 {
            let count = r.read_varint().with_context(|| format!("track count at {}", r.position()))? as usize;
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                v.push(Track::nrclip_read(r, ver).with_context(|| format!("track at {}", r.position()))?);
            }
            v
        } else { vec![] };

        // Section order: signals → stations → buildings → track_kinds → building_kinds → demands → mods
        let signals = if ver >= 198 {
            let count = r.read_varint().with_context(|| format!("signal count at {}", r.position()))? as usize;
            let mut v = Vec::with_capacity(count);
            for _ in 0..count { v.push(Signal::nrclip_read(r, ver)?); }
            v
        } else { vec![] };

        let station_groups = if ver >= 66 {
            let count = r.read_varint()? as usize;
            let mut v = Vec::with_capacity(count);
            for _ in 0..count { v.push(StationGroup::nrclip_read(r, ver)?); }
            v
        } else { vec![] };

        let buildings = if ver >= 66 {
            let count = r.read_varint()? as usize;
            let mut v = Vec::with_capacity(count);
            for _ in 0..count { v.push(Building::nrclip_read(r, ver)?); }
            v
        } else { vec![] };

        let track_kinds = if ver >= 66 {
            let count = r.read_varint()? as usize;
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                let key = r.read_i32z()?;
                v.push((key, TrackKind::nrclip_read(r, ver)?));
            }
            v
        } else { vec![] };

        let building_kinds = if ver >= 66 {
            let count = r.read_varint()? as usize;
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                let key = r.read_i32z()?;
                v.push((key, BuildingKind::nrclip_read(r, ver)?));
            }
            v
        } else { vec![] };

        let demands = if ver >= 158 {
            let count = r.read_varint()? as usize;
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                let key = r.read_varint()?;
                v.push((key, Demand::nrclip_read(r, ver)?));
            }
            v
        } else { vec![] };

        let mod_metas = if ver >= 66 {
            let count = r.read_varint()? as usize;
            let mut v = Vec::with_capacity(count);
            for _ in 0..count { v.push(ModMeta::nrclip_read(r, ver)?); }
            v
        } else { vec![] };

        Ok(Clip {
            guid, clip_id, center_x, center_y,
            tracks, signals, station_groups, buildings,
            track_kinds, building_kinds, demands, mod_metas,
        })
    }
}

impl NrclipWrite for Clip {
    fn nrclip_write(&self, w: &mut PayloadWriter, ver: u32) {
        if ver >= 66 { w.write_string(&self.guid); }
        if ver >= 66 { w.write_varint(self.clip_id); }
        if ver >= 147 {
            w.write_f64(self.center_x);
            w.write_f64(self.center_y);
        }

        if ver >= 66 {
            w.write_varint(self.tracks.len() as u64);
            for t in &self.tracks { t.nrclip_write(w, ver); }
        }
        if ver >= 198 {
            w.write_varint(self.signals.len() as u64);
            for s in &self.signals { s.nrclip_write(w, ver); }
        }
        if ver >= 66 {
            w.write_varint(self.station_groups.len() as u64);
            for sg in &self.station_groups { sg.nrclip_write(w, ver); }
        }
        if ver >= 66 {
            w.write_varint(self.buildings.len() as u64);
            for b in &self.buildings { b.nrclip_write(w, ver); }
        }
        if ver >= 66 {
            w.write_varint(self.track_kinds.len() as u64);
            for (key, tk) in &self.track_kinds {
                w.write_i32z(*key);
                tk.nrclip_write(w, ver);
            }
        }
        if ver >= 66 {
            w.write_varint(self.building_kinds.len() as u64);
            for (key, bk) in &self.building_kinds {
                w.write_i32z(*key);
                bk.nrclip_write(w, ver);
            }
        }
        if ver >= 158 {
            w.write_varint(self.demands.len() as u64);
            for (key, d) in &self.demands {
                w.write_varint(*key);
                d.nrclip_write(w, ver);
            }
        }
        if ver >= 66 {
            w.write_varint(self.mod_metas.len() as u64);
            for m in &self.mod_metas { m.nrclip_write(w, ver); }
        }
    }
}
