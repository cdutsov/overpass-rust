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
/// Info key counting the commits: two read transactions with the same
/// count see the same data (`Snapshot::sibling`).
const COMMITS: &str = "commits";

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
    /// Read transactions `Snapshot::scan_tags_par` may hold at once.
    sibling_cap: usize,
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

/// Read transactions `scan_tags_par` holds besides the snapshots of the
/// queries, over all queries at once: the reader table of LMDB has 126
/// slots, and each query and the updates need one. Fewer when the server
/// leaves fewer slots free (`Db::open_read`).
const SIBLING_CAP: usize = 64;
static SIBLINGS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The values a thread of `scan_tags_par` walks: from `at` (the value it
/// is in) up to `hi` (exclusive).
#[derive(Default)]
struct Part {
    at: Vec<u8>,
    hi: Vec<u8>,
}

/// The smallest byte string after every string starting with `p` (which
/// is not all 0xFF: a key prefix holds the 0 after the key).
fn successor(p: &[u8]) -> Vec<u8> {
    let mut s = p.to_vec();
    while s.last() == Some(&0xFF) {
        s.pop();
    }
    if let Some(b) = s.last_mut() {
        *b += 1;
    }
    s
}

/// The first 8 bytes of `s` from `n` on as a number (zeros after the end).
fn word_at(s: &[u8], n: usize) -> u64 {
    let mut w = [0u8; 8];
    for (i, b) in s.iter().skip(n).take(8).enumerate() {
        w[i] = *b;
    }
    u64::from_be_bytes(w)
}

/// A byte string about halfway between `a` and `b` in byte order, strictly
/// between them; None when `a` is not below `b` or they are too close.
fn midpoint(a: &[u8], b: &[u8]) -> Option<Vec<u8>> {
    if a >= b {
        return None;
    }
    let n = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let (x, y) = (word_at(a, n), word_at(b, n));
    if y <= x + 1 {
        return None;
    }
    let mut m = a[..n].to_vec();
    m.extend_from_slice(&(x + (y - x) / 2).to_be_bytes());
    Some(m)
}

/// Rough size of the byte order interval from `a` to `b` (to pick the
/// part to split), None when it cannot be split.
fn split_width(a: &[u8], b: &[u8]) -> Option<f64> {
    midpoint(a, b)?;
    let n = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    Some((word_at(b, n) - word_at(a, n)) as f64 * 256f64.powi(-(n as i32)))
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
    let mut end = Scan::Next;
    codec::visit_tag_entry(base, v, |cell| ranges.is_none_or(|r| cell_in_ranges(cell, r)), |cell, ids| {
        end = f(val, cell, ids)?;
        Ok(end == Scan::Next)
    })?;
    Ok(end)
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
        Ok(Db { env, kinds, relmembers, pending, users, info, area_blocks, sibling_cap: SIBLING_CAP })
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
        // The slots beyond the readers asked for, for parallel scans.
        let sibling_cap = ((readers.max(126) - readers) as usize).min(SIBLING_CAP);
        Ok(Db { env, kinds, relmembers, pending, users, info, area_blocks, sibling_cap })
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

/// The entries of `db` that hold keys of the cell ranges (ascending), each
/// once, in key order. `key` writes the low or the high key (`true`) of a
/// range; the cell is 5 bytes at `cell_at` in every key. An entry holds
/// the keys from its own up to that of the next entry (a chunk its
/// elements, a tag index entry its cells): the entries of a range are the
/// last one at or before its low key, when `holder` accepts that key, and
/// those up to its high key. Entries at or before `after` are left out,
/// and none at or past `upper` is read. `visit` gets key and value, and
/// stops the walk with false (which is then returned).
///
/// One cursor walks the entries along the ranges. A search per range
/// (two, in fact) cost a statement over a country area tens of thousands
/// of searches per tag value, also for a value with one entry, and a
/// region scan as many per element kind. `next` is the first entry after
/// `last` (the last one visited or stepped over) once the cursor is
/// placed: a range that ends before its cell costs one comparison.
/// Entries before the one that holds the low key of a range are stepped
/// over, up to STEPS of them per range; then a search places the cursor
/// anew (few ranges among many entries).
#[allow(clippy::too_many_arguments)]
fn walk_ranges<'t>(db: Database<Bytes, Bytes>, txn: &'t RoTxn, upper: Bound<&[u8]>, after: Option<&[u8]>, ranges: &[CodeRange], cell_at: usize, key: impl Fn(&CodeRange, bool, &mut Vec<u8>), holder: impl Fn(&[u8]) -> bool, mut visit: impl FnMut(&'t [u8], &'t [u8]) -> Result<bool>) -> Result<bool> {
    const STEPS: usize = 16;
    let mut last: Option<Vec<u8>> = after.map(|a| a.to_vec());
    let seen = |last: &mut Option<Vec<u8>>, k: &[u8]| match last {
        Some(l) => {
            l.clear();
            l.extend_from_slice(k);
        }
        None => *last = Some(k.to_vec()),
    };
    let (mut lo, mut hi) = (Vec::new(), Vec::new());
    let mut cursor: Option<heed::RoRange<'t, Bytes, Bytes>> = None;
    let mut next: Option<(&'t [u8], &'t [u8])> = None;
    for r in ranges {
        if cursor.is_some() {
            // nothing after the last entry: no later range has any
            let Some((k, _)) = next else { return Ok(true) };
            // the next entry lies in a cell after the range: the entries
            // up to the high key have been visited
            if k.get(cell_at..cell_at + 5).is_some_and(|c| c > &Cell { level: r.level, code: r.hi }.to_bytes()[..]) {
                continue;
            }
        }
        key(r, true, &mut hi);
        if last.as_ref().is_some_and(|l| *l >= hi) {
            continue;
        }
        key(r, false, &mut lo);
        let mut placed = false;
        if let Some(it) = cursor.as_mut() {
            let mut steps = 0;
            loop {
                let Some((k, v)) = next else { return Ok(true) };
                if k >= &lo[..] {
                    // in the range, or past its low key: then the entry
                    // that holds the low key is the one before, which was
                    // visited (an entry is stepped over only when the
                    // next one is at or before the low key too)
                    placed = true;
                    break;
                }
                if steps == STEPS {
                    break;
                }
                steps += 1;
                let following = it.next().transpose()?;
                seen(&mut last, k);
                next = following;
                if following.is_some_and(|(n, _)| n <= &lo[..]) {
                    // not the entry that holds the low key
                    continue;
                }
                // the entry that holds the low key of the range
                if holder(k) && !visit(k, v)? {
                    return Ok(false);
                }
                placed = true;
                break;
            }
        }
        if !placed {
            let start = match db.get_lower_than_or_equal_to(txn, &lo)? {
                Some((k, _)) if holder(k) => k,
                _ => &lo[..],
            };
            let lower = match &last {
                Some(l) if l.as_slice() >= start => Bound::Excluded(l.as_slice()),
                _ => Bound::Included(start),
            };
            let mut it = db.range(txn, &(lower, upper))?;
            next = it.next().transpose()?;
            cursor = Some(it);
        }
        let Some(it) = cursor.as_mut() else { continue };
        while let Some((k, v)) = next {
            if k > &hi[..] {
                break;
            }
            seen(&mut last, k);
            next = it.next().transpose()?;
            if !visit(k, v)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Chunks that may hold elements in the ranges (sorted by level and start),
/// each once, in key order (`walk_ranges`).
fn chunks_in_ranges<'t>(db: Database<Bytes, Bytes>, txn: &'t RoTxn, ranges: &[CodeRange]) -> Result<Vec<&'t [u8]>> {
    let mut out = Vec::new();
    let key = |r: &CodeRange, high: bool, k: &mut Vec<u8>| {
        k.clear();
        k.extend_from_slice(&if high { chunk_key(Cell { level: r.level, code: r.hi }, u64::MAX) } else { chunk_key(Cell { level: r.level, code: r.lo }, 0) });
    };
    walk_ranges(db, txn, Bound::Unbounded, None, ranges, 0, key, |_| true, |k, v| {
        to_chunk_key(k)?;
        out.push(v);
        Ok(true)
    })?;
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

/// Buckets of the id index picked on the thread pool as one item.
const LOOKUP_GROUP: usize = 64;

/// Fewer buckets than this are picked on the calling thread.
const LOOKUP_PAR_MIN: usize = 512;

/// The buckets of the id index that hold the ids (sorted and unique): the
/// range of each bucket in `sorted` and its value. A bucket up to `STEPS`
/// entries after the last one is reached by stepping a cursor, which
/// costs less than a search from the root of the tree (the nodes of the
/// ways of a town lie in neighbouring buckets).
fn id_buckets<'t>(db: Database<Bytes, Bytes>, txn: &'t RoTxn, sorted: &[Id]) -> Result<Vec<(std::ops::Range<usize>, &'t [u8])>> {
    const STEPS: u64 = 4;
    let mut out = Vec::new();
    // a cursor after the bucket `at`
    let mut cursor: Option<heed::RoRange<Bytes, Bytes>> = None;
    let mut at = 0u64;
    let mut i = 0;
    while i < sorted.len() {
        let (start, bucket) = (i, sorted[i] >> codec::ID_BUCKET_BITS);
        while i < sorted.len() && sorted[i] >> codec::ID_BUCKET_BITS == bucket {
            i += 1;
        }
        let mut found = None;
        let mut stepped = false;
        if let Some(it) = cursor.as_mut().filter(|_| bucket > at && bucket - at <= STEPS) {
            for _ in 0..STEPS {
                match it.next() {
                    Some(e) => {
                        let (k, v) = e?;
                        let k = u64::from_be_bytes(k.try_into().map_err(|_| anyhow::anyhow!("corrupt id index key"))?);
                        at = k;
                        if k == bucket {
                            found = Some(v);
                        }
                        if k >= bucket {
                            break;
                        }
                    }
                    None => {
                        at = u64::MAX;
                        break;
                    }
                }
            }
            // the cursor is at or past the bucket now (keys are bucket
            // numbers, each step goes up by one at least); else search
            stepped = at >= bucket;
        }
        if !stepped {
            let key = bucket.to_be_bytes();
            let mut it = db.range(txn, &(Bound::Included(&key[..]), Bound::Unbounded))?;
            match it.next() {
                Some(e) => {
                    let (k, v) = e?;
                    let k = u64::from_be_bytes(k.try_into().map_err(|_| anyhow::anyhow!("corrupt id index key"))?);
                    at = k;
                    if k == bucket {
                        found = Some(v);
                    }
                }
                None => at = u64::MAX,
            }
            cursor = Some(it);
        }
        if let Some(v) = found {
            out.push((start..i, v));
        }
    }
    Ok(out)
}

/// (id, cell) of the ids found in the id index (ids sorted and unique).
/// The buckets are found on the calling thread (a transaction belongs to
/// one thread) and, when there are many, picked on the thread pool.
fn lookup_cells(db: Database<Bytes, Bytes>, txn: &RoTxn, sorted: &[Id]) -> Result<Vec<(Id, Cell)>> {
    let buckets = id_buckets(db, txn, sorted)?;
    let pick = |part: &[(std::ops::Range<usize>, &[u8])], out: &mut Vec<(Id, Cell)>| -> Result<()> {
        let mut wanted: Vec<u16> = Vec::new();
        for (range, v) in part {
            let ids = &sorted[range.clone()];
            wanted.clear();
            wanted.extend(ids.iter().map(|id| (id & ((1 << codec::ID_BUCKET_BITS) - 1)) as u16));
            codec::pick_id_bucket(v, &wanted, |j, c| out.push((ids[j], c)))?;
        }
        Ok(())
    };
    let mut out = Vec::with_capacity(sorted.len());
    if buckets.len() < LOOKUP_PAR_MIN {
        pick(&buckets, &mut out)?;
        return Ok(out);
    }
    let groups: Vec<&[(std::ops::Range<usize>, &[u8])]> = buckets.chunks(LOOKUP_GROUP).collect();
    let parts = crate::exec::par_map(&groups, 2, |part| -> Result<Vec<(Id, Cell)>> {
        let mut found = Vec::with_capacity(part.iter().map(|(r, _)| r.len()).sum());
        pick(part, &mut found)?;
        Ok(found)
    });
    for part in parts {
        out.extend(part?);
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
    /// A second read transaction on the same data, for a scan on another
    /// thread (a transaction is used by one thread at a time). None when a
    /// commit came in between, or no reader slot is free.
    pub fn sibling(&self) -> Result<Option<Snapshot<'e>>> {
        let Ok(s) = self.db.snapshot() else { return Ok(None) };
        Ok((s.info(COMMITS)? == self.info(COMMITS)?).then_some(s))
    }

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

    /// Meta data chunks of elements at known (cell, id) positions (meta
    /// data lies under the cell of its element), with the ids each holds
    /// (sorted): `meta_chunks` without the id index. `pairs` are sorted
    /// and unique; the chunks take them in order, each a run of them.
    pub fn meta_chunks_at(&self, kind: Kind, pairs: &[(Cell, Id)]) -> Result<Vec<(&[u8], Vec<Id>)>> {
        Ok(chunks_for_pairs(self.db.k(kind).meta, &self.txn, pairs)?.into_iter().map(|(_, v, ids)| (v, ids)).collect())
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

    /// The (cell, id) pairs of the values of `key` (with one of `prefixes`)
    /// that pass `accept`, in cells in `ranges` that pass `keep`, read on
    /// `threads` threads: the pages of a key with many values are mostly
    /// read from disk, one at a time per thread. Each thread has its own
    /// read transaction on this snapshot (fewer threads when `SIBLING_CAP`
    /// is reached). The prefixes are the first parts;
    /// a thread without one takes the upper half of what another thread
    /// has left (by byte order of the values). The pairs are in no
    /// particular order. None when another transaction cannot be opened on
    /// the same data. `stop` ends the scan early (out of time); so does a
    /// count of pairs over `max`, and then more than `max` are returned.
    #[allow(clippy::too_many_arguments)]
    pub fn scan_tags_par(&self, kind: Kind, key: &[u8], prefixes: Option<&[Vec<u8>]>, ranges: Option<&[CodeRange]>, threads: usize, max: usize, stop: impl Fn() -> bool + Sync, accept: impl Fn(&[u8]) -> bool + Sync, keep: impl Fn(Cell) -> bool + Sync) -> Result<Option<Vec<(Cell, Id)>>> {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
        // Extra transactions within the cap (all queries together).
        let cap = self.db.sibling_cap;
        let want = threads.max(1) - 1;
        let held = SIBLINGS.try_update(SeqCst, SeqCst, |n| Some(n + want.min(cap.saturating_sub(n)))).unwrap_or(cap);
        let got = want.min(cap.saturating_sub(held));
        struct Release(usize);
        impl Drop for Release {
            fn drop(&mut self) {
                SIBLINGS.fetch_sub(self.0, SeqCst);
            }
        }
        let _release = Release(got);
        if got == 0 {
            return Ok(None);
        }
        let mut snaps = Vec::new();
        for _ in 0..got {
            match self.sibling()? {
                Some(s) => snaps.push(s),
                None => return Ok(None),
            }
        }
        let vstart = key.len() + 1;
        let empty = [Vec::new()];
        let parts: Vec<(Vec<u8>, Vec<u8>)> = prefixes.unwrap_or(&empty).iter().map(|p| {
            let within = [key, &[0], p].concat();
            let hi = successor(&within);
            (within, hi)
        }).collect();
        let todo = Mutex::new(parts);
        let running: Vec<Mutex<Part>> = (0..=got).map(|_| Mutex::new(Part::default())).collect();
        // Threads holding a part (in `running` or about to put it there).
        let busy = AtomicUsize::new(0);
        // Pairs kept by all threads; past `max` every thread stops.
        let kept = AtomicUsize::new(0);
        let over = AtomicBool::new(false);
        let halt = || over.load(SeqCst) || stop();
        // Next part for thread `i`: a prefix not started, else the upper half
        // of the widest part another thread is in. None when every part is
        // done; a thread waits while others still work, as their parts may
        // be split.
        let take = |i: usize| -> Option<(Vec<u8>, Vec<u8>)> {
            let mut wait = 50;
            loop {
                {
                    let mut t = todo.lock().unwrap();
                    if let Some(p) = t.pop() {
                        busy.fetch_add(1, SeqCst);
                        return Some(p);
                    }
                }
                let widest = (0..running.len()).filter(|&j| j != i).filter_map(|j| {
                    let g = running[j].lock().unwrap();
                    Some((split_width(&g.at, &g.hi)?, j))
                }).max_by(|a, b| a.0.total_cmp(&b.0));
                if let Some((_, j)) = widest {
                    let mut g = running[j].lock().unwrap();
                    if let Some(mid) = midpoint(&g.at, &g.hi) {
                        let hi = std::mem::replace(&mut g.hi, mid.clone());
                        busy.fetch_add(1, SeqCst);
                        return Some((mid, hi));
                    }
                    continue;
                }
                if busy.load(SeqCst) == 0 || halt() {
                    return None;
                }
                // A walker cannot be split while it reads a value with
                // many entries (highway=residential).
                std::thread::sleep(std::time::Duration::from_micros(wait));
                wait = (wait * 2).min(5000);
            }
        };
        // Gives a part back when its walk ends, also by an error or a
        // panic, so that no thread waits for it to be split.
        struct Done<'a>(&'a Mutex<Part>, &'a AtomicUsize);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Part::default();
                self.1.fetch_sub(1, SeqCst);
            }
        }
        let work = |i: usize, snap: &Snapshot<'e>| -> Result<Vec<(Cell, Id)>> {
            let mut out = Vec::new();
            let n = std::cell::Cell::new(0u32);
            let stopped = std::cell::Cell::new(false);
            // Values and entries visited; every 4096th checks `halt`.
            let tick = || {
                n.set(n.get().wrapping_add(1));
                if n.get() % 4096 == 0 && halt() {
                    stopped.set(true);
                }
                stopped.get()
            };
            while let Some((lo, hi)) = take(i) {
                let done = Done(&running[i], &busy);
                {
                    let mut g = running[i].lock().unwrap();
                    g.at.clone_from(&lo);
                    g.hi = hi;
                }
                let within: Vec<u8> = lo[..vstart].to_vec();
                let accept = |val: &[u8]| match (tick(), accept(val)) {
                    (true, _) => Scan::Stop,
                    (false, true) => Scan::Next,
                    (false, false) => Scan::SkipValue,
                };
                let r = snap.walk_values(kind, vstart, &within, &lo, Some(&running[i]), ranges, accept, &mut |_, cell, ids| {
                    if tick() {
                        return Ok(Scan::Stop);
                    }
                    if keep(cell) {
                        out.extend(ids.iter().map(|id| (cell, *id)));
                        if kept.fetch_add(ids.len(), SeqCst) + ids.len() > max {
                            over.store(true, SeqCst);
                            stopped.set(true);
                            return Ok(Scan::Stop);
                        }
                    }
                    Ok(Scan::Next)
                });
                // Nothing left here to split off.
                drop(done);
                r?;
                if stopped.get() {
                    break;
                }
            }
            Ok(out)
        };
        let results: Vec<Result<Vec<(Cell, Id)>>> = std::thread::scope(|s| {
            let work = &work;
            // A thread the system refuses leaves the parts to the others.
            let handles: Vec<_> = snaps.into_iter().enumerate().filter_map(|(i, snap)| std::thread::Builder::new().spawn_scoped(s, move || work(i + 1, &snap)).ok()).collect();
            let mut res = vec![work(0, self)];
            res.extend(handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("tag index scan thread failed")))));
            res
        });
        let mut out = Vec::new();
        for r in results {
            out.extend(r?);
        }
        Ok(Some(out))
    }

    /// Walk the values of `key` that start with `prefix` in one pass of a
    /// cursor: each value is tested once with `accept` (see
    /// `scan_tags_matching`); the entries of accepted values go to `f`
    /// (cells in `ranges` only), those of other values are stepped over, or
    /// skipped with a seek when there are many.
    fn scan_tag_values(&self, kind: Kind, key: &[u8], prefix: &[u8], ranges: Option<&[CodeRange]>, accept: impl FnMut(&[u8]) -> Scan, f: &mut impl FnMut(&[u8], Cell, &[Id]) -> Result<Scan>) -> Result<Scan> {
        let within = [key, &[0], prefix].concat();
        self.walk_values(kind, key.len() + 1, &within, &within, None, ranges, accept, f)
    }

    /// The walk of `scan_tag_values` over the values whose entry prefix
    /// (`key 0 value 0`) starts with `within` and is not below `lo`; with
    /// `part`, also below its bound `hi`, which another thread may lower
    /// while the walk runs (`scan_tags_par`).
    #[allow(clippy::too_many_arguments)]
    fn walk_values(&self, kind: Kind, vstart: usize, within: &[u8], lo: &[u8], part: Option<&std::sync::Mutex<Part>>, ranges: Option<&[CodeRange]>, mut accept: impl FnMut(&[u8]) -> Scan, f: &mut impl FnMut(&[u8], Cell, &[Id]) -> Result<Scan>) -> Result<Scan> {
        // Entries of one value read in turn before seeking past the rest
        // (rejected values) or reading the rest by range (accepted values
        // with `ranges`, see `scan_value`).
        const STEP: usize = 16;
        if ranges.is_some_and(|r| r.is_empty()) {
            return Ok(Scan::Next);
        }
        let db = self.db.k(kind).tags;
        let mut seek = lo.to_vec();
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
                    if let Some(part) = part {
                        // A value whose entries begin before `lo` belongs
                        // to the part below.
                        if p < lo {
                            seek = past(p);
                            continue 'walk;
                        }
                        let mut g = part.lock().unwrap();
                        if p >= g.hi.as_slice() {
                            return Ok(Scan::Next);
                        }
                        g.at.clear();
                        g.at.extend_from_slice(p);
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
        // The entries of the value along the ranges (`walk_ranges`).
        let past = successor(p);
        let upper = if past.is_empty() { Bound::Unbounded } else { Bound::Excluded(&past[..]) };
        let key = |r: &CodeRange, high: bool, k: &mut Vec<u8>| {
            k.clear();
            k.extend_from_slice(p);
            k.extend_from_slice(&Cell { level: r.level, code: if high { r.hi } else { r.lo } }.to_bytes());
        };
        let whole = |k: &[u8]| k.len() == p.len() + 5;
        let mut end = Scan::Next;
        walk_ranges(db, &self.txn, upper, after, ranges, p.len(), key, |k| k.starts_with(p) && whole(k), |k, v| {
            if whole(k) {
                end = visit_entry(p, vstart, k, v, Some(ranges), f)?;
            }
            Ok(end == Scan::Next)
        })?;
        Ok(end)
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

    /// The ids in `n` id buckets picked at random (with `seed`), a sample
    /// for consistency checks; all ids for `n = 0`.
    pub fn sample_ids(&self, kind: Kind, n: usize, seed: u64) -> Result<Vec<Id>> {
        let db = self.db.k(kind).ids;
        let mut out = Vec::new();
        let add = |k: &[u8], v: &[u8], out: &mut Vec<Id>| -> Result<()> {
            let bucket = u64::from_be_bytes(k.try_into().context("bad id bucket key")?);
            out.extend(codec::decode_id_bucket(v)?.into_iter().map(|(lo, _)| bucket << codec::ID_BUCKET_BITS | lo as u64));
            Ok(())
        };
        if n == 0 {
            for item in db.iter(&self.txn)? {
                let (k, v) = item?;
                add(k, v, &mut out)?;
            }
            return Ok(out);
        }
        let Some((last, _)) = db.last(&self.txn)? else { return Ok(out) };
        let max = u64::from_be_bytes(last.try_into().context("bad id bucket key")?);
        let mut x = seed | 1;
        let mut seen = BTreeSet::new();
        for _ in 0..n {
            // xorshift64
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if let Some((k, v)) = db.get_greater_than_or_equal_to(&self.txn, &(x % (max + 1)).to_be_bytes())? {
                if seen.insert(k.to_vec()) {
                    add(k, v, &mut out)?;
                }
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// Ways recorded as waiting for the node `node`.
    pub fn pending_ways(&self, node: Id) -> Result<Vec<Id>> {
        let mut out = Vec::new();
        for item in self.db.pending.prefix_iter(&self.txn, &node.to_be_bytes()[..])? {
            let (k, _) = item?;
            out.extend(k.get(8..16).and_then(|b| b.try_into().ok()).map(u64::from_be_bytes));
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
                Some(v) => codec::decode_id_bucket(v).with_context(|| format!("decoding the {kind:?} id bucket {bucket}"))?,
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
            Some(v) => codec::decode_member_bucket(v).with_context(|| format!("decoding the relation member bucket {mtype}/{bucket}"))?.into_iter().collect(),
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
            out.extend(k.get(8..16).and_then(|b| b.try_into().ok()).map(u64::from_be_bytes));
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
                Some(v) => codec::decode_id_bucket(v).with_context(|| format!("decoding the {kind:?} id bucket {bucket}"))?.into_iter().collect(),
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
                Some(v) => codec::decode_member_bucket(v).with_context(|| format!("decoding the relation member bucket {mtype}/{bucket}"))?.into_iter().collect(),
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
                Some((k, v)) if own(k) => (Some(k.to_vec()), codec::decode_tag_entry(Cell::from_bytes(&k[p.len()..]), v).with_context(|| format!("decoding the {kind:?} tag index entry {}", String::from_utf8_lossy(k)))?.into_iter().collect()),
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
        let n: u64 = self.db.info.get(&self.txn, COMMITS.as_bytes())?.and_then(|v| std::str::from_utf8(v).ok()?.parse().ok()).unwrap_or(0);
        self.db.info.put(&mut self.txn, COMMITS.as_bytes(), (n + 1).to_string().as_bytes())?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The chunk lookup as it was before `walk_ranges`: two searches per range.
    fn chunks_in_ranges_plain<'t>(db: Database<Bytes, Bytes>, txn: &'t RoTxn, ranges: &[CodeRange]) -> Vec<&'t [u8]> {
        let mut out = Vec::new();
        let mut last: Option<ChunkKey> = None;
        for r in ranges {
            let lo = chunk_key(Cell { level: r.level, code: r.lo }, 0);
            let hi = chunk_key(Cell { level: r.level, code: r.hi }, u64::MAX);
            if last.is_some_and(|l| l >= hi) {
                continue;
            }
            let start = match db.get_lower_than_or_equal_to(txn, &lo).unwrap() {
                Some((k, _)) => to_chunk_key(k).unwrap(),
                None => lo,
            };
            let prev = last;
            let lower = match &prev {
                Some(l) if *l >= start => Bound::Excluded(&l[..]),
                _ => Bound::Included(&start[..]),
            };
            for item in db.range(txn, &(lower, Bound::Included(&hi[..]))).unwrap() {
                let (k, v) = item.unwrap();
                out.push(v);
                last = Some(to_chunk_key(k).unwrap());
            }
        }
        out
    }

    /// Region scans and tag index scans over cell ranges (`walk_ranges`)
    /// give what a search per range gave (chunks) and what a scan of the
    /// whole value gives for the cells in the ranges (tag index), for few
    /// and many ranges, of small and large rectangles, on nodes (one
    /// level) and ways (all levels).
    #[test]
    fn range_walks_match_plain_scans() {
        use crate::geo::Rect;
        let dir = std::env::temp_dir().join(format!("store-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        // nodes in clusters over the world, a tag on most; ways of two
        // nodes, short and long (stored at different levels)
        let mut osm = String::from("<osm version=\"0.6\">\n");
        let n = 120_000u64;
        let mut pos = Vec::new();
        let (mut clat, mut clon) = (0.0, 0.0);
        for i in 0..n {
            if i % 400 == 0 {
                (clat, clon) = (-70.0 + rnd(14_000) as f64 / 100.0, -175.0 + rnd(35_000) as f64 / 100.0);
            }
            let (lat, lon) = (clat + rnd(2000) as f64 / 10_000.0, clon + rnd(2000) as f64 / 10_000.0);
            pos.push((lat, lon));
            let v = match rnd(60) {
                0..20 => "a".to_string(),
                20 => "b".to_string(),
                21..30 => format!("u{}", rnd(5000)),
                _ => String::new(),
            };
            let v = if i % 40_000 == 7 { "rare".to_string() } else { v };
            let tag = if v.is_empty() { String::new() } else { format!("<tag k=\"k\" v=\"{v}\"/>") };
            osm += &format!("<node id=\"{}\" lat=\"{lat:.5}\" lon=\"{lon:.5}\">{tag}</node>\n", i + 1);
        }
        for w in 0..30_000u64 {
            let a = rnd(n);
            let b = match rnd(10) {
                0 => rnd(n),
                1..4 => (a + 400 * rnd(30)) % n,
                _ => (a + 1 + rnd(300)) % n,
            };
            let v = ["a", "b", "rare", "c"][[0, 0, 0, 0, 1, 3, 3, 3][rnd(8) as usize] + (w % 9000 == 5) as usize * 2 % 4];
            osm += &format!("<way id=\"{}\"><nd ref=\"{}\"/><nd ref=\"{}\"/><tag k=\"k\" v=\"{v}\"/></way>\n", w + 1, a + 1, b + 1);
        }
        let file = dir.join("in.osm");
        std::fs::write(&file, osm + "</osm>\n").unwrap();
        let db = Db::open(&dir.join("db")).unwrap();
        crate::import::import_file(&db, &file, &mut Default::default()).unwrap();
        let snap = db.snapshot().unwrap();
        assert!(snap.chunk_count(Kind::Node).unwrap() >= 40);
        let (mut chunks_seen, mut cells_seen) = (0, 0);
        for (count, size) in [(1, 0.01), (1, 30.0), (3, 0.5), (40, 0.02), (40, 3.0), (500, 0.001), (500, 0.3), (3000, 0.05), (3000, 0.0002), (20_000, 0.01)] {
            for round in 0..3 {
                // around nodes (hits) and anywhere (misses)
                let rects: Vec<Rect> = (0..count)
                    .map(|_| {
                        let (lat, lon) = if rnd(4) > 0 || round == 2 { pos[rnd(n) as usize] } else { (-80.0 + rnd(1600) as f64 / 10.0, -179.0 + rnd(3500) as f64 / 10.0) };
                        let p0 = crate::geo::Pt::from_ll((lat - size / 2.0).max(-89.0), (lon - size / 2.0).max(-179.9));
                        let p1 = crate::geo::Pt::from_ll((lat + size / 2.0).min(89.0), (lon + size / 2.0).min(179.9));
                        Rect { y0: p0.y, x0: p0.x, y1: p1.y, x1: p1.x }
                    })
                    .collect();
                for kind in [Kind::Node, Kind::Way] {
                    let ranges = crate::exec::fetch::ranges_for(kind, &rects);
                    let got = chunks_in_ranges(db.k(kind).data, &snap.txn, &ranges).unwrap();
                    let want = chunks_in_ranges_plain(db.k(kind).data, &snap.txn, &ranges);
                    assert_eq!(got.iter().map(|v| v.as_ptr()).collect::<Vec<_>>(), want.iter().map(|v| v.as_ptr()).collect::<Vec<_>>(), "{kind:?} chunks of {count} rectangles of {size}");
                    chunks_seen += got.len();
                    for value in [Some("a"), Some("b"), Some("rare"), Some("c"), Some("none"), None] {
                        let mut got: Vec<(Vec<u8>, Cell, Vec<Id>)> = Vec::new();
                        snap.scan_tags(kind, "k", value, Some(&ranges), |val, cell, ids| {
                            got.push((val.to_vec(), cell, ids.to_vec()));
                            Ok(Scan::Next)
                        })
                        .unwrap();
                        let mut want: Vec<(Vec<u8>, Cell, Vec<Id>)> = Vec::new();
                        snap.scan_tags(kind, "k", value, None, |val, cell, ids| {
                            if cell_in_ranges(cell, &ranges) {
                                want.push((val.to_vec(), cell, ids.to_vec()));
                            }
                            Ok(Scan::Next)
                        })
                        .unwrap();
                        assert_eq!(got, want, "{kind:?} k={value:?} in {count} rectangles of {size}");
                        cells_seen += got.len();
                    }
                }
            }
        }
        assert!(chunks_seen > 1000 && cells_seen > 10_000, "{chunks_seen} chunks, {cells_seen} cells");
        drop(snap);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `cells_of` finds what a search and a full decode of each bucket
    /// find: ids in neighbouring buckets (cursor steps), far apart (new
    /// searches), in buckets and at ids that do not exist, few (picked on
    /// the calling thread) and many (on the pool).
    #[test]
    fn cells_of_matches_a_plain_lookup() {
        // long sections on the pool
        let _one = crate::exec::tests::pool_test();
        let dir = std::env::temp_dir().join(format!("store-cells-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // node ids in stretches with gaps of whole buckets between them
        let mut ids: Vec<Id> = Vec::new();
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut id = 1u64;
        while ids.len() < 400_000 {
            let run = 1 + rnd(3000);
            for _ in 0..run {
                id += 1 + rnd(3) * rnd(2);
                ids.push(id);
            }
            id += rnd(6) * 512 * rnd(2) + rnd(40_000) * (rnd(8) == 0) as u64;
        }
        let mut osm = String::from("<osm version=\"0.6\">\n");
        for (i, id) in ids.iter().enumerate() {
            osm += &format!("<node id=\"{id}\" lat=\"{:.4}\" lon=\"{:.4}\"/>\n", -80.0 + (i % 1600) as f64 * 0.1, -170.0 + (i / 1600) as f64 * 0.1 + rnd(5) as f64);
        }
        let file = dir.join("in.osm");
        std::fs::write(&file, osm + "</osm>\n").unwrap();
        let db = Db::open(&dir.join("db")).unwrap();
        crate::import::import_file(&db, &file, &mut Default::default()).unwrap();
        let snap = db.snapshot().unwrap();
        let plain = |ids: &[Id]| -> Vec<(Id, Cell)> {
            let mut sorted = ids.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            let mut out = Vec::new();
            for id in sorted {
                if let Some(v) = db.k(Kind::Node).ids.get(&snap.txn, &(id >> codec::ID_BUCKET_BITS).to_be_bytes()).unwrap() {
                    let lo = (id & ((1 << codec::ID_BUCKET_BITS) - 1)) as u16;
                    out.extend(codec::decode_id_bucket(v).unwrap().into_iter().filter(|e| e.0 == lo).map(|e| (id, e.1)));
                }
            }
            out
        };
        let last = *ids.last().unwrap();
        let mut found_some = 0;
        for (n, spread) in [(1, last), (10, last), (300, 5000), (300, last), (5000, 600_000), (20_000, last), (150_000, last), (700_000, last + 3_000_000)] {
            for _ in 0..3 {
                let from = rnd(last);
                let want: Vec<Id> = (0..n).map(|_| from + rnd(spread)).collect();
                let got = snap.cells_of(Kind::Node, &want).unwrap();
                assert_eq!(got, plain(&want), "{n} ids over {spread}");
                found_some += got.len();
            }
        }
        // every id, and every second one
        assert_eq!(snap.cells_of(Kind::Node, &ids).unwrap().len(), ids.len());
        let half: Vec<Id> = ids.iter().step_by(2).copied().collect();
        assert_eq!(snap.cells_of(Kind::Node, &half).unwrap(), plain(&half));
        assert!(found_some > 100_000, "{found_some}");
        drop(snap);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review (rv-area): `walk_ranges` against a plain model on small
    /// random tables, with entries at the ends of the ranges, ranges at
    /// every level (level 0, `hi` = u32::MAX), merged ranges and ranges
    /// that overlap (sorted by level and start only, as `is_in` makes
    /// them), with and without `after`, for chunk keys and tag keys (other
    /// values around the one read).
    #[test]
    fn review_walk_ranges_model() {
        let dir = std::env::temp_dir().join(format!("store-rvwalk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("db")).unwrap();
        let mut seed = 0x0DDB_1A5E_5BAD_5EEDu64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n.max(1)
        };
        let (data, tags) = (db.k(Kind::Node).data, db.k(Kind::Node).tags);
        let p: Vec<u8> = b"k\0v\0".to_vec();
        let (mut visits, mut cases) = (0usize, 0usize);
        for round in 0..3000 {
            // ranges: a few levels, codes from a small pool so that they
            // and the entries meet at their ends
            let pool: Vec<u32> = (0..2 + rnd(12)).map(|_| match rnd(6) {
                0 => 0,
                1 => u32::MAX,
                2 => u32::MAX - rnd(3) as u32,
                _ => rnd(1 << 12) as u32 * 4096 + rnd(3) as u32,
            }).collect();
            let levels: Vec<u8> = (0..1 + rnd(3)).map(|_| if rnd(5) == 0 { 0 } else { [3u8, 9, 15, 16][rnd(4) as usize] }).collect();
            let mut raw: Vec<CodeRange> = Vec::new();
            for _ in 0..rnd(14) {
                let level = levels[rnd(levels.len() as u64) as usize];
                let (a, b) = (pool[rnd(pool.len() as u64) as usize], pool[rnd(pool.len() as u64) as usize]);
                let (lo, hi) = (a.min(b), a.max(b).saturating_add(rnd(3) as u32));
                raw.push(if level == 0 { CodeRange { level, lo: 0, hi: 0 } } else { CodeRange { level, lo, hi } });
            }
            let ranges = if round % 2 == 0 {
                crate::geo::merge_ranges(raw)
            } else {
                // as `is_in`: sorted by level and start, exact copies removed
                raw.sort_by_key(|r| (r.level, r.lo));
                raw.dedup();
                raw
            };
            // entries: cells at and next to the ends of the ranges and of the pool
            let mut cells: Vec<Cell> = Vec::new();
            for _ in 0..rnd(40) {
                let level = if rnd(4) == 0 { [0u8, 3, 9, 15, 16, 200][rnd(6) as usize] } else { levels[rnd(levels.len() as u64) as usize] };
                let base = if !ranges.is_empty() && rnd(3) > 0 {
                    let r = ranges[rnd(ranges.len() as u64) as usize];
                    [r.lo, r.hi][rnd(2) as usize]
                } else {
                    pool[rnd(pool.len() as u64) as usize]
                };
                let code = match rnd(3) {
                    0 => base.wrapping_sub(1),
                    1 => base,
                    _ => base.wrapping_add(1),
                };
                cells.push(Cell { level, code: if level == 0 { 0 } else { code } });
            }
            let ids = [0u64, 1, 77, u64::MAX - 1, u64::MAX];
            let mut chunk_keys: Vec<Vec<u8>> = cells.iter().map(|c| chunk_key(*c, ids[rnd(5) as usize]).to_vec()).collect();
            chunk_keys.sort();
            chunk_keys.dedup();
            let mut tag_keys: Vec<Vec<u8>> = cells.iter().map(|c| [&p[..], &c.to_bytes()].concat()).collect();
            tag_keys.sort();
            tag_keys.dedup();
            {
                let mut w = db.env.write_txn().unwrap();
                data.clear(&mut w).unwrap();
                tags.clear(&mut w).unwrap();
                for k in &chunk_keys {
                    data.put(&mut w, k, k).unwrap();
                }
                for k in &tag_keys {
                    tags.put(&mut w, k, k).unwrap();
                }
                // other values before and after the one read
                for (q, n) in [(&b"k\0u\0"[..], 3), (&b"k\0v\0\x01"[..], 0), (&b"k\0w\0"[..], 3), (&b"k\0"[..], 0)] {
                    for i in 0..n {
                        let k = [q, &Cell { level: 16, code: i * 1000 }.to_bytes()].concat();
                        tags.put(&mut w, &k, &k).unwrap();
                    }
                }
                w.commit().unwrap();
            }
            let txn = db.env.read_txn().unwrap();
            // model: an entry is visited when it is after `after`, at or
            // before the high key of a range, and at or after its low key
            // or the last entry at or before it
            let model = |keys: &[Vec<u8>], lo_of: &dyn Fn(&CodeRange) -> Vec<u8>, hi_of: &dyn Fn(&CodeRange) -> Vec<u8>, after: Option<&[u8]>| -> Vec<Vec<u8>> {
                let mut out = Vec::new();
                for (i, k) in keys.iter().enumerate() {
                    if after.is_some_and(|a| k.as_slice() <= a) {
                        continue;
                    }
                    let next = keys.get(i + 1);
                    if ranges.iter().any(|r| *k <= hi_of(r) && (*k >= lo_of(r) || next.is_none_or(|n| *n > lo_of(r)))) {
                        out.push(k.clone());
                    }
                }
                out
            };
            // chunks
            let lo_c = |r: &CodeRange| chunk_key(Cell { level: r.level, code: r.lo }, 0).to_vec();
            let hi_c = |r: &CodeRange| chunk_key(Cell { level: r.level, code: r.hi }, u64::MAX).to_vec();
            let want = model(&chunk_keys, &lo_c, &hi_c, None);
            let got: Vec<Vec<u8>> = chunks_in_ranges(data, &txn, &ranges).unwrap().into_iter().map(|v| v.to_vec()).collect();
            assert_eq!(got, want, "round {round} chunks, ranges {ranges:?}, keys {chunk_keys:?}");
            if round % 2 == 0 {
                // the function before the patch, where the ranges do not overlap
                let old: Vec<Vec<u8>> = chunks_in_ranges_plain(data, &txn, &ranges).into_iter().map(|v| v.to_vec()).collect();
                assert_eq!(got, old, "round {round} chunks (old)");
            }
            visits += got.len();
            // tag entries of the value, with and without `after`
            let lo_t = |r: &CodeRange| [&p[..], &Cell { level: r.level, code: r.lo }.to_bytes()].concat();
            let hi_t = |r: &CodeRange| [&p[..], &Cell { level: r.level, code: r.hi }.to_bytes()].concat();
            let past = successor(&p);
            let mut afters: Vec<Option<Vec<u8>>> = vec![None];
            afters.extend(tag_keys.iter().take(3).cloned().map(Some));
            if let Some(k) = tag_keys.get(tag_keys.len() / 2) {
                afters.push(Some(k.clone()));
            }
            for after in afters {
                let want = model(&tag_keys, &lo_t, &hi_t, after.as_deref());
                let mut got: Vec<Vec<u8>> = Vec::new();
                let key = |r: &CodeRange, high: bool, k: &mut Vec<u8>| {
                    k.clear();
                    k.extend_from_slice(&p);
                    k.extend_from_slice(&Cell { level: r.level, code: if high { r.hi } else { r.lo } }.to_bytes());
                };
                let whole = |k: &[u8]| k.len() == p.len() + 5;
                let done = walk_ranges(tags, &txn, Bound::Excluded(&past[..]), after.as_deref(), &ranges, p.len(), key, |k| k.starts_with(&p) && whole(k), |k, _| {
                    got.push(k.to_vec());
                    Ok(true)
                })
                .unwrap();
                assert!(done);
                assert_eq!(got, want, "round {round} tags after {after:?}, ranges {ranges:?}, keys {tag_keys:?}");
                visits += got.len();
                cases += 1;
            }
        }
        assert!(visits > 10_000 && cases > 3000, "{visits} visits, {cases} cases");
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn midpoints_lie_between() {
        let cases: [(&[u8], &[u8]); 7] = [
            (b"key\0", b"key\x01"),
            (b"key\0http://a\0", b"key\0www\0"),
            (b"key\0https://www.a", b"key\0https://www.b"),
            (b"key\0abc\0", b"key\0abc\0\x05"),
            (b"a", b"a\0\0\0\0\0\0\0\0\x01"),
            (b"a\xff\xff\xff\xff\xff\xff\xff\xff", b"b"),
            (b"", b"\x01"),
        ];
        for (a, b) in cases {
            if let Some(m) = midpoint(a, b) {
                assert!(a < m.as_slice() && m.as_slice() < b, "{a:?} {m:?} {b:?}");
                assert!(split_width(a, b).is_some_and(|w| w > 0.0));
            } else {
                assert!(split_width(a, b).is_none());
            }
        }
        assert!(midpoint(b"key\0http://a\0", b"key\0www\0").is_some());
        assert_eq!(midpoint(b"b", b"a"), None);
        assert_eq!(midpoint(b"a", b"a"), None);
        // repeated halving reaches every gap of 2 or more
        let (a, mut b) = (b"key\0abc".to_vec(), b"key\0abd".to_vec());
        for _ in 0..200 {
            match midpoint(&a, &b) {
                Some(m) => b = m,
                None => break,
            }
        }
        assert!(a < b && midpoint(&a, &b).is_none());
        assert_eq!(successor(b"key\0ab"), b"key\0ac");
        assert_eq!(successor(b"key\0a\xff"), b"key\0b");
        assert_eq!(successor(b"key\0"), b"key\x01");
    }
}
