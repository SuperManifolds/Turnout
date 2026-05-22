/// NRC1 container: header + zstd + wyhash checksum.

use anyhow::{Context, Result};
use crate::wire::{PayloadReader, PayloadWriter};
use crate::types::{Collection, NrclipRead, NrclipWrite};

pub struct NrclipFile {
    pub version: u32,
    pub collections: Vec<Collection>,
}

impl NrclipFile {
    /// Parse from raw .nrclip file bytes (NRC1 header + zstd payload).
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let (version, payload) = Self::decode_container(data)?;
        Self::from_payload(&payload, version)
    }

    /// Parse from decompressed payload bytes.
    pub fn from_payload(payload: &[u8], version: u32) -> Result<Self> {
        let mut r = PayloadReader::new(payload);
        let count = r.read_varint()? as usize;
        let mut collections = Vec::with_capacity(count);
        for _ in 0..count {
            collections.push(Collection::nrclip_read(&mut r, version)?);
        }
        if r.remaining() > 0 {
            anyhow::bail!("{} trailing bytes at offset {} (payload size {})",
                r.remaining(), r.position(), payload.len());
        }
        Ok(NrclipFile { version, collections })
    }

    /// Serialize to raw .nrclip file bytes (NRC1 header + zstd payload).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let payload = self.to_payload();
        Self::encode_container(&payload, self.version)
    }

    /// Serialize to payload bytes (no NRC1 container).
    pub fn to_payload(&self) -> Vec<u8> {
        let mut w = PayloadWriter::new();
        w.write_varint(self.collections.len() as u64);
        for coll in &self.collections {
            coll.nrclip_write(&mut w, self.version);
        }
        w.into_bytes()
    }

    /// Decode NRC1 container: validate header, decompress zstd.
    pub fn decode_container(data: &[u8]) -> Result<(u32, Vec<u8>)> {
        use zstd_pure_rs::prelude::*;

        if data.len() < 32 || &data[0..4] != b"NRC1" {
            anyhow::bail!("not an NRC1 file (too short or bad magic)");
        }
        let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let uncompressed_size = u64::from_le_bytes(data[8..16].try_into().unwrap()) as usize;

        let compressed = &data[32..];
        let mut payload = vec![0u8; uncompressed_size];
        let decoded_size = ZSTD_decompress(&mut payload, compressed);
        if ERR_isError(decoded_size) {
            anyhow::bail!("zstd decompress failed");
        }
        payload.truncate(decoded_size);

        Ok((version, payload))
    }

    /// Encode NRC1 container: zstd compress + header + checksum.
    pub fn encode_container(payload: &[u8], version: u32) -> Result<Vec<u8>> {
        use zstd_pure_rs::prelude::*;

        let mut compressed = vec![0u8; ZSTD_compressBound(payload.len())];
        let c_size = ZSTD_compress(&mut compressed, payload, 3);
        if ERR_isError(c_size) {
            anyhow::bail!("zstd compress failed");
        }
        compressed.truncate(c_size);

        let checksum = crate::wyhash_nrc1::checksum(payload);
        let mut out = Vec::with_capacity(32 + compressed.len());
        out.extend_from_slice(b"NRC1");
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(&compressed);
        Ok(out)
    }
}
