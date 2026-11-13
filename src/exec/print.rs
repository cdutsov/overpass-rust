//! The `out` statement (docs/spec/output.md §5–§7, §11).

use super::Engine;
use super::geom::way_vertices;
use super::set::Set;
use crate::geo::{BBox, Pt, Rect, ilon_of_x, lat_of, lon_of};
use crate::model::*;
use crate::output::flags::*;
use crate::output::{GeomOut, Item, TagList, LatLon, MemberGeom, MetaOut, calc_bounds, combine_bounds};
use crate::ql::ast::{GeomMode, Order, OutMode, OutOpts};
use crate::store::Kind;
use anyhow::Result;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::io::Write;

pub fn out_flags(o: &OutOpts) -> u32 {
    let mut fl = if o.count {
        ID | COUNT | TAGS
    } else {
        match o.mode {
            OutMode::Ids => ID,
            OutMode::Skel => ID | COORDS | NDS | MEMBERS,
            OutMode::Body => ID | COORDS | NDS | MEMBERS | TAGS,
            OutMode::Tags => ID | TAGS,
            OutMode::Meta => ID | COORDS | NDS | MEMBERS | TAGS | VERSION | META,
        }
    };
    match o.geom {
        GeomMode::Geom => fl |= GEOMETRY | BOUNDS,
        GeomMode::Bb => fl |= BOUNDS,
        GeomMode::Center => fl |= CENTER,
        GeomMode::None => {}
    }
    if o.noids {
        fl &= !ID;
    }
    fl
}

fn ll(p: Pt) -> LatLon {
    (p.lat(), p.lon())
}

fn plain_minmax(pts: &[LatLon]) -> Option<(f64, f64, f64, f64)> {
    if pts.is_empty() {
        return None;
    }
    let mut b = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (lat, lon) in pts {
        b.0 = b.0.min(*lat);
        b.1 = b.1.min(*lon);
        b.2 = b.2.max(*lat);
        b.3 = b.3.max(*lon);
    }
    Some(b)
}

fn inside_clip(p: Pt, b: &BBox) -> bool {
    use crate::geo::{ilat, ilon};
    let (s, n, w, e) = (ilat(b.south), ilat(b.north), ilon(b.west), ilon(b.east));
    let lo = p.ilon();
    s <= p.y && p.y <= n && if w <= e { w <= lo && lo <= e } else { lo >= w || lo <= e }
}

/// Geometry of a way for printing.
/// Position used by the C++ code for missing nodes in `bb`/`center`.
const SENTINEL: LatLon = (-91.0, -214.7483648);

/// Whether a way with a missing node keeps the positions of its other nodes
/// in C++: only when its C++ index stores geometry (compound, bit 0 clear).
pub(crate) fn cpp_keeps_partial_way(idx: u32) -> bool {
    idx & 0x8000_0000 != 0 && idx & 1 == 0
}

/// Way positions as the C++ geometry store yields them: with a missing
/// node, only ways whose C++ index stores geometry (compound, bit 0 clear)
/// keep their other positions; others have no geometry at all.
fn cpp_way_positions(v: Vec<Option<Pt>>, idx: u32) -> Vec<Option<Pt>> {
    if v.iter().any(|p| p.is_none()) && !cpp_keeps_partial_way(idx) {
        return Vec::new();
    }
    v
}

/// Geometry of a way for printing. Missing nodes are invalid positions in
/// `geom` and the C++ sentinel coordinate in `bb`/`center`.
fn way_geom(v: &[Option<Pt>], fl: u32, clip: Option<&BBox>) -> GeomOut {
    let mut g = GeomOut::default();
    if fl & GEOMETRY != 0 {
        if v.is_empty() {
            g.way_pts = Some(Vec::new());
            g.bounds = Some((100.0, 200.0, 100.0, 200.0));
            return g;
        }
        let n = v.len();
        let pts: Vec<Option<LatLon>> = match clip {
            None => v.iter().map(|p| p.map(ll)).collect(),
            Some(b) => (0..n)
                .map(|i| {
                    let p = v[i]?;
                    let seg = |a: Option<Pt>, c: Pt| a.is_some_and(|a| crate::geo::bbox_intersects(b, ll(a), ll(c)));
                    let keep = inside_clip(p, b) || (i > 0 && seg(v[i - 1], p)) || (i + 1 < n && v[i + 1].is_some_and(|nx| crate::geo::bbox_intersects(b, ll(p), ll(nx))));
                    keep.then(|| ll(p))
                })
                .collect(),
        };
        let valid: Vec<LatLon> = pts.iter().flatten().copied().collect();
        g.bounds = calc_bounds(valid.iter());
        g.center = g.bounds.map(|b| crate::output::bounds_center(&b));
        g.way_pts = Some(pts);
        return g;
    }
    let pts: Vec<LatLon> = v.iter().map(|p| p.map(ll).unwrap_or(SENTINEL)).collect();
    if let Some(b) = plain_minmax(&pts) {
        if fl & BOUNDS != 0 {
            g.bounds = Some(b);
        }
        if fl & CENTER != 0 {
            g.center = Some(((b.0 + b.2) / 2.0, (b.1 + b.3) / 2.0));
        }
    }
    g
}

#[cfg(test)]
thread_local! {
    /// Turns off `bb` and `center` from stored boxes.
    pub(super) static BOXES_OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `bb` and `center` from a stored box, which is the box of the positions
/// `way_geom` and `rel_geom` take their minimum and maximum of: the same
/// numbers, as the decoding of a coordinate keeps its order.
fn box_geom(b: &Rect, fl: u32) -> GeomOut {
    let mut g = GeomOut::default();
    if b.is_empty() {
        return g;
    }
    let b = (lat_of(b.y0), lon_of(ilon_of_x(b.x0)), lat_of(b.y1), lon_of(ilon_of_x(b.x1)));
    if fl & BOUNDS != 0 {
        g.bounds = Some(b);
    }
    if fl & CENTER != 0 {
        g.center = Some(((b.0 + b.2) / 2.0, (b.1 + b.3) / 2.0));
    }
    g
}

/// Elements prepared and printed at a time.
const WINDOW: usize = 8192;

/// Print windows whose meta data is fetched at once (512k elements, about
/// 25 MB of meta data): the elements of one window are spread over many
/// meta chunks, which the following windows need again.
const META_WINDOWS: usize = 64;

/// The elements whose meta data is fetched with window `i`.
fn meta_span<T>(all: &[T], i: usize) -> &[T] {
    &all[i * WINDOW..((i + META_WINDOWS) * WINDOW).min(all.len())]
}

impl<'s, W: Write> Engine<'s, W> {
    pub fn print(&mut self, s: &Set, o: &OutOpts) -> Result<()> {
        let fl = out_flags(o);
        let mut remaining = o.limit.unwrap_or(u64::MAX);
        if o.count {
            if remaining == 0 {
                return Ok(());
            }
            let mut tags = vec![
                ("nodes".to_string(), s.nodes.len().to_string()),
                ("ways".to_string(), s.ways.len().to_string()),
                ("relations".to_string(), s.rels.len().to_string()),
            ];
            let mut total = s.nodes.len() + s.ways.len() + s.rels.len();
            if self.area_level {
                tags.push(("areas".to_string(), s.areas.len().to_string()));
                total += s.areas.len();
            }
            tags.push(("total".to_string(), total.to_string()));
            self.p.item(&Item::Derived { tname: "count", id: 0, tags: TagList::Pairs(&tags) }, fl, &GeomOut::default(), None)?;
            return Ok(());
        }
        let qt = o.order == Order::Quadtile;
        let clip = o.geom_bbox.as_ref();
        let want_meta = fl & META != 0;

        // Nodes
        let mut nodes: Vec<&Node> = s.nodes.iter().collect();
        if qt {
            nodes.sort_by_key(|n| (crate::geo::tile_of(n.pos), n.id));
        }
        let nodes: Vec<&Node> = nodes.into_iter().take(remaining.min(usize::MAX as u64) as usize).collect();
        remaining -= nodes.len() as u64;
        let no_geom = GeomOut::default();
        // Elements are prepared and printed in windows to bound memory.
        let mut metas = HashMap::default();
        for (i, win) in nodes.chunks(WINDOW).enumerate() {
            if want_meta && i % META_WINDOWS == 0 {
                metas = self.f.meta(Kind::Node, &meta_span(&nodes, i).iter().map(|n| n.id).collect::<Vec<_>>())?;
            }
            let users = self.users_for(win.iter().filter_map(|n| metas.get(&n.id)))?;
            let list: Vec<(Item, GeomOut, Option<MetaOut>)> = win
                .iter()
                .map(|n| (Item::Node(n), GeomOut::default(), metas.get(&n.id).map(|m| MetaOut { meta: *m, user: users.get(&m.uid).map(|s| s.as_str()) })))
                .collect();
            self.p.items(&list, fl)?;
            self.check()?;
        }

        // Ways
        let mut ways: Vec<&Way> = s.ways.iter().collect();
        if qt {
            ways.sort_by_key(|w| (crate::geo::cpp_index_order(w.idx), w.id));
        }
        let ways: Vec<&Way> = ways.into_iter().take(remaining.min(usize::MAX as u64) as usize).collect();
        remaining -= ways.len() as u64;
        let need_geom = fl & (GEOMETRY | BOUNDS | CENTER) != 0;
        // `bb` and `center` of a way with all its nodes come from its box
        let boxes = fl & GEOMETRY == 0;
        #[cfg(test)]
        let boxes = boxes && !BOXES_OFF.get();
        let by_box = |w: &Way| boxes && !w.missing && !w.bbox.is_empty();
        let mut metas = HashMap::default();
        for (i, win) in ways.chunks(WINDOW).enumerate() {
            let mut verts = if need_geom { way_vertices(&self.f, &win.iter().copied().filter(|w| !by_box(w)).collect::<Vec<&Way>>())? } else { HashMap::default() };
            if want_meta && i % META_WINDOWS == 0 {
                metas = self.f.meta(Kind::Way, &meta_span(&ways, i).iter().map(|w| w.id).collect::<Vec<_>>())?;
            }
            let users = self.users_for(win.iter().filter_map(|w| metas.get(&w.id)))?;
            let list: Vec<(Item, GeomOut, Option<MetaOut>)> = win
                .iter()
                .map(|w| {
                    let g = match verts.remove(&w.id) {
                        Some(v) => way_geom(&cpp_way_positions(v, w.idx), fl, clip),
                        None if need_geom && by_box(w) => box_geom(&w.bbox, fl),
                        None => GeomOut::default(),
                    };
                    (Item::Way(w), g, metas.get(&w.id).map(|m| MetaOut { meta: *m, user: users.get(&m.uid).map(|s| s.as_str()) }))
                })
                .collect();
            self.p.items(&list, fl)?;
            self.check()?;
        }

        // Relations
        let mut rels: Vec<&Relation> = s.rels.iter().collect();
        if qt {
            rels.sort_by_key(|r| (crate::geo::cpp_index_order(r.idx), r.id));
        }
        let rels: Vec<&Relation> = rels.into_iter().take(remaining.min(usize::MAX as u64) as usize).collect();
        remaining -= rels.len() as u64;
        let mut metas = HashMap::default();
        for (i, win) in rels.chunks(WINDOW).enumerate() {
            if want_meta && i % META_WINDOWS == 0 {
                metas = self.f.meta(Kind::Relation, &meta_span(&rels, i).iter().map(|r| r.id).collect::<Vec<_>>())?;
            }
            let users = self.users_for(win.iter().filter_map(|r| metas.get(&r.id)))?;
            let boxed = if need_geom && boxes && clip.is_none() { self.rels_by_box(win)? } else { HashSet::default() };
            let open: Vec<&Relation> = win.iter().copied().filter(|r| !boxed.contains(&r.id)).collect();
            let (npos, wverts) = if need_geom { self.member_geometry(&open)? } else { (HashMap::default(), HashMap::default()) };
            let list: Vec<(Item, GeomOut, Option<MetaOut>)> = win
                .iter()
                .map(|r| {
                    let g = if boxed.contains(&r.id) {
                        box_geom(&r.bbox, fl)
                    } else if need_geom {
                        rel_geom(r, fl, clip, &npos, &wverts)
                    } else {
                        GeomOut::default()
                    };
                    (Item::Relation(r), g, metas.get(&r.id).map(|m| MetaOut { meta: *m, user: users.get(&m.uid).map(|s| s.as_str()) }))
                })
                .collect();
            self.p.items(&list, fl)?;
            self.check()?;
        }

        // Areas (only when the query uses areas)
        if self.area_level {
            let mut areas: Vec<&Area> = s.areas.iter().collect();
            if qt {
                areas.sort_by_key(|a| (crate::geo::cpp_index_order(a.idx), a.id));
            }
            let areas: Vec<&Area> = areas.into_iter().take(remaining.min(usize::MAX as u64) as usize).collect();
            remaining -= areas.len() as u64;
            for a in areas {
                self.p.item(&Item::Derived { tname: "area", id: a.id, tags: TagList::Sorted(&a.tags) }, fl, &no_geom, None)?;
            }
        }

        // Derived elements
        let mut derived: Vec<&super::set::Derived> = s.derived.iter().collect();
        derived.sort_by_key(|d| d.id);
        for d in derived.into_iter().take(remaining.min(usize::MAX as u64) as usize) {
            let g = GeomOut { derived: d.geom.clone(), ..Default::default() };
            self.p.item(&Item::Derived { tname: &d.tname, id: d.id, tags: TagList::Pairs(&d.tags) }, fl, &g, None)?;
        }
        Ok(())
    }

    fn users_for<'m>(&self, metas: impl Iterator<Item = &'m Meta>) -> Result<HashMap<u32, String>> {
        let mut out = HashMap::default();
        for m in metas {
            if !out.contains_key(&m.uid) {
                if let Some(n) = self.f.user_name(m.uid)? {
                    out.insert(m.uid, n);
                }
            }
        }
        Ok(out)
    }

    /// The relations whose `bb` and `center` come from their stored box:
    /// the box of their member nodes and the boxes of their member ways,
    /// when every member node exists and every member way that exists has
    /// all its nodes (missing ones enter `rel_geom` as the C++ sentinel),
    /// and the box is less than 180 degrees wide (`rel_geom` treats wider
    /// boxes as across the antimeridian). Member ways are read without their
    /// nodes: a route has thousands of them.
    fn rels_by_box(&self, rels: &[&Relation]) -> Result<HashSet<Id>> {
        let mut nids = Vec::new();
        let mut wids = Vec::new();
        for r in rels {
            for m in &r.members {
                match m.mtype {
                    ElemType::Node => nids.push(m.id),
                    ElemType::Way => wids.push(m.id),
                    _ => {}
                }
            }
        }
        nids.sort_unstable();
        nids.dedup();
        wids.sort_unstable();
        wids.dedup();
        let nodes: HashSet<Id> = self.f.snap.cells_of(Kind::Node, &nids)?.into_iter().map(|(id, _)| id).collect();
        let broken: HashSet<Id> = self.f.ways_untagged(&wids)?.into_iter().filter(|w| w.missing).map(|w| w.id).collect();
        Ok(rels
            .iter()
            .filter(|r| {
                (r.bbox.is_empty() || r.bbox.x1 - r.bbox.x0 < 1_790_000_000)
                    && r.members.iter().all(|m| match m.mtype {
                        ElemType::Node => nodes.contains(&m.id),
                        ElemType::Way => !broken.contains(&m.id),
                        _ => true,
                    })
            })
            .map(|r| r.id)
            .collect())
    }

    #[allow(clippy::type_complexity)]
    fn member_geometry(&self, rels: &[&Relation]) -> Result<(HashMap<Id, Pt>, HashMap<Id, (u32, Vec<Option<Pt>>)>)> {
        let mut nids = Vec::new();
        let mut wids = Vec::new();
        for r in rels {
            for m in &r.members {
                match m.mtype {
                    ElemType::Node => nids.push(m.id),
                    ElemType::Way => wids.push(m.id),
                    _ => {}
                }
            }
        }
        wids.sort_unstable();
        wids.dedup();
        let ways = self.f.ways_untagged(&wids)?;
        let mut wverts = way_vertices(&self.f, &ways)?;
        let mut out = HashMap::default();
        for w in &ways {
            if let Some(v) = wverts.remove(&w.id) {
                out.insert(w.id, (w.idx, cpp_way_positions(v, w.idx)));
            }
        }
        let npos = self.f.node_positions(&nids)?;
        Ok((npos, out))
    }
}

fn rel_geom(r: &Relation, fl: u32, clip: Option<&BBox>, npos: &HashMap<Id, Pt>, wverts: &HashMap<Id, (u32, Vec<Option<Pt>>)>) -> GeomOut {
    let mut g = GeomOut::default();
    // Way member positions; empty if the way is missing, and with a box (as
    // C++, which reads the member ways by their index) if the cell of its
    // index does not meet the box: "geometry": [] instead of nulls.
    let way_pts = |id: Id| -> Vec<Option<Pt>> {
        match wverts.get(&id) {
            Some((idx, _)) if clip.is_some_and(|b| !crate::geo::cpp_index_meets(*idx, b)) => Vec::new(),
            Some((_, v)) => v.clone(),
            None => Vec::new(),
        }
    };
    if fl & GEOMETRY != 0 {
        let mut members = Vec::with_capacity(r.members.len());
        let mut parts = Vec::new();
        for m in &r.members {
            match m.mtype {
                ElemType::Node => {
                    let p = npos.get(&m.id).copied().filter(|p| clip.is_none_or(|b| inside_clip(*p, b)));
                    if let Some(p) = p {
                        let q = ll(p);
                        parts.push((q.0, q.1, q.0, q.1));
                    }
                    members.push(MemberGeom::Node(p.map(ll)));
                }
                ElemType::Way => {
                    let v = way_pts(m.id);
                    let n = v.len();
                    let inside = |p: Option<Pt>, b: &BBox| p.is_some_and(|p| inside_clip(p, b));
                    let pts: Vec<Option<LatLon>> = (0..n)
                        .map(|i| {
                            let p = v[i]?;
                            let keep = match clip {
                                None => true,
                                Some(b) => inside_clip(p, b) || (i > 0 && inside(v[i - 1], b)) || (i + 1 < n && inside(v[i + 1], b)),
                            };
                            keep.then(|| ll(p))
                        })
                        .collect();
                    let valid: Vec<LatLon> = pts.iter().flatten().copied().collect();
                    if let Some(b) = calc_bounds(valid.iter()) {
                        parts.push(b);
                    }
                    members.push(MemberGeom::Way(pts));
                }
                _ => members.push(MemberGeom::Relation),
            }
        }
        g.bounds = combine_bounds(&parts);
        g.center = g.bounds.map(|b| crate::output::bounds_center(&b));
        g.members = Some(members);
        return g;
    }
    // C++ quirk: missing node members and missing way nodes enter the box
    // as the sentinel coordinate.
    let mut pts: Vec<LatLon> = Vec::new();
    for m in &r.members {
        match m.mtype {
            ElemType::Node => pts.push(npos.get(&m.id).map(|p| ll(*p)).unwrap_or(SENTINEL)),
            ElemType::Way => pts.extend(way_pts(m.id).into_iter().map(|p| p.map(ll).unwrap_or(SENTINEL))),
            _ => {}
        }
    }
    let Some(mut b) = plain_minmax(&pts) else { return g };
    if b.3 - b.1 >= 180.0 {
        let mut minlon = 200.0f64;
        let mut maxlon = -200.0f64;
        for (_, lon) in &pts {
            if *lon >= 0.0 {
                minlon = minlon.min(*lon);
            } else {
                maxlon = maxlon.max(*lon);
            }
        }
        b.1 = minlon;
        b.3 = maxlon;
    }
    if fl & BOUNDS != 0 {
        g.bounds = Some(b);
    }
    if fl & CENTER != 0 {
        g.center = Some(((b.0 + b.2) / 2.0, (b.1 + b.3) / 2.0));
    }
    g
}
