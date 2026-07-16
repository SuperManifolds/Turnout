//! Wire-format primitives matching the game's serde vtable.
//! `PayloadReader` for deserialization, `PayloadWriter` for serialization.

use crate::error::{CoreError, Result};

// ══════════════════════════════════════════════════════════════════════
// Reader
// ══════════════════════════════════════════════════════════════════════

pub struct PayloadReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PayloadReader<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }
    #[must_use]
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
                return Err(CoreError::UnexpectedEof { reading: "varint", pos: start });
            }
            let b = self.data[self.pos];
            self.pos += 1;
            result |= u64::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err(CoreError::VarintOverflow { pos: start });
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
            return Err(CoreError::UnexpectedEof { reading: "u8", pos: self.pos });
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    /// Raw f32 LE.
    pub fn read_f32(&mut self) -> Result<f32> {
        if self.pos + 4 > self.data.len() {
            return Err(CoreError::UnexpectedEof { reading: "f32", pos: self.pos });
        }
        let v = f32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().expect("4-byte slice"));
        self.pos += 4;
        Ok(v)
    }

    /// Raw f64 LE.
    pub fn read_f64(&mut self) -> Result<f64> {
        if self.pos + 8 > self.data.len() {
            return Err(CoreError::UnexpectedEof { reading: "f64", pos: self.pos });
        }
        let v = f64::from_le_bytes(self.data[self.pos..self.pos + 8].try_into().expect("8-byte slice"));
        self.pos += 8;
        Ok(v)
    }

    /// String: varint(length) + raw UTF-8 bytes.
    pub fn read_string(&mut self) -> Result<String> {
        let len = self.read_varint()? as usize;
        if self.pos + len > self.data.len() {
            return Err(CoreError::UnexpectedEof { reading: "string", pos: self.pos });
        }
        let bytes = &self.data[self.pos..self.pos + len];
        self.pos += len;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| CoreError::InvalidUtf8 { pos: self.pos - len })
    }

    /// `vec_set`<i64>: varint(count) + count × zigzag i64.
    pub fn read_vec_set_i64(&mut self) -> Result<Vec<i64>> {
        let count = self.read_varint()? as usize;
        let mut v = Vec::with_capacity(count);
        for _ in 0..count {
            v.push(self.read_i64z()?);
        }
        Ok(v)
    }

    /// Alias for `read_vec_set_i64` (tags use the same wire format).
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

    /// pair<`ModSource`, string>: i64z `workshop_id` + string path.
    pub fn read_mod_source_pair(&mut self) -> Result<(i64, String)> {
        let workshop_id = self.read_i64z()?;
        let path = self.read_string()?;
        Ok((workshop_id, path))
    }

    /// `ModRelFile`: pair<`ModSource`, string> + string name.
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

impl Default for PayloadWriter {
    fn default() -> Self {
        Self { buf: Vec::with_capacity(4096) }
    }
}

impl PayloadWriter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
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
        self.write_i64z(i64::from(v));
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

    /// `vec_set`<i64>: varint(count) + count × zigzag i64.
    pub fn write_vec_set_i64(&mut self, v: &[i64]) {
        self.write_varint(v.len() as u64);
        for &val in v { self.write_i64z(val); }
    }

    /// optional<pair<`ModSource`, string>>: u8 flag + conditional pair.
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

    /// pair<`ModSource`, string>: i64z + string.
    pub fn write_mod_source_pair(&mut self, workshop_id: i64, path: &str) {
        self.write_i64z(workshop_id);
        self.write_string(path);
    }

    /// `ModRelFile`: pair<`ModSource`, string> + string name.
    pub fn write_mod_rel_file(&mut self, workshop_id: i64, path: &str, name: &str) {
        self.write_mod_source_pair(workshop_id, path);
        self.write_string(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip every scalar writer through its matching reader.
    #[test]
    fn varint_roundtrips_across_leb128_boundaries() {
        for v in [0u64, 1, 127, 128, 16_383, 16_384, 300, u64::from(u32::MAX), u64::MAX] {
            let mut w = PayloadWriter::new();
            w.write_varint(v);
            let bytes = w.into_bytes();
            let mut r = PayloadReader::new(&bytes);
            assert_eq!(r.read_varint().expect("read"), v, "varint {v}");
            assert_eq!(r.remaining(), 0, "consumed all bytes for {v}");
        }
    }

    #[test]
    fn zigzag_i64_roundtrips_including_extremes() {
        for v in [0i64, 1, -1, 2, -2, i64::from(i32::MIN), i64::MIN, i64::MAX, -123_456_789] {
            let mut w = PayloadWriter::new();
            w.write_i64z(v);
            let bytes = w.into_bytes();
            let mut r = PayloadReader::new(&bytes);
            assert_eq!(r.read_i64z().expect("read"), v, "i64z {v}");
        }
    }

    #[test]
    fn zigzag_i32_roundtrips_including_extremes() {
        for v in [0i32, 1, -1, i32::MIN, i32::MAX, -70_000] {
            let mut w = PayloadWriter::new();
            w.write_i32z(v);
            let bytes = w.into_bytes();
            let mut r = PayloadReader::new(&bytes);
            assert_eq!(r.read_i32z().expect("read"), v, "i32z {v}");
        }
    }

    #[test]
    fn float_and_byte_roundtrip() {
        let mut w = PayloadWriter::new();
        w.write_raw_u8(0xAB);
        w.write_f32(std::f32::consts::PI);
        w.write_f64(-1.234_567_890_123e42);
        let bytes = w.into_bytes();
        let mut r = PayloadReader::new(&bytes);
        assert_eq!(r.read_raw_u8().expect("u8"), 0xAB);
        assert_eq!(r.read_f32().expect("f32"), std::f32::consts::PI);
        assert_eq!(r.read_f64().expect("f64"), -1.234_567_890_123e42);
    }

    #[test]
    fn string_roundtrip_empty_unicode_and_long() {
        for s in ["", "a", "Bahnhof Zürich HB · 日本語 🚆", &"x".repeat(5000)] {
            let mut w = PayloadWriter::new();
            w.write_string(s);
            let bytes = w.into_bytes();
            let mut r = PayloadReader::new(&bytes);
            assert_eq!(r.read_string().expect("read"), s);
        }
    }

    #[test]
    fn vec_set_i64_roundtrip() {
        for v in [vec![], vec![0], vec![-1, 1, i64::MIN, i64::MAX, 42]] {
            let mut w = PayloadWriter::new();
            w.write_vec_set_i64(&v);
            let bytes = w.into_bytes();
            let mut r = PayloadReader::new(&bytes);
            assert_eq!(r.read_vec_set_i64().expect("read"), v);
        }
    }

    #[test]
    fn truncated_varint_errors_instead_of_panicking() {
        // 0x80 = "continuation bit set" but no following byte -> clean EOF error.
        let mut r = PayloadReader::new(&[0x80]);
        assert!(r.read_varint().is_err());
    }

    #[test]
    fn string_with_length_past_eof_errors() {
        // Claims length 10 but has no payload bytes.
        let mut w = PayloadWriter::new();
        w.write_varint(10);
        let bytes = w.into_bytes();
        let mut r = PayloadReader::new(&bytes);
        assert!(r.read_string().is_err());
    }
}
