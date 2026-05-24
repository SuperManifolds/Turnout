// Empirically verify the coordinate formula for Nimby Rails blueprints.
// Tests Formula A [offset = (merc - center_merc) * cos(center_lat)]
// vs Formula C [x = R * cos(center_lat) * delta_lon, y = R * delta_lat]
// using game-created workshop blueprints as ground truth.

const R: f64 = 6_378_137.0;

fn merc_y_to_lat(merc_y: f64) -> f64 {
    (merc_y / R).sinh().atan()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: verify_coords <file.nrclip> [file2.nrclip] ...");
        std::process::exit(1);
    }

    for path in &args[1..] {
        eprintln!("\n{}", "=".repeat(70));
        eprintln!("FILE: {path}");
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => { eprintln!("  SKIP: {e}"); continue; }
        };
        let file = match turnout_core::nrc1::NrclipFile::from_bytes(&data) {
            Ok(f) => f,
            Err(e) => { eprintln!("  SKIP (parse): {e}"); continue; }
        };
        eprintln!("  version: {}", file.version);

        for (ci, coll) in file.collections.iter().enumerate() {
            for (cli, clip) in coll.clips.iter().enumerate() {
                if clip.tracks.len() < 10 { continue; }

                let center_lat = merc_y_to_lat(clip.center_y);
                let cos_center = center_lat.cos();

                let min_y = clip.tracks.iter().map(|t| t.y).fold(f64::MAX, f64::min);
                let max_y = clip.tracks.iter().map(|t| t.y).fold(f64::MIN, f64::max);
                let min_x = clip.tracks.iter().map(|t| t.x).fold(f64::MAX, f64::min);
                let max_x = clip.tracks.iter().map(|t| t.x).fold(f64::MIN, f64::max);
                let span = ((max_x - min_x).powi(2) + (max_y - min_y).powi(2)).sqrt();

                if span < 1000.0 { continue; }

                eprintln!("\n  Coll {ci} clip {cli} \"{}\": {} tracks, span {span:.0}m",
                    coll.name, clip.tracks.len());
                eprintln!("    center: ({:.6}, {:.6})",
                    center_lat.to_degrees(), (clip.center_x / R).to_degrees());
                eprintln!("    cos(center_lat) = {cos_center:.10}");
                eprintln!("    y: [{min_y:.1}, {max_y:.1}] span {:.1}m", max_y - min_y);

                run_analysis(clip, cos_center, center_lat);
            }
        }
    }
}

fn run_analysis(
    clip: &turnout_core::types::Clip,
    cos_center: f64,
    center_lat: f64,
) {
    use std::collections::HashMap;

    let id_to_idx: HashMap<i64, usize> = clip.tracks.iter()
        .enumerate()
        .map(|(i, t)| (t.node_id, i))
        .collect();

    let mut visited = vec![false; clip.tracks.len()];
    let mut longest_chain: Vec<usize> = Vec::new();

    for (start_idx, t) in clip.tracks.iter().enumerate() {
        if t.prev_node != 0 { continue; }
        if visited[start_idx] { continue; }
        let mut chain = vec![start_idx];
        visited[start_idx] = true;
        let mut current = start_idx;
        loop {
            let next_id = clip.tracks[current].next_node;
            if next_id == 0 { break; }
            let Some(&next_idx) = id_to_idx.get(&next_id) else { break };
            if visited[next_idx] { break; }
            visited[next_idx] = true;
            chain.push(next_idx);
            current = next_idx;
        }
        if chain.len() > longest_chain.len() {
            longest_chain = chain;
        }
    }

    if longest_chain.len() < 5 {
        eprintln!("    (no chain with 5+ nodes found)");
        return;
    }

    let chain = &longest_chain;
    let n = chain.len();
    let first = &clip.tracks[chain[0]];
    let last = &clip.tracks[chain[n - 1]];
    let chain_y_span = (last.y - first.y).abs();

    eprintln!("    Longest chain: {n} nodes, y_span={chain_y_span:.0}m");

    // TEST 1: Predicted endpoint lat/lon under each formula
    eprintln!();
    eprintln!("    ENDPOINT COORDINATES under each formula:");
    for (label, t) in [("FIRST", first), ("LAST", last)] {
        let lat_a = merc_y_to_lat(clip.center_y + t.y / cos_center);
        let lon_a = (clip.center_x + t.x / cos_center) / R;
        let lat_c = center_lat + t.y / R;
        eprintln!("      {label}: offset=({:.2}, {:.2})", t.x, t.y);
        eprintln!("        Formula A: ({:.6}, {:.6})", lat_a.to_degrees(), lon_a.to_degrees());
        eprintln!("        Formula C: ({:.6}, {:.6})", lat_c.to_degrees(), lon_a.to_degrees());
        eprintln!("        Diff: {:.4}m lat", R * (lat_a - lat_c).abs());
    }

    // TEST 2: Cumulative haversine vs offset distance
    let mut cum_offset = 0.0f64;
    let mut cum_hav_a = 0.0f64;
    let mut cum_hav_c = 0.0f64;

    let start = &clip.tracks[chain[0]];
    let mut prev_lat_a = merc_y_to_lat(clip.center_y + start.y / cos_center);
    let mut prev_lon_a = (clip.center_x + start.x / cos_center) / R;
    let mut prev_lat_c = center_lat + start.y / R;
    let mut prev_lon_c = prev_lon_a;
    let mut prev_x = start.x;
    let mut prev_y = start.y;

    for &idx in &chain[1..] {
        let t = &clip.tracks[idx];
        let dx = t.x - prev_x;
        let dy = t.y - prev_y;
        cum_offset += (dx * dx + dy * dy).sqrt();

        let lat_a = merc_y_to_lat(clip.center_y + t.y / cos_center);
        let lon_a = (clip.center_x + t.x / cos_center) / R;
        cum_hav_a += haversine(prev_lat_a, prev_lon_a, lat_a, lon_a);

        let lat_c = center_lat + t.y / R;
        let lon_c = (clip.center_x + t.x / cos_center) / R;
        cum_hav_c += haversine(prev_lat_c, prev_lon_c, lat_c, lon_c);

        prev_lat_a = lat_a;
        prev_lon_a = lon_a;
        prev_lat_c = lat_c;
        prev_lon_c = lon_c;
        prev_x = t.x;
        prev_y = t.y;
    }

    let err_a = (cum_hav_a - cum_offset).abs();
    let err_c = (cum_hav_c - cum_offset).abs();

    eprintln!();
    eprintln!("    CUMULATIVE DISTANCE along {n}-node chain:");
    eprintln!("      Offset (flat):  {cum_offset:.4}m");
    eprintln!("      Haversine (A):  {cum_hav_a:.4}m  residual={:.4}m", cum_hav_a - cum_offset);
    eprintln!("      Haversine (C):  {cum_hav_c:.4}m  residual={:.4}m", cum_hav_c - cum_offset);
    eprintln!("      |err_A| = {err_a:.4}m   |err_C| = {err_c:.4}m");
    if err_a < err_c {
        eprintln!("      ==> Formula A better by {:.4}m", err_c - err_a);
    } else {
        eprintln!("      ==> Formula C better by {:.4}m", err_a - err_c);
    }

    // TEST 3: Direct end-to-end
    let first = &clip.tracks[chain[0]];
    let last = &clip.tracks[chain[n - 1]];
    let off_dist = ((last.x - first.x).powi(2) + (last.y - first.y).powi(2)).sqrt();

    let lat1_a = merc_y_to_lat(clip.center_y + first.y / cos_center);
    let lon1_a = (clip.center_x + first.x / cos_center) / R;
    let lat2_a = merc_y_to_lat(clip.center_y + last.y / cos_center);
    let lon2_a = (clip.center_x + last.x / cos_center) / R;

    let lat1_c = center_lat + first.y / R;
    let lat2_c = center_lat + last.y / R;

    let hav_a = haversine(lat1_a, lon1_a, lat2_a, lon2_a);
    let hav_c = haversine(lat1_c, lon1_a, lat2_c, lon2_a);

    let eq_a = equirect_flat(lat1_a, lon1_a, lat2_a, lon2_a);
    let eq_c = equirect_flat(lat1_c, lon1_a, lat2_c, lon2_a);

    eprintln!();
    eprintln!("    END-TO-END (chain start to end):");
    eprintln!("      Offset distance: {off_dist:.4}m");
    eprintln!("      Haversine A:     {hav_a:.4}m  (err {:.4})", (hav_a - off_dist).abs());
    eprintln!("      Haversine C:     {hav_c:.4}m  (err {:.4})", (hav_c - off_dist).abs());
    eprintln!("      Equirect A:      {eq_a:.4}m  (err {:.4})", (eq_a - off_dist).abs());
    eprintln!("      Equirect C:      {eq_c:.4}m  (err {:.4})", (eq_c - off_dist).abs());

    // TEST 4: Formula difference magnitude
    eprintln!();
    eprintln!("    FORMULA DIFFERENCE at extremal offsets:");
    let min_t = clip.tracks.iter()
        .min_by(|a, b| a.y.partial_cmp(&b.y).expect("no NaN"))
        .expect("non-empty tracks");
    let max_t = clip.tracks.iter()
        .max_by(|a, b| a.y.partial_cmp(&b.y).expect("no NaN"))
        .expect("non-empty tracks");
    for (label, t) in [("min_y", min_t), ("max_y", max_t)] {
        let lat_a = merc_y_to_lat(clip.center_y + t.y / cos_center);
        let lat_c = center_lat + t.y / R;
        let diff_m = R * (lat_a - lat_c).abs();
        eprintln!("      {label}: offset_y={:.2}, lat_A={:.6}, lat_C={:.6}, diff={diff_m:.2}m",
            t.y, lat_a.to_degrees(), lat_c.to_degrees());
    }
}

fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().asin()
}

fn equirect_flat(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let dy = R * (lat2 - lat1);
    let dx = R * (lon2 - lon1) * ((lat1 + lat2) / 2.0).cos();
    (dx * dx + dy * dy).sqrt()
}
