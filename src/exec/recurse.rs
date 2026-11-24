//! Recursion: `<`, `<<`, `>`, `>>` and the recurse criteria `(w)`, `(r)`,
//! `(bn)`, `(bw)`, `(br)` (docs/spec/query-semantics.md §11).

use super::Engine;
use super::query::QElem;
use super::set::{Set, sort_dedup};
use crate::model::*;
use crate::ql::ast::{RecurseFilter, RecurseOp};
use anyhow::Result;
use rustc_hash::FxHashSet as HashSet;
use std::io::Write;
use std::rc::Rc;

fn member_ids(rels: &[Relation], t: ElemType, role: Option<&str>) -> Vec<Id> {
    let mut v: Vec<Id> = rels
        .iter()
        .flat_map(|r| r.members.iter())
        .filter(|m| m.mtype == t && role.is_none_or(|ro| &*m.role == ro))
        .map(|m| m.id)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Of `ids` (sorted, distinct): the elements that the sets `known` hold,
/// and the ids that none of them holds (sorted).
fn split_known<'a, T: QElem>(known: &'a [Rc<Set>], ids: &[Id]) -> (Vec<&'a T>, Vec<Id>) {
    if ids.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut found: Vec<Option<&'a T>> = vec![None; ids.len()];
    let mut hits = 0;
    for set in known {
        let v = T::from_set(set);
        if v.is_empty() {
            continue;
        }
        if v.len() < ids.len() / 8 {
            // a small set: its elements are looked up in the ids
            for e in v {
                if let Ok(i) = ids.binary_search(&e.id()) {
                    hits += found[i].is_none() as usize;
                    found[i] = Some(e);
                }
            }
            continue;
        }
        if ids.len() < v.len() / 8 {
            // few ids: each is looked up in the set (a walk through a set
            // of a million elements for each member of a union with many
            // members cost more than reading the few elements)
            for (i, id) in ids.iter().enumerate() {
                if let Ok(k) = v.binary_search_by_key(id, |e| e.id()) {
                    hits += found[i].is_none() as usize;
                    found[i] = Some(&v[k]);
                }
            }
            continue;
        }
        // both sorted by id: one pass, from the first id of the other
        let mut j = v.partition_point(|e| e.id() < ids[0]);
        let mut i = ids.partition_point(|id| v.get(j).is_some_and(|e| *id < e.id()));
        while i < ids.len() && j < v.len() {
            match ids[i].cmp(&v[j].id()) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    hits += found[i].is_none() as usize;
                    found[i] = Some(&v[j]);
                    i += 1;
                    j += 1;
                }
            }
        }
    }
    #[cfg(test)]
    super::KNOWN_HELD.set(super::KNOWN_HELD.get() + hits);
    let mut missing = Vec::with_capacity(ids.len() - hits);
    let mut have = Vec::with_capacity(hits);
    for (id, f) in ids.iter().zip(found) {
        match f {
            Some(e) => have.push(e),
            None => missing.push(*id),
        }
    }
    (have, missing)
}

impl<'s, W: Write> Engine<'s, W> {
    /// The elements with the given ids (sorted, distinct). As a member of
    /// a union that already holds some of them (`Engine::known`), only the
    /// others: the union is the same, and they are not read again.
    fn by_ids_new<T: QElem>(&self, ids: &[Id]) -> Result<Vec<T>> {
        if self.known.is_empty() || ids.is_empty() {
            return self.f.by_ids::<T>(ids);
        }
        let (_, missing) = split_known::<T>(&self.known, ids);
        self.f.by_ids::<T>(&missing)
    }

    fn nodes_of_ways<'a>(&self, ways: impl IntoIterator<Item = &'a Way>) -> Result<Vec<Node>> {
        let mut ids: Vec<Id> = ways.into_iter().flat_map(|w| w.nodes.iter().copied()).collect();
        ids.sort_unstable();
        ids.dedup();
        self.by_ids_new::<Node>(&ids)
    }

    fn ways_of_nodes(&self, nodes: &[Node]) -> Result<Vec<Way>> {
        let pts: Vec<(Id, crate::geo::Pt)> = nodes.iter().map(|n| (n.id, n.pos)).collect();
        self.f.ways_with_nodes(&pts)
    }

    /// Relations having a member of type `t` among `ids` (with optional role).
    fn parents(&self, t: ElemType, ids: &[Id], role: Option<&str>) -> Result<Vec<Relation>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut rels = self.f.rels_with_members(t, ids)?;
        if let Some(ro) = role {
            let set: HashSet<Id> = ids.iter().copied().collect();
            rels.retain(|r| r.members.iter().any(|m| m.mtype == t && &*m.role == ro && set.contains(&m.id)));
        }
        Ok(rels)
    }

    fn down_closure(&self, rels: &[Relation]) -> Result<Vec<Relation>> {
        let mut c: Vec<Relation> = rels.to_vec();
        sort_dedup(&mut c);
        loop {
            let ids = member_ids(&c, ElemType::Relation, None);
            let have: HashSet<Id> = c.iter().map(|r| r.id).collect();
            let new: Vec<Id> = ids.into_iter().filter(|i| !have.contains(i)).collect();
            if new.is_empty() {
                return Ok(c);
            }
            let more = self.f.by_ids::<Relation>(&new)?;
            if more.is_empty() {
                return Ok(c);
            }
            c.extend(more);
            sort_dedup(&mut c);
            self.check()?;
        }
    }

    fn up_closure(&self, rels: Vec<Relation>) -> Result<Vec<Relation>> {
        let mut c = rels;
        sort_dedup(&mut c);
        let mut frontier: Vec<Id> = c.iter().map(|r| r.id).collect();
        while !frontier.is_empty() {
            let have: HashSet<Id> = c.iter().map(|r| r.id).collect();
            let parents = self.parents(ElemType::Relation, &frontier, None)?;
            let new: Vec<Relation> = parents.into_iter().filter(|r| !have.contains(&r.id)).collect();
            frontier = new.iter().map(|r| r.id).collect();
            c.extend(new);
            sort_dedup(&mut c);
            self.check()?;
        }
        Ok(c)
    }

    /// Standalone recursion statements.
    pub fn recurse(&mut self, op: RecurseOp, s: &Set) -> Result<Set> {
        let mut out = Set::default();
        match op {
            RecurseOp::Down | RecurseOp::DownRel => {
                let closure = if op == RecurseOp::DownRel { Some(self.down_closure(&s.rels)?) } else { None };
                let rels: &[Relation] = closure.as_deref().unwrap_or(&s.rels);
                // The member ways a running union already holds are not
                // read again (`by_ids_new`); their nodes still count.
                let known = self.known.clone();
                let way_ids = member_ids(rels, ElemType::Way, None);
                let (held, missing) = if known.is_empty() { (Vec::new(), way_ids) } else { split_known::<Way>(&known, &way_ids) };
                let ways = self.f.by_ids::<Way>(&missing)?;
                let mut nodes = self.nodes_of_ways(ways.iter().chain(held).chain(&s.ways))?;
                nodes.extend(self.by_ids_new::<Node>(&member_ids(rels, ElemType::Node, None))?);
                sort_dedup(&mut nodes);
                out.nodes = nodes;
                out.ways = ways;
                if let Some(c) = closure {
                    out.rels = c;
                }
            }
            RecurseOp::Up | RecurseOp::UpRel => {
                let ways = self.ways_of_nodes(&s.nodes)?;
                let mut way_ids: Vec<Id> = ways.iter().map(|w| w.id).chain(s.ways.iter().map(|w| w.id)).collect();
                way_ids.sort_unstable();
                way_ids.dedup();
                let node_ids: Vec<Id> = s.nodes.iter().map(|n| n.id).collect();
                let mut rels = s.rels.clone();
                rels.extend(self.parents(ElemType::Way, &way_ids, None)?);
                rels.extend(self.parents(ElemType::Node, &node_ids, None)?);
                sort_dedup(&mut rels);
                if op == RecurseOp::UpRel {
                    rels = self.up_closure(rels)?;
                }
                out.ways = ways;
                out.rels = rels;
            }
        }
        Ok(out)
    }

    /// `(<)`, `(<<)`, `(>)`, `(>>)` inside a query (quirk Q-R1 differences).
    pub fn recurse_constraint(&mut self, op: RecurseOp, s: &Set) -> Result<Set> {
        let mut out = Set::default();
        match op {
            RecurseOp::Down | RecurseOp::DownRel => {
                let rels = if op == RecurseOp::DownRel { self.down_closure(&s.rels)? } else { s.rels.clone() };
                let ways = self.f.by_ids::<Way>(&member_ids(&rels, ElemType::Way, None))?;
                let mut nodes = self.nodes_of_ways(&ways)?;
                nodes.extend(self.f.by_ids::<Node>(&member_ids(&rels, ElemType::Node, None))?);
                sort_dedup(&mut nodes);
                out.nodes = nodes;
                out.ways = ways;
                if op == RecurseOp::DownRel {
                    out.rels = rels;
                }
            }
            RecurseOp::Up | RecurseOp::UpRel => {
                let ways = self.ways_of_nodes(&s.nodes)?;
                let mut way_ids: Vec<Id> = ways.iter().map(|w| w.id).chain(s.ways.iter().map(|w| w.id)).collect();
                way_ids.sort_unstable();
                way_ids.dedup();
                let node_ids: Vec<Id> = s.nodes.iter().map(|n| n.id).collect();
                let mut rels = self.parents(ElemType::Way, &way_ids, None)?;
                rels.extend(self.parents(ElemType::Node, &node_ids, None)?);
                if op == RecurseOp::UpRel {
                    rels.extend(s.rels.iter().cloned());
                    rels = self.up_closure(rels)?;
                }
                sort_dedup(&mut rels);
                out.ways = ways;
                out.rels = rels;
            }
        }
        Ok(out)
    }

    /// Recurse criteria; `target` is the queried element type.
    pub fn recurse_filter(&mut self, kind: RecurseFilter, s: &Set, role: Option<&str>, positions: Option<&[i64]>, count: Option<(u32, u32)>, target: ElemType) -> Result<Set> {
        // Vertex at a 1-based position, negative positions counting from the end.
        fn at(nodes: &[Id], p: i64) -> Option<Id> {
            let len = nodes.len() as i64;
            let i = if p > 0 { p - 1 } else { len + p };
            (0..len).contains(&i).then(|| nodes[i as usize])
        }
        let mut out = Set::default();
        match (kind, target) {
            (RecurseFilter::W, ElemType::Node) => match positions {
                Some(ps) => {
                    let mut ids: Vec<Id> = s.ways.iter().flat_map(|w| ps.iter().filter_map(|p| at(&w.nodes, *p))).collect();
                    ids.sort_unstable();
                    ids.dedup();
                    out.nodes = self.by_ids_new::<Node>(&ids)?;
                }
                None => out.nodes = self.nodes_of_ways(&s.ways)?,
            },
            (RecurseFilter::WayCnt | RecurseFilter::WayLink, ElemType::Node) => {
                let (lower, upper) = count.unwrap_or((1, 0));
                // C++ way_cnt_nd_ids / way_link_nd_ids: per node, the number
                // of ways using it, or of way segments attached to it.
                let mut cnt: rustc_hash::FxHashMap<Id, u64> = rustc_hash::FxHashMap::default();
                for w in &s.ways {
                    if kind == RecurseFilter::WayCnt {
                        let mut ids = w.nodes.clone();
                        ids.sort_unstable();
                        ids.dedup();
                        for n in ids {
                            *cnt.entry(n).or_default() += 1;
                        }
                    } else {
                        for n in &w.nodes {
                            *cnt.entry(*n).or_default() += 2;
                        }
                        if let (Some(f), Some(l)) = (w.nodes.first(), w.nodes.last()) {
                            *cnt.get_mut(f).unwrap() -= 1;
                            *cnt.get_mut(l).unwrap() -= 1;
                        }
                    }
                }
                let mut ids: Vec<Id> = cnt.into_iter().filter(|(_, c)| (lower as u64..=upper as u64).contains(c)).map(|(n, _)| n).collect();
                ids.sort_unstable();
                out.nodes = self.f.by_ids::<Node>(&ids)?;
            }
            (RecurseFilter::Bn, ElemType::Way) if positions.is_some() => {
                let ps = positions.unwrap();
                let ids: std::collections::HashSet<Id> = s.nodes.iter().map(|n| n.id).collect();
                let mut ways = self.ways_of_nodes(&s.nodes)?;
                ways.retain(|w| ps.iter().any(|p| at(&w.nodes, *p).is_some_and(|n| ids.contains(&n))));
                out.ways = ways;
            }
            (RecurseFilter::R, ElemType::Node) => out.nodes = self.by_ids_new::<Node>(&member_ids(&s.rels, ElemType::Node, role))?,
            (RecurseFilter::R, ElemType::Way) => out.ways = self.by_ids_new::<Way>(&member_ids(&s.rels, ElemType::Way, role))?,
            (RecurseFilter::R, ElemType::Relation) => out.rels = self.by_ids_new::<Relation>(&member_ids(&s.rels, ElemType::Relation, role))?,
            (RecurseFilter::Bn, ElemType::Way) => out.ways = self.ways_of_nodes(&s.nodes)?,
            (RecurseFilter::Bn, ElemType::Relation) => {
                let ids: Vec<Id> = s.nodes.iter().map(|n| n.id).collect();
                out.rels = self.parents(ElemType::Node, &ids, role)?;
            }
            (RecurseFilter::Bw, ElemType::Relation) => {
                let ids: Vec<Id> = s.ways.iter().map(|w| w.id).collect();
                out.rels = self.parents(ElemType::Way, &ids, role)?;
            }
            (RecurseFilter::Br, ElemType::Relation) => {
                let ids: Vec<Id> = s.rels.iter().map(|r| r.id).collect();
                out.rels = self.parents(ElemType::Relation, &ids, role)?;
            }
            _ => {}
        }
        sort_dedup(&mut out.nodes);
        sort_dedup(&mut out.ways);
        sort_dedup(&mut out.rels);
        Ok(out)
    }
}

#[cfg(test)]
mod rv_tests {
    use super::*;

    /// `split_known` gives what a lookup of each id in each set gives, for
    /// sets far larger and far smaller than the ids and of the same size.
    #[test]
    fn split_known_matches_lookups() {
        let node = |id: Id| Node { id, pos: crate::geo::Pt::from_ll(1.0, 2.0), tags: Default::default() };
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for round in 0..2000 {
            let span = [50, 5_000, 500_000][round % 3];
            let sizes = [0usize, 1, 3, 20, 200, 3_000, 40_000];
            let mut ids: Vec<Id> = (0..sizes[rnd(7) as usize]).map(|_| rnd(span)).collect();
            ids.sort_unstable();
            ids.dedup();
            let sets: Vec<Rc<Set>> = (0..1 + rnd(3))
                .map(|_| {
                    let mut v: Vec<Id> = (0..sizes[rnd(7) as usize]).map(|_| rnd(span)).collect();
                    v.sort_unstable();
                    v.dedup();
                    Rc::new(Set { nodes: v.into_iter().map(node).collect(), ..Default::default() })
                })
                .collect();
            let (have, missing) = split_known::<Node>(&sets, &ids);
            let held = |id: &Id| sets.iter().any(|s| s.nodes.binary_search_by_key(id, |n| n.id).is_ok());
            let want_have: Vec<Id> = ids.iter().copied().filter(|i| held(i)).collect();
            let want_missing: Vec<Id> = ids.iter().copied().filter(|i| !held(i)).collect();
            assert_eq!(have.iter().map(|n| n.id).collect::<Vec<_>>(), want_have, "{round}");
            assert_eq!(missing, want_missing, "{round}");
        }
    }
}
