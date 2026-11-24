//! Compact binary encoding of stored values.
//!
//! Element chunks (data and meta tables) start with a one byte compression
//! flag (0 = raw, 1 = lz4 with prepended size). Their payload holds the
//! elements of a key range sorted by (cell, id); one chunk may span many
//! cells. It begins with a head: the element count, the cells as runs
//! (run count; per run the cell delta and length) and the ids (zigzag delta
//! at a run start, plain delta inside a run). Columns specific to the
//! element type follow. Tags come last: a per-chunk dictionary of keys and
//! of values, a tag count per element and (key, value) index pairs.
//!
//! Index values (id buckets, tag index entries, relation member buckets)
//! are uncompressed delta-coded varint lists.

use crate::geo::{Cell, CodeRange, Pt, Rect, cell_in_ranges, morton, unmorton};
use crate::model::*;
use anyhow::{Result, bail};
use rustc_hash::FxHashMap;
use std::ops::Range;

const COMPRESS_THRESHOLD: usize = 512;

/// Largest element chunk value worth building: four LMDB overflow pages
/// (4 KiB each, 16 bytes of page header). Chunks are packed close to it.
pub const CHUNK_MAX: usize = 4 * 4096 - 16;

/// Largest tag index entry or relation member bucket: small enough to stay
/// inside an LMDB leaf page.
pub const ENTRY_MAX: usize = 1900;

pub struct Writer {
    pub buf: Vec<u8>,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl Writer {
    pub fn new() -> Writer {
        Writer { buf: Vec::with_capacity(256) }
    }
    #[inline]
    pub fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
    #[inline]
    pub fn zigzag(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    #[inline]
    pub fn str(&mut self, s: &str) {
        self.varint(s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
    }
    #[inline]
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Finish the block, compressing it when worthwhile.
    pub fn finish(self) -> Vec<u8> {
        if self.buf.len() >= COMPRESS_THRESHOLD {
            let c = lz4_flex::compress_prepend_size(&self.buf);
            if c.len() + 1 < self.buf.len() {
                let mut out = Vec::with_capacity(c.len() + 1);
                out.push(1);
                out.extend_from_slice(&c);
                return out;
            }
        }
        let mut out = Vec::with_capacity(self.buf.len() + 1);
        out.push(0);
        out.extend_from_slice(&self.buf);
        out
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

#[cold]
#[inline(never)]
fn truncated() -> anyhow::Error {
    anyhow::anyhow!("truncated block")
}

#[cold]
#[inline(never)]
fn bad_varint() -> anyhow::Error {
    anyhow::anyhow!("bad varint")
}

/// Stored strings as `&str` (invalid UTF-8, which the writer never
/// produces, reads as U+FFFD).
#[inline]
fn as_str(b: &[u8]) -> &str {
    std::str::from_utf8(b).unwrap_or("\u{fffd}")
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    /// Bytes not yet read.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
    pub fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }
    /// Errors are built out of line (`truncated`, `bad_varint`), which keeps
    /// this loop small enough to inline into the decoding loops.
    #[inline(always)]
    pub fn varint(&mut self) -> Result<u64> {
        let mut v: u64 = 0;
        let mut shift = 0;
        let mut pos = self.pos;
        loop {
            let Some(&b) = self.buf.get(pos) else { return Err(truncated()) };
            pos += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                self.pos = pos;
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(bad_varint());
            }
        }
    }
    #[inline]
    pub fn zigzag(&mut self) -> Result<i64> {
        let v = self.varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }
    #[inline]
    pub fn byte(&mut self) -> Result<u8> {
        let Some(&b) = self.buf.get(self.pos) else { return Err(truncated()) };
        self.pos += 1;
        Ok(b)
    }
    /// The next `n` bytes.
    #[inline]
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let Some(b) = self.buf.get(self.pos..self.pos.saturating_add(n)) else { return Err(truncated()) };
        self.pos += n;
        Ok(b)
    }
    #[inline]
    pub fn str_ref(&mut self) -> Result<&'a str> {
        Ok(as_str(self.bytes_ref()?))
    }
    /// A length-prefixed string as bytes, not yet checked to be UTF-8.
    #[inline]
    pub fn bytes_ref(&mut self) -> Result<&'a [u8]> {
        let n = self.varint()? as usize;
        self.take(n)
    }
    /// Step over `n` varints without decoding them: a varint ends at a byte
    /// below 0x80, and those are counted eight bytes at a time.
    #[inline]
    pub fn skip_varints(&mut self, mut n: usize) -> Result<()> {
        const HIGH: u64 = 0x8080_8080_8080_8080;
        const LOW: u64 = 0x0101_0101_0101_0101;
        let mut pos = self.pos;
        while n > 0 {
            match self.buf.get(pos..pos + 8) {
                Some(b) => {
                    // byte i: the varints ending in bytes 0..=i (at most 8)
                    let ends = (!u64::from_le_bytes(b.try_into().unwrap()) & HIGH) >> 7;
                    let upto = ends.wrapping_mul(LOW);
                    let c = (upto >> 56) as usize;
                    if c < n {
                        n -= c;
                        pos += 8;
                        continue;
                    }
                    // the first byte where n end: 0x80 + upto - n has its
                    // high bit set from there on (no borrow, upto <= 8)
                    let reached = ((upto | HIGH) - n as u64 * LOW) & HIGH;
                    pos += reached.trailing_zeros() as usize / 8 + 1;
                    n = 0;
                }
                None => {
                    let Some(&b) = self.buf.get(pos) else { return Err(truncated()) };
                    pos += 1;
                    n -= (b < 0x80) as usize;
                }
            }
        }
        self.pos = pos;
        Ok(())
    }
    /// Element count with a sanity bound: every element takes at least one
    /// byte, so a corrupt count cannot cause a huge allocation.
    fn count(&mut self) -> Result<usize> {
        let n = self.varint()? as usize;
        if n > self.remaining().saturating_mul(8) + 64 {
            bail!("corrupt block: count {n}");
        }
        Ok(n)
    }
}

/// Decompress a stored block value into its payload.
pub fn payload(value: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>> {
    match value.first() {
        Some(0) => Ok(std::borrow::Cow::Borrowed(&value[1..])),
        Some(1) => Ok(std::borrow::Cow::Owned(lz4_flex::decompress_size_prepended(&value[1..])?)),
        _ => bail!("unknown block encoding"),
    }
}

/// Cells as one ordered number: level, then Morton code.
#[inline]
pub fn cell_value(c: Cell) -> u64 {
    ((c.level as u64) << 32) | c.code as u64
}

#[inline]
pub fn cell_from_value(v: u64) -> Cell {
    Cell { level: (v >> 32) as u8, code: v as u32 }
}

/// South-west corner of a cell in key space.
fn cell_origin(cell: Cell) -> Pt {
    if cell.level == 255 || cell.level == 0 {
        return Pt { y: 0, x: 0 };
    }
    let (ty, tx) = unmorton(cell.code);
    Pt { y: ty << 16, x: tx << 16 }
}

/// Which elements of a chunk to decode.
#[derive(Clone, Copy)]
pub enum Pick<'a> {
    All,
    /// Elements with these ids (sorted).
    Ids(&'a [Id]),
    /// Elements whose cell lies in these ranges (sorted, see `CodeRange`).
    Cells(&'a [CodeRange]),
}

impl Pick<'_> {
    /// At most this many of the `n` elements of a chunk are picked, as far
    /// as the pick tells without its tests (`Cells`: all of them).
    fn bound(&self, n: usize) -> usize {
        match self {
            Pick::All | Pick::Cells(_) => n,
            Pick::Ids(ids) => ids.len().min(n),
        }
    }
}

/// Tests the elements of a chunk head against a `Pick`, in the order of
/// the head: sorted by (cell, id), so the cell stays the same and the ids
/// ascend over a run of elements. A cell is looked up in the ranges once
/// per run; an id is searched from the place of the previous one of its
/// run (a node chunk holds thousands of ids in a few runs).
struct Picker<'a> {
    pick: Pick<'a>,
    /// `cell_value` of the cell of the previous element (u64::MAX before
    /// the first: no cell has that value).
    cell: u64,
    /// Id of the previous element.
    id: Id,
    /// `Cells`: whether the cell lies in the ranges.
    cell_ok: bool,
    /// `Ids`: the ids before this place are below `id`.
    at: usize,
    /// `Ids`: the id at `at` (Id::MAX past the end).
    next: Id,
}

impl<'a> Picker<'a> {
    fn new(pick: Pick<'a>) -> Picker<'a> {
        Picker { pick, cell: u64::MAX, id: 0, cell_ok: false, at: 0, next: 0 }
    }

    #[inline(always)]
    fn test(&mut self, cell: Cell, id: Id) -> bool {
        let key = cell_value(cell);
        match self.pick {
            Pick::All => true,
            Pick::Ids(ids) => {
                // within a run, below the next wanted id: most elements
                if key == self.cell && id >= self.id && id < self.next {
                    self.id = id;
                    return false;
                }
                self.seek(ids, key, id)
            }
            Pick::Cells(r) => {
                if key != self.cell {
                    self.cell = key;
                    self.cell_ok = cell_in_ranges(cell, r);
                }
                self.cell_ok
            }
        }
    }

    /// `Ids`: move `at` to the first id not below `id`; whether it is `id`.
    #[inline(never)]
    fn seek(&mut self, ids: &[Id], key: u64, id: Id) -> bool {
        if key != self.cell || id < self.id {
            self.cell = key;
            self.at = ids.partition_point(|x| *x < id);
        } else if self.at < ids.len() && ids[self.at] < id {
            // gallop: the next wanted id is mostly close
            let mut step = 1;
            while self.at + step < ids.len() && ids[self.at + step] < id {
                self.at += step;
                step *= 2;
            }
            let end = (self.at + step).min(ids.len());
            self.at += 1 + ids[self.at + 1..end].partition_point(|x| *x < id);
        }
        self.id = id;
        self.next = ids.get(self.at).copied().unwrap_or(Id::MAX);
        self.at < ids.len() && self.next == id
    }
}

// ---------------------------------------------------------------------------
// Chunk head and tags, shared by all element chunks.

fn write_head(w: &mut Writer, keys: &[(Cell, Id)]) {
    w.varint(keys.len() as u64);
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for (c, _) in keys {
        let v = cell_value(*c);
        match runs.last_mut() {
            Some((cv, n)) if *cv == v => *n += 1,
            _ => runs.push((v, 1)),
        }
    }
    w.varint(runs.len() as u64);
    let mut prev = 0u64;
    for (v, n) in &runs {
        w.varint(v.wrapping_sub(prev));
        w.varint(*n);
        prev = *v;
    }
    let mut prev_id = 0u64;
    let mut i = 0;
    for (_, n) in &runs {
        for j in 0..*n as usize {
            let id = keys[i].1;
            if j == 0 {
                w.zigzag(id.wrapping_sub(prev_id) as i64);
            } else {
                w.varint(id - prev_id);
            }
            prev_id = id;
            i += 1;
        }
    }
}

fn read_head(r: &mut Reader) -> Result<Vec<(Cell, Id)>> {
    let n = r.count()?;
    let nruns = r.count()?;
    let mut runs = Vec::with_capacity(nruns.min(n));
    let mut prev = 0u64;
    let mut total = 0usize;
    for _ in 0..nruns {
        prev = prev.wrapping_add(r.varint()?);
        let len = r.varint()? as usize;
        total = total.saturating_add(len);
        runs.push((cell_from_value(prev), len));
    }
    if total != n {
        bail!("corrupt chunk head");
    }
    let mut out = Vec::with_capacity(n);
    let mut prev_id = 0u64;
    for (cell, len) in runs {
        for j in 0..len {
            let id = if j == 0 { prev_id.wrapping_add(r.zigzag()? as u64) } else { prev_id.wrapping_add(r.varint()?) };
            out.push((cell, id));
            prev_id = id;
        }
    }
    Ok(out)
}

fn write_tags<'a>(w: &mut Writer, all: impl Iterator<Item = &'a Tags> + Clone) {
    let mut keys: FxHashMap<&str, u64> = FxHashMap::default();
    let mut vals: FxHashMap<&str, u64> = FxHashMap::default();
    let mut key_list: Vec<&str> = Vec::new();
    let mut val_list: Vec<&str> = Vec::new();
    let mut refs: Vec<(u64, u64)> = Vec::new();
    for t in all.clone() {
        for (k, v) in t.iter() {
            let ki = *keys.entry(k).or_insert_with(|| {
                key_list.push(k);
                key_list.len() as u64 - 1
            });
            let vi = *vals.entry(v).or_insert_with(|| {
                val_list.push(v);
                val_list.len() as u64 - 1
            });
            refs.push((ki, vi));
        }
    }
    w.varint(key_list.len() as u64);
    for k in &key_list {
        w.str(k);
    }
    w.varint(val_list.len() as u64);
    for v in &val_list {
        w.str(v);
    }
    for t in all {
        w.varint(t.len() as u64);
    }
    for (k, v) in refs {
        w.varint(k);
        w.varint(v);
    }
}

/// A string of a chunk's tag tables, checked as UTF-8 on first use.
enum Lazy<'a> {
    Raw(&'a [u8]),
    Str(&'a str),
}

impl<'a> Lazy<'a> {
    #[inline]
    fn len(&self) -> usize {
        match *self {
            Lazy::Str(s) => s.len(),
            Lazy::Raw(b) => b.len(),
        }
    }

    #[inline]
    fn get(&mut self) -> &'a str {
        match *self {
            Lazy::Str(s) => s,
            Lazy::Raw(b) => {
                let s = as_str(b);
                *self = Lazy::Str(s);
                s
            }
        }
    }
}

/// The string tables and the tag counts of a tag section of `n` elements.
/// Each string is checked as UTF-8 when an element first uses it (sparse
/// picks use few of them).
fn tag_tables<'a>(r: &mut Reader<'a>, n: usize) -> Result<(Vec<Lazy<'a>>, Vec<Lazy<'a>>, Vec<usize>)> {
    let nk = r.count()?;
    let mut keys: Vec<Lazy> = Vec::with_capacity(nk);
    for _ in 0..nk {
        keys.push(Lazy::Raw(r.bytes_ref()?));
    }
    let nv = r.count()?;
    let mut vals: Vec<Lazy> = Vec::with_capacity(nv);
    for _ in 0..nv {
        vals.push(Lazy::Raw(r.bytes_ref()?));
    }
    let mut counts = Vec::with_capacity(n);
    for _ in 0..n {
        counts.push(r.varint()? as usize);
    }
    Ok((keys, vals, counts))
}

/// Append the `c` tags at `r` to `t`.
#[inline]
fn push_tags<'a>(r: &mut Reader<'a>, c: usize, keys: &mut [Lazy<'a>], vals: &mut [Lazy<'a>], t: &mut Tags) -> Result<()> {
    // A first pass over the references sums the string lengths, so the
    // buffer of the tags is allocated once instead of growing by doubling.
    let start = r.pos;
    let mut bytes = 0;
    for _ in 0..c {
        let (ki, vi) = (r.varint()? as usize, r.varint()? as usize);
        let (Some(k), Some(v)) = (keys.get(ki), vals.get(vi)) else { bail!("corrupt tag reference") };
        bytes += k.len() + v.len();
    }
    r.pos = start;
    t.reserve(c, bytes);
    for _ in 0..c {
        let (ki, vi) = (r.varint()? as usize, r.varint()? as usize);
        let (Some(k), Some(v)) = (keys.get_mut(ki).map(Lazy::get), vals.get_mut(vi).map(Lazy::get)) else { bail!("corrupt tag reference") };
        t.push(k, v);
    }
    Ok(())
}

/// Read the tag section for the elements at `picked` (ascending positions
/// among `n`); `set(k, tags)` receives the tags of `picked[k]`.
fn read_tags(r: &mut Reader, n: usize, picked: &[usize], mut set: impl FnMut(usize, Tags)) -> Result<()> {
    if picked.is_empty() {
        return Ok(());
    }
    let (mut keys, mut vals, counts) = tag_tables(r, n)?;
    let mut p = 0;
    for (i, &c) in counts.iter().enumerate() {
        if picked[p] == i {
            let mut t = Tags::new();
            push_tags(r, c, &mut keys, &mut vals, &mut t)?;
            set(p, t);
            p += 1;
            if p == picked.len() {
                break;
            }
        } else {
            r.skip_varints(2 * c)?;
        }
    }
    Ok(())
}

/// What the `decode_*_where` functions keep: the elements whose tags pass
/// `keep`, in chunks whose payload passes `chunk`, which is false only
/// where no element can pass `keep` (a value the filters need does not
/// occur in it). The others are not decoded whole.
#[derive(Clone, Copy)]
pub struct TagTest<'a> {
    pub chunk: &'a (dyn Fn(&[u8]) -> bool + Sync),
    pub keep: &'a (dyn Fn(&Tags) -> bool + Sync),
    /// Keys that every element passing `keep` has (may be empty). A chunk
    /// without one of them in its key table is not read further, and the
    /// tags of an element are built only when its references hold them
    /// all: `nwr["shop"]["name"~"x"]` over a region builds the tags of the
    /// shops, not of every tagged element.
    pub keys: &'a [&'a str],
    /// Tests of a key: every element passing `keep` has, for each of them,
    /// a key that passes it (the key regex of `[~"k"~"v"]`; may be empty).
    /// Each is asked once per key of the key table of a chunk; a chunk
    /// without such a key is not read further, and the tags of an element
    /// are built only when its keys serve them all.
    pub key_tests: &'a [&'a (dyn Fn(&str) -> bool + Sync)],
}

/// Step over the head of a chunk (`read_head` without its list); the
/// number of elements.
fn skip_head(r: &mut Reader) -> Result<usize> {
    let n = r.count()?;
    let nruns = r.count()?;
    r.skip_varints(2 * nruns)?;
    r.skip_varints(n)?;
    Ok(n)
}

/// What the keys of the key table at `pos` (the start of a tag section)
/// serve of the needs of `test`: per key a bit for each required key it is
/// (`TagTest::keys`) and for each key test it passes
/// (`TagTest::key_tests`). None when a need is served by no key: no element
/// of the chunk passes. Empty when there are no needs (or more than 32:
/// then they are left to `keep`). A region scan asks this before it reads
/// the head of a chunk and picks its elements: most chunks of a region lack
/// a key that is not a common one, and reading the head and picking was
/// half of the time spent on them.
fn key_bits(buf: &[u8], pos: usize, test: &TagTest) -> Result<Option<Vec<u32>>> {
    let needs = test.keys.len() + test.key_tests.len();
    if needs == 0 || needs > 32 {
        return Ok(Some(Vec::new()));
    }
    let mut r = Reader { buf, pos };
    let nk = r.count()?;
    let mut bits = Vec::with_capacity(nk);
    let mut seen = 0u32;
    for _ in 0..nk {
        let k = r.bytes_ref()?;
        let mut b = 0u32;
        for (j, want) in test.keys.iter().enumerate() {
            if want.as_bytes() == k {
                b |= 1 << j;
            }
        }
        if !test.key_tests.is_empty() {
            let key = as_str(k);
            for (j, t) in test.key_tests.iter().enumerate() {
                if t(key) {
                    b |= 1 << (test.keys.len() + j);
                }
            }
        }
        seen |= b;
        bits.push(b);
    }
    Ok((seen == u32::MAX >> (32 - needs)).then_some(bits))
}

/// The picked elements (positions among `n`, ascending) whose tags pass
/// `keep`, with their tags, from the tag section at `r`. The tags of each
/// are tested in one reused buffer and copied when they pass.
fn tags_passing(r: &mut Reader, n: usize, picked: &[usize], test: TagTest, bits: Option<Vec<u32>>) -> Result<Vec<(usize, Tags)>> {
    let keep = test.keep;
    let mut out = Vec::new();
    if picked.is_empty() {
        return Ok(out);
    }
    // what each key of the key table serves (`key_bits`), and all needs
    let bits = match bits {
        Some(b) => b,
        None => match key_bits(r.buf, r.pos, &test)? {
            Some(b) => b,
            None => return Ok(out),
        },
    };
    let all = if bits.is_empty() { 0 } else { u32::MAX >> (32 - (test.keys.len() + test.key_tests.len())) };
    let (mut keys, mut vals, counts) = tag_tables(r, n)?;
    let mut t = Tags::new();
    let mut empty: Option<bool> = None;
    let mut p = 0;
    for (i, &c) in counts.iter().enumerate() {
        if picked[p] != i {
            r.skip_varints(2 * c)?;
            continue;
        }
        if c == 0 {
            // most nodes: the test of an element without tags, made once
            if all == 0 && *empty.get_or_insert_with(|| keep(&Tags::new())) {
                out.push((i, Tags::new()));
            }
        } else {
            let start = r.pos;
            let mut have = 0;
            if all != 0 {
                for _ in 0..c {
                    let ki = r.varint()? as usize;
                    r.varint()?;
                    have |= bits.get(ki).copied().unwrap_or(0);
                }
            }
            if have == all {
                r.pos = start;
                t.clear();
                push_tags(r, c, &mut keys, &mut vals, &mut t)?;
                if keep(&t) {
                    out.push((i, t.clone()));
                }
            }
        }
        p += 1;
        if p == picked.len() {
            break;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Nodes: coordinates relative to their level-16 tile (the cell).

pub fn encode_nodes(nodes: &[&Node]) -> Vec<u8> {
    let mut w = Writer::new();
    let keys: Vec<(Cell, Id)> = nodes.iter().map(|n| (n.cell(), n.id)).collect();
    write_head(&mut w, &keys);
    for n in nodes {
        w.u16((n.pos.y & 0xffff) as u16);
        w.u16((n.pos.x & 0xffff) as u16);
    }
    write_tags(&mut w, nodes.iter().map(|n| &n.tags));
    w.finish()
}

pub fn decode_nodes(value: &[u8], pick: Pick, with_tags: bool) -> Result<Vec<Node>> {
    let mut pick = Picker::new(pick);
    let data = payload(value)?;
    let mut r = Reader::new(&data);
    let head = read_head(&mut r)?;
    let coords = r.take(4 * head.len())?;
    // the places first, so that the nodes are allocated once
    let mut picked = Vec::with_capacity(pick.pick.bound(head.len()));
    for (i, &(cell, id)) in head.iter().enumerate() {
        if pick.test(cell, id) {
            picked.push(i);
        }
    }
    let mut out = Vec::with_capacity(picked.len());
    for &i in &picked {
        let (cell, id) = head[i];
        let (ty, tx) = unmorton(cell.code);
        let c = &coords[4 * i..4 * i + 4];
        let ly = u16::from_le_bytes([c[0], c[1]]) as u32;
        let lx = u16::from_le_bytes([c[2], c[3]]) as u32;
        out.push(Node { id, pos: Pt { y: (ty << 16) | ly, x: (tx << 16) | lx }, tags: Tags::new() });
    }
    if with_tags {
        read_tags(&mut r, head.len(), &picked, |k, t| out[k].tags = t)?;
    }
    Ok(out)
}

/// The positions of the nodes with the given ids (sorted) in a node chunk,
/// as (id, position): `decode_nodes` without tags for `Pick::Ids`, without
/// building the nodes or the list of the head (way geometry reads a few
/// nodes out of the thousands of a chunk).
pub fn decode_node_positions(value: &[u8], ids: &[Id]) -> Result<Vec<(Id, Pt)>> {
    let data = payload(value)?;
    let mut r = Reader::new(&data);
    let n = r.count()?;
    let nruns = r.count()?;
    let mut runs = Vec::with_capacity(nruns.min(n));
    let mut prev = 0u64;
    let mut total = 0usize;
    for _ in 0..nruns {
        prev = prev.wrapping_add(r.varint()?);
        let len = r.varint()? as usize;
        total = total.saturating_add(len);
        runs.push((cell_from_value(prev), len));
    }
    if total != n {
        bail!("corrupt chunk head");
    }
    let mut pick = Picker::new(Pick::Ids(ids));
    // place in the chunk, cell and id of the nodes found
    let mut found: Vec<(usize, Cell, Id)> = Vec::with_capacity(ids.len().min(n));
    let mut prev_id = 0u64;
    let mut i = 0;
    for (cell, len) in runs {
        for j in 0..len {
            let id = if j == 0 { prev_id.wrapping_add(r.zigzag()? as u64) } else { prev_id.wrapping_add(r.varint()?) };
            if pick.test(cell, id) {
                found.push((i, cell, id));
            }
            prev_id = id;
            i += 1;
        }
    }
    let coords = r.take(4 * n)?;
    Ok(found
        .into_iter()
        .map(|(i, cell, id)| {
            let (ty, tx) = unmorton(cell.code);
            let c = &coords[4 * i..4 * i + 4];
            let ly = u16::from_le_bytes([c[0], c[1]]) as u32;
            let lx = u16::from_le_bytes([c[2], c[3]]) as u32;
            (id, Pt { y: (ty << 16) | ly, x: (tx << 16) | lx })
        })
        .collect())
}

/// `decode_nodes` with tags of the picked nodes whose tags pass `keep`.
pub fn decode_nodes_where(value: &[u8], pick: Pick, test: TagTest) -> Result<Vec<Node>> {
    let mut pick = Picker::new(pick);
    let data = payload(value)?;
    if !(test.chunk)(&data) {
        return Ok(Vec::new());
    }
    let mut r = Reader::new(&data);
    let mut bits = Vec::new();
    if !test.keys.is_empty() || !test.key_tests.is_empty() {
        let n = skip_head(&mut r)?;
        r.take(4 * n)?;
        match key_bits(&data, r.pos, &test)? {
            Some(b) => bits = b,
            None => return Ok(Vec::new()),
        }
        r = Reader::new(&data);
    }
    let head = read_head(&mut r)?;
    let coords = r.take(4 * head.len())?;
    let mut picked: Vec<usize> = Vec::with_capacity(head.len());
    for (i, &(cell, id)) in head.iter().enumerate() {
        if pick.test(cell, id) {
            picked.push(i);
        }
    }
    let pass = tags_passing(&mut r, head.len(), &picked, test, Some(bits))?;
    Ok(pass
        .into_iter()
        .map(|(i, tags)| {
            let (cell, id) = head[i];
            let (ty, tx) = unmorton(cell.code);
            let c = &coords[4 * i..4 * i + 4];
            let ly = u16::from_le_bytes([c[0], c[1]]) as u32;
            let lx = u16::from_le_bytes([c[2], c[3]]) as u32;
            Node { id, pos: Pt { y: (ty << 16) | ly, x: (tx << 16) | lx }, tags }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Ways

fn write_rect(w: &mut Writer, cell: Cell, r: &Rect) {
    let o = cell_origin(cell);
    w.varint((r.y0 - o.y.min(r.y0)) as u64);
    w.varint((r.x0 - o.x.min(r.x0)) as u64);
    w.varint((r.y1 - r.y0) as u64);
    w.varint((r.x1 - r.x0) as u64);
}

fn read_rect(r: &mut Reader, cell: Cell) -> Result<Rect> {
    let o = cell_origin(cell);
    let y0 = o.y as u64 + r.varint()?;
    let x0 = o.x as u64 + r.varint()?;
    let y1 = y0 + r.varint()?;
    let x1 = x0 + r.varint()?;
    if y1 > u32::MAX as u64 || x1 > u32::MAX as u64 {
        bail!("corrupt bounding box");
    }
    Ok(Rect { y0: y0 as u32, x0: x0 as u32, y1: y1 as u32, x1: x1 as u32 })
}

/// Bounding box and one flag: bit 0 = box present, bit 1 = `flag`.
fn write_box(w: &mut Writer, cell: Cell, r: &Rect, flag: bool) {
    w.varint((!r.is_empty()) as u64 | (flag as u64) << 1);
    if !r.is_empty() {
        write_rect(w, cell, r);
    }
}

fn read_box(r: &mut Reader, cell: Cell) -> Result<(Rect, bool)> {
    let h = r.varint()?;
    let rect = if h & 1 != 0 { read_rect(r, cell)? } else { Rect::EMPTY };
    Ok((rect, h & 2 != 0))
}

pub fn encode_ways(ways: &[&Way]) -> Vec<u8> {
    let mut w = Writer::new();
    let keys: Vec<(Cell, Id)> = ways.iter().map(|x| (x.cell(), x.id)).collect();
    write_head(&mut w, &keys);
    let mut pidx = 0i64;
    let mut pn = 0i64;
    for (way, (cell, _)) in ways.iter().zip(&keys) {
        write_box(&mut w, *cell, &way.bbox, way.missing);
        w.zigzag(way.idx as i64 - pidx);
        pidx = way.idx as i64;
        w.varint(way.nodes.len() as u64);
        for &nd in &way.nodes {
            w.zigzag((nd as i64).wrapping_sub(pn));
            pn = nd as i64;
        }
    }
    write_tags(&mut w, ways.iter().map(|x| &x.tags));
    w.finish()
}

pub fn decode_ways(value: &[u8], pick: Pick, with_tags: bool) -> Result<Vec<Way>> {
    let mut pick = Picker::new(pick);
    let data = payload(value)?;
    let mut r = Reader::new(&data);
    let head = read_head(&mut r)?;
    let mut out = Vec::new();
    let mut picked = Vec::new();
    let mut pidx = 0i64;
    let mut pn = 0i64;
    for (i, &(cell, id)) in head.iter().enumerate() {
        let (bbox, missing) = read_box(&mut r, cell)?;
        pidx += r.zigzag()?;
        let nn = r.count()?;
        if pick.test(cell, id) {
            let mut nodes = Vec::with_capacity(nn);
            for _ in 0..nn {
                pn = pn.wrapping_add(r.zigzag()?);
                nodes.push(pn as u64);
            }
            out.push(Way { id, nodes, tags: Tags::new(), bbox, idx: pidx as u32, missing });
            picked.push(i);
        } else {
            for _ in 0..nn {
                pn = pn.wrapping_add(r.zigzag()?);
            }
        }
    }
    if with_tags {
        read_tags(&mut r, head.len(), &picked, |k, t| out[k].tags = t)?;
    }
    Ok(out)
}

/// `decode_ways` with tags, of the picked ways whose box passes `boxed`
/// and whose node list then passes `keep`. The node list of a way is read
/// into one buffer and copied when the way is kept; the tags of the others
/// are not built (`way(bn)` keeps the ways of a region that use nodes of a
/// set, a part of those stored around them).
pub fn decode_ways_using(value: &[u8], pick: Pick, boxed: impl Fn(&Rect) -> bool, keep: impl Fn(&[Id]) -> bool) -> Result<Vec<Way>> {
    let mut pick = Picker::new(pick);
    let data = payload(value)?;
    let mut r = Reader::new(&data);
    let head = read_head(&mut r)?;
    let mut out = Vec::new();
    let mut picked = Vec::new();
    let mut nodes: Vec<Id> = Vec::new();
    let mut pidx = 0i64;
    let mut pn = 0i64;
    for (i, &(cell, id)) in head.iter().enumerate() {
        let (bbox, missing) = read_box(&mut r, cell)?;
        pidx += r.zigzag()?;
        let nn = r.count()?;
        if pick.test(cell, id) && boxed(&bbox) {
            nodes.clear();
            nodes.reserve(nn);
            for _ in 0..nn {
                pn = pn.wrapping_add(r.zigzag()?);
                nodes.push(pn as u64);
            }
            if keep(&nodes) {
                out.push(Way { id, nodes: nodes.clone(), tags: Tags::new(), bbox, idx: pidx as u32, missing });
                picked.push(i);
            }
        } else {
            for _ in 0..nn {
                pn = pn.wrapping_add(r.zigzag()?);
            }
        }
    }
    read_tags(&mut r, head.len(), &picked, |k, t| out[k].tags = t)?;
    Ok(out)
}

/// `decode_ways` with tags of the picked ways whose tags pass `keep`. A
/// first pass steps over the node lists to the tags (`skip_varints`); the
/// ways that pass are then decoded, the node lists up to the last of them
/// as before (node ids are deltas across ways). Where few pass (the
/// candidates of a tag index read with another tag filter), most of the
/// chunk is not decoded.
pub fn decode_ways_where(value: &[u8], pick: Pick, test: TagTest) -> Result<Vec<Way>> {
    let mut pick = Picker::new(pick);
    let data = payload(value)?;
    if !(test.chunk)(&data) {
        return Ok(Vec::new());
    }
    let mut r = Reader::new(&data);
    let n = skip_head(&mut r)?;
    let body = r.pos;
    for _ in 0..n {
        // box flags, the box when present, the index, the node list
        let h = r.varint()?;
        r.skip_varints(if h & 1 != 0 { 5 } else { 1 })?;
        let nn = r.count()?;
        r.skip_varints(nn)?;
    }
    let Some(bits) = key_bits(&data, r.pos, &test)? else { return Ok(Vec::new()) };
    let head = read_head(&mut Reader::new(&data))?;
    let mut picked = Vec::with_capacity(pick.pick.bound(n));
    for (i, &(cell, id)) in head.iter().enumerate() {
        if pick.test(cell, id) {
            picked.push(i);
        }
    }
    let pass = tags_passing(&mut r, head.len(), &picked, test, Some(bits))?;
    let Some(&(last, _)) = pass.last() else { return Ok(Vec::new()) };
    let mut r = Reader { buf: &data, pos: body };
    let mut out = Vec::with_capacity(pass.len());
    let mut pass = pass.into_iter().peekable();
    let mut pidx = 0i64;
    let mut pn = 0i64;
    for (i, &(cell, id)) in head[..=last].iter().enumerate() {
        let (bbox, missing) = read_box(&mut r, cell)?;
        pidx += r.zigzag()?;
        let nn = r.count()?;
        match pass.next_if(|(p, _)| *p == i) {
            Some((_, tags)) => {
                let mut nodes = Vec::with_capacity(nn);
                for _ in 0..nn {
                    pn = pn.wrapping_add(r.zigzag()?);
                    nodes.push(pn as u64);
                }
                out.push(Way { id, nodes, tags, bbox, idx: pidx as u32, missing });
            }
            None => {
                for _ in 0..nn {
                    pn = pn.wrapping_add(r.zigzag()?);
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Relations; roles go through a per-chunk dictionary.

pub fn encode_relations(rels: &[&Relation]) -> Vec<u8> {
    let mut w = Writer::new();
    let keys: Vec<(Cell, Id)> = rels.iter().map(|x| (x.cell(), x.id)).collect();
    write_head(&mut w, &keys);
    let mut roles: Vec<&str> = Vec::new();
    let mut role_idx: FxHashMap<&str, u64> = FxHashMap::default();
    for rel in rels {
        for m in &rel.members {
            role_idx.entry(&*m.role).or_insert_with(|| {
                roles.push(&*m.role);
                roles.len() as u64 - 1
            });
        }
    }
    w.varint(roles.len() as u64);
    for r in &roles {
        w.str(r);
    }
    let mut pidx = 0i64;
    let mut pm = [0i64; 3];
    for (rel, (cell, _)) in rels.iter().zip(&keys) {
        write_box(&mut w, *cell, &rel.bbox, rel.spatial);
        w.zigzag(rel.idx as i64 - pidx);
        pidx = rel.idx as i64;
        w.varint(rel.members.len() as u64);
        for m in &rel.members {
            let t = m.mtype.code().min(2) as usize;
            w.varint((role_idx[&*m.role] << 2) | t as u64);
            w.zigzag((m.id as i64).wrapping_sub(pm[t]));
            pm[t] = m.id as i64;
        }
    }
    write_tags(&mut w, rels.iter().map(|x| &x.tags));
    w.finish()
}

pub fn decode_relations(value: &[u8], pick: Pick, with_tags: bool) -> Result<Vec<Relation>> {
    let mut pick = Picker::new(pick);
    let data = payload(value)?;
    let mut r = Reader::new(&data);
    let head = read_head(&mut r)?;
    let nroles = r.count()?;
    let mut roles: Vec<std::sync::Arc<str>> = Vec::with_capacity(nroles);
    for _ in 0..nroles {
        roles.push(std::sync::Arc::from(r.str_ref()?));
    }
    let mut out = Vec::new();
    let mut picked = Vec::new();
    let mut pidx = 0i64;
    let mut pm = [0i64; 3];
    for (i, &(cell, id)) in head.iter().enumerate() {
        let (bbox, spatial) = read_box(&mut r, cell)?;
        pidx += r.zigzag()?;
        let nm = r.count()?;
        let want = pick.test(cell, id);
        let mut members = Vec::with_capacity(if want { nm } else { 0 });
        for _ in 0..nm {
            let h = r.varint()?;
            let t = (h & 3) as usize;
            if t == 3 {
                bail!("corrupt relation block: member type 3");
            }
            pm[t] = pm[t].wrapping_add(r.zigzag()?);
            if want {
                let Some(role) = roles.get((h >> 2) as usize) else { bail!("corrupt relation block: role") };
                members.push(Member { mtype: ElemType::from_code(t as u8), id: pm[t] as u64, role: role.clone() });
            }
        }
        if want {
            out.push(Relation { id, members, tags: Tags::new(), bbox, idx: pidx as u32, spatial });
            picked.push(i);
        }
    }
    if with_tags {
        read_tags(&mut r, head.len(), &picked, |k, t| out[k].tags = t)?;
    }
    Ok(out)
}

/// `decode_relations` with tags of the picked relations whose tags pass
/// `keep`, as `decode_ways_where` (member ids are deltas across
/// relations).
pub fn decode_relations_where(value: &[u8], pick: Pick, test: TagTest) -> Result<Vec<Relation>> {
    let mut pick = Picker::new(pick);
    let data = payload(value)?;
    if !(test.chunk)(&data) {
        return Ok(Vec::new());
    }
    let mut r = Reader::new(&data);
    let n = skip_head(&mut r)?;
    let nroles = r.count()?;
    let roles_at = r.pos;
    for _ in 0..nroles {
        r.bytes_ref()?;
    }
    let body = r.pos;
    for _ in 0..n {
        // box flags, the box when present, the index, the members (role
        // and type, id)
        let h = r.varint()?;
        r.skip_varints(if h & 1 != 0 { 5 } else { 1 })?;
        let nm = r.count()?;
        r.skip_varints(2 * nm)?;
    }
    let Some(bits) = key_bits(&data, r.pos, &test)? else { return Ok(Vec::new()) };
    let head = read_head(&mut Reader::new(&data))?;
    let mut picked = Vec::with_capacity(pick.pick.bound(n));
    for (i, &(cell, id)) in head.iter().enumerate() {
        if pick.test(cell, id) {
            picked.push(i);
        }
    }
    let pass = tags_passing(&mut r, head.len(), &picked, test, Some(bits))?;
    let Some(&(last, _)) = pass.last() else { return Ok(Vec::new()) };
    let mut r = Reader { buf: &data, pos: roles_at };
    let mut roles: Vec<std::sync::Arc<str>> = Vec::with_capacity(nroles);
    for _ in 0..nroles {
        roles.push(std::sync::Arc::from(r.str_ref()?));
    }
    debug_assert_eq!(r.pos, body);
    let mut out = Vec::with_capacity(pass.len());
    let mut pass = pass.into_iter().peekable();
    let mut pidx = 0i64;
    let mut pm = [0i64; 3];
    for (i, &(cell, id)) in head[..=last].iter().enumerate() {
        let (bbox, spatial) = read_box(&mut r, cell)?;
        pidx += r.zigzag()?;
        let nm = r.count()?;
        let want = pass.next_if(|(p, _)| *p == i);
        let mut members = Vec::with_capacity(if want.is_some() { nm } else { 0 });
        for _ in 0..nm {
            let h = r.varint()?;
            let t = (h & 3) as usize;
            if t == 3 {
                bail!("corrupt relation block: member type 3");
            }
            pm[t] = pm[t].wrapping_add(r.zigzag()?);
            if want.is_some() {
                let Some(role) = roles.get((h >> 2) as usize) else { bail!("corrupt relation block: role") };
                members.push(Member { mtype: ElemType::from_code(t as u8), id: pm[t] as u64, role: role.clone() });
            }
        }
        if let Some((_, tags)) = want {
            out.push(Relation { id, members, tags, bbox, idx: pidx as u32, spatial });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Areas

pub fn encode_areas(areas: &[&Area]) -> Vec<u8> {
    let mut w = Writer::new();
    let keys: Vec<(Cell, Id)> = areas.iter().map(|x| (x.cell(), x.id)).collect();
    write_head(&mut w, &keys);
    let mut pidx = 0i64;
    for (a, (cell, _)) in areas.iter().zip(&keys) {
        write_box(&mut w, *cell, &a.bbox, false);
        w.zigzag(a.idx as i64 - pidx);
        pidx = a.idx as i64;
        w.varint(a.used.len() as u64);
        let mut pu = 0u64;
        for &u in &a.used {
            w.varint((u as u64).wrapping_sub(pu));
            pu = u as u64;
        }
    }
    write_tags(&mut w, areas.iter().map(|x| &x.tags));
    w.finish()
}

pub fn decode_areas(value: &[u8], pick: Pick) -> Result<Vec<Area>> {
    let mut pick = Picker::new(pick);
    let data = payload(value)?;
    let mut r = Reader::new(&data);
    let head = read_head(&mut r)?;
    let mut out = Vec::new();
    let mut picked = Vec::new();
    let mut pidx = 0i64;
    for (i, &(cell, id)) in head.iter().enumerate() {
        let (bbox, _) = read_box(&mut r, cell)?;
        pidx += r.zigzag()?;
        let nu = r.count()?;
        let mut used = Vec::with_capacity(nu);
        let mut pu = 0u64;
        for _ in 0..nu {
            pu = pu.wrapping_add(r.varint()?);
            used.push(pu as u32);
        }
        if pick.test(cell, id) {
            out.push(Area { id, tags: Tags::new(), used, idx: pidx as u32, bbox });
            picked.push(i);
        }
    }
    read_tags(&mut r, head.len(), &picked, |k, t| out[k].tags = t)?;
    Ok(out)
}

/// `decode_areas` of the picked areas whose tags pass `keep`, as
/// `decode_ways_where`; the tile lists of the others are stepped over.
pub fn decode_areas_where(value: &[u8], pick: Pick, test: TagTest) -> Result<Vec<Area>> {
    let mut pick = Picker::new(pick);
    let data = payload(value)?;
    if !(test.chunk)(&data) {
        return Ok(Vec::new());
    }
    let mut r = Reader::new(&data);
    let head = read_head(&mut r)?;
    // per picked area: where its box starts, its index
    let mut picked = Vec::new();
    let mut at = Vec::new();
    let mut pidx = 0i64;
    for (i, &(cell, id)) in head.iter().enumerate() {
        let start = r.pos;
        let h = r.varint()?;
        if h & 1 != 0 {
            r.skip_varints(4)?;
        }
        pidx += r.zigzag()?;
        let nu = r.count()?;
        r.skip_varints(nu)?;
        if pick.test(cell, id) {
            picked.push(i);
            at.push((start, pidx));
        }
    }
    let pass = tags_passing(&mut r, head.len(), &picked, test, None)?;
    let mut out = Vec::with_capacity(pass.len());
    let mut k = 0;
    for (i, tags) in pass {
        while picked[k] != i {
            k += 1;
        }
        let (start, idx) = at[k];
        let mut r = Reader { buf: &data, pos: start };
        let (cell, id) = head[i];
        let (bbox, _) = read_box(&mut r, cell)?;
        r.zigzag()?;
        let nu = r.count()?;
        let mut used = Vec::with_capacity(nu);
        let mut pu = 0u64;
        for _ in 0..nu {
            pu = pu.wrapping_add(r.varint()?);
            used.push(pu as u32);
        }
        out.push(Area { id, tags, used, idx: idx as u32, bbox });
    }
    Ok(out)
}

/// Area blocks of one area in one tile: polylines of (ilat, ilon).
pub fn encode_area_blocks(blocks: &[Vec<(u32, i32)>]) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(blocks.len() as u64);
    for b in blocks {
        w.varint(b.len() as u64);
        let (mut pl, mut po) = (0i64, 0i64);
        for &(la, lo) in b {
            w.zigzag(la as i64 - pl);
            w.zigzag(lo as i64 - po);
            pl = la as i64;
            po = lo as i64;
        }
    }
    w.finish()
}

pub fn decode_area_blocks(value: &[u8]) -> Result<Vec<Vec<(u32, i32)>>> {
    let data = payload(value)?;
    let mut r = Reader::new(&data);
    let n = r.count()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let m = r.count()?;
        let mut b = Vec::with_capacity(m);
        let (mut pl, mut po) = (0i64, 0i64);
        for _ in 0..m {
            pl += r.zigzag()?;
            po += r.zigzag()?;
            b.push((pl as u32, po as i32));
        }
        out.push(b);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Meta chunks: own key ranges, columns version, timestamp, changeset, uid
// (the last three as deltas to the previous element).

/// Meta data of one element, stored under the element's (cell, id).
#[derive(Clone, Debug, PartialEq)]
pub struct MetaRec {
    pub cell: Cell,
    pub id: Id,
    pub meta: Meta,
}

pub fn encode_meta(recs: &[&MetaRec]) -> Vec<u8> {
    let mut w = Writer::new();
    let keys: Vec<(Cell, Id)> = recs.iter().map(|m| (m.cell, m.id)).collect();
    write_head(&mut w, &keys);
    for m in recs {
        w.varint(m.meta.version as u64);
    }
    let mut p = 0i64;
    for m in recs {
        w.zigzag(m.meta.timestamp.wrapping_sub(p));
        p = m.meta.timestamp;
    }
    let mut p = 0i64;
    for m in recs {
        w.zigzag((m.meta.changeset as i64).wrapping_sub(p));
        p = m.meta.changeset as i64;
    }
    let mut p = 0i64;
    for m in recs {
        w.zigzag(m.meta.uid as i64 - p);
        p = m.meta.uid as i64;
    }
    w.finish()
}

/// The sums of a delta column of `n` values at the places `at`
/// (ascending, not empty), handed to `set` with the number of the place;
/// the values after the last place are stepped over.
#[inline(always)]
fn delta_column(r: &mut Reader, n: usize, at: &[u32], mut set: impl FnMut(usize, i64)) -> Result<()> {
    let mut sum = 0i64;
    let mut i = 0u32;
    for (k, &p) in at.iter().enumerate() {
        while i <= p {
            sum = sum.wrapping_add(r.zigzag()?);
            i += 1;
        }
        set(k, sum);
    }
    r.skip_varints(n - i as usize)
}

/// The meta data of the picked elements of a chunk. The head is tested
/// as it is read, and the columns are read for the picked elements only:
/// the version column is stepped over between them, the three delta
/// columns are summed up to the last of them (`out meta` of a box takes a
/// part of most meta chunks it reads, which hold some thousand elements).
pub fn decode_meta(value: &[u8], pick: Pick) -> Result<Vec<MetaRec>> {
    let data = payload(value)?;
    let mut r = Reader::new(&data);
    let n = r.count()?;
    let nruns = r.count()?;
    let mut runs = Vec::with_capacity(nruns.min(n));
    let mut prev = 0u64;
    let mut total = 0usize;
    for _ in 0..nruns {
        prev = prev.wrapping_add(r.varint()?);
        let len = r.varint()? as usize;
        total = total.saturating_add(len);
        runs.push((cell_from_value(prev), len));
    }
    if total != n || n > u32::MAX as usize {
        bail!("corrupt chunk head");
    }
    // the places of the picked elements among the n of the chunk
    let bound = if matches!(pick, Pick::Cells(_)) { 0 } else { pick.bound(n) };
    let mut pick = Picker::new(pick);
    let mut at: Vec<u32> = Vec::with_capacity(bound);
    let mut out: Vec<MetaRec> = Vec::with_capacity(bound);
    let mut prev_id = 0u64;
    let mut i = 0u32;
    for (cell, len) in runs {
        for j in 0..len {
            let id = if j == 0 { prev_id.wrapping_add(r.zigzag()? as u64) } else { prev_id.wrapping_add(r.varint()?) };
            if pick.test(cell, id) {
                at.push(i);
                out.push(MetaRec { cell, id, meta: Meta::default() });
            }
            prev_id = id;
            i += 1;
        }
    }
    if at.is_empty() {
        return Ok(out);
    }
    let mut i = 0;
    for (m, &p) in out.iter_mut().zip(&at) {
        r.skip_varints(p as usize - i)?;
        m.meta.version = r.varint()? as u32;
        i = p as usize + 1;
    }
    r.skip_varints(n - i)?;
    delta_column(&mut r, n, &at, |k, v| out[k].meta.timestamp = v)?;
    delta_column(&mut r, n, &at, |k, v| out[k].meta.changeset = v as u64)?;
    delta_column(&mut r, n, &at, |k, v| out[k].meta.uid = v as u32)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Packing sorted elements into chunks.

/// Split `n` sorted items into consecutive chunks whose encoding stays
/// within `CHUNK_MAX` (single items may exceed it), filling each close to
/// it. `encode` encodes a range of the items.
pub fn pack(n: usize, mut encode: impl FnMut(Range<usize>) -> Vec<u8>) -> Vec<(Range<usize>, Vec<u8>)> {
    const TARGET: f64 = CHUNK_MAX as f64 * 0.97;
    let mut out = Vec::new();
    if n == 0 {
        return out;
    }
    // Items per chunk, estimated from a sample.
    let sample = n.min(256);
    let mut guess = ((sample as f64 * TARGET / encode(0..sample).len().max(1) as f64) as usize).max(1);
    let mut start = 0;
    while start < n {
        let rest = n - start;
        let mut cnt = guess.clamp(1, rest);
        // Largest count known to fit (with its encoding), smallest known not to.
        let mut fit: Option<(usize, Vec<u8>)> = None;
        let mut too_big = usize::MAX;
        for _ in 0..8 {
            let v = encode(start..start + cnt);
            let len = v.len().max(1) as f64;
            if v.len() > CHUNK_MAX && cnt > 1 {
                too_big = cnt;
                let lower = fit.as_ref().map_or(1, |(c, _)| c + 1);
                if lower >= cnt {
                    break;
                }
                cnt = ((cnt as f64 * TARGET / len) as usize).clamp(lower, cnt - 1);
                continue;
            }
            fit = Some((cnt, v));
            if cnt == rest || len >= TARGET * 0.92 {
                break;
            }
            let next = ((cnt as f64 * TARGET / len) as usize).max(cnt + 1).min(rest).min(too_big.saturating_sub(1));
            if next <= cnt {
                break;
            }
            cnt = next;
        }
        let (c, v) = match fit {
            Some(f) => f,
            None => {
                // Every try was too big: halve until it fits.
                let mut c = cnt;
                loop {
                    let v = encode(start..start + c);
                    if v.len() <= CHUNK_MAX || c == 1 {
                        break (c, v);
                    }
                    c /= 2;
                }
            }
        };
        out.push((start..start + c, v));
        start += c;
        guess = c;
    }
    out
}

// ---------------------------------------------------------------------------
// Id index buckets: the cells of ids `bucket << ID_BUCKET_BITS ..`, as
// (low bits, cell) entries sorted by id. Each entry: varint (low bits delta
// << 2 | t) where t = 0: same cell as the previous entry, 1: a step at the
// same level, 2: a level byte and a step (see `cell_step`).

pub const ID_BUCKET_BITS: u32 = 9;

fn spread32(v: u32) -> u64 {
    let mut x = v as u64;
    x = (x | x << 16) & 0x0000_ffff_0000_ffff;
    x = (x | x << 8) & 0x00ff_00ff_00ff_00ff;
    x = (x | x << 4) & 0x0f0f_0f0f_0f0f_0f0f;
    x = (x | x << 2) & 0x3333_3333_3333_3333;
    (x | x << 1) & 0x5555_5555_5555_5555
}

fn compact32(mut x: u64) -> u32 {
    x &= 0x5555_5555_5555_5555;
    x = (x | x >> 1) & 0x3333_3333_3333_3333;
    x = (x | x >> 2) & 0x0f0f_0f0f_0f0f_0f0f;
    x = (x | x >> 4) & 0x00ff_00ff_00ff_00ff;
    x = (x | x >> 8) & 0x0000_ffff_0000_ffff;
    (x | x >> 16) as u32
}

/// Offset between two cell codes: the zigzag-coded tile deltas in y and x,
/// interleaved, so that nearby cells give small numbers.
fn cell_step(from: u32, to: u32) -> u64 {
    let (fy, fx) = unmorton(from);
    let (ty, tx) = unmorton(to);
    let zz = |d: i64| ((d << 1) ^ (d >> 63)) as u32;
    spread32(zz(ty as i64 - fy as i64)) << 1 | spread32(zz(tx as i64 - fx as i64))
}

fn apply_step(from: u32, step: u64) -> Result<u32> {
    let (fy, fx) = unmorton(from);
    let unzz = |z: u32| ((z >> 1) as i64) ^ -((z & 1) as i64);
    let y = fy as i64 + unzz(compact32(step >> 1));
    let x = fx as i64 + unzz(compact32(step));
    if !(0..=0xffff).contains(&y) || !(0..=0xffff).contains(&x) {
        bail!("corrupt cell step");
    }
    Ok(morton(y as u32, x as u32))
}

pub fn encode_id_bucket(entries: &[(u16, Cell)]) -> Vec<u8> {
    let mut w = Writer::new();
    let (mut plo, mut prev) = (0u16, Cell::tile(0));
    for &(lo, cell) in entries {
        let t = if cell == prev { 0 } else if cell.level == prev.level { 1 } else { 2 };
        w.varint(((lo - plo) as u64) << 2 | t);
        if t == 2 {
            w.buf.push(cell.level);
        }
        if t != 0 {
            w.varint(cell_step(prev.code, cell.code));
        }
        (plo, prev) = (lo, cell);
    }
    w.buf
}

/// The cells of some entries of an id bucket: `wanted` holds low bits in
/// ascending order, and `found` gets the place in `wanted` and the cell of
/// each one the bucket holds. The same cells as `decode_id_bucket`, with
/// the tile coordinates kept apart between the entries and put together
/// only for those found, and the reading stopped after the last one
/// wanted: a lookup of the members of long routes wants about three ids of
/// a bucket of 512.
pub fn pick_id_bucket(value: &[u8], wanted: &[u16], mut found: impl FnMut(usize, Cell)) -> Result<()> {
    let mut r = Reader::new(value);
    let unzz = |z: u32| ((z >> 1) as i64) ^ -((z & 1) as i64);
    // from the cell the entries start at, as in `decode_id_bucket`
    let start = Cell::tile(0);
    let (mut plo, mut level) = (0u64, start.level);
    let (y0, x0) = unmorton(start.code);
    let (mut y, mut x) = (y0 as i64, x0 as i64);
    let mut j = 0;
    while j < wanted.len() && !r.at_end() {
        let h = r.varint()?;
        plo += h >> 2;
        match h & 3 {
            0 => {}
            t @ (1 | 2) => {
                if t == 2 {
                    level = r.byte()?;
                }
                let step = r.varint()?;
                y += unzz(compact32(step >> 1));
                x += unzz(compact32(step));
                if !(0..=0xffff).contains(&y) || !(0..=0xffff).contains(&x) {
                    bail!("corrupt cell step");
                }
            }
            _ => bail!("corrupt id bucket"),
        }
        if plo >= 1 << ID_BUCKET_BITS {
            bail!("corrupt id bucket");
        }
        while j < wanted.len() && (wanted[j] as u64) < plo {
            j += 1;
        }
        if j < wanted.len() && wanted[j] as u64 == plo {
            found(j, Cell { level, code: morton(y as u32, x as u32) });
            j += 1;
        }
    }
    Ok(())
}

pub fn decode_id_bucket(value: &[u8]) -> Result<Vec<(u16, Cell)>> {
    let mut r = Reader::new(value);
    let mut out = Vec::with_capacity(value.len());
    let (mut plo, mut prev) = (0u64, Cell::tile(0));
    while !r.at_end() {
        let h = r.varint()?;
        plo += h >> 2;
        match h & 3 {
            0 => {}
            1 => prev.code = apply_step(prev.code, r.varint()?)?,
            2 => {
                prev.level = r.byte()?;
                prev.code = apply_step(prev.code, r.varint()?)?;
            }
            _ => bail!("corrupt id bucket"),
        }
        if plo >= 1 << ID_BUCKET_BITS {
            bail!("corrupt id bucket");
        }
        out.push((plo as u16, prev));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tag index entries: the ids of one (key, value) in a range of cells,
// starting at the cell of the entry key. Per cell: varint cell delta, varint
// count, the first id as zigzag delta to the previous cell's first id, then
// id deltas.

pub fn encode_tag_entry(base: Cell, groups: &[(Cell, Vec<Id>)]) -> Vec<u8> {
    let mut w = Writer::new();
    let mut pc = cell_value(base);
    let mut pfirst = 0u64;
    for (cell, ids) in groups {
        let v = cell_value(*cell);
        w.varint(v - pc);
        pc = v;
        w.varint(ids.len() as u64);
        let mut prev = 0;
        for (i, &id) in ids.iter().enumerate() {
            if i == 0 {
                w.zigzag(id.wrapping_sub(pfirst) as i64);
                pfirst = id;
            } else {
                w.varint(id - prev);
            }
            prev = id;
        }
    }
    w.buf
}

pub fn decode_tag_entry(base: Cell, value: &[u8]) -> Result<Vec<(Cell, Vec<Id>)>> {
    let mut r = Reader::new(value);
    let mut out = Vec::new();
    let mut pc = cell_value(base);
    let mut pfirst = 0u64;
    while !r.at_end() {
        pc = pc.wrapping_add(r.varint()?);
        let n = r.count()?;
        let mut ids = Vec::with_capacity(n);
        let mut prev = 0u64;
        for i in 0..n {
            let id = if i == 0 {
                pfirst = pfirst.wrapping_add(r.zigzag()? as u64);
                pfirst
            } else {
                prev.wrapping_add(r.varint()?)
            };
            ids.push(id);
            prev = id;
        }
        out.push((cell_from_value(pc), ids));
    }
    Ok(out)
}

/// The groups of a tag index entry in turn, as `decode_tag_entry` gives
/// them, without building the list: `f(cell, ids)` gets the ids of the
/// cells that pass `want` (the ids of the others are stepped over) and
/// returns false to stop. Returns false if `f` stopped.
pub fn visit_tag_entry(base: Cell, value: &[u8], mut want: impl FnMut(Cell) -> bool, mut f: impl FnMut(Cell, &[Id]) -> Result<bool>) -> Result<bool> {
    let mut r = Reader::new(value);
    let mut pc = cell_value(base);
    let mut pfirst = 0u64;
    let mut ids: Vec<Id> = Vec::new();
    while !r.at_end() {
        pc = pc.wrapping_add(r.varint()?);
        let n = r.count()?;
        let cell = cell_from_value(pc);
        if n == 0 {
            if want(cell) && !f(cell, &[])? {
                return Ok(false);
            }
            continue;
        }
        pfirst = pfirst.wrapping_add(r.zigzag()? as u64);
        if !want(cell) {
            r.skip_varints(n - 1)?;
            continue;
        }
        ids.clear();
        ids.reserve(n);
        ids.push(pfirst);
        let mut prev = pfirst;
        for _ in 1..n {
            prev = prev.wrapping_add(r.varint()?);
            ids.push(prev);
        }
        if !f(cell, &ids)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Encoded size of `encode_tag_entry` for one more group, for packing.
pub fn tag_group_size(ids: &[Id]) -> usize {
    let mut s = 6 + varint_len(ids.len() as u64) + 5;
    for w in ids.windows(2) {
        s += varint_len(w[1] - w[0]);
    }
    s
}

fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

// ---------------------------------------------------------------------------
// Relation member buckets: (member id low byte, relation id) pairs of the
// member ids `bucket << 8 ..`, sorted; varint low byte delta, zigzag
// relation id delta.

pub fn encode_member_bucket(entries: &[(u8, Id)]) -> Vec<u8> {
    let mut w = Writer::new();
    let (mut plo, mut prel) = (0u8, 0u64);
    for &(lo, rel) in entries {
        w.varint((lo - plo) as u64);
        w.zigzag(rel.wrapping_sub(prel) as i64);
        (plo, prel) = (lo, rel);
    }
    w.buf
}

pub fn decode_member_bucket(value: &[u8]) -> Result<Vec<(u8, Id)>> {
    let mut r = Reader::new(value);
    let mut out = Vec::new();
    let (mut plo, mut prel) = (0u64, 0u64);
    while !r.at_end() {
        plo += r.varint()?;
        prel = prel.wrapping_add(r.zigzag()? as u64);
        if plo > 255 {
            bail!("corrupt member bucket");
        }
        out.push((plo as u8, prel));
    }
    Ok(out)
}

/// Morton code helper re-exported for tests.
pub fn tile_code(cy: u32, cx: u32) -> u32 {
    morton(cy, cx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::anchor_cell;

    #[test]
    fn node_positions_match_decode_nodes() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut nodes: Vec<Node> = (0..3000)
            .map(|_| {
                let tile = rnd() % 7;
                let pos = Pt { y: (1000 + tile as u32) << 16 | (rnd() as u32 & 0xffff), x: (2000 + (tile as u32 * 3) % 5) << 16 | (rnd() as u32 & 0xffff) };
                Node { id: rnd() % 20_000, pos, tags: if rnd() % 5 == 0 { Tags::from_pairs([("k", "v")]) } else { Tags::new() } }
            })
            .collect();
        nodes.sort_by_key(|n| n.id);
        nodes.dedup_by_key(|n| n.id);
        nodes.sort_by_key(|n| (n.cell(), n.id));
        let enc = encode_nodes(&nodes.iter().collect::<Vec<_>>());
        for step in [1usize, 2, 3, 17, 400, 5000] {
            for off in [0u64, 1, 7] {
                let mut ids: Vec<Id> = (0..20_000u64).filter(|i| (i + off) % step as u64 == 0).collect();
                ids.push(1 << 40);
                let want: Vec<(Id, Pt)> = decode_nodes(&enc, Pick::Ids(&ids), false).unwrap().into_iter().map(|n| (n.id, n.pos)).collect();
                let all: Vec<(Id, Pt)> = nodes.iter().filter(|n| ids.binary_search(&n.id).is_ok()).map(|n| (n.id, n.pos)).collect();
                assert_eq!(want, all, "step {step} off {off}");
                assert_eq!(decode_node_positions(&enc, &ids).unwrap(), all, "step {step} off {off}");
            }
        }
        assert!(decode_node_positions(&enc, &[]).unwrap().is_empty());
    }

    /// A damaged compressed chunk is an error or some payload, never a
    /// panic: cut at every length, single bytes changed, a wrong size in
    /// front. A fuzz test (AGENTS.md): run it on its own with
    /// `cargo test --release -- --ignored damaged_chunks_do_not_panic`.
    #[test]
    #[ignore]
    fn damaged_chunks_do_not_panic() {
        let mut r = Rnd(0x5eed_1234_abcd_ef01);
        let nodes: Vec<Node> = (0..3000u64).map(|i| Node { id: i * 3 + 1, pos: Pt { y: 0x6000_0000 + r.n(1 << 16) as u32, x: 0x7000_0000 + r.n(1 << 16) as u32 }, tags: some_tags(&mut r) }).collect();
        let mut sorted: Vec<&Node> = nodes.iter().collect();
        sorted.sort_by_key(|n| (n.cell(), n.id));
        let enc = encode_nodes(&sorted);
        assert_eq!(enc[0], 1, "the chunk is compressed");
        let whole = payload(&enc).unwrap().into_owned();
        for cut in 0..enc.len() {
            if let Ok(p) = payload(&enc[..cut]) {
                assert!(p.len() <= whole.len());
            }
        }
        for _ in 0..20_000 {
            let mut bad = enc.clone();
            for _ in 0..1 + r.n(3) {
                let at = 1 + r.n(bad.len() as u64 - 1) as usize;
                bad[at] = r.n(256) as u8;
            }
            if r.n(4) == 0 {
                bad.truncate(1 + r.n(bad.len() as u64) as usize);
            }
            // the size in front (4 bytes) stays small: it is what is allocated
            if u32::from_le_bytes([bad.get(1).copied().unwrap_or(0), bad.get(2).copied().unwrap_or(0), bad.get(3).copied().unwrap_or(0), bad.get(4).copied().unwrap_or(0)]) > 1 << 24 {
                continue;
            }
            if let Ok(p) = payload(&bad) {
                let _ = decode_nodes(&p_as_raw(&p), Pick::All, true);
            }
        }
        fn p_as_raw(p: &[u8]) -> Vec<u8> {
            let mut v = vec![0u8];
            v.extend_from_slice(p);
            v
        }
    }

    #[test]
    fn nodes_roundtrip() {
        let p = Pt::from_ll(51.0125, 7.0125);
        let q = Pt::from_ll(-33.5, 151.2);
        let mut nodes = vec![
            Node { id: 5, pos: p, tags: Tags::from_pairs([("a", "b")]) },
            Node { id: 9, pos: Pt { y: p.y + 3, x: p.x + 7 }, tags: Tags::new() },
            Node { id: 2, pos: q, tags: Tags::from_pairs([("a", "c"), ("x", "b")]) },
        ];
        nodes.sort_by_key(|n| (n.cell(), n.id));
        let refs: Vec<&Node> = nodes.iter().collect();
        let enc = encode_nodes(&refs);
        assert_eq!(decode_nodes(&enc, Pick::All, true).unwrap(), nodes);
        let one = decode_nodes(&enc, Pick::Ids(&[9]), true).unwrap();
        assert_eq!(one, vec![nodes.iter().find(|n| n.id == 9).unwrap().clone()]);
        let c = nodes[0].cell();
        let r = [CodeRange { level: c.level, lo: c.code, hi: c.code }];
        assert!(decode_nodes(&enc, Pick::Cells(&r), true).unwrap().iter().all(|n| n.cell() == c));
    }

    #[test]
    fn ways_and_relations_roundtrip() {
        let mut bbox = Rect::point(Pt::from_ll(51.0, 7.0));
        bbox.extend(Pt::from_ll(51.01, 7.02));
        let way = Way { id: 77, nodes: vec![5, 3, 1_000_000_000_000], tags: Tags::from_pairs([("highway".to_string(), "x".repeat(600))]), bbox, idx: 0x8000_0001, missing: true };
        let empty = Way { id: 78, nodes: vec![1], tags: Tags::new(), bbox: Rect::EMPTY, idx: 0xfe, missing: true };
        let enc = encode_ways(&[&way, &empty]);
        assert_eq!(decode_ways(&enc, Pick::All, true).unwrap(), vec![way.clone(), empty.clone()]);
        assert_eq!(decode_ways(&enc, Pick::Ids(&[78]), true).unwrap(), vec![empty]);
        let rel = Relation {
            id: 3,
            members: vec![
                Member { mtype: ElemType::Way, id: 77, role: "outer".into() },
                Member { mtype: ElemType::Node, id: 5, role: "".into() },
                Member { mtype: ElemType::Relation, id: 2, role: "outer".into() },
            ],
            tags: Tags::new(),
            bbox,
            idx: 7,
            spatial: true,
        };
        assert_eq!(anchor_cell(&bbox), rel.cell());
        let enc = encode_relations(&[&rel]);
        assert_eq!(decode_relations(&enc, Pick::All, true).unwrap(), vec![rel]);
    }

    #[test]
    fn meta_and_indexes_roundtrip() {
        let c = Cell::tile(12345);
        let m = vec![
            MetaRec { cell: c, id: 1, meta: Meta { version: 3, timestamp: 1_700_000_000, changeset: 99, uid: 7 } },
            MetaRec { cell: c, id: 4, meta: Meta::default() },
        ];
        let refs: Vec<&MetaRec> = m.iter().collect();
        assert_eq!(decode_meta(&encode_meta(&refs), Pick::All).unwrap(), m);
        let b = vec![(0u16, Cell::tile(5)), (1, Cell::tile(5)), (2, Cell::tile(0xffff_ffff)), (3, Cell { level: 3, code: 0 }), (511, Cell::NONE)];
        assert_eq!(decode_id_bucket(&encode_id_bucket(&b)).unwrap(), b);
        let g = vec![(Cell::tile(9), vec![4, 7, 100]), (Cell::tile(11), vec![2]), (Cell::NONE, vec![1 << 40])];
        assert_eq!(decode_tag_entry(Cell::tile(8), &encode_tag_entry(Cell::tile(8), &g)).unwrap(), g);
        // visit_tag_entry: the same groups, for every choice of cells and stop
        let g = vec![(Cell::tile(9), vec![4, 7, 100]), (Cell::tile(10), vec![]), (Cell::tile(11), vec![2]), (Cell::tile(500), vec![50, 51, 1 << 33]), (Cell::NONE, vec![1 << 40])];
        let enc = encode_tag_entry(Cell::tile(8), &g);
        assert_eq!(decode_tag_entry(Cell::tile(8), &enc).unwrap(), g);
        for mask in 0u32..32 {
            for stop in 0..=g.len() {
                let wanted: Vec<&(Cell, Vec<Id>)> = g.iter().enumerate().filter(|(i, _)| mask >> i & 1 == 1).map(|(_, x)| x).collect();
                let mut seen: Vec<(Cell, Vec<Id>)> = Vec::new();
                let done = visit_tag_entry(Cell::tile(8), &enc, |c| wanted.iter().any(|w| w.0 == c), |c, ids| {
                    seen.push((c, ids.to_vec()));
                    Ok(seen.len() != stop)
                })
                .unwrap();
                let expect: Vec<(Cell, Vec<Id>)> = wanted.iter().take(if stop == 0 { usize::MAX } else { stop }).map(|x| (*x).clone()).collect();
                assert_eq!(seen, expect, "mask {mask} stop {stop}");
                assert_eq!(done, stop == 0 || wanted.len() < stop, "mask {mask} stop {stop}");
            }
        }
        let mb = vec![(0u8, 9u64), (0, 12), (200, 3)];
        assert_eq!(decode_member_bucket(&encode_member_bucket(&mb)).unwrap(), mb);
    }

    /// rv-scan: `decode_meta` picks the first and the last element of a
    /// chunk, both, and those of a chunk of one element; cells change
    /// between them; a cut chunk is an error whatever is picked.
    #[test]
    fn rv_decode_meta_ends() {
        for n in [1usize, 2, 3, 7, 1000, 4097] {
            let mut recs: Vec<MetaRec> = Vec::new();
            for i in 0..n {
                let cell = Cell::tile(1000 + (i / 3) as u32);
                let meta = Meta { version: 1 + i as u32, timestamp: 1_500_000_000 - (i as i64) * 7_919, changeset: (i as u64) << 33, uid: u32::MAX - i as u32 };
                recs.push(MetaRec { cell, id: (1u64 << 40) + (i as u64) * 3, meta });
            }
            let refs: Vec<&MetaRec> = recs.iter().collect();
            let enc = encode_meta(&refs);
            let (first, last) = (recs[0].id, recs[n - 1].id);
            for want in [vec![first], vec![last], vec![first, last], vec![first - 1, last + 1], vec![]] {
                let mut w = want.clone();
                w.dedup();
                let got = decode_meta(&enc, Pick::Ids(&w)).unwrap();
                let exp: Vec<MetaRec> = recs.iter().filter(|m| w.contains(&m.id)).cloned().collect();
                assert_eq!(got, exp, "n {n} want {w:?}");
            }
            let c = recs[n - 1].cell;
            let ranges = [CodeRange { level: c.level, lo: c.code, hi: c.code }];
            assert_eq!(decode_meta(&enc, Pick::Cells(&ranges)).unwrap(), recs.iter().filter(|m| m.cell == c).cloned().collect::<Vec<_>>());
            for cut in [1, 2, 5] {
                if enc.len() > cut + 1 && enc[0] == 0 {
                    assert!(decode_meta(&enc[..enc.len() - cut], Pick::Ids(&[last])).is_err(), "n {n} cut {cut}");
                }
            }
        }
    }

    /// `decode_meta` gives the records that were encoded, for every kind
    /// of pick: all, ids present, absent and in part, cells in ranges.
    #[test]
    fn decode_meta_picks() {
        let mut r = Rnd(0x1234_5678_9abc_def1);
        for round in 0..300 {
            let n = 1 + r.n(if round % 10 == 0 { 3000 } else { 60 }) as usize;
            let mut recs: Vec<MetaRec> = Vec::new();
            let mut cell = Cell::tile(r.n(1 << 20) as u32);
            let mut id = r.n(1 << 34);
            for _ in 0..n {
                if r.n(6) == 0 {
                    cell = Cell::tile(cell.code + 1 + r.n(5) as u32);
                    id = r.n(1 << 34);
                }
                id += 1 + r.skewed(0, 12);
                let meta = Meta { version: 1 + r.skewed(0, 10) as u32, timestamp: 1_100_000_000 + r.n(700_000_000) as i64, changeset: r.n(1 << 28), uid: r.n(1 << 24) as u32 };
                recs.push(MetaRec { cell, id, meta });
            }
            recs.sort_by_key(|m| (m.cell, m.id));
            recs.dedup_by_key(|m| (m.cell, m.id));
            let refs: Vec<&MetaRec> = recs.iter().collect();
            let enc = encode_meta(&refs);
            assert_eq!(decode_meta(&enc, Pick::All).unwrap(), recs);
            for density in [1u64, 2, 5, 50] {
                let mut ids: Vec<Id> = recs.iter().filter(|_| r.n(density) == 0).map(|m| m.id).collect();
                // ids that are not in the chunk
                for _ in 0..r.n(4) {
                    ids.push(r.n(1 << 35));
                }
                ids.sort_unstable();
                ids.dedup();
                let want: Vec<MetaRec> = recs.iter().filter(|m| ids.binary_search(&m.id).is_ok()).cloned().collect();
                assert_eq!(decode_meta(&enc, Pick::Ids(&ids)).unwrap(), want, "round {round} density {density}");
            }
            let c = recs[r.n(recs.len() as u64) as usize].cell;
            let ranges = [CodeRange { level: c.level, lo: c.code, hi: c.code }];
            let want: Vec<MetaRec> = recs.iter().filter(|m| m.cell == c).cloned().collect();
            assert_eq!(decode_meta(&enc, Pick::Cells(&ranges)).unwrap(), want, "round {round} cells");
            // a cut chunk is an error, not a short answer
            assert!(decode_meta(&enc[..enc.len() - 1], Pick::All).is_err());
        }
    }

    /// `pick_id_bucket` finds the cells `decode_id_bucket` gives, for any
    /// wanted entries: buckets of cells at random levels and steps, with
    /// runs of one cell, and wanted low bits present, absent and past the
    /// last entry.
    #[test]
    fn pick_id_bucket_matches_decode() {
        let mut r = Rnd(0x2545_f491_4f6c_dd1d);
        for _ in 0..2000 {
            let mut entries: Vec<(u16, Cell)> = Vec::new();
            let mut lo = r.n(4) as u16;
            let mut cell = Cell::tile(r.n(1 << 32) as u32);
            while lo < 512 {
                if r.n(3) > 0 {
                    let level = if r.n(4) == 0 || cell.level > 16 { r.n(17) as u8 } else { cell.level };
                    let code = if level == 0 { 0 } else { ((r.n(1 << 32) as u32) >> (32 - 2 * level as u32)) << (32 - 2 * level as u32) };
                    cell = if r.n(50) == 0 { Cell::NONE } else { Cell { level, code } };
                }
                entries.push((lo, cell));
                let gap = if r.n(2) == 0 { 2 } else { 40 };
                lo += 1 + r.n(gap) as u16;
            }
            let enc = encode_id_bucket(&entries);
            let all = decode_id_bucket(&enc).unwrap();
            assert_eq!(all, entries);
            let k = r.n(12);
            let mut wanted: Vec<u16> = (0..k).map(|_| r.n(520) as u16).collect();
            wanted.sort_unstable();
            wanted.dedup();
            let mut got = Vec::new();
            pick_id_bucket(&enc, &wanted, |j, c| got.push((wanted[j], c))).unwrap();
            let expect: Vec<(u16, Cell)> = wanted.iter().filter_map(|w| all.binary_search_by_key(w, |e| e.0).ok().map(|p| (*w, all[p].1))).collect();
            assert_eq!(got, expect);
        }
    }

    /// xorshift numbers.
    struct Rnd(u64);

    impl Rnd {
        /// A number below `n`.
        fn n(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) % n.max(1)
        }

        /// A number below 2^(`lo` + a number below `spread`).
        fn skewed(&mut self, lo: u64, spread: u64) -> u64 {
            let b = self.n(spread);
            self.n(1 << (lo + b))
        }

        /// A length below `small`, one time in `rare` below `big`.
        fn len(&mut self, rare: u64, big: u64, small: u64) -> u64 {
            let m = if self.n(rare) == 0 { big } else { small };
            self.n(m)
        }
    }

    /// `skip_varints` ends where decoding as many varints ends: values of
    /// every length, counts across word borders and up to the end.
    #[test]
    fn skip_varints_as_decoding() {
        let mut r = Rnd(0x9e37_79b9_7f4a_7c15);
        for case in 0..300 {
            let n = 1 + r.n(60) as usize;
            let vals: Vec<u64> = (0..n).map(|_| if r.n(64) == 0 { u64::MAX } else { r.n(u64::MAX) >> r.n(64) }).collect();
            let mut w = Writer::new();
            for v in &vals {
                w.varint(*v);
            }
            for k in 0..=n {
                let mut r = Reader::new(&w.buf);
                r.skip_varints(k).unwrap();
                let rest: Vec<u64> = (k..n).map(|_| r.varint().unwrap()).collect();
                assert_eq!(rest, vals[k..], "case {case} skip {k}");
                assert!(r.at_end());
            }
            assert!(Reader::new(&w.buf).skip_varints(n + 1).is_err(), "case {case}");
        }
    }

    /// Tags from a small vocabulary, some of them none.
    fn some_tags(r: &mut Rnd) -> Tags {
        let keys = ["name", "highway", "boundary", "building", "x"];
        let vals = ["Zürich", "Bern", "residential", "administrative", "yes", "", "a long value that is not short at all"];
        Tags::from_pairs((0..r.n(4)).map(|_| (keys[r.n(5) as usize], vals[r.n(7) as usize])))
    }

    /// The `decode_*_where` functions give the elements of the plain
    /// decoding that pass `keep`, for every pick, and nothing where the
    /// chunk test fails.
    #[test]
    fn decoding_where_as_decoding_and_filtering() {
        let mut r = Rnd(0x2545_f491_4f6c_dd1d);
        let keeps: [&(dyn Fn(&Tags) -> bool + Sync); 6] = [
            &|_| true,
            &|_| false,
            &|t| crate::model::tag(t, "name") == Some("Zürich"),
            &|t| t.is_empty(),
            &|t| t.iter().any(|(k, v)| k.starts_with('b') && !v.is_empty()),
            &|t| t.iter().any(|(k, _)| k.starts_with('b')) && t.iter().any(|(k, _)| k.len() == 1) && crate::model::tag(t, "highway").is_some(),
        ];
        // tests of keys that the last two need
        let (kb, k1): (&(dyn Fn(&str) -> bool + Sync), &(dyn Fn(&str) -> bool + Sync)) = (&|k| k.starts_with('b'), &|k| k.len() == 1);
        let no_key: &(dyn Fn(&str) -> bool + Sync) = &|k| k == "no such key";
        for case in 0..40 {
            let n = 1 + r.n(400);
            let rect = |r: &mut Rnd| {
                if r.n(10) == 0 {
                    return Rect::EMPTY;
                }
                let (y, x) = (0x6000_0000 + r.n(1 << 24) as u32, 0x7000_0000 + r.n(1 << 24) as u32);
                Rect { y0: y, x0: x, y1: y + r.skewed(4, 20) as u32, x1: x + r.skewed(4, 20) as u32 }
            };
            let mut ways: Vec<Way> = (0..n)
                .map(|i| Way { id: i * 3 + r.n(3), nodes: (0..r.len(20, 2000, 30)).map(|_| r.n(1 << 40)).collect(), tags: some_tags(&mut r), bbox: rect(&mut r), idx: r.n(1 << 32) as u32, missing: r.n(5) == 0 })
                .collect();
            ways.sort_by_key(|w| (w.cell(), w.id));
            let roles = ["", "outer", "inner", "stop"];
            let types = [ElemType::Node, ElemType::Way, ElemType::Relation];
            let mut rels: Vec<Relation> = (0..n)
                .map(|i| Relation {
                    id: i * 2 + r.n(2),
                    members: (0..r.len(20, 1500, 12)).map(|_| Member { mtype: types[r.n(3) as usize], id: r.n(1 << 36), role: roles[r.n(4) as usize].into() }).collect(),
                    tags: some_tags(&mut r),
                    bbox: rect(&mut r),
                    idx: r.n(1 << 32) as u32,
                    spatial: r.n(2) == 0,
                })
                .collect();
            rels.sort_by_key(|r| (r.cell(), r.id));
            let mut areas: Vec<Area> = (0..n)
                .map(|i| {
                    let mut used: Vec<u32> = (0..r.n(40)).map(|_| r.n(1 << 32) as u32).collect();
                    used.sort_unstable();
                    used.dedup();
                    Area { id: i * 2 + r.n(2), tags: some_tags(&mut r), used, idx: r.n(1 << 32) as u32, bbox: rect(&mut r) }
                })
                .collect();
            areas.sort_by_key(|a| (a.cell(), a.id));
            let mut nodes: Vec<Node> = (0..n).map(|i| Node { id: i * 2 + r.n(2), pos: Pt { y: 0x6000_0000 + r.n(1 << 24) as u32, x: 0x7000_0000 + r.n(1 << 24) as u32 }, tags: some_tags(&mut r) }).collect();
            nodes.sort_by_key(|n| (n.cell(), n.id));
            let enc_w = encode_ways(&ways.iter().collect::<Vec<_>>());
            let enc_r = encode_relations(&rels.iter().collect::<Vec<_>>());
            let enc_a = encode_areas(&areas.iter().collect::<Vec<_>>());
            let enc_n = encode_nodes(&nodes.iter().collect::<Vec<_>>());
            let some = |ids: Vec<Id>, r: &mut Rnd| -> Vec<Id> {
                let mut v: Vec<Id> = ids.into_iter().filter(|_| r.n(4) == 0).collect();
                v.push(u64::MAX);
                v.sort_unstable();
                v
            };
            let (iw, ir) = (some(ways.iter().map(|w| w.id).collect(), &mut r), some(rels.iter().map(|r| r.id).collect(), &mut r));
            let (ia, inn) = (some(areas.iter().map(|a| a.id).collect(), &mut r), some(nodes.iter().map(|n| n.id).collect(), &mut r));
            let cell = ways[0].cell();
            let cells = [CodeRange { level: cell.level, lo: cell.code, hi: cell.code }];
            for (k, keep) in keeps.iter().enumerate() {
                // the third test needs the key "name"; a key no element has leaves nothing
                let keys: &[&str] = match k {
                    2 => &["name"],
                    5 => &["highway"],
                    _ => &[],
                };
                let key_tests: &[&(dyn Fn(&str) -> bool + Sync)] = match k {
                    4 => &[kb],
                    5 => &[kb, k1],
                    _ => &[],
                };
                let all = TagTest { chunk: &|_| true, keep: *keep, keys, key_tests };
                let no_such = TagTest { chunk: &|_| true, keep: *keep, keys: &[], key_tests: &[no_key] };
                assert!(decode_ways_where(&enc_w, Pick::All, no_such).unwrap().is_empty() && decode_nodes_where(&enc_n, Pick::All, no_such).unwrap().is_empty() && decode_relations_where(&enc_r, Pick::All, no_such).unwrap().is_empty() && decode_areas_where(&enc_a, Pick::All, no_such).unwrap().is_empty());
                let none = TagTest { chunk: &|_| false, keep: *keep, keys: &[], key_tests: &[] };
                let absent = TagTest { chunk: &|_| true, keep: *keep, keys: &["no such key"], key_tests: &[] };
                assert!(decode_ways_where(&enc_w, Pick::All, absent).unwrap().is_empty() && decode_nodes_where(&enc_n, Pick::All, absent).unwrap().is_empty());
                let at = format!("case {case} keep {k}");
                for (pw, pr, pa, pn) in [(Pick::All, Pick::All, Pick::All, Pick::All), (Pick::Ids(&iw), Pick::Ids(&ir), Pick::Ids(&ia), Pick::Ids(&inn)), (Pick::Cells(&cells), Pick::Cells(&cells), Pick::Cells(&cells), Pick::Cells(&cells))] {
                    let mut w = decode_ways(&enc_w, pw, true).unwrap();
                    w.retain(|e| keep(&e.tags));
                    assert_eq!(decode_ways_where(&enc_w, pw, all).unwrap(), w, "{at}");
                    let mut r = decode_relations(&enc_r, pr, true).unwrap();
                    r.retain(|e| keep(&e.tags));
                    assert_eq!(decode_relations_where(&enc_r, pr, all).unwrap(), r, "{at}");
                    let mut a = decode_areas(&enc_a, pa).unwrap();
                    a.retain(|e| keep(&e.tags));
                    assert_eq!(decode_areas_where(&enc_a, pa, all).unwrap(), a, "{at}");
                    let mut nd = decode_nodes(&enc_n, pn, true).unwrap();
                    nd.retain(|e| keep(&e.tags));
                    assert_eq!(decode_nodes_where(&enc_n, pn, all).unwrap(), nd, "{at}");
                    assert!(decode_ways_where(&enc_w, pw, none).unwrap().is_empty());
                    assert!(decode_relations_where(&enc_r, pr, none).unwrap().is_empty());
                    assert!(decode_areas_where(&enc_a, pa, none).unwrap().is_empty());
                    assert!(decode_nodes_where(&enc_n, pn, none).unwrap().is_empty());
                }
            }
        }
    }

    /// `decode_ways_using` gives the ways `decode_ways` gives that pass
    /// both tests.
    #[test]
    fn ways_using_as_decoding_and_filtering() {
        let mut r = Rnd(0x0dd0_1234_5678_9abc);
        for round in 0..200 {
            let mut ways: Vec<Way> = Vec::new();
            for id in 1..=1 + r.n(80) {
                let nodes: Vec<Id> = (0..r.len(8, 300, 12)).map(|_| 1 + r.n(400)).collect();
                let y0 = r.n(1 << 31) as u32;
                let x0 = r.n(1 << 31) as u32;
                let bbox = if r.n(10) == 0 { Rect::EMPTY } else { Rect { y0, x0, y1: y0 + r.skewed(4, 16) as u32, x1: x0 + r.skewed(4, 16) as u32 } };
                ways.push(Way { id, nodes, tags: some_tags(&mut r), bbox, idx: r.n(1 << 32) as u32, missing: r.n(5) == 0 });
            }
            ways.sort_by_key(|w| (w.cell(), w.id));
            let enc = encode_ways(&ways.iter().collect::<Vec<_>>());
            let wanted: Vec<Id> = (0..r.n(40)).map(|_| 1 + r.n(400)).collect();
            let limit = r.n(1 << 31) as u32;
            let boxed = |b: &Rect| !b.is_empty() && b.y0 <= limit;
            let keep = |n: &[Id]| n.iter().any(|x| wanted.contains(x));
            let mut ids: Vec<Id> = ways.iter().filter(|_| r.n(2) == 0).map(|w| w.id).collect();
            ids.sort_unstable();
            for pick in [Pick::All, Pick::Ids(&ids)] {
                let want: Vec<Way> = decode_ways(&enc, pick, true).unwrap().into_iter().filter(|w| boxed(&w.bbox) && keep(&w.nodes)).collect();
                assert_eq!(decode_ways_using(&enc, pick, boxed, keep).unwrap(), want, "round {round}");
            }
        }
    }

    #[test]
    fn pack_fills_chunks() {
        let nodes: Vec<Node> = (0..20000u64).map(|i| Node { id: i, pos: Pt { y: (i * 7919) as u32, x: (i * 104729) as u32 }, tags: Tags::new() }).collect();
        let parts = pack(nodes.len(), |r| encode_nodes(&nodes[r].iter().collect::<Vec<_>>()));
        assert_eq!(parts.iter().map(|(r, _)| r.len()).sum::<usize>(), nodes.len());
        assert!(parts.iter().all(|(_, v)| v.len() <= CHUNK_MAX));
        let full = parts[..parts.len() - 1].iter().filter(|(_, v)| v.len() * 10 >= CHUNK_MAX * 8).count();
        assert_eq!(full, parts.len() - 1);
    }
}
