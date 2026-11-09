//! OSM element model shared by storage, executor and output.

use crate::geo::{Cell, Pt, Rect};
use std::fmt;

pub type Id = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ElemType {
    Node,
    Way,
    Relation,
    Area,
    Derived,
}

impl ElemType {
    pub fn name(self) -> &'static str {
        match self {
            ElemType::Node => "node",
            ElemType::Way => "way",
            ElemType::Relation => "relation",
            ElemType::Area => "area",
            ElemType::Derived => "derived",
        }
    }
    pub fn code(self) -> u8 {
        self as u8
    }
    pub fn from_code(c: u8) -> ElemType {
        match c {
            0 => ElemType::Node,
            1 => ElemType::Way,
            2 => ElemType::Relation,
            3 => ElemType::Area,
            _ => ElemType::Derived,
        }
    }
    pub fn from_name(s: &str) -> Option<ElemType> {
        Some(match s {
            "node" | "n" => ElemType::Node,
            "way" | "w" => ElemType::Way,
            "relation" | "rel" | "r" => ElemType::Relation,
            "area" => ElemType::Area,
            _ => return None,
        })
    }
}

impl fmt::Display for ElemType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Tags of an element, sorted by key (keys are unique).
///
/// All keys and values share one string buffer, so an element costs two
/// allocations regardless of its number of tags.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct Tags {
    data: String,
    /// End offsets of each key and value in `data`.
    ends: Vec<(u32, u32)>,
}

impl Tags {
    pub fn new() -> Tags {
        Tags::default()
    }

    pub fn with_capacity(n: usize, bytes: usize) -> Tags {
        Tags { data: String::with_capacity(bytes), ends: Vec::with_capacity(n) }
    }

    /// Append a tag; callers keep keys sorted (see `from_pairs`).
    pub fn push(&mut self, k: &str, v: &str) {
        self.data.push_str(k);
        let ke = self.data.len() as u32;
        self.data.push_str(v);
        self.ends.push((ke, self.data.len() as u32));
    }

    /// Build from unsorted pairs: sorted by key, first value of a key wins.
    pub fn from_pairs<K: AsRef<str>, V: AsRef<str>>(pairs: impl IntoIterator<Item = (K, V)>) -> Tags {
        let mut v: Vec<(K, V)> = pairs.into_iter().collect();
        v.sort_by(|a, b| a.0.as_ref().cmp(b.0.as_ref()));
        v.dedup_by(|a, b| a.0.as_ref() == b.0.as_ref());
        let bytes = v.iter().map(|(k, x)| k.as_ref().len() + x.as_ref().len()).sum();
        let mut t = Tags::with_capacity(v.len(), bytes);
        for (k, x) in &v {
            t.push(k.as_ref(), x.as_ref());
        }
        t
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Remove all tags, keeping the memory.
    pub fn clear(&mut self) {
        self.data.clear();
        self.ends.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    #[inline]
    pub fn at(&self, i: usize) -> (&str, &str) {
        let start = if i == 0 { 0 } else { self.ends[i - 1].1 as usize };
        let (ke, ve) = self.ends[i];
        (&self.data[start..ke as usize], &self.data[ke as usize..ve as usize])
    }

    pub fn iter(&self) -> TagIter<'_> {
        TagIter { t: self, i: 0 }
    }

    /// Value of `key` (binary search over the sorted keys).
    pub fn get(&self, key: &str) -> Option<&str> {
        let (mut lo, mut hi) = (0, self.ends.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            let (k, v) = self.at(mid);
            match k.cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(v),
            }
        }
        None
    }

    pub fn byte_len(&self) -> usize {
        self.data.len()
    }
}

impl fmt::Debug for Tags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

pub struct TagIter<'a> {
    t: &'a Tags,
    i: usize,
}

impl<'a> Iterator for TagIter<'a> {
    type Item = (&'a str, &'a str);
    fn next(&mut self) -> Option<Self::Item> {
        if self.i >= self.t.len() {
            return None;
        }
        self.i += 1;
        Some(self.t.at(self.i - 1))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.t.len() - self.i;
        (n, Some(n))
    }
}

impl ExactSizeIterator for TagIter<'_> {}

impl<'a> IntoIterator for &'a Tags {
    type Item = (&'a str, &'a str);
    type IntoIter = TagIter<'a>;
    fn into_iter(self) -> TagIter<'a> {
        self.iter()
    }
}

pub fn tag<'a>(tags: &'a Tags, key: &str) -> Option<&'a str> {
    tags.get(key)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub version: u32,
    /// Seconds since the Unix epoch.
    pub timestamp: i64,
    pub changeset: u64,
    pub uid: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: Id,
    pub pos: Pt,
    pub tags: Tags,
}

impl Node {
    pub fn cell(&self) -> Cell {
        Cell::tile(crate::geo::tile_of(self.pos))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Way {
    pub id: Id,
    pub nodes: Vec<Id>,
    pub tags: Tags,
    /// Bounding rectangle of the member nodes found in the database.
    pub bbox: Rect,
    /// C++-compatible storage index; only used for iteration order.
    pub idx: u32,
    /// Some referenced node does not exist: spatial filters never match.
    pub missing: bool,
}

impl Way {
    pub fn cell(&self) -> Cell {
        crate::geo::anchor_cell(&self.bbox)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Member {
    pub mtype: ElemType,
    pub id: Id,
    /// Shared with other members of the same block that have this role.
    pub role: std::sync::Arc<str>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Relation {
    pub id: Id,
    pub members: Vec<Member>,
    pub tags: Tags,
    /// Bounding rectangle of node and way members found in the database.
    pub bbox: Rect,
    /// C++-compatible storage index; only used for iteration order.
    pub idx: u32,
    /// Has an existing node member or a complete way member, i.e. can
    /// match spatial filters.
    pub spatial: bool,
}

impl Relation {
    pub fn cell(&self) -> Cell {
        crate::geo::anchor_cell(&self.bbox)
    }
}

/// A DB area built by `make-areas`. Its geometry lives in per-tile blocks
/// stored separately (see `exec::areageom`).
#[derive(Clone, Debug, PartialEq)]
pub struct Area {
    pub id: Id,
    pub tags: Tags,
    /// Block tiles (2^20 units square) that hold blocks of this area.
    pub used: Vec<u32>,
    /// C++-compatible storage index (`calc_index(used)`).
    pub idx: u32,
    pub bbox: Rect,
}

impl Area {
    pub fn cell(&self) -> Cell {
        crate::geo::anchor_cell(&self.bbox)
    }
}

pub const AREA_WAY_OFFSET: Id = 2_400_000_000;
pub const AREA_REL_OFFSET: Id = 3_600_000_000;

/// Unix timestamp to `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_timestamp(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, secs / 3600, (secs / 60) % 60, secs % 60)
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` (the time part and `Z` optional).
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let b = s.trim().as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = b.get(r)?;
        if part.iter().all(|c| c.is_ascii_digit()) {
            std::str::from_utf8(part).ok()?.parse().ok()
        } else {
            None
        }
    };
    let y = num(0..4)?;
    if b.get(4) != Some(&b'-') {
        return None;
    }
    let mo = num(5..7)?;
    if b.get(7) != Some(&b'-') {
        return None;
    }
    let d = num(8..10)?;
    let (mut h, mut mi, mut se) = (0, 0, 0);
    if b.len() > 10 {
        if b[10] != b'T' && b[10] != b' ' {
            return None;
        }
        h = num(11..13)?;
        mi = num(14..16)?;
        se = num(17..19)?;
    }
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 24 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3600 + mi * 60 + se)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_roundtrip() {
        for s in ["1970-01-01T00:00:00Z", "2024-03-20T22:57:12Z", "2000-02-29T12:00:00Z"] {
            assert_eq!(format_timestamp(parse_timestamp(s).unwrap()), s);
        }
        assert_eq!(parse_timestamp("2012-09-14"), parse_timestamp("2012-09-14T00:00:00Z"));
    }
}
