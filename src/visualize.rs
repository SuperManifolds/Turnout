#![allow(dead_code)]

use anyhow::{Context, Result};
use binrw::BinRead;
use image::{Rgb, RgbImage};
use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use std::{env, fs::File, io::BufReader};

mod nrclip;
use nrclip::{parse_payload, Track};

#[derive(BinRead)]
#[br(little, magic = b"NRC1")]
struct NrcHeader {
    version: u32,
    uncompressed_size: u64,
    compressed_size: u64,
    checksum: u64,
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let path = args.get(1).map(|s| s.as_str())
        .unwrap_or("2949234540/blueprints.nrclip");
    let output = args.get(2).map(|s| s.as_str())
        .unwrap_or("tracks.png");
    let img_size: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2048);

    // Decode
    let f = File::open(path).with_context(|| format!("open {}", path))?;
    let mut r = BufReader::new(f);
    let header: NrcHeader = BinRead::read(&mut r).context("NRC1 header")?;
    let zstd_offset = r.stream_position()?;
    r.seek(SeekFrom::Start(zstd_offset))?;
    let buf = zstd::stream::decode_all(&mut r).context("zstd")?;
    let collections = parse_payload(&buf, header.version).context("payload")?;

    // Collect all tracks across all clips
    let mut all_tracks: Vec<&Track> = Vec::new();
    for coll in &collections {
        for clip in &coll.clips {
            all_tracks.extend(clip.tracks.iter());
        }
    }

    if all_tracks.is_empty() {
        anyhow::bail!("no tracks found");
    }

    println!("Rendering {} tracks from v{} file...", all_tracks.len(), header.version);

    // Build node ID → track lookup
    let node_map: HashMap<i64, &Track> = all_tracks.iter()
        .map(|t| (t.node_id, *t))
        .collect();

    // Build chains: walk prev→next links to form ordered polylines
    let chains = build_chains(&all_tracks, &node_map);
    println!("  {} chains extracted", chains.len());

    // Compute bounding box
    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;
    for t in &all_tracks {
        min_x = min_x.min(t.x);
        min_y = min_y.min(t.y);
        max_x = max_x.max(t.x);
        max_y = max_y.max(t.y);
    }

    let range_x = max_x - min_x;
    let range_y = max_y - min_y;
    let range = range_x.max(range_y).max(1.0);
    let cx = (min_x + max_x) / 2.0;
    let cy = (min_y + max_y) / 2.0;

    let margin = 0.05;
    let scale = (img_size as f64) * (1.0 - 2.0 * margin) / range;
    let offset_x = (img_size as f64) / 2.0;
    let offset_y = (img_size as f64) / 2.0;

    let to_px = |x: f64, y: f64| -> (f64, f64) {
        let px = (x - cx) * scale + offset_x;
        let py = (cy - y) * scale + offset_y; // flip Y
        (px, py)
    };

    // Create image
    let bg = Rgb([24u8, 24, 32]);
    let mut img = RgbImage::from_pixel(img_size, img_size, bg);

    let node_color = Rgb([255u8, 120, 60]);
    let station_color = Rgb([60u8, 255, 120]);
    let endpoint_color = Rgb([255u8, 255, 80]);

    // Collect layer stats for reporting
    let mut layer_counts = HashMap::new();
    for t in &all_tracks {
        *layer_counts.entry(t.layer).or_insert(0usize) += 1;
    }
    if layer_counts.len() > 1 || !layer_counts.contains_key(&0) {
        println!("  Layers: {:?}", layer_counts);
    }

    // Draw spline-interpolated track chains
    for chain in &chains {
        if chain.len() < 2 { continue; }

        let points: Vec<(f64, f64)> = chain.iter().map(|t| (t.x, t.y)).collect();

        for i in 0..points.len() - 1 {
            // Color by elevation layer of the segment start node
            let track_color = layer_color(chain[i].layer);

            let is_straight = chain[i].straight.unwrap_or(0) != 0
                || chain[i + 1].straight.unwrap_or(0) != 0;

            if is_straight {
                let (x1, y1) = to_px(points[i].0, points[i].1);
                let (x2, y2) = to_px(points[i + 1].0, points[i + 1].1);
                draw_line(&mut img, x1 as i32, y1 as i32, x2 as i32, y2 as i32, track_color);
            } else {
                // Spline mode: Catmull-Rom interpolation
                // For endpoints/junctions, mirror the tangent instead of using
                // a neighbor that might be on a different branch
                let p0 = if i > 0 {
                    points[i - 1]
                } else {
                    // Mirror: p0 = p1 - (p2 - p1) = 2*p1 - p2
                    (2.0 * points[i].0 - points[i + 1].0,
                     2.0 * points[i].1 - points[i + 1].1)
                };
                let p1 = points[i];
                let p2 = points[i + 1];
                let p3 = if i + 2 < points.len() {
                    points[i + 2]
                } else {
                    // Mirror: p3 = p2 + (p2 - p1) = 2*p2 - p1
                    (2.0 * points[i + 1].0 - points[i].0,
                     2.0 * points[i + 1].1 - points[i].1)
                };

                let subdiv = 16;
                let mut prev_px = to_px(p1.0, p1.1);
                for s in 1..=subdiv {
                    let t = s as f64 / subdiv as f64;
                    let pt = catmull_rom(p0, p1, p2, p3, t);
                    let cur_px = to_px(pt.0, pt.1);
                    draw_line(&mut img,
                        prev_px.0 as i32, prev_px.1 as i32,
                        cur_px.0 as i32, cur_px.1 as i32,
                        track_color);
                    prev_px = cur_px;
                }
            }
        }
    }

    // Draw nodes on top
    for t in &all_tracks {
        let (px, py) = to_px(t.x, t.y);
        let is_endpoint = t.prev_node == 0 || t.prev_node == -1
            || t.next_node == 0 || t.next_node == -1;
        let has_station = t.station_group_id != 0;

        let color = if has_station {
            station_color
        } else if is_endpoint {
            endpoint_color
        } else {
            node_color
        };
        let radius = if has_station { 3 } else if is_endpoint { 2 } else { 1 };
        draw_circle(&mut img, px as i32, py as i32, radius, color);
    }

    img.save(output).with_context(|| format!("save {}", output))?;
    println!("Saved {}x{} image to {}", img_size, img_size, output);
    println!("  Bounds: ({:.1}, {:.1}) to ({:.1}, {:.1})", min_x, min_y, max_x, max_y);
    println!("  Scale: {:.2} px/unit", scale);

    Ok(())
}

/// Map elevation layer to a color.
/// Ground (0) = white, elevated (>0) = green, underground (<0) = blue.
fn layer_color(layer: i32) -> Rgb<u8> {
    match layer {
        0 => Rgb([210, 210, 220]),          // ground: white
        1 => Rgb([80, 220, 80]),            // elevated +1: green
        2 => Rgb([50, 255, 50]),            // elevated +2: bright green
        3 => Rgb([30, 255, 100]),           // elevated +3: vivid green
        -1 => Rgb([80, 130, 220]),          // underground -1: blue
        -2 => Rgb([50, 100, 255]),          // underground -2: brighter blue
        -3 => Rgb([30, 70, 255]),           // underground -3: vivid blue
        n if n > 3 => Rgb([20, 255, 120]),  // very elevated: bright green
        n if n < -3 => Rgb([20, 50, 255]),  // deep underground: deep blue
        _ => Rgb([210, 210, 220]),
    }
}

/// Catmull-Rom spline interpolation between p1 and p2,
/// using p0 and p3 as tangent guides. t in [0, 1].
fn catmull_rom(p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), t: f64) -> (f64, f64) {
    let t2 = t * t;
    let t3 = t2 * t;

    let x = 0.5 * ((2.0 * p1.0)
        + (-p0.0 + p2.0) * t
        + (2.0 * p0.0 - 5.0 * p1.0 + 4.0 * p2.0 - p3.0) * t2
        + (-p0.0 + 3.0 * p1.0 - 3.0 * p2.0 + p3.0) * t3);

    let y = 0.5 * ((2.0 * p1.1)
        + (-p0.1 + p2.1) * t
        + (2.0 * p0.1 - 5.0 * p1.1 + 4.0 * p2.1 - p3.1) * t2
        + (-p0.1 + 3.0 * p1.1 - 3.0 * p2.1 + p3.1) * t3);

    (x, y)
}

/// Walk the prev/next graph to extract ordered chains of track nodes.
/// Each chain is a maximal sequence following next_node links.
fn build_chains<'a>(tracks: &[&'a Track], node_map: &HashMap<i64, &'a Track>) -> Vec<Vec<&'a Track>> {
    let mut visited = std::collections::HashSet::new();
    let mut chains = Vec::new();

    // Find chain starts: nodes with no prev, or whose prev doesn't point back
    for &t in tracks {
        if visited.contains(&t.node_id) { continue; }

        // Walk backwards to find the start of this chain
        let mut start = t;
        let mut seen = std::collections::HashSet::new();
        seen.insert(start.node_id);
        loop {
            if start.prev_node == 0 || start.prev_node == -1 { break; }
            match node_map.get(&start.prev_node) {
                Some(prev) if !seen.contains(&prev.node_id) => {
                    seen.insert(prev.node_id);
                    start = prev;
                }
                _ => break,
            }
        }

        // Walk forward from start
        let mut chain = Vec::new();
        let mut current = start;
        loop {
            if visited.contains(&current.node_id) { break; }
            visited.insert(current.node_id);
            chain.push(current);

            if current.next_node == 0 || current.next_node == -1 { break; }
            match node_map.get(&current.next_node) {
                Some(next) if !visited.contains(&next.node_id) => current = next,
                _ => break,
            }
        }

        if chain.len() >= 2 {
            chains.push(chain);
        }
    }

    chains
}

fn draw_line(img: &mut RgbImage, x0: i32, y0: i32, x1: i32, y1: i32, color: Rgb<u8>) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0;
    let mut y = y0;
    let (w, h) = (img.width() as i32, img.height() as i32);

    loop {
        if x >= 0 && x < w && y >= 0 && y < h {
            img.put_pixel(x as u32, y as u32, color);
        }
        if x == x1 && y == y1 { break; }
        let e2 = 2 * err;
        if e2 >= dy { err += dy; x += sx; }
        if e2 <= dx { err += dx; y += sy; }
    }
}

fn draw_circle(img: &mut RgbImage, cx: i32, cy: i32, r: i32, color: Rgb<u8>) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                let px = cx + dx;
                let py = cy + dy;
                if px >= 0 && px < w && py >= 0 && py < h {
                    img.put_pixel(px as u32, py as u32, color);
                }
            }
        }
    }
}
