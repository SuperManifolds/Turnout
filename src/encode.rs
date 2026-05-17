use anyhow::Result;

/// Wire-format writer matching the game's serde::Serializer vtable.
pub struct PayloadWriter {
    buf: Vec<u8>,
}

impl PayloadWriter {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(4096) }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// LEB128 unsigned varint.
    pub fn write_varint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                return;
            }
            self.buf.push(byte | 0x80);
        }
    }

    /// Zigzag-encoded signed i64.
    pub fn write_i64z(&mut self, v: i64) {
        let encoded = ((v << 1) ^ (v >> 63)) as u64;
        self.write_varint(encoded);
    }

    /// Zigzag-encoded signed i32.
    pub fn write_i32z(&mut self, v: i32) {
        self.write_i64z(v as i64);
    }

    /// Raw u8.
    pub fn write_raw_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Raw f32 LE.
    pub fn write_f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Raw f64 LE.
    pub fn write_f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// String: varint(length) + raw bytes.
    pub fn write_string(&mut self, s: &str) {
        self.write_varint(s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
    }

    /// vec_set<i64>: varint(count) + count × zigzag i64.
    pub fn write_vec_set_i64(&mut self, v: &[i64]) {
        self.write_varint(v.len() as u64);
        for &val in v { self.write_i64z(val); }
    }

    /// optional<pair<ModSource, string>>: u8 flag + conditional pair.
    pub fn write_optional_mod_source(&mut self, v: &Option<(i64, String)>) {
        match v {
            Some((id, path)) => {
                self.write_raw_u8(1);
                self.write_i64z(*id);
                self.write_string(path);
            }
            None => self.write_raw_u8(0),
        }
    }

    /// pair<ModSource, string>: i64z + string.
    pub fn write_mod_source_pair(&mut self, workshop_id: i64, path: &str) {
        self.write_i64z(workshop_id);
        self.write_string(path);
    }

    /// ModRelFile: pair<ModSource, string> + string name.
    pub fn write_mod_rel_file(&mut self, workshop_id: i64, path: &str, name: &str) {
        self.write_mod_source_pair(workshop_id, path);
        self.write_string(name);
    }
}
