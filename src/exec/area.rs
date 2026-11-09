//! Areas: `area` queries, the `(area)` filter, `is_in`, `map_to_area` and
//! `pivot` (docs/spec/areas.md).

use super::Engine;
use super::areageom::{self, Block, HIT, IC, INTERSECT, RelPos, TOGGLE_EAST, TOGGLE_WEST, Tilewise};
use super::eval::{Cx, El, Evaluator};
use super::query::Plan;
use super::set::{Set, sort_dedup};
use crate::geo::{MAX_LEVEL, Pt, Rect, ilat, ilon, level_ranges};
use crate::model::*;
use crate::store::Kind;
use anyhow::Result;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::io::Write;

#[cfg(test)]
thread_local! {
    /// The tag filter whose index entries gave the candidates of the last
    /// `area` query.
    pub(super) static AREA_SOURCE: std::cell::Cell<Option<super::filter::TagFilter>> = const { std::cell::Cell::new(None) };
}

/// Polygons of one `(area)` clause.
pub struct AreaCtx {
    /// Requested DB areas' blocks per block tile: (area id, polylines), ascending id.
    tiles: BTreeMap<u32, Vec<(u32, Vec<Block>)>>,
    /// Closed ways of the input set.
    implicit: Tilewise,
    /// Bounding rectangles for candidate selection.
    pub rects: Vec<Rect>,
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

fn tile_rect(tile: u32) -> Rect {
    let (s, w) = areageom::tile_sw(tile);
    let p = Pt::from_ilat_ilon(s, w);
    Rect { y0: p.y, x0: p.x, y1: p.y.saturating_add(0xfffff), x1: p.x.saturating_add(0xfffff) }
}

impl AreaCtx {
    /// Point test against the DB areas (`add_border`: the border counts).
    fn db_point(&self, p: IC, add_border: bool) -> bool {
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

    /// Way test against the DB areas (C++ `collect_ways`, add_border = false).
    fn db_way(&self, geom: &[IC]) -> bool {
        if self.tiles.is_empty() || geom.is_empty() {
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
            let Some(areas) = self.tiles.get(tile) else { continue };
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
        for w in ways {
            if !is_closed(w) {
                continue;
            }
            if let Some(g) = self.complete_geom(w)? {
                rects.push(bulge_rect(&g));
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
        let ways: Vec<&Way> = input.ways.iter().filter(|w| is_closed(w)).collect();
        let (implicit, way_rects) = self.tilewise_of(&ways)?;
        rects.extend(way_rects);
        Ok(AreaCtx { tiles, implicit, rects })
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
        for part in ways.chunks(BATCH) {
            self.check()?;
            let ids: Vec<Id> = part.iter().flat_map(|w| w.nodes.iter().copied()).collect();
            let pos = self.f.node_positions(&ids)?;
            let keep = |w: &&Way| -> bool {
                let g: Option<Vec<IC>> = w.nodes.iter().map(|n| pos.get(n).map(|p| ic(*p))).collect();
                match g {
                    Some(g) => (!ctx.implicit.is_empty() && ctx.implicit.way(&g)) || ctx.db_way(&g),
                    None => false,
                }
            };
            if part.len() >= 64 && super::parallel_ok() {
                use rayon::prelude::*;
                out.extend(part.par_iter().map(keep).collect::<Vec<bool>>());
            } else {
                out.extend(part.iter().map(keep));
            }
        }
        Ok(out)
    }

    pub fn area_keep_rels(&self, ctx: &AreaCtx, rels: &[&Relation]) -> Result<Vec<bool>> {
        let nids: Vec<Id> = rels.iter().flat_map(|r| r.members.iter()).filter(|m| m.mtype == ElemType::Node).map(|m| m.id).collect();
        let wids: Vec<Id> = rels.iter().flat_map(|r| r.members.iter()).filter(|m| m.mtype == ElemType::Way).map(|m| m.id).collect();
        let npos = self.f.node_positions(&nids)?;
        let ways = self.f.by_ids::<Way>(&wids)?;
        let wrefs: Vec<&Way> = ways.iter().collect();
        let wok: FxHashMap<Id, bool> = ways.iter().map(|w| w.id).zip(self.area_keep_ways(ctx, &wrefs)?).collect();
        let mut nok: FxHashMap<Id, bool> = FxHashMap::default();
        for (id, p) in &npos {
            let q = ic(*p);
            nok.insert(*id, ctx.implicit.point(q.0, q.1, false) == RelPos::Inside || ctx.db_point(q, false));
        }
        Ok(rels
            .iter()
            .map(|r| {
                r.members.iter().any(|m| match m.mtype {
                    ElemType::Node => nok.get(&m.id).copied().unwrap_or(false),
                    ElemType::Way => wok.get(&m.id).copied().unwrap_or(false),
                    _ => false,
                })
            })
            .collect())
    }

    /// DB areas of an `area` query (spatial clauses never filter areas).
    pub fn query_areas(&mut self, plan: &Plan) -> Result<Vec<Area>> {
        if !plan.newer.is_empty() || !plan.users.is_empty() || !plan.uids.is_empty() {
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
        // DB areas: any block hit or odd parity in the point's block tile.
        let mut found: FxHashSet<u32> = FxHashSet::default();
        let mut by_tile: BTreeMap<u32, Vec<IC>> = BTreeMap::new();
        for p in &points {
            by_tile.entry(areageom::block_tile(p.0, p.1)).or_default().push(*p);
        }
        for (tile, pts) in &by_tile {
            let blocks = self.f.snap.area_blocks_in_tile(*tile)?;
            for p in pts {
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
        let cands = self.f.in_ranges::<Way>(&ranges, is_closed)?;
        let refs: Vec<&Way> = cands.iter().collect();
        let (tw, _) = self.tilewise_of(&refs)?;
        let mut hit: FxHashSet<Id> = FxHashSet::default();
        for p in &points {
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
