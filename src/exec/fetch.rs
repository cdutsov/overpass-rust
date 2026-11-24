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
    /// The DB areas of the last `(area)` clause, by their ids (`Engine::area_ctx`).
    pub(super) area_db: RefCell<Option<(Vec<Id>, std::sync::Arc<super::area::AreaDb>)>>,
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

    /// Take bytes off the memory estimate that `add` counted for data
    /// since dropped (lists of an evaluator).
    pub fn release(&self, bytes: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let mut cur = self.bytes.load(Relaxed);
        while let Err(now) = self.bytes.compare_exchange_weak(cur, cur.saturating_sub(bytes), Relaxed, Relaxed) {
            cur = now;
        }
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

/// A node position read for way geometry; counted as the untagged node
/// it is read from.
struct NodePos {
    id: Id,
    pos: Pt,
}

impl MemSize for NodePos {
    fn mem_size(&self) -> (u64, u64) {
        (8, std::mem::size_of::<Node>() as u64)
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

/// Gives back, when dropped, what was counted against the memory estimate
/// since it was made (`Limits::mem_scope`).
pub struct MemScope {
    mem: Option<std::sync::Arc<MemBudget>>,
    mark: u64,
}

impl Drop for MemScope {
    fn drop(&mut self) {
        if let Some(m) = &self.mem {
            m.bytes.fetch_min(self.mark, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl Limits {
    /// For lookups whose results are dropped before the statement ends
    /// (the vertices of a batch of ways for a geometry test or a print
    /// window): the memory estimate returns to its present value when the
    /// scope is dropped. Without it every decoded node stays counted (64
    /// bytes) until the statement ends, and a statement that looks at the
    /// geometry of 3 x 10^6 ways runs out of the 2 GiB of the default
    /// `maxsize` while it holds a few MB.
    pub fn mem_scope(&self) -> MemScope {
        MemScope { mark: self.mem.as_ref().map_or(0, |m| m.bytes.load(std::sync::atomic::Ordering::Relaxed)), mem: self.mem.clone() }
    }
}

/// Bytes of one candidate pair of a tag index read.
const PAIR_BYTES: u64 = std::mem::size_of::<(Cell, Id)>() as u64;

impl Limits {
    /// Account for candidate pairs of a tag index read against the memory
    /// budget: their list is held while the elements are decoded, and a
    /// key alone can list most elements of the planet.
    fn account_pairs(&self, n: usize) -> Result<()> {
        match &self.mem {
            Some(m) => m.add(0, n as u64 * PAIR_BYTES),
            None => Ok(()),
        }
    }

    /// Candidate pairs the memory budget has room for.
    fn pairs_room(&self) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        match &self.mem {
            Some(m) => (m.max_bytes.load(Relaxed).saturating_sub(m.bytes.load(Relaxed)) / PAIR_BYTES) as usize,
            None => usize::MAX,
        }
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

/// Count bytes that a geometry algorithm holds for the query running on
/// this thread. False when the query is past its memory limit: the
/// algorithm stops, and the evaluator reports the limit afterwards.
pub fn thread_hold(bytes: u64) -> bool {
    THREAD_LIMITS.with(|t| match &t.borrow().mem {
        Some(m) => m.add(0, bytes).is_ok(),
        None => true,
    })
}

/// The keys an element has when it passes all of the filters.
pub fn required_keys<'a>(filters: impl IntoIterator<Item = &'a TagFilter>) -> Vec<&'a str> {
    filters
        .into_iter()
        .filter_map(|f| match f {
            TagFilter::HasKey(k) | TagFilter::Eq(k, _) | TagFilter::Regex { key: k, neg: false, .. } => Some(k.as_str()),
            _ => None,
        })
        .collect()
}

/// Entries kept in the node position cache before it is reset.
const POS_CACHE_LIMIT: usize = 8_000_000;

pub use crate::geo::cell_in_ranges;


/// Cell ranges where elements of a kind intersecting the rectangles may be stored.
/// Many rectangles (the tiles of an area) are joined into rows and walked
/// together (`level_ranges_many`).
pub fn ranges_for(kind: Kind, rects: &[Rect]) -> Vec<CodeRange> {
    if rects.len() > 32 {
        return ranges_for_many(kind, rects);
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

/// `ranges_for` as it works for many rectangles, whatever their number:
/// joined into rows and walked together. The tiles of an area and the
/// same tiles joined into rows beforehand give the same ranges.
pub fn ranges_for_many(kind: Kind, rects: &[Rect]) -> Vec<CodeRange> {
    let rows = join_rows(tile_runs(rects));
    merge_ranges(match kind {
        Kind::Node => level_ranges_many(MAX_LEVEL, &rows, false),
        _ => (0..=MAX_LEVEL).flat_map(|level| level_ranges_many(level, &rows, true)).collect(),
    })
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

/// Decode work items, in parallel when there are many (`par_run`). Stops
/// early when the query runs out of time.
fn par_flat_map<I: Sync, T: Send + MemSize>(limits: &Limits, items: &[I], f: impl Fn(&I) -> Result<Vec<T>> + Sync + Send) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let parts = super::par_parts(items, PAR_THRESHOLD, &|it| -> Result<Vec<T>> {
        limits.check()?;
        let es = f(it)?;
        limits.account(&es)?;
        Ok(es)
    }, |es| {
        out.extend(es);
        Ok(())
    })?;
    // the parts of the pool in one reservation: growing the vector part by
    // part copies it and holds up to twice its size meanwhile
    out.reserve_exact(parts.iter().flatten().map(Vec::len).sum());
    for es in parts.into_iter().flatten() {
        out.extend(es);
    }
    Ok(out)
}

/// Pairs of bytes one of which every value that a regex matches holds side
/// by side: the first two bytes of each match (the prefixes of the pattern
/// cut to two bytes; a case-insensitive pattern has a few spellings of
/// them). A scan of the values of a key asks this first: most values hold
/// none of the pairs and are passed over without the UTF-8 check and the
/// regex, which were half of the time of a scan of all names. None when
/// the pattern tells no such pairs (a match may start with anything, or be
/// shorter than two bytes).
struct FirstPairs(Box<[u64; 1024]>);

impl FirstPairs {
    fn of(re: &super::filter::Ere) -> Option<FirstPairs> {
        use regex_syntax::hir::literal::{ExtractKind, Extractor};
        // the parser of `filter::required_literals`
        let hir = regex_syntax::ParserBuilder::new().dot_matches_new_line(true).build().parse(re.as_str()).ok()?;
        let seq = Extractor::new().kind(ExtractKind::Prefix).limit_literal_len(2).extract(&hir);
        let lits = seq.literals()?;
        // Substitutes of word-difference characters (lead byte F1) are not
        // in values (as in `required_literals`).
        if lits.is_empty() || lits.iter().any(|l| l.len() < 2 || l.as_bytes().contains(&0xF1)) {
            return None;
        }
        let mut bits = Box::new([0u64; 1024]);
        for l in lits {
            let b = l.as_bytes();
            let i = (b[0] as usize) << 8 | b[1] as usize;
            bits[i >> 6] |= 1 << (i & 63);
        }
        Some(FirstPairs(bits))
    }

    #[inline]
    fn may_match(&self, v: &[u8]) -> bool {
        v.windows(2).any(|w| {
            let i = (w[0] as usize) << 8 | w[1] as usize;
            self.0[i >> 6] >> (i & 63) & 1 == 1
        })
    }
}

/// Whether a value of the tag index passes the regex: a truncated value
/// (it holds a byte 1) is taken and tested on the element.
#[inline]
fn value_passes(re: &super::filter::Ere, pairs: Option<&FirstPairs>, val: &[u8]) -> bool {
    val.contains(&1) || (pairs.is_none_or(|p| p.may_match(val)) && std::str::from_utf8(val).is_ok_and(|s| re.is_match(s)))
}

/// The places of `elems` in the order of their (cell, id). The elements of
/// a set come sorted by id and lie in few cells compared with their number
/// (the nodes of a box in its tiles): they are counted per cell and put in
/// place, which keeps the ids of a cell in order; a cell whose ids do not
/// ascend (`out qt`) is sorted. About a quarter of the time of sorting the
/// pairs.
fn by_cell(elems: &[(Cell, Id)]) -> Vec<u32> {
    let value = |c: Cell| codec::cell_value(c);
    // group number per cell, in order of first appearance
    let mut group: FxHashMap<u64, u32> = FxHashMap::default();
    let mut cells: Vec<(u64, u32)> = Vec::new();
    let mut of: Vec<u32> = Vec::with_capacity(elems.len());
    let mut last = (u64::MAX, 0u32);
    for (c, _) in elems {
        let v = value(*c);
        if v != last.0 {
            let g = *group.entry(v).or_insert_with(|| {
                cells.push((v, 0));
                cells.len() as u32 - 1
            });
            last = (v, g);
        }
        cells[last.1 as usize].1 += 1;
        of.push(last.1);
    }
    // where the elements of each group start, the groups in cell order
    let mut by_value: Vec<u32> = (0..cells.len() as u32).collect();
    by_value.sort_unstable_by_key(|&g| cells[g as usize].0);
    let mut next: Vec<u32> = vec![0; cells.len()];
    let mut at = 0;
    for &g in &by_value {
        next[g as usize] = at;
        at += cells[g as usize].1;
    }
    let starts = next.clone();
    let mut order: Vec<u32> = vec![0; elems.len()];
    for (i, &g) in of.iter().enumerate() {
        order[next[g as usize] as usize] = i as u32;
        next[g as usize] += 1;
    }
    for (g, &s) in starts.iter().enumerate() {
        let run = &mut order[s as usize..next[g] as usize];
        if run.windows(2).any(|w| elems[w[0] as usize].1 > elems[w[1] as usize].1) {
            run.sort_unstable_by_key(|&i| elems[i as usize].1);
        }
    }
    order
}

/// Sort decoded elements by id. They come chunk by chunk in the order of
/// their (cell, id): ascending ids per cell. For nodes, up to RUNS such
/// runs are merged (the stable sort finds them: a box of a few tiles, the
/// nodes of its ways), which costs a third of sorting from scratch; more
/// of them, or more than 2^18 nodes (the merge takes a buffer of half of
/// them, which the limits do not count), are sorted in place. Ways and
/// relations are sorted in place: for elements of their size the merge
/// was slower than the unstable sort from 8 runs on.
fn sort_by_id<T: Stored>(v: &mut [T]) {
    const RUNS: usize = 64;
    if T::KIND != Kind::Node {
        v.sort_unstable_by_key(|e| e.id());
        return;
    }
    let mut runs = 1;
    if v.len() <= 1 << 18 {
        for w in v.windows(2) {
            if w[1].id() < w[0].id() {
                runs += 1;
                if runs > RUNS {
                    break;
                }
            }
        }
    } else {
        runs = RUNS + 1;
    }
    if runs == 1 {
        return;
    }
    if runs <= RUNS {
        v.sort_by_key(|e| e.id());
    } else {
        v.sort_unstable_by_key(|e| e.id());
    }
}

impl<'s> Fetch<'s> {
    pub fn new(snap: &'s Snapshot<'s>) -> Fetch<'s> {
        Fetch { snap, pos_cache: RefCell::new(FxHashMap::default()), area_db: RefCell::new(None), user_cache: RefCell::new(FxHashMap::default()), limits: Limits::default() }
    }

    /// Elements with the given ids, sorted by id.
    pub fn by_ids<T: Stored>(&self, ids: &[Id]) -> Result<Vec<T>> {
        let chunks = self.snap.chunks_with_ids(T::KIND, ids)?;
        let mut out = decode_chunks(&self.limits, &chunks, |(v, _)| *v, |(v, want)| T::decode(v, Pick::Ids(want)))?;
        sort_by_id(&mut out);
        out.dedup_by_key(|e| e.id());
        Ok(out)
    }

    /// Ways with the given ids, without their tags (for geometry), sorted by id.
    pub fn ways_untagged(&self, ids: &[Id]) -> Result<Vec<Way>> {
        let chunks = self.snap.chunks_with_ids(Kind::Way, ids)?;
        let mut out = decode_chunks(&self.limits, &chunks, |(v, _)| *v, |(v, want)| codec::decode_ways(v, Pick::Ids(want), false))?;
        sort_by_id(&mut out);
        Ok(out)
    }

    /// `ways_untagged` for ways at known (cell, id) positions.
    pub fn ways_untagged_at(&self, pairs: Vec<(Cell, Id)>) -> Result<Vec<Way>> {
        let chunks = self.snap.chunks_at(Kind::Way, pairs)?;
        let mut out = decode_chunks(&self.limits, &chunks, |(v, _)| *v, |(v, want)| codec::decode_ways(v, Pick::Ids(want), false))?;
        sort_by_id(&mut out);
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
        sort_by_id(&mut out);
        out.dedup_by_key(|e| e.id());
        Ok(out)
    }

    /// `in_ranges` for a test on the tags alone: the elements that fail it
    /// are not decoded whole (`Stored::decode_where`). Most elements of a
    /// region fail a tag filter: their node lists, members and tag strings
    /// are stepped over. `filters`: tag filters that `keep` includes (their
    /// keys are looked up before any tags are built, `TagTest::keys`);
    /// `chunk`: a test of the whole chunk (`TagTest::chunk`).
    pub fn in_ranges_tagged<T: Stored>(&self, ranges: &[CodeRange], filters: &[TagFilter], chunk: &(dyn Fn(&[u8]) -> bool + Sync), keep: &(dyn Fn(&Tags) -> bool + Sync)) -> Result<Vec<T>> {
        let chunks = self.snap.chunks_in_ranges(T::KIND, ranges)?;
        let keys = required_keys(filters);
        // the key regex of a `[~"k"~"v"]` filter, asked once per key of a chunk
        let key_res: Vec<&crate::exec::filter::Ere> = filters.iter().filter_map(|f| match f { TagFilter::KeyRegex { key, .. } => Some(key), _ => None }).collect();
        let key_fns: Vec<Box<dyn Fn(&str) -> bool + Sync + '_>> = key_res.iter().map(|re| Box::new(move |k: &str| re.is_match(k)) as Box<dyn Fn(&str) -> bool + Sync + '_>).collect();
        let key_tests: Vec<&(dyn Fn(&str) -> bool + Sync)> = key_fns.iter().map(|b| &**b).collect();
        // A required key lies in the key table of a chunk as its bytes: where
        // they do not occur in the payload at all, no element has the key, and
        // the chunk is dropped before anything of it is read (to reach the key
        // table of a way chunk every way has to be stepped over).
        let key_bytes = keys.iter().filter(|k| k.len() >= 3 && k.is_ascii()).map(|k| regex::bytes::Regex::new(&regex::escape(k))).collect::<std::result::Result<Vec<_>, _>>()?;
        let chunk = &|data: &[u8]| key_bytes.iter().all(|re| re.is_match(data)) && chunk(data);
        let test = codec::TagTest { chunk, keep, keys: &keys, key_tests: &key_tests };
        let mut out = decode_chunks(&self.limits, &chunks, |v| *v, |v| T::decode_where(v, Pick::Cells(ranges), test))?;
        sort_by_id(&mut out);
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
        sort_by_id(&mut out);
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
        sort_by_id(&mut out);
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
        sort_by_id(&mut out);
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
            self.limits.account_pairs(ids.len())?;
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
                self.limits.account_pairs(ids.len())?;
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
        let pairs = re.and_then(FirstPairs::of);
        let accept = |val: &[u8]| re.is_none_or(|re| value_passes(re, pairs.as_ref(), val));
        // The threads stop past the pairs the memory budget has room for;
        // more than that is then an error.
        let out = self.snap.scan_tags_par(kind, &k, prefixes, ranges, SCAN_THREADS, max.min(limits.pairs_room()), || limits.expired(), accept, keep)?;
        if let Some(out) = &out {
            limits.account_pairs(out.len())?;
        }
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
        let pairs = match f {
            TagFilter::Regex { re, neg: false, .. } => FirstPairs::of(re),
            TagFilter::KeyRegex { value, .. } => FirstPairs::of(value),
            _ => None,
        };
        let value_test = |re: &super::filter::Ere, val: &[u8]| value_passes(re, pairs.as_ref(), val);
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
        let found = decode_chunks(&self.limits, &chunks, |(v, _)| *v, |(v, want)| Ok(codec::decode_node_positions(v, want)?.into_iter().map(|(id, pos)| NodePos { id, pos }).collect()))?;
        let mut cache = self.pos_cache.borrow_mut();
        if cache.len() > POS_CACHE_LIMIT {
            cache.clear();
        }
        cache.reserve(missing.len());
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
        // A way holds the positions of its nodes in its box (the ranges
        // above rest on that too): one whose box lies apart from the box
        // of the nodes uses none of them.
        let mut span = Rect::EMPTY;
        for (_, p) in nodes {
            span.extend(*p);
        }
        let boxed = |b: &Rect| b.y0 <= span.y1 && span.y0 <= b.y1 && b.x0 <= span.x1 && span.x0 <= b.x1;
        let chunks = self.snap.chunks_in_ranges(Kind::Way, &ranges)?;
        let mut out = decode_chunks(&self.limits, &chunks, |v| *v, |v| codec::decode_ways_using(v, Pick::Cells(&ranges), &boxed, |nds| nds.iter().any(|n| ids.contains(n))))?;
        sort_by_id(&mut out);
        out.dedup_by_key(|w| w.id);
        Ok(out)
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
        self.meta_of(chunks, ids.len())
    }

    /// `meta` for elements in hand, given as (cell, id) with the cell of
    /// `Stored::cell`, in the order given (None: no meta data). An element
    /// and its meta data are stored under that cell, so the id index is
    /// not read (a bucket of 512 ids per lookup), and the answer needs no
    /// map: the elements are put in the order of the chunks (`by_cell`),
    /// which is the order the chunks give their records in. An element
    /// without meta data at its cell is looked up by the id index, as
    /// `meta` does.
    pub fn meta_in_order(&self, kind: Kind, elems: &[(Cell, Id)]) -> Result<Vec<Option<Meta>>> {
        let mut out: Vec<Option<Meta>> = vec![None; elems.len()];
        let order = by_cell(elems);
        let pairs: Vec<(Cell, Id)> = order.iter().map(|&i| elems[i as usize]).collect();
        if pairs.windows(2).any(|w| w[0] == w[1]) {
            // an element twice: no caller does that; the plain way
            let ids: Vec<Id> = elems.iter().map(|e| e.1).collect();
            let m = self.meta(kind, &ids)?;
            return Ok(ids.iter().map(|id| m.get(id).copied()).collect());
        }
        let chunks = self.snap.meta_chunks_at(kind, &pairs)?;
        // the run of `pairs` of each chunk; pairs before the first chunk of
        // the table are in none
        let mut start = pairs.len() - chunks.iter().map(|c| c.1.len()).sum::<usize>().min(pairs.len());
        let work: Vec<(&[u8], &[Id], std::ops::Range<usize>)> = chunks
            .iter()
            .map(|(v, ids)| {
                let r = start..start + ids.len();
                start = r.end;
                (*v, &ids[..], r)
            })
            .collect();
        let limits = &self.limits;
        let mut found = 0;
        super::par_run(&work, PAR_THRESHOLD, |(v, ids, r)| -> Result<(Vec<codec::MetaRec>, std::ops::Range<usize>)> {
            limits.check()?;
            Ok((codec::decode_meta(v, Pick::Ids(ids))?, r.clone()))
        }, |(recs, r)| {
            // records and pairs of a chunk come in the same order
            let mut j = r.start;
            for m in recs {
                let at = match (j..r.end).find(|&k| pairs[k].1 == m.id) {
                    Some(k) => k,
                    None => match r.clone().find(|&k| pairs[k].1 == m.id) {
                        Some(k) => k,
                        None => continue,
                    },
                };
                // a slot counts once: a second record of an id (a stale copy
                // in the chunk) must not stand in for a missing element
                let slot = &mut out[order[at] as usize];
                found += slot.is_none() as usize;
                *slot = Some(m.meta);
                j = at + 1;
            }
            Ok(())
        })?;
        if found < elems.len() {
            let rest: Vec<Id> = elems.iter().zip(&out).filter(|(_, m)| m.is_none()).map(|(e, _)| e.1).collect();
            let m = self.meta(kind, &rest)?;
            if !m.is_empty() {
                for (e, o) in elems.iter().zip(out.iter_mut()) {
                    if o.is_none() {
                        *o = m.get(&e.1).copied();
                    }
                }
            }
        }
        Ok(out)
    }

    fn meta_of(&self, chunks: Vec<(&[u8], Vec<Id>)>, n: usize) -> Result<FxHashMap<Id, Meta>> {
        let decode = |(v, want): &(&[u8], Vec<Id>)| -> Result<Vec<(Id, Meta)>> {
            Ok(codec::decode_meta(v, Pick::Ids(want))?.into_iter().map(|m| (m.id, m.meta)).collect())
        };
        let mut out = FxHashMap::with_capacity_and_hasher(n, Default::default());
        let limits = &self.limits;
        super::par_run(&chunks, PAR_THRESHOLD, |c| {
            limits.check()?;
            decode(c)
        }, |part| {
            out.extend(part);
            Ok(())
        })?;
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

    /// rv-scan: `FirstPairs` passes every value its regex matches, for
    /// generated patterns: classes, POSIX classes, alternatives, groups,
    /// repeats, anchors, `.`, escapes, non-ASCII letters with special case
    /// rules, with and without `,i`.
    #[test]
    #[ignore = "randomized, about 3 s: cargo test --release --lib rv_first_pairs_generated_patterns -- --ignored"]
    fn rv_first_pairs_generated_patterns() {
        use super::super::filter::compile_regex;
        let mut x = 0x0123_4567_89ab_cdefu64;
        let mut next = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) % n.max(1)
        };
        let alpha: Vec<char> = "abABsSkKiIıİüÜßẞſ\u{212a}ǅǆǄуУ日🙂. xé\u{301}01-\u{200d}‿\u{e48}_".chars().collect();
        let words = ["\\b", "\\<", "\\>", "\\w", "\\W", "\\B"];
        let posix = ["[[:alpha:]]", "[[:upper:]]", "[[:lower:]]", "[[:digit:]]", "[[:alnum:]]", "[[:space:]]", "[[:punct:]]"];
        let quant = ["", "", "", "?", "*", "+", "{2}", "{1,2}", "{0,1}"];
        fn esc(c: char) -> String {
            if ".[]()*+?{}|^$\\".contains(c) { format!("\\{c}") } else { c.to_string() }
        }
        let (mut with, mut checked, mut matched, mut wsub) = (0, 0u64, 0u64, 0u64);
        for round in 0..6000 {
            let mut pat = String::new();
            if next(4) == 0 {
                pat.push('^');
            }
            let atoms = 1 + next(4);
            let mut lits: Vec<char> = Vec::new();
            for _ in 0..atoms {
                let atom = match next(12) {
                    0 => ".".to_string(),
                    10 | 11 => words[next(words.len() as u64) as usize].to_string(),
                    1 => {
                        let neg = if next(4) == 0 { "^" } else { "" };
                        let cs: String = (0..1 + next(3)).map(|_| alpha[next(alpha.len() as u64) as usize]).filter(|c| !"[]^-\\".contains(*c)).collect();
                        if cs.is_empty() { "a".to_string() } else { format!("[{neg}{cs}]") }
                    }
                    2 => posix[next(posix.len() as u64) as usize].to_string(),
                    3 => {
                        let a: String = (0..1 + next(3)).map(|_| esc(alpha[next(alpha.len() as u64) as usize])).collect();
                        let b: String = (0..1 + next(3)).map(|_| esc(alpha[next(alpha.len() as u64) as usize])).collect();
                        format!("({a}|{b})")
                    }
                    _ => {
                        let c = alpha[next(alpha.len() as u64) as usize];
                        lits.push(c);
                        esc(c)
                    }
                };
                pat += &atom;
                pat += quant[next(quant.len() as u64) as usize];
            }
            if next(4) == 0 {
                pat.push('$');
            }
            for ci in [false, true] {
                let Ok(re) = compile_regex(&pat, ci) else { continue };
                let Some(pairs) = FirstPairs::of(&re) else { continue };
                with += 1;
                for k in 0..300 {
                    let len = next(7) as usize;
                    let mut v = String::new();
                    for _ in 0..len {
                        let c = if !lits.is_empty() && next(2) == 0 { lits[next(lits.len() as u64) as usize] } else { alpha[next(alpha.len() as u64) as usize] };
                        // case variants
                        match next(4) {
                            0 => v.extend(c.to_uppercase()),
                            1 => v.extend(c.to_lowercase()),
                            _ => v.push(c),
                        }
                    }
                    if k % 3 == 0 {
                        // the pattern text as a value, as far as it is literal
                        v = pat.chars().filter(|c| !".[]()*+?{}|^$\\:".contains(*c)).collect();
                    }
                    checked += 1;
                    if re.is_match(&v) {
                        matched += 1;
                        if words.iter().any(|w| pat.contains(*w)) && v.chars().any(|c| "\u{301}\u{200d}‿\u{e48}".contains(c)) {
                            wsub += 1;
                        }
                        assert!(pairs.may_match(v.as_bytes()), "round {round}: {pat:?} (ci {ci}) matches {v:?}");
                        assert!(value_passes(&re, Some(&pairs), v.as_bytes()));
                    }
                }
            }
        }
        eprintln!("{with} patterns with pairs, {matched} of {checked} values matched, {wsub} with word boundaries and word-difference characters");
        assert!(with > 1000 && matched > 20_000 && wsub > 50, "{with} patterns with pairs, {matched} of {checked} values matched, {wsub} word");
    }

    /// rv-scan: after diffs that move nodes (to other tiles, across the
    /// antimeridian), create nodes that ways waited for, delete nodes and
    /// change ways and relations: (1) the meta data of every element lies
    /// at its cell, as `meta_in_order` assumes, and `meta_in_order` gives
    /// what `meta` gives; (2) `ways_with_nodes` with its box test gives the
    /// ways that use the nodes; (3) `check_ways` finds no problem.
    #[test]
    fn rv_meta_cells_and_way_boxes_after_diffs() {
        use std::io::BufReader;
        let dir = std::env::temp_dir().join(format!("rv-meta-diffs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let m = |v: u64| format!("version=\"{v}\" timestamp=\"2020-0{}-01T00:00:00Z\" changeset=\"{}\" uid=\"{}\" user=\"u{}\"", 1 + v % 9, 100 + v, 1 + v % 7, 1 + v % 7);
        let mut osm = String::from("<osm version=\"0.6\">\n");
        // 40 x 40 nodes on tile borders and inside tiles near (47, 8); a few near the antimeridian
        let tile_lat = 180.0 / 65536.0;
        let tile_lon = 360.0 / 65536.0;
        let mut id = 0u64;
        for i in 0..40 {
            for j in 0..40 {
                id += 1;
                let (lat, lon) = (47.0 + (i / 2) as f64 * tile_lat + if i % 2 == 0 { 1e-7 } else { tile_lat - 1e-7 }, 8.0 + (j / 2) as f64 * tile_lon + if j % 2 == 0 { 1e-7 } else { tile_lon - 1e-7 });
                osm += &format!("<node id=\"{id}\" {} lat=\"{lat:.7}\" lon=\"{lon:.7}\"/>\n", m(id));
            }
        }
        for k in 0..20u64 {
            osm += &format!("<node id=\"{}\" {} lat=\"{:.7}\" lon=\"{:.7}\"/>\n", 2000 + k, m(k), -16.5 + k as f64 * 0.01, if k % 2 == 0 { 179.9999 } else { -179.9999 });
        }
        // ways: rows, columns, with missing nodes (ids 5000+), one of missing nodes only
        let mut wid = 0u64;
        let mut ways: Vec<Vec<u64>> = Vec::new();
        for i in 0..40u64 {
            ways.push((0..40).map(|j| 1 + i * 40 + j).collect());
            ways.push((0..40).map(|j| 1 + j * 40 + i).collect());
        }
        for k in 0..30u64 {
            ways.push(vec![1 + k * 37 % 1600, 5000 + k, 1 + (k * 53 + 7) % 1600]);
        }
        ways.push(vec![6000, 6001, 6002]);
        ways.push((2000..2020).collect());
        for nds in &ways {
            wid += 1;
            let nd: String = nds.iter().map(|n| format!("<nd ref=\"{n}\"/>")).collect();
            osm += &format!("<way id=\"{wid}\" {}>{nd}<tag k=\"highway\" v=\"path\"/></way>\n", m(wid));
        }
        for r in 1..=60u64 {
            let mem = match r % 4 {
                0 => format!("<member type=\"way\" ref=\"{}\" role=\"\"/><member type=\"node\" ref=\"{}\" role=\"x\"/>", r, r * 13),
                1 => format!("<member type=\"node\" ref=\"{}\" role=\"\"/><member type=\"node\" ref=\"{}\" role=\"\"/>", 5000 + r, r * 7),
                2 => format!("<member type=\"relation\" ref=\"{}\" role=\"\"/>", r + 1),
                _ => String::new(),
            };
            osm += &format!("<relation id=\"{r}\" {}>{mem}<tag k=\"type\" v=\"route\"/></relation>\n", m(r));
        }
        let file = dir.join("in.osm");
        std::fs::write(&file, osm + "</osm>\n").unwrap();
        let db = crate::store::Db::open(&dir.join("db")).unwrap();
        crate::import::import_file(&db, &file, &mut Default::default()).unwrap();
        let apply = |body: String| {
            let b = format!("<osmChange version=\"0.6\">\n{body}</osmChange>\n");
            let mut t = db.write().unwrap();
            let mut r = crate::import::ImportReport::default();
            crate::import::apply_reader(&mut t, BufReader::new(b.as_bytes()), &mut r).unwrap();
            assert!(r.skipped.is_empty(), "{:?}", r.skipped);
            t.commit().unwrap();
        };
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) % n.max(1)
        };
        fn at_cells<T: Stored>(f: &Fetch, all: &[T]) -> usize {
            let mut pairs: Vec<(Cell, Id)> = all.iter().map(|e| (e.cell(), e.id())).collect();
            pairs.sort_unstable();
            let mut found = 0;
            for (v, ids) in f.snap.meta_chunks_at(T::KIND, &pairs).unwrap() {
                let recs = codec::decode_meta(v, Pick::Ids(&ids)).unwrap();
                for r in &recs {
                    // the record lies at the cell of its element
                    if pairs.binary_search(&(r.cell, r.id)).is_ok() {
                        found += 1;
                    }
                }
            }
            found
        }
        let check = |round: usize, next: &mut dyn FnMut(u64) -> u64| {
            let snap = db.snapshot().unwrap();
            let f = Fetch::new(&snap);
            let nodes = f.all::<Node>(|_| true).unwrap();
            let ways = f.all::<Way>(|_| true).unwrap();
            let rels = f.all::<Relation>(|_| true).unwrap();
            assert_eq!(at_cells(&f, &nodes), nodes.len(), "round {round}: nodes");
            assert_eq!(at_cells(&f, &ways), ways.len(), "round {round}: ways");
            assert_eq!(at_cells(&f, &rels), rels.len(), "round {round}: relations");
            fn same<T: Stored>(f: &Fetch, all: &[T], round: usize) {
                let elems: Vec<(Cell, Id)> = all.iter().map(|e| (e.cell(), e.id())).collect();
                let ids: Vec<Id> = all.iter().map(|e| e.id()).collect();
                let map = f.meta(T::KIND, &ids).unwrap();
                let want: Vec<Option<Meta>> = ids.iter().map(|id| map.get(id).copied()).collect();
                assert!(want.iter().all(|m| m.is_some()), "round {round}");
                assert_eq!(f.meta_in_order(T::KIND, &elems).unwrap(), want, "round {round}");
            }
            same(&f, &nodes, round);
            same(&f, &ways, round);
            same(&f, &rels, round);
            for k in 0..30 {
                let step = 1 + next(if k % 2 == 0 { 5 } else { 300 });
                let sub: Vec<(Id, Pt)> = nodes.iter().filter(|_| next(step) == 0).map(|n| (n.id, n.pos)).collect();
                let set: FxHashSet<Id> = sub.iter().map(|s| s.0).collect();
                let want: Vec<Id> = ways.iter().filter(|w| w.nodes.iter().any(|n| set.contains(n))).map(|w| w.id).collect();
                let got: Vec<Id> = f.ways_with_nodes(&sub).unwrap().iter().map(|w| w.id).collect();
                assert_eq!(got, want, "round {round} subset {k}");
            }
            drop(f);
            drop(snap);
            let (_, problems) = crate::update::check::check_ways(&db, 0, 1).unwrap();
            assert!(problems.is_empty(), "round {round}: {problems:?}");
        };
        check(0, &mut next);
        for round in 1..=6 {
            let mut body = String::from("<modify>\n");
            let mut v = 10 + round as u64;
            // nodes moved: a little, to another tile, far away, over the antimeridian
            for _ in 0..60 {
                let n = 1 + next(1600);
                v += 1;
                let (lat, lon) = match next(4) {
                    0 => (47.0 + next(40) as f64 * tile_lat * 0.5 + 1e-7, 8.0 + next(40) as f64 * tile_lon * 0.5 + 1e-7),
                    1 => (47.0 + next(1000) as f64 * 0.001, 8.0 + next(1000) as f64 * 0.001),
                    2 => (-33.0 + next(100) as f64 * 0.01, 151.0 + next(100) as f64 * 0.01),
                    _ => (-16.4, if next(2) == 0 { 179.99999 } else { -179.99999 }),
                };
                body += &format!("<node id=\"{n}\" {} lat=\"{lat:.7}\" lon=\"{lon:.7}\"/>\n", m(v));
            }
            for k in 0..20u64 {
                if next(2) == 0 {
                    let n = 2000 + k;
                    body += &format!("<node id=\"{n}\" {} lat=\"-16.5\" lon=\"{:.7}\"/>\n", m(v + k), if next(2) == 0 { 179.99 } else { -179.99 });
                }
            }
            // nodes that ways and relations waited for
            for _ in 0..5 {
                let n = 5000 + next(70);
                body += &format!("<node id=\"{n}\" {} lat=\"{:.7}\" lon=\"{:.7}\"/>\n", m(v), 46.0 + next(100) as f64 * 0.02, 7.0 + next(100) as f64 * 0.02);
            }
            if round == 3 {
                body += &format!("<node id=\"6001\" {} lat=\"10.0\" lon=\"10.0\"/>\n", m(v));
            }
            // ways with other nodes
            for _ in 0..10 {
                let w = 1 + next(wid);
                let nd: String = (0..2 + next(5)).map(|_| format!("<nd ref=\"{}\"/>", if next(5) == 0 { 5000 + next(70) } else { 1 + next(1600) })).collect();
                body += &format!("<way id=\"{w}\" {}>{nd}<tag k=\"highway\" v=\"track\"/></way>\n", m(v));
            }
            // relations with other members
            for _ in 0..5 {
                let r = 1 + next(60);
                body += &format!("<relation id=\"{r}\" {}><member type=\"way\" ref=\"{}\" role=\"\"/><tag k=\"type\" v=\"route\"/></relation>\n", m(v), 1 + next(wid));
            }
            body += "</modify>\n<delete>\n";
            for _ in 0..5 {
                body += &format!("<node id=\"{}\" {}/>\n", 1 + next(1600), m(v + 1));
            }
            body += "</delete>\n";
            apply(body);
            check(round, &mut next);
        }
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A value the regex matches passes its `FirstPairs`, for patterns with
    /// and without `,i`, classes, alternatives, anchors and non-ASCII letters;
    /// patterns whose matches may start with anything have none.
    #[test]
    fn first_pairs_pass_every_match() {
        use super::super::filter::compile_regex;
        let words = ["Dead Sea", "dead sea", "DEAD SEA", "Red Sea", "Zürich", "ZÜRICH", "zürich", "Bahnhofstrasse", "bahnhof", "Rue de la Paix", "RUE", "rue", "Mini", "MINI", "mını", "İstanbul", "istanbul", "ISTANBUL", "Kelvin", "\u{212a}elvin", "ſea", "улица Ленина", "УЛИЦА", "a", "ab", "", "x", "Straße", "STRASSE", "ǅ", "ǆ", "Ǆ"];
        let mut values: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        for a in words {
            for b in words {
                values.push(format!("{a} {b}"));
                values.push(format!("{a}{b}"));
            }
        }
        let patterns = ["Dead Sea", "dead", "Sea$", "^Red", "(Bahnhof|Station)", "Zürich", "rue", "Mini", "istanbul", "kelvin", "sea", "улица", "str(a|ä)(ss|ß)e", "[Dd]ead", "D.ad", "ea", "x?Sea", "a*b", ".*Sea", "^$", "S", "ǆ", "Straße", "[[:alpha:]]+ Sea", "(Dead|Red) Sea"];
        let mut with = 0;
        let mut dropped = 0;
        for p in patterns {
            for ci in [false, true] {
                let re = compile_regex(p, ci).unwrap();
                let Some(pairs) = FirstPairs::of(&re) else { continue };
                with += 1;
                for v in &values {
                    if re.is_match(v) {
                        assert!(pairs.may_match(v.as_bytes()), "{p} (ci {ci}) matches {v:?}");
                    } else if !pairs.may_match(v.as_bytes()) {
                        dropped += 1;
                    }
                    assert_eq!(value_passes(&re, Some(&pairs), v.as_bytes()), re.is_match(v));
                }
            }
        }
        assert!(with >= 20 && dropped > 10_000, "{with} patterns with pairs, {dropped} values dropped");
        for p in [".*Sea", "a*b", "S", "^$", "[[:alpha:]]+ Sea"] {
            assert!(FirstPairs::of(&compile_regex(p, false).unwrap()).is_none(), "{p}");
        }
    }

    /// `by_cell` gives the order of sorting the pairs, for ids that ascend
    /// (a set), that do not (`out qt`), and for many and few cells.
    #[test]
    fn by_cell_orders_as_a_sort() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) % n
        };
        for round in 0..400 {
            let n = next(if round % 20 == 0 { 5000 } else { 60 }) as usize;
            let cells = 1 + next(if round % 3 == 0 { 3 } else { 300 });
            let mut elems: Vec<(Cell, Id)> = Vec::new();
            let mut id = 0;
            for _ in 0..n {
                id += 1 + next(50);
                let c = next(cells) as u32;
                let cell = if c % 7 == 0 { Cell { level: (c % 16) as u8, code: c << 8 } } else { Cell::tile(c) };
                elems.push((cell, id));
            }
            if round % 4 == 1 {
                // not by id
                for i in (1..elems.len()).rev() {
                    elems.swap(i, next(i as u64 + 1) as usize);
                }
            }
            let got: Vec<(Cell, Id)> = by_cell(&elems).into_iter().map(|i| elems[i as usize]).collect();
            let mut want = elems.clone();
            want.sort_unstable();
            assert_eq!(got, want, "round {round}");
        }
    }

    /// `meta_in_order` gives the meta data `meta` finds, in the order
    /// asked, for elements in any order, with unknown ids, and with cells
    /// that are not those of the elements (the id index answers then).
    #[test]
    fn meta_in_order_as_meta() {
        let dir = std::env::temp_dir().join(format!("fetch-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut osm = String::from("<osm version=\"0.6\">\n");
        let n = 30_000u64;
        for i in 0..n {
            osm += &format!(
                "<node id=\"{}\" version=\"{}\" timestamp=\"2020-01-{:02}T00:00:{:02}Z\" changeset=\"{}\" uid=\"{}\" user=\"u{}\" lat=\"{:.5}\" lon=\"{:.5}\"/>\n",
                i + 1,
                1 + i % 5,
                1 + i % 28,
                i % 60,
                1000 + i / 7,
                1 + i % 40,
                1 + i % 40,
                47.0 + (i % 300) as f64 * 0.001,
                8.0 + (i / 300) as f64 * 0.001
            );
        }
        for w in 0..2000u64 {
            let len = 2 + w % 9 + if w % 100 == 0 { 250 } else { 0 };
            osm += &format!("<way id=\"{}\" version=\"{}\" timestamp=\"2021-02-03T04:05:06Z\" changeset=\"{}\" uid=\"7\" user=\"u7\">", w + 1, 1 + w % 3, 5000 + w);
            for k in 0..len {
                osm += &format!("<nd ref=\"{}\"/>", 1 + (w * 13 + k * (1 + w % 3)) % n);
            }
            osm += "<tag k=\"highway\" v=\"path\"/></way>\n";
        }
        for r in 0..300u64 {
            osm += &format!("<relation id=\"{}\" version=\"2\" timestamp=\"2022-03-04T05:06:07Z\" changeset=\"{}\" uid=\"9\" user=\"u9\">", r + 1, 9000 + r);
            for k in 0..1 + r % 5 {
                osm += &format!("<member type=\"way\" ref=\"{}\" role=\"\"/>", 1 + (r * 7 + k) % 2000);
            }
            osm += "<tag k=\"type\" v=\"route\"/></relation>\n";
        }
        let file = dir.join("in.osm");
        std::fs::write(&file, osm + "</osm>\n").unwrap();
        let db = crate::store::Db::open(&dir.join("db")).unwrap();
        crate::import::import_file(&db, &file, &mut Default::default()).unwrap();
        let snap = db.snapshot().unwrap();
        let f = Fetch::new(&snap);
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) % n
        };
        fn check<T: Stored>(f: &Fetch, all: &[T], next: &mut impl FnMut(u64) -> u64) {
            assert!(all.len() > 100);
            for round in 0..40 {
                let step = 1 + next(if round % 2 == 0 { 3 } else { 200 });
                let mut elems: Vec<(Cell, Id)> = all.iter().filter(|_| next(step) == 0).map(|e| (e.cell(), e.id())).collect();
                if round % 5 == 1 {
                    // ids that do not exist, and elements under another cell
                    for _ in 0..10 {
                        elems.push((Cell::tile(next(1 << 30) as u32), 1_000_000 + next(1000)));
                    }
                    for e in elems.iter_mut().step_by(7) {
                        e.0 = Cell::tile(next(1 << 30) as u32);
                    }
                    elems.sort_unstable_by_key(|e| e.1);
                    elems.dedup_by_key(|e| e.1);
                }
                if round % 5 == 2 {
                    for i in (1..elems.len()).rev() {
                        elems.swap(i, next(i as u64 + 1) as usize);
                    }
                }
                if round % 5 == 3 && !elems.is_empty() {
                    // an element twice
                    elems.push(elems[0]);
                }
                let ids: Vec<Id> = elems.iter().map(|e| e.1).collect();
                let map = f.meta(T::KIND, &ids).unwrap();
                let want: Vec<Option<Meta>> = ids.iter().map(|id| map.get(id).copied()).collect();
                assert_eq!(f.meta_in_order(T::KIND, &elems).unwrap(), want, "round {round}");
                if round % 5 == 0 {
                    assert!(want.iter().all(|m| m.is_some()));
                }
            }
        }
        check(&f, &f.all::<Node>(|_| true).unwrap(), &mut next);
        check(&f, &f.all::<Way>(|_| true).unwrap(), &mut next);
        check(&f, &f.all::<Relation>(|_| true).unwrap(), &mut next);
        drop(snap);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A decode on the pool (`par_run`) stops at the deadline, at a cancel
    /// and at the memory limit of the query, with the error of that limit,
    /// alone on the pool and beside the long section of another query.
    #[test]
    fn decode_stops_at_the_limits_on_the_pool() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::{Relaxed, SeqCst}};
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("fetch-limits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut osm = String::from("<osm version=\"0.6\">\n");
        for i in 0..20_000u64 {
            // a long tag value that does not compress: about 50 nodes a chunk
            let v: String = (0..20u64).map(|k| format!("{:016x}", (i * 31 + k).wrapping_mul(0x9E37_79B9_7F4A_7C15))).collect();
            osm += &format!("<node id=\"{}\" lat=\"{:.4}\" lon=\"{:.4}\"><tag k=\"k\" v=\"{v}\"/></node>\n", i + 1, -80.0 + (i % 400) as f64 * 0.4, -170.0 + (i / 400) as f64 * 0.4);
        }
        let file = dir.join("in.osm");
        std::fs::write(&file, osm + "</osm>\n").unwrap();
        let db = crate::store::Db::open(&dir.join("db")).unwrap();
        crate::import::import_file(&db, &file, &mut Default::default()).unwrap();
        let snap = db.snapshot().unwrap();
        let chunks = snap.all_chunks(Kind::Node).unwrap().len();
        assert!(chunks >= 100, "{chunks} chunks");
        let per = Fetch::new(&snap).all::<Node>(|_| true).unwrap()[0].mem_size().1;
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            for beside in [false, true] {
                if beside {
                    s.spawn(|| {
                        while !stop.load(SeqCst) {
                            let items: Vec<u64> = (0..4000).collect();
                            crate::exec::par_map(&items, 2, |&x| {
                                std::thread::sleep(std::time::Duration::from_micros(200));
                                x
                            });
                        }
                    });
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let mut f = Fetch::new(&snap);
                assert_eq!(f.all::<Node>(|_| true).unwrap().len(), 20_000);
                let seen = AtomicUsize::new(0);
                let count = |_: &Node| {
                    seen.fetch_add(1, Relaxed);
                    true
                };
                f.limits.deadline = Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
                let e = f.all::<Node>(count).unwrap_err();
                assert!(e.downcast_ref::<Interrupted>().is_some(), "{e}");
                assert_eq!(seen.swap(0, Relaxed), 0, "decoded after the deadline (beside: {beside})");
                f.limits.deadline = None;
                f.limits.cancel = Some(Arc::new(AtomicBool::new(true)));
                let e = f.all::<Node>(count).unwrap_err();
                assert!(e.downcast_ref::<Interrupted>().is_some(), "{e}");
                assert_eq!(seen.swap(0, Relaxed), 0, "decoded after the cancel (beside: {beside})");
                f.limits.cancel = None;
                // room for about 100 nodes
                f.limits.mem = Some(Arc::new(MemBudget::new(u64::MAX, 100 * per)));
                let e = f.all::<Node>(count).unwrap_err();
                assert!(e.downcast_ref::<OutOfMemory>().is_some(), "{e}");
                let n = seen.swap(0, Relaxed);
                assert!(n < 1000, "{n} nodes decoded with room for 100 (beside: {beside})");
            }
            stop.store(true, SeqCst);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A key alone lists every element that has it: the list counts
    /// against the memory limit of the query, on one thread and on several.
    #[test]
    fn tag_index_candidates_count_against_the_memory_limit() {
        let dir = std::env::temp_dir().join(format!("fetch-candidates-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut osm = String::from("<osm version=\"0.6\">\n");
        for i in 1..=5000u64 {
            osm += &format!("<node id=\"{i}\" lat=\"{:.4}\" lon=\"{:.4}\"><tag k=\"k\" v=\"v{}\"/></node>\n", 10.0 + (i % 70) as f64 * 0.01, 20.0 + (i / 70) as f64 * 0.01, i % 50);
        }
        let file = dir.join("in.osm");
        std::fs::write(&file, osm + "</osm>\n").unwrap();
        let db = crate::store::Db::open(&dir.join("db")).unwrap();
        crate::import::import_file(&db, &file, &mut Default::default()).unwrap();
        let snap = db.snapshot().unwrap();
        let mut f = Fetch::new(&snap);
        let t = TagFilter::HasKey("k".into());
        for par in [false, true] {
            // more than one value: the scan on several threads
            PAR_SCAN_VALUES.set(par.then_some(1));
            PAR_SCANS.set(0);
            f.limits.mem = Some(std::sync::Arc::new(MemBudget::new(u64::MAX, 1 << 30)));
            assert_eq!(f.tag_candidates(Kind::Node, &t, None).unwrap().len(), 5000);
            assert_eq!(f.tag_candidates_in(Kind::Node, &t, None, usize::MAX).unwrap().unwrap().len(), 5000);
            assert_eq!(PAR_SCANS.get() > 0, par);
            // room for 1000 pairs
            for run in 0..2 {
                f.limits.mem = Some(std::sync::Arc::new(MemBudget::new(u64::MAX, 1000 * PAIR_BYTES)));
                let r = if run == 0 { f.tag_candidates(Kind::Node, &t, None).map(|p| p.len()) } else { f.tag_candidates_in(Kind::Node, &t, None, usize::MAX).map(|p| p.map_or(0, |p| p.len())) };
                match r {
                    Err(e) => assert!(e.downcast_ref::<OutOfMemory>().is_some(), "{e}"),
                    Ok(n) => panic!("{n} candidates read with room for 1000 (several threads: {par})"),
                }
            }
        }
        PAR_SCAN_VALUES.set(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// The ranges hold every cell that may hold an element meeting one of
    /// the rectangles (`cell_meets`), computed cell by cell: random sets of
    /// 1 to 300 rectangles (alone through `level_ranges`, more than 32
    /// through `tile_runs`, `join_rows` and `level_ranges_many`), from points
    /// to the whole key space, many at its ends in longitude (the
    /// antimeridian, boxes across it as two rectangles) and bands of the
    /// whole width. Every cell of the levels up to 7; above, the cells around
    /// the corners of 40 rectangles and random ones inside and anywhere.
    /// Each `level_ranges` and `level_ranges_many` holds the cells of its
    /// rectangles as well.
    #[test]
    fn ranges_hold_every_cell_of_the_rectangles() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) % n.max(1)
        };
        let cell = |level: u8, cy: u32, cx: u32| -> Cell {
            if level == 0 {
                return Cell { level: 0, code: 0 };
            }
            let s = 16 - level as u32;
            Cell { level, code: crate::geo::morton(cy << s, cx << s) }
        };
        for case in 0..48u64 {
            let n = [1, 3, 20, 33, 60, 300][(case % 6) as usize];
            let mut rects: Vec<Rect> = Vec::new();
            while rects.len() < n {
                let size = |rnd: &mut dyn FnMut(u64) -> u64| -> u64 { (1u64 << rnd(33)) - 1 + rnd(2) };
                let (h, w) = (size(&mut rnd), size(&mut rnd));
                let y0 = rnd(u32::MAX as u64 + 1);
                let y1 = (y0 + h).min(u32::MAX as u64);
                let (x0, x1) = match rnd(6) {
                    // at the west or east end
                    0 => (0, w.min(u32::MAX as u64)),
                    1 => ((u32::MAX as u64).saturating_sub(w), u32::MAX as u64),
                    // the whole width
                    2 => (0, u32::MAX as u64),
                    _ => {
                        let x0 = rnd(u32::MAX as u64 + 1);
                        (x0, (x0 + w).min(u32::MAX as u64))
                    }
                };
                let r = Rect { y0: y0 as u32, x0: x0 as u32, y1: y1 as u32, x1: x1 as u32 };
                if rnd(4) == 0 {
                    // a box across the antimeridian
                    let (s, wl, d) = (rnd(170) as f64 - 85.0, 180.0 - rnd(30_000) as f64 / 1000.0, rnd(5_000) as f64 / 1000.0);
                    rects.extend(crate::geo::BBox { south: s, west: wl, north: s + d, east: -180.0 + d }.rects());
                } else {
                    rects.push(r);
                }
            }
            if case % 5 == 0 {
                rects.push(Rect::EMPTY);
            }
            for kind in [Kind::Node, Kind::Way] {
                let ranges = ranges_for(kind, &rects);
                let levels: Vec<u8> = if kind == Kind::Node { vec![MAX_LEVEL] } else { (0..=MAX_LEVEL).collect() };
                for &level in &levels {
                    let loose = kind != Kind::Node;
                    let many = level_ranges_many(level, &rects, loose);
                    let each: Vec<Vec<CodeRange>> = rects.iter().map(|r| level_ranges(level, r, loose)).collect();
                    let check = |c: Cell| {
                        if !cell_meets(kind, c, &rects) {
                            return;
                        }
                        let ok = cell_in_ranges(c, &ranges) && cell_in_ranges(c, &many);
                        assert!(ok, "case {case} {kind:?} {c:?} ranges_for {} many {}", cell_in_ranges(c, &ranges), cell_in_ranges(c, &many));
                        for (r, e) in rects.iter().zip(&each) {
                            if cell_meets(kind, c, std::slice::from_ref(r)) {
                                assert!(cell_in_ranges(c, e), "case {case} {kind:?} {c:?} {r:?}");
                            }
                        }
                    };
                    if level <= 7 {
                        for cy in 0..1u32 << level {
                            for cx in 0..1u32 << level {
                                check(cell(level, cy, cx));
                            }
                        }
                        continue;
                    }
                    let shift = 32 - level as u32;
                    let top = (1u64 << level) - 1;
                    let mut cells = Vec::new();
                    for r in rects.iter().filter(|r| !r.is_empty()).take(40) {
                        for (y, x) in [(r.y0, r.x0), (r.y0, r.x1), (r.y1, r.x0), (r.y1, r.x1)] {
                            for dy in -2i64..=2 {
                                for dx in -2i64..=2 {
                                    let cy = ((y >> shift) as i64 + dy).clamp(0, top as i64) as u32;
                                    let cx = ((x >> shift) as i64 + dx).clamp(0, top as i64) as u32;
                                    cells.push((cy, cx));
                                }
                            }
                        }
                        for _ in 0..8 {
                            let cy = (r.y0 >> shift) as u64 + rnd(((r.y1 >> shift) - (r.y0 >> shift)) as u64 + 1);
                            let cx = (r.x0 >> shift) as u64 + rnd(((r.x1 >> shift) - (r.x0 >> shift)) as u64 + 1);
                            cells.push((cy as u32, cx as u32));
                        }
                    }
                    for _ in 0..200 {
                        cells.push((rnd(top + 1) as u32, rnd(top + 1) as u32));
                    }
                    for (cy, cx) in cells {
                        check(cell(level, cy, cx));
                    }
                }
            }
        }
    }
}
