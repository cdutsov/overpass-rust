// Contains code translated from C++ to Rust in 2026 from Overpass API,
// Copyright 2008-2018 Roland Olbricht et al. Modified by Chavdar Dutsov.
// GNU Affero General Public License, version 3 or later. See LICENSE and NOTICE.

//! Area geometry, ported from the C++ implementation (docs/spec/areas.md).
//!
//! Two models exist:
//! - DB areas (built by `make-areas`): polylines per 2^20 x 2^20 "block
//!   tile", tested by casting a ray south (`check_area_block`).
//! - Implicit areas (closed ways): segments per 2^16 node tile, tested with
//!   an L-shaped path from the tile's south-west corner (`Tilewise`).
//!
//! Coordinates are integer pairs `(ilat: u32, ilon: i32)`.

use crate::geo::{morton, unmorton};
use std::collections::BTreeMap;

pub type IC = (u32, i32);

pub const HIT: u8 = 1;
pub const TOGGLE_EAST: u8 = 2;
pub const TOGGLE_WEST: u8 = 4;
pub const INTERSECT: u8 = 8;

/// C++ `ll_upper_`: node tile index of a coordinate.
#[inline]
pub fn node_tile(lat: u32, lon: i32) -> u32 {
    morton(lat >> 16, ((lon as u32) ^ 0x8000_0000) >> 16)
}

#[inline]
pub fn block_tile(lat: u32, lon: i32) -> u32 {
    node_tile(lat, lon) & 0xffff_ff00
}

/// South-west corner of a node tile (or block tile).
#[inline]
pub fn tile_sw(tile: u32) -> IC {
    let (ty, tx) = unmorton(tile);
    (ty << 16, ((tx << 16) ^ 0x8000_0000) as i32)
}

// ---------------------------------------------------------------------------
// DB areas: block construction (core/type_area.h, data/collect_members.cc,
// statements/make_area.cc)

fn proportion(clow: i32, cmid: i32, cup: i32, low: i32, up: i32) -> i32 {
    if cup == clow {
        return low;
    }
    ((up as i64 - low as i64) * (cmid as i64 - clow as i64) / (cup as i64 - clow as i64) + low as i64) as i32
}

/// One aligned piece: (block tile of the first point, from, to).
pub type Piece = (u32, IC, IC);

fn piece(out: &mut Vec<Piece>, a: IC, b: IC) {
    out.push((block_tile(a.0, a.1), a, b));
}

fn horiz(out: &mut Vec<Piece>, from_lat: u32, from_lon: i32, to_lat: u32, to_lon: i32) {
    if (from_lat & 0xfff0_0000) == (to_lat & 0xfff0_0000) {
        piece(out, (from_lat, from_lon), (to_lat, to_lon));
        return;
    }
    let (s_lat, s_lon, n_lat, n_lon) = if from_lat < to_lat { (from_lat, from_lon, to_lat, to_lon) } else { (to_lat, to_lon, from_lat, from_lon) };
    let p = |y: u32| proportion(s_lat as i32, y as i32, n_lat as i32, s_lon, n_lon);
    let mut split = (s_lat & 0xfff0_0000) + 0x10_0000;
    piece(out, (s_lat, s_lon), (split - 1, p(split)));
    while split < (n_lat & 0xfff0_0000) {
        piece(out, (split, p(split)), (split + 0xfffff, p(split + 0x10_0000)));
        split += 0x10_0000;
    }
    piece(out, (split, p(split)), (n_lat, n_lon));
}

fn vert(out: &mut Vec<Piece>, from_lat: u32, from_lon: i32, to_lat: u32, to_lon: i32) {
    if (from_lon as u32 & 0xfff0_0000) == (to_lon as u32 & 0xfff0_0000) {
        horiz(out, from_lat, from_lon, to_lat, to_lon);
        return;
    }
    let (w_lat, w_lon, e_lat, e_lon) = if from_lon < to_lon { (from_lat, from_lon, to_lat, to_lon) } else { (to_lat, to_lon, from_lat, from_lon) };
    let p = |x: i32| proportion(w_lon, x, e_lon, w_lat as i32, e_lat as i32) as u32;
    let mut split = ((w_lon as u32 & 0xfff0_0000) as i32).wrapping_add(0x10_0000);
    horiz(out, w_lat, w_lon, p(split - 1), split - 1);
    while split < (e_lon as u32 & 0xfff0_0000) as i32 {
        horiz(out, p(split), split, p(split + 0xfffff), split + 0xfffff);
        split += 0x10_0000;
    }
    horiz(out, p(split), split, e_lat, e_lon);
}

/// Split a segment at block tile boundaries. Returns whether it is taken
/// across the antimeridian.
pub fn aligned_segments(out: &mut Vec<Piece>, from: IC, to: IC) -> bool {
    let (from_lat, from_lon) = from;
    let (to_lat, to_lon) = to;
    if from_lon < -900_000_000 && to_lon > 900_000_000 {
        let m1 = proportion(to_lon, 1_800_000_000, (from_lon as i64 + 3_600_000_000) as i32, to_lat as i32, from_lat as i32) as u32;
        vert(out, to_lat, to_lon, m1, 1_800_000_000);
        let m2 = proportion((to_lon as i64 - 3_600_000_000) as i32, -1_800_000_000, from_lon, to_lat as i32, from_lat as i32) as u32;
        vert(out, m2, -1_800_000_000, from_lat, from_lon);
        return true;
    }
    if to_lon < -900_000_000 && from_lon > 900_000_000 {
        let m1 = proportion(from_lon, 1_800_000_000, (to_lon as i64 + 3_600_000_000) as i32, from_lat as i32, to_lat as i32) as u32;
        vert(out, from_lat, from_lon, m1, 1_800_000_000);
        let m2 = proportion((from_lon as i64 - 3_600_000_000) as i32, -1_800_000_000, to_lon, from_lat as i32, to_lat as i32) as u32;
        vert(out, m2, -1_800_000_000, to_lat, to_lon);
        return true;
    }
    vert(out, from_lat, from_lon, to_lat, to_lon);
    false
}

/// Polylines per block tile.
pub type Blocks = BTreeMap<u32, Vec<Vec<IC>>>;

/// C++ `add_way_to_area_blocks`.
pub fn add_way_to_blocks(coords: &[IC], blocks: &mut Blocks) -> bool {
    let mut wraps = false;
    if coords.len() < 2 {
        return false;
    }
    let mut cur_idx: Option<u32> = None;
    let mut poly: Vec<IC> = Vec::new();
    for &c in coords {
        let t = block_tile(c.0, c.1);
        if cur_idx != Some(t) {
            if let Some(ci) = cur_idx {
                if poly.len() > 1 {
                    blocks.entry(ci).or_default().push(poly.clone());
                }
                let mut pieces = Vec::new();
                wraps ^= aligned_segments(&mut pieces, *poly.last().unwrap(), c);
                for (tile, a, b) in pieces {
                    blocks.entry(tile).or_default().push(vec![a, b]);
                }
                poly.clear();
            }
            cur_idx = Some(t);
        }
        poly.push(c);
    }
    if let Some(ci) = cur_idx {
        if poly.len() > 1 {
            blocks.entry(ci).or_default().push(poly);
        }
    }
    wraps
}

fn add_south_pole_line(blocks: &mut Blocks) {
    let mut delta = 0i64;
    for &tile in blocks.keys() {
        let (s, _) = tile_sw(tile);
        if (s as f64) / 10_000_000.0 - 91.0 < 0.0 {
            delta += 1;
        } else {
            delta -= 1;
        }
    }
    if delta < 0 {
        return;
    }
    let lat = 10_000_000u32;
    let mut lon: i32 = -1_800_000_000 + 0x10_0000;
    while lon < 1_800_000_000 - 0x10_0000 {
        let from = (lon as u32 & 0xfff0_0000) as i32;
        blocks.entry(block_tile(lat, lon)).or_default().push(vec![(lat, from), (lat, from + 0xfffff)]);
        lon += 0x10_0000;
    }
    let to = ((-1_800_000_000i32) as u32 & 0xfff0_0000) as i32 + 0xfffff;
    blocks.entry(block_tile(lat, -1_800_000_000)).or_default().push(vec![(lat, -1_800_000_000), (lat, to)]);
    let from = (1_800_000_000u32 & 0xfff0_0000) as i32;
    blocks.entry(block_tile(lat, 1_800_000_000)).or_default().push(vec![(lat, from), (lat, 1_800_000_000)]);
}

/// Carry segments so that the southward ray stops at each tile's south edge.
/// Returns false when the area is too large (all blocks dropped).
fn add_segment_blocks(blocks: &mut Blocks) -> bool {
    let mut cur: Option<u32> = None;
    loop {
        let next = match cur {
            None => blocks.keys().next().copied(),
            Some(c) => blocks.range(c + 1..).next().map(|(k, _)| *k),
        };
        let Some(tile) = next else { break };
        cur = Some(tile);
        if blocks.len() > 384 * 1024 {
            blocks.clear();
            return false;
        }
        let mut lons: std::collections::BTreeSet<i32> = std::collections::BTreeSet::new();
        for b in &blocks[&tile] {
            if b.is_empty() {
                continue;
            }
            for l in [b[0].1, b[b.len() - 1].1] {
                if !lons.remove(&l) {
                    lons.insert(l);
                }
            }
        }
        if lons.is_empty() {
            continue;
        }
        let (south, west) = tile_sw(tile);
        let lat = south + 0x10_0000;
        let north = block_tile(lat, west);
        let v: Vec<i32> = lons.into_iter().collect();
        let entry = blocks.entry(north).or_default();
        for pair in v.chunks(2) {
            if pair.len() == 2 {
                entry.push(vec![(lat, pair[0]), (lat, pair[1])]);
            }
        }
    }
    true
}

/// Storage order of blocks inside a tile (C++ `Area_Block::operator<` on the
/// packed coordinates), duplicates removed.
fn packed(c: &IC) -> u64 {
    let lower = crate::geo::Pt::from_ilat_ilon(c.0, c.1);
    let tile = node_tile(c.0, c.1);
    (((tile & 0xff) as u64) << 32) | ll_lower(lower.y, lower.x) as u64
}

fn ll_lower(y: u32, x: u32) -> u32 {
    // Interleave the low 16 bits; lon is offset binary here, which only
    // matters for the low bits (unchanged by the sign flip).
    morton(y & 0xffff, x & 0xffff)
}

pub fn sort_blocks(blocks: &mut Blocks) {
    for v in blocks.values_mut() {
        v.sort_by(|a, b| a.iter().map(packed).cmp(b.iter().map(packed)));
        v.dedup();
    }
}

/// Build the blocks of an area from its way geometries. `None` when the
/// area would be too large.
pub fn build_area(ways: &[Vec<IC>]) -> Option<Blocks> {
    let mut blocks = Blocks::new();
    let mut wraps = false;
    for w in ways {
        wraps ^= add_way_to_blocks(w, &mut blocks);
    }
    if wraps {
        add_south_pole_line(&mut blocks);
    }
    if !add_segment_blocks(&mut blocks) {
        return None;
    }
    if blocks.is_empty() {
        return None;
    }
    sort_blocks(&mut blocks);
    Some(blocks)
}

// ---------------------------------------------------------------------------
// DB areas: tests

/// C++ `Coord_Query_Statement::check_area_block`.
pub fn check_area_block(block: &[IC], coord_lat: u32, coord_lon: i32) -> u8 {
    let mut state = 0u8;
    for w in block.windows(2) {
        match seg_check(w[0], w[1], coord_lat, coord_lon) {
            HIT => return HIT,
            t => state ^= t,
        }
    }
    state
}

/// One segment of `check_area_block`: HIT, toggle bits or 0. Nonzero only
/// when the segment's longitude range (inclusive) holds `coord_lon`.
#[inline]
fn seg_check((last_lat, last_lon): IC, (lat, lon): IC, coord_lat: u32, coord_lon: i32) -> u8 {
    if last_lon < lon {
        if lon < coord_lon || last_lon > coord_lon {
            return 0;
        } else if lon == coord_lon {
            return if lat < coord_lat { TOGGLE_WEST } else if lat == coord_lat { HIT } else { 0 };
        } else if last_lon == coord_lon {
            return if last_lat < coord_lat { TOGGLE_EAST } else if last_lat == coord_lat { HIT } else { 0 };
        }
    } else if last_lon > lon {
        if lon > coord_lon || last_lon < coord_lon {
            return 0;
        } else if lon == coord_lon {
            return if lat < coord_lat { TOGGLE_EAST } else if lat == coord_lat { HIT } else { 0 };
        } else if last_lon == coord_lon {
            return if last_lat < coord_lat { TOGGLE_WEST } else if last_lat == coord_lat { HIT } else { 0 };
        }
    } else {
        let on = lon == coord_lon && ((last_lat <= coord_lat && coord_lat <= lat) || (lat <= coord_lat && coord_lat <= last_lat));
        return if on { HIT } else { 0 };
    }
    let isect = (lat as i64 + (coord_lon as i64 - lon as i64) * (last_lat as i64 - lat as i64) / (last_lon as i64 - lon as i64)) as u32;
    if coord_lat > isect {
        TOGGLE_EAST | TOGGLE_WEST
    } else if coord_lat == isect {
        HIT
    } else {
        0
    }
}

/// `check_area_block` over the segments `segs` (starting points in `pts`)
/// of a block: the same result when they include every segment whose
/// longitude range holds `coord_lon` (HIT wins, toggles commute).
#[inline]
fn check_segments(pts: &[IC], segs: &[u32], coord_lat: u32, coord_lon: i32) -> u8 {
    let mut state = 0u8;
    for &k in segs {
        let k = k as usize;
        match seg_check(pts[k], pts[k + 1], coord_lat, coord_lon) {
            HIT => return HIT,
            t => state ^= t,
        }
    }
    state
}

/// Tile-local coordinates as the C++ way tests see them.
pub fn local(c: &IC) -> (u32, u32) {
    (c.0 & 0xfffff, 0x8000_0000 | ((c.1 as u32) & 0xfffff))
}

fn ordered_intersects_inner(a0: (u32, u32), a1: (u32, u32), b0: (u32, u32), b1: (u32, u32)) -> u8 {
    let (lat_a0, lon_a0) = a0;
    let (lat_a1, lon_a1) = a1;
    let (lat_b0, lon_b0) = b0;
    let (lat_b1, lon_b1) = b1;
    let d = |x: u32| x as f64;
    let det = (d(lat_a1) - d(lat_a0)) * (d(lon_b1) - d(lon_b0)) - (d(lat_b1) - d(lat_b0)) * (d(lon_a1) - d(lon_a0));
    if det != 0.0 {
        // The u32 wrap of `lat_b0 - lat_a0` is a C++ quirk kept on purpose.
        let lon = (d(lat_b0.wrapping_sub(lat_a0)) + d(lon_a0) / (d(lon_a1) - d(lon_a0)) * (d(lat_a1) - d(lat_a0))
            - d(lon_b0) / (d(lon_b1) - d(lon_b0)) * (d(lat_b1) - d(lat_b0)))
            / det
            * (d(lon_a1) - d(lon_a0))
            * (d(lon_b1) - d(lon_b0));
        if d(lon_a0) < lon && lon < d(lon_a1) && d(lon_b0) <= lon && lon <= d(lon_b1) {
            return if (lat_a0 != lat_b0 || lon_a0 != lon_b0) && (lat_a1 != lat_b1 || lon_a1 != lon_b1) { INTERSECT } else { 0 };
        }
    } else if (d(lat_a0) - (d(lon_a0) - d(lon_b0)) / (d(lon_b1) - d(lon_b0)) * (d(lat_b1) - d(lat_b0)) - d(lat_b0)).abs() <= 1.0
        || (d(lat_a1) - (d(lon_a1) - d(lon_b0)) / (d(lon_b1) - d(lon_b0)) * (d(lat_b1) - d(lat_b0)) - d(lat_b0)).abs() <= 1.0
    {
        return HIT;
    }
    0
}

fn ordered_a_intersects_inner(a0: (u32, u32), a1: (u32, u32), b0: (u32, u32), b1: (u32, u32)) -> u8 {
    let d = |x: u32| x as f64;
    if b0.1 < b1.1 {
        if a0.1 < b1.1 && b0.1 < a1.1 {
            return ordered_intersects_inner(a0, a1, b0, b1);
        }
    } else if b1.1 < b0.1 {
        if a0.1 < b0.1 && b1.1 < a1.1 {
            return ordered_intersects_inner(a0, a1, b1, b0);
        }
    } else if a0.1 < b0.1 && b0.1 < a1.1 {
        let lat = (d(b0.1) - d(a0.1)) / (d(a1.1) - d(a0.1)) * (d(a1.0) - d(a0.0)) + d(a0.0);
        return if (d(b0.0) < lat && lat < d(b1.0)) || (d(b1.0) < lat && lat < d(b0.0)) { INTERSECT } else { 0 };
    }
    0
}

fn longitude_a_intersects_inner(lat_a0: u32, lon_a: u32, lat_a1: u32, b0: (u32, u32), b1: (u32, u32)) -> u8 {
    let d = |x: u32| x as f64;
    let (lat_b0, lon_b0) = b0;
    let (lat_b1, lon_b1) = b1;
    if lon_b0 < lon_b1 {
        if lon_a < lon_b1 && lon_b0 < lon_a {
            let lat = (d(lon_a) - d(lon_b0)) / (d(lon_b1) - d(lon_b0)) * (d(lat_b1) - d(lat_b0)) + d(lat_b0);
            return if (d(lat_a0) < lat && lat < d(lat_a1)) || (d(lat_a1) < lat && lat < d(lat_a0)) { INTERSECT } else { 0 };
        }
    } else if lon_b1 < lon_b0 {
        if lon_a < lon_b0 && lon_b1 < lon_a {
            let lat = (d(lon_a) - d(lon_b0)) / (d(lon_b1) - d(lon_b0)) * (d(lat_b1) - d(lat_b0)) + d(lat_b0);
            return if (d(lat_a0) < lat && lat < d(lat_a1)) || (d(lat_a1) < lat && lat < d(lat_a0)) { INTERSECT } else { 0 };
        }
    } else {
        if lon_a != lon_b0 {
            return 0;
        }
        let hit = if lat_a0 < lat_a1 {
            if lat_b0 < lat_b1 { lat_a0 < lat_b1 && lat_b0 < lat_a1 } else { lat_a0 < lat_b0 && lat_b1 < lat_a1 }
        } else if lat_b0 < lat_b1 {
            lat_a1 < lat_b1 && lat_b0 < lat_a0
        } else {
            lat_a1 < lat_b0 && lat_b1 < lat_a0
        };
        return if hit { HIT } else { 0 };
    }
    0
}

/// C++ `intersects_inner` (way piece `a` against area block `b`).
pub fn intersects_inner(a: &[IC], b: &[IC]) -> u8 {
    let ca: Vec<(u32, u32)> = a.iter().map(local).collect();
    let cb: Vec<(u32, u32)> = b.iter().map(local).collect();
    for i in 0..ca.len().saturating_sub(1) {
        for j in 0..cb.len().saturating_sub(1) {
            let r = pair_inner(ca[i], ca[i + 1], cb[j], cb[j + 1]);
            if r != 0 {
                return r;
            }
        }
    }
    0
}

/// One segment pair of `intersects_inner` (tile-local coordinates).
/// Nonzero only when their longitude ranges (inclusive) meet.
#[inline]
fn pair_inner(a0: (u32, u32), a1: (u32, u32), b0: (u32, u32), b1: (u32, u32)) -> u8 {
    if a0.1 < a1.1 {
        ordered_a_intersects_inner(a0, a1, b0, b1)
    } else if a1.1 < a0.1 {
        ordered_a_intersects_inner(a1, a0, b0, b1)
    } else {
        longitude_a_intersects_inner(a0.0, a0.1, a1.0, b0, b1)
    }
}

/// C++ `has_inner_points`: segment midpoints tested against block `b`.
pub fn has_inner_points(a: &[IC], b: &[IC], inside: &mut u8) {
    let ca: Vec<(u32, u32)> = a.iter().map(local).collect();
    let lb: Vec<IC> = b.iter().map(|c| {
        let (la, lo) = local(c);
        (la, lo as i32)
    }).collect();
    for i in 0..ca.len().saturating_sub(1) {
        let lat = ca[i].0.wrapping_add(ca[i + 1].0) / 2;
        let lon = (ca[i].1.wrapping_add(ca[i + 1].1) / 2).wrapping_add(0x8000_0000);
        let c = check_area_block(&lb, lat, lon as i32);
        if c & HIT != 0 {
            *inside = c;
        } else if c != 0 {
            *inside ^= c;
        }
    }
}

// ---------------------------------------------------------------------------
// DB areas: blocks prepared for many tests

const LON_BUCKETS: usize = 64;

/// Segments of a polyline (segment k from point k to k + 1) bucketed by
/// longitude, to find the ones whose longitude range may meet a value or
/// a range without looking at the others.
struct LonIndex {
    lo: i64,
    shift: u32,
    buckets: Vec<Vec<u32>>,
}

impl LonIndex {
    fn build(lons: &[i64]) -> LonIndex {
        let lo = lons.iter().copied().min().unwrap_or(0);
        let hi = lons.iter().copied().max().unwrap_or(0);
        let mut shift = 0;
        while ((hi - lo) >> shift) as usize >= LON_BUCKETS {
            shift += 1;
        }
        let mut buckets = vec![Vec::new(); LON_BUCKETS];
        for k in 0..lons.len().saturating_sub(1) {
            let (a, b) = if lons[k] <= lons[k + 1] { (lons[k], lons[k + 1]) } else { (lons[k + 1], lons[k]) };
            for bucket in &mut buckets[((a - lo) >> shift) as usize..=((b - lo) >> shift) as usize] {
                bucket.push(k as u32);
            }
        }
        LonIndex { lo, shift, buckets }
    }

    /// Segments whose longitude range may hold `x` (ascending).
    fn at(&self, x: i64) -> &[u32] {
        if x < self.lo {
            return &[];
        }
        self.buckets.get(((x - self.lo) >> self.shift) as usize).map_or(&[], |b| b.as_slice())
    }

    /// Segments whose longitude range may meet `a..=b`, ascending and unique.
    fn meeting(&self, a: i64, b: i64, out: &mut Vec<u32>) {
        out.clear();
        if b < self.lo {
            return;
        }
        let first = (a.max(self.lo) - self.lo) >> self.shift;
        if first as usize >= LON_BUCKETS {
            return;
        }
        let last = (((b - self.lo) >> self.shift) as usize).min(LON_BUCKETS - 1);
        for bucket in &self.buckets[first as usize..=last] {
            out.extend_from_slice(bucket);
        }
        if last > first as usize {
            out.sort_unstable();
            out.dedup();
        }
    }
}

/// Blocks with at most this many points are tested without an index.
const SMALL_BLOCK: usize = 8;

/// A DB area block prepared for repeated tests. The tests give the results
/// of `check_area_block`, `intersects_inner` and `has_inner_points` on its
/// points; they only skip segments that cannot change them.
pub struct Block {
    pub pts: Vec<IC>,
    prep: std::sync::OnceLock<Prep>,
}

struct Prep {
    /// Longitude index of `pts`.
    global: LonIndex,
    /// The block as `intersects_inner` sees it, and its index.
    local: Vec<(u32, u32)>,
    local_idx: LonIndex,
    /// The block as `has_inner_points` sees it, and its index.
    lb: Vec<IC>,
    lb_idx: LonIndex,
}

impl Block {
    pub fn new(pts: Vec<IC>) -> Block {
        Block { pts, prep: std::sync::OnceLock::new() }
    }

    /// Prepared on first use: large areas have many blocks no element meets.
    fn prep(&self) -> &Prep {
        self.prep.get_or_init(|| {
            let local: Vec<(u32, u32)> = self.pts.iter().map(local).collect();
            let lb: Vec<IC> = local.iter().map(|&(la, lo)| (la, lo as i32)).collect();
            Prep {
                global: LonIndex::build(&self.pts.iter().map(|p| p.1 as i64).collect::<Vec<_>>()),
                local_idx: LonIndex::build(&local.iter().map(|p| p.1 as i64).collect::<Vec<_>>()),
                lb_idx: LonIndex::build(&lb.iter().map(|p| p.1 as i64).collect::<Vec<_>>()),
                local,
                lb,
            }
        })
    }

    /// `check_area_block(&self.pts, lat, lon)`.
    pub fn check(&self, lat: u32, lon: i32) -> u8 {
        if self.pts.len() <= SMALL_BLOCK {
            return check_area_block(&self.pts, lat, lon);
        }
        check_segments(&self.pts, self.prep().global.at(lon as i64), lat, lon)
    }

    /// `intersects_inner(piece, &self.pts)` for a piece in tile-local
    /// coordinates (see `local`). `scratch` is working memory.
    pub fn intersects_inner(&self, piece: &[(u32, u32)], scratch: &mut Vec<u32>) -> u8 {
        let p = self.prep();
        for w in piece.windows(2) {
            let (a0, a1) = (w[0], w[1]);
            p.local_idx.meeting(a0.1.min(a1.1) as i64, a0.1.max(a1.1) as i64, scratch);
            for &j in scratch.iter() {
                let j = j as usize;
                let r = pair_inner(a0, a1, p.local[j], p.local[j + 1]);
                if r != 0 {
                    return r;
                }
            }
        }
        0
    }

    /// `has_inner_points(piece, &self.pts, inside)` for a piece in
    /// tile-local coordinates.
    pub fn has_inner_points(&self, piece: &[(u32, u32)], inside: &mut u8) {
        let p = self.prep();
        for w in piece.windows(2) {
            let lat = w[0].0.wrapping_add(w[1].0) / 2;
            let lon = (w[0].1.wrapping_add(w[1].1) / 2).wrapping_add(0x8000_0000) as i32;
            let c = check_segments(&p.lb, p.lb_idx.at(lon as i64), lat, lon);
            if c & HIT != 0 {
                *inside = c;
            } else if c != 0 {
                *inside ^= c;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Implicit areas (data/tilewise_geometry.h)

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TSeg {
    pub lat_w: u32,
    pub lon_w: i32,
    pub lat_e: u32,
    pub lon_e: i32,
}

impl TSeg {
    pub fn new(lat_l: u32, lon_l: i32, lat_r: u32, lon_r: i32) -> TSeg {
        if lon_l < lon_r { TSeg { lat_w: lat_l, lon_w: lon_l, lat_e: lat_r, lon_e: lon_r } } else { TSeg { lat_w: lat_r, lon_w: lon_r, lat_e: lat_l, lon_e: lon_l } }
    }
}

fn ilat_f(lat: f64) -> u32 {
    ((lat + 91.0) * 10_000_000.0 + 0.5) as u32
}

fn lon_of(ilon: i32) -> f64 {
    ilon as f64 / 10_000_000.0
}

struct GreatCircle {
    s: f64,
    cs: f64,
    cc: f64,
}

impl GreatCircle {
    fn new(l: (f64, f64), r: (f64, f64)) -> GreatCircle {
        let pi = std::f64::consts::PI;
        let ls = (l.0 / 180.0 * pi).sin();
        let lcos = (l.0 / 180.0 * pi).cos();
        let lcs = lcos * (l.1 / 180.0 * pi).sin();
        let lcc = lcos * (l.1 / 180.0 * pi).cos();
        let rs = (r.0 / 180.0 * pi).sin();
        let rcos = (r.0 / 180.0 * pi).cos();
        let rcs = rcos * (r.1 / 180.0 * pi).sin();
        let rcc = rcos * (r.1 / 180.0 * pi).cos();
        let mut s = lcs * rcc - lcc * rcs;
        let mut cs = lcc * rs - ls * rcc;
        let mut cc = ls * rcs - lcs * rs;
        if (l.1 < r.1 && r.1 - l.1 > 180.0) || (r.1 < l.1 && l.1 - r.1 < 180.0) {
            s = -s;
            cs = -cs;
            cc = -cc;
        }
        let norm = (s * s + cs * cs + cc * cc).sqrt();
        if norm > 0.0 {
            s /= norm;
            cs /= norm;
            cc /= norm;
        }
        GreatCircle { s, cs, cc }
    }

    fn lat_of(&self, lon: f64) -> f64 {
        let pi = std::f64::consts::PI;
        let g_cc = self.cc * (lon / 180.0 * pi).cos() + self.cs * (lon / 180.0 * pi).sin();
        let norm = (g_cc * g_cc + self.s * self.s).sqrt();
        if g_cc > norm {
            return 90.0;
        }
        (g_cc / norm).asin() / pi * 180.0
    }
}

fn to_deg(c: IC) -> (f64, f64) {
    (c.0 as f64 / 10_000_000.0 - 91.0, c.1 as f64 / 10_000_000.0)
}

fn touched_index(p: IC, q: IC) -> u32 {
    let (lat_l, lon_l) = (p.0 >> 16, p.1 >> 16);
    let (lat_r, lon_r) = (q.0 >> 16, q.1 >> 16);
    let near = |a: u32, b: u32| b.wrapping_sub(a) <= 1 || a.wrapping_sub(b) <= 1;
    if lat_l == lat_r && (lon_r - lon_l).abs() <= 1 {
        return 0;
    }
    if lon_l == lon_r && near(lat_l, lat_r) {
        return 0;
    }
    if near(lat_l, lat_r) && (lon_r - lon_l).abs() <= 1 {
        let (lat_l, lon_l, lat_r, lon_r) = (p.0, p.1, q.0, q.1);
        let lon_b = ((lon_l.max(lon_r) as u32) & 0xffff_0000) as i32;
        let lat_b = ((lat_r as f64 - lat_l as f64) * (lon_b as f64 - lon_l as f64) / (lon_r as f64 - lon_l as f64) + lat_l as f64) as u32;
        let missing = if lat_l < lat_r {
            if lat_b < (lat_r & 0xffff_0000) { lon_r } else { lon_l }
        } else if lat_b < (lat_l & 0xffff_0000) {
            lon_l
        } else {
            lon_r
        };
        return node_tile(lat_b, missing);
    }
    0xff
}

fn south_north(push: &mut impl FnMut(u32, TSeg), lat_l: u32, lon_l: i32, lat_r: u32, lon_r: i32) {
    if (lat_l & 0xffff_0000) == (lat_r & 0xffff_0000) {
        push(node_tile(lat_l, lon_l), TSeg::new(lat_l, lon_l, lat_r, lon_r));
        return;
    }
    let (lat_l, lon_l, lat_r, lon_r) = if lat_r < lat_l { (lat_r, lon_r, lat_l, lon_l) } else { (lat_l, lon_l, lat_r, lon_r) };
    let min_lon = lon_l.min(lon_r);
    let x = |y: u32| (lon_l as f64 + (y as f64 - lat_l as f64) / (lat_r as i32 as f64 - lat_l as i32 as f64) * (lon_r as f64 - lon_l as f64)) as i32;
    let mut i_lat = (lat_l & 0xffff_0000) + 0x10000;
    let mut i_lon = x(i_lat);
    push(node_tile(lat_l, min_lon), TSeg::new(lat_l, lon_l, i_lat, i_lon));
    while i_lat + 0x10000 < lat_r {
        let (last_lat, last_lon) = (i_lat, i_lon);
        i_lat += 0x10000;
        i_lon = x(i_lat);
        push(node_tile(last_lat, min_lon), TSeg::new(last_lat, last_lon, i_lat, i_lon));
    }
    push(node_tile(i_lat, min_lon), TSeg::new(i_lat, i_lon, lat_r, lon_r));
}

fn auxiliary(push: &mut impl FnMut(u32, TSeg), p: IC, q: IC) {
    let (lat_l, lon_l, lat_r, lon_r) = (p.0, p.1, q.0, q.1);
    if (lon_l as u32 & 0xffff_0000) == (lon_r as u32 & 0xffff_0000) {
        south_north(push, lat_l, lon_l, lat_r, lon_r);
        return;
    }
    let gc = GreatCircle::new(to_deg(p), to_deg(q));
    let (min, max) = if lon_l < lon_r { (p, q) } else { (q, p) };
    let mut i_ilon = min.1;
    let ilon_max = max.1;
    let col = |x: i32| ((x as u32 & 0xffff_0000) as i32).wrapping_add(0x10000);
    if (ilon_max as i64 - i_ilon as i64) < 1_800_000_000 {
        let mut i_ilat = ilat_f(gc.lat_of(lon_of(col(i_ilon))));
        south_north(push, min.0, i_ilon, i_ilat, col(i_ilon));
        i_ilon = col(i_ilon);
        while i_ilon + 0x10000 < ilon_max {
            let last = i_ilat;
            i_ilat = ilat_f(gc.lat_of(lon_of(i_ilon + 0x10000)));
            south_north(push, last, i_ilon, i_ilat, i_ilon + 0x10000);
            i_ilon += 0x10000;
        }
        south_north(push, i_ilat, i_ilon, max.0, ilon_max);
    } else if ilon_max as i64 - i_ilon as i64 == 1_800_000_000 {
        let (a, b) = (min.0 as u64, max.0 as u64);
        if a + b > 2 * 910_000_000 {
            south_north(push, a as u32, i_ilon, 1_810_000_000, i_ilon);
            south_north(push, b as u32, ilon_max, 1_810_000_000, ilon_max);
        } else {
            south_north(push, 10_000_000, i_ilon, a as u32, i_ilon);
            south_north(push, 10_000_000, ilon_max, b as u32, ilon_max);
        }
    } else {
        // Across the antimeridian.
        if (i_ilon as u32 & 0xffff_0000) == ((-1_800_000_000i32) as u32 & 0xffff_0000) {
            south_north(push, ilat_f(gc.lat_of(-180.0)), -1_800_000_000, min.0, i_ilon);
        } else {
            let mut i_ilat = ilat_f(gc.lat_of(-180.0));
            let lmax = i_ilon;
            i_ilon = -1_800_000_000;
            south_north(push, min.0, i_ilon, i_ilat, col(i_ilon));
            i_ilon = col(i_ilon);
            while i_ilon + 0x10000 < lmax {
                let last = i_ilat;
                i_ilat = ilat_f(gc.lat_of(lon_of(i_ilon + 0x10000)));
                south_north(push, last, i_ilon, i_ilat, i_ilon + 0x10000);
                i_ilon += 0x10000;
            }
            south_north(push, i_ilat, i_ilon, min.0, lmax);
        }
        if (ilon_max as u32 & 0xffff_0000) == (1_800_000_000u32 & 0xffff_0000) {
            let anti = ilat_f(gc.lat_of(180.0));
            south_north(push, max.0, ilon_max, anti, 1_800_000_000);
            push(node_tile(anti, 1_800_000_000), TSeg::new(anti, 1_800_000_000, anti, 1_800_000_001));
        } else {
            i_ilon = ilon_max;
            let mut i_ilat = ilat_f(gc.lat_of(lon_of(col(i_ilon))));
            south_north(push, max.0, i_ilon, i_ilat, col(i_ilon));
            i_ilon = col(i_ilon);
            while i_ilon + 0x10000 < 1_800_000_000 {
                let last = i_ilat;
                i_ilat = ilat_f(gc.lat_of(lon_of(i_ilon + 0x10000)));
                south_north(push, last, i_ilon, i_ilat, i_ilon + 0x10000);
                i_ilon += 0x10000;
            }
            let anti = ilat_f(gc.lat_of(180.0));
            south_north(push, i_ilat, i_ilon, anti, 1_800_000_000);
            push(node_tile(anti, 1_800_000_000), TSeg::new(anti, 1_800_000_000, anti, 1_800_000_001));
        }
    }
}

/// C++ `make_entries`: segments of a way per node tile.
pub fn make_entries(geom: &[IC], push: &mut impl FnMut(u32, TSeg)) {
    for i in 1..geom.len() {
        let (p, q) = (geom[i - 1], geom[i]);
        let tp = node_tile(p.0, p.1);
        let tq = node_tile(q.0, q.1);
        if tp == tq {
            push(tq, TSeg::new(p.0, p.1, q.0, q.1));
            continue;
        }
        let extra = touched_index(p, q);
        if extra == 0xff {
            auxiliary(push, p, q);
        } else {
            let s = TSeg::new(p.0, p.1, q.0, q.1);
            push(tp, s);
            push(tq, s);
            if extra != 0 {
                push(extra, s);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelPos {
    Outside,
    Inside,
    Border,
}

#[derive(Clone, Debug, Default)]
pub struct TBlock {
    pub segs: Vec<TSeg>,
    pub sw: bool,
}

fn isect_lon(s: &TSeg, south: u32) -> f64 {
    s.lon_w as f64 + (south as f64 - s.lat_w as f64) * (s.lon_e as f64 - s.lon_w as f64) / (s.lat_e as i32 as f64 - s.lat_w as i32 as f64)
}

fn isect_lat(s: &TSeg, lon_p: i32) -> f64 {
    s.lat_w as f64 + (lon_p as f64 - s.lon_w as f64) * (s.lat_e as i32 as f64 - s.lat_w as i32 as f64) / (s.lon_e as f64 - s.lon_w as f64)
}

fn eastern_sw_is_inside(b: &TBlock, south: u32, west: i32) -> bool {
    let mut inside = b.sw;
    for s in &b.segs {
        if (s.lat_w <= south) ^ (s.lat_e <= south) {
            let x = isect_lon(s, south);
            if west as f64 <= x && x < west as f64 + 65536.0 {
                inside = !inside;
            }
        }
    }
    inside
}

fn northern_sw_is_inside(b: &TBlock) -> bool {
    let mut inside = b.sw;
    for s in &b.segs {
        if s.lon_w == -1_800_000_000 && s.lon_e != -1_800_000_000 {
            inside = !inside;
        }
    }
    inside
}

/// C++ free function `rel_pos_ilat_ilon` for one way's block.
pub fn rel_pos(lat_p: u32, lon_p: i32, b: &TBlock, south: u32, west: i32) -> RelPos {
    let mut inside = b.sw;
    let fw = west as f64;
    for s in &b.segs {
        let northern = |inside: &mut bool, vertical_case: bool| -> Option<RelPos> {
            if s.lon_w == lon_p {
                if s.lat_w == lat_p {
                    return Some(RelPos::Border);
                }
                if vertical_case && s.lon_w == s.lon_e {
                    if s.lat_w.min(s.lat_e) <= lat_p && lat_p <= s.lat_w.max(s.lat_e) {
                        return Some(RelPos::Border);
                    }
                } else {
                    *inside ^= s.lat_w < lat_p;
                }
            } else if s.lon_e == lon_p {
                if s.lat_e == lat_p {
                    return Some(RelPos::Border);
                }
            } else if s.lon_w < lon_p && lon_p < s.lon_e {
                let y = isect_lat(s, lon_p);
                if y - 0.5 < lat_p as f64 {
                    if (lat_p as f64) < y + 0.5 {
                        return Some(RelPos::Border);
                    }
                    *inside = !*inside;
                }
            }
            None
        };
        if s.lat_w > south && s.lat_e > south {
            if let Some(r) = northern(&mut inside, true) {
                return r;
            }
        } else if (s.lat_w <= south) ^ (s.lat_e <= south) {
            let x = isect_lon(s, south);
            if fw <= x && x < fw + 65536.0 {
                if s.lat_w <= south {
                    if s.lon_e <= lon_p {
                        if s.lon_e == lon_p {
                            if s.lat_e == lat_p {
                                return RelPos::Border;
                            }
                            if s.lon_w == s.lon_e && lat_p <= s.lat_e {
                                return RelPos::Border;
                            }
                        }
                        inside = !inside;
                    } else if x < lon_p as f64 {
                        let y = isect_lat(s, lon_p);
                        if (lat_p as f64) < y + 0.5 {
                            if y - 0.5 < lat_p as f64 {
                                return RelPos::Border;
                            }
                            inside = !inside;
                        }
                    }
                } else if x <= lon_p as f64 {
                    if x == lon_p as f64 {
                        if lat_p == south {
                            return RelPos::Border;
                        }
                        if x == s.lon_w as f64 && lat_p <= s.lat_w {
                            return RelPos::Border;
                        }
                    }
                    inside = !inside;
                } else if s.lon_w <= lon_p {
                    let y = isect_lat(s, lon_p);
                    if y - 0.5 < lat_p as f64 {
                        if (lat_p as f64) < y + 0.5 {
                            return RelPos::Border;
                        }
                        inside = !inside;
                    }
                }
            } else if let Some(r) = northern(&mut inside, false) {
                return r;
            }
        } else if s.lat_w == south && s.lat_e == south {
            if lat_p == south && s.lon_w <= lon_p && lon_p <= s.lon_e {
                return RelPos::Border;
            }
            if s.lon_w == -1_800_000_000 {
                inside = !inside;
            }
        }
    }
    if inside { RelPos::Inside } else { RelPos::Outside }
}

/// C++ free function `rel_position(idx, segment, block)`.
fn seg_rel_position(tile: u32, seg: &TSeg, b: &TBlock) -> RelPos {
    let mut touch: Vec<(u32, i32)> = Vec::new();
    for a in &b.segs {
        if !(a.lon_w <= seg.lon_e && seg.lon_w <= a.lon_e) {
            continue;
        }
        let (sw_lat, sw_lon, se_lat, se_lon) = (seg.lat_w as f64, seg.lon_w, seg.lat_e as f64, seg.lon_e);
        let s_aw = (a.lat_w as f64 - sw_lat) * ((se_lon - sw_lon) as f64) - ((a.lon_w - sw_lon) as f64) * (se_lat - sw_lat);
        let s_ae = (a.lat_e as f64 - sw_lat) * ((se_lon - sw_lon) as f64) - ((a.lon_e - sw_lon) as f64) * (se_lat - sw_lat);
        if s_ae * s_aw > 0.0 {
            continue;
        }
        let s_sw = (seg.lat_w as f64 - a.lat_w as f64) * ((a.lon_e - a.lon_w) as f64) - ((seg.lon_w - a.lon_w) as f64) * (a.lat_e as f64 - a.lat_w as f64);
        let s_se = (seg.lat_e as f64 - a.lat_w as f64) * ((a.lon_e - a.lon_w) as f64) - ((seg.lon_e - a.lon_w) as f64) * (a.lat_e as f64 - a.lat_w as f64);
        if s_se * s_sw > 0.0 {
            continue;
        }
        if s_aw == 0.0 {
            touch.push((a.lat_w, a.lon_w));
            if s_ae == 0.0 {
                touch.push((a.lat_e, a.lon_e));
            }
        } else if s_ae == 0.0 {
            touch.push((a.lat_e, a.lon_e));
        } else if s_sw == 0.0 {
            touch.push((seg.lat_w, seg.lon_w));
        } else if s_se != 0.0 {
            return RelPos::Inside;
        }
    }
    if !touch.is_empty() {
        touch.push((seg.lat_w, seg.lon_w));
        touch.push((seg.lat_e, seg.lon_e));
        touch.sort_unstable();
        touch.dedup();
        let (south, west) = tile_sw(tile);
        for w in touch.windows(2) {
            let m_lat = ((w[0].0 as u64 + w[1].0 as u64) / 2) as u32;
            let m_lon = ((w[0].1 as i64 + w[1].1 as i64) / 2) as i32;
            if rel_pos(m_lat, m_lon, b, south, west) == RelPos::Inside {
                return RelPos::Inside;
            }
        }
    }
    RelPos::Outside
}

type TKey = (u32, u32, u64);

/// Union of implicit areas, indexed by node tile. Entries per tile are kept
/// in the C++ order of the area ways (index order, then id).
///
/// Only tiles that hold segments are stored. The C++ code also materializes
/// every tile inside an area (the inside flag travels east along tile rows,
/// and north along the westmost column); those tiles are kept as runs of
/// columns per row instead, so memory follows the outline, not the surface.
#[derive(Default)]
pub struct Tilewise {
    tiles: BTreeMap<u32, BTreeMap<TKey, TBlock>>,
    /// Per tile row (south edge): (west exclusive, west exclusive, area) runs
    /// of tiles without segments of that area that lie inside it.
    fills: rustc_hash::FxHashMap<u32, Vec<(i64, i64, TKey)>>,
}

/// Southern edge of the northernmost tile row that can hold points.
const MAX_SOUTH: u32 = 1_810_000_000;

impl Tilewise {
    /// `ways`: (C++ index, way id, vertex coordinates) of closed ways.
    pub fn build(ways: &[(u32, u64, Vec<IC>)]) -> Tilewise {
        let mut t = Tilewise::default();
        for (idx, id, geom) in ways {
            let (a, b) = crate::geo::cpp_index_order(*idx);
            let key = (a, b, *id);
            make_entries(geom, &mut |tile, seg| {
                t.tiles.entry(tile).or_default().entry(key).or_default().segs.push(seg);
            });
        }
        // Tile keys are Morton codes: ascending order visits every row from
        // west to east and the westmost column from south to north.
        let mut last_in_row: rustc_hash::FxHashMap<(u32, TKey), (i32, bool)> = rustc_hash::FxHashMap::default();
        let mut westmost: rustc_hash::FxHashMap<TKey, BTreeMap<u32, bool>> = rustc_hash::FxHashMap::default();
        let chain = |westmost: &rustc_hash::FxHashMap<TKey, BTreeMap<u32, bool>>, key: &TKey, south: u32| {
            westmost.get(key).and_then(|m| m.range(..south).next_back()).is_some_and(|(_, inside)| *inside)
        };
        for (&tile, entries) in t.tiles.iter_mut() {
            let (south, west) = tile_sw(tile);
            let is_westmost = west < -1_800_000_000;
            for (key, b) in entries.iter_mut() {
                b.sw = if is_westmost {
                    chain(&westmost, key, south)
                } else {
                    match last_in_row.get(&(south, *key)) {
                        Some(&(pw, inside)) => {
                            if inside && pw.wrapping_add(0x10000) != west {
                                t.fills.entry(south).or_default().push((pw as i64, west as i64, *key));
                            }
                            inside
                        }
                        None => {
                            let inside = chain(&westmost, key, south);
                            if inside {
                                t.fills.entry(south).or_default().push((i64::MIN, west as i64, *key));
                            }
                            inside
                        }
                    }
                };
                last_in_row.insert((south, *key), (west, eastern_sw_is_inside(b, south, west)));
                if is_westmost {
                    westmost.entry(*key).or_default().insert(south, northern_sw_is_inside(b));
                }
            }
        }
        for (&(south, key), &(west, inside)) in &last_in_row {
            if inside {
                t.fills.entry(south).or_default().push((west as i64, i64::MAX, key));
            }
        }
        // Rows without segments of an area, inside it through the westmost column.
        for (key, col) in &westmost {
            let rows: Vec<(u32, bool)> = col.iter().map(|(s, i)| (*s, *i)).collect();
            for (i, &(south, inside)) in rows.iter().enumerate() {
                if !inside {
                    continue;
                }
                let end = rows.get(i + 1).map_or(MAX_SOUTH.saturating_add(1), |r| r.0);
                let mut row = south.saturating_add(0x10000);
                while row < end && row <= MAX_SOUTH {
                    if !last_in_row.contains_key(&(row, *key)) {
                        t.fills.entry(row).or_default().push((i64::MIN, i64::MAX, *key));
                    }
                    row += 0x10000;
                }
            }
        }
        t
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// Areas inside which a whole tile lies (the tile holds none of their segments).
    fn filled(&self, tile: u32) -> impl Iterator<Item = &TKey> {
        let (south, west) = tile_sw(tile);
        let w = west as i64;
        self.fills.get(&south).into_iter().flatten().filter(move |(lo, hi, _)| *lo < w && w < *hi).map(|(_, _, k)| k)
    }

    fn in_tile(tile: u32, lat: u32, lon: i32) -> bool {
        let (south, west) = tile_sw(tile);
        !(lat < south || lat - south > 65535 || lon < west || (lon as i64 - west as i64) > 65535)
    }

    /// Position of a point against the union (tile-checked).
    pub fn point(&self, lat: u32, lon: i32, accept_border: bool) -> RelPos {
        let tile = node_tile(lat, lon);
        if self.filled(tile).next().is_some() && Self::in_tile(tile, lat, lon) {
            return RelPos::Inside;
        }
        let Some(entries) = self.tiles.get(&tile) else { return RelPos::Outside };
        Self::point_in(tile, entries, lat, lon, accept_border)
    }

    fn point_in(tile: u32, entries: &BTreeMap<TKey, TBlock>, lat: u32, lon: i32, accept_border: bool) -> RelPos {
        if !Self::in_tile(tile, lat, lon) {
            return RelPos::Outside;
        }
        let (south, west) = tile_sw(tile);
        for b in entries.values() {
            match rel_pos(lat, lon, b, south, west) {
                RelPos::Inside => return RelPos::Inside,
                RelPos::Border if accept_border => return RelPos::Border,
                _ => {}
            }
        }
        RelPos::Outside
    }

    /// Whether a way lies inside the union (C++ `ways_contained_in`).
    pub fn way(&self, geom: &[IC]) -> bool {
        let mut per_tile: BTreeMap<u32, Vec<TSeg>> = BTreeMap::new();
        make_entries(geom, &mut |tile, seg| per_tile.entry(tile).or_default().push(seg));
        for (tile, segs) in per_tile {
            if self.filled(tile).next().is_some() && segs.iter().any(|s| Self::in_tile(tile, s.lat_w, s.lon_w) || Self::in_tile(tile, s.lat_e, s.lon_e)) {
                return true;
            }
            let Some(entries) = self.tiles.get(&tile) else { continue };
            for s in &segs {
                if Self::point_in(tile, entries, s.lat_w, s.lon_w, false) == RelPos::Inside {
                    return true;
                }
                if Self::point_in(tile, entries, s.lat_e, s.lon_e, false) == RelPos::Inside {
                    return true;
                }
                for b in entries.values() {
                    if seg_rel_position(tile, s, b) != RelPos::Outside {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Ids of the area ways containing a point (border included).
    pub fn covering(&self, lat: u32, lon: i32) -> Vec<u64> {
        let tile = node_tile(lat, lon);
        let mut out: Vec<u64> = if Self::in_tile(tile, lat, lon) { self.filled(tile).map(|k| k.2).collect() } else { Vec::new() };
        let Some(entries) = self.tiles.get(&tile) else { return out };
        let (south, west) = tile_sw(tile);
        out.extend(entries.iter().filter(|(_, b)| rel_pos(lat, lon, b, south, west) != RelPos::Outside).map(|(k, _)| k.2));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::{ilat, ilon};

    /// xorshift: deterministic test data without a dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Points in one block tile, many on a small grid so that equal
    /// coordinates, vertical and horizontal segments are common.
    fn polyline(rng: &mut Rng, n: usize) -> Vec<IC> {
        let (lat0, lon0) = (ilat(47.3) & !0xfffff, ilon(8.5) & !0xfffff);
        (0..n)
            .map(|_| {
                if rng.below(3) == 0 {
                    (lat0 + rng.below(0x100000) as u32, lon0 + rng.below(0x100000) as i32)
                } else {
                    (lat0 + 0x10000 * rng.below(16) as u32, lon0 + 0x10000 * rng.below(16) as i32)
                }
            })
            .collect()
    }

    #[test]
    fn prepared_blocks_match_plain_tests() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for round in 0..400 {
            let n = 2 + rng.below(if round % 4 == 0 { 6 } else { 120 }) as usize;
            let b = polyline(&mut rng, n);
            let block = Block::new(b.clone());
            for _ in 0..40 {
                let p = polyline(&mut rng, 1)[0];
                assert_eq!(block.check(p.0, p.1), check_area_block(&b, p.0, p.1), "check {b:?} {p:?}");
                let n = 2 + rng.below(6) as usize;
                let piece = polyline(&mut rng, n);
                let local_piece: Vec<(u32, u32)> = piece.iter().map(local).collect();
                let mut scratch = Vec::new();
                assert_eq!(block.intersects_inner(&local_piece, &mut scratch), intersects_inner(&piece, &b), "intersects {piece:?} {b:?}");
                let mut x = rng.below(16) as u8;
                let mut y = x;
                block.has_inner_points(&local_piece, &mut x);
                has_inner_points(&piece, &b, &mut y);
                assert_eq!(x, y, "inner points {piece:?} {b:?}");
            }
            // Points on the block's own vertices and segment ends.
            for &(la, lo) in &b {
                assert_eq!(block.check(la, lo), check_area_block(&b, la, lo));
                assert_eq!(block.check(la.wrapping_add(1), lo), check_area_block(&b, la.wrapping_add(1), lo));
            }
        }
    }

    fn ic(lat: f64, lon: f64) -> IC {
        (ilat(lat), ilon(lon))
    }

    #[test]
    fn square_db_area() {
        let ring = vec![ic(10.0, 20.0), ic(10.0, 20.5), ic(10.5, 20.5), ic(10.5, 20.0), ic(10.0, 20.0)];
        let blocks = build_area(&[ring]).unwrap();
        let inside = |p: IC| {
            let t = block_tile(p.0, p.1);
            let Some(bs) = blocks.get(&t) else { return false };
            let mut st = 0u8;
            for b in bs {
                let c = check_area_block(b, p.0, p.1);
                if c == HIT {
                    return true;
                }
                st ^= c;
            }
            st != 0
        };
        assert!(inside(ic(10.25, 20.25)));
        assert!(inside(ic(10.0, 20.25)));
        assert!(!inside(ic(9.9, 20.1)));
        assert!(!inside(ic(10.6, 20.25)));
    }

    #[test]
    fn square_implicit_area() {
        let ring = vec![ic(51.25, 7.15), ic(51.25, 7.16), ic(51.26, 7.16), ic(51.26, 7.15), ic(51.25, 7.15)];
        let t = Tilewise::build(&[(0, 1, ring)]);
        assert_eq!(t.point(ilat(51.255), ilon(7.155), false), RelPos::Inside);
        assert_eq!(t.point(ilat(51.27), ilon(7.155), true), RelPos::Outside);
        assert_eq!(t.point(ilat(51.25), ilon(7.155), true), RelPos::Border);
    }
}
