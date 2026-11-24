//! Element geometry: resolving coordinates and spatial predicates.

use super::fetch::Fetch;
use crate::geo::{BBox, Cell, Pt, Rect, Segment};
use crate::store::Kind;
use crate::model::*;
use anyhow::Result;
use std::borrow::Borrow;
use rustc_hash::FxHashMap as HashMap;

#[cfg(test)]
thread_local! {
    /// Member ways the last `rel_geoms_near` decoded.
    pub(super) static NEAR_WAYS_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Resolved geometry of an element for spatial tests.
#[derive(Clone, Debug)]
pub enum EGeom {
    Point(Pt),
    /// Way vertices; `None` for nodes missing from the database.
    Line(Vec<Option<Pt>>),
    /// Relation members with geometry: points and lines.
    Multi { points: Vec<Pt>, lines: Vec<Vec<Option<Pt>>> },
    Empty,
}

impl EGeom {
    pub fn points(&self) -> Vec<Pt> {
        match self {
            EGeom::Point(p) => vec![*p],
            EGeom::Line(v) => v.iter().flatten().copied().collect(),
            EGeom::Multi { points, lines } => points.iter().copied().chain(lines.iter().flatten().flatten().copied()).collect(),
            EGeom::Empty => vec![],
        }
    }

    /// Segments between consecutive known vertices (missing nodes skipped).
    pub fn segments(&self) -> Vec<(Pt, Pt)> {
        fn line_segs(v: &[Option<Pt>], out: &mut Vec<(Pt, Pt)>) {
            let pts: Vec<Pt> = v.iter().flatten().copied().collect();
            for w in pts.windows(2) {
                out.push((w[0], w[1]));
            }
        }
        let mut out = Vec::new();
        match self {
            EGeom::Line(v) => line_segs(v, &mut out),
            EGeom::Multi { lines, .. } => {
                for l in lines {
                    line_segs(l, &mut out);
                }
            }
            _ => {}
        }
        out
    }
}

/// Vertex positions of ways.
pub fn way_vertices<W: Borrow<Way>>(f: &Fetch, ways: &[W]) -> Result<HashMap<Id, Vec<Option<Pt>>>> {
    let ids: Vec<Id> = ways.iter().flat_map(|w| w.borrow().nodes.iter().copied()).collect();
    let pos = f.node_positions(&ids)?;
    Ok(ways
        .iter()
        .map(|w| {
            let w = w.borrow();
            (w.id, w.nodes.iter().map(|n| pos.get(n).copied()).collect())
        })
        .collect())
}

pub fn way_geoms<W: Borrow<Way>>(f: &Fetch, ways: &[W]) -> Result<Vec<EGeom>> {
    let ids: Vec<Id> = ways.iter().flat_map(|w| w.borrow().nodes.iter().copied()).collect();
    let pos = f.node_positions(&ids)?;
    Ok(ways.iter().map(|w| EGeom::Line(w.borrow().nodes.iter().map(|n| pos.get(n).copied()).collect())).collect())
}

pub fn rel_geoms<R: Borrow<Relation>>(f: &Fetch, rels: &[R]) -> Result<Vec<EGeom>> {
    let mut node_ids = Vec::new();
    let mut way_ids = Vec::new();
    for r in rels {
        for m in &r.borrow().members {
            match m.mtype {
                ElemType::Node => node_ids.push(m.id),
                ElemType::Way => way_ids.push(m.id),
                _ => {}
            }
        }
    }
    way_ids.sort_unstable();
    way_ids.dedup();
    let ways = f.ways_untagged(&way_ids)?;
    let wv = way_vertices(f, &ways)?;
    let npos = f.node_positions(&node_ids)?;
    Ok(rels
        .iter()
        .map(|r| {
            let r = r.borrow();
            let mut points = Vec::new();
            let mut lines = Vec::new();
            for m in &r.members {
                match m.mtype {
                    ElemType::Node => {
                        if let Some(p) = npos.get(&m.id) {
                            points.push(*p);
                        }
                    }
                    ElemType::Way => {
                        if let Some(v) = wv.get(&m.id) {
                            lines.push(v.clone());
                        }
                    }
                    _ => {}
                }
            }
            EGeom::Multi { points, lines }
        })
        .collect())
}

/// Relation geometry for spatial tests that pass when one member passes
/// (bbox, around, poly): way members whose stored box `near` rejects pass
/// none of them and are left out without loading their nodes, as are ways
/// with a missing node or without nodes, which never pass. Members stored
/// in a cell whose rectangle (`Cell::loose_rect`, which holds the box of
/// every element of the cell) `near` rejects are not read: it accepts every
/// rectangle that holds a box it accepts or a point that may pass. The
/// cells come from the id index. Long routes and large boundaries near a
/// small region need only a few of their members.
pub fn rel_geoms_near<R: Borrow<Relation>>(f: &Fetch, rels: &[R], near: impl Fn(&Rect) -> bool) -> Result<Vec<EGeom>> {
    let members = |t: ElemType| -> Vec<Id> {
        let mut v: Vec<Id> = rels.iter().flat_map(|r| r.borrow().members.iter().filter(|m| m.mtype == t).map(|m| m.id)).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    // the test of each cell once
    let mut tested: HashMap<Cell, bool> = HashMap::default();
    // The members of long routes lie in many cells, and a cell test against
    // a large `around` source is not cheap: the limits are checked between
    // the tests.
    let mut tests = 0usize;
    let mut stored_near = |kind: Kind, ids: &[Id]| -> Result<Vec<(Cell, Id)>> {
        let mut out = Vec::new();
        for (id, c) in f.snap.cells_of(kind, ids)? {
            let hit = match tested.get(&c) {
                Some(hit) => *hit,
                None => {
                    tests += 1;
                    if tests % 256 == 0 {
                        f.limits.check()?;
                    }
                    let hit = near(&c.loose_rect());
                    tested.insert(c, hit);
                    hit
                }
            };
            if hit {
                out.push((c, id));
            }
        }
        Ok(out)
    };
    let mut ways = f.ways_untagged_at(stored_near(Kind::Way, &members(ElemType::Way))?)?;
    #[cfg(test)]
    NEAR_WAYS_READ.set(ways.len());
    // an edge across the antimeridian leaves the box: kept
    ways.retain(|w| !w.missing && !w.bbox.is_empty() && (w.bbox.may_cross() || near(&w.bbox)));
    let wv = way_vertices(f, &ways)?;
    let node_ids: Vec<Id> = stored_near(Kind::Node, &members(ElemType::Node))?.into_iter().map(|(_, id)| id).collect();
    let npos = f.node_positions(&node_ids)?;
    Ok(rels
        .iter()
        .map(|r| {
            let r = r.borrow();
            let mut points = Vec::new();
            let mut lines = Vec::new();
            for m in &r.members {
                match m.mtype {
                    ElemType::Node => points.extend(npos.get(&m.id).copied()),
                    ElemType::Way => lines.extend(wv.get(&m.id).cloned()),
                    _ => {}
                }
            }
            EGeom::Multi { points, lines }
        })
        .collect())
}

/// Relation geometry reduced for a test against one bbox (`rects`, split at
/// the antimeridian): way members are decided by their stored bounding box
/// where possible. A way strictly inside becomes one point inside the bbox,
/// a way whose box misses the bbox is left out, and only the remaining ways
/// have their nodes loaded. The bbox test gives the same answer as on the
/// full geometry.
pub fn rel_geoms_for_bbox<R: Borrow<Relation>>(f: &Fetch, rels: &[R], rects: &[crate::geo::Rect]) -> Result<Vec<EGeom>> {
    let grow = |r: &crate::geo::Rect| crate::geo::Rect { y0: r.y0.saturating_sub(1), x0: r.x0.saturating_sub(1), y1: r.y1.saturating_add(1), x1: r.x1.saturating_add(1) };
    let mut way_ids = Vec::new();
    for r in rels {
        way_ids.extend(r.borrow().members.iter().filter(|m| m.mtype == ElemType::Way).map(|m| m.id));
    }
    way_ids.sort_unstable();
    way_ids.dedup();
    let ways = f.ways_untagged(&way_ids)?;
    // Per way: Some(point inside the bbox), None = needs its nodes; ways
    // that cannot intersect are absent.
    let mut decided: HashMap<Id, Option<Pt>> = HashMap::default();
    let mut open: Vec<&Way> = Vec::new();
    for w in &ways {
        let sb = w.bbox;
        if w.missing || sb.is_empty() {
            continue;
        }
        if sb.may_cross() {
            decided.insert(w.id, None);
            open.push(w);
            continue;
        }
        if !rects.iter().any(|r| grow(r).intersects(&sb)) {
            continue;
        }
        if rects.iter().any(|r| r.y0 < sb.y0 && r.x0 < sb.x0 && sb.y1 < r.y1 && sb.x1 < r.x1) {
            decided.insert(w.id, Some(Pt { y: sb.y0, x: sb.x0 }));
        } else {
            decided.insert(w.id, None);
            open.push(w);
        }
    }
    let wv = way_vertices(f, &open)?;
    let mut node_ids = Vec::new();
    let inside_way = |r: &Relation| r.members.iter().any(|m| m.mtype == ElemType::Way && matches!(decided.get(&m.id), Some(Some(_))));
    for r in rels {
        let r = r.borrow();
        if !inside_way(r) {
            node_ids.extend(r.members.iter().filter(|m| m.mtype == ElemType::Node).map(|m| m.id));
        }
    }
    let npos = f.node_positions(&node_ids)?;
    Ok(rels
        .iter()
        .map(|r| {
            let r = r.borrow();
            let mut points = Vec::new();
            let mut lines = Vec::new();
            for m in &r.members {
                match m.mtype {
                    ElemType::Node => points.extend(npos.get(&m.id).copied()),
                    ElemType::Way => match decided.get(&m.id) {
                        Some(Some(p)) => points.push(*p),
                        Some(None) => lines.extend(wv.get(&m.id).cloned()),
                        None => {}
                    },
                    _ => {}
                }
            }
            EGeom::Multi { points, lines }
        })
        .collect())
}

/// Input geometry for `around`: points and segments. The ends of the
/// segments are points of the source too: `around_match` tests them with
/// their segment (the box of a point lies within the box of a segment
/// ending there), so `points` holds only the others (nodes, lines of one
/// vertex), and the index and the cover take half the items. A vertex
/// between two segments of a line is tested once (`first_end`).
#[derive(Default)]
pub struct AroundSource {
    pub points: Vec<(f64, f64)>,
    pub segments: Vec<Segment>,
    /// Grid over the source, built by `prepare` for larger inputs.
    pub index: Option<AroundIndex>,
    /// `bbox_rects` of the radius given to `prepare`, for the clauses of
    /// every element type: 82 000 forests in a country are 1.5 x 10^7
    /// points and segments, which take most of a second to cover.
    cover: Option<(f64, Vec<crate::geo::Rect>)>,
    /// Cost of reading the region of the cover, set by its first use.
    pub cost: std::sync::OnceLock<u64>,
    /// Per segment, whether its first end is the second end of the segment
    /// before it (`add_line`): that end is tested there. Segments past the
    /// end of this list are tested at both ends.
    chained: Vec<bool>,
    /// Ways of a large set whose vertices are not loaded (`add_lazy`).
    lazy: LazyWays,
    /// `cartesian` of the points, set by `prepare` (`point_c`).
    points_c: Vec<crate::geo::V3>,
}

/// The ways of an `around` source that are known by their stored box only.
/// A set of 82 000 forests has 7.7 x 10^6 vertices, whose positions,
/// segments and index take most of the time of the statement, while the
/// candidates (the hotels of the country) come near a tenth of the ways.
/// The cover and the index take the box of such a way (with the bulge of
/// its edges, `Rect::arc_box`), which holds all its segments; the tests on
/// candidates take the vertices of the ways whose box they come near
/// (`lazy_near`, built into a source of its own by the caller).
#[derive(Default)]
struct LazyWays {
    /// The node ids of all ways, one after the other.
    nodes: Vec<Id>,
    /// Per way: where its ids start, and its stored box.
    ways: Vec<(u32, Rect)>,
    /// Per way: its box in the index (set by `prepare`), None when wide.
    boxes: Vec<Option<BBox>>,
    /// Ways whose vertices were loaded so far, over all calls.
    loaded: std::sync::atomic::AtomicUsize,
    /// All ways with their vertices, once the calls together have loaded
    /// as many as there are: the candidates are everywhere (a statement
    /// without a tag filter, in batches that each span the region).
    full: std::sync::OnceLock<std::sync::Arc<AroundSource>>,
}

/// The box of all points within `radius` of the edges of a way with the
/// stored box `b` (not `Rect::may_cross`).
fn lazy_box(b: &Rect, radius: f64) -> BBox {
    use crate::geo::{ilon_of_x, lat_of, lon_of};
    let a = b.arc_box();
    radius_box(lat_of(a.y0), lat_of(a.y1), lon_of(ilon_of_x(a.x0)), lon_of(ilon_of_x(a.x1)), radius)
}

/// Grid over the boxes within the radius of each source point and segment
/// (items: points first, then segments, then the lazy ways), so that a test
/// only looks at nearby items.
pub struct AroundIndex {
    cell: f64,
    cells: HashMap<(i32, i32), Vec<u32>>,
    /// Items whose box crosses the antimeridian or reaches a pole.
    wide: Vec<u32>,
}

/// Cells of the grid covered by a box, or None if too many.
fn grid_span(cell: f64, south: f64, west: f64, north: f64, east: f64, limit: usize) -> Option<(i32, i32, i32, i32)> {
    let f = |v: f64, off: f64| ((v + off) / cell).floor() as i32;
    let (y0, y1, x0, x1) = (f(south, 90.0), f(north, 90.0), f(west, 180.0), f(east, 180.0));
    let n = (y1 - y0 + 1) as usize * (x1 - x0 + 1) as usize;
    (n <= limit).then_some((y0, y1, x0, x1))
}

impl AroundIndex {
    /// The index, and the boxes of the lazy ways in it.
    fn build(src: &AroundSource, radius: f64) -> (AroundIndex, Vec<Option<BBox>>) {
        // Boxes slightly larger than the C++ radius box, so that the exact
        // test never misses (also covers the 1e-7 equality for radius 0).
        let slack = 1e-6;
        // (item, box); an item without a usable box is wide. A long segment
        // has a box for each of its pieces (`segment_spans`).
        let mut boxes: Vec<(u32, BBox)> = Vec::with_capacity(src.points.len() + src.segments.len());
        let mut wide = Vec::new();
        let fits = |b: BBox| -> Option<BBox> {
            let b = BBox { south: b.south - slack, west: b.west - slack, north: b.north + slack, east: b.east + slack };
            (b.west >= -180.0 && b.east <= 180.0 && b.south > -90.0 && b.north < 90.0).then_some(b)
        };
        for (i, &(lat, lon)) in src.points.iter().enumerate() {
            match fits(radius_box(lat, lat, lon, lon, radius * 1.01)) {
                Some(b) => boxes.push((i as u32, b)),
                None => wide.push(i as u32),
            }
        }
        let np = src.points.len();
        for (k, s) in src.segments.iter().enumerate() {
            let i = (np + k) as u32;
            if (s.a.1 - s.b.1).abs() > 180.0 {
                wide.push(i);
                continue;
            }
            let first = boxes.len();
            let mut ok = true;
            segment_spans(s, radius, |lo, hi, w, e, r| match fits(radius_box(lo, hi, w, e, r * 1.01)) {
                Some(b) => boxes.push((i, b)),
                None => ok = false,
            });
            if !ok {
                boxes.truncate(first);
                wide.push(i);
            }
        }
        let base = (np + src.segments.len()) as u32;
        let mut lazy_boxes = Vec::with_capacity(src.lazy.ways.len());
        for (k, (_, b)) in src.lazy.ways.iter().enumerate() {
            let fit = fits(lazy_box(b, radius * 1.01));
            match fit {
                Some(b) => boxes.push((base + k as u32, b)),
                None => wide.push(base + k as u32),
            }
            lazy_boxes.push(fit);
        }
        // Cell size: about the box size of a point, coarser if the
        // segments would land in too many cells.
        let mut cell = (radius * 720.0 / 40_000_000.0).max(1e-4);
        let budget = 8 * boxes.len() + 100_000;
        while cell < 10.0 {
            let total: usize = boxes.iter().map(|(_, b)| grid_span(cell, b.south, b.west, b.north, b.east, usize::MAX).map_or(0, |(y0, y1, x0, x1)| (y1 - y0 + 1) as usize * (x1 - x0 + 1) as usize)).sum();
            if total <= budget {
                break;
            }
            cell *= 2.0;
        }
        let mut cells: HashMap<(i32, i32), Vec<u32>> = HashMap::default();
        // an item whose boxes cannot all be placed is wide: its entries
        // in the cells stay, and are found twice at most
        let mut last_wide = None;
        for (i, b) in &boxes {
            match grid_span(cell, b.south, b.west, b.north, b.east, budget) {
                Some((y0, y1, x0, x1)) => {
                    for y in y0..=y1 {
                        for x in x0..=x1 {
                            // the boxes of an item follow each other
                            let v = cells.entry((y, x)).or_default();
                            if v.last() != Some(i) {
                                v.push(*i);
                            }
                        }
                    }
                }
                None if last_wide != Some(*i) => {
                    wide.push(*i);
                    last_wide = Some(*i);
                    if *i >= base {
                        lazy_boxes[(*i - base) as usize] = None;
                    }
                }
                None => {}
            }
        }
        (AroundIndex { cell, cells, wide }, lazy_boxes)
    }

    /// Items whose box may contain the point: the wide ones and those of
    /// its cell (a cell of a source with a radius of hundreds of km holds
    /// 10^4 items, which are not copied for each point).
    pub fn at_point(&self, lat: f64, lon: f64) -> impl Iterator<Item = u32> + '_ {
        let cell = grid_span(self.cell, lat, lon, lat, lon, 1).and_then(|(y, _, x, _)| self.cells.get(&(y, x)));
        self.wide.iter().copied().chain(cell.into_iter().flatten().copied())
    }

    /// Whether the box of an item may meet the box (degrees, west <= east):
    /// the cells of the box are looked up when there are at most `probe`,
    /// else the cells in use are compared when there are at most `scan`.
    /// None if unknown: items across the antimeridian or at a pole, or both
    /// limits passed.
    fn box_hit(&self, south: f64, west: f64, north: f64, east: f64, probe: usize, scan: usize) -> Option<bool> {
        if !self.wide.is_empty() {
            return None;
        }
        if let Some((y0, y1, x0, x1)) = grid_span(self.cell, south, west, north, east, probe) {
            return Some((y0..=y1).any(|y| (x0..=x1).any(|x| self.cells.contains_key(&(y, x)))));
        }
        if self.cells.len() > scan {
            return None;
        }
        let (y0, y1, x0, x1) = grid_span(self.cell, south, west, north, east, usize::MAX)?;
        Some(self.cells.keys().any(|&(y, x)| (y0..=y1).contains(&y) && (x0..=x1).contains(&x)))
    }

    /// Items whose box may meet the segment; false if all items must be tested.
    pub fn near_segment(&self, s: &Segment, out: &mut Vec<u32>) -> bool {
        out.clear();
        if (s.a.1 - s.b.1).abs() > 180.0 {
            return false;
        }
        let (lo, hi) = s.lat_extent();
        let Some((y0, y1, x0, x1)) = grid_span(self.cell, lo, s.a.1.min(s.b.1), hi, s.a.1.max(s.b.1), 4096) else { return false };
        out.extend_from_slice(&self.wide);
        for y in y0..=y1 {
            for x in x0..=x1 {
                if let Some(v) = self.cells.get(&(y, x)) {
                    out.extend_from_slice(v);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        true
    }
}

/// C++ radius box of a point (`add_coord`): latitude margin from a 40 000 km
/// meridian, longitude margin scaled at the poleward edge, the one farther
/// from the equator, where a degree of longitude is shortest. (Taking the
/// edge by `lat_hi` against `lat_lo` took the equator side for a single point
/// in the northern hemisphere: the box was too narrow, and points near its
/// east and west edges were never tested.)
fn radius_box(lat_lo: f64, lat_hi: f64, lon_lo: f64, lon_hi: f64, radius: f64) -> BBox {
    let dlat = radius * 360.0 / 40_000_000.0;
    let south = lat_lo - dlat;
    let north = lat_hi + dlat;
    // A circle that reaches a pole takes every longitude. (C++ caps the
    // latitude of the scale at 89.9 degrees, so near the poles its box
    // misses points at other longitudes, and at a pole it finds nothing.)
    if north >= 90.0 || south <= -90.0 {
        return BBox { south: south.max(-90.0), north: north.min(90.0), west: -180.0, east: 180.0 };
    }
    let scl = if north.abs() > south.abs() { north } else { south };
    let dlon = dlat / scl.to_radians().cos();
    BBox { south, north, west: lon_lo - dlon, east: lon_hi + dlon }
}

/// The boxes that hold a segment of an `around` source, for the radius
/// box of each: latitudes (with the apex of the arc), longitudes west to
/// east (east past 180 degrees for a segment across the antimeridian) and
/// the radius to take. A segment that runs far in both directions (a line
/// `(around:r,lat,lon,lat,lon)` between two towns) is cut into pieces, so
/// that the boxes follow it like stairs: its one box holds a region many
/// times that within the radius, all of which would be read. A piece
/// reaches about as far in the shorter direction as the radius, and not
/// under 700 m, the size of a tile. The pieces are arcs of the same great
/// circle; their ends are rounded, for which their radius is 1 mm longer.
fn segment_spans(s: &Segment, radius: f64, mut f: impl FnMut(f64, f64, f64, f64, f64)) {
    const DEG: f64 = 40_000_000.0 / 360.0;
    const MAX_PIECES: f64 = 256.0;
    let (dlat, dlon) = ((s.a.0 - s.b.0).abs(), (s.a.1 - s.b.1).abs());
    let step = radius.max(700.0);
    let whole = |f: &mut dyn FnMut(f64, f64, f64, f64, f64)| {
        let (lo, hi) = s.lat_extent();
        let (w, e) = if dlon > 180.0 {
            // Across the antimeridian: from the eastern end eastwards.
            (s.a.1.max(s.b.1), s.a.1.min(s.b.1) + 360.0)
        } else {
            (s.a.1.min(s.b.1), s.a.1.max(s.b.1))
        };
        f(lo, hi, w, e, radius);
    };
    // the common case first, without trigonometry: a degree of longitude
    // is at most as long as one of latitude
    if dlat.min(dlon) * DEG <= step || dlon > 180.0 || !radius.is_finite() {
        return whole(&mut f);
    }
    let short = (dlat * DEG).min(dlon * DEG * ((s.a.0 + s.b.0) / 2.0).to_radians().cos());
    let n = (short / step).ceil().min(MAX_PIECES);
    // arcs up to a quarter circle, away from the poles
    if n < 2.0 || s.ca.dot(&s.cb) <= 0.0 || s.a.0.abs().max(s.b.0.abs()) > 85.0 {
        return whole(&mut f);
    }
    let n = n as usize;
    let mut prev = s.a;
    for i in 1..=n {
        let next = if i == n {
            s.b
        } else {
            let t = i as f64 / n as f64;
            let v = s.ca.scale(1.0 - t).add(&s.cb.scale(t));
            let v = v.scale(1.0 / v.dot(&v).sqrt());
            (v.0.clamp(-1.0, 1.0).asin().to_degrees(), v.1.atan2(v.2).to_degrees())
        };
        let (lo, hi) = Segment::new(prev.0, prev.1, next.0, next.1).lat_extent();
        f(lo, hi, prev.1.min(next.1), prev.1.max(next.1), radius + 0.001);
        prev = next;
    }
}

fn box_rects(b: BBox) -> Vec<crate::geo::Rect> {
    if b.east - b.west >= 360.0 {
        return BBox { west: -180.0, east: 180.0, ..b }.rects();
    }
    let mut b = b;
    if b.west < -180.0 {
        b.west += 360.0;
    }
    if b.east > 180.0 {
        b.east -= 360.0;
    }
    b.rects()
}

impl AroundSource {
    /// Build the grid index when the source is large enough to need one,
    /// and the cover of the radius.
    pub fn prepare(&mut self, radius: f64) {
        self.prepare_index(radius);
        self.cover = Some((radius, self.cover_rects(radius)));
    }

    /// `prepare` without the cover, for a source that only answers tests.
    pub fn prepare_index(&mut self, radius: f64) {
        self.points_c = self.points.iter().map(|p| crate::geo::cartesian(p.0, p.1)).collect();
        if self.points.len() + self.segments.len() > 32 || self.has_lazy() {
            let (index, boxes) = AroundIndex::build(self, radius);
            self.index = Some(index);
            self.lazy.boxes = boxes;
        }
    }

    /// `cartesian` of point `i`: the test of a way against the source asks
    /// for it with every segment of the way.
    #[inline]
    pub fn point_c(&self, i: usize) -> crate::geo::V3 {
        match self.points_c.get(i) {
            Some(c) if self.points_c.len() == self.points.len() => *c,
            _ => crate::geo::cartesian(self.points[i].0, self.points[i].1),
        }
    }

    /// Add a way of the input set by its node ids and stored box, without
    /// its vertices (`LazyWays`): not for a box that is empty or
    /// `Rect::may_cross`. Before `prepare`.
    pub fn add_lazy(&mut self, w: &Way) {
        self.lazy.ways.push((self.lazy.nodes.len() as u32, w.bbox));
        self.lazy.nodes.extend_from_slice(&w.nodes);
    }

    pub fn has_lazy(&self) -> bool {
        !self.lazy.ways.is_empty()
    }

    /// The source of all lazy ways with their vertices, when it was built.
    pub fn lazy_full(&self) -> Option<std::sync::Arc<AroundSource>> {
        self.lazy.full.get().cloned()
    }

    /// Keeps the source of all lazy ways for the tests that follow.
    pub fn set_lazy_full(&self, full: std::sync::Arc<AroundSource>) {
        let _ = self.lazy.full.set(full);
    }

    /// Counts `n` more lazy ways as loaded; whether that makes more than
    /// there are, so that the next load should take all of them, once.
    pub fn lazy_loaded(&self, n: usize) -> bool {
        self.lazy.loaded.fetch_add(n, std::sync::atomic::Ordering::Relaxed) + n > self.lazy.ways.len()
    }

    /// The node ids of every lazy way.
    pub fn lazy_all(&self) -> Vec<&[Id]> {
        (0..self.lazy.ways.len()).map(|k| self.lazy_ids(k)).collect()
    }

    fn lazy_ids(&self, k: usize) -> &[Id] {
        let start = self.lazy.ways[k].0 as usize;
        let end = self.lazy.ways.get(k + 1).map_or(self.lazy.nodes.len(), |w| w.0 as usize);
        &self.lazy.nodes[start..end]
    }

    /// The node ids of each lazy way that may come within the radius of an
    /// element with one of the boxes (key space; with the bulge of its
    /// edges where it has edges, `Rect::arc_box`): the ways whose box in
    /// the index meets a box. All of them for a box over more than 4096
    /// cells of the grid (also boxes across the antimeridian, which span
    /// it). After `prepare`.
    pub fn lazy_near(&self, boxes: &[Rect]) -> Vec<&[Id]> {
        use crate::geo::{ilon_of_x, lat_of, lon_of};
        let count = self.lazy.ways.len();
        let Some(ix) = &self.index else { return Vec::new() };
        let base = (self.points.len() + self.segments.len()) as u32;
        let mut need = vec![false; count];
        let mut all = false;
        for i in &ix.wide {
            if *i >= base {
                need[(*i - base) as usize] = true;
            }
        }
        let m = 1e-6;
        for b in boxes {
            if b.is_empty() {
                continue;
            }
            let (s, w, n, e) = (lat_of(b.y0) - m, lon_of(ilon_of_x(b.x0)) - m, lat_of(b.y1) + m, lon_of(ilon_of_x(b.x1)) + m);
            let Some((y0, y1, x0, x1)) = grid_span(ix.cell, s, w, n, e, 4096) else {
                all = true;
                break;
            };
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let Some(items) = ix.cells.get(&(y, x)) else { continue };
                    // the lazy ways are the last items of a cell
                    for i in items.iter().rev().take_while(|i| **i >= base) {
                        let k = (*i - base) as usize;
                        if !need[k] {
                            need[k] = self.lazy.boxes[k].is_none_or(|lb| lb.south <= n && s <= lb.north && lb.west <= e && w <= lb.east);
                        }
                    }
                }
            }
        }
        (0..count).filter(|k| all || need[*k]).map(|k| self.lazy_ids(k)).collect()
    }

    /// Add an element's geometry. Ways with missing nodes contribute nothing.
    pub fn add(&mut self, g: &EGeom) {
        match g {
            EGeom::Point(p) => self.points.push((p.lat(), p.lon())),
            EGeom::Line(v) => self.add_way(v),
            EGeom::Multi { points, lines } => {
                for p in points {
                    self.points.push((p.lat(), p.lon()));
                }
                for l in lines {
                    self.add_way(l);
                }
            }
            EGeom::Empty => {}
        }
    }

    /// The vertices of a way, unless one is missing.
    fn add_way(&mut self, v: &[Option<Pt>]) {
        if v.iter().all(|p| p.is_some()) {
            let pts: Vec<(f64, f64)> = v.iter().flatten().map(|p| (p.lat(), p.lon())).collect();
            self.add_line(&pts);
        }
    }

    /// A line through the points: its segments, or the point of a line of
    /// one point.
    pub fn add_line(&mut self, pts: &[(f64, f64)]) {
        match pts {
            [] => {}
            [p] => self.points.push(*p),
            _ => {
                let mut prev = (pts[0], crate::geo::cartesian(pts[0].0, pts[0].1));
                self.chained.resize(self.segments.len(), false);
                for (k, &q) in pts[1..].iter().enumerate() {
                    let cq = crate::geo::cartesian(q.0, q.1);
                    self.segments.push(Segment::with_cartesian(prev.0, q, prev.1, cq));
                    self.chained.push(k > 0);
                    prev = (q, cq);
                }
            }
        }
    }

    /// Whether the first end of segment `i` needs its own test: not when
    /// it is the second end of segment `i - 1`, which is tested with that
    /// one. The index gives segment `i - 1` wherever it gives a test that
    /// the shared end can pass: its box holds the box of that end.
    #[inline]
    pub fn first_end(&self, i: usize) -> bool {
        !self.chained.get(i).copied().unwrap_or(false)
    }

    /// Whether a stored element box (key space, not `Rect::may_cross`) may
    /// come within the radius: it meets one of `rects` (from
    /// `bbox_rects`, grown by one unit). With a grid index, its cells answer
    /// instead of every rectangle when the box covers fewer cells than an
    /// eighth of the rectangles (a cell lookup costs more than a rectangle
    /// test); the boxes of the cells' items contain the rectangles. A box
    /// over more cells (a large cell of the store) is compared with the
    /// cells in use when they are fewer than an eighth of the rectangles.
    pub fn near_box(&self, rects: &[crate::geo::Rect], b: &crate::geo::Rect) -> bool {
        use crate::geo::{ilon_of_x, lat_of, lon_of};
        if let Some(ix) = &self.index {
            let m = 1e-6;
            let (s, w, n, e) = (lat_of(b.y0) - m, lon_of(ilon_of_x(b.x0)) - m, lat_of(b.y1) + m, lon_of(ilon_of_x(b.x1)) + m);
            if let Some(hit) = ix.box_hit(s, w, n, e, (rects.len() / 8).min(4096), rects.len() / 8) {
                return hit;
            }
        }
        let grow = |r: &crate::geo::Rect| crate::geo::Rect { y0: r.y0.saturating_sub(1), x0: r.x0.saturating_sub(1), y1: r.y1.saturating_add(1), x1: r.x1.saturating_add(1) };
        rects.iter().any(|r| grow(r).intersects(b))
    }

    /// Conservative rectangles covering everything within `radius`; those
    /// of `prepare` when it had the same radius.
    pub fn bbox_rects(&self, radius: f64) -> std::borrow::Cow<'_, [crate::geo::Rect]> {
        match &self.cover {
            Some((r, rects)) if r.to_bits() == radius.to_bits() => std::borrow::Cow::Borrowed(rects),
            _ => std::borrow::Cow::Owned(self.cover_rects(radius)),
        }
    }

    fn cover_rects(&self, radius: f64) -> Vec<crate::geo::Rect> {
        let mut out = Vec::new();
        for &(lat, lon) in &self.points {
            out.extend(box_rects(radius_box(lat, lat, lon, lon, radius)));
        }
        for s in &self.segments {
            segment_spans(s, radius, |lo, hi, w, e, r| out.extend(box_rects(radius_box(lo, hi, w, e, r))));
        }
        for (_, b) in &self.lazy.ways {
            out.extend(box_rects(lazy_box(b, radius)));
        }
        out
    }
}

/// Polygon for `(poly:...)` on quantized integer coordinates.
///
/// Edges from below -90 to above +90 degrees longitude (or back) are taken
/// across the antimeridian, as in the C++ code: the ring is unwrapped into a
/// continuous longitude range and tests also try points shifted by 360.
pub struct Polygon {
    pub pts: Vec<(f64, f64)>,
}

type IP = (i64, i64);

const FULL: i64 = 3_600_000_000;
const QUARTER: i64 = 900_000_000;

fn q(lat: f64, lon: f64) -> IP {
    (crate::geo::ilat(lat) as i64, crate::geo::ilon(lon) as i64)
}

fn qp(p: Pt) -> IP {
    (p.y as i64, p.ilon() as i64)
}

fn orient(a: IP, b: IP, c: IP) -> i128 {
    (b.0 - a.0) as i128 * (c.1 - a.1) as i128 - (b.1 - a.1) as i128 * (c.0 - a.0) as i128
}

fn on_seg(p: IP, a: IP, b: IP) -> bool {
    orient(a, b, p) == 0 && p.0 >= a.0.min(b.0) && p.0 <= a.0.max(b.0) && p.1 >= a.1.min(b.1) && p.1 <= a.1.max(b.1)
}

/// Unwrap longitudes so that antimeridian edges become short.
fn unwrap(pts: &[IP]) -> Vec<IP> {
    let mut out = Vec::with_capacity(pts.len());
    let mut offset = 0i64;
    for (i, p) in pts.iter().enumerate() {
        if i > 0 {
            let prev = pts[i - 1].1;
            if prev < -QUARTER && p.1 > QUARTER {
                offset -= FULL;
            } else if prev > QUARTER && p.1 < -QUARTER {
                offset += FULL;
            }
        }
        out.push((p.0, p.1 + offset));
    }
    out
}

impl Polygon {
    fn ring(&self) -> Vec<IP> {
        let raw: Vec<IP> = self.pts.iter().map(|(la, lo)| q(*la, *lo)).collect();
        unwrap(&raw)
    }

    fn edges(&self) -> Vec<(IP, IP)> {
        let r = self.ring();
        let mut e: Vec<(IP, IP)> = (0..r.len() - 1).map(|i| (r[i], r[i + 1])).collect();
        // Closing edge, also unwrapped relative to the last vertex.
        let last = r[r.len() - 1];
        let first_raw = q(self.pts[0].0, self.pts[0].1);
        let mut f = first_raw;
        let lr = last.1.rem_euclid(FULL);
        let lr = if lr > FULL / 2 { lr - FULL } else { lr };
        let base = last.1 - lr;
        f.1 += base;
        if lr < -QUARTER && first_raw.1 > QUARTER {
            f.1 -= FULL;
        } else if lr > QUARTER && first_raw.1 < -QUARTER {
            f.1 += FULL;
        }
        e.push((last, f));
        // If the ring does not close in unwrapped space, close it at the start.
        if f != r[0] {
            e.push((f, r[0]));
        }
        e
    }

    fn shifts(p: IP) -> [IP; 3] {
        [p, (p.0, p.1 + FULL), (p.0, p.1 - FULL)]
    }

    fn on_border(edges: &[(IP, IP)], p: IP) -> bool {
        edges.iter().any(|(a, b)| on_seg(p, *a, *b))
    }

    /// Even-odd test; doubled coordinates allow testing segment midpoints.
    fn inside2(edges: &[(IP, IP)], p2: IP) -> bool {
        let mut inside = false;
        for (a, b) in edges {
            let (a, b) = ((a.0 * 2, a.1 * 2), (b.0 * 2, b.1 * 2));
            if (a.0 > p2.0) != (b.0 > p2.0) {
                let lhs = (p2.1 - a.1) as i128 * (b.0 - a.0) as i128;
                let rhs = (p2.0 - a.0) as i128 * (b.1 - a.1) as i128;
                let left = if b.0 > a.0 { lhs < rhs } else { lhs > rhs };
                if left {
                    inside = !inside;
                }
            }
        }
        inside
    }

    /// The edges computed once, for testing many points, lines and boxes.
    pub fn prepare(&self) -> PolyTest {
        let edges = self.edges();
        let (mut lo, mut hi) = ((i64::MAX, i64::MAX), (i64::MIN, i64::MIN));
        for p in edges.iter().flat_map(|(a, b)| [a, b]) {
            lo = (lo.0.min(p.0), lo.1.min(p.1));
            hi = (hi.0.max(p.0), hi.1.max(p.1));
        }
        PolyTest { edges, lo, hi }
    }

    /// Whether the rectangle lies strictly inside the polygon, without
    /// shifts: its corners are inside and not on the border, and no edge
    /// meets it. Every point in it then passes the node and way tests
    /// (a way with all its vertices there is strictly inside).
    pub fn contains_rect(&self, r: &crate::geo::Rect) -> bool {
        self.prepare().contains_rect(r)
    }

    /// Node test: the border counts as inside.
    pub fn contains_pt(&self, p: Pt) -> bool {
        self.prepare().contains_pt(p)
    }

    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        self.prepare().contains_ip(q(lat, lon))
    }

    /// Relation member node test (C++ `add_border = false`): a point on an
    /// edge counts, a point on a vertex shared by two edges cancels out.
    pub fn contains_member(&self, p: Pt) -> bool {
        self.prepare().contains_member(p)
    }

    pub fn bbox(&self) -> BBox {
        let r = self.ring();
        let (mut s, mut n, mut w, mut e) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
        for p in &r {
            s = s.min(p.0);
            n = n.max(p.0);
            w = w.min(p.1);
            e = e.max(p.1);
        }
        let lat = |v: i64| crate::geo::lat_of(v as u32);
        let lon = |v: i64| v as f64 / 10_000_000.0;
        let (mut west, mut east) = (lon(w), lon(e));
        if east - west >= 360.0 {
            west = -180.0;
            east = 180.0;
        } else {
            if west < -180.0 {
                west += 360.0;
            }
            if east > 180.0 {
                east -= 360.0;
            }
        }
        BBox { south: lat(s), north: lat(n), west, east }
    }

    pub fn matches_line(&self, v: &[Pt], add_border: bool) -> bool {
        self.prepare().matches_line(v, add_border)
    }

    /// Spatial test of an element (ways with missing nodes never match).
    pub fn matches(&self, g: &EGeom) -> bool {
        let t = self.prepare();
        let line = |l: &Vec<Option<Pt>>, add_border: bool| -> bool {
            if l.is_empty() || l.iter().any(|x| x.is_none()) {
                return false;
            }
            let pts: Vec<Pt> = l.iter().flatten().copied().collect();
            match pts.len() {
                1 => t.contains_pt(pts[0]),
                _ => t.matches_line(&pts, add_border),
            }
        };
        match g {
            EGeom::Point(p) => t.contains_pt(*p),
            EGeom::Line(v) => line(v, true),
            EGeom::Multi { points, lines } => points.iter().any(|p| t.contains_member(*p)) || lines.iter().any(|l| line(l, false)),
            EGeom::Empty => false,
        }
    }
}

/// Where a rectangle lies against a polygon (`PolyTest::rect_pos`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RectPos {
    /// Strictly inside, without shifts (`contains_rect`).
    Inside,
    /// No point of the closed rectangle, also shifted by 360 degrees, is
    /// inside the polygon or on its border.
    Apart,
    /// Across the border, or not decided.
    Border,
}

/// A polygon with its edges (unwrapped, see `Polygon`) and their box,
/// computed once (`Polygon::prepare`). Points outside the box are neither
/// inside nor on the border, so most shifted points cost one comparison.
pub struct PolyTest {
    edges: Vec<(IP, IP)>,
    lo: IP,
    hi: IP,
}

impl PolyTest {
    fn in_box(&self, p: IP) -> bool {
        p.0 >= self.lo.0 && p.0 <= self.hi.0 && p.1 >= self.lo.1 && p.1 <= self.hi.1
    }

    fn on_border(&self, p: IP) -> bool {
        Polygon::on_border(&self.edges, p)
    }

    fn inside2(&self, p2: IP) -> bool {
        Polygon::inside2(&self.edges, p2)
    }

    fn contains_ip(&self, p: IP) -> bool {
        Polygon::shifts(p).into_iter().any(|ip| self.in_box(ip) && (self.on_border(ip) || self.inside2((ip.0 * 2, ip.1 * 2))))
    }

    /// Node test: the border counts as inside.
    pub fn contains_pt(&self, p: Pt) -> bool {
        self.contains_ip(qp(p))
    }

    /// Relation member node test (C++ `add_border = false`): a point on an
    /// edge counts, a point on a vertex shared by two edges cancels out.
    pub fn contains_member(&self, p: Pt) -> bool {
        Polygon::shifts(qp(p)).into_iter().any(|ip| {
            if !self.in_box(ip) {
                return false;
            }
            let hits = self.edges.iter().filter(|(a, b)| on_seg(ip, *a, *b)).count();
            if hits == 0 {
                return self.inside2((ip.0 * 2, ip.1 * 2));
            }
            hits % 2 == 1
        })
    }

    /// Whether an edge meets the closed rectangle `lo`..`hi`: their boxes
    /// overlap and the corners are not all on one side of the edge's line.
    fn edge_meets(&self, lo: IP, hi: IP) -> bool {
        let corners = [lo, (lo.0, hi.1), (hi.0, lo.1), hi];
        self.edges.iter().any(|(a, b)| {
            let overlap = a.0.min(b.0) <= hi.0 && a.0.max(b.0) >= lo.0 && a.1.min(b.1) <= hi.1 && a.1.max(b.1) >= lo.1;
            overlap && {
                let sides = corners.map(|c| orient(*a, *b, c).signum());
                !(sides.iter().all(|s| *s > 0) || sides.iter().all(|s| *s < 0))
            }
        })
    }

    /// See `Polygon::contains_rect`.
    pub fn contains_rect(&self, r: &crate::geo::Rect) -> bool {
        if r.is_empty() {
            return false;
        }
        let (lo, hi) = (qp(Pt { y: r.y0, x: r.x0 }), qp(Pt { y: r.y1, x: r.x1 }));
        let corners = [lo, (lo.0, hi.1), (hi.0, lo.1), hi];
        corners.iter().all(|c| !self.on_border(*c) && self.inside2((c.0 * 2, c.1 * 2))) && !self.edge_meets(lo, hi)
    }

    /// Where the stored box of a way lies: `Inside`, every vertex strictly
    /// inside, passes the way tests; `Apart`, every vertex and segment
    /// outside at every shift, fails them. A connected rectangle that no
    /// edge meets is on one side of the border, which one of its corners
    /// tells. Boxes that may hold an edge across the antimeridian
    /// (`Rect::may_cross`) are `Border`: `matches_line` unwraps it.
    pub fn rect_pos(&self, r: &crate::geo::Rect) -> RectPos {
        if r.is_empty() {
            return RectPos::Border;
        }
        if self.contains_rect(r) {
            return RectPos::Inside;
        }
        if r.may_cross() {
            return RectPos::Border;
        }
        let (lo, hi) = (qp(Pt { y: r.y0, x: r.x0 }), qp(Pt { y: r.y1, x: r.x1 }));
        for shift in [0, FULL, -FULL] {
            let (lo, hi) = ((lo.0, lo.1 + shift), (hi.0, hi.1 + shift));
            if hi.0 < self.lo.0 || lo.0 > self.hi.0 || hi.1 < self.lo.1 || lo.1 > self.hi.1 {
                continue;
            }
            if self.edge_meets(lo, hi) || self.inside2((lo.0 * 2, lo.1 * 2)) {
                return RectPos::Border;
            }
        }
        RectPos::Apart
    }

    /// Way test: a vertex strictly inside, a proper crossing of the border,
    /// a segment midpoint strictly inside, or (`add_border`) a segment
    /// running along the border. Touching the border in a single point does
    /// not count.
    fn matches_line_edges(edges: &[(IP, IP)], pts: &[IP], add_border: bool) -> bool {
        let strictly_inside = |p: IP| !Polygon::on_border(edges, p) && Polygon::inside2(edges, (p.0 * 2, p.1 * 2));
        if pts.iter().any(|p| strictly_inside(*p)) {
            return true;
        }
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let mid2 = (a.0 + b.0, a.1 + b.1);
            let mid_on_border = edges.iter().any(|(c, d)| on_seg(mid2, (c.0 * 2, c.1 * 2), (d.0 * 2, d.1 * 2)));
            if !mid_on_border && Polygon::inside2(edges, mid2) {
                return true;
            }
            for (c, d) in edges {
                let o1 = orient(*c, *d, a);
                let o2 = orient(*c, *d, b);
                let o3 = orient(a, b, *c);
                let o4 = orient(a, b, *d);
                if ((o1 > 0 && o2 < 0) || (o1 < 0 && o2 > 0)) && ((o3 > 0 && o4 < 0) || (o3 < 0 && o4 > 0)) {
                    return true;
                }
                if add_border && o1 == 0 && o2 == 0 && a != b {
                    let along = |p: IP| on_seg(p, *c, *d);
                    let back = |p: IP| on_seg(p, a, b);
                    let shared: Vec<IP> = [a, b].into_iter().filter(|p| along(*p)).chain([*c, *d].into_iter().filter(|p| back(*p))).collect();
                    if shared.iter().any(|p| *p != shared[0]) {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub fn matches_line(&self, v: &[Pt], add_border: bool) -> bool {
        let raw: Vec<IP> = v.iter().map(|p| qp(*p)).collect();
        let pts = unwrap(&raw);
        [0, FULL, -FULL].into_iter().any(|shift| {
            let shifted: Vec<IP> = pts.iter().map(|p| (p.0, p.1 + shift)).collect();
            Self::matches_line_edges(&self.edges, &shifted, add_border)
        })
    }
}

/// Planar segment intersection (inclusive).
pub fn seg_cross(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    fn orient(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> f64 {
        (q.1 - p.1) * (r.0 - p.0) - (q.0 - p.0) * (r.1 - p.1)
    }
    fn within(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> bool {
        r.0 >= p.0.min(q.0) && r.0 <= p.0.max(q.0) && r.1 >= p.1.min(q.1) && r.1 <= p.1.max(q.1)
    }
    let d1 = orient(c, d, a);
    let d2 = orient(c, d, b);
    let d3 = orient(a, b, c);
    let d4 = orient(a, b, d);
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0)) && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0)) {
        return true;
    }
    (d1 == 0.0 && within(c, d, a)) || (d2 == 0.0 && within(c, d, b)) || (d3 == 0.0 && within(a, b, c)) || (d4 == 0.0 && within(a, b, d))
}

#[cfg(test)]
mod tests {
    use super::radius_box;
    use crate::geo::{ARC_TO_METER, great_circle_dist};

    /// The point at `dist` metres from (lat, lon) in direction `bearing`
    /// (degrees), on the sphere of `great_circle_dist`.
    fn destination(lat: f64, lon: f64, bearing: f64, dist: f64) -> (f64, f64) {
        let (p1, l1, th, d) = (lat.to_radians(), lon.to_radians(), bearing.to_radians(), dist / ARC_TO_METER);
        let p2 = (p1.sin() * d.cos() + p1.cos() * d.sin() * th.cos()).asin();
        let l2 = l1 + (th.sin() * d.sin() * p1.cos()).atan2(d.cos() - p1.sin() * p2.sin());
        (p2.to_degrees(), l2.to_degrees())
    }

    fn inside(lat: f64, lon: f64, r: f64, p: (f64, f64)) -> bool {
        let b = radius_box(lat, lat, lon, lon, r);
        p.0 >= b.south && p.0 <= b.north && p.1 >= b.west && p.1 <= b.east
    }

    /// A circle that reaches a pole takes every longitude, and near a pole
    /// the box holds the whole circle (the scale latitude was capped at 89.9:
    /// around:2000 at the Amundsen-Scott station found 1 of 6 named nodes).
    #[test]
    fn radius_box_at_the_poles() {
        for (lat, lon) in [(90.0, 0.0), (90.0, 123.0), (-90.0, -180.0), (89.995, 45.0), (-89.998, -139.2728), (89.95, 10.0), (89.9, 10.0), (-89.5, 0.0), (89.0, 179.9)] {
            for r in [0.0, 1_000.0, 2_000.0, 50_000.0] {
                let b = radius_box(lat, lat, lon, lon, r);
                let full = b.east - b.west >= 360.0;
                if great_circle_dist(lat, lon, 90f64.copysign(lat), 0.0) <= r {
                    assert!(full, "({lat}, {lon}) r {r} reaches the pole: {b:?}");
                }
                // Radius 0: the destination lands 3e-11 degrees off, at distance 0.
                for k in 0..if r > 0.0 { 72 } else { 0 } {
                    for f in [0.5, 0.99] {
                        let p = destination(lat, lon, k as f64 * 5.0, r * f);
                        if great_circle_dist(lat, lon, p.0, p.1) > r {
                            continue;
                        }
                        let lon_in = full || [p.1, p.1 - 360.0, p.1 + 360.0].iter().any(|x| *x >= b.west && *x <= b.east);
                        assert!(p.0 >= b.south && p.0 <= b.north && lon_in, "({lat}, {lon}) r {r} bearing {} -> {p:?} outside {b:?}", k * 5);
                    }
                }
            }
        }
    }

    // way(around:120000,...) near 52 degrees north: ways 119 km east of the
    // centre were missing.
    #[test]
    fn radius_box_reaches_the_east_edge() {
        let (lat, lon, r) = (52.3, 10.2, 120_000.0);
        let p = destination(lat, lon, 95.0, 119_500.0);
        assert!(great_circle_dist(lat, lon, p.0, p.1) <= r);
        assert!(inside(lat, lon, r, p), "{p:?} outside {:?}", radius_box(lat, lat, lon, lon, r));
    }

    /// A rectangle that contains_rect accepts has every point strictly
    /// inside: grid points pass the node test and are not on the border,
    /// segments across it pass the way test. A U shape whose notch the
    /// rectangle spans is not accepted, although its corners are inside.
    #[test]
    fn rect_strictly_inside_polygon() {
        use super::Polygon;
        use crate::geo::{Pt, Rect};
        let u = Polygon { pts: vec![(0.0, 0.0), (0.0, 3.0), (3.0, 3.0), (3.0, 2.0), (1.0, 2.0), (1.0, 1.0), (3.0, 1.0), (3.0, 0.0)] };
        let diamond = Polygon { pts: vec![(48.9, 2.35), (48.85, 2.42), (48.8, 2.35), (48.85, 2.28)] };
        let rect = |s: f64, w: f64, n: f64, e: f64| {
            let (a, b) = (Pt::from_ll(s, w), Pt::from_ll(n, e));
            Rect { y0: a.y, x0: a.x, y1: b.y, x1: b.x }
        };
        // across the notch of the U, corners inside
        assert!(!u.contains_rect(&rect(0.5, 0.5, 2.5, 2.5)));
        assert!(u.contains_rect(&rect(0.2, 0.2, 0.8, 2.8)));
        assert!(!u.contains_rect(&rect(0.0, 0.2, 0.8, 2.8)), "on the border");
        assert!(!u.contains_rect(&rect(0.5, 1.2, 1.0, 1.8)), "corners on the bottom of the notch");
        assert!(u.contains_rect(&rect(1.5, 0.2, 2.5, 0.8)));
        assert!(!u.contains_rect(&Rect::EMPTY));
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let (mut inside, mut outside) = (0, 0);
        for (poly, (s, w, size)) in [(&u, (-0.5, -0.5, 4.0)), (&diamond, (48.79, 2.27, 0.16))] {
            for _ in 0..3000 {
                let (la, lo) = (s + rnd() * size, w + rnd() * size);
                let (dla, dlo) = (rnd() * size * 0.2, rnd() * size * 0.2);
                let r = rect(la, lo, la + dla, lo + dlo);
                if !poly.contains_rect(&r) {
                    outside += 1;
                    continue;
                }
                inside += 1;
                let at = |fy: f64, fx: f64| Pt { y: r.y0 + ((r.y1 - r.y0) as f64 * fy) as u32, x: r.x0 + ((r.x1 - r.x0) as f64 * fx) as u32 };
                for (fy, fx) in [(0.0, 0.0), (0.0, 1.0), (1.0, 0.0), (1.0, 1.0), (0.5, 0.5), (0.3, 0.9)] {
                    let p = at(fy, fx);
                    assert!(poly.contains_pt(p) && poly.contains_member(p), "{r:?}");
                    assert!(poly.matches_line(&[p, p], false), "{r:?}");
                }
                assert!(poly.matches_line(&[at(0.0, 0.0), at(1.0, 1.0)], false));
            }
        }
        assert!(inside > 100 && outside > 100, "{inside} inside, {outside} outside");
    }

    /// rect_pos: points and lines in a rectangle it puts apart from the
    /// polygon fail the node, member and way tests, those in one it puts
    /// inside pass them; also for a polygon across the antimeridian, whose
    /// rectangles on the other side are inside only shifted by 360 degrees.
    #[test]
    fn rect_apart_from_polygon() {
        use super::{Polygon, RectPos};
        use crate::geo::{Pt, Rect};
        let u = Polygon { pts: vec![(0.0, 0.0), (0.0, 3.0), (3.0, 3.0), (3.0, 2.0), (1.0, 2.0), (1.0, 1.0), (3.0, 1.0), (3.0, 0.0)] };
        let diamond = Polygon { pts: vec![(48.9, 2.35), (48.85, 2.42), (48.8, 2.35), (48.85, 2.28)] };
        let across = Polygon { pts: vec![(10.0, 179.0), (10.0, -179.0), (12.0, -179.0), (12.0, 179.0)] };
        let rect = |s: f64, w: f64, n: f64, e: f64| {
            let (a, b) = (Pt::from_ll(s, w), Pt::from_ll(n, e));
            Rect { y0: a.y, x0: a.x, y1: b.y, x1: b.x }
        };
        let pos = |p: &Polygon, r: Rect| p.prepare().rect_pos(&r);
        assert_eq!(pos(&u, rect(1.2, 1.2, 2.8, 1.8)), RectPos::Apart, "in the notch");
        assert_eq!(pos(&u, rect(1.0, 1.2, 2.8, 1.8)), RectPos::Border, "on the bottom of the notch");
        assert_eq!(pos(&u, rect(0.5, 0.5, 2.5, 2.5)), RectPos::Border);
        assert_eq!(pos(&u, rect(4.0, 4.0, 5.0, 5.0)), RectPos::Apart);
        assert_eq!(pos(&across, rect(10.5, 179.5, 11.0, 179.9)), RectPos::Inside);
        assert_eq!(pos(&across, rect(10.5, -179.9, 11.0, -179.5)), RectPos::Border, "inside shifted by 360");
        assert_eq!(pos(&across, rect(10.5, -170.0, 11.0, -160.0)), RectPos::Apart);
        assert_eq!(pos(&across, rect(10.5, 170.0, 11.0, 178.0)), RectPos::Apart);
        assert_eq!(pos(&u, Rect::EMPTY), RectPos::Border);
        let mut seed = 0x0fed_cba9_8765_4321u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut counts = [0usize; 3];
        for (poly, (s, w, size)) in [(&u, (-0.5, -0.5, 4.0)), (&diamond, (48.79, 2.27, 0.16)), (&across, (9.5, 178.0, 3.0)), (&across, (9.5, -181.0, 3.0))] {
            let test = poly.prepare();
            for _ in 0..3000 {
                let (la, lo) = (s + rnd() * size, w + rnd() * size);
                let (dla, dlo) = (rnd() * size * 0.2, rnd() * size * 0.2);
                let lo = if lo < -180.0 { lo + 360.0 } else { lo };
                let r = rect(la, lo, la + dla, (lo + dlo).min(180.0));
                let p = test.rect_pos(&r);
                counts[p as usize] += 1;
                let at = |fy: f64, fx: f64| Pt { y: r.y0 + ((r.y1 - r.y0) as f64 * fy) as u32, x: r.x0 + ((r.x1 - r.x0) as f64 * fx) as u32 };
                let pts = [(0.0, 0.0), (0.0, 1.0), (1.0, 0.0), (1.0, 1.0), (0.5, 0.5), (0.3, 0.9)].map(|(fy, fx)| at(fy, fx));
                match p {
                    RectPos::Apart => {
                        for q in pts {
                            assert!(!poly.contains_pt(q) && !poly.contains_member(q) && !poly.matches_line(&[q, q], true), "{r:?}");
                        }
                        for line in [[pts[0], pts[3]], [pts[1], pts[2]]] {
                            assert!(!poly.matches_line(&line, true) && !poly.matches_line(&line, false), "{r:?}");
                        }
                    }
                    RectPos::Inside => {
                        for q in pts {
                            assert!(poly.contains_pt(q) && poly.contains_member(q), "{r:?}");
                        }
                        assert!(poly.matches_line(&[pts[0], pts[3]], false), "{r:?}");
                    }
                    RectPos::Border => {}
                }
            }
        }
        assert!(counts.iter().all(|n| *n > 200), "{counts:?}");
    }

    // Every point within the radius lies in the box, both hemispheres.
    #[test]
    fn radius_box_covers_the_circle() {
        for lat in [-85.0, -60.0, -52.3, -30.0, -1.0, 0.0, 1.0, 30.0, 47.0, 52.3, 60.0, 85.0] {
            for r in [10.0, 1_000.0, 50_000.0, 120_000.0, 500_000.0] {
                for b in 0..72 {
                    for f in [0.5, 0.9, 0.99, 0.999] {
                        let p = destination(lat, 10.0, b as f64 * 5.0, r * f);
                        if great_circle_dist(lat, 10.0, p.0, p.1) <= r {
                            assert!(inside(lat, 10.0, r, p), "lat {lat} r {r} bearing {} -> {p:?}", b * 5);
                        }
                    }
                }
            }
        }
    }
}
