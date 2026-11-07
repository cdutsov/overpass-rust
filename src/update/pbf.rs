//! Reader for `.osm.pbf` files. Blobs are decoded in parallel; elements
//! reach the sink in file order.

use super::{Change, Elem};
use crate::geo::Rect;
use crate::model::*;
use anyhow::Result;
use osmpbf::{Blob, BlobDecode, BlobReader, Element, RelMemberType};
use rayon::prelude::*;
use std::path::Path;

/// Replication state recorded in the file header (Osmosis fields).
#[derive(Clone, Debug, Default)]
pub struct PbfHeader {
    pub sequence: Option<u64>,
    /// Seconds since the epoch.
    pub timestamp: Option<i64>,
    pub url: Option<String>,
}

fn meta_of(version: i32, millis: i64, changeset: i64, uid: i32) -> Option<Meta> {
    if version <= 0 && millis == 0 {
        return None;
    }
    Some(Meta { version: version.max(0) as u32, timestamp: millis / 1000, changeset: changeset.max(0) as u64, uid: uid.max(0) as u32 })
}

fn convert(el: Element) -> Result<Change> {
    Ok(match el {
        Element::Node(n) => {
            let info = n.info();
            let tags = Tags::from_pairs(n.tags());
            Change {
                elem: Elem::Node(Node { id: n.id() as Id, pos: crate::update::checked_pos(n.id() as Id, n.lat(), n.lon())?, tags }),
                delete: false,
                meta: meta_of(info.version().unwrap_or(0), info.milli_timestamp().unwrap_or(0), info.changeset().unwrap_or(0), info.uid().unwrap_or(0)),
                user: info.user().and_then(|u| u.ok()).map(|u| u.to_string()),
            }
        }
        Element::DenseNode(n) => {
            let tags = Tags::from_pairs(n.tags());
            let (meta, user) = match n.info() {
                Some(i) => (meta_of(i.version(), i.milli_timestamp(), i.changeset(), i.uid()), i.user().ok().map(|u| u.to_string())),
                None => (None, None),
            };
            Change { elem: Elem::Node(Node { id: n.id() as Id, pos: crate::update::checked_pos(n.id() as Id, n.lat(), n.lon())?, tags }), delete: false, meta, user }
        }
        Element::Way(w) => {
            let info = w.info();
            let tags = Tags::from_pairs(w.tags());
            let nodes = w.refs().map(|r| r as Id).collect();
            Change {
                elem: Elem::Way(Way { id: w.id() as Id, nodes, tags, bbox: Rect::EMPTY, idx: 0, missing: false }),
                delete: false,
                meta: meta_of(info.version().unwrap_or(0), info.milli_timestamp().unwrap_or(0), info.changeset().unwrap_or(0), info.uid().unwrap_or(0)),
                user: info.user().and_then(|u| u.ok()).map(|u| u.to_string()),
            }
        }
        Element::Relation(r) => {
            let info = r.info();
            let tags = Tags::from_pairs(r.tags());
            let members = r
                .members()
                .map(|m| Member {
                    mtype: match m.member_type {
                        RelMemberType::Node => ElemType::Node,
                        RelMemberType::Way => ElemType::Way,
                        RelMemberType::Relation => ElemType::Relation,
                    },
                    id: m.member_id as Id,
                    role: m.role().unwrap_or("").into(),
                })
                .collect();
            Change {
                elem: Elem::Relation(Relation { id: r.id() as Id, members, tags, bbox: Rect::EMPTY, idx: 0, spatial: false }),
                delete: false,
                meta: meta_of(info.version().unwrap_or(0), info.milli_timestamp().unwrap_or(0), info.changeset().unwrap_or(0), info.uid().unwrap_or(0)),
                user: info.user().and_then(|u| u.ok()).map(|u| u.to_string()),
            }
        }
    })
}

enum Decoded {
    Header(PbfHeader),
    Data(Vec<Change>),
}

fn decode_blob(blob: &Blob) -> Result<Decoded> {
    match blob.decode()? {
        BlobDecode::OsmHeader(h) => Ok(Decoded::Header(PbfHeader {
            sequence: h.osmosis_replication_sequence_number().filter(|s| *s > 0).map(|s| s as u64),
            timestamp: h.osmosis_replication_timestamp().filter(|t| *t > 0),
            url: h.osmosis_replication_base_url().filter(|u| !u.is_empty()).map(String::from),
        })),
        BlobDecode::OsmData(block) => {
            let mut out = Vec::new();
            let mut err = None;
            block.for_each_element(|el| {
                if err.is_none() {
                    match convert(el) {
                        Ok(c) => out.push(c),
                        Err(e) => err = Some(e),
                    }
                }
            });
            match err {
                Some(e) => Err(e),
                None => Ok(Decoded::Data(out)),
            }
        }
        BlobDecode::Unknown(_) => Ok(Decoded::Data(Vec::new())),
    }
}

/// Stream the elements of a PBF file to `sink` in file order; returns the
/// replication fields of the header.
pub fn read(path: &Path, mut sink: impl FnMut(Change) -> Result<()>) -> Result<PbfHeader> {
    let mut reader = BlobReader::from_path(path)?;
    let mut header = PbfHeader::default();
    let batch = (rayon::current_num_threads() * 4).max(4);
    loop {
        let mut blobs = Vec::with_capacity(batch);
        for b in reader.by_ref().take(batch) {
            blobs.push(b?);
        }
        if blobs.is_empty() {
            return Ok(header);
        }
        let decoded: Vec<Result<Decoded>> = blobs.par_iter().map(decode_blob).collect();
        for d in decoded {
            match d? {
                Decoded::Header(h) => header = h,
                Decoded::Data(changes) => {
                    for c in changes {
                        sink(c)?;
                    }
                }
            }
        }
    }
}
