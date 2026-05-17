/// Compare Hobby spline rendering of a blueprint against raw OSM polylines.
/// Measures perpendicular deviation at sampled points along each chain.

use anyhow::{Context, Result};
use std::collections::HashMap;

mod encode;
mod hobby;
mod nrclip;
mod wyhash_nrc1;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let nrclip_path = args.get(1).context("usage: compare_orm <blueprint.nrclip> <tracks.json>")?;
    let json_path = args.get(2).context("usage: compare_orm <blueprint.nrclip> <tracks.json>")?;

    // Load OSM data
    let raw_json = std::fs::read_to_string(json_path)?;
    let data: serde_json::Value = serde_json::from_str(&raw_json)?;
    let elements = data["elements"].as_array().context("no elements")?;

    let mut osm_nodes: HashMap<u64, (f64, f64)> = HashMap::new();
    let mut osm_ways: Vec<Vec<u64>> = Vec::new();
    for e in elements {
        match e["type"].as_str() {
            Some("node") => {
                let id = e["id"].as_u64().unwrap();
                osm_nodes.insert(id, (e["lat"].as_f64().unwrap(), e["lon"].as_f64().unwrap()));
            }
            Some("way") => {
                let nids: Vec<u64> = e["nodes"].as_array().unwrap()
                    .iter().map(|n| n.as_u64().unwrap()).collect();
                if nids.len() >= 2 { osm_ways.push(nids); }
            }
            _ => {}
        }
    }

    // Load blueprint
    let raw = std::fs::read(nrclip_path)?;
    let ver = u32::from_le_bytes(raw[4..8].try_into().unwrap());
    let payload = zstd::stream::decode_all(&raw[32..])?;
    let colls = nrclip::parse_payload(&payload, ver)?;

    let clip = &colls[0].clips[0];
    let cx = clip.center_x;
    let cy = clip.center_y;

    // Convert OSM nodes to ground-meter offsets from center
    let center_lat = (cy / 6_378_137.0_f64).sinh().atan();
    let cos_lat = center_lat.cos();

    let osm_rel: HashMap<u64, (f64, f64)> = osm_nodes.iter().map(|(&nid, &(lat, lon))| {
        let mx = lon.to_radians() * 6_378_137.0;
        let my = (lat.to_radians() / 2.0 + std::f64::consts::FRAC_PI_4).tan().ln() * 6_378_137.0;
        (nid, ((mx - cx) * cos_lat, (my - cy) * cos_lat))
    }).collect();

    // Build OSM segment list for nearest-distance queries
    let mut osm_segments: Vec<((f64, f64), (f64, f64))> = Vec::new();
    for way in &osm_ways {
        let pts: Vec<(f64, f64)> = way.iter()
            .filter_map(|nid| osm_rel.get(nid).copied())
            .collect();
        for i in 0..pts.len().saturating_sub(1) {
            osm_segments.push((pts[i], pts[i + 1]));
        }
    }

    // Walk chains from blueprint
    let track_map: HashMap<i64, &nrclip::Track> = clip.tracks.iter()
        .map(|t| (t.node_id, t)).collect();

    let mut visited = std::collections::HashSet::new();
    let mut chains: Vec<Vec<&nrclip::Track>> = Vec::new();
    for t in &clip.tracks {
        if visited.contains(&t.node_id) { continue; }
        let mut cur = t.node_id;
        while let Some(p) = track_map.get(&cur) {
            if p.prev_node == 0 || visited.contains(&p.prev_node) { break; }
            cur = p.prev_node;
        }
        let mut chain = Vec::new();
        while let Some(node) = track_map.get(&cur) {
            if visited.contains(&cur) { break; }
            visited.insert(cur);
            chain.push(*node);
            cur = node.next_node;
        }
        if chain.len() >= 2 { chains.push(chain); }
    }

    // Measure deviation per chain
    let mut all_devs: Vec<f64> = Vec::new();
    let mut chain_stats: Vec<(usize, f64, f64, f64, f64)> = Vec::new(); // (nodes, length, avg, p95, max)

    for chain in &chains {
        let points: Vec<(f64, f64)> = chain.iter().map(|t| (t.x, t.y)).collect();
        let segments = hobby::hobby_spline(&points, 0.0);

        let mut devs = Vec::new();
        for seg in &segments {
            for s in 0..=16 {
                let pt = hobby::bezier_point(seg, s as f64 / 16.0);
                let d = nearest_segment_dist(pt, &osm_segments);
                devs.push(d);
            }
        }

        let chain_len: f64 = (0..points.len() - 1).map(|i| {
            let dx = points[i + 1].0 - points[i].0;
            let dy = points[i + 1].1 - points[i].1;
            (dx * dx + dy * dy).sqrt()
        }).sum();

        let max_dev = devs.iter().cloned().fold(0.0f64, f64::max);
        let avg_dev = devs.iter().sum::<f64>() / devs.len() as f64;
        let mut sorted = devs.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p95 = sorted[sorted.len() * 95 / 100];

        chain_stats.push((chain.len(), chain_len, avg_dev, p95, max_dev));
        all_devs.extend(devs);
    }

    // Report
    all_devs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let total = all_devs.len();
    let overall_avg = all_devs.iter().sum::<f64>() / total as f64;
    let overall_p95 = all_devs[total * 95 / 100];
    let overall_max = *all_devs.last().unwrap();

    println!("=== Hobby Spline vs OSM Deviation ===");
    println!("Chains: {}, sample points: {}", chains.len(), total);
    println!("Overall: avg={:.1}m  p95={:.1}m  max={:.1}m", overall_avg, overall_p95, overall_max);

    chain_stats.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap());
    println!("\nWorst chains:");
    for &(nodes, length, avg, p95, max) in chain_stats.iter().take(15) {
        println!("  {:>3} nodes  {:>6.0}m  avg={:>5.1}m  p95={:>5.1}m  max={:>6.1}m",
                 nodes, length, avg, p95, max);
    }

    println!("\nDeviation distribution:");
    for &thresh in &[1.0, 2.0, 5.0, 10.0, 20.0, 50.0] {
        let count = all_devs.iter().filter(|&&d| d <= thresh).count();
        let pct = count as f64 / total as f64 * 100.0;
        println!("  <={:>3.0}m: {:>5.1}% ({}/{})", thresh, pct, count, total);
    }

    Ok(())
}

fn nearest_segment_dist(pt: (f64, f64), segments: &[((f64, f64), (f64, f64))]) -> f64 {
    let mut best = f64::MAX;
    for &((ax, ay), (bx, by)) in segments {
        let dx = bx - ax;
        let dy = by - ay;
        let len_sq = dx * dx + dy * dy;
        let d = if len_sq < 1e-10 {
            ((pt.0 - ax).powi(2) + (pt.1 - ay).powi(2)).sqrt()
        } else {
            let t = ((pt.0 - ax) * dx + (pt.1 - ay) * dy) / len_sq;
            let t = t.clamp(0.0, 1.0);
            let proj_x = ax + t * dx;
            let proj_y = ay + t * dy;
            ((pt.0 - proj_x).powi(2) + (pt.1 - proj_y).powi(2)).sqrt()
        };
        if d < best { best = d; }
    }
    best
}
