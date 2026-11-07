//! External merge sort of (key, value) byte records, for imports larger
//! than memory. Records are buffered up to a memory limit, sorted by key and
//! written to lz4-compressed run files, which are merged on reading.

use anyhow::{Context, Result};
use lz4_flex::frame::{FrameDecoder, FrameEncoder};
use rayon::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Runs merged at once; more are first merged in groups.
const MAX_MERGE: usize = 128;

pub struct Sorter {
    dir: PathBuf,
    name: String,
    data: Vec<u8>,
    /// (offset, key length, value length) into `data`.
    recs: Vec<(u64, u32, u32)>,
    limit: usize,
    runs: Vec<PathBuf>,
    next_run: usize,
    count: u64,
}

impl Sorter {
    /// `dir` holds the run files; `limit` is the memory budget in bytes.
    pub fn new(dir: &Path, name: &str, limit: usize) -> Sorter {
        Sorter { dir: dir.to_path_buf(), name: name.to_string(), data: Vec::new(), recs: Vec::new(), limit: limit.max(1 << 20), runs: Vec::new(), next_run: 0, count: 0 }
    }

    pub fn push(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.recs.push((self.data.len() as u64, key.len() as u32, value.len() as u32));
        self.data.extend_from_slice(key);
        self.data.extend_from_slice(value);
        self.count += 1;
        if self.data.len() + self.recs.len() * 16 >= self.limit {
            self.spill()?;
        }
        Ok(())
    }

    pub fn len(&self) -> u64 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn sort_buffer(&mut self) {
        let data = &self.data;
        let key = |r: &(u64, u32, u32)| &data[r.0 as usize..r.0 as usize + r.1 as usize];
        self.recs.par_sort_unstable_by(|a, b| key(a).cmp(key(b)));
    }

    fn run_path(&mut self) -> PathBuf {
        self.next_run += 1;
        self.dir.join(format!("{}.{}.run", self.name, self.next_run))
    }

    fn spill(&mut self) -> Result<()> {
        if self.recs.is_empty() {
            return Ok(());
        }
        self.sort_buffer();
        let path = self.run_path();
        let mut w = RunWriter::create(&path)?;
        for &(off, kl, vl) in &self.recs {
            let o = off as usize;
            w.write(&self.data[o..o + kl as usize], &self.data[o + kl as usize..o + kl as usize + vl as usize])?;
        }
        w.finish()?;
        self.runs.push(path);
        self.data.clear();
        self.recs.clear();
        // Keep the number of runs mergeable in one pass.
        if self.runs.len() >= 2 * MAX_MERGE {
            let group: Vec<PathBuf> = self.runs.drain(..MAX_MERGE).collect();
            let path = self.run_path();
            let mut m = Merged::runs(group)?;
            let mut w = RunWriter::create(&path)?;
            while let Some((k, v)) = m.next()? {
                w.write(k, v)?;
            }
            w.finish()?;
            self.runs.insert(0, path);
        }
        Ok(())
    }

    /// All records in key order (equal keys in no particular order).
    pub fn finish(mut self) -> Result<Merged> {
        if self.runs.is_empty() {
            self.sort_buffer();
            return Ok(Merged(Inner::Memory { data: std::mem::take(&mut self.data), recs: std::mem::take(&mut self.recs), pos: 0 }));
        }
        self.spill()?;
        Merged::runs(std::mem::take(&mut self.runs))
    }
}

struct RunWriter {
    w: BufWriter<FrameEncoder<File>>,
}

impl RunWriter {
    fn create(path: &Path) -> Result<RunWriter> {
        let f = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        Ok(RunWriter { w: BufWriter::with_capacity(1 << 16, FrameEncoder::new(f)) })
    }

    fn write(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        write_varint(&mut self.w, key.len() as u64)?;
        write_varint(&mut self.w, value.len() as u64)?;
        self.w.write_all(key)?;
        self.w.write_all(value)?;
        Ok(())
    }

    fn finish(self) -> Result<()> {
        let enc = self.w.into_inner().map_err(|e| e.into_error())?;
        enc.finish()?.sync_data().ok();
        Ok(())
    }
}

fn write_varint(w: &mut impl Write, mut v: u64) -> std::io::Result<()> {
    let mut buf = [0u8; 10];
    let mut n = 0;
    while v >= 0x80 {
        buf[n] = (v as u8) | 0x80;
        v >>= 7;
        n += 1;
    }
    buf[n] = v as u8;
    w.write_all(&buf[..=n])
}

/// Next varint, or `None` at a clean end of input.
fn read_varint(r: &mut impl Read) -> Result<Option<u64>> {
    let mut v = 0u64;
    let mut shift = 0;
    let mut b = [0u8; 1];
    loop {
        if r.read(&mut b)? == 0 {
            if shift == 0 {
                return Ok(None);
            }
            anyhow::bail!("truncated sort run");
        }
        v |= ((b[0] & 0x7f) as u64) << shift;
        if b[0] < 0x80 {
            return Ok(Some(v));
        }
        shift += 7;
        if shift > 63 {
            anyhow::bail!("corrupt sort run");
        }
    }
}

struct RunReader {
    r: BufReader<FrameDecoder<File>>,
    path: PathBuf,
    key: Vec<u8>,
    val: Vec<u8>,
}

impl RunReader {
    fn open(path: PathBuf) -> Result<RunReader> {
        let f = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        Ok(RunReader { r: BufReader::with_capacity(1 << 16, FrameDecoder::new(f)), path, key: Vec::new(), val: Vec::new() })
    }

    /// Read the next record into `key`/`val`; false at the end.
    fn advance(&mut self) -> Result<bool> {
        let Some(kl) = read_varint(&mut self.r)? else { return Ok(false) };
        let vl = read_varint(&mut self.r)?.context("truncated sort run")?;
        self.key.resize(kl as usize, 0);
        self.r.read_exact(&mut self.key)?;
        self.val.resize(vl as usize, 0);
        self.r.read_exact(&mut self.val)?;
        Ok(true)
    }
}

impl Drop for RunReader {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Sorted records of a finished sorter.
pub struct Merged(Inner);

enum Inner {
    Memory { data: Vec<u8>, recs: Vec<(u64, u32, u32)>, pos: usize },
    Runs { readers: Vec<RunReader>, heap: BinaryHeap<Reverse<(Vec<u8>, usize)>>, key: Vec<u8>, val: Vec<u8> },
}

impl Merged {
    fn runs(paths: Vec<PathBuf>) -> Result<Merged> {
        let mut readers = Vec::with_capacity(paths.len());
        let mut heap = BinaryHeap::new();
        for p in paths {
            let mut r = RunReader::open(p)?;
            if r.advance()? {
                heap.push(Reverse((std::mem::take(&mut r.key), readers.len())));
            }
            readers.push(r);
        }
        Ok(Merged(Inner::Runs { readers, heap, key: Vec::new(), val: Vec::new() }))
    }

    /// The next record; the slices stay valid until the next call.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<(&[u8], &[u8])>> {
        match &mut self.0 {
            Inner::Memory { data, recs, pos } => {
                let Some(&(off, kl, vl)) = recs.get(*pos) else { return Ok(None) };
                *pos += 1;
                let o = off as usize;
                Ok(Some((&data[o..o + kl as usize], &data[o + kl as usize..o + kl as usize + vl as usize])))
            }
            Inner::Runs { readers, heap, key, val } => {
                let Some(Reverse((k, i))) = heap.pop() else { return Ok(None) };
                // Hand the previous key buffer back to the reader for reuse.
                let old = std::mem::replace(key, k);
                let r = &mut readers[i];
                std::mem::swap(val, &mut r.val);
                r.key = old;
                if r.advance()? {
                    heap.push(Reverse((std::mem::take(&mut r.key), i)));
                }
                Ok(Some((&key[..], &val[..])))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_across_runs() {
        let dir = std::env::temp_dir().join(format!("overpass-sorter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Sorter::new(&dir, "t", 1 << 20);
        let n = 300_000u64;
        for i in 0..n {
            let k = (i.wrapping_mul(2_654_435_761) % n).to_be_bytes();
            s.push(&k, &i.to_le_bytes()).unwrap();
        }
        let mut m = s.finish().unwrap();
        let mut prev: Option<Vec<u8>> = None;
        let mut count = 0;
        while let Some((k, _)) = m.next().unwrap() {
            if let Some(p) = &prev {
                assert!(p.as_slice() <= k);
            }
            prev = Some(k.to_vec());
            count += 1;
        }
        assert_eq!(count, n);
        drop(m);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
