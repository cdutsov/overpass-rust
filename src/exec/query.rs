// Contains code translated from C++ to Rust in 2026 from Overpass API,
// Copyright 2008-2018 Roland Olbricht et al. Modified by Chavdar Dutsov.
// GNU Affero General Public License, version 3 or later. See LICENSE and NOTICE.

//! The query statement: candidate generation and filtering per element type.

use super::fetch::{Fetch, Source, ranges_for, tile_runs};
use super::filter::TagFilter;
use super::geom::{AroundSource, EGeom, PolyTest, Polygon, RectPos, rel_geoms, rel_geoms_near, way_geoms};
use super::set::Set;
use super::{Abort, Engine};
use crate::geo::{BBox, Cell, CodeRange, Rect, cell_in_ranges, tile_of};
use crate::model::*;
use crate::ql::ast::*;
use crate::store::Kind;
use crate::store::codec::{self, TagTest};
use crate::update::Stored;
use anyhow::Result;
use rustc_hash::FxHashSet as HashSet;
use std::io::Write;

/// Compiled clauses of one query statement.
pub struct Plan {
    pub ids: Option<Vec<Id>>,
    pub inputs: Vec<String>,
    pub tags: Vec<TagFilter>,
    pub bboxes: Vec<BBox>,
    pub arounds: Vec<(Option<String>, f64, Vec<(f64, f64)>)>,
    pub polys: Vec<Polygon>,
    pub newer: Vec<i64>,
    pub users: Vec<Vec<String>>,
    pub uids: Vec<Vec<u32>>,
    pub ifs: Vec<Expr>,
    pub recurse: Vec<(RecurseFilter, String, Option<String>, Option<Vec<i64>>, Option<(u32, u32)>)>,
    pub recurse_ops: Vec<RecurseOp>,
    pub areas: Vec<(String, Option<Id>)>,
    pub pivots: Vec<String>,
    /// Elements already decoded that the other clauses narrow down (the
    /// relations of `rel_spatial_by_candidates`); taken by the query.
    pub given: std::sync::Mutex<Option<Set>>,
}

impl Plan {
    pub fn new(filters: &[Filter]) -> Result<Plan> {
        let mut p = Plan {
            ids: None,
            inputs: vec![],
            tags: vec![],
            bboxes: vec![],
            arounds: vec![],
            polys: vec![],
            newer: vec![],
            users: vec![],
            uids: vec![],
            ifs: vec![],
            recurse: vec![],
            recurse_ops: vec![],
            areas: vec![],
            pivots: vec![],
            given: std::sync::Mutex::new(None),
        };
        for f in filters {
            if let Some(t) = TagFilter::from_ast(f)? {
                p.tags.push(t);
                continue;
            }
            match f {
                Filter::Ids(ids) => {
                    let mut ids = ids.clone();
                    ids.sort_unstable();
                    ids.dedup();
                    p.ids = Some(match p.ids.take() {
                        Some(prev) => prev.into_iter().filter(|i| ids.binary_search(i).is_ok()).collect(),
                        None => ids,
                    });
                }
                Filter::Input(s) => p.inputs.push(s.clone()),
                Filter::BBox(b) => p.bboxes.push(*b),
                Filter::Around { input, radius, points } => p.arounds.push((input.clone(), *radius, points.clone())),
                Filter::Poly(pts) => p.polys.push(Polygon { pts: pts.clone() }),
                Filter::Newer(t) => p.newer.push(*t),
                Filter::User(n) => p.users.push(n.clone()),
                Filter::Uid(u) => p.uids.push(u.clone()),
                Filter::If(e) => p.ifs.push(e.clone()),
                Filter::Recurse { kind, input, role, positions, count } => p.recurse.push((*kind, input.clone(), role.clone(), positions.clone(), *count)),
                Filter::RecurseOp(op) => p.recurse_ops.push(*op),
                Filter::Area { set, id } => p.areas.push((set.clone(), *id)),
                Filter::Pivot(s) => p.pivots.push(s.clone()),
                _ => {}
            }
        }
        Ok(p)
    }

    fn has_spatial(&self) -> bool {
        !self.bboxes.is_empty() || !self.arounds.is_empty() || !self.polys.is_empty() || !self.areas.is_empty()
    }
}

/// A single criterion that the C++ parser turns into its own statement;
/// those ignore the global bbox.
fn is_standalone(types: &Types, filters: &[Filter]) -> bool {
    if filters.len() != 1 {
        return false;
    }
    let only_node = types.node && !types.way && !types.relation && !types.area && !types.derived;
    let single = [types.node, types.way, types.relation, types.area, types.derived].iter().filter(|b| **b).count() == 1;
    let nwr = types.node && types.way && types.relation;
    match &filters[0] {
        Filter::BBox(_) | Filter::Around { .. } | Filter::Poly(_) => only_node,
        Filter::Ids(_) => single && !types.area && !types.derived,
        Filter::Recurse { .. } => !nwr,
        _ => false,
    }
}

/// Whether an expression evaluates from the element alone, without node
/// coordinates or meta data.
fn is_cheap_expr(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) | Expr::Tag(_) | Expr::Generic | Expr::SetProp { .. } => true,
        Expr::Call(name, args) => {
            !matches!(
                name.as_str(),
                "geom" | "length" | "center" | "trace" | "hull" | "lat" | "lon" | "per_vertex" | "angle" | "pt" | "lstr" | "poly" | "version" | "timestamp" | "changeset" | "uid" | "user"
            ) && args.iter().all(is_cheap_expr)
        }
        Expr::Agg { .. } => false,
        Expr::Unary(_, a) => is_cheap_expr(a),
        Expr::Binary(_, a, b) => is_cheap_expr(a) && is_cheap_expr(b),
        Expr::Ternary(a, b, c) => is_cheap_expr(a) && is_cheap_expr(b) && is_cheap_expr(c),
    }
}

/// The C++ statement name of a query that is a standalone criterion, as
/// error messages show it ("Query timed out in \"bbox-query\" ...").
pub fn standalone_name(types: &Types, filters: &[Filter]) -> Option<&'static str> {
    if !is_standalone(types, filters) {
        return None;
    }
    Some(match &filters[0] {
        Filter::BBox(_) => "bbox-query",
        Filter::Around { .. } => "around",
        Filter::Poly(_) => "polygon-query",
        Filter::Ids(_) => "id-query",
        Filter::Recurse { .. } => "recurse",
        _ => return None,
    })
}

/// Element access common to nodes, ways and relations.
pub trait QElem: Stored {
    fn tags_of(&self) -> &Tags;
    fn from_set(s: &Set) -> &Vec<Self>;
    fn from_set_mut(s: &mut Set) -> &mut Vec<Self>;
    fn etype() -> ElemType;
    fn geoms(f: &Fetch, elems: &[&Self]) -> Result<Vec<EGeom>>;
    /// Geometry sufficient for a test against one bbox only (`rects`).
    fn bbox_geoms(f: &Fetch, elems: &[&Self], _rects: &[Rect]) -> Result<Vec<EGeom>> {
        Self::geoms(f, elems)
    }
    /// Geometry sufficient for bbox and around tests whose regions meet
    /// the stored boxes `near` accepts (relations: see `rel_geoms_near`).
    fn near_geoms(f: &Fetch, elems: &[&Self], _near: &dyn Fn(&Rect) -> bool) -> Result<Vec<EGeom>> {
        Self::geoms(f, elems)
    }
    fn as_el(&self) -> super::eval::El<'_>;
    /// `(area)` clause test.
    fn area_keep<W: Write>(eng: &Engine<'_, W>, ctx: &super::area::AreaCtx, elems: &[&Self]) -> Result<Vec<bool>>;
    /// Bounding rectangle stored with the element (ways and relations).
    fn stored_bbox(&self) -> Option<Rect> {
        None
    }
    /// The element can never match a spatial clause (way with a missing
    /// node, relation without usable members).
    fn spatially_empty(&self) -> bool {
        false
    }
}

impl QElem for Node {
    fn tags_of(&self) -> &Tags {
        &self.tags
    }
    fn from_set(s: &Set) -> &Vec<Self> {
        &s.nodes
    }
    fn from_set_mut(s: &mut Set) -> &mut Vec<Self> {
        &mut s.nodes
    }
    fn etype() -> ElemType {
        ElemType::Node
    }
    fn geoms(_f: &Fetch, elems: &[&Self]) -> Result<Vec<EGeom>> {
        Ok(elems.iter().map(|n| EGeom::Point(n.pos)).collect())
    }
    fn as_el(&self) -> super::eval::El<'_> {
        super::eval::El::N(self)
    }
    fn area_keep<W: Write>(eng: &Engine<'_, W>, ctx: &super::area::AreaCtx, elems: &[&Self]) -> Result<Vec<bool>> {
        eng.area_keep_nodes(ctx, elems)
    }
}
impl QElem for Way {
    fn tags_of(&self) -> &Tags {
        &self.tags
    }
    fn from_set(s: &Set) -> &Vec<Self> {
        &s.ways
    }
    fn from_set_mut(s: &mut Set) -> &mut Vec<Self> {
        &mut s.ways
    }
    fn etype() -> ElemType {
        ElemType::Way
    }
    fn geoms(f: &Fetch, elems: &[&Self]) -> Result<Vec<EGeom>> {
        way_geoms(f, elems)
    }
    fn as_el(&self) -> super::eval::El<'_> {
        super::eval::El::W(self)
    }
    fn area_keep<W: Write>(eng: &Engine<'_, W>, ctx: &super::area::AreaCtx, elems: &[&Self]) -> Result<Vec<bool>> {
        eng.area_keep_ways(ctx, elems)
    }
    fn stored_bbox(&self) -> Option<Rect> {
        Some(self.bbox)
    }
    fn spatially_empty(&self) -> bool {
        self.missing
    }
}
impl QElem for Relation {
    fn tags_of(&self) -> &Tags {
        &self.tags
    }
    fn from_set(s: &Set) -> &Vec<Self> {
        &s.rels
    }
    fn from_set_mut(s: &mut Set) -> &mut Vec<Self> {
        &mut s.rels
    }
    fn etype() -> ElemType {
        ElemType::Relation
    }
    fn geoms(f: &Fetch, elems: &[&Self]) -> Result<Vec<EGeom>> {
        rel_geoms(f, elems)
    }
    fn bbox_geoms(f: &Fetch, elems: &[&Self], rects: &[Rect]) -> Result<Vec<EGeom>> {
        super::geom::rel_geoms_for_bbox(f, elems, rects)
    }
    fn near_geoms(f: &Fetch, elems: &[&Self], near: &dyn Fn(&Rect) -> bool) -> Result<Vec<EGeom>> {
        rel_geoms_near(f, elems, near)
    }
    fn as_el(&self) -> super::eval::El<'_> {
        super::eval::El::R(self)
    }
    fn area_keep<W: Write>(eng: &Engine<'_, W>, ctx: &super::area::AreaCtx, elems: &[&Self]) -> Result<Vec<bool>> {
        eng.area_keep_rels(ctx, elems)
    }
    fn stored_bbox(&self) -> Option<Rect> {
        Some(self.bbox)
    }
    fn spatially_empty(&self) -> bool {
        !self.spatial
    }
}

/// Integer node-in-bbox test (C++ `Bbox_Filter` for nodes).
fn node_in_bbox(p: crate::geo::Pt, b: &BBox) -> bool {
    use crate::geo::{ilat, ilon};
    let (s, n, w, e) = (ilat(b.south), ilat(b.north), ilon(b.west), ilon(b.east));
    let la = p.y;
    let lo = p.ilon();
    s <= la && la <= n && ((w <= lo && lo <= e) || (e < w && (lo >= w || lo <= e)))
}

/// Way test: ways with a missing node have no geometry and never match.
fn line_in_bbox(v: &[Option<crate::geo::Pt>], b: &BBox) -> bool {
    if v.is_empty() || v.iter().any(|p| p.is_none()) {
        return false;
    }
    let pts: Vec<(f64, f64)> = v.iter().flatten().map(|p| (p.lat(), p.lon())).collect();
    if pts.len() == 1 {
        return crate::geo::bbox_contains(b, pts[0]);
    }
    pts.windows(2).any(|w| crate::geo::bbox_intersects(b, w[0], w[1]))
}

/// Element-type specific bbox test (spec §5).
fn geom_in_bbox(g: &EGeom, b: &BBox) -> bool {
    match g {
        EGeom::Point(p) => node_in_bbox(*p, b),
        EGeom::Line(v) => line_in_bbox(v, b),
        EGeom::Multi { points, lines } => {
            points.iter().any(|p| crate::geo::bbox_contains(b, (p.lat(), p.lon()))) || lines.iter().any(|l| line_in_bbox(l, b))
        }
        EGeom::Empty => false,
    }
}

impl<'s, W: Write> Engine<'s, W> {
    pub fn query(&mut self, types: &Types, filters: &[Filter]) -> Result<Set> {
        // `node(pivot.s)` alone is the standalone pivot statement: it returns
        // nodes, ways and relations (C++ quirk); the global bbox does not apply.
        if let [Filter::Pivot(set)] = filters {
            if types.node && !types.way && !types.relation && !types.area && !types.derived {
                let inp = self.get(set);
                let mut r = self.pivot(&inp)?;
                r.ways = super::set::merge(&r.ways, &inp.ways);
                return Ok(r);
            }
        }
        let mut filters = filters.to_vec();
        if !is_standalone(types, &filters) {
            if let Some(b) = self.settings.bbox {
                if b.south <= b.north {
                    filters.push(Filter::BBox(b));
                }
            }
        }
        let plan = Plan::new(&filters)?;
        let mut out = Set::default();
        // Area polygons and around sources are shared by all element types.
        let mut area_ctxs = Vec::new();
        let mut arounds = Vec::new();
        if types.node || types.way || types.relation || types.area {
            for (set, id) in &plan.areas {
                area_ctxs.push(self.area_ctx(set, *id)?);
            }
            for (input, radius, points) in &plan.arounds {
                let mut src = self.around_source(input, points)?;
                src.prepare(*radius);
                arounds.push((src, *radius));
            }
        }
        let pre = (&area_ctxs[..], &arounds[..]);
        if types.node {
            out.nodes = self.query_type::<Node>(&plan, pre)?;
        }
        if types.way {
            out.ways = self.query_type::<Way>(&plan, pre)?;
        }
        if types.relation {
            out.rels = match self.rel_spatial_by_members(&filters, &arounds)? {
                Some(rplan) => self.query_type::<Relation>(&rplan, (&area_ctxs[..], &[]))?,
                None => self.query_type::<Relation>(&plan, pre)?,
            };
        }
        if types.area {
            out.areas = self.query_areas(&plan)?;
            // `area` queries also return matching closed ways.
            let mut ways = self.query_type::<Way>(&plan, pre)?;
            ways.retain(|w| !w.nodes.is_empty() && w.nodes.first() == w.nodes.last());
            out.ways = super::set::merge(&out.ways, &ways);
        }
        if types.derived {
            out.derived = self.query_derived(&plan)?;
        }
        Ok(out)
    }

    fn around_source(&self, input: &Option<String>, points: &[(f64, f64)]) -> Result<AroundSource> {
        let mut src = AroundSource::default();
        if !points.is_empty() {
            src.points = points.to_vec();
            for w in points.windows(2) {
                src.segments.push(crate::geo::Segment::new(w[0].0, w[0].1, w[1].0, w[1].1));
            }
            return Ok(src);
        }
        let set = self.get(input.as_deref().unwrap_or("_"));
        for n in &set.nodes {
            src.points.push((n.pos.lat(), n.pos.lon()));
        }
        for g in way_geoms(&self.f, &set.ways)? {
            src.add(&g);
        }
        for g in rel_geoms(&self.f, &set.rels)? {
            if let EGeom::Multi { points, lines } = g {
                for p in points {
                    src.points.push((p.lat(), p.lon()));
                }
                for l in lines {
                    src.add(&EGeom::Line(l));
                }
            }
        }
        Ok(src)
    }

    /// Candidate ids from all clauses that collect data directly.
    fn collected<T: QElem>(&mut self, plan: &Plan) -> Result<Option<Vec<T>>> {
        let mut sources: Vec<Vec<T>> = Vec::new();
        if let Some(mut given) = plan.given.lock().unwrap_or_else(|e| e.into_inner()).take() {
            sources.push(std::mem::take(T::from_set_mut(&mut given)));
        }
        if let Some(ids) = &plan.ids {
            let ids: Vec<Id> = if T::KIND == Kind::Node { ids.clone() } else { ids.iter().copied().filter(|i| *i <= u32::MAX as u64).collect() };
            sources.push(self.f.by_ids::<T>(&ids)?);
        }
        for name in &plan.inputs {
            sources.push(self.lookup(name).map(|s| T::from_set(s).clone()).unwrap_or_default());
        }
        for (kind, input, role, positions, count) in &plan.recurse {
            let inp = self.get(input);
            let r = self.recurse_filter(*kind, &inp, role.as_deref(), positions.as_deref(), *count, T::etype())?;
            sources.push(T::from_set(&r).clone());
        }
        for op in &plan.recurse_ops {
            let inp = self.get("_");
            let r = self.recurse_constraint(*op, &inp)?;
            sources.push(T::from_set(&r).clone());
        }
        for set in &plan.pivots {
            let inp = self.get(set);
            let r = self.pivot(&inp)?;
            let mut v = T::from_set(&r).clone();
            if T::KIND == Kind::Way {
                // `way(pivot)` passes all ways of the input set through.
                let extra = T::from_set(&inp).clone();
                v.extend(extra);
                v.sort_by_key(|e| e.id());
                v.dedup_by_key(|e| e.id());
            }
            sources.push(v);
        }
        if sources.is_empty() {
            return Ok(None);
        }
        sources.sort_by_key(|s| s.len());
        let mut it = sources.into_iter();
        let mut acc = it.next().unwrap();
        for s in it {
            let ids: HashSet<Id> = s.iter().map(|e| e.id()).collect();
            acc.retain(|e| ids.contains(&e.id()));
        }
        Ok(Some(acc))
    }

    fn spatial_rects(&self, plan: &Plan, arounds: &[(AroundSource, f64)], area_ctxs: &[super::area::AreaCtx]) -> Result<Option<Vec<Rect>>> {
        // Use the most restrictive single spatial clause for range selection.
        let mut options: Vec<(&'static str, Vec<Rect>)> = plan.bboxes.iter().map(|b| ("bbox", b.rects())).collect();
        options.extend(plan.polys.iter().map(|p| ("poly", p.bbox().rects())));
        options.extend(arounds.iter().map(|(src, r)| ("around", src.bbox_rects(*r))));
        options.extend(area_ctxs.iter().map(|c| ("area", c.rects.clone())));
        let mut best: Option<(u64, &'static str, Vec<Rect>)> = None;
        if options.len() == 1 {
            best = options.pop().map(|(what, rects)| (0, what, rects));
        }
        for (what, rects) in options {
            let n = region_cost(&rects);
            if best.as_ref().is_none_or(|(b, _, _)| n < *b) {
                best = Some((n, what, rects));
            }
        }
        #[cfg(test)]
        tests::REGION.set(best.as_ref().map(|(_, what, _)| *what));
        Ok(best.map(|(_, _, r)| r))
    }

    fn query_type<T: QElem>(&mut self, plan: &Plan, (area_ctxs, arounds): (&[super::area::AreaCtx], &[(AroundSource, f64)])) -> Result<Vec<T>> {
        let kind = T::KIND;
        let collected = self.collected::<T>(plan)?;
        let mut cands: Vec<T> = match collected {
            Some(c) => c,
            None => {
                let rects = self.spatial_rects(plan, arounds, area_ctxs)?;
                let ranges: Option<Vec<CodeRange>> = rects.as_ref().map(|r| ranges_for(kind, r));
                if let Some((pairs, read)) = self.indexed_candidates(kind, plan, ranges.as_deref())? {
                    self.check()?;
                    self.at_tagged::<T>(plan, pairs, &read)?
                } else if let Some(r) = &ranges {
                    // A key or value-regex filter over a large region (a
                    // country) reads its tag index instead of every chunk.
                    // In a smaller region, an anchored regex reads the
                    // index entries of its prefixes in the region, unless
                    // that takes more steps than the region has chunks.
                    let indexed = plan.tags.iter().find(|t| matches!(t, TagFilter::HasKey(_) | TagFilter::Regex { neg: false, .. }));
                    let anchored = plan.tags.iter().find(|t| anchored(t));
                    let pairs = match indexed {
                        Some(t) => {
                            let (region, total) = self.f.chunk_share(kind, r)?;
                            if total > 0 && region > (total as f64 * 0.1).ceil() as usize {
                                Some((self.f.tag_candidates_in(kind, t, r)?, t))
                            } else if let Some(a) = anchored {
                                #[cfg(test)]
                                let region = tests::INDEX_STEPS.get().unwrap_or(region);
                                self.f.tag_candidates_bounded(kind, a, Some(r), region)?.map(|p| (p, a))
                            } else {
                                None
                            }
                        }
                        None => None,
                    };
                    #[cfg(test)]
                    tests::INDEXED.set(pairs.is_some());
                    match pairs {
                        Some((pairs, t)) => {
                            self.check()?;
                            self.at_tagged::<T>(plan, pairs, &[t])?
                        }
                        None => self.f.in_ranges::<T>(r, |e| plan.tags.iter().all(|t| t.matches(e.tags_of())))?,
                    }
                } else if let Some(t) = plan.tags.iter().find(|t| anchored(t)).or_else(|| plan.tags.iter().find(|t| t.is_positive())) {
                    let pairs = self.f.tag_candidates(kind, t, None)?;
                    self.check()?;
                    self.at_tagged::<T>(plan, pairs, &[t])?
                } else {
                    if kind != Kind::Relation && !self.negated_tags_exist(kind, plan)? {
                        self.report("runtime error: Filters too weak in query statement: specify in addition a bbox, a tag filter, or similar.");
                    }
                    self.f.all::<T>(|e| plan.tags.iter().all(|t| t.matches(e.tags_of())))?
                }
            }
        };
        self.check()?;
        cands.retain(|e| plan.tags.iter().all(|t| t.matches(e.tags_of())));
        // (if:) is a per-element test, so its place among the filters does
        // not change the result; one that needs neither geometry nor meta
        // data is cheaper than the spatial tests and runs first.
        let ifs_early = !plan.ifs.is_empty() && plan.has_spatial() && plan.ifs.iter().all(is_cheap_expr);
        if ifs_early && !cands.is_empty() {
            cands = self.filter_if::<T>(cands, &plan.ifs)?;
        }
        // Spatial clauses. The stored bbox of ways and relations decides
        // most candidates without resolving node coordinates.
        if plan.has_spatial() && !cands.is_empty() {
            let grow = |r: &Rect| Rect { y0: r.y0.saturating_sub(1), x0: r.x0.saturating_sub(1), y1: r.y1.saturating_add(1), x1: r.x1.saturating_add(1) };
            let bbox_rects: Vec<Vec<Rect>> = plan.bboxes.iter().map(|b| b.rects()).collect();
            let around_rects: Vec<Vec<Rect>> = arounds.iter().map(|(s, r)| s.bbox_rects(*r)).collect();
            let mut other_rects: Vec<Vec<Rect>> = plan.polys.iter().map(|p| p.bbox().rects()).collect();
            other_rects.extend(area_ctxs.iter().map(|c| c.rects.clone()));
            let only_bbox = arounds.is_empty() && plan.polys.is_empty() && plan.areas.is_empty();
            let mut keep = vec![true; cands.len()];
            let mut undecided: Vec<usize> = Vec::new();
            for (i, e) in cands.iter().enumerate() {
                if let Some(sb) = e.stored_bbox() {
                    if sb.is_empty() || (e.spatially_empty() && area_ctxs.is_empty()) {
                        keep[i] = false;
                        continue;
                    }
                    // Wider than half the world: may cross the antimeridian,
                    // where the stored box covers the wrong side.
                    if sb.x1 - sb.x0 > 0x8000_0000 {
                        undecided.push(i);
                        continue;
                    }
                    let mut all_inside = true;
                    let mut reject = false;
                    for rs in &bbox_rects {
                        if !rs.iter().any(|r| grow(r).intersects(&sb)) {
                            reject = true;
                            break;
                        }
                        let inside = rs.iter().any(|r| r.y0 < sb.y0 && r.x0 < sb.x0 && sb.y1 < r.y1 && sb.x1 < r.x1);
                        all_inside &= inside;
                    }
                    if !reject {
                        reject = arounds.iter().zip(&around_rects).any(|((s, _), rs)| !s.near_box(rs, &sb)) || other_rects.iter().any(|rs| rs.iter().all(|r| !grow(r).intersects(&sb)));
                    }
                    if reject {
                        keep[i] = false;
                        continue;
                    }
                    if only_bbox && all_inside {
                        continue;
                    }
                }
                undecided.push(i);
            }
            if !undecided.is_empty() {
                let refs: Vec<&T> = undecided.iter().map(|&i| &cands[i]).collect();
                // With only (area) clauses there is nothing to test on these
                // geometries: the area tests below build their own.
                let geom_tests = !plan.bboxes.is_empty() || !arounds.is_empty() || !plan.polys.is_empty();
                let mut sub_keep: Vec<bool> = if !geom_tests {
                    vec![true; refs.len()]
                } else {
                    let geoms = if only_bbox && bbox_rects.len() == 1 {
                        T::bbox_geoms(&self.f, &refs, &bbox_rects[0])?
                    } else if reduce_rel_geoms(plan) {
                        let near = |b: &Rect| bbox_rects.iter().any(|rs| rs.iter().any(|r| grow(r).intersects(b))) || arounds.iter().zip(&around_rects).any(|((s, _), rs)| s.near_box(rs, b));
                        T::near_geoms(&self.f, &refs, &near)?
                    } else {
                        T::geoms(&self.f, &refs)?
                    };
                    let test = |g: &EGeom| {
                        plan.bboxes.iter().all(|b| geom_in_bbox(g, b))
                            && arounds.iter().all(|(src, r)| around_match(src, g, *r))
                            && plan.polys.iter().all(|p| poly_match(p, g))
                    };
                    // Geometric tests are pure: run them in parallel for large batches.
                    if geoms.len() >= 512 && super::parallel_ok() {
                        use rayon::prelude::*;
                        geoms.par_iter().map(test).collect()
                    } else {
                        geoms.iter().map(test).collect()
                    }
                };
                if !area_ctxs.is_empty() {
                    let live: Vec<usize> = (0..refs.len()).filter(|&j| sub_keep[j]).collect();
                    let live_refs: Vec<&T> = live.iter().map(|&j| refs[j]).collect();
                    for ctx in area_ctxs {
                        let ok = T::area_keep(self, ctx, &live_refs)?;
                        for (k, &j) in live.iter().enumerate() {
                            sub_keep[j] &= ok[k];
                        }
                    }
                }
                for (j, &i) in undecided.iter().enumerate() {
                    keep[i] = sub_keep[j];
                }
            }
            let mut i = 0;
            cands.retain(|_| {
                i += 1;
                keep[i - 1]
            });
        }
        // Meta clauses.
        if !plan.newer.is_empty() || !plan.users.is_empty() || !plan.uids.is_empty() {
            let ids: Vec<Id> = cands.iter().map(|e| e.id()).collect();
            let metas = self.f.meta(kind, &ids)?;
            let mut uid_sets: Vec<HashSet<u32>> = plan.uids.iter().map(|u| u.iter().copied().filter(|x| *x != 0).collect()).collect();
            for names in &plan.users {
                uid_sets.push(self.f.snap.uids_for_names(names)?.into_iter().collect());
            }
            cands.retain(|e| match metas.get(&e.id()) {
                None => false,
                Some(m) => plan.newer.iter().all(|t| m.timestamp >= *t) && uid_sets.iter().all(|s| s.contains(&m.uid)),
            });
        }
        if !plan.ifs.is_empty() && !ifs_early {
            cands = self.filter_if::<T>(cands, &plan.ifs)?;
        }
        cands.sort_by_key(|e| e.id());
        cands.dedup_by_key(|e| e.id());
        // filters above may have dropped most candidates; the set is kept
        cands.shrink_to_fit();
        Ok(cands)
    }

    /// `around` and `poly` on relations through their members. C++ matches
    /// a relation iff a member node passes the node test or a member way
    /// the way test (`around.cc:268-311`; for `poly` the member variants
    /// with `add_border = false`, `polygon_query.cc:93-127`), so the
    /// relations having a member among the nodes and ways that pass are the
    /// result, without building relation geometries (long routes cost
    /// seconds). Returns the plan without these clauses, restricted to those
    /// ids, or None when a region is large (its nodes would cost more than
    /// the geometries).
    fn rel_spatial_by_members(&mut self, filters: &[Filter], arounds: &[(AroundSource, f64)]) -> Result<Option<Plan>> {
        // About 0.01 square degrees (10 x 7 km in central Europe).
        const MAX_REGION: f64 = 1e12;
        let size = |rects: &[Rect]| -> f64 { rects.iter().map(|x| (x.y1 - x.y0) as f64 * (x.x1 - x.x0) as f64).sum() };
        let clauses: Vec<&Filter> = filters.iter().filter(|f| matches!(f, Filter::Around { .. } | Filter::Poly(_))).collect();
        if clauses.is_empty() || clauses.iter().filter(|f| matches!(f, Filter::Around { .. })).count() != arounds.len() {
            return Ok(None);
        }
        // The member tests of the clauses, in their order, with their rectangles.
        let mut next_around = arounds.iter();
        let member_tests: Vec<(MemberTest, Vec<Rect>)> = clauses
            .iter()
            .map(|f| match f {
                Filter::Poly(pts) => {
                    let poly = Polygon { pts: pts.clone() };
                    let rects = poly.bbox().rects();
                    (MemberTest::Poly(poly.prepare()), rects)
                }
                _ => {
                    let (src, r) = next_around.next().expect("counted above");
                    (MemberTest::Around(src, *r), src.bbox_rects(*r))
                }
            })
            .collect();
        if member_tests.iter().any(|(_, rects)| size(rects) > MAX_REGION) {
            return Ok(None);
        }
        #[cfg(not(test))]
        let try_candidates = true;
        #[cfg(test)]
        let try_candidates = !tests::CANDIDATES_OFF.get();
        if try_candidates {
            if let Some(rels) = self.rel_spatial_by_candidates(filters, &member_tests)? {
                #[cfg(test)]
                tests::CANDIDATES.set(Some("answered"));
                let rest: Vec<Filter> = filters.iter().filter(|f| !matches!(f, Filter::Around { .. } | Filter::Poly(_))).cloned().collect();
                let mut plan = Plan::new(&rest)?;
                plan.given = std::sync::Mutex::new(Some(Set { rels, ..Default::default() }));
                return Ok(Some(plan));
            }
        }
        let mut ids: Option<Vec<Id>> = None;
        let mut next_around = arounds.iter();
        for clause in clauses {
            let mut members = Set::default();
            match clause {
                Filter::Around { .. } => {
                    let around = next_around.next().expect("counted above");
                    let sub = Plan::new(std::slice::from_ref(clause))?;
                    let pre = (&[][..], std::slice::from_ref(around));
                    members.nodes = self.query_type::<Node>(&sub, pre)?;
                    members.ways = self.query_type::<Way>(&sub, pre)?;
                }
                Filter::Poly(pts) => {
                    let poly = Polygon { pts: pts.clone() };
                    let test = poly.prepare();
                    let rects = poly.bbox().rects();
                    // ids and positions only: the tags are not tested
                    let nodes = self.f.in_ranges_by::<Node>(&ranges_for(Kind::Node, &rects), |v, p| codec::decode_nodes(v, p, false), |n| test.contains_member(n.pos))?;
                    let grow = |r: &Rect| Rect { y0: r.y0.saturating_sub(1), x0: r.x0.saturating_sub(1), y1: r.y1.saturating_add(1), x1: r.x1.saturating_add(1) };
                    // ways apart from the polygon fail without their vertices
                    let boxes = way_boxes_on();
                    let mut ways = self.f.in_ranges_by::<Way>(&ranges_for(Kind::Way, &rects), |v, p| codec::decode_ways(v, p, false), |w| rects.iter().any(|r| grow(r).intersects(&w.bbox)) && !(boxes && test.rect_pos(&w.bbox) == RectPos::Apart))?;
                    self.check()?;
                    let pass = poly_member_ways(&self.f, &test, &ways.iter().collect::<Vec<_>>())?;
                    let mut i = 0;
                    ways.retain(|_| {
                        i += 1;
                        pass[i - 1]
                    });
                    members.nodes = nodes;
                    members.ways = ways;
                }
                _ => unreachable!(),
            }
            // the relations are decoded once, by the plan below
            let node_ids: Vec<Id> = members.nodes.iter().map(|n| n.id).collect();
            let way_ids: Vec<Id> = members.ways.iter().map(|w| w.id).collect();
            let mut v = self.f.snap.relations_with_members(ElemType::Node, &node_ids)?;
            v.extend(self.f.snap.relations_with_members(ElemType::Way, &way_ids)?);
            v.sort_unstable();
            v.dedup();
            ids = Some(match ids {
                None => v,
                Some(prev) => prev.into_iter().filter(|i| v.binary_search(i).is_ok()).collect(),
            });
        }
        let rest: Vec<Filter> = filters.iter().filter(|f| !matches!(f, Filter::Around { .. } | Filter::Poly(_))).cloned().collect();
        let mut plan = Plan::new(&rest)?;
        let ids = ids.unwrap_or_default();
        plan.ids = Some(match plan.ids.take() {
            Some(prev) => prev.into_iter().filter(|i| ids.binary_search(i).is_ok()).collect(),
            None => ids,
        });
        Ok(Some(plan))
    }

    /// `around` and `poly` on relations starting from the relations a tag
    /// filter selects near the region of the first clause (tag index,
    /// restricted to the cells of the region): only their member nodes and
    /// ways get the node and way tests of `rel_spatial_by_members`, so the
    /// result is the same. A city centre holds 10^5 to 10^6 nodes and 10^5
    /// ways within a few km, while a route filter there selects a few
    /// hundred relations. A bbox clause cheaper to read than the region
    /// (`region_cost`; a few km of a long river) takes the place of the
    /// region. The index is read for a `key=value` filter or an anchored
    /// regex; without such a filter, or when that scan takes too many
    /// steps, the relations stored in the cells of the region are the
    /// candidates.
    ///
    /// A polygon decides a relation by its box where it can: the box holds
    /// every member node and the box of every member way, so with the box
    /// strictly inside a member node or a complete member way
    /// (`Relation::spatial`) passes, and with the box apart from it none
    /// does. Only the relations across a border need their members, which
    /// are looked up by id when they cost less than the nodes of the
    /// region, else picked out of the nodes and ways of the region (polygons
    /// only). Of the 2 x 10^4 relations of a polygon over a city centre, 10^3
    /// cross its border, with 4 x 10^5 members. The relations that pass
    /// every clause (sorted by id), or None when the candidates or their
    /// members would cost more than the member path.
    fn rel_spatial_by_candidates(&mut self, filters: &[Filter], member_tests: &[(MemberTest, Vec<Rect>)]) -> Result<Option<Vec<Relation>>> {
        const MAX_CANDIDATES: usize = 20_000;
        const NODES_PER_CHUNK: usize = 500;
        const MEMBER_COST: usize = 4;
        const MEMBERS_PER_RELATION: usize = 50;
        let Some((_, rects0)) = member_tests.first() else { return Ok(None) };
        let rest: Vec<Filter> = filters.iter().filter(|f| !matches!(f, Filter::Around { .. } | Filter::Poly(_))).cloned().collect();
        let plan = Plan::new(&rest)?;
        let region_cost0 = region_cost(rects0);
        let bbox = plan.bboxes.iter().map(|b| (region_cost(&b.rects()), b.rects())).filter(|(n, _)| *n < region_cost0).min_by_key(|(n, _)| *n).map(|(_, rs)| rs);
        let ranges = ranges_for(Kind::Relation, bbox.as_deref().unwrap_or(rects0));
        // An unanchored regex or a key filter would walk every value of its
        // key (every name on the planet): those are tested on the relations
        // stored in the cells of the region instead.
        let probe = match self.most_selective(Kind::Relation, &plan, Some(&ranges))? {
            Some(eq) => Some(eq),
            None => plan.tags.iter().find(|t| anchored(t)),
        };
        // Cost: a member is a lookup by id, a node of the region a part of
        // a chunk read in order (about 500 per chunk), and the region path
        // also reads its ways. Long routes near a small region (100 m) have
        // more members than the region has nodes: keep the region path.
        let region_nodes = self.f.snap.chunks_in_ranges(Kind::Node, &ranges_for(Kind::Node, rects0))?.len() * NODES_PER_CHUNK;
        // Candidates as many as their members (about 50 each) would cost.
        // The index scan may take as many steps (values, cell groups);
        // past that, as without a probe.
        let cap = (region_nodes / (MEMBER_COST * MEMBERS_PER_RELATION)).min(MAX_CANDIDATES);
        let pairs = match probe {
            Some(_) if cap == 0 => None,
            Some(t) => self.f.tag_candidates_bounded(Kind::Relation, t, Some(&ranges), cap)?,
            None => None,
        };
        #[cfg(test)]
        tests::CANDIDATES_FROM.set(Some(if pairs.is_some() { "tags" } else if bbox.is_some() { "bbox" } else { "cells" }));
        if pairs.as_ref().is_some_and(|p| p.len() > MAX_CANDIDATES) {
            #[cfg(test)]
            tests::CANDIDATES.set(Some("too_many_candidates"));
            return Ok(None);
        }
        // as the stored-bbox test of query_type: boxes wider than half the
        // world may cross the antimeridian and go to the geometry test
        let wide = |b: &Rect| b.x1 - b.x0 > 0x8000_0000;
        let grow = |r: &Rect| Rect { y0: r.y0.saturating_sub(1), x0: r.x0.saturating_sub(1), y1: r.y1.saturating_add(1), x1: r.x1.saturating_add(1) };
        let meets = |b: &Rect, rs: &[Rect]| rs.iter().any(|x| grow(x).intersects(b));
        let near = |b: &Rect, test: &MemberTest, rs: &[Rect]| {
            wide(b)
                || match test {
                    MemberTest::Around(src, _) => src.near_box(rs, b),
                    MemberTest::Poly(_) => meets(b, rs),
                }
        };
        // A clause the box of the relation decides: passed or failed.
        let by_box = |r: &Relation, test: &MemberTest| -> Option<bool> {
            match test {
                MemberTest::Poly(p) if rel_boxes_on() && !wide(&r.bbox) => match p.rect_pos(&r.bbox) {
                    RectPos::Inside => Some(true),
                    RectPos::Apart => Some(false),
                    RectPos::Border => None,
                },
                _ => None,
            }
        };
        let open = |r: &Relation| member_tests.iter().any(|(test, _)| by_box(r, test).is_none());
        let keep = |r: &Relation| {
            r.spatial
                && !r.bbox.is_empty()
                && plan.tags.iter().all(|t| t.matches(&r.tags))
                && member_tests.iter().all(|(test, rs)| near(&r.bbox, test, rs) && by_box(r, test) != Some(false))
                && bbox.as_ref().is_none_or(|rs| wide(&r.bbox) || meets(&r.bbox, rs))
        };
        // Up to the estimate the candidates are kept whatever their
        // members. More are kept while they and the members they need cost
        // less than the nodes of the region, a candidate costing as a
        // member (the buildings or multipolygons of a city: 10^4 relations
        // with a few ways each); the reading stops once they cost more (the
        // thousands of routes near a city centre, 10^6 members). With
        // polygons only, members past that are picked out of the region,
        // and the reading stops at twice as many members as the region has
        // nodes: decoding a member costs a fraction of reading a node.
        let scan_ok = member_tests.iter().all(|(test, _)| matches!(test, MemberTest::Poly(_)));
        let over = |n: usize, members: usize| {
            n > MAX_CANDIDATES || if scan_ok { members > 2 * region_nodes } else { n > cap && (n + members) * MEMBER_COST > region_nodes }
        };
        let source = match pairs {
            Some(pairs) => Source::At(pairs),
            None => Source::Ranges(&ranges),
        };
        self.check()?;
        let Some(cands) = self.f.bounded::<Relation>(source, keep, |r| if open(r) { r.members.len() } else { 0 }, over)? else {
            #[cfg(test)]
            tests::CANDIDATES.set(Some("too_many_candidates"));
            return Ok(None);
        };
        let (mut nids, mut wids) = (Vec::new(), Vec::new());
        for r in cands.iter().filter(|r| open(r)) {
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
        let lookups = (nids.len() + wids.len()) * MEMBER_COST <= region_nodes;
        #[cfg(test)]
        let lookups = lookups && !tests::MEMBER_SCAN.get();
        if !lookups && !scan_ok {
            #[cfg(test)]
            tests::CANDIDATES.set(Some("too_many_members"));
            return Ok(None);
        }
        #[cfg(test)]
        tests::MEMBERS_FROM.set(if nids.is_empty() && wids.is_empty() { None } else if lookups { Some("ids") } else { Some("region") });
        let (positions, ways) = if lookups && !(nids.is_empty() && wids.is_empty()) { (self.f.node_positions(&nids)?, self.f.ways_untagged(&wids)?) } else { Default::default() };
        self.check()?;
        let mut pass = vec![true; cands.len()];
        for (test, rs) in member_tests {
            let decided: Vec<Option<bool>> = cands.iter().map(|r| by_box(r, test)).collect();
            if decided.iter().all(|d| d.is_some()) {
                for (p, d) in pass.iter_mut().zip(&decided) {
                    *p &= d.unwrap();
                }
                continue;
            }
            let (pass_nodes, pass_ways): (HashSet<Id>, HashSet<Id>) = match test {
                MemberTest::Around(src, r) => {
                    let nodes = positions.iter().filter(|(_, p)| around_match(src, &EGeom::Point(**p), *r)).map(|(id, _)| *id).collect();
                    let close: Vec<&Way> = ways.iter().filter(|w| !w.bbox.is_empty() && !w.missing && near(&w.bbox, test, rs)).collect();
                    let geoms = way_geoms(&self.f, &close)?;
                    (nodes, close.iter().zip(&geoms).filter(|(_, g)| around_match(src, g, *r)).map(|(w, _)| w.id).collect())
                }
                // the nodes and ways the member path reads: nodes in the
                // tiles of the rectangles, ways whose box meets them
                MemberTest::Poly(poly) if lookups => {
                    let tiles = ranges_for(Kind::Node, rs);
                    let nodes = positions.iter().filter(|(_, p)| cell_in_ranges(Cell::tile(tile_of(**p)), &tiles) && poly.contains_member(**p)).map(|(id, _)| *id).collect();
                    let close: Vec<&Way> = ways.iter().filter(|w| meets(&w.bbox, rs)).collect();
                    let pass = poly_member_ways(&self.f, poly, &close)?;
                    (nodes, close.iter().zip(pass).filter(|(_, p)| *p).map(|(w, _)| w.id).collect())
                }
                MemberTest::Poly(poly) => {
                    let want_nodes: HashSet<Id> = nids.iter().copied().collect();
                    let want_ways: HashSet<Id> = wids.iter().copied().collect();
                    let nodes = self.f.in_ranges_by::<Node>(&ranges_for(Kind::Node, rs), |v, p| codec::decode_nodes(v, p, false), |n| want_nodes.contains(&n.id) && poly.contains_member(n.pos))?;
                    let close = self.f.in_ranges_by::<Way>(&ranges_for(Kind::Way, rs), |v, p| codec::decode_ways(v, p, false), |w| want_ways.contains(&w.id) && meets(&w.bbox, rs))?;
                    self.check()?;
                    let refs: Vec<&Way> = close.iter().collect();
                    let pass = poly_member_ways(&self.f, poly, &refs)?;
                    (nodes.iter().map(|n| n.id).collect(), close.iter().zip(pass).filter(|(_, p)| *p).map(|(w, _)| w.id).collect())
                }
            };
            self.check()?;
            for ((p, rel), d) in pass.iter_mut().zip(&cands).zip(&decided) {
                *p &= match d {
                    Some(d) => *d,
                    None => rel.members.iter().any(|m| match m.mtype {
                        ElemType::Node => pass_nodes.contains(&m.id),
                        ElemType::Way => pass_ways.contains(&m.id),
                        _ => false,
                    }),
                };
            }
        }
        let mut i = 0;
        let mut cands = cands;
        cands.retain(|_| {
            i += 1;
            pass[i - 1]
        });
        Ok(Some(cands))
    }

    /// The filter to read the tag index for, when there is a `key=value`
    /// filter: the `key=value` filter or anchored regex (`anchored`) with
    /// the fewest index entries. All tag filters are checked on the
    /// candidates afterwards, so the choice only changes the cost:
    /// `["leisure"="park"]["name"="X"]` reads one entry instead of every
    /// park, `["boundary"="administrative"]["name"~"^X$"]` the names
    /// starting with X instead of every boundary. Counting stops past
    /// 20,000 entries or steps (`tag_count_capped`); on equal counts the
    /// earlier filter wins, a `key=value` filter before a regex.
    pub(super) fn most_selective<'p>(&self, kind: Kind, plan: &'p Plan, ranges: Option<&[CodeRange]>) -> Result<Option<&'p TagFilter>> {
        Ok(self.most_selective_counted(kind, plan, ranges)?.map(|(t, _)| t))
    }

    /// `most_selective` with the count of its filter (`count_cap()` + 1:
    /// more; 0 when there was nothing to compare).
    fn most_selective_counted<'p>(&self, kind: Kind, plan: &'p Plan, ranges: Option<&[CodeRange]>) -> Result<Option<(&'p TagFilter, usize)>> {
        let cap = count_cap();
        let mut eqs = plan.tags.iter().filter(|t| matches!(t, TagFilter::Eq(..)));
        let Some(first) = eqs.next() else { return Ok(None) };
        let rest: Vec<&TagFilter> = eqs.chain(plan.tags.iter().filter(|t| anchored(t))).collect();
        if rest.is_empty() {
            return Ok(Some((first, 0)));
        }
        let mut best = (self.f.tag_count_capped(kind, first, ranges, cap)?, first);
        for t in rest {
            if best.0 == 0 {
                break;
            }
            let n = self.f.tag_count_capped(kind, t, ranges, best.0.min(cap))?;
            if n < best.0 {
                best = (n, t);
            }
        }
        #[cfg(test)]
        tests::PROBE.set(Some(format!("{:?}", best.1)));
        Ok(Some((best.1, best.0)))
    }

    /// Candidates from the tag index when there is a `key=value` filter:
    /// the entries of the filter `most_selective` chooses, and the filters
    /// it read for them. Its counts stop at 20,000: when every filter has
    /// more, they are read in turn with a budget of index steps that grows
    /// eightfold from 160,000 to 1.3 x 10^6, and the first read to end
    /// gives the candidates (`way["highway"="residential"]["name"~
    /// "^Bahnhof"]`: the 4.6 x 10^4 names, not the 7 x 10^7 roads), else the
    /// filter `most_selective` chose. When those are many (over `NARROW`),
    /// each other `key=value` filter and anchored regex keeps only the
    /// candidates it lists too (`tag_intersect`): `way["boundary"=
    /// "administrative"]["name"~"^Z"]` reads 1.4 x 10^6 boundary ways and
    /// 3.6 x 10^5 names from the index and decodes the 700 in both. A
    /// filter is read while it takes at most `STEPS` index steps per
    /// candidate: decoding a candidate costs more than that.
    pub(super) fn indexed_candidates<'p>(&self, kind: Kind, plan: &'p Plan, ranges: Option<&[CodeRange]>) -> Result<Option<(Vec<(Cell, Id)>, Vec<&'p TagFilter>)>> {
        const NARROW: usize = 20_000;
        const STEPS: usize = 4;

        #[cfg(not(test))]
        let narrow = NARROW;
        #[cfg(test)]
        let narrow = tests::NARROW.get().unwrap_or(NARROW);
        let Some((probe, count)) = self.most_selective_counted(kind, plan, ranges)? else { return Ok(None) };
        let indexed: Vec<&TagFilter> = plan.tags.iter().filter(|t| matches!(t, TagFilter::Eq(..)) || anchored(t)).collect();
        let mut first = None;
        if count > count_cap() {
            'budgets: for budget in [8 * count_cap(), 64 * count_cap()] {
                for &t in std::iter::once(&probe).chain(indexed.iter().filter(|t| !std::ptr::eq(**t, probe))) {
                    self.check()?;
                    if let Some(pairs) = self.f.tag_candidates_bounded(kind, t, ranges, budget)? {
                        first = Some((pairs, t));
                        break 'budgets;
                    }
                }
            }
        }
        let (mut pairs, probe) = match first {
            Some(f) => f,
            None => (self.f.tag_candidates(kind, probe, ranges)?, probe),
        };
        let mut read = vec![probe];
        for &t in &indexed {
            if pairs.len() <= narrow {
                break;
            }
            if std::ptr::eq(t, probe) {
                continue;
            }
            self.check()?;
            if let Some(both) = self.f.tag_intersect(kind, t, ranges, &pairs, pairs.len().saturating_mul(STEPS))? {
                pairs = both;
                read.push(t);
            }
        }
        #[cfg(test)]
        tests::READ.set(tests::READ.get().max(read.len()));
        #[cfg(test)]
        tests::FIRST.set(Some(format!("{probe:?}")));
        Ok(Some((pairs, read)))
    }

    /// The elements at candidate pairs from the tag index, which `read`
    /// selected. The other tag filters of the plan are tested on the tags
    /// before an element is decoded whole, and a chunk where a value they
    /// need does not occur is not read further (`value_literals`):
    /// `area["name"~"x"]["boundary"="administrative"]` decodes the closed
    /// ways among 1.4 x 10^6 boundary ways only for the few with such a
    /// name, and reads the tags only in the chunks with the name in them.
    pub(super) fn at_tagged<T: Stored>(&self, plan: &Plan, pairs: Vec<(Cell, Id)>, read: &[&TagFilter]) -> Result<Vec<T>> {
        let rest: Vec<&TagFilter> = plan.tags.iter().filter(|t| !read.iter().any(|r| std::ptr::eq(*r, *t))).collect();
        #[cfg(test)]
        tests::TESTED_FIRST.set(rest.len());
        if rest.is_empty() {
            return self.f.at::<T>(pairs);
        }
        let needs = rest.iter().filter_map(|t| t.value_literals()).map(|lits| literal_search(&lits)).collect::<Result<Vec<_>>>()?;
        let chunk = |data: &[u8]| needs.iter().all(|re| re.is_match(data));
        let keep = |tags: &Tags| rest.iter().all(|t| t.matches(tags));
        self.f.at_where::<T>(pairs, Some(TagTest { chunk: &chunk, keep: &keep }))
    }

    fn negated_tags_exist(&self, kind: Kind, plan: &Plan) -> Result<bool> {
        for t in &plan.tags {
            let probe = match t {
                TagFilter::Ne(k, v) => TagFilter::Eq(k.clone(), v.clone()),
                TagFilter::NotKey(k) => TagFilter::HasKey(k.clone()),
                TagFilter::Regex { key, re, neg: true, prefixes, literals } => TagFilter::Regex { key: key.clone(), re: re.clone(), neg: false, prefixes: prefixes.clone(), literals: literals.clone() },
                _ => continue,
            };
            if self.f.tag_exists(kind, &probe)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn abort(&self, msg: impl Into<String>) -> anyhow::Error {
        Abort(msg.into()).into()
    }
}

/// Where `most_selective` stops counting the entries of a filter: 20,000.
fn count_cap() -> usize {
    #[cfg(test)]
    if let Some(c) = tests::COUNT_CAP.get() {
        return c;
    }
    20_000
}

/// A search for any of the byte strings.
fn literal_search(lits: &[&[u8]]) -> Result<regex::bytes::Regex> {
    let alts: Vec<String> = lits.iter().map(|l| l.iter().map(|b| format!("\\x{b:02x}")).collect()).collect();
    Ok(regex::bytes::RegexBuilder::new(&alts.join("|")).unicode(false).build()?)
}

/// A regex filter with literal prefixes (`^X`): the tag index has its
/// candidates under the values with those prefixes.
pub(super) fn anchored(t: &TagFilter) -> bool {
    matches!(t, TagFilter::Regex { neg: false, prefixes: Some(_), .. })
}

/// A spatial clause of a relation statement as `rel_spatial_by_members`
/// tests it on the member nodes and ways.
enum MemberTest<'a> {
    Around(&'a AroundSource, f64),
    Poly(PolyTest),
}

/// The member way test of `poly` on relations (C++ `add_border = false`);
/// ways with a missing node never match.
fn poly_member_way(poly: &PolyTest, v: &[Option<crate::geo::Pt>]) -> bool {
    if v.is_empty() || v.iter().any(|p| p.is_none()) {
        return false;
    }
    let pts: Vec<crate::geo::Pt> = v.iter().flatten().copied().collect();
    if pts.len() == 1 { poly.contains_pt(pts[0]) } else { poly.matches_line(&pts, false) }
}

/// `poly_member_way` for each of the ways. A way with its box strictly
/// inside the polygon passes and one with its box apart from it fails
/// without its vertices; only those of the others are looked up. In a city
/// polygon most ways are inside or apart: the vertices of all would be
/// 10^6 node lookups.
fn poly_member_ways(f: &Fetch, poly: &PolyTest, ways: &[&Way]) -> Result<Vec<bool>> {
    let mut pass = vec![false; ways.len()];
    let mut border = Vec::new();
    let boxes = way_boxes_on();
    for (i, w) in ways.iter().enumerate() {
        match if boxes { poly.rect_pos(&w.bbox) } else { RectPos::Border } {
            RectPos::Inside if !w.missing && !w.nodes.is_empty() => pass[i] = true,
            RectPos::Apart => {}
            _ => border.push(i),
        }
    }
    let refs: Vec<&Way> = border.iter().map(|&i| ways[i]).collect();
    let verts = super::geom::way_vertices(f, &refs)?;
    for i in border {
        pass[i] = poly_member_way(poly, &verts[&ways[i].id]);
    }
    Ok(pass)
}

/// Whether the way tests of `poly` on relations decide by the box of a way
/// where they can (`poly_member_ways`); off in tests to compare.
fn way_boxes_on() -> bool {
    #[cfg(test)]
    return !tests::WAY_BOXES_OFF.get();
    #[cfg(not(test))]
    true
}

/// Whether `rel_spatial_by_candidates` decides relations by their box
/// where it can; off in tests to compare.
fn rel_boxes_on() -> bool {
    #[cfg(test)]
    return !tests::REL_BOXES_OFF.get();
    #[cfg(not(test))]
    true
}

/// Whether relation geometry for the spatial tests may leave out member
/// ways far from every region (not for polygons).
fn reduce_rel_geoms(plan: &Plan) -> bool {
    #[cfg(test)]
    if tests::NEAR_GEOMS_OFF.get() {
        return false;
    }
    plan.polys.is_empty()
}

/// Cost of reading a region, for choosing where to read: the level-16
/// tiles of its rectangles (see `tile_runs`: those of small rectangles
/// count once, larger ones count whole), and a sixteenth of a tile for
/// each rectangle, which its ranges cost to prepare. Many small rectangles
/// along a long `around` source (a river with radius 0) cover more tiles
/// than their area says, and those of a cluster of points share tiles.
fn region_cost(rects: &[Rect]) -> u64 {
    let tiles: u64 = tile_runs(rects).iter().map(|r| ((r.y1 >> 16) - (r.y0 >> 16) + 1) as u64 * ((r.x1 >> 16) - (r.x0 >> 16) + 1) as u64).sum();
    tiles + rects.len() as u64 / 16
}

/// `around` test per spec §6.
pub fn around_match(src: &AroundSource, g: &EGeom, r: f64) -> bool {
    use crate::geo::{Segment, cartesian, great_circle_dist};
    let np = src.points.len();
    let mut cand: Vec<u32> = Vec::new();
    let point_inside = |lat: f64, lon: f64, cand: &mut Vec<u32>| -> bool {
        let cp = cartesian(lat, lon);
        let near_point = |c: &(f64, f64)| (r > 0.0 && great_circle_dist(c.0, c.1, lat, lon) <= r) || ((c.0 - lat).abs() < 1e-7 && (c.1 - lon).abs() < 1e-7);
        let near_segment = |s: &Segment| {
            s.line_dist(&cp) <= r && {
                let ab = great_circle_dist(s.a.0, s.a.1, s.b.0, s.b.1);
                let lim = (ab * ab + r * r).sqrt();
                great_circle_dist(lat, lon, s.a.0, s.a.1) <= lim && great_circle_dist(lat, lon, s.b.0, s.b.1) <= lim
            }
        };
        match &src.index {
            None => src.points.iter().any(near_point) || src.segments.iter().any(near_segment),
            Some(ix) => {
                ix.at_point(lat, lon, cand);
                cand.iter().any(|&i| if (i as usize) < np { near_point(&src.points[i as usize]) } else { near_segment(&src.segments[i as usize - np]) })
            }
        }
    };
    let seg_inside = |a: (f64, f64), b: (f64, f64), cand: &mut Vec<u32>| -> bool {
        let seg = Segment::new(a.0, a.1, b.0, b.1);
        let ab = great_circle_dist(a.0, a.1, b.0, b.1);
        let lim = (ab * ab + r * r).sqrt();
        let near_point = |c: &(f64, f64)| seg.line_dist(&cartesian(c.0, c.1)) <= r && great_circle_dist(c.0, c.1, a.0, a.1) <= lim && great_circle_dist(c.0, c.1, b.0, b.1) <= lim;
        match &src.index {
            Some(ix) if ix.near_segment(&seg, cand) => {
                cand.iter().any(|&i| if (i as usize) < np { near_point(&src.points[i as usize]) } else { src.segments[i as usize - np].intersects(&seg) })
            }
            _ => src.points.iter().any(near_point) || src.segments.iter().any(|t| t.intersects(&seg)),
        }
    };
    let way_match = |v: &[Option<crate::geo::Pt>], cand: &mut Vec<u32>| -> bool {
        if v.is_empty() || v.iter().any(|p| p.is_none()) {
            return false;
        }
        let pts: Vec<(f64, f64)> = v.iter().flatten().map(|p| (p.lat(), p.lon())).collect();
        if pts.iter().any(|p| point_inside(p.0, p.1, cand)) {
            return true;
        }
        pts.windows(2).any(|w| seg_inside(w[0], w[1], cand))
    };
    match g {
        EGeom::Point(p) => point_inside(p.lat(), p.lon(), &mut cand),
        EGeom::Line(v) => way_match(v, &mut cand),
        EGeom::Multi { points, lines } => points.iter().any(|p| point_inside(p.lat(), p.lon(), &mut cand)) || lines.iter().any(|l| way_match(l, &mut cand)),
        EGeom::Empty => false,
    }
}

fn poly_match(p: &Polygon, g: &EGeom) -> bool {
    match g {
        _ => p.matches(g),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::Pt;
    use std::cell::Cell;

    thread_local! {
        /// Turns off the candidate path of relation `around` on this thread.
        pub(super) static CANDIDATES_OFF: Cell<bool> = const { Cell::new(false) };
        /// What the candidate path did in the last query: answered, or why
        /// it left the query to the member path; None when not tried.
        pub(super) static CANDIDATES: Cell<Option<&'static str>> = const { Cell::new(None) };
        /// Where the candidate path started in the last query: "tags" or "bbox".
        pub(super) static CANDIDATES_FROM: Cell<Option<&'static str>> = const { Cell::new(None) };
        /// The clause whose region the last element query read: bbox,
        /// poly, around or area.
        pub(super) static REGION: Cell<Option<&'static str>> = const { Cell::new(None) };
        /// Turns off the reduced relation geometry (`rel_geoms_near`).
        pub(super) static NEAR_GEOMS_OFF: Cell<bool> = const { Cell::new(false) };
        /// The filter `most_selective` chose last, when it compared filters.
        pub(super) static PROBE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
        /// Turns off the way box tests of `poly` on relations (`way_boxes_on`).
        pub(super) static WAY_BOXES_OFF: Cell<bool> = const { Cell::new(false) };
        /// Turns off the relation box tests of `poly` (`rel_boxes_on`).
        pub(super) static REL_BOXES_OFF: Cell<bool> = const { Cell::new(false) };
        /// Makes the candidate path pick the members of polygon clauses
        /// out of the region.
        pub(super) static MEMBER_SCAN: Cell<bool> = const { Cell::new(false) };
        /// Where the candidate path took the members it tested: "ids" or
        /// "region"; None when the boxes decided every relation.
        pub(super) static MEMBERS_FROM: Cell<Option<&'static str>> = const { Cell::new(None) };
        /// Whether the last element query with a region and without a
        /// `key=value` filter took its candidates from the tag index.
        pub(super) static INDEXED: Cell<bool> = const { Cell::new(false) };
        /// Index steps an anchored regex may take in a small region, in
        /// place of the chunk count of the region.
        pub(super) static INDEX_STEPS: Cell<Option<usize>> = const { Cell::new(None) };
        /// The most filters that gave the candidates of an element query
        /// with a `key=value` filter since it was last reset
        /// (`indexed_candidates`).
        pub(super) static READ: Cell<usize> = const { Cell::new(0) };
        /// Candidates over which `indexed_candidates` reads the other
        /// filters, in place of 20,000.
        pub(super) static NARROW: Cell<Option<usize>> = const { Cell::new(None) };
        /// Where `most_selective` stops counting, in place of 20,000.
        pub(super) static COUNT_CAP: Cell<Option<usize>> = const { Cell::new(None) };
        /// The filter whose candidates `indexed_candidates` read first,
        /// last time.
        pub(super) static FIRST: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
        /// How many tag filters the last decoding of index candidates
        /// tested before decoding an element whole (`at_tagged`).
        pub(super) static TESTED_FIRST: Cell<usize> = const { Cell::new(0) };
    }

    /// A 30 x 30 grid of nodes 0.002 degrees apart in Paris (node 466 at
    /// 48.85, 2.35), 40,000 untagged nodes 0.0001 degrees apart around the
    /// centre (the density of a city, which the cost estimate assumes), a
    /// way along each row (1000 + row) and column (2000 + column), way 3000
    /// (466, 467), way 3001 with a missing node, and the given relations,
    /// as OSM XML.
    fn grid_osm(relations: &str) -> String {
        grid_osm_with("", "", relations)
    }

    /// `grid_osm` with more nodes (ids from 200,000) and ways (from 4000).
    fn grid_osm_with(nodes: &str, ways: &str, relations: &str) -> String {
        let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osm version=\"0.6\">\n");
        let m = r#"version="1" timestamp="2020-01-01T00:00:00Z" uid="1" user="a" changeset="1""#;
        let id = |i: usize, j: usize| i * 30 + j + 1;
        for i in 0..30 {
            for j in 0..30 {
                x += &format!("<node id=\"{}\" lat=\"{:.3}\" lon=\"{:.3}\" {m}/>\n", id(i, j), 48.82 + i as f64 * 0.002, 2.32 + j as f64 * 0.002);
            }
        }
        for i in 0..200 {
            for j in 0..200 {
                x += &format!("<node id=\"{}\" lat=\"{:.5}\" lon=\"{:.5}\" {m}/>\n", 100_000 + i * 200 + j, 48.84005 + i as f64 * 0.0001, 2.34005 + j as f64 * 0.0001);
            }
        }
        x += &nodes.replace("{m}", m);
        let way = |wid: usize, nodes: &[usize]| {
            let nds: String = nodes.iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect();
            format!("<way id=\"{wid}\" {m}>{nds}<tag k=\"highway\" v=\"residential\"/></way>\n")
        };
        for i in 0..30 {
            x += &way(1000 + i, &(0..30).map(|j| id(i, j)).collect::<Vec<_>>());
        }
        for j in 0..30 {
            x += &way(2000 + j, &(0..30).map(|i| id(i, j)).collect::<Vec<_>>());
        }
        x += &way(3000, &[466, 467]);
        x += &way(3001, &[466, 999_999]);
        x += &ways.replace("{m}", m);
        x += &relations.replace("{m}", m);
        x += "</osm>\n";
        x
    }

    fn relation(id: usize, members: &[(&str, usize)], tags: &[(&str, &str)]) -> String {
        let ms: String = members.iter().map(|(t, r)| format!("<member type=\"{t}\" ref=\"{r}\" role=\"\"/>")).collect();
        let ts: String = tags.iter().map(|(k, v)| format!("<tag k=\"{k}\" v=\"{v}\"/>")).collect();
        format!("<relation id=\"{id}\" {{m}}>{ms}{ts}</relation>\n")
    }

    fn grid_db(name: &str, relations: &str) -> (crate::store::Db, std::path::PathBuf) {
        osm_db(name, &grid_osm(relations))
    }

    fn osm_db(name: &str, osm_xml: &str) -> (crate::store::Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("overpass-relaround-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let osm = dir.join("grid.osm");
        std::fs::write(&osm, osm_xml).unwrap();
        let db = crate::store::Db::open(&dir.join("db")).unwrap();
        let mut report = crate::import::ImportReport::default();
        crate::import::import_file(&db, &osm, &mut report).unwrap();
        crate::import::set_base_timestamp(&db, Some("test"), &report).unwrap();
        (db, dir)
    }

    /// Output of the query with the candidate path on or off, and what the
    /// candidate path did.
    fn run_q(db: &crate::store::Db, q: &str, off: bool) -> (String, Option<&'static str>) {
        CANDIDATES_OFF.set(off);
        CANDIDATES.set(None);
        CANDIDATES_FROM.set(None);
        let out = output(db, q);
        CANDIDATES_OFF.set(false);
        (out, CANDIDATES.get())
    }

    /// Output of the query with the hooks as set on this thread.
    fn output(db: &crate::store::Db, q: &str) -> String {
        let opts = crate::exec::RunOptions { cli: true, ..Default::default() };
        let out = match crate::exec::run(db, q, Vec::new(), &opts) {
            Ok(p) => p.w,
            Err((Some(p), _)) => p.w,
            Err((None, e)) => panic!("{q}: {e}"),
        };
        String::from_utf8(out).unwrap()
    }

    /// Runs the query both ways: the outputs must be equal; returns the
    /// output (ids for csv queries) and what the candidate path did.
    fn both_ways(db: &crate::store::Db, q: &str) -> (Vec<String>, Option<&'static str>) {
        let (members, _) = run_q(db, q, true);
        let (cands, what) = run_q(db, q, false);
        assert_eq!(cands, members, "{q}");
        (cands.lines().map(String::from).collect(), what)
    }

    /// The candidate path of relation `around` gives the member path's
    /// answers, and it is used where it should be.
    #[test]
    fn rel_around_candidates_match_member_path() {
        let rels = [
            relation(1, &[("way", 1015)], &[("route", "bus")]),
            relation(2, &[("way", 1000)], &[("route", "bus")]),
            relation(3, &[("node", 466)], &[("route", "tram")]),
            relation(4, &[("way", 3001), ("node", 1)], &[("route", "bus")]),
            relation(5, &[("relation", 1)], &[("route", "bus")]),
            relation(6, &[("node", 999_998), ("way", 1029)], &[("route", "bus")]),
            // box over the centre, members 3 km away
            relation(7, &[("way", 1000), ("way", 1029)], &[("route", "bus")]),
            relation(8, &[("way", 3000)], &[("type", "multipolygon")]),
            relation(9, &[("way", 2015), ("way", 1000)], &[("route", "bus")]),
            // 586 m east of the centre
            relation(10, &[("node", 470)], &[("route", "bus")]),
            relation(11, &[("node", 466)], &[("route", "bus"), ("name", "X")]),
        ]
        .concat();
        let (db, dir) = grid_db("match", &rels);
        let c = "[out:csv(::id;false)];";
        // query, what the candidate path does (None: not checked), ids
        type Case<'a> = (&'a str, Option<&'a str>, Option<&'a [&'a str]>);
        let cases: &[Case] = &[
            ("relation[\"route\"=\"bus\"](around:500,48.85,2.35);out ids;", Some("answered"), Some(&["1", "9", "11"])),
            ("relation[\"route\"~\"^(bus|tram)$\"](around:500,48.85,2.35);out ids;", Some("answered"), Some(&["1", "3", "9", "11"])),
            ("relation[route](around:600,48.85,2.35);out ids;", Some("answered"), Some(&["1", "3", "9", "10", "11"])),
            ("relation[\"route\"=\"bus\"][\"name\"=\"X\"](around:500,48.85,2.35);out ids;", Some("answered"), Some(&["11"])),
            ("relation[\"route\"=\"bus\"](around:500,48.85,2.35)(around:300,48.851,2.351);out ids;", Some("answered"), None),
            ("way(1015)->.w;relation[\"route\"=\"bus\"](around.w:50);out ids;", Some("answered"), None),
            // 10 relations against a 100 m circle: either path
            ("node(466);relation[route](around:100);out ids;", None, Some(&["1", "3", "9", "11"])),
            ("relation[\"route\"=\"bus\"](around:0,48.85,2.35);out ids;", None, Some(&["1", "9", "11"])),
            // no tag filter the index can use: the relations of the cells
            ("relation(around:500,48.85,2.35);out ids;", Some("answered"), Some(&["1", "3", "8", "9", "11"])),
            ("relation[!\"route\"](around:500,48.85,2.35);out ids;", Some("answered"), Some(&["8"])),
        ];
        for (q, what, ids) in cases {
            let q = format!("{c}{q}");
            let (out, did) = both_ways(&db, &q);
            if let Some(w) = what {
                assert_eq!(did, Some(*w), "{q}");
            }
            if let Some(ids) = ids {
                assert_eq!(out, ids.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "{q}");
            }
        }
        // With a polygon too; not tried for regions over MAX_REGION.
        let q = "relation[\"route\"=\"bus\"](poly:\"48.84 2.34 48.86 2.34 48.86 2.36\")(around:500,48.85,2.35);out ids;";
        assert_eq!(both_ways(&db, &format!("{c}{q}")).1, Some("answered"), "{q}");
        let q = "relation[\"route\"=\"bus\"](around:20000,48.85,2.35);out ids;";
        assert_eq!(both_ways(&db, &format!("{c}{q}")).1, None, "{q}");
        // Full output with geometry and tags.
        both_ways(&db, "relation[\"route\"=\"bus\"](around:500,48.85,2.35);out geom;");
        both_ways(&db, "[out:json];relation[route](around:600,48.85,2.35);out geom(48.845,2.345,48.855,2.355);");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// More relations with the tag near a small region than the estimate of
    /// 50 members each allows: they answer while they and their members
    /// cost less than the nodes of the region, else the member path does.
    #[test]
    fn rel_around_many_candidates_use_member_path() {
        // nodes 466 (the centre) and 467 (147 m east), in one or two tiles
        let few: String = (0..40).map(|k| relation(100 + k, &[("node", 466 + k % 2)], &[("route", "bus")])).collect();
        // and the 900 nodes of the grid each
        let many: String = (0..40)
            .map(|k| {
                let mut members = vec![("node", 466 + k % 2)];
                members.extend((1..=900).map(|n| ("node", n)));
                relation(100 + k, &members, &[("route", "bus")])
            })
            .collect();
        for (name, rels, what) in [("many-few", few, "answered"), ("many-members", many, "too_many_candidates")] {
            let (db, dir) = grid_db(name, &rels);
            let (out, did) = both_ways(&db, "[out:csv(::id;false)];relation[\"route\"=\"bus\"](around:50,48.85,2.35);out ids;");
            assert_eq!((did, CANDIDATES_FROM.get()), (Some(what), Some("tags")), "{name}");
            assert_eq!(out.len(), if name == "many-few" { 20 } else { 40 }, "{out:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A relation with more members than the region has nodes (a long
    /// route near a small circle): the member path answers.
    #[test]
    fn rel_around_many_members_use_member_path() {
        let mut members: Vec<(&str, usize)> = (1..=900).map(|n| ("node", n)).collect();
        members.extend((0..30).map(|i| ("way", 1000 + i)));
        members.extend((0..30).map(|j| ("way", 2000 + j)));
        let (db, dir) = grid_db("members", &relation(1, &members, &[("route", "bus")]));
        let (out, did) = both_ways(&db, "[out:csv(::id;false)];relation[\"route\"=\"bus\"](around:30,48.85,2.35);out ids;");
        assert_eq!(did, Some("too_many_members"));
        assert_eq!(out, vec!["1".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bbox clause over fewer tiles than an `around` region is the region
    /// read, also when the around rectangles have less area (a line with
    /// radius 0), with the answers of reading the around region; a cluster
    /// of points in fewer tiles than the bbox is read instead.
    #[test]
    fn around_with_bbox_reads_the_smaller_region() {
        let rels = [relation(1, &[("node", 3)], &[("waterway", "weir")]), relation(2, &[("way", 2002)], &[("waterway", "dam")])].concat();
        let (db, dir) = grid_db("region", &rels);
        let c = "[out:csv(::type,::id;false)];";
        let cb = "[out:csv(::type,::id;false)][bbox:48.819,2.323,48.821,2.327];";
        // way 1000 runs along 48.82 from 2.32 to 2.378 (ten tiles); the
        // small bbox holds two of its nodes, the large one the whole grid
        let (small, large) = ("(48.819,2.323,48.821,2.327)", "(48.80,2.30,48.90,2.40)");
        // query, region read, the same query reading the around region first
        let cases = [
            (format!("{c}way(1000)->.w;nwr(around.w:0){small};out ids;"), "bbox", format!("way(1000)->.w;nwr(around.w:0)->.a;nwr.a{small};out ids;")),
            (format!("{c}way(1000)->.w;way(around.w:20){small};out ids;"), "bbox", format!("way(1000)->.w;way(around.w:20)->.a;way.a{small};out ids;")),
            (format!("{c}way(1000)->.w;way(around.w:0){large};out ids;"), "around", format!("way(1000)->.w;way(around.w:0)->.a;way.a{large};out ids;")),
            (format!("{c}node(466)->.p;way(around.p:10){large};out ids;"), "around", format!("node(466)->.p;way(around.p:10)->.a;way.a{large};out ids;")),
            (format!("{cb}way(1000)->.w;way(around.w:0);out ids;"), "bbox", format!("way(1000)->.w;way(around.w:0)->.a;way.a{small};out ids;")),
            // 400 nodes of the dense centre: rectangles over about 1600
            // tiles one by one, 9 together
            (format!("{c}node(48.849,2.349,48.851,2.351)->.p;way(around.p:200){large};out ids;"), "around", format!("node(48.849,2.349,48.851,2.351)->.p;way(around.p:200)->.a;way.a{large};out ids;")),
        ];
        for (q, region, reference) in &cases {
            REGION.set(None);
            let out = output(&db, q);
            assert_eq!(REGION.get(), Some(*region), "{q}");
            assert_eq!(out, output(&db, &format!("{c}{reference}")), "{q}");
            assert!(!out.is_empty(), "{q}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With a bbox clause over fewer tiles than the `around` region, the
    /// candidate path starts from the bbox and gives the member path's
    /// answers; too many relations or members leave the query to the member
    /// path.
    #[test]
    fn rel_around_candidates_from_bbox() {
        // Way 1015 runs along 48.85 from 2.32 to 2.378, through the dense
        // centre; the bbox holds its nodes 453 and 454 (2.324, 2.326),
        // where ways 2002 and 2003 cross it.
        let rels = [
            relation(1, &[("way", 2002)], &[("waterway", "dam")]),
            relation(2, &[("node", 453)], &[("waterway", "weir")]),
            // on the line, outside the bbox
            relation(3, &[("node", 466)], &[("waterway", "dam")]),
            relation(4, &[("way", 1000), ("way", 2003)], &[("bridge", "yes")]),
            relation(5, &[("way", 3001)], &[("waterway", "dam")]),
            relation(6, &[("way", 2029)], &[("waterway", "dam")]),
            relation(7, &[("way", 2002), ("way", 1029)], &[("name", "X")]),
        ]
        .concat();
        let (db, dir) = grid_db("frombbox", &rels);
        let c = "[out:csv(::id;false)];way(1015)->.w;";
        let bbox = "(48.845,2.321,48.855,2.327)";
        // query, what the candidate path does, where it starts, ids
        type Case = (String, &'static str, Option<&'static str>, Option<&'static [&'static str]>);
        let cases: Vec<Case> = vec![
            (format!("rel(around.w:0){bbox};out ids;"), "answered", Some("bbox"), Some(&["1", "2", "4", "7"])),
            (format!("rel(around.w:0){bbox}[~\"^(bridge|waterway)$\"~\"dam|weir|yes\"];out ids;"), "answered", Some("bbox"), Some(&["1", "2", "4"])),
            (format!("rel(around.w:0){bbox}[waterway!=weir][!name];out ids;"), "answered", Some("bbox"), Some(&["1", "4"])),
            (format!("rel(around.w:50){bbox}[waterway=dam];out ids;"), "answered", Some("tags"), Some(&["1"])),
            (format!("rel(around.w:0)(around.w:30){bbox};out ids;"), "answered", Some("bbox"), Some(&["1", "2", "4", "7"])),
            ("[bbox:48.845,2.321,48.855,2.327];way(1015)->.w;rel(around.w:0);out ids;".to_string(), "answered", Some("bbox"), Some(&["1", "2", "4", "7"])),
            (format!("rel(around.w:0){bbox}[name=Y];out ids;"), "answered", Some("tags"), Some(&[])),
            // the bbox has more tiles than the line
            ("rel(around.w:0)(48.80,2.30,48.90,2.40);out ids;".to_string(), "answered", Some("cells"), Some(&["1", "2", "3", "4", "6", "7"])),
        ];
        for (q, what, from, ids) in &cases {
            let q = if q.starts_with('[') { format!("[out:csv(::id;false)]{q}") } else { format!("{c}{q}") };
            let (out, did) = both_ways(&db, &q);
            assert_eq!(did, Some(*what), "{q}");
            assert_eq!(CANDIDATES_FROM.get(), *from, "{q}");
            if let Some(ids) = ids {
                assert_eq!(out, ids.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "{q}");
            }
        }
        both_ways(&db, &format!("{c}rel(around.w:0){bbox};out geom;"));
        let _ = std::fs::remove_dir_all(&dir);
        // Fallbacks: more relations in the bbox than the line's nodes would
        // cost, and a relation with more members.
        let many: String = (0..2000).map(|k| relation(100 + k, &[("node", 453 + k % 2)], &[])).collect();
        let (db, dir) = grid_db("frombbox-many", &many);
        let (out, did) = both_ways(&db, &format!("{c}rel(around.w:0){bbox};out ids;"));
        assert_eq!((did, CANDIDATES_FROM.get()), (Some("too_many_candidates"), Some("bbox")));
        assert_eq!(out.len(), 2000);
        let _ = std::fs::remove_dir_all(&dir);
        let mut members: Vec<(&str, usize)> = (1..=900).map(|n| ("node", n)).collect();
        members.extend((100_000..120_000).map(|n| ("node", n)));
        members.extend((0..30).map(|j| ("way", 2000 + j)));
        let (db, dir) = grid_db("frombbox-members", &relation(1, &members, &[]));
        let (out, did) = both_ways(&db, &format!("{c}rel(around.w:0){bbox};out ids;"));
        assert_eq!((did, CANDIDATES_FROM.get()), (Some("too_many_members"), Some("bbox")));
        assert_eq!(out, vec!["1".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Relation geometry without the member ways far from every region
    /// gives the answers of the full geometry: ways near Orléans, 110 km
    /// from the grid, one across the antimeridian, missing ways and nodes.
    #[test]
    fn rel_geoms_near_match_full_geometry() {
        let m = "{m}";
        let mut nodes = String::new();
        for k in 0..10 {
            nodes += &format!("<node id=\"{}\" lat=\"{:.3}\" lon=\"1.900\" {m}/>\n", 200_001 + k, 47.9 + k as f64 * 0.001);
        }
        nodes += &format!("<node id=\"200011\" lat=\"10.0\" lon=\"179.999\" {m}/>\n<node id=\"200012\" lat=\"10.0\" lon=\"-179.999\" {m}/>\n");
        nodes += &format!("<node id=\"200013\" lat=\"47.9\" lon=\"1.95\" {m}/>\n");
        let way = |id: usize, nds: &[usize]| format!("<way id=\"{id}\" {m}>{}</way>\n", nds.iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect::<String>());
        let ways = [way(4000, &[200_001, 200_002, 200_003, 200_004, 200_005]), way(4001, &[200_006, 200_007, 200_008, 200_009, 200_010]), way(4002, &[200_011, 200_012])].concat();
        let rels = [
            relation(21, &[("way", 1000), ("way", 4000)], &[("route", "bus")]),
            relation(22, &[("way", 4000), ("node", 200_013)], &[("route", "bus")]),
            relation(23, &[("way", 4002), ("way", 1015)], &[("route", "ferry")]),
            relation(24, &[("way", 4002)], &[("route", "ferry")]),
            relation(25, &[("way", 3001), ("way", 4001)], &[]),
            relation(26, &[("node", 466), ("way", 4001)], &[]),
            relation(27, &[("relation", 21), ("way", 4001)], &[]),
            relation(28, &[("way", 999_000), ("node", 1)], &[]),
        ]
        .concat();
        let (db, dir) = osm_db("near-geoms", &grid_osm_with(&nodes, &ways, &rels));
        let (paris, orleans) = ("(around:20000,48.85,2.35)", "(47.85,1.85,47.95,1.96)");
        // query, ids (None: not checked)
        let cases: &[(String, Option<&[&str]>)] = &[
            (format!("rel{paris};out ids;"), Some(&["21", "23", "26", "28"])),
            (format!("rel{paris}{orleans};out ids;"), Some(&["21", "26"])),
            ("rel(around:20000,47.9,1.9);out ids;".to_string(), Some(&["21", "22", "25", "26", "27"])),
            ("rel(around:20000,47.9,1.9)(48.80,2.30,48.90,2.40);out ids;".to_string(), Some(&["21", "26"])),
            ("way(1000)->.w;rel(around.w:20000);out ids;".to_string(), Some(&["21", "23", "26", "28"])),
            (format!("rel(48.80,2.30,48.90,2.40){orleans};out ids;"), Some(&["21", "26"])),
            ("rel(around:20000,10.0,179.99);out ids;".to_string(), Some(&["23", "24"])),
            (format!("rel[route=bus]{paris};out geom;"), None),
            (format!("rel{paris};out center;"), None),
            (format!("[bbox:47.85,1.85,47.95,1.96];rel{paris};out bb;"), None),
        ];
        let c = "[out:csv(::id;false)];";
        for (q, ids) in cases {
            let q = if ids.is_some() { format!("{c}{q}") } else { q.clone() };
            CANDIDATES.set(None);
            let reduced = output(&db, &q);
            assert_eq!(CANDIDATES.get(), None, "{q}");
            NEAR_GEOMS_OFF.set(true);
            let full = output(&db, &q);
            NEAR_GEOMS_OFF.set(false);
            assert_eq!(reduced, full, "{q}");
            if let Some(ids) = ids {
                assert_eq!(reduced.lines().collect::<Vec<_>>(), *ids, "{q}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The grid test of stored boxes never rejects a box that meets the
    /// rectangles of the source, and rejects boxes far from it.
    #[test]
    fn around_near_box_is_conservative() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let grow = |r: &Rect| Rect { y0: r.y0.saturating_sub(1), x0: r.x0.saturating_sub(1), y1: r.y1.saturating_add(1), x1: r.x1.saturating_add(1) };
        for (radius, spread, center_lon) in [(50.0, 0.05, 7.0), (0.0, 0.01, 7.0), (2000.0, 0.5, 7.0), (300.0, 0.2, 179.9)] {
            let lon = |r: f64| {
                let v: f64 = center_lon + (r - 0.5) * spread;
                if v > 180.0 { v - 360.0 } else { v }
            };
            let mut src = AroundSource::default();
            for _ in 0..60 {
                src.points.push((50.0 + (rnd() - 0.5) * spread, lon(rnd())));
            }
            for _ in 0..40 {
                let a = (50.0 + (rnd() - 0.5) * spread, lon(rnd()));
                let b = (a.0 + (rnd() - 0.5) * spread * 0.3, lon(rnd()));
                src.segments.push(crate::geo::Segment::new(a.0, a.1, b.0, b.1));
            }
            src.prepare(radius);
            let rects = src.bbox_rects(radius);
            let (mut met, mut rejected) = (0, 0);
            for k in 0..4000 {
                let size = spread * [0.0, 0.001, 0.01, 0.3][k % 4];
                let p = Pt::from_ll(50.0 + (rnd() - 0.5) * spread * 1.5, lon(rnd()));
                let q = Pt::from_ll((p.lat() + rnd() * size).min(89.0), lon(rnd()).clamp(p.lon(), p.lon() + size));
                let b = Rect { y0: p.y.min(q.y), x0: p.x.min(q.x), y1: p.y.max(q.y), x1: p.x.max(q.x) };
                if b.x1 - b.x0 > 0x8000_0000 {
                    continue;
                }
                let linear = rects.iter().any(|r| grow(r).intersects(&b));
                let near = src.near_box(&rects, &b);
                assert!(near || !linear, "{b:?} radius {radius}");
                met += linear as usize;
                rejected += !near as usize;
            }
            assert!(met > 0 && rejected > 0, "radius {radius}: {met} met, {rejected} rejected");
        }
    }

    /// The grid index must give the same answers as testing every item.
    #[test]
    fn around_index_matches_linear_scan() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        for (radius, spread, center_lon) in [(50.0, 0.05, 7.0), (2000.0, 0.5, 7.0), (0.0, 0.01, 7.0), (300.0, 0.2, 179.9), (5000.0, 2.0, 0.0)] {
            let mut lon = |r: f64| {
                let v: f64 = center_lon + (r - 0.5) * spread;
                if v > 180.0 { v - 360.0 } else { v }
            };
            let mut src = AroundSource::default();
            for _ in 0..60 {
                src.points.push((50.0 + (rnd() - 0.5) * spread, lon(rnd())));
            }
            for _ in 0..40 {
                let a = (50.0 + (rnd() - 0.5) * spread, lon(rnd()));
                let b = (a.0 + (rnd() - 0.5) * spread * 0.3, lon(rnd()));
                src.segments.push(crate::geo::Segment::new(a.0, a.1, b.0, b.1));
            }
            let plain = AroundSource { points: src.points.clone(), segments: src.segments.clone(), index: None };
            src.prepare(radius);
            assert!(src.index.is_some());
            let mut hits = 0;
            for _ in 0..400 {
                let p = Pt::from_ll(50.0 + (rnd() - 0.5) * spread * 1.2, lon(rnd()));
                let q = Pt::from_ll(50.0 + (rnd() - 0.5) * spread * 1.2, lon(rnd()));
                for g in [EGeom::Point(p), EGeom::Line(vec![Some(p), Some(q)])] {
                    let a = around_match(&plain, &g, radius);
                    assert_eq!(a, around_match(&src, &g, radius));
                    hits += a as usize;
                }
            }
            assert!(hits > 0);
        }
    }

    /// Tagged nodes in the dense centre of the grid (ids from 200,001; 20
    /// rows of 20, 0.0002 degrees apart), ways along the rows (ids from
    /// 4000), and 20,000 nodes with a note far away, so that the centre
    /// holds few of the chunks.
    fn tagged_grid(name: &str) -> (crate::store::Db, std::path::PathBuf) {
        let m = "{m}";
        let mut nodes = String::new();
        for k in 0..20_000 {
            nodes += &format!("<node id=\"{}\" lat=\"{:.4}\" lon=\"{:.4}\" {m}><tag k=\"note\" v=\"far away from the grid, number {k}\"/></node>\n", 300_000 + k, 10.0 + (k / 200) as f64 * 0.01, 10.0 + (k % 200) as f64 * 0.01);
        }
        for k in 0..400 {
            let place = match k % 40 {
                7 => "<tag k=\"place\" v=\"town\"/>",
                8 => "<tag k=\"place\" v=\"townland\"/>",
                9 => "<tag k=\"place\" v=\"village\"/>",
                _ => "",
            };
            let (lat, lon) = (48.84805 + (k / 20) as f64 * 0.0002, 2.34805 + (k % 20) as f64 * 0.0002);
            nodes += &format!("<node id=\"{}\" lat=\"{lat:.5}\" lon=\"{lon:.5}\" {m}><tag k=\"name\" v=\"{}{k}\"/>{place}</node>\n", 200_001 + k, ["Alpha", "alpha", "Beta", "Gamma"][k % 4]);
        }
        let mut ways = String::new();
        for i in 0..20 {
            let nds: String = (0..20).map(|j| format!("<nd ref=\"{}\"/>", 200_001 + i * 20 + j)).collect();
            ways += &format!("<way id=\"{}\" {m}>{nds}<tag k=\"highway\" v=\"{}\"/><tag k=\"name\" v=\"Way {i}\"/></way>\n", 4000 + i, ["primary", "secondary", "service"][i % 3]);
        }
        osm_db(name, &grid_osm_with(&nodes, &ways, ""))
    }

    /// An anchored regex in a region with few of the chunks reads the index
    /// entries of its prefixes in the region: the answers of decoding the
    /// region.
    #[test]
    fn anchored_regex_in_small_region_reads_the_index() {
        let (db, dir) = tagged_grid("anchored");
        let c = "[out:csv(::type,::id;false)];";
        // a hundredth of the nodes
        let bbox = "(48.849,2.349,48.851,2.351)";
        let cases = [
            format!("node[\"place\"~\"^(town)$\"]{bbox};out ids;"),
            format!("node[\"place\"~\"^town\"]{bbox};out ids;"),
            format!("node[\"name\"~\"^alpha1\",i]{bbox};out ids;"),
            format!("node[\"name\"~\"^(Beta|Gamma)\"][\"place\"]{bbox};out ids;"),
            format!("way[\"highway\"~\"^(primary|service)$\"]{bbox};out ids;"),
            format!("way[\"name\"~\"^Way 1\"]{bbox};out ids;"),
            format!("node[\"place\"~\"^(town|village)$\"](around:100,48.85,2.35);out ids;"),
        ];
        {
            let snap = db.snapshot().unwrap();
            let rects = BBox { south: 48.849, west: 2.349, north: 48.851, east: 2.351 }.rects();
            let (region, total) = Fetch::new(&snap).chunk_share(Kind::Node, &ranges_for(Kind::Node, &rects)).unwrap();
            assert!(region * 10 < total as usize, "{region} of {total} chunks");
        }
        for q in &cases {
            let q = format!("{c}{q}");
            INDEX_STEPS.set(Some(usize::MAX));
            INDEXED.set(false);
            let indexed = output(&db, &q);
            assert!(INDEXED.get(), "{q}");
            INDEX_STEPS.set(Some(0));
            let decoded = output(&db, &q);
            assert!(!INDEXED.get(), "{q}");
            INDEX_STEPS.set(None);
            assert_eq!(indexed, decoded, "{q}");
        }
        let towns = output(&db, &format!("{c}{}", cases[0]));
        // rows 6 to 14 of the 20 rows
        assert_eq!(towns.lines().count(), 5, "{towns}");
        // No value with the prefix: the index answers without a step.
        INDEX_STEPS.set(Some(0));
        INDEXED.set(false);
        let none = output(&db, &format!("{c}node[\"place\"~\"^(city)$\"]{bbox};out ids;"));
        INDEX_STEPS.set(None);
        assert!(INDEXED.get() && none.is_empty(), "{none}");
        // Not anchored, or key only: the region is decoded.
        for q in [format!("node[\"place\"~\"town\"]{bbox};out ids;"), format!("node[\"place\"]{bbox};out ids;")] {
            INDEX_STEPS.set(Some(usize::MAX));
            INDEXED.set(true);
            output(&db, &format!("{c}{q}"));
            INDEX_STEPS.set(None);
            assert!(!INDEXED.get(), "{q}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bounded scan gives the candidates of the full scan, or None past
    /// its step limit.
    #[test]
    fn bounded_tag_scan_stops_at_its_limits() {
        let (db, dir) = tagged_grid("bounded");
        let snap = db.snapshot().unwrap();
        let f = Fetch::new(&snap);
        let rects = BBox { south: 48.849, west: 2.349, north: 48.851, east: 2.351 }.rects();
        let ranges = ranges_for(Kind::Node, &rects);
        let re = |s: &str| TagFilter::from_ast(&Filter::Regex { key: "name".into(), re: s.into(), ci: false, neg: false }).unwrap().unwrap();
        for (t, r) in [(re("^Alpha"), Some(&ranges[..])), (re("^Alpha"), None), (re("^(Beta|Gamma)"), Some(&ranges[..])), (TagFilter::HasKey("place".into()), None)] {
            let all = f.tag_candidates(Kind::Node, &t, r).unwrap();
            assert!(!all.is_empty());
            assert_eq!(f.tag_candidates_bounded(Kind::Node, &t, r, usize::MAX).unwrap(), Some(all.clone()), "{t:?}");
            // every value of the key is a step
            assert_eq!(f.tag_candidates_bounded(Kind::Node, &t, r, 2).unwrap(), None, "{t:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// An `area` query with several key=value filters reads the index
    /// entries of the one with the fewest, in any order of the filters.
    #[test]
    fn area_query_reads_the_most_selective_eq() {
        use super::super::area::AREA_SOURCE;
        let id = |i: usize, j: usize| i * 30 + j + 1;
        let (mut ways, mut rels) = (String::new(), String::new());
        // 40 boundaries over squares of the grid, one with an English name
        for k in 0..40 {
            let (i, j) = (k / 10 * 2, k % 10 * 2);
            let nds: String = [id(i, j), id(i, j + 1), id(i + 1, j + 1), id(i + 1, j), id(i, j)].iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect();
            ways += &format!("<way id=\"{}\" {{m}}>{nds}</way>\n", 5000 + k);
            let name = format!("Town {k}");
            let mut tags = vec![("type", "boundary"), ("boundary", "administrative"), ("admin_level", "8"), ("name", name.as_str())];
            if k == 7 {
                tags.push(("name:en", "Seventh"));
            }
            rels += &relation(10 + k, &[("way", 5000 + k)], &tags);
        }
        let (db, dir) = osm_db("area-eq", &grid_osm_with("", &ways, &rels));
        crate::update::areas::make_areas(&db).unwrap();
        let c = "[out:csv(::type,::id;false)];";
        let source = |q: &str| {
            AREA_SOURCE.set(None);
            let out = output(&db, &format!("{c}{q}"));
            (out, AREA_SOURCE.take().map(|t| format!("{t:?}")))
        };
        let (a, sa) = source("area[\"boundary\"=\"administrative\"][\"name:en\"=\"Seventh\"];out ids;");
        let (b, sb) = source("area[\"name:en\"=\"Seventh\"][\"boundary\"=\"administrative\"];out ids;");
        assert_eq!(a, "area\t3600000017\n");
        assert_eq!(a, b);
        assert_eq!(sa, Some("Eq(\"name:en\", \"Seventh\")".to_string()));
        assert_eq!(sa, sb);
        // One key=value filter, or none: as before.
        let (all, s) = source("area[\"boundary\"=\"administrative\"][\"name\"~\"^Town\"];out ids;");
        assert_eq!((all.lines().count(), s), (40, Some("Eq(\"boundary\", \"administrative\")".to_string())));
        let (one, s) = source("area[\"name\"~\"^Town 3\"][\"admin_level\"];out ids;");
        assert_eq!(one.lines().count(), 11, "{one}");
        assert!(s.is_some_and(|s| s.starts_with("Regex")), "{one}");
        // An anchored regex with fewer entries than the key=value filter,
        // in either order, or with a key filter: the regex.
        for q in [
            "area[\"name\"~\"^Town 7$\"][\"boundary\"=\"administrative\"];out ids;",
            "area[\"boundary\"=\"administrative\"][\"name\"~\"^Town 7$\"];out ids;",
            "area[\"admin_level\"][\"name\"~\"^Town 7$\"];out ids;",
        ] {
            let (one, s) = source(q);
            assert_eq!(one, "area\t3600000017\n", "{q}");
            assert!(s.is_some_and(|s| s.starts_with("Regex")), "{q}");
        }
        // An unanchored regex is tested on the tags of the boundaries
        // first, the chunks without its value skipped.
        for (q, n) in [("area[\"boundary\"=\"administrative\"][\"name\"~\"own 7\"];out ids;", 1), ("area[\"name\"~\"Town\"][\"boundary\"=\"administrative\"];out ids;", 40), ("area[\"name\"~\"Zürich\"][\"boundary\"=\"administrative\"];out ids;", 0)] {
            TESTED_FIRST.set(0);
            let (out, s) = source(q);
            assert_eq!((out.lines().count(), s.as_deref()), (n, Some("Eq(\"boundary\", \"administrative\")")), "{q}");
            assert_eq!(TESTED_FIRST.get(), 1, "{q}");
        }
        // NARROW 0: the areas of both filters
        NARROW.set(Some(0));
        READ.set(0);
        let (both, _) = source("area[\"boundary\"=\"administrative\"][\"name\"=\"Town 7\"];out ids;");
        NARROW.set(None);
        assert_eq!((both.as_str(), READ.get()), ("area\t3600000017\n", 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Node statements with a key=value filter and an anchored regex read
    /// the index of the one with fewer entries, globally and in a region,
    /// with the answers of testing the regex on the nodes of the key=value
    /// filter.
    #[test]
    fn most_selective_takes_an_anchored_regex() {
        let (db, dir) = tagged_grid("selective");
        let c = "[out:csv(::type,::id;false)];";
        // place=townland: 10 nodes named Alpha8, Alpha48, ..., Alpha368;
        // names starting with Alpha12: 4, with Alpha: 100
        let cases = [
            ("[\"place\"=\"townland\"]", "[\"name\"~\"^Alpha12\"]", Some(true)),
            ("[\"place\"=\"townland\"]", "[\"name\"~\"^(Alpha128|Beta)$\"]", None),
            ("[\"place\"=\"townland\"]", "[\"name\"~\"^Alpha\"]", Some(false)),
            ("[\"place\"=\"townland\"]", "[\"name\"~\"^ALPHA12\",i]", None),
            ("[\"place\"=\"townland\"]", "[\"name\"~\"Alpha12\"]", Some(false)),
        ];
        for (eq, re, regex) in cases {
            for region in ["", "(48.849,2.349,48.851,2.351)"] {
                let reference = output(&db, &format!("{c}node{eq}{region}->.a;node.a{re};out ids;"));
                for q in [format!("{c}node{eq}{re}{region};out ids;"), format!("{c}node{re}{eq}{region};out ids;")] {
                    PROBE.set(None);
                    assert_eq!(output(&db, &q), reference, "{q}");
                    if let Some(regex) = regex {
                        assert_eq!(PROBE.take().is_some_and(|p| p.starts_with("Regex")), regex, "{q}");
                    }
                }
            }
        }
        assert_eq!(output(&db, &format!("{c}node[\"place\"=\"townland\"][\"name\"~\"^Alpha12\"];out ids;")), "node\t200129\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other `key=value` filters and anchored regexes keep the
    /// candidates they list too, when their index reads take few enough
    /// steps: the answers of testing the filters on the elements.
    #[test]
    fn index_candidates_intersect() {
        let (db, dir) = tagged_grid("intersect");
        let c = "[out:csv(::type,::id;false)];";
        let cases = [
            ("node", "[\"place\"=\"townland\"]", "[\"name\"~\"^Alpha\"]"),
            ("node", "[\"place\"=\"town\"]", "[\"name\"~\"^(alpha|Beta)\"]"),
            ("node", "[\"place\"=\"village\"]", "[\"name\"=\"Alpha9\"]"),
            ("node", "[\"place\"=\"village\"]", "[\"name\"~\"^Alpha\"][\"place\"=\"village\"]"),
            ("way", "[\"highway\"=\"primary\"]", "[\"name\"=\"Way 3\"]"),
            ("way", "[\"highway\"=\"primary\"]", "[\"name\"~\"^Way 1\"]"),
            ("way", "[\"highway\"=\"service\"]", "[\"name\"=\"Way 3\"]"),
        ];
        let mut both = 0;
        for (t, a, b) in cases {
            for region in ["", "(48.849,2.349,48.851,2.351)"] {
                let reference = output(&db, &format!("{c}{t}{a}{region}->.a;{t}.a{b};out ids;"));
                for q in [format!("{c}{t}{a}{b}{region};out ids;"), format!("{c}{t}{b}{a}{region};out ids;")] {
                    for narrow in [None, Some(0)] {
                        NARROW.set(narrow);
                        READ.set(0);
                        assert_eq!(output(&db, &q), reference, "{q} {narrow:?}");
                        if narrow.is_none() {
                            assert!(READ.get() < 2, "{q}");
                        }
                        both += (READ.get() >= 2) as usize;
                    }
                    NARROW.set(None);
                }
            }
        }
        assert!(both >= 10, "{both}");
        NARROW.set(Some(0));
        READ.set(0);
        assert_eq!(output(&db, &format!("{c}way[\"highway\"=\"primary\"][\"name\"=\"Way 3\"];out ids;")), "way\t4003\n");
        assert_eq!(READ.get(), 2);
        // 10 townland nodes allow 40 steps: the names of the 100 Alpha
        // nodes take more, and only the townland nodes are read
        READ.set(0);
        assert_eq!(output(&db, &format!("{c}node[\"place\"=\"townland\"][\"name\"~\"^Alpha\"];out ids;")).lines().count(), 10);
        assert_eq!(READ.get(), 1);
        NARROW.set(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Filters with more entries than `most_selective` counts: the first
    /// whose index read ends within the budget gives the candidates, here
    /// a regex over 30 nodes of one tile before a `key=value` filter over
    /// 300 tiles.
    #[test]
    fn index_candidates_from_the_first_read_to_end() {
        let m = "{m}";
        let mut nodes = String::new();
        for k in 0..300 {
            let rare = if k < 30 { format!("<tag k=\"name\" v=\"Rare {k}\"/>") } else { String::new() };
            nodes += &format!("<node id=\"{}\" lat=\"{:.4}\" lon=\"{:.4}\" {m}><tag k=\"kind\" v=\"common\"/>{rare}</node>\n", 300_000 + k, 10.0 + (k / 20) as f64 * 0.01, 10.0 + (k % 20) as f64 * 0.01);
        }
        for k in 0..30 {
            nodes += &format!("<node id=\"{}\" lat=\"20.0001\" lon=\"{:.5}\" {m}><tag k=\"kind\" v=\"common\"/><tag k=\"name\" v=\"Rare tile {k}\"/></node>\n", 310_000 + k, 20.0 + k as f64 * 0.00001);
        }
        let (db, dir) = osm_db("first-read", &grid_osm_with(&nodes, "", ""));
        let c = "[out:csv(::type,::id;false)];";
        // counting stops at 10: both filters have more entries. The
        // budgets are 80 and 640 steps: `^Rare tile` takes 60 (30 values,
        // one cell each), the key=value filter 301 (its cells), `^Rare` 120.
        COUNT_CAP.set(Some(10));
        for (re, n, first) in [("^Rare tile", 30, "Regex"), ("^Rare", 60, "Eq")] {
            let reference = output(&db, &format!("{c}node[\"kind\"=\"common\"]->.a;node.a[\"name\"~\"{re}\"];out ids;"));
            assert_eq!(reference.lines().count(), n);
            for q in [format!("node[\"kind\"=\"common\"][\"name\"~\"{re}\"]"), format!("node[\"name\"~\"{re}\"][\"kind\"=\"common\"]")] {
                FIRST.set(None);
                assert_eq!(output(&db, &format!("{c}{q};out ids;")), reference, "{q}");
                assert!(FIRST.take().is_some_and(|f| f.starts_with(first)), "{q}");
            }
        }
        COUNT_CAP.set(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Candidates from the tag index with further tag filters: the
    /// elements that pass all of them, whether or not the chunks hold the
    /// values these need (`at_tagged`).
    #[test]
    fn index_candidates_tested_on_their_tags_first() {
        let (db, dir) = tagged_grid("tags-first");
        let c = "[out:csv(::type,::id;false)];";
        let cases = [
            ("node", "[\"place\"=\"town\"]", "[\"name\"~\"lpha\"]"),
            ("node", "[\"place\"=\"town\"]", "[\"name\"~\"Zürich\"]"),
            ("node", "[\"place\"=\"village\"]", "[\"name\"~\"(alpha|Beta)\"]"),
            ("node", "[\"place\"]", "[\"name\"~\"AMMA1\",i]"),
            ("node", "[\"place\"]", "[\"name\"!~\"^Alpha\"]"),
            ("node", "[\"name\"~\"^Alpha\"]", "[!\"place\"]"),
            ("node", "[\"name\"~\"^Alpha\"]", "[\"place\"!=\"town\"]"),
            ("node", "[\"note\"~\"^far away\"]", "[\"note\"~\"number 1999\"]"),
            ("node", "[\"note\"~\"^far away\"]", "[\"note\"~\".*1999$\"]"),
            ("way", "[\"highway\"=\"secondary\"]", "[\"name\"~\"y 1\"]"),
            ("way", "[\"highway\"]", "[\"name\"~\"Way\"][\"highway\"!=\"service\"]"),
            ("way", "[\"highway\"=\"primary\"]", "[~\"^na\"~\"^Way\"]"),
            ("way", "[\"highway\"=\"primary\"]", "[\"name\"]"),
        ];
        for (t, a, b) in cases {
            for region in ["", "(48.849,2.349,48.851,2.351)", "(48.0,2.0,49.0,3.0)"] {
                let reference = output(&db, &format!("{c}{t}{a}{region}->.a;{t}.a{b};out ids;"));
                let q = format!("{c}{t}{a}{b}{region};out ids;");
                TESTED_FIRST.set(0);
                assert_eq!(output(&db, &q), reference, "{q}");
                // in the small region a key filter alone has the region decoded
                assert!(TESTED_FIRST.get() > 0 || !region.is_empty(), "{q}");
            }
        }
        // place=town: the 10 nodes named Gamma7, Gamma47, ..., Gamma367;
        // village: alpha9, alpha49, ...; townland: Alpha8, Alpha48, ...
        let count = |q: &str| output(&db, &format!("{c}{q};out ids;")).lines().count();
        assert_eq!(count("node[\"place\"=\"town\"][\"name\"~\"lpha\"]"), 0);
        assert_eq!(count("node[\"place\"=\"village\"][\"name\"~\"(alpha|Beta)\"]"), 10);
        assert_eq!(count("node[\"place\"][\"name\"~\"AMMA1\",i]"), 2);
        assert_eq!(count("node[\"note\"~\"^far away\"][\"note\"~\".*1999$\"]"), 2);
        assert_eq!(count("way[\"highway\"=\"secondary\"][\"name\"~\"y 1\"]"), 5);
        assert_eq!(output(&db, &format!("{c}node[\"note\"~\"^far away\"][\"note\"~\"number 1999$\"];out ids;")), "node\t301999\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where the candidate path of relation `around` starts: the index for
    /// a key=value filter or an anchored regex, the cells of the region for
    /// other tag filters (an unanchored regex would walk every value of its
    /// key), and when the index scan takes more steps than there may be
    /// candidates.
    #[test]
    fn rel_around_candidates_from_cells() {
        let names: Vec<String> = (0..300).map(|k| format!("L{k}")).collect();
        let mut rels = [
            relation(1, &[("way", 1015)], &[("route", "bus"), ("name", "Line one")]),
            relation(2, &[("node", 466)], &[("route", "tram"), ("name", "Tram one")]),
            relation(3, &[("way", 1000)], &[("route", "bus"), ("name", "Far line")]),
        ]
        .concat();
        // names starting with L far from the centre
        for (k, n) in names.iter().enumerate() {
            rels += &relation(100 + k, &[("node", 1 + k % 30)], &[("name", n.as_str())]);
        }
        let (db, dir) = grid_db("cells", &rels);
        let c = "[out:csv(::id;false)];";
        let near = "(around:500,48.85,2.35)";
        // query, where the candidates come from, ids
        let cases: &[(&str, &str, &[&str])] = &[
            ("relation[\"name\"~\"one\"]", "cells", &["1", "2"]),
            ("relation[\"name\"~\"ONE\",i]", "cells", &["1", "2"]),
            ("relation[\"name\"]", "cells", &["1", "2"]),
            ("relation[\"name\"~\"^Line\"]", "tags", &["1"]),
            ("relation[\"name\"~\"^L\"]", "cells", &["1"]),
            ("relation[\"route\"=\"bus\"]", "tags", &["1"]),
        ];
        for (q, from, ids) in cases {
            let q = format!("{c}{q}{near};out ids;");
            let (out, did) = both_ways(&db, &q);
            assert_eq!((did, CANDIDATES_FROM.get()), (Some("answered"), Some(*from)), "{q}");
            assert_eq!(out, ids.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "{q}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// The candidate path of relation `poly` gives the answers of the
    /// member path: nodes on edges and corners of the polygon, ways that
    /// cross it between their nodes or touch it in one point, missing
    /// nodes, with `around` and a bbox.
    #[test]
    fn rel_poly_candidates_match_member_path() {
        let rels = [
            relation(1, &[("way", 1015)], &[("route", "bus")]),
            relation(2, &[("node", 466)], &[("route", "tram")]),
            relation(3, &[("way", 1000)], &[("route", "bus")]),
            relation(4, &[("way", 3001), ("node", 1)], &[("route", "bus")]),
            relation(5, &[("relation", 1)], &[("route", "bus")]),
            // a node of the dense centre at 48.84105 2.34105
            relation(6, &[("node", 102_010)], &[("type", "multipolygon"), ("building", "yes")]),
            relation(7, &[("way", 2015), ("way", 1000)], &[("landuse", "grass")]),
            // grid nodes at 48.84 2.344 and 48.84 2.34
            relation(8, &[("node", 313)], &[("building", "yes")]),
            relation(9, &[("node", 311)], &[("building", "roof")]),
            relation(10, &[("way", 3000)], &[("building", "yes")]),
        ]
        .concat();
        let (db, dir) = grid_db("poly", &rels);
        let polys = [
            // triangle over the centre
            "48.84 2.34 48.86 2.34 48.86 2.36",
            // square with grid nodes on its edges and corners
            "48.840 2.340 48.840 2.360 48.860 2.360 48.860 2.340",
            // inside one grid cell: dense nodes only
            "48.8405 2.3405 48.8405 2.3415 48.8415 2.3415 48.8415 2.3405",
            // way 1015 crosses it between two of its nodes
            "48.849 2.3505 48.851 2.3505 48.851 2.3515 48.849 2.3515",
            // a corner on node 466: way 1015 touches it in one point
            "48.85 2.35 48.851 2.3505 48.851 2.3495",
        ];
        let c = "[out:csv(::id;false)];";
        for poly in polys {
            let p = format!("(poly:\"{poly}\")");
            // filters, whether the candidate path answers
            let cases = [
                ("relation", false),
                ("relation[\"route\"]", true),
                ("relation[\"route\"=\"bus\"]", true),
                ("relation[\"route\"~\"^(bus|tram)$\"]", true),
                ("relation[\"building\"]", true),
                ("relation[~\"^(building|landuse)$\"~\".\"]", true),
            ];
            for (filters, answered) in cases {
                for q in [format!("{c}{filters}{p};out ids;"), format!("{c}{filters}{p}(around:300,48.85,2.35);out ids;"), format!("[out:csv(::id;false)][bbox:48.845,2.33,48.86,2.37];{filters}{p};out ids;")] {
                    let (_, did) = both_ways(&db, &q);
                    if answered {
                        assert_eq!(did, Some("answered"), "{q}");
                    }
                }
            }
            both_ways(&db, &format!("relation[\"route\"]{p};out geom;"));
        }
        // The way box tests change nothing: every way by its vertices.
        for poly in polys {
            for filters in ["relation", "relation[\"route\"]", "relation[\"building\"]"] {
                let q = format!("{c}{filters}(poly:\"{poly}\");out ids;");
                let with = (run_q(&db, &q, true).0, run_q(&db, &q, false).0);
                WAY_BOXES_OFF.set(true);
                let without = (run_q(&db, &q, true).0, run_q(&db, &q, false).0);
                WAY_BOXES_OFF.set(false);
                assert_eq!(with, without, "{q}");
            }
        }
        // Nor do the relation box tests, or members picked out of the
        // region in place of lookups by id.
        for poly in polys {
            for filters in ["relation", "relation[\"route\"]", "relation[\"building\"]", "relation[\"route\"](around:300,48.85,2.35)"] {
                let q = format!("{c}{filters}(poly:\"{poly}\");out ids;");
                let plain = both_ways(&db, &q).0;
                for (boxes_off, scan) in [(true, false), (false, true), (true, true)] {
                    REL_BOXES_OFF.set(boxes_off);
                    MEMBER_SCAN.set(scan);
                    let (out, _) = both_ways(&db, &q);
                    REL_BOXES_OFF.set(false);
                    MEMBER_SCAN.set(false);
                    assert_eq!(out, plain, "{q} {boxes_off} {scan}");
                }
            }
        }
        let ids = |q: &str| both_ways(&db, &format!("{c}{q}")).0;
        assert_eq!(ids(&format!("relation[building](poly:\"{}\");out ids;", polys[1])), ["6", "8", "10"]);
        assert_eq!(ids(&format!("relation[building](poly:\"{}\");out ids;", polys[2])), ["6"]);
        assert_eq!(ids(&format!("relation[route](poly:\"{}\");out ids;", polys[3])), ["1"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// The buildings of a city: more relations than the estimate of 50
    /// members each allows, with one small way each, inside a polygon or
    /// across its border. The candidates come from the index (key=value) or
    /// the cells of the region (a key filter, no filter); the answers are
    /// those of the member path.
    #[test]
    fn rel_poly_many_small_relations() {
        let dense = |i: usize, j: usize| 100_000 + i * 200 + j;
        let (mut ways, mut rels) = (String::new(), String::new());
        for k in 0..300 {
            let (i, j) = (20 + k / 20 * 5, 20 + k % 20 * 5);
            let nds: String = [dense(i, j), dense(i, j + 1), dense(i + 1, j + 1), dense(i + 1, j), dense(i, j)].iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect();
            ways += &format!("<way id=\"{}\" {{m}}>{nds}<tag k=\"building\" v=\"yes\"/></way>\n", 6000 + k);
            rels += &relation(1000 + k, &[("way", 6000 + k)], &[("type", "multipolygon"), ("building", "yes")]);
        }
        let (db, dir) = osm_db("poly-many", &grid_osm_with("", &ways, &rels));
        let c = "[out:csv(::id;false)];";
        // the whole dense centre; a border through the 8th of 15 rows of
        // buildings (48.84555 to 48.84565)
        // the boxes decide all relations of the first polygon, those of
        // the 8th row need their members
        for (poly, n, members) in [("48.840 2.340 48.840 2.360 48.860 2.360 48.860 2.340", 300, None), ("48.840 2.340 48.8456 2.340 48.8456 2.360 48.840 2.360", 160, Some("ids"))] {
            for (f, what, from) in [("relation[\"building\"]", "answered", "cells"), ("relation[\"type\"=\"multipolygon\"]", "answered", "tags"), ("relation", "answered", "cells")] {
                let q = format!("{c}{f}(poly:\"{poly}\");out ids;");
                MEMBERS_FROM.set(None);
                let (out, did) = both_ways(&db, &q);
                assert_eq!((did, CANDIDATES_FROM.get(), MEMBERS_FROM.get()), (Some(what), Some(from), members), "{q}");
                assert_eq!(out.len(), n, "{q}");
                WAY_BOXES_OFF.set(true);
                let (without, _) = both_ways(&db, &q);
                WAY_BOXES_OFF.set(false);
                assert_eq!(out, without, "{q}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
