//! Streaming reader for OSM XML (`.osm`) and OsmChange (`.osc`) files.

use super::{Change, Elem};
use crate::geo::{Pt, Rect};
use crate::model::*;
use anyhow::{Context, Result, bail};
use quick_xml::events::{BytesStart, Event};
use std::io::BufRead;

#[derive(Default)]
struct Pending {
    kind: u8, // 0 none, 1 node, 2 way, 3 relation
    id: Id,
    lat: f64,
    lon: f64,
    tags: Vec<(String, String)>,
    nodes: Vec<Id>,
    members: Vec<Member>,
    meta: Option<Meta>,
    user: Option<String>,
    /// Why the element cannot be read (a number that does not parse, an
    /// unknown member type).
    bad: Option<String>,
    /// Why the position of a node cannot be read (only matters when the
    /// node is not deleted).
    bad_pos: Option<String>,
}

fn attr_str(e: &BytesStart, name: &[u8]) -> Result<Option<String>> {
    for a in e.attributes() {
        let a = a?;
        if a.key.as_ref() == name {
            return Ok(Some(a.unescape_value()?.into_owned()));
        }
    }
    Ok(None)
}

fn parse_head(e: &BytesStart, p: &mut Pending) -> Result<()> {
    let mut meta = Meta::default();
    let mut has_meta = false;
    for a in e.attributes() {
        let a = a?;
        let v = a.unescape_value()?;
        match a.key.as_ref() {
            b"id" => match v.parse() {
                Ok(id) => p.id = id,
                Err(_) => p.bad = Some(format!("bad id {v}")),
            },
            b"lat" => match v.parse() {
                Ok(x) => p.lat = x,
                Err(_) => p.bad_pos = Some(format!("bad lat {v}")),
            },
            b"lon" => match v.parse() {
                Ok(x) => p.lon = x,
                Err(_) => p.bad_pos = Some(format!("bad lon {v}")),
            },
            b"version" => {
                meta.version = v.parse().unwrap_or(0);
                has_meta = true;
            }
            b"timestamp" => {
                meta.timestamp = parse_timestamp(&v).unwrap_or(0);
                has_meta = true;
            }
            b"changeset" => {
                meta.changeset = v.parse().unwrap_or(0);
                has_meta = true;
            }
            b"uid" => {
                meta.uid = v.parse().unwrap_or(0);
                has_meta = true;
            }
            b"user" => p.user = Some(v.into_owned()),
            _ => {}
        }
    }
    p.meta = has_meta.then_some(meta);
    if p.kind == 1 && (!e.attributes().flatten().any(|a| a.key.as_ref() == b"lat") || !e.attributes().flatten().any(|a| a.key.as_ref() == b"lon")) {
        p.bad_pos.get_or_insert_with(|| "no lat or lon".into());
    }
    Ok(())
}

/// Read an OSM or OsmChange file and hand changes to `sink` in order. An
/// element that cannot be read fails the whole file.
pub fn read<R: BufRead>(input: R, sink: impl FnMut(Change) -> Result<()>) -> Result<()> {
    read_with(input, false, sink, None)
}

/// `read` for replication diffs. `osc`: the input must be one complete
/// `<osmChange>` document, so that a cut or foreign file (an error page)
/// fails instead of being taken as a diff with fewer changes. With `skip`,
/// an element that cannot be read goes there with the reason, and the rest
/// of the file is applied: one bad element must not stop the updates.
pub fn read_with<R: BufRead>(input: R, osc: bool, mut sink: impl FnMut(Change) -> Result<()>, mut skip: Option<&mut dyn FnMut(String)>) -> Result<()> {
    let mut emit = |p: &mut Pending, delete: bool, sink: &mut dyn FnMut(Change) -> Result<()>| -> Result<()> {
        let c = finish(p, delete);
        match (c, skip.as_mut()) {
            (Ok(c), _) => sink(c),
            (Err(e), Some(s)) => {
                s(format!("{e:#}"));
                Ok(())
            }
            (Err(e), None) => Err(e),
        }
    };
    let mut root: Option<Vec<u8>> = None;
    let mut closed = false;
    let mut reader = quick_xml::Reader::from_reader(input);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut p = Pending::default();
    let mut delete = false;
    loop {
        let ev = reader.read_event_into(&mut buf).with_context(|| format!("XML error at byte {}", reader.buffer_position()))?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                if root.is_none() {
                    root = Some(e.name().as_ref().to_vec());
                    closed = empty;
                }
                match e.name().as_ref() {
                    b"delete" => delete = true,
                    b"create" | b"modify" => delete = false,
                    b"node" | b"way" | b"relation" => {
                        p = Pending::default();
                        p.kind = match e.name().as_ref() {
                            b"node" => 1,
                            b"way" => 2,
                            _ => 3,
                        };
                        parse_head(e, &mut p)?;
                        if empty {
                            emit(&mut p, delete, &mut sink)?;
                        }
                    }
                    b"tag" if p.kind != 0 => {
                        let k = attr_str(e, b"k")?.unwrap_or_default();
                        let v = attr_str(e, b"v")?.unwrap_or_default();
                        p.tags.push((k, v));
                    }
                    b"nd" if p.kind == 2 => {
                        if let Some(r) = attr_str(e, b"ref")? {
                            match r.parse() {
                                Ok(n) => p.nodes.push(n),
                                Err(_) => p.bad = Some(format!("bad nd ref {r}")),
                            }
                        }
                    }
                    b"member" if p.kind == 3 => {
                        let t = attr_str(e, b"type")?.unwrap_or_default();
                        let r = attr_str(e, b"ref")?.unwrap_or_default();
                        let role = attr_str(e, b"role")?.unwrap_or_default();
                        match (ElemType::from_name(&t), r.parse()) {
                            (Some(mtype), Ok(id)) if mtype != ElemType::Area => p.members.push(Member { mtype, id, role: role.into() }),
                            _ => p.bad = Some(format!("bad member {t} {r}")),
                        }
                    }
                    _ => {}
                }
            }
            Event::End(ref e) => {
                if root.as_deref() == Some(e.name().as_ref()) {
                    closed = true;
                }
                match e.name().as_ref() {
                    b"node" | b"way" | b"relation" if p.kind != 0 => emit(&mut p, delete, &mut sink)?,
                    b"delete" => delete = false,
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if osc {
        match root.as_deref() {
            Some(b"osmChange") if closed => {}
            Some(b"osmChange") => bail!("the diff ends before </osmChange>"),
            Some(r) => bail!("not an OsmChange file (root element <{}>)", String::from_utf8_lossy(r)),
            None => bail!("empty diff"),
        }
    }
    Ok(())
}

fn finish(p: &mut Pending, delete: bool) -> Result<Change> {
    let tags = Tags::from_pairs(std::mem::take(&mut p.tags));
    let kind = p.kind;
    p.kind = 0;
    let what = ["", "node", "way", "relation"][kind as usize];
    let bad_pos = p.bad_pos.take();
    if let Some(b) = p.bad.take() {
        bail!("{what} {}: {b}", p.id);
    }
    if kind == 1 && !delete {
        if let Some(b) = bad_pos {
            bail!("{what} {}: {b}", p.id);
        }
        crate::update::check_coordinates(p.id, p.lat, p.lon)?;
    }
    let elem = match kind {
        1 => Elem::Node(Node { id: p.id, pos: Pt::from_ll(p.lat, p.lon), tags }),
        2 => Elem::Way(Way { id: p.id, nodes: std::mem::take(&mut p.nodes), tags, bbox: Rect::EMPTY, idx: 0, missing: false }),
        _ => Elem::Relation(Relation { id: p.id, members: std::mem::take(&mut p.members), tags, bbox: Rect::EMPTY, idx: 0, spatial: false }),
    };
    Ok(Change { elem, delete, meta: p.meta.take(), user: p.user.take() })
}
