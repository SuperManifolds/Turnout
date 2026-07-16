//! Route simplification: reduce each merged route's polyline to control points
//! (junctions, layer changes, direction changes via a Hobby-spline deviation
//! test), then re-subdivide segments that ended up longer than `MAX_SPACING`.

use std::collections::{HashMap, HashSet};

use crate::hobby;

use super::{RouteData, JUNCTION_ENDPOINT_SPACING, MAX_SPACING, SPLINE_TOLERANCE};

pub(super) fn simplify_routes(
    rd: &RouteData,
    node_layer: &HashMap<u64, i32>,
) -> Vec<Vec<(usize, f64, f64)>> {
    rd.routes.iter().zip(rd.route_coords.iter())
        .map(|(route, coords)| {
            let mut keep = vec![false; coords.len()];
            keep[0] = true;
            *keep.last_mut().expect("non-empty coords") = true;

            // Force keep junction nodes and layer-change boundaries
            for (i, &nid) in route.iter().enumerate() {
                if rd.junction_nodes.contains(&nid) {
                    keep[i] = true;
                }
                if i > 0 {
                    let prev_layer = node_layer.get(&route[i - 1]).copied().unwrap_or(0);
                    let cur_layer = node_layer.get(&nid).copied().unwrap_or(0);
                    if prev_layer != cur_layer {
                        keep[i - 1] = true;
                        keep[i] = true;
                    }
                }
            }

            // Keep control point near junction endpoints
            keep_near_junction_endpoints(route, coords, &rd.junction_nodes, &mut keep);

            // Enforce max spacing
            enforce_max_spacing(coords, &mut keep);

            // Spline-first simplification
            spline_simplify(coords, &mut keep);

            coords.iter().enumerate()
                .filter(|(i, _)| keep[*i])
                .map(|(i, &(x, y))| (i, x, y))
                .collect()
        }).collect()
}

fn keep_near_junction_endpoints(
    route: &[u64],
    coords: &[(f64, f64)],
    junction_nodes: &HashSet<u64>,
    keep: &mut [bool],
) {
    let start_is_junction = junction_nodes.contains(&route[0]);
    let end_is_junction = junction_nodes.contains(route.last().expect("non-empty route"));

    if start_is_junction && coords.len() > 2 {
        for i in 1..coords.len() - 1 {
            let dx = coords[i].0 - coords[0].0;
            let dy = coords[i].1 - coords[0].1;
            if dx * dx + dy * dy >= JUNCTION_ENDPOINT_SPACING * JUNCTION_ENDPOINT_SPACING {
                keep[i] = true;
                break;
            }
        }
    }
    if end_is_junction && coords.len() > 2 {
        let last = coords.len() - 1;
        for i in (1..last).rev() {
            let dx = coords[i].0 - coords[last].0;
            let dy = coords[i].1 - coords[last].1;
            if dx * dx + dy * dy >= JUNCTION_ENDPOINT_SPACING * JUNCTION_ENDPOINT_SPACING {
                keep[i] = true;
                break;
            }
        }
    }
}

fn enforce_max_spacing(coords: &[(f64, f64)], keep: &mut [bool]) {
    let mut last_kept = 0;
    for i in 1..coords.len() {
        if keep[i] { last_kept = i; continue; }
        let dx = coords[i].0 - coords[last_kept].0;
        let dy = coords[i].1 - coords[last_kept].1;
        if dx * dx + dy * dy >= MAX_SPACING * MAX_SPACING {
            keep[i] = true;
            last_kept = i;
        }
    }
}

fn spline_simplify(coords: &[(f64, f64)], keep: &mut [bool]) {
    for _ in 0..20 {
        let kept_pts: Vec<(f64, f64)> = (0..coords.len())
            .filter(|&i| keep[i]).map(|i| coords[i]).collect();
        let kept_idx: Vec<usize> = (0..coords.len())
            .filter(|&i| keep[i]).collect();

        if kept_pts.len() < 2 { break; }
        let segs = hobby::hobby_spline(&kept_pts);
        let mut added = false;

        for (si, seg) in segs.iter().enumerate() {
            let orig_start = kept_idx[si];
            let orig_end = kept_idx[si + 1];
            if orig_end - orig_start <= 1 { continue; }

            let mut worst_dev = 0.0f64;
            let mut worst_orig = orig_start;
            for (oi, &(ox, oy)) in coords.iter().enumerate().take(orig_end).skip(orig_start + 1) {
                let mut best_d = f64::MAX;
                for s in 0..=32 {
                    let pt = hobby::bezier_point(seg, f64::from(s) / 32.0);
                    let d = ((ox - pt.0).powi(2) + (oy - pt.1).powi(2)).sqrt();
                    if d < best_d { best_d = d; }
                }
                if best_d > worst_dev {
                    worst_dev = best_d;
                    worst_orig = oi;
                }
            }

            if worst_dev > SPLINE_TOLERANCE {
                keep[worst_orig] = true;
                added = true;
            }
        }

        if !added { break; }
    }
}

pub(super) fn subdivide_long_segments(
    simplified: Vec<Vec<(usize, f64, f64)>>,
    route_coords: &[Vec<(f64, f64)>],
) -> Vec<Vec<(usize, f64, f64)>> {
    simplified.into_iter().zip(route_coords.iter())
        .map(|(simp, coords)| {
            let mut result = Vec::new();
            for i in 0..simp.len() {
                result.push(simp[i]);
                if i + 1 >= simp.len() { continue; }
                let (idx0, x0, y0) = simp[i];
                let (idx1, x1, y1) = simp[i + 1];
                let seg_dist = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
                if seg_dist <= MAX_SPACING { continue; }

                if idx0 != usize::MAX && idx1 != usize::MAX && idx0 < idx1 {
                    interpolate_along_polyline(&mut result, coords, idx0, idx1);
                } else {
                    // Straight-line fallback for interpolated endpoints
                    let n = (seg_dist / MAX_SPACING).ceil() as usize;
                    for j in 1..n {
                        let t = j as f64 / n as f64;
                        result.push((usize::MAX, x0 + (x1 - x0) * t, y0 + (y1 - y0) * t));
                    }
                }
            }
            result
        }).collect()
}

fn interpolate_along_polyline(
    result: &mut Vec<(usize, f64, f64)>,
    coords: &[(f64, f64)],
    start: usize,
    end: usize,
) {
    let mut cum_len = vec![0.0f64];
    for k in start..end {
        let dx = coords[k + 1].0 - coords[k].0;
        let dy = coords[k + 1].1 - coords[k].1;
        cum_len.push(cum_len.last().expect("non-empty cum_len") + (dx * dx + dy * dy).sqrt());
    }
    let total_len = *cum_len.last().expect("non-empty cum_len");
    if total_len < 1.0 { return; }

    let n = (total_len / MAX_SPACING).ceil() as usize;
    for j in 1..n {
        let target = total_len * j as f64 / n as f64;
        let seg = cum_len.partition_point(|&l| l < target).min(cum_len.len() - 1).max(1) - 1;
        let seg_start_len = cum_len[seg];
        let seg_end_len = cum_len[seg + 1];
        let local_t = if seg_end_len > seg_start_len {
            (target - seg_start_len) / (seg_end_len - seg_start_len)
        } else { 0.0 };
        let oi = start + seg;
        let px = coords[oi].0 + (coords[oi + 1].0 - coords[oi].0) * local_t;
        let py = coords[oi].1 + (coords[oi + 1].1 - coords[oi].1) * local_t;
        result.push((usize::MAX, px, py));
    }
}
