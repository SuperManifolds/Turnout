use anyhow::{Context, Result};
use binrw::BinRead;
use image::{Rgb, RgbImage};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
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

    // Margin: 5% padding
    let margin = 0.05;
    let scale = (img_size as f64) * (1.0 - 2.0 * margin) / range;
    let offset_x = (img_size as f64) / 2.0;
    let offset_y = (img_size as f64) / 2.0;

    let to_px = |x: f64, y: f64| -> (i32, i32) {
        let px = ((x - cx) * scale + offset_x) as i32;
        let py = ((cy - y) * scale + offset_y) as i32; // flip Y
        (px, py)
    };

    // Build node ID → track lookup
    let node_map: HashMap<i64, &Track> = all_tracks.iter()
        .map(|t| (t.node_id, *t))
        .collect();

    // Create image
    let bg = Rgb([24u8, 24, 32]);
    let mut img = RgbImage::from_pixel(img_size, img_size, bg);

    let track_color = Rgb([180u8, 200, 255]);
    let node_color = Rgb([255u8, 120, 60]);
    let station_color = Rgb([60u8, 255, 120]);
    let endpoint_color = Rgb([255u8, 255, 80]);

    // Draw track edges (lines between connected nodes)
    for t in &all_tracks {
        let (x1, y1) = to_px(t.x, t.y);
        if t.next_node != 0 && t.next_node != -1 {
            if let Some(next) = node_map.get(&t.next_node) {
                let (x2, y2) = to_px(next.x, next.y);
                draw_line(&mut img, x1, y1, x2, y2, track_color);
            }
        }
    }

    // Draw nodes on top of edges
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
        draw_circle(&mut img, px, py, radius, color);
    }

    img.save(output).with_context(|| format!("save {}", output))?;
    println!("Saved {}x{} image to {}", img_size, img_size, output);
    println!("  Bounds: ({:.1}, {:.1}) to ({:.1}, {:.1})", min_x, min_y, max_x, max_y);
    println!("  Scale: {:.2} px/unit", scale);

    Ok(())
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
