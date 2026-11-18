//! Keeping a database current with OSM replication diffs (minutely, hourly
//! or daily OsmChange files from planet.openstreetmap.org, Geofabrik, ...).
//!
//! The sequence number of the last applied diff is stored in the database,
//! in the same transaction as the data, so an interrupted run resumes
//! exactly where it stopped.

use crate::import::{ImportReport, apply_reader};
use crate::model::{format_timestamp, parse_timestamp};
use crate::store::Db;
use anyhow::{Context, Result, bail};
use std::io::{BufReader, Read};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ReplicateOptions {
    /// Replication base URL (default: the one recorded at import).
    pub url: Option<String>,
    /// Start after this sequence number instead of the recorded one.
    pub sequence: Option<u64>,
    /// Apply what is available and return.
    pub once: bool,
    /// Wait between checks for new diffs.
    pub interval: Duration,
    /// Diffs applied in one transaction.
    pub batch: usize,
}

/// State of a replication sequence: number and timestamp (seconds).
#[derive(Clone, Copy, Debug)]
pub struct State {
    pub sequence: u64,
    pub timestamp: i64,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(30)).timeout_read(Duration::from_secs(600)).user_agent(concat!("overpass-rust/", env!("CARGO_PKG_VERSION"))).build()
}

fn seq_path(url: &str, seq: u64, ext: &str) -> String {
    format!("{}/{:03}/{:03}/{:03}.{ext}", url.trim_end_matches('/'), seq / 1_000_000, seq / 1000 % 1000, seq % 1000)
}

/// Parse a replication `state.txt`.
pub fn parse_state(text: &str) -> Result<State> {
    let mut seq = None;
    let mut ts = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().replace("\\:", ":");
        match k.trim() {
            "sequenceNumber" => seq = v.parse().ok(),
            "timestamp" => ts = parse_timestamp(&v),
            _ => {}
        }
    }
    match (seq, ts) {
        (Some(sequence), Some(timestamp)) => Ok(State { sequence, timestamp }),
        _ => bail!("not a replication state file"),
    }
}

/// State of `seq`, or the latest state for `None`; `Ok(None)` if the
/// server does not have it (not yet published, or removed).
pub fn fetch_state(agent: &ureq::Agent, url: &str, seq: Option<u64>) -> Result<Option<State>> {
    let u = match seq {
        Some(s) => seq_path(url, s, "state.txt"),
        None => format!("{}/state.txt", url.trim_end_matches('/')),
    };
    match agent.get(&u).call() {
        Ok(r) => Ok(Some(parse_state(&r.into_string()?).with_context(|| u.clone())?)),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(u)),
    }
}

fn fetch_diff(agent: &ureq::Agent, url: &str, seq: u64) -> Result<Vec<u8>> {
    let u = seq_path(url, seq, "osc.gz");
    let r = agent.get(&u).call().with_context(|| u.clone())?;
    let mut buf = Vec::new();
    r.into_reader().read_to_end(&mut buf).with_context(|| u.clone())?;
    Ok(buf)
}

/// Largest sequence whose state is not newer than `ts` (searching back from
/// the latest state).
pub fn sequence_for(agent: &ureq::Agent, url: &str, ts: i64) -> Result<u64> {
    let latest = fetch_state(agent, url, None)?.context("the replication server has no state.txt")?;
    if latest.timestamp <= ts {
        return Ok(latest.sequence);
    }
    // Step back exponentially to an old enough state, then bisect.
    let mut hi = latest.sequence;
    let mut step = 60u64;
    let mut lo = loop {
        let s = hi.saturating_sub(step);
        match fetch_state(agent, url, Some(s))? {
            Some(st) if st.timestamp > ts && s > 0 => {
                hi = s;
                step = step.saturating_mul(2);
            }
            _ => break s,
        }
    };
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        match fetch_state(agent, url, Some(mid))? {
            Some(st) if st.timestamp <= ts => lo = mid,
            Some(_) => hi = mid,
            None => lo = mid,
        }
    }
    Ok(lo)
}

/// Apply available diffs; returns the number applied.
pub fn step(db: &Db, opts: &ReplicateOptions) -> Result<usize> {
    let agent = agent();
    let (url, current) = {
        let snap = db.snapshot()?;
        let url = match &opts.url {
            Some(u) => u.clone(),
            None => snap.info("replication_url")?.context("no replication URL: pass --url (it is taken from the header of .osm.pbf imports)")?,
        };
        // A sequence number belongs to its feed: with another URL (an
        // extract imported with the Geofabrik header, then the planet
        // minutely feed) the recorded number would select diffs years old.
        let recorded_url = snap.info("replication_url")?;
        let same_feed = recorded_url.as_deref().is_none_or(|r| r.trim_end_matches('/') == url.trim_end_matches('/'));
        if !same_feed {
            eprintln!("replicate: {url} is not the recorded feed {}; finding the start from the data timestamp", recorded_url.as_deref().unwrap_or(""));
        }
        let current = match snap.info("replication_sequence")?.and_then(|s| s.parse::<u64>().ok()).filter(|_| same_feed) {
            Some(s) => s,
            None => {
                let base = snap.info("osm_base")?.and_then(|t| parse_timestamp(&t)).context("unknown data timestamp: pass --sequence")?;
                // An hour of overlap: replaying older changes is harmless.
                let s = sequence_for(&agent, &url, base - 3600)?;
                eprintln!("replicate: starting after sequence {s} (data timestamp {})", format_timestamp(base));
                s
            }
        };
        (url, current)
    };
    let latest = fetch_state(&agent, &url, None)?.context("the replication server has no state.txt")?;
    if latest.sequence < current {
        bail!("{url} is at sequence {}, behind the database ({current}): another feed, or a reset of this one", latest.sequence);
    }
    if latest.sequence == current {
        return Ok(0);
    }
    let mut last = latest.sequence.min(current + opts.batch.max(1) as u64);
    let start = Instant::now();
    // Download first, so the write lock is held only while applying; at
    // most 256 MB of compressed diffs per transaction.
    let mut diffs = Vec::new();
    let mut bytes = 0;
    for seq in current + 1..=last {
        let diff = fetch_diff(&agent, &url, seq)?;
        bytes += diff.len();
        diffs.push(diff);
        if bytes >= 256 << 20 {
            last = seq;
            break;
        }
    }
    // Only the timestamp of the last diff is stored. Without its state file
    // (a gap in the feed), the newest element timestamp of the diffs stands
    // for it, so that a missing state file does not hold up the updates.
    let state = if last == latest.sequence { Some(latest) } else { fetch_state(&agent, &url, Some(last))? };
    let mut report = ImportReport::default();
    let mut skipped = Vec::new();
    let mut txn = db.write()?;
    for (i, gz) in diffs.iter().enumerate() {
        let seq = current + 1 + i as u64;
        apply_reader(&mut txn, BufReader::new(flate2::read::MultiGzDecoder::new(&gz[..])), &mut report).with_context(|| format!("sequence {seq}"))?;
        skipped.extend(report.skipped.drain(..).map(|s| format!("sequence {seq}: {s}")));
    }
    let ts = match state {
        Some(st) => format_timestamp(st.timestamp),
        None => {
            eprintln!("replicate: {url} has no state of sequence {last}; data timestamp from the newest element");
            let base = txn.info("osm_base")?.and_then(|t| parse_timestamp(&t)).unwrap_or(0);
            format_timestamp(report.max_timestamp.max(base))
        }
    };
    if !skipped.is_empty() {
        let n: u64 = txn.info("replicate_skipped")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        txn.set_info("replicate_skipped", &(n + skipped.len() as u64).to_string())?;
        txn.set_info("replicate_last_skipped", skipped.last().unwrap())?;
    }
    txn.set_info("replication_sequence", &last.to_string())?;
    txn.set_info("replication_timestamp", &ts)?;
    txn.set_info("replication_url", &url)?;
    txn.set_info("osm_base", &ts)?;
    txn.commit()?;
    for s in &skipped {
        eprintln!("replicate: left out {s}");
    }
    eprintln!("replicate: applied sequences {}..={} ({} elements) in {:.1?}, data now at {ts}", current + 1, last, report.elements, start.elapsed());
    Ok(diffs.len())
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into())
}

/// `f` with a panic turned into an error: a panic in a step must not end
/// the replication process (and with it the container and its queries).
pub fn guarded<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|p| Err(anyhow::anyhow!("panic: {}", panic_text(&*p))))
}

/// File in the database directory with the state of the replication, for
/// monitoring: `key=value` lines `time`, `sequence`, `data_timestamp`,
/// `failures` (attempts in a row that failed), `error`, `skipped`.
pub const STATUS_FILE: &str = "replicate.status";

fn write_status(db: &Db, failures: u32, error: &str) {
    let info = |k: &str| db.snapshot().ok().and_then(|s| s.info(k).ok().flatten()).unwrap_or_default();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let text = format!(
        "time={}\nsequence={}\ndata_timestamp={}\nfailures={failures}\nerror={}\nskipped={}\n",
        format_timestamp(now),
        info("replication_sequence"),
        info("osm_base"),
        error.replace(['\n', '\r'], " "),
        info("replicate_skipped"),
    );
    let path = db.path().join(STATUS_FILE);
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)) {
        eprintln!("replicate: writing {}: {e}", path.display());
    }
}

/// Wait after the `failures`-th failed attempt in a row: the interval,
/// doubled with each failure, at most 10 minutes.
pub fn backoff(interval: Duration, failures: u32) -> Duration {
    (interval * 2u32.pow(failures.clamp(1, 6) - 1)).min(Duration::from_secs(600)).max(interval)
}

/// Apply diffs, then keep waiting for new ones (unless `once`).
pub fn run(db: &Db, opts: &ReplicateOptions) -> Result<()> {
    if let Some(s) = opts.sequence {
        let mut txn = db.write()?;
        txn.set_info("replication_sequence", &s.to_string())?;
        if let Some(u) = &opts.url {
            txn.set_info("replication_url", u)?;
        }
        txn.commit()?;
    }
    let mut failures = 0u32;
    // After a failure, diffs go one per transaction for a whole batch: the
    // diffs before a failing one are kept, and a retry fetches only it.
    let mut single = 0usize;
    loop {
        let batch = if single > 0 { 1 } else { opts.batch.max(1) };
        let o = ReplicateOptions { batch, ..opts.clone() };
        let wait = match guarded(|| step(db, &o)) {
            Ok(n) => {
                if failures > 0 {
                    eprintln!("replicate: applying again after {failures} failed attempts");
                }
                failures = 0;
                single = single.saturating_sub(n);
                write_status(db, 0, "");
                // Catching up: continue right away.
                if n >= o.batch {
                    continue;
                }
                if opts.once {
                    return Ok(());
                }
                opts.interval
            }
            Err(e) if opts.once => return Err(e),
            Err(e) => {
                failures += 1;
                single = opts.batch.max(1);
                eprintln!("replicate: attempt {failures} failed: {e:#}");
                write_status(db, failures, &format!("{e:#}"));
                backoff(opts.interval, failures)
            }
        };
        std::thread::sleep(wait);
    }
}
