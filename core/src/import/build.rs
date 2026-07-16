//! Building game track nodes from simplified routes: assign game node ids and
//! per-node attributes, wire junction branch attachments, and serialize the
//! result into an `.nrclip` blueprint.

use std::collections::HashMap;

use crate::error::Result;

use crate::geo::{inverse_geodesic, merc_y_to_lat_rad};
use crate::nrc1::NrclipFile;
use crate::types::{Clip, Collection, ModMeta, Track, TrackKind};

use super::{OsmData, RouteData, RouteNodeInfo, BRANCH_OFFSET, EARTH_RADIUS, MODEL_VERSION, TRACK_TYPE_MEDIUM};

pub(super) fn build_track_nodes(
    simplified: &[Vec<(usize, f64, f64)>],
    rd: &RouteData,
    osm: &OsmData,
    apply_speed_limits: bool,
    tangent_mode: bool,
    type_speed_overrides: &HashMap<String, u32>,
) -> Vec<Track> {
    let mut track_nodes: Vec<Track> = Vec::new();
    let mut node_id_counter: i64 = 100;

    for (ri, simp) in simplified.iter().enumerate() {
        let mut last_layer: i32 = 0;
        let mut last_track_type: i32 = TRACK_TYPE_MEDIUM;
        let mut last_maxspeed: Option<f32> = None;
        let mut last_railway_type = String::new();

        for (si, &(orig_idx, x, y)) in simp.iter().enumerate() {
            let gid = node_id_counter;
            node_id_counter += 100;
            let prev = if si > 0 { track_nodes[track_nodes.len() - 1].node_id } else { 0 };

            let (layer, track_type, max_speed, railway_type) = if orig_idx == usize::MAX {
                (last_layer, last_track_type, last_maxspeed, last_railway_type.clone())
            } else {
                let osm_nid = rd.routes[ri][orig_idx];
                let l = osm.node_layer.get(&osm_nid).copied().unwrap_or(0);
                let tt = osm.node_track_type.get(&osm_nid).copied().unwrap_or(TRACK_TYPE_MEDIUM);
                let ms = osm.node_maxspeed.get(&osm_nid).copied();
                let rt = osm.node_railway_type.get(&osm_nid).cloned().unwrap_or_default();
                last_layer = l;
                last_track_type = tt;
                last_maxspeed = ms;
                last_railway_type.clone_from(&rt);
                (l, tt, ms, rt)
            };

            // Speed priority: specific override (rail:yard) > base override (rail) > OSM > none
            let base_type = railway_type.split(':').next().unwrap_or(&railway_type);
            let override_kmh = type_speed_overrides.get(&railway_type)
                .or_else(|| type_speed_overrides.get(base_type));
            let speed = if let Some(&kmh) = override_kmh {
                Some((f64::from(kmh) / 3.6) as f32)
            } else if apply_speed_limits {
                max_speed.or(Some(0.0))
            } else {
                Some(0.0)
            };

            track_nodes.push(Track {
                node_id: gid, x, y, layer, track_type,
                user_max_speed: speed,
                prev_node: prev,
                tangential: Some(u8::from(tangent_mode)),
                ..Track::default()
            });
            if si > 0 {
                let prev_idx = track_nodes.len() - 2;
                track_nodes[prev_idx].next_node = gid;
            }
        }
    }

    track_nodes
}

pub(super) fn attach_branches(
    track_nodes: &mut [Track],
    simplified: &[Vec<(usize, f64, f64)>],
    rd: &RouteData,
) {
    // Build route → game node chains and junction game ID map
    let mut route_game_nodes: Vec<Vec<RouteNodeInfo>> = Vec::new();
    let mut junction_game_ids: HashMap<u64, i64> = HashMap::new();
    let mut idx = 0;
    for (ri, simp) in simplified.iter().enumerate() {
        let mut chain = Vec::new();
        for (si, &(orig_idx, _, _)) in simp.iter().enumerate() {
            let gid = track_nodes[idx + si].node_id;
            chain.push(RouteNodeInfo { game_id: gid });
            if orig_idx != usize::MAX {
                let osm_nid = rd.routes[ri][orig_idx];
                if rd.junction_nodes.contains(&osm_nid) && rd.junction_owner.get(&osm_nid) == Some(&ri) {
                    junction_game_ids.insert(osm_nid, gid);
                }
            }
        }
        idx += simp.len();
        route_game_nodes.push(chain);
    }

    // Build O(1) node_id → index lookup
    let node_idx: HashMap<i64, usize> = track_nodes.iter()
        .enumerate()
        .map(|(i, t)| (t.node_id, i))
        .collect();

    for (ri, simp) in simplified.iter().enumerate() {
        if simp.len() < 2 { continue; }

        for &is_start in &[true, false] {
            let endpoint_orig_idx = if is_start { simp[0].0 } else { simp.last().expect("non-empty simp").0 };
            // Skip synthetic interpolated nodes — they can't be junctions
            if endpoint_orig_idx == usize::MAX { continue; }
            let endpoint_osm = rd.routes[ri][endpoint_orig_idx];
            if !rd.junction_nodes.contains(&endpoint_osm) { continue; }

            let Some(&owner_ri) = rd.junction_owner.get(&endpoint_osm) else { continue };
            if owner_ri == ri { continue; }

            let branch_gid = if is_start {
                route_game_nodes[ri][0].game_id
            } else {
                route_game_nodes[ri].last().expect("non-empty chain").game_id
            };

            let junction_orig = rd.routes[ri][endpoint_orig_idx];

            // Attach to the junction owner's game node directly
            let Some(&parent_game_id) = junction_game_ids.get(&junction_orig) else { continue };
            let parent_node_id = parent_game_id;

            // Compute t = 0.5 (center of parent node's shape polyline)
            let t = 0.5;

            // Determine branch direction relative to parent's next node
            let br_idx = node_idx[&branch_gid];
            let pi = node_idx[&parent_node_id];
            let parent_next = track_nodes[pi].next_node;
            let parent_prev = track_nodes[pi].prev_node;
            let neighbor_node = if parent_next != 0 { parent_next }
                else if parent_prev != 0 { parent_prev }
                else { continue };
            let qi = node_idx[&neighbor_node];
            let seg_dx = track_nodes[qi].x - track_nodes[pi].x;
            let seg_dy = track_nodes[qi].y - track_nodes[pi].y;

            let neighbor_gid = if is_start {
                if route_game_nodes[ri].len() > 1 { route_game_nodes[ri][1].game_id } else { continue; }
            } else {
                let len = route_game_nodes[ri].len();
                if len > 1 { route_game_nodes[ri][len - 2].game_id } else { continue; }
            };
            let ni = node_idx[&neighbor_gid];
            let br_dx = track_nodes[ni].x - track_nodes[br_idx].x;
            let br_dy = track_nodes[ni].y - track_nodes[br_idx].y;
            let dot = br_dx * seg_dx + br_dy * seg_dy;
            let dir = if dot >= 0.0 { 1 } else { -1 };

            // Nudge branch root away from parent in ground meters.
            // Pipeline works in Mercator where distances are stretched by 1/cos(lat),
            // so scale the offset to achieve the desired ground-meter distance.
            let merc_offset = BRANCH_OFFSET / merc_y_to_lat_rad(track_nodes[br_idx].y).cos();
            let br_len = (br_dx * br_dx + br_dy * br_dy).sqrt().max(1e-10);
            track_nodes[br_idx].x += (br_dx / br_len) * merc_offset;
            track_nodes[br_idx].y += (br_dy / br_len) * merc_offset;

            // Set attachment fields
            track_nodes[br_idx].attached_to_id = parent_node_id;
            track_nodes[br_idx].attached_to_t = t;
            track_nodes[br_idx].attached_to_direction = Some(dir);

            let par_idx = node_idx[&parent_node_id];
            track_nodes[par_idx].attached_by.push(branch_gid);
        }
    }
}

pub(super) fn serialize_to_nrclip(
    mut track_nodes: Vec<Track>,
    name: &str,
    track_kinds: Vec<(i32, TrackKind)>,
    mod_metas: Vec<ModMeta>,
) -> Result<Vec<u8>> {
    let cx = track_nodes.iter().map(|t| t.x).sum::<f64>() / track_nodes.len() as f64;
    let cy = track_nodes.iter().map(|t| t.y).sum::<f64>() / track_nodes.len() as f64;
    let center_lat = merc_y_to_lat_rad(cy);
    let center_lon = cx / EARTH_RADIUS;
    for t in &mut track_nodes {
        let node_lat = merc_y_to_lat_rad(t.y);
        let node_lon = t.x / EARTH_RADIUS;

        // Compute inverse geodesic: the (dx, dy) ground-meter offset that the
        // game's spherical destination formula (RVA 0xB9300) will reconstruct
        // back to this node's exact lat/lon.
        let (dx, dy) = inverse_geodesic(center_lat, center_lon, node_lat, node_lon);
        t.x = dx;
        t.y = dy;
    }

    let name_hash = name.bytes().fold(0x0012_3456_7890_u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)));
    let file = NrclipFile {
        version: MODEL_VERSION,
        collections: vec![Collection {
            id_a: name_hash,
            id_b: name_hash.wrapping_mul(7),
            name: name.to_string(),
            clips: vec![Clip {
                guid: name.to_string(),
                clip_id: name_hash.wrapping_mul(13),
                center_x: cx,
                center_y: cy,
                tracks: track_nodes,
                track_kinds,
                mod_metas,
                ..Clip::default()
            }],
            ..Collection::default()
        }],
    };

    file.to_bytes()
}
