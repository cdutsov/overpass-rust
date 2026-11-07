//! Element sets: the values of Overpass QL set variables.

use crate::model::*;
use crate::output::DGeom;

/// An element produced by `make`, `convert` or `out count`.
#[derive(Clone, Debug, PartialEq)]
pub struct Derived {
    pub tname: String,
    pub id: Id,
    /// Tags in their defined order (not sorted).
    pub tags: Vec<(String, String)>,
    pub geom: Option<DGeom>,
}

impl Derived {
    /// Approximate heap size, for the memory limit.
    pub fn approx_bytes(&self) -> u64 {
        fn points(g: &DGeom) -> usize {
            match g {
                DGeom::Null => 0,
                DGeom::Point(_) => 1,
                DGeom::LineString(v) => v.len(),
                DGeom::PartialWay(v) => v.len(),
                DGeom::Polygon(r) => r.iter().map(Vec::len).sum(),
                DGeom::Collection(c) | DGeom::PartialRel(c) => c.iter().map(points).sum(),
            }
        }
        let tags: usize = self.tags.iter().map(|(k, v)| 48 + k.len() + v.len()).sum();
        (96 + self.tname.len() + tags + self.geom.as_ref().map_or(0, |g| 24 * points(g))) as u64
    }
}

/// A set of elements; every vector is sorted by id and free of duplicates.
/// Derived elements are sorted by id (stable) when a set is stored, as the
/// C++ code sorts them; they may share an id until a union merges them.
#[derive(Clone, Debug, Default)]
pub struct Set {
    pub nodes: Vec<Node>,
    pub ways: Vec<Way>,
    pub rels: Vec<Relation>,
    pub areas: Vec<Area>,
    pub derived: Vec<Derived>,
}

pub trait HasId {
    fn id(&self) -> Id;
}

impl HasId for Node {
    fn id(&self) -> Id {
        self.id
    }
}
impl HasId for Way {
    fn id(&self) -> Id {
        self.id
    }
}
impl HasId for Relation {
    fn id(&self) -> Id {
        self.id
    }
}
impl HasId for Area {
    fn id(&self) -> Id {
        self.id
    }
}

pub fn sort_dedup<T: HasId>(v: &mut Vec<T>) {
    v.sort_by_key(|e| e.id());
    v.dedup_by_key(|e| e.id());
}

/// Merge two id-sorted vectors.
/// C++ `indexed_set_union` for deriveds (compared by id only): the added
/// elements win over equal ids, except when a single element joins a set of
/// more than 64, which keeps the existing one.
pub fn union_derived(acc: &[Derived], add: &[Derived]) -> Vec<Derived> {
    if acc.is_empty() {
        return add.to_vec();
    }
    if add.is_empty() {
        return acc.to_vec();
    }
    if add.len() == 1 && acc.len() > 64 {
        let mut out = acc.to_vec();
        let i = out.partition_point(|d| d.id < add[0].id);
        if out.get(i).is_none_or(|d| d.id != add[0].id) {
            out.insert(i, add[0].clone());
        }
        return out;
    }
    // std::set_union(add, acc): equal runs take the elements of `add` and
    // then the surplus of `acc`.
    let mut out = Vec::with_capacity(acc.len() + add.len());
    let (mut i, mut j) = (0, 0);
    while i < add.len() && j < acc.len() {
        if add[i].id < acc[j].id {
            out.push(add[i].clone());
            i += 1;
        } else if acc[j].id < add[i].id {
            out.push(acc[j].clone());
            j += 1;
        } else {
            out.push(add[i].clone());
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&add[i..]);
    out.extend_from_slice(&acc[j..]);
    out
}

pub fn merge<T: HasId + Clone>(a: &[T], b: &[T]) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].id().cmp(&b[j].id()) {
            std::cmp::Ordering::Less => {
                out.push(a[i].clone());
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j].clone());
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i].clone());
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// Elements of `a` whose id is not in `b`.
pub fn minus<T: HasId + Clone>(a: &[T], b: &[T]) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len());
    let mut j = 0;
    for e in a {
        while j < b.len() && b[j].id() < e.id() {
            j += 1;
        }
        if j < b.len() && b[j].id() == e.id() {
            continue;
        }
        out.push(e.clone());
    }
    out
}

/// Elements of `a` whose id is in `b`.
pub fn intersect<T: HasId + Clone>(a: &[T], b: &[T]) -> Vec<T> {
    let mut out = Vec::new();
    let mut j = 0;
    for e in a {
        while j < b.len() && b[j].id() < e.id() {
            j += 1;
        }
        if j < b.len() && b[j].id() == e.id() {
            out.push(e.clone());
        }
    }
    out
}

pub fn contains_id<T: HasId>(v: &[T], id: Id) -> bool {
    v.binary_search_by_key(&id, |e| e.id()).is_ok()
}

impl Set {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.ways.is_empty() && self.rels.is_empty() && self.areas.is_empty() && self.derived.is_empty()
    }

    pub fn len(&self) -> usize {
        self.nodes.len() + self.ways.len() + self.rels.len() + self.areas.len() + self.derived.len()
    }

    pub fn union(&self, o: &Set) -> Set {
        Set {
            nodes: merge(&self.nodes, &o.nodes),
            ways: merge(&self.ways, &o.ways),
            rels: merge(&self.rels, &o.rels),
            areas: merge(&self.areas, &o.areas),
            derived: union_derived(&self.derived, &o.derived),
        }
    }

    /// Union of several sets in statement order, merged once.
    pub fn union_all(parts: &[std::rc::Rc<Set>]) -> Set {
        fn cat<T: HasId + Clone>(parts: &[std::rc::Rc<Set>], f: impl Fn(&Set) -> &Vec<T>) -> Vec<T> {
            let nonempty: Vec<&Vec<T>> = parts.iter().map(|p| f(p)).filter(|v| !v.is_empty()).collect();
            match nonempty.len() {
                0 => Vec::new(),
                1 => nonempty[0].clone(),
                2 => merge(nonempty[0], nonempty[1]),
                _ => {
                    let mut v: Vec<T> = nonempty.iter().flat_map(|v| v.iter().cloned()).collect();
                    v.sort_by_key(|e| e.id());
                    v.dedup_by_key(|e| e.id());
                    v
                }
            }
        }
        let mut derived: Vec<Derived> = Vec::new();
        for p in parts {
            if !p.derived.is_empty() {
                derived = union_derived(&derived, &p.derived);
            }
        }
        Set { nodes: cat(parts, |s| &s.nodes), ways: cat(parts, |s| &s.ways), rels: cat(parts, |s| &s.rels), areas: cat(parts, |s| &s.areas), derived }
    }

    pub fn minus(&self, o: &Set) -> Set {
        Set {
            nodes: minus(&self.nodes, &o.nodes),
            ways: minus(&self.ways, &o.ways),
            rels: minus(&self.rels, &o.rels),
            areas: minus(&self.areas, &o.areas),
            derived: {
                let ids: rustc_hash::FxHashSet<Id> = o.derived.iter().map(|d| d.id).collect();
                self.derived.iter().filter(|d| !ids.contains(&d.id)).cloned().collect()
            },
        }
    }

    /// Size as counted by the C++ resource manager: per type and index
    /// bucket 64 bytes plus 8 (node), 128 (way, area) or 192 (relation)
    /// per element; derived elements are not counted.
    pub fn counted_size(&self) -> u64 {
        fn buckets<T>(v: &[T], key: impl Fn(&T) -> crate::geo::Cell) -> u64 {
            let mut cells: Vec<crate::geo::Cell> = v.iter().map(key).collect();
            cells.sort_unstable();
            cells.dedup();
            cells.len() as u64
        }
        buckets(&self.nodes, |n| n.cell()) * 64
            + 8 * self.nodes.len() as u64
            + buckets(&self.ways, |w| w.cell()) * 64
            + 128 * self.ways.len() as u64
            + buckets(&self.rels, |r| r.cell()) * 64
            + 192 * self.rels.len() as u64
            + buckets(&self.areas, |a| a.cell()) * 64
            + 128 * self.areas.len() as u64
    }

    /// Rough memory footprint.
    pub fn approx_bytes(&self) -> u64 {
        fn tags_size(t: &Tags) -> u64 {
            t.iter().map(|(k, v)| (k.len() + v.len() + 48) as u64).sum()
        }
        let mut n = 0u64;
        for e in &self.nodes {
            n += 48 + tags_size(&e.tags);
        }
        for e in &self.ways {
            n += 64 + 8 * e.nodes.len() as u64 + tags_size(&e.tags);
        }
        for e in &self.rels {
            n += 64 + e.members.iter().map(|m| 40 + m.role.len() as u64).sum::<u64>() + tags_size(&e.tags);
        }
        for e in &self.areas {
            n += 64 + 4 * e.used.len() as u64 + tags_size(&e.tags);
        }
        for e in &self.derived {
            n += 64 + e.tags.iter().map(|(k, v)| (k.len() + v.len() + 48) as u64).sum::<u64>();
        }
        n
    }
}
