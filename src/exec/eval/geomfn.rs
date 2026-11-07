//! Geometry functions of the evaluator: `length`, `trace`, `hull`, `poly`
//! (docs/spec/evaluators.md §10).

use crate::geo::{V3, cartesian, great_circle_dist};
use super::rhr::rhr_polygon;
use crate::output::{DGeom, LatLon};

fn gc(a: LatLon, b: LatLon) -> f64 {
    great_circle_dist(a.0, a.1, b.0, b.1)
}

/// Length in meters.
pub fn length(g: &DGeom) -> f64 {
    match g {
        // Folds start at +0.0: `Iterator::sum` for floats starts at -0.0.
        DGeom::Collection(c) | DGeom::PartialRel(c) => c.iter().map(length).fold(0.0, |a, b| a + b),
        DGeom::LineString(p) => p.windows(2).map(|w| gc(w[0], w[1])).fold(0.0, |a, b| a + b),
        DGeom::PartialWay(p) => {
            // A single valid run is the line; otherwise sum where both ends are known.
            p.windows(2)
                .filter_map(|w| match (w[0], w[1]) {
                    (Some(a), Some(b)) => Some(gc(a, b)),
                    _ => None,
                })
                .fold(0.0, |a, b| a + b)
        }
        _ => 0.0,
    }
}

fn lt(a: &LatLon, b: &LatLon) -> std::cmp::Ordering {
    a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
}

fn collect_trace(g: &DGeom, nodes: &mut Vec<LatLon>, lines: &mut Vec<Vec<LatLon>>) {
    if let Some(c) = g.components() {
        for x in c {
            collect_trace(x, nodes, lines);
        }
    } else if let Some(l) = g.line() {
        lines.push(l);
    } else if let Some(ml) = g.multiline() {
        lines.extend(ml);
    } else if let Some(c) = g.center() {
        nodes.push(c);
    }
}

/// `trace(g)`: points and line pieces split where they touch.
pub fn trace(g: &DGeom) -> DGeom {
    let mut nodes = Vec::new();
    let mut lines = Vec::new();
    collect_trace(g, &mut nodes, &mut lines);
    nodes.sort_by(lt);
    nodes.dedup();
    let key = |p: &LatLon| (p.0.to_bits(), p.1.to_bits());
    let mut cnt: rustc_hash::FxHashMap<(u64, u64), u32> = rustc_hash::FxHashMap::default();
    for n in &nodes {
        *cnt.entry(key(n)).or_default() += 1;
    }
    for l in &lines {
        if l.is_empty() {
            continue;
        }
        *cnt.entry(key(&l[0])).or_default() += 2;
        *cnt.entry(key(l.last().unwrap())).or_default() += 2;
        for p in l.get(1..l.len().saturating_sub(1)).unwrap_or(&[]) {
            *cnt.entry(key(p)).or_default() += 1;
        }
    }
    let mut out: Vec<DGeom> = nodes.iter().map(|p| DGeom::Point(*p)).collect();
    let mut seen: rustc_hash::FxHashSet<Vec<(u64, u64)>> = rustc_hash::FxHashSet::default();
    let mut emit = |piece: Vec<LatLon>, out: &mut Vec<DGeom>| {
        let oriented: Vec<(u64, u64)> = if piece.len() > 1 && lt(piece.last().unwrap(), &piece[0]) == std::cmp::Ordering::Less {
            piece.iter().rev().map(key).collect()
        } else {
            piece.iter().map(key).collect()
        };
        if seen.insert(oriented) {
            out.push(DGeom::LineString(piece));
        }
    };
    for l in &lines {
        if l.is_empty() {
            continue;
        }
        if l.len() == 1 {
            emit(vec![l[0], l[0]], &mut out);
            continue;
        }
        let mut cur = vec![l[0]];
        for (i, p) in l.iter().enumerate().skip(1) {
            cur.push(*p);
            if i + 1 < l.len() && cnt.get(&key(p)).copied().unwrap_or(0) > 1 {
                emit(std::mem::take(&mut cur), &mut out);
                cur.push(*p);
            }
        }
        emit(cur, &mut out);
    }
    DGeom::Collection(out)
}

fn collect_points(g: &DGeom, out: &mut Vec<LatLon>) {
    if let Some(c) = g.components() {
        for x in c {
            collect_points(x, out);
        }
    } else if let Some(l) = g.line() {
        out.extend(l);
    } else if let Some(ml) = g.multiline() {
        out.extend(ml.into_iter().flatten());
    } else if let Some(c) = g.center() {
        out.push(c);
    }
}

/// `Spherical_Vector(lhs, rhs)`: cross product divided by its length
/// (a zero vector yields NaNs, as in C++).
fn norm_cross(a: &V3, b: &V3) -> V3 {
    let c = a.cross(b);
    let n = (c.0 * c.0 + c.1 * c.1 + c.2 * c.2).sqrt();
    V3(c.0 / n, c.1 / n, c.2 / n)
}

struct HullSeg {
    pt: LatLon,
    v: V3,
    edge: V3,
}

/// `hull(g)`: spherical convex hull (algorithm of the C++ implementation).
pub fn hull(g: &DGeom) -> DGeom {
    let mut pts = Vec::new();
    collect_points(g, &mut pts);
    pts.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)));
    pts.dedup();
    match pts.len() {
        0 => return DGeom::Null,
        1 => return DGeom::Point(pts[0]),
        2 => return DGeom::LineString(vec![pts[0], pts[1], pts[0]]),
        _ => {}
    }
    let mut s = pts[0];
    let mut n = pts[0];
    for p in &pts {
        if p.0 < s.0 {
            s = *p;
        }
        if p.0 > n.0 {
            n = *p;
        }
    }
    let last = pts.len() - 1;
    let mut gap = pts[0].1 + 360.0 - pts[last].1;
    let mut w = pts[0];
    let mut e = pts[last];
    for i in 1..pts.len() {
        if pts[i].1 - pts[i - 1].1 > gap {
            gap = pts[i].1 - pts[i - 1].1;
            w = pts[i];
            e = pts[i - 1];
        }
    }
    let v = |p: &LatLon| cartesian(p.0, p.1);
    let mut segs: Vec<HullSeg> = Vec::new();
    if w.1 != e.1 {
        segs.push(HullSeg { pt: w, v: v(&w), edge: V3(0.0, 0.0, 0.0) });
        segs.push(HullSeg { pt: e, v: v(&e), edge: V3(0.0, 0.0, 0.0) });
    } else if n.0 != s.0 {
        segs.push(HullSeg { pt: s, v: v(&s), edge: V3(0.0, 0.0, 0.0) });
        segs.push(HullSeg { pt: n, v: v(&n), edge: V3(0.0, 0.0, 0.0) });
    }
    if !segs.is_empty() {
        segs[1].edge = norm_cross(&segs[0].v, &segs[1].v);
        segs[0].edge = norm_cross(&segs[1].v, &segs[0].v);
    }
    for p in &pts {
        if segs.is_empty() {
            break;
        }
        let sv = v(p);
        let vis = |segs: &Vec<HullSeg>, i: usize| segs[i].edge.dot(&sv) >= 0.0;
        // `while (edge*s < 1e-8) ++it; if (it != end && edge*s >= 1e-8)` (NaN stops the scan).
        let mut i = 0;
        while i < segs.len() && segs[i].edge.dot(&sv) < 1e-8 {
            i += 1;
        }
        if i == segs.len() || !(segs[i].edge.dot(&sv) >= 1e-8) {
            continue;
        }
        let new_at;
        if vis(&segs, 0) {
            let mut j = segs.len() - 1;
            while j != 0 && vis(&segs, j) {
                j -= 1;
            }
            segs.truncate(j + 1);
            let mut k = 0;
            while k < segs.len() && vis(&segs, k) {
                k += 1;
            }
            let k = k.saturating_sub(1);
            segs[k].edge = norm_cross(&sv, &segs[k].v);
            if k == 0 {
                segs.insert(0, HullSeg { pt: *p, v: sv, edge: V3(0.0, 0.0, 0.0) });
            } else {
                segs.drain(1..k);
                segs[0] = HullSeg { pt: *p, v: sv, edge: V3(0.0, 0.0, 0.0) };
            }
            new_at = 0;
        } else {
            let mut t = i;
            while t < segs.len() && vis(&segs, t) {
                t += 1;
            }
            let t = t - 1;
            let mut f = i;
            while f != 0 && vis(&segs, f) {
                f -= 1;
            }
            let f = f + 1;
            segs[t].edge = norm_cross(&sv, &segs[t].v);
            if f == t {
                segs.insert(f, HullSeg { pt: *p, v: sv, edge: V3(0.0, 0.0, 0.0) });
            } else {
                segs.drain(f + 1..t);
                segs[f] = HullSeg { pt: *p, v: sv, edge: V3(0.0, 0.0, 0.0) };
            }
            new_at = f;
        }
        let prev = if new_at == 0 { segs.len() - 1 } else { new_at - 1 };
        let pv = segs[prev].v;
        segs[new_at].edge = norm_cross(&pv, &sv);
    }
    let ring: Vec<LatLon> = segs.iter().map(|s| s.pt).collect();
    if ring.len() < 3 {
        return DGeom::LineString(ring);
    }
    // RHR_Polygon_Geometry(Free_Polygon_Geometry({ring})): the ring is closed
    // and normalised like any `poly()` input.
    DGeom::Polygon(rhr_polygon(&[ring]))
}

/// `poly(l1, l2, ...)`: every argument with a line geometry becomes a ring of
/// a `Free_Polygon_Geometry` (fewer than 2 points skipped, open rings closed),
/// which is then normalised to a right-hand-rule polygon (§10.4).
pub fn make_polygon(gs: &[DGeom]) -> DGeom {
    let rings: Vec<Vec<LatLon>> = gs.iter().filter_map(|g| g.line()).collect();
    DGeom::Polygon(rhr_polygon(&rings))
}
