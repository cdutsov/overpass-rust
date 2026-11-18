//! Consistency check of the data the updater derives from the nodes: the
//! bounding box and the missing flag of ways, which also decide which ways
//! a later node move reaches (ways are found by their box), and the
//! pending entries of ways with missing nodes.

use crate::geo::{Rect, level_ranges, merge_ranges, MAX_LEVEL};
use crate::model::*;
use crate::store::codec::{self, Pick};
use crate::store::{Db, Kind};
use anyhow::{Context, Result};
use std::collections::HashMap;

/// A way whose stored data disagrees with its nodes.
#[derive(Debug)]
pub struct Problem {
    pub way: Id,
    pub what: String,
}

/// Check the ways of `sample` random id buckets (all ways for 0). Returns
/// the number of ways checked and the problems.
pub fn check_ways(db: &Db, sample: usize, seed: u64) -> Result<(usize, Vec<Problem>)> {
    let snap = db.snapshot()?;
    let ids = snap.sample_ids(Kind::Way, sample, seed)?;
    let mut problems = Vec::new();
    let mut checked = 0;
    for part in ids.chunks(50_000) {
        let mut ways = Vec::new();
        for (v, want) in snap.chunks_with_ids(Kind::Way, part)? {
            ways.extend(codec::decode_ways(v, Pick::Ids(&want), false).with_context(|| format!("decoding the way chunk of way {}", want[0]))?);
        }
        let mut found: Vec<Id> = ways.iter().map(|w| w.id).collect();
        found.sort_unstable();
        for id in part.iter().filter(|id| found.binary_search(id).is_err()) {
            problems.push(Problem { way: *id, what: "in the id index, not at its cell".into() });
        }
        let nids: Vec<Id> = ways.iter().flat_map(|w| w.nodes.iter().copied()).collect();
        let mut pos: HashMap<Id, crate::geo::Pt> = HashMap::new();
        for (v, want) in snap.chunks_with_ids(Kind::Node, &nids)? {
            for n in codec::decode_nodes(v, Pick::Ids(&want), false).with_context(|| format!("decoding the node chunk of node {}", want[0]))? {
                pos.insert(n.id, n.pos);
            }
        }
        for w in &ways {
            checked += 1;
            let (bbox, _, missing) = super::way_geometry(w, |n| pos.get(&n).copied());
            if bbox != w.bbox {
                problems.push(Problem { way: w.id, what: format!("stored box {:?}, nodes give {:?}", w.bbox, bbox) });
            } else if missing != w.missing {
                problems.push(Problem { way: w.id, what: format!("stored missing={}, nodes give {missing}", w.missing) });
            }
            for n in w.nodes.iter().filter(|n| !pos.contains_key(n)) {
                if !snap.pending_ways(*n)?.contains(&w.id) {
                    problems.push(Problem { way: w.id, what: format!("missing node {n} has no pending entry") });
                }
            }
            // A move of each node must reach the way: its cell lies in the
            // ranges searched for the node position.
            if !bbox.is_empty() && bbox == w.bbox {
                let cell = w.cell();
                for n in &w.nodes {
                    let Some(p) = pos.get(n) else { continue };
                    let mut r = Vec::new();
                    for level in 0..=MAX_LEVEL {
                        r.extend(level_ranges(level, &Rect::point(*p), true));
                    }
                    if !crate::geo::cell_in_ranges(cell, &merge_ranges(r)) {
                        problems.push(Problem { way: w.id, what: format!("cell {cell:?} not reached from node {n}") });
                        break;
                    }
                }
            }
        }
    }
    Ok((checked, problems))
}

/// Compute the given ways again from their nodes, in one transaction.
pub fn repair_ways(db: &Db, ways: &[Id]) -> Result<()> {
    let mut txn = db.write()?;
    super::apply_with(&mut txn, Vec::new(), ways)?;
    txn.commit()
}
