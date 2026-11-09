//! Applying element changes (import and OSC diffs) to the store.

pub mod areas;
pub mod osm_xml;
pub mod pbf;

use crate::geo::{Cell, Pt, Rect, anchor_cell, level_ranges, MAX_LEVEL};
use crate::model::*;
use crate::store::codec::{self, MetaRec, Pick};
use crate::store::{ChunkKey, Kind, Table, WriteTxn, chunk_key};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};

/// One element version from an input file.
#[derive(Clone, Debug)]
pub enum Elem {
    Node(Node),
    Way(Way),
    Relation(Relation),
}

impl Elem {
    pub fn id(&self) -> Id {
        match self {
            Elem::Node(n) => n.id,
            Elem::Way(w) => w.id,
            Elem::Relation(r) => r.id,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Change {
    pub elem: Elem,
    pub delete: bool,
    pub meta: Option<Meta>,
    pub user: Option<String>,
}

/// Records stored in chunk tables, ordered by (cell, id).
pub trait Chunked: Clone + Send + Sync {
    fn key(&self) -> (Cell, Id);
    fn encode(items: &[&Self]) -> Vec<u8>;
    fn decode(value: &[u8], pick: Pick) -> Result<Vec<Self>>;
}

/// Element types that live in chunks.
pub trait Stored: Chunked + crate::exec::fetch::MemSize {
    const KIND: Kind;
    fn id(&self) -> Id;
    fn cell(&self) -> Cell;
    fn tags(&self) -> &Tags;
    /// `decode` of the elements that pass `test`; the others need not be
    /// decoded whole (see `codec::TagTest`).
    fn decode_where(value: &[u8], pick: Pick, test: codec::TagTest) -> Result<Vec<Self>>;
}

macro_rules! stored {
    ($t:ty, $kind:expr, $enc:expr, $dec:expr, $dec_where:expr) => {
        impl Chunked for $t {
            fn key(&self) -> (Cell, Id) {
                (<$t>::cell(self), self.id)
            }
            fn encode(items: &[&Self]) -> Vec<u8> {
                $enc(items)
            }
            fn decode(value: &[u8], pick: Pick) -> Result<Vec<Self>> {
                $dec(value, pick)
            }
        }
        impl Stored for $t {
            const KIND: Kind = $kind;
            fn id(&self) -> Id {
                self.id
            }
            fn cell(&self) -> Cell {
                <$t>::cell(self)
            }
            fn tags(&self) -> &Tags {
                &self.tags
            }
            fn decode_where(value: &[u8], pick: Pick, test: codec::TagTest) -> Result<Vec<Self>> {
                $dec_where(value, pick, test)
            }
        }
    };
}

stored!(Node, Kind::Node, codec::encode_nodes, |v, p| codec::decode_nodes(v, p, true), codec::decode_nodes_where);
stored!(Way, Kind::Way, codec::encode_ways, |v, p| codec::decode_ways(v, p, true), codec::decode_ways_where);
stored!(Relation, Kind::Relation, codec::encode_relations, |v, p| codec::decode_relations(v, p, true), codec::decode_relations_where);
stored!(Area, Kind::Area, codec::encode_areas, codec::decode_areas, codec::decode_areas_where);

impl Chunked for MetaRec {
    fn key(&self) -> (Cell, Id) {
        (self.cell, self.id)
    }
    fn encode(items: &[&Self]) -> Vec<u8> {
        codec::encode_meta(items)
    }
    fn decode(value: &[u8], pick: Pick) -> Result<Vec<Self>> {
        codec::decode_meta(value, pick)
    }
}

/// A chunk loaded for editing.
struct Loaded<T> {
    /// Its key in the table; `None` for the range before the first chunk.
    stored: Option<ChunkKey>,
    /// Key of the following chunk in the table.
    next: Option<ChunkKey>,
    items: BTreeMap<(Cell, Id), T>,
    dirty: bool,
}

/// Loads chunks of a table on demand, edits their records, and writes the
/// dirty ones back, repacking runs of adjacent dirty chunks.
pub struct ChunkEditor<T: Chunked> {
    table: Table,
    /// By chunk key (all zero for the range before the first chunk). Chunk
    /// bounds are those of the table, which is not written before `flush`.
    chunks: BTreeMap<ChunkKey, Loaded<T>>,
}

impl<T: Chunked> ChunkEditor<T> {
    pub fn new(table: Table) -> Self {
        ChunkEditor { table, chunks: BTreeMap::new() }
    }

    /// Load the chunk holding position `pos`; returns its map key.
    fn locate(&mut self, txn: &WriteTxn, pos: (Cell, Id)) -> Result<ChunkKey> {
        let k = chunk_key(pos.0, pos.1);
        if let Some((lk, l)) = self.chunks.range(..=k).next_back() {
            if l.next.is_none_or(|n| k < n) {
                return Ok(*lk);
            }
        }
        let span = txn.chunk_span(self.table, &k)?;
        let items = match &span.value {
            Some(v) => T::decode(v, Pick::All)?.into_iter().map(|e| (e.key(), e)).collect(),
            None => BTreeMap::new(),
        };
        let lk = span.key.unwrap_or([0; 13]);
        self.chunks.insert(lk, Loaded { stored: span.key, next: span.next, items, dirty: false });
        Ok(lk)
    }

    pub fn get(&mut self, txn: &WriteTxn, cell: Cell, id: Id) -> Result<Option<T>> {
        let lk = self.locate(txn, (cell, id))?;
        Ok(self.chunks[&lk].items.get(&(cell, id)).cloned())
    }

    pub fn remove(&mut self, txn: &WriteTxn, cell: Cell, id: Id) -> Result<Option<T>> {
        let lk = self.locate(txn, (cell, id))?;
        let l = self.chunks.get_mut(&lk).unwrap();
        let old = l.items.remove(&(cell, id));
        l.dirty |= old.is_some();
        Ok(old)
    }

    pub fn insert(&mut self, txn: &WriteTxn, e: T) -> Result<()> {
        let key = e.key();
        let lk = self.locate(txn, key)?;
        let l = self.chunks.get_mut(&lk).unwrap();
        l.items.insert(key, e);
        l.dirty = true;
        Ok(())
    }

    pub fn flush(&mut self, txn: &mut WriteTxn) -> Result<()> {
        let mut group: Vec<Loaded<T>> = Vec::new();
        for (_, l) in std::mem::take(&mut self.chunks) {
            let adjacent = group.last().is_some_and(|g| g.next.is_some() && g.next == l.stored);
            if !l.dirty || !adjacent {
                self.write_group(txn, std::mem::take(&mut group))?;
            }
            if l.dirty {
                group.push(l);
            }
        }
        self.write_group(txn, group)
    }

    /// Write adjacent dirty chunks as chunks packed close to `CHUNK_MAX`.
    fn write_group(&self, txn: &mut WriteTxn, group: Vec<Loaded<T>>) -> Result<()> {
        let Some(first) = group.first() else { return Ok(()) };
        let first = first.stored;
        let stored: Vec<ChunkKey> = group.iter().filter_map(|l| l.stored).collect();
        let items: Vec<T> = group.into_iter().flat_map(|l| l.items.into_values()).collect();
        let refs: Vec<&T> = items.iter().collect();
        let mut written = Vec::new();
        for (i, (r, v)) in codec::pack(refs.len(), |r| T::encode(&refs[r])).into_iter().enumerate() {
            let key = match (first, i) {
                (Some(k), 0) => k,
                _ => {
                    let (c, id) = items[r.start].key();
                    chunk_key(c, id)
                }
            };
            txn.put_chunk(self.table, &key, &v)?;
            written.push(key);
        }
        for k in stored {
            if !written.contains(&k) {
                txn.delete_chunk(self.table, &k)?;
            }
        }
        Ok(())
    }
}

/// Edits elements of one kind with their meta data, keeping the id and tag
/// indexes up to date.
pub struct ElemEditor<T: Stored> {
    data: ChunkEditor<T>,
    meta: ChunkEditor<MetaRec>,
}

impl<T: Stored> Default for ElemEditor<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Stored> ElemEditor<T> {
    pub fn new() -> Self {
        ElemEditor { data: ChunkEditor::new(Table::Data(T::KIND)), meta: ChunkEditor::new(Table::Meta(T::KIND)) }
    }

    pub fn get(&mut self, txn: &WriteTxn, cell: Cell, id: Id) -> Result<Option<T>> {
        self.data.get(txn, cell, id)
    }

    pub fn remove(&mut self, txn: &mut WriteTxn, cell: Cell, id: Id) -> Result<Option<(T, Option<Meta>)>> {
        let Some(e) = self.data.remove(txn, cell, id)? else { return Ok(None) };
        for (k, v) in e.tags() {
            txn.tag_remove(T::KIND, k, v, cell, id);
        }
        txn.set_cell(T::KIND, id, None);
        let m = self.meta.remove(txn, cell, id)?.map(|r| r.meta);
        Ok(Some((e, m)))
    }

    pub fn insert(&mut self, txn: &mut WriteTxn, e: T, meta: Option<Meta>) -> Result<()> {
        let cell = e.cell();
        let id = e.id();
        for (k, v) in e.tags() {
            txn.tag_add(T::KIND, k, v, cell, id);
        }
        txn.set_cell(T::KIND, id, Some(cell));
        self.data.insert(txn, e)?;
        if let Some(meta) = meta {
            self.meta.insert(txn, MetaRec { cell, id, meta })?;
        }
        Ok(())
    }

    pub fn flush(&mut self, txn: &mut WriteTxn) -> Result<()> {
        self.data.flush(txn)?;
        self.meta.flush(txn)
    }
}

/// Summary of an applied batch.
#[derive(Default, Debug)]
pub struct Stats {
    pub nodes: usize,
    pub ways: usize,
    pub relations: usize,
    pub cascaded_ways: usize,
    pub cascaded_relations: usize,
}

/// Look up node positions inside a write transaction.
fn node_positions(txn: &WriteTxn, ids: &[Id]) -> Result<HashMap<Id, Pt>> {
    let mut out = HashMap::with_capacity(ids.len());
    for (v, want) in txn.chunks_with_ids(Kind::Node, ids)? {
        for n in codec::decode_nodes(v, Pick::Ids(&want), false)? {
            out.insert(n.id, n.pos);
        }
    }
    Ok(out)
}

/// Bounding box, C++ index and missing-node flag of a way from its existing nodes.
/// Bounding box, C++ index and missing flag of a way. `pos` is called once
/// per node, in order (the bulk loader hands out positions it looked up before).
pub(crate) fn way_geometry(way: &Way, mut pos: impl FnMut(Id) -> Option<Pt>) -> (Rect, u32, bool) {
    let mut r = Rect::EMPTY;
    let mut tiles = Vec::with_capacity(way.nodes.len());
    let mut missing = false;
    for nd in &way.nodes {
        match pos(*nd) {
            Some(p) => {
                r.extend(p);
                tiles.push(crate::geo::tile_of(p));
            }
            None => missing = true,
        }
    }
    (r, crate::geo::cpp_calc_index(&tiles), missing)
}

/// Look up way bounding boxes, C++ indexes and completeness inside a write transaction.
fn way_bboxes(txn: &WriteTxn, ids: &[Id]) -> Result<HashMap<Id, (Rect, u32, bool)>> {
    let mut out = HashMap::new();
    for (v, want) in txn.chunks_with_ids(Kind::Way, ids)? {
        for w in codec::decode_ways(v, Pick::Ids(&want), false)? {
            out.insert(w.id, (w.bbox, w.idx, !w.missing));
        }
    }
    Ok(out)
}

/// Bounding box, C++ index and spatial flag of a relation from its node and
/// way members (way lookups give bounding box, C++ index and completeness).
/// `nodes` and `ways` are called once per node or way member, in member order.
pub(crate) fn relation_geometry(rel: &Relation, mut nodes: impl FnMut(Id) -> Option<Pt>, mut ways: impl FnMut(Id) -> Option<(Rect, u32, bool)>) -> (Rect, u32, bool) {
    let mut r = Rect::EMPTY;
    let mut idxs = Vec::new();
    let mut spatial = false;
    for m in &rel.members {
        match m.mtype {
            ElemType::Node => {
                if let Some(p) = nodes(m.id) {
                    r.extend(p);
                    idxs.push(crate::geo::tile_of(p));
                    spatial = true;
                }
            }
            ElemType::Way => {
                if let Some((b, i, complete)) = ways(m.id) {
                    r.union(&b);
                    idxs.push(i);
                    spatial |= complete && !b.is_empty();
                }
            }
            _ => {}
        }
    }
    idxs.sort_unstable();
    idxs.dedup();
    (r, crate::geo::cpp_calc_index(&idxs), spatial)
}

/// Apply a batch of changes in one transaction. Changes may mix types; they
/// are applied nodes first, then ways, then relations.
/// Reject coordinates outside the WGS84 range (or NaN).
pub fn check_coordinates(id: Id, lat: f64, lon: f64) -> Result<()> {
    if !((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)) {
        anyhow::bail!("node {id}: invalid coordinates {lat},{lon}");
    }
    Ok(())
}

pub fn checked_pos(id: Id, lat: f64, lon: f64) -> Result<crate::geo::Pt> {
    check_coordinates(id, lat, lon)?;
    Ok(crate::geo::Pt::from_ll(lat, lon))
}

pub fn apply(txn: &mut WriteTxn, changes: Vec<Change>) -> Result<Stats> {
    let mut stats = Stats::default();
    let mut nodes = Vec::new();
    let mut ways = Vec::new();
    let mut rels = Vec::new();
    for c in changes {
        // Like the C++ updater, every element with metadata registers its
        // user, including uid 0 with an empty name.
        if let Some(m) = &c.meta {
            txn.set_user(m.uid, c.user.as_deref().unwrap_or(""))?;
        }
        match &c.elem {
            Elem::Node(_) => nodes.push(c),
            Elem::Way(_) => ways.push(c),
            Elem::Relation(_) => rels.push(c),
        }
    }
    // Later versions of the same element win.
    fn last_wins(v: Vec<Change>) -> Vec<Change> {
        let mut m: BTreeMap<Id, Change> = BTreeMap::new();
        for c in v {
            m.insert(c.elem.id(), c);
        }
        m.into_values().collect()
    }
    let nodes = last_wins(nodes);
    let ways = last_wins(ways);
    let rels = last_wins(rels);
    stats.nodes = nodes.len();
    stats.ways = ways.len();
    stats.relations = rels.len();

    // --- Nodes
    let node_ids: Vec<Id> = nodes.iter().map(|c| c.elem.id()).collect();
    let old_cells = txn.cells_of(Kind::Node, &node_ids)?;
    let mut moved: Vec<(Id, Pt)> = Vec::new(); // old positions of moved or deleted nodes
    let mut created: Vec<Id> = Vec::new();
    let mut node_ed: ElemEditor<Node> = ElemEditor::new();
    for c in nodes {
        let Elem::Node(n) = c.elem else { unreachable!() };
        let mut old_pos = None;
        if let Some(cell) = old_cells.get(&n.id) {
            if let Some((old, _)) = node_ed.remove(txn, *cell, n.id)? {
                old_pos = Some(old.pos);
            }
        }
        match (c.delete, old_pos) {
            (true, Some(p)) => moved.push((n.id, p)),
            (false, Some(p)) if p != n.pos => moved.push((n.id, p)),
            (false, None) => created.push(n.id),
            _ => {}
        }
        if !c.delete {
            node_ed.insert(txn, n, c.meta)?;
        }
    }
    node_ed.flush(txn)?;

    // --- Ways: explicit changes plus ways using moved nodes.
    let mut way_changes: BTreeMap<Id, Change> = ways.into_iter().map(|c| (c.elem.id(), c)).collect();
    let mut touched_ways: BTreeSet<Id> = BTreeSet::new();
    if !moved.is_empty() {
        let moved_ids: HashSet<Id> = moved.iter().map(|(id, _)| *id).collect();
        let mut ranges = Vec::new();
        for (_, p) in &moved {
            for level in 0..=MAX_LEVEL {
                ranges.extend(level_ranges(level, &Rect::point(*p), true));
            }
        }
        let ranges = crate::geo::merge_ranges(ranges);
        for v in txn.chunks_in_ranges(Kind::Way, &ranges)? {
            for w in codec::decode_ways(v, Pick::Cells(&ranges), false)? {
                if w.nodes.iter().any(|n| moved_ids.contains(n)) {
                    touched_ways.insert(w.id);
                }
            }
        }
    }
    // Ways that waited for a node created now.
    for id in &created {
        touched_ways.extend(txn.pending_ways(*id)?);
    }
    touched_ways.retain(|id| !way_changes.contains_key(id));
    stats.cascaded_ways = touched_ways.len();
    let all_way_ids: Vec<Id> = way_changes.keys().copied().chain(touched_ways.iter().copied()).collect();
    let old_way_cells = txn.cells_of(Kind::Way, &all_way_ids)?;
    let mut way_ed: ElemEditor<Way> = ElemEditor::new();
    // Remove old versions, gather the new state.
    let mut new_ways: Vec<(Way, Option<Meta>)> = Vec::new();
    let mut changed_way_ids: Vec<Id> = Vec::new();
    for id in &all_way_ids {
        let old = match old_way_cells.get(id) {
            Some(cell) => way_ed.remove(txn, *cell, *id)?,
            None => None,
        };
        if let Some((ow, _)) = &old {
            if ow.missing {
                for n in &ow.nodes {
                    txn.pending_remove(*n, *id)?;
                }
            }
        }
        match way_changes.remove(id) {
            Some(c) => {
                let Elem::Way(w) = c.elem else { unreachable!() };
                changed_way_ids.push(*id);
                if !c.delete {
                    new_ways.push((w, c.meta));
                }
            }
            None => {
                if let Some((w, m)) = old {
                    changed_way_ids.push(*id);
                    new_ways.push((w, m));
                }
            }
        }
    }
    let nd_refs: Vec<Id> = new_ways.iter().flat_map(|(w, _)| w.nodes.iter().copied()).collect();
    let pos = node_positions(txn, &nd_refs)?;
    for (mut w, m) in new_ways {
        (w.bbox, w.idx, w.missing) = way_geometry(&w, |n| pos.get(&n).copied());
        if w.missing {
            for n in w.nodes.iter().filter(|n| !pos.contains_key(n)) {
                txn.pending_add(*n, w.id)?;
            }
        }
        way_ed.insert(txn, w, m)?;
    }
    way_ed.flush(txn)?;

    // --- Relations: explicit changes plus relations with changed members.
    let mut rel_changes: BTreeMap<Id, Change> = rels.into_iter().map(|c| (c.elem.id(), c)).collect();
    let mut touched_rels: BTreeSet<Id> = BTreeSet::new();
    for id in moved.iter().map(|(id, _)| id).chain(&created) {
        touched_rels.extend(txn.relations_with_member(ElemType::Node, *id)?);
    }
    for id in &changed_way_ids {
        touched_rels.extend(txn.relations_with_member(ElemType::Way, *id)?);
    }
    touched_rels.retain(|id| !rel_changes.contains_key(id));
    stats.cascaded_relations = touched_rels.len();
    let all_rel_ids: Vec<Id> = rel_changes.keys().copied().chain(touched_rels.iter().copied()).collect();
    let old_rel_cells = txn.cells_of(Kind::Relation, &all_rel_ids)?;
    let mut rel_ed: ElemEditor<Relation> = ElemEditor::new();
    let mut new_rels: Vec<(Relation, Option<Meta>)> = Vec::new();
    for id in &all_rel_ids {
        let old = match old_rel_cells.get(id) {
            Some(cell) => rel_ed.remove(txn, *cell, *id)?,
            None => None,
        };
        match rel_changes.remove(id) {
            Some(c) => {
                let Elem::Relation(r) = c.elem else { unreachable!() };
                if let Some((o, _)) = &old {
                    for mem in &o.members {
                        txn.relmember_remove(mem.mtype, mem.id, *id)?;
                    }
                }
                if !c.delete {
                    for mem in &r.members {
                        txn.relmember_add(mem.mtype, mem.id, *id)?;
                    }
                    new_rels.push((r, c.meta));
                }
            }
            None => {
                if let Some((r, m)) = old {
                    new_rels.push((r, m));
                }
            }
        }
    }
    let mut n_ids = Vec::new();
    let mut w_ids = Vec::new();
    for (r, _) in &new_rels {
        for m in &r.members {
            match m.mtype {
                ElemType::Node => n_ids.push(m.id),
                ElemType::Way => w_ids.push(m.id),
                _ => {}
            }
        }
    }
    let npos = node_positions(txn, &n_ids)?;
    let wbox = way_bboxes(txn, &w_ids)?;
    for (mut r, m) in new_rels {
        (r.bbox, r.idx, r.spatial) = relation_geometry(&r, |n| npos.get(&n).copied(), |w| wbox.get(&w).copied());
        rel_ed.insert(txn, r, m)?;
    }
    rel_ed.flush(txn)?;
    Ok(stats)
}

use std::collections::BTreeSet;

/// Replace areas (record and blocks). Old blocks of a replaced area are
/// removed first, like the C++ `set_area`.
pub fn write_areas(txn: &mut WriteTxn, areas: Vec<(Area, crate::exec::areageom::Blocks)>) -> Result<()> {
    let ids: Vec<Id> = areas.iter().map(|(a, _)| a.id).collect();
    let old = txn.cells_of(Kind::Area, &ids)?;
    let mut ed: ElemEditor<Area> = ElemEditor::new();
    for id in &ids {
        if let Some(cell) = old.get(id) {
            if let Some((prev, _)) = ed.remove(txn, *cell, *id)? {
                for t in &prev.used {
                    txn.delete_area_blocks(*t, prev.id as u32)?;
                }
            }
        }
    }
    for (a, blocks) in areas {
        for (tile, bs) in &blocks {
            txn.put_area_blocks(*tile, a.id as u32, bs)?;
        }
        ed.insert(txn, a, None)?;
    }
    ed.flush(txn)?;
    Ok(())
}

pub fn anchor(r: &Rect) -> Cell {
    anchor_cell(r)
}
