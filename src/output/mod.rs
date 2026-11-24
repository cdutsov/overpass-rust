//! XML, JSON and CSV serialization, byte-compatible with Overpass API 0.7.62.

use crate::model::*;
use crate::ql::ast::{CsvSpec, OutFormat};
use std::io::{self, Write};

pub const GENERATOR: &str = concat!("Overpass API via overpass-rust v.", env!("CARGO_PKG_VERSION"));
pub const COPYRIGHT: &str =
    "The data included in this document is from www.openstreetmap.org. The data is made available under ODbL.";

/// Print flags (same bits as the C++ code).
pub mod flags {
    pub const ID: u32 = 0x1;
    pub const COORDS: u32 = 0x2;
    pub const NDS: u32 = 0x4;
    pub const MEMBERS: u32 = 0x8;
    pub const TAGS: u32 = 0x10;
    pub const VERSION: u32 = 0x20;
    pub const META: u32 = 0x40;
    pub const GEOMETRY: u32 = 0x80;
    pub const BOUNDS: u32 = 0x100;
    pub const CENTER: u32 = 0x200;
    pub const COUNT: u32 = 0x400;
    pub const ANY_POS: u32 = COORDS | GEOMETRY | BOUNDS | CENTER;
}
use flags::*;

pub type LatLon = (f64, f64);

/// Bounds as (south, west, north, east).
pub type Bounds = (f64, f64, f64, f64);

/// Geometry of a relation member as needed for `out geom`.
#[derive(Clone, Debug)]
pub enum MemberGeom {
    Node(Option<LatLon>),
    Way(Vec<Option<LatLon>>),
    Relation,
}

/// Geometry of a derived element (make/convert), mirroring the C++
/// `Opaque_Geometry` variants.
#[derive(Clone, Debug, PartialEq)]
pub enum DGeom {
    /// A geometry object without coordinates.
    Null,
    Point(LatLon),
    LineString(Vec<LatLon>),
    /// Way geometry with missing positions.
    PartialWay(Vec<Option<LatLon>>),
    /// Polygon rings (closed).
    Polygon(Vec<Vec<LatLon>>),
    /// Compound geometry (relation members, `gcat`, `trace`).
    Collection(Vec<DGeom>),
    /// Relation geometry with missing members or positions.
    PartialRel(Vec<DGeom>),
}

/// Everything the printers need about one element's geometry.
#[derive(Clone, Debug, Default)]
pub struct GeomOut {
    pub bounds: Option<Bounds>,
    pub center: Option<LatLon>,
    /// Way vertex positions for `geom` (None entries are invalid).
    pub way_pts: Option<Vec<Option<LatLon>>>,
    /// Relation member geometries for `geom`.
    pub members: Option<Vec<MemberGeom>>,
    /// Derived element geometry.
    pub derived: Option<DGeom>,
}

pub struct MetaOut<'a> {
    pub meta: Meta,
    pub user: Option<&'a str>,
}

pub enum Item<'a> {
    Node(&'a Node),
    Way(&'a Way),
    Relation(&'a Relation),
    /// Areas and derived elements: type name, id, tags.
    Derived { tname: &'a str, id: Id, tags: TagList<'a> },
}

/// Tags of any element kind: sorted OSM tags or ordered derived tags.
#[derive(Clone, Copy)]
pub enum TagList<'a> {
    Sorted(&'a Tags),
    Pairs(&'a [(String, String)]),
}

impl<'a> TagList<'a> {
    pub fn len(&self) -> usize {
        match self {
            TagList::Sorted(t) => t.len(),
            TagList::Pairs(p) => p.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn iter(&self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        let (a, b) = match *self {
            TagList::Sorted(t) => (Some(t.iter()), None),
            TagList::Pairs(p) => (None, Some(p.iter().map(|(k, v)| (k.as_str(), v.as_str())))),
        };
        a.into_iter().flatten().chain(b.into_iter().flatten())
    }
    pub fn get(&self, key: &str) -> Option<&'a str> {
        match *self {
            TagList::Sorted(t) => t.get(key),
            TagList::Pairs(p) => p.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()),
        }
    }
}

macro_rules! w {
    ($b:expr, $($a:tt)*) => {{
        use std::io::Write as _;
        let _ = $b.write_fmt(format_args!($($a)*));
    }};
}

pub fn fmt7(v: f64) -> String {
    F7(v).to_string()
}

/// Fixed 7-decimal formatting of a value in units of 1e-7.
fn write_e7(f: &mut std::fmt::Formatter<'_>, v: i64) -> std::fmt::Result {
    let neg = v < 0;
    let a = v.unsigned_abs();
    write!(f, "{}{}.{:07}", if neg { "-" } else { "" }, a / 10_000_000, a % 10_000_000)
}

/// `%.7f`, with an integer fast path for values on the 1e-7 grid.
pub struct F7(pub f64);

impl std::fmt::Display for F7 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scaled = self.0 * 10_000_000.0;
        let r = scaled.round();
        if self.0.is_finite() && (scaled - r).abs() < 1e-3 && r.abs() < 9e15 && !(r == 0.0 && self.0.is_sign_negative()) {
            return write_e7(f, r as i64);
        }
        write!(f, "{:.7}", self.0)
    }
}

/// Latitude of a stored integer coordinate.
pub struct Lat(pub u32);

impl std::fmt::Display for Lat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_e7(f, self.0 as i64 - 910_000_000)
    }
}

/// Longitude of a stored integer coordinate.
pub struct Lon(pub i32);

impl std::fmt::Display for Lon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_e7(f, self.0 as i64)
    }
}

/// The numbers 00 to 99 as two digits each.
const PAIRS: &[u8; 200] = b"0001020304050607080910111213141516171819202122232425262728293031323334353637383940414243444546474849\
5051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

/// An unsigned integer in decimal, two digits per division (ids and
/// references are most of the numbers of an answer).
#[inline]
fn put_u64(b: &mut Vec<u8>, mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v >= 100 {
        let p = 2 * (v % 100) as usize;
        v /= 100;
        i -= 2;
        buf[i] = PAIRS[p];
        buf[i + 1] = PAIRS[p + 1];
    }
    if v >= 10 {
        let p = 2 * v as usize;
        i -= 2;
        buf[i] = PAIRS[p];
        buf[i + 1] = PAIRS[p + 1];
    } else {
        i -= 1;
        buf[i] = b'0' + v as u8;
    }
    b.extend_from_slice(&buf[i..]);
}

/// `write_e7` into a byte buffer: a value in units of 1e-7 with 7 decimals.
#[inline]
fn put_e7(b: &mut Vec<u8>, v: i64) {
    if v < 0 {
        b.push(b'-');
    }
    let a = v.unsigned_abs();
    put_u64(b, a / 10_000_000);
    let mut f = a % 10_000_000;
    let mut buf = [b'.'; 8];
    for d in buf[1..].iter_mut().rev() {
        *d = b'0' + (f % 10) as u8;
        f /= 10;
    }
    b.extend_from_slice(&buf);
}

/// `F7` into a byte buffer.
#[inline]
fn put_f7(b: &mut Vec<u8>, v: f64) {
    let scaled = v * 10_000_000.0;
    let r = scaled.round();
    if v.is_finite() && (scaled - r).abs() < 1e-3 && r.abs() < 9e15 && !(r == 0.0 && v.is_sign_negative()) {
        put_e7(b, r as i64);
    } else {
        w!(b, "{:.7}", v);
    }
}

/// Coordinates as `F7`, each after its text, and the text that ends them
/// (the bounds and the center of an element).
#[inline]
fn put_parts<const N: usize>(b: &mut Vec<u8>, before: [&[u8]; N], vals: [f64; N], end: &[u8]) {
    for (t, v) in before.iter().zip(vals) {
        b.extend_from_slice(t);
        put_f7(b, v);
    }
    b.extend_from_slice(end);
}

/// `Lat` and `Lon` of a node into a byte buffer.
#[inline]
fn put_lat(b: &mut Vec<u8>, y: u32) {
    put_e7(b, y as i64 - 910_000_000);
}

#[inline]
fn put_lon(b: &mut Vec<u8>, ilon: i32) {
    put_e7(b, ilon as i64);
}

/// Copy `s`, with `esc(byte)` in place of each byte it names; the bytes
/// between go as runs (most strings have nothing to replace).
#[inline]
fn escape_with(s: &str, out: &mut Vec<u8>, esc: impl Fn(u8) -> Option<&'static [u8]>) {
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if let Some(r) = esc(b) {
            out.extend_from_slice(&bytes[start..i]);
            out.extend_from_slice(r);
            start = i + 1;
        }
    }
    out.extend_from_slice(&bytes[start..]);
}

pub fn escape_xml(s: &str, out: &mut Vec<u8>) {
    escape_with(s, out, |b| {
        let r: &'static [u8] = match b {
            b'&' => b"&amp;",
            b'"' => b"&quot;",
            b'<' => b"&lt;",
            b'>' => b"&gt;",
            b'\n' => b"&#x0a;",
            b'\t' => b"&#x09;",
            b'\r' => b"&#x0d;",
            0..=0x1f => b"?",
            _ => return None,
        };
        Some(r)
    })
}

pub fn escape_json(s: &str, out: &mut Vec<u8>) {
    escape_with(s, out, |b| {
        let r: &'static [u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\t' => b"\\t",
            b'\r' => b"\\r",
            0..=0x1f => b"?",
            _ => return None,
        };
        Some(r)
    })
}

pub fn escape_csv(s: &str) -> String {
    if s.contains(['\n', ',', '"']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Bounds of a set of points with the C++ `calc_bounds` antimeridian rule.
pub fn calc_bounds<'a>(pts: impl Iterator<Item = &'a LatLon> + Clone) -> Option<Bounds> {
    let (mut s, mut w, mut n, mut e) = (100.0f64, 200.0f64, -100.0f64, -200.0f64);
    let mut any = false;
    for &(lat, lon) in pts.clone() {
        any = true;
        s = s.min(lat);
        n = n.max(lat);
        w = w.min(lon);
        e = e.max(lon);
    }
    if !any {
        return None;
    }
    if e - w > 180.0 {
        let mut ww = 180.0f64;
        let mut we = -180.0f64;
        for &(_, lon) in pts {
            if lon > 0.0 {
                ww = ww.min(lon);
            } else {
                we = we.max(lon);
            }
        }
        if ww - we > 180.0 {
            return Some((s, ww, n, we));
        }
        return Some((s, -180.0, n, 180.0));
    }
    Some((s, w, n, e))
}

/// Combine component bounds (C++ compound geometry rule).
pub fn combine_bounds(parts: &[Bounds]) -> Option<Bounds> {
    if parts.is_empty() {
        return None;
    }
    let mut s = 100.0f64;
    let mut n = -100.0f64;
    let mut w = 200.0f64;
    let mut e = -200.0f64;
    let mut wrapped = false;
    for b in parts {
        s = s.min(b.0);
        n = n.max(b.2);
        w = w.min(b.1);
        e = e.max(b.3);
        wrapped |= b.3 < b.1;
    }
    if !wrapped && e - w <= 180.0 {
        return Some((s, w, n, e));
    }
    let mut w = 200.0f64;
    let mut e = -200.0f64;
    for b in parts {
        if b.3 <= 0.0 {
            e = e.max(b.3);
            if b.1 >= 0.0 {
                w = w.min(b.1);
            }
        } else if b.1 >= 0.0 {
            w = w.min(b.1);
        } else {
            return Some((s, -180.0, n, 180.0));
        }
    }
    if w - e > 180.0 { Some((s, w, n, e)) } else { Some((s, -180.0, n, 180.0)) }
}

pub fn bounds_center(b: &Bounds) -> LatLon {
    let lat = (b.0 + b.2) / 2.0;
    let mid = (b.1 + b.3) / 2.0;
    let lon = if b.1 <= b.3 {
        mid
    } else if mid <= 0.0 {
        mid + 180.0
    } else {
        mid - 180.0
    };
    (lat, lon)
}

const INVALID_BOUNDS: Bounds = (100.0, 200.0, 100.0, 200.0);

fn bounds_valid(b: &Bounds) -> bool {
    b.0 >= -90.0 && b.0 <= b.2 && b.2 <= 90.0 && (-180.0..=180.0).contains(&b.1) && (-180.0..=180.0).contains(&b.3)
}

impl DGeom {
    /// Maximal runs of valid positions of a partial way.
    pub fn runs(pts: &[Option<LatLon>]) -> Vec<Vec<LatLon>> {
        let mut out = Vec::new();
        let mut cur = Vec::new();
        for p in pts {
            match p {
                Some(q) => cur.push(*q),
                None => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    fn has_coords(&self) -> bool {
        match self {
            DGeom::Null => false,
            DGeom::Point(_) | DGeom::LineString(_) | DGeom::Polygon(_) => true,
            DGeom::PartialWay(p) => p.iter().any(|x| x.is_some()),
            DGeom::Collection(c) | DGeom::PartialRel(c) => c.iter().any(|g| g.has_coords()),
        }
    }

    pub fn bounds(&self) -> Option<Bounds> {
        match self {
            DGeom::Null => None,
            DGeom::Point(p) => Some((p.0, p.1, p.0, p.1)),
            DGeom::LineString(pts) => Some(calc_bounds(pts.iter()).unwrap_or(INVALID_BOUNDS)),
            DGeom::Polygon(rings) => Some(calc_bounds(rings.iter().flatten()).unwrap_or(INVALID_BOUNDS)),
            DGeom::PartialWay(pts) => {
                let v: Vec<LatLon> = pts.iter().flatten().copied().collect();
                calc_bounds(v.iter())
            }
            DGeom::Collection(parts) | DGeom::PartialRel(parts) => {
                if matches!(self, DGeom::PartialRel(_)) && !self.has_coords() {
                    return None;
                }
                let b: Vec<Bounds> = parts.iter().filter_map(|p| p.bounds()).filter(bounds_valid).collect();
                combine_bounds(&b).filter(bounds_valid)
            }
        }
    }

    /// Number of positions held, missing ones included.
    pub fn points(&self) -> usize {
        match self {
            DGeom::Null => 0,
            DGeom::Point(_) => 1,
            DGeom::LineString(v) => v.len(),
            DGeom::PartialWay(v) => v.len(),
            DGeom::Polygon(r) => r.iter().map(Vec::len).sum(),
            DGeom::Collection(c) | DGeom::PartialRel(c) => c.iter().map(DGeom::points).sum(),
        }
    }

    pub fn center(&self) -> Option<LatLon> {
        match self {
            DGeom::Null => None,
            DGeom::Point(p) => Some(*p),
            _ => self.bounds().map(|b| bounds_center(&b)),
        }
    }

    pub fn components(&self) -> Option<&[DGeom]> {
        match self {
            DGeom::Collection(c) | DGeom::PartialRel(c) => Some(c),
            _ => None,
        }
    }

    /// Single line geometry, if the geometry is one.
    pub fn line(&self) -> Option<Vec<LatLon>> {
        match self {
            DGeom::LineString(p) => Some(p.clone()),
            DGeom::PartialWay(p) => {
                let r = DGeom::runs(p);
                if r.len() == 1 { r.into_iter().next() } else { None }
            }
            _ => None,
        }
    }

    pub fn multiline(&self) -> Option<Vec<Vec<LatLon>>> {
        match self {
            DGeom::Polygon(r) => Some(r.clone()),
            DGeom::PartialWay(p) => Some(DGeom::runs(p)),
            _ => None,
        }
    }
}

/// Output sink for one query.
pub struct Printer<W: Write> {
    pub w: W,
    fmt: OutFormat,
    buf: Vec<u8>,
    first_elem: bool,
    remarks: String,
    /// Bytes per element of the last large batch (`items`).
    per_item: usize,
    pub area_level: bool,
    /// Close the JSON document with `);` for JSONP.
    pub jsonp: bool,
}

impl<W: Write> Printer<W> {
    pub fn new(w: W, fmt: OutFormat) -> Printer<W> {
        Printer { w, fmt, buf: Vec::with_capacity(1 << 16), first_elem: true, remarks: String::new(), per_item: 128, area_level: false, jsonp: false }
    }

    pub fn format(&self) -> &OutFormat {
        &self.fmt
    }

    pub fn content_type(&self) -> &'static str {
        match self.fmt {
            OutFormat::Xml => "application/osm3s+xml",
            OutFormat::Json => "application/json",
            OutFormat::Csv(_) => "text/csv",
        }
    }

    fn flush_if_big(&mut self) -> io::Result<()> {
        if self.buf.len() >= 1 << 16 {
            self.w.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.w.write_all(&self.buf)?;
        self.buf.clear();
        self.w.flush()
    }

    pub fn header(&mut self, ts: &str, area_ts: Option<&str>) {
        let b = &mut self.buf;
        match &self.fmt {
            OutFormat::Xml => {
                b.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
                w!(b, "<osm version=\"0.6\" generator=\"{GENERATOR}\">\n");
                w!(b, "<note>{COPYRIGHT}</note>\n");
                match area_ts {
                    Some(a) => w!(b, "<meta osm_base=\"{ts}\" areas=\"{a}\"/>\n\n"),
                    None => w!(b, "<meta osm_base=\"{ts}\"/>\n\n"),
                }
            }
            OutFormat::Json => {
                w!(b, "{{\n  \"version\": 0.6,\n  \"generator\": \"{GENERATOR}\",\n  \"osm3s\": {{\n    \"timestamp_osm_base\": \"{ts}\",\n");
                if let Some(a) = area_ts {
                    w!(b, "    \"timestamp_areas_base\": \"{a}\",\n");
                }
                w!(b, "    \"copyright\": \"{COPYRIGHT}\"\n  }},\n  \"elements\": [\n\n");
            }
            OutFormat::Csv(spec) => {
                if spec.header {
                    let cols: Vec<String> = spec
                        .fields
                        .iter()
                        .map(|f| match f.strip_prefix("::") {
                            Some(x) => format!("@{}", escape_csv(x)),
                            None => escape_csv(f),
                        })
                        .collect();
                    b.extend_from_slice(cols.join(&spec.separator).as_bytes());
                    b.push(b'\n');
                }
            }
        }
    }

    pub fn global_bbox(&mut self, b: &Bounds) {
        if let OutFormat::Xml = self.fmt {
            w!(self.buf, "  <bounds minlat=\"{}\" minlon=\"{}\" maxlat=\"{}\" maxlon=\"{}\"/>\n\n",
                    F7(b.0),
                    F7(b.1),
                    F7(b.2),
                    F7(b.3));
        }
    }

    /// Error or remark text produced after the header was written.
    pub fn remark(&mut self, text: &str) {
        match self.fmt {
            OutFormat::Xml => w!(self.buf, "<remark> {text} </remark>\n"),
            OutFormat::Json => self.remarks.push_str(text),
            OutFormat::Csv(_) => {}
        }
    }

    pub fn footer(&mut self) -> io::Result<()> {
        match self.fmt {
            OutFormat::Xml => self.buf.extend_from_slice(b"\n</osm>\n"),
            OutFormat::Json => {
                self.buf.extend_from_slice(b"\n\n  ]");
                if !self.remarks.is_empty() {
                    self.buf.extend_from_slice(b",\n\"remark\": \"");
                    let r = std::mem::take(&mut self.remarks);
                    escape_json(&r, &mut self.buf);
                    self.buf.push(b'"');
                }
                self.buf.extend_from_slice(if self.jsonp { b"\n});\n" } else { b"\n}\n" });
            }
            OutFormat::Csv(_) => {}
        }
        self.flush()
    }

    pub fn item(&mut self, item: &Item, fl: u32, geom: &GeomOut, meta: Option<&MetaOut>) -> io::Result<()> {
        if let OutFormat::Json = self.fmt {
            if !std::mem::replace(&mut self.first_elem, false) {
                self.buf.extend_from_slice(b",\n");
            }
        }
        Self::format_item(&self.fmt, &mut self.buf, item, fl, geom, meta);
        self.flush_if_big()
    }

    fn format_item(fmt: &OutFormat, b: &mut Vec<u8>, item: &Item, fl: u32, geom: &GeomOut, meta: Option<&MetaOut>) {
        match fmt {
            OutFormat::Xml => Self::xml_item(b, item, fl, geom, meta),
            OutFormat::Json => Self::json_item(b, item, fl, geom, meta),
            OutFormat::Csv(spec) => Self::csv_item(b, spec, item, fl, geom, meta),
        }
    }

    /// Print many elements; large batches are formatted in parallel, in
    /// chunks of CHUNK elements (`exec::par_map`).
    pub fn items(&mut self, list: &[(Item, GeomOut, Option<MetaOut>)], fl: u32) -> io::Result<()> {
        const CHUNK: usize = 512;
        if list.len() < 4 * CHUNK {
            for (it, g, m) in list {
                self.item(it, fl, g, m.as_ref())?;
            }
            return Ok(());
        }
        let json = matches!(self.fmt, OutFormat::Json);
        // each chunk with whether an element comes before it in the document
        let lead = !std::mem::replace(&mut self.first_elem, false);
        let chunks: Vec<(bool, &[(Item, GeomOut, Option<MetaOut>)])> = list.chunks(CHUNK).enumerate().map(|(i, c)| (i > 0 || lead, c)).collect();
        let Printer { w, fmt, buf, per_item, .. } = self;
        let fmt = &*fmt;
        // room for a chunk as the last call needed it: a buffer that grows
        // is copied again and again
        let room = *per_item + *per_item / 8 + 16;
        let mut bytes = 0;
        // A chunk formatted on this thread (a busy host, or the start
        // beside another section) is written at once, as by `item`; those
        // of the pool when all of them are done.
        let r = crate::exec::par_run(&chunks, 4, |(lead, chunk)| -> io::Result<Vec<u8>> {
            let mut b = Vec::with_capacity(chunk.len() * room);
            for (i, (it, g, m)) in chunk.iter().enumerate() {
                if json && (i > 0 || *lead) {
                    b.extend_from_slice(b",\n");
                }
                Self::format_item(fmt, &mut b, it, fl, g, m.as_ref());
            }
            Ok(b)
        }, |p| {
            bytes += p.len();
            if p.len() < 1 << 15 {
                buf.extend_from_slice(&p);
                if buf.len() >= 1 << 16 {
                    w.write_all(buf)?;
                    buf.clear();
                }
                return Ok(());
            }
            // a large part goes to the writer as it is, after what waits
            if !buf.is_empty() {
                w.write_all(buf)?;
                buf.clear();
            }
            w.write_all(&p)
        });
        *per_item = (bytes / list.len()).clamp(64, 1 << 16);
        r
    }

    // ------------------------------------------------------------------ XML

    fn xml_item(b: &mut Vec<u8>, item: &Item, fl: u32, geom: &GeomOut, meta: Option<&MetaOut>) {
        let (name, id): (&str, Id) = match item {
            Item::Node(n) => ("node", n.id),
            Item::Way(w) => ("way", w.id),
            Item::Relation(r) => ("relation", r.id),
            Item::Derived { tname, id, .. } => (tname, *id),
        };
        b.extend_from_slice(b"  <");
        b.extend_from_slice(name.as_bytes());
        if fl & ID != 0 {
            b.extend_from_slice(b" id=\"");
            put_u64(b, id);
            b.push(b'"');
        }
        if let Item::Node(n) = item {
            if fl & ANY_POS != 0 {
                b.extend_from_slice(b" lat=\"");
                put_lat(b, n.pos.y);
                b.extend_from_slice(b"\" lon=\"");
                put_lon(b, n.pos.ilon());
                b.push(b'"');
            }
        }
        if !matches!(item, Item::Derived { .. }) && fl & META != 0 {
            if let Some(m) = meta {
                b.extend_from_slice(b" version=\"");
                put_u64(b, m.meta.version as u64);
                b.extend_from_slice(b"\" timestamp=\"");
                put_timestamp(b, m.meta.timestamp);
                b.extend_from_slice(b"\" changeset=\"");
                put_u64(b, m.meta.changeset);
                b.extend_from_slice(b"\" uid=\"");
                put_u64(b, m.meta.uid as u64);
                b.push(b'"');
                if let Some(u) = m.user {
                    b.extend_from_slice(b" user=\"");
                    escape_xml(u, b);
                    b.push(b'"');
                }
            }
        }
        let mut open = false;
        let mut child = |b: &mut Vec<u8>, open: &mut bool| {
            if !*open {
                b.extend_from_slice(b">\n");
                *open = true;
            }
        };
        match item {
            Item::Node(_) => {}
            Item::Way(w) => {
                xml_bounds_center(b, fl, geom, &mut open, &mut child);
                if fl & NDS != 0 {
                    for (i, nd) in w.nodes.iter().enumerate() {
                        child(b, &mut open);
                        let pos = if fl & GEOMETRY != 0 { geom.way_pts.as_ref().and_then(|p| p.get(i).copied().flatten()) } else { None };
                        b.extend_from_slice(b"    <nd ref=\"");
                        put_u64(b, *nd);
                        if let Some((lat, lon)) = pos {
                            b.extend_from_slice(b"\" lat=\"");
                            put_f7(b, lat);
                            b.extend_from_slice(b"\" lon=\"");
                            put_f7(b, lon);
                        }
                        b.extend_from_slice(b"\"/>\n");
                    }
                }
            }
            Item::Relation(r) => {
                xml_bounds_center(b, fl, geom, &mut open, &mut child);
                if fl & MEMBERS != 0 {
                    for (i, m) in r.members.iter().enumerate() {
                        child(b, &mut open);
                        b.extend_from_slice(b"    <member type=\"");
                        b.extend_from_slice(m.mtype.name().as_bytes());
                        b.extend_from_slice(b"\" ref=\"");
                        put_u64(b, m.id);
                        b.extend_from_slice(b"\" role=\"");
                        escape_xml(&m.role, b);
                        b.push(b'"');
                        let mg = if fl & GEOMETRY != 0 { geom.members.as_ref().and_then(|v| v.get(i)) } else { None };
                        match mg {
                            Some(MemberGeom::Node(Some((lat, lon)))) => {
                                b.extend_from_slice(b" lat=\"");
                                put_f7(b, *lat);
                                b.extend_from_slice(b"\" lon=\"");
                                put_f7(b, *lon);
                                b.extend_from_slice(b"\"/>\n");
                            }
                            Some(MemberGeom::Way(pts)) if pts.len() > 1 && pts.iter().any(|p| p.is_some()) => {
                                b.extend_from_slice(b">\n");
                                for p in pts {
                                    match p {
                                        Some((lat, lon)) => {
                                            b.extend_from_slice(b"      <nd lat=\"");
                                            put_f7(b, *lat);
                                            b.extend_from_slice(b"\" lon=\"");
                                            put_f7(b, *lon);
                                            b.extend_from_slice(b"\"/>\n");
                                        }
                                        None => b.extend_from_slice(b"      <nd/>\n"),
                                    }
                                }
                                b.extend_from_slice(b"    </member>\n");
                            }
                            _ => b.extend_from_slice(b"/>\n"),
                        }
                    }
                }
            }
            Item::Derived { .. } => {
                if let Some(d) = &geom.derived {
                    xml_dgeom(b, d, fl, &mut open, "    ");
                }
            }
        }
        if fl & TAGS != 0 {
            let tags: TagList = match item {
                Item::Node(n) => TagList::Sorted(&n.tags),
                Item::Way(w) => TagList::Sorted(&w.tags),
                Item::Relation(r) => TagList::Sorted(&r.tags),
                Item::Derived { tags, .. } => *tags,
            };
            for (k, v) in tags.iter() {
                child(b, &mut open);
                b.extend_from_slice(b"    <tag k=\"");
                escape_xml(k, b);
                b.extend_from_slice(b"\" v=\"");
                escape_xml(v, b);
                b.extend_from_slice(b"\"/>\n");
            }
        }
        if open {
            b.extend_from_slice(b"  </");
            b.extend_from_slice(name.as_bytes());
            b.extend_from_slice(b">\n");
        } else {
            b.extend_from_slice(b"/>\n");
        }
    }

    // ----------------------------------------------------------------- JSON

    fn json_item(b: &mut Vec<u8>, item: &Item, fl: u32, geom: &GeomOut, meta: Option<&MetaOut>) {
        let (tname, id): (&str, Id) = match item {
            Item::Node(n) => ("node", n.id),
            Item::Way(w) => ("way", w.id),
            Item::Relation(r) => ("relation", r.id),
            Item::Derived { tname, id, .. } => (tname, *id),
        };
        b.extend_from_slice(b"{\n  \"type\": \"");
        b.extend_from_slice(tname.as_bytes());
        b.push(b'"');
        if fl & ID != 0 {
            b.extend_from_slice(b",\n  \"id\": ");
            put_u64(b, id);
        }
        if let Item::Node(n) = item {
            if fl & ANY_POS != 0 {
                b.extend_from_slice(b",\n  \"lat\": ");
                put_lat(b, n.pos.y);
                b.extend_from_slice(b",\n  \"lon\": ");
                put_lon(b, n.pos.ilon());
            }
        }
        if !matches!(item, Item::Derived { .. }) && fl & META != 0 {
            if let Some(m) = meta {
                b.extend_from_slice(b",\n  \"timestamp\": \"");
                put_timestamp(b, m.meta.timestamp);
                b.extend_from_slice(b"\",\n  \"version\": ");
                put_u64(b, m.meta.version as u64);
                b.extend_from_slice(b",\n  \"changeset\": ");
                put_u64(b, m.meta.changeset);
                if let Some(u) = m.user {
                    b.extend_from_slice(b",\n  \"user\": \"");
                    escape_json(u, b);
                    b.push(b'"');
                }
                b.extend_from_slice(b",\n  \"uid\": ");
                put_u64(b, m.meta.uid as u64);
            }
        }
        match item {
            Item::Node(_) => {}
            Item::Way(w) => {
                json_bounds_center(b, fl, geom);
                if fl & NDS != 0 && !w.nodes.is_empty() {
                    b.extend_from_slice(b",\n  \"nodes\": [");
                    for (i, nd) in w.nodes.iter().enumerate() {
                        if i > 0 {
                            b.push(b',');
                        }
                        b.extend_from_slice(b"\n    ");
                        put_u64(b, *nd);
                    }
                    b.extend_from_slice(b"\n  ]");
                }
                if fl & GEOMETRY != 0 {
                    if let Some(pts) = &geom.way_pts {
                        b.extend_from_slice(b",\n  \"geometry\": [");
                        for (i, p) in pts.iter().enumerate() {
                            if i > 0 {
                                b.push(b',');
                            }
                            match p {
                                Some((lat, lon)) => {
                                    b.extend_from_slice(b"\n    { \"lat\": ");
                                    put_f7(b, *lat);
                                    b.extend_from_slice(b", \"lon\": ");
                                    put_f7(b, *lon);
                                    b.extend_from_slice(b" }");
                                }
                                None => b.extend_from_slice(b"\n    null"),
                            }
                        }
                        b.extend_from_slice(b"\n  ]");
                    }
                }
            }
            Item::Relation(r) => {
                json_bounds_center(b, fl, geom);
                if fl & MEMBERS != 0 && !r.members.is_empty() {
                    b.extend_from_slice(b",\n  \"members\": [");
                    for (i, m) in r.members.iter().enumerate() {
                        if i > 0 {
                            b.push(b',');
                        }
                        b.extend_from_slice(b"\n    {\n      \"type\": \"");
                        b.extend_from_slice(m.mtype.name().as_bytes());
                        b.extend_from_slice(b"\",\n      \"ref\": ");
                        put_u64(b, m.id);
                        b.extend_from_slice(b",\n      \"role\": \"");
                        escape_json(&m.role, b);
                        b.push(b'"');
                        let mg = if fl & GEOMETRY != 0 { geom.members.as_ref().and_then(|v| v.get(i)) } else { None };
                        match mg {
                            Some(MemberGeom::Node(Some((lat, lon)))) => {
                                b.extend_from_slice(b",\n      \"lat\": ");
                                put_f7(b, *lat);
                                b.extend_from_slice(b",\n      \"lon\": ");
                                put_f7(b, *lon);
                            }
                            Some(MemberGeom::Way(pts)) => {
                                b.extend_from_slice(b",\n      \"geometry\": [");
                                let pts: &[Option<LatLon>] = if pts.len() > 1 { pts } else { &[] };
                                for (j, p) in pts.iter().enumerate() {
                                    if j > 0 {
                                        b.push(b',');
                                    }
                                    match p {
                                        Some((lat, lon)) => {
                                            b.extend_from_slice(b"\n         { \"lat\": ");
                                            put_f7(b, *lat);
                                            b.extend_from_slice(b", \"lon\": ");
                                            put_f7(b, *lon);
                                            b.extend_from_slice(b" }");
                                        }
                                        None => b.extend_from_slice(b"\n         null"),
                                    }
                                }
                                b.extend_from_slice(b"\n      ]");
                            }
                            _ => {}
                        }
                        b.extend_from_slice(b"\n    }");
                    }
                    b.extend_from_slice(b"\n  ]");
                }
            }
            Item::Derived { .. } => {
                if let Some(d) = &geom.derived {
                    if fl & GEOMETRY != 0 && d.center().is_some() {
                        b.extend_from_slice(b",\n  \"geometry\": ");
                        json_dgeom(b, d, "  ");
                    } else if fl & BOUNDS != 0 && d.bounds().is_some() {
                        let bb = d.bounds().unwrap();
                        let (s, w, n, e) = (F7(bb.0), F7(bb.1), F7(bb.2), F7(bb.3));
                        w!(
                            b,
                            ",\n  \"geometry\": {{\n    \"type\": \"Polygon\",\n    \"coordinates\": [\n      [{w}, {s}],\n      [{e}, {s}],\n      [{e}, {n}],\n      [{w}, {n}],\n      [{w}, {s}]\n    ]\n  }}"
                        );
                    } else if fl & CENTER != 0 {
                        if let Some(c) = d.center() {
                            w!(b, ",\n  \"geometry\": {{\n    \"type\": \"Point\",\n    \"coordinates\": [ {}, {} ]\n  }}", F7(c.1), F7(c.0));
                        }
                    }
                }
            }
        }
        if fl & TAGS != 0 {
            let tags: TagList = match item {
                Item::Node(n) => TagList::Sorted(&n.tags),
                Item::Way(w) => TagList::Sorted(&w.tags),
                Item::Relation(r) => TagList::Sorted(&r.tags),
                Item::Derived { tags, .. } => *tags,
            };
            if !tags.is_empty() {
                b.extend_from_slice(b",\n  \"tags\": {");
                for (i, (k, v)) in tags.iter().enumerate() {
                    if i > 0 {
                        b.push(b',');
                    }
                    b.extend_from_slice(b"\n    \"");
                    escape_json(k, b);
                    b.extend_from_slice(b"\": \"");
                    escape_json(v, b);
                    b.push(b'"');
                }
                b.extend_from_slice(b"\n  }");
            }
        }
        b.extend_from_slice(b"\n}");
    }

    // ------------------------------------------------------------------ CSV

    fn csv_item(b: &mut Vec<u8>, spec: &CsvSpec, item: &Item, fl: u32, geom: &GeomOut, meta: Option<&MetaOut>) {
        let (tname, id, tags): (&str, Id, TagList) = match item {
            Item::Node(n) => ("node", n.id, TagList::Sorted(&n.tags)),
            Item::Way(w) => ("way", w.id, TagList::Sorted(&w.tags)),
            Item::Relation(r) => ("relation", r.id, TagList::Sorted(&r.tags)),
            Item::Derived { tname, id, tags } => (tname, *id, *tags),
        };
        let center: Option<LatLon> = if fl & ANY_POS == 0 {
            None
        } else {
            match item {
                Item::Node(n) => Some((n.pos.lat(), n.pos.lon())),
                Item::Derived { .. } => geom.derived.as_ref().and_then(|d| d.center()),
                _ => geom.center.or_else(|| geom.bounds.map(|b| bounds_center(&b))),
            }
        };
        let is_count = tname == "count" && matches!(item, Item::Derived { .. });
        let lookup = |k: &str| tags.get(k);
        let mut cols = Vec::with_capacity(spec.fields.len());
        for f in &spec.fields {
            let v = match f.strip_prefix("::") {
                None => {
                    if fl & TAGS != 0 {
                        lookup(f).map(escape_csv).unwrap_or_default()
                    } else {
                        String::new()
                    }
                }
                Some(special) => match special {
                    "id" => {
                        if fl & ID != 0 {
                            id.to_string()
                        } else {
                            String::new()
                        }
                    }
                    "otype" => match item {
                        Item::Node(_) => "1".into(),
                        Item::Way(_) => "2".into(),
                        Item::Relation(_) => "3".into(),
                        Item::Derived { .. } => "4".into(),
                    },
                    "type" => tname.to_string(),
                    "lat" => center.map(|c| fmt7(c.0)).unwrap_or_default(),
                    "lon" => center.map(|c| fmt7(c.1)).unwrap_or_default(),
                    "version" | "timestamp" | "changeset" | "uid" | "user" => match (meta, fl & META != 0) {
                        (Some(m), true) => match special {
                            "version" => m.meta.version.to_string(),
                            "timestamp" => format_timestamp(m.meta.timestamp),
                            "changeset" => m.meta.changeset.to_string(),
                            "uid" => m.meta.uid.to_string(),
                            _ => m.user.unwrap_or("").to_string(),
                        },
                        _ => String::new(),
                    },
                    "count" | "count:nodes" | "count:ways" | "count:relations" | "count:areas" if is_count => {
                        let key = match special {
                            "count" => "total",
                            s => &s[6..],
                        };
                        lookup(key).unwrap_or("0").to_string()
                    }
                    _ => String::new(),
                },
            };
            cols.push(v);
        }
        b.extend_from_slice(cols.join(&spec.separator).as_bytes());
        b.push(b'\n');
    }
}

fn xml_bounds_center(b: &mut Vec<u8>, fl: u32, geom: &GeomOut, open: &mut bool, child: &mut impl FnMut(&mut Vec<u8>, &mut bool)) {
    if fl & BOUNDS != 0 {
        if let Some(bb) = geom.bounds {
            child(b, open);
            put_parts(b, [b"    <bounds minlat=\"", b"\" minlon=\"", b"\" maxlat=\"", b"\" maxlon=\""], [bb.0, bb.1, bb.2, bb.3], b"\"/>\n");
            return;
        }
    }
    if fl & CENTER != 0 {
        if let Some(c) = geom.center {
            child(b, open);
            put_parts(b, [b"    <center lat=\"", b"\" lon=\""], [c.0, c.1], b"\"/>\n");
        }
    }
}

fn json_bounds_center(b: &mut Vec<u8>, fl: u32, geom: &GeomOut) {
    if fl & BOUNDS != 0 {
        if let Some(bb) = geom.bounds {
            put_parts(b, [b",\n  \"bounds\": {\n    \"minlat\": ", b",\n    \"minlon\": ", b",\n    \"maxlat\": ", b",\n    \"maxlon\": "], [bb.0, bb.1, bb.2, bb.3], b"\n  }");
            return;
        }
    }
    if fl & CENTER != 0 {
        if let Some(c) = geom.center {
            put_parts(b, [b",\n  \"center\": {\n    \"lat\": ", b",\n    \"lon\": "], [c.0, c.1], b"\n  }");
        }
    }
}

/// C++ `print_geometry` for derived elements (XML).
fn xml_dgeom(b: &mut Vec<u8>, g: &DGeom, fl: u32, open: &mut bool, pad: &str) {
    let opn = |b: &mut Vec<u8>, open: &mut bool| {
        if !*open {
            b.extend_from_slice(b">\n");
            *open = true;
        }
    };
    if fl & GEOMETRY != 0 && g.components().is_some() {
        opn(b, open);
        let inner = format!("{pad}  ");
        for c in g.components().unwrap() {
            w!(b, "{pad}<group>\n");
            xml_dgeom(b, c, fl, open, &inner);
            w!(b, "{pad}</group>\n");
        }
    } else if fl & GEOMETRY != 0 && g.line().is_some() {
        opn(b, open);
        for (lat, lon) in g.line().unwrap() {
            w!(b, "{pad}<vertex lat=\"{}\" lon=\"{}\"/>\n", F7(lat), F7(lon));
        }
    } else if fl & GEOMETRY != 0 && g.multiline().is_some() {
        opn(b, open);
        for ring in g.multiline().unwrap() {
            w!(b, "{pad}<linestring>\n");
            for (lat, lon) in ring {
                w!(b, "{pad}  <vertex lat=\"{}\" lon=\"{}\"/>\n", F7(lat), F7(lon));
            }
            w!(b, "{pad}</linestring>\n");
        }
    } else if fl & GEOMETRY != 0 && g.center().is_some() {
        opn(b, open);
        let (lat, lon) = g.center().unwrap();
        w!(b, "{pad}<point lat=\"{}\" lon=\"{}\"/>\n", F7(lat), F7(lon));
    } else if fl & BOUNDS != 0 && g.bounds().is_some() {
        opn(b, open);
        let bb = g.bounds().unwrap();
        w!(b, "    <bounds minlat=\"{}\" minlon=\"{}\" maxlat=\"{}\" maxlon=\"{}\"/>\n", F7(bb.0), F7(bb.1), F7(bb.2), F7(bb.3));
    } else if fl & CENTER != 0 && g.center().is_some() {
        opn(b, open);
        let (lat, lon) = g.center().unwrap();
        w!(b, "{pad}<center lat=\"{}\" lon=\"{}\"/>\n", F7(lat), F7(lon));
    }
}

/// C++ `print_geometry` for derived elements (JSON).
fn json_dgeom(b: &mut Vec<u8>, g: &DGeom, ind: &str) {
    if let Some(parts) = g.components() {
        w!(b, "{{\n{ind}  \"type\": \"GeometryCollection\",\n{ind}  \"geometries\": [");
        let mut first = true;
        let inner = format!("{ind}    ");
        for p in parts {
            if p.center().is_none() {
                continue;
            }
            if !first {
                b.push(b',');
            }
            first = false;
            w!(b, "\n{ind}    ");
            json_dgeom(b, p, &inner);
        }
        w!(b, "\n{ind}  ]\n{ind}}}");
    } else if let Some(pts) = g.line() {
        w!(b, "{{\n{ind}  \"type\": \"LineString\",\n{ind}  \"coordinates\": [");
        for (i, (lat, lon)) in pts.iter().enumerate() {
            if i > 0 {
                b.push(b',');
            }
            w!(b, "\n{ind}    [{}, {}]", F7(*lon), F7(*lat));
        }
        w!(b, "\n{ind}  ]\n{ind}}}");
    } else if let Some(rings) = g.multiline() {
        w!(b, "{{\n{ind}  \"type\": \"Polygon\",\n{ind}  \"coordinates\": [");
        for (i, ring) in rings.iter().enumerate() {
            if i > 0 {
                b.push(b',');
            }
            w!(b, "\n{ind}    [");
            for (j, (lat, lon)) in ring.iter().enumerate() {
                if j > 0 {
                    b.push(b',');
                }
                w!(b, "\n{ind}      [{}, {}]", F7(*lon), F7(*lat));
            }
            w!(b, "\n{ind}    ]");
        }
        w!(b, "\n{ind}  ]\n{ind}}}");
    } else if let Some((lat, lon)) = g.center() {
        w!(b, "{{\n{ind}  \"type\": \"Point\",\n{ind}  \"coordinates\": [ {}, {} ]\n{ind}}}", F7(lon), F7(lat));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::Pt;

    /// `items` prints what `item` prints element by element, in every
    /// format, for lists on both sides of 4 x 512, as the first elements or
    /// after one; alone on the pool and beside the long section of another
    /// query (own-thread start, turns).
    #[test]
    fn items_match_item_by_item() {
        let nodes: Vec<Node> = (0..9000u64)
            .map(|i| {
                let tags = if i % 3 == 0 { Tags::from_pairs([("name", format!("n\"{i}<&>,;"))]) } else { Tags::default() };
                Node { id: i + 1, pos: Pt::from_ll(47.0 + i as f64 * 1e-5, 8.0 - i as f64 * 1e-5), tags }
            })
            .collect();
        let metas: Vec<MetaOut> = (0..9000u32).map(|i| MetaOut { meta: Meta { version: i, timestamp: 1_600_000_000 + i as i64, changeset: i as u64 * 7, uid: i }, user: Some("u<\"x\">") }).collect();
        let csv = OutFormat::Csv(CsvSpec { fields: vec!["::id".into(), "name".into(), "::lat".into(), "::lon".into(), "::version".into()], header: true, separator: ";".into() });
        let run = || {
            for fmt in [OutFormat::Json, OutFormat::Xml, csv.clone()] {
                for len in [2047, 2048, 2049, 5000, 8192, 9000] {
                    for lead in [false, true] {
                        let fl = ID | COORDS | TAGS | META;
                        let list: Vec<(Item, GeomOut, Option<MetaOut>)> = nodes[..len].iter().zip(&metas).map(|(n, m)| (Item::Node(n), GeomOut::default(), Some(MetaOut { meta: m.meta, user: m.user }))).collect();
                        let mut a = Printer::new(Vec::new(), fmt.clone());
                        let mut b = Printer::new(Vec::new(), fmt.clone());
                        for p in [&mut a, &mut b] {
                            p.header("t", None);
                            if lead {
                                p.item(&Item::Node(&nodes[8999]), fl, &GeomOut::default(), None).unwrap();
                            }
                        }
                        a.items(&list, fl).unwrap();
                        for (it, g, m) in &list {
                            b.item(it, fl, g, m.as_ref()).unwrap();
                        }
                        a.footer().unwrap();
                        b.footer().unwrap();
                        assert!(a.w == b.w, "{fmt:?} {len} {lead}");
                    }
                }
            }
        };
        run();
        // beside a long section of another thread
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    let items: Vec<u64> = (0..4000).collect();
                    crate::exec::par_map(&items, 2, |&x| {
                        std::thread::sleep(std::time::Duration::from_micros(200));
                        x
                    });
                }
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            for _ in 0..3 {
                run();
            }
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }

    #[test]
    fn xml_node_matches_cpp() {
        let mut p = Printer::new(Vec::new(), OutFormat::Xml);
        p.header("mock-up-init", None);
        let n = Node { id: 5, pos: Pt::from_ll(51.0125, 7.1125), tags: Tags::from_pairs([("node_key", "node_few")]) };
        p.item(&Item::Node(&n), ID | COORDS | TAGS, &GeomOut::default(), None).unwrap();
        p.item(&Item::Node(&n), ID, &GeomOut::default(), None).unwrap();
        p.footer().unwrap();
        let s = String::from_utf8(p.w).unwrap();
        assert_eq!(
            s,
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osm version=\"0.6\" generator=\"{GENERATOR}\">\n<note>The data included in this document is from www.openstreetmap.org. The data is made available under ODbL.</note>\n<meta osm_base=\"mock-up-init\"/>\n\n  <node id=\"5\" lat=\"51.0125000\" lon=\"7.1125000\">\n    <tag k=\"node_key\" v=\"node_few\"/>\n  </node>\n  <node id=\"5\"/>\n\n</osm>\n")
        );
    }

    #[test]
    fn f7_matches_printf() {
        assert_eq!(F7(-0.0).to_string(), "-0.0000000");
        assert_eq!(F7(-0.00000001).to_string(), "-0.0000000");
        assert_eq!(F7(0.0).to_string(), "0.0000000");
        assert_eq!(F7(-7.1125).to_string(), "-7.1125000");
        assert_eq!(F7(51.0125).to_string(), "51.0125000");
    }

    /// Bounds, center and the meta attributes come out as the format
    /// strings wrote them.
    #[test]
    fn bounds_center_and_meta_as_formatted() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..2000 {
            let v: Vec<f64> = (0..4).map(|_| (next() % 3_600_000_000) as f64 / 1e7 - 180.0).collect();
            let bb = (v[0], v[1], v[2], v[3]);
            let c = (v[0], v[1]);
            let mut open = true;
            let mut child = |_: &mut Vec<u8>, _: &mut bool| {};
            let g = GeomOut { bounds: Some(bb), center: Some(c), ..Default::default() };
            let (mut xb, mut xc, mut jb, mut jc) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            xml_bounds_center(&mut xb, BOUNDS, &g, &mut open, &mut child);
            xml_bounds_center(&mut xc, CENTER, &g, &mut open, &mut child);
            json_bounds_center(&mut jb, BOUNDS, &g);
            json_bounds_center(&mut jc, CENTER, &g);
            assert_eq!(String::from_utf8(xb).unwrap(), format!("    <bounds minlat=\"{}\" minlon=\"{}\" maxlat=\"{}\" maxlon=\"{}\"/>\n", F7(bb.0), F7(bb.1), F7(bb.2), F7(bb.3)));
            assert_eq!(String::from_utf8(xc).unwrap(), format!("    <center lat=\"{}\" lon=\"{}\"/>\n", F7(c.0), F7(c.1)));
            assert_eq!(String::from_utf8(jb).unwrap(), format!(",\n  \"bounds\": {{\n    \"minlat\": {},\n    \"minlon\": {},\n    \"maxlat\": {},\n    \"maxlon\": {}\n  }}", F7(bb.0), F7(bb.1), F7(bb.2), F7(bb.3)));
            assert_eq!(String::from_utf8(jc).unwrap(), format!(",\n  \"center\": {{\n    \"lat\": {},\n    \"lon\": {}\n  }}", F7(c.0), F7(c.1)));
            let m = Meta { version: next() as u32, timestamp: (next() % 2_000_000_000) as i64, changeset: next() >> (next() % 64), uid: next() as u32 };
            let n = Node { id: next() >> 20, pos: Pt::from_ll(1.0, 2.0), tags: Tags::from_pairs([("k", "v")]) };
            let mo = MetaOut { meta: m, user: Some("u") };
            let mut b = Vec::new();
            Printer::<Vec<u8>>::xml_item(&mut b, &Item::Node(&n), ID | META | TAGS, &GeomOut::default(), Some(&mo));
            let want = format!(
                "  <node id=\"{}\" version=\"{}\" timestamp=\"{}\" changeset=\"{}\" uid=\"{}\" user=\"u\">\n    <tag k=\"k\" v=\"v\"/>\n  </node>\n",
                n.id,
                m.version,
                format_timestamp(m.timestamp),
                m.changeset,
                m.uid
            );
            assert_eq!(String::from_utf8(b).unwrap(), want);
            let mut b = Vec::new();
            Printer::<Vec<u8>>::json_item(&mut b, &Item::Node(&n), ID | META, &GeomOut::default(), Some(&mo));
            let want = format!(
                "{{\n  \"type\": \"node\",\n  \"id\": {},\n  \"timestamp\": \"{}\",\n  \"version\": {},\n  \"changeset\": {},\n  \"user\": \"u\",\n  \"uid\": {}\n}}",
                n.id,
                format_timestamp(m.timestamp),
                m.version,
                m.changeset,
                m.uid
            );
            assert_eq!(String::from_utf8(b).unwrap(), want);
        }
    }

    #[test]
    fn byte_writers_match_the_formatters() {
        let mut vals: Vec<f64> = vec![0.0, -0.0, 1e-8, -1e-8, 5e-8, -5e-8, 51.0125, -7.1125, 90.0, -90.0, 180.0, -180.0, 0.00000005, 123456789.1234567, 9.1e8, -9.1e8, 1e16, f64::MAX, f64::MIN_POSITIVE];
        let mut x = 0x2545F4914F6CDD1Du64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let y = (x % 1_820_000_000) as i64 - 910_000_000;
            vals.push(y as f64 / 1e7);
            vals.push(y as f64 * 1e-7);
            vals.push((x % 3_600_000_000) as f64 / 1e7 - 180.0);
            vals.push(f64::from_bits(x) % 400.0);
            let mut b = Vec::new();
            put_lat(&mut b, (x % 1_800_000_001) as u32);
            assert_eq!(String::from_utf8(b).unwrap(), Lat((x % 1_800_000_001) as u32).to_string());
            let mut b = Vec::new();
            put_lon(&mut b, y as i32 * 2 - 1);
            assert_eq!(String::from_utf8(b).unwrap(), Lon(y as i32 * 2 - 1).to_string());
            let mut b = Vec::new();
            put_u64(&mut b, x >> (x % 64));
            assert_eq!(String::from_utf8(b).unwrap(), (x >> (x % 64)).to_string());
        }
        for v in vals {
            if v.is_nan() {
                continue;
            }
            let mut b = Vec::new();
            put_f7(&mut b, v);
            assert_eq!(String::from_utf8(b).unwrap(), F7(v).to_string(), "{v:e}");
        }
        let mut b = Vec::new();
        put_u64(&mut b, 0);
        put_u64(&mut b, u64::MAX);
        assert_eq!(b, b"018446744073709551615");
        for t in ["", "plain", "a\"b\\c\nd\te\rf\u{1}g", "\"", "x\"", "<&>\"'", "\u{7f}é\u{1f}"] {
            let (mut j, mut x) = (Vec::new(), Vec::new());
            escape_json(t, &mut j);
            escape_xml(t, &mut x);
            let (mut ej, mut ex) = (String::new(), String::new());
            for c in t.chars() {
                match c {
                    '"' => {
                        ej += "\\\"";
                        ex += "&quot;";
                    }
                    '\\' => {
                        ej += "\\\\";
                        ex.push(c);
                    }
                    '\n' => {
                        ej += "\\n";
                        ex += "&#x0a;";
                    }
                    '\t' => {
                        ej += "\\t";
                        ex += "&#x09;";
                    }
                    '\r' => {
                        ej += "\\r";
                        ex += "&#x0d;";
                    }
                    '&' => {
                        ej.push(c);
                        ex += "&amp;";
                    }
                    '<' => {
                        ej.push(c);
                        ex += "&lt;";
                    }
                    '>' => {
                        ej.push(c);
                        ex += "&gt;";
                    }
                    c if (c as u32) < 0x20 => {
                        ej.push('?');
                        ex.push('?');
                    }
                    c => {
                        ej.push(c);
                        ex.push(c);
                    }
                }
            }
            assert_eq!(String::from_utf8(j).unwrap(), ej);
            assert_eq!(String::from_utf8(x).unwrap(), ex);
        }
    }

    #[test]
    fn json_empty() {
        let mut p = Printer::new(Vec::new(), OutFormat::Json);
        p.header("x", None);
        p.footer().unwrap();
        let s = String::from_utf8(p.w).unwrap();
        assert!(s.ends_with("\"elements\": [\n\n\n\n  ]\n}\n"), "{s}");
    }
}
