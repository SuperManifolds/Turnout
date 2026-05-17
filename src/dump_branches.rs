use anyhow::Result;
use std::collections::HashMap;

mod nrclip;

fn main() -> Result<()> {
    let path = std::env::args().nth(1).unwrap();
    let raw = std::fs::read(&path)?;
    let ver = u32::from_le_bytes(raw[4..8].try_into().unwrap());
    let payload = zstd::stream::decode_all(&raw[32..])?;
    let colls = nrclip::parse_payload(&payload, ver)?;
    
    for coll in &colls {
        for clip in &coll.clips {
            let tmap: HashMap<i64, &nrclip::Track> = clip.tracks.iter().map(|t| (t.node_id, t)).collect();
            
            println!("Clip: {} tracks", clip.tracks.len());
            
            for t in &clip.tracks {
                if t.attached_to_id == 0 { continue; }
                let dir = t.attached_to_direction.unwrap_or(0);
                let parent = tmap.get(&t.attached_to_id);
                
                if let Some(p) = parent {
                    let seg_end_id = if dir >= 0 { p.next_node } else { p.prev_node };
                    if let Some(e) = tmap.get(&seg_end_id) {
                        let sx = e.x - p.x;
                        let sy = e.y - p.y;
                        let seg_len = (sx*sx + sy*sy).sqrt();
                        let bx = t.x - p.x;
                        let by = t.y - p.y;
                        let geo_t = if seg_len > 0.001 { (bx*sx + by*sy) / (sx*sx + sy*sy) } else { 0.0 };
                        let perp_dist = if seg_len > 0.001 {
                            ((bx*sy - by*sx) / seg_len).abs()
                        } else { 0.0 };
                        println!("  branch={} parent={} stored_t={:.6} geo_t={:.6} dir={} seg={:.1}m perp={:.2}m",
                            t.node_id, t.attached_to_id, t.attached_to_t, geo_t, dir, seg_len, perp_dist);
                    } else {
                        println!("  branch={} parent={} stored_t={:.6} dir={} seg_end={} MISSING",
                            t.node_id, t.attached_to_id, t.attached_to_t, dir, seg_end_id);
                    }
                } else {
                    println!("  branch={} parent={} PARENT_MISSING", t.node_id, t.attached_to_id);
                }
            }
        }
    }
    Ok(())
}
