//! Element retrieval on top of a database snapshot, with per-query caches.
//!
//! Chunk values are collected from LMDB sequentially (cursor operations are
//! cheap pointer walks over the memory map) and decoded in parallel.

use super::filter::TagFilter;
use crate::geo::{Cell, CodeRange, Pt, Rect, arc_bulge, level_ranges, level_ranges_many, loose_ranges, merge_ranges, tile_ranges, MAX_LEVEL};
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

/// Threads of a tag index scan over a key with many values: they mostly
/// wait for the disk.
const SCAN_THREADS: usize = 8;

/// Values of a key above which its tag index scan runs on `SCAN_THREADS`
/// threads (`tag_candidates_par`).
fn par_scan_values() -> usize {
    #[cfg(test)]
    if let Some(n) = PAR_SCAN_VALUES.get() {
        return n;
    }
    20_000
}

#[cfg(test)]
thread_local! {
    pub static PAR_SCAN_VALUES: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    pub static PAR_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Where `Fetch::bounded` reads: the elements stored in cell ranges, or
/// those at known (cell, id) positions.
pub enum Source<'a> {
    Ranges(&'a [CodeRange]),
    At(Vec<(Cell, Id)>),
}

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
/// Many rectangles (the tiles of an area) are joined into rows and walked
/// together (`level_ranges_many`).
pub fn ranges_for(kind: Kind, rects: &[Rect]) -> Vec<CodeRange> {
    if rects.len() > 32 {
        let rows = join_rows(tile_runs(rects));
        return merge_ranges(match kind {
            Kind::Node => level_ranges_many(MAX_LEVEL, &rows, false),
            _ => (0..=MAX_LEVEL).flat_map(|level| level_ranges_many(level, &rows, true)).collect(),
        });
    }
    let mut r = Vec::new();
    for rect in rects {
        match kind {
            Kind::Node => r.extend(tile_ranges(rect)),
            _ => r.extend(loose_ranges(rect)),
        }
    }
    merge_ranges(r)
}

/// Cell ranges where ways or relations whose great-circle edges may meet
/// the rectangles are stored (`around`, implicit areas): as `ranges_for`,
/// with the rectangles grown north and south at each level by the bulge of
/// an edge one cell wide (`arc_bulge`), as a cell holds boxes at most one
/// cell wide. Nodes as `ranges_for`.
pub fn arc_ranges(kind: Kind, rects: &[Rect]) -> Vec<CodeRange> {
    if kind == Kind::Node {
        return ranges_for(kind, rects);
    }
    let mut r = Vec::new();
    if rects.len() > 32 {
        let rows = join_rows(tile_runs(rects));
        for level in 0..=MAX_LEVEL {
            let m = arc_bulge(1u64 << (32 - level as u32));
            let grown: Vec<Rect> = rows.iter().map(|rect| rect.grow_lat(m)).collect();
            r.extend(level_ranges_many(level, &grown, true));
        }
        return merge_ranges(r);
    }
    for level in 0..=MAX_LEVEL {
        let m = arc_bulge(1u64 << (32 - level as u32));
        for rect in rects {
            r.extend(level_ranges(level, &rect.grow_lat(m), true));
        }
    }
    merge_ranges(r)
}

/// The rectangles with those of one row (same south and north edge) that
/// touch or overlap joined: the tiles of an area become a few runs a row.
fn join_rows(mut rects: Vec<Rect>) -> Vec<Rect> {
    rects.sort_unstable_by_key(|r| (r.y0, r.y1, r.x0));
    let mut out: Vec<Rect> = Vec::with_capacity(rects.len());
    for r in rects {
        if let Some(l) = out.last_mut() {
            if l.y0 == r.y0 && l.y1 == r.y1 && r.x0 <= l.x1.saturating_add(1) {
                l.x1 = l.x1.max(r.x1);
                continue;
            }
        }
        out.push(r);
    }
    out
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

    /// `ways_untagged` for ways at known (cell, id) positions.
    pub fn ways_untagged_at(&self, pairs: Vec<(Cell, Id)>) -> Result<Vec<Way>> {
        let chunks = self.snap.chunks_at(Kind::Way, pairs)?;
        let mut out = decode_chunks(&self.limits, &chunks, |(v, _)| *v, |(v, want)| codec::decode_ways(v, Pick::Ids(want), false))?;
        out.sort_unstable_by_key(|w| w.id);
        Ok(out)
    }

    /// All elements stored in the given cell ranges that pass `keep`.
    pub fn in_ranges<T: Stored>(&self, ranges: &[CodeRange], keep: impl Fn(&T) -> bool + Sync + Send) -> Result<Vec<T>> {
        self.in_ranges_by(ranges, T::decode, keep)
    }

    /// `in_ranges` with a decoder of its own (e.g. without tags).
    pub fn in_ranges_by<T: Stored>(&self, ranges: &[CodeRange], decode: impl Fn(&[u8], Pick) -> Result<Vec<T>> + Sync + Send, keep: impl Fn(&T) -> bool + Sync + Send) -> Result<Vec<T>> {
        let chunks = self.snap.chunks_in_ranges(T::KIND, ranges)?;
        let mut out = decode_chunks(&self.limits, &chunks, |v| *v, |v| {
            let mut es = decode(v, Pick::Cells(ranges))?;
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
        self.at_where(pairs, None)
    }

    /// `at` without the elements that fail `test`, which are not decoded
    /// whole (`Stored::decode_where`).
    pub fn at_where<T: Stored>(&self, pairs: Vec<(Cell, Id)>, test: Option<codec::TagTest>) -> Result<Vec<T>> {
        // In batches, so that a query with millions of candidates (a common
        // key over a country) sees its deadline while they are grouped by
        // chunk; a chunk on a batch border is read twice for disjoint ids.
        const BATCH: usize = 1 << 20;
        let mut out = Vec::new();
        for part in pairs.chunks(BATCH) {
            self.limits.check()?;
            let work = self.snap.chunks_at(T::KIND, part.to_vec())?;
            out.extend(decode_chunks(&self.limits, &work, |(v, _)| *v, |(v, want)| match test {
                Some(t) => T::decode_where(v, Pick::Ids(want), t),
                None => T::decode(v, Pick::Ids(want)),
            })?);
        }
        out.sort_unstable_by_key(|e| e.id());
        out.dedup_by_key(|e| e.id());
        Ok(out)
    }

    /// The elements of `src` that pass `keep` (as `in_ranges` and `at`), or
    /// None as soon as those read so far are too many: after each chunk
    /// `over` gets their number and the sum of their `weight`, and once it
    /// holds no further chunk is decoded. `over` must not turn false again
    /// as both grow. Lets a caller try a source whose cost only shows while
    /// it is read (relations with their members), and lose about what it
    /// allows. The pairs of `At` are read in one batch (see `at`).
    pub fn bounded<T: Stored>(&self, src: Source<'_>, keep: impl Fn(&T) -> bool + Sync + Send, weight: impl Fn(&T) -> usize + Sync + Send, over: impl Fn(usize, usize) -> bool + Sync + Send) -> Result<Option<Vec<T>>> {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
        let (count, sum, stop) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicBool::new(false));
        let take = |mut es: Vec<T>| {
            es.retain(|e| keep(e));
            es.shrink_to_fit();
            let w: usize = es.iter().map(&weight).sum();
            let n = count.fetch_add(es.len(), Relaxed) + es.len();
            let s = sum.fetch_add(w, Relaxed) + w;
            if over(n, s) {
                stop.store(true, Relaxed);
            }
            es
        };
        let mut out = match src {
            Source::Ranges(ranges) => {
                let chunks = self.snap.chunks_in_ranges(T::KIND, ranges)?;
                decode_chunks(&self.limits, &chunks, |v| *v, |v| if stop.load(Relaxed) { Ok(Vec::new()) } else { Ok(take(T::decode(v, Pick::Cells(ranges))?)) })?
            }
            Source::At(pairs) => {
                let work = self.snap.chunks_at(T::KIND, pairs)?;
                decode_chunks(&self.limits, &work, |(v, _)| *v, |(v, want)| if stop.load(Relaxed) { Ok(Vec::new()) } else { Ok(take(T::decode(v, Pick::Ids(want))?)) })?
            }
        };
        if stop.load(Relaxed) {
            return Ok(None);
        }
        out.sort_unstable_by_key(|e| e.id());
        out.dedup_by_key(|e| e.id());
        Ok(Some(out))
    }

    /// Candidate (cell, id) pairs from the tag index. `ranges` optionally
    /// restricts the cells. Entries with truncated keys or values are always
    /// included; callers verify candidates against the element.
    pub fn tag_candidates(&self, kind: Kind, f: &TagFilter, ranges: Option<&[CodeRange]>) -> Result<Vec<(Cell, Id)>> {
        if let Some(mut out) = self.tag_candidates_par(kind, f, ranges, |_| true, usize::MAX)? {
            self.limits.check()?;
            out.sort_unstable();
            out.dedup();
            return Ok(out);
        }
        let mut out = Vec::new();
        let mut n = 0u32;
        self.scan_candidates(kind, f, ranges, || true, |cell, ids| {
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

    /// `tag_candidates` that gives up: None once the scan has visited more
    /// than `max_steps` values and cell groups of the index (a regex prefix
    /// shared by many values, a common value). Lets a caller try the index
    /// where another source may cost less, and lose at most `max_steps`
    /// index steps.
    pub fn tag_candidates_bounded(&self, kind: Kind, f: &TagFilter, ranges: Option<&[CodeRange]>, max_steps: usize) -> Result<Option<Vec<(Cell, Id)>>> {
        let mut out = Vec::new();
        let steps = std::cell::Cell::new(0usize);
        let step = || {
            steps.set(steps.get() + 1);
            steps.get() <= max_steps
        };
        let mut over = false;
        self.scan_candidates(kind, f, ranges, step, |cell, ids| {
            if !step() {
                over = true;
                return Ok(Scan::Stop);
            }
            if steps.get() % 4096 == 0 {
                self.limits.check()?;
            }
            out.extend(ids.iter().map(|id| (cell, *id)));
            Ok(Scan::Next)
        })?;
        if over || steps.get() > max_steps {
            return Ok(None);
        }
        self.limits.check()?;
        out.sort_unstable();
        out.dedup();
        Ok(Some(out))
    }

    /// The pairs (sorted) that the tag index also lists for `f`, or None
    /// once the scan has taken more than `max_steps` steps (as
    /// `tag_candidates_bounded`). An element has its tag index entries in
    /// its own cell, so a pair that one filter lists and another does not
    /// is an element without one of the tags.
    pub fn tag_intersect(&self, kind: Kind, f: &TagFilter, ranges: Option<&[CodeRange]>, pairs: &[(Cell, Id)], max_steps: usize) -> Result<Option<Vec<(Cell, Id)>>> {
        let mut hit = vec![false; pairs.len()];
        let steps = std::cell::Cell::new(0usize);
        let step = || {
            steps.set(steps.get() + 1);
            steps.get() <= max_steps
        };
        let mut over = false;
        self.scan_candidates(kind, f, ranges, step, |cell, ids| {
            if !step() {
                over = true;
                return Ok(Scan::Stop);
            }
            if steps.get() % 4096 == 0 {
                self.limits.check()?;
            }
            let lo = pairs.partition_point(|p| p.0 < cell);
            let run = &pairs[lo..lo + pairs[lo..].partition_point(|p| p.0 == cell)];
            if !run.is_empty() {
                for id in ids {
                    if let Ok(i) = run.binary_search_by_key(id, |p| p.1) {
                        hit[lo + i] = true;
                    }
                }
            }
            Ok(Scan::Next)
        })?;
        if over || steps.get() > max_steps {
            return Ok(None);
        }
        Ok(Some(pairs.iter().zip(hit).filter(|(_, h)| *h).map(|(p, _)| *p).collect()))
    }

    /// Chunks of a kind in the cell ranges, and in all.
    pub fn chunk_share(&self, kind: Kind, ranges: &[CodeRange]) -> Result<(usize, u64)> {
        let total = self.snap.chunk_count(kind)?;
        if total == 0 {
            return Ok((0, 0));
        }
        Ok((self.snap.chunks_in_ranges(kind, ranges)?.len(), total))
    }

    /// Candidates from the tag index without ranges, kept when their cell
    /// lies in `within` (a filter, not a seek per range and value), or None
    /// once more than `max` are kept. On several threads where the key has
    /// many values (`tag_candidates_par`).
    pub fn tag_candidates_in(&self, kind: Kind, f: &TagFilter, within: Option<&[CodeRange]>, max: usize) -> Result<Option<Vec<(Cell, Id)>>> {
        if let Some(mut out) = self.tag_candidates_par(kind, f, None, |cell| within.is_none_or(|r| cell_in_ranges(cell, r)), max)? {
            if out.len() > max {
                return Ok(None);
            }
            self.limits.check()?;
            out.sort_unstable();
            out.dedup();
            return Ok(Some(out));
        }
        let mut out = Vec::new();
        let mut n = 0u32;
        let mut over = false;
        self.scan_candidates(kind, f, None, || true, |cell, ids| {
            n = n.wrapping_add(1);
            if n % 4096 == 0 {
                self.limits.check()?;
            }
            if within.is_none_or(|r| cell_in_ranges(cell, r)) {
                out.extend(ids.iter().map(|id| (cell, *id)));
                if out.len() > max {
                    over = true;
                    return Ok(Scan::Stop);
                }
            }
            Ok(Scan::Next)
        })?;
        if over {
            return Ok(None);
        }
        self.limits.check()?;
        out.sort_unstable();
        out.dedup();
        Ok(Some(out))
    }

    /// `tag_candidates` on several threads (`Snapshot::scan_tags_par`) for
    /// a regex or `[key]` filter on a key with more than `par_scan_values()`
    /// values: such a scan reads many index pages, mostly from disk, and a
    /// thread waits for each read (`nwr["website"~"example",i]` reads
    /// 10^5 pages). Counting the values first reads the start of the scan,
    /// which the threads then find in memory. None: the filter is of
    /// another kind, the key has fewer values, or the threads cannot read
    /// the same snapshot; the caller scans on its own thread.
    /// The threads stop once more than `max` pairs are kept, and then
    /// return more than `max`.
    fn tag_candidates_par(&self, kind: Kind, f: &TagFilter, ranges: Option<&[CodeRange]>, keep: impl Fn(Cell) -> bool + Sync, max: usize) -> Result<Option<Vec<(Cell, Id)>>> {
        let (key, re, prefixes) = match f {
            TagFilter::HasKey(key) => (key, None, None),
            TagFilter::Regex { key, re, neg: false, prefixes, .. } => (key, Some(re), prefixes.as_deref()),
            _ => return Ok(None),
        };
        let mut k = Vec::new();
        crate::store::index_part(key, &mut k);
        let limit = par_scan_values();
        let mut n = 0usize;
        let r = self.snap.scan_tags_matching(kind, &k, prefixes, ranges, |_| {
            n += 1;
            if n > limit { Scan::Stop } else { Scan::SkipValue }
        }, |_, _, _| Ok(Scan::Next))?;
        if r != Scan::Stop {
            return Ok(None);
        }
        self.limits.check()?;
        let limits = &self.limits;
        // Values failing the regex are skipped as a whole (`scan_candidates`).
        let accept = |val: &[u8]| re.is_none_or(|re| val.contains(&1) || std::str::from_utf8(val).is_ok_and(|s| re.is_match(s)));
        let out = self.snap.scan_tags_par(kind, &k, prefixes, ranges, SCAN_THREADS, max, || limits.expired(), accept, keep)?;
        #[cfg(test)]
        PAR_SCANS.set(PAR_SCANS.get() + out.is_some() as usize);
        Ok(out)
    }

    /// Number of tag index candidates for the filter, counting stops past
    /// `cap` (to compare the selectivity of filters cheaply). A scan that
    /// takes more than `cap` steps (values of the key and cell groups, as
    /// `tag_candidates_bounded`) counts as `cap + 1`: a regex prefix shared
    /// by many values that fail the regex would otherwise walk them all.
    pub fn tag_count_capped(&self, kind: Kind, f: &TagFilter, ranges: Option<&[CodeRange]>, cap: usize) -> Result<usize> {
        let mut n = 0usize;
        let steps = std::cell::Cell::new(0usize);
        let step = || {
            steps.set(steps.get() + 1);
            steps.get() <= cap
        };
        self.scan_candidates(kind, f, ranges, step, |_, ids| {
            n += ids.len();
            Ok(if n > cap || !step() { Scan::Stop } else { Scan::Next })
        })?;
        Ok(if steps.get() > cap { n.max(cap + 1) } else { n })
    }

    /// Whether the tag index has any candidate for the filter.
    pub fn tag_exists(&self, kind: Kind, f: &TagFilter) -> Result<bool> {
        let mut found = false;
        self.scan_candidates(kind, f, None, || true, |_, _| {
            found = true;
            Ok(Scan::Stop)
        })?;
        Ok(found)
    }

    /// Index entries (cell, encoded ids) that may match a positive filter.
    /// `visit` is called for every value of the key the scan comes to
    /// (not for `key=value`); false ends the scan.
    fn scan_candidates(&self, kind: Kind, f: &TagFilter, ranges: Option<&[CodeRange]>, mut visit: impl FnMut() -> bool, mut push: impl FnMut(Cell, &[Id]) -> Result<Scan>) -> Result<()> {
        // Value test for regexes: values failing it are skipped as a whole.
        let value_test = |re: &regex::Regex, val: &[u8]| val.contains(&1) || std::str::from_utf8(val).is_ok_and(|s| re.is_match(s));
        let mut accept = |ok: bool| match (visit(), ok) {
            (false, _) => Scan::Stop,
            (true, true) => Scan::Next,
            (true, false) => Scan::SkipValue,
        };
        match f {
            TagFilter::Eq(k, v) => self.snap.scan_tags(kind, k, Some(v), ranges, |_, cell, ids| push(cell, ids)),
            TagFilter::HasKey(key) => {
                let mut k = Vec::new();
                crate::store::index_part(key, &mut k);
                self.snap.scan_tags_matching(kind, &k, None, ranges, |_| accept(true), |_, cell, ids| push(cell, ids))?;
                Ok(())
            }
            TagFilter::Regex { key, re, neg: false, prefixes, .. } => {
                let mut k = Vec::new();
                crate::store::index_part(key, &mut k);
                self.snap.scan_tags_matching(kind, &k, prefixes.as_deref(), ranges, |val| accept(value_test(re, val)), |_, cell, ids| push(cell, ids))?;
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
                    let r = self.snap.scan_tags_matching(kind, &k, prefixes, ranges, |val| accept(truncated || value_test(value, val)), |_, cell, ids| push(cell, ids))?;
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

    /// Whether a cell may hold an element of the kind that meets one of the
    /// rectangles: its tile (nodes) or its loose extent (ways, relations).
    fn cell_meets(kind: Kind, c: Cell, rects: &[Rect]) -> bool {
        let ext = if kind == Kind::Node {
            let (ty, tx) = crate::geo::unmorton(c.code);
            Rect { y0: ty << 16, x0: tx << 16, y1: (ty << 16) | 0xffff, x1: (tx << 16) | 0xffff }
        } else {
            c.loose_rect()
        };
        rects.iter().any(|r| !r.is_empty() && ext.intersects(r))
    }

    /// Cells of the ranges for a check: the first, middle and last of each.
    fn sample_cells(ranges: &[CodeRange]) -> Vec<Cell> {
        let mut out = Vec::new();
        for r in ranges {
            let mask = if r.level == 0 { u32::MAX } else { (1u32 << (2 * (16 - r.level as u32))) - 1 };
            for code in [r.lo, r.lo + (r.hi - r.lo) / 2, r.hi] {
                out.push(Cell { level: r.level, code: code & !mask });
            }
        }
        out
    }

    /// Many rectangles give the cells that meet them, within the cell ranges
    /// of each one computed alone (which takes nodes whole when a depth has
    /// many): points, small and larger rectangles, at the edges of the key
    /// space, and the tiles of an area on a grid offset from the cells.
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
        let mut cases: Vec<Vec<Rect>> = Vec::new();
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
            cases.push(rects);
        }
        // 2000 points in 40 x 40 tiles: at most 20 runs a row
        let points: Vec<Rect> = (0..2000).map(|_| Rect::point(Pt { y: 0x6000_0000 + rnd(40 << 16) as u32, x: 0x7000_0000 + rnd(40 << 16) as u32 })).collect();
        assert!(tile_runs(&points).len() <= 800);
        cases.push(points);
        // the tiles of an area (2^20 wide, offset from the cells) with holes
        let mut area = Vec::new();
        for ty in 0..30u32 {
            for tx in 0..30u32 {
                if rnd(5) > 0 {
                    let (y0, x0) = (0x6008_9680 + (ty << 20), 0x7003_0000 + (tx << 20));
                    area.push(Rect { y0, x0, y1: y0 + 0xfffff, x1: x0 + 0xfffff });
                }
            }
        }
        cases.push(area);
        for (case, rects) in cases.iter().enumerate() {
            for kind in [Kind::Node, Kind::Way] {
                let many = ranges_for(kind, rects);
                let each = one_by_one(kind, rects);
                assert!(covers(&each, &many), "case {case}");
                let mut cells = sample_cells(&each);
                for _ in 0..2000 {
                    let r = &rects[rnd(rects.len() as u64) as usize];
                    if r.is_empty() {
                        continue;
                    }
                    let level = if kind == Kind::Node { MAX_LEVEL } else { rnd(17) as u8 };
                    let mut near = |a: u32, b: u32| (a as u64).saturating_sub(1 << 21) + rnd((b - a) as u64 + (2 << 21));
                    let p = Pt { y: near(r.y0, r.y1).min(u32::MAX as u64) as u32, x: near(r.x0, r.x1).min(u32::MAX as u64) as u32 };
                    cells.push(crate::geo::cell_at(level, p));
                }
                for c in cells {
                    assert_eq!(cell_in_ranges(c, &many), cell_meets(kind, c, rects), "case {case} {kind:?} {c:?}");
                }
            }
        }
    }

    /// Whether every range of `small` lies within a range of `big`.
    fn covers(big: &[CodeRange], small: &[CodeRange]) -> bool {
        small.iter().all(|s| big.iter().any(|b| b.level == s.level && b.lo <= s.lo && s.hi <= b.hi))
    }

    /// The cell of a way is in the `arc_ranges` of a rectangle that its
    /// great-circle edge meets north or south of its box (a thin band at
    /// the extreme latitude of the edge, over its longitudes), which
    /// `ranges_for` misses for some. `arc_ranges` hold `ranges_for`, and
    /// with many rectangles the ranges of each one.
    #[test]
    fn arc_ranges_hold_ways_whose_edges_meet() {
        let mut seed = 0x5851_f42d_4c95_7f2du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut missed = 0;
        let mut bands = Vec::new();
        for _ in 0..4000 {
            // widths from 10^-3 to 170 degrees
            let d = 10f64.powf(next() * 5.3 - 3.0).min(170.0);
            let la = next() * 160.0 - 80.0;
            let lb = la + (next() - 0.5) * d * 0.01;
            let lo = next() * (360.0 - d) - 180.0;
            let (a, b) = (Pt::from_ll(la, lo), Pt::from_ll(lb, lo + d));
            let mut bx = Rect::point(a);
            bx.extend(b);
            let cell = crate::geo::anchor_cell(&bx);
            let (s, n) = crate::geo::Segment::new(a.lat(), a.lon(), b.lat(), b.lon()).lat_extent();
            let y = ((if la >= 0.0 { n } else { s } + 91.0) * 1e7) as u32;
            let band = Rect { y0: y - 1, x0: bx.x0, y1: y + 1, x1: bx.x1 };
            let arc = arc_ranges(Kind::Way, &[band]);
            let plain = ranges_for(Kind::Way, &[band]);
            assert!(cell_in_ranges(cell, &arc), "{la} {lb} {lo} {d}");
            assert!(bx.arc_box().intersects(&band), "{la} {lb} {lo} {d}");
            assert!(covers(&arc, &plain), "{la} {lb} {lo} {d}");
            missed += !cell_in_ranges(cell, &plain) as usize;
            bands.push(band);
        }
        assert!(missed > 0);
        // many small rectangles go through `tile_runs`
        let small: Vec<Rect> = bands.iter().take(200).map(|b| Rect { x1: b.x0.saturating_add(50_000), ..*b }).collect();
        let each: Vec<CodeRange> = small.iter().flat_map(|r| arc_ranges(Kind::Way, std::slice::from_ref(r))).collect();
        assert!(covers(&arc_ranges(Kind::Way, &small), &each));
        assert_eq!(arc_ranges(Kind::Node, &small), ranges_for(Kind::Node, &small));
    }
}
