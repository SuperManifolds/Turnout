use anyhow::{Context, Result};
use std::fs;

use nimby_gen_core::nrc1::NrclipFile;
use nimby_gen_core::types::{Collection, Clip, Track};

const MODEL_VERSION: u32 = 226;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let output = args.iter().find(|a| !a.starts_with('-') && *a != &args[0])
        .cloned().unwrap_or_else(|| "generated.nrclip".to_string());

    let empty = args.iter().any(|a| a == "--empty");
    let simple = args.iter().any(|a| a == "--simple");
    let count: usize = args.iter().find_map(|a| a.strip_prefix("--count=").and_then(|s| s.parse().ok())).unwrap_or(170);

    let tracks: Vec<Track> = if empty {
        Vec::new()
    } else if simple {
        generate_simple_track(count)
    } else {
        generate_double_track()
    };
    println!("Generated {} track nodes", tracks.len());

    let file = NrclipFile {
        version: MODEL_VERSION,
        collections: vec![Collection {
            id_a: 5641124955619280206,
            id_b: 81985529216486895,
            name: "Rust Test".into(),
            clips: vec![Clip {
                guid: "test".into(),
                clip_id: 0xDEADBEEF,
                tracks,
                ..Clip::default()
            }],
            ..Collection::default()
        }],
    };

    let file_data = file.to_bytes()?;
    fs::write(&output, &file_data).with_context(|| format!("write {}", output))?;
    println!("Wrote {} bytes to {}", file_data.len(), output);

    let decoded = NrclipFile::from_bytes(&file_data)?;
    let total_tracks: usize = decoded.collections.iter()
        .flat_map(|c| &c.clips).map(|c| c.tracks.len()).sum();
    println!("Verified: {} tracks", total_tracks);

    Ok(())
}

fn generate_simple_track(count: usize) -> Vec<Track> {
    (0..count).map(|i| Track {
        node_id: (i + 1) as i64,
        x: (i as f64) * 50.0,
        y: 0.0,
        prev_node: if i == 0 { 0 } else { i as i64 },
        next_node: if i == count - 1 { 0 } else { (i + 2) as i64 },
        ..Track::default()
    }).collect()
}

fn generate_double_track() -> Vec<Track> {
    let track_spacing = 15.0;

    struct Waypoint { x: f64, y: f64, layer: i32 }
    struct Segment { length: f64, turn: f64, layer: i32, nodes: usize }

    let segments = vec![
        Segment { length: 300.0, turn: 0.0,    layer: 0,  nodes: 0 },
        Segment { length: 300.0, turn: 0.26,   layer: 0,  nodes: 1 },
        Segment { length: 200.0, turn: 0.0,    layer: 1,  nodes: 0 },
        Segment { length: 300.0, turn: -0.35,  layer: 0,  nodes: 1 },
        Segment { length: 200.0, turn: 0.0,    layer: 0,  nodes: 0 },
        Segment { length: 200.0, turn: 0.17,   layer: 0,  nodes: 1 },
        Segment { length: 150.0, turn: -0.12,  layer: -1, nodes: 0 },
        Segment { length: 200.0, turn: 0.0,    layer: -1, nodes: 0 },
        Segment { length: 200.0, turn: -0.08,  layer: 0,  nodes: 0 },
        Segment { length: 200.0, turn: 0.0,    layer: 0,  nodes: 0 },
    ];

    let mut waypoints: Vec<Waypoint> = Vec::new();
    let (mut x, mut y, mut heading) = (0.0f64, 0.0f64, 0.0f64);

    for seg in &segments {
        waypoints.push(Waypoint { x, y, layer: seg.layer });
        let total_steps = seg.nodes + 1;
        let step_len = seg.length / total_steps as f64;
        let step_turn = seg.turn / total_steps as f64;
        for s in 0..total_steps {
            heading += step_turn;
            x += heading.cos() * step_len;
            y += heading.sin() * step_len;
            if s < total_steps - 1 {
                waypoints.push(Waypoint { x, y, layer: seg.layer });
            }
        }
    }
    waypoints.push(Waypoint { x, y, layer: 0 });

    let n = waypoints.len();
    let id_base_2 = (n as i64) + 1;
    let mut nodes = Vec::new();

    // Track 1
    for i in 0..n {
        let id = (i + 1) as i64;
        nodes.push(Track {
            node_id: id,
            x: waypoints[i].x, y: waypoints[i].y, layer: waypoints[i].layer,
            prev_node: if i == 0 { 0 } else { id - 1 },
            next_node: if i == n - 1 { 0 } else { id + 1 },
            ..Track::default()
        });
    }

    // Track 2 (parallel offset)
    for i in 0..n {
        let id = id_base_2 + i as i64;
        let (dx, dy) = if i + 1 < n {
            (waypoints[i+1].x - waypoints[i].x, waypoints[i+1].y - waypoints[i].y)
        } else {
            (waypoints[i].x - waypoints[i-1].x, waypoints[i].y - waypoints[i-1].y)
        };
        let len = (dx * dx + dy * dy).sqrt().max(0.001);
        nodes.push(Track {
            node_id: id,
            x: waypoints[i].x + dy / len * track_spacing,
            y: waypoints[i].y - dx / len * track_spacing,
            layer: waypoints[i].layer,
            prev_node: if i == 0 { 0 } else { id - 1 },
            next_node: if i == n - 1 { 0 } else { id + 1 },
            ..Track::default()
        });
    }

    nodes
}
