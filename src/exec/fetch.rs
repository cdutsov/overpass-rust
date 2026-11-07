//! Element retrieval on top of a database snapshot, with per-query caches.
//!
//! Chunk values are collected from LMDB sequentially (cursor operations are
//! cheap pointer walks over the memory map) and decoded in parallel.

use super::filter::TagFilter;
use crate::geo::{Cell, CodeRange, Pt, Rect, level_ranges, loose_ranges, merge_ranges, tile_ranges, MAX_LEVEL};
use crate::model::*;
use crate::store::codec::{self, Pick};
use crate::store::{Kind, Snapshot, Scan};
use crate::update::Stored;
use anyhow::Result;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;

/// Chunks below this count are decoded on the calling thread.
const PAR_THRESHOLD: usize = 8;

pub struct Fetch<'s> {
    pub snap: &'s Snapshot<'s>,
    pos_cache: RefCell<FxHashMap<Id, Option<Pt>>>,
    user_cache: RefCell<FxHashMap<u32, Option<String>>>,
    pub limits: Limits,
}

/// Error raised when a query exceeds its time budget or is cancelled; the
/// engine turns it into the C++ message naming the current statement.
#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("query interrupted")
    }
}
impl std::error::Error for Interrupted {}

/// Error raised when the elements decoded by the running statement, added to
/// the stored sets, exceed the query's memory limits. C++ checks its set
/// sizes while a statement collects data (`Resource_Manager::health_check`)
/// and runs under an address-space limit; without this check one statement
/// could decode the whole database before the size check at its end.
#[derive(Debug)]
pub struct OutOfMemory {
    /// The C++ counted size (`maxsize`) was exceeded, not the memory estimate.
    pub counted: bool,
    pub counted_size: u64,
}

impl std::fmt::Display for OutOfMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("query out of memory")
    }
}
impl std::error::Error for OutOfMemory {}

/// Memory accounting of one query, shared by the decoding threads. The
/// engine sets the base (the stored sets) after every statement; decoding
/// adds what it produces.
#[derive(Default)]
pub struct MemBudget {
    pub counted: std::sync::atomic::AtomicU64,
    pub bytes: std::sync::atomic::AtomicU64,
    pub max_counted: std::sync::atomic::AtomicU64,
    pub max_bytes: std::sync::atomic::AtomicU64,
}

impl MemBudget {
    pub fn new(max_counted: u64, max_bytes: u64) -> MemBudget {
        let b = MemBudget::default();
        b.max_counted.store(max_counted, std::sync::atomic::Ordering::Relaxed);
        b.max_bytes.store(max_bytes, std::sync::atomic::Ordering::Relaxed);
        b
    }

    /// Reset the base to the size of the stored sets.
    pub fn reset(&self, counted: u64, bytes: u64) {
        self.counted.store(counted, std::sync::atomic::Ordering::Relaxed);
        self.bytes.store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Account for newly decoded elements; fails when the memory estimate
    /// passes its limit. The counted size (`maxsize`) is only tracked here:
    /// decoded candidates may still fail later filters, and C++ counts the
    /// filtered set, so the engine checks `maxsize` when the statement ends.
    pub fn add(&self, counted: u64, bytes: u64) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        let c = self.counted.fetch_add(counted, Relaxed) + counted;
        let b = self.bytes.fetch_add(bytes, Relaxed) + bytes;
        if b > self.max_bytes.load(Relaxed) {
            return Err(OutOfMemory { counted: false, counted_size: c }.into());
        }
        Ok(())
    }
}

/// Size of a decoded element for the memory limits: the C++ counted size
/// (without the per-index-bucket share) and an estimate of the heap bytes.
pub trait MemSize {
    fn mem_size(&self) -> (u64, u64);
}

pub fn tags_bytes(t: &Tags) -> u64 {
    t.iter().map(|(k, v)| (k.len() + v.len() + 48) as u64).sum()
}

impl MemSize for Node {
    fn mem_size(&self) -> (u64, u64) {
        (8, std::mem::size_of::<Node>() as u64 + tags_bytes(&self.tags))
    }
}

impl MemSize for Way {
    fn mem_size(&self) -> (u64, u64) {
        (128, std::mem::size_of::<Way>() as u64 + 8 * self.nodes.len() as u64 + tags_bytes(&self.tags))
    }
}

impl MemSize for Area {
    fn mem_size(&self) -> (u64, u64) {
        (128, std::mem::size_of::<Area>() as u64 + 4 * self.used.len() as u64 + tags_bytes(&self.tags))
    }
}

impl MemSize for Relation {
    fn mem_size(&self) -> (u64, u64) {
        (192, std::mem::size_of::<Relation>() as u64 + self.members.iter().map(|m| std::mem::size_of::<Member>() as u64 + m.role.len() as u64).sum::<u64>() + tags_bytes(&self.tags))
    }
}

/// Deadline, cancellation flag and memory budget, checked inside long loops.
#[derive(Clone, Default)]
pub struct Limits {
    pub deadline: Option<std::time::Instant>,
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub mem: Option<std::sync::Arc<MemBudget>>,
}

impl Limits {
    #[inline]
    pub fn expired(&self) -> bool {
        self.deadline.is_some_and(|d| std::time::Instant::now() > d) || self.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn check(&self) -> Result<()> {
        if self.expired() { Err(Interrupted.into()) } else { Ok(()) }
    }

    /// Account for decoded elements against the memory budget.
    pub fn account<T: MemSize>(&self, elems: &[T]) -> Result<()> {
        let Some(m) = &self.mem else { return Ok(()) };
        let (mut c, mut b) = (0u64, 0u64);
        for e in elems {
            let (ec, eb) = e.mem_size();
            c += ec;
            b += eb;
        }
        m.add(c, b)
    }
}

thread_local! {
    /// Limits of the query running on this thread, for code without access
    /// to the engine (geometry algorithms).
    static THREAD_LIMITS: std::cell::RefCell<Limits> = std::cell::RefCell::new(Limits::default());
}

/// Install the limits of the query that runs on this thread.
pub fn set_thread_limits(l: Limits) {
    THREAD_LIMITS.with(|t| *t.borrow_mut() = l);
}

/// Whether the query running on this thread is out of time or cancelled.
/// Long computations stop early; the engine reports the timeout afterwards.
pub fn thread_expired() -> bool {
    THREAD_LIMITS.with(|t| t.borrow().expired())
}

/// Entries kept in the node position cache before it is reset.
const POS_CACHE_LIMIT: usize = 8_000_000;

pub use crate::geo::cell_in_ranges;


/// Cell ranges where elements of a kind intersecting the rectangles may be stored.
pub fn ranges_for(kind: Kind, rects: &[Rect]) -> Vec<CodeRange> {
    let runs;
    let rects = if rects.len() > 32 {
        runs = tile_runs(rects);
        &runs[..]
    } else {
        rects
    };
    let mut r = Vec::new();
    for rect in rects {
        match kind {
            Kind::Node => r.extend(tile_ranges(rect)),
            _ => r.extend(loose_ranges(rect)),
        }
    }
    merge_ranges(r)
}

/// The rectangles with the small ones (up to 64 level-16 tiles) replaced
/// by the tiles they touch, adjacent tiles of a row joined (up to 128):
/// the same cells at every level, since a cell holds whole tiles, from
/// far fewer rectangles when many share tiles (an `around` source of many
/// points and segments). Larger rectangles are kept, and all of them when
/// the small ones touch more than 2^20 tiles.
pub fn tile_runs(rects: &[Rect]) -> Vec<Rect> {
    const SMALL: u64 = 64;
    const MAX_TILES: u64 = 1 << 20;
    const MAX_RUN: u32 = 128;
    let rects: Vec<&Rect> = rects.iter().filter(|r| !r.is_empty()).collect();
    let tiles_of = |r: &Rect| ((r.y1 >> 16) - (r.y0 >> 16) + 1) as u64 * ((r.x1 >> 16) - (r.x0 >> 16) + 1) as u64;
    if rects.iter().map(|r| tiles_of(r)).filter(|n| *n <= SMALL).sum::<u64>() > MAX_TILES {
        return rects.into_iter().copied().collect();
    }
    let mut out = Vec::new();
    let mut tiles: Vec<(u32, u32)> = Vec::new();
    for r in rects {
        if tiles_of(r) > SMALL {
            out.push(*r);
            continue;
        }
        for ty in r.y0 >> 16..=r.y1 >> 16 {
            tiles.extend((r.x0 >> 16..=r.x1 >> 16).map(|tx| (ty, tx)));
        }
    }
    tiles.sort_unstable();
    tiles.dedup();
    let mut i = 0;
    while i < tiles.len() {
        let (ty, tx0) = tiles[i];
        let mut tx1 = tx0;
        i += 1;
        while i < tiles.len() && tiles[i] == (ty, tx1 + 1) && tx1 - tx0 + 1 < MAX_RUN {
            tx1 += 1;
            i += 1;
        }
        out.push(Rect { y0: ty << 16, x0: tx0 << 16, y1: (ty << 16) | 0xffff, x1: (tx1 << 16) | 0xffff });
    }
    out
}

/// Bytes of chunks requested from the kernel ahead of decoding at a time.
/// At most two batches are ahead: pages requested much earlier than they
/// are decoded could be evicted again in a container short of memory.
const PREFETCH_BATCH: usize = 64 << 20;

/// Asks the kernel to read the pages of these chunks in the background
/// (madvise WILLNEED). The database is mapped without read-ahead, so a
/// chunk that is not in the page cache costs one synchronous 4 KiB read per
/// page when it is decoded; a query decoding chunks on one thread (busy
/// server) then waits for each read in turn. With the hint the reads of a
/// batch overlap. Chunks less than 64 KiB apart are joined into one call;
/// pages that are cached cost a page cache lookup.
fn prefetch<'a>(slices: impl Iterator<Item = &'a [u8]>) {
    unsafe extern "C" {
        fn madvise(addr: *mut std::ffi::c_void, len: usize, advice: i32) -> i32;
    }
    const MADV_WILLNEED: i32 = 3;
    const PAGE: usize = 4096;
    const GAP: usize = 16 * PAGE;
    let mut spans: Vec<(usize, usize)> = slices.filter(|s| s.len() >= PAGE).map(|s| (s.as_ptr() as usize & !(PAGE - 1), s.as_ptr() as usize + s.len())).collect();
    if spans.is_empty() {
        return;
    }
    spans.sort_unstable();
    // The hint only starts reads: a failure changes nothing but the speed.
    let flush = |(a, b): (usize, usize)| unsafe {
        madvise(a as *mut std::ffi::c_void, b - a, MADV_WILLNEED);
    };
    let mut cur = spans[0];
    for &(a, b) in &spans[1..] {
        if a <= cur.1 + GAP {
            cur.1 = cur.1.max(b);
        } else {
            flush(cur);
            cur = (a, b);
        }
    }
    flush(cur);
}

/// `par_flat_map` over database chunks in batches of about PREFETCH_BATCH
/// bytes: the pages of the next batch are requested while the current one
/// is decoded.
fn decode_chunks<I: Sync, T: Send + MemSize>(limits: &Limits, items: &[I], bytes: impl Fn(&I) -> &[u8], f: impl Fn(&I) -> Result<Vec<T>> + Sync + Send) -> Result<Vec<T>> {
    let mut batches: Vec<&[I]> = Vec::new();
    let (mut start, mut size) = (0, 0);
    for (i, it) in items.iter().enumerate() {
        size += bytes(it).len();
        if size >= PREFETCH_BATCH {
            batches.push(&items[start..=i]);
            (start, size) = (i + 1, 0);
        }
    }
    if start < items.len() {
        batches.push(&items[start..]);
    }
    let Some(first) = batches.first() else { return Ok(Vec::new()) };
    prefetch(first.iter().map(&bytes));
    if batches.len() == 1 {
        return par_flat_map(limits, items, f);
    }
    let mut out = Vec::new();
    for (k, batch) in batches.iter().enumerate() {
        if let Some(next) = batches.get(k + 1) {
            prefetch(next.iter().map(&bytes));
        }
        out.extend(par_flat_map(limits, batch, &f)?);
    }
    Ok(out)
}

/// Decode work items, in parallel when there are many. Stops early when the
/// query runs out of time.
fn par_flat_map<I: Sync, T: Send + MemSize>(limits: &Limits, items: &[I], f: impl Fn(&I) -> Result<Vec<T>> + Sync + Send) -> Result<Vec<T>> {
    if items.len() < PAR_THRESHOLD || !super::parallel_ok() {
        let mut out = Vec::new();
        for (i, it) in items.iter().enumerate() {
            if i % 64 == 63 {
                limits.check()?;
            }
            let es = f(it)?;
            limits.account(&es)?;
            out.extend(es);
        }
        return Ok(out);
    }
    let parts = items.par_iter().map(|it| {
        limits.check()?;
        let es = f(it)?;
        limits.account(&es)?;
        Ok(es)
    }).collect::<Result<Vec<Vec<T>>>>()?;
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
    for p in parts {
        out.extend(p);
    }
    Ok(out)
}

impl<'s> Fetch<'s> {
    pub fn new(snap: &'s Snapshot<'s>) -> Fetch<'s> {
        Fetch { snap, pos_cache: RefCell::new(FxHashMap::default()), user_cache: RefCell::new(FxHashMap::default()), limits: Limits::default() }
    }

    /// Elements with the given ids, sorted by id.
    pub fn by_ids<T: Stored>(&self, ids: &[Id]) -> Result<Vec<T>> {
        let chunks = self.snap.chunks_with_ids(T::KIND, ids)?;
        let mut out = decode_chunks(&self.limits, &chunks, |(v, _)| *v, |(v, want)| T::decode(v, Pick::Ids(want)))?;
        out.sort_unstable_by_key(|e| e.id());
        out.dedup_by_key(|e| e.id());
        Ok(out)
    }

    /// Ways with the given ids, without their tags (for geometry), sorted by id.
    pub fn ways_untagged(&self, ids: &[Id]) -> Result<Vec<Way>> {
        let chunks = self.snap.chunks_with_ids(Kind::Way, ids)?;
        let mut out = decode_chunks(&self.limits, &chunks, |(v, _)| *v, |(v, want)| codec::decode_ways(v, Pick::Ids(want), false))?;
        out.sort_unstable_by_key(|w| w.id);
        Ok(out)
    }

    /// All elements stored in the given cell ranges that pass `keep`.
    pub fn in_ranges<T: Stored>(&self, ranges: &[CodeRange], keep: impl Fn(&T) -> bool + Sync + Send) -> Result<Vec<T>> {
        let chunks = self.snap.chunks_in_ranges(T::KIND, ranges)?;
        let mut out = decode_chunks(&self.limits, &chunks, |v| *v, |v| {
            let mut es = T::decode(v, Pick::Cells(ranges))?;
            es.retain(|e| keep(e));
            // retain keeps the capacity of the whole chunk; without this a
            // scan of many chunks holds memory for every element it decoded
            es.shrink_to_fit();
            Ok(es)
        })?;
        out.sort_unstable_by_key(|e| e.id());
        out.dedup_by_key(|e| e.id());
        Ok(out)
    }

    /// Every element of a kind (used only for queries without any index).
    pub fn all<T: Stored>(&self, keep: impl Fn(&T) -> bool + Sync + Send) -> Result<Vec<T>> {
        let chunks = self.snap.all_chunks(T::KIND)?;
        let mut out = par_flat_map(&self.limits, &chunks, |v| {
            let mut es = T::decode(v, Pick::All)?;
            es.retain(|e| keep(e));
            // retain keeps the capacity of the whole chunk; without this a
            // scan of many chunks holds memory for every element it decoded
            es.shrink_to_fit();
            Ok(es)
        })?;
        out.sort_unstable_by_key(|e| e.id());
        Ok(out)
    }

    /// Elements at known (cell, id) positions.
    pub fn at<T: Stored>(&self, pairs: Vec<(Cell, Id)>) -> Result<Vec<T>> {
        // In batches, so that a query with millions of candidates (a common
        // key over a country) sees its deadline while they are grouped by
        // chunk; a chunk on a batch border is read twice for disjoint ids.
        const BATCH: usize = 1 << 20;
        let mut out = Vec::new();
        for part in pairs.chunks(BATCH) {
            self.limits.check()?;
            let work = self.snap.chunks_at(T::KIND, part.to_vec())?;
            out.extend(decode_chunks(&self.limits, &work, |(v, _)| *v, |(v, want)| T::decode(v, Pick::Ids(want)))?);
        }
        out.sort_unstable_by_key(|e| e.id());
        out.dedup_by_key(|e| e.id());
        Ok(out)
    }

    /// Candidate (cell, id) pairs from the tag index. `ranges` optionally
    /// restricts the cells. Entries with truncated keys or values are always
    /// included; callers verify candidates against the element.
    pub fn tag_candidates(&self, kind: Kind, f: &TagFilter, ranges: Option<&[CodeRange]>) -> Result<Vec<(Cell, Id)>> {
        let mut out = Vec::new();
        let mut n = 0u32;
        self.scan_candidates(kind, f, ranges, |cell, ids| {
            n = n.wrapping_add(1);
            if n % 4096 == 0 {
                self.limits.check()?;
            }
            out.extend(ids.iter().map(|id| (cell, *id)));
            Ok(Scan::Next)
        })?;
        // sorting millions of candidates takes a second or more
        self.limits.check()?;
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Whether the cell ranges hold more than `share` of the chunks of a
    /// kind, so that reading a tag index without ranges is cheaper than
    /// decoding every chunk in them.
    pub fn ranges_are_large(&self, kind: Kind, ranges: &[CodeRange], share: f64) -> Result<bool> {
        let total = self.snap.chunk_count(kind)?;
        if total == 0 {
            return Ok(false);
        }
        let limit = (total as f64 * share).ceil() as usize;
        Ok(self.snap.chunks_in_ranges(kind, ranges)?.len() > limit)
    }

    /// Candidates from the tag index without ranges, kept when their cell
    /// lies in `ranges` (a filter, not a seek per range and value).
    pub fn tag_candidates_in(&self, kind: Kind, f: &TagFilter, ranges: &[CodeRange]) -> Result<Vec<(Cell, Id)>> {
        let mut out = Vec::new();
        let mut n = 0u32;
        self.scan_candidates(kind, f, None, |cell, ids| {
            n = n.wrapping_add(1);
            if n % 4096 == 0 {
                self.limits.check()?;
            }
            if cell_in_ranges(cell, ranges) {
                out.extend(ids.iter().map(|id| (cell, *id)));
            }
            Ok(Scan::Next)
        })?;
        self.limits.check()?;
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Number of tag index candidates for the filter, counting stops past
    /// `cap` (to compare the selectivity of filters cheaply).
    pub fn tag_count_capped(&self, kind: Kind, f: &TagFilter, ranges: Option<&[CodeRange]>, cap: usize) -> Result<usize> {
        let mut n = 0usize;
        self.scan_candidates(kind, f, ranges, |_, ids| {
            n += ids.len();
            Ok(if n > cap { Scan::Stop } else { Scan::Next })
        })?;
        Ok(n)
    }

    /// Whether the tag index has any candidate for the filter.
    pub fn tag_exists(&self, kind: Kind, f: &TagFilter) -> Result<bool> {
        let mut found = false;
        self.scan_candidates(kind, f, None, |_, _| {
            found = true;
            Ok(Scan::Stop)
        })?;
        Ok(found)
    }

    /// Index entries (cell, encoded ids) that may match a positive filter.
    fn scan_candidates(&self, kind: Kind, f: &TagFilter, ranges: Option<&[CodeRange]>, mut push: impl FnMut(Cell, &[Id]) -> Result<Scan>) -> Result<()> {
        // Value test for regexes: values failing it are skipped as a whole.
        let value_test = |re: &regex::Regex, val: &[u8]| val.contains(&1) || std::str::from_utf8(val).is_ok_and(|s| re.is_match(s));
        match f {
            TagFilter::Eq(k, v) => self.snap.scan_tags(kind, k, Some(v), ranges, |_, cell, ids| push(cell, ids)),
            TagFilter::HasKey(k) => self.snap.scan_tags(kind, k, None, ranges, |_, cell, ids| push(cell, ids)),
            TagFilter::Regex { key, re, neg: false, prefixes } => {
                let mut k = Vec::new();
                crate::store::index_part(key, &mut k);
                self.snap.scan_tags_matching(kind, &k, prefixes.as_deref(), ranges, |val| value_test(re, val), |_, cell, ids| push(cell, ids))?;
                Ok(())
            }
            TagFilter::KeyRegex { key, value, value_prefixes } => {
                for k in self.snap.tag_keys(kind)? {
                    // Truncated or hashed keys cannot be tested here: take all
                    // their entries and verify on the elements.
                    let truncated = k.contains(&1);
                    if !truncated && !std::str::from_utf8(&k).is_ok_and(|ks| key.is_match(ks)) {
                        continue;
                    }
                    let prefixes = if truncated { None } else { value_prefixes.as_deref() };
                    let r = self.snap.scan_tags_matching(kind, &k, prefixes, ranges, |val| truncated || value_test(value, val), |_, cell, ids| push(cell, ids))?;
                    if r == Scan::Stop {
                        break;
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }


    /// Positions of nodes (missing nodes are absent from the map).
    pub fn node_positions(&self, ids: &[Id]) -> Result<FxHashMap<Id, Pt>> {
        let mut out = FxHashMap::with_capacity_and_hasher(ids.len(), Default::default());
        let mut missing = Vec::new();
        {
            let cache = self.pos_cache.borrow();
            for id in ids {
                match cache.get(id) {
                    Some(Some(p)) => {
                        out.insert(*id, *p);
                    }
                    Some(None) => {}
                    None => missing.push(*id),
                }
            }
        }
        if missing.is_empty() {
            return Ok(out);
        }
        missing.sort_unstable();
        missing.dedup();
        let chunks = self.snap.chunks_with_ids(Kind::Node, &missing)?;
        let found = decode_chunks(&self.limits, &chunks, |(v, _)| *v, |(v, want)| codec::decode_nodes(v, Pick::Ids(want), false))?;
        let mut cache = self.pos_cache.borrow_mut();
        if cache.len() > POS_CACHE_LIMIT {
            cache.clear();
        }
        for n in &found {
            cache.insert(n.id, Some(n.pos));
            out.insert(n.id, n.pos);
        }
        for id in missing {
            cache.entry(id).or_insert(None);
        }
        Ok(out)
    }

    pub fn note_positions(&self, nodes: &[Node]) {
        let mut cache = self.pos_cache.borrow_mut();
        for n in nodes {
            cache.insert(n.id, Some(n.pos));
        }
    }

    /// Ways that reference any of the given nodes.
    pub fn ways_with_nodes(&self, nodes: &[(Id, Pt)]) -> Result<Vec<Way>> {
        if nodes.is_empty() {
            return Ok(Vec::new());
        }
        let ids: FxHashSet<Id> = nodes.iter().map(|(id, _)| *id).collect();
        let mut ranges = Vec::new();
        // Group nodes by tile to keep the range list small.
        let tiles: FxHashSet<u32> = nodes.iter().map(|(_, p)| crate::geo::tile_of(*p)).collect();
        for t in tiles {
            let (ty, tx) = crate::geo::unmorton(t);
            let r = Rect { y0: ty << 16, x0: tx << 16, y1: (ty << 16) | 0xffff, x1: (tx << 16) | 0xffff };
            for level in 0..=MAX_LEVEL {
                ranges.extend(level_ranges(level, &r, true));
            }
        }
        let ranges = merge_ranges(ranges);
        self.in_ranges::<Way>(&ranges, |w| w.nodes.iter().any(|n| ids.contains(n)))
    }

    /// Relations having any of the given elements as members.
    pub fn rels_with_members(&self, mtype: ElemType, ids: &[Id]) -> Result<Vec<Relation>> {
        let rel_ids = self.snap.relations_with_members(mtype, ids)?;
        self.by_ids::<Relation>(&rel_ids)
    }

    /// Meta data of elements; each chunk is decoded once, in parallel when
    /// there are many.
    pub fn meta(&self, kind: Kind, ids: &[Id]) -> Result<FxHashMap<Id, Meta>> {
        let chunks = self.snap.meta_chunks(kind, ids)?;
        let decode = |(v, want): &(&[u8], Vec<Id>)| -> Result<Vec<(Id, Meta)>> {
            Ok(codec::decode_meta(v, Pick::Ids(want))?.into_iter().map(|m| (m.id, m.meta)).collect())
        };
        let mut out = FxHashMap::with_capacity_and_hasher(ids.len(), Default::default());
        if chunks.len() >= PAR_THRESHOLD && super::parallel_ok() {
            self.limits.check()?;
            for part in chunks.par_iter().map(decode).collect::<Result<Vec<_>>>()? {
                out.extend(part);
            }
        } else {
            for (i, c) in chunks.iter().enumerate() {
                if i % 256 == 255 {
                    self.limits.check()?;
                }
                out.extend(decode(c)?);
            }
        }
        Ok(out)
    }

    pub fn user_name(&self, uid: u32) -> Result<Option<String>> {
        if let Some(n) = self.user_cache.borrow().get(&uid) {
            return Ok(n.clone());
        }
        let n = self.snap.user_name(uid)?;
        self.user_cache.borrow_mut().insert(uid, n.clone());
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Many rectangles give the cell ranges of each one computed alone:
    /// points, small and larger rectangles, at the edges of the key space.
    #[test]
    fn tile_runs_keep_the_ranges() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) % n
        };
        let one_by_one = |kind: Kind, rects: &[Rect]| {
            let mut r = Vec::new();
            for rect in rects {
                r.extend(if kind == Kind::Node { tile_ranges(rect) } else { loose_ranges(rect) });
            }
            merge_ranges(r)
        };
        for case in 0..40u64 {
            // a cluster of 40 tiles, or the whole key space
            let (y, x, spread) = match case % 4 {
                0 => (0x6000_0000, 0x7000_0000, 40u64 << 16),
                1 => (0, 0, 40 << 16),
                2 => (u32::MAX - (40 << 16), u32::MAX - (40 << 16), 40 << 16),
                _ => (0, 0, u32::MAX as u64),
            };
            let mut rects = Vec::new();
            for k in 0..(33 + rnd(400)) {
                let size = [0, 1 << 12, 3 << 16, 10 << 16, 300 << 16][(k % 5) as usize];
                let (y0, x0) = (y as u64 + rnd(spread), x as u64 + rnd(spread));
                let y1 = (y0 + rnd(size + 1)).min(u32::MAX as u64);
                let x1 = (x0 + rnd(size + 1)).min(u32::MAX as u64);
                rects.push(Rect { y0: y0 as u32, x0: x0 as u32, y1: y1 as u32, x1: x1 as u32 });
            }
            rects.push(Rect::EMPTY);
            for kind in [Kind::Node, Kind::Way] {
                assert_eq!(ranges_for(kind, &rects), one_by_one(kind, &rects), "case {case}");
            }
        }
        // 2000 points in 40 x 40 tiles: at most 20 runs a row
        let points: Vec<Rect> = (0..2000).map(|_| Rect::point(Pt { y: 0x6000_0000 + rnd(40 << 16) as u32, x: 0x7000_0000 + rnd(40 << 16) as u32 })).collect();
        assert!(tile_runs(&points).len() <= 800);
        assert_eq!(ranges_for(Kind::Way, &points), one_by_one(Kind::Way, &points));
    }
}
