//! LMDB-backed element store.
//!
//! Tables per element kind (`n`, `w`, `r`, `a`):
//! - `{k}_data`: element chunks. The key `cell(5) ++ id(8)` is the lower
//!   bound of the chunk: it holds the elements whose (cell, id) lies between
//!   its key and the next key, sorted, possibly from many cells.
//! - `{k}_meta`: meta data chunks, keyed the same way with their own bounds.
//! - `{k}_ids`: `id >> 9` (8 bytes BE) -> delta-coded (id low bits, cell).
//! - `{k}_tags`: `key 0 value 0 cell(5)` -> (cell, ids) groups of that tag
//!   from the key's cell up to the next entry's cell.
//!
//! Shared tables: `relmembers` (`mtype(1) member_id >> 8 (8)` -> (member
//! id low byte, relation id) pairs), `pending` (`node_id(8) way_id(8)` ->
//! empty: ways waiting for a node that does not exist yet), `users` (uid BE
//! -> name), `info` (name -> value) and `area_blocks`.
//!
//! LMDB gives readers consistent snapshots while a single writer applies
//! updates, which replaces the C++ dispatcher.

pub mod codec;

use crate::geo::{Cell, CodeRange, cell_in_ranges};
use crate::model::*;
use anyhow::{Context, Result, bail};
use codec::{MetaRec, Pick};
use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions, RoTxn, RwTxn};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound;
use std::path::Path;

/// Version of the storage format, kept in `info` under `format`.
const FORMAT: &str = "2";

/// `info` key present while a bulk import runs.
pub const IMPORT_RUNNING: &str = "import_running";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Node = 0,
    Way = 1,
    Relation = 2,
    Area = 3,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Node, Kind::Way, Kind::Relation, Kind::Area];
    fn prefix(self) -> &'static str {
        match self {
            Kind::Node => "n",
            Kind::Way => "w",
            Kind::Relation => "r",
            Kind::Area => "a",
        }
    }
    pub fn elem_type(self) -> ElemType {
        match self {
            Kind::Node => ElemType::Node,
            Kind::Way => ElemType::Way,
            Kind::Relation => ElemType::Relation,
            Kind::Area => ElemType::Area,
        }
    }
}

/// A chunk table: element data or meta data of a kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Table {
    Data(Kind),
    Meta(Kind),
}

struct KindDbs {
    data: Database<Bytes, Bytes>,
    meta: Database<Bytes, Bytes>,
    ids: Database<Bytes, Bytes>,
    tags: Database<Bytes, Bytes>,
}

pub struct Db {
    env: Env,
    kinds: Vec<KindDbs>,
    relmembers: Database<Bytes, Bytes>,
    pending: Database<Bytes, Bytes>,
    users: Database<Bytes, Bytes>,
    info: Database<Bytes, Bytes>,
    /// `block_tile(4) area_id(4)` -> polylines of that area in that tile
    area_blocks: Database<Bytes, Bytes>,
}

pub type ChunkKey = [u8; 13];

pub fn chunk_key(cell: Cell, id: Id) -> ChunkKey {
    let mut k = [0u8; 13];
    k[..5].copy_from_slice(&cell.to_bytes());
    k[5..].copy_from_slice(&id.to_be_bytes());
    k
}

fn to_chunk_key(k: &[u8]) -> Result<ChunkKey> {
    k.try_into().context("bad chunk key")
}

const MAX_INDEX_PART: usize = 200;

/// Encode a tag key or value for the tag index. Long strings, and strings
/// with NUL bytes (the field separator), become a prefix, the marker byte 1
/// and a hash in hex; such entries never contain NUL, and matches against
/// them are verified on the element.
pub fn index_part(s: &str, out: &mut Vec<u8>) {
    if s.len() <= MAX_INDEX_PART && !s.as_bytes().contains(&0) {
        out.extend_from_slice(s.as_bytes());
        return;
    }
    let mut cut = s.len().min(MAX_INDEX_PART - 9);
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    out.extend(s.as_bytes()[..cut].iter().map(|&b| if b == 0 { 1 } else { b }));
    out.push(1);
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    out.extend_from_slice(format!("{h:08x}").as_bytes());
}

/// Tag index key prefix of one (key, value): `key 0 value 0`.
pub fn tag_prefix(k: &str, v: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(k.len() + v.len() + 7);
    index_part(k, &mut p);
    p.push(0);
    index_part(v, &mut p);
    p.push(0);
    p
}

fn memchr0(b: &[u8]) -> Option<usize> {
    b.iter().position(|&c| c == 0)
}

/// What a tag index scan does after an entry, or with a value it is
/// about to read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scan {
    Next,
    /// Skip the remaining cells of this value.
    SkipValue,
    Stop,
}

/// Pass the (cell, ids) groups of the tag index entry `k` -> `v` of the
/// value with prefix `p` (`key 0 value 0`, the value from `vstart`) to `f`,
/// cells in `ranges` only.
fn visit_entry(p: &[u8], vstart: usize, k: &[u8], v: &[u8], ranges: Option<&[CodeRange]>, f: &mut impl FnMut(&[u8], Cell, &[Id]) -> Result<Scan>) -> Result<Scan> {
    let val = &p[vstart..p.len() - 1];
    let base = Cell::from_bytes(&k[p.len()..]);
    for (cell, ids) in codec::decode_tag_entry(base, v)? {
        if ranges.is_some_and(|r| !cell_in_ranges(cell, r)) {
            continue;
        }
        match f(val, cell, &ids)? {
            Scan::Next => {}
            s => return Ok(s),
        }
    }
    Ok(Scan::Next)
}

/// Address space reserved for the memory map (sparse; the file grows as needed).
fn map_size() -> usize {
    std::env::var("OVERPASS_MAP_SIZE").ok().and_then(|v| v.parse().ok()).unwrap_or(1 << 40)
}

impl Db {
    /// Open a database for writing, creating it if needed (import, update,
    /// make-areas).
    pub fn open(path: &Path) -> Result<Db> {
        std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(map_size())
                .max_dbs(32)
                .max_readers(126)
                .open(path)
                .with_context(|| format!("opening database at {}", path.display()))?
        };
        let mut wtxn = env.write_txn()?;
        let mut kinds = Vec::new();
        for kind in Kind::ALL {
            let p = kind.prefix();
            kinds.push(KindDbs {
                data: env.create_database(&mut wtxn, Some(&format!("{p}_data")))?,
                meta: env.create_database(&mut wtxn, Some(&format!("{p}_meta")))?,
                ids: env.create_database(&mut wtxn, Some(&format!("{p}_ids")))?,
                tags: env.create_database(&mut wtxn, Some(&format!("{p}_tags")))?,
            });
        }
        let relmembers = env.create_database(&mut wtxn, Some("relmembers"))?;
        let pending = env.create_database(&mut wtxn, Some("pending"))?;
        let users = env.create_database(&mut wtxn, Some("users"))?;
        let info: Database<Bytes, Bytes> = env.create_database(&mut wtxn, Some("info"))?;
        let area_blocks = env.create_database(&mut wtxn, Some("area_blocks"))?;
        let format = info.get(&wtxn, "format".as_bytes())?.map(|f| f.to_vec());
        match format {
            Some(f) => check_format(path, Some(&f))?,
            None if kinds.iter().any(|k| k.ids.len(&wtxn).is_ok_and(|n| n > 0)) => check_format(path, None)?,
            None => info.put(&mut wtxn, "format".as_bytes(), FORMAT.as_bytes())?,
        }
        wtxn.commit()?;
        Ok(Db { env, kinds, relmembers, pending, users, info, area_blocks })
    }

    /// Open an existing database for queries, without taking the writer
    /// lock. `readers`: concurrent snapshots to allow.
    pub fn open_read(path: &Path, readers: u32) -> Result<Db> {
        if !path.join("data.mdb").exists() {
            anyhow::bail!("no database at {}; create one with `overpass import`", path.display());
        }
        let env = unsafe {
            let mut o = EnvOpenOptions::new();
            o.map_size(map_size()).max_dbs(32).max_readers(readers.max(126));
            // Random access: read-ahead only pollutes the page cache.
            o.flags(heed::EnvFlags::NO_READ_AHEAD);
            o.open(path).with_context(|| format!("opening database at {}", path.display()))?
        };
        // Reader slots of crashed processes would otherwise stay taken.
        env.clear_stale_readers()?;
        let rtxn = env.read_txn()?;
        let open = |name: &str| -> Result<Database<Bytes, Bytes>> {
            env.open_database::<Bytes, Bytes>(&rtxn, Some(name))?.with_context(|| format!("database at {} has no table {name}", path.display()))
        };
        let info = open("info")?;
        check_format(path, info.get(&rtxn, "format".as_bytes())?)?;
        if info.get(&rtxn, IMPORT_RUNNING.as_bytes())?.is_some() {
            bail!("the import into {} did not finish; delete the database and import again", path.display());
        }
        let mut kinds = Vec::new();
        for kind in Kind::ALL {
            let p = kind.prefix();
            kinds.push(KindDbs { data: open(&format!("{p}_data"))?, meta: open(&format!("{p}_meta"))?, ids: open(&format!("{p}_ids"))?, tags: open(&format!("{p}_tags"))? });
        }
        let (relmembers, pending, users, area_blocks) = (open("relmembers")?, open("pending")?, open("users")?, open("area_blocks")?);
        rtxn.commit()?;
        Ok(Db { env, kinds, relmembers, pending, users, info, area_blocks })
    }

    fn k(&self, kind: Kind) -> &KindDbs {
        &self.kinds[kind as usize]
    }

    fn chunks(&self, t: Table) -> Database<Bytes, Bytes> {
        match t {
            Table::Data(k) => self.k(k).data,
            Table::Meta(k) => self.k(k).meta,
        }
    }

    pub fn snapshot(&self) -> Result<Snapshot<'_>> {
        Ok(Snapshot { db: self, txn: self.env.read_txn()? })
    }

    pub fn write(&self) -> Result<WriteTxn<'_>> {
        Ok(WriteTxn { db: self, txn: self.env.write_txn()?, tag_delta: BTreeMap::new(), id_delta: BTreeMap::new(), member_delta: BTreeMap::new() })
    }

    /// Directory of the database.
    pub fn path(&self) -> &Path {
        self.env.path()
    }

    /// Whether the database holds no elements (a bulk import can start).
    pub fn is_empty(&self) -> Result<bool> {
        let txn = self.env.read_txn()?;
        for k in &self.kinds {
            if !k.ids.is_empty(&txn)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Fail if a bulk import into this database did not finish.
    pub fn check_complete(&self, path: &Path) -> Result<()> {
        let txn = self.env.read_txn()?;
        if self.info.get(&txn, IMPORT_RUNNING.as_bytes())?.is_some() {
            bail!("the import into {} did not finish; delete the database and import again", path.display());
        }
        Ok(())
    }

    /// Bytes in use and file size.
    pub fn usage(&self) -> Result<(u64, u64)> {
        Ok((self.env.non_free_pages_size()?, self.env.real_disk_size()?))
    }

    /// Rewrite the database file without free pages. The writer lock is
    /// held meanwhile, so no update gets lost; readers that keep the old
    /// file open still see the old data.
    pub fn compact(self, path: &Path) -> Result<()> {
        let tmp = path.join("data.mdb.compact");
        let _ = std::fs::remove_file(&tmp);
        let lock = self.env.write_txn()?;
        self.env.copy_to_file(&tmp, heed::CompactionOption::Enabled).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path.join("data.mdb"))?;
        lock.abort();
        Ok(())
    }
}

fn check_format(path: &Path, found: Option<&[u8]>) -> Result<()> {
    match found {
        Some(f) if f == FORMAT.as_bytes() => Ok(()),
        Some(f) => bail!("database at {} has storage format {}, this version needs {FORMAT}; import the data again", path.display(), String::from_utf8_lossy(f)),
        None => bail!("database at {} has an older storage format; import the data again", path.display()),
    }
}

// ---------------------------------------------------------------------------
// Chunk lookups shared by snapshots and write transactions.

/// Chunks that may hold elements in the ranges (sorted by level and start),
/// each once, in key order.
fn chunks_in_ranges<'t>(db: Database<Bytes, Bytes>, txn: &'t RoTxn, ranges: &[CodeRange]) -> Result<Vec<&'t [u8]>> {
    let mut out = Vec::new();
    let mut last: Option<ChunkKey> = None;
    for r in ranges {
        let lo = chunk_key(Cell { level: r.level, code: r.lo }, 0);
        let hi = chunk_key(Cell { level: r.level, code: r.hi }, u64::MAX);
        if last.is_some_and(|l| l >= hi) {
            continue;
        }
        // The chunk holding the start of the range may begin before it.
        let start = match db.get_lower_than_or_equal_to(txn, &lo)? {
            Some((k, _)) => to_chunk_key(k)?,
            None => lo,
        };
        let prev = last;
        let lower = match &prev {
            Some(l) if *l >= start => Bound::Excluded(&l[..]),
            _ => Bound::Included(&start[..]),
        };
        for item in db.range(txn, &(lower, Bound::Included(&hi[..])))? {
            let (k, v) = item?;
            out.push(v);
            last = Some(to_chunk_key(k)?);
        }
    }
    Ok(out)
}

/// Chunks holding the given (cell, id) positions (sorted), with the ids
/// each holds (sorted). A chunk up to `STEPS` entries after the last one
/// is reached by stepping a cursor, which costs less than a search from
/// the root of the tree: the 1.4 x 10^6 ways tagged boundary=
/// administrative lie in a ninth of the 3 x 10^6 way chunks.
fn chunks_for_pairs<'t>(db: Database<Bytes, Bytes>, txn: &'t RoTxn, pairs: &[(Cell, Id)]) -> Result<Vec<(ChunkKey, &'t [u8], Vec<Id>)>> {
    const STEPS: usize = 8;
    fn entry<'t>(it: &mut impl Iterator<Item = heed::Result<(&'t [u8], &'t [u8])>>) -> Result<Option<(ChunkKey, &'t [u8])>> {
        match it.next() {
            Some(e) => {
                let (k, v) = e?;
                Ok(Some((to_chunk_key(k)?, v)))
            }
            None => Ok(None),
        }
    }
    let mut out: Vec<(ChunkKey, &[u8], Vec<Id>)> = Vec::new();
    // The entries after the last chunk of `out`: the first one, and a
    // cursor at the following ones.
    let mut next: Option<(ChunkKey, &[u8])> = None;
    let mut cursor = None;
    for &(cell, id) in pairs {
        let k = chunk_key(cell, id);
        if let Some((s, _, ids)) = out.last_mut() {
            if *s <= k && next.is_none_or(|(n, _)| k < n) {
                ids.push(id);
                continue;
            }
        }
        // The chunk holding k: the last one starting at or before it.
        let mut found = None;
        if let Some(it) = cursor.as_mut() {
            for _ in 0..STEPS {
                let Some((nk, nv)) = next else { break };
                if nk > k {
                    break;
                }
                next = entry(it)?;
                if next.is_none_or(|(n, _)| k < n) {
                    found = Some((nk, nv));
                    break;
                }
            }
        }
        let (sk, v) = match found {
            Some(f) => f,
            None => {
                let Some((sk, v)) = db.get_lower_than_or_equal_to(txn, &k)? else { continue };
                let sk = to_chunk_key(sk)?;
                let mut it = db.range(txn, &(Bound::Excluded(&sk[..]), Bound::Unbounded))?;
                next = entry(&mut it)?;
                cursor = Some(it);
                (sk, v)
            }
        };
        out.push((sk, v, vec![id]));
    }
    for (_, _, ids) in out.iter_mut() {
        ids.sort_unstable();
        ids.dedup();
    }
    Ok(out)
}

/// (id, cell) of the ids found in the id index (ids sorted and unique).
fn lookup_cells(db: Database<Bytes, Bytes>, txn: &RoTxn, sorted: &[Id]) -> Result<Vec<(Id, Cell)>> {
    let mut out = Vec::with_capacity(sorted.len());
    let mut wanted = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let (start, bucket) = (i, sorted[i] >> codec::ID_BUCKET_BITS);
        wanted.clear();
        while i < sorted.len() && sorted[i] >> codec::ID_BUCKET_BITS == bucket {
            wanted.push((sorted[i] & ((1 << codec::ID_BUCKET_BITS) - 1)) as u16);
            i += 1;
        }
        if let Some(v) = db.get(txn, &bucket.to_be_bytes())? {
            codec::pick_id_bucket(v, &wanted, |j, c| out.push((sorted[start + j], c)))?;
        }
    }
    Ok(out)
}

fn sorted_pairs(cells: impl IntoIterator<Item = (Id, Cell)>) -> Vec<(Cell, Id)> {
    let mut pairs: Vec<(Cell, Id)> = cells.into_iter().map(|(id, c)| (c, id)).collect();
    pairs.sort_unstable();
    pairs
}

fn member_key(mtype: ElemType, bucket: u64) -> [u8; 9] {
    let mut k = [0u8; 9];
    k[0] = mtype.code();
    k[1..].copy_from_slice(&bucket.to_be_bytes());
    k
}

/// A consistent read view of the database.
pub struct Snapshot<'e> {
    db: &'e Db,
    txn: RoTxn<'e>,
}

impl<'e> Snapshot<'e> {
    pub fn info(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db.info.get(&self.txn, key.as_bytes())?.map(|v| String::from_utf8_lossy(v).into_owned()))
    }

    pub fn user_name(&self, uid: u32) -> Result<Option<String>> {
        Ok(self.db.users.get(&self.txn, &uid.to_be_bytes())?.map(|v| String::from_utf8_lossy(v).into_owned()))
    }

    /// Chunks that may hold elements in the cell ranges, each once; decode
    /// them with `Pick::Cells(ranges)`.
    pub fn chunks_in_ranges(&self, kind: Kind, ranges: &[CodeRange]) -> Result<Vec<&[u8]>> {
        chunks_in_ranges(self.db.k(kind).data, &self.txn, ranges)
    }

    /// Number of chunks of a kind (from the LMDB statistics, O(1)).
    pub fn chunk_count(&self, kind: Kind) -> Result<u64> {
        Ok(self.db.k(kind).data.len(&self.txn)?)
    }

    /// Every chunk of a kind, borrowed from the memory map.
    pub fn all_chunks(&self, kind: Kind) -> Result<Vec<&[u8]>> {
        let mut out = Vec::new();
        for item in self.db.k(kind).data.iter(&self.txn)? {
            out.push(item?.1);
        }
        Ok(out)
    }

    /// Look up the cells of the given ids (ids need not be sorted).
    pub fn cells_of(&self, kind: Kind, ids: &[Id]) -> Result<Vec<(Id, Cell)>> {
        let mut sorted: Vec<Id> = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        lookup_cells(self.db.k(kind).ids, &self.txn, &sorted)
    }

    /// Chunks holding elements at known (cell, id) positions, with the ids
    /// each holds (sorted).
    pub fn chunks_at(&self, kind: Kind, mut pairs: Vec<(Cell, Id)>) -> Result<Vec<(&[u8], Vec<Id>)>> {
        pairs.sort_unstable();
        pairs.dedup();
        Ok(chunks_for_pairs(self.db.k(kind).data, &self.txn, &pairs)?.into_iter().map(|(_, v, ids)| (v, ids)).collect())
    }

    /// Chunks that hold the given ids, with the ids each holds (sorted).
    pub fn chunks_with_ids(&self, kind: Kind, ids: &[Id]) -> Result<Vec<(&[u8], Vec<Id>)>> {
        let pairs = sorted_pairs(self.cells_of(kind, ids)?);
        Ok(chunks_for_pairs(self.db.k(kind).data, &self.txn, &pairs)?.into_iter().map(|(_, v, ids)| (v, ids)).collect())
    }

    /// Meta data of the given elements.
    pub fn metas(&self, kind: Kind, ids: &[Id]) -> Result<Vec<(Id, Meta)>> {
        let mut out = Vec::with_capacity(ids.len());
        for (v, want) in self.meta_chunks(kind, ids)? {
            out.extend(codec::decode_meta(v, Pick::Ids(&want))?.into_iter().map(|m| (m.id, m.meta)));
        }
        Ok(out)
    }

    /// Meta data chunks holding the given elements, with the ids each holds
    /// (sorted), for decoding with `codec::decode_meta`.
    pub fn meta_chunks(&self, kind: Kind, ids: &[Id]) -> Result<Vec<(&[u8], Vec<Id>)>> {
        let pairs = sorted_pairs(self.cells_of(kind, ids)?);
        Ok(chunks_for_pairs(self.db.k(kind).meta, &self.txn, &pairs)?.into_iter().map(|(_, v, ids)| (v, ids)).collect())
    }

    /// Tag index entries of `key` (and `value`), optionally only in cells
    /// of `ranges` (sorted by level and start, as `CodeRange` lists are).
    /// `f(value, cell, ids)` is called per cell; it can skip the rest of the
    /// current value or stop the scan. Cells outside the ranges are skipped
    /// by seeking.
    pub fn scan_tags(&self, kind: Kind, key: &str, value: Option<&str>, ranges: Option<&[CodeRange]>, f: impl FnMut(&[u8], Cell, &[Id]) -> Result<Scan>) -> Result<()> {
        let mut prefix = Vec::new();
        index_part(key, &mut prefix);
        self.scan_tags_raw(kind, &prefix, value, ranges, f)
    }

    /// `scan_tags` for a key as stored in the index (see `index_part`).
    pub fn scan_tags_raw(&self, kind: Kind, key: &[u8], value: Option<&str>, ranges: Option<&[CodeRange]>, mut f: impl FnMut(&[u8], Cell, &[Id]) -> Result<Scan>) -> Result<()> {
        if ranges.is_some_and(|r| r.is_empty()) {
            return Ok(());
        }
        let db = self.db.k(kind).tags;
        let mut kp = key.to_vec();
        kp.push(0);
        if let Some(v) = value {
            let mut p = kp.clone();
            index_part(v, &mut p);
            p.push(0);
            self.scan_value(db, kp.len(), &p, ranges, None, &mut f)?;
            return Ok(());
        }
        self.scan_tag_values(kind, key, b"", ranges, |_| Scan::Next, &mut f)?;
        Ok(())
    }

    /// Tag index entries of `key` (as stored, see `index_part`) whose value
    /// starts with one of `prefixes` (all values if None). `accept` is
    /// asked once per value: `Next` reads its entries, `SkipValue` steps
    /// over them, `Stop` ends the scan. As `scan_tags_raw` otherwise;
    /// returns `Stop` if a callback stopped the scan.
    pub fn scan_tags_matching(&self, kind: Kind, key: &[u8], prefixes: Option<&[Vec<u8>]>, ranges: Option<&[CodeRange]>, mut accept: impl FnMut(&[u8]) -> Scan, mut f: impl FnMut(&[u8], Cell, &[Id]) -> Result<Scan>) -> Result<Scan> {
        match prefixes {
            None => self.scan_tag_values(kind, key, b"", ranges, &mut accept, &mut f),
            Some(ps) => {
                for p in ps {
                    if self.scan_tag_values(kind, key, p, ranges, &mut accept, &mut f)? == Scan::Stop {
                        return Ok(Scan::Stop);
                    }
                }
                Ok(Scan::Next)
            }
        }
    }

    /// Walk the values of `key` that start with `prefix` in one pass of a
    /// cursor: each value is tested once with `accept` (see
    /// `scan_tags_matching`); the entries of accepted values go to `f`
    /// (cells in `ranges` only), those of other values are stepped over, or
    /// skipped with a seek when there are many.
    fn scan_tag_values(&self, kind: Kind, key: &[u8], prefix: &[u8], ranges: Option<&[CodeRange]>, mut accept: impl FnMut(&[u8]) -> Scan, f: &mut impl FnMut(&[u8], Cell, &[Id]) -> Result<Scan>) -> Result<Scan> {
        // Entries of one value read in turn before seeking past the rest
        // (rejected values) or reading the rest by range (accepted values
        // with `ranges`, see `scan_value`).
        const STEP: usize = 16;
        if ranges.is_some_and(|r| r.is_empty()) {
            return Ok(Scan::Next);
        }
        let db = self.db.k(kind).tags;
        let mut kp = key.to_vec();
        kp.push(0);
        let vstart = kp.len();
        let mut within = kp.clone();
        within.extend_from_slice(prefix);
        let mut seek = within.clone();
        // Smallest key after every entry of the value with prefix `p`.
        let past = |p: &[u8]| {
            let mut s = p.to_vec();
            *s.last_mut().unwrap() = 1;
            s
        };
        'walk: loop {
            // The value being read: `key 0 value 0`, whether it is wanted,
            // entries seen.
            let mut cur: Vec<u8> = Vec::new();
            let mut wanted = false;
            let mut seen = 0usize;
            // Last entry of `cur` passed to `f`.
            let mut visited: Vec<u8> = Vec::new();
            let entries = db.range(&self.txn, &(Bound::Included(&seek[..]), Bound::Unbounded))?;
            for item in entries {
                let (k, v) = item?;
                if !k.starts_with(&within) {
                    return Ok(Scan::Next);
                }
                // Entry keys are `key 0 value 0 cell(5)`; values hold no NUL.
                if k.len() < vstart + 6 || k[k.len() - 6] != 0 {
                    continue;
                }
                let p = &k[..k.len() - 5];
                if p != cur.as_slice() {
                    if memchr0(&p[vstart..p.len() - 1]).is_some() {
                        continue;
                    }
                    cur.clear();
                    cur.extend_from_slice(p);
                    wanted = match accept(&p[vstart..p.len() - 1]) {
                        Scan::Next => true,
                        Scan::SkipValue => false,
                        Scan::Stop => return Ok(Scan::Stop),
                    };
                    seen = 0;
                }
                seen += 1;
                if !wanted {
                    if seen > STEP {
                        seek = past(&cur);
                        continue 'walk;
                    }
                    continue;
                }
                if ranges.is_some() && seen > STEP {
                    if self.scan_value(db, vstart, &cur, ranges, Some(&visited), f)? == Scan::Stop {
                        return Ok(Scan::Stop);
                    }
                    seek = past(&cur);
                    continue 'walk;
                }
                if ranges.is_some() {
                    visited.clear();
                    visited.extend_from_slice(k);
                }
                match visit_entry(&cur, vstart, k, v, ranges, f)? {
                    Scan::Next => {}
                    Scan::SkipValue => wanted = false,
                    Scan::Stop => return Ok(Scan::Stop),
                }
            }
            return Ok(Scan::Next);
        }
    }

    /// Visit the entries of one (key, value) prefix `p`; the value starts at
    /// `vstart`. With `after`, only entries after that key. Returns `Stop`
    /// if the callback stopped the scan.
    fn scan_value(&self, db: Database<Bytes, Bytes>, vstart: usize, p: &[u8], ranges: Option<&[CodeRange]>, after: Option<&[u8]>, f: &mut impl FnMut(&[u8], Cell, &[Id]) -> Result<Scan>) -> Result<Scan> {
        let Some(ranges) = ranges else {
            for item in db.prefix_iter(&self.txn, p)? {
                let (k, v) = item?;
                if k.len() != p.len() + 5 || after.is_some_and(|a| k <= a) {
                    continue;
                }
                match visit_entry(p, vstart, k, v, None, f)? {
                    Scan::Next => {}
                    s => return Ok(s),
                }
            }
            return Ok(Scan::Next);
        };
        let mut last: Option<Vec<u8>> = after.map(|a| a.to_vec());
        let at = |level: u8, code: u32| -> Vec<u8> {
            let mut k = p.to_vec();
            k.extend_from_slice(&Cell { level, code }.to_bytes());
            k
        };
        for r in ranges {
            let lo = at(r.level, r.lo);
            let hi = at(r.level, r.hi);
            if last.as_ref().is_some_and(|l| *l >= hi) {
                continue;
            }
            // The entry holding the start of the range may begin before it.
            let start = match db.get_lower_than_or_equal_to(&self.txn, &lo)? {
                Some((k, _)) if k.starts_with(p) && k.len() == p.len() + 5 => k.to_vec(),
                _ => lo,
            };
            let prev = last.clone();
            let lower = match &prev {
                Some(l) if *l >= start => Bound::Excluded(&l[..]),
                _ => Bound::Included(&start[..]),
            };
            for item in db.range(&self.txn, &(lower, Bound::Included(&hi[..])))? {
                let (k, v) = item?;
                last = Some(k.to_vec());
                if k.len() != p.len() + 5 {
                    continue;
                }
                match visit_entry(p, vstart, k, v, Some(ranges), f)? {
                    Scan::Next => {}
                    s => return Ok(s),
                }
            }
        }
        Ok(Scan::Next)
    }

    /// Distinct tag keys of a kind (walks the index by seeking past each key).
    pub fn tag_keys(&self, kind: Kind) -> Result<Vec<Vec<u8>>> {
        let db = self.db.k(kind).tags;
        let mut out = Vec::new();
        let mut start: Vec<u8> = Vec::new();
        loop {
            let bounds: (Bound<&[u8]>, Bound<&[u8]>) =
                if start.is_empty() { (Bound::Unbounded, Bound::Unbounded) } else { (Bound::Included(&start[..]), Bound::Unbounded) };
            let mut it = db.range(&self.txn, &bounds)?;
            let Some(item) = it.next() else { break };
            let (k, _) = item?;
            let Some(z) = memchr0(k) else { break };
            let key = k[..z].to_vec();
            start = key.clone();
            start.push(1);
            out.push(key);
        }
        Ok(out)
    }

    /// Relations having one of the given elements as member.
    pub fn relations_with_members(&self, mtype: ElemType, ids: &[Id]) -> Result<Vec<Id>> {
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mut out = Vec::new();
        let mut i = 0;
        while i < sorted.len() {
            let bucket = sorted[i] >> 8;
            let j = i + sorted[i..].partition_point(|id| id >> 8 == bucket);
            if let Some(v) = self.db.relmembers.get(&self.txn, &member_key(mtype, bucket))? {
                for (lo, rel) in codec::decode_member_bucket(v)? {
                    if sorted[i..j].binary_search(&(bucket << 8 | lo as u64)).is_ok() {
                        out.push(rel);
                    }
                }
            }
            i = j;
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    pub fn count_chunks(&self, kind: Kind) -> Result<u64> {
        Ok(self.db.k(kind).data.len(&self.txn)?)
    }

    /// All area blocks in a block tile: (area id, polylines), ascending id.
    pub fn area_blocks_in_tile(&self, tile: u32) -> Result<Vec<(u32, Vec<Vec<(u32, i32)>>)>> {
        let mut out = Vec::new();
        for item in self.db.area_blocks.prefix_iter(&self.txn, &tile.to_be_bytes()[..])? {
            let (k, v) = item?;
            out.push((u32::from_be_bytes(k[4..8].try_into().unwrap()), codec::decode_area_blocks(v)?));
        }
        Ok(out)
    }

    /// Blocks of one area in one tile.
    pub fn area_blocks(&self, tile: u32, id: u32) -> Result<Option<Vec<Vec<(u32, i32)>>>> {
        match self.db.area_blocks.get(&self.txn, &area_block_key(tile, id))? {
            Some(v) => Ok(Some(codec::decode_area_blocks(v)?)),
            None => Ok(None),
        }
    }

    /// All uids whose stored user name equals one of `names`.
    pub fn uids_for_names(&self, names: &[String]) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        for item in self.db.users.iter(&self.txn)? {
            let (k, v) = item?;
            if names.iter().any(|n| n.as_bytes() == v) {
                out.push(u32::from_be_bytes(k[..4].try_into().unwrap()));
            }
        }
        Ok(out)
    }
}

/// The chunk of a table holding a key position.
pub struct ChunkSpan {
    /// Key of the chunk; `None` before the first chunk.
    pub key: Option<ChunkKey>,
    pub value: Option<Vec<u8>>,
    /// Key of the following chunk, if any.
    pub next: Option<ChunkKey>,
}

/// Tables written in key order by bulk loads (`WriteTxn::append`).
#[derive(Clone, Copy, Debug)]
pub enum Raw {
    Chunks(Table),
    Ids(Kind),
    Tags(Kind),
    RelMembers,
    Pending,
    AreaBlocks,
}

/// Tag index changes of one (kind, key, value): cell -> (added, removed).
type TagChanges = BTreeMap<Cell, (Vec<Id>, Vec<Id>)>;

/// A write transaction with buffered index deltas.
pub struct WriteTxn<'e> {
    db: &'e Db,
    txn: RwTxn<'e>,
    /// (kind, tag prefix) -> changes
    tag_delta: BTreeMap<(Kind, Vec<u8>), TagChanges>,
    /// (kind, bucket) -> id low bits -> new cell (None = removed)
    id_delta: BTreeMap<(Kind, u64), BTreeMap<u16, Option<Cell>>>,
    /// (member type, bucket) -> (member id low byte, relation) -> added
    member_delta: BTreeMap<(u8, u64), BTreeMap<(u8, Id), bool>>,
}

impl<'e> WriteTxn<'e> {
    pub fn set_info(&mut self, key: &str, value: &str) -> Result<()> {
        self.db.info.put(&mut self.txn, key.as_bytes(), value.as_bytes())?;
        Ok(())
    }

    pub fn info(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db.info.get(&self.txn, key.as_bytes())?.map(|v| String::from_utf8_lossy(v).into_owned()))
    }

    pub fn set_user(&mut self, uid: u32, name: &str) -> Result<()> {
        let key = uid.to_be_bytes();
        if self.db.users.get(&self.txn, &key)? != Some(name.as_bytes()) {
            self.db.users.put(&mut self.txn, &key, name.as_bytes())?;
        }
        Ok(())
    }

    /// Cells for ids, honouring changes buffered in this transaction.
    pub fn cells_of(&self, kind: Kind, ids: &[Id]) -> Result<HashMap<Id, Cell>> {
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let db = self.db.k(kind).ids;
        let mut out = HashMap::with_capacity(sorted.len());
        let mut i = 0;
        while i < sorted.len() {
            let bucket = sorted[i] >> codec::ID_BUCKET_BITS;
            let entries = match db.get(&self.txn, &bucket.to_be_bytes())? {
                Some(v) => codec::decode_id_bucket(v)?,
                None => Vec::new(),
            };
            let delta = self.id_delta.get(&(kind, bucket));
            while i < sorted.len() && sorted[i] >> codec::ID_BUCKET_BITS == bucket {
                let lo = (sorted[i] & ((1 << codec::ID_BUCKET_BITS) - 1)) as u16;
                let cell = match delta.and_then(|d| d.get(&lo)) {
                    Some(c) => *c,
                    None => entries.binary_search_by_key(&lo, |e| e.0).ok().map(|p| entries[p].1),
                };
                if let Some(c) = cell {
                    out.insert(sorted[i], c);
                }
                i += 1;
            }
        }
        Ok(out)
    }

    pub fn set_cell(&mut self, kind: Kind, id: Id, cell: Option<Cell>) {
        let lo = (id & ((1 << codec::ID_BUCKET_BITS) - 1)) as u16;
        self.id_delta.entry((kind, id >> codec::ID_BUCKET_BITS)).or_default().insert(lo, cell);
    }

    pub fn tag_add(&mut self, kind: Kind, k: &str, v: &str, cell: Cell, id: Id) {
        self.tag_delta.entry((kind, tag_prefix(k, v))).or_default().entry(cell).or_default().0.push(id);
    }

    pub fn tag_remove(&mut self, kind: Kind, k: &str, v: &str, cell: Cell, id: Id) {
        self.tag_delta.entry((kind, tag_prefix(k, v))).or_default().entry(cell).or_default().1.push(id);
    }

    pub fn relmember_add(&mut self, mtype: ElemType, mid: Id, rel: Id) -> Result<()> {
        self.member_delta.entry((mtype.code(), mid >> 8)).or_default().insert(((mid & 255) as u8, rel), true);
        Ok(())
    }

    pub fn relmember_remove(&mut self, mtype: ElemType, mid: Id, rel: Id) -> Result<()> {
        self.member_delta.entry((mtype.code(), mid >> 8)).or_default().insert(((mid & 255) as u8, rel), false);
        Ok(())
    }

    /// Entries of a relation member bucket, with buffered changes applied.
    fn member_bucket(&self, mtype: u8, bucket: u64) -> Result<BTreeSet<(u8, Id)>> {
        let mut set: BTreeSet<(u8, Id)> = match self.db.relmembers.get(&self.txn, &member_key(ElemType::from_code(mtype), bucket))? {
            Some(v) => codec::decode_member_bucket(v)?.into_iter().collect(),
            None => BTreeSet::new(),
        };
        if let Some(d) = self.member_delta.get(&(mtype, bucket)) {
            for (e, add) in d {
                if *add {
                    set.insert(*e);
                } else {
                    set.remove(e);
                }
            }
        }
        Ok(set)
    }

    pub fn relations_with_member(&self, mtype: ElemType, id: Id) -> Result<Vec<Id>> {
        let lo = (id & 255) as u8;
        Ok(self.member_bucket(mtype.code(), id >> 8)?.range((lo, 0)..=(lo, Id::MAX)).map(|e| e.1).collect())
    }

    /// Remember that `way` refers to the missing node `node`.
    pub fn pending_add(&mut self, node: Id, way: Id) -> Result<()> {
        self.db.pending.put(&mut self.txn, &pair_key(node, way), &[])?;
        Ok(())
    }

    pub fn pending_remove(&mut self, node: Id, way: Id) -> Result<()> {
        self.db.pending.delete(&mut self.txn, &pair_key(node, way))?;
        Ok(())
    }

    /// Ways that refer to `node` while it does not exist.
    pub fn pending_ways(&self, node: Id) -> Result<Vec<Id>> {
        let mut out = Vec::new();
        for item in self.db.pending.prefix_iter(&self.txn, &node.to_be_bytes()[..])? {
            let (k, _) = item?;
            out.push(u64::from_be_bytes(k[8..16].try_into().unwrap()));
        }
        Ok(out)
    }

    /// The chunk holding position `key`.
    pub fn chunk_span(&self, t: Table, key: &ChunkKey) -> Result<ChunkSpan> {
        let db = self.db.chunks(t);
        let (k, value) = match db.get_lower_than_or_equal_to(&self.txn, key)? {
            Some((k, v)) => (Some(to_chunk_key(k)?), Some(v.to_vec())),
            None => (None, None),
        };
        let next = match db.get_greater_than(&self.txn, key)? {
            Some((nk, _)) => Some(to_chunk_key(nk)?),
            None => None,
        };
        Ok(ChunkSpan { key: k, value, next })
    }

    pub fn put_chunk(&mut self, t: Table, key: &ChunkKey, value: &[u8]) -> Result<()> {
        self.db.chunks(t).put(&mut self.txn, key, value)?;
        Ok(())
    }

    pub fn delete_chunk(&mut self, t: Table, key: &ChunkKey) -> Result<()> {
        self.db.chunks(t).delete(&mut self.txn, key)?;
        Ok(())
    }

    /// Chunks that may hold elements in the ranges (decode them with
    /// `Pick::Cells`), e.g. to find ways using a moved node.
    pub fn chunks_in_ranges(&self, kind: Kind, ranges: &[CodeRange]) -> Result<Vec<&[u8]>> {
        chunks_in_ranges(self.db.k(kind).data, &self.txn, ranges)
    }

    /// Chunks that hold the given ids, with the ids each holds (sorted).
    pub fn chunks_with_ids(&self, kind: Kind, ids: &[Id]) -> Result<Vec<(&[u8], Vec<Id>)>> {
        let pairs = sorted_pairs(self.cells_of(kind, ids)?);
        Ok(chunks_for_pairs(self.db.k(kind).data, &self.txn, &pairs)?.into_iter().map(|(_, v, ids)| (v, ids)).collect())
    }

    pub fn put_area_blocks(&mut self, tile: u32, id: u32, blocks: &[Vec<(u32, i32)>]) -> Result<()> {
        self.db.area_blocks.put(&mut self.txn, &area_block_key(tile, id), &codec::encode_area_blocks(blocks))?;
        Ok(())
    }

    pub fn delete_area_blocks(&mut self, tile: u32, id: u32) -> Result<()> {
        self.db.area_blocks.delete(&mut self.txn, &area_block_key(tile, id))?;
        Ok(())
    }

    /// Write pending index changes into the transaction.
    pub fn flush_indexes(&mut self) -> Result<()> {
        let id_delta = std::mem::take(&mut self.id_delta);
        for ((kind, bucket), changes) in id_delta {
            let db = self.db.k(kind).ids;
            let key = bucket.to_be_bytes();
            let mut entries: BTreeMap<u16, Cell> = match db.get(&self.txn, &key)? {
                Some(v) => codec::decode_id_bucket(v)?.into_iter().collect(),
                None => BTreeMap::new(),
            };
            for (lo, cell) in changes {
                match cell {
                    Some(c) => entries.insert(lo, c),
                    None => entries.remove(&lo),
                };
            }
            if entries.is_empty() {
                db.delete(&mut self.txn, &key)?;
            } else {
                let list: Vec<(u16, Cell)> = entries.into_iter().collect();
                db.put(&mut self.txn, &key, &codec::encode_id_bucket(&list))?;
            }
        }
        let member_delta = std::mem::take(&mut self.member_delta);
        for ((mtype, bucket), changes) in member_delta {
            let key = member_key(ElemType::from_code(mtype), bucket);
            let mut set: BTreeSet<(u8, Id)> = match self.db.relmembers.get(&self.txn, &key)? {
                Some(v) => codec::decode_member_bucket(v)?.into_iter().collect(),
                None => BTreeSet::new(),
            };
            for (e, add) in changes {
                if add {
                    set.insert(e);
                } else {
                    set.remove(&e);
                }
            }
            if set.is_empty() {
                self.db.relmembers.delete(&mut self.txn, &key)?;
            } else {
                let list: Vec<(u8, Id)> = set.into_iter().collect();
                self.db.relmembers.put(&mut self.txn, &key, &codec::encode_member_bucket(&list))?;
            }
        }
        let tag_delta = std::mem::take(&mut self.tag_delta);
        for ((kind, p), changes) in tag_delta {
            self.flush_tag(kind, &p, changes)?;
        }
        Ok(())
    }

    /// Apply the changes of one (key, value) to its index entries.
    fn flush_tag(&mut self, kind: Kind, p: &[u8], changes: TagChanges) -> Result<()> {
        let db = self.db.k(kind).tags;
        let own = |k: &[u8]| k.starts_with(p) && k.len() == p.len() + 5;
        let mut changes: Vec<(Cell, (Vec<Id>, Vec<Id>))> = changes.into_iter().collect();
        let mut i = 0;
        while i < changes.len() {
            let mut probe = p.to_vec();
            probe.extend_from_slice(&changes[i].0.to_bytes());
            // The entry holding this cell, and where the next one starts.
            let (stored, mut groups): (Option<Vec<u8>>, BTreeMap<Cell, Vec<Id>>) = match db.get_lower_than_or_equal_to(&self.txn, &probe)? {
                Some((k, v)) if own(k) => (Some(k.to_vec()), codec::decode_tag_entry(Cell::from_bytes(&k[p.len()..]), v)?.into_iter().collect()),
                _ => (None, BTreeMap::new()),
            };
            let next: Option<Cell> = match db.get_greater_than(&self.txn, &probe)? {
                Some((k, _)) if own(k) => Some(Cell::from_bytes(&k[p.len()..])),
                _ => None,
            };
            while i < changes.len() && next.is_none_or(|n| changes[i].0 < n) {
                let cell = changes[i].0;
                let (add, mut rem) = std::mem::take(&mut changes[i].1);
                let ids = groups.entry(cell).or_default();
                rem.sort_unstable();
                // Removals first: an id moved within the same cell appears in both.
                ids.retain(|id| rem.binary_search(id).is_err());
                ids.extend(add);
                ids.sort_unstable();
                ids.dedup();
                if ids.is_empty() {
                    groups.remove(&cell);
                }
                i += 1;
            }
            let groups: Vec<(Cell, Vec<Id>)> = groups.into_iter().collect();
            if groups.is_empty() {
                if let Some(k) = stored {
                    db.delete(&mut self.txn, &k)?;
                }
                continue;
            }
            // Split into entries that stay inside a leaf page.
            let mut parts = Vec::new();
            let (mut s, mut size) = (0, 0);
            for (j, (_, ids)) in groups.iter().enumerate() {
                let g = codec::tag_group_size(ids);
                if j > s && size + g > codec::ENTRY_MAX {
                    parts.push(s..j);
                    (s, size) = (j, 0);
                }
                size += g;
            }
            parts.push(s..groups.len());
            for (pi, r) in parts.into_iter().enumerate() {
                let key = match (&stored, pi) {
                    (Some(k), 0) => k.clone(),
                    _ => [p, &groups[r.start].0.to_bytes()[..]].concat(),
                };
                let base = Cell::from_bytes(&key[p.len()..]);
                db.put(&mut self.txn, &key, &codec::encode_tag_entry(base, &groups[r]))?;
            }
        }
        Ok(())
    }

    pub fn commit(mut self) -> Result<()> {
        self.flush_indexes()?;
        self.txn.commit()?;
        Ok(())
    }

    fn raw(&self, t: Raw) -> Database<Bytes, Bytes> {
        match t {
            Raw::Chunks(t) => self.db.chunks(t),
            Raw::Ids(k) => self.db.k(k).ids,
            Raw::Tags(k) => self.db.k(k).tags,
            Raw::RelMembers => self.db.relmembers,
            Raw::Pending => self.db.pending,
            Raw::AreaBlocks => self.db.area_blocks,
        }
    }

    /// Append a record after the last key of a table (bulk loads write
    /// every table in key order; pages are filled completely).
    pub fn append(&mut self, t: Raw, key: &[u8], value: &[u8]) -> Result<()> {
        let db = self.raw(t);
        db.put_with_flags(&mut self.txn, heed::PutFlags::APPEND, key, value).with_context(|| format!("appending to {t:?}"))?;
        Ok(())
    }

    /// Remove all records of a table.
    pub fn clear(&mut self, t: Raw) -> Result<()> {
        let db = self.raw(t);
        db.clear(&mut self.txn)?;
        Ok(())
    }

    pub fn delete_info(&mut self, key: &str) -> Result<()> {
        self.db.info.delete(&mut self.txn, key.as_bytes())?;
        Ok(())
    }

    /// Commit and continue in a new transaction (long bulk loads).
    pub fn checkpoint(self) -> Result<WriteTxn<'e>> {
        let db = self.db;
        self.commit()?;
        db.write()
    }
}

/// Decode the meta records of a meta chunk value (all of them).
pub fn decode_meta_chunk(value: &[u8]) -> Result<Vec<MetaRec>> {
    codec::decode_meta(value, Pick::All)
}

fn pair_key(a: Id, b: Id) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&a.to_be_bytes());
    k[8..].copy_from_slice(&b.to_be_bytes());
    k
}

fn area_block_key(tile: u32, id: u32) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[..4].copy_from_slice(&tile.to_be_bytes());
    k[4..].copy_from_slice(&id.to_be_bytes());
    k
}
