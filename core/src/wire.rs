/// Wire-format primitives matching the game's serde vtable.
/// PayloadReader for deserialization, PayloadWriter for serialization.

use anyhow::Result;

// ══════════════════════════════════════════════════════════════════════
// Reader
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

    /// LEB128 unsigned varint.
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

    /// Zigzag-decoded signed i64.
    pub fn read_i64z(&mut self) -> Result<i64> {
        let v = self.read_varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    /// Zigzag-decoded signed i32.
    pub fn read_i32z(&mut self) -> Result<i32> {
        self.read_i64z().map(|v| v as i32)
    }

    /// Raw u8 (NOT varint).
    pub fn read_raw_u8(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            anyhow::bail!("EOF reading u8 at {}", self.pos);
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    /// Raw f32 LE.
    pub fn read_f32(&mut self) -> Result<f32> {
        if self.pos + 4 > self.data.len() {
            anyhow::bail!("EOF reading f32 at {}", self.pos);
        }
        let v = f32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Ok(v)
    }

    /// Raw f64 LE.
    pub fn read_f64(&mut self) -> Result<f64> {
        if self.pos + 8 > self.data.len() {
            anyhow::bail!("EOF reading f64 at {}", self.pos);
        }
        let v = f64::from_le_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        Ok(v)
    }

    /// String: varint(length) + raw UTF-8 bytes.
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

    /// vec_set<i64>: varint(count) + count × zigzag i64.
    pub fn read_vec_set_i64(&mut self) -> Result<Vec<i64>> {
        let count = self.read_varint()? as usize;
        let mut v = Vec::with_capacity(count);
        for _ in 0..count {
            v.push(self.read_i64z()?);
        }
        Ok(v)
    }

    /// Alias for read_vec_set_i64 (tags use the same wire format).
    pub fn read_tags(&mut self) -> Result<Vec<i64>> {
        self.read_vec_set_i64()
    }

    /// vec<u64>: varint(count) + count × unsigned varint.
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
    pub fn read_mod_rel_file(&mut self) -> Result<crate::types::ModRelFile> {
        let (workshop_id, path) = self.read_mod_source_pair()?;
        let name = self.read_string()?;
        Ok(crate::types::ModRelFile { workshop_id, path, name })
    }

    /// optional<pair<ModSource,string>>: bool flag + conditional pair.
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
// Writer
// ══════════════════════════════════════════════════════════════════════

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
