//! Query execution engine.
//!
//! Semantics follow docs/spec/query-semantics.md: sets live in a stack of
//! frames, block statements push frames and leak their sets back out.

pub mod area;
pub mod areageom;
pub mod eval;
pub mod fetch;
pub mod filter;
pub mod geom;
pub mod print;
pub mod query;
pub mod recurse;
pub mod set;
mod towupper;

use crate::output::Printer;
use crate::ql::ast::*;
use crate::store::{Db, Snapshot};
use anyhow::Result;
use fetch::Fetch;
use set::Set;
use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// A stored set with its size as counted for the maxsize limit.
#[derive(Clone)]
struct Slot {
    set: Rc<Set>,
    /// Size as the C++ resource manager counts it (`maxsize`).
    size: u64,
    /// `size` plus derived elements, which C++ only meets through its
    /// address-space limit.
    bytes: u64,
}

fn derived_bytes(s: &Set) -> u64 {
    s.derived.iter().map(set::Derived::approx_bytes).sum()
}

/// Error that aborts query execution.
#[derive(Debug)]
pub enum RunError {
    /// Parse or static error: nothing was written.
    Rejected(String),
    /// Runtime error after output started (already embedded in the output).
    Runtime(String),
    Io(std::io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Rejected(m) | RunError::Runtime(m) => f.write_str(m),
            RunError::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Fatal runtime condition raised during execution.
#[derive(Debug)]
pub struct Abort(pub String);

impl std::fmt::Display for Abort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Abort {}

pub struct RunOptions {
    /// Maximum timeout a query may request.
    pub max_timeout: Option<u32>,
    /// Maximum `[maxsize:]` a query may request.
    pub max_maxsize: Option<u64>,
    /// Set when the client went away; the query stops at the next check.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Report non-fatal errors to this callback (CLI prints them to stderr).
    pub cli: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions { max_timeout: None, max_maxsize: None, cancel: None, cli: false }
    }
}

pub struct Engine<'s, W: Write> {
    pub f: Fetch<'s>,
    frames: Vec<HashMap<String, Slot>>,
    /// Keys stored on sets (`for` writes `val`).
    pub set_keys: HashMap<String, HashMap<String, String>>,
    pub p: Printer<W>,
    pub settings: Settings,
    started: Instant,
    timeout: Duration,
    pub derived_counter: u64,
    pub area_level: bool,
    cur_stmt: (&'static str, usize),
    /// Size (as `Slot::bytes`) of the sets that running `union` statements
    /// hold outside the frames: their parts and what they merged so far.
    held_bytes: u64,
    pub cli: bool,
    pub messages: Vec<String>,
    /// The polygons of the last `(area)` clause, with its input set (weak:
    /// the set may go, the polygons then no longer match) and id
    /// (`Engine::shared_area_ctx`).
    area_ctx_last: Option<(std::rc::Weak<Set>, Option<crate::model::Id>, Rc<area::AreaCtx>)>,
    /// The cell ranges of those polygons per element kind and edge kind
    /// (`Engine::area_ranges`).
    area_ranges_last: Vec<(crate::store::Kind, bool, Rc<Vec<crate::geo::CodeRange>>)>,
    /// Bytes of the two above beyond the database areas, counted against
    /// the memory limit while they are kept (at most `AREA_KEEP_MAX`).
    area_kept_bytes: u64,
}

pub fn stmt_name(k: &StmtKind) -> &'static str {
    match k {
        StmtKind::Query { types, filters } => query::standalone_name(types, filters).unwrap_or("query"),
        StmtKind::Union(_) => "union",
        StmtKind::Difference(..) => "difference",
        StmtKind::Recurse { .. } => "recurse",
        StmtKind::Item { .. } => "item",
        StmtKind::Out { .. } => "print",
        StmtKind::Foreach { .. } => "foreach",
        StmtKind::For { .. } => "for",
        StmtKind::If { .. } => "if",
        StmtKind::Complete { .. } => "complete",
        StmtKind::IsIn { .. } => "coord-query",
        StmtKind::MapToArea { .. } => "map-to-area",
        StmtKind::Make { .. } => "make",
        StmtKind::Convert { .. } => "convert",
        StmtKind::Unsupported(_) => "unsupported",
    }
}

/// Whether the script needs area support (decides area output and headers).
pub fn uses_areas(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| match &s.kind {
        StmtKind::IsIn { .. } | StmtKind::MapToArea { .. } => true,
        StmtKind::Query { types, filters } => types.area || filters.iter().any(|f| matches!(f, Filter::Area { .. })),
        StmtKind::Union(v) => uses_areas(v),
        StmtKind::Difference(a, b) => uses_areas(std::slice::from_ref(a)) || uses_areas(std::slice::from_ref(b)),
        StmtKind::Foreach { body, .. } | StmtKind::For { body, .. } | StmtKind::Complete { body, .. } => uses_areas(body),
        StmtKind::If { then, els, .. } => uses_areas(then) || uses_areas(els),
        _ => false,
    })
}

/// Parse and run a query, writing the document to `w`.
pub fn run<W: Write>(db: &Db, src: &str, w: W, opts: &RunOptions) -> std::result::Result<Printer<W>, (Option<Printer<W>>, RunError)> {
    let script = match crate::ql::parse(src) {
        Ok(s) => s,
        Err(e) => return Err((None, RunError::Rejected(e.to_string()))),
    };
    let snap = match db.snapshot() {
        Ok(s) => s,
        Err(e) => return Err((None, RunError::Rejected(format!("runtime error: {e}")))),
    };
    run_script(&snap, script, w, opts, false)
}

/// Queries running in this process.
static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct ActiveGuard;

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Worker threads for parallel sections (0 = not set: all cores).
static THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Set the size of the thread pool used for parallel sections.
pub fn set_threads(n: usize) {
    THREADS.store(n.max(1), std::sync::atomic::Ordering::Relaxed);
}

/// Pool size: as set, else half the CPUs of this process, at most 8.
/// Larger pools cost more in idle spinning of the workers between parallel
/// sections than they gain (measured on 16 threads: 8 is fastest).
fn threads() -> usize {
    match THREADS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => (cpus() / 2).clamp(1, 8),
        n => n,
    }
}

/// CPUs this process may use: its affinity mask and the CPU quota of its
/// cgroup (a container with `--cpus`), as `available_parallelism` reads
/// them. Read once.
fn cpus() -> usize {
    static CPUS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CPUS.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// CPUs of the host, which the run queue of /proc/loadavg is spread over
/// (inside a container too). Read once; `cpus()` if unknown.
fn host_cpus() -> usize {
    static HOST: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *HOST.get_or_init(|| std::fs::read_to_string("/sys/devices/system/cpu/online").ok().and_then(|s| count_cpus(&s)).unwrap_or_else(cpus).max(cpus()))
}

/// Number of CPUs in a list such as `0-23` or `0-3,8-11,16`.
fn count_cpus(list: &str) -> Option<usize> {
    let mut n = 0;
    for part in list.trim().split(',') {
        n += match part.split_once('-') {
            Some((a, b)) => b.parse::<usize>().ok()?.checked_sub(a.parse::<usize>().ok()?)? + 1,
            None => part.parse::<usize>().map(|_| 1).ok()?,
        };
    }
    (n > 0).then_some(n)
}

/// Runnable threads on the host (this one included), the fourth field of
/// /proc/loadavg (`runnable/total`). Read at most every 50 ms; 0 if
/// unknown.
fn runnable() -> usize {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CHECKED: AtomicU64 = AtomicU64::new(0);
    static RUNNABLE: AtomicU64 = AtomicU64::new(0);
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let now = START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1;
    let last = CHECKED.load(Ordering::Relaxed);
    if last == 0 || now.saturating_sub(last) >= 50 {
        CHECKED.store(now, Ordering::Relaxed);
        let n = std::fs::read_to_string("/proc/loadavg").ok().and_then(|s| s.split_whitespace().nth(3)?.split('/').next()?.parse().ok()).unwrap_or(0);
        RUNNABLE.store(n, Ordering::Relaxed);
    }
    RUNNABLE.load(Ordering::Relaxed) as usize
}

/// What `spread` decides on.
#[derive(Clone, Copy, Debug)]
struct Load {
    /// Threads of the pool.
    threads: usize,
    /// Queries running in this process, the asking one included.
    active: usize,
    /// CPUs this process may use (`cpus`).
    cpus: usize,
    /// CPUs of the host (`host_cpus`).
    host_cpus: usize,
    /// Runnable threads on the host, the asking one included (`runnable`).
    runnable: usize,
}

/// Whether a query may hand a parallel section to the pool: the pool must
/// find as many idle CPUs as it can use (its threads, at most the CPUs of
/// this process) on the host, where other processes run (e.g. queries
/// run as separate processes), and among the CPUs of this process, where
/// its other queries keep one each; and at most half as many queries as
/// the pool has threads may run in this process. The asking thread waits
/// while the pool works, so it does not count. Handing work to a saturated
/// machine only adds waiting; on a host with more CPUs than the pool, a
/// few other runnable threads leave room for it.
fn spread(l: Load) -> bool {
    if l.threads < 2 || l.active * 2 > l.threads {
        return false;
    }
    let need = l.threads.min(l.cpus);
    let host_idle = l.host_cpus.saturating_sub(l.runnable.saturating_sub(1));
    let own_idle = l.cpus.saturating_sub(l.active.saturating_sub(1));
    host_idle >= need && own_idle >= need
}

/// Whether a query may spread work over the thread pool (`spread`). Starts
/// the pool on first use. Outside a query (`make-areas` runs as a process
/// of its own) the work stays on the calling thread, as in 0.2.8: there the
/// pool would not take turns with the queries of the server. The unit tests
/// call sections directly.
fn parallel_ok() -> bool {
    static POOL: std::sync::Once = std::sync::Once::new();
    let active = ACTIVE.load(std::sync::atomic::Ordering::Relaxed);
    if active == 0 && !cfg!(test) {
        return false;
    }
    let threads = threads();
    let ok = spread(Load { threads, active, cpus: cpus(), host_cpus: host_cpus(), runnable: runnable() });
    if ok {
        // The pool starts on first use (fails harmlessly if already built).
        POOL.call_once(|| {
            let _ = rayon::ThreadPoolBuilder::new().num_threads(threads).build_global();
        });
    }
    ok
}

/// How long a section keeps the thread pool before it gives way to the
/// section of another query that waits for it, and how long a section
/// works on its own thread before it waits for the pool (`par_run`).
const TURN: std::time::Duration = std::time::Duration::from_millis(10);

/// About how long a pool thread works on one run of items of a section
/// before it takes the next: a section gives way within about this after
/// its turn, and its last runs end about together.
const RUN: std::time::Duration = std::time::Duration::from_millis(1);

/// Sections on the thread pool now, of all queries: from when they queue
/// for it until their tasks end.
static ON_POOL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Counts a section on the pool (`ON_POOL`) while it lives.
struct OnPool;

impl OnPool {
    fn enter() -> OnPool {
        ON_POOL.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        OnPool
    }
}

impl Drop for OnPool {
    fn drop(&mut self) {
        ON_POOL.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Stops the other tasks of a section when a task panics (the panic
/// reaches the caller when they have ended).
struct StopOnPanic<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// `f` over `items`, each result handed to `sink` in input order; stops at
/// the first error. All parallel work of a query goes through here
/// (`par_parts`).
pub fn par_run<I: Sync, T: Send, E: Send>(items: &[I], min: usize, f: impl Fn(&I) -> std::result::Result<T, E> + Sync, mut sink: impl FnMut(T) -> std::result::Result<(), E>) -> std::result::Result<(), E> {
    for part in par_parts(items, min, &f, &mut sink)? {
        for r in part {
            sink(r)?;
        }
    }
    Ok(())
}

/// `f` over `items`, results in input order (`par_parts`).
pub fn par_map<I: Sync, T: Send>(items: &[I], min: usize, f: impl Fn(&I) -> T + Sync) -> Vec<T> {
    let mut out = Vec::with_capacity(items.len());
    let parts: std::result::Result<Vec<Vec<T>>, std::convert::Infallible> = par_parts(items, min, &|it| Ok(f(it)), |r| {
        out.push(r);
        Ok(())
    });
    let Ok(parts) = parts;
    for part in parts {
        out.extend(part);
    }
    out
}

/// `f` over `items`, stopping at the first error. The results of the items
/// that run on the calling thread go to `one` as they come; those of the
/// thread pool follow them in the parts returned, in input order.
///
/// Sections of at least `min` items run on the thread pool while the host
/// has room for it (`parallel_ok`). Rayon takes a job from the queue of
/// the pool only when the jobs it runs have no work left to hand out, so
/// the section of a light query used to wait for the whole running section
/// of another one (seconds). Hence: while another section is on the pool,
/// a section starts on its own thread, where a small one ends; after TURN
/// it queues for the pool, and a section on the pool that has had it for
/// TURN stops taking items when another one is there, and queues the rest
/// behind it. Sections take turns of about TURN; one alone on the pool
/// runs in one go.
///
/// On the pool a section is one task per pool thread (the first one queues
/// the others). Each takes runs of items that take about RUN (one slow
/// item, a chunk read from disk, holds one thread, not the others), at
/// most 1/64 of its share and smaller toward the end, until none are left
/// or it gives way. The
/// tasks never wait for each other: a pool thread that waits (as in a
/// rayon join) runs any queued job meanwhile, also the whole section of
/// another query, and the section it waits in ends only after that.
pub fn par_parts<I: Sync, T: Send, E: Send>(items: &[I], min: usize, f: &(impl Fn(&I) -> std::result::Result<T, E> + Sync), mut one: impl FnMut(T) -> std::result::Result<(), E>) -> std::result::Result<Vec<Vec<T>>, E> {
    use std::sync::atomic::Ordering::{Relaxed, SeqCst};
    use std::sync::Mutex;
    if items.len() < min.max(2) || rayon::current_thread_index().is_some() {
        // small, or a section inside a section: on a pool thread it would
        // have to wait for the others (see above)
        for it in items {
            one(f(it)?)?;
        }
        return Ok(Vec::new());
    }
    let mut done = 0;
    if ON_POOL.load(SeqCst) > 0 {
        let start = std::time::Instant::now();
        while done < items.len() && ON_POOL.load(Relaxed) > 0 && start.elapsed() < TURN {
            one(f(&items[done])?)?;
            done += 1;
        }
        // a rest that takes less than another TURN at this rate ends here
        let left = (items.len() - done) as u128;
        if done > 0 && start.elapsed().as_nanos() * left < TURN.as_nanos() * done as u128 {
            for it in &items[done..] {
                one(f(it)?)?;
            }
            return Ok(Vec::new());
        }
    }
    let rest = &items[done..];
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    if !parallel_ok() {
        // the host is busy
        for it in rest {
            one(f(it)?)?;
        }
        return Ok(Vec::new());
    }
    let n = rest.len();
    let tasks = rayon::current_num_threads().min(n);
    // runs of at most 1/64 of the share of a thread (16 items at least): the
    // cost of items can change along a section (a city of long ways after
    // open land), and a run grown on cheap items must not take a whole
    // stretch of dear ones (1/16 cost the around tests of a railway query 8%)
    let most = (n / (tasks * 64)).max(16);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let parts: Mutex<Vec<(usize, Vec<T>)>> = Mutex::new(Vec::new());
    let failed: Mutex<Option<E>> = Mutex::new(None);
    let stop = std::sync::atomic::AtomicBool::new(false);
    // a section that gave way keeps the pool: the host load is checked
    // once, as for a section that runs in one go
    while next.load(SeqCst) < n && !stop.load(SeqCst) {
        let on = OnPool::enter();
        let started = std::sync::OnceLock::new();
        let (next, parts, failed, stop, started) = (&next, &parts, &failed, &stop, &started);
        let work = move || {
            let _panic = StopOnPanic(stop);
            let turn = *started.get_or_init(std::time::Instant::now);
            let mut mine = Vec::new();
            let mut k = 1;
            'runs: while !stop.load(Relaxed) && (ON_POOL.load(Relaxed) < 2 || turn.elapsed() < TURN) {
                // toward the end, at most half the share of a thread
                // of what is left: the last runs end about together
                let left = n.saturating_sub(next.load(Relaxed));
                let take = k.min(most).min((left / (2 * tasks)).max(1));
                let c = next.fetch_add(take, SeqCst);
                if c >= n {
                    break;
                }
                let end = (c + take).min(n);
                let t = std::time::Instant::now();
                let mut part = Vec::with_capacity(end - c);
                for it in &rest[c..end] {
                    match f(it) {
                        Ok(r) => part.push(r),
                        Err(e) => {
                            failed.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert(e);
                            stop.store(true, Relaxed);
                            break 'runs;
                        }
                    }
                }
                mine.push((c, part));
                let took = t.elapsed();
                k = if took < RUN / 2 {
                    (take * 2).min(most)
                } else if took > RUN * 2 {
                    (take / 2).max(1)
                } else {
                    take
                };
            }
            parts.lock().unwrap_or_else(|p| p.into_inner()).extend(mine);
        };
        let work = &work;
        rayon::in_place_scope(|s| {
            // one task from this thread (one wake-up, as for a rayon job); it
            // queues the others on its pool thread, where idle threads take
            // them
            s.spawn(move |s| {
                for _ in 1..tasks {
                    s.spawn(move |_| work());
                }
                work();
            });
        });
        drop(on);
        if let Some(e) = failed.lock().unwrap_or_else(|p| p.into_inner()).take() {
            return Err(e);
        }
    }
    let mut parts = parts.into_inner().unwrap_or_else(|p| p.into_inner());
    parts.sort_unstable_by_key(|p| p.0);
    Ok(parts.into_iter().map(|p| p.1).collect())
}

pub fn run_script<W: Write>(snap: &Snapshot, script: Script, w: W, opts: &RunOptions, jsonp: bool) -> std::result::Result<Printer<W>, (Option<Printer<W>>, RunError)> {
    let mut script = script;
    query::fuse_filter_pairs(&mut script.stmts);
    ACTIVE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _active = ActiveGuard;
    let area_level = uses_areas(&script.stmts);
    let ts = snap.info("osm_base").ok().flatten().unwrap_or_default();
    let area_ts = if area_level { Some(snap.info("area_base").ok().flatten().unwrap_or_else(|| ts.clone())) } else { None };
    let mut timeout = script.settings.timeout;
    if let Some(m) = opts.max_timeout {
        timeout = timeout.min(m);
    }
    let mut p = Printer::new(w, script.settings.out.clone());
    p.area_level = area_level;
    p.jsonp = jsonp;
    p.header(&ts, area_ts.as_deref());
    if let Some(b) = &script.settings.bbox {
        if b.south <= b.north {
            p.global_bbox(&(b.south, b.west, b.north, b.east));
        }
    }
    let started = Instant::now();
    let mut fetch = Fetch::new(snap);
    let mut settings = script.settings.clone();
    if let Some(m) = opts.max_maxsize {
        settings.maxsize = settings.maxsize.min(m);
    }
    let mem = std::sync::Arc::new(fetch::MemBudget::new(settings.maxsize, memory_limit(settings.maxsize)));
    fetch.limits = fetch::Limits { deadline: Some(started + Duration::from_secs(timeout as u64)), cancel: opts.cancel.clone(), mem: Some(mem) };
    fetch::set_thread_limits(fetch.limits.clone());
    let mut eng = Engine {
        f: fetch,
        frames: vec![HashMap::new()],
        set_keys: HashMap::new(),
        p,
        settings,
        started,
        timeout: Duration::from_secs(timeout as u64),
        derived_counter: 0,
        area_level,
        cur_stmt: ("", 0),
        held_bytes: 0,
        cli: opts.cli,
        messages: Vec::new(),
        area_ctx_last: None,
        area_ranges_last: Vec::new(),
        area_kept_bytes: 0,
    };
    let res = eng.exec_block(&script.stmts);
    let mut err = None;
    if let Err(e) = res {
        let msg = match e.downcast_ref::<Abort>() {
            Some(a) => a.0.clone(),
            None if e.downcast_ref::<fetch::Interrupted>().is_some() => eng.interrupted_message(),
            None if e.downcast_ref::<fetch::OutOfMemory>().is_some() => eng.out_of_memory_message(e.downcast_ref::<fetch::OutOfMemory>().unwrap()),
            None => format!("Query failed with the exception: {e}"),
        };
        let text = format!("runtime error: {msg}");
        eng.report(&text);
        err = Some(text);
    }
    if let Err(e) = eng.p.footer() {
        return Err((None, RunError::Io(e)));
    }
    match err {
        Some(m) => Err((Some(eng.p), RunError::Runtime(m))),
        None => Ok(eng.p),
    }
}

/// C++ runs queries under an address-space limit of 2 * maxsize + 1 GiB.
fn memory_limit(maxsize: u64) -> u64 {
    maxsize.saturating_mul(2).saturating_add(1 << 30)
}

impl<'s, W: Write> Engine<'s, W> {
    /// Non-fatal error or remark: embedded in the output (HTTP) or kept
    /// for stderr (CLI).
    pub fn report(&mut self, text: &str) {
        if self.cli {
            eprintln!("{text}");
            self.messages.push(text.to_string());
        } else {
            self.p.remark(text);
        }
    }

    pub fn lookup(&self, name: &str) -> Option<&Rc<Set>> {
        self.frames.iter().rev().find_map(|f| f.get(name)).map(|s| &s.set)
    }

    pub fn get(&self, name: &str) -> Rc<Set> {
        self.lookup(name).cloned().unwrap_or_default()
    }

    fn lookup_from(&self, depth: usize, name: &str) -> Option<&Rc<Set>> {
        self.frames[..depth].iter().rev().find_map(|f| f.get(name)).map(|s| &s.set)
    }

    pub fn set(&mut self, name: &str, s: Set) {
        self.set_rc(name, Rc::new(s));
    }

    pub fn set_rc(&mut self, name: &str, mut s: Rc<Set>) {
        if !s.derived.is_sorted_by_key(|d| d.id) {
            Rc::make_mut(&mut s).derived.sort_by_key(|d| d.id);
        }
        self.set_keys.remove(name);
        let size = s.counted_size();
        let bytes = size + derived_bytes(&s);
        self.frames.last_mut().unwrap().insert(name.to_string(), Slot { set: s, size, bytes });
    }

    fn interrupted_message(&self) -> String {
        if self.f.limits.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            return "Query cancelled: the client closed the connection.".into();
        }
        format!("Query timed out in \"{}\" at line {} after {} seconds.", self.cur_stmt.0, self.cur_stmt.1, self.started.elapsed().as_secs())
    }

    /// Periodic resource check (timeout and cancellation).
    pub fn check(&self) -> Result<()> {
        if self.started.elapsed() > self.timeout || self.f.limits.expired() {
            return Err(Abort(self.interrupted_message()).into());
        }
        Ok(())
    }

    /// C++ runs queries under an address-space limit of 2 * maxsize + 1 GiB.
    fn memory_limit(&self) -> u64 {
        memory_limit(self.settings.maxsize)
    }

    /// The C++ message for a statement that passed a memory limit while
    /// collecting data.
    fn out_of_memory_message(&self, m: &fetch::OutOfMemory) -> String {
        if m.counted {
            format!(
                "Query ran out of memory in \"{}\" at line {}. It would need at least {} MB of RAM to continue.",
                self.cur_stmt.0,
                self.cur_stmt.1,
                m.counted_size >> 20
            )
        } else {
            format!("Query run out of memory using about {} MB of RAM.", self.memory_limit() >> 20)
        }
    }

    /// Set the memory budget base to the stored sets (after a statement).
    fn reset_mem_budget(&self) {
        if let Some(m) = &self.f.limits.mem {
            let size: u64 = self.frames.iter().flat_map(|f| f.values()).map(|s| s.size).sum();
            let bytes: u64 = self.frames.iter().flat_map(|f| f.values()).map(|s| s.bytes).sum();
            m.reset(size, bytes.saturating_add(self.held_bytes).saturating_add(self.area_kept_bytes));
        }
    }

    /// Fail like the C++ allocation failure when sets plus `extra` bytes
    /// exceed the memory limit.
    pub fn check_memory(&self, extra: u64) -> Result<()> {
        let bytes: u64 = self.frames.iter().flat_map(|f| f.values()).map(|s| s.bytes).sum();
        if bytes.saturating_add(self.held_bytes).saturating_add(self.area_kept_bytes).saturating_add(extra) > self.memory_limit() {
            return Err(Abort(format!("Query run out of memory using about {} MB of RAM.", self.memory_limit() >> 20)).into());
        }
        Ok(())
    }

    fn check_size(&self) -> Result<()> {
        self.check_memory(0)?;
        let size: u64 = self.frames.iter().flat_map(|f| f.values()).map(|s| s.size).sum();
        if size > self.settings.maxsize {
            return Err(Abort(format!(
                "Query ran out of memory in \"{}\" at line {}. It would need at least {} MB of RAM to continue.",
                self.cur_stmt.0,
                self.cur_stmt.1,
                size >> 20
            ))
            .into());
        }
        Ok(())
    }

    pub fn exec_block(&mut self, stmts: &[Stmt]) -> Result<()> {
        for s in stmts {
            self.exec(s)?;
        }
        Ok(())
    }

    pub fn exec(&mut self, s: &Stmt) -> Result<()> {
        self.cur_stmt = (stmt_name(&s.kind), s.line);
        self.check()?;
        match &s.kind {
            StmtKind::Query { types, filters } => {
                let r = self.query(types, filters)?;
                self.set(&s.into, r);
            }
            StmtKind::Item { set } => {
                if set != &s.into {
                    let v = self.get(set);
                    self.set_rc(&s.into, v);
                }
            }
            StmtKind::Union(stmts) => self.union(stmts, &s.into, s.line)?,
            StmtKind::Difference(a, b) => self.difference(a, b, &s.into)?,
            StmtKind::Recurse { op, input } => {
                let inp = self.get(input);
                let r = self.recurse(*op, &inp)?;
                self.set(&s.into, r);
            }
            StmtKind::Out { input, opts } => {
                let inp = self.get(input);
                self.print(&inp, opts)?;
            }
            StmtKind::Foreach { input, var, body } => self.foreach(input, var.as_deref().unwrap_or("_"), body)?,
            StmtKind::Complete { input, var, limit, body } => self.complete(input, var.as_deref().unwrap_or("_"), limit.unwrap_or(4096), body)?,
            StmtKind::For { input, var, key, body } => self.for_loop(input, var.as_deref().unwrap_or("_"), key, body)?,
            StmtKind::If { cond, then, els } => {
                let v = self.eval_global(cond)?;
                if eval::num::truthy(&v) {
                    self.exec_block(then)?;
                } else {
                    self.exec_block(els)?;
                }
            }
            StmtKind::Make { name, entries } => {
                let r = self.make(name, entries)?;
                self.set(&s.into, r);
            }
            StmtKind::Convert { input, name, entries } => {
                let inp = self.get(input);
                let r = self.convert(&inp, name, entries)?;
                self.set(&s.into, r);
            }
            StmtKind::IsIn { input, coord } => {
                let r = self.is_in(input, *coord)?;
                self.set(&s.into, r);
            }
            StmtKind::MapToArea { input } => {
                let inp = self.get(input);
                let r = self.map_to_area(&inp)?;
                self.set(&s.into, r);
            }
            StmtKind::Unsupported(name) => return Err(Abort(format!("The statement \"{name}\" is not supported.")).into()),
        }
        self.cur_stmt = (stmt_name(&s.kind), s.line);
        self.check()?;
        self.check_size()?;
        self.reset_mem_budget();
        Ok(())
    }

    fn take_into_frame(&mut self, name: &str) {
        let caller = self.frames.len() - 2;
        let v = if self.frames[caller].contains_key(name) {
            self.frames[caller].remove(name).map(|s| s.set)
        } else {
            self.lookup_from(caller, name).cloned()
        };
        self.set_rc(name, v.unwrap_or_default());
    }

    pub fn push_frame(&mut self) {
        self.frames.push(HashMap::new());
    }

    pub fn pop_frame_leak(&mut self, except: Option<&str>) {
        self.pop_and_leak(except)
    }

    fn pop_and_leak(&mut self, except: Option<&str>) {
        let f = self.frames.pop().unwrap();
        let caller = self.frames.last_mut().unwrap();
        for (k, v) in f {
            if Some(k.as_str()) != except {
                caller.insert(k, v);
            }
        }
    }

    /// The parts of a union are merged at its end, in one pass. They are
    /// held outside the frames, so their size is counted in `held_bytes`.
    /// Two cases merge earlier, into `acc`: many parts (64 and more, when
    /// they outgrow twice the merged ones: `(.a; .b; .a; .b; ...)` with a
    /// million parts would hold a million copies), and large parts: once
    /// they take a quarter of the memory limit, or once they and the stored
    /// sets take half of it as the limits count them. A set that follows
    /// itself (`(.a; .a;)`) is taken once. Once the merged parts alone pass
    /// `maxsize` the statement fails, as it would at its end.
    fn union(&mut self, stmts: &[Stmt], into: &str, line: usize) -> Result<()> {
        const MANY_PARTS: usize = 64;
        self.frames.push(HashMap::new());
        self.take_into_frame(into);
        let mut acc: Option<Rc<Set>> = None;
        let (mut acc_len, mut acc_bytes) = (0usize, 0u64);
        let mut parts: Vec<Rc<Set>> = Vec::with_capacity(stmts.len().min(64));
        let (mut parts_len, mut parts_bytes) = (0usize, 0u64);
        // what the parts take in memory: a node counts 8 bytes in
        // `Slot::bytes` and takes 50 and more
        let mut parts_real = 0u64;
        let mut last: Option<Rc<Set>> = None;
        for st in stmts {
            self.exec(st)?;
            let Some((set, bytes)) = self.frames.iter().rev().find_map(|f| f.get(&st.into)).map(|s| (s.set.clone(), s.bytes)) else { continue };
            if last.as_ref().is_some_and(|l| Rc::ptr_eq(l, &set)) {
                continue;
            }
            last = Some(set.clone());
            parts_len += set.len();
            parts_bytes += bytes;
            parts_real += set.approx_bytes();
            self.held_bytes += bytes;
            parts.push(set);
            let many = parts.len() >= MANY_PARTS && parts_len > 2 * acc_len + 100_000;
            // Past half the limit the parts are merged once they are a
            // quarter of the merged ones or 256 MiB: the next statement may
            // then store a set of `maxsize` without passing the limit, and
            // parts that add little do not each cost a merge.
            let stored: u64 = self.frames.iter().flat_map(|f| f.values()).map(|s| s.bytes).sum();
            let large = stored.saturating_add(self.held_bytes) > self.memory_limit() / 2 && (parts_bytes >= acc_bytes / 4 || parts_bytes >= 256 << 20);
            if !many && !large && parts_real <= self.memory_limit() / 4 {
                continue;
            }
            let mut all: Vec<Rc<Set>> = acc.take().into_iter().collect();
            all.append(&mut parts);
            let merged = Set::union_all(&all);
            drop(all);
            self.held_bytes -= acc_bytes + parts_bytes;
            let size = merged.counted_size();
            acc_len = merged.len();
            acc_bytes = size + derived_bytes(&merged);
            (parts_len, parts_bytes, parts_real) = (0, 0, 0);
            self.held_bytes += acc_bytes;
            acc = Some(Rc::new(merged));
            if size > self.settings.maxsize {
                return Err(Abort(format!("Query ran out of memory in \"union\" at line {line}. It would need at least {} MB of RAM to continue.", size >> 20)).into());
            }
            self.check_memory(0)?;
            self.check()?;
        }
        drop(last);
        let acc = match (acc, parts.is_empty()) {
            (Some(a), true) => Rc::try_unwrap(a).unwrap_or_else(|a| (*a).clone()),
            (a, _) => {
                let mut all: Vec<Rc<Set>> = a.into_iter().collect();
                all.append(&mut parts);
                Set::union_all(&all)
            }
        };
        self.held_bytes -= acc_bytes + parts_bytes;
        self.pop_and_leak(Some(into));
        self.set(into, acc);
        Ok(())
    }

    fn difference(&mut self, a: &Stmt, b: &Stmt, into: &str) -> Result<()> {
        self.frames.push(HashMap::new());
        self.exec(a)?;
        let acc = self.get(&a.into);
        self.exec(b)?;
        let acc = acc.minus(&self.get(&b.into));
        self.pop_and_leak(Some(into));
        self.set(into, acc);
        Ok(())
    }

    /// Elements of a set in internal iteration order, as singleton sets.
    pub fn singletons(&self, src: &Set) -> Vec<Set> {
        let mut out = Vec::with_capacity(src.len());
        let mut nodes: Vec<&crate::model::Node> = src.nodes.iter().collect();
        nodes.sort_by_key(|n| (crate::geo::tile_of(n.pos), n.id));
        for n in nodes {
            out.push(Set { nodes: vec![n.clone()], ..Default::default() });
        }
        let mut ways: Vec<&crate::model::Way> = src.ways.iter().collect();
        ways.sort_by_key(|w| (crate::geo::cpp_index_order(w.idx), w.id));
        for w in ways {
            out.push(Set { ways: vec![w.clone()], ..Default::default() });
        }
        let mut rels: Vec<&crate::model::Relation> = src.rels.iter().collect();
        rels.sort_by_key(|r| (crate::geo::cpp_index_order(r.idx), r.id));
        for r in rels {
            out.push(Set { rels: vec![r.clone()], ..Default::default() });
        }
        let mut areas: Vec<&crate::model::Area> = src.areas.iter().collect();
        areas.sort_by_key(|a| (crate::geo::cpp_index_order(a.idx), a.id));
        for a in areas {
            out.push(Set { areas: vec![a.clone()], ..Default::default() });
        }
        for d in &src.derived {
            out.push(Set { derived: vec![d.clone()], ..Default::default() });
        }
        out
    }

    fn foreach(&mut self, input: &str, var: &str, body: &[Stmt]) -> Result<()> {
        let saved = self.get(var);
        self.set_rc(var, Rc::default());
        self.frames.push(HashMap::new());
        let src = if input == var { saved } else { self.get(input) };
        for item in self.singletons(&src) {
            self.set(var, item);
            self.exec_block(body)?;
            self.check()?;
        }
        self.pop_and_leak(Some(var));
        Ok(())
    }

    fn complete(&mut self, input: &str, var: &str, limit: u64, body: &[Stmt]) -> Result<()> {
        self.frames.push(HashMap::new());
        let depth = self.frames.len() - 1;
        let mut cnt = 0u64;
        loop {
            let cur = self.lookup_from(depth, input).cloned().unwrap_or_default();
            self.set_rc(var, cur.clone());
            self.exec_block(body)?;
            let body_s = self.lookup(input).cloned().unwrap_or_default();
            let merged = cur.union(&body_s);
            let grown = merged.len() > cur.len();
            let size = merged.counted_size();
            let bytes = size + derived_bytes(&merged);
            self.frames[depth - 1].insert(input.to_string(), Slot { set: Rc::new(merged), size, bytes });
            cnt += 1;
            if !grown || cnt > limit {
                break;
            }
            self.check()?;
        }
        let cur = self.lookup_from(depth, input).cloned().unwrap_or_default();
        self.set_rc(var, cur);
        self.pop_and_leak(None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Load, count_cpus, par_map, par_run, spread, threads};
    use std::time::Duration;

    /// The tests that time sections beside a long one run one at a time.
    fn pool_test() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Results in input order, as on one thread, for sections of any size.
    #[test]
    fn par_map_keeps_the_order() {
        for len in [0, 1, 2, 7, 511, 512, 513, 100_000, 1_000_000] {
            let items: Vec<u64> = (0..len).collect();
            let want: Vec<u64> = items.iter().map(|x| x * 3 + 1).collect();
            assert_eq!(par_map(&items, 512, |x| x * 3 + 1), want, "{len}");
            assert_eq!(par_map(&items, 2, |x| x * 3 + 1), want, "{len}");
        }
    }

    /// A section inside a section (on a pool thread) finishes.
    #[test]
    fn par_map_nested() {
        let outer: Vec<u64> = (0..32).collect();
        let inner: Vec<u64> = (0..10_000).collect();
        let got = par_map(&outer, 2, |&k| par_map(&inner, 2, |x| x + k).iter().sum::<u64>());
        let want: Vec<u64> = outer.iter().map(|&k| inner.iter().map(|x| x + k).sum()).collect();
        assert_eq!(got, want);
    }

    /// An error of an item or of the sink ends the section; the sink has
    /// seen the results before it, in order, or none.
    #[test]
    fn par_run_stops_at_the_first_error() {
        let items: Vec<u32> = (0..200_000).collect();
        let mut sunk = Vec::new();
        let r = par_run(&items, 8, |&x| if x == 5000 { Err(x) } else { Ok(x) }, |x| {
            sunk.push(x);
            Ok(())
        });
        assert_eq!(r, Err(5000));
        assert!(sunk.iter().copied().eq(0..sunk.len() as u32) && sunk.len() <= 5000, "{}", sunk.len());
        let mut n = 0;
        let r = par_run(&items, 8, |&x| Ok::<u32, u32>(x), |x| {
            n += 1;
            if x == 7 { Err(x) } else { Ok(()) }
        });
        assert_eq!((r, n), (Err(7), 8));
        // below `min`: on this thread
        let r: Result<(), ()> = par_run(&items[..7], 8, |_| {
            assert!(rayon::current_thread_index().is_none());
            Ok(())
        }, |_| Ok(()));
        assert!(r.is_ok());
    }

    /// Runs `second` while a long section of another thread (400 items of
    /// 2 ms per pool thread, about 0.8 s on the pool) has the pool. Returns
    /// whether the long section used the pool, and whether it was still
    /// running when `second` ended.
    fn beside_a_long_section(second: impl FnOnce()) -> (bool, bool) {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        let long_done = AtomicBool::new(false);
        let on_pool = AtomicBool::new(false);
        let mut running = false;
        std::thread::scope(|s| {
            s.spawn(|| {
                let items: Vec<usize> = (0..400 * threads()).collect();
                let got = par_map(&items, 2, |&x| {
                    if rayon::current_thread_index().is_some() {
                        on_pool.store(true, SeqCst);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                    x * 2
                });
                assert!(got.iter().copied().eq(items.iter().map(|x| x * 2)));
                long_done.store(true, SeqCst);
            });
            std::thread::sleep(Duration::from_millis(200));
            second();
            running = !long_done.load(SeqCst);
        });
        let used = on_pool.load(SeqCst);
        if !used {
            // vacuous when the host had no room for the pool
            eprintln!("beside_a_long_section: the pool was not used");
        }
        (used, running)
    }

    /// A small section does not wait for the long section of another
    /// query on the pool (with one rayon job per section it waited for all
    /// of it: rayon takes a new job only when the running ones have no work
    /// left to hand out).
    #[test]
    fn small_section_does_not_wait_for_a_long_one() {
        let _one = pool_test();
        let mut took = Duration::ZERO;
        let (busy, _) = beside_a_long_section(|| {
            let items: Vec<u32> = (0..64).collect();
            let start = std::time::Instant::now();
            assert_eq!(par_map(&items, 2, |x| x + 1), (1..65).collect::<Vec<u32>>());
            took = start.elapsed();
        });
        assert!(!busy || took < Duration::from_millis(50), "{took:?}");
    }

    /// A large section gets turns on the pool beside the long section of
    /// another query, and ends first (it waited for all of it), with its
    /// results in order across its own work and the turns.
    #[test]
    fn large_sections_take_turns() {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        let _one = pool_test();
        let on_pool = AtomicBool::new(false);
        let (busy, long_running) = beside_a_long_section(|| {
            let items: Vec<usize> = (0..50 * threads()).collect();
            let got = par_map(&items, 2, |&x| {
                if rayon::current_thread_index().is_some() {
                    on_pool.store(true, SeqCst);
                }
                std::thread::sleep(Duration::from_millis(2));
                x + 1
            });
            assert!(got.iter().copied().eq(items.iter().map(|x| x + 1)));
        });
        assert!(!busy || (on_pool.load(SeqCst) && long_running), "{} {long_running}", on_pool.load(SeqCst));
    }

    /// No active query (0 used to wrap in `active - 1`): the host has room.
    /// (`parallel_ok` keeps sections outside a query off the pool.)
    #[test]
    fn spread_without_an_active_query() {
        assert!(spread(Load { active: 0, ..HOST24 }));
    }

    /// A section that has to go to the pool beside the long section of
    /// another query waits about a TURN, not for the other section (on
    /// 0.2.8 all of its remaining 0.6 s).
    #[test]
    fn medium_section_waits_about_a_turn() {
        let _one = pool_test();
        let mut took = Duration::ZERO;
        let (busy, _) = beside_a_long_section(|| {
            let items: Vec<u32> = (0..40).collect();
            let start = std::time::Instant::now();
            let got = par_map(&items, 2, |&x| {
                std::thread::sleep(Duration::from_millis(1));
                x + 1
            });
            took = start.elapsed();
            assert!(got.iter().copied().eq(1..41));
        });
        assert!(!busy || took < Duration::from_millis(100), "{took:?}");
    }

    /// The rest of a section after its TURN on its own thread ends there
    /// when it takes less than another TURN, instead of waiting for the
    /// pool.
    #[test]
    fn small_rest_ends_on_its_own_thread() {
        let _one = pool_test();
        let mut took = Duration::ZERO;
        let (busy, _) = beside_a_long_section(|| {
            let items: Vec<u32> = (0..12).collect();
            let start = std::time::Instant::now();
            let got = par_map(&items, 2, |&x| {
                assert!(rayon::current_thread_index().is_none());
                std::thread::sleep(Duration::from_millis(1));
                x
            });
            took = start.elapsed();
            assert!(got.iter().copied().eq(0..12));
        });
        assert!(!busy || took < Duration::from_millis(30), "{took:?}");
    }

    /// A pool of 8 on a host of 24 CPUs without a quota, one query.
    const HOST24: Load = Load { threads: 8, active: 1, cpus: 24, host_cpus: 24, runnable: 1 };

    /// The pool goes to work while the host has as many idle CPUs as the
    /// pool has threads: up to 17 runnable threads on 24 CPUs (16 others
    /// and the asking one), where a run queue of 5 used to keep a query on
    /// one thread.
    #[test]
    fn spread_while_the_host_has_room() {
        for runnable in [0, 1, 3, 5, 9, 17] {
            assert!(spread(Load { runnable, ..HOST24 }), "{runnable}");
        }
        for runnable in [18, 24, 100] {
            assert!(!spread(Load { runnable, ..HOST24 }), "{runnable}");
        }
        // 16 CPUs: up to 9 runnable
        assert!(spread(Load { host_cpus: 16, cpus: 16, runnable: 9, ..HOST24 }));
        assert!(!spread(Load { host_cpus: 16, cpus: 16, runnable: 10, ..HOST24 }));
        // a pool larger than the CPUs needs them all idle
        assert!(spread(Load { host_cpus: 4, cpus: 4, runnable: 1, ..HOST24 }));
        assert!(!spread(Load { host_cpus: 4, cpus: 4, runnable: 2, ..HOST24 }));
        // 4 CPUs, a pool of 2 (the default): up to 3 runnable
        let small = Load { threads: 2, active: 1, cpus: 4, host_cpus: 4, runnable: 3 };
        assert!(spread(small));
        assert!(!spread(Load { runnable: 4, ..small }));
    }

    /// Queries in this process: at most half as many as the pool has
    /// threads, and each other one keeps one of the CPUs of the process.
    #[test]
    fn spread_with_other_queries() {
        for active in 1..=4 {
            assert!(spread(Load { active, runnable: active + 2, ..HOST24 }), "{active}");
        }
        assert!(!spread(Load { active: 5, ..HOST24 }));
        // a quota of 8 CPUs (a container with --cpus 8) and a pool of 8:
        // one query only; the host does not matter
        let quota = Load { cpus: 8, ..HOST24 };
        assert!(spread(quota));
        assert!(!spread(Load { active: 2, ..quota }));
        // the default pool of half the quota: up to 2 queries
        let half = Load { threads: 4, ..quota };
        assert!(spread(Load { active: 2, ..half }));
        assert!(!spread(Load { active: 3, ..half }));
        // a pool of one thread never spreads
        assert!(!spread(Load { threads: 1, ..HOST24 }));
    }

    /// More runnable threads or queries never turn a refusal into a yes.
    #[test]
    fn spread_is_monotone() {
        for host_cpus in [2, 4, 8, 16, 24, 64] {
            for cpus in [1, 2, 4, 8, host_cpus] {
                for threads in [1, 2, 4, 8] {
                    for active in 1..6 {
                        let mut was = true;
                        for runnable in 0..80 {
                            let now = spread(Load { threads, active, cpus, host_cpus, runnable });
                            assert!(was || !now, "{threads} {active} {cpus} {host_cpus} {runnable}");
                            was = now;
                            assert!(!now || spread(Load { threads, active: active.max(2) - 1, cpus, host_cpus, runnable }));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn cpu_lists() {
        assert_eq!(count_cpus("0-23\n"), Some(24));
        assert_eq!(count_cpus("0"), Some(1));
        assert_eq!(count_cpus("0-3,8-11,16"), Some(9));
        assert_eq!(count_cpus(""), None);
        assert_eq!(count_cpus("3-1"), None);
        assert_eq!(count_cpus("x"), None);
    }
}
