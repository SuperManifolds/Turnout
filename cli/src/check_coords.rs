fn main() {
    let r = 6_378_137.0_f64;
    let path = std::env::args().nth(1).expect("usage: check_coords <file.nrclip>");
    let data = std::fs::read(&path).expect("read");
    let file = turnout_core::nrc1::NrclipFile::from_bytes(&data).expect("parse");
    for (ci, coll) in file.collections.iter().enumerate() {
        for (cli, clip) in coll.clips.iter().enumerate() {
            if clip.tracks.is_empty() { continue; }
            let center_lat = (clip.center_y / r).sinh().atan();
            let cos_lat = center_lat.cos();
            eprintln!("Coll {} clip {} \"{}\": center=({:.6}°, {:.6}°) cos={:.10} tracks={}",
                ci, cli, coll.name,
                center_lat.to_degrees(), (clip.center_x / r).to_degrees(),
                cos_lat, clip.tracks.len());
            let min_y = clip.tracks.iter().map(|t| t.y).fold(f64::MAX, f64::min);
            let max_y = clip.tracks.iter().map(|t| t.y).fold(f64::MIN, f64::max);
            eprintln!("  y range: {:.1} to {:.1} (span {:.1}m)", min_y, max_y, max_y-min_y);
            for ti in [0, clip.tracks.len()/2, clip.tracks.len()-1] {
                let t = &clip.tracks[ti];
                // X: merc_x = center_x + offset_x / cos(center_lat)
                let mx = clip.center_x + t.x / cos_lat;
                // Y (equirectangular): lat = center_lat + offset_y / R
                let lat = (center_lat + t.y / r).to_degrees();
                let lon = (mx / r).to_degrees();
                eprintln!("  track[{}]: offset=({:.2},{:.2}) -> ({:.6}°, {:.6}°)", ti, t.x, t.y, lat, lon);
            }
        }
    }
}
