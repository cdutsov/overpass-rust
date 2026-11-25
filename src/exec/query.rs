// Contains code translated from C++ to Rust in 2026 from Overpass API,
// Copyright 2008-2018 Roland Olbricht et al. Modified by Chavdar Dutsov.
// GNU Affero General Public License, version 3 or later. See LICENSE and NOTICE.

//! The query statement: candidate generation and filtering per element type.

use std::borrow::Cow;

use super::fetch::{Fetch, Source, arc_ranges, ranges_for, tile_runs};
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
    /// Geometry sufficient for bbox, around and poly tests whose regions
    /// meet the stored boxes `near` accepts (relations: see
    /// `rel_geoms_near`, which also asks it about cells).
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
    /// The position of a node: its whole geometry.
    fn point(&self) -> Option<crate::geo::Pt> {
        None
    }
}

impl QElem for Node {
    fn tags_of(&self) -> &Tags {
        &self.tags
    }
    fn point(&self) -> Option<crate::geo::Pt> {
        Some(self.pos)
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
    node_test(b)(p)
}

/// `node_in_bbox` with the box converted once.
fn node_test(b: &BBox) -> impl Fn(crate::geo::Pt) -> bool {
    use crate::geo::{ilat, ilon};
    let (s, n, w, e) = (ilat(b.south), ilat(b.north), ilon(b.west), ilon(b.east));
    move |p| {
        let (la, lo) = (p.y, p.ilon());
        s <= la && la <= n && ((w <= lo && lo <= e) || (e < w && (lo >= w || lo <= e)))
    }
}

/// Way test: ways with a missing node have no geometry and never match.
/// A way passes when an end of one of its segments passes the node test,
/// so a node on a box edge takes its way (the f64 latitude of such a node
/// can lie 1e-14 outside the edge), and otherwise when a segment crosses
/// the box. The ends of a segment taken across the antimeridian do not
/// count: C++ tests the segment only (`bbox_intersects`), which then meets
/// a box east or west of the antimeridian only when the box holds both
/// ends.
fn line_in_bbox(v: &[Option<crate::geo::Pt>], b: &BBox) -> bool {
    if v.is_empty() || v.iter().any(|p| p.is_none()) {
        return false;
    }
    let inside = node_test(b);
    let pts: Vec<crate::geo::Pt> = v.iter().flatten().copied().collect();
    if pts.len() == 1 {
        return inside(pts[0]);
    }
    let ll = |p: crate::geo::Pt| (p.lat(), p.lon());
    pts.windows(2).any(|w| (inside(w[0]) || inside(w[1])) && !crate::geo::bbox_across(ll(w[0]), ll(w[1])))
        || pts.windows(2).any(|w| crate::geo::bbox_intersects(b, ll(w[0]), ll(w[1])))
}

/// Element-type specific bbox test (spec §5).
fn geom_in_bbox(g: &EGeom, b: &BBox) -> bool {
    match g {
        EGeom::Point(p) => node_in_bbox(*p, b),
        EGeom::Line(v) => line_in_bbox(v, b),
        EGeom::Multi { points, lines } => {
            let inside = node_test(b);
            points.iter().any(|p| inside(*p)) || lines.iter().any(|l| line_in_bbox(l, b))
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
                area_ctxs.push(self.shared_area_ctx(set, *id)?);
            }
            for (input, radius, points) in &plan.arounds {
                let mut src = self.around_source(input, points)?;
                // a source of a million ways takes seconds to index
                self.check()?;
                src.prepare(*radius);
                self.check()?;
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
            // The nodes and ways of a statement with one `around` clause
            // and nothing else (`nwr(around.p:3000)`) are those the member
            // path looks for.
            let found = (types.node && types.way && matches!(&filters[..], [Filter::Around { .. }])).then_some((&out.nodes[..], &out.ways[..]));
            out.rels = match self.rel_spatial_by_members(&filters, &arounds, found)? {
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

    /// The polygons of an `(area.set:id)` clause, built once for the
    /// statements that follow each other with the same input set (the same
    /// stored set, not only the same name) and id: the members of
    /// `(rel(area.c)[name="x"];rel(area.c)["name:en"="x"];...)` over a
    /// country each read the same tens of thousands of area tiles.
    fn shared_area_ctx(&mut self, set: &str, id: Option<Id>) -> Result<std::rc::Rc<super::area::AreaCtx>> {
        let input = self.get(set);
        if let Some((s, i, ctx)) = &self.area_ctx_last {
            if std::rc::Weak::ptr_eq(s, &std::rc::Rc::downgrade(&input)) && *i == id {
                #[cfg(test)]
                tests::AREA_CTX_SHARED.set(tests::AREA_CTX_SHARED.get() + 1);
                return Ok(ctx.clone());
            }
        }
        // the last one goes before the next is built
        self.area_ctx_last = None;
        self.area_ranges_last.clear();
        self.area_kept_bytes = 0;
        let ctx = std::rc::Rc::new(self.area_ctx(set, id)?);
        // Kept for the statements after this one only up to AREA_KEEP_MAX
        // and within the memory limit, and counted against it while kept:
        // the closed ways of a large input set go with their statement.
        let bytes = ctx.kept_bytes();
        if bytes <= area_keep_max() && self.check_memory(bytes).is_ok() {
            self.area_ctx_last = Some((std::rc::Rc::downgrade(&input), id, ctx.clone()));
            self.area_kept_bytes = bytes;
        }
        Ok(ctx)
    }

    /// The cell ranges to read for a statement whose only spatial clause is
    /// the shared `(area)` clause (`shared_area_ctx`), kept with it per
    /// element kind: what `spatial_rects` and `ranges_for` give for that
    /// single clause. The ranges of a country come from tens of thousands
    /// of tile rectangles, and every statement of
    /// `(rel(area.c)[name="x"];rel(area.c)["name:en"="x"];...)` needs them.
    /// None for every other statement.
    fn area_ranges(&mut self, kind: Kind, plan: &Plan, arounds: &[(AroundSource, f64)], area_ctxs: &[std::rc::Rc<super::area::AreaCtx>]) -> Option<Vec<CodeRange>> {
        let [ctx] = area_ctxs else { return None };
        if !plan.bboxes.is_empty() || !plan.polys.is_empty() || !arounds.is_empty() {
            return None;
        }
        if !self.area_ctx_last.as_ref().is_some_and(|(_, _, last)| std::rc::Rc::ptr_eq(last, ctx)) {
            return None;
        }
        #[cfg(test)]
        tests::REGION.set(Some("area"));
        let arcs = ctx.has_implicit();
        // ways and relations lie in the same cells (`ranges_for`, `arc_ranges`)
        let kind = if kind == Kind::Node { Kind::Node } else { Kind::Way };
        if let Some((_, _, r)) = self.area_ranges_last.iter().find(|(k, a, _)| *k == kind && *a == arcs) {
            #[cfg(test)]
            tests::AREA_RANGES_SHARED.set(tests::AREA_RANGES_SHARED.get() + 1);
            return Some(r.to_vec());
        }
        let r = std::rc::Rc::new(if arcs { arc_ranges(kind, &ctx.rects) } else { ctx.cell_ranges(kind).into_owned() });
        let bytes = (r.capacity() * std::mem::size_of::<CodeRange>()) as u64;
        if self.area_kept_bytes + bytes <= area_keep_max() && self.check_memory(bytes).is_ok() {
            self.area_ranges_last.push((kind, arcs, r.clone()));
            self.area_kept_bytes += bytes;
        }
        Some(r.to_vec())
    }

    fn around_source(&self, input: &Option<String>, points: &[(f64, f64)]) -> Result<AroundSource> {
        let mut src = AroundSource::default();
        if !points.is_empty() {
            src.add_line(points);
            return Ok(src);
        }
        let set = self.get(input.as_deref().unwrap_or("_"));
        for n in &set.nodes {
            src.points.push((n.pos.lat(), n.pos.lon()));
        }
        // The ways of a large set are known by their stored box until a
        // candidate comes near (`AroundSource::add_lazy`). Not the ways over
        // more than a few tiles: the box of a long way or a large polygon
        // holds much that is far from its line. A way without a box has no
        // node in the database and no geometry.
        let lazy = |w: &Way| !w.bbox.may_cross() && (w.bbox.y1 >> 16) - (w.bbox.y0 >> 16) < LAZY_TILES && (w.bbox.x1 >> 16) - (w.bbox.x0 >> 16) < LAZY_TILES;
        if set.ways.len() >= lazy_min() {
            let mut whole: Vec<&Way> = Vec::new();
            for w in set.ways.iter().filter(|w| !w.bbox.is_empty()) {
                if lazy(w) {
                    src.add_lazy(w);
                } else {
                    whole.push(w);
                }
            }
            for g in way_geoms(&self.f, &whole)? {
                src.add(&g);
            }
        } else {
            for g in way_geoms(&self.f, &set.ways)? {
                src.add(&g);
            }
        }
        for g in rel_geoms(&self.f, &set.rels)? {
            src.add(&g);
        }
        Ok(src)
    }

    /// The vertices of the lazy ways of the source that come near one of
    /// the geometries, as a source of their own for `around_match_with`;
    /// None when there are none.
    fn around_fine(&self, src: &AroundSource, r: f64, geoms: &[EGeom]) -> Result<Option<std::sync::Arc<AroundSource>>> {
        if !src.has_lazy() {
            return Ok(None);
        }
        if let Some(full) = src.lazy_full() {
            return Ok(Some(full));
        }
        let boxes: Vec<Rect> = geoms.iter().filter_map(geom_box).collect();
        let mut ways = src.lazy_near(&boxes);
        if ways.is_empty() {
            return Ok(None);
        }
        // Batches of candidates all over the region each come near most
        // ways: once as many were loaded as there are, all are loaded and
        // kept for the rest of the statement.
        let all = src.lazy_loaded(ways.len());
        if all {
            ways = src.lazy_all();
        }
        #[cfg(test)]
        tests::LAZY_LOADED.set(tests::LAZY_LOADED.get() + ways.len());
        self.check()?;
        let ids: Vec<Id> = ways.iter().flat_map(|w| w.iter().copied()).collect();
        let pos = self.f.node_positions(&ids)?;
        drop(ids);
        self.check()?;
        let mut fine = AroundSource::default();
        let mut pts: Vec<(f64, f64)> = Vec::new();
        for w in ways {
            pts.clear();
            // a way with a missing node contributes nothing
            if w.iter().all(|id| pos.get(id).map(|p| pts.push((p.lat(), p.lon()))).is_some()) {
                fine.add_line(&pts);
            }
        }
        fine.prepare_index(r);
        let fine = std::sync::Arc::new(fine);
        if all {
            src.set_lazy_full(fine.clone());
        }
        Ok(Some(fine))
    }

    /// Which of the geometries pass the `around` test of the source.
    fn around_pass(&self, src: &AroundSource, r: f64, geoms: &[EGeom]) -> Result<Vec<bool>> {
        let fine = self.around_fine(src, r, geoms)?;
        Ok(super::par_map(geoms, 512, |g| around_match_with(src, fine.as_deref(), g, r)))
    }

    /// Candidate ids from all clauses that collect data directly.
    ///
    /// Input sets (`node.a.b`) are read in place: the elements of the
    /// smallest source that every input set holds (sets are sorted by id)
    /// and that pass the tag filters are the only ones copied. A statement
    /// such as `nwr.named[name~"x"]` over a large set, or `node._.stops`
    /// inside a loop, does not copy the sets it reads.
    fn collected<T: QElem>(&mut self, plan: &Plan) -> Result<Option<Vec<T>>> {
        let mut sources: Vec<Vec<T>> = Vec::new();
        if let Some(mut given) = plan.given.lock().unwrap_or_else(|e| e.into_inner()).take() {
            sources.push(std::mem::take(T::from_set_mut(&mut given)));
        }
        if let Some(ids) = &plan.ids {
            let ids: Vec<Id> = if T::KIND == Kind::Node { ids.clone() } else { ids.iter().copied().filter(|i| *i <= u32::MAX as u64).collect() };
            sources.push(self.f.by_ids::<T>(&ids)?);
        }
        for (kind, input, role, positions, count) in &plan.recurse {
            let inp = self.get(input);
            let mut r = self.recurse_filter(*kind, &inp, role.as_deref(), positions.as_deref(), *count, T::etype())?;
            sources.push(std::mem::take(T::from_set_mut(&mut r)));
        }
        for op in &plan.recurse_ops {
            let inp = self.get("_");
            let mut r = self.recurse_constraint(*op, &inp)?;
            sources.push(std::mem::take(T::from_set_mut(&mut r)));
        }
        for set in &plan.pivots {
            let inp = self.get(set);
            let mut r = self.pivot(&inp)?;
            let mut v = std::mem::take(T::from_set_mut(&mut r));
            if T::KIND == Kind::Way {
                // `way(pivot)` passes all ways of the input set through.
                let extra = T::from_set(&inp).clone();
                v.extend(extra);
                v.sort_by_key(|e| e.id());
                v.dedup_by_key(|e| e.id());
            }
            sources.push(v);
        }
        let inputs: Vec<std::rc::Rc<Set>> = plan.inputs.iter().map(|name| self.get(name)).collect();
        if sources.is_empty() && inputs.is_empty() {
            return Ok(None);
        }
        let mut input_vecs: Vec<&Vec<T>> = inputs.iter().map(|s| T::from_set(s)).collect();
        debug_assert!(input_vecs.iter().all(|v| v.is_sorted_by_key(|e| e.id())));
        let holds = |v: &Vec<T>, id: Id| v.binary_search_by_key(&id, |e| e.id()).is_ok();
        if sources.is_empty() {
            input_vecs.sort_by_key(|v| v.len());
            let (base, rest) = input_vecs.split_first().unwrap();
            let acc = base.iter().filter(|e| rest.iter().all(|v| holds(v, e.id())) && plan.tags.iter().all(|t| t.matches(e.tags_of()))).cloned().collect();
            return Ok(Some(acc));
        }
        sources.sort_by_key(|s| s.len());
        let mut it = sources.into_iter();
        let mut acc = it.next().unwrap();
        for s in it {
            let ids: HashSet<Id> = s.iter().map(|e| e.id()).collect();
            acc.retain(|e| ids.contains(&e.id()));
        }
        acc.retain(|e| input_vecs.iter().all(|v| holds(v, e.id())));
        Ok(Some(acc))
    }

    /// The relations of the input sets of a statement (`rel.a.b`: those in
    /// every set) that pass `keep`, in the order of the first set.
    fn input_rels(&self, inputs: &[String], keep: impl Fn(&Relation) -> bool) -> Vec<Relation> {
        let Some((first, rest)) = inputs.split_first() else { return Vec::new() };
        let Some(first) = self.lookup(first) else { return Vec::new() };
        let rest: Vec<HashSet<Id>> = rest.iter().map(|n| self.lookup(n).map(|s| s.rels.iter().map(|r| r.id).collect()).unwrap_or_default()).collect();
        first.rels.iter().filter(|r| rest.iter().all(|ids| ids.contains(&r.id)) && keep(r)).cloned().collect()
    }

    /// The rectangles of the most restrictive single spatial clause, for
    /// range selection, and whether its test follows great-circle edges
    /// (`around`, implicit areas: `arc_ranges`).
    fn spatial_rects<'a>(&self, plan: &Plan, arounds: &'a [(AroundSource, f64)], area_ctxs: &'a [std::rc::Rc<super::area::AreaCtx>]) -> Result<Option<(Cow<'a, [Rect]>, bool)>> {
        // the cost of an around cover is kept with its source
        type Opt<'a> = (&'static str, Cow<'a, [Rect]>, bool, Option<&'a AroundSource>);
        let mut options: Vec<Opt<'a>> = plan.bboxes.iter().map(|b| ("bbox", Cow::Owned(b.rects()), false, None)).collect();
        options.extend(plan.polys.iter().map(|p| ("poly", Cow::Owned(p.bbox().rects()), false, None)));
        options.extend(arounds.iter().map(|(src, r)| ("around", src.bbox_rects(*r), true, Some(src))));
        options.extend(area_ctxs.iter().map(|c| ("area", Cow::Borrowed(&c.rects[..]), c.has_implicit(), None)));
        let mut best: Option<(u64, &'static str, Cow<'a, [Rect]>, bool)> = None;
        if options.len() == 1 {
            best = options.pop().map(|(what, rects, arcs, _)| (0, what, rects, arcs));
        }
        for (what, rects, arcs, cost) in options {
            let n = match cost {
                Some(src) => *src.cost.get_or_init(|| around_cost(src, &rects)),
                None => region_cost(&rects),
            };
            if best.as_ref().is_none_or(|(b, _, _, _)| n < *b) {
                best = Some((n, what, rects, arcs));
            }
        }
        #[cfg(test)]
        tests::REGION.set(best.as_ref().map(|(_, what, _, _)| *what));
        Ok(best.map(|(_, _, r, arcs)| (r, arcs)))
    }

    fn query_type<T: QElem>(&mut self, plan: &Plan, (area_ctxs, arounds): (&[std::rc::Rc<super::area::AreaCtx>], &[(AroundSource, f64)])) -> Result<Vec<T>> {
        let kind = T::KIND;
        let collected = self.collected::<T>(plan)?;
        let mut cands: Vec<T> = match collected {
            Some(c) => c,
            None => {
                let ranges: Option<Vec<CodeRange>> = match self.area_ranges(kind, plan, arounds, area_ctxs) {
                    Some(r) => Some(r),
                    None => {
                        let rects = self.spatial_rects(plan, arounds, area_ctxs)?;
                        self.check()?;
                        rects.as_ref().map(|(r, arcs)| if *arcs { arc_ranges(kind, r) } else { ranges_for(kind, r) })
                    }
                };
                self.check()?;
                if let Some((pairs, read)) = self.indexed_candidates(kind, plan, ranges.as_deref())? {
                    self.check()?;
                    self.at_tagged::<T>(plan, pairs, &read)?
                } else if let Some(r) = &ranges {
                    // A key or value-regex filter over a large region (a
                    // country) reads its tag index instead of every chunk
                    // (which one: `index_candidates`).
                    // In a smaller region, an anchored regex reads the
                    // index entries of its prefixes in the region, unless
                    // that takes more steps than the region has chunks.
                    let indexed = plan.tags.iter().any(|t| matches!(t, TagFilter::HasKey(_) | TagFilter::Regex { neg: false, .. }));
                    let anchored = plan.tags.iter().find(|t| anchored(t));
                    let pairs = if indexed {
                        let (region, total) = self.f.chunk_share(kind, r)?;
                        if total > 0 && region > (total as f64 * 0.1).ceil() as usize {
                            self.index_candidates(kind, plan, Some(r))?
                        } else if let Some(a) = anchored {
                            #[cfg(test)]
                            let region = tests::INDEX_STEPS.get().unwrap_or(region);
                            self.f.tag_candidates_bounded(kind, a, Some(r), region)?.map(|p| (p, a))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    #[cfg(test)]
                    tests::INDEXED.set(pairs.is_some());
                    match pairs {
                        Some((pairs, t)) => {
                            self.check()?;
                            self.at_tagged::<T>(plan, pairs, &[t])?
                        }
                        None => self.in_ranges_tagged::<T>(plan, r)?,
                    }
                } else if let Some((pairs, t)) = self.index_candidates(kind, plan, None)? {
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
            let around_rects: Vec<Cow<'_, [Rect]>> = arounds.iter().map(|(s, r)| s.bbox_rects(*r)).collect();
            let poly_rects: Vec<Vec<Rect>> = plan.polys.iter().map(|p| p.bbox().rects()).collect();
            // the polygons with their edge rows, once for every candidate,
            // when a candidate needs them (in a `foreach`, a statement whose
            // candidates all lie outside the box of a polygon of 10^4 edges
            // took 0.6 ms more per run for the rows)
            let polys_once: std::cell::OnceCell<Vec<PolyTest>> = std::cell::OnceCell::new();
            let polys = || polys_once.get_or_init(|| plan.polys.iter().map(|p| p.prepare()).collect());
            let poly_boxes = !plan.polys.is_empty() && way_boxes_on() && rel_boxes_on();
            // `around` and implicit areas follow great-circle edges, which
            // pass north or south of the box (`Rect::arc_box`)
            let arcs = !arounds.is_empty() || area_ctxs.iter().any(|c| c.has_implicit());
            let only_bbox = arounds.is_empty() && plan.polys.is_empty() && plan.areas.is_empty();
            let mut keep = vec![true; cands.len()];
            let mut undecided: Vec<usize> = Vec::new();
            // A node against boxes only: the test on its position is the
            // whole test (`geom_in_bbox` of a point), no geometry is built.
            let node_tests: Vec<_> = if only_bbox { plan.bboxes.iter().map(node_test).collect() } else { Vec::new() };
            #[cfg(test)]
            let only_bbox_nodes = only_bbox && !tests::NODE_BOX_PLAIN.get();
            #[cfg(not(test))]
            let only_bbox_nodes = only_bbox;
            let mut boxes_tested = 0usize;
            for (i, e) in cands.iter().enumerate() {
                if only_bbox_nodes {
                    if let Some(p) = e.point() {
                        keep[i] = node_tests.iter().all(|t| t(p));
                        continue;
                    }
                }
                if let Some(sb) = e.stored_bbox() {
                    if sb.is_empty() || (e.spatially_empty() && area_ctxs.is_empty()) {
                        keep[i] = false;
                        continue;
                    }
                    // An edge across the antimeridian leaves the box.
                    if sb.may_cross() {
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
                        let arc = if arcs { sb.arc_box() } else { sb };
                        reject = arounds.iter().zip(&around_rects).any(|((s, _), rs)| !s.near_box(rs, &arc))
                            || poly_rects.iter().any(|rs| rs.iter().all(|r| !grow(r).intersects(&sb)))
                            || area_ctxs.iter().any(|c| {
                                let b = if c.has_implicit() { &arc } else { &sb };
                                c.rects.iter().all(|r| !grow(r).intersects(b))
                            });
                    }
                    // A polygon decides a way or relation by its stored box
                    // where it can (`PolyTest::rect_pos`): apart from it, the
                    // element fails its test; strictly inside, it passes, as
                    // it has a vertex there (a way with all its nodes, a
                    // relation with a node or a complete way: not
                    // `spatially_empty`). Of 1,720 protected areas in the box
                    // of a country polygon, 608 lay inside and 872 apart:
                    // their members were not read.
                    let mut polys_decided = plan.polys.is_empty();
                    if !reject && poly_boxes {
                        // a box test costs up to all edges where the rows
                        // do not help: the ways of a city against a comb of
                        // 2 x 10^4 edges ran 46 s with [timeout:5]
                        boxes_tested += 1;
                        if boxes_tested % 64 == 0 {
                            self.check()?;
                        }
                        polys_decided = true;
                        for p in polys() {
                            match p.rect_pos(&sb) {
                                RectPos::Apart => {
                                    reject = true;
                                    break;
                                }
                                RectPos::Inside => {}
                                RectPos::Border => polys_decided = false,
                            }
                        }
                    }
                    #[cfg(test)]
                    if poly_boxes && (reject || polys_decided) {
                        let (pass, fail) = tests::POLY_BOXES.get();
                        tests::POLY_BOXES.set(if reject { (pass, fail + 1) } else { (pass + 1, fail) });
                    }
                    if reject {
                        keep[i] = false;
                        continue;
                    }
                    if all_inside && polys_decided && arounds.is_empty() && plan.areas.is_empty() {
                        continue;
                    }
                }
                undecided.push(i);
            }
            if !undecided.is_empty() {
                // Batches of neighbours: a batch then reads the node chunks
                // of its own region, and few chunks are read for two
                // batches (candidates from a set come ordered by id).
                if undecided.len() > geom_batch() {
                    undecided.sort_by_cached_key(|&i| cands[i].stored_bbox().map_or(0, |b| crate::geo::morton(b.y0 >> 16, b.x0 >> 16)));
                }
                let refs: Vec<&T> = undecided.iter().map(|&i| &cands[i]).collect();
                // With only (area) clauses there is nothing to test on these
                // geometries: the area tests below build their own.
                let geom_tests = !plan.bboxes.is_empty() || !arounds.is_empty() || !plan.polys.is_empty();
                let mut sub_keep: Vec<bool> = if !geom_tests {
                    vec![true; refs.len()]
                } else {
                    // A way apart from a polygon at every shift fails its
                    // way test (`PolyTest::rect_pos`).
                    let polys = polys();
                    let near_poly = |b: &Rect| !b.is_empty() && polys.iter().any(|p| p.rect_pos(b) != RectPos::Apart);
                    let near_bbox = |b: &Rect| bbox_rects.iter().any(|rs| rs.iter().any(|r| grow(r).intersects(b)));
                    let near_around = |b: &Rect| {
                        if arounds.is_empty() {
                            return false;
                        }
                        let a = b.arc_box();
                        arounds.iter().zip(&around_rects).any(|((s, _), rs)| s.near_box(rs, &a))
                    };
                    // Also for cells: `near_around` takes the cell grown by the
                    // bulge of the edges it may hold (boxes up to its width),
                    // and `near_box` accepts every box that meets the
                    // rectangles of the clause, which hold all it can pass.
                    let near = |b: &Rect| near_bbox(b) || near_around(b) || near_poly(b);
                    let limits = &self.f.limits;
                    let test = |g: &EGeom, fine: &[Option<std::sync::Arc<AroundSource>>]| -> Result<bool> {
                        if !(plan.bboxes.iter().all(|b| geom_in_bbox(g, b)) && arounds.iter().zip(fine).all(|((src, r), fine)| around_match_with(src, fine.as_deref(), g, *r))) {
                            return Ok(false);
                        }
                        for p in polys {
                            if !p.matches_checked(g, limits)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    };
                    // The geometries are built and tested for a batch of
                    // elements at a time: those of all candidates at once
                    // (3 x 10^6 ways of a polygon over a small country) took
                    // the memory limit of the query, and no timeout stopped
                    // them. What a batch looked up is dropped with it.
                    let mut sub_keep = Vec::with_capacity(refs.len());
                    for part in refs.chunks(geom_batch()) {
                        self.check()?;
                        let scope = self.f.limits.mem_scope();
                        let geoms = if only_bbox && bbox_rects.len() == 1 {
                            T::bbox_geoms(&self.f, part, &bbox_rects[0])?
                        } else if reduce_rel_geoms(plan) {
                            T::near_geoms(&self.f, part, &near)?
                        } else {
                            T::geoms(&self.f, part)?
                        };
                        // the lazy ways of the sources near this batch
                        let fine = arounds.iter().map(|(src, r)| self.around_fine(src, *r, &geoms)).collect::<Result<Vec<_>>>()?;
                        // Geometric tests are pure: run them in parallel for
                        // large batches. The limits are checked between the
                        // tests: one batch of relations against a polygon of
                        // 10^4 edges ran 23 s past the timeout.
                        super::par_run::<_, _, anyhow::Error>(
                            &geoms,
                            512,
                            |g| {
                                limits.check()?;
                                test(g, &fine)
                            },
                            |k| {
                                sub_keep.push(k);
                                Ok(())
                            },
                        )?;
                        drop(fine);
                        drop(geoms);
                        drop(scope);
                    }
                    sub_keep
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
    ///
    /// `found`: the nodes and ways that pass the clause, when the statement
    /// has found them already (its only clause, also asked for nodes and
    /// ways). Else the member path reads them without their tags, which no
    /// test here looks at.
    fn rel_spatial_by_members(&mut self, filters: &[Filter], arounds: &[(AroundSource, f64)], found: Option<(&[Node], &[Way])>) -> Result<Option<Plan>> {
        // About 0.01 square degrees (10 x 7 km in central Europe).
        const MAX_REGION: f64 = 1e12;
        let size = |rects: &[Rect]| -> f64 { rects.iter().map(|x| (x.y1 - x.y0) as f64 * (x.x1 - x.x0) as f64).sum() };
        let clauses: Vec<&Filter> = filters.iter().filter(|f| matches!(f, Filter::Around { .. } | Filter::Poly(_))).collect();
        if clauses.is_empty() || clauses.iter().filter(|f| matches!(f, Filter::Around { .. })).count() != arounds.len() {
            return Ok(None);
        }
        // The member tests of the clauses, in their order, with their rectangles.
        let mut next_around = arounds.iter();
        let member_tests: Vec<(MemberTest, Cow<'_, [Rect]>)> = clauses
            .iter()
            .map(|f| match f {
                Filter::Poly(pts) => {
                    let poly = Polygon { pts: pts.clone() };
                    let rects = poly.bbox().rects();
                    (MemberTest::Poly(poly.prepare()), Cow::Owned(rects))
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
            let (node_ids, way_ids): (Vec<Id>, Vec<Id>) = match clause {
                Filter::Around { .. } => {
                    let (src, r) = next_around.next().expect("counted above");
                    match found {
                        Some((nodes, ways)) => {
                            #[cfg(test)]
                            tests::AROUND_MEMBERS.set(Some("found"));
                            (nodes.iter().map(|n| n.id).collect(), ways.iter().map(|w| w.id).collect())
                        }
                        None => {
                            #[cfg(test)]
                            tests::AROUND_MEMBERS.set(Some("read"));
                            self.around_members(src, *r)?
                        }
                    }
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
                    let mut ways = self.f.in_ranges_by::<Way>(&ranges_for(Kind::Way, &rects), |v, p| codec::decode_ways(v, p, false), |w| (w.bbox.may_cross() || rects.iter().any(|r| grow(r).intersects(&w.bbox))) && !(boxes && test.rect_pos(&w.bbox) == RectPos::Apart))?;
                    self.check()?;
                    let pass = poly_member_ways(&self.f, &test, &ways.iter().collect::<Vec<_>>())?;
                    let mut i = 0;
                    ways.retain(|_| {
                        i += 1;
                        pass[i - 1]
                    });
                    members.nodes = nodes;
                    members.ways = ways;
                    (members.nodes.iter().map(|n| n.id).collect(), members.ways.iter().map(|w| w.id).collect())
                }
                _ => unreachable!(),
            };
            drop(members);
            // the relations are decoded once, by the plan below
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

    /// The ids of the nodes and ways an `around` clause passes, as
    /// `query_type` finds them for the clause alone (the nodes in the tiles
    /// of its rectangles, the ways stored where an edge may meet them, then
    /// the stored box and the test on the vertices), decoded without their
    /// tags: for the member path of relations, which needs the ids only.
    fn around_members(&mut self, src: &AroundSource, r: f64) -> Result<(Vec<Id>, Vec<Id>)> {
        let rects = src.bbox_rects(r);
        // with lazy ways, the nodes in their boxes first
        let lazy = src.has_lazy();
        let nodes = self.f.in_ranges_by::<Node>(&ranges_for(Kind::Node, &rects), |v, p| codec::decode_nodes(v, p, false), |n| if lazy { src.near_box(&rects, &Rect::point(n.pos)) } else { around_match(src, &EGeom::Point(n.pos), r) })?;
        let mut node_ids: Vec<Id> = nodes.iter().map(|n| n.id).collect();
        if lazy {
            node_ids.clear();
            for part in nodes.chunks(geom_batch()) {
                self.check()?;
                let geoms: Vec<EGeom> = part.iter().map(|n| EGeom::Point(n.pos)).collect();
                let pass = self.around_pass(src, r, &geoms)?;
                node_ids.extend(part.iter().zip(pass).filter(|(_, p)| *p).map(|(n, _)| n.id));
            }
        }
        drop(nodes);
        self.check()?;
        // an edge across the antimeridian leaves the box
        let mut close = self.f.in_ranges_by::<Way>(&arc_ranges(Kind::Way, &rects), |v, p| codec::decode_ways(v, p, false), |w| !w.missing && !w.bbox.is_empty() && (w.bbox.may_cross() || src.near_box(&rects, &w.bbox.arc_box())))?;
        if close.len() > geom_batch() {
            close.sort_by_cached_key(|w| crate::geo::morton(w.bbox.y0 >> 16, w.bbox.x0 >> 16));
        }
        let mut way_ids = Vec::new();
        for part in close.chunks(geom_batch()) {
            self.check()?;
            let scope = self.f.limits.mem_scope();
            let geoms = way_geoms(&self.f, part)?;
            let pass = self.around_pass(src, r, &geoms)?;
            way_ids.extend(part.iter().zip(pass).filter(|(_, p)| *p).map(|(w, _)| w.id));
            drop(geoms);
            drop(scope);
        }
        Ok((node_ids, way_ids))
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
    fn rel_spatial_by_candidates(&mut self, filters: &[Filter], member_tests: &[(MemberTest, Cow<'_, [Rect]>)]) -> Result<Option<Vec<Relation>>> {
        const MAX_CANDIDATES: usize = 20_000;
        const NODES_PER_CHUNK: usize = 500;
        const MEMBER_COST: usize = 4;
        const MEMBERS_PER_RELATION: usize = 50;
        let Some((_, rects0)) = member_tests.first() else { return Ok(None) };
        let rest: Vec<Filter> = filters.iter().filter(|f| !matches!(f, Filter::Around { .. } | Filter::Poly(_))).cloned().collect();
        let plan = Plan::new(&rest)?;
        let region_cost0 = match member_tests.first() {
            Some((MemberTest::Around(src, _), _)) => around_cost(src, rects0),
            _ => region_cost(rects0),
        };
        let bbox = plan.bboxes.iter().map(|b| (region_cost(&b.rects()), b.rects())).filter(|(n, _)| *n < region_cost0).min_by_key(|(n, _)| *n).map(|(_, rs)| rs);
        let ranges = match (&bbox, member_tests.first()) {
            (Some(rs), _) => ranges_for(Kind::Relation, rs),
            (None, Some((MemberTest::Around(..), _))) => arc_ranges(Kind::Relation, rects0),
            (None, _) => ranges_for(Kind::Relation, rects0),
        };
        // An unanchored regex or a key filter would walk every value of its
        // key (every name on the planet): those are tested on the relations
        // stored in the cells of the region instead.
        // A statement on sets (`nwr.a(around:…)`) takes the relations of
        // its input sets as the candidates: the region may hold many more
        // (600 relations within 2.5 km of a town centre, 70 hiking routes
        // in the set).
        let on_sets = !plan.inputs.is_empty();
        let probe = if on_sets {
            None
        } else {
            match self.most_selective(Kind::Relation, &plan, Some(&ranges))? {
                Some(eq) => Some(eq),
                None => plan.tags.iter().find(|t| anchored(t)),
            }
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
        tests::CANDIDATES_FROM.set(Some(if on_sets { "sets" } else if pairs.is_some() { "tags" } else if bbox.is_some() { "bbox" } else { "cells" }));
        if pairs.as_ref().is_some_and(|p| p.len() > MAX_CANDIDATES) {
            #[cfg(test)]
            tests::CANDIDATES.set(Some("too_many_candidates"));
            return Ok(None);
        }
        // as the stored-bbox test of query_type: an edge across the
        // antimeridian leaves the box, which goes to the geometry test
        let wide = |b: &Rect| b.may_cross();
        let grow = |r: &Rect| Rect { y0: r.y0.saturating_sub(1), x0: r.x0.saturating_sub(1), y1: r.y1.saturating_add(1), x1: r.x1.saturating_add(1) };
        let meets = |b: &Rect, rs: &[Rect]| rs.iter().any(|x| grow(x).intersects(b));
        // `around` follows great-circle edges (`Rect::arc_box`)
        let near = |b: &Rect, test: &MemberTest, rs: &[Rect]| {
            wide(b)
                || match test {
                    MemberTest::Around(src, _) => src.near_box(rs, &b.arc_box()),
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
        self.check()?;
        // The relations of the sets are in memory: all of them are
        // candidates. With too many members the member path answers, as
        // for the other sources.
        let cands = if on_sets {
            self.input_rels(&plan.inputs, keep)
        } else {
            let source = match pairs {
                Some(pairs) => Source::At(pairs),
                None => Source::Ranges(&ranges),
            };
            let Some(cands) = self.f.bounded::<Relation>(source, keep, |r| if open(r) { r.members.len() } else { 0 }, over)? else {
                #[cfg(test)]
                tests::CANDIDATES.set(Some("too_many_candidates"));
                return Ok(None);
            };
            cands
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
                    let nodes = if src.has_lazy() {
                        let (ids, geoms): (Vec<Id>, Vec<EGeom>) = positions.iter().map(|(id, p)| (*id, EGeom::Point(*p))).unzip();
                        let pass = self.around_pass(src, *r, &geoms)?;
                        ids.into_iter().zip(pass).filter(|(_, p)| *p).map(|(id, _)| id).collect()
                    } else {
                        positions.iter().filter(|(_, p)| around_match(src, &EGeom::Point(**p), *r)).map(|(id, _)| *id).collect()
                    };
                    let close: Vec<&Way> = ways.iter().filter(|w| !w.bbox.is_empty() && !w.missing && near(&w.bbox, test, rs)).collect();
                    let geoms = way_geoms(&self.f, &close)?;
                    let pass = self.around_pass(src, *r, &geoms)?;
                    (nodes, close.iter().zip(pass).filter(|(_, p)| *p).map(|(w, _)| w.id).collect())
                }
                // the nodes and ways the member path reads: nodes in the
                // tiles of the rectangles, ways whose box meets them
                MemberTest::Poly(poly) if lookups => {
                    let tiles = ranges_for(Kind::Node, rs);
                    let nodes = positions.iter().filter(|(_, p)| cell_in_ranges(Cell::tile(tile_of(**p)), &tiles) && poly.contains_member(**p)).map(|(id, _)| *id).collect();
                    let close: Vec<&Way> = ways.iter().filter(|w| wide(&w.bbox) || meets(&w.bbox, rs)).collect();
                    let pass = poly_member_ways(&self.f, poly, &close)?;
                    (nodes, close.iter().zip(pass).filter(|(_, p)| *p).map(|(w, _)| w.id).collect())
                }
                MemberTest::Poly(poly) => {
                    let want_nodes: HashSet<Id> = nids.iter().copied().collect();
                    let want_ways: HashSet<Id> = wids.iter().copied().collect();
                    let nodes = self.f.in_ranges_by::<Node>(&ranges_for(Kind::Node, rs), |v, p| codec::decode_nodes(v, p, false), |n| want_nodes.contains(&n.id) && poly.contains_member(n.pos))?;
                    let close = self.f.in_ranges_by::<Way>(&ranges_for(Kind::Way, rs), |v, p| codec::decode_ways(v, p, false), |w| want_ways.contains(&w.id) && (wide(&w.bbox) || meets(&w.bbox, rs)))?;
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

    /// Candidates from the tag index for a query without `key=value`
    /// filters on the planet, or in a large region (`within`), and the
    /// filter they come from: on the planet an anchored regex, else the
    /// first positive filter (in a region the first `[key]` filter or value
    /// regex). A `[key]` filter lists every element with the key, each then
    /// decoded from its chunk; a regex reads every index entry of its key
    /// and decodes the elements with a matching value. So a first `[key]`
    /// filter with more than `MANY` candidates gives way to a regex of the
    /// query with at most `MANY` (`["name"~"."]` lists every name).
    /// `node["place"]["name"~"Springfield"]`: 7.7 x 10^6 place
    /// nodes in 1.8 x 10^6 chunks (26 GB, 9 to 33 s), the node names 1.5 s;
    /// in the box of the US: 2.7 x 10^5 place nodes 0.6 s, the names 1.7 s.
    fn index_candidates<'p>(&self, kind: Kind, plan: &'p Plan, within: Option<&[CodeRange]>) -> Result<Option<(Vec<(Cell, Id)>, &'p TagFilter)>> {
        const MANY: usize = 500_000;
        #[cfg(test)]
        let many = tests::MANY.get().unwrap_or(MANY);
        #[cfg(not(test))]
        let many = MANY;
        let planet = within.is_none();
        let first = if planet {
            plan.tags.iter().find(|t| anchored(t)).or_else(|| plan.tags.iter().find(|t| t.is_positive()))
        } else {
            plan.tags.iter().find(|t| matches!(t, TagFilter::HasKey(_) | TagFilter::Regex { neg: false, .. }))
        };
        let Some(first) = first else { return Ok(None) };
        let regex = plan.tags.iter().find(|t| matches!(t, TagFilter::Regex { neg: false, .. }) || planet && matches!(t, TagFilter::KeyRegex { .. }));
        let (pairs, t) = match (first, regex) {
            (TagFilter::HasKey(_), Some(re)) => match self.f.tag_candidates_in(kind, first, within, many)? {
                Some(pairs) => (pairs, first),
                None => match self.f.tag_candidates_in(kind, re, within, many)? {
                    Some(pairs) => (pairs, re),
                    None => (self.f.tag_candidates_in(kind, first, within, usize::MAX)?.unwrap_or_default(), first),
                },
            },
            _ => (self.f.tag_candidates_in(kind, first, within, usize::MAX)?.unwrap_or_default(), first),
        };
        #[cfg(test)]
        tests::INDEX_FILTER.set(Some(format!("{t:?}")));
        Ok(Some((pairs, t)))
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
        self.f.at_where::<T>(pairs, Some(TagTest { chunk: &chunk, keep: &keep, keys: &super::fetch::required_keys(rest.iter().copied()), key_tests: &[] }))
    }

    /// The elements stored in the ranges that pass the tag filters of the
    /// plan. The tags are tested before an element is decoded whole
    /// (`Fetch::in_ranges_tagged`): a chunk without a key the filters need
    /// in its key table (`TagTest::keys`), or where a value they need does
    /// not occur (`TagFilter::value_literals`, as for index candidates in
    /// `at_tagged`), is not read further. `nwr(area.a)[name][name~"x"]`
    /// builds the few elements with such a name, not every element of the
    /// region.
    fn in_ranges_tagged<T: QElem>(&self, plan: &Plan, ranges: &[CodeRange]) -> Result<Vec<T>> {
        #[cfg(test)]
        let plain = tests::RANGES_PLAIN.get();
        #[cfg(not(test))]
        let plain = false;
        if plan.tags.is_empty() || plain {
            // The cells of the ranges reach beyond the boxes of the
            // statement: the nodes outside a box are dropped chunk by
            // chunk, before they are collected and sorted (`node(bbox)`
            // reads up to several times the nodes it returns).
            #[cfg(test)]
            if tests::NODE_BOX_PLAIN.get() {
                return self.f.in_ranges::<T>(ranges, |e| plan.tags.iter().all(|t| t.matches(e.tags_of())));
            }
            let inside: Vec<_> = plan.bboxes.iter().map(node_test).collect();
            return self.f.in_ranges::<T>(ranges, |e| plan.tags.iter().all(|t| t.matches(e.tags_of())) && e.point().is_none_or(|p| inside.iter().all(|t| t(p))));
        }
        let needs = plan.tags.iter().filter_map(|t| t.value_literals()).map(|lits| literal_search(&lits)).collect::<Result<Vec<_>>>()?;
        let chunk = |data: &[u8]| needs.iter().all(|re| re.is_match(data));
        let keep = |t: &Tags| plan.tags.iter().all(|f| f.matches(t));
        self.f.in_ranges_tagged::<T>(ranges, &plan.tags, &chunk, &keep)
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

/// Most bytes the polygons of an `(area)` clause (beyond its database
/// areas) and their cell ranges hold while they are kept for the
/// statements after it (`shared_area_ctx`): 64 MiB.
const AREA_KEEP_MAX: u64 = 64 << 20;

fn area_keep_max() -> u64 {
    #[cfg(test)]
    if let Some(c) = tests::AREA_KEEP_MAX.get() {
        return c;
    }
    AREA_KEEP_MAX
}

/// A search for any of the byte strings.
fn literal_search(lits: &[&[u8]]) -> Result<regex::bytes::Regex> {
    let alts: Vec<String> = lits.iter().map(|l| l.iter().map(|b| format!("\\x{b:02x}")).collect()).collect();
    Ok(regex::bytes::RegexBuilder::new(&alts.join("|")).unicode(false).build()?)
}

/// Count the reads of every set name in the statements: inputs of
/// statements and filters, loop variables, and the sets of evaluators.
fn count_reads(stmts: &[Stmt], reads: &mut std::collections::HashMap<String, usize>) {
    fn expr(e: &Expr, reads: &mut std::collections::HashMap<String, usize>) {
        match e {
            Expr::Lit(_) | Expr::Tag(_) | Expr::Generic => {}
            Expr::Call(_, args) => args.iter().for_each(|a| expr(a, reads)),
            Expr::Agg { set, args, .. } => {
                *reads.entry(set.clone().unwrap_or_else(|| "_".into())).or_default() += 1;
                args.iter().for_each(|a| expr(a, reads));
            }
            Expr::SetProp { set, .. } => *reads.entry(set.clone()).or_default() += 1,
            Expr::Unary(_, a) => expr(a, reads),
            Expr::Binary(_, a, b) => {
                expr(a, reads);
                expr(b, reads);
            }
            Expr::Ternary(a, b, c) => {
                expr(a, reads);
                expr(b, reads);
                expr(c, reads);
            }
        }
    }
    fn entries(es: &[MakeEntry], reads: &mut std::collections::HashMap<String, usize>) {
        for e in es {
            if let MakeKey::Generic { from: Some(set) } = &e.key {
                *reads.entry(set.clone()).or_default() += 1;
            }
            if let Some(v) = &e.value {
                expr(v, reads);
            }
        }
    }
    let read = |name: &str, reads: &mut std::collections::HashMap<String, usize>| *reads.entry(name.to_string()).or_default() += 1;
    for s in stmts {
        match &s.kind {
            StmtKind::Query { filters, .. } => {
                for f in filters {
                    match f {
                        Filter::Around { input, .. } => read(input.as_deref().unwrap_or("_"), reads),
                        Filter::Area { set, .. } | Filter::Pivot(set) | Filter::Input(set) => read(set, reads),
                        Filter::Recurse { input, .. } => read(input, reads),
                        Filter::RecurseOp(_) => read("_", reads),
                        Filter::If(e) => expr(e, reads),
                        Filter::HasKey(_)
                        | Filter::NotKey(_)
                        | Filter::Eq(..)
                        | Filter::Ne(..)
                        | Filter::Regex { .. }
                        | Filter::KeyRegex { .. }
                        | Filter::BBox(_)
                        | Filter::Ids(_)
                        | Filter::Poly(_)
                        | Filter::Newer(_)
                        | Filter::User(_)
                        | Filter::Uid(_) => {}
                    }
                }
            }
            StmtKind::Union(v) => {
                // a union reads the result of each member
                for m in v {
                    read(&m.into, reads);
                }
                count_reads(v, reads);
            }
            StmtKind::Difference(a, b) => {
                read(&a.into, reads);
                read(&b.into, reads);
                count_reads(std::slice::from_ref(a), reads);
                count_reads(std::slice::from_ref(b), reads);
            }
            StmtKind::Recurse { input, .. } | StmtKind::Out { input, .. } | StmtKind::IsIn { input, .. } | StmtKind::MapToArea { input } => read(input, reads),
            StmtKind::Item { set } => read(set, reads),
            StmtKind::Foreach { input, var, body } | StmtKind::Complete { input, var, body, .. } => {
                read(input, reads);
                read(var.as_deref().unwrap_or("_"), reads);
                count_reads(body, reads);
            }
            StmtKind::For { input, var, key, body } => {
                read(input, reads);
                read(var.as_deref().unwrap_or("_"), reads);
                expr(key, reads);
                count_reads(body, reads);
            }
            StmtKind::If { cond, then, els } => {
                expr(cond, reads);
                count_reads(then, reads);
                count_reads(els, reads);
            }
            StmtKind::Make { entries: es, .. } => entries(es, reads),
            StmtKind::Convert { input, entries: es, .. } => {
                read(input, reads);
                entries(es, reads);
            }
            StmtKind::Unsupported(_) => {}
        }
    }
}

/// Whether a statement (with the statements inside it) reads the set.
pub(super) fn reads_set(s: &Stmt, set: &str) -> bool {
    let mut reads = std::collections::HashMap::new();
    count_reads(std::slice::from_ref(s), &mut reads);
    reads.contains_key(set)
}

/// The statement that answers `b` without storing the result of `a`, for
/// the pair `nwr(area.c)[name]->.s; nwr.s[name~"x"];`: a query into a named
/// set, then a query that takes that set and tests tags only. Every filter
/// of a query is a test of the element alone, so the second result is
/// what the filters of both statements pass, of the types both name. The
/// tag filters of `b` then run where the elements are decoded, before the
/// spatial tests of `a` (the area test of every named element of a city
/// costs far more than the regex).
///
/// Only for pairs where the fused statement behaves as `a` does: element
/// types only, `a` not a standalone criterion (those ignore the global
/// bbox) and with a clause that keeps it from the "filters too weak"
/// remark. The caller checks that nothing else reads the set of `a`.
fn fused(a: &Stmt, b: &Stmt) -> Option<Stmt> {
    let (StmtKind::Query { types: ta, filters: fa }, StmtKind::Query { types: tb, filters: fb }) = (&a.kind, &b.kind) else { return None };
    if a.into == "_" || ta.area || ta.derived || tb.area || tb.derived || is_standalone(ta, fa) {
        return None;
    }
    let is_tag = |f: &Filter| matches!(f, Filter::HasKey(_) | Filter::NotKey(_) | Filter::Eq(..) | Filter::Ne(..) | Filter::Regex { .. } | Filter::KeyRegex { .. });
    let strong = |f: &Filter| match f {
        Filter::HasKey(_) | Filter::Eq(..) | Filter::KeyRegex { .. } | Filter::Regex { neg: false, .. } => true,
        Filter::BBox(_) | Filter::Ids(_) | Filter::Around { .. } | Filter::Poly(_) | Filter::Area { .. } | Filter::Recurse { .. } | Filter::RecurseOp(_) | Filter::Input(_) => true,
        _ => false,
    };
    if fa.iter().any(|f| matches!(f, Filter::Pivot(_))) || !fa.iter().any(strong) {
        return None;
    }
    let inputs = fb.iter().filter(|f| matches!(f, Filter::Input(_))).count();
    let tags: Vec<Filter> = fb.iter().filter(|f| is_tag(f)).cloned().collect();
    if inputs != 1 || tags.is_empty() || tags.len() + 1 != fb.len() || !fb.iter().any(|f| matches!(f, Filter::Input(s) if *s == a.into)) {
        return None;
    }
    let types = Types { node: ta.node && tb.node, way: ta.way && tb.way, relation: ta.relation && tb.relation, area: false, derived: false };
    let mut filters = fa.clone();
    filters.extend(tags);
    Some(Stmt { kind: StmtKind::Query { types, filters }, into: b.into.clone(), line: a.line })
}

/// Every statement list that `exec_block` runs: the script and the bodies
/// of loops and `if`. The members of a union are not such a list: the
/// union reads the result of each.
fn for_each_block(stmts: &mut Vec<Stmt>, f: &mut dyn FnMut(&mut Vec<Stmt>)) {
    for s in stmts.iter_mut() {
        match &mut s.kind {
            StmtKind::Foreach { body, .. } | StmtKind::For { body, .. } | StmtKind::Complete { body, .. } => for_each_block(body, f),
            StmtKind::If { then, els, .. } => {
                for_each_block(then, f);
                for_each_block(els, f);
            }
            _ => {}
        }
    }
    f(stmts);
}

/// Replace each pair of statements `fused` accepts by its statement, when
/// the only reads of the set between them are the second statements of
/// such pairs (so no statement sees that the set is not stored).
pub fn fuse_filter_pairs(stmts: &mut Vec<Stmt>) {
    #[cfg(test)]
    if tests::FUSE_OFF.get() {
        return;
    }
    let mut reads = std::collections::HashMap::new();
    count_reads(stmts, &mut reads);
    let mut pairs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for_each_block(stmts, &mut |block| {
        for w in block.windows(2) {
            if fused(&w[0], &w[1]).is_some() {
                *pairs.entry(w[0].into.clone()).or_default() += 1;
            }
        }
    });
    pairs.retain(|set, n| reads.get(set) == Some(n));
    if pairs.is_empty() {
        return;
    }
    for_each_block(stmts, &mut |block| {
        let mut i = 0;
        while i + 1 < block.len() {
            match fused(&block[i], &block[i + 1]) {
                Some(f) if pairs.contains_key(&block[i].into) => {
                    #[cfg(test)]
                    tests::FUSED.set(tests::FUSED.get() + 1);
                    block[i] = f;
                    block.remove(i + 1);
                }
                _ => i += 1,
            }
        }
    });
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
        if i % 1024 == 0 {
            f.limits.check()?;
        }
        match if boxes { poly.rect_pos(&w.bbox) } else { RectPos::Border } {
            RectPos::Inside if !w.missing && !w.nodes.is_empty() => pass[i] = true,
            RectPos::Apart => {}
            _ => border.push(i),
        }
    }
    let refs: Vec<&Way> = border.iter().map(|&i| ways[i]).collect();
    let verts = super::geom::way_vertices(f, &refs)?;
    for i in border {
        f.limits.check()?;
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

/// Elements whose geometries a spatial test builds at a time (`query_type`).
fn geom_batch() -> usize {
    #[cfg(test)]
    if let Some(n) = tests::GEOM_BATCH.get() {
        return n;
    }
    1 << 16
}

/// Whether relation geometry for the spatial tests may leave out the
/// members far from every region; off in tests to compare.
fn reduce_rel_geoms(_plan: &Plan) -> bool {
    #[cfg(test)]
    return !tests::NEAR_GEOMS_OFF.get();
    #[cfg(not(test))]
    true
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

/// Ways an `around` source must have in a set before it takes them by
/// their stored box (`AroundSource::add_lazy`): the vertices of fewer are
/// looked up faster than their boxes are sorted out.
fn lazy_min() -> usize {
    #[cfg(test)]
    if let Some(n) = tests::LAZY_MIN.get() {
        return n;
    }
    2000
}

/// Level-16 tiles in each direction that the box of a lazy way of an
/// `around` source spans at most (about 2 km).
const LAZY_TILES: u32 = 3;

/// The box of a geometry for `AroundSource::lazy_near`, with the bulge of
/// its edges; None when it has no point that an `around` test can pass (a
/// way with a missing node never passes).
fn geom_box(g: &EGeom) -> Option<Rect> {
    let line = |b: &mut Rect, v: &[Option<crate::geo::Pt>]| {
        if v.iter().all(|p| p.is_some()) {
            v.iter().flatten().for_each(|p| b.extend(*p));
        }
    };
    let mut b = Rect::EMPTY;
    match g {
        EGeom::Point(p) => return Some(Rect::point(*p)),
        EGeom::Line(v) => line(&mut b, v),
        EGeom::Multi { points, lines } => {
            points.iter().for_each(|p| b.extend(*p));
            lines.iter().for_each(|l| line(&mut b, l));
        }
        EGeom::Empty => {}
    }
    (!b.is_empty()).then(|| b.arc_box())
}

/// `around_match` on the source and on the vertices of its lazy ways near
/// the element (`Engine::around_fine`).
fn around_match_with(src: &AroundSource, fine: Option<&AroundSource>, g: &EGeom, r: f64) -> bool {
    around_match(src, g, r) || fine.is_some_and(|f| around_match(f, g, r))
}

/// `region_cost` of the cover of an `around` source, with each segment
/// counted twice: the cover held a rectangle for each vertex and each
/// segment when the costs were set, and they decide between this region
/// and a bbox. Counted so, a cover of more than 2^20 tiles is not sorted
/// into distinct tiles (`tile_runs` does not either) and costs the sum
/// of its rectangles. With the plain count, the two rivers of a set
/// (10^5 segments, 10^6 tiles) cost less than a bbox over a canyon that
/// holds little data: the statement sorted the tiles three times and read
/// the rivers, 0.48 s for 0.25 s.
fn around_cost(src: &AroundSource, rects: &[Rect]) -> u64 {
    const MAX_TILES: u64 = 1 << 20;
    let segments = src.segments.len() as u64;
    let items = (rects.len() as u64).max(1);
    let tiles: u64 = rects.iter().filter(|r| !r.is_empty()).map(|r| ((r.y1 >> 16) - (r.y0 >> 16) + 1) as u64 * ((r.x1 >> 16) - (r.x0 >> 16) + 1) as u64).sum();
    // the share of the segments in the rectangles, once more
    let twice = |n: u64| n + (n as u128 * segments.min(items) as u128 / items as u128) as u64;
    if twice(tiles) > MAX_TILES {
        return twice(tiles) + twice(rects.len() as u64) / 16;
    }
    region_cost(rects) + segments / 16
}

/// `around` test per spec §6, on the points and segments of the source
/// (not its lazy ways: `around_match_with`).
pub fn around_match(src: &AroundSource, g: &EGeom, r: f64) -> bool {
    use crate::geo::{Segment, cartesian, great_circle_dist};
    // a source of lazy ways only (`around_match_with` tests those): its
    // index holds no item to test
    if src.points.is_empty() && src.segments.is_empty() {
        return false;
    }
    let np = src.points.len();
    let mut cand: Vec<u32> = Vec::new();
    // Two points are at least as far from each other as their latitudes,
    // and as one is from the meridian of the other (the arc sine of the
    // cosine of its latitude times the sine of the longitudes apart): past
    // the radius by more than the rounding of the distance (5 cm; from a
    // radius of 1 m on), the distance is not computed. A source with a
    // radius of hundreds of km has thousands of points in the grid cell of
    // each candidate.
    const DEG: f64 = 40_000_000.0 / 360.0;
    let reach = (r + 0.05) * (1.0 + 1e-9);
    let (lat_reach, sin_reach) = if r >= 1.0 && reach / crate::geo::ARC_TO_METER < 1.5 { (reach / DEG, (reach / crate::geo::ARC_TO_METER).sin() * (1.0 + 1e-12)) } else { (f64::INFINITY, f64::INFINITY) };
    // the ends of a segment are points of the source (`AroundSource`); the
    // first end of segment `i` only where no segment before it ends there
    let point_inside = |lat: f64, lon: f64| -> bool {
        // for the segments only: a source of nodes does not need it
        let cp = std::cell::OnceCell::new();
        let cos_lat = std::cell::OnceCell::new();
        let near_point = |c: &(f64, f64)| {
            if (c.0 - lat).abs() > lat_reach {
                return false;
            }
            let dlon = (c.1 - lon).abs();
            if dlon <= 90.0 && *cos_lat.get_or_init(|| lat.to_radians().cos()) * dlon.to_radians().sin() > sin_reach {
                return false;
            }
            (r > 0.0 && great_circle_dist(c.0, c.1, lat, lon) <= r) || ((c.0 - lat).abs() < 1e-7 && (c.1 - lon).abs() < 1e-7)
        };
        let near_segment = |i: usize, s: &Segment| {
            near_point(&s.b) || (src.first_end(i) && near_point(&s.a)) || s.line_dist(cp.get_or_init(|| cartesian(lat, lon))) <= r && {
                let ab = great_circle_dist(s.a.0, s.a.1, s.b.0, s.b.1);
                let lim = (ab * ab + r * r).sqrt();
                great_circle_dist(lat, lon, s.a.0, s.a.1) <= lim && great_circle_dist(lat, lon, s.b.0, s.b.1) <= lim
            }
        };
        match &src.index {
            None => src.points.iter().any(near_point) || src.segments.iter().enumerate().any(|(i, s)| near_segment(i, s)),
            // items past the segments are lazy ways
            Some(ix) => ix.at_point(lat, lon).any(|i| if (i as usize) < np { near_point(&src.points[i as usize]) } else { src.segments.get(i as usize - np).is_some_and(|s| near_segment(i as usize - np, s)) }),
        }
    };
    let seg_inside = |a: (f64, f64), b: (f64, f64), ca: crate::geo::V3, cb: crate::geo::V3, cand: &mut Vec<u32>| -> bool {
        let seg = Segment::with_cartesian(a, b, ca, cb);
        let ab = great_circle_dist(a.0, a.1, b.0, b.1);
        let lim = (ab * ab + r * r).sqrt();
        let near_at = |c: &(f64, f64), cc: &crate::geo::V3| seg.line_dist(cc) <= r && great_circle_dist(c.0, c.1, a.0, a.1) <= lim && great_circle_dist(c.0, c.1, b.0, b.1) <= lim;
        let near_point = |i: usize| near_at(&src.points[i], &src.point_c(i));
        let near_segment = |i: usize, t: &Segment| near_at(&t.b, &t.cb) || (src.first_end(i) && near_at(&t.a, &t.ca)) || t.intersects(&seg);
        match &src.index {
            Some(ix) if ix.near_segment(&seg, cand) => {
                cand.iter().any(|&i| if (i as usize) < np { near_point(i as usize) } else { src.segments.get(i as usize - np).is_some_and(|t| near_segment(i as usize - np, t)) })
            }
            _ => (0..np).any(near_point) || src.segments.iter().enumerate().any(|(i, t)| near_segment(i, t)),
        }
    };
    let way_match = |v: &[Option<crate::geo::Pt>], cand: &mut Vec<u32>| -> bool {
        if v.is_empty() || v.iter().any(|p| p.is_none()) {
            return false;
        }
        let pts: Vec<(f64, f64)> = v.iter().flatten().map(|p| (p.lat(), p.lon())).collect();
        if pts.iter().any(|p| point_inside(p.0, p.1)) {
            return true;
        }
        if pts.len() < 2 {
            return false;
        }
        // a vertex is an end of two segments
        let carts: Vec<crate::geo::V3> = pts.iter().map(|p| cartesian(p.0, p.1)).collect();
        (1..pts.len()).any(|k| seg_inside(pts[k - 1], pts[k], carts[k - 1], carts[k], cand))
    };
    match g {
        EGeom::Point(p) => point_inside(p.lat(), p.lon()),
        EGeom::Line(v) => way_match(v, &mut cand),
        EGeom::Multi { points, lines } => points.iter().any(|p| point_inside(p.lat(), p.lon())) || lines.iter().any(|l| way_match(l, &mut cand)),
        EGeom::Empty => false,
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
        /// Elements per batch of the geometry tests, in place of 65,536.
        pub(super) static GEOM_BATCH: Cell<Option<usize>> = const { Cell::new(None) };
        /// Turns off the reduced relation geometry (`rel_geoms_near`).
        pub(super) static NEAR_GEOMS_OFF: Cell<bool> = const { Cell::new(false) };
        /// Ways and relations of `query_type` that a polygon decided by
        /// their box, passed or failed (counted on).
        pub(super) static POLY_BOXES: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
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
        /// Where the member path took the nodes and ways of its last
        /// `around` clause: "found" (those of the statement) or "read".
        pub(super) static AROUND_MEMBERS: Cell<Option<&'static str>> = const { Cell::new(None) };
        /// `lazy_min` on this thread.
        pub(super) static LAZY_MIN: Cell<Option<usize>> = const { Cell::new(None) };
        /// Lazy ways of `around` sources whose vertices were loaded.
        pub(super) static LAZY_LOADED: Cell<usize> = const { Cell::new(0) };
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
        /// The filter whose candidates `index_candidates` read last.
        pub(super) static INDEX_FILTER: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
        /// Makes a region scan decode every element before the tag filters
        /// (`in_ranges_tagged`).
        pub(super) static RANGES_PLAIN: Cell<bool> = const { Cell::new(false) };
        /// `(area)` clauses that took the polygons of the clause before
        /// (`shared_area_ctx`).
        pub(super) static AREA_CTX_SHARED: Cell<usize> = const { Cell::new(0) };
        /// Statements that took the cell ranges of the shared `(area)`
        /// clause from the statement before (`area_ranges`).
        pub(super) static AREA_RANGES_SHARED: Cell<usize> = const { Cell::new(0) };
        /// In place of `AREA_KEEP_MAX`.
        pub(super) static AREA_KEEP_MAX: Cell<Option<u64>> = const { Cell::new(None) };
        /// Turns off `fuse_filter_pairs`.
        pub(in crate::exec) static FUSE_OFF: Cell<bool> = const { Cell::new(false) };
        /// Nodes meet their boxes as geometries after the scan, as ways do.
        pub(super) static NODE_BOX_PLAIN: Cell<bool> = const { Cell::new(false) };
        /// Pairs `fuse_filter_pairs` replaced since it was last reset.
        pub(in crate::exec) static FUSED: Cell<usize> = const { Cell::new(0) };
        /// Candidates of a first `[key]` filter over which
        /// `index_candidates` reads a regex, in place of 500,000.
        pub(super) static MANY: Cell<Option<usize>> = const { Cell::new(None) };
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

    /// The geometry tests of a statement run on batches of candidates,
    /// ordered by place when they are many: the answers are those of one
    /// batch, whatever its size.
    #[test]
    fn geometry_tests_in_batches() {
        let rels = relation(5000, &[("way", 1003), ("node", 466)], &[("type", "route")]) + &relation(5001, &[("way", 2020)], &[("type", "route")]) + &relation(5002, &[("node", 100_000)], &[]);
        let (db, dir) = grid_db("batches", &rels);
        let queries = [
            "way(poly:\"48.83 2.33 48.87 2.33 48.87 2.37 48.83 2.36\");out ids;",
            "nwr(poly:\"48.83 2.33 48.87 2.33 48.85 2.37\");out ids;",
            "way(around:300,48.85,2.35);out ids;",
            "nwr(around:150,48.84,2.34,48.86,2.36);out ids;",
            "way(1003)->.a;way(around.a:100);out ids;",
            "way(1003)->.a;nwr(around.a:250);out ids;",
            "way(48.8,2.3,48.9,2.4)->.a;way.a(48.83,2.33,48.85,2.35);out ids;",
            "way(48.8,2.3,48.9,2.4)->.a;way.a(poly:\"48.83 2.33 48.87 2.33 48.85 2.37\");out ids;",
            "way(48.83,2.33,48.85,2.35)(around:900,48.85,2.35);out ids;",
            "way(48.8,2.3,48.9,2.4)->.a;way(id:1003,1005)->.b;way.a(around.b:500);out ids;",
            "rel(48.83,2.33,48.85,2.35);out ids;",
            "rel(around:2000,48.85,2.35);out ids;",
        ];
        let whole: Vec<String> = queries.iter().map(|q| output(&db, q)).collect();
        assert!(whole[0].contains("<way id=\"1010\"/>") && whole[2].contains("<way id=\"3000\"/>"), "{}", whole[0]);
        for n in [1, 3, 7, 50] {
            GEOM_BATCH.set(Some(n));
            for (q, want) in queries.iter().zip(&whole) {
                assert_eq!(&output(&db, q), want, "{n} {q}");
            }
        }
        GEOM_BATCH.set(None);
        let _ = std::fs::remove_dir_all(dir);
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
        // A statement on sets: the relations of the sets are the candidates.
        let on_sets: &[(&str, &[&str])] = &[
            ("relation[route]->.r;nwr.r(around:500,48.85,2.35);out ids;", &["1", "3", "9", "11"]),
            ("relation[route]->.r;relation[\"route\"=\"bus\"]->.b;rel.r.b(around:500,48.85,2.35);out ids;", &["1", "9", "11"]),
            ("relation[\"type\"=\"multipolygon\"]->.m;rel.m(around:500,48.85,2.35);out ids;", &["8"]),
            ("relation[route];rel._[\"route\"=\"bus\"](around:600,48.85,2.35);out ids;", &["1", "9", "10", "11"]),
            ("rel.none(around:500,48.85,2.35);out ids;", &[]),
        ];
        for (q, ids) in on_sets {
            let q = format!("{c}{q}");
            let (out, did) = both_ways(&db, &q);
            assert_eq!((did, CANDIDATES_FROM.get()), (Some("answered"), Some("sets")), "{q}");
            assert_eq!(out, ids.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "{q}");
        }
        // and with a polygon: the answer of the statement with the tag
        // filter of the set in place of the set
        let poly = "(poly:\"48.84 2.34 48.86 2.34 48.86 2.36 48.84 2.36\")";
        for tags in ["[route]", "[\"route\"=\"bus\"]"] {
            let q = format!("{c}relation{tags}->.r;rel.r{poly};out ids;");
            let (out, did) = both_ways(&db, &q);
            assert_eq!((did, CANDIDATES_FROM.get()), (Some("answered"), Some("sets")), "{q}");
            let (direct, _) = both_ways(&db, &format!("{c}relation{tags}{poly};out ids;"));
            assert!(!direct.is_empty(), "{q}");
            assert_eq!(out, direct, "{q}");
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

    /// The member path of `around` takes the nodes and ways of a statement
    /// with that one clause and nothing else, and reads them without their
    /// tags otherwise: the relations are those of the candidate path and
    /// of the three statements by type.
    #[test]
    fn rel_around_members_from_the_statement() {
        let rels = [
            relation(1, &[("way", 1015)], &[("route", "bus")]),
            relation(2, &[("way", 1000)], &[("route", "bus")]),
            relation(3, &[("node", 466)], &[("route", "tram")]),
            relation(4, &[("way", 3001), ("node", 1)], &[("route", "bus")]),
            relation(5, &[("relation", 1)], &[("route", "bus")]),
            relation(6, &[("node", 999_998), ("way", 1029)], &[("route", "bus")]),
            relation(7, &[("way", 1000), ("way", 1029)], &[("route", "bus")]),
            relation(8, &[("way", 3000)], &[("type", "multipolygon")]),
        ]
        .concat();
        let (db, dir) = grid_db("found", &rels);
        let c = "[out:csv(::type,::id;false)];";
        let around = "(around:500,48.85,2.35)";
        // query, where the member path takes its nodes and ways
        let cases: &[(String, Option<&str>)] = &[
            (format!("nwr{around};out ids;"), Some("found")),
            (format!("(node{around};way{around};rel{around};);out ids;"), Some("read")),
            (format!("rel{around};out ids;"), Some("read")),
            (format!("nwr[route]{around};out ids;"), Some("read")),
            (format!("nwr{around}(48.84,2.34,48.86,2.36);out ids;"), Some("read")),
            (format!("nw{around};out ids;"), None),
            ("way(1015)->.w;nwr(around.w:50);out ids;".to_string(), Some("found")),
            ("way(1015)->.w;rel(around.w:50);out ids;".to_string(), Some("read")),
            ("node(466);nwr(around:100);out ids;".to_string(), Some("found")),
            (format!("nwr{around}(around:300,48.851,2.351);out ids;"), Some("read")),
            (format!("[bbox:48.84,2.34,48.86,2.36];nwr{around};out ids;"), Some("read")),
        ];
        let mut outs = Vec::new();
        for (q, from) in cases {
            let q = format!("{}{q}", if q.starts_with('[') { c.trim_end_matches(';') } else { c });
            AROUND_MEMBERS.set(None);
            let (members, _) = run_q(&db, &q, true);
            assert_eq!(AROUND_MEMBERS.get(), *from, "{q}");
            let (cands, _) = run_q(&db, &q, false);
            assert_eq!(members, cands, "{q}");
            outs.push(members);
        }
        assert!(outs[0].contains("relation\t1\n") && outs[0].contains("relation\t3\n") && outs[0].contains("relation\t8\n") && !outs[0].contains("relation\t2\n"), "{}", outs[0]);
        assert_eq!(outs[0], outs[1]);
        assert!(outs[6].contains("relation\t1\n"), "{}", outs[6]);
        assert!(outs[6].ends_with(&outs[7]), "{} {}", outs[6], outs[7]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A large set of ways as an `around` source is taken by the stored
    /// boxes of its ways, and the vertices are loaded for the ways near the
    /// candidates: the answers are those of the source with all vertices,
    /// on both paths of relations, in batches, and few ways are loaded for
    /// candidates in one corner.
    #[test]
    fn around_lazy_ways_match_loaded_ways() {
        let m = "{m}";
        // 150 small ways on the dense grid (rows and columns 20 apart): an
        // L of three nodes, every tenth closed, one with a missing node
        let dense = |i: usize, j: usize| 100_000 + i * 200 + j;
        let mut ways = String::new();
        for k in 0..150 {
            let (i, j) = (5 + (k / 15) * 20, 5 + (k % 15) * 13);
            let mut nds = vec![dense(i, j), dense(i + 3, j), dense(i + 3, j + 4)];
            if k % 10 == 0 {
                nds.push(dense(i, j));
            }
            if k == 77 {
                nds.push(999_997);
            }
            let nds: String = nds.iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect();
            ways += &format!("<way id=\"{}\" {m}>{nds}<tag k=\"small\" v=\"yes\"/></way>\n", 5000 + k);
        }
        // a way of one node, and one across the antimeridian
        ways += &format!("<way id=\"5200\" {m}><nd ref=\"{}\"/><tag k=\"small\" v=\"yes\"/></way>\n", dense(190, 190));
        ways += &format!("<way id=\"5201\" {m}><nd ref=\"300001\"/><nd ref=\"300002\"/><tag k=\"small\" v=\"yes\"/></way>\n");
        let nodes = format!("<node id=\"300001\" lat=\"10.0\" lon=\"179.9995\" {m}/>\n<node id=\"300002\" lat=\"10.0\" lon=\"-179.9995\" {m}/>\n<node id=\"300003\" lat=\"10.0003\" lon=\"180.0\" {m}><tag k=\"probe\" v=\"yes\"/></node>\n");
        let rels = [
            relation(1, &[("way", 1015)], &[("route", "bus")]),
            relation(2, &[("way", 1000)], &[("route", "bus")]),
            relation(3, &[("node", 466)], &[("route", "tram")]),
            relation(4, &[("node", dense(8, 5))], &[("route", "tram")]),
            relation(5, &[("way", 5003), ("way", 1029)], &[("route", "bus")]),
            relation(6, &[("way", 5077)], &[]),
            relation(7, &[("node", 300_003)], &[]),
        ]
        .concat();
        let (db, dir) = osm_db("lazy", &grid_osm_with(&nodes, &ways, &rels));
        let c = "[out:csv(::type,::id;false)];";
        let s = "way[small]->.s;";
        let queries = [
            format!("{s}node(around.s:10);out ids;"),
            format!("{s}node(around.s:0);out ids;"),
            format!("{s}way(around.s:0);out ids;"),
            format!("{s}way(around.s:12);out ids;"),
            format!("{s}nwr(around.s:30);out ids;"),
            format!("{s}nwr(around.s:0);out ids;"),
            format!("{s}rel(around.s:25);out ids;"),
            format!("{s}rel[route](around.s:60);out ids;"),
            format!("{s}nwr[highway](around.s:5);out ids;"),
            format!("{s}node(around.s:15)(48.845,2.345,48.85,2.35);out ids;"),
            format!("{s}node(around.s:20)(around:300,48.85,2.35);out ids;"),
            format!("{s}way[small](around.s:40);out ids;"),
            format!("{s}node(around.s:2000);out count;"),
            format!("{s}nwr[probe](around.s:100);out ids;"),
            format!("({s}node(466);way(1003);rel(3);)->.s;nwr(around.s:20);out ids;"),
            format!("{s}way(1010)->.t;node(around.s:20)(around.t:100);out ids;"),
            format!("{s}node(around.s:10)->.a;way[small](around.a:0);out ids;"),
        ];
        let mut loaded = 0;
        for q in &queries {
            let q = format!("{c}{q}");
            LAZY_MIN.set(Some(usize::MAX));
            let (want, _) = run_q(&db, &q, false);
            assert!(want.lines().count() > 0, "{q}");
            for (min, batch, off) in [(1, None, false), (1, None, true), (1, Some(7), false), (100, Some(40), true), (usize::MAX, None, true)] {
                LAZY_MIN.set(Some(min));
                GEOM_BATCH.set(batch);
                LAZY_LOADED.set(0);
                let (got, _) = run_q(&db, &q, off);
                assert_eq!(got, want, "{q} (lazy from {min} ways, batch {batch:?}, candidates off: {off})");
                assert!(min < 1000 || LAZY_LOADED.get() == 0, "{q}");
                // per source at most all ways once more than there are
                assert!(LAZY_LOADED.get() <= 2 * 2 * 153, "{q}: {} (batch {batch:?})", LAZY_LOADED.get());
                loaded += LAZY_LOADED.get();
            }
            GEOM_BATCH.set(None);
        }
        assert!(loaded > 1000, "{loaded}");
        // candidates in one corner of the grid: the ways near the nodes of
        // the tiles read for it, not all 152
        LAZY_MIN.set(Some(1));
        LAZY_LOADED.set(0);
        let out = output(&db, &format!("{c}{s}node(around.s:10)(48.84,2.34,48.8412,2.3412);out ids;"));
        assert!(out.lines().count() > 5, "{out}");
        assert!((1..=40).contains(&LAZY_LOADED.get()), "{}", LAZY_LOADED.get());
        LAZY_MIN.set(None);
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
    /// route near a small circle), also in a set: the member path answers.
    #[test]
    fn rel_around_many_members_use_member_path() {
        let mut members: Vec<(&str, usize)> = (1..=900).map(|n| ("node", n)).collect();
        members.extend((0..30).map(|i| ("way", 1000 + i)));
        members.extend((0..30).map(|j| ("way", 2000 + j)));
        let (db, dir) = grid_db("members", &relation(1, &members, &[("route", "bus")]));
        let (out, did) = both_ways(&db, "[out:csv(::id;false)];relation[\"route\"=\"bus\"](around:30,48.85,2.35);out ids;");
        assert_eq!(did, Some("too_many_members"));
        assert_eq!(out, vec!["1".to_string()]);
        // the same with the relation in a set
        let (out, did) = both_ways(&db, "[out:csv(::id;false)];relation[\"route\"=\"bus\"]->.r;rel.r(around:30,48.85,2.35);out ids;");
        assert_eq!((did, CANDIDATES_FROM.get()), (Some("too_many_members"), Some("sets")));
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
        // north of Paris; a way 200 degrees wide, taken across the
        // antimeridian; north of the antimeridian polygon below
        nodes += &format!("<node id=\"200014\" lat=\"49.5\" lon=\"2.35\" {m}/>\n<node id=\"200015\" lat=\"10.0\" lon=\"-100.0\" {m}/>\n<node id=\"200016\" lat=\"10.0\" lon=\"100.0\" {m}/>\n<node id=\"200017\" lat=\"10.5\" lon=\"179.9\" {m}/>\n");
        let way = |id: usize, nds: &[usize]| format!("<way id=\"{id}\" {m}>{}</way>\n", nds.iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect::<String>());
        let ways = [way(4000, &[200_001, 200_002, 200_003, 200_004, 200_005]), way(4001, &[200_006, 200_007, 200_008, 200_009, 200_010]), way(4002, &[200_011, 200_012]), way(4003, &[200_015, 200_016])].concat();
        let rels = [
            relation(21, &[("way", 1000), ("way", 4000)], &[("route", "bus")]),
            relation(22, &[("way", 4000), ("node", 200_013)], &[("route", "bus")]),
            relation(23, &[("way", 4002), ("way", 1015)], &[("route", "ferry")]),
            relation(24, &[("way", 4002)], &[("route", "ferry")]),
            relation(25, &[("way", 3001), ("way", 4001)], &[]),
            relation(26, &[("node", 466), ("way", 4001)], &[]),
            relation(27, &[("relation", 21), ("way", 4001)], &[]),
            relation(28, &[("way", 999_000), ("node", 1)], &[]),
            relation(29, &[("way", 4000), ("node", 200_014)], &[("route", "bus")]),
            relation(30, &[("way", 4003), ("node", 200_017)], &[]),
        ]
        .concat();
        let (db, dir) = osm_db("near-geoms", &grid_osm_with(&nodes, &ways, &rels));
        // polygons larger than rel_spatial_by_members takes: over Paris, and
        // across the antimeridian
        let big = "(poly:\"48.7 2.2 48.7 2.5 49.0 2.5 49.0 2.2\")";
        let anti = "(poly:\"9.8 179.8 9.8 -179.8 10.2 -179.8 10.2 179.8\")";
        let (paris, orleans) = ("(around:20000,48.85,2.35)", "(47.85,1.85,47.95,1.96)");
        // query, ids (None: not checked)
        let cases: &[(String, Option<&[&str]>)] = &[
            (format!("rel{paris};out ids;"), Some(&["21", "23", "26", "28"])),
            (format!("rel{paris}{orleans};out ids;"), Some(&["21", "26"])),
            ("rel(around:20000,47.9,1.9);out ids;".to_string(), Some(&["21", "22", "25", "26", "27", "29"])),
            ("rel(around:20000,47.9,1.9)(48.80,2.30,48.90,2.40);out ids;".to_string(), Some(&["21", "26"])),
            ("way(1000)->.w;rel(around.w:20000);out ids;".to_string(), Some(&["21", "23", "26", "28"])),
            (format!("rel(48.80,2.30,48.90,2.40){orleans};out ids;"), Some(&["21", "26"])),
            ("rel(around:20000,10.0,179.99);out ids;".to_string(), Some(&["23", "24"])),
            (format!("rel[route=bus]{paris};out geom;"), None),
            (format!("rel{paris};out center;"), None),
            (format!("[bbox:47.85,1.85,47.95,1.96];rel{paris};out bb;"), None),
            (format!("rel{big};out ids;"), Some(&["21", "23", "26", "28"])),
            (format!("rel{big}{orleans};out ids;"), Some(&["21", "26"])),
            (format!("rel{anti};out ids;"), Some(&["23", "24", "30"])),
            (format!("rel[route=bus]{big};out geom;"), None),
            (format!("nwr{big}[route];out center;"), None),
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
        // the way of relation 21 near Orleans is stored apart from the polygon
        output(&db, &format!("rel[route=bus]{big};out ids;"));
        assert_eq!(super::super::geom::NEAR_WAYS_READ.get(), 1);
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
                if b.may_cross() {
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
            let mut plain = AroundSource::default();
            plain.points = src.points.clone();
            plain.segments = src.segments.clone();
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

    /// A long segment is covered in pieces (`segment_spans`): every point
    /// and line the test passes lies in the rectangles and in the cells of
    /// the index, the index answers as the scan of all items, and a
    /// diagonal takes a fraction of the area of its one box.
    #[test]
    fn around_long_segments_are_covered_in_pieces() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let grow = |r: &Rect| Rect { y0: r.y0.saturating_sub(1), x0: r.x0.saturating_sub(1), y1: r.y1.saturating_add(1), x1: r.x1.saturating_add(1) };
        let area = |rs: &[Rect]| -> f64 { rs.iter().map(|r| (r.y1 - r.y0) as f64 * (r.x1 - r.x0) as f64).sum() };
        // radius, ends, whether the cover is cut
        let cases: &[(f64, (f64, f64), (f64, f64), bool)] = &[
            (25.0, (51.17571, 7.21023), (51.24529, 6.40653), true),
            (10_000.0, (48.5, 2.5), (46.0, 7.7), true),
            (0.0, (47.0, 8.0), (47.3, 8.4), true),
            (5000.0, (60.0, 10.0), (70.0, 40.0), true),
            (300.0, (-30.0, -60.0), (-35.0, -50.0), true),
            (300.0, (10.0, 170.0), (12.0, 179.5), true),
            // along a meridian, along a parallel, short, across the antimeridian, near a pole
            (300.0, (10.0, 20.0), (12.0, 20.001), false),
            (300.0, (10.0, 20.0), (10.001, 22.0), false),
            (300.0, (10.0, 20.0), (10.004, 20.004), false),
            (300.0, (10.0, 179.0), (12.0, -179.0), false),
            (300.0, (86.0, 20.0), (88.0, 60.0), false),
        ];
        for &(radius, a, b, cut) in cases {
            let mut src = AroundSource::default();
            src.add_line(&[a, b]);
            let rects = src.bbox_rects(radius).into_owned();
            assert_eq!(rects.len() > 2, cut, "{a:?} {b:?}");
            if cut {
                let one = Rect { y0: rects.iter().map(|r| r.y0).min().unwrap(), x0: rects.iter().map(|r| r.x0).min().unwrap(), y1: rects.iter().map(|r| r.y1).max().unwrap(), x1: rects.iter().map(|r| r.x1).max().unwrap() };
                assert!(area(&rects) < area(&[one]) / 3.0, "{a:?} {b:?}: {} pieces", rects.len());
            }
            // 39 more segments, so that the index is built
            let mut many = AroundSource::default();
            many.add_line(&[a, b]);
            for _ in 0..39 {
                let c = (a.0 + (rnd() - 0.5) * 2.0, a.1 + (rnd() - 0.5) * 0.5);
                many.add_line(&[c, (c.0 + (rnd() - 0.5) * 2.0, c.1 + (rnd() - 0.5) * 2.0)]);
            }
            let mut plain = AroundSource::default();
            plain.segments = many.segments.clone();
            many.prepare(radius);
            assert!(many.index.is_some());
            let many_rects = many.bbox_rects(radius);
            let deg = radius * 360.0 / 40_000_000.0;
            let mut hits = 0;
            for k in 0..3000 {
                // near the arc: within 1.5 radii of a point between the ends, and past the ends
                let t = rnd() * 1.02 - 0.01;
                let off = if k % 3 == 0 { 1.5 } else { 1.05 };
                // a point of the arc
                let (ca, cb) = (crate::geo::cartesian(a.0, a.1), crate::geo::cartesian(b.0, b.1));
                let v = ca.scale(1.0 - t).add(&cb.scale(t));
                let v = v.scale(1.0 / v.dot(&v).sqrt());
                let (alat, alon) = (v.0.asin().to_degrees(), v.1.atan2(v.2).to_degrees());
                let lat = alat + (rnd() - 0.5) * 2.0 * off * deg + (rnd() - 0.5) * 0.02 * (k % 2) as f64;
                let lon = alon + (rnd() - 0.5) * 2.0 * off * deg / lat.to_radians().cos();
                let p = Pt::from_ll(lat.clamp(-89.9, 89.9), if lon > 180.0 { lon - 360.0 } else if lon < -180.0 { lon + 360.0 } else { lon });
                let q = Pt::from_ll((p.lat() + (rnd() - 0.5) * 0.01).clamp(-89.9, 89.9), (p.lon() + (rnd() - 0.5) * 0.01).clamp(-180.0, 180.0));
                for g in [EGeom::Point(p), EGeom::Line(vec![Some(p), Some(q)])] {
                    let hit = around_match(&src, &g, radius);
                    let mut bx = Rect::point(p);
                    if matches!(g, EGeom::Line(_)) {
                        bx.extend(q);
                    }
                    let bx = bx.arc_box();
                    if hit {
                        assert!(rects.iter().any(|r| grow(r).intersects(&bx)), "{g:?} outside the cover of {a:?} {b:?}");
                        assert!(many.near_box(&many_rects, &bx), "{g:?} outside the cells of {a:?} {b:?}");
                    }
                    assert_eq!(around_match(&plain, &g, radius), around_match(&many, &g, radius), "{g:?} {a:?} {b:?}");
                    hits += hit as usize;
                }
            }
            assert!(hits > 100, "{a:?} {b:?}: {hits}");
        }
    }

    /// The vertices of a line are tested once, with the segment that ends
    /// there: the answers are those of segments tested at both ends, with
    /// and without the index.
    #[test]
    fn around_line_vertices_are_tested_once() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        for (radius, step, n) in [(30.0, 0.002, 80), (0.0, 0.002, 80), (500.0, 0.02, 80), (30.0, 0.002, 6), (2000.0, 0.3, 40)] {
            // two lines of n vertices each, and a node
            let mut line = AroundSource::default();
            let mut both_ends = AroundSource::default();
            for l in 0..2 {
                let mut p = (47.0 + l as f64 * step, 8.0);
                let mut pts = vec![p];
                for _ in 1..n {
                    p = (p.0 + (rnd() - 0.5) * step, p.1 + rnd() * step);
                    pts.push(p);
                }
                line.add_line(&pts);
                for w in pts.windows(2) {
                    both_ends.segments.push(crate::geo::Segment::new(w[0].0, w[0].1, w[1].0, w[1].1));
                }
            }
            line.points.push((47.0, 8.0 + step));
            both_ends.points.push((47.0, 8.0 + step));
            assert_eq!(line.segments.len(), 2 * (n - 1));
            assert!(line.first_end(0) && !line.first_end(1) && line.first_end(n - 1) && !line.first_end(n));
            let mut indexed = AroundSource::default();
            indexed.points = line.points.clone();
            for l in 0..2 {
                let pts: Vec<(f64, f64)> = std::iter::once(line.segments[l * (n - 1)].a).chain(line.segments[l * (n - 1)..(l + 1) * (n - 1)].iter().map(|s| s.b)).collect();
                indexed.add_line(&pts);
            }
            indexed.prepare(radius);
            assert_eq!(indexed.index.is_some(), n > 20);
            let deg = radius * 360.0 / 40_000_000.0;
            let mut hits = 0;
            for k in 0..6000 {
                // near a vertex (also exactly on it), and anywhere near the lines
                let v = line.segments[(rnd() * line.segments.len() as f64) as usize];
                let v = if k % 2 == 0 { v.a } else { v.b };
                let p = match k % 3 {
                    0 => Pt::from_ll(v.0 + (rnd() - 0.5) * 2.2 * deg, v.1 + (rnd() - 0.5) * 3.2 * deg),
                    1 => Pt::from_ll(v.0, v.1),
                    _ => Pt::from_ll(47.0 + (rnd() - 0.5) * step * 4.0, 8.0 + rnd() * step * n as f64 * 0.5),
                };
                let q = Pt::from_ll(p.lat() + (rnd() - 0.5) * step * 0.2, p.lon() + (rnd() - 0.5) * step * 0.2);
                for g in [EGeom::Point(p), EGeom::Line(vec![Some(p), Some(q)]), EGeom::Line(vec![Some(q), Some(Pt::from_ll(q.lat() + deg, q.lon()))])] {
                    let want = around_match(&both_ends, &g, radius);
                    assert_eq!(around_match(&line, &g, radius), want, "{g:?} radius {radius}");
                    assert_eq!(around_match(&indexed, &g, radius), want, "{g:?} radius {radius} (index)");
                    hits += want as usize;
                }
            }
            assert!(hits > 500 && hits < 17_500, "radius {radius}: {hits}");
        }
    }

    /// The bounds that spare the distance of two points (their latitudes,
    /// the meridian of one) never change the answer of the distance
    /// itself: points at the radius and close to it on both sides, in all
    /// directions, for radii from under a metre to a quarter of the globe,
    /// up to high latitudes and across the antimeridian.
    #[test]
    fn around_point_bounds_keep_the_answers() {
        use crate::geo::great_circle_dist;
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let exact = |c: (f64, f64), p: Pt, r: f64| (r > 0.0 && great_circle_dist(c.0, c.1, p.lat(), p.lon()) <= r) || ((c.0 - p.lat()).abs() < 1e-7 && (c.1 - p.lon()).abs() < 1e-7);
        let (mut inside, mut outside) = (0, 0);
        for radius in [0.0f64, 0.3, 1.0, 1.5, 25.0, 300.0, 3000.0, 50_000.0, 666_888.0, 3_000_000.0, 9_000_000.0, 12_000_000.0] {
            for (lat, lon) in [(0.0, 0.0), (47.0, 8.0), (-33.0, 151.0), (10.0, 179.9999), (84.0, -30.0), (-89.0, 100.0), (63.5, -179.99)] {
                let mut src = AroundSource::default();
                src.points.push((lat, lon));
                // a segment far away, so that the segment tests run too
                src.add_line(&[(-lat * 0.5 + 1.0, lon * 0.5 + 2.0), (-lat * 0.5 + 1.001, lon * 0.5 + 2.001)]);
                let deg = radius * 360.0 / 40_000_000.0;
                for k in 0..4000 {
                    // around the radius: far, near, and within the rounding of the coordinates
                    let f = match k % 4 {
                        0 => rnd() * 2.0,
                        1 => 1.0 + (rnd() - 0.5) * 1e-3,
                        2 => 1.0 + (rnd() - 0.5) * 1e-6,
                        _ => 1.0 + (rnd() - 0.5) * 4e-7 / deg.max(1e-7),
                    };
                    let dir = rnd() * std::f64::consts::TAU;
                    let plat = (lat + dir.sin() * deg * f).clamp(-90.0, 90.0);
                    let scale = plat.to_radians().cos().max(1e-6);
                    let mut plon = lon + dir.cos() * deg * f / scale;
                    while plon > 180.0 {
                        plon -= 360.0;
                    }
                    while plon < -180.0 {
                        plon += 360.0;
                    }
                    let p = Pt::from_ll(plat, plon);
                    let want = exact((lat, lon), p, radius) || exact(src.segments[0].a, p, radius) || exact(src.segments[0].b, p, radius);
                    let far = great_circle_dist(src.segments[0].a.0, src.segments[0].a.1, p.lat(), p.lon()) > radius + 1000.0;
                    if far {
                        assert_eq!(around_match(&src, &EGeom::Point(p), radius), want, "radius {radius} at {lat} {lon}: {} {}", p.lat(), p.lon());
                        inside += want as usize;
                        outside += !want as usize;
                    }
                }
            }
        }
        assert!(inside > 20_000 && outside > 20_000, "{inside} {outside}");
    }

    /// The cost of an `around` cover: that of its distinct tiles and its
    /// rectangles while it is small, with the segments counted twice; the
    /// sum of the rectangles, unsorted, from 2^20 tiles on.
    #[test]
    fn around_cost_of_small_and_large_covers() {
        let line = |n: usize| {
            let mut src = AroundSource::default();
            let pts: Vec<(f64, f64)> = (0..=n).map(|k| (47.0 + k as f64 * 0.0002, 8.0 + (k % 7) as f64 * 0.0001)).collect();
            src.add_line(&pts);
            src.prepare(1000.0);
            src
        };
        // 1000 segments along 20 km: a few hundred distinct tiles
        let small = line(1000);
        let rects = small.bbox_rects(1000.0);
        let cost = around_cost(&small, &rects);
        assert_eq!(cost, region_cost(&rects) + 1000 / 16);
        assert!(cost < 2000, "{cost}");
        // 40,000 segments of 20 tiles each: under 2^20 tiles, over it counted
        // twice, where the distinct tiles are a few thousand
        let large = line(40_000);
        let rects = large.bbox_rects(1000.0);
        let cost = around_cost(&large, &rects);
        assert!(cost > 1 << 20 && region_cost(&rects) < 100_000, "{cost} {}", region_cost(&rects));
        // 60,000: `region_cost` does not sort them either
        let large = line(60_000);
        let rects = large.bbox_rects(1000.0);
        assert_eq!(around_cost(&large, &rects), 2 * region_cost(&rects));
        // nodes count once
        let mut nodes = AroundSource::default();
        nodes.points = (0..1000).map(|k| (47.0 + k as f64 * 0.001, 8.0)).collect();
        nodes.prepare(100.0);
        let rects = nodes.bbox_rects(100.0);
        assert_eq!(around_cost(&nodes, &rects), region_cost(&rects));
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

    /// Comparisons of `lat()` and `lon()` of a node with a literal give
    /// what the same comparison of the strings gives (`Evaluator::cmp_fast`;
    /// `lat()+""` is the same string on the general path).
    #[test]
    fn coordinate_comparisons_as_numbers_equal_the_strings() {
        let (db, dir) = grid_db("cmp-fast", "");
        let mut entries = Vec::new();
        for lit in ["48.85", "48.8500000", "48.850", "48.8499999", "2.35", "2.3500001", "48", "\"\"", "\"abc\"", "\" 48.85\"", "\"48.85 \"", "\"0x30\"", "\"4.885e1\"", "\"nan\"", "\"inf\"", "-0"] {
            for op in ["<", "<=", ">", ">=", "==", "!="] {
                for f in ["lat()", "lon()"] {
                    let k = entries.len();
                    entries.push(format!("a{k}=({f}{op}{lit}?1:0),b{k}=(({f}+\"\"){op}{lit}?1:0),c{k}={lit}{op}{f},d{k}={lit}{op}({f}+\"\")"));
                }
            }
        }
        let n = entries.len();
        let cols: Vec<String> = (0..n).flat_map(|k| [format!("a{k}"), format!("b{k}"), format!("c{k}"), format!("d{k}")]).collect();
        let q = format!("[out:csv({};false)];node(48.846,2.346,48.854,2.354);convert r {};out;", cols.join(","), entries.join(","));
        let out = output(&db, &q);
        assert!(out.lines().count() > 20, "{out}");
        let (mut ones, mut zeros) = (0, 0);
        for line in out.lines() {
            let v: Vec<&str> = line.split('\t').collect();
            assert_eq!(v.len(), 4 * n);
            for k in 0..n {
                assert_eq!(v[4 * k], v[4 * k + 1], "{} in {line}", entries[k]);
                assert_eq!(v[4 * k + 2], v[4 * k + 3], "{} in {line}", entries[k]);
                ones += (v[4 * k] == "1") as usize;
                zeros += (v[4 * k] == "0") as usize;
            }
        }
        assert!(ones > 100 && zeros > 100);
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// rv-scan: tag filters give the answers of filtering a set in memory
    /// on every path: region scans with key and value tests of the chunk
    /// (`in_ranges_tagged`, and with `RANGES_PLAIN`), the tag index on one
    /// and on several threads (`FirstPairs`), for keys and values with
    /// non-ASCII letters, empty keys and values, keys inside longer keys,
    /// long (truncated) values, regexes that match the empty string, `,i`.
    #[test]
    #[ignore = "randomized, about 5 s: cargo test --release --lib rv_tag_filters_on_all_paths -- --ignored"]
    fn rv_tag_filters_on_all_paths() {
        use crate::exec::fetch::PAR_SCAN_VALUES;
        let mut x = 0x5151_2468_ace0_1357u64;
        let mut next = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) % n.max(1)
        };
        let long1 = "x".repeat(250);
        let long2 = format!("{}Zürich", "y".repeat(201));
        let keys = ["name", "name:en", "Name", "NAME", "highway", "high", "addr:street", "k", "", "ü", "Ü", "straße", "STRASSE", "a.b", "a+b", "x y", "ключ", "name_old", "e\\f", "q\"t"];
        let vals = ["", "a", "ab", "Zürich", "zürich", "ZÜRICH", "Main Street", "main street", "MAIN", "Straße", "STRASSE", "İstanbul", "istanbul", "ıi", "Kelvin", "\u{212a}elvin", "ſea", "Sea", "улица", "УЛИЦА", "日本", "🙂", long1.as_str(), long2.as_str(), "a\u{301}", "ǅ", "ǆ", "Ǆ", "1", "12", "a.b", "a*b", "line\nbreak", "e\\f", "q\"t", "primary", "secondary"];
        let res = ["^Zür", "zürich", "ZÜRICH", "^$", ".", "^.$", "^..?$", "a", "ab", "^a", "b$", "Sea", "sea", "kelvin", "^K", "straße", "STRASSE", "ss", "улица", "^У", "日", "🙂", "x{3}", "^x+$", "(Main|Sea)", "[Mm]ain", "Mai?n", ".*", "a*", "^(a|ab)$", "[[:alpha:]]+", "[[:upper:]]", "\\.", "a\\*b", "İ", "ı", "i", "I", "ǅ", "ǆ", "\u{301}", "line.break", "e\\\\f", "q\"t", "Zürich$", "^y+Z", "rich", "^prim", "ary$", "^ZÜR", "ICH$"];
        let kres = ["^name", "name:", ":en$", "^.ame$", "^NAME$", "^$", "high", "^high$", "^k$", "^ü$", "e\\\\f", ".", "^STR", "straße", "^a.b$", "^a\\+b$", "x y", "ключ", "q\"t", "^n", "AME"];
        let xml = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\n', "&#10;");
        let ql = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let tags = |next: &mut dyn FnMut(u64) -> u64| -> String {
            let mut t = String::new();
            let mut used = Vec::new();
            for _ in 0..next(5) {
                let k = keys[next(keys.len() as u64) as usize];
                if used.contains(&k) {
                    continue;
                }
                used.push(k);
                t += &format!("<tag k=\"{}\" v=\"{}\"/>", xml(k), xml(vals[next(vals.len() as u64) as usize]));
            }
            t
        };
        let mut nodes = String::new();
        let n = 15_000u64;
        for i in 0..n {
            let (lat, lon) = (46.0 + next(400_000) as f64 * 1e-6, 7.0 + next(400_000) as f64 * 1e-6);
            let t = if next(2) == 0 { tags(&mut next) } else { String::new() };
            nodes += &format!("<node id=\"{}\" lat=\"{lat:.6}\" lon=\"{lon:.6}\" {{m}}>{t}</node>\n", 300_000 + i);
        }
        let mut ways = String::new();
        for w in 0..2500u64 {
            let first = next(n);
            let nds: String = (0..2 + next(3)).map(|k| format!("<nd ref=\"{}\"/>", 300_000 + if next(8) == 0 { next(n) } else { (first + k) % n })).collect();
            ways += &format!("<way id=\"{}\" {{m}}>{nds}{}</way>\n", 10_000 + w, tags(&mut next));
        }
        let mut rels = String::new();
        for r in 0..300u64 {
            let mem: String = (0..1 + next(3)).map(|_| if next(2) == 0 { format!("<member type=\"node\" ref=\"{}\" role=\"\"/>", 300_000 + next(n)) } else { format!("<member type=\"way\" ref=\"{}\" role=\"\"/>", 10_000 + next(2500)) }).collect();
            rels += &format!("<relation id=\"{}\" {{m}}>{mem}{}</relation>\n", 20_000 + r, tags(&mut next));
        }
        let (db, dir) = osm_db("rv-tags", &grid_osm_with(&nodes, &ways, &rels));
        let run = |q: &str| -> Option<String> {
            let opts = crate::exec::RunOptions { cli: true, ..Default::default() };
            match crate::exec::run(&db, q, Vec::new(), &opts) {
                Ok(p) => Some(String::from_utf8(p.w).unwrap()),
                Err((Some(p), _)) => Some(String::from_utf8(p.w).unwrap()),
                Err((None, _)) => None,
            }
        };
        let c = "[out:csv(::type,::id;false)];";
        let (mut compared, mut nonempty, mut skipped) = (0, 0, 0);
        for round in 0..700 {
            let mut f = String::new();
            let mut positive = false;
            for _ in 0..1 + next(3) {
                let k = ql(keys[next(keys.len() as u64) as usize]);
                let v = ql(vals[next(vals.len() as u64) as usize]);
                let re = ql(res[next(res.len() as u64) as usize]);
                let ci = if next(2) == 0 { ",i" } else { "" };
                f += &match next(9) {
                    0 => { positive = true; format!("[\"{k}\"]") }
                    1 => format!("[!\"{k}\"]"),
                    2 => { positive = true; format!("[\"{k}\"=\"{v}\"]") }
                    3 => format!("[\"{k}\"!=\"{v}\"]"),
                    4 | 5 => { positive = true; format!("[\"{k}\"~\"{re}\"{ci}]") }
                    6 => format!("[\"{k}\"!~\"{re}\"{ci}]"),
                    _ => { positive = true; format!("[~\"{}\"~\"{re}\"{ci}]", ql(kres[next(kres.len() as u64) as usize])) }
                };
            }
            let ty = ["node", "way", "rel", "nwr"][next(4) as usize];
            let (s, w) = (46.0 + next(37) as f64 * 0.01, 7.0 + next(37) as f64 * 0.01);
            let size = [0.03, 0.15, 0.5][next(3) as usize];
            let bbox = format!("({s:.2},{w:.2},{:.2},{:.2})", s + size, w + size);
            PAR_SCAN_VALUES.set(if round % 3 == 0 { Some(2) } else { None });
            FUSE_OFF.set(true);
            let reference = run(&format!("{c}{ty}{bbox}->.s;{ty}.s{f};out;"));
            FUSE_OFF.set(false);
            let Some(reference) = reference else {
                skipped += 1;
                continue;
            };
            let q = format!("{c}{ty}{f}{bbox};out;");
            let tagged = run(&q).unwrap();
            RANGES_PLAIN.set(true);
            let plain = run(&q).unwrap();
            RANGES_PLAIN.set(false);
            assert_eq!(plain, reference, "round {round}: {q} (plain)");
            assert_eq!(tagged, reference, "round {round}: {q}");
            compared += 1;
            nonempty += !reference.is_empty() as usize;
            if positive {
                FUSE_OFF.set(true);
                let reference = run(&format!("{c}({ty}(-90,-180,90,180);way(3001);)->.s;{ty}.s{f};out;")).unwrap();
                FUSE_OFF.set(false);
                let q = format!("{c}{ty}{f};out;");
                let global = run(&q).unwrap();
                if !global.contains("Filters too weak") {
                    assert_eq!(global, reference, "round {round}: {q}");
                    compared += 1;
                }
            }
        }
        PAR_SCAN_VALUES.set(None);
        assert!(compared > 700 && nonempty > 150 && skipped < 100, "{compared} compared, {nonempty} not empty, {skipped} skipped");
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A region scan that tests the tags before it decodes an element gives
    /// the answers of decoding every element first.
    #[test]
    fn region_scan_tests_tags_first() {
        let rels = [
            relation(1, &[("way", 4001), ("node", 200_010)], &[("type", "route"), ("route", "bus"), ("name", "Line 1")]),
            relation(2, &[("way", 4002)], &[("type", "route"), ("route", "tram")]),
            relation(3, &[("node", 200_020)], &[("type", "site"), ("name", "Beta site")]),
        ]
        .concat();
        let m = "{m}";
        let mut nodes = String::new();
        for k in 0..400 {
            let place = if k % 40 == 7 { "<tag k=\"place\" v=\"town\"/>" } else { "" };
            let (lat, lon) = (48.84805 + (k / 20) as f64 * 0.0002, 2.34805 + (k % 20) as f64 * 0.0002);
            nodes += &format!("<node id=\"{}\" lat=\"{lat:.5}\" lon=\"{lon:.5}\" {m}><tag k=\"name\" v=\"{}{k}\"/>{place}</node>\n", 200_001 + k, ["Alpha", "alpha", "Beta", "Gamma"][k % 4]);
        }
        let mut ways = String::new();
        for i in 0..20 {
            let nds: String = (0..20).map(|j| format!("<nd ref=\"{}\"/>", 200_001 + i * 20 + j)).collect();
            ways += &format!("<way id=\"{}\" {m}>{nds}<tag k=\"highway\" v=\"{}\"/><tag k=\"name\" v=\"Way {i}\"/></way>\n", 4000 + i, ["primary", "secondary", "service"][i % 3]);
        }
        let (db, dir) = osm_db("scan-tags", &grid_osm_with(&nodes, &ways, &rels));
        let c = "[out:csv(::type,::id,name;false)];";
        let bbox = "(48.84,2.34,48.86,2.36)";
        for (q, n) in [
            (format!("nwr[name]{bbox};out;"), 422),
            (format!("nwr[name][name~\"^alpha\",i]{bbox};out;"), 200),
            (format!("nwr[name~\"Way 1|Beta\"]{bbox};out;"), 112),
            (format!("node[place]{bbox};out;"), 10),
            (format!("node[!place][name~\"7$\"]{bbox};out;"), 30),
            (format!("way[highway][highway!=\"residential\"]{bbox};out;"), 20),
            (format!("way[highway~\"^(primary|service)$\"][name]{bbox};out;"), 13),
            (format!("way[highway!~\"res\"]{bbox};out;"), 20),
            (format!("way[~\"^high\"~\"^sec\"]{bbox};out;"), 7),
            (format!("rel[route]{bbox};out;"), 2),
            (format!("rel[type][!route]{bbox};out;"), 1),
            (format!("nwr[name][nosuchkey]{bbox};out;"), 0),
            (format!("nwr[name~\"nosuchvalue\"]{bbox};out;"), 0),
            ("node[name](around:30,48.85,2.35);out;".to_string(), usize::MAX),
        ] {
            RANGES_PLAIN.set(true);
            let plain = output(&db, &format!("{c}{q}"));
            RANGES_PLAIN.set(false);
            let tagged = output(&db, &format!("{c}{q}"));
            assert_eq!(tagged, plain, "{q}");
            assert!(tagged.lines().count() == n || (n == usize::MAX && tagged.lines().count() > 0), "{q}: {}", tagged.lines().count());
        }
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Statements that follow each other with the same area set build its
    /// polygons once; another set, or the set stored again, builds anew.
    #[test]
    fn area_polygons_shared_by_consecutive_statements() {
        let id = |i: usize, j: usize| i * 30 + j + 1;
        let (mut ways, mut rels) = (String::new(), String::new());
        for k in 0..4 {
            let (i, j) = (k * 4, k * 4);
            let nds: String = [id(i, j), id(i, j + 3), id(i + 3, j + 3), id(i + 3, j), id(i, j)].iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect();
            ways += &format!("<way id=\"{}\" {{m}}>{nds}</way>\n", 5000 + k);
            let name = format!("Town {k}");
            rels += &relation(10 + k, &[("way", 5000 + k)], &[("type", "boundary"), ("boundary", "administrative"), ("admin_level", "8"), ("name", name.as_str())]);
        }
        let (db, dir) = osm_db("area-shared", &grid_osm_with("", &ways, &rels));
        crate::update::areas::make_areas(&db).unwrap();
        let c = "[out:csv(::type,::id;false)];";
        let run = |q: &str| {
            AREA_CTX_SHARED.set(0);
            let out = output(&db, &format!("{c}{q}"));
            (out, AREA_CTX_SHARED.get())
        };
        let (one, n) = run("area[name=\"Town 1\"]->.a;nwr(area.a);out ids;");
        assert_eq!(n, 0);
        assert!(one.lines().count() > 10, "{one}");
        // the members of a union, and the statement after it
        AREA_RANGES_SHARED.set(0);
        let (three, n) = run("area[name=\"Town 1\"]->.a;(node(area.a);way(area.a);rel(area.a););out ids;way(area.a);out ids;");
        assert_eq!(n, 3);
        // the ranges of the ways for the relations and for the ways a
        // second time; with another clause, not shared
        assert_eq!(AREA_RANGES_SHARED.get(), 2);
        let (boxed, _) = run("area[name=\"Town 1\"]->.a;way(area.a);out ids;way(area.a)(48.82,2.32,48.88,2.38);out ids;");
        assert_eq!(AREA_RANGES_SHARED.get(), 2);
        assert_eq!(boxed.lines().count() % 2, 0, "{boxed}");
        assert!(three.starts_with(&one), "{three}");
        // by id, with the same default set
        let (by_id, n) = run("node(area:3600000011);out ids;way(area:3600000011);out ids;");
        assert_eq!((by_id.lines().count(), n), (one.lines().filter(|l| !l.starts_with("relation")).count(), 0));
        let (by_id2, n) = run("area[name=\"Town 2\"];node(area:3600000011)->.x;way(area:3600000011)->.y;(.x;.y;);out ids;");
        assert_eq!((by_id2, n), (by_id, 1));
        // another set, and the same name stored again
        let (two, n) = run("area[name=\"Town 1\"]->.a;area[name=\"Town 2\"]->.b;node(area.a);out ids;node(area.b);out ids;node(area.a);out ids;");
        assert_eq!(n, 0);
        let (again, n) = run("area[name=\"Town 1\"]->.a;node(area.a);out ids;area[name=\"Town 2\"]->.a;node(area.a);out ids;area[name=\"Town 1\"]->.a;node(area.a);out ids;");
        assert_eq!((again, n), (two, 0));
        // over the cap the polygons go with their statement, with the same answers
        AREA_KEEP_MAX.set(Some(0));
        AREA_RANGES_SHARED.set(0);
        let (capped, n) = run("area[name=\"Town 1\"]->.a;(node(area.a);way(area.a);rel(area.a););out ids;way(area.a);out ids;");
        AREA_KEEP_MAX.set(None);
        assert_eq!((capped, n, AREA_RANGES_SHARED.get()), (three, 0, 0));
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A query into a set that only a tag query on that set reads runs as
    /// one statement, with the answers of the two; other pairs stay.
    #[test]
    fn filter_pairs_fuse_to_the_same_answers() {
        let (db, dir) = tagged_grid("fuse");
        let c = "[out:csv(::type,::id,name;false)];";
        let bbox = "(48.84,2.34,48.86,2.36)";
        let run = |q: &str| {
            // settings of the case go behind the output format
            let q = match q.strip_prefix("[bbox") {
                Some(rest) => format!("{}[bbox{rest}", c.trim_end_matches(';')),
                None => format!("{c}{q}"),
            };
            FUSE_OFF.set(true);
            let plain = output(&db, &q);
            FUSE_OFF.set(false);
            FUSED.set(0);
            let fused = output(&db, &q);
            assert_eq!(fused, plain, "{q}");
            assert!(!fused.contains("error"), "{q}: {fused}");
            (fused.lines().count(), FUSED.get())
        };
        // fused: the set is read by the tag query only
        for (q, n, pairs) in [
            (format!("nwr[name]{bbox}->.a;nwr.a[name~\"^alpha\",i];out;"), 200, 1),
            (format!("nwr[name]{bbox}->.a;node.a[name~\"7$\"][place];out;"), 10, 1),
            (format!("node[name]{bbox}->.a;nwr.a[name~\"Way|Beta\"];out;"), 100, 1),
            (format!("way[highway]{bbox}->.a;way.a[name][highway!=\"primary\"]->.b;.b out;"), 13, 1),
            // twice in a script, the set written again in between
            (format!("nwr[name]{bbox}->.a;nwr.a[name~\"^Way\"];out;nwr[place]{bbox}->.a;nwr.a[name~\"^Beta\"];out;"), 20, 2),
            // a chain
            (format!("nwr[name]{bbox}->.a;nwr.a[name~\"a\"]->.b;nwr.b[place=\"town\"];out;"), 10, 2),
            // inside a loop body
            ("way[name=\"Way 2\"];foreach(node(w)[name]->.n;node.n[place];out;);".to_string(), 3, 1),
            // no element type in common
            (format!("node[name]{bbox}->.a;way.a[name~\"a\"];out;"), 0, 1),
        ] {
            assert_eq!(run(&q), (n, pairs), "{q}");
        }
        // not fused
        for q in [
            // the set is read again
            format!("nwr[name]{bbox}->.a;nwr.a[name~\"^alpha\",i];out;.a out ids;"),
            format!("nwr[name]{bbox}->.a;nwr.a[name~\"^Way\"];out;node(w.a);out ids;"),
            format!("nwr[name]{bbox}->.a;nwr.a[name~\"^Way\"];out;make n c=a.count(ways);out;"),
            // the default set
            format!("nwr[name]{bbox};nwr._[name~\"^Way\"];out;"),
            // a union reads its members
            format!("(nwr[name]{bbox}->.a;nwr.a[name~\"^Way\"];);out;"),
            // more than tag filters on the set
            format!("nwr[name]{bbox}->.a;nwr.a[name~\"^Way\"](48.84,2.34,48.85,2.35);out;"),
            format!("nwr[name]{bbox}->.a;nwr.a;out;"),
            // a standalone criterion, which ignores the global bbox
            "[bbox:48.84,2.34,48.85,2.35];node(48.84,2.34,48.86,2.36)->.a;node.a[name~\"^alpha\",i];out;".to_string(),
            // only a negative tag filter: \"filters too weak\"
            "way[!highway]->.a;way.a[name];out;".to_string(),
        ] {
            assert_eq!(run(&q).1, 0, "{q}");
        }
        // with a global bbox
        assert_eq!(run(&format!("[bbox:48.848,2.348,48.849,2.349];node[name]->.a;node.a[name~\"^alpha\",i];out;")).1, 1);
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Nested ternaries over `lat()` or `lon()` with literals in order are
    /// searched by bisection (`Evaluator::chain`): the values of the
    /// evaluation level by level, for ascending and descending levels, ties,
    /// levels out of order, branches that are expressions, other elements
    /// than nodes, and as the key of `for`.
    #[test]
    fn ternary_chains_by_bisection_equal_the_levels() {
        use crate::exec::eval::{CHAINS_OFF, CHAIN_HITS};
        let (db, dir) = grid_db("chains", "");
        // levels 0.0005 apart from 48.8395, 2.3395: on node positions and between
        let chain = |f: &str, op: &str, from: f64, step: f64, n: usize, leaf: &dyn Fn(usize) -> String| {
            let mut e = leaf(n);
            for k in (0..n).rev() {
                e = format!("({f}{op}{:.4}?{}:{e})", from + step * k as f64, leaf(k));
            }
            e
        };
        let num = |k: usize| k.to_string();
        let lat_up = chain("lat()", "<", 48.8395, 0.0005, 40, &num);
        let lon_up = chain("lon()", "<=", 2.3395, 0.0005, 40, &num);
        let lat_down = chain("lat()", ">", 48.8595, -0.0005, 40, &num);
        let lon_down = chain("lon()", ">=", 2.3595, -0.0005, 40, &num);
        let ties = "(lat()<48.84?0:(lat()<48.84?1:(lat()<48.845?2:(lat()<48.845?3:(lat()<48.85?4:5)))))";
        let unordered = "(lat()<48.84?0:(lat()<48.85?1:(lat()<48.845?2:(lat()<48.86?3:(lat()<48.855?4:(lat()<48.87?5:(lat()<48.88?6:(lat()<48.89?7:8))))))))";
        let mixed = "(lat()<48.84?0:(lat()<48.845?1:(lat()<48.85?2:(lat()<48.855?3:(lon()<2.34?4:(lon()<2.345?5:(lon()<2.35?6:(lon()<2.355?7:(lat()>48.86?8:9)))))))))";
        let exprs = chain("lat()", "<", 48.8395, 0.001, 12, &|k| format!("id()+{k}"));
        let texts = chain("lon()", "<", 2.3395, 0.001, 12, &|k| format!("\"c{k}\""));
        let nested = chain("lat()", "<", 48.8395, 0.002, 8, &|k| format!("({k}*100+{})", chain("lon()", "<", 2.3395, 0.002, 8, &num)));
        let odd = "(lat()<\"48.85 \"?0:(lat()<\"0x31\"?1:(lat()<\"4.886e1\"?2:(lat()<\"inf\"?3:(lat()<\"nan\"?4:5)))))";
        let short = "(lat()<48.84?0:(lat()<48.85?1:2))";
        let cols = [&lat_up[..], &lon_up, &lat_down, &lon_down, ties, unordered, mixed, &exprs, &texts, &nested, odd, short];
        let entries: Vec<String> = cols.iter().enumerate().map(|(k, e)| format!("c{k}={e}")).collect();
        let names: Vec<String> = (0..cols.len()).map(|k| format!("c{k}")).collect();
        let key = format!("{lat_up}*1000+{lon_up}");
        let queries = [
            format!("[out:csv(::id,{};false)];node(48.838,2.338,48.862,2.362);convert r ::id=id(),{};out;", names.join(","), entries.join(",")),
            // ways and relations take the general path
            format!("[out:csv(::id,{};false)];way(48.838,2.338,48.862,2.362);convert r ::id=id(),{};out;", names.join(","), entries.join(",")),
            format!("[out:csv(v,n;false)];node(48.838,2.338,48.862,2.362);for({key}){{make c v=_.val,n=count(nodes);out;}}"),
            format!("[out:csv(::id;false)];node(48.838,2.338,48.862,2.362)(if:{lat_up}==7&&{lon_down}>3);out;"),
        ];
        for (k, q) in queries.iter().enumerate() {
            CHAINS_OFF.set(true);
            let plain = output(&db, q);
            CHAINS_OFF.set(false);
            CHAIN_HITS.set(0);
            let got = output(&db, q);
            assert_eq!(got, plain, "{q}");
            assert!(!got.contains("error") && got.lines().count() > 20, "{q}: {got}");
            assert_eq!(CHAIN_HITS.get() > 1000, k != 1, "{k}: {}", CHAIN_HITS.get());
        }
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `node(bbox)` drops the nodes outside its boxes in the scan and tests
    /// the others on their position: the answers of the geometry test
    /// after the scan, also on the edges of a box, with two boxes, a global
    /// bbox, a box across the antimeridian and with other clauses.
    #[test]
    fn nodes_meet_their_boxes_in_the_scan() {
        let far = "<node id=\"300001\" lat=\"10.0\" lon=\"179.9995\" {m}/><node id=\"300002\" lat=\"10.0\" lon=\"-179.9995\" {m}/><node id=\"300003\" lat=\"10.0\" lon=\"180.0\" {m}/><node id=\"300004\" lat=\"10.0\" lon=\"-180.0\" {m}/><node id=\"300005\" lat=\"10.0005\" lon=\"179.0\" {m}><tag k=\"name\" v=\"x\"/></node>\n";
        let (db, dir) = osm_db("node-box", &grid_osm_with(far, "", ""));
        let c = "[out:csv(::type,::id;false)];";
        let mut queries: Vec<String> = Vec::new();
        for b in [
            "(48.83,2.33,48.85,2.35)",
            // edges on grid lines and on single nodes
            "(48.84,2.34,48.84,2.34)",
            "(48.8400001,2.34,48.8419999,2.3419999)",
            "(48.84005,2.34005,48.84105,2.34105)",
            "(48.840049,2.340051,48.841051,2.341049)",
            "(48.82,2.32,48.878,2.378)",
            "(48.9,2.3,48.95,2.4)",
            // across the antimeridian, and at it
            "(9.9,179.9,10.1,-179.9)",
            "(9.9,179.9995,10.1,180.0)",
            "(9.9,-180.0,10.1,-179.9995)",
            "(10.0,180.0,10.0,-180.0)",
        ] {
            queries.push(format!("{c}node{b};out;"));
            queries.push(format!("{c}node{b}(48.8405,2.3405,48.8415,2.3415);out;"));
            queries.push(format!("{c}node{b}(around:150,48.841,2.341);out;"));
            queries.push(format!("{c}node{b}->.a;node.a(48.8405,2.3405,48.8415,2.3415);out;"));
            queries.push(format!("{c}(node{b};way{b};);out;"));
            queries.push(format!("[bbox:48.8402,2.3402,48.8412,2.3412]{c}node{b};out;"));
            queries.push(format!("{c}node{b}[name];out;"));
            queries.push(format!("{c}node{b}[!name];out count;"));
        }
        let mut seen = 0;
        for q in &queries {
            NODE_BOX_PLAIN.set(true);
            let plain = output(&db, q);
            NODE_BOX_PLAIN.set(false);
            assert_eq!(output(&db, q), plain, "{q}");
            seen += plain.lines().count();
        }
        assert!(seen > 5000, "{seen}");
        let one = |q: &str| output(&db, &format!("{c}{q}"));
        assert_eq!(one("node(48.84005,2.34005,48.84005,2.34005);out;"), "node\t100000\n");
        assert_eq!(one("node(9.9,179.9995,10.1,-179.9995);out;").lines().count(), 4);
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A member of a union that recurses down, and whose result only the
    /// union reads, does not read the elements the union holds already:
    /// the answers of reading them all. A member whose result is read
    /// again reads them all.
    #[test]
    fn union_members_leave_out_what_the_union_holds() {
        use crate::exec::{KNOWN_HELD, KNOWN_OFF};
        let rels = relation(5000, &[("way", 1011), ("node", 342), ("node", 100_005)], &[("type", "route")])
            + &relation(5001, &[("way", 2011), ("way", 1011), ("relation", 5000)], &[("type", "route")])
            + &relation(5002, &[("node", 100_000), ("relation", 5001)], &[]);
        let (db, dir) = grid_db("union-known", &rels);
        // node 342 of the coarse grid (48.842, 2.342), where the ways 1011
        // and 2011 cross, and the fine nodes around it
        let b = "(48.8405,2.3405,48.8425,2.3425)";
        let run = |q: &str| {
            KNOWN_OFF.set(true);
            let plain = output(&db, q);
            KNOWN_OFF.set(false);
            KNOWN_HELD.set(0);
            let got = output(&db, q);
            assert_eq!(got, plain, "{q}");
            assert!(!got.contains("error"), "{q}: {got}");
            (got.lines().count(), KNOWN_HELD.get())
        };
        // left out: the nodes of the box are held when the ways bring them again
        for q in [
            format!("node{b}->.n;way(bn.n)->.w;(.n;.w;node(w.w););out meta;"),
            format!("node{b}->.n;way(bn.n)->.w;(rel(bn.n);rel(bw.w);)->.r;rel(br.r)->.p;(.n;.w;.r;.p;node(w.w););out meta;"),
            format!("(node{b};way{b};rel{b};);(._;>;);out;"),
            format!("(node{b};way{b};rel{b};);(._;>>;);out meta;"),
            format!("(node(48.82,2.32,48.88,2.38);rel(5002););(._;>>;);out;"),
            format!("(way(1011);rel(5001););(._;>;);out;"),
            format!("rel(5001)->.r;way(1011)->.w;node{b}->.n;(.n;.w;way(r.r);node(w);.r;);out;"),
            format!("rel(5000)->.r;node(342)->.n;(.n;node(r.r);node(100005););out;"),
            format!("rel(5002)->.r;rel(5001)->.s;(.s;rel(r.r););out;"),
            format!("node{b}->.n;way(bn.n)->.w;(.n;.w;node(w.w)[!highway];);out;"),
            format!("node{b}->.n;way(id:1011,2011)->.w;(.n;node(w.w:12,13););out;"),
            // into a named set that is read afterwards
            format!("node{b}->.n;way(bn.n)->.w;(.n;.w;node(w.w)->.x;.n->.x;)->.all;.all out;.x out ids;"),
        ] {
            let (lines, held) = run(&q);
            assert!(lines > 5 && held > 0, "{q}: {lines} {held}");
        }
        // read them all: the result of the member is read again
        for q in [
            // the last member of a union into another set: `_` after it
            format!("node{b}->.n;way(bn.n)->.w;(.n;.w;node(w.w);)->.all;out ids;.all out;"),
            // the next member reads it
            format!("node{b}->.n;way(bn.n)->.w;(.n;.w;node(w.w);way(bn);)->.all;.all out;"),
            format!("node{b}->.n;way(bn.n)->.w;(.n;.w;node(w.w);node._(48.84,2.34,48.841,2.341);)->.all;.all out;"),
            format!("node{b}->.n;way(bn.n)->.w;(.n;.w;node(w.w);._;);out;"),
            // a named result that stays
            format!("node{b}->.n;way(bn.n)->.w;(.n;.w;node(w.w)->.x;);out;.x out ids;"),
            // not a recursion down
            format!("node{b}->.n;way(bn.n)->.w;(.n;.w;way(bn.n);rel(bw.w););out;"),
            // the first member
            format!("way{b}->.w;(node(w.w);.w;);out;"),
        ] {
            let (lines, held) = run(&q);
            assert!(lines > 5 && held == 0, "{q}: {lines} {held}");
        }
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
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

    /// Without a `key=value` filter, on the planet or over a large region,
    /// a first `[key]` filter with many candidates (in the region) gives
    /// way to a regex, and the answers are those of reading the key.
    #[test]
    fn many_keyed_elements_yield_to_a_regex() {
        let (db, dir) = tagged_grid("index-candidates");
        let c = "[out:csv(::type,::id;false)];";
        // 400 named nodes in Paris, 30 with a place; 20,000 with a note at
        // 10 to 11 N, half of them south of 10.495 N
        let cases = [
            ("node[\"name\"][\"place\"~\"town\"];", 100, "Regex", 20),
            ("node[\"name\"][\"place\"~\"town\"];", 400, "HasKey(\"name\")", 20),
            ("node[\"place\"~\"town\"][\"name\"];", 100, "Regex", 20),
            // 200 names with "lpha": over the limit the key, under it the regex
            ("node[\"name\"][\"place\"][\"name\"~\"lpha\"];", 100, "HasKey(\"name\")", 20),
            ("node[\"name\"][\"place\"][\"name\"~\"lpha\"];", 250, "Regex", 20),
            ("node[\"name\"][\"name\"~\"a\",i];", 100, "HasKey(\"name\")", 400),
            ("node[\"name\"][~\"^pla\"~\"village\"];", 100, "KeyRegex", 10),
            ("node[\"name\"][\"place\"!~\".\"];", 100, "HasKey(\"name\")", 370),
            ("node[\"name\"][\"place\"~\"town\"](-80,-170,80,170);", 100, "Regex", 20),
            ("node[\"name\"][\"place\"~\"town\"](-80,-170,80,170);", 400, "HasKey(\"name\")", 20),
            ("node[\"note\"][\"note\"~\"number 12\"](9,9,10.495,13);", 5_000, "Regex", 111),
            ("node[\"note\"][\"note\"~\"number 12\"](9,9,10.495,13);", 15_000, "HasKey(\"note\")", 111),
        ];
        for (q, many, chosen, n) in cases {
            let q = format!("{c}{q}out ids;");
            MANY.set(Some(1_000_000));
            let keyed = output(&db, &q);
            MANY.set(Some(many));
            INDEX_FILTER.set(None);
            INDEXED.set(false);
            let out = output(&db, &q);
            MANY.set(None);
            let t = INDEX_FILTER.take().unwrap_or_default();
            assert!(t.starts_with(chosen), "{q} {many}: {t}");
            assert_eq!(out, keyed, "{q}");
            assert_eq!(out.lines().count(), n, "{q}");
            assert_eq!(INDEXED.get(), q.contains("(9,9") || q.contains("(-80"), "{q}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tag index scan on several threads gives the candidates of the
    /// scan on one thread: regexes (with and without prefixes, any case),
    /// keys, with ranges, and filtered by ranges afterwards.
    #[test]
    fn parallel_tag_scan_gives_the_same_candidates() {
        use super::super::fetch::PAR_SCAN_VALUES;
        let (db, dir) = tagged_grid("parscan");
        let snap = db.snapshot().unwrap();
        let f = Fetch::new(&snap);
        let rects = BBox { south: 10.3, west: 10.2, north: 11.05, east: 11.4 }.rects();
        let ranges = ranges_for(Kind::Node, &rects);
        let re = |s: &str, ci: bool| TagFilter::from_ast(&Filter::Regex { key: "note".into(), re: s.into(), ci, neg: false }).unwrap().unwrap();
        let filters = [re("number 1", false), re("NUMBER 1[0-9]*7$", true), re("^far away from the grid, number 1(2|5)", false), re("^(far|near)", false), re("x", false), TagFilter::HasKey("note".into())];
        for t in &filters {
            for r in [None, Some(&ranges[..])] {
                PAR_SCAN_VALUES.set(Some(usize::MAX));
                let one = f.tag_candidates(Kind::Node, t, r).unwrap();
                let one_in = f.tag_candidates_in(Kind::Node, t, Some(&ranges), usize::MAX).unwrap().unwrap();
                for limit in [0, 1, 100, usize::MAX] {
                    PAR_SCAN_VALUES.set(Some(limit));
                    assert_eq!(f.tag_candidates(Kind::Node, t, r).unwrap(), one, "{t:?} {}", r.is_some());
                    assert_eq!(f.tag_candidates_in(Kind::Node, t, Some(&ranges), usize::MAX).unwrap().unwrap(), one_in, "{t:?}");
                    // a limit of as many pairs keeps them, one fewer gives none
                    assert_eq!(f.tag_candidates_in(Kind::Node, t, Some(&ranges), one_in.len()).unwrap(), Some(one_in.clone()), "{t:?}");
                    if !one_in.is_empty() {
                        assert_eq!(f.tag_candidates_in(Kind::Node, t, Some(&ranges), one_in.len() - 1).unwrap(), None, "{t:?}");
                    }
                }
            }
        }
        PAR_SCAN_VALUES.set(None);
        // the scans above ran on threads
        assert!(super::super::fetch::PAR_SCANS.get() >= filters.len() * 2 * 3 * 2, "{}", super::super::fetch::PAR_SCANS.get());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A snapshot opens a second transaction on its data only while no
    /// commit came in between.
    #[test]
    fn sibling_snapshot_sees_the_same_data() {
        let (db, dir) = tagged_grid("sibling");
        let snap = db.snapshot().unwrap();
        assert!(snap.sibling().unwrap().is_some());
        db.write().unwrap().commit().unwrap();
        assert!(snap.sibling().unwrap().is_none());
        let now = db.snapshot().unwrap();
        assert!(now.sibling().unwrap().is_some());
        db.write().unwrap().commit().unwrap();
        assert!(now.sibling().unwrap().is_none());
        drop((snap, now));
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

    /// `rel(area)` reads the members stored where they may reach an area,
    /// with the answers of testing every member. A DB area with a hole: a
    /// member node inside, a way across, a way inside, a long way across
    /// pass; members on both sides only and a way in the hole do not; a
    /// node on the border. An implicit area: a way whose great-circle edge
    /// runs through it south of its nodes passes, from a cell apart from it.
    #[test]
    fn rel_area_reads_the_members_near_the_area() {
        use super::super::area::{MEMBER_WAYS_READ, MEMBERS_ALL};
        let m = r#"version="1" timestamp="2020-01-01T00:00:00Z" uid="1" user="a" changeset="1""#;
        let mut nodes: Vec<(usize, f64, f64)> = Vec::new();
        let mut ways: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut rels = String::new();
        let ring = |nodes: &mut Vec<(usize, f64, f64)>, ways: &mut Vec<(usize, Vec<usize>)>, id: usize, s: f64, w: f64, n: f64, e: f64| {
            let ids: Vec<usize> = (0..4).map(|k| id * 10 + k).collect();
            for (k, (la, lo)) in [(s, w), (s, e), (n, e), (n, w)].into_iter().enumerate() {
                nodes.push((ids[k], la, lo));
            }
            ways.push((id, vec![ids[0], ids[1], ids[2], ids[3], ids[0]]));
        };
        // the area: 48.8-48.9 N, 2.3-2.4 E with a hole 48.84-48.86 N, 2.34-2.36 E
        ring(&mut nodes, &mut ways, 901, 48.80, 2.30, 48.90, 2.40);
        ring(&mut nodes, &mut ways, 902, 48.84, 2.34, 48.86, 2.36);
        rels += &relation(900, &[("way", 901), ("way", 902)], &[("type", "multipolygon"), ("name", "Square")]);
        // the implicit area: closed way 7100, 37.75-37.6 S, 144-146 E
        ring(&mut nodes, &mut ways, 7100, -37.75, 144.0, -37.60, 146.0);
        // 300 short ways far away
        for i in 0..300 {
            nodes.push((20_000 + 2 * i, 40.0 + i as f64 * 0.01, 20.0));
            nodes.push((20_001 + 2 * i, 40.0 + i as f64 * 0.01, 20.005));
            ways.push((5000 + i, vec![20_000 + 2 * i, 20_001 + 2 * i]));
        }
        let pt = [(101, 48.82, 2.32), (102, 48.82, 2.0), (103, 48.82, 2.7), (104, 48.0, 2.0), (105, 49.5, 3.0), (106, 48.849, 2.349), (107, 48.851, 2.351), (108, 49.5, 2.35), (109, 48.80, 2.35), (110, 40.0, 20.0), (111, 48.85, -10.0), (112, 48.85, 15.0), (113, -37.30, 135.0), (114, -37.30, 155.0), (115, -38.5, 145.0), (116, 48.81, 2.31), (117, 48.815, 2.315)];
        nodes.extend(pt);
        for (id, nds) in [(7001, vec![102, 103]), (7002, vec![106, 107]), (7003, vec![116, 117]), (7005, vec![111, 112]), (7006, vec![113, 114])] {
            ways.push((id, nds));
        }
        let far = |from: usize, to: usize| (from..to).map(|i| ("way", i)).collect::<Vec<_>>();
        let with = |mut v: Vec<(&'static str, usize)>, more: &[(&'static str, usize)]| {
            v.extend_from_slice(more);
            v
        };
        let route = [("type", "route")];
        rels += &relation(1, &with(far(5000, 5100), &[("node", 101)]), &route);
        rels += &relation(2, &with(far(5100, 5200), &[("way", 7001)]), &route);
        rels += &relation(3, &with(far(5200, 5210), &[("node", 104), ("node", 105)]), &route);
        rels += &relation(4, &[("way", 7002), ("node", 108)], &route);
        rels += &relation(5, &[("node", 109), ("node", 110)], &route);
        rels += &relation(6, &[("way", 7005)], &route);
        rels += &relation(7, &[("way", 7006), ("node", 115)], &route);
        rels += &relation(8, &with(far(5210, 5300), &[("way", 7003)]), &route);
        let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osm version=\"0.6\">\n");
        nodes.sort_by_key(|n| n.0);
        for (id, la, lo) in &nodes {
            x += &format!("<node id=\"{id}\" lat=\"{la:.7}\" lon=\"{lo:.7}\" {m}/>\n");
        }
        ways.sort_by_key(|w| w.0);
        for (id, nds) in &ways {
            let nds: String = nds.iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect();
            x += &format!("<way id=\"{id}\" {m}>{nds}</way>\n");
        }
        x += &rels.replace("{m}", m);
        x += "</osm>\n";
        let (db, dir) = osm_db("rel-area", &x);
        crate::update::areas::make_areas(&db).unwrap();
        let ids = |q: &str, all: bool| {
            MEMBERS_ALL.set(all);
            MEMBER_WAYS_READ.set(0);
            let out = output(&db, q);
            MEMBERS_ALL.set(false);
            let ids: Vec<usize> = out.lines().filter_map(|l| l.strip_prefix("relation\t")).map(|i| i.parse().unwrap()).collect();
            (ids, MEMBER_WAYS_READ.get())
        };
        let q = "[out:csv(::type,::id;false)];area[name=Square]->.a;rel(area.a);out ids;";
        let (near, read) = ids(q, false);
        let (all, read_all) = ids(q, true);
        assert_eq!(near, all);
        for id in [1, 2, 6, 8] {
            assert!(near.contains(&id), "{id}: {near:?}");
        }
        for id in [3, 4, 7] {
            assert!(!near.contains(&id), "{id}: {near:?}");
        }
        // the ways near the area, not the 200 far ones of the relations a
        // node does not decide
        assert!(read < 10 && read_all > 200, "{read} {read_all}");
        // with a tag filter, and with nwr
        for q in ["[out:csv(::type,::id;false)];area[name=Square]->.a;rel(area.a)[type=route];out ids;", "[out:csv(::type,::id;false)];area[name=Square]->.a;nwr(area.a);out ids;"] {
            assert_eq!(ids(q, false).0, ids(q, true).0, "{q}");
        }
        let q = "[out:csv(::type,::id;false)];way(7100);rel(area);out ids;";
        let (near, read) = ids(q, false);
        assert_eq!(near, vec![7]);
        assert_eq!((near, read), (ids(q, true).0, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The implicit area of a closed way across the antimeridian lies at
    /// both ends of the key space, outside the box of its vertices: the
    /// nodes, ways and relations in it are found, at both ends. A ring
    /// around a pole (one edge across the antimeridian) holds the tile rows
    /// north of it. `node(area)` of each way agrees with `is_in` at every
    /// node, and with a DB area in the set (more than 32 rectangles) the
    /// answer is the union of the answers of each.
    #[test]
    fn implicit_areas_across_the_antimeridian() {
        let m = r#"version="1" timestamp="2020-01-01T00:00:00Z" uid="1" user="a" changeset="1""#;
        let mut nodes: Vec<(usize, f64, f64)> = Vec::new();
        let mut ways: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut ring = |id: usize, pts: &[(f64, f64)], nodes: &mut Vec<(usize, f64, f64)>| {
            let ids: Vec<usize> = (0..pts.len()).map(|k| id * 10 + k).collect();
            for (k, (la, lo)) in pts.iter().enumerate() {
                nodes.push((ids[k], *la, *lo));
            }
            ways.push((id, ids.iter().chain(&ids[..1]).copied().collect()));
        };
        // 10.0-10.1 N, from 179.5 E across the antimeridian to 179.5 W
        ring(101, &[(10.0, 179.5), (10.0, -179.5), (10.1, -179.5), (10.1, 179.5)], &mut nodes);
        // around the south pole and the north pole: the edge 45 E - 160 W is 155 degrees wide across the antimeridian
        ring(102, &[(-89.5, -5.0), (-89.5, 45.0), (-89.5, -160.0)], &mut nodes);
        ring(103, &[(89.5, -5.0), (89.5, 45.0), (89.5, -160.0)], &mut nodes);
        // the DB area: 30-32 N, 30-32 E (400 block tiles)
        ring(900, &[(30.0, 30.0), (30.0, 32.0), (32.0, 32.0), (32.0, 30.0)], &mut nodes);
        let pt = [
            (1, 10.05, 179.75),
            (2, 10.05, -179.75),
            (3, 10.05, 179.999),
            (4, 10.05, -179.999),
            (5, 10.05, 0.0),
            (6, 10.05, 179.0),
            (7, 10.05, -179.0),
            (8, 0.0, 0.0),
            (9, 50.0, 100.0),
            (10, -89.7, 10.0),
            (11, 89.7, 10.0),
            (12, 31.0, 31.0),
            (13, 10.04, 179.7),
            (14, 10.06, 179.8),
            (15, 10.05, 170.0),
            (16, 10.05, 171.0),
        ];
        nodes.extend(pt);
        ways.push((7001, vec![13, 14]));
        ways.push((7002, vec![15, 16]));
        let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osm version=\"0.6\">\n");
        nodes.sort_by_key(|n| n.0);
        for (id, la, lo) in &nodes {
            x += &format!("<node id=\"{id}\" lat=\"{la:.7}\" lon=\"{lo:.7}\" {m}/>\n");
        }
        ways.sort_by_key(|w| w.0);
        for (id, nds) in &ways {
            let nds: String = nds.iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect();
            x += &format!("<way id=\"{id}\" {m}>{nds}</way>\n");
        }
        x += &relation(1, &[("node", 2)], &[("type", "route")]).replace("{m}", m);
        x += &relation(2, &[("node", 5)], &[("type", "route")]).replace("{m}", m);
        x += &relation(900, &[("way", 900)], &[("type", "multipolygon"), ("name", "Square")]).replace("{m}", m);
        x += "</osm>\n";
        let (db, dir) = osm_db("implicit-anti", &x);
        crate::update::areas::make_areas(&db).unwrap();
        let ids = |q: &str| -> Vec<String> {
            let mut v: Vec<String> = output(&db, &format!("[out:csv(::type,::id;false)];{q}")).lines().map(String::from).collect();
            v.sort();
            v
        };
        let n = |ids: &[usize]| -> Vec<String> {
            let mut v: Vec<String> = ids.iter().map(|i| format!("node\t{i}")).collect();
            v.sort();
            v
        };
        // the nodes of a ring and more
        let with = |w: usize, more: &[usize]| {
            let nds = &ways.iter().find(|x| x.0 == w).unwrap().1;
            n(&nds[1..].iter().chain(more).copied().collect::<Vec<_>>())
        };
        assert_eq!(ids("way(101);node(area);out ids;"), with(101, &[1, 2, 3, 4, 13, 14]));
        let got = ids("way(101);way(area);out ids;");
        assert!(got.contains(&"way\t7001".to_string()) && !got.contains(&"way\t7002".to_string()), "{got:?}");
        assert_eq!(ids("way(101);rel(area);out ids;"), vec!["relation\t1"]);
        // the poles: is_in says so
        for (w, inside, outside) in [(102, [8, 9, 11], 10), (103, [11, 11, 11], 8)] {
            let got = ids(&format!("way({w});node(area);out ids;"));
            for i in inside {
                assert!(got.contains(&format!("node\t{i}")), "{w} {i} {got:?}");
            }
            assert!(!got.contains(&format!("node\t{outside}")), "{w} {outside} {got:?}");
        }
        // node(area) of each way and is_in at each node agree
        for w in [101, 102, 103] {
            let got = ids(&format!("way({w});node(area);out ids;"));
            for (i, la, lo) in &nodes {
                let covers = ids(&format!("is_in({la},{lo});out ids;")).contains(&format!("way\t{w}"));
                assert_eq!(got.contains(&format!("node\t{i}")), covers, "way {w} node {i} ({la} {lo})");
            }
        }
        // with the DB area in the set
        let area = ids("area(3600000900);node(area);out ids;");
        assert_eq!(area, with(900, &[12]));
        for w in [101, 102, 103] {
            let mut union = ids(&format!("way({w});node(area);out ids;"));
            union.extend(area.iter().cloned());
            union.sort();
            union.dedup();
            assert_eq!(ids(&format!("(way({w});area(3600000900););node(area);out ids;")), union, "{w}");
        }
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
    /// A way 200 degrees wide is taken across the antimeridian by the
    /// polygon and bbox tests: every path finds it at 175 degrees east, where
    /// its nodes are not, and none finds a way of two edges of 95 degrees
    /// there. The polygons: a small one (candidate and member paths) and one
    /// larger than they take.
    #[test]
    fn ways_across_the_antimeridian_on_every_path() {
        let m = "{m}";
        let node = |id: usize, lat: f64, lon: f64| format!("<node id=\"{id}\" lat=\"{lat}\" lon=\"{lon}\" {m}/>\n");
        let nodes = [node(300_001, 10.0, -100.0), node(300_002, 10.0, 100.0), node(300_003, 10.002, -95.0), node(300_004, 10.002, 0.0), node(300_005, 10.002, 95.0)].concat();
        let way = |id: usize, nds: &[usize]| format!("<way id=\"{id}\" {m}>{}</way>\n", nds.iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect::<String>());
        let ways = [way(4100, &[300_001, 300_002]), way(4101, &[300_003, 300_004, 300_005])].concat();
        let rels = [relation(41, &[("way", 4100)], &[("route", "ferry")]), relation(42, &[("way", 4101)], &[("route", "ferry")])].concat();
        let (db, dir) = osm_db("across", &grid_osm_with(&nodes, &ways, &rels));
        let c = "[out:csv(::type,::id;false)];";
        let small = "(poly:\"9.975 174.975 9.975 175.025 10.025 175.025 10.025 174.975\")";
        let large = "(poly:\"9.5 174.5 9.5 175.5 10.5 175.5 10.5 174.5\")";
        for (f, t, want) in [
            (small, "way", "way\t4100"),
            (small, "rel", "relation\t41"),
            (small, "rel[route=ferry]", "relation\t41"),
            (large, "way", "way\t4100"),
            (large, "rel", "relation\t41"),
            (large, "rel[route=ferry]", "relation\t41"),
            ("(9.5,175,10.5,-175)", "way", "way\t4100"),
            ("(9.5,175,10.5,-175)", "rel", "relation\t41"),
            ("(9.5,175,10.5,-175)(9.9,170,10.1,-170)", "rel", "relation\t41"),
        ] {
            let q = format!("{c}{t}{f};out ids;");
            let plain = both_ways(&db, &q).0;
            assert_eq!(plain, [want], "{q}");
            for (boxes_off, scan, near_off) in [(true, false, false), (false, true, false), (true, true, true)] {
                REL_BOXES_OFF.set(boxes_off);
                WAY_BOXES_OFF.set(boxes_off);
                MEMBER_SCAN.set(scan);
                NEAR_GEOMS_OFF.set(near_off);
                let out = both_ways(&db, &q).0;
                REL_BOXES_OFF.set(false);
                WAY_BOXES_OFF.set(false);
                MEMBER_SCAN.set(false);
                NEAR_GEOMS_OFF.set(false);
                assert_eq!(out, plain, "{q} {boxes_off} {scan} {near_off}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ways and relations that a polygon decides by their stored box in
    /// `query_type` (apart: fail, strictly inside: pass) get the answers of
    /// their geometry tests: polygons larger than the relation paths take,
    /// with grid nodes on their edges and corners, edges along grid ways, a
    /// notch, and short ways and relations of a few nodes on both sides.
    #[test]
    fn poly_box_decisions_match_geometry_tests() {
        let m = "{m}";
        let id = |i: usize, j: usize| i * 30 + j + 1;
        let mut ways = String::new();
        let mut rels = String::new();
        let mut k = 0;
        for i in (0..29).step_by(2) {
            for j in (0..29).step_by(3) {
                let nds = [id(i, j), id(i, j + 1), id(i + 1, j + 1)];
                let w = 4000 + k;
                ways += &format!("<way id=\"{w}\" {m}>{}<tag k=\"building\" v=\"yes\"/></way>\n", nds.iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect::<String>());
                rels += &relation(6000 + k, &[("way", w)], &[("type", "multipolygon"), ("building", "yes")]);
                rels += &relation(7000 + k, &[("node", id(i + 1, j))], &[("type", "site")]);
                k += 1;
            }
        }
        // one with a missing node: never passes
        ways += &format!("<way id=\"4999\" {m}><nd ref=\"466\"/><nd ref=\"999999\"/></way>\n");
        rels += &relation(6999, &[("way", 4999)], &[("building", "yes")]);
        rels += &relation(7999, &[("way", 1003), ("node", 466)], &[("type", "route")]);
        let (db, dir) = osm_db("polyboxes", &grid_osm_with("", &ways, &rels));
        let polys = [
            // a large square with a notch from the east along grid ways 1015 and 1020
            "48.7 2.2 48.7 2.5 48.85 2.5 48.85 2.35 48.86 2.35 48.86 2.5 49.0 2.5 49.0 2.2",
            // a large triangle, one edge across the grid through grid nodes
            "48.6 2.1 48.84 2.33 48.88 2.37 49.1 2.1",
            // edges along grid way 2010 and through grid nodes
            "48.6 2.34 49.1 2.34 49.1 2.6 48.6 2.6",
            // a spike into the grid, and a ring that runs back along itself
            "48.6 2.0 48.85 2.35 48.6 2.01 48.6 2.6 49.0 2.6 49.0 2.0",
            "48.6 2.0 48.6 2.6 48.85 2.6 48.85 2.35 48.85 2.6 49.0 2.6 49.0 2.0",
            // smaller ones (the relation paths answer for relations)
            "48.840 2.340 48.840 2.360 48.860 2.360 48.860 2.340",
            "48.83 2.33 48.87 2.33 48.85 2.37",
        ];
        let c = "[out:csv(::type,::id;false)];";
        let (mut pass, mut fail) = (0, 0);
        for poly in polys {
            let p = format!("(poly:\"{poly}\")");
            for q in [
                format!("{c}way{p};out ids;"),
                format!("{c}way[building]{p};out ids;"),
                format!("{c}rel{p};out ids;"),
                format!("{c}rel[type]{p};out ids;"),
                format!("{c}nwr{p};out ids;"),
                format!("{c}way(48.84,2.33,48.87,2.37){p};out ids;"),
                format!("[out:csv(::type,::id;false)][bbox:48.845,2.33,48.86,2.37];nwr[building]{p};out ids;"),
                format!("{c}way[building]{p}(around:300,48.85,2.35);out ids;"),
                format!("{c}way[building]{p}{p};out ids;"),
            ] {
                POLY_BOXES.set((0, 0));
                let plain = both_ways(&db, &q).0;
                let (a, b) = POLY_BOXES.get();
                (pass, fail) = (pass + a, fail + b);
                REL_BOXES_OFF.set(true);
                WAY_BOXES_OFF.set(true);
                let out = both_ways(&db, &q).0;
                REL_BOXES_OFF.set(false);
                WAY_BOXES_OFF.set(false);
                assert_eq!(out, plain, "{q}");
            }
        }
        assert!(pass > 100 && fail > 100, "{pass} passed, {fail} failed by box");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review (rv-poly): relations of mixed members decided by their box:
    /// incomplete member ways inside with complete ones outside, missing
    /// member nodes, only relation members, member nodes on polygon
    /// vertices and edges, member ways along an edge, ways with repeated
    /// nodes, and the same with an `(area)`-free `(if:)` and `out` modes.
    #[test]
    fn rv_poly_box_decisions_mixed_relations() {
        let m = "{m}";
        let id = |i: usize, j: usize| i * 30 + j + 1;
        let mut ways = String::new();
        // incomplete ways (one missing node) at several places, complete short ways, a way with repeated nodes
        for (k, (i, j)) in [(2, 2), (10, 10), (15, 15), (20, 25), (27, 27), (14, 16)].iter().enumerate() {
            ways += &format!("<way id=\"{}\" {m}><nd ref=\"{}\"/><nd ref=\"{}\"/><nd ref=\"9999{k}\"/></way>\n", 4100 + k, id(*i, *j), id(*i, *j + 1));
            ways += &format!("<way id=\"{}\" {m}><nd ref=\"{}\"/><nd ref=\"{}\"/><nd ref=\"{}\"/></way>\n", 4200 + k, id(*i, *j), id(*i + 1, *j), id(*i, *j));
            ways += &format!("<way id=\"{}\" {m}><nd ref=\"{}\"/></way>\n", 4300 + k, id(*i, *j));
        }
        let mut rels = String::new();
        let mut rid = 8000;
        let mut add = |members: &[(&str, usize)]| {
            rels += &relation(rid, members, &[("type", "multipolygon"), ("building", "yes")]);
            rid += 1;
        };
        for a in 0..6 {
            for b in 0..6 {
                // an incomplete way at one place and a complete one at another
                add(&[("way", 4100 + a), ("way", 4200 + b)]);
                // a one-node way and a missing node
                add(&[("way", 4300 + a), ("node", 999_990 + b)]);
                // a node at one place and an incomplete way at another
                add(&[("node", id([2, 10, 15, 20, 27, 14][a], [2, 10, 15, 25, 27, 16][a])), ("way", 4100 + b)]);
            }
            // only a relation member, and a relation of a relation and a node
            add(&[("relation", 8000 + a)]);
            add(&[("relation", 8001 + a), ("node", id(15, 15 + a))]);
            // a grid row (along polygon edges at 48.85 and 48.86) and a node
            add(&[("way", 1015), ("node", id(a * 5, a * 5))]);
            add(&[("way", 1020)]);
            add(&[("way", 2000 + a * 5), ("way", 1000 + a * 5)]);
            // nodes only, on grid points along the polygon edges
            add(&[("node", id(15, a * 5)), ("node", id(20, a * 5 + 2))]);
            // nothing that exists
            add(&[("node", 999_980 + a), ("way", 999_970 + a)]);
        }
        let (db, dir) = osm_db("rvpolymixed", &grid_osm_with("", &ways, &rels));
        let polys = [
            "48.7 2.2 48.7 2.5 48.85 2.5 48.85 2.35 48.86 2.35 48.86 2.5 49.0 2.5 49.0 2.2",
            "48.6 2.1 48.84 2.33 48.88 2.37 49.1 2.1",
            "48.6 2.34 49.1 2.34 49.1 2.6 48.6 2.6",
            "48.6 2.0 48.85 2.35 48.6 2.01 48.6 2.6 49.0 2.6 49.0 2.0",
            "48.6 2.0 48.6 2.6 48.85 2.6 48.85 2.35 48.85 2.6 49.0 2.6 49.0 2.0",
            // vertices on grid nodes, edges through them
            "48.7 2.2 48.7 2.36 48.85 2.36 48.85 2.35 48.83 2.33 48.87 2.33 48.87 2.36 49.0 2.36 49.0 2.2",
            // a bow tie over the grid
            "48.7 2.2 49.0 2.5 49.0 2.2 48.7 2.5",
        ];
        let c = "[out:csv(::type,::id;false)];";
        let (mut pass, mut fail) = (0, 0);
        for poly in polys {
            let p = format!("(poly:\"{poly}\")");
            for q in [
                format!("{c}rel{p};out ids;"),
                format!("{c}rel[building]{p};out ids;"),
                format!("{c}way{p};out ids;"),
                format!("{c}nwr{p};out ids;"),
                format!("{c}rel{p}(if:count_members()>1);out ids;"),
                format!("[out:json];rel{p};out bb;"),
                format!("[out:json];rel{p};out center;"),
                format!("[out:json];way{p};out geom;"),
                format!("{c}rel(48.84,2.33,48.87,2.37){p};out ids;"),
                format!("{c}rel{p}(poly:\"48.5 2.0 48.5 2.7 49.2 2.7 49.2 2.0\");out ids;"),
            ] {
                POLY_BOXES.set((0, 0));
                let plain = both_ways(&db, &q).0;
                let (a, b) = POLY_BOXES.get();
                (pass, fail) = (pass + a, fail + b);
                REL_BOXES_OFF.set(true);
                WAY_BOXES_OFF.set(true);
                let out = both_ways(&db, &q).0;
                REL_BOXES_OFF.set(false);
                WAY_BOXES_OFF.set(false);
                assert_eq!(out, plain, "{q}");
            }
        }
        assert!(pass > 100 && fail > 100, "{pass} passed, {fail} failed by box");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `around` and implicit areas follow great-circle edges, which pass
    /// north (or south) of the box of their nodes. A way of one edge 20
    /// degrees long at 48.41 N (and one at 48.41 S) reaches 48.85 at 2.35 E,
    /// over the Paris grid, 49 km off its box: `around` there finds the way
    /// and its relation, as does a closed way there used as an area. A
    /// triangle with that edge holds a node in the bulge, and the long way
    /// as an `around` source finds a node on its edge (C++ answers the same).
    /// Every path gives the answer: tag index and cell candidates, the
    /// relation candidate and member paths, reduced and full relation
    /// geometries, and the relation area test with every member read.
    #[test]
    fn great_circle_edges_on_every_path() {
        use super::super::area::MEMBERS_ALL;
        let m = "{m}";
        let half = 10.0f64;
        // ends at 48.41: the edge reaches 48.85 at 2.35
        let low = (48.85f64.to_radians().tan() * half.to_radians().cos()).atan().to_degrees();
        let low: f64 = format!("{low:.7}").parse().unwrap();
        let apex = (low.to_radians().tan() / half.to_radians().cos()).atan().to_degrees();
        let node = |id: usize, lat: f64, lon: f64, tags: &str| format!("<node id=\"{id}\" lat=\"{lat:.7}\" lon=\"{lon:.7}\" {m}>{tags}</node>\n");
        let probe = "<tag k=\"probe\" v=\"n\"/>";
        let way = |id: usize, nds: &[usize], tags: &str| format!("<way id=\"{id}\" {m}>{}{tags}</way>\n", nds.iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect::<String>());
        let (mut nodes, mut ways, mut rels) = (String::new(), String::new(), String::new());
        // per hemisphere: the long way, a square around the apex, a
        // triangle with the long edge, a node in the bulge, one beyond
        // the apex and one on it
        for (k, s) in [(0usize, 1.0f64), (1, -1.0)] {
            let n0 = 300_100 + 20 * k;
            let (lo, ap) = (s * low, s * apex);
            nodes += &node(n0, lo, 2.35 - half, "");
            nodes += &node(n0 + 1, lo, 2.35 + half, "");
            for (i, (dy, dx)) in [(-0.01, -0.01), (-0.01, 0.01), (0.01, 0.01), (0.01, -0.01)].into_iter().enumerate() {
                nodes += &node(n0 + 2 + i, ap + dy, 2.35 + dx, "");
            }
            nodes += &node(n0 + 6, lo, 2.35 - half, "");
            nodes += &node(n0 + 7, lo, 2.35 + half, "");
            nodes += &node(n0 + 8, lo - s * 5.0, 2.35, "");
            nodes += &node(n0 + 9, (lo + ap) / 2.0, 2.35, probe);
            nodes += &node(n0 + 10, ap + s * 0.05, 2.35, probe);
            nodes += &node(n0 + 11, ap, 2.35, probe);
            ways += &way(4500 + k, &[n0, n0 + 1], "<tag k=\"probe\" v=\"w\"/>");
            ways += &way(4502 + k, &[n0 + 2, n0 + 3, n0 + 4, n0 + 5, n0 + 2], "<tag k=\"area\" v=\"yes\"/>");
            ways += &way(4504 + k, &[n0 + 6, n0 + 7, n0 + 8, n0 + 6], "<tag k=\"area\" v=\"yes\"/>");
            rels += &relation(71 + k, &[("way", 4500 + k)], &[("route", "ferry")]);
        }
        let (db, dir) = osm_db("arcs", &grid_osm_with(&nodes, &ways, &rels));
        let c = "[out:csv(::type,::id;false)];";
        let mut answered = 0;
        for (k, s) in [(0usize, 1.0f64), (1, -1.0)] {
            let a = format!("{:.7},2.35", s * apex);
            let (w, r, sq, tri, n0) = (4500 + k, 71 + k, 4502 + k, 4504 + k, 300_100 + 20 * k);
            let (w, r, inside, on) = (format!("way\t{w}"), format!("relation\t{r}"), format!("node\t{}", n0 + 9), format!("node\t{}", n0 + 11));
            for (q, want, exact) in [
                (format!("way(around:100,{a})"), &w, false),
                (format!("way[probe](around:100,{a})"), &w, true),
                (format!("way[probe](around:10000,{a})"), &w, true),
                (format!("rel(around:100,{a})"), &r, false),
                (format!("rel[route=ferry](around:100,{a})"), &r, true),
                (format!("rel[route=ferry](around:10000,{a})"), &r, true),
                (format!("way({sq});way(area)[probe]"), &w, true),
                (format!("way({sq});way(area)"), &w, false),
                (format!("way({sq});rel(area)[route=ferry]"), &r, true),
                (format!("way({sq});rel(area)"), &r, false),
                (format!("way({w});node[probe](around:100)", w = 4500 + k), &on, true),
                (format!("way({tri});node[probe](area)"), &inside, true),
            ] {
                let q = format!("{c}{q};out ids;");
                let (plain, what) = both_ways(&db, &q);
                answered += (what == Some("answered")) as usize;
                if exact {
                    assert_eq!(plain, [want.as_str()], "{q}");
                } else {
                    assert!(plain.contains(want), "{q}: {plain:?}");
                }
                for (near_off, all) in [(true, false), (false, true)] {
                    NEAR_GEOMS_OFF.set(near_off);
                    MEMBERS_ALL.set(all);
                    let out = both_ways(&db, &q).0;
                    NEAR_GEOMS_OFF.set(false);
                    MEMBERS_ALL.set(false);
                    assert_eq!(out, plain, "{q} {near_off} {all}");
                }
            }
        }
        // the relation candidate path answered over the Paris grid
        assert!(answered > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `bb` and `center` from stored boxes give the output of the member
    /// positions: ways with all nodes and with a missing one, an empty way,
    /// relations with a missing node, a missing way, a way with a missing
    /// node, one 200 degrees wide, one of a relation only; in each format,
    /// and with a box after `out`.
    #[test]
    fn bb_and_center_from_stored_boxes() {
        use super::super::print::BOXES_OFF;
        let m = "{m}";
        let nodes = format!("<node id=\"300001\" lat=\"10.0\" lon=\"-100.0\" {m}/>\n<node id=\"300002\" lat=\"10.0\" lon=\"100.0\" {m}/>\n");
        let ways = format!("<way id=\"4200\" {m}></way>\n");
        let rels = [
            relation(51, &[("way", 1000), ("node", 466), ("way", 2005)], &[]),
            relation(52, &[("way", 3001), ("node", 466)], &[]),
            relation(53, &[("node", 999_998), ("way", 1001)], &[]),
            relation(54, &[("way", 999_000), ("node", 1)], &[]),
            relation(55, &[("node", 300_001), ("node", 300_002)], &[]),
            relation(56, &[("relation", 51)], &[]),
            relation(57, &[("way", 4200), ("node", 2)], &[]),
            relation(58, &[("way", 3000), ("way", 1000), ("way", 1029)], &[]),
        ]
        .concat();
        let (db, dir) = osm_db("boxes", &grid_osm_with(&nodes, &ways, &rels));
        let sets = ["way(id:1000,1015,2005,3000,3001,4200)", "rel(id:51,52,53,54,55,56,57,58)", "(way(id:1000,3001);rel(id:51,55);)"];
        let outs = ["out center;", "out bb;", "out center qt;", "out ids bb;", "out tags center;", "out skel center;", "out center(48.83,2.33,48.85,2.36);", "out bb(48.83,2.33,48.85,2.36);"];
        let heads = ["", "[out:json];", "[out:csv(::type,::id,::lat,::lon;true)];"];
        let mut boxed = 0;
        for set in sets {
            for out in outs {
                for head in heads {
                    let q = format!("{head}{set};{out}");
                    let fast = output(&db, &q);
                    BOXES_OFF.set(true);
                    let full = output(&db, &q);
                    BOXES_OFF.set(false);
                    assert_eq!(fast, full, "{q}");
                    boxed += fast.contains("center") as usize;
                }
            }
        }
        assert!(boxed > 0);
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

    // A way whose only point in a box is a node on an edge passes, as the
    // node does, on each of the four edges and in both directions.
    #[test]
    fn way_meets_a_box_at_a_node_on_its_edge() {
        let b = BBox { south: 40.4168, west: 8.5417, north: 47.3769, east: 8.5517 };
        let edges = [((47.3769, 8.545), (47.38, 8.545)), ((40.4168, 8.545), (40.41, 8.545)), ((45.0, 8.5417), (45.0, 8.54)), ((45.0, 8.5517), (45.0, 8.56))];
        for (on, out) in edges {
            let (p, q) = (Pt::from_ll(on.0, on.1), Pt::from_ll(out.0, out.1));
            assert!(node_in_bbox(p, &b), "{on:?}");
            assert!(line_in_bbox(&[Some(q), Some(p)], &b), "{on:?}");
            assert!(line_in_bbox(&[Some(p), Some(q)], &b), "{on:?}");
            assert!(geom_in_bbox(&EGeom::Multi { points: vec![p], lines: vec![] }, &b), "{on:?}");
        }
        // a way along the north edge, and one just outside it
        let (a, c) = (Pt::from_ll(47.3769, 8.545), Pt::from_ll(47.3769, 8.55));
        assert!(line_in_bbox(&[Some(a), Some(c)], &b));
        let (a, c) = (Pt::from_ll(47.3769001, 8.545), Pt::from_ll(47.3769001, 8.55));
        assert!(!line_in_bbox(&[Some(a), Some(c)], &b));
        // an edge between two 7-digit values: the node test rounds it, the way follows
        let b = BBox { north: 47.37689996, ..b };
        let (p, q) = (Pt::from_ll(47.3769, 8.545), Pt::from_ll(47.38, 8.545));
        assert!(node_in_bbox(p, &b) && line_in_bbox(&[Some(q), Some(p)], &b));
        // a segment across the box without a node in it
        let (p, q) = (Pt::from_ll(45.0, 8.53), Pt::from_ll(45.0, 8.56));
        assert!(!node_in_bbox(p, &b) && !node_in_bbox(q, &b) && line_in_bbox(&[Some(p), Some(q)], &b));
        assert!(!line_in_bbox(&[Some(p)], &b));
    }

    // ---- reviewer rv-eval: randomised differentials (bounded, #[ignore]) ----

    /// `output`, with a rejected query as its message (generated queries).
    fn output_or_rejected(db: &crate::store::Db, q: &str) -> String {
        let opts = crate::exec::RunOptions { cli: true, ..Default::default() };
        match crate::exec::run(db, q, Vec::new(), &opts) {
            Ok(p) => String::from_utf8(p.w).unwrap(),
            Err((Some(p), _)) => String::from_utf8(p.w).unwrap(),
            Err((None, e)) => format!("REJECTED {e}"),
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick<'a, T: ?Sized>(&mut self, v: &[&'a T]) -> &'a T {
            v[(self.next() % v.len() as u64) as usize]
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Patch 06: random unions with recursion members in many contexts
    /// give the answers of reading every element.
    #[test]
    #[ignore]
    fn rv_union_known_random() {
        use crate::exec::{KNOWN_HELD, KNOWN_OFF};
        let rels = relation(5000, &[("way", 1011), ("node", 342), ("node", 100_005)], &[("type", "route")])
            + &relation(5001, &[("way", 2011), ("way", 1011), ("relation", 5000)], &[("type", "route")])
            + &relation(5002, &[("node", 100_000), ("relation", 5001)], &[])
            + &relation(5003, &[("way", 1015), ("way", 2015), ("node", 466), ("way", 3001)], &[("type", "multipolygon")])
            + &relation(5004, &[("relation", 5005), ("node", 100_201)], &[])
            + &relation(5005, &[("relation", 5004), ("way", 1012)], &[])
            + &relation(5006, &[], &[("type", "empty")]);
        let (db, dir) = grid_db("rv-union-known", &rels);
        let b = "(48.8405,2.3405,48.8425,2.3425)";
        let prelude = format!("node{b}->.n;way(bn.n)->.w;rel(bn.n)->.r1;rel(bw.w)->.r2;(.r1;.r2;)->.r;rel(id:5001,5002,5003,5004,5006)->.R;way(1011)->.w1;node(100000)->.x;");
        let names = ["_", "x", "n", "w", "all", "R"];
        let downs = [
            ">", ">>", ".w >", ".R >", ".R >>", ".r >>", ".w1 >", "node(w.w)", "node(w)", "node(w.w)[!highway]", "node(w.w)[highway]",
            "node(w.w:1,-1)", "way(r.R)", "node(r.R)", "rel(r.R)", "nwr(r.R)", "way(r)", "node(r)", "node(w.w1)", "node(w.w)(w.w1)",
            "way(r.R)[highway=residential]", "node(r.R:\"\")", "nw(r.R)", "rel(r)",
        ];
        let others = [
            ".n", ".w", ".r", ".R", ".w1", "._", ".x", "way(bn)", "way(bn.n)", "rel(bw)", "<", "node._(48.841,2.341,48.842,2.342)",
            "(.n; - node(w.w);)", "(.w; >;)", "(.R; >>;)", "make m n=count(nodes)",
            "make m n=x.count(nodes)", "convert t ::id=id()", "foreach.w((._;>;)->.y;)", "if (count(nodes) > 0) {(.n;node(w.w););}",
            "foreach.w->.it((.it;node(w.it););)", "for (t[\"type\"]) {(.r;way(r););}", "complete {(._;node(w););}",
            "node(id:100000,100001,342)", ".n->.x", "._->.x", ".x->._", "way(1012)", "(._;)", "(.x;node(w.w1);)",
        ];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let member = |rng: &mut Rng, depth: usize| -> String {
            let k = rng.below(10);
            let base = if k < 6 { rng.pick(&downs).to_string() } else { rng.pick(&others).to_string() };
            let into = if rng.below(3) == 0 { format!("->.{}", rng.pick(&names)) } else { String::new() };
            let _ = depth;
            if base.starts_with("out") || base.starts_with("foreach") || base.starts_with("if") || base.contains(" out") {
                format!("{base};")
            } else {
                format!("{base}{into};")
            }
        };
        let (mut runs, mut held_runs, mut errors) = (0, 0, 0);
        for round in 0..2500 {
            let n = 2 + rng.below(5) as usize;
            let mut body = String::new();
            for _ in 0..n {
                if rng.below(8) == 0 {
                    // a nested union
                    let m = 2 + rng.below(3) as usize;
                    let inner: String = (0..m).map(|_| member(&mut rng, 1)).collect();
                    let into = if rng.below(2) == 0 { format!("->.{}", rng.pick(&names)) } else { String::new() };
                    body += &format!("({inner}){into};");
                } else {
                    body += &member(&mut rng, 0);
                }
            }
            let into = if rng.below(2) == 0 { format!("->.{}", rng.pick(&names)) } else { String::new() };
            let mut un = format!("({body}){into};");
            un += match rng.below(4) {
                0 => "out ids;",
                1 => ".x out ids;",
                2 => "out count;.x out count;",
                _ => "",
            };
            let wrapped = match rng.below(7) {
                0 => format!("foreach.R->.it({un});"),
                1 => format!("if (1) {{{un}}}"),
                2 => format!("for (t[\"type\"]) {{{un}}}"),
                3 => format!("foreach.w1({un});"),
                4 => format!("complete(3) {{{un}}}"),
                _ => un,
            };
            let q = format!("[out:csv(::type,::id;false)];{prelude}{wrapped}out;.x out;.n out count;.w out count;.all out;.R out;");
            KNOWN_OFF.set(true);
            let plain = output_or_rejected(&db, &q);
            KNOWN_OFF.set(false);
            KNOWN_HELD.set(0);
            let got = output_or_rejected(&db, &q);
            assert_eq!(got, plain, "round {round}: {q}");
            runs += 1;
            held_runs += (KNOWN_HELD.get() > 0) as usize;
            errors += (plain.contains("error") || plain.starts_with("REJECTED")) as usize;
        }
        eprintln!("rv_union_known_random: {runs} queries, {held_runs} left elements out, {errors} with errors");
        assert!(held_runs > 300, "{held_runs}");
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Patch 07: random ternary chains (orders, ties, odd literals, mixed
    /// operators and coordinates, nested chains in the branches) give the
    /// values of the evaluation level by level, in `convert`, `for` and
    /// `(if:)`.
    #[test]
    #[ignore]
    fn rv_chains_random() {
        use crate::exec::eval::{CHAINS_OFF, CHAIN_HITS};
        let (db, dir) = grid_db("rv-chains", "");
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        // grid nodes at 48.82 + 0.002 i, 2.32 + 0.002 j: the box holds 11 x 11
        let lits = |rng: &mut Rng, lat: bool| -> String {
            let (o, step) = if lat { (48.82, 0.002) } else { (2.32, 0.002) };
            let i = rng.below(13) as f64 - 1.0;
            match rng.below(16) {
                0..=5 => format!("{:.3}", o + step * i),
                6 => format!("{:.4}", o + step * i + 0.001),
                7 => format!("{:.7}", o + step * i),
                8 => format!("\"{:.3}\"", o + step * i),
                9 => format!("{:.5}e1", (o + step * i) / 10.0),
                10 => rng.pick(&["\"nan\"", "\"inf\"", "\"-inf\"", "\"1e400\"", "\"-1e400\""]).to_string(),
                11 => rng.pick(&["\"48.83 \"", "\" 48.83\"", "\"abc\"", "\"\"", "\"0x30\"", "\"+48.83\"", "\"-0\"", "\"48,83\""]).to_string(),
                12 => format!("{:.9}", o + step * i + 1e-8),
                13 => format!("{:.8}", o + step * i - 5e-8),
                _ => format!("{:.3}", o + step * i),
            }
        };
        fn chain(rng: &mut Rng, lits: &dyn Fn(&mut Rng, bool) -> String, depth: usize) -> String {
            let lat = rng.below(2) == 0;
            let op = *["<", "<=", ">", ">="].iter().nth(rng.below(4) as usize).unwrap();
            let n = 1 + rng.below(14) as usize;
            let mode = rng.below(4); // 0 ascending, 1 descending, 2 random, 3 sorted for the op
            let mut ts: Vec<String> = (0..n).map(|_| lits(rng, lat)).collect();
            let num = |s: &String| crate::exec::eval::num::dbl(s.trim_matches('"')).unwrap_or(f64::NAN);
            match mode {
                0 => ts.sort_by(|a, b| num(a).partial_cmp(&num(b)).unwrap_or(std::cmp::Ordering::Equal)),
                1 => ts.sort_by(|a, b| num(b).partial_cmp(&num(a)).unwrap_or(std::cmp::Ordering::Equal)),
                3 if op.starts_with('<') => ts.sort_by(|a, b| num(a).partial_cmp(&num(b)).unwrap_or(std::cmp::Ordering::Equal)),
                3 => ts.sort_by(|a, b| num(b).partial_cmp(&num(a)).unwrap_or(std::cmp::Ordering::Equal)),
                _ => {}
            }
            let leaf = |rng: &mut Rng, k: usize| -> String {
                match rng.below(9) {
                    0 if depth < 2 => chain(rng, lits, depth + 1),
                    1 => "id()".into(),
                    2 => format!("\"s{k}\""),
                    3 => "lat()".into(),
                    4 => format!("id()*{}", k + 2),
                    _ => k.to_string(),
                }
            };
            let mut e = leaf(rng, n);
            for k in (0..n).rev() {
                // now and then a level of another form
                let c = match rng.below(24) {
                    0 => format!("{}{op}{}", if lat { "lon()" } else { "lat()" }, ts[k]),
                    1 => format!("{}=={}", if lat { "lat()" } else { "lon()" }, ts[k]),
                    2 => format!("{}{op}{}", ts[k], if lat { "lat()" } else { "lon()" }),
                    3 => format!("{}{}{}", if lat { "lat()" } else { "lon()" }, ["<", "<=", ">", ">="][rng.below(4) as usize], ts[k]),
                    4 => format!("lat()<lon()"),
                    5 => format!("{}!={}", if lat { "lat()" } else { "lon()" }, ts[k]),
                    _ => format!("{}{op}{}", if lat { "lat()" } else { "lon()" }, ts[k]),
                };
                let l = leaf(rng, k);
                e = format!("({c}?{l}:{e})");
            }
            e
        }
        let (mut hits, mut errors) = (0usize, 0usize);
        for round in 0..1500 {
            let m = 1 + rng.below(6) as usize;
            let cols: Vec<String> = (0..m).map(|_| chain(&mut rng, &lits, 0)).collect();
            let names: Vec<String> = (0..m).map(|k| format!("c{k}")).collect();
            let entries: Vec<String> = cols.iter().enumerate().map(|(k, e)| format!("c{k}={e}")).collect();
            let box_ = "(48.819,2.319,48.841,2.341)";
            let q = match rng.below(5) {
                0 => format!("[out:csv(v,n;false)];node{box_};for({}){{make c v=_.val,n=count(nodes);out;}}", cols[0]),
                1 => format!("[out:csv(::id;false)];node{box_}(if:({})==({}));out;", cols[0], cols[m - 1]),
                2 => format!("[out:csv(::id,{};false)];(node{box_};way{box_};);convert r ::id=id(),{};out;", names.join(","), entries.join(",")),
                _ => format!("[out:csv(::id,{};false)];node{box_};convert r ::id=id(),{};out;", names.join(","), entries.join(",")),
            };
            CHAINS_OFF.set(true);
            let plain = output_or_rejected(&db, &q);
            CHAINS_OFF.set(false);
            CHAIN_HITS.set(0);
            let got = output_or_rejected(&db, &q);
            assert_eq!(got, plain, "round {round}: {q}");
            hits += (CHAIN_HITS.get() > 0) as usize;
            errors += (plain.contains("error") || plain.starts_with("REJECTED")) as usize;
        }
        eprintln!("rv_chains_random: {hits} queries used chains, {errors} with errors");
        assert!(hits > 300, "{hits}");
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Patch 02: random boxes (edges on nodes, a step of 1e-7 beside them,
    /// the antimeridian, the poles, south above north, global bbox) give
    /// the answers of the geometry test after the scan.
    #[test]
    #[ignore]
    fn rv_node_box_random() {
        let far = "<node id=\"300001\" lat=\"10.0\" lon=\"179.9995\" {m}/><node id=\"300002\" lat=\"10.0\" lon=\"-179.9995\" {m}/><node id=\"300003\" lat=\"10.0\" lon=\"180.0\" {m}/><node id=\"300004\" lat=\"10.0\" lon=\"-180.0\" {m}/><node id=\"300005\" lat=\"10.0005\" lon=\"179.0\" {m}><tag k=\"name\" v=\"x\"/></node><node id=\"300006\" lat=\"90.0\" lon=\"0.0\" {m}/><node id=\"300007\" lat=\"-90.0\" lon=\"0.0\" {m}/><node id=\"300008\" lat=\"89.9999999\" lon=\"180.0\" {m}/><node id=\"300009\" lat=\"-89.9999999\" lon=\"-180.0\" {m}/><node id=\"300010\" lat=\"48.8400500\" lon=\"2.3400501\" {m}/>\n";
        let (db, dir) = osm_db("rv-node-box", &grid_osm_with(far, "", ""));
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        let lat = |rng: &mut Rng| -> String {
            match rng.below(10) {
                0..=4 => format!("{:.5}", 48.84005 + rng.below(200) as f64 * 0.0001),
                5 => format!("{:.7}", 48.84005 + rng.below(200) as f64 * 0.0001 + [1e-7, -1e-7][rng.below(2) as usize]),
                6 => format!("{:.3}", 48.82 + rng.below(30) as f64 * 0.002),
                7 => rng.pick(&["90", "-90", "89.9999999", "-89.9999999", "10.0", "9.9", "10.1"]).to_string(),
                _ => format!("{:.8}", 48.84 + rng.below(1000) as f64 * 0.00002),
            }
        };
        let lon = |rng: &mut Rng| -> String {
            match rng.below(10) {
                0..=4 => format!("{:.5}", 2.34005 + rng.below(200) as f64 * 0.0001),
                5 => format!("{:.7}", 2.34005 + rng.below(200) as f64 * 0.0001 + [1e-7, -1e-7][rng.below(2) as usize]),
                6 => format!("{:.3}", 2.32 + rng.below(30) as f64 * 0.002),
                7 => rng.pick(&["180", "-180", "179.9995", "-179.9995", "179.9", "-179.9", "0"]).to_string(),
                _ => format!("{:.8}", 2.34 + rng.below(1000) as f64 * 0.00002),
            }
        };
        let bbox = |rng: &mut Rng| -> String {
            let (mut s, mut n) = (lat(rng), lat(rng));
            if rng.below(6) != 0 && s.parse::<f64>().unwrap() > n.parse::<f64>().unwrap() {
                std::mem::swap(&mut s, &mut n);
            }
            let (mut w, mut e) = (lon(rng), lon(rng));
            if rng.below(4) != 0 && w.parse::<f64>().unwrap() > e.parse::<f64>().unwrap() {
                std::mem::swap(&mut w, &mut e);
            }
            format!("({s},{w},{n},{e})")
        };
        let c = "[out:csv(::type,::id;false)]";
        let (mut lines, mut errors) = (0usize, 0usize);
        for round in 0..3000 {
            let (b, b2) = (bbox(&mut rng), bbox(&mut rng));
            let q = match rng.below(9) {
                0 => format!("{c};node{b}{b2};out;"),
                1 => format!("[bbox:{}]{c};node{b};out;", &b2[1..b2.len() - 1]),
                2 => format!("{c};node{b}(around:150,48.841,2.341);out;"),
                3 => format!("{c};node{b}[!name];out;"),
                4 => format!("{c};node{b}(if:id()<100100);out;"),
                5 => format!("{c};(node{b};way{b};rel{b};);out;"),
                6 => format!("{c};node{b}->.a;way(bn.a);out;node.a{b2};out;"),
                7 => format!("{c};node{b}(poly:\"48.84 2.34 48.86 2.34 48.85 2.36\");out;"),
                _ => format!("{c};node{b};out;"),
            };
            NODE_BOX_PLAIN.set(true);
            let plain = output_or_rejected(&db, &q);
            NODE_BOX_PLAIN.set(false);
            let got = output_or_rejected(&db, &q);
            assert_eq!(got, plain, "round {round}: {q}");
            lines += plain.lines().count();
            errors += (plain.contains("error") || plain.starts_with("REJECTED")) as usize;
        }
        eprintln!("rv_node_box_random: {lines} lines, {errors} with errors");
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Review tests (rv-around): differential fuzz of the around changes.
#[cfg(test)]
mod rv_tests {
    use super::*;
    use crate::geo::{Pt, great_circle_dist, ARC_TO_METER};
    use rustc_hash::FxHashMap;

    struct Rng(u64);
    impl Rng {
        fn f(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    fn wrap(lon: f64) -> f64 {
        let l = (lon + 180.0).rem_euclid(360.0) - 180.0;
        if l == -180.0 && lon > 0.0 { 180.0 } else { l }
    }

    fn dest(lat: f64, lon: f64, bearing: f64, dist: f64) -> (f64, f64) {
        let (p1, l1, th, d) = (lat.to_radians(), lon.to_radians(), bearing, dist / ARC_TO_METER);
        let p2 = (p1.sin() * d.cos() + p1.cos() * d.sin() * th.cos()).clamp(-1.0, 1.0).asin();
        let l2 = l1 + (th.sin() * d.sin() * p1.cos()).atan2(d.cos() - p1.sin() * p2.sin());
        (p2.to_degrees().clamp(-90.0, 90.0), wrap(l2.to_degrees()))
    }

    fn grow(r: &Rect) -> Rect {
        Rect { y0: r.y0.saturating_sub(1), x0: r.x0.saturating_sub(1), y1: r.y1.saturating_add(1), x1: r.x1.saturating_add(1) }
    }

    /// The fine source of `Engine::around_fine`, from a position map.
    fn fine_of(src: &AroundSource, pos: &FxHashMap<Id, Pt>, boxes: &[Rect], r: f64) -> Option<AroundSource> {
        let ways = src.lazy_near(boxes);
        if ways.is_empty() {
            return None;
        }
        let mut fine = AroundSource::default();
        let mut pts = Vec::new();
        for w in ways {
            pts.clear();
            if w.iter().all(|id| pos.get(id).map(|p| pts.push((p.lat(), p.lon()))).is_some()) {
                fine.add_line(&pts);
            }
        }
        fine.prepare_index(r);
        Some(fine)
    }

    /// Patch 06: a source with lazy ways answers as the source with all
    /// vertices, at the poles, near the antimeridian, for radii from 0 to
    /// 2000 km, for points, lines and relations, tested in batches; and every
    /// candidate that passes lies in the cover and in the cells.
    #[test]
    #[ignore = "randomized, about 11 s: cargo test --release --lib rv_lazy_ways_match_all_vertices -- --ignored"]
    fn rv_lazy_ways_match_all_vertices() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let places: [(f64, f64); 10] = [(47.0, 8.0), (0.0, 0.0), (70.0, 20.0), (84.9, -40.0), (89.95, 10.0), (-89.93, 100.0), (60.0, 179.997), (-17.0, -179.997), (65.0, 180.0), (-60.0, 0.0)];
        let radii: [f64; 9] = [0.0, 0.1, 1.0, 1.04, 10.0, 300.0, 5000.0, 100_000.0, 2_000_000.0];
        let (mut hits, mut total, mut lazy_n) = (0usize, 0usize, 0usize);
        for (pi, &(clat, clon)) in places.iter().enumerate() {
            for &r in &radii {
                let spread: f64 = 0.03;
                let cos = clat.to_radians().cos().max(0.02);
                let mut pos: FxHashMap<Id, Pt> = FxHashMap::default();
                let mut lazy = AroundSource::default();
                let mut plain = AroundSource::default();
                let mut nid: Id = 1;
                // a few nodes as points
                for _ in 0..5 {
                    let p = Pt::from_ll((clat + (rng.f() - 0.5) * spread).clamp(-90.0, 90.0), wrap(clon + (rng.f() - 0.5) * spread / cos));
                    lazy.points.push((p.lat(), p.lon()));
                    plain.points.push((p.lat(), p.lon()));
                }
                for k in 0..80 {
                    let n = 1 + (rng.f() * 6.0) as usize;
                    let (slat, slon) = (clat + (rng.f() - 0.5) * spread, clon + (rng.f() - 0.5) * spread / cos);
                    let mut nodes = Vec::new();
                    let mut first = None;
                    for j in 0..n {
                        // closed ways, repeated nodes, a missing node
                        if j == n - 1 && n > 3 && k % 5 == 0 {
                            nodes.push(first.unwrap());
                            continue;
                        }
                        if j > 0 && k % 7 == 1 && j == 1 {
                            nodes.push(*nodes.last().unwrap());
                            continue;
                        }
                        let p = Pt::from_ll((slat + (rng.f() - 0.5) * 0.008).clamp(-90.0, 90.0), wrap(slon + (rng.f() - 0.5) * 0.008 / cos));
                        pos.insert(nid, p);
                        nodes.push(nid);
                        first.get_or_insert(nid);
                        nid += 1;
                    }
                    if k % 13 == 3 {
                        nodes.push(999_999_999);
                    }
                    let mut bbox = Rect::EMPTY;
                    let mut missing = false;
                    for id in &nodes {
                        match pos.get(id) {
                            Some(p) => bbox.extend(*p),
                            None => missing = true,
                        }
                    }
                    let w = Way { id: k as Id, nodes, tags: Default::default(), bbox, idx: 0, missing };
                    let geom = EGeom::Line(w.nodes.iter().map(|id| pos.get(id).copied()).collect());
                    plain.add(&geom);
                    if bbox.is_empty() {
                        continue;
                    }
                    let is_lazy = !w.bbox.may_cross() && (w.bbox.y1 >> 16) - (w.bbox.y0 >> 16) < LAZY_TILES && (w.bbox.x1 >> 16) - (w.bbox.x0 >> 16) < LAZY_TILES;
                    if is_lazy {
                        lazy.add_lazy(&w);
                        lazy_n += 1;
                    } else {
                        lazy.add(&geom);
                    }
                }
                lazy.prepare(r);
                plain.prepare(r);
                let rects = lazy.bbox_rects(r).into_owned();
                let deg = r * 360.0 / 40_000_000.0;
                // candidates in batches of 16
                let mut batch: Vec<EGeom> = Vec::new();
                for c in 0..1600 {
                    let reach = (deg * 1.3 + spread * 0.7).min(170.0);
                    let rp = |rng: &mut Rng| Pt::from_ll((clat + (rng.f() - 0.5) * 2.0 * reach).clamp(-90.0, 90.0), wrap(clon + (rng.f() - 0.5) * 2.0 * reach / cos));
                    let p = rp(&mut rng);
                    let g = match c % 6 {
                        0 | 1 => EGeom::Point(p),
                        2 => EGeom::Line(vec![Some(p), Some(Pt::from_ll((p.lat() + (rng.f() - 0.5) * 0.01).clamp(-90.0, 90.0), wrap(p.lon() + (rng.f() - 0.5) * 0.01)))]),
                        3 => EGeom::Line(vec![Some(p), Some(rp(&mut rng)), Some(rp(&mut rng))]),
                        4 => EGeom::Line(vec![Some(p), Some(Pt::from_ll(p.lat(), wrap(p.lon() + 0.02 * (rng.f() - 0.5) + if pi >= 6 && pi <= 8 { 180.0 } else { 0.0 })))]),
                        _ => EGeom::Multi { points: vec![rp(&mut rng)], lines: vec![vec![Some(p), Some(rp(&mut rng))], vec![Some(rp(&mut rng)), None]] },
                    };
                    batch.push(g);
                    if batch.len() < 16 {
                        continue;
                    }
                    let boxes: Vec<Rect> = batch.iter().filter_map(geom_box).collect();
                    let fine = fine_of(&lazy, &pos, &boxes, r);
                    for g in batch.drain(..) {
                        let want = around_match(&plain, &g, r);
                        let got = around_match_with(&lazy, fine.as_ref(), &g, r);
                        assert_eq!(got, want, "place {clat},{clon} radius {r}: {g:?}");
                        total += 1;
                        hits += want as usize;
                        if want {
                            let mut b = Rect::EMPTY;
                            for q in g.points() {
                                b.extend(q);
                            }
                            let a = if matches!(g, EGeom::Point(_)) { b } else { b.arc_box() };
                            if !a.may_cross() {
                                assert!(rects.iter().any(|x| grow(x).intersects(&a)), "outside the cover: place {clat},{clon} radius {r}: {g:?}");
                                assert!(lazy.near_box(&rects, &a), "outside the cells: place {clat},{clon} radius {r}: {g:?}");
                            }
                        }
                    }
                }
            }
        }
        eprintln!("rv_lazy: {hits} hits of {total}, {lazy_n} lazy ways");
        assert!(hits > 5000 && hits < total - 5000, "{hits} {total}");
    }

    /// Patch 08: the bounds before the distance do not change any answer at
    /// the boundary of the circle, for points on the quantized grid next to
    /// the boundary, at small and large radii and near the poles.
    #[test]
    fn rv_point_bounds_at_the_boundary() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let exact = |c: (f64, f64), p: Pt, r: f64| (r > 0.0 && great_circle_dist(c.0, c.1, p.lat(), p.lon()) <= r) || ((c.0 - p.lat()).abs() < 1e-7 && (c.1 - p.lon()).abs() < 1e-7);
        let mut checked = 0usize;
        let mut ins = 0usize;
        for r in [0.0f64, 0.05, 0.1, 0.5, 0.999, 1.0, 1.0000001, 1.01, 1.05, 1.5, 2.0, 7.3, 100.0, 12_345.0, 1e6, 5e6, 9.5e6, 9.6e6, 1e7, 1.9e7] {
            for (lat, lon) in [(0.0f64, 0.0f64), (47.3, 8.5), (-33.9, 151.2), (80.0, -170.0), (89.99, 45.0), (89.99999, 0.0), (-89.9999, 179.9999), (90.0, 0.0), (-90.0, 10.0), (12.0, 179.99999)] {
                let c = Pt::from_ll(lat, lon);
                let c = (c.lat(), c.lon());
                let mut src = AroundSource::default();
                src.points.push(c);
                src.prepare(r);
                for _ in 0..150 {
                    let dir = rng.f() * std::f64::consts::TAU;
                    // bisection on the distance along the direction
                    let (mut lo, mut hi) = (0.0f64, (r * 1.5).max(0.5));
                    for _ in 0..80 {
                        let mid = (lo + hi) / 2.0;
                        let (a, b) = dest(c.0, c.1, dir, mid);
                        if great_circle_dist(c.0, c.1, a, b) <= r { lo = mid } else { hi = mid }
                    }
                    let (a, b) = dest(c.0, c.1, dir, lo);
                    let q = Pt::from_ll(a, b);
                    for dy in -2i64..=2 {
                        for dx in -2i64..=2 {
                            let y = (q.y as i64 + dy).clamp(Pt::from_ll(-90.0, 0.0).y as i64, Pt::from_ll(90.0, 0.0).y as i64) as u32;
                            let x = (q.x as i64 + dx).clamp(Pt::from_ll(0.0, -180.0).x as i64, Pt::from_ll(0.0, 180.0).x as i64) as u32;
                            let p = Pt { y, x };
                            let want = exact(c, p, r);
                            assert_eq!(around_match(&src, &EGeom::Point(p), r), want, "radius {r} at {c:?}: {} {}", p.lat(), p.lon());
                            checked += 1;
                            ins += want as usize;
                        }
                    }
                }
            }
        }
        eprintln!("rv_bounds: {checked} checked, {ins} inside");
        assert!(ins > 10_000 && checked - ins > 10_000);
    }

    /// Patch 04 (and 03): long segments in pieces. Every point and short line
    /// that passes lies in the cover and in the cells, the index answers as
    /// the scan, and a polyline answers as segments tested at both ends.
    #[test]
    fn rv_long_segments_cover_fuzz() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut hits = 0usize;
        let mut cut = 0usize;
        for case in 0..400 {
            let r: f64 = [0.0, 1.0, 25.0, 700.0, 5000.0, 50_000.0, 500_000.0][case % 7];
            let a = ((rng.f() - 0.5) * 168.0, (rng.f() - 0.5) * 360.0);
            let len = [5e3, 5e4, 3e5, 1e6, 3e6, 9e6][(case / 7) % 6] * (0.5 + rng.f());
            let b = dest(a.0, a.1, rng.f() * std::f64::consts::TAU, len);
            // special cases: exactly 180 degrees apart, ends at the antimeridian
            let (a, b) = match case % 23 {
                0 => ((a.0 * 0.5, 170.0), (a.0 * 0.5 + 10.0, -10.0)),
                1 => ((a.0 * 0.5, 179.5), (a.0 * 0.5 + 3.0, 170.0)),
                2 => ((83.0, a.1), (84.5, wrap(a.1 + 170.0))),
                _ => (a, b),
            };
            let qa = Pt::from_ll(a.0, a.1);
            let qb = Pt::from_ll(b.0, b.1);
            let (a, b) = ((qa.lat(), qa.lon()), (qb.lat(), qb.lon()));
            let mid = dest(a.0, a.1, rng.f() * std::f64::consts::TAU, len * 0.3);
            let qm = Pt::from_ll(mid.0, mid.1);
            let mid = (qm.lat(), qm.lon());
            // a polyline a - b - mid, and the same with both ends tested
            let mut line = AroundSource::default();
            line.add_line(&[a, b, mid]);
            let mut both = AroundSource::default();
            both.segments.push(crate::geo::Segment::new(a.0, a.1, b.0, b.1));
            both.segments.push(crate::geo::Segment::new(b.0, b.1, mid.0, mid.1));
            // indexed copy with more segments far away
            let mut many = AroundSource::default();
            many.add_line(&[a, b, mid]);
            for _ in 0..40 {
                let c = (-a.0 * 0.5, wrap(a.1 + 90.0 + rng.f()));
                many.add_line(&[c, (c.0 + 0.001, wrap(c.1 + 0.001))]);
            }
            let mut plain_many = AroundSource::default();
            plain_many.segments = many.segments.clone();
            line.prepare(r);
            many.prepare(r);
            let rects = line.bbox_rects(r).into_owned();
            cut += (rects.len() > 6) as usize;
            let many_rects = many.bbox_rects(r).into_owned();
            let (ca, cb, cm) = (crate::geo::cartesian(a.0, a.1), crate::geo::cartesian(b.0, b.1), crate::geo::cartesian(mid.0, mid.1));
            for k in 0..600 {
                let (u, v) = if k % 2 == 0 { (ca, cb) } else { (cb, cm) };
                let t = rng.f() * 1.04 - 0.02;
                let w = u.scale(1.0 - t).add(&v.scale(t));
                let w = w.scale(1.0 / w.dot(&w).sqrt());
                let (plat, plon) = (w.0.asin().to_degrees(), w.1.atan2(w.2).to_degrees());
                let off = r * [1.0, 0.999, 1.001, 1.02, 0.5][k % 5] + rng.f() * 2.0;
                let (qlat, qlon) = dest(plat, plon, rng.f() * std::f64::consts::TAU, off);
                let p = Pt::from_ll(qlat, qlon);
                let q = Pt::from_ll((p.lat() + (rng.f() - 0.5) * 0.002).clamp(-90.0, 90.0), wrap(p.lon() + (rng.f() - 0.5) * 0.002));
                for g in [EGeom::Point(p), EGeom::Line(vec![Some(p), Some(q)])] {
                    let want = around_match(&both, &g, r);
                    assert_eq!(around_match(&line, &g, r), want, "case {case} {a:?} {b:?} {mid:?} r {r}: {g:?}");
                    assert_eq!(around_match(&many, &g, r), around_match(&plain_many, &g, r), "index: case {case} {a:?} {b:?} r {r}: {g:?}");
                    if want {
                        hits += 1;
                        let mut bx = Rect::point(p);
                        if matches!(g, EGeom::Line(_)) {
                            bx.extend(q);
                        }
                        let bx = if matches!(g, EGeom::Point(_)) { bx } else { bx.arc_box() };
                        if !bx.may_cross() {
                            assert!(rects.iter().any(|x| grow(x).intersects(&bx)), "outside the cover: case {case} {a:?} {b:?} {mid:?} r {r}: {g:?}");
                            assert!(many.near_box(&many_rects, &bx), "outside the cells: case {case} {a:?} {b:?} {mid:?} r {r}: {g:?}");
                        }
                    }
                }
            }
        }
        eprintln!("rv_long: {hits} hits, {cut} cut covers");
        assert!(hits > 50_000 && cut > 50, "{hits} {cut}");
    }
}
