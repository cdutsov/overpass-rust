//! XML, JSON and CSV serialization, byte-compatible with Overpass API 0.7.62.

use crate::model::*;
use crate::ql::ast::{CsvSpec, OutFormat};
use std::io::{self, Write};

pub const GENERATOR: &str = "Overpass API 0.7.62 compatible";
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

pub fn escape_xml(s: &str, out: &mut Vec<u8>) {
    for &b in s.as_bytes() {
        match b {
            b'&' => out.extend_from_slice(b"&amp;"),
            b'"' => out.extend_from_slice(b"&quot;"),
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            b'\n' => out.extend_from_slice(b"&#x0a;"),
            b'\t' => out.extend_from_slice(b"&#x09;"),
            b'\r' => out.extend_from_slice(b"&#x0d;"),
            0..=0x1f => out.push(b'?'),
            _ => out.push(b),
        }
    }
}

pub fn escape_json(s: &str, out: &mut Vec<u8>) {
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0..=0x1f => out.push(b'?'),
            _ => out.push(b),
        }
    }
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
    pub area_level: bool,
    /// Close the JSON document with `);` for JSONP.
    pub jsonp: bool,
}

impl<W: Write> Printer<W> {
    pub fn new(w: W, fmt: OutFormat) -> Printer<W> {
        Printer { w, fmt, buf: Vec::with_capacity(1 << 16), first_elem: true, remarks: String::new(), area_level: false, jsonp: false }
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

    /// Print many elements; large batches are formatted in parallel.
    pub fn items(&mut self, list: &[(Item, GeomOut, Option<MetaOut>)], fl: u32) -> io::Result<()> {
        const CHUNK: usize = 512;
        if list.len() < 4 * CHUNK || !crate::exec::parallel_ok() {
            for (it, g, m) in list {
                self.item(it, fl, g, m.as_ref())?;
            }
            return Ok(());
        }
        use rayon::prelude::*;
        let json = matches!(self.fmt, OutFormat::Json);
        let fmt = &self.fmt;
        let parts: Vec<Vec<u8>> = list
            .par_chunks(CHUNK)
            .map(|chunk| {
                let mut b = Vec::with_capacity(chunk.len() * 128);
                for (i, (it, g, m)) in chunk.iter().enumerate() {
                    if json && i > 0 {
                        b.extend_from_slice(b",\n");
                    }
                    Self::format_item(fmt, &mut b, it, fl, g, m.as_ref());
                }
                b
            })
            .collect();
        for p in parts {
            if json && !std::mem::replace(&mut self.first_elem, false) {
                self.buf.extend_from_slice(b",\n");
            }
            self.buf.extend_from_slice(&p);
            self.flush_if_big()?;
        }
        Ok(())
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
            w!(b, " id=\"{id}\"");
        }
        if let Item::Node(n) = item {
            if fl & ANY_POS != 0 {
                w!(b, " lat=\"{}\" lon=\"{}\"", Lat(n.pos.y), Lon(n.pos.ilon()));
            }
        }
        if !matches!(item, Item::Derived { .. }) && fl & META != 0 {
            if let Some(m) = meta {
                w!(b, " version=\"{}\" timestamp=\"{}\" changeset=\"{}\" uid=\"{}\"",
                        m.meta.version,
                        format_timestamp(m.meta.timestamp),
                        m.meta.changeset,
                        m.meta.uid);
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
                        match pos {
                            Some((lat, lon)) => w!(b, "    <nd ref=\"{nd}\" lat=\"{}\" lon=\"{}\"/>\n", F7(lat), F7(lon)),
                            None => w!(b, "    <nd ref=\"{nd}\"/>\n"),
                        }
                    }
                }
            }
            Item::Relation(r) => {
                xml_bounds_center(b, fl, geom, &mut open, &mut child);
                if fl & MEMBERS != 0 {
                    for (i, m) in r.members.iter().enumerate() {
                        child(b, &mut open);
                        w!(b, "    <member type=\"{}\" ref=\"{}\" role=\"", m.mtype.name(), m.id);
                        escape_xml(&m.role, b);
                        b.push(b'"');
                        let mg = if fl & GEOMETRY != 0 { geom.members.as_ref().and_then(|v| v.get(i)) } else { None };
                        match mg {
                            Some(MemberGeom::Node(Some((lat, lon)))) => {
                                w!(b, " lat=\"{}\" lon=\"{}\"/>\n", F7(*lat), F7(*lon));
                            }
                            Some(MemberGeom::Way(pts)) if pts.len() > 1 && pts.iter().any(|p| p.is_some()) => {
                                b.extend_from_slice(b">\n");
                                for p in pts {
                                    match p {
                                        Some((lat, lon)) => w!(b, "      <nd lat=\"{}\" lon=\"{}\"/>\n", F7(*lat), F7(*lon)),
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
            w!(b, "  </{name}>\n");
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
        w!(b, "{{\n  \"type\": \"{tname}\"");
        if fl & ID != 0 {
            w!(b, ",\n  \"id\": {id}");
        }
        if let Item::Node(n) = item {
            if fl & ANY_POS != 0 {
                w!(b, ",\n  \"lat\": {},\n  \"lon\": {}", Lat(n.pos.y), Lon(n.pos.ilon()));
            }
        }
        if !matches!(item, Item::Derived { .. }) && fl & META != 0 {
            if let Some(m) = meta {
                w!(b, ",\n  \"timestamp\": \"{}\",\n  \"version\": {},\n  \"changeset\": {}",
                        format_timestamp(m.meta.timestamp),
                        m.meta.version,
                        m.meta.changeset);
                if let Some(u) = m.user {
                    b.extend_from_slice(b",\n  \"user\": \"");
                    escape_json(u, b);
                    b.push(b'"');
                }
                w!(b, ",\n  \"uid\": {}", m.meta.uid);
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
                        w!(b, "\n    {nd}");
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
                                Some((lat, lon)) => w!(b, "\n    {{ \"lat\": {}, \"lon\": {} }}", F7(*lat), F7(*lon)),
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
                        w!(b, "\n    {{\n      \"type\": \"{}\",\n      \"ref\": {},\n      \"role\": \"", m.mtype.name(), m.id);
                        escape_json(&m.role, b);
                        b.push(b'"');
                        let mg = if fl & GEOMETRY != 0 { geom.members.as_ref().and_then(|v| v.get(i)) } else { None };
                        match mg {
                            Some(MemberGeom::Node(Some((lat, lon)))) => {
                                w!(b, ",\n      \"lat\": {},\n      \"lon\": {}", F7(*lat), F7(*lon));
                            }
                            Some(MemberGeom::Way(pts)) => {
                                b.extend_from_slice(b",\n      \"geometry\": [");
                                let pts: &[Option<LatLon>] = if pts.len() > 1 { pts } else { &[] };
                                for (j, p) in pts.iter().enumerate() {
                                    if j > 0 {
                                        b.push(b',');
                                    }
                                    match p {
                                        Some((lat, lon)) => w!(b, "\n         {{ \"lat\": {}, \"lon\": {} }}", F7(*lat), F7(*lon)),
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
            w!(b, "    <bounds minlat=\"{}\" minlon=\"{}\" maxlat=\"{}\" maxlon=\"{}\"/>\n", F7(bb.0), F7(bb.1), F7(bb.2), F7(bb.3));
            return;
        }
    }
    if fl & CENTER != 0 {
        if let Some(c) = geom.center {
            child(b, open);
            w!(b, "    <center lat=\"{}\" lon=\"{}\"/>\n", F7(c.0), F7(c.1));
        }
    }
}

fn json_bounds_center(b: &mut Vec<u8>, fl: u32, geom: &GeomOut) {
    if fl & BOUNDS != 0 {
        if let Some(bb) = geom.bounds {
            w!(b, ",\n  \"bounds\": {{\n    \"minlat\": {},\n    \"minlon\": {},\n    \"maxlat\": {},\n    \"maxlon\": {}\n  }}",
                    F7(bb.0),
                    F7(bb.1),
                    F7(bb.2),
                    F7(bb.3));
            return;
        }
    }
    if fl & CENTER != 0 {
        if let Some(c) = geom.center {
            w!(b, ",\n  \"center\": {{\n    \"lat\": {},\n    \"lon\": {}\n  }}", F7(c.0), F7(c.1));
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
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osm version=\"0.6\" generator=\"Overpass API 0.7.62 compatible\">\n<note>The data included in this document is from www.openstreetmap.org. The data is made available under ODbL.</note>\n<meta osm_base=\"mock-up-init\"/>\n\n  <node id=\"5\" lat=\"51.0125000\" lon=\"7.1125000\">\n    <tag k=\"node_key\" v=\"node_few\"/>\n  </node>\n  <node id=\"5\"/>\n\n</osm>\n"
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

    #[test]
    fn json_empty() {
        let mut p = Printer::new(Vec::new(), OutFormat::Json);
        p.header("x", None);
        p.footer().unwrap();
        let s = String::from_utf8(p.w).unwrap();
        assert!(s.ends_with("\"elements\": [\n\n\n\n  ]\n}\n"), "{s}");
    }
}
