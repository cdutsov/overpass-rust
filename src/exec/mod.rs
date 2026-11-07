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
    pub cli: bool,
    pub messages: Vec<String>,
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

/// Pool size: as set, else half the hardware threads, at most 8. Larger
/// pools cost more in idle spinning of the workers between parallel
/// sections than they gain (measured on 16 threads: 8 is fastest).
fn threads() -> usize {
    match THREADS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => (std::thread::available_parallelism().map_or(1, |n| n.get()) / 2).clamp(1, 8),
        n => n,
    }
}

/// Whether the machine's run queue is at least half full (other processes,
/// e.g. queries run as separate processes, keep the cores busy). Read from
/// /proc/loadavg at most every 50 ms.
fn system_busy(threads: usize) -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CHECKED: AtomicU64 = AtomicU64::new(0);
    static RUNNABLE: AtomicU64 = AtomicU64::new(0);
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let now = START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1;
    let last = CHECKED.load(Ordering::Relaxed);
    if last == 0 || now.saturating_sub(last) >= 50 {
        CHECKED.store(now, Ordering::Relaxed);
        // Fourth field: runnable/total scheduling entities (this process included).
        let n = std::fs::read_to_string("/proc/loadavg").ok().and_then(|s| s.split_whitespace().nth(3)?.split('/').next()?.parse().ok()).unwrap_or(0);
        RUNNABLE.store(n, Ordering::Relaxed);
    }
    RUNNABLE.load(Ordering::Relaxed) as usize * 2 > threads
}

/// Whether a query may spread work over the thread pool: not when other
/// queries, in this process or others, already keep the cores busy (handing
/// work to a saturated pool only adds waiting). Does not start the pool.
pub fn parallel_ok() -> bool {
    static POOL: std::sync::Once = std::sync::Once::new();
    let threads = threads();
    let ok = threads > 1 && ACTIVE.load(std::sync::atomic::Ordering::Relaxed) * 2 <= threads && !system_busy(threads);
    if ok {
        // The pool starts on first use (fails harmlessly if already built).
        POOL.call_once(|| {
            let _ = rayon::ThreadPoolBuilder::new().num_threads(threads).build_global();
        });
    }
    ok
}

pub fn run_script<W: Write>(snap: &Snapshot, script: Script, w: W, opts: &RunOptions, jsonp: bool) -> std::result::Result<Printer<W>, (Option<Printer<W>>, RunError)> {
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
        cli: opts.cli,
        messages: Vec::new(),
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
            m.reset(size, bytes);
        }
    }

    /// Fail like the C++ allocation failure when sets plus `extra` bytes
    /// exceed the memory limit.
    pub fn check_memory(&self, extra: u64) -> Result<()> {
        let bytes: u64 = self.frames.iter().flat_map(|f| f.values()).map(|s| s.bytes).sum();
        if bytes.saturating_add(extra) > self.memory_limit() {
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
            StmtKind::Union(stmts) => self.union(stmts, &s.into)?,
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

    fn union(&mut self, stmts: &[Stmt], into: &str) -> Result<()> {
        self.frames.push(HashMap::new());
        self.take_into_frame(into);
        let mut parts: Vec<Rc<Set>> = Vec::with_capacity(stmts.len());
        for st in stmts {
            self.exec(st)?;
            if let Some(r) = self.lookup(&st.into) {
                parts.push(r.clone());
            }
        }
        let acc = Set::union_all(&parts);
        drop(parts);
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
