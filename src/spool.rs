//! The output of a query on its way to the HTTP client.
//!
//! The query thread hands its output over in chunks; the response body takes
//! them at the pace of the client. A client that reads slower than the query
//! writes leaves the chunks waiting here: up to `MEM_LIMIT` bytes in memory,
//! the rest in an unlinked file in the temp directory. The query thread does
//! not wait for the client: the query finishes at its own pace, frees its slot
//! and its database snapshot, and the time the client takes to read the answer
//! does not count against `[timeout:]` (as with the C++ server, whose web
//! server buffers the answer).
//!
//! The files of all queries share a byte budget (`serve --spool-max`): the
//! bytes they take on disk. What the client has read is given back to the
//! file system as it goes (a hole at the start of the file), so the file of
//! a client that keeps reading takes about what still waits. When the budget
//! is used up, the query thread waits for its client until its deadline, as
//! when there was no spool. A client that takes nothing for a while, or does
//! not finish reading in time, loses its answer; its memory, file and budget
//! are freed (`Spool::watch`).

use axum::body::Bytes;
use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// Bytes of a query that wait in memory before the file takes the rest.
pub const MEM_LIMIT: usize = 1 << 20;
/// Largest piece read back from the file at a time.
const READ_CHUNK: u64 = 256 << 10;
/// Bytes read from the file before their disk space is given back.
const FREE_STEP: u64 = 1 << 20;

/// Gives the disk space of `len` bytes at `offset` of the file back to the
/// file system (a hole; the file keeps its length). False when the file
/// system cannot do that.
fn punch_hole(f: &File, offset: u64, len: u64) -> bool {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn fallocate(fd: i32, mode: i32, offset: i64, len: i64) -> i32;
    }
    const FALLOC_FL_KEEP_SIZE: i32 = 1;
    const FALLOC_FL_PUNCH_HOLE: i32 = 2;
    unsafe { fallocate(f.as_raw_fd(), FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, offset as i64, len as i64) == 0 }
}

/// Bytes that spool files take on disk over all queries, against a maximum.
pub struct Budget {
    used: AtomicU64,
    max: u64,
}

impl Budget {
    pub fn new(max: u64) -> Arc<Budget> {
        Arc::new(Budget { used: AtomicU64::new(0), max })
    }

    fn reserve(&self, n: u64) -> bool {
        self.used.try_update(Ordering::AcqRel, Ordering::Acquire, |u| u.checked_add(n).filter(|&t| t <= self.max)).is_ok()
    }

    fn release(&self, n: u64) {
        self.used.fetch_sub(n, Ordering::AcqRel);
    }

    /// Bytes spool files take on disk now.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
}

struct State {
    /// Chunks that wait in memory; they come before every unread byte of the file.
    mem: VecDeque<Bytes>,
    mem_bytes: usize,
    file: Option<File>,
    /// Bytes written to the file, and bytes of it taken by the client.
    file_len: u64,
    file_pos: u64,
    /// Bytes at the start of the file whose disk space was given back; the
    /// budget holds the rest (`file_len - file_freed`).
    file_freed: u64,
    /// The query thread has finished, at this time.
    done: Option<Instant>,
    /// Why the answer was given up (client gone, too slow, file error).
    failed: Option<String>,
    waker: Option<Waker>,
    /// When the client last took a chunk, or when data began to wait.
    last_take: Instant,
}

impl State {
    fn waiting(&self) -> bool {
        !self.mem.is_empty() || self.file_pos < self.file_len
    }

    fn wake(&mut self) {
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }
}

pub struct Spool {
    st: Mutex<State>,
    /// The query thread waits here for its client when the budget is used up.
    room: Condvar,
    budget: Arc<Budget>,
    dir: PathBuf,
}

impl Spool {
    pub fn new(budget: Arc<Budget>, dir: PathBuf) -> Arc<Spool> {
        Arc::new(Spool {
            st: Mutex::new(State { mem: VecDeque::new(), mem_bytes: 0, file: None, file_len: 0, file_pos: 0, file_freed: 0, done: None, failed: None, waker: None, last_take: Instant::now() }),
            room: Condvar::new(),
            budget,
            dir,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hands a chunk to the client (query thread). Waits only when the
    /// budget is used up, until `deadline`.
    pub fn push(&self, chunk: Bytes, deadline: Instant) -> io::Result<()> {
        let len = chunk.len();
        let mut st = self.lock();
        loop {
            if let Some(why) = &st.failed {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, why.clone()));
            }
            if !st.waiting() {
                st.last_take = Instant::now();
            }
            let file_empty = st.file_pos == st.file_len;
            if file_empty && (st.mem.is_empty() || st.mem_bytes + len <= MEM_LIMIT) {
                st.mem_bytes += len;
                st.mem.push_back(chunk);
                st.wake();
                return Ok(());
            }
            if self.budget.reserve(len as u64) {
                if let Err(e) = self.append(&mut st, &chunk) {
                    self.budget.release(len as u64);
                    self.fail(&mut st, format!("spool file: {e}"));
                    return Err(e);
                }
                st.wake();
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "client too slow"));
            }
            st = self.room.wait_timeout(st, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn append(&self, st: &mut State, chunk: &[u8]) -> io::Result<()> {
        if st.file.is_none() {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = self.dir.join(format!("overpass-spool-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            let f = File::options().read(true).write(true).create_new(true).open(&path)?;
            std::fs::remove_file(&path)?;
            st.file = Some(f);
        }
        st.file.as_ref().unwrap().write_all_at(chunk, st.file_len)?;
        st.file_len += chunk.len() as u64;
        Ok(())
    }

    /// The query thread has written everything.
    pub fn finish(&self) {
        let mut st = self.lock();
        st.done.get_or_insert_with(Instant::now);
        st.wake();
    }

    /// The next chunk for the client (response body).
    pub fn poll_take(&self, cx: &mut Context<'_>) -> Poll<Option<io::Result<Bytes>>> {
        let mut st = self.lock();
        if let Some(why) = &st.failed {
            return Poll::Ready(Some(Err(io::Error::other(why.clone()))));
        }
        if let Some(c) = st.mem.pop_front() {
            st.mem_bytes -= c.len();
            st.last_take = Instant::now();
            self.room.notify_all();
            return Poll::Ready(Some(Ok(c)));
        }
        if st.file_pos < st.file_len {
            let n = READ_CHUNK.min(st.file_len - st.file_pos);
            let mut buf = vec![0; n as usize];
            if let Err(e) = st.file.as_ref().unwrap().read_exact_at(&mut buf, st.file_pos) {
                self.fail(&mut st, format!("spool file: {e}"));
                return Poll::Ready(Some(Err(e)));
            }
            st.file_pos += n;
            if st.file_pos == st.file_len {
                // all read: give the disk space back
                self.budget.release(st.file_len - st.file_freed);
                if st.file.as_ref().unwrap().set_len(0).is_err() {
                    st.file = None;
                }
                st.file_pos = 0;
                st.file_len = 0;
                st.file_freed = 0;
            } else if st.file_pos - st.file_freed >= FREE_STEP {
                // The blocks read so far. Where the file system cannot free
                // them, the budget keeps them until the file is read.
                let upto = st.file_pos & !4095;
                if punch_hole(st.file.as_ref().unwrap(), st.file_freed, upto - st.file_freed) {
                    self.budget.release(upto - st.file_freed);
                    st.file_freed = upto;
                }
            }
            st.last_take = Instant::now();
            self.room.notify_all();
            return Poll::Ready(Some(Ok(Bytes::from(buf))));
        }
        if st.done.is_some() {
            return Poll::Ready(None);
        }
        st.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    /// Gives the answer up: frees memory, file and budget; the query thread
    /// and the response body get an error.
    fn fail(&self, st: &mut State, why: String) {
        if st.failed.is_none() {
            st.failed = Some(why);
        }
        st.mem.clear();
        st.mem_bytes = 0;
        self.budget.release(st.file_len - st.file_freed);
        st.file = None;
        st.file_pos = 0;
        st.file_len = 0;
        st.file_freed = 0;
        st.wake();
        self.room.notify_all();
    }

    /// The query thread stops writing before the end of the answer (it
    /// waited for its client until its deadline): the response body ends
    /// with an error, as an answer that only ends would look complete.
    pub fn give_up(&self, why: &str) {
        let mut st = self.lock();
        self.fail(&mut st, why.into());
    }

    /// The client went away.
    pub fn close(&self) {
        let mut st = self.lock();
        self.fail(&mut st, "client went away".into());
    }

    /// Gives the answer up when data has waited `stall` without the client
    /// taking any, or is still waiting `max` after the query finished.
    /// Returns true when there is nothing more to watch.
    pub fn check(&self, now: Instant, stall: Duration, max: Duration) -> bool {
        let mut st = self.lock();
        if st.failed.is_some() || (st.done.is_some() && !st.waiting()) {
            return true;
        }
        if st.waiting() && now.saturating_duration_since(st.last_take) > stall {
            self.fail(&mut st, format!("the client did not take any data for {} s", stall.as_secs()));
            return true;
        }
        if st.done.is_some_and(|d| now.saturating_duration_since(d) > max) {
            self.fail(&mut st, format!("the client did not read the answer within {} s", max.as_secs()));
            return true;
        }
        false
    }

    /// Runs `check` once a second until there is nothing more to watch or the
    /// spool is gone (async task next to the response).
    pub async fn watch(spool: std::sync::Weak<Spool>, stall: Duration, max: Duration) {
        let tick = Duration::from_secs(1).min(stall / 2).max(Duration::from_millis(10));
        loop {
            tokio::time::sleep(tick).await;
            let Some(s) = spool.upgrade() else { return };
            if s.check(Instant::now(), stall, max) {
                return;
            }
        }
    }

    #[cfg(test)]
    fn in_file(&self) -> u64 {
        let st = self.lock();
        st.file_len - st.file_pos
    }

    /// Bytes the spool file takes on disk.
    #[cfg(test)]
    fn on_disk(&self) -> u64 {
        use std::os::unix::fs::MetadataExt;
        self.lock().file.as_ref().map_or(0, |f| f.metadata().unwrap().blocks() * 512)
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        let st = self.st.get_mut().unwrap_or_else(|e| e.into_inner());
        self.budget.release(st.file_len - st.file_freed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Wake;

    struct Noop;
    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }

    fn take_all(s: &Spool) -> (Vec<u8>, bool) {
        let w = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&w);
        let mut out = Vec::new();
        loop {
            match s.poll_take(&mut cx) {
                Poll::Ready(Some(Ok(b))) => out.extend_from_slice(&b),
                Poll::Ready(Some(Err(_))) => return (out, false),
                Poll::Ready(None) => return (out, true),
                Poll::Pending => return (out, false),
            }
        }
    }

    fn chunk(i: u32, len: usize) -> Bytes {
        Bytes::from((0..len).map(|j| (i as usize * 31 + j) as u8).collect::<Vec<_>>())
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn order_kept_through_memory_and_file() {
        let b = Budget::new(1 << 30);
        let s = Spool::new(b.clone(), std::env::temp_dir());
        let mut want = Vec::new();
        for i in 0..100 {
            let c = chunk(i, 50_000);
            want.extend_from_slice(&c);
            s.push(c, far()).unwrap();
        }
        assert!(s.in_file() > 0, "past MEM_LIMIT the chunks go to the file");
        assert_eq!(b.used(), s.in_file());
        // the client takes part, the query writes more, then finishes
        let w = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&w);
        let mut got = Vec::new();
        for _ in 0..30 {
            if let Poll::Ready(Some(Ok(c))) = s.poll_take(&mut cx) {
                got.extend_from_slice(&c);
            }
        }
        for i in 100..140 {
            let c = chunk(i, 70_000);
            want.extend_from_slice(&c);
            s.push(c, far()).unwrap();
        }
        s.finish();
        let (rest, complete) = take_all(&s);
        got.extend_from_slice(&rest);
        assert!(complete);
        assert_eq!(got.len(), want.len());
        assert!(got == want);
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn memory_again_after_the_file_is_read() {
        let b = Budget::new(1 << 30);
        let s = Spool::new(b.clone(), std::env::temp_dir());
        let mut want = Vec::new();
        for i in 0..40 {
            let c = chunk(i, 64 << 10);
            want.extend_from_slice(&c);
            s.push(c, far()).unwrap();
        }
        let (mut got, complete) = take_all(&s);
        assert!(!complete);
        assert_eq!(s.in_file(), 0);
        for i in 40..45 {
            let c = chunk(i, 64 << 10);
            want.extend_from_slice(&c);
            s.push(c, far()).unwrap();
        }
        assert_eq!(s.in_file(), 0, "new chunks go to memory once the file is read");
        s.finish();
        let (rest, complete) = take_all(&s);
        got.extend_from_slice(&rest);
        assert!(complete && got == want);
    }

    #[test]
    fn budget_used_up_waits_for_the_client_until_the_deadline() {
        let b = Budget::new(200_000);
        let s = Spool::new(b.clone(), std::env::temp_dir());
        for i in 0..16 {
            s.push(chunk(i, 64 << 10), far()).unwrap(); // memory
        }
        s.push(chunk(16, 150_000), far()).unwrap(); // file, within the budget
        let t = Instant::now();
        let e = s.push(chunk(17, 150_000), Instant::now() + Duration::from_millis(200)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(t.elapsed() >= Duration::from_millis(200));
        // a client that reads frees the budget, and the query thread goes on
        let s2 = s.clone();
        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            take_all(&s2)
        });
        s.push(chunk(18, 150_000), far()).unwrap();
        reader.join().unwrap();
        s.finish();
        take_all(&s);
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn stalled_or_late_client_frees_everything() {
        let b = Budget::new(1 << 30);
        let s = Spool::new(b.clone(), std::env::temp_dir());
        for i in 0..40 {
            s.push(chunk(i, 64 << 10), far()).unwrap();
        }
        assert!(b.used() > 0);
        let stall = Duration::from_secs(60);
        assert!(!s.check(Instant::now(), stall, stall));
        assert!(s.check(Instant::now() + Duration::from_secs(61), stall, stall));
        assert_eq!(b.used(), 0);
        assert!(s.push(chunk(0, 10), far()).is_err(), "the query thread stops");
        let (_, complete) = take_all(&s);
        assert!(!complete, "the client gets an error, not a cut answer that looks complete");

        // a client that keeps reading, but does not finish within max after the query
        let s = Spool::new(b.clone(), std::env::temp_dir());
        for i in 0..40 {
            s.push(chunk(i, 64 << 10), far()).unwrap();
        }
        s.finish();
        let max = Duration::from_secs(600);
        assert!(!s.check(Instant::now() + Duration::from_secs(30), stall, max));
        assert!(s.check(Instant::now() + Duration::from_secs(601), Duration::from_secs(3600), max));
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn waiting_starts_when_data_arrives() {
        // a query that computes long before its first output is not a stalled client
        let b = Budget::new(1 << 30);
        let s = Spool::new(b, std::env::temp_dir());
        std::thread::sleep(Duration::from_millis(50));
        s.push(chunk(0, 10), far()).unwrap();
        assert!(!s.check(Instant::now(), Duration::from_millis(40), Duration::from_secs(60)));
    }

    #[test]
    fn closed_by_the_client() {
        let b = Budget::new(1 << 30);
        let s = Spool::new(b.clone(), std::env::temp_dir());
        for i in 0..40 {
            s.push(chunk(i, 64 << 10), far()).unwrap();
        }
        s.close();
        assert_eq!(b.used(), 0);
        assert_eq!(s.push(chunk(0, 10), far()).unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn file_stays_within_the_budget_while_the_client_reads() {
        // a client that keeps reading, slower than the query writes: 200 MiB pass
        // through a spool of 4 MiB
        let max = 4 << 20;
        let b = Budget::new(max);
        let s = Spool::new(b.clone(), std::env::temp_dir());
        let w = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&w);
        let mut take = || match s.poll_take(&mut cx) {
            Poll::Ready(Some(Ok(c))) => c.len(),
            _ => panic!("no chunk"),
        };
        let mut largest = 0;
        for i in 0..3200 {
            // the query waits for room in the budget, as `push` does
            while b.used() + (64 << 10) > max {
                take();
            }
            s.push(chunk(i, 64 << 10), far()).unwrap();
            // the client takes a piece (256 KiB of the file) for five chunks written (320 KiB)
            if i % 5 == 4 {
                take();
            }
            largest = largest.max(s.on_disk());
        }
        assert!(largest <= max + (2 << 20), "the spool file took {} MiB on disk with a budget of {} MiB", largest >> 20, max >> 20);
    }
}
