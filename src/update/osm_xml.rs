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
            b"id" => p.id = v.parse().with_context(|| format!("bad id {v}"))?,
            b"lat" => p.lat = v.parse().with_context(|| format!("bad lat {v}"))?,
            b"lon" => p.lon = v.parse().with_context(|| format!("bad lon {v}"))?,
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
    Ok(())
}

/// Read an OSM or OsmChange file and hand changes to `sink` in order.
pub fn read<R: BufRead>(input: R, mut sink: impl FnMut(Change) -> Result<()>) -> Result<()> {
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
                            sink(finish(&mut p, delete)?)?;
                        }
                    }
                    b"tag" if p.kind != 0 => {
                        let k = attr_str(e, b"k")?.unwrap_or_default();
                        let v = attr_str(e, b"v")?.unwrap_or_default();
                        p.tags.push((k, v));
                    }
                    b"nd" if p.kind == 2 => {
                        if let Some(r) = attr_str(e, b"ref")? {
                            p.nodes.push(r.parse()?);
                        }
                    }
                    b"member" if p.kind == 3 => {
                        let t = attr_str(e, b"type")?.unwrap_or_default();
                        let mtype = match ElemType::from_name(&t) {
                            Some(m) if m != ElemType::Area => m,
                            _ => bail!("bad member type {t}"),
                        };
                        let id = attr_str(e, b"ref")?.unwrap_or_default().parse()?;
                        let role = attr_str(e, b"role")?.unwrap_or_default();
                        p.members.push(Member { mtype, id, role: role.into() });
                    }
                    _ => {}
                }
            }
            Event::End(ref e) => match e.name().as_ref() {
                b"node" | b"way" | b"relation" if p.kind != 0 => sink(finish(&mut p, delete)?)?,
                b"delete" => delete = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

fn finish(p: &mut Pending, delete: bool) -> Result<Change> {
    let tags = Tags::from_pairs(std::mem::take(&mut p.tags));
    if p.kind == 1 && !delete {
        crate::update::check_coordinates(p.id, p.lat, p.lon)?;
    }
    let elem = match p.kind {
        1 => Elem::Node(Node { id: p.id, pos: Pt::from_ll(p.lat, p.lon), tags }),
        2 => Elem::Way(Way { id: p.id, nodes: std::mem::take(&mut p.nodes), tags, bbox: Rect::EMPTY, idx: 0, missing: false }),
        _ => Elem::Relation(Relation { id: p.id, members: std::mem::take(&mut p.members), tags, bbox: Rect::EMPTY, idx: 0, spatial: false }),
    };
    p.kind = 0;
    Ok(Change { elem, delete, meta: p.meta.take(), user: p.user.take() })
}
