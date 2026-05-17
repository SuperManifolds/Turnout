/// NRC1 checksum: custom wyhash variant from NIMBYRails.exe RVA 0x1FEB0.
/// NOT standard wyhash — no initial seed mixing, different final mix.

const SEED: u64 = 0x9c3805fc2c85cacc;
const WYP1: u64 = 0xe7037ed1a0b428db;
const WYP2: u64 = 0x8ebc6af09c88c6e3;
const WYP3: u64 = 0x589965cc75374cc3;
const WYP4: u64 = 0x1d8e4e27c47d124f;

fn wymum(a: u64, b: u64) -> (u64, u64) {
    let r = (a as u128) * (b as u128);
    (r as u64, (r >> 64) as u64)
}

fn read64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
}

fn read32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}

pub fn checksum(data: &[u8]) -> u64 {
    let length = data.len();
    let mut r8 = SEED;
    let mut pos = 0;
    let mut remaining = length;

    // Main loop: 64 bytes per iteration, 4 lanes
    if length > 64 {
        let mut r11 = SEED;
        remaining -= 64;
        loop {
            let d0 = read64(data, pos);
            let d8 = read64(data, pos + 8);
            let d16 = read64(data, pos + 16);
            let d24 = read64(data, pos + 24);
            let d32 = read64(data, pos + 32);
            let d40 = read64(data, pos + 40);
            let d48 = read64(data, pos + 48);
            let d56 = read64(data, pos + 56);

            let (lo1, hi1) = wymum(r8 ^ d8, d0 ^ WYP1);
            let (lo2, hi2) = wymum(r8 ^ d24, d16 ^ WYP2);
            r8 = lo2 ^ hi2 ^ lo1 ^ hi1;

            let (lo3, hi3) = wymum(r11 ^ d40, d32 ^ WYP3);
            let (lo4, hi4) = wymum(r11 ^ d56, d48 ^ WYP4);
            r11 = lo4 ^ hi4 ^ lo3 ^ hi3;

            pos += 64;
            if remaining <= 64 { break; }
            remaining -= 64;
        }
        r8 ^= r11;
    }

    // Tail: 16-byte inner loop
    if remaining > 0x10 {
        let loop_count = ((remaining - 0x11) >> 4) + 1;
        remaining -= loop_count * 16;
        for _ in 0..loop_count {
            let a = read64(data, pos);
            let b = read64(data, pos + 8);
            r8 ^= b;
            let (lo, hi) = wymum(r8, a ^ WYP1);
            r8 = lo ^ hi;
            pos += 16;
        }
    }

    // Tail: final 0-16 bytes
    let (rdx_val, rax_val) = if remaining > 8 {
        (read64(data, pos), read64(data, pos + remaining - 8))
    } else if remaining >= 4 {
        (read32(data, pos) as u64, read32(data, pos + remaining - 4) as u64)
    } else if remaining > 0 {
        let b0 = data[pos] as u64;
        let b_mid = data[pos + (remaining >> 1)] as u64;
        let b_last = data[pos + remaining - 1] as u64;
        (((b0 << 8) | b_mid) << 8 | b_last, 0)
    } else {
        (0, 0)
    };

    // Final mix (two stages)
    let rax_val = rax_val ^ r8;
    let rdx_val = rdx_val ^ WYP1;
    let (lo, hi) = wymum(rax_val, rdx_val);
    let rax_val = lo ^ hi;

    let (lo, hi) = wymum(rax_val, (length as u64) ^ WYP1);
    lo ^ hi
}
