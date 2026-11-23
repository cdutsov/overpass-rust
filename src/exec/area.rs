//! Areas: `area` queries, the `(area)` filter, `is_in`, `map_to_area` and
//! `pivot` (docs/spec/areas.md).

use super::Engine;
use super::areageom::{self, Block, HIT, IC, INTERSECT, RelPos, TOGGLE_EAST, TOGGLE_WEST, Tilewise};
use super::eval::{Cx, El, Evaluator};
use super::query::Plan;
use super::set::{Set, sort_dedup};
use super::fetch::ranges_for;
use crate::geo::{Cell, CodeRange, MAX_LEVEL, Pt, Rect, cell_in_ranges, ilat, ilon, level_ranges, level_ranges_many, merge_ranges};
use crate::model::*;
use crate::store::Kind;
use anyhow::Result;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    /// The tag filter whose index entries gave the candidates of the last
    /// `area` query.
    pub(super) static AREA_SOURCE: std::cell::Cell<Option<super::filter::TagFilter>> = const { std::cell::Cell::new(None) };
    /// Makes the relation test read every member, wherever it is stored.
    pub(super) static MEMBERS_ALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Member ways the last relation test decoded.
    pub(super) static MEMBER_WAYS_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Polygons of one `(area)` clause.
pub struct AreaCtx {
    /// The DB areas of the clause.
    db: Arc<AreaDb>,
    /// Closed ways of the input set.
    implicit: Tilewise,
    /// Rectangles around the closed ways of the input set (`ring_rects`).
    implicit_rects: Vec<Rect>,
    /// Bounding rectangles for candidate selection.
    pub rects: Vec<Rect>,
}

/// The DB areas of an `(area)` clause, prepared once per query for a set
/// of areas (`Fetch::area_db`): the statements of a union over one area
/// share it.
pub struct AreaDb {
    /// Blocks per block tile: (area id, polylines), ascending id.
    tiles: BTreeMap<u32, Vec<(u32, Vec<Block>)>>,
    /// Rectangles that hold all the tests pass: by node tile
    /// (`fine_rects`), else the block tiles.
    rects: Vec<Rect>,
    /// The block tiles as rectangles.
    tile_rects: Vec<Rect>,
    /// Node tiles inside an area as a whole, by block tile (`fine_rects`).
    inside: FxHashMap<u32, [u16; 16]>,
}

fn ic(p: Pt) -> IC {
    (p.y, p.ilon())
}

fn is_closed(w: &Way) -> bool {
    !w.nodes.is_empty() && w.nodes.first() == w.nodes.last()
}

/// Rectangle around a closed way, including the parts of its great-circle
/// edges that bulge beyond the vertices (plus a small margin).
fn bulge_rect(g: &[IC]) -> Rect {
    let pts: Vec<Pt> = g.iter().map(|&(y, x)| Pt::from_ilat_ilon(y, x)).collect();
    let mut r = Rect::point(pts[0]);
    for w in pts.windows(2) {
        r.extend(w[1]);
        let (lo, hi) = crate::geo::Segment::new(w[0].lat(), w[0].lon(), w[1].lat(), w[1].lon()).lat_extent();
        r.extend(Pt::from_ll((lo - 1e-4).max(-90.0), w[0].lon()));
        r.extend(Pt::from_ll((hi + 1e-4).min(90.0), w[0].lon()));
    }
    r
}

/// Rectangles that hold the implicit area of a closed way: the
/// `bulge_rect`, with the longitudes followed along the ring. An edge more
/// than 180 degrees wide crosses the antimeridian (`make_entries`). A ring
/// that crosses it there and back lies at both ends of the key space, where
/// the box of its vertices does not reach, and gets a rectangle at each end.
/// A ring that crosses it once goes around a pole: its area is every tile
/// row from its southernmost one north, at all longitudes
/// (`Tilewise::build`), and it gets the whole width north of that row.
fn ring_rects(g: &[IC]) -> Vec<Rect> {
    const HALF: i64 = 1_800_000_000;
    let r = bulge_rect(g);
    // longitude along the ring, without the jumps at the antimeridian
    let mut u = g[0].1 as i64;
    let (mut lo, mut hi) = (u, u);
    for w in g.windows(2) {
        let d = w[1].1 as i64 - w[0].1 as i64;
        u += if d > HALF {
            d - 2 * HALF
        } else if d < -HALF {
            d + 2 * HALF
        } else {
            d
        };
        lo = lo.min(u);
        hi = hi.max(u);
    }
    let x = |ilon: i64| crate::geo::x_of_ilon(ilon as i32);
    if u != g[0].1 as i64 {
        vec![Rect { y0: r.y0 & !0xffff, x0: 0, y1: u32::MAX, x1: u32::MAX }]
    } else if hi - lo >= 2 * HALF {
        vec![Rect { x0: 0, x1: u32::MAX, ..r }]
    } else if hi > HALF {
        vec![Rect { x0: x(lo), x1: x(HALF), ..r }, Rect { x0: x(-HALF), x1: x(hi - 2 * HALF), ..r }]
    } else if lo < -HALF {
        vec![Rect { x0: x(lo + 2 * HALF), x1: x(HALF), ..r }, Rect { x0: x(-HALF), x1: x(hi), ..r }]
    } else {
        vec![r]
    }
}

/// `geo::arc_bulge` (the way test of implicit areas follows great-circle
/// edges), plus 10^-4 degrees for rounding, as in `bulge_rect`.
fn arc_bulge(width: u64) -> u32 {
    crate::geo::arc_bulge(width).saturating_add(1_000)
}

fn tile_rect(tile: u32) -> Rect {
    let (s, w) = areageom::tile_sw(tile);
    let p = Pt::from_ilat_ilon(s, w);
    Rect { y0: p.y, x0: p.x, y1: p.y.saturating_add(0xfffff), x1: p.x.saturating_add(0xfffff) }
}

/// Block tiles of the DB areas of a clause up to which its rectangles
/// follow the node tiles (`AreaCtx::fine_rects`), and the most rectangles
/// that may give (or two per block tile, if that is more): the candidates
/// are tested against each in turn.
const FINE_TILES: usize = 4096;
const FINE_RECTS: usize = 1024;

/// Units by which a block segment is grown when the node tiles it passes
/// are marked: the pieces of the way test are rounded to whole units.
const FINE_MARGIN: i64 = 4;

impl AreaDb {
    /// Rectangles of the DB areas that follow the node tiles: a block tile
    /// is 16 x 16 node tiles, and an area fills a part of the block tiles at
    /// its border (a city of 90 km2 touches block tiles of 600 km2). A
    /// node tile is kept when a block segment passes it, or when its centre
    /// is in an area: the point test changes only across a segment, so a
    /// tile without one lies inside or outside as a whole. What the tests
    /// pass has a point in a kept tile: a node inside; of a way, a vertex
    /// inside, the midpoint of a piece inside, or a crossing with a block
    /// segment, each within its bounding box. The kept tiles of a block
    /// tile become rectangles by row, equal runs of rows joined. None for
    /// many block tiles or rectangles, when little would be left out, and
    /// when a block does not lie in its tile.
    ///
    /// Also returns, per block tile, the node tiles (bit `column` of row
    /// `row`) that lie inside an area as a whole: no segment near, the
    /// centre inside (`AreaCtx::covers`).
    ///
    /// The point test changes only across a segment where the polylines
    /// of an area close within the tile or end on its north, west or east
    /// edge (or at 180 degrees). An end elsewhere that is not matched by
    /// another polyline (an open ring, where make-areas left out a member
    /// way; a piece across a tile border that occurs twice in a ring and
    /// that the block order keeps once) makes it change along the line
    /// north of that end, away from any segment: such an area keeps its
    /// block tiles (None, no inside tiles). `check` is called per block
    /// tile (deadline).
    fn fine_rects(&self, check: impl Fn() -> Result<()>) -> Result<(Option<Vec<Rect>>, FxHashMap<u32, [u16; 16]>)> {
        let mut inside: FxHashMap<u32, [u16; 16]> = FxHashMap::default();
        if self.tiles.is_empty() || self.tiles.len() > FINE_TILES {
            return Ok((None, inside));
        }
        let mut out: Vec<Rect> = Vec::new();
        let mut too_many = false;
        let mut kept = 0usize;
        for (&tile, areas) in &self.tiles {
            check()?;
            let (south, west) = areageom::tile_sw(tile);
            let (south, west) = (south as i64, west as i64);
            for (_, blocks) in areas {
                let mut ends: Vec<IC> = Vec::new();
                for b in blocks {
                    if let (Some(&f), Some(&l)) = (b.pts.first(), b.pts.last()) {
                        for e in [f, l] {
                            let (la, lo) = (e.0 as i64, e.1 as i64);
                            if la != south + 0xfffff && lo != west && lo != west + 0xfffff && e.1.unsigned_abs() != 1_800_000_000 {
                                ends.push(e);
                            }
                        }
                    }
                }
                ends.sort_unstable();
                if ends.chunk_by(|a, b| a == b).any(|g| g.len() % 2 == 1) {
                    return Ok((None, FxHashMap::default()));
                }
            }
            let mut rows = [0u16; 16];
            let mut mark = |lat0: i64, lat1: i64, lon0: i64, lon1: i64| -> bool {
                if lat0 < south || lat1 > south + 0xfffff || lon0 < west || lon1 > west + 0xfffff {
                    return false;
                }
                let sub = |v: i64, base: i64| ((v - base) >> 16).clamp(0, 15) as usize;
                let (r0, r1) = (sub(lat0 - FINE_MARGIN, south), sub(lat1 + FINE_MARGIN, south));
                let (c0, c1) = (sub(lon0 - FINE_MARGIN, west), sub(lon1 + FINE_MARGIN, west));
                let bits = (((1u32 << (c1 - c0 + 1)) - 1) << c0) as u16;
                for row in &mut rows[r0..=r1] {
                    *row |= bits;
                }
                true
            };
            for (_, blocks) in areas {
                for b in blocks {
                    if let [p] = b.pts.as_slice() {
                        if !mark(p.0 as i64, p.0 as i64, p.1 as i64, p.1 as i64) {
                            return Ok((None, FxHashMap::default()));
                        }
                    }
                    for w in b.pts.windows(2) {
                        let (p, q) = (w[0], w[1]);
                        if !mark(p.0.min(q.0) as i64, p.0.max(q.0) as i64, p.1.min(q.1) as i64, p.1.max(q.1) as i64) {
                            return Ok((None, FxHashMap::default()));
                        }
                    }
                }
            }
            let mut whole = [0u16; 16];
            for (r, row) in rows.iter_mut().enumerate() {
                for c in 0..16 {
                    if *row >> c & 1 == 0 {
                        let centre = ((south + ((r as i64) << 16) + 0x8000) as u32, (west + ((c as i64) << 16) + 0x8000) as i32);
                        if self.point(centre, true) {
                            *row |= 1 << c;
                            whole[r] |= 1 << c;
                        }
                    }
                }
            }
            if whole.iter().any(|w| *w != 0) {
                inside.insert(tile, whole);
            }
            kept += rows.iter().map(|r| r.count_ones() as usize).sum::<usize>();
            if too_many {
                continue;
            }
            let origin = Pt::from_ilat_ilon(south as u32, west as i32);
            // rectangles still open at the previous row, by their columns
            let mut open: Vec<(usize, usize, usize)> = Vec::new();
            for (r, row) in rows.iter().enumerate() {
                let mut next: Vec<(usize, usize, usize)> = Vec::new();
                let mut c = 0;
                while c < 16 {
                    if row >> c & 1 == 0 {
                        c += 1;
                        continue;
                    }
                    let c0 = c;
                    while c < 16 && row >> c & 1 == 1 {
                        c += 1;
                    }
                    let (c1, at) = (c - 1, out.len());
                    match open.iter().find(|o| o.0 == c0 && o.1 == c1) {
                        Some(o) => {
                            out[o.2].y1 = origin.y + ((r as u32) << 16) + 0xffff;
                            next.push(*o);
                        }
                        None => {
                            out.push(Rect { y0: origin.y + ((r as u32) << 16), x0: origin.x + ((c0 as u32) << 16), y1: origin.y + ((r as u32) << 16) + 0xffff, x1: origin.x + ((c1 as u32) << 16) + 0xffff });
                            next.push((c0, c1, at));
                        }
                    }
                }
                open = next;
            }
            too_many = out.len() > FINE_RECTS.max(2 * self.tiles.len());
        }
        // Little left out: the block tiles are fewer rectangles.
        if too_many || kept * 8 > self.tiles.len() * 256 * 7 {
            return Ok((None, inside));
        }
        Ok((Some(out), inside))
    }

    /// The areas with their rectangles and inside tiles; `rects`: the
    /// block tiles. Also tells whether the rectangles follow the node tiles.
    fn build(tiles: BTreeMap<u32, Vec<(u32, Vec<Block>)>>, rects: Vec<Rect>, check: impl Fn() -> Result<()>) -> Result<(AreaDb, bool)> {
        let mut db = AreaDb { tiles, tile_rects: rects.clone(), rects, inside: FxHashMap::default() };
        let (fine, inside) = db.fine_rects(check)?;
        db.inside = inside;
        let is_fine = fine.is_some();
        if let Some(fine) = fine {
            db.rects = fine;
        }
        Ok((db, is_fine))
    }

    /// Point test against the DB areas (`add_border`: the border counts).
    fn point(&self, p: IC, add_border: bool) -> bool {
        let Some(areas) = self.tiles.get(&areageom::block_tile(p.0, p.1)) else { return false };
        for (_, blocks) in areas {
            let mut inside = 0u8;
            for b in blocks {
                let c = b.check(p.0, p.1);
                if c == HIT && add_border {
                    inside = 1;
                    break;
                } else if c != 0 {
                    inside ^= c;
                }
            }
            if inside != 0 {
                return true;
            }
        }
        false
    }
}

impl AreaCtx {
    /// Heap bytes of the clause beyond its database areas (which the query
    /// keeps in `Fetch::area_db` in any case), roughly: the closed ways of
    /// the input set and the rectangles. What keeping the clause for the
    /// statements after it costs (`Engine::shared_area_ctx`).
    pub fn kept_bytes(&self) -> u64 {
        let rects = (self.rects.capacity() + self.implicit_rects.capacity()) * std::mem::size_of::<Rect>();
        self.implicit.approx_bytes() + rects as u64
    }

    /// Whether the box lies in node tiles that are inside a DB area as a
    /// whole (up to 64 of them): every point in it then passes the point
    /// test, so a way with all its nodes in the box passes the way test by
    /// its vertices, and no node position is read for it.
    fn covers(&self, b: &Rect) -> bool {
        if self.db.inside.is_empty() || b.is_empty() || b.may_cross() {
            return false;
        }
        let (ty0, ty1, tx0, tx1) = (b.y0 >> 16, b.y1 >> 16, b.x0 >> 16, b.x1 >> 16);
        if (ty1 - ty0 + 1) as u64 * (tx1 - tx0 + 1) as u64 > 64 {
            return false;
        }
        (ty0..=ty1).all(|ty| (tx0..=tx1).all(|tx| self.db.inside.get(&(crate::geo::morton(ty, tx) & 0xffff_ff00)).is_some_and(|rows| rows[(ty & 15) as usize] >> (tx & 15) & 1 == 1)))
    }

    /// Whether the clause holds closed ways (implicit areas), whose way test
    /// follows great-circle edges (`Rect::arc_box`).
    pub fn has_implicit(&self) -> bool {
        !self.implicit_rects.is_empty()
    }

    /// Cell ranges of the ways that may pass the way test (`db_way` or that
    /// of the implicit areas). A way is stored in the cell of the corner of
    /// its box at the level where the box is at most one cell wide and
    /// high, and the box lies within that cell and its neighbours to the
    /// north and east (`Cell::loose_rect`). Against DB areas the edges are
    /// straight lines between the nodes, inside the box; a way wider than
    /// 180 degrees may cross the antimeridian, and it is stored at level 0
    /// or 1, in a cell these ranges always hold. Against implicit areas the
    /// edges are great-circle arcs, which may pass north or south of the
    /// box: at each level the rectangles of the implicit areas are grown by
    /// the bulge of an arc one cell wide (`arc_bulge`).
    fn way_ranges(&self) -> Vec<CodeRange> {
        let mut out = ranges_for(Kind::Way, if self.implicit_rects.is_empty() { &self.db.rects } else { &self.db.tile_rects });
        if !self.implicit_rects.is_empty() {
            for level in 0..=MAX_LEVEL {
                let m = arc_bulge(1u64 << (32 - level as u32));
                let grown: Vec<Rect> = self.implicit_rects.iter().map(|r| Rect { y0: r.y0.saturating_sub(m), y1: r.y1.saturating_add(m), ..*r }).collect();
                out.extend(level_ranges_many(level, &grown, true));
            }
        }
        merge_ranges(out)
    }

    /// Point test against the DB areas (`add_border`: the border counts).
    fn db_point(&self, p: IC, add_border: bool) -> bool {
        self.db.point(p, add_border)
    }

    /// The vertex test of `db_way`: a vertex inside a DB area.
    fn db_vertex(&self, v: IC) -> bool {
        let Some(areas) = self.db.tiles.get(&areageom::block_tile(v.0, v.1)) else { return false };
        for (_, blocks) in areas {
            let mut st = 0u8;
            for b in blocks {
                let c = b.check(v.0, v.1);
                if c == HIT {
                    st = HIT;
                    break;
                }
                st ^= c;
            }
            if st & (TOGGLE_EAST | TOGGLE_WEST) != 0 {
                return true;
            }
        }
        false
    }

    /// Way test against the DB areas (C++ `collect_ways`, add_border = false).
    fn db_way(&self, geom: &[IC]) -> bool {
        if self.db.tiles.is_empty() || geom.is_empty() {
            return false;
        }
        let mut pieces = areageom::Blocks::new();
        areageom::add_way_to_blocks(geom, &mut pieces);
        let mut verts: BTreeMap<u32, Vec<IC>> = BTreeMap::new();
        for p in geom {
            verts.entry(areageom::block_tile(p.0, p.1)).or_default().push(*p);
        }
        // Only the way's own tiles can decide; the result is an OR, so the
        // order of the tiles does not matter.
        let way_tiles: std::collections::BTreeSet<u32> = verts.keys().chain(pieces.keys()).copied().collect();
        for tile in &way_tiles {
            let Some(areas) = self.db.tiles.get(tile) else { continue };
            if let Some(vs) = verts.get(tile) {
                for v in vs {
                    for (_, blocks) in areas {
                        let mut st = 0u8;
                        for b in blocks {
                            let c = b.check(v.0, v.1);
                            if c == HIT {
                                st = HIT;
                                break;
                            }
                            st ^= c;
                        }
                        if st & (TOGGLE_EAST | TOGGLE_WEST) != 0 {
                            return true;
                        }
                    }
                }
            }
            let Some(ps) = pieces.get(tile) else { continue };
            let mut scratch = Vec::new();
            for piece in ps {
                let local: Vec<(u32, u32)> = piece.iter().map(areageom::local).collect();
                let mut status: BTreeMap<u32, u8> = BTreeMap::new();
                for (aid, blocks) in areas {
                    for b in blocks {
                        let r = b.intersects_inner(&local, &mut scratch);
                        if r == INTERSECT {
                            return true;
                        } else if r == HIT {
                            status.insert(*aid, HIT);
                            break;
                        }
                        b.has_inner_points(&local, status.entry(*aid).or_insert(0));
                    }
                }
                if status.values().any(|s| *s != 0 && s & HIT == 0) {
                    return true;
                }
            }
        }
        false
    }
}

impl<'s, W: Write> Engine<'s, W> {
    fn complete_geom(&self, w: &Way) -> Result<Option<Vec<IC>>> {
        let pos = self.f.node_positions(&w.nodes)?;
        Ok(w.nodes.iter().map(|n| pos.get(n).map(|p| ic(*p))).collect())
    }

    /// Implicit areas of closed ways, with the rectangles that hold them.
    fn tilewise_of(&self, ways: &[&Way]) -> Result<(Tilewise, Vec<Rect>)> {
        let mut input = Vec::new();
        let mut rects = Vec::new();
        for (i, w) in ways.iter().enumerate() {
            if !is_closed(w) {
                continue;
            }
            // the vertices of each way are looked up on their own
            if i % 256 == 255 {
                self.check()?;
            }
            if let Some(g) = self.complete_geom(w)? {
                rects.extend(ring_rects(&g));
                input.push((w.idx, w.id, g));
            }
        }
        Ok((Tilewise::build(&input), rects))
    }

    /// Polygons of an `(area.set:id)` clause.
    pub fn area_ctx(&self, set: &str, id: Option<Id>) -> Result<AreaCtx> {
        let input = self.get(set);
        let mut areas: Vec<Area> = match id {
            Some(i) => self.f.by_ids::<Area>(&[i])?,
            None => input.areas.clone(),
        };
        areas.sort_by_key(|a| a.id);
        // The blocks and rectangles of the last set of areas are kept for
        // the query: a union of statements over one area builds them once.
        let key: Vec<Id> = areas.iter().map(|a| a.id).collect();
        let kept = self.f.area_db.borrow().as_ref().filter(|(k, _)| *k == key).map(|(_, db)| db.clone());
        let db = match kept {
            Some(db) => db,
            None => {
                // the areas kept before go first: never two sets at once
                *self.f.area_db.borrow_mut() = None;
                let mut tiles: BTreeMap<u32, Vec<(u32, Vec<Block>)>> = BTreeMap::new();
                let mut rects = Vec::new();
                for a in &areas {
                    for &t in &a.used {
                        if let Some(b) = self.f.snap.area_blocks(t, a.id as u32)? {
                            tiles.entry(t).or_default().push((a.id as u32, b.into_iter().map(Block::new).collect()));
                            rects.push(tile_rect(t));
                        }
                    }
                }
                let db = Arc::new(AreaDb::build(tiles, rects, || self.check())?.0);
                *self.f.area_db.borrow_mut() = Some((key, db.clone()));
                db
            }
        };
        let ways: Vec<&Way> = input.ways.iter().filter(|w| is_closed(w)).collect();
        let (implicit, implicit_rects) = self.tilewise_of(&ways)?;
        // With closed ways in the set the block tiles stay: the rectangles
        // of the closed ways do not hold all their test passes (ways over
        // degrees), and the nodes found beyond them are those in the block
        // tiles of the DB areas, as before.
        let db_rects = if implicit_rects.is_empty() { &db.rects } else { &db.tile_rects };
        let rects = db_rects.iter().chain(&implicit_rects).copied().collect();
        Ok(AreaCtx { db, implicit, implicit_rects, rects })
    }

    pub fn area_keep_nodes(&self, ctx: &AreaCtx, nodes: &[&Node]) -> Result<Vec<bool>> {
        Ok(nodes
            .iter()
            .map(|n| {
                let p = ic(n.pos);
                ctx.implicit.point(p.0, p.1, true) != RelPos::Outside || ctx.db_point(p, true)
            })
            .collect())
    }

    pub fn area_keep_ways(&self, ctx: &AreaCtx, ways: &[&Way]) -> Result<Vec<bool>> {
        // Node positions are fetched for a batch of ways at once, then the
        // (pure) geometric tests run in parallel.
        const BATCH: usize = 1 << 16;
        let mut out = Vec::with_capacity(ways.len());
        for all in ways.chunks(BATCH) {
            self.check()?;
            // the positions are dropped with the batch
            let _scope = self.f.limits.mem_scope();
            // A way with its first vertex inside a DB area passes (the
            // vertex test of `db_way`), whatever the others are: most ways
            // found for an area lie inside it, and one node position is
            // read for them instead of all. Ways with a missing node never
            // pass and go the whole way.
            // Before that: a way whose stored box lies in node tiles
            // inside an area as a whole passes without any position.
            let mut first_in: Vec<bool> = all.iter().map(|w| !w.missing && !w.nodes.is_empty() && ctx.covers(&w.bbox)).collect();
            if !ctx.db.tiles.is_empty() {
                let firsts: Vec<Id> = all.iter().zip(&first_in).filter(|(w, f)| !**f && !w.missing).filter_map(|(w, _)| w.nodes.first().copied()).collect();
                let pos = self.f.node_positions(&firsts)?;
                for (w, f) in all.iter().zip(first_in.iter_mut()).filter(|(_, f)| !**f) {
                    if let Some(p) = w.nodes.first().filter(|_| !w.missing).and_then(|n| pos.get(n)) {
                        *f = ctx.db_vertex(ic(*p));
                    }
                }
            }
            let part: Vec<&Way> = all.iter().zip(&first_in).filter(|(_, f)| !**f).map(|(w, _)| *w).collect();
            let ids: Vec<Id> = part.iter().flat_map(|w| w.nodes.iter().copied()).collect();
            let pos = self.f.node_positions(&ids)?;
            let keep = |w: &&Way| -> bool {
                let g: Option<Vec<IC>> = w.nodes.iter().map(|n| pos.get(n).map(|p| ic(*p))).collect();
                match g {
                    Some(g) => (!ctx.implicit.is_empty() && ctx.implicit.way(&g)) || ctx.db_way(&g),
                    None => false,
                }
            };
            let mut rest = if part.len() >= 64 && super::parallel_ok() {
                use rayon::prelude::*;
                part.par_iter().map(keep).collect::<Vec<bool>>()
            } else {
                part.iter().map(keep).collect::<Vec<bool>>()
            }
            .into_iter();
            out.extend(first_in.iter().map(|f| *f || rest.next().unwrap_or(false)));
        }
        Ok(out)
    }

    /// Relation test: a member node or a member way passes (the member
    /// variants of the node and way tests). Only the members stored in
    /// cells that may reach an area are read (the cells come from the id
    /// index): a route or boundary across a city has most of its members
    /// elsewhere. The ways of the relations a member node decides are not
    /// read.
    pub fn area_keep_rels(&self, ctx: &AreaCtx, all_rels: &[&Relation]) -> Result<Vec<bool>> {
        let members = |t: ElemType, rels: &[&Relation]| -> Vec<Id> {
            let mut v: Vec<Id> = rels.iter().flat_map(|r| r.members.iter()).filter(|m| m.mtype == t).map(|m| m.id).collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        #[cfg(not(test))]
        let keep = |c: Cell, ranges: &[CodeRange]| cell_in_ranges(c, ranges);
        #[cfg(test)]
        let keep = |c: Cell, ranges: &[CodeRange]| MEMBERS_ALL.get() || cell_in_ranges(c, ranges);
        let near = |kind: Kind, ids: &[Id], ranges: &[CodeRange]| -> Result<Vec<(Cell, Id)>> {
            Ok(self.f.snap.cells_of(kind, ids)?.into_iter().filter(|(_, c)| keep(*c, ranges)).map(|(id, c)| (c, id)).collect())
        };
        // A relation with a member that has a place (`spatial`: a node, or a
        // way with all its nodes) whose stored box lies in node tiles inside
        // an area as a whole passes by that member, which is not read.
        let whole: Vec<bool> = all_rels.iter().map(|r| r.spatial && ctx.covers(&r.bbox)).collect();
        let rels: Vec<&Relation> = all_rels.iter().zip(&whole).filter(|(_, w)| !**w).map(|(r, _)| *r).collect();
        let rels = &rels[..];
        // A node passes only inside the rectangles of the areas.
        let nids: Vec<Id> = near(Kind::Node, &members(ElemType::Node, rels), &ranges_for(Kind::Node, &ctx.rects))?.into_iter().map(|(_, id)| id).collect();
        let nok: FxHashSet<Id> = self
            .f
            .node_positions(&nids)?
            .into_iter()
            .filter(|(_, p)| {
                let q = ic(*p);
                ctx.implicit.point(q.0, q.1, false) == RelPos::Inside || ctx.db_point(q, false)
            })
            .map(|(id, _)| id)
            .collect();
        let by_node = |r: &Relation| r.members.iter().any(|m| m.mtype == ElemType::Node && nok.contains(&m.id));
        let mut ok: Vec<bool> = rels.iter().map(|r| by_node(r)).collect();
        let way_ranges = ctx.way_ranges();
        // The member ways in rounds: the first 8 of each relation, the next
        // 32, the next 128, then the rest, each round for the relations no
        // earlier member has decided. One way that passes decides a
        // relation, and a relation inside the area passes by its first
        // ways: the hundreds of ways of a route in a city are not read.
        fn way_members(r: &Relation) -> impl Iterator<Item = &Member> {
            r.members.iter().filter(|m| m.mtype == ElemType::Way)
        }
        let (mut from, mut size) = (0usize, 8usize);
        loop {
            self.check()?;
            let upto = from.saturating_add(size);
            let mut more = false;
            let mut ids: Vec<Id> = Vec::new();
            for (r, _) in rels.iter().zip(&ok).filter(|(_, done)| !**done) {
                let mut left = way_members(r).skip(from);
                ids.extend(left.by_ref().take(size).map(|m| m.id));
                more |= left.next().is_some();
            }
            ids.sort_unstable();
            ids.dedup();
            if !ids.is_empty() {
                let ways = self.f.ways_untagged_at(near(Kind::Way, &ids, &way_ranges)?)?;
                #[cfg(test)]
                MEMBER_WAYS_READ.set(MEMBER_WAYS_READ.get() + ways.len());
                let wrefs: Vec<&Way> = ways.iter().collect();
                let wok: FxHashSet<Id> = ways.iter().zip(self.area_keep_ways(ctx, &wrefs)?).filter(|(_, ok)| *ok).map(|(w, _)| w.id).collect();
                if !wok.is_empty() {
                    for (r, done) in rels.iter().zip(ok.iter_mut()).filter(|(_, done)| !**done) {
                        *done = way_members(r).skip(from).take(size).any(|m| wok.contains(&m.id));
                    }
                }
            }
            if !more {
                break;
            }
            (from, size) = (upto, if upto >= 168 { usize::MAX } else { size * 4 });
        }
        let mut ok = ok.into_iter();
        Ok(whole.iter().map(|w| *w || ok.next().unwrap_or(false)).collect())
    }

    /// DB areas of an `area` query (spatial clauses never filter areas).
    pub fn query_areas(&mut self, plan: &Plan) -> Result<Vec<Area>> {
        // `(pivot)` and the recursion clauses give elements, never areas:
        // without this, `area(pivot.a)` read every area of the database.
        if !plan.newer.is_empty() || !plan.users.is_empty() || !plan.uids.is_empty() || !plan.pivots.is_empty() || !plan.recurse_ops.is_empty() {
            return Ok(Vec::new());
        }
        let mut cands: Vec<Area> = if let Some(ids) = &plan.ids {
            let ids: Vec<Id> = ids.iter().copied().filter(|i| *i <= u32::MAX as u64).collect();
            self.f.by_ids::<Area>(&ids)?
        } else if let Some(first) = plan.inputs.first() {
            self.get(first).areas.clone()
        } else if let Some((pairs, read)) = self.indexed_candidates(Kind::Area, plan, None)? {
            #[cfg(test)]
            AREA_SOURCE.set(Some(read[0].clone()));
            self.at_tagged::<Area>(plan, pairs, &read)?
        } else if let Some(t) = plan.tags.iter().find(|t| super::query::anchored(t)).or_else(|| plan.tags.iter().find(|t| t.is_positive())) {
            #[cfg(test)]
            AREA_SOURCE.set(Some(t.clone()));
            let pairs = self.f.tag_candidates(Kind::Area, t, None)?;
            self.at_tagged::<Area>(plan, pairs, &[t])?
        } else {
            self.f.all::<Area>(|_| true)?
        };
        for name in &plan.inputs {
            let s = self.get(name);
            let ids: FxHashSet<Id> = s.areas.iter().map(|a| a.id).collect();
            cands.retain(|a| ids.contains(&a.id));
        }
        cands.retain(|a| plan.tags.iter().all(|t| t.matches(&a.tags)));
        if !plan.ifs.is_empty() {
            let keep: Vec<bool> = {
                let ev = Evaluator::new(self);
                let mut keep = Vec::with_capacity(cands.len());
                for a in &cands {
                    let mut ok = true;
                    for e in &plan.ifs {
                        if !super::eval::num::truthy(&ev.s(e, Cx { el: Some(El::A(a)), pos: None, key: None })?) {
                            ok = false;
                            break;
                        }
                    }
                    keep.push(ok);
                }
                keep
            };
            let mut i = 0;
            cands.retain(|_| {
                i += 1;
                keep[i - 1]
            });
        }
        sort_dedup(&mut cands);
        Ok(cands)
    }

    pub fn is_in(&mut self, input: &str, coord: Option<(f64, f64)>) -> Result<Set> {
        let points: Vec<IC> = match coord {
            Some((la, lo)) => vec![(ilat(la), ilon(lo))],
            None => self.get(input).nodes.iter().map(|n| ic(n.pos)).collect(),
        };
        let mut out = Set::default();
        if points.is_empty() {
            return Ok(out);
        }
        // of the closed ways read below and their vertices, only the ways
        // around a point stay
        let _scope = self.f.limits.mem_scope();
        // DB areas: any block hit or odd parity in the point's block tile.
        let mut found: FxHashSet<u32> = FxHashSet::default();
        let mut by_tile: BTreeMap<u32, Vec<IC>> = BTreeMap::new();
        for p in &points {
            by_tile.entry(areageom::block_tile(p.0, p.1)).or_default().push(*p);
        }
        for (tile, pts) in &by_tile {
            self.check()?;
            let blocks = self.f.snap.area_blocks_in_tile(*tile)?;
            for (i, p) in pts.iter().enumerate() {
                // a tile of a city holds the blocks of hundreds of areas
                if i % 256 == 255 {
                    self.check()?;
                }
                for (aid, bs) in &blocks {
                    let mut st = 0u8;
                    let mut hit = false;
                    for b in bs {
                        let c = areageom::check_area_block(b, p.0, p.1);
                        if c == HIT {
                            hit = true;
                            break;
                        }
                        st ^= c;
                    }
                    if hit || st != 0 {
                        found.insert(*aid);
                    }
                }
            }
        }
        let ids: Vec<Id> = found.into_iter().map(|i| i as Id).collect();
        out.areas = self.f.by_ids::<Area>(&ids)?;
        // Closed ways covering a point (border included).
        let mut ranges = Vec::new();
        for p in &points {
            let r = Rect::point(Pt::from_ilat_ilon(p.0, p.1));
            for level in 0..=MAX_LEVEL {
                ranges.extend(level_ranges(level, &r, true));
            }
        }
        ranges.sort_by_key(|r| (r.level, r.lo));
        ranges.dedup();
        // No bbox prefilter: stored boxes of antimeridian-crossing ways do
        // not contain points next to the antimeridian.
        self.check()?;
        let cands = self.f.in_ranges::<Way>(&ranges, is_closed)?;
        self.check()?;
        let refs: Vec<&Way> = cands.iter().collect();
        let (tw, _) = self.tilewise_of(&refs)?;
        let mut hit: FxHashSet<Id> = FxHashSet::default();
        for (i, p) in points.iter().enumerate() {
            if i % 1024 == 1023 {
                self.check()?;
            }
            hit.extend(tw.covering(p.0, p.1));
        }
        out.ways = cands.into_iter().filter(|w| hit.contains(&w.id)).collect();
        Ok(out)
    }

    pub fn map_to_area(&mut self, s: &Set) -> Result<Set> {
        let mut ids: Vec<Id> = s.ways.iter().map(|w| (w.id as u32).wrapping_add(2_400_000_000) as Id).collect();
        ids.extend(s.rels.iter().map(|r| (r.id as u32).wrapping_add(3_600_000_000) as Id));
        let mut out = Set { areas: self.f.by_ids::<Area>(&ids)?, ..Default::default() };
        out.ways = s.ways.iter().filter(|w| is_closed(w)).cloned().collect();
        Ok(out)
    }

    /// Elements the areas of a set were built from.
    pub fn pivot(&mut self, s: &Set) -> Result<Set> {
        let mut nids = Vec::new();
        let mut wids = Vec::new();
        let mut rids = Vec::new();
        for a in &s.areas {
            if a.id < 2_400_000_000 {
                nids.push(a.id);
            } else if a.id > 2_400_000_000 && a.id < 3_600_000_000 {
                wids.push(a.id - 2_400_000_000);
            } else if a.id > 3_600_000_000 {
                rids.push(a.id - 3_600_000_000);
            }
        }
        Ok(Set { nodes: self.f.by_ids::<Node>(&nids)?, ways: self.f.by_ids::<Way>(&wids)?, rels: self.f.by_ids::<Relation>(&rids)?, ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// The rectangles `AreaDb::build` chose for the last `ctx_of`.
        static FINE: std::cell::RefCell<Option<Vec<Rect>>> = const { std::cell::RefCell::new(None) };
    }

    fn ctx_of(rings: &[Vec<IC>]) -> AreaCtx {
        let blocks = areageom::build_area(rings).unwrap();
        let mut tiles: BTreeMap<u32, Vec<(u32, Vec<Block>)>> = BTreeMap::new();
        let mut rects = Vec::new();
        for (t, bs) in blocks {
            tiles.insert(t, vec![(1, bs.into_iter().map(Block::new).collect())]);
            rects.push(tile_rect(t));
        }
        // `rects`: the block tiles; the test asks for the fine ones itself
        let (mut db, _) = AreaDb::build(tiles, rects.clone(), || Ok(())).unwrap();
        FINE.with(|f| *f.borrow_mut() = Some(std::mem::replace(&mut db.rects, rects.clone())));
        AreaCtx { db: Arc::new(db), implicit: Tilewise::default(), implicit_rects: Vec::new(), rects }
    }

    /// The rectangles by node tile hold every point the point test passes
    /// and meet the box of every way the way test passes, for a star (with
    /// a hole) over some twenty block tiles; they cover far less than the
    /// block tiles.
    #[test]
    fn fine_rects_hold_what_the_tests_pass() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let (clat, clon) = (47.03, 8.02);
        let ring = |r0: f64, r1: f64, n: usize| -> Vec<IC> {
            let mut v: Vec<IC> = (0..n)
                .map(|i| {
                    let a = i as f64 / n as f64 * std::f64::consts::TAU;
                    let r = if i % 2 == 0 { r0 } else { r1 };
                    (ilat(clat + r * a.sin()), ilon(clon + 1.4 * r * a.cos()))
                })
                .collect();
            v.push(v[0]);
            v
        };
        for rings in [vec![ring(0.22, 0.09, 26)], vec![ring(0.2, 0.17, 40), ring(0.05, 0.04, 12)], vec![ring(0.012, 0.01, 8)]] {
            let ctx = ctx_of(&rings);
            let coarse = ctx.rects.clone();
            let fine = FINE.with(|f| f.borrow_mut().take()).filter(|f| *f != coarse).expect("fine rectangles");
            let inside = |rs: &[Rect], p: Pt| rs.iter().any(|r| r.y0 <= p.y && p.y <= r.y1 && r.x0 <= p.x && p.x <= r.x1);
            let grow = |r: &Rect| Rect { y0: r.y0 - 1, x0: r.x0 - 1, y1: r.y1 + 1, x1: r.x1 + 1 };
            let span = (2.6 * rings[0].iter().map(|p| (crate::geo::lat_of(p.0) - clat).abs()).fold(0.0, f64::max)).max(0.12);
            let (mut passed, mut in_fine, mut in_coarse, mut ways_passed) = (0, 0, 0, 0);
            for _ in 0..60_000 {
                let p = (ilat(clat + (rnd() - 0.5) * span), ilon(clon + (rnd() - 0.5) * 1.4 * span));
                let pt = Pt::from_ilat_ilon(p.0, p.1);
                in_fine += inside(&fine, pt) as usize;
                in_coarse += inside(&coarse, pt) as usize;
                for border in [true, false] {
                    if ctx.db_point(p, border) {
                        passed += 1;
                        assert!(inside(&fine, pt), "point {p:?} passes outside the rectangles");
                    }
                }
                // a way of two to four vertices from this point, short or long
                let len = if rnd() < 0.7 { 0.004 } else { 0.15 };
                let mut g = vec![p];
                for _ in 0..1 + (rnd() * 3.0) as usize {
                    let last = *g.last().unwrap();
                    g.push((ilat(crate::geo::lat_of(last.0) + (rnd() - 0.5) * len), last.1 + ((rnd() - 0.5) * len * 1e7) as i32));
                }
                if ctx.db_way(&g) {
                    ways_passed += 1;
                    let mut b = Rect::point(Pt::from_ilat_ilon(g[0].0, g[0].1));
                    for q in &g {
                        b.extend(Pt::from_ilat_ilon(q.0, q.1));
                    }
                    assert!(fine.iter().any(|r| grow(r).intersects(&b)), "way {g:?} passes apart from the rectangles");
                }
            }
            // boxes in node tiles inside as a whole: their corners and ways across them pass
            let mut covered = 0;
            for _ in 0..40_000 {
                let p = (ilat(clat + (rnd() - 0.5) * span), ilon(clon + (rnd() - 0.5) * 1.4 * span));
                let q = (p.0 + (rnd() * 150_000.0) as u32, p.1 + (rnd() * 150_000.0) as i32);
                let mut b = Rect::point(Pt::from_ilat_ilon(p.0, p.1));
                b.extend(Pt::from_ilat_ilon(q.0, q.1));
                if ctx.covers(&b) {
                    covered += 1;
                    for c in [p, q, (p.0, q.1), (q.0, p.1)] {
                        assert!(ctx.db_point(c, false) && ctx.db_vertex(c), "corner {c:?} of a covered box is outside");
                    }
                    assert!(ctx.db_way(&[p, q]) && ctx.db_way(&[(p.0, q.1), (q.0, p.1), p]), "way in a covered box fails");
                }
            }
            assert!(rings[0].len() < 12 || covered > 300, "{covered} boxes covered");
            assert!(passed > 1_000 && ways_passed > 500, "{passed} points, {ways_passed} ways passed");
            assert!(in_fine * 10 < in_coarse * 9, "{in_fine} of {in_coarse} points in the rectangles");
        }
    }
}

/// Review tests (review-sd): the invariants patches 07, 08, 10 and 15 rest on,
/// on random and crafted areas. Run with `-- --ignored review_`.
#[cfg(test)]
mod review_tests {
    use super::*;
    use crate::geo::{anchor_cell, tile_of, x_of_ilon};

    struct Rng(u64);
    impl Rng {
        fn u(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn f(&mut self) -> f64 {
            (self.u() >> 11) as f64 / (1u64 << 53) as f64
        }
        fn r(&mut self, a: f64, b: f64) -> f64 {
            a + (b - a) * self.f()
        }
        fn n(&mut self, k: u64) -> u64 {
            self.u() % k.max(1)
        }
        fn chance(&mut self, p: f64) -> bool {
            self.f() < p
        }
    }

    fn wrap(lon: f64) -> f64 {
        let mut l = lon;
        while l > 180.0 {
            l -= 360.0;
        }
        while l < -180.0 {
            l += 360.0;
        }
        l
    }

    fn icd(lat: f64, lon: f64) -> IC {
        (ilat(lat.clamp(-90.0, 90.0)), ilon(wrap(lon)))
    }

    const XMIN: u32 = 0x8000_0000 ^ (-1_800_000_000i32 as u32);
    const XMAX: u32 = 0x8000_0000 ^ (1_800_000_000u32);

    /// A coordinate moved onto a node tile or block tile border (or next to one).
    fn snap(rng: &mut Rng, c: IC) -> IC {
        let (mut y, mut x) = (c.0, x_of_ilon(c.1));
        let mask = if rng.chance(0.5) { 0xffffu32 } else { 0xfffffu32 };
        let d = [0i64, 0, 1, -1, 2][rng.n(5) as usize];
        if rng.chance(0.6) {
            y = (((y & !mask) as i64) + d).clamp(10_000_000, 1_810_000_000) as u32;
        }
        if rng.chance(0.6) {
            x = (((x & !mask) as i64) + d).clamp(XMIN as i64, XMAX as i64) as u32;
        }
        (y, (x ^ 0x8000_0000) as i32)
    }

    fn star(rng: &mut Rng, clat: f64, clon: f64, r: f64, nmin: usize, nspread: u64, snapping: f64) -> Vec<IC> {
        let n = nmin + rng.n(nspread) as usize;
        let k = 1.0 / clat.to_radians().cos().max(0.2);
        let mut v: Vec<IC> = (0..n)
            .map(|i| {
                let a = i as f64 / n as f64 * std::f64::consts::TAU;
                let rr = r * rng.r(0.3, 1.0);
                let c = icd(clat + rr * a.sin(), clon + k * rr * a.cos());
                if rng.chance(snapping) { snap(rng, c) } else { c }
            })
            .collect();
        v.dedup();
        v.push(v[0]);
        v
    }

    fn rect_ring(lat0: f64, lon0: f64, lat1: f64, lon1: f64) -> Vec<IC> {
        vec![icd(lat0, lon0), icd(lat0, lon1), icd(lat1, lon1), icd(lat1, lon0), icd(lat0, lon0)]
    }

    struct Shape {
        areas: Vec<Vec<Vec<IC>>>,
        clat: f64,
        clon: f64,
        spread: f64,
        kind: usize,
    }

    fn shape(rng: &mut Rng, kind: usize) -> Shape {
        let (clat, clon) = (rng.r(-60.0, 70.0), rng.r(-179.0, 179.0));
        let sn = if rng.chance(0.5) { 0.0 } else { rng.r(0.05, 0.5) };
        match kind {
            0 => {
                let r = rng.r(0.01, 0.3);
                Shape { areas: vec![vec![star(rng, clat, clon, r, 3, 60, sn)]], clat, clon, spread: r * 1.6, kind }
            }
            1 => {
                let r = rng.r(0.0002, 0.004);
                Shape { areas: vec![vec![star(rng, clat, clon, r, 3, 12, sn)]], clat, clon, spread: r * 3.0, kind }
            }
            2 => {
                let r = rng.r(0.5, 2.2);
                let clat = rng.r(-50.0, 50.0);
                Shape { areas: vec![vec![star(rng, clat, clon, r, 8, 200, sn)]], clat, clon, spread: r * 1.4, kind }
            }
            3 => {
                let clon = if rng.chance(0.5) { rng.r(179.8, 180.0) } else { rng.r(-180.0, -179.8) };
                let r = rng.r(0.02, 0.6);
                Shape { areas: vec![vec![star(rng, clat, clon, r, 3, 40, sn)]], clat, clon, spread: r * 1.6, kind }
            }
            4 | 5 => {
                let clat = if kind == 4 { rng.r(89.4, 90.0) } else { rng.r(-90.0, -89.4) };
                let r = rng.r(0.05, 0.6);
                Shape { areas: vec![vec![star(rng, clat, clon, r, 3, 40, sn)]], clat, clon, spread: r * 1.6, kind }
            }
            6 => {
                let r = rng.r(0.05, 0.4);
                let mut rings = vec![star(rng, clat, clon, r, 10, 50, sn)];
                for _ in 0..1 + rng.n(3) {
                    let (dl, dn) = (rng.r(-0.15, 0.15) * r, rng.r(-0.15, 0.15) * r);
                    let rr = r * rng.r(0.05, 0.25);
                    rings.push(star(rng, clat + dl, clon + dn, rr, 3, 20, sn));
                }
                Shape { areas: vec![rings], clat, clon, spread: r * 1.6, kind }
            }
            7 => {
                // axis-parallel rings with corners on tile borders, one with a hole
                let s = |rng: &mut Rng, c: IC| snap(rng, c);
                let w = rng.r(0.01, 0.4);
                let h = rng.r(0.01, 0.4);
                let mut outer = rect_ring(clat, clon, clat + h, clon + w);
                for c in outer.iter_mut() {
                    *c = s(rng, *c);
                }
                let n = outer.len();
                outer[n - 1] = outer[0];
                let inner = rect_ring(clat + h * 0.3, clon + w * 0.3, clat + h * 0.6, clon + w * 0.6);
                let rings = if rng.chance(0.5) { vec![outer, inner] } else { vec![outer] };
                Shape { areas: vec![rings], clat: clat + h / 2.0, clon: clon + w / 2.0, spread: (w + h), kind }
            }
            8 => {
                let r = rng.r(0.02, 0.3);
                let mut areas = Vec::new();
                for _ in 0..2 + rng.n(2) {
                    let (dl, dn) = (rng.r(-1.0, 1.0) * r, rng.r(-1.0, 1.0) * r);
                    let rr = r * rng.r(0.3, 1.0);
                    areas.push(vec![star(rng, clat + dl, clon + dn, rr, 3, 30, sn)]);
                }
                Shape { areas, clat, clon, spread: r * 2.5, kind }
            }
            9 => {
                // across the antimeridian, high north
                let clat = rng.r(60.0, 75.0);
                let clon = if rng.chance(0.5) { 179.95 } else { -179.95 };
                let r = rng.r(0.05, 0.5);
                Shape { areas: vec![vec![star(rng, clat, clon, r, 5, 60, sn)]], clat, clon, spread: r * 3.0, kind }
            }
            _ => {
                // split at the antimeridian as in the data: two rings ending at exactly 180 and -180
                let (h, w) = (rng.r(0.02, 0.3), rng.r(0.02, 0.3));
                let mut east = rect_ring(clat, 180.0 - w, clat + h, 180.0);
                let mut west = rect_ring(clat, -180.0, clat + h * rng.r(0.5, 1.0), -180.0 + w);
                for c in east.iter_mut().chain(west.iter_mut()) {
                    if rng.chance(sn) && c.1.abs() != 1_800_000_000 {
                        *c = (snap(rng, *c).0, c.1);
                    }
                }
                let (ne, nw) = (east.len(), west.len());
                east[ne - 1] = east[0];
                west[nw - 1] = west[0];
                let areas = if rng.chance(0.5) { vec![vec![east, west]] } else { vec![vec![east], vec![west]] };
                Shape { areas, clat: clat + h / 2.0, clon: 180.0, spread: h + w, kind }
            }
        }
    }

    /// An area context as `area_ctx` builds it, for areas built from these
    /// rings (None when an area is too large).
    fn ctx_for(areas: &[Vec<Vec<IC>>]) -> Option<(AreaCtx, bool)> {
        let mut tiles: BTreeMap<u32, Vec<(u32, Vec<Block>)>> = BTreeMap::new();
        let mut rects = Vec::new();
        for (k, rings) in areas.iter().enumerate() {
            let blocks = areageom::build_area(rings)?;
            for (t, bs) in blocks {
                tiles.entry(t).or_default().push((k as u32 + 1, bs.into_iter().map(Block::new).collect()));
                rects.push(tile_rect(t));
            }
        }
        let (db, is_fine) = AreaDb::build(tiles, rects, || Ok(())).unwrap();
        let rects = db.rects.clone();
        let ctx = AreaCtx { db: Arc::new(db), implicit: Tilewise::default(), implicit_rects: Vec::new(), rects };
        Some((ctx, is_fine))
    }

    fn pt(c: IC) -> Pt {
        Pt::from_ilat_ilon(c.0, c.1)
    }

    fn bbox(g: &[IC]) -> Rect {
        let mut b = Rect::EMPTY;
        for c in g {
            b.extend(pt(*c));
        }
        b
    }

    fn grow(r: &Rect) -> Rect {
        Rect { y0: r.y0.saturating_sub(1), x0: r.x0.saturating_sub(1), y1: r.y1.saturating_add(1), x1: r.x1.saturating_add(1) }
    }

    /// A point of the shape: anywhere around it, next to a segment, on a
    /// tile border, at the antimeridian or a pole.
    fn sample_point(rng: &mut Rng, s: &Shape) -> IC {
        let k = 1.0 / s.clat.to_radians().cos().max(0.2);
        let mut c = icd(s.clat + rng.r(-1.0, 1.0) * s.spread, s.clon + k * rng.r(-1.0, 1.0) * s.spread);
        match rng.n(8) {
            0 | 1 => {
                // next to a segment of a ring
                let rings: Vec<&Vec<IC>> = s.areas.iter().flatten().collect();
                let ring = rings[rng.n(rings.len() as u64) as usize];
                let i = rng.n(ring.len() as u64 - 1) as usize;
                let (a, b) = (ring[i], ring[i + 1]);
                if (a.1 as i64 - b.1 as i64).abs() < 900_000_000 {
                    let t = rng.f();
                    let d = [0i64, 1, -1, 2, -2, 4, -4, 7, 40][rng.n(9) as usize];
                    let e = [0i64, 1, -1, 3, -5][rng.n(5) as usize];
                    let y = (a.0 as f64 + t * (b.0 as f64 - a.0 as f64)) as i64 + d;
                    let x = (a.1 as f64 + t * (b.1 as f64 - a.1 as f64)) as i64 + e;
                    c = (y.clamp(10_000_000, 1_810_000_000) as u32, x.clamp(-1_800_000_000, 1_800_000_000) as i32);
                }
            }
            2 => c = snap(rng, c),
            3 if s.kind == 3 || s.kind == 9 || s.kind == 10 => c.1 = if rng.chance(0.5) { 1_800_000_000 } else { -1_800_000_000 },
            3 if s.kind == 4 => c.0 = 1_810_000_000,
            3 if s.kind == 5 => c.0 = 10_000_000,
            _ => {}
        }
        c
    }

    fn sample_way(rng: &mut Rng, s: &Shape) -> Vec<IC> {
        let mut g = vec![sample_point(rng, s)];
        let len = [0.0003, 0.003, 0.03, 0.3][rng.n(4) as usize] * s.spread.max(0.01) / 0.3;
        for _ in 0..1 + rng.n(4) {
            let last = *g.last().unwrap();
            let c = if rng.chance(0.3) {
                sample_point(rng, s)
            } else {
                icd(lat_of_ic(last) + rng.r(-1.0, 1.0) * len, lon_of_ic(last) + rng.r(-1.0, 1.0) * len)
            };
            g.push(if rng.chance(0.1) { snap(rng, c) } else { c });
        }
        if rng.chance(0.2) {
            g.push(g[0]);
        }
        g
    }

    fn lat_of_ic(c: IC) -> f64 {
        crate::geo::lat_of(c.0)
    }
    fn lon_of_ic(c: IC) -> f64 {
        crate::geo::lon_of(c.1)
    }

    #[test]
    #[ignore]
    fn review_area_invariants() {
        let secs: u64 = std::env::var("REVIEW_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
        let seed: u64 = std::env::var("REVIEW_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x1234_5678_9abc_def1);
        let t0 = std::time::Instant::now();
        let mut rng = Rng(seed);
        let (mut shapes, mut fine_n, mut pts_in, mut ways_in, mut covered, mut first_in) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
        let mut by_kind = [0usize; 11];
        let mut fails: Vec<String> = Vec::new();
        while t0.elapsed().as_secs() < secs && fails.len() < 20 {
            let kind = shapes % 11;
            shapes += 1;
            let s = shape(&mut rng, kind);
            let Some((ctx, is_fine)) = ctx_for(&s.areas) else { continue };
            fine_n += is_fine as usize;
            by_kind[kind] += is_fine as usize;
            let node_ranges = ranges_for(Kind::Node, &ctx.rects);
            let way_ranges = ranges_for(Kind::Way, &ctx.rects);
            let member_ranges = ctx.way_ranges();
            let inside = |p: Pt| ctx.rects.iter().any(|r| r.y0 <= p.y && p.y <= r.y1 && r.x0 <= p.x && p.x <= r.x1);
            for _ in 0..3000 {
                let p = sample_point(&mut rng, &s);
                if ctx.db_point(p, true) || ctx.db_point(p, false) {
                    pts_in += 1;
                    if !inside(pt(p)) || !cell_in_ranges(Cell::tile(tile_of(pt(p))), &node_ranges) {
                        fails.push(format!("kind {kind} shape {shapes}: point {p:?} passes outside the rectangles; areas {:?}", s.areas));
                    }
                }
            }
            for _ in 0..2000 {
                let g = sample_way(&mut rng, &s);
                let sb = bbox(&g);
                let passes = ctx.db_way(&g);
                if passes {
                    ways_in += 1;
                    let cell = anchor_cell(&sb);
                    if (!sb.may_cross() && !ctx.rects.iter().any(|r| grow(r).intersects(&sb))) || !cell_in_ranges(cell, &way_ranges) || !cell_in_ranges(cell, &member_ranges) {
                        fails.push(format!("kind {kind} shape {shapes}: way {g:?} passes apart from the rectangles; areas {:?}", s.areas));
                    }
                }
                if ctx.db_vertex(g[0]) {
                    first_in += 1;
                    if !passes {
                        fails.push(format!("kind {kind}: first vertex inside, way fails: {g:?}"));
                    }
                }
                if ctx.covers(&sb) && !passes {
                    fails.push(format!("kind {kind} shape {shapes}: covered way fails: {g:?}; areas {:?}", s.areas));
                }
            }
            for _ in 0..1500 {
                let p = sample_point(&mut rng, &s);
                let q = (p.0.saturating_add(rng.n(200_000) as u32).min(1_810_000_000), (p.1 as i64 + rng.n(200_000) as i64).min(1_800_000_000) as i32);
                let b = bbox(&[p, q]);
                if !ctx.covers(&b) {
                    continue;
                }
                covered += 1;
                let mut probes = vec![p, q, (p.0, q.1), (q.0, p.1)];
                for _ in 0..6 {
                    probes.push((p.0 + rng.n((q.0 - p.0) as u64 + 1) as u32, (p.1 as i64 + rng.n((q.1 as i64 - p.1 as i64) as u64 + 1) as i64) as i32));
                }
                for c in &probes {
                    if !(ctx.db_point(*c, false) && ctx.db_point(*c, true) && ctx.db_vertex(*c)) {
                        fails.push(format!("kind {kind} shape {shapes}: point {c:?} of covered box {b:?} is outside; areas {:?}", s.areas));
                    }
                }
                let n = probes.len();
                for w in [vec![probes[0], probes[1]], vec![probes[2], probes[3], probes[4]], vec![probes[n - 1], probes[n - 2], probes[n - 3], probes[n - 1]]] {
                    if !ctx.db_way(&w) {
                        fails.push(format!("kind {kind} shape {shapes}: way {w:?} in covered box fails; areas {:?}", s.areas));
                    }
                }
            }
        }
        eprintln!("review_area_invariants: {shapes} shapes ({fine_n} with fine rectangles, by kind {by_kind:?}), {pts_in} points and {ways_in} ways passed, {first_in} first vertices inside, {covered} covered boxes, {} failures, {:?}", fails.len(), t0.elapsed());
        for f in fails.iter().take(5) {
            eprintln!("FAIL {}", &f[..f.len().min(3000)]);
        }
        assert!(fails.is_empty());
    }

    /// An area whose ring is open (a member way left out because a node of
    /// it has no position): the point test of the base changes along a line
    /// north of the open end, away from any segment. Prints what the fine
    /// rectangles lose.
    #[test]
    #[ignore]
    fn review_open_ring() {
        let mut rng = Rng(77);
        let mut lost = 0;
        let mut example = None;
        for _ in 0..200 {
            let (clat, clon) = (rng.r(40.0, 50.0), rng.r(5.0, 10.0));
            let ring = star(&mut rng, clat, clon, 0.2, 30, 0, 0.0);
            // the ring as three ways; the second is left out
            let n = ring.len();
            let ways = vec![ring[..n / 3 + 1].to_vec(), ring[2 * n / 3..].to_vec()];
            let Some((ctx, true)) = ctx_for(&[ways]) else { continue };
            let s = Shape { areas: vec![vec![ring.clone()]], clat, clon, spread: 0.5, kind: 0 };
            for _ in 0..4000 {
                let p = sample_point(&mut rng, &s);
                let pp = pt(p);
                if ctx.db_point(p, true) && !ctx.rects.iter().any(|r| r.y0 <= pp.y && pp.y <= r.y1 && r.x0 <= pp.x && pp.x <= r.x1) {
                    lost += 1;
                    example.get_or_insert((p, crate::geo::lat_of(p.0), crate::geo::lon_of(p.1)));
                }
            }
        }
        eprintln!("review_open_ring: {lost} points pass the point test outside the fine rectangles; first {example:?}");
    }
}
