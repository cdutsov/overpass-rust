//! Loading files into the database.

use crate::bulk::{self, BulkOptions, Loader};
use crate::model::format_timestamp;
use crate::store::Db;
use crate::update::pbf::PbfHeader;
use crate::update::{self, Change};
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

const BATCH: usize = 1_000_000;

#[derive(Default, Debug)]
pub struct ImportReport {
    pub elements: usize,
    pub batches: usize,
    pub max_timestamp: i64,
    /// Replication state from a PBF header.
    pub replication: Option<PbfHeader>,
}

#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Directory for temporary files of a bulk import (default: `import-tmp`
    /// inside the database directory).
    pub tmp: Option<PathBuf>,
    /// Memory for sorting during a bulk import, in bytes.
    pub memory: usize,
    /// Store meta data (version, timestamp, changeset, user).
    pub meta: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions { tmp: None, memory: 1 << 30, meta: true }
    }
}

/// Open a file, decompressing `.gz` and `.bz2`; `-` is stdin.
pub fn open_input(path: &Path) -> Result<Box<dyn BufRead>> {
    let name = path.to_string_lossy();
    if name == "-" {
        return Ok(Box::new(BufReader::with_capacity(1 << 20, std::io::stdin())));
    }
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let r: Box<dyn Read> = if name.ends_with(".gz") {
        Box::new(flate2::read::MultiGzDecoder::new(BufReader::with_capacity(1 << 20, f)))
    } else if name.ends_with(".bz2") {
        Box::new(bzip2::read::MultiBzDecoder::new(BufReader::with_capacity(1 << 20, f)))
    } else {
        Box::new(f)
    };
    Ok(Box::new(BufReader::with_capacity(1 << 20, r)))
}

fn is_change_file(path: &Path) -> bool {
    path.to_string_lossy().contains(".osc")
}

/// Stream the changes of an `.osm`, `.osc` (optionally `.gz`/`.bz2`) or
/// `.osm.pbf` file; returns the replication state of a PBF header.
fn read_changes(path: &Path, sink: &mut dyn FnMut(Change) -> Result<()>) -> Result<Option<PbfHeader>> {
    if path.to_string_lossy().ends_with(".pbf") {
        return update::pbf::read(path, sink).map(Some);
    }
    update::osm_xml::read(open_input(path)?, sink)?;
    Ok(None)
}

/// Whether the database keeps meta data (set at import with `meta: false`).
fn keeps_meta(txn: &crate::store::WriteTxn) -> Result<bool> {
    Ok(txn.info("meta")?.as_deref() != Some("no"))
}

fn apply_batch(txn: &mut crate::store::WriteTxn, batch: &mut Vec<Change>, report: &mut ImportReport) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let meta = keeps_meta(txn)?;
    for c in batch.iter_mut() {
        if let Some(m) = &c.meta {
            report.max_timestamp = report.max_timestamp.max(m.timestamp);
        }
        if !meta {
            c.meta = None;
            c.user = None;
        }
    }
    report.elements += batch.len();
    report.batches += 1;
    update::apply(txn, std::mem::take(batch))?;
    txn.flush_indexes()
}

/// Apply changes read from `input` within `txn` (in batches).
pub fn apply_reader(txn: &mut crate::store::WriteTxn, input: impl BufRead, report: &mut ImportReport) -> Result<()> {
    let mut batch: Vec<Change> = Vec::new();
    update::osm_xml::read(input, |c| {
        batch.push(c);
        if batch.len() >= BATCH {
            apply_batch(txn, &mut batch, report)?;
        }
        Ok(())
    })?;
    apply_batch(txn, &mut batch, report)
}

/// Apply OsmChange (or OSM) files and the new data timestamp as one
/// transaction: readers see all of it or nothing, also after a crash.
pub fn update_files(db: &Db, files: &[PathBuf], timestamp: Option<&str>, report: &mut ImportReport) -> Result<()> {
    let mut txn = db.write()?;
    for f in files {
        let mut batch: Vec<Change> = Vec::new();
        read_changes(f, &mut |c| {
            batch.push(c);
            if batch.len() >= BATCH {
                apply_batch(&mut txn, &mut batch, report)?;
            }
            Ok(())
        })?;
        apply_batch(&mut txn, &mut batch, report)?;
    }
    if let Some(ts) = base_timestamp(timestamp, report) {
        txn.set_info("osm_base", &ts)?;
    }
    txn.commit()
}

fn base_timestamp(value: Option<&str>, report: &ImportReport) -> Option<String> {
    match value {
        Some(v) => Some(v.to_string()),
        None => match report.replication.as_ref().and_then(|r| r.timestamp) {
            Some(t) => Some(format_timestamp(t)),
            None if report.max_timestamp > 0 => Some(format_timestamp(report.max_timestamp)),
            None => None,
        },
    }
}

/// Read an `.osm`, `.osc` or `.osm.pbf` file (`-` for OSM XML on stdin) and
/// apply its content with default options.
pub fn import_file(db: &Db, path: &Path, report: &mut ImportReport) -> Result<()> {
    import_file_with(db, path, &ImportOptions::default(), report)
}

/// Import a file. Into an empty database, OSM data files go through the bulk
/// loader (any size up to the planet); otherwise the content is applied as
/// changes in one transaction.
pub fn import_file_with(db: &Db, path: &Path, opts: &ImportOptions, report: &mut ImportReport) -> Result<()> {
    if !is_change_file(path) && db.is_empty()? {
        let tmp = opts.tmp.clone().unwrap_or_else(|| bulk::default_tmp(db.path()));
        let mut loader = Loader::new(db, BulkOptions { tmp: tmp.clone(), memory: opts.memory, meta: opts.meta })?;
        let header = read_changes(path, &mut |c| loader.add(c))?;
        report.elements += loader.elements;
        report.max_timestamp = report.max_timestamp.max(loader.max_timestamp);
        report.batches += 1;
        loader.finish()?;
        let _ = std::fs::remove_dir(&tmp);
        if header.as_ref().is_some_and(|h| h.timestamp.is_some() || h.sequence.is_some()) {
            report.replication = header;
        }
        return Ok(());
    }
    let mut txn = db.write()?;
    if !opts.meta {
        txn.set_info("meta", "no")?;
    }
    let mut batch: Vec<Change> = Vec::with_capacity(BATCH);
    read_changes(path, &mut |c| {
        batch.push(c);
        if batch.len() >= BATCH {
            apply_batch(&mut txn, &mut batch, report)?;
        }
        Ok(())
    })?;
    apply_batch(&mut txn, &mut batch, report)?;
    txn.commit()
}

/// Set the data timestamp shown in output headers (explicit value, else the
/// replication timestamp of a PBF header, else the newest element), and the
/// replication state for `overpass replicate`.
pub fn set_base_timestamp(db: &Db, value: Option<&str>, report: &ImportReport) -> Result<()> {
    let mut txn = db.write()?;
    if let Some(ts) = base_timestamp(value, report) {
        txn.set_info("osm_base", &ts)?;
    }
    if let Some(h) = &report.replication {
        if let Some(s) = h.sequence {
            txn.set_info("replication_sequence", &s.to_string())?;
        }
        if let Some(u) = &h.url {
            txn.set_info("replication_url", u)?;
        }
        if let Some(t) = h.timestamp {
            txn.set_info("replication_timestamp", &format_timestamp(t))?;
        }
    }
    txn.commit()
}
