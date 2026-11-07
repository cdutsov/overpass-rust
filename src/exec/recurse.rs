//! Recursion: `<`, `<<`, `>`, `>>` and the recurse criteria `(w)`, `(r)`,
//! `(bn)`, `(bw)`, `(br)` (docs/spec/query-semantics.md §11).

use super::Engine;
use super::set::{Set, sort_dedup};
use crate::model::*;
use crate::ql::ast::{RecurseFilter, RecurseOp};
use anyhow::Result;
use rustc_hash::FxHashSet as HashSet;
use std::io::Write;

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

impl<'s, W: Write> Engine<'s, W> {
    fn nodes_of_ways(&self, ways: &[Way]) -> Result<Vec<Node>> {
        let mut ids: Vec<Id> = ways.iter().flat_map(|w| w.nodes.iter().copied()).collect();
        ids.sort_unstable();
        ids.dedup();
        self.f.by_ids::<Node>(&ids)
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
                let rels = if op == RecurseOp::DownRel { self.down_closure(&s.rels)? } else { s.rels.clone() };
                let mut ways = self.f.by_ids::<Way>(&member_ids(&rels, ElemType::Way, None))?;
                let mut all_ways = ways.clone();
                all_ways.extend(s.ways.iter().cloned());
                sort_dedup(&mut all_ways);
                let mut nodes = self.nodes_of_ways(&all_ways)?;
                nodes.extend(self.f.by_ids::<Node>(&member_ids(&rels, ElemType::Node, None))?);
                sort_dedup(&mut nodes);
                sort_dedup(&mut ways);
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
                    out.nodes = self.f.by_ids::<Node>(&ids)?;
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
            (RecurseFilter::R, ElemType::Node) => out.nodes = self.f.by_ids::<Node>(&member_ids(&s.rels, ElemType::Node, role))?,
            (RecurseFilter::R, ElemType::Way) => out.ways = self.f.by_ids::<Way>(&member_ids(&s.rels, ElemType::Way, role))?,
            (RecurseFilter::R, ElemType::Relation) => out.rels = self.f.by_ids::<Relation>(&member_ids(&s.rels, ElemType::Relation, role))?,
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
