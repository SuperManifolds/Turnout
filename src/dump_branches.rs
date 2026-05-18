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
            dump_chains(clip);
        }
    }
    Ok(())
}

fn dump_chains(clip: &nrclip::Clip) {
    use std::collections::{HashMap, HashSet};
    let tmap: HashMap<i64, &nrclip::Track> = clip.tracks.iter().map(|t| (t.node_id, t)).collect();
    
    let mut visited = HashSet::new();
    let mut chains: Vec<Vec<i64>> = Vec::new();
    for t in &clip.tracks {
        if visited.contains(&t.node_id) { continue; }
        let mut cur = t.node_id;
        while let Some(p) = tmap.get(&cur) {
            if p.prev_node == 0 || visited.contains(&p.prev_node) { break; }
            cur = p.prev_node;
        }
        let mut ch = Vec::new();
        while let Some(node) = tmap.get(&cur) {
            if visited.contains(&cur) { break; }
            visited.insert(cur);
            ch.push(cur);
            cur = node.next_node;
        }
        chains.push(ch);
    }
    
    chains.sort_by(|a, b| b.len().cmp(&a.len()));
    println!("\nChains: {}", chains.len());
    println!("Lengths: {:?}", chains.iter().take(20).map(|c| c.len()).collect::<Vec<_>>());
    println!("1-node: {}, 2-node: {}, 3+: {}", 
        chains.iter().filter(|c| c.len() == 1).count(),
        chains.iter().filter(|c| c.len() == 2).count(),
        chains.iter().filter(|c| c.len() >= 3).count());
    
    // Show first few multi-node chains with detail
    for ch in chains.iter().filter(|c| c.len() >= 2).take(3) {
        println!("\n  Chain ({} nodes):", ch.len());
        for &nid in ch {
            let t = tmap[&nid];
            let att = if t.attached_to_id != 0 { format!(" ATT={}@{:.3}", t.attached_to_id, t.attached_to_t) } else { String::new() };
            let by = if !t.attached_by.is_empty() { format!(" BY={}", t.attached_by.len()) } else { String::new() };
            println!("    {} ({:.1},{:.1}) p={} n={}{}{}", nid, t.x, t.y, t.prev_node, t.next_node, att, by);
        }
    }
}
