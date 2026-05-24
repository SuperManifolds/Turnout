fn main() {
    let path = std::env::args().nth(1).expect("usage: check_coords <file.nrclip>");
    let data = std::fs::read(&path).expect("read");
    let file = turnout_core::nrc1::NrclipFile::from_bytes(&data).expect("parse");
    for (ci, coll) in file.collections.iter().enumerate() {
        for (cli, clip) in coll.clips.iter().enumerate() {
            if clip.tracks.is_empty() { continue; }
            let (center_lat, center_lon) = turnout_core::geo::mercator_to_latlon(clip.center_x, clip.center_y);
            eprintln!("Coll {ci} clip {cli} \"{}\": center=({center_lat:.6}°, {center_lon:.6}°) tracks={}",
                coll.name, clip.tracks.len());
            let min_y = clip.tracks.iter().map(|t| t.y).fold(f64::MAX, f64::min);
            let max_y = clip.tracks.iter().map(|t| t.y).fold(f64::MIN, f64::max);
            eprintln!("  y range: {min_y:.1} to {max_y:.1} (span {:.1}m)", max_y - min_y);
            for ti in [0, clip.tracks.len() / 2, clip.tracks.len() - 1] {
                let t = &clip.tracks[ti];
                eprintln!("  track[{ti}]: offset=({:.2},{:.2})", t.x, t.y);
            }
        }
    }
}
