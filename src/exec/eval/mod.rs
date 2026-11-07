//! Evaluators: expressions for `make`, `convert`, `(if:)`, `if`, `for`
//! (docs/spec/evaluators.md).
//!
//! Every value is a string, a geometry or (only for `keys()`) a list of
//! strings. `None` results model the C++ "null task" of aggregators over
//! sets that do not exist.

pub mod geomfn;
pub mod num;
pub mod rhr;

use super::Engine;
use super::query::{Plan, QElem};
use super::set::{Derived, Set};
use crate::geo::cartesian;
use crate::model::*;
use crate::output::{DGeom, LatLon};
use crate::ql::ast::*;
use crate::store::Kind;
use anyhow::Result;
use num::{date, dbl, fix, g14, int, pdbl, truthy};
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write;
use std::rc::Rc;

#[derive(Clone, Debug)]
pub enum V {
    S(String),
    G(DGeom),
    L(Vec<String>),
}

impl V {
    pub fn into_string(self) -> String {
        match self {
            V::S(s) => s,
            V::L(l) => l.join(";"),
            V::G(_) => "<Opaque_Geometry>".into(),
        }
    }
}

/// An element in evaluation context.
#[derive(Clone, Copy)]
pub enum El<'a> {
    N(&'a Node),
    W(&'a Way),
    R(&'a Relation),
    A(&'a Area),
    D(&'a Derived),
}

impl<'a> El<'a> {
    fn key(&self) -> (u8, Id) {
        match self {
            El::N(n) => (0, n.id),
            El::W(w) => (1, w.id),
            El::R(r) => (2, r.id),
            El::A(a) => (3, a.id),
            El::D(d) => (4, d.id),
        }
    }

    fn tag(&self, k: &str) -> Option<&'a str> {
        match self {
            El::N(n) => n.tags.get(k),
            El::W(w) => w.tags.get(k),
            El::R(r) => r.tags.get(k),
            El::A(a) => a.tags.get(k),
            El::D(d) => d.tags.iter().find(|(tk, _)| tk == k).map(|(_, v)| v.as_str()),
        }
    }

    fn keys(&self) -> Vec<&'a str> {
        let mut v: Vec<&str> = match self {
            El::N(n) => n.tags.iter().map(|(k, _)| k).collect(),
            El::W(w) => w.tags.iter().map(|(k, _)| k).collect(),
            El::R(r) => r.tags.iter().map(|(k, _)| k).collect(),
            El::A(a) => a.tags.iter().map(|(k, _)| k).collect(),
            El::D(d) => d.tags.iter().map(|(k, _)| k.as_str()).collect(),
        };
        v.sort_unstable();
        v.dedup();
        v
    }

    fn tag_count(&self) -> usize {
        match self {
            El::N(n) => n.tags.len(),
            El::W(w) => w.tags.len(),
            El::R(r) => r.tags.len(),
            El::A(a) => a.tags.len(),
            El::D(d) => d.tags.len(),
        }
    }
}

/// Evaluation context: current element, member position, generic key.
#[derive(Clone, Copy, Default)]
pub struct Cx<'a> {
    pub el: Option<El<'a>>,
    pub pos: Option<usize>,
    pub key: Option<&'a str>,
}

/// Elements of a set in the C++ iteration order (kind, storage index, id).
pub fn ordered(s: &Set) -> Vec<El<'_>> {
    let mut out = Vec::with_capacity(s.len());
    let mut nodes: Vec<&Node> = s.nodes.iter().collect();
    nodes.sort_by_key(|n| (crate::geo::tile_of(n.pos), n.id));
    out.extend(nodes.into_iter().map(El::N));
    let mut ways: Vec<&Way> = s.ways.iter().collect();
    ways.sort_by_key(|w| (crate::geo::cpp_index_order(w.idx), w.id));
    out.extend(ways.into_iter().map(El::W));
    let mut rels: Vec<&Relation> = s.rels.iter().collect();
    rels.sort_by_key(|r| (crate::geo::cpp_index_order(r.idx), r.id));
    out.extend(rels.into_iter().map(El::R));
    let mut areas: Vec<&Area> = s.areas.iter().collect();
    areas.sort_by_key(|a| (crate::geo::cpp_index_order(a.idx), a.id));
    out.extend(areas.into_iter().map(El::A));
    out.extend(s.derived.iter().map(El::D));
    out
}

/// Whether an expression calls one of the element functions `names`.
fn calls_any(e: &Expr, names: &[&str]) -> bool {
    match e {
        Expr::Call(name, args) => names.contains(&name.as_str()) || args.iter().any(|a| calls_any(a, names)),
        Expr::Agg { args, .. } => args.iter().any(|a| calls_any(a, names)),
        Expr::Unary(_, a) => calls_any(a, names),
        Expr::Binary(_, a, b) => calls_any(a, names) || calls_any(b, names),
        Expr::Ternary(a, b, c) => calls_any(a, names) || calls_any(b, names) || calls_any(c, names),
        _ => false,
    }
}

/// Whether an expression needs metadata of its elements.
fn needs_meta(e: &Expr) -> bool {
    calls_any(e, &["version", "timestamp", "changeset", "uid", "user"])
}

/// Whether an expression needs node positions of its elements.
fn needs_geom(e: &Expr) -> bool {
    calls_any(e, &["geom", "length", "lat", "lon", "angle"])
}

const MULTIPLE: &str = "< multiple values found >";

pub struct Evaluator<'e, 's, W: Write> {
    eng: &'e Engine<'s, W>,
    agg_cache: RefCell<FxHashMap<(usize, Option<String>), Option<V>>>,
    meta_cache: RefCell<FxHashMap<(u8, Id), Option<Meta>>>,
    geom_cache: RefCell<FxHashMap<(u8, Id), DGeom>>,
    /// Member ways of relations, loaded by `prefetch`.
    way_cache: RefCell<FxHashMap<Id, Rc<Way>>>,
    /// Keys of the sets used by `S::=`, computed once per statement.
    set_keys_cache: RefCell<FxHashMap<String, Rc<Vec<String>>>>,
    /// Aggregator sets as they were when the evaluator was created.
    sets: RefCell<FxHashMap<String, Option<Rc<Set>>>>,
}

fn i_str(v: i64) -> String {
    v.to_string()
}

impl<'e, 's, W: Write> Evaluator<'e, 's, W> {
    pub fn new(eng: &'e Engine<'s, W>) -> Self {
        Evaluator {
            eng,
            agg_cache: RefCell::new(FxHashMap::default()),
            meta_cache: RefCell::new(FxHashMap::default()),
            geom_cache: RefCell::new(FxHashMap::default()),
            way_cache: RefCell::new(FxHashMap::default()),
            set_keys_cache: RefCell::new(FxHashMap::default()),
            sets: RefCell::new(FxHashMap::default()),
        }
    }

    fn set_of(&self, name: &str) -> Option<Rc<Set>> {
        if let Some(s) = self.sets.borrow().get(name) {
            return s.clone();
        }
        let s = self.eng.lookup(name).cloned();
        self.sets.borrow_mut().insert(name.to_string(), s.clone());
        s
    }

    /// Fetch metadata for many elements at once.
    /// Batch-load what the expressions will need for these elements.
    pub fn prefetch<'x>(&self, els: &[El], exprs: impl IntoIterator<Item = &'x Expr>) -> Result<()> {
        let (mut meta, mut geom) = (false, false);
        for e in exprs {
            meta |= needs_meta(e);
            geom |= needs_geom(e);
        }
        if meta {
            self.prefetch_meta(els)?;
        }
        if geom {
            self.prefetch_geom(els)?;
        }
        Ok(())
    }

    /// Load the node positions of all ways and relation members in one pass.
    fn prefetch_geom(&self, els: &[El]) -> Result<()> {
        let mut nids: Vec<Id> = Vec::new();
        let mut wids: Vec<Id> = Vec::new();
        for el in els {
            match el {
                El::W(w) => nids.extend_from_slice(&w.nodes),
                El::R(r) => {
                    for m in &r.members {
                        match m.mtype {
                            ElemType::Node => nids.push(m.id),
                            ElemType::Way => wids.push(m.id),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if !wids.is_empty() {
            wids.sort_unstable();
            wids.dedup();
            let mut cache = self.way_cache.borrow_mut();
            for w in self.eng.f.by_ids::<Way>(&wids)? {
                nids.extend_from_slice(&w.nodes);
                cache.insert(w.id, Rc::new(w));
            }
        }
        if !nids.is_empty() {
            nids.sort_unstable();
            nids.dedup();
            self.eng.f.node_positions(&nids)?;
        }
        Ok(())
    }

    pub fn prefetch_meta(&self, els: &[El]) -> Result<()> {
        for (kind, code) in [(Kind::Node, 0u8), (Kind::Way, 1), (Kind::Relation, 2)] {
            let ids: Vec<Id> = els.iter().filter(|e| e.key().0 == code).map(|e| e.key().1).collect();
            if ids.is_empty() {
                continue;
            }
            let metas = self.eng.f.meta(kind, &ids)?;
            let mut cache = self.meta_cache.borrow_mut();
            for id in ids {
                cache.insert((code, id), metas.get(&id).copied());
            }
        }
        Ok(())
    }

    fn meta(&self, el: El) -> Result<Option<Meta>> {
        let key = el.key();
        if key.0 > 2 {
            return Ok(None);
        }
        if let Some(m) = self.meta_cache.borrow().get(&key) {
            return Ok(*m);
        }
        let kind = [Kind::Node, Kind::Way, Kind::Relation][key.0 as usize];
        let m = self.eng.f.meta(kind, &[key.1])?.get(&key.1).copied();
        self.meta_cache.borrow_mut().insert(key, m);
        Ok(m)
    }

    /// Sorted distinct tag keys of a set (`S::=`), computed once.
    fn keys_of_set(&self, set: &str) -> Rc<Vec<String>> {
        if let Some(k) = self.set_keys_cache.borrow().get(set) {
            return k.clone();
        }
        let keys = match self.set_of(set) {
            None => Vec::new(),
            Some(s) => {
                let mut ks: Vec<String> = ordered(&s).iter().flat_map(|e| e.keys()).map(String::from).collect();
                ks.sort_unstable();
                ks.dedup();
                ks
            }
        };
        let keys = Rc::new(keys);
        self.set_keys_cache.borrow_mut().insert(set.to_string(), keys.clone());
        keys
    }

    /// Geometry of an element (docs/spec/evaluators.md §10.1).
    pub fn geom(&self, el: El) -> Result<DGeom> {
        let key = el.key();
        if let Some(g) = self.geom_cache.borrow().get(&key) {
            return Ok(g.clone());
        }
        let g = match el {
            El::N(n) => DGeom::Point((n.pos.lat(), n.pos.lon())),
            El::W(w) => {
                let pos = self.eng.f.node_positions(&w.nodes)?;
                let pts: Vec<Option<LatLon>> = w.nodes.iter().map(|n| pos.get(n).map(|p| (p.lat(), p.lon()))).collect();
                if pts.iter().all(|p| p.is_some()) { DGeom::LineString(pts.into_iter().flatten().collect()) } else { DGeom::PartialWay(pts) }
            }
            El::R(r) => {
                let nids: Vec<Id> = r.members.iter().filter(|m| m.mtype == ElemType::Node).map(|m| m.id).collect();
                let wids: Vec<Id> = r.members.iter().filter(|m| m.mtype == ElemType::Way).map(|m| m.id).collect();
                let npos = self.eng.f.node_positions(&nids)?;
                let ways: FxHashMap<Id, Rc<Way>> = {
                    let cache = self.way_cache.borrow();
                    let missing: Vec<Id> = wids.iter().filter(|id| !cache.contains_key(id)).copied().collect();
                    let mut ways: FxHashMap<Id, Rc<Way>> = wids.iter().filter_map(|id| cache.get(id).map(|w| (*id, w.clone()))).collect();
                    drop(cache);
                    for w in self.eng.f.by_ids::<Way>(&missing)? {
                        ways.insert(w.id, Rc::new(w));
                    }
                    ways
                };
                let all_nd: Vec<Id> = ways.values().flat_map(|w| w.nodes.iter().copied()).collect();
                let wpos = self.eng.f.node_positions(&all_nd)?;
                let lists: Vec<Vec<Option<LatLon>>> = r
                    .members
                    .iter()
                    .map(|m| match m.mtype {
                        ElemType::Node => vec![npos.get(&m.id).map(|p| (p.lat(), p.lon()))],
                        ElemType::Way => match ways.get(&m.id) {
                            Some(w) => w.nodes.iter().map(|n| wpos.get(n).map(|p| (p.lat(), p.lon()))).collect(),
                            None => vec![],
                        },
                        _ => vec![],
                    })
                    .collect();
                let complete = lists.iter().all(|l| !l.is_empty() && l.iter().all(|p| p.is_some()));
                if complete {
                    DGeom::Collection(
                        lists
                            .into_iter()
                            .map(|l| {
                                let pts: Vec<LatLon> = l.into_iter().flatten().collect();
                                if pts.len() == 1 { DGeom::Point(pts[0]) } else { DGeom::LineString(pts) }
                            })
                            .collect(),
                    )
                } else {
                    DGeom::PartialRel(
                        lists
                            .into_iter()
                            .map(|l| {
                                if l.is_empty() {
                                    DGeom::Null
                                } else if l.len() == 1 && l[0].is_some() {
                                    DGeom::Point(l[0].unwrap())
                                } else {
                                    DGeom::PartialWay(l)
                                }
                            })
                            .collect(),
                    )
                }
            }
            El::A(_) => DGeom::Null,
            El::D(d) => d.geom.clone().unwrap_or(DGeom::Null),
        };
        self.geom_cache.borrow_mut().insert(key, g.clone());
        Ok(g)
    }

    /// Evaluate to a string; null tasks become "".
    pub fn s(&self, e: &Expr, cx: Cx) -> Result<String> {
        Ok(self.eval(e, cx)?.map(V::into_string).unwrap_or_default())
    }

    pub fn eval(&self, e: &Expr, cx: Cx) -> Result<Option<V>> {
        Ok(Some(match e {
            Expr::Lit(s) => V::S(s.clone()),
            Expr::Tag(k) => V::S(cx.el.and_then(|el| el.tag(k)).unwrap_or("").to_string()),
            Expr::Generic => V::S(match (cx.el, cx.key) {
                (Some(el), Some(k)) => el.tag(k).unwrap_or("").to_string(),
                _ => String::new(),
            }),
            Expr::SetProp { set, prop } => V::S(self.eng.set_keys.get(set).and_then(|m| m.get(prop)).cloned().unwrap_or_default()),
            Expr::Unary(op, a) => {
                let v = self.s(a, cx)?;
                V::S(match op {
                    UnOp::Not => if truthy(&v) { "0" } else { "1" }.into(),
                    UnOp::Neg => match int(&v) {
                        Some(i) => i_str(i.wrapping_neg()),
                        None => match dbl(&v) {
                            Some(d) => g14(-d),
                            None => "NaN".into(),
                        },
                    },
                })
            }
            Expr::Binary(op, a, b) => {
                let l = self.s(a, cx)?;
                let r = self.s(b, cx)?;
                V::S(binary(*op, &l, &r))
            }
            Expr::Ternary(c, a, b) => {
                let Some(cv) = self.eval(c, cx)? else { return Ok(Some(V::S("0".into()))) };
                let branch = if truthy(&cv.into_string()) { a } else { b };
                return Ok(Some(self.eval(branch, cx)?.unwrap_or(V::S(String::new()))));
            }
            Expr::Agg { set, name, args } => return self.aggregate(e, set.as_deref().unwrap_or("_"), name, args, cx),
            Expr::Call(name, args) => return self.call(name, args, cx),
        }))
    }

    fn aggregate(&self, node: &Expr, set: &str, name: &str, args: &[Expr], cx: Cx) -> Result<Option<V>> {
        let memo_key = (node as *const Expr as usize, cx.key.map(|k| k.to_string()));
        if let Some(v) = self.agg_cache.borrow().get(&memo_key) {
            return Ok(v.clone());
        }
        let v = self.aggregate_uncached(set, name, args, cx)?;
        self.agg_cache.borrow_mut().insert(memo_key, v.clone());
        Ok(v)
    }

    /// C++ "null task": an aggregator over a missing set, or over such an
    /// aggregator, has no value at all.
    fn null_task(&self, e: &Expr) -> bool {
        match e {
            Expr::Agg { set, name, args } => name != "count" && (self.set_of(set.as_deref().unwrap_or("_")).is_none() || args.first().is_some_and(|a| self.null_task(a))),
            _ => false,
        }
    }

    fn aggregate_uncached(&self, set: &str, name: &str, args: &[Expr], cx: Cx) -> Result<Option<V>> {
        let Some(s) = self.set_of(set) else {
            return Ok(if name == "count" { Some(V::S("0".into())) } else { None });
        };
        if name == "count" {
            let what = match args.first() {
                Some(Expr::Lit(w)) => w.as_str(),
                _ => "",
            };
            let (n, w, r, d) = (s.nodes.len(), s.ways.len(), s.rels.len(), s.areas.len() + s.derived.len());
            let c = match what {
                "nodes" => n,
                "ways" => w,
                "relations" => r,
                "deriveds" => d,
                "nwr" => n + w + r,
                "nw" => n + w,
                "wr" => w + r,
                "nr" => n + r,
                _ => 0,
            };
            return Ok(Some(V::S(c.to_string())));
        }
        let Some(arg) = args.first() else { return Ok(None) };
        if self.null_task(arg) {
            return Ok(None);
        }
        let els = ordered(&s);
        self.prefetch(&els, [arg])?;
        if name == "gcat" {
            let mut parts = Vec::new();
            for el in &els {
                if let Some(V::G(g)) = self.eval(arg, Cx { el: Some(*el), pos: None, key: cx.key })? {
                    parts.push(g);
                }
            }
            return Ok(if parts.is_empty() { None } else { Some(V::G(DGeom::Collection(parts))) });
        }
        let mut vals = Vec::with_capacity(els.len());
        for el in &els {
            vals.push(self.s(arg, Cx { el: Some(*el), pos: None, key: cx.key })?);
        }
        Ok(Some(V::S(match name {
            "u" => {
                let mut res: Option<&str> = None;
                for v in &vals {
                    if v.is_empty() {
                        continue;
                    }
                    match res {
                        None => res = Some(v),
                        Some(r) if r != v => {
                            res = Some(MULTIPLE);
                            break;
                        }
                        _ => {}
                    }
                }
                res.unwrap_or("").to_string()
            }
            "set" => {
                let mut v: Vec<&str> = vals.iter().map(|s| s.as_str()).filter(|s| !s.is_empty()).collect();
                v.sort_unstable();
                v.dedup();
                v.join(";")
            }
            "min" | "max" => min_max(vals.iter().map(|s| s.as_str()), name == "max", false),
            "sum" => {
                let mut ty = 0; // 0 int, 1 double, 2 string
                let mut l: i64 = 0;
                let mut d: f64 = 0.0;
                for v in &vals {
                    if ty == 0 {
                        match int(v) {
                            Some(i) => l = l.wrapping_add(i),
                            None => ty = 1,
                        }
                    }
                    if ty <= 1 {
                        match dbl(v) {
                            Some(x) => d += x,
                            None => ty = 2,
                        }
                    }
                }
                match ty {
                    0 => i_str(l),
                    1 => g14(d),
                    _ => "NaN".into(),
                }
            }
            _ => return Err(self.eng.abort(format!("Function \"{name}\" not known"))),
        })))
    }

    fn call(&self, name: &str, args: &[Expr], cx: Cx) -> Result<Option<V>> {
        let el = cx.el;
        let arg_s = |i: usize| -> Result<String> {
            match args.get(i) {
                Some(a) => self.s(a, cx),
                None => Ok(String::new()),
            }
        };
        let s = |v: String| Ok(Some(V::S(v)));
        match name {
            "t" => {
                let k = match args.first() {
                    Some(a) => self.s(a, Cx { pos: None, ..cx })?,
                    None => String::new(),
                };
                s(el.and_then(|e| e.tag(&k)).unwrap_or("").to_string())
            }
            "is_tag" => {
                let k = match args.first() {
                    Some(Expr::Lit(k)) => k.clone(),
                    Some(a) => self.s(a, cx)?,
                    None => String::new(),
                };
                s(if el.is_some_and(|e| e.tag(&k).is_some()) { "1" } else { "0" }.into())
            }
            "keys" => Ok(Some(V::L(el.map(|e| e.keys().into_iter().map(String::from).collect()).unwrap_or_default()))),
            "id" => s(el.map(|e| e.key().1.to_string()).unwrap_or_default()),
            "type" => s(match el {
                Some(El::N(_)) => "node".into(),
                Some(El::W(_)) => "way".into(),
                Some(El::R(_)) => "relation".into(),
                Some(El::A(_)) => "area".into(),
                Some(El::D(d)) => d.tname.clone(),
                None => String::new(),
            }),
            "version" | "timestamp" | "changeset" | "uid" | "user" => {
                let Some(e) = el else { return s(String::new()) };
                let Some(m) = self.meta(e)? else { return s(String::new()) };
                s(match name {
                    "version" => m.version.to_string(),
                    "timestamp" => format_timestamp(m.timestamp),
                    "changeset" => m.changeset.to_string(),
                    "uid" => m.uid.to_string(),
                    _ => self.eng.f.user_name(m.uid)?.unwrap_or_default(),
                })
            }
            "count_tags" => s(el.map(|e| e.tag_count()).unwrap_or(0).to_string()),
            "count_members" | "count_distinct_members" | "count_by_role" | "count_distinct_by_role" => s(count_members(name, args, el).to_string()),
            "is_closed" => s(match el {
                Some(El::W(w)) => if !w.nodes.is_empty() && w.nodes.first() == w.nodes.last() { "1" } else { "0" }.into(),
                _ => "NaW".into(),
            }),
            "geom" => match el {
                Some(e) => Ok(Some(V::G(self.geom(e)?))),
                None => Ok(Some(V::G(DGeom::Null))),
            },
            "length" => s(match el {
                Some(e @ (El::W(_) | El::R(_))) => fix(geomfn::length(&self.geom(e)?), 3),
                _ => "0".into(),
            }),
            "lat" | "lon" => {
                let c = match el {
                    Some(El::A(_)) | None => None,
                    Some(e) => self.geom(e)?.center(),
                };
                s(match c {
                    Some((la, lo)) => fix(if name == "lat" { la } else { lo }, 7),
                    None => "NaN".into(),
                })
            }
            "per_member" | "per_vertex" => {
                let positions: Vec<usize> = match (name, el) {
                    ("per_member", Some(El::W(w))) => (0..w.nodes.len()).collect(),
                    ("per_member", Some(El::R(r))) => (0..r.members.len()).collect(),
                    ("per_vertex", Some(El::W(w))) => {
                        let n = w.nodes.len();
                        if n > 2 {
                            let mut v: Vec<usize> = (1..n - 1).collect();
                            if w.nodes.first() == w.nodes.last() {
                                v.push(n - 1);
                            }
                            v
                        } else {
                            vec![]
                        }
                    }
                    _ => vec![],
                };
                let mut parts = Vec::with_capacity(positions.len());
                let Some(arg) = args.first() else { return s(String::new()) };
                for p in positions {
                    parts.push(self.s(arg, Cx { pos: Some(p), ..cx })?);
                }
                s(parts.join(";"))
            }
            "pos" => s(cx.pos.map(|p| (p + 1).to_string()).unwrap_or_default()),
            "mtype" => s(match (el, cx.pos) {
                (Some(El::W(_)), Some(_)) => "node".into(),
                (Some(El::R(r)), Some(p)) => r.members.get(p).map(|m| m.mtype.name().to_string()).unwrap_or_default(),
                _ => String::new(),
            }),
            "ref" => s(match (el, cx.pos) {
                (Some(El::W(w)), Some(p)) => w.nodes.get(p).map(|n| n.to_string()).unwrap_or_default(),
                (Some(El::R(r)), Some(p)) => r.members.get(p).map(|m| m.id.to_string()).unwrap_or_default(),
                _ => String::new(),
            }),
            "role" => s(match (el, cx.pos) {
                (Some(El::R(r)), Some(p)) => r.members.get(p).map(|m| m.role.to_string()).unwrap_or_default(),
                _ => String::new(),
            }),
            "angle" => s(match (el, cx.pos) {
                (Some(e @ El::W(w)), Some(p)) => self.angle(e, w, p)?,
                _ => String::new(),
            }),
            "number" => s({
                let v = arg_s(0)?;
                match int(&v) {
                    Some(i) => i_str(i),
                    None => match pdbl(&v) {
                        Some(d) => g14(d),
                        None => "NaN".into(),
                    },
                }
            }),
            "is_number" => s({
                let v = arg_s(0)?;
                if int(&v).is_some() || pdbl(&v).is_some() { "1" } else { "0" }.into()
            }),
            "suffix" => s(num::suffix(&arg_s(0)?)),
            "abs" => s({
                let v = arg_s(0)?;
                match int(&v) {
                    Some(i) => i_str(if i == i64::MIN { i } else { i.abs() }),
                    None => match pdbl(&v) {
                        Some(d) => g14(d.abs()),
                        None => "NaN".into(),
                    },
                }
            }),
            "date" => s(date(&arg_s(0)?).map(g14).unwrap_or_else(|| "NaD".into())),
            "is_date" => s(if date(&arg_s(0)?).is_some() { "1" } else { "0" }.into()),
            "lrs_in" => s(lrs_in(&arg_s(0)?, &arg_s(1)?)),
            "lrs_isect" => s(lrs_isect(&arg_s(0)?, &arg_s(1)?)),
            "lrs_union" => s(lrs_union(&arg_s(0)?, &arg_s(1)?)),
            "lrs_min" | "lrs_max" => {
                let l = arg_s(0)?;
                s(if l.is_empty() { String::new() } else { min_max(l.split(';').map(trim_buggy), name == "lrs_max", true) })
            }
            "pt" => {
                let la = arg_s(0)?;
                let lo = arg_s(1)?;
                Ok(Some(V::G(match (dbl(&la), dbl(&lo)) {
                    (Some(a), Some(o)) if (-90.0..=90.0).contains(&a) && (-180.0..=180.0).contains(&o) => DGeom::Point((a, o)),
                    _ => DGeom::Null,
                })))
            }
            "lstr" | "poly" | "center" | "trace" | "hull" => {
                let mut gs = Vec::new();
                for a in args {
                    if let Some(V::G(g)) = self.eval(a, cx)? {
                        gs.push(g);
                    }
                }
                let g = match name {
                    "lstr" => DGeom::LineString(gs.iter().filter_map(|g| g.center()).collect()),
                    "poly" => geomfn::make_polygon(&gs),
                    "center" => match gs.first() {
                        None => return Ok(None),
                        Some(g) => g.center().map(DGeom::Point).unwrap_or(DGeom::Null),
                    },
                    "trace" => match gs.first() {
                        None => return Ok(None),
                        Some(g) => geomfn::trace(g),
                    },
                    _ => match gs.first() {
                        None => return Ok(None),
                        Some(g) => geomfn::hull(g),
                    },
                };
                // Geometry algorithms stop early when the query runs out of time.
                self.eng.check()?;
                Ok(Some(V::G(g)))
            }
            _ => Err(self.eng.abort(format!("Function \"{name}\" not known"))),
        }
    }

    fn angle(&self, e: El, w: &Way, p: usize) -> Result<String> {
        let n = w.nodes.len();
        let closed = n > 0 && w.nodes.first() == w.nodes.last();
        if p == 0 || !(p + 1 < n || closed) {
            return Ok(String::new());
        }
        let DGeom::LineString(line) = self.geom(e)? else { return Ok(String::new()) };
        let c: Vec<crate::geo::V3> = line.iter().map(|(la, lo)| cartesian(*la, *lo)).collect();
        let prev = c[p - 1];
        let mid = c[p];
        let next = if p + 1 < c.len() { c[p + 1] } else { c[1] };
        let inn = mid.cross(&prev);
        let out = next.cross(&mid);
        let ni = inn.dot(&inn).sqrt();
        let no = out.dot(&out).sqrt();
        if ni < 1e-10 || no < 1e-10 {
            return Ok("NaN".into());
        }
        let prod = inn.dot(&out) / ni / no;
        let diff = crate::geo::V3(mid.0 - prev.0, mid.1 - prev.1, mid.2 - prev.2);
        let cw = diff.dot(&out);
        if prod.abs() > 1.0 {
            return Ok("0.000".into());
        }
        let deg = (prod.acos() / (std::f64::consts::PI / 2.0) * 90.0).copysign(cw);
        Ok(fix(deg, 3))
    }
}

/// Binary operators (docs/spec/evaluators.md §4).
pub fn binary(op: BinOp, l: &str, r: &str) -> String {
    let b = |x: bool| if x { "1".to_string() } else { "0".to_string() };
    match op {
        BinOp::And => b(truthy(l) && truthy(r)),
        BinOp::Or => b(truthy(l) || truthy(r)),
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let ord = match (int(l), int(r)) {
                (Some(a), Some(c)) => Some(a.cmp(&c)),
                _ => match (dbl(l), dbl(r)) {
                    (Some(a), Some(c)) => a.partial_cmp(&c),
                    _ => Some(l.as_bytes().cmp(r.as_bytes())),
                },
            };
            use std::cmp::Ordering::*;
            b(match (op, ord) {
                (_, None) => op == BinOp::Ne,
                (BinOp::Eq, Some(o)) => o == Equal,
                (BinOp::Ne, Some(o)) => o != Equal,
                (BinOp::Lt, Some(o)) => o == Less,
                (BinOp::Le, Some(o)) => o != Greater,
                (BinOp::Gt, Some(o)) => o == Greater,
                (BinOp::Ge, Some(o)) => o != Less,
                _ => false,
            })
        }
        BinOp::Add | BinOp::Sub | BinOp::Mul => match (int(l), int(r)) {
            (Some(a), Some(c)) => i_str(match op {
                BinOp::Add => a.wrapping_add(c),
                BinOp::Sub => a.wrapping_sub(c),
                _ => a.wrapping_mul(c),
            }),
            _ => match (dbl(l), dbl(r)) {
                (Some(a), Some(c)) => g14(match op {
                    BinOp::Add => a + c,
                    BinOp::Sub => a - c,
                    _ => a * c,
                }),
                _ if op == BinOp::Add => format!("{l}{r}"),
                _ => "NaN".into(),
            },
        },
        BinOp::Div => match (dbl(l), dbl(r)) {
            (Some(a), Some(c)) => g14(a / c),
            _ => "NaN".into(),
        },
    }
}

/// `min`/`max` state machine (docs/spec/evaluators.md §9.2). `start_int`
/// skips the initial "void" state as `lrs_min`/`lrs_max` do.
fn min_max<'a>(vals: impl Iterator<Item = &'a str>, max: bool, start_int: bool) -> String {
    let mut ty = if start_int { 1 } else { 0 }; // 0 void, 1 int, 2 double, 3 string
    let mut l: i64 = if max { i64::MIN } else { i64::MAX };
    let mut d: f64 = if max { -f64::MAX } else { f64::MAX };
    let mut s = String::new();
    for v in vals {
        if ty == 0 {
            ty = 1;
        }
        if ty <= 1 {
            match int(v) {
                Some(i) => l = if max { l.max(i) } else { l.min(i) },
                None => ty = 2,
            }
        }
        if ty <= 2 {
            match dbl(v) {
                Some(x) => {
                    if max {
                        if d < x {
                            d = x
                        }
                    } else if x < d {
                        d = x
                    }
                }
                None => ty = 3,
            }
        }
        if !v.is_empty() && (s.is_empty() || (max && v.as_bytes() > s.as_bytes()) || (!max && v.as_bytes() < s.as_bytes())) {
            s = v.to_string();
        }
    }
    match ty {
        0 => String::new(),
        1 => i_str(l),
        2 => g14(d),
        _ => s,
    }
}

/// The C++ `trim` used by list functions (keeps trailing spaces after leading ones).
fn trim_buggy(x: &str) -> &str {
    let b = x.as_bytes();
    let is_sp = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
    let Some(from) = b.iter().position(|c| !is_sp(*c)) else { return " " };
    let to = b.iter().rposition(|c| !is_sp(*c)).unwrap();
    let end = (from + to + 1).min(b.len());
    &x[from..end]
}

fn lrs_members(l: &str) -> Vec<String> {
    if l.is_empty() {
        return Vec::new();
    }
    let mut v: Vec<String> = l.split(';').map(|p| trim_buggy(p).to_string()).collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn lrs_in(x: &str, l: &str) -> String {
    if l.is_empty() {
        return "0".into();
    }
    let t = trim_buggy(x);
    if l.split(';').any(|p| trim_buggy(p) == t) { "1" } else { "0" }.into()
}

fn lrs_isect(a: &str, b: &str) -> String {
    let bm = lrs_members(b);
    lrs_members(a).into_iter().filter(|x| bm.binary_search(x).is_ok()).collect::<Vec<_>>().join(";")
}

fn lrs_union(a: &str, b: &str) -> String {
    let mut v = lrs_members(a);
    v.extend(lrs_members(b));
    v.sort_unstable();
    v.dedup();
    v.join(";")
}

fn count_members(name: &str, args: &[Expr], el: Option<El>) -> usize {
    let lit = |i: usize| match args.get(i) {
        Some(Expr::Lit(s)) => Some(s.as_str()),
        _ => None,
    };
    let by_role = name.ends_with("by_role");
    let role = if by_role { lit(0) } else { None };
    let type_arg = if by_role { lit(1) } else { lit(0) };
    let mtype = type_arg.and_then(|t| match t {
        "nodes" => Some(ElemType::Node),
        "ways" => Some(ElemType::Way),
        "relations" | "rels" => Some(ElemType::Relation),
        _ => None,
    });
    let distinct = name.contains("distinct");
    match el {
        Some(El::W(w)) => {
            if by_role || type_arg.is_some() {
                return 0;
            }
            if distinct {
                let mut v = w.nodes.clone();
                v.sort_unstable();
                v.dedup();
                v.len()
            } else {
                w.nodes.len()
            }
        }
        Some(El::R(r)) => {
            if by_role && role.is_none() {
                return 0;
            }
            let mut ms: Vec<&Member> = r
                .members
                .iter()
                .filter(|m| mtype.is_none_or(|t| m.mtype == t) && role.is_none_or(|ro| &*m.role == ro))
                .collect();
            if !distinct {
                return ms.len();
            }
            ms.sort_by_key(|m| (m.mtype, m.id));
            let mut n = 0;
            for i in 0..ms.len() {
                if i == 0 || (ms[i].mtype, ms[i].id, &ms[i].role) != (ms[i - 1].mtype, ms[i - 1].id, &ms[i - 1].role) {
                    n += 1;
                }
            }
            n
        }
        _ => 0,
    }
}

impl<'s, W: Write> Engine<'s, W> {
    pub fn eval_global(&mut self, e: &Expr) -> Result<String> {
        Evaluator::new(self).s(e, Cx::default())
    }

    pub fn filter_if<T: QElem>(&mut self, cands: Vec<T>, exprs: &[Expr]) -> Result<Vec<T>> {
        let keep = {
            let ev = Evaluator::new(self);
            let els: Vec<El> = cands.iter().map(|c| c.as_el()).collect();
            ev.prefetch(&els, exprs)?;
            let mut keep = Vec::with_capacity(cands.len());
            for el in &els {
                let mut ok = true;
                for e in exprs {
                    if !truthy(&ev.s(e, Cx { el: Some(*el), pos: None, key: None })?) {
                        ok = false;
                        break;
                    }
                }
                keep.push(ok);
            }
            keep
        };
        let mut i = 0;
        let mut cands = cands;
        cands.retain(|_| {
            i += 1;
            keep[i - 1]
        });
        Ok(cands)
    }

    fn build_derived(&self, ev: &Evaluator<'_, 's, W>, name: &str, entries: &[MakeEntry], el: Option<El>, counter: &mut u64) -> Result<Derived> {
        let mut d = Derived { tname: name.to_string(), id: 0, tags: Vec::new(), geom: None };
        let mut id_set = false;
        let declared: Vec<&str> = entries
            .iter()
            .filter_map(|e| match &e.key {
                MakeKey::Tag(k) | MakeKey::Remove(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        let cx = Cx { el, pos: None, key: None };
        for entry in entries {
            let Some(value) = &entry.value else { continue };
            match &entry.key {
                MakeKey::Tag(k) => {
                    if let Some(v) = ev.eval(value, cx)? {
                        d.tags.push((k.clone(), v.into_string()));
                    }
                }
                MakeKey::Remove(_) => {}
                MakeKey::Id => {
                    if let Some(v) = ev.eval(value, cx)? {
                        if let Some(i) = int(&v.into_string()) {
                            d.id = i as u64;
                            id_set = true;
                        }
                    }
                }
                MakeKey::Geom => {
                    if let Some(V::G(g)) = ev.eval(value, cx)? {
                        d.geom = Some(g);
                    }
                }
                MakeKey::Generic { from } => {
                    let keys: Vec<String> = match from {
                        None => el.map(|e| e.keys().into_iter().map(String::from).collect()).unwrap_or_default(),
                        Some(set) => ev.keys_of_set(set).to_vec(),
                    };
                    for k in keys {
                        if declared.contains(&k.as_str()) {
                            continue;
                        }
                        if let Some(v) = ev.eval(value, Cx { key: Some(&k), ..cx })? {
                            d.tags.push((k.clone(), v.into_string()));
                        }
                    }
                }
            }
        }
        if !id_set {
            *counter += 1;
            d.id = *counter;
        }
        Ok(d)
    }

    pub fn make(&mut self, name: &str, entries: &[MakeEntry]) -> Result<Set> {
        let mut counter = self.derived_counter;
        let d = {
            let ev = Evaluator::new(self);
            self.build_derived(&ev, name, entries, None, &mut counter)?
        };
        self.derived_counter = counter;
        Ok(Set { derived: vec![d], ..Default::default() })
    }

    pub fn convert(&mut self, input: &Set, name: &str, entries: &[MakeEntry]) -> Result<Set> {
        let mut counter = self.derived_counter;
        let mut out = Vec::new();
        {
            let ev = Evaluator::new(self);
            let els = ordered(input);
            ev.prefetch(&els, entries.iter().filter_map(|e| e.value.as_ref()))?;
            let mut bytes = 0u64;
            for (i, el) in els.into_iter().enumerate() {
                let d = self.build_derived(&ev, name, entries, Some(el), &mut counter)?;
                bytes += d.approx_bytes();
                out.push(d);
                if i % 1024 == 1023 {
                    self.check_memory(bytes)?;
                    self.check()?;
                }
            }
        }
        self.derived_counter = counter;
        Ok(Set { derived: out, ..Default::default() })
    }

    pub fn for_loop(&mut self, input: &str, var: &str, key: &Expr, body: &[Stmt]) -> Result<()> {
        let src = self.get(input);
        let mut groups: BTreeMap<Vec<u8>, Set> = BTreeMap::new();
        {
            let ev = Evaluator::new(self);
            let els = ordered(&src);
            ev.prefetch(&els, [key])?;
            for el in els {
                let vals: Vec<String> = match ev.eval(key, Cx { el: Some(el), pos: None, key: None })? {
                    Some(V::L(l)) => l,
                    Some(v) => vec![v.into_string()],
                    None => vec![String::new()],
                };
                for v in vals {
                    let g = groups.entry(v.into_bytes()).or_default();
                    match el {
                        El::N(n) => g.nodes.push(n.clone()),
                        El::W(w) => g.ways.push(w.clone()),
                        El::R(r) => g.rels.push(r.clone()),
                        El::A(a) => g.areas.push(a.clone()),
                        El::D(d) => g.derived.push(d.clone()),
                    }
                }
            }
        }
        self.set(var, Set::default());
        self.push_frame();
        for (val, mut set) in groups {
            super::set::sort_dedup(&mut set.nodes);
            super::set::sort_dedup(&mut set.ways);
            super::set::sort_dedup(&mut set.rels);
            super::set::sort_dedup(&mut set.areas);
            self.set(var, set);
            self.set_keys.entry(var.to_string()).or_default().insert("val".into(), String::from_utf8_lossy(&val).into_owned());
            self.exec_block(body)?;
            self.check()?;
        }
        self.pop_frame_leak(Some(var));
        self.set_keys.remove(var);
        Ok(())
    }

    pub fn query_derived(&mut self, plan: &Plan) -> Result<Vec<Derived>> {
        // Derived elements only come from item clauses (`derived.a[...]`).
        if plan.inputs.is_empty() {
            return Ok(Vec::new());
        }
        let mut cands: Vec<Derived> = self.get(&plan.inputs[0]).derived.clone();
        for other in &plan.inputs[1..] {
            let o = self.get(other);
            cands.retain(|d| o.derived.iter().any(|x| x.id == d.id && x.tname == d.tname));
        }
        let tags: Vec<&super::filter::TagFilter> = plan.tags.iter().filter(|t| !matches!(t, super::filter::TagFilter::KeyRegex { .. })).collect();
        cands.retain(|d| {
            let t = Tags::from_pairs(d.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            tags.iter().all(|f| f.matches(&t))
        });
        if !plan.ifs.is_empty() {
            let keep = {
                let ev = Evaluator::new(self);
                let mut keep = Vec::new();
                for d in &cands {
                    let mut ok = true;
                    for e in &plan.ifs {
                        if !truthy(&ev.s(e, Cx { el: Some(El::D(d)), pos: None, key: None })?) {
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
        Ok(cands)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_vectors() {
        let cases = [
            (BinOp::Add, "1", "0 ", "10 "),
            (BinOp::Add, " 1", "10", "11"),
            (BinOp::Add, " 1", "2_", " 12_"),
            (BinOp::Add, "5.5", "3.5", "9"),
            (BinOp::Add, "100000000000000000", "1", "100000000000000001"),
            (BinOp::Mul, "2", "6.5", "13"),
            (BinOp::Mul, "_2", "7", "NaN"),
            (BinOp::Sub, "2", "5", "-3"),
            (BinOp::Div, "8", "9", "0.88888888888889"),
            (BinOp::Eq, "9.5", "9.50", "1"),
            (BinOp::Eq, "nine", "nine ", "0"),
            (BinOp::Eq, "99", "99 ", "0"),
            (BinOp::Lt, "9.5", "10", "1"),
            (BinOp::Lt, "1", "a", "1"),
            (BinOp::Lt, " ", "1", "1"),
            (BinOp::Le, "a", "a.0", "1"),
            (BinOp::Le, "9.0", "9", "1"),
            (BinOp::And, "true", "", "0"),
            (BinOp::And, "false", "false", "1"),
            (BinOp::Or, "", "", "0"),
            (BinOp::Eq, "010", "8", "1"),
            (BinOp::Eq, "010", "10", "0"),
            (BinOp::Eq, "NaN", "NaN", "0"),
            (BinOp::Ne, "NaN", "NaN", "1"),
        ];
        for (op, l, r, want) in cases {
            assert_eq!(binary(op, l, r), want, "{l:?} {op:?} {r:?}");
        }
    }

    #[test]
    fn list_vectors() {
        assert_eq!(trim_buggy(" a "), "a ");
        assert_eq!(trim_buggy("  a  "), "a  ");
        assert_eq!(trim_buggy(" a"), "a");
        assert_eq!(trim_buggy("a "), "a");
        assert_eq!(min_max("  foo".split(';').map(trim_buggy), true, true), "foo");
        assert_eq!(min_max("1e3".split(';').map(trim_buggy), true, true), "1000");
        assert_eq!(min_max("9;10".split(';').map(trim_buggy), true, true), "10");
        assert_eq!(min_max("9;10".split(';').map(trim_buggy), false, true), "9");
        assert_eq!(min_max(".;9;10".split(';').map(trim_buggy), true, true), "9");
        assert_eq!(min_max("a;9;10".split(';').map(trim_buggy), false, true), "10");
        assert_eq!(min_max("9 bis;10 bis".split(';').map(trim_buggy), true, true), "9 bis");
        assert_eq!(lrs_isect("\t", "\n"), " ");
        assert_eq!(lrs_isect("a;;b", "a; "), " ;a");
        assert_eq!(lrs_union("a;b", "a; "), " ;a;b");
        assert_eq!(lrs_in("  ", "\t"), "1");
        assert_eq!(lrs_in("  ", ""), "0");
        assert_eq!(lrs_in("", "a;  ;c"), "1");
        assert_eq!(lrs_isect(" foo", " foo"), "foo");
        assert_eq!(min_max(["10", "9", ""].into_iter(), false, false), "10");
        assert_eq!(min_max(["10", "9"].into_iter(), false, false), "9");
    }
}
