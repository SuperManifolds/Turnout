use crate::error::Result;
use std::fmt;
use crate::wire::{PayloadReader, PayloadWriter};
use super::{NrclipRead, NrclipWrite};

#[derive(Debug, Clone)]
pub struct ModRelFile {
    pub workshop_id: i64,
    pub path: String,
    pub name: String,
}

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
    pub content_items: Vec<(i32, String, String)>,
    pub content_loaded: u8,
    pub has_local_data: u8,
}

impl fmt::Display for ModMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mod src={} \"{}\" path=\"{}\"",
            self.source_id, self.display_name, self.source_path)
    }
}

impl NrclipRead for ModMeta {
    fn nrclip_read(r: &mut PayloadReader, ver: u32) -> Result<Self> {
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
        Ok(ModMeta {
            source_id, source_path, folder, display_name, author, description,
            version, tag, provides, content_items, content_loaded, has_local_data,
        })
    }
}

impl NrclipWrite for ModMeta {
    fn nrclip_write(&self, w: &mut PayloadWriter, ver: u32) {
        w.write_i64z(self.source_id);
        w.write_string(&self.source_path);
        w.write_string(&self.folder);
        w.write_string(&self.display_name);
        w.write_string(&self.author);
        w.write_string(&self.description);
        w.write_string(&self.version);
        w.write_string(&self.tag);
        w.write_vec_set_i64(&self.provides);
        if ver >= 117 {
            w.write_varint(self.content_items.len() as u64);
            for (kt, kn, vn) in &self.content_items {
                w.write_i32z(*kt);
                w.write_string(kn);
                w.write_string(vn);
            }
        }
        w.write_raw_u8(self.content_loaded);
        w.write_raw_u8(self.has_local_data);
    }
}
