//! Pure formatting/estimation helpers for the tile-download panel: human-readable
//! counts, size/time estimates, and progress text. The component owns the signals
//! and clock; these functions take plain values so they stay testable.

/// Thresholds for the "M tiles" / "K tiles" count summary.
const MILLION: u64 = 1_000_000;
const THOUSAND: u64 = 1_000;

const AVG_TILE_KB: f64 = 5.0;
const TILES_PER_SEC: f64 = 120.0;
const VECTOR_AVG_TILE_KB: f64 = 14.0;
/// One composite request per tile over an HTTP/1.1 pool, streamed 64 concurrent.
/// Throughput is latency-bound and scales with concurrency; measured 250-550
/// tiles/s on a low-latency link, held conservative here for higher-latency ones.
const VECTOR_TILES_PER_SEC: f64 = 150.0;
/// Sprites (8) + glyph ranges (4 fonts × 256) fetched once per download.
const VECTOR_RESOURCE_REQUESTS: f64 = 1032.0;
/// Approximate on-disk payload of the glyph + sprite resources (~3.4 MB × 4 fonts
/// of full-coverage glyph PBFs, plus sprite sheets).
const VECTOR_RESOURCE_MB: f64 = 14.0;

/// A coarse duration: hours over an hour, minutes over a minute, else seconds.
pub fn format_duration(secs: f64) -> String {
    if secs > 3600.0 {
        format!("{:.1}h", secs / 3600.0)
    } else if secs > 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{secs:.0}s")
    }
}

/// A megabyte figure as "N.N GB" over a gigabyte, else "N MB".
fn format_size_mb(mb: f64) -> String {
    if mb > 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

/// A tile count as "N.NM tiles" / "N.NK tiles" / "N tiles".
pub fn format_tile_count(count: u64) -> String {
    if count > MILLION {
        format!("{:.1}M tiles", count as f64 / MILLION as f64)
    } else if count > THOUSAND {
        format!("{:.1}K tiles", count as f64 / THOUSAND as f64)
    } else {
        format!("{count} tiles")
    }
}

/// The pre-download estimate line: "N tiles · ~size · ~time". Vector downloads
/// carry heavier per-tile payloads plus a fixed glyph/sprite resource cost.
pub fn download_summary(count: u64, is_vector: bool) -> String {
    let (avg_kb, per_sec, extra_mb, extra_reqs) = if is_vector {
        (VECTOR_AVG_TILE_KB, VECTOR_TILES_PER_SEC, VECTOR_RESOURCE_MB, VECTOR_RESOURCE_REQUESTS)
    } else {
        (AVG_TILE_KB, TILES_PER_SEC, 0.0, 0.0)
    };
    let size_mb = count as f64 * avg_kb / 1024.0 + extra_mb;
    let time = format_duration((count as f64 + extra_reqs) / per_sec);
    format!("{} · ~{} · ~{time}", format_tile_count(count), format_size_mb(size_mb))
}

/// Completion percentage (0-100) counting both succeeded and failed tiles as done.
pub fn progress_pct(completed: u64, failed: u64, total: u64) -> f64 {
    let done = completed + failed;
    if total > 0 { done as f64 / total as f64 * 100.0 } else { 0.0 }
}

/// The live progress line: "done/total · speed · ETA · ~size, N failed", with the
/// size projected from the average completed-tile byte size and failures omitted
/// when there are none. `elapsed_s` is the wall time since the download started.
pub fn progress_summary(completed: u64, failed: u64, total: u64, bytes: u64, elapsed_s: f64) -> String {
    let done = completed + failed;
    let speed = if elapsed_s > 1.0 { done as f64 / elapsed_s } else { 0.0 };
    let remaining = total.saturating_sub(done);
    let eta = if speed > 0.0 { remaining as f64 / speed } else { 0.0 };

    let size_str = if completed > 0 {
        let est_total_mb = (bytes as f64 / completed as f64) * total as f64 / (1024.0 * 1024.0);
        format!(" · ~{}", format_size_mb(est_total_mb))
    } else {
        String::new()
    };
    let fail_str = if failed > 0 { format!(", {failed} failed") } else { String::new() };
    format!("{done}/{total} · {speed:.0} tiles/s · {} remaining{size_str}{fail_str}", format_duration(eta))
}
