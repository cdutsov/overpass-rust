//! Element sets: the values of Overpass QL set variables.

use crate::model::*;
use crate::output::DGeom;

/// An element produced by `make`, `convert` or `out count`.
#[derive(Clone, Debug, PartialEq)]
pub struct Derived {
    /// Shared with the copies of the element: a copy costs two counters.
    /// A loop that collects one element per round in a set
    /// (`(.acc;.x;)->.acc;`) copies the whole set each round.
    pub tname: std::sync::Arc<str>,
    pub id: Id,
    /// Tags in their defined order (not sorted); shared like the name.
    pub tags: std::sync::Arc<Vec<(String, String)>>,
    pub geom: Option<DGeom>,
}

impl Derived {
    /// Approximate heap size, for the memory limit.
    pub fn approx_bytes(&self) -> u64 {
        let tags: usize = self.tags.iter().map(|(k, v)| 48 + k.len() + v.len()).sum();
        (96 + self.tname.len() + tags + self.geom.as_ref().map_or(0, |g| 24 * g.points())) as u64
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

/// `union_derived` into a vector the caller owns: a single element joins
/// a set of more than 64 in place (a loop that collects one element per
/// round, `(.acc;.x;)->.acc;`, copied the whole set each round).
pub fn union_derived_into(mut acc: Vec<Derived>, add: &[Derived]) -> Vec<Derived> {
    if add.len() == 1 && acc.len() > 64 {
        let i = acc.partition_point(|d| d.id < add[0].id);
        if acc.get(i).is_none_or(|d| d.id != add[0].id) {
            acc.insert(i, add[0].clone());
        }
        return acc;
    }
    union_derived(&acc, add)
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

/// The elements of one kind of a part of a union: owned by the union, or
/// those of a set that something else holds too.
enum Part<'a, T> {
    Own(Vec<T>),
    Ref(&'a [T]),
}

impl<T: Clone> Part<'_, T> {
    fn len(&self) -> usize {
        match self {
            Part::Own(v) => v.len(),
            Part::Ref(v) => v.len(),
        }
    }

    fn into_vec(self) -> Vec<T> {
        match self {
            Part::Own(v) => v,
            Part::Ref(v) => v.to_vec(),
        }
    }
}

/// A `Part` read from its start: owned elements are moved out, the others
/// copied when taken.
enum Src<'a, T> {
    Own(std::vec::IntoIter<T>),
    Ref(std::slice::Iter<'a, T>),
}

impl<T: HasId + Clone> Src<'_, T> {
    fn peek(&self) -> Option<Id> {
        match self {
            Src::Own(it) => it.as_slice().first().map(HasId::id),
            Src::Ref(it) => it.as_slice().first().map(HasId::id),
        }
    }

    fn take(&mut self) -> Option<T> {
        match self {
            Src::Own(it) => it.next(),
            Src::Ref(it) => it.next().cloned(),
        }
    }

    fn skip(&mut self) {
        match self {
            Src::Own(it) => drop(it.next()),
            Src::Ref(it) => drop(it.next()),
        }
    }

    fn rest_into(self, out: &mut Vec<T>) {
        match self {
            Src::Own(it) => out.extend(it),
            Src::Ref(it) => out.extend_from_slice(it.as_slice()),
        }
    }
}

/// `merge` of two parts of a union (the element of `a` wins an equal id).
fn merge_parts<T: HasId + Clone>(a: Part<'_, T>, b: Part<'_, T>) -> Vec<T> {
    if let (Part::Ref(a), Part::Ref(b)) = (&a, &b) {
        return merge(a, b);
    }
    fn src<T>(p: Part<'_, T>) -> Src<'_, T> {
        match p {
            Part::Own(v) => Src::Own(v.into_iter()),
            Part::Ref(v) => Src::Ref(v.iter()),
        }
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut a, mut b) = (src(a), src(b));
    while let (Some(x), Some(y)) = (a.peek(), b.peek()) {
        match x.cmp(&y) {
            std::cmp::Ordering::Less => out.extend(a.take()),
            std::cmp::Ordering::Greater => out.extend(b.take()),
            std::cmp::Ordering::Equal => {
                out.extend(a.take());
                b.skip();
            }
        }
    }
    a.rest_into(&mut out);
    b.rest_into(&mut out);
    out
}

#[cfg(test)]
thread_local! {
    /// Parts of unions whose elements were moved (`Set::union_all`).
    pub static MOVED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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

    /// Union of several sets in statement order, merged once. The elements
    /// of a part that nothing else holds are moved into the result, those
    /// of a stored set are copied: `(._; >;)` and `(node(...); way(...);)`
    /// copy nothing.
    pub fn union_all(parts: Vec<std::rc::Rc<Set>>) -> Set {
        fn cat<'a, T: HasId + Clone + 'static>(parts: &'a mut [Result<Set, std::rc::Rc<Set>>], own: impl Fn(&mut Set) -> &mut Vec<T>, shared: impl Fn(&'a Set) -> &'a Vec<T>) -> Vec<T> {
            let mut srcs: Vec<Part<'a, T>> = Vec::new();
            for p in parts.iter_mut() {
                let src = match p {
                    Ok(set) => Part::Own(std::mem::take(own(set))),
                    Err(rc) => Part::Ref(shared(rc)),
                };
                if src.len() > 0 {
                    srcs.push(src);
                }
            }
            match srcs.len() {
                0 => Vec::new(),
                1 => srcs.pop().unwrap().into_vec(),
                2 => {
                    let b = srcs.pop().unwrap();
                    merge_parts(srcs.pop().unwrap(), b)
                }
                _ => {
                    let mut v: Vec<T> = Vec::with_capacity(srcs.iter().map(Part::len).sum());
                    for src in srcs {
                        match src {
                            Part::Own(x) => v.extend(x),
                            Part::Ref(x) => v.extend_from_slice(x),
                        }
                    }
                    v.sort_by_key(|e| e.id());
                    v.dedup_by_key(|e| e.id());
                    v
                }
            }
        }
        let mut parts: Vec<Result<Set, std::rc::Rc<Set>>> = parts.into_iter().map(std::rc::Rc::try_unwrap).collect();
        #[cfg(test)]
        MOVED.set(MOVED.get() + parts.iter().filter(|p| p.is_ok()).count());
        let mut derived: Vec<Derived> = Vec::new();
        for p in parts.iter_mut() {
            let (own, add): (Option<Vec<Derived>>, &[Derived]) = match p {
                Ok(set) if derived.is_empty() => (Some(std::mem::take(&mut set.derived)), &[]),
                Ok(set) => (None, &set.derived),
                Err(rc) => (None, &rc.derived),
            };
            match own {
                Some(v) => derived = v,
                None if add.is_empty() => {}
                None => derived = union_derived_into(derived, add),
            }
        }
        Set {
            nodes: cat(&mut parts, |s| &mut s.nodes, |s| &s.nodes),
            ways: cat(&mut parts, |s| &mut s.ways, |s| &s.ways),
            rels: cat(&mut parts, |s| &mut s.rels, |s| &s.rels),
            areas: cat(&mut parts, |s| &mut s.areas, |s| &s.areas),
            derived,
        }
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
        self.approx_bytes_elements() + self.derived.iter().map(|e| 64 + e.tags.iter().map(|(k, v)| (k.len() + v.len() + 48) as u64).sum::<u64>()).sum::<u64>()
    }

    /// `approx_bytes` without the derived elements.
    pub fn approx_bytes_elements(&self) -> u64 {
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
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn node(id: Id, v: &str) -> Node {
        Node { id, pos: crate::geo::Pt::from_ll(1.0, 2.0), tags: Tags::from_pairs([("from", v)]) }
    }

    fn set(name: &str, ids: &[Id]) -> Set {
        Set { nodes: ids.iter().map(|i| node(*i, name)).collect(), ..Default::default() }
    }

    /// What `union_all` gave when it copied every element: per kind the
    /// first part that holds an id gives its element.
    fn reference(parts: &[Rc<Set>]) -> Vec<Node> {
        let mut v: Vec<Node> = parts.iter().flat_map(|p| p.nodes.iter().cloned()).collect();
        v.sort_by_key(|n| n.id);
        v.dedup_by_key(|n| n.id);
        v
    }

    fn derived(id: Id, v: &str) -> Derived {
        Derived { tname: "d".into(), id, tags: std::sync::Arc::new(vec![("from".to_string(), v.to_string())]), geom: None }
    }

    /// Derived elements of owned and shared parts: what `union_derived`
    /// gives part by part, also for a single element that joins more than
    /// 64 (kept in place) and for equal ids.
    #[test]
    fn union_of_derived_elements_as_part_by_part() {
        let big: Vec<Id> = (0..100).map(|i| i * 2).collect();
        let lists: [&[Id]; 7] = [&[], &[1, 2, 3], &big, &[5], &[4], &[2, 2, 7], &[199]];
        let mut x = 99u64;
        for round in 0..3000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = 1 + (x >> 60) as usize % 5;
            let mut parts: Vec<Rc<Set>> = Vec::new();
            let mut held: Vec<Rc<Set>> = Vec::new();
            for k in 0..n {
                let pick = (x >> (8 * k)) as usize;
                let s = Rc::new(Set { derived: lists[pick % lists.len()].iter().map(|i| derived(*i, &format!("p{k}"))).collect(), ..Default::default() });
                if pick / 8 % 2 == 0 {
                    held.push(s.clone());
                }
                parts.push(s);
            }
            let mut want: Vec<Derived> = Vec::new();
            for p in &parts {
                if !p.derived.is_empty() {
                    want = union_derived(&want, &p.derived);
                }
            }
            assert_eq!(Set::union_all(parts).derived, want, "{round}");
        }
    }

    /// Unions of owned and shared parts in every mix give the elements of
    /// the copying union, and the shared sets stay as they were.
    #[test]
    fn union_moves_what_it_owns() {
        let lists: [&[Id]; 6] = [&[], &[1, 2, 3], &[2, 3, 4, 9], &[5], &[1, 9, 10, 11], &[3]];
        let mut x = 12345u64;
        for round in 0..3000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = 1 + (x >> 60) as usize % 5;
            let mut parts: Vec<Rc<Set>> = Vec::new();
            let mut held: Vec<Rc<Set>> = Vec::new();
            for k in 0..n {
                let pick = (x >> (8 * k)) as usize;
                let s = Rc::new(set(&format!("p{k}"), lists[pick % lists.len()]));
                if pick / 8 % 2 == 0 {
                    held.push(s.clone());
                }
                parts.push(s);
            }
            let before: Vec<Vec<Node>> = held.iter().map(|s| s.nodes.clone()).collect();
            let want = reference(&parts);
            MOVED.set(0);
            let owned = parts.iter().filter(|p| Rc::strong_count(p) == 1).count();
            let got = Set::union_all(parts);
            assert_eq!(got.nodes, want, "{round}");
            assert_eq!(MOVED.get(), owned);
            assert_eq!(held.iter().map(|s| s.nodes.clone()).collect::<Vec<_>>(), before);
        }
    }
}
