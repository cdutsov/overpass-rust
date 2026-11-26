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
    /// Candidates that did not give an area this time (an open ring while a
    /// border is being edited) and keep the area of the last build, as C++
    /// does. Counted in `created` too.
    pub kept: usize,
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
/// (areas whose relation is deleted or no longer qualifies disappear; a
/// relation that qualifies but does not give an area keeps its stored one). The computation
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
            let (area, blocks) = match build(&f, rel, &mut report)? {
                Some(built) => built,
                None => match stored_area(&snap, &f, id)? {
                    Some(old) => {
                        report.kept += 1;
                        old
                    }
                    None => continue,
                },
            };
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
    // One transaction: with a commit in between, queries would read area
    // tables that are empty or half written.
    let mut app = Appender::one_transaction(txn);
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

/// The stored area of relation `rel_id` with its blocks. C++ replaces the area
/// of a relation only when `make-area` succeeds: a relation whose rings are
/// open at the time of the run keeps the area of the run before (2026-11-25:
/// China and India lost theirs for a day while their common border was being
/// edited during the build).
fn stored_area(snap: &crate::store::Snapshot, f: &Fetch, rel_id: Id) -> Result<Option<(Area, Blocks)>> {
    let id = (rel_id as u32).wrapping_add(3_600_000_000) as Id;
    let Some(area) = f.by_ids::<Area>(&[id])?.into_iter().next() else { return Ok(None) };
    let mut blocks = Blocks::new();
    for &tile in &area.used {
        if let Some(b) = snap.area_blocks(tile, id as u32)? {
            blocks.insert(tile, b);
        }
    }
    Ok(Some((area, blocks)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::codec::Pick;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Areas stored, and blocks found for the tiles they list.
    fn stored(db: &Db) -> (usize, usize) {
        let snap = db.snapshot().unwrap();
        let (mut areas, mut blocks) = (0, 0);
        for v in snap.all_chunks(Kind::Area).unwrap() {
            for a in codec::decode_areas(v, Pick::All).unwrap() {
                areas += 1;
                blocks += a.used.iter().filter(|t| snap.area_blocks(**t, a.id as u32).unwrap().is_some()).count();
            }
        }
        (areas, blocks)
    }

    /// `area` with `(pivot)` or a recursion clause gives no areas (it read
    /// every area of the database).
    #[test]
    fn area_statement_with_pivot_or_recursion_gives_no_areas() {
        let dir = std::env::temp_dir().join(format!("areas-pivot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db")).unwrap();
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("compat/data/monaco.osm.gz");
        crate::import::import_file(&db, &file, &mut Default::default()).unwrap();
        make_areas(&db).unwrap();
        let types = |q: &str| -> Vec<String> {
            let p = match crate::exec::run(&db, &format!("[out:csv(::type;false)];{q}"), Vec::new(), &Default::default()) {
                Ok(p) => p,
                Err((_, e)) => panic!("{q}: {e}"),
            };
            String::from_utf8(p.w).unwrap().lines().map(String::from).collect()
        };
        // the closed ways with the tags and the areas of the database
        let areas = types("area[name];out ids;").iter().filter(|t| *t == "area").count();
        assert!(areas > 20, "{areas} areas");
        for q in ["area(pivot);out ids;", "area[name];area(pivot);out ids;", "area[name]->.a;area(pivot.a);out ids;", "rel[type];area(>);out ids;", "node[name];area(<);out ids;", "rel[type];area(<<);out ids;"] {
            let got = types(q);
            assert!(!got.iter().any(|t| t == "area"), "{q}: {} areas", got.len());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn change(db: &Db, xml: &str) {
        let mut t = db.write().unwrap();
        let mut r = crate::import::ImportReport::default();
        crate::import::apply_reader(&mut t, std::io::BufReader::new(xml.as_bytes()), &mut r).unwrap();
        t.commit().unwrap();
    }

    /// A relation whose ring is open at the time of a build keeps the area of
    /// the build before, as on C++; a deleted relation loses it.
    #[test]
    fn open_ring_keeps_the_last_area() {
        let dir = std::env::temp_dir().join(format!("areas-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db")).unwrap();
        let n = |id: u32, lat: f64, lon: f64| format!(r#"<node id="{id}" version="1" lat="{lat}" lon="{lon}"/>"#);
        let base = format!(
            r#"<osmChange version="0.6"><create>{}{}{}{}{}
<way id="11" version="1"><nd ref="1"/><nd ref="2"/><nd ref="3"/></way>
<way id="12" version="1"><nd ref="3"/><nd ref="4"/><nd ref="1"/></way>
<relation id="21" version="1"><member type="way" ref="11" role="outer"/><member type="way" ref="12" role="outer"/>
<tag k="type" v="boundary"/><tag k="boundary" v="administrative"/><tag k="name" v="Square"/></relation>
</create></osmChange>"#,
            n(1, 47.0, 8.0), n(2, 47.0, 8.1), n(3, 47.1, 8.1), n(4, 47.1, 8.0), n(5, 47.2, 8.2)
        );
        change(&db, &base);
        let first = make_areas(&db).unwrap();
        assert_eq!((first.created, first.kept), (1, 0));
        let full = stored(&db);
        assert!(full.0 == 1 && full.1 >= 1, "{full:?}");
        let is_in = |db: &Db| -> String {
            let p = crate::exec::run(db, "[out:csv(::id;false)];is_in(47.05,8.05);out ids;", Vec::new(), &Default::default()).map_err(|(_, e)| e).unwrap();
            String::from_utf8(p.w).unwrap()
        };
        assert_eq!(is_in(&db).trim(), "3600000021");
        // a mapper moves the end of way 12 to node 5: the ring is open
        change(&db, r#"<osmChange version="0.6"><modify><way id="12" version="2"><nd ref="3"/><nd ref="4"/><nd ref="5"/></way></modify></osmChange>"#);
        let second = make_areas(&db).unwrap();
        assert_eq!((second.created, second.kept), (1, 1));
        assert!(second.remarks.iter().any(|r| r.contains("odd number of ways")), "{:?}", second.remarks);
        assert_eq!(stored(&db), full);
        assert_eq!(is_in(&db).trim(), "3600000021");
        // the relation is deleted: its area goes
        change(&db, r#"<osmChange version="0.6"><delete><relation id="21" version="2"/></delete></osmChange>"#);
        let third = make_areas(&db).unwrap();
        assert_eq!((third.created, third.kept), (0, 0));
        assert_eq!(stored(&db), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn commits(db: &Db) -> u64 {
        db.snapshot().unwrap().info("commits").unwrap().unwrap().parse().unwrap()
    }

    /// The areas are replaced in one transaction: a query never sees the
    /// tables empty or half written, whatever their size.
    #[test]
    fn rebuild_is_one_transaction() {
        let dir = std::env::temp_dir().join(format!("areas-one-txn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db")).unwrap();
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("compat/data/monaco.osm.gz");
        crate::import::import_file(&db, &file, &mut Default::default()).unwrap();
        make_areas(&db).unwrap();
        let full = stored(&db);
        assert!(full.0 > 20 && full.1 >= full.0, "{full:?}");
        // as if the area tables were larger than the checkpoint size of the appender (1 GiB)
        crate::bulk::CHECKPOINT_BYTES_TEST.set(Some(4096));
        let before = commits(&db);
        let stop = AtomicBool::new(false);
        let seen = std::thread::scope(|s| {
            let reader = s.spawn(|| {
                let mut seen = std::collections::BTreeSet::new();
                while !stop.load(Ordering::Relaxed) {
                    seen.insert(stored(&db));
                }
                seen
            });
            for _ in 0..3 {
                make_areas(&db).unwrap();
            }
            stop.store(true, Ordering::Relaxed);
            reader.join().unwrap()
        });
        crate::bulk::CHECKPOINT_BYTES_TEST.set(None);
        assert_eq!(stored(&db), full);
        assert_eq!(commits(&db) - before, 3, "commits of 3 rebuilds");
        assert_eq!(seen.into_iter().collect::<Vec<_>>(), vec![full], "what a reader saw during the rebuilds");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
