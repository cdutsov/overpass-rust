//! Building DB areas, equivalent to running the C++ rules script
//! `rules/areas.osm3s` with `osm3s_query --rules` (docs/spec/areas.md §3).

use crate::exec::areageom::{self, Blocks, IC};
use crate::exec::fetch::Fetch;
use crate::exec::filter::TagFilter;
use crate::geo::{Rect, cpp_calc_index, cpp_index_order};
use crate::model::*;
use crate::bulk::Appender;
use crate::sorter::Sorter;
use crate::store::codec;
use crate::store::{Db, Kind, Raw, Table};
use anyhow::Result;
use rustc_hash::FxHashSet;

#[derive(Default, Debug)]
pub struct AreaReport {
    pub candidates: usize,
    pub created: usize,
    pub remarks: Vec<String>,
}

/// The relation selections of `rules/areas.osm3s`.
fn rules() -> Vec<Vec<TagFilter>> {
    let has = |k: &str| TagFilter::HasKey(k.into());
    let eq = |k: &str, v: &str| TagFilter::Eq(k.into(), v.into());
    vec![
        vec![eq("type", "multipolygon"), has("name")],
        vec![eq("type", "boundary"), has("name")],
        vec![has("admin_level"), has("name")],
        vec![has("postal_code")],
        vec![has("addr:postcode")],
    ]
}

fn ic(p: crate::geo::Pt) -> IC {
    (p.y, p.ilon())
}

/// Bounding rectangle of the used block tiles.
fn tiles_rect(used: &[u32]) -> Rect {
    let mut r = Rect::EMPTY;
    for &t in used {
        let (s, w) = areageom::tile_sw(t);
        let p0 = crate::geo::Pt::from_ilat_ilon(s, w);
        r.extend(p0);
        r.extend(crate::geo::Pt { y: p0.y.saturating_add(0xfffff), x: p0.x.saturating_add(0xfffff) });
    }
    r
}

/// Candidate relations handled per snapshot. Snapshots are renewed so that a
/// long run does not pin old pages that concurrent updates have freed.
const GROUP: usize = 2000;

/// Compute all areas from the current data and replace the stored ones
/// (areas whose relation no longer qualifies disappear). The computation
/// reads short-lived snapshots; the results go through external sorters
/// and are written in one short transaction at the end.
pub fn make_areas(db: &Db) -> Result<AreaReport> {
    let tmp = crate::bulk::default_tmp(db.path()).with_file_name("areas-tmp");
    make_areas_with(db, &tmp, 256 << 20)
}

pub fn make_areas_with(db: &Db, tmp: &std::path::Path, memory: usize) -> Result<AreaReport> {
    let mut report = AreaReport::default();
    std::fs::create_dir_all(tmp)?;
    let mut data = Sorter::new(tmp, "area-data", memory * 4 / 10);
    let mut ids_s = Sorter::new(tmp, "area-ids", memory / 10);
    let mut tags = Sorter::new(tmp, "area-tags", memory / 10);
    let mut blocks_s = Sorter::new(tmp, "area-blocks", memory * 4 / 10);
    // Candidates in C++ order (index order, then id).
    let (base, order) = {
        let snap = db.snapshot()?;
        let f = Fetch::new(&snap);
        let mut ids: FxHashSet<Id> = FxHashSet::default();
        for rule in rules() {
            let pairs = f.tag_candidates(Kind::Relation, &rule[0], None)?;
            for chunk in pairs.chunks(50_000) {
                for r in f.at::<Relation>(chunk.to_vec())? {
                    if rule.iter().all(|t| t.matches(&r.tags)) {
                        ids.insert(r.id);
                    }
                }
            }
        }
        let mut ids: Vec<Id> = ids.into_iter().collect();
        ids.sort_unstable();
        let mut order: Vec<(u64, Id)> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(50_000) {
            for r in f.by_ids::<Relation>(chunk)? {
                let (a, b) = cpp_index_order(r.idx);
                order.push(((a as u64) << 32 | b as u64, r.id));
            }
        }
        order.sort_unstable();
        (snap.info("osm_base")?.unwrap_or_default(), order)
    };
    report.candidates = order.len();
    for group in order.chunks(GROUP) {
        let snap = db.snapshot()?;
        let f = Fetch::new(&snap);
        let gids: Vec<Id> = group.iter().map(|(_, id)| *id).collect();
        let mut by_id: rustc_hash::FxHashMap<Id, Relation> = f.by_ids::<Relation>(&gids)?.into_iter().map(|r| (r.id, r)).collect();
        for id in gids {
            // Deleted since the candidates were collected.
            let Some(rel) = by_id.remove(&id) else { continue };
            let Some((area, blocks)) = build(&f, rel, &mut report)? else { continue };
            report.created += 1;
            let cell = area.cell();
            let key = crate::store::chunk_key(cell, area.id);
            data.push(&key, &crate::bulk::area_record(&area))?;
            ids_s.push(&area.id.to_be_bytes(), &cell.to_bytes())?;
            for (k, v) in area.tags.iter() {
                tags.push(&crate::bulk::tag_record(k, v, cell, area.id), &[])?;
            }
            for (tile, bs) in &blocks {
                let mut k = tile.to_be_bytes().to_vec();
                k.extend_from_slice(&(area.id as u32).to_be_bytes());
                blocks_s.push(&k, &codec::encode_area_blocks(bs))?;
            }
        }
    }
    let mut txn = db.write()?;
    for t in [Raw::Chunks(Table::Data(Kind::Area)), Raw::Chunks(Table::Meta(Kind::Area)), Raw::Ids(Kind::Area), Raw::Tags(Kind::Area), Raw::AreaBlocks] {
        txn.clear(t)?;
    }
    let mut app = Appender::new(txn);
    crate::bulk::write_ids(&mut app, Kind::Area, &mut ids_s.finish()?)?;
    crate::bulk::write_chunks(&mut app, Table::Data(Kind::Area), &mut data.finish()?, crate::bulk::area_from)?;
    crate::bulk::write_tags(&mut app, Kind::Area, &mut tags.finish()?)?;
    let mut blocks = blocks_s.finish()?;
    while let Some((k, v)) = blocks.next()? {
        app.append(Raw::AreaBlocks, k, v)?;
    }
    app.txn().set_info("area_base", &base)?;
    app.commit()?;
    let _ = std::fs::remove_dir(tmp);
    Ok(report)
}

/// The area of one candidate relation (C++ `make-area` of the rules).
fn build(f: &Fetch, rel: Relation, report: &mut AreaReport) -> Result<Option<(Area, Blocks)>> {
    let way_ids: Vec<Id> = rel.members.iter().filter(|m| m.mtype == ElemType::Way).map(|m| m.id).collect();
    let ways = f.ways_untagged(&way_ids)?;
    // Parity check over all member ways (C++ check_node_parity).
    let mut odd: std::collections::BTreeSet<Id> = std::collections::BTreeSet::new();
    for w in &ways {
        if w.nodes.len() < 2 {
            continue;
        }
        for n in [w.nodes[0], w.nodes[w.nodes.len() - 1]] {
            if !odd.remove(&n) {
                odd.insert(n);
            }
        }
    }
    let id = (rel.id as u32).wrapping_add(3_600_000_000) as Id;
    if let Some(n) = odd.iter().next() {
        report.remarks.push(format!("make-area: Node {n} is contained in an odd number of ways."));
        return Ok(None);
    }
    let nids: Vec<Id> = ways.iter().flat_map(|w| w.nodes.iter().copied()).collect();
    let pos = f.node_positions(&nids)?;
    let mut ways = ways;
    ways.sort_by_key(|w| (cpp_index_order(w.idx), w.id));
    let geoms: Vec<Vec<IC>> = ways
        .iter()
        .filter_map(|w| {
            let g: Option<Vec<IC>> = w.nodes.iter().map(|n| pos.get(n).map(|p| ic(*p))).collect();
            g
        })
        .collect();
    let Some(blocks) = areageom::build_area(&geoms) else { return Ok(None) };
    let used: Vec<u32> = blocks.keys().copied().collect();
    let idx = cpp_calc_index(&used);
    if idx == 0 {
        return Ok(None);
    }
    let bbox = tiles_rect(&used);
    Ok(Some((Area { id, tags: rel.tags, used, idx, bbox }, blocks)))
}
