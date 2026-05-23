//! Compare Hobby spline rendering of a blueprint against raw OSM polylines.
//! Measures perpendicular deviation at sampled points along each chain.

use anyhow::{Context, Result};
use std::collections::HashMap;

use turnout_core::hobby;
use turnout_core::nrc1::NrclipFile;

type Segment2D = ((f64, f64), (f64, f64));

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let nrclip_path = args.get(1).context("usage: compare_orm <blueprint.nrclip> [tracks.json]")?;
    let json_path = args.get(2).map(std::string::String::as_str);

    // Load OSM data (optional — if not provided, just render splines)
    let mut osm_nodes: HashMap<u64, (f64, f64)> = HashMap::new();
    let mut osm_ways: Vec<Vec<u64>> = Vec::new();
    if let Some(jp) = json_path {
        let raw_json = std::fs::read_to_string(jp)?;
        let data: serde_json::Value = serde_json::from_str(&raw_json)?;
        let elements = data["elements"].as_array().context("no elements")?;
        for e in elements {
            match e["type"].as_str() {
                Some("node") => {
                    let id = e["id"].as_u64().expect("OSM node id is u64");
                    osm_nodes.insert(id, (e["lat"].as_f64().expect("OSM node lat is f64"), e["lon"].as_f64().expect("OSM node lon is f64")));
                }
                Some("way") => {
                    let nids: Vec<u64> = e["nodes"].as_array().expect("OSM way nodes is array")
                        .iter().map(|n| n.as_u64().expect("OSM way node id is u64")).collect();
                    if nids.len() >= 2 { osm_ways.push(nids); }
                }
                _ => {}
            }
        }
    }

    // Load blueprint
    let raw = std::fs::read(nrclip_path)?;
    let file = NrclipFile::from_bytes(&raw)?;

    let clip = &file.collections[0].clips[0];
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
    let track_map: HashMap<i64, &turnout_core::types::Track> = clip.tracks.iter()
        .map(|t| (t.node_id, t)).collect();

    let mut visited = std::collections::HashSet::new();
    let mut chains: Vec<Vec<&turnout_core::types::Track>> = Vec::new();
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

    // Measure deviation per chain (only if OSM data available)
    if !osm_segments.is_empty() {
        let mut all_devs: Vec<f64> = Vec::new();
        let mut chain_stats: Vec<(usize, f64, f64, f64, f64)> = Vec::new();

        for chain in &chains {
            let points: Vec<(f64, f64)> = chain.iter().map(|t| (t.x, t.y)).collect();
            let segments = hobby::hobby_spline(&points, 0.0);

            let mut devs = Vec::new();
            for seg in &segments {
                for s in 0..=16 {
                    let pt = hobby::bezier_point(seg, f64::from(s) / 16.0);
                    let d = nearest_segment_dist(pt, &osm_segments);
                    devs.push(d);
                }
            }

            let chain_len: f64 = (0..points.len() - 1).map(|i| {
                let dx = points[i + 1].0 - points[i].0;
                let dy = points[i + 1].1 - points[i].1;
                (dx * dx + dy * dy).sqrt()
            }).sum();

            let max_dev = devs.iter().copied().fold(0.0f64, f64::max);
            let avg_dev = devs.iter().sum::<f64>() / devs.len() as f64;
            let mut sorted = devs.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).expect("deviation values are not NaN"));
            let p95 = sorted[sorted.len() * 95 / 100];

            chain_stats.push((chain.len(), chain_len, avg_dev, p95, max_dev));
            all_devs.extend(devs);
        }

        all_devs.sort_by(|a, b| a.partial_cmp(b).expect("deviation values are not NaN"));
        let total = all_devs.len();
        let overall_avg = all_devs.iter().sum::<f64>() / total as f64;
        let overall_p95 = all_devs[total * 95 / 100];
        let overall_max = *all_devs.last().expect("all_devs is non-empty");

        println!("=== Hobby Spline vs OSM Deviation ===");
        println!("Chains: {}, sample points: {}", chains.len(), total);
        println!("Overall: avg={overall_avg:.1}m  p95={overall_p95:.1}m  max={overall_max:.1}m");

        chain_stats.sort_by(|a, b| b.4.partial_cmp(&a.4).expect("deviation values are not NaN"));
        println!("\nWorst chains:");
        for &(nodes, length, avg, p95, max) in chain_stats.iter().take(15) {
            println!("  {nodes:>3} nodes  {length:>6.0}m  avg={avg:>5.1}m  p95={p95:>5.1}m  max={max:>6.1}m");
        }

        println!("\nDeviation distribution:");
        for &thresh in &[0.5, 1.0, 2.0, 5.0, 10.0, 20.0] {
            let count = all_devs.iter().filter(|&&d| d <= thresh).count();
            let pct = count as f64 / total as f64 * 100.0;
            println!("  <={thresh:>3.0}m: {pct:>5.1}% ({count}/{total})");
        }
    }

    // Render overlay image
    let img_path = format!("{}.png", nrclip_path.trim_end_matches(".nrclip"));
    render_overlay(&clip.tracks, &chains, &osm_ways, &osm_rel, &img_path)?;
    println!("Rendered: {img_path}");

    Ok(())
}

fn render_overlay(
    tracks: &[turnout_core::types::Track],
    chains: &[Vec<&turnout_core::types::Track>],
    osm_ways: &[Vec<u64>],
    osm_rel: &HashMap<u64, (f64, f64)>,
    path: &str,
) -> Result<()> {
    use image::{Rgb, RgbImage};

    let size = 4096u32;
    let margin = 50.0;

    // Compute bounds from both datasets
    let mut all_x: Vec<f64> = tracks.iter().map(|t| t.x).collect();
    let mut all_y: Vec<f64> = tracks.iter().map(|t| t.y).collect();
    for &(x, y) in osm_rel.values() { all_x.push(x); all_y.push(y); }

    let xmin = all_x.iter().copied().fold(f64::MAX, f64::min) - margin;
    let xmax = all_x.iter().copied().fold(f64::MIN, f64::max) + margin;
    let ymin = all_y.iter().copied().fold(f64::MAX, f64::min) - margin;
    let ymax = all_y.iter().copied().fold(f64::MIN, f64::max) + margin;

    let scale = (f64::from(size) / (xmax - xmin)).min(f64::from(size) / (ymax - ymin)) * 0.9;
    let to_px = |x: f64, y: f64| -> (i32, i32) {
        let px = (x - xmin) * scale + (f64::from(size) - (xmax - xmin) * scale) / 2.0;
        let py = f64::from(size) - ((y - ymin) * scale + (f64::from(size) - (ymax - ymin) * scale) / 2.0);
        (px as i32, py as i32)
    };

    let mut img = RgbImage::from_pixel(size, size, Rgb([30, 32, 40]));
    let red = Rgb([200, 50, 50]);
    let white = Rgb([255, 255, 255]);
    let yellow = Rgb([255, 255, 0]);

    // Draw OSM ways (red)
    for way in osm_ways {
        let pts: Vec<(i32, i32)> = way.iter()
            .filter_map(|nid| osm_rel.get(nid).map(|&(x, y)| to_px(x, y)))
            .collect();
        for i in 0..pts.len().saturating_sub(1) {
            draw_line(&mut img, pts[i].0, pts[i].1, pts[i+1].0, pts[i+1].1, red);
        }
    }

    // Draw Hobby spline curves (white)
    for chain in chains {
        if chain.len() < 2 { continue; }
        let points: Vec<(f64, f64)> = chain.iter().map(|t| (t.x, t.y)).collect();
        let segs = hobby::hobby_spline(&points, 0.0);
        for seg in &segs {
            let mut prev = to_px(seg.p0.0, seg.p0.1);
            for s in 1..=16 {
                let pt = hobby::bezier_point(seg, f64::from(s) / 16.0);
                let cur = to_px(pt.0, pt.1);
                draw_line(&mut img, prev.0, prev.1, cur.0, cur.1, white);
                prev = cur;
            }
        }
    }

    // Draw nodes (yellow)
    for t in tracks {
        let (px, py) = to_px(t.x, t.y);
        for dy in -2..=2i32 {
            for dx in -2..=2i32 {
                let x = px + dx;
                let y = py + dy;
                if x >= 0 && x < size as i32 && y >= 0 && y < size as i32 {
                    img.put_pixel(x as u32, y as u32, yellow);
                }
            }
        }
    }

    img.save(path)?;
    Ok(())
}

fn nearest_segment_dist(pt: (f64, f64), segments: &[Segment2D]) -> f64 {
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

fn draw_line(img: &mut image::RgbImage, x0: i32, y0: i32, x1: i32, y1: i32, color: image::Rgb<u8>) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    let (w, h) = (img.width() as i32, img.height() as i32);
    loop {
        if x >= 0 && x < w && y >= 0 && y < h { img.put_pixel(x as u32, y as u32, color); }
        if x == x1 && y == y1 { break; }
        let e2 = 2 * err;
        if e2 >= dy { err += dy; x += sx; }
        if e2 <= dx { err += dx; y += sy; }
    }
}
