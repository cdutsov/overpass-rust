//! Bulk import into an empty database, for extracts up to the whole planet.
//!
//! The input is read once (nodes, then ways, then relations, as in sorted
//! OSM files). Node positions and way bounding boxes go to lookup files
//! sorted by id, so ways and relations get their geometry without database
//! lookups. Ways and relations are collected in batches whose lookups run in
//! id order, so a node table larger than memory is read forward, not at
//! random. Every table is fed through an external sorter and then written in key order
//! with LMDB appends, so memory stays within the configured budget whatever
//! the input size, and pages are filled completely.

use crate::geo::{Cell, Pt, Rect};
use crate::model::*;
use crate::sorter::{Merged, Sorter};
use crate::store::codec::{self, MetaRec, Reader, Writer};
use crate::store::{Db, IMPORT_RUNNING, Kind, Raw, Table, WriteTxn, chunk_key, tag_prefix};
use crate::update::{Change, Chunked, Elem, relation_geometry, way_geometry};
use anyhow::{Context, Result, bail};
use memmap2::Mmap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Bytes appended before a transaction is committed and a new one begun.
const CHECKPOINT_BYTES: usize = 1 << 30;

/// Raw record bytes collected before chunks are packed.
const WINDOW_BYTES: usize = 4 << 20;

#[derive(Clone, Debug)]
pub struct BulkOptions {
    /// Directory for sort runs and lookup files (needs sparse file support).
    pub tmp: PathBuf,
    /// Memory budget of the sorters in bytes.
    pub memory: usize,
    /// Store meta data (version, timestamp, changeset, user).
    pub meta: bool,
}

// ---------------------------------------------------------------------------
// Database writes in key order, with periodic commits.

pub struct Appender<'d> {
    txn: Option<WriteTxn<'d>>,
    written: usize,
}

impl<'d> Appender<'d> {
    pub fn new(txn: WriteTxn<'d>) -> Self {
        Appender { txn: Some(txn), written: 0 }
    }

    pub fn txn(&mut self) -> &mut WriteTxn<'d> {
        self.txn.as_mut().unwrap()
    }

    pub fn append(&mut self, t: Raw, key: &[u8], value: &[u8]) -> Result<()> {
        self.txn().append(t, key, value)?;
        self.written += key.len() + value.len();
        if self.written >= CHECKPOINT_BYTES {
            self.checkpoint()?;
        }
        Ok(())
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        let t = self.txn.take().unwrap();
        self.txn = Some(t.checkpoint()?);
        self.written = 0;
        Ok(())
    }

    pub fn commit(mut self) -> Result<()> {
        self.txn.take().unwrap().commit()
    }
}

/// Pack sorted records into chunks of `table` (keys strictly increasing).
pub fn write_chunks<T: Chunked>(app: &mut Appender, table: Table, merged: &mut Merged, decode: impl Fn(&[u8], &[u8]) -> Result<T>) -> Result<()> {
    let mut items: Vec<T> = Vec::new();
    let mut raw = 0usize;
    while let Some((k, v)) = merged.next()? {
        items.push(decode(k, v)?);
        raw += k.len() + v.len();
        if raw >= WINDOW_BYTES {
            let before = items.len();
            flush_chunks(app, table, &mut items, false)?;
            raw = raw * items.len() / before.max(1);
        }
    }
    flush_chunks(app, table, &mut items, true)
}

fn flush_chunks<T: Chunked>(app: &mut Appender, table: Table, items: &mut Vec<T>, last: bool) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let refs: Vec<&T> = items.iter().collect();
    let parts = codec::pack(refs.len(), |r| T::encode(&refs[r]));
    // Keep the last, partly filled chunk for the following records.
    let n = if last { parts.len() } else { parts.len() - 1 };
    let mut consumed = 0;
    for (r, v) in &parts[..n] {
        let (c, id) = items[r.start].key();
        app.append(Raw::Chunks(table), &chunk_key(c, id), v)?;
        consumed = r.end;
    }
    drop(refs);
    items.drain(..consumed);
    Ok(())
}

/// Id index from records `id(8)` -> `cell(5)`.
pub fn write_ids(app: &mut Appender, kind: Kind, merged: &mut Merged) -> Result<()> {
    let mut bucket = u64::MAX;
    let mut entries: Vec<(u16, Cell)> = Vec::new();
    let mut last: Option<Id> = None;
    let mask = (1u64 << codec::ID_BUCKET_BITS) - 1;
    while let Some((k, v)) = merged.next()? {
        let id = u64::from_be_bytes(k.try_into()?);
        if last == Some(id) {
            bail!("{} {id} occurs twice; the import needs a file with one version per element", kind.elem_type().name());
        }
        last = Some(id);
        let b = id >> codec::ID_BUCKET_BITS;
        if b != bucket {
            if !entries.is_empty() {
                app.append(Raw::Ids(kind), &bucket.to_be_bytes(), &codec::encode_id_bucket(&entries))?;
                entries.clear();
            }
            bucket = b;
        }
        entries.push(((id & mask) as u16, Cell::from_bytes(v)));
    }
    if !entries.is_empty() {
        app.append(Raw::Ids(kind), &bucket.to_be_bytes(), &codec::encode_id_bucket(&entries))?;
    }
    Ok(())
}

/// Tag index from records `key 0 value 0 cell(5) id(8)`.
pub fn write_tags(app: &mut Appender, kind: Kind, merged: &mut Merged) -> Result<()> {
    struct Part {
        p: Vec<u8>,
        groups: Vec<(Cell, Vec<Id>)>,
        size: usize,
    }
    fn emit(app: &mut Appender, kind: Kind, part: &mut Part) -> Result<()> {
        if let Some((first, _)) = part.groups.first() {
            let mut key = part.p.clone();
            key.extend_from_slice(&first.to_bytes());
            app.append(Raw::Tags(kind), &key, &codec::encode_tag_entry(*first, &part.groups))?;
        }
        part.groups.clear();
        part.size = 0;
        Ok(())
    }
    let mut part = Part { p: Vec::new(), groups: Vec::new(), size: 0 };
    // Group being collected: (cell, ids).
    let mut cur: Option<(Cell, Vec<Id>)> = None;
    let close = |app: &mut Appender, part: &mut Part, cur: &mut Option<(Cell, Vec<Id>)>| -> Result<()> {
        if let Some((cell, mut ids)) = cur.take() {
            ids.dedup();
            let g = codec::tag_group_size(&ids);
            if !part.groups.is_empty() && part.size + g > codec::ENTRY_MAX {
                emit(app, kind, part)?;
            }
            part.size += g;
            part.groups.push((cell, ids));
        }
        Ok(())
    };
    while let Some((k, _)) = merged.next()? {
        if k.len() < 15 {
            bail!("corrupt tag record");
        }
        let (p, rest) = k.split_at(k.len() - 13);
        let cell = Cell::from_bytes(&rest[..5]);
        let id = u64::from_be_bytes(rest[5..].try_into()?);
        if p != part.p.as_slice() {
            close(app, &mut part, &mut cur)?;
            emit(app, kind, &mut part)?;
            part.p = p.to_vec();
        }
        match &mut cur {
            Some((c, ids)) if *c == cell => ids.push(id),
            _ => {
                close(app, &mut part, &mut cur)?;
                cur = Some((cell, vec![id]));
            }
        }
    }
    close(app, &mut part, &mut cur)?;
    emit(app, kind, &mut part)
}

/// Relation member index from records `mtype(1) bucket(8) low(1) rel(8)`.
pub fn write_members(app: &mut Appender, merged: &mut Merged) -> Result<()> {
    let mut key: Vec<u8> = Vec::new();
    let mut entries: Vec<(u8, Id)> = Vec::new();
    while let Some((k, _)) = merged.next()? {
        if k.len() != 18 {
            bail!("corrupt member record");
        }
        if k[..9] != key[..] {
            if !entries.is_empty() {
                app.append(Raw::RelMembers, &key, &codec::encode_member_bucket(&entries))?;
                entries.clear();
            }
            key = k[..9].to_vec();
        }
        let e = (k[9], u64::from_be_bytes(k[10..18].try_into()?));
        if entries.last() != Some(&e) {
            entries.push(e);
        }
    }
    if !entries.is_empty() {
        app.append(Raw::RelMembers, &key, &codec::encode_member_bucket(&entries))?;
    }
    Ok(())
}

/// Keys without values, deduplicated.
fn write_keys(app: &mut Appender, t: Raw, merged: &mut Merged) -> Result<()> {
    let mut last: Vec<u8> = Vec::new();
    let mut first = true;
    while let Some((k, _)) = merged.next()? {
        if first || k != last.as_slice() {
            app.append(t, k, &[])?;
            last = k.to_vec();
            first = false;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Record encodings for the sorters (key: chunk key; value below).

fn put_tags(w: &mut Writer, t: &Tags) {
    w.varint(t.len() as u64);
    for (k, v) in t.iter() {
        w.str(k);
        w.str(v);
    }
}

fn get_tags(r: &mut Reader) -> Result<Tags> {
    let n = r.varint()? as usize;
    let mut t = Tags::with_capacity(n.min(r.remaining()), 0);
    for _ in 0..n {
        let k = r.str_ref()?;
        let v = r.str_ref()?;
        t.push(k, v);
    }
    Ok(t)
}

fn put_rect(w: &mut Writer, r: &Rect) {
    for v in [r.y0, r.x0, r.y1, r.x1] {
        w.buf.extend_from_slice(&v.to_le_bytes());
    }
}

fn get_u32(r: &mut Reader) -> Result<u32> {
    Ok(u32::from_le_bytes(r.take(4)?.try_into()?))
}

fn get_rect(r: &mut Reader) -> Result<Rect> {
    Ok(Rect { y0: get_u32(r)?, x0: get_u32(r)?, y1: get_u32(r)?, x1: get_u32(r)? })
}

fn split_key(k: &[u8]) -> Result<(Cell, Id)> {
    if k.len() != 13 {
        bail!("corrupt chunk record key");
    }
    Ok((Cell::from_bytes(&k[..5]), u64::from_be_bytes(k[5..].try_into()?)))
}

fn node_record(n: &Node) -> Vec<u8> {
    let mut w = Writer::new();
    w.buf.extend_from_slice(&n.pos.y.to_le_bytes());
    w.buf.extend_from_slice(&n.pos.x.to_le_bytes());
    put_tags(&mut w, &n.tags);
    w.buf
}

fn node_from(k: &[u8], v: &[u8]) -> Result<Node> {
    let (_, id) = split_key(k)?;
    let mut r = Reader::new(v);
    let y = get_u32(&mut r)?;
    let x = get_u32(&mut r)?;
    Ok(Node { id, pos: Pt { y, x }, tags: get_tags(&mut r)? })
}

fn way_record(w: &Way) -> Vec<u8> {
    let mut o = Writer::new();
    o.varint(w.idx as u64);
    o.varint(w.missing as u64 | (w.bbox.is_empty() as u64) << 1);
    if !w.bbox.is_empty() {
        put_rect(&mut o, &w.bbox);
    }
    o.varint(w.nodes.len() as u64);
    let mut p = 0i64;
    for &n in &w.nodes {
        o.zigzag((n as i64).wrapping_sub(p));
        p = n as i64;
    }
    put_tags(&mut o, &w.tags);
    o.buf
}

fn way_from(k: &[u8], v: &[u8]) -> Result<Way> {
    let (_, id) = split_key(k)?;
    let mut r = Reader::new(v);
    let idx = r.varint()? as u32;
    let flags = r.varint()?;
    let bbox = if flags & 2 != 0 { Rect::EMPTY } else { get_rect(&mut r)? };
    let n = r.varint()? as usize;
    let mut nodes = Vec::with_capacity(n.min(r.remaining()));
    let mut p = 0i64;
    for _ in 0..n {
        p = p.wrapping_add(r.zigzag()?);
        nodes.push(p as Id);
    }
    Ok(Way { id, nodes, tags: get_tags(&mut r)?, bbox, idx, missing: flags & 1 != 0 })
}

fn rel_record(rel: &Relation) -> Vec<u8> {
    let mut o = Writer::new();
    o.varint(rel.idx as u64);
    o.varint(rel.spatial as u64 | (rel.bbox.is_empty() as u64) << 1);
    if !rel.bbox.is_empty() {
        put_rect(&mut o, &rel.bbox);
    }
    o.varint(rel.members.len() as u64);
    for m in &rel.members {
        o.varint(m.mtype.code() as u64);
        o.varint(m.id);
        o.str(&m.role);
    }
    put_tags(&mut o, &rel.tags);
    o.buf
}

fn rel_from(k: &[u8], v: &[u8]) -> Result<Relation> {
    let (_, id) = split_key(k)?;
    let mut r = Reader::new(v);
    let idx = r.varint()? as u32;
    let flags = r.varint()?;
    let bbox = if flags & 2 != 0 { Rect::EMPTY } else { get_rect(&mut r)? };
    let n = r.varint()? as usize;
    let mut members = Vec::with_capacity(n.min(r.remaining()));
    for _ in 0..n {
        let t = r.varint()?;
        if t > 2 {
            bail!("corrupt relation record");
        }
        let mid = r.varint()?;
        members.push(Member { mtype: ElemType::from_code(t as u8), id: mid, role: r.str_ref()?.into() });
    }
    Ok(Relation { id, members, tags: get_tags(&mut r)?, bbox, idx, spatial: flags & 1 != 0 })
}

pub fn area_record(a: &Area) -> Vec<u8> {
    let mut o = Writer::new();
    o.varint(a.idx as u64);
    o.varint(a.bbox.is_empty() as u64);
    if !a.bbox.is_empty() {
        put_rect(&mut o, &a.bbox);
    }
    o.varint(a.used.len() as u64);
    for &u in &a.used {
        o.varint(u as u64);
    }
    put_tags(&mut o, &a.tags);
    o.buf
}

pub fn area_from(k: &[u8], v: &[u8]) -> Result<Area> {
    let (_, id) = split_key(k)?;
    let mut r = Reader::new(v);
    let idx = r.varint()? as u32;
    let bbox = if r.varint()? != 0 { Rect::EMPTY } else { get_rect(&mut r)? };
    let n = r.varint()? as usize;
    let mut used = Vec::with_capacity(n.min(r.remaining()));
    for _ in 0..n {
        used.push(r.varint()? as u32);
    }
    Ok(Area { id, tags: get_tags(&mut r)?, used, idx, bbox })
}

fn meta_record(m: &Meta) -> Vec<u8> {
    let mut o = Writer::new();
    o.varint(m.version as u64);
    o.zigzag(m.timestamp);
    o.varint(m.changeset);
    o.varint(m.uid as u64);
    o.buf
}

fn meta_from(k: &[u8], v: &[u8]) -> Result<MetaRec> {
    let (cell, id) = split_key(k)?;
    let mut r = Reader::new(v);
    let meta = Meta { version: r.varint()? as u32, timestamp: r.zigzag()?, changeset: r.varint()?, uid: r.varint()? as u32 };
    Ok(MetaRec { cell, id, meta })
}

/// Tag index record key of one tag of an element.
pub fn tag_record(k: &str, v: &str, cell: Cell, id: Id) -> Vec<u8> {
    let mut key = tag_prefix(k, v);
    key.extend_from_slice(&cell.to_bytes());
    key.extend_from_slice(&id.to_be_bytes());
    key
}

// ---------------------------------------------------------------------------
// Fixed-width values by element id.

/// Records per block of the in-memory index.
const ID_BLOCK: u64 = 4096;

/// Threads for batched lookups in the id tables (see `IdTable::get_many`).
const LOOKUP_THREADS: usize = 8;

/// Values of one fixed width by element id, in a file of (id offset, value)
/// records sorted by id, with an in-memory index of blocks (base id, first
/// record). Space follows the number of ids, not the largest id (OSM ids of
/// a region are spread over the whole id range). Ids are expected in
/// ascending order, as in sorted OSM files; otherwise they are sorted on disk
/// when the table is frozen.
struct IdTable {
    path: PathBuf,
    tmp: PathBuf,
    width: usize,
    out: Option<BufWriter<File>>,
    records: u64,
    index: Vec<(u64, u64)>,
    block_len: u64,
    last: Option<u64>,
    unsorted: Option<Sorter>,
    map: Option<Mmap>,
}

impl IdTable {
    fn create(tmp: &Path, name: &str, width: usize) -> Result<IdTable> {
        let path = tmp.join(format!("{name}.bin"));
        let f = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(IdTable { path, tmp: tmp.to_path_buf(), width, out: Some(BufWriter::with_capacity(1 << 20, f)), records: 0, index: Vec::new(), block_len: 0, last: None, unsorted: None, map: None })
    }

    fn push(&mut self, id: Id, value: &[u8]) -> Result<()> {
        if let Some(s) = &mut self.unsorted {
            return s.push(&id.to_be_bytes(), value);
        }
        if self.last.is_some_and(|l| id <= l) {
            let mut s = Sorter::new(&self.tmp, "ids-unsorted", 64 << 20);
            s.push(&id.to_be_bytes(), value)?;
            self.unsorted = Some(s);
            return Ok(());
        }
        self.append(id, value)
    }

    fn append(&mut self, id: Id, value: &[u8]) -> Result<()> {
        let new_block = match self.index.last() {
            None => true,
            Some(&(base, _)) => self.block_len >= ID_BLOCK || id - base > u32::MAX as u64,
        };
        if new_block {
            self.index.push((id, self.records));
            self.block_len = 0;
        }
        let off = (id - self.index.last().unwrap().0) as u32;
        let out = self.out.as_mut().context("id table is frozen")?;
        out.write_all(&off.to_le_bytes())?;
        out.write_all(value)?;
        self.records += 1;
        self.block_len += 1;
        self.last = Some(id);
        Ok(())
    }

    /// Finish writing; lookups work from now on.
    fn freeze(&mut self) -> Result<()> {
        let Some(out) = self.out.take() else { return Ok(()) };
        let file = out.into_inner().map_err(|e| e.into_error())?;
        if let Some(mut s) = self.unsorted.take() {
            // Feed the records written so far to the sorter, then rewrite.
            drop(file);
            let rw = 4 + self.width;
            let data = std::fs::read(&self.path)?;
            for (bi, &(base, first)) in self.index.iter().enumerate() {
                let end = self.index.get(bi + 1).map_or(self.records, |b| b.1);
                for r in first..end {
                    let rec = &data[r as usize * rw..(r as usize + 1) * rw];
                    let id = base + u32::from_le_bytes(rec[..4].try_into()?) as u64;
                    s.push(&id.to_be_bytes(), &rec[4..])?;
                }
            }
            drop(data);
            self.out = Some(BufWriter::with_capacity(1 << 20, File::create(&self.path)?));
            self.records = 0;
            self.index.clear();
            self.last = None;
            let mut m = s.finish()?;
            while let Some((k, v)) = m.next()? {
                let id = u64::from_be_bytes(k.try_into()?);
                if self.last != Some(id) {
                    self.append(id, v)?;
                }
            }
            return self.freeze();
        }
        file.sync_data().ok();
        drop(file);
        let f = File::open(&self.path)?;
        if self.records > 0 {
            self.map = Some(unsafe { Mmap::map(&f)? });
        }
        Ok(())
    }

    fn view(&self) -> Option<IdView<'_>> {
        Some(IdView { map: self.map.as_ref()?, index: &self.index, records: self.records, rw: 4 + self.width })
    }

    /// Values of many ids at once. `refs` holds (id, slot) pairs, the result
    /// is indexed by slot. The ids are looked up in sorted order, in parallel
    /// ranges, so the reads go forward through the file: one at a time, each
    /// lookup in a table larger than memory is a random read (the planet node
    /// table is 130 GB).
    fn get_many<T: Copy + Send>(&self, refs: Vec<(Id, u32)>, decode: impl Fn(&[u8]) -> T + Sync) -> Vec<Option<T>> {
        // The lookups mostly wait for the disk: more threads than the
        // default pool keep more reads in flight.
        static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
        let pool = POOL.get_or_init(|| rayon::ThreadPoolBuilder::new().num_threads(LOOKUP_THREADS).build().ok());
        let decode = &decode;
        match pool {
            Some(p) => p.install(|| self.get_many_in(refs, decode)),
            None => self.get_many_in(refs, decode),
        }
    }

    fn get_many_in<T: Copy + Send>(&self, mut refs: Vec<(Id, u32)>, decode: impl Fn(&[u8]) -> T + Sync) -> Vec<Option<T>> {
        use rayon::prelude::*;
        let mut out = vec![None; refs.len()];
        let Some(view) = self.view() else { return out };
        refs.par_sort_unstable();
        let decode = &decode;
        let found: Vec<Option<T>> = refs
            .par_chunks(1 << 16)
            .flat_map_iter(|part| {
                let mut hint = 0;
                let mut last: Option<(Id, Option<T>)> = None;
                part.iter().map(move |&(id, _)| match last {
                    Some((l, v)) if l == id => v,
                    _ => {
                        let v = view.get(id, &mut hint).map(decode);
                        last = Some((id, v));
                        v
                    }
                })
            })
            .collect();
        for (&(_, slot), v) in refs.iter().zip(found) {
            out[slot as usize] = v;
        }
        out
    }
}

/// The lookup side of a frozen `IdTable`, shareable between threads.
#[derive(Clone, Copy)]
struct IdView<'a> {
    map: &'a [u8],
    index: &'a [(u64, u64)],
    records: u64,
    rw: usize,
}

impl<'a> IdView<'a> {
    /// `hint` is the block of the previous lookup (lookups tend to stay in one block).
    fn get(&self, id: Id, hint: &mut usize) -> Option<&'a [u8]> {
        let n = self.index.len();
        let within = |b: usize| self.index[b].0 <= id && self.index.get(b + 1).is_none_or(|nb| id < nb.0);
        let b = if *hint < n && within(*hint) {
            *hint
        } else {
            let p = self.index.partition_point(|b| b.0 <= id);
            if p == 0 {
                return None;
            }
            *hint = p - 1;
            p - 1
        };
        let (base, first) = self.index[b];
        let end = self.index.get(b + 1).map_or(self.records, |nb| nb.1);
        let off = id - base;
        if off > u32::MAX as u64 {
            return None;
        }
        let (map, rw) = (self.map, self.rw);
        let rec = |r: u64| &map[r as usize * rw..(r as usize + 1) * rw];
        let (mut lo, mut hi) = (first, end);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let o = u32::from_le_bytes(rec(mid)[..4].try_into().unwrap()) as u64;
            match o.cmp(&off) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(&rec(mid)[4..]),
            }
        }
        None
    }
}

impl Drop for IdTable {
    fn drop(&mut self) {
        self.map = None;
        self.out = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Node positions (y, x).
struct Locations(IdTable);

impl Locations {
    fn set(&mut self, id: Id, p: Pt) -> Result<()> {
        let mut v = [0u8; 8];
        v[..4].copy_from_slice(&p.y.to_le_bytes());
        v[4..].copy_from_slice(&p.x.to_le_bytes());
        self.0.push(id, &v)
    }

    fn decode(v: &[u8]) -> Pt {
        Pt { y: u32::from_le_bytes(v[..4].try_into().unwrap()), x: u32::from_le_bytes(v[4..8].try_into().unwrap()) }
    }

    fn get_many(&self, refs: Vec<(Id, u32)>) -> Vec<Option<Pt>> {
        self.0.get_many(refs, Self::decode)
    }
}

/// Way bounding boxes, C++ indexes and completeness.
struct WayBoxes(IdTable);

impl WayBoxes {
    fn set(&mut self, id: Id, bbox: &Rect, idx: u32, complete: bool) -> Result<()> {
        let mut v = [0u8; 24];
        for (i, x) in [bbox.y0, bbox.x0, bbox.y1, bbox.x1, idx, complete as u32].iter().enumerate() {
            v[4 * i..4 * i + 4].copy_from_slice(&x.to_le_bytes());
        }
        self.0.push(id, &v)
    }

    fn decode(s: &[u8]) -> (Rect, u32, bool) {
        let u = |i: usize| u32::from_le_bytes(s[4 * i..4 * i + 4].try_into().unwrap());
        (Rect { y0: u(0), x0: u(1), y1: u(2), x1: u(3) }, u(4), u(5) != 0)
    }

    fn get_many(&self, refs: Vec<(Id, u32)>) -> Vec<Option<(Rect, u32, bool)>> {
        self.0.get_many(refs, Self::decode)
    }
}

// ---------------------------------------------------------------------------
// The loader.

/// Sorters of one element kind.
struct Phase {
    kind: Kind,
    data: Sorter,
    meta: Sorter,
    ids: Sorter,
    tags: Sorter,
}

pub struct Loader<'d> {
    app: Appender<'d>,
    opts: BulkOptions,
    /// 0 nodes, 1 ways, 2 relations.
    stage: usize,
    phase: Phase,
    locations: Locations,
    ways: WayBoxes,
    pending: Sorter,
    members: Sorter,
    /// Ways and relations waiting for their geometry, which is looked up for a
    /// whole batch at once (see `IdTable::get_many`).
    ways_batch: Vec<(Way, Option<Meta>, Option<String>)>,
    rels_batch: Vec<(Relation, Option<Meta>, Option<String>)>,
    /// Lookups the batch needs, and the limit that starts them.
    batch_refs: usize,
    batch_limit: usize,
    /// User names by uid; the last name seen wins, as in the C++ updater.
    users: rustc_hash::FxHashMap<u32, String>,
    pub elements: usize,
    pub max_timestamp: i64,
    start: std::time::Instant,
}

const STAGES: [Kind; 3] = [Kind::Node, Kind::Way, Kind::Relation];

impl<'d> Loader<'d> {
    pub fn new(db: &'d Db, opts: BulkOptions) -> Result<Loader<'d>> {
        if !db.is_empty()? {
            bail!("the bulk import needs an empty database");
        }
        std::fs::create_dir_all(&opts.tmp).with_context(|| format!("creating {}", opts.tmp.display()))?;
        let mut txn = db.write()?;
        txn.set_info(IMPORT_RUNNING, "1")?;
        if !opts.meta {
            txn.set_info("meta", "no")?;
        }
        let mut app = Appender::new(txn);
        // A crash from here on leaves a database marked as incomplete.
        app.checkpoint()?;
        let small = opts.memory / 20;
        Ok(Loader {
            app,
            stage: 0,
            phase: Self::new_phase(&opts, Kind::Node),
            locations: Locations(IdTable::create(&opts.tmp, "node-locations", 8)?),
            ways: WayBoxes(IdTable::create(&opts.tmp, "way-boxes", 24)?),
            pending: Sorter::new(&opts.tmp, "pending", small),
            members: Sorter::new(&opts.tmp, "members", small),
            ways_batch: Vec::new(),
            rels_batch: Vec::new(),
            batch_refs: 0,
            // About 40 bytes of lookup state per reference plus the elements:
            // 64 M references (7 M planet ways, 6 GB) for a 16 GB budget.
            batch_limit: (opts.memory / 256).max(1 << 16),
            users: Default::default(),
            elements: 0,
            max_timestamp: 0,
            start: std::time::Instant::now(),
            opts,
        })
    }

    fn new_phase(opts: &BulkOptions, kind: Kind) -> Phase {
        let m = opts.memory;
        let tmp = &opts.tmp;
        let p = kind.elem_type().name();
        Phase {
            kind,
            data: Sorter::new(tmp, &format!("{p}-data"), m * 45 / 100),
            meta: Sorter::new(tmp, &format!("{p}-meta"), if opts.meta { m * 15 / 100 } else { 0 }),
            ids: Sorter::new(tmp, &format!("{p}-ids"), m / 10),
            tags: Sorter::new(tmp, &format!("{p}-tags"), m * 2 / 10),
        }
    }

    /// Write the tables of the current kind and start the next one.
    fn next_stage(&mut self) -> Result<()> {
        // Positions are complete once the ways start, way boxes once the relations start.
        match self.stage {
            0 => self.locations.0.freeze()?,
            _ => {
                self.flush_ways()?;
                self.ways.0.freeze()?
            }
        }
        let next = Self::new_phase(&self.opts, STAGES[(self.stage + 1).min(2)]);
        let phase = std::mem::replace(&mut self.phase, next);
        self.write_phase(phase)?;
        self.stage += 1;
        Ok(())
    }

    fn write_phase(&mut self, ph: Phase) -> Result<()> {
        let kind = ph.kind;
        eprintln!("import: {} elements read, writing {} tables ({:.0?})", self.elements, kind.elem_type().name(), self.start.elapsed());
        let app = &mut self.app;
        // Ids first: they reveal duplicate elements before anything else is written.
        write_ids(app, kind, &mut ph.ids.finish()?)?;
        let mut data = ph.data.finish()?;
        match kind {
            Kind::Node => write_chunks(app, Table::Data(kind), &mut data, node_from)?,
            Kind::Way => write_chunks(app, Table::Data(kind), &mut data, way_from)?,
            _ => write_chunks(app, Table::Data(kind), &mut data, rel_from)?,
        }
        drop(data);
        write_chunks(app, Table::Meta(kind), &mut ph.meta.finish()?, meta_from)?;
        write_tags(app, kind, &mut ph.tags.finish()?)?;
        app.checkpoint()
    }

    fn add_common(&mut self, cell: Cell, id: Id, tags: &Tags, meta: Option<Meta>, user: Option<String>, record: Vec<u8>) -> Result<()> {
        let key = chunk_key(cell, id);
        self.phase.data.push(&key, &record)?;
        self.phase.ids.push(&id.to_be_bytes(), &cell.to_bytes())?;
        for (k, v) in tags.iter() {
            self.phase.tags.push(&tag_record(k, v, cell, id), &[])?;
        }
        if let Some(m) = meta {
            self.max_timestamp = self.max_timestamp.max(m.timestamp);
            if self.opts.meta {
                self.phase.meta.push(&key, &meta_record(&m))?;
                let name = user.unwrap_or_default();
                match self.users.get_mut(&m.uid) {
                    Some(n) if *n != name => *n = name,
                    Some(_) => {}
                    None => {
                        self.users.insert(m.uid, name);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn add(&mut self, c: Change) -> Result<()> {
        if c.delete {
            bail!("{} {} is deleted: the import needs a plain OSM file, not a change file", elem_name(&c.elem), c.elem.id());
        }
        let stage = match &c.elem {
            Elem::Node(_) => 0,
            Elem::Way(_) => 1,
            Elem::Relation(_) => 2,
        };
        if stage < self.stage {
            bail!("{} {} follows elements of a later type: the import needs nodes, then ways, then relations (sort the file with `osmium sort`)", elem_name(&c.elem), c.elem.id());
        }
        while self.stage < stage {
            self.next_stage()?;
        }
        self.elements += 1;
        if self.elements % 50_000_000 == 0 {
            eprintln!("import: {} elements read ({:.0?})", self.elements, self.start.elapsed());
        }
        match c.elem {
            Elem::Node(n) => {
                self.locations.set(n.id, n.pos)?;
                self.add_common(n.cell(), n.id, &n.tags, c.meta, c.user, node_record(&n))?;
            }
            Elem::Way(w) => {
                self.batch_refs += w.nodes.len();
                self.ways_batch.push((w, c.meta, c.user));
                if self.batch_refs >= self.batch_limit {
                    self.flush_ways()?;
                }
            }
            Elem::Relation(r) => {
                self.batch_refs += r.members.len();
                self.rels_batch.push((r, c.meta, c.user));
                if self.batch_refs >= self.batch_limit {
                    self.flush_relations()?;
                }
            }
        }
        Ok(())
    }

    /// Give the collected ways their geometry and store them.
    fn flush_ways(&mut self) -> Result<()> {
        let batch = std::mem::take(&mut self.ways_batch);
        self.batch_refs = 0;
        let mut refs = Vec::with_capacity(batch.iter().map(|b| b.0.nodes.len()).sum());
        for (w, ..) in &batch {
            for &n in &w.nodes {
                refs.push((n, refs.len() as u32));
            }
        }
        let pos = self.locations.get_many(refs);
        let mut at = 0;
        for (mut w, meta, user) in batch {
            let p = &pos[at..at + w.nodes.len()];
            at += w.nodes.len();
            let mut it = p.iter();
            (w.bbox, w.idx, w.missing) = way_geometry(&w, |_| it.next().copied().flatten());
            debug_assert!(it.next().is_none());
            self.ways.set(w.id, &w.bbox, w.idx, !w.missing)?;
            if w.missing {
                for (&n, q) in w.nodes.iter().zip(p) {
                    if q.is_none() {
                        let mut k = n.to_be_bytes().to_vec();
                        k.extend_from_slice(&w.id.to_be_bytes());
                        self.pending.push(&k, &[])?;
                    }
                }
            }
            self.add_common(w.cell(), w.id, &w.tags, meta, user, way_record(&w))?;
        }
        Ok(())
    }

    /// Give the collected relations their geometry and store them.
    fn flush_relations(&mut self) -> Result<()> {
        let batch = std::mem::take(&mut self.rels_batch);
        self.batch_refs = 0;
        let (mut nrefs, mut wrefs) = (Vec::new(), Vec::new());
        for (r, ..) in &batch {
            for m in &r.members {
                match m.mtype {
                    ElemType::Node => nrefs.push((m.id, nrefs.len() as u32)),
                    ElemType::Way => wrefs.push((m.id, wrefs.len() as u32)),
                    _ => {}
                }
            }
        }
        let npos = self.locations.get_many(nrefs);
        let wbox = self.ways.get_many(wrefs);
        let (mut ni, mut wi) = (npos.iter(), wbox.iter());
        for (mut r, meta, user) in batch {
            (r.bbox, r.idx, r.spatial) = relation_geometry(&r, |_| ni.next().copied().flatten(), |_| wi.next().copied().flatten());
            for m in &r.members {
                let mut k = Vec::with_capacity(18);
                k.push(m.mtype.code());
                k.extend_from_slice(&(m.id >> 8).to_be_bytes());
                k.push((m.id & 255) as u8);
                k.extend_from_slice(&r.id.to_be_bytes());
                self.members.push(&k, &[])?;
            }
            self.add_common(r.cell(), r.id, &r.tags, meta, user, rel_record(&r))?;
        }
        debug_assert!(ni.next().is_none() && wi.next().is_none());
        Ok(())
    }

    /// Write the remaining tables and mark the import complete.
    pub fn finish(mut self) -> Result<()> {
        while self.stage < 2 {
            self.next_stage()?;
        }
        self.flush_relations()?;
        let last = Self::new_phase(&self.opts, Kind::Relation);
        let phase = std::mem::replace(&mut self.phase, last);
        self.write_phase(phase)?;
        let pending = std::mem::replace(&mut self.pending, Sorter::new(&self.opts.tmp, "x", 0));
        write_keys(&mut self.app, Raw::Pending, &mut pending.finish()?)?;
        let members = std::mem::replace(&mut self.members, Sorter::new(&self.opts.tmp, "x", 0));
        write_members(&mut self.app, &mut members.finish()?)?;
        let mut users: Vec<(u32, String)> = std::mem::take(&mut self.users).into_iter().collect();
        users.sort_unstable();
        for (uid, name) in users {
            self.app.txn().set_user(uid, &name)?;
        }
        self.app.txn().delete_info(IMPORT_RUNNING)?;
        let Loader { app, .. } = self;
        app.commit()
    }
}

fn elem_name(e: &Elem) -> &'static str {
    match e {
        Elem::Node(_) => "node",
        Elem::Way(_) => "way",
        Elem::Relation(_) => "relation",
    }
}

/// Default directory for temporary import files: next to the database.
pub fn default_tmp(db_path: &Path) -> PathBuf {
    db_path.join("import-tmp")
}
