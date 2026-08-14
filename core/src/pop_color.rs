//! Colorization of population-density values into RGBA.
//!
//! NIMBY Rails' `pop400.pmtiles` stores population density as 16-bit grayscale
//! pixels (people per cell). Rendering that raw would be an unreadable grey
//! smear, so we map each density through a log transfer function onto a
//! perceptual color ramp with rising opacity: empty areas are transparent,
//! sparse population reads as faint purple, dense city cores as solid orange /
//! pale yellow. Kept dependency-free and pure so it is reused by the tile
//! server, the future editor previews, and unit tests.

/// Normalization reference for the log transfer: the density mapped to the top
/// of the ramp. Observed z10 peaks sit in the low hundreds; anything at or above
/// this saturates to the brightest ramp stop. Tunable (a future UI slider).
pub const V_REF: f64 = 400.0;

/// Ramp stops as `(t, r, g, b, a)` with `t` in `[0, 1]`, linearly interpolated.
/// Inferno/magma-style: dark purple → magenta → red → orange → pale yellow, with
/// alpha climbing from fully transparent so unpopulated cells show the base map.
const RAMP: &[(f64, u8, u8, u8, u8)] = &[
    (0.00, 0, 0, 0, 0),
    (0.15, 40, 11, 84, 60),
    (0.35, 101, 21, 110, 110),
    (0.55, 159, 42, 99, 150),
    (0.70, 212, 72, 66, 180),
    (0.85, 245, 125, 21, 205),
    (1.00, 252, 255, 164, 225),
];

/// Map a raw density value to `t` in `[0, 1]` via `ln(1 + v) / ln(1 + V_REF)`.
/// The log keeps both villages and megacities legible on one ramp.
#[must_use]
pub fn transfer(value: u16) -> f64 {
    if value == 0 {
        return 0.0;
    }
    let t = (1.0 + f64::from(value)).ln() / (1.0 + V_REF).ln();
    t.clamp(0.0, 1.0)
}

/// Sample the ramp at `t` in `[0, 1]`, returning straight (non-premultiplied)
/// RGBA.
#[must_use]
pub fn ramp(t: f64) -> [u8; 4] {
    let t = t.clamp(0.0, 1.0);
    let mut i = 0;
    while i + 1 < RAMP.len() && t > RAMP[i + 1].0 {
        i += 1;
    }
    let (t0, r0, g0, b0, a0) = RAMP[i];
    let (t1, r1, g1, b1, a1) = RAMP[(i + 1).min(RAMP.len() - 1)];
    let span = t1 - t0;
    let f = if span > 0.0 { (t - t0) / span } else { 0.0 };
    let lerp = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * f).round() as u8;
    [lerp(r0, r1), lerp(g0, g1), lerp(b0, b1), lerp(a0, a1)]
}

/// Convenience: density value straight to RGBA.
#[must_use]
pub fn color(value: u16) -> [u8; 4] {
    ramp(transfer(value))
}

/// Colorize a grid of density values into a straight-RGBA buffer (`w * h * 4`).
#[must_use]
pub fn colorize(values: &[u16], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 4];
    for (px, &v) in values.iter().take(width * height).enumerate() {
        let rgba = color(v);
        out[px * 4..px * 4 + 4].copy_from_slice(&rgba);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_transparent() {
        assert_eq!(color(0), [0, 0, 0, 0]);
    }

    #[test]
    fn transfer_is_monotonic_and_bounded() {
        assert_eq!(transfer(0), 0.0);
        let mut prev = 0.0;
        for v in [1u16, 5, 20, 80, 200, 400, 1000, u16::MAX] {
            let t = transfer(v);
            assert!((0.0..=1.0).contains(&t), "t out of range for {v}: {t}");
            assert!(t >= prev, "transfer not monotonic at {v}");
            prev = t;
        }
        assert!((transfer(V_REF as u16) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn denser_is_more_opaque() {
        assert!(color(5)[3] < color(80)[3]);
        assert!(color(80)[3] < color(400)[3]);
    }

    #[test]
    fn ramp_endpoints_match_table() {
        assert_eq!(ramp(0.0), [0, 0, 0, 0]);
        assert_eq!(ramp(1.0), [252, 255, 164, 225]);
    }

    #[test]
    fn colorize_fills_buffer() {
        let vals = [0u16, 400, 0, 400];
        let buf = colorize(&vals, 2, 2);
        assert_eq!(buf.len(), 16);
        assert_eq!(&buf[0..4], &[0, 0, 0, 0]);
        assert_eq!(&buf[4..8], &[252, 255, 164, 225]);
    }
}
