// Contains code translated from C++ to Rust in 2026 from Overpass API,
// Copyright 2008-2018 Roland Olbricht et al. Modified by Chavdar Dutsov.
// GNU Affero General Public License, version 3 or later. See LICENSE and NOTICE.

//! Coordinate quantization, quad-tile cells and spherical geometry.
//!
//! Coordinates use the same integer encoding as the C++ Overpass API:
//! `ilat = (lat + 91) * 1e7 + 0.5` (unsigned) and `ilon = lon * 1e7 ± 0.5` (signed).
//! Spatial keys work on two unsigned 32 bit axes: `y = ilat` and
//! `x = ilon as u32 ^ 0x8000_0000`, so both axes grow monotonically.

use std::f64::consts::PI;

/// Earth radius implied by the C++ code: 1e7 m per quarter meridian.
pub const ARC_TO_METER: f64 = 10_000_000.0 / (PI / 2.0);
const DEG_TO_ARC: f64 = PI / 180.0;

pub const MAX_LEVEL: u8 = 16;

#[inline]
pub fn ilat(lat: f64) -> u32 {
    ((lat + 91.0) * 10_000_000.0 + 0.5) as u32
}

#[inline]
pub fn ilon(lon: f64) -> i32 {
    (lon * 10_000_000.0 + if lon > 0.0 { 0.5 } else { -0.5 }) as i32
}

#[inline]
pub fn lat_of(ilat: u32) -> f64 {
    ilat as f64 / 10_000_000.0 - 91.0
}

#[inline]
pub fn lon_of(ilon: i32) -> f64 {
    ilon as f64 / 10_000_000.0
}

#[inline]
pub fn x_of_ilon(ilon: i32) -> u32 {
    (ilon as u32) ^ 0x8000_0000
}

#[inline]
pub fn ilon_of_x(x: u32) -> i32 {
    (x ^ 0x8000_0000) as i32
}

/// A point in the unsigned spatial key space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pt {
    pub y: u32,
    pub x: u32,
}

impl Pt {
    pub fn from_ll(lat: f64, lon: f64) -> Pt {
        Pt { y: ilat(lat), x: x_of_ilon(ilon(lon)) }
    }
    pub fn from_ilat_ilon(ilat: u32, ilon: i32) -> Pt {
        Pt { y: ilat, x: x_of_ilon(ilon) }
    }
    pub fn lat(&self) -> f64 {
        lat_of(self.y)
    }
    pub fn lon(&self) -> f64 {
        lon_of(ilon_of_x(self.x))
    }
    pub fn ilon(&self) -> i32 {
        ilon_of_x(self.x)
    }
}

/// Bounding box in key space, inclusive on all sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rect {
    pub y0: u32,
    pub x0: u32,
    pub y1: u32,
    pub x1: u32,
}

impl Rect {
    /// Whether the box of the nodes of a way (or of the members of a
    /// relation) may hold an edge that the exact tests take across the
    /// antimeridian, which leaves the box: ends more than 180 degrees apart
    /// (C++ `Bbox_Double::intersects`, polygons and areas, the shorter
    /// great circle of `around`), or exactly 180 apart, which the way test of
    /// implicit areas takes over a pole. The box alone cannot decide such an
    /// element.
    pub fn may_cross(&self) -> bool {
        !self.is_empty() && self.x1 - self.x0 >= 1_800_000_000
    }

    pub const EMPTY: Rect = Rect { y0: u32::MAX, x0: u32::MAX, y1: 0, x1: 0 };
    pub const WORLD: Rect = Rect { y0: 0, x0: 0, y1: u32::MAX, x1: u32::MAX };

    pub fn point(p: Pt) -> Rect {
        Rect { y0: p.y, x0: p.x, y1: p.y, x1: p.x }
    }
    pub fn is_empty(&self) -> bool {
        self.y0 > self.y1 || self.x0 > self.x1
    }
    pub fn extend(&mut self, p: Pt) {
        self.y0 = self.y0.min(p.y);
        self.x0 = self.x0.min(p.x);
        self.y1 = self.y1.max(p.y);
        self.x1 = self.x1.max(p.x);
    }
    pub fn union(&mut self, o: &Rect) {
        if o.is_empty() {
            return;
        }
        self.y0 = self.y0.min(o.y0);
        self.x0 = self.x0.min(o.x0);
        self.y1 = self.y1.max(o.y1);
        self.x1 = self.x1.max(o.x1);
    }
    pub fn contains(&self, p: Pt) -> bool {
        p.y >= self.y0 && p.y <= self.y1 && p.x >= self.x0 && p.x <= self.x1
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        !(o.y0 > self.y1 || o.y1 < self.y0 || o.x0 > self.x1 || o.x1 < self.x0)
    }

    /// The box grown north and south by `m` units (an empty box stays empty).
    pub fn grow_lat(&self, m: u32) -> Rect {
        if self.is_empty() {
            return *self;
        }
        Rect { y0: self.y0.saturating_sub(m), y1: self.y1.saturating_add(m), ..*self }
    }

    /// The box that holds the great-circle edges of an element whose nodes
    /// (or members) have this box: grown north and south by the `arc_bulge`
    /// of its width. `around` and the way test of implicit areas follow
    /// these edges; the bbox test, polygons and DB areas take straight lines
    /// between the nodes, which stay in the box.
    pub fn arc_box(&self) -> Rect {
        if self.is_empty() {
            return *self;
        }
        self.grow_lat(arc_bulge((self.x1 - self.x0) as u64))
    }
}

/// Bound on how far, in units of 10^-7 degrees, the great-circle arc between
/// two points at most `width` units apart in longitude passes north or south
/// of the latitudes of its ends, plus 2 units for rounding. From 180 degrees
/// on, the arc may pass a pole or cross the antimeridian, and the bound is
/// the whole range.
pub fn arc_bulge(width: u64) -> u32 {
    if width >= 1_800_000_000 {
        return u32::MAX;
    }
    // Between ends at latitudes up to p, tan(lat) <= tan(p) / cos(width / 2)
    // = tan(p) / c; atan(tan(p) / c) - p is largest at tan(p) = sqrt(c).
    let c = (width as f64 / 2e7).to_radians().cos().sqrt();
    let d = ((1.0 / c).atan() - c.atan()).to_degrees();
    (d * 1e7).ceil() as u32 + 2
}

/// A geographic bbox as given in queries (south, west, north, east) in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BBox {
    pub south: f64,
    pub west: f64,
    pub north: f64,
    pub east: f64,
}

impl BBox {
    /// Key space rectangles covered; two when the box crosses the antimeridian.
    pub fn rects(&self) -> Vec<Rect> {
        let y0 = ilat(self.south);
        let y1 = ilat(self.north);
        if self.west <= self.east {
            vec![Rect { y0, y1, x0: x_of_ilon(ilon(self.west)), x1: x_of_ilon(ilon(self.east)) }]
        } else {
            vec![
                Rect { y0, y1, x0: x_of_ilon(ilon(self.west)), x1: x_of_ilon(ilon(180.0)) },
                Rect { y0, y1, x0: x_of_ilon(ilon(-180.0)), x1: x_of_ilon(ilon(self.east)) },
            ]
        }
    }

    pub fn contains_ll(&self, lat: f64, lon: f64) -> bool {
        if lat < self.south || lat > self.north {
            return false;
        }
        if self.west <= self.east {
            lon >= self.west && lon <= self.east
        } else {
            lon >= self.west || lon <= self.east
        }
    }
}

// ---------------------------------------------------------------------------
// Quad-tile cells

/// Spread the lower 16 bits of `v` to the even bit positions of a u32.
#[inline]
fn spread16(v: u32) -> u32 {
    let mut v = v & 0xffff;
    v = (v | (v << 8)) & 0x00ff_00ff;
    v = (v | (v << 4)) & 0x0f0f_0f0f;
    v = (v | (v << 2)) & 0x3333_3333;
    v = (v | (v << 1)) & 0x5555_5555;
    v
}

#[inline]
fn compact16(v: u32) -> u32 {
    let mut v = v & 0x5555_5555;
    v = (v | (v >> 1)) & 0x3333_3333;
    v = (v | (v >> 2)) & 0x0f0f_0f0f;
    v = (v | (v >> 4)) & 0x00ff_00ff;
    v = (v | (v >> 8)) & 0x0000_ffff;
    v
}

/// Morton code of 16 bit cell coordinates; latitude occupies the higher bit of
/// each pair. Equals the C++ `ll_upper_` quad-tile index.
#[inline]
pub fn morton(cy: u32, cx: u32) -> u32 {
    (spread16(cy) << 1) | spread16(cx)
}

#[inline]
pub fn unmorton(code: u32) -> (u32, u32) {
    (compact16(code >> 1), compact16(code))
}

/// The level-16 quad-tile of a point (same value as C++ `ll_upper_`).
#[inline]
pub fn tile_of(p: Pt) -> u32 {
    morton(p.y >> 16, p.x >> 16)
}

/// A spatial cell of the loose quadtree. `code` is the level-16 Morton code of
/// the cell's south-west tile; lower bits below the level are zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cell {
    pub level: u8,
    pub code: u32,
}

impl Cell {
    /// Cell used for elements without any geometry.
    pub const NONE: Cell = Cell { level: 255, code: 0 };

    pub fn tile(tile: u32) -> Cell {
        Cell { level: MAX_LEVEL, code: tile }
    }

    pub fn to_bytes(self) -> [u8; 5] {
        let c = self.code.to_be_bytes();
        [self.level, c[0], c[1], c[2], c[3]]
    }

    pub fn from_bytes(b: &[u8]) -> Cell {
        Cell { level: b[0], code: u32::from_be_bytes([b[1], b[2], b[3], b[4]]) }
    }

    /// Key space rectangle this element may extend into: the anchor cell plus
    /// one cell to the north and east.
    pub fn loose_rect(self) -> Rect {
        if self.level == 255 {
            return Rect::EMPTY;
        }
        if self.level == 0 {
            return Rect::WORLD;
        }
        let shift = 32 - self.level as u32;
        let (ty, tx) = unmorton(self.code);
        let y0 = (ty << 16) >> shift << shift;
        let x0 = (tx << 16) >> shift << shift;
        let size = 1u64 << shift;
        let y1 = ((y0 as u64 + 2 * size - 1).min(u32::MAX as u64)) as u32;
        let x1 = ((x0 as u64 + 2 * size - 1).min(u32::MAX as u64)) as u32;
        Rect { y0, x0, y1, x1 }
    }
}

/// Anchor cell for an element with the given bounding rectangle.
pub fn anchor_cell(r: &Rect) -> Cell {
    if r.is_empty() {
        return Cell::NONE;
    }
    let dy = (r.y1 - r.y0) as u64;
    let dx = (r.x1 - r.x0) as u64;
    let ext = dy.max(dx);
    let mut level = MAX_LEVEL;
    while level > 0 && (1u64 << (32 - level as u32)) < ext {
        level -= 1;
    }
    cell_at(level, Pt { y: r.y0, x: r.x0 })
}

pub fn cell_at(level: u8, p: Pt) -> Cell {
    if level == 0 {
        return Cell { level: 0, code: 0 };
    }
    let shift = 32 - level as u32;
    let cy = (p.y >> shift) << (16 - level as u32);
    let cx = (p.x >> shift) << (16 - level as u32);
    Cell { level, code: morton(cy, cx) }
}

/// Inclusive range of level-16 Morton codes within one level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeRange {
    pub level: u8,
    pub lo: u32,
    pub hi: u32,
}

const MAX_RANGES_PER_LEVEL: usize = 512;

/// Ranges of cells at `level` whose loose extent may intersect `r`.
pub fn level_ranges(level: u8, r: &Rect, loose: bool) -> Vec<CodeRange> {
    if r.is_empty() {
        return Vec::new();
    }
    if level == 0 {
        return vec![CodeRange { level: 0, lo: 0, hi: 0 }];
    }
    let shift = 32 - level as u32;
    let mut cy0 = r.y0 >> shift;
    let mut cx0 = r.x0 >> shift;
    if loose {
        cy0 = cy0.saturating_sub(1);
        cx0 = cx0.saturating_sub(1);
    }
    let cy1 = r.y1 >> shift;
    let cx1 = r.x1 >> shift;
    let mut out: Vec<(u32, u32)> = Vec::new();
    // Breadth-first quadtree decomposition in level-local coordinates.
    let lv = level as u32;
    let mut frontier: Vec<(u32, u32, u32)> = vec![(0, 0, 0)]; // depth, cy, cx
    while let Some(&(depth, _, _)) = frontier.first() {
        let mut next = Vec::new();
        let coarse = frontier.len() > MAX_RANGES_PER_LEVEL;
        for (d, cy, cx) in frontier.drain(..) {
            debug_assert_eq!(d, depth);
            let span = lv - d;
            let ny0 = cy << span;
            let nx0 = cx << span;
            let ny1 = ((cy + 1) << span) - 1;
            let nx1 = ((cx + 1) << span) - 1;
            if ny0 > cy1 || ny1 < cy0 || nx0 > cx1 || nx1 < cx0 {
                continue;
            }
            let full = ny0 >= cy0 && ny1 <= cy1 && nx0 >= cx0 && nx1 <= cx1;
            if full || d == lv || coarse {
                let base = morton(cy << (16 - d), cx << (16 - d));
                let width = if d == 0 { u32::MAX } else { (1u32 << (2 * (16 - d))) - 1 };
                out.push((base, base + width));
            } else {
                for (sy, sx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    next.push((d + 1, cy * 2 + sy, cx * 2 + sx));
                }
            }
        }
        frontier = next;
    }
    out.sort_unstable();
    let mut merged: Vec<CodeRange> = Vec::new();
    for (lo, hi) in out {
        if let Some(last) = merged.last_mut() {
            if last.hi != u32::MAX && lo <= last.hi + 1 {
                last.hi = last.hi.max(hi);
                continue;
            }
        }
        merged.push(CodeRange { level, lo, hi });
    }
    merged
}

/// `level_ranges` of many rectangles in one walk of the quadtree: a node
/// is taken whole once one rectangle covers it, or up to 8 do together
/// (rectangles that meet in a seam), and dropped once none meets it.
/// The cost follows the edges of the union, not of each rectangle: the
/// 3 x 10^5 tiles of a large country share most of their edges. The cells
/// are those whose (loose) extent meets a rectangle, as with `level_ranges`
/// of each one; nodes are taken whole when a depth has more than 512 for
/// each rectangle.
pub fn level_ranges_many(level: u8, rects: &[Rect], loose: bool) -> Vec<CodeRange> {
    if rects.iter().all(|r| r.is_empty()) {
        return Vec::new();
    }
    if level == 0 {
        return vec![CodeRange { level: 0, lo: 0, hi: 0 }];
    }
    let shift = 32 - level as u32;
    let lv = level as u32;
    // The rectangles in cells of this level, [y0, x0, y1, x1].
    let mut cells: Vec<[u32; 4]> = rects
        .iter()
        .filter(|r| !r.is_empty())
        .map(|r| {
            let (mut y0, mut x0) = (r.y0 >> shift, r.x0 >> shift);
            if loose {
                y0 = y0.saturating_sub(1);
                x0 = x0.saturating_sub(1);
            }
            [y0, x0, r.y1 >> shift, r.x1 >> shift]
        })
        .collect();
    cells.sort_unstable();
    cells.dedup();
    let budget = MAX_RANGES_PER_LEVEL.saturating_mul(cells.len());
    let meets = |c: &[u32; 4], n: &[u32; 4]| c[0] <= n[2] && n[0] <= c[2] && c[1] <= n[3] && n[1] <= c[3];
    let holds = |c: &[u32; 4], n: &[u32; 4]| c[0] <= n[0] && n[2] <= c[2] && c[1] <= n[1] && n[3] <= c[3];
    let mut out: Vec<(u32, u32)> = Vec::new();
    // Nodes of one depth: (cy, cx) and their rectangles in `idx[a..b]`.
    let mut idx: Vec<u32> = (0..cells.len() as u32).collect();
    let mut frontier: Vec<(u32, u32, usize, usize)> = vec![(0, 0, 0, idx.len())];
    for d in 0..=lv {
        let span = lv - d;
        let coarse = frontier.len() > budget;
        let mut next = Vec::new();
        let mut next_idx: Vec<u32> = Vec::new();
        for &(cy, cx, a, b) in &frontier {
            let node = [cy << span, cx << span, ((cy + 1) << span) - 1, ((cx + 1) << span) - 1];
            let list = &idx[a..b];
            let full = d == lv
                || coarse
                || list.iter().any(|&i| holds(&cells[i as usize], &node))
                || (list.len() <= 8 && union_holds(&node, list.iter().map(|&i| &cells[i as usize])));
            if full {
                let base = morton(cy << (16 - d), cx << (16 - d));
                let width = if d == 0 { u32::MAX } else { (1u32 << (2 * (16 - d))) - 1 };
                out.push((base, base + width));
                continue;
            }
            for (sy, sx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let (ny, nx) = (cy * 2 + sy, cx * 2 + sx);
                let child = [ny << (span - 1), nx << (span - 1), ((ny + 1) << (span - 1)) - 1, ((nx + 1) << (span - 1)) - 1];
                let start = next_idx.len();
                next_idx.extend(list.iter().copied().filter(|&i| meets(&cells[i as usize], &child)));
                if next_idx.len() > start {
                    next.push((ny, nx, start, next_idx.len()));
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
        idx = next_idx;
    }
    out.sort_unstable();
    let mut merged: Vec<CodeRange> = Vec::new();
    for (lo, hi) in out {
        if let Some(last) = merged.last_mut() {
            if last.hi != u32::MAX && lo <= last.hi + 1 {
                last.hi = last.hi.max(hi);
                continue;
            }
        }
        merged.push(CodeRange { level, lo, hi });
    }
    merged
}

/// Whether the rectangles (at most 8, each meeting `node`) cover `node`
/// together: every piece of the grid their edges cut it into is covered.
fn union_holds<'a>(node: &[u32; 4], rs: impl Iterator<Item = &'a [u32; 4]> + Clone) -> bool {
    let mut ys = [0u32; 17];
    let mut xs = [0u32; 17];
    let (mut ny, mut nx) = (1, 1);
    ys[0] = node[0];
    xs[0] = node[1];
    for c in rs.clone() {
        if c[0] > node[0] {
            ys[ny] = c[0];
            ny += 1;
        }
        if c[2] < node[2] {
            ys[ny] = c[2] + 1;
            ny += 1;
        }
        if c[1] > node[1] {
            xs[nx] = c[1];
            nx += 1;
        }
        if c[3] < node[3] {
            xs[nx] = c[3] + 1;
            nx += 1;
        }
    }
    let (ys, xs) = (&mut ys[..ny], &mut xs[..nx]);
    ys.sort_unstable();
    xs.sort_unstable();
    ys.iter().all(|&y| xs.iter().all(|&x| rs.clone().any(|c| c[0] <= y && y <= c[2] && c[1] <= x && x <= c[3])))
}

/// Sort ranges by level and start and merge overlapping or adjacent ones.
pub fn merge_ranges(mut r: Vec<CodeRange>) -> Vec<CodeRange> {
    r.sort_by_key(|c| (c.level, c.lo));
    let mut out: Vec<CodeRange> = Vec::with_capacity(r.len());
    for c in r {
        if let Some(l) = out.last_mut() {
            if l.level == c.level && c.lo <= l.hi.saturating_add(1) {
                l.hi = l.hi.max(c.hi);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Whether a cell lies in one of the ranges (sorted by level and start).
pub fn cell_in_ranges(cell: Cell, ranges: &[CodeRange]) -> bool {
    let i = ranges.partition_point(|r| (r.level, r.hi) < (cell.level, cell.code));
    i < ranges.len() && ranges[i].level == cell.level && ranges[i].lo <= cell.code && cell.code <= ranges[i].hi
}

/// Ranges over all levels for elements stored in the loose quadtree.
pub fn loose_ranges(r: &Rect) -> Vec<CodeRange> {
    let mut out = Vec::new();
    for level in 0..=MAX_LEVEL {
        out.extend(level_ranges(level, r, true));
    }
    out
}

/// Ranges of level-16 tiles intersecting `r` (for nodes).
pub fn tile_ranges(r: &Rect) -> Vec<CodeRange> {
    level_ranges(MAX_LEVEL, r, false)
}

// ---------------------------------------------------------------------------
// The C++ way/relation index (`Uint31_Index`), kept only to reproduce the
// C++ iteration order of `foreach`, `convert` and `out qt`.

fn upper_ilat(q: u32) -> u32 {
    compact16(q >> 1)
}

fn upper_ilon(q: u32) -> u32 {
    compact16(q)
}

/// C++ `ll_upper` (without the sign flip) for 16 bit tile coordinates
/// already shifted into the upper half.
fn ll_upper_plain(ilat: u32, ilon: u32) -> u32 {
    (spread16(ilat >> 16) << 1) | spread16(ilon >> 16)
}

/// Levels of a compound C++ index: (flag bit, latitude mask, longitude mask,
/// cell size in tiles - 1).
const CPP_LEVELS: [(u32, u32, u32, u32); 7] = [
    (0x01, 0x2aaaaaa8, 0x55555554, 3),
    (0x02, 0x2aaaaa80, 0x55555540, 0xf),
    (0x04, 0x2aaaa800, 0x55555400, 0x3f),
    (0x08, 0x2aaa8000, 0x55554000, 0xff),
    (0x10, 0x2aa80000, 0x55540000, 0x3ff),
    (0x20, 0x2a800000, 0x55400000, 0xfff),
    (0x40, 0x28000000, 0x54000000, 0x3fff),
];

/// Level-16 tiles a C++ `Uint31_Index` covers: (lat0, lon0, lat1, lon1),
/// None for the index of the whole world (and unknown levels).
fn cpp_index_tiles(idx: u32) -> Option<(u32, u32, u32, u32)> {
    if idx & 0x8000_0000 == 0 {
        let (y, x) = unmorton(idx);
        return Some((y, x, y, x));
    }
    for (bit, lm, om, add) in CPP_LEVELS {
        if idx & bit != 0 {
            let (y, x) = (upper_ilat(idx & lm), upper_ilon(idx & om));
            return Some((y, x, y + add, x + add));
        }
    }
    None
}

/// Whether C++ reads a way or relation with this index for a bounding box:
/// its cell meets the level-16 tiles of the box (`get_ranges_31` with the
/// parent indexes). Used for the members of `out geom(s,w,n,e)`.
pub fn cpp_index_meets(idx: u32, b: &BBox) -> bool {
    let Some((y0, x0, y1, x1)) = cpp_index_tiles(idx) else { return true };
    let (by0, by1) = (ilat(b.south) >> 16, ilat(b.north) >> 16);
    let (bx0, bx1) = (x_of_ilon(ilon(b.west)) >> 16, x_of_ilon(ilon(b.east)) >> 16);
    let lon = if bx0 <= bx1 { x0 <= bx1 && x1 >= bx0 } else { x1 >= bx0 || x0 <= bx1 };
    y0 <= by1 && y1 >= by0 && lon
}

/// Port of C++ `calc_index` (index_computations.h).
pub fn cpp_calc_index(idxs: &[u32]) -> u32 {
    let Some(&first) = idxs.first() else { return 0xfe };
    // Extent of a compound index: (lat_min, lon_min, lat_max, lon_max).
    fn compound(v: u32) -> Option<(u32, u32, u32, u32)> {
        for (bit, lm, om, add) in CPP_LEVELS {
            if v & bit != 0 {
                let lat = v & lm;
                let lon = v & om;
                return Some((lat, lon, ll_upper_plain((upper_ilat(lat) + add) << 16, 0), ll_upper_plain(0, (upper_ilon(lon) + add) << 16)));
            }
        }
        None
    }
    let (mut lat_min, mut lon_min, mut lat_max, mut lon_max);
    if first & 0x8000_0000 != 0 {
        match compound(first) {
            Some((a, b, c, d)) => {
                lat_min = a;
                lon_min = b;
                lat_max = c;
                lon_max = d;
            }
            None => return 0x8000_0080,
        }
    } else {
        lat_min = first & 0x2aaa_aaaa;
        lat_max = lat_min;
        lon_min = first & 0x5555_5555;
        lon_max = lon_min;
    }
    for &v in &idxs[1..] {
        if v & 0x8000_0000 != 0 {
            let Some((lat, lon, lat_u, lon_u)) = compound(v) else { return 0x8000_0080 };
            lat_min = lat_min.min(lat);
            lat_max = lat_max.max(lat_u);
            lon_min = lon_min.min(lon);
            lon_max = lon_max.max(lon_u);
        } else {
            let lat = v & 0x2aaa_aaaa;
            let lon = v & 0x5555_5555;
            if lat < lat_min {
                lat_min = lat;
            } else if lat > lat_max {
                lat_max = lat;
            }
            if lon < lon_min {
                lon_min = lon;
            } else if lon > lon_max {
                lon_max = lon;
            }
        }
    }
    if lat_max == lat_min && lon_max == lon_min {
        return first;
    }
    let (ilat_min, ilat_max) = (upper_ilat(lat_min), upper_ilat(lat_max));
    let (ilon_min, ilon_max) = (upper_ilon(lon_min), upper_ilon(lon_max));
    let fits = |m: u32, d: u32| (ilat_max & m).wrapping_sub(ilat_min & m) < d && (ilon_max & m).wrapping_sub(ilon_min & m) < d;
    let sw = lon_min | lat_min;
    if fits(0xfffe, 4) {
        return (sw & 0xffff_fffc) | 0x8000_0001;
    }
    if fits(0xfff8, 0x10) {
        return (sw & 0xffff_ffc0) | 0x8000_0002;
    }
    if fits(0xffe0, 0x40) {
        return (sw & 0xffff_fc00) | 0x8000_0004;
    }
    if fits(0xff80, 0x100) {
        return (sw & 0xffff_c000) | 0x8000_0008;
    }
    if fits(0xfe00, 0x400) {
        return (sw & 0xfffc_0000) | 0x8000_0010;
    }
    if fits(0xf800, 0x1000) {
        return (sw & 0xffc0_0000) | 0x8000_0020;
    }
    if fits(0xe000, 0x4000) {
        return (sw & 0xfc00_0000) | 0x8000_0040;
    }
    0x8000_0080
}

/// Sort key of a C++ `Uint31_Index`.
pub fn cpp_index_order(idx: u32) -> (u32, u32) {
    (idx & 0x7fff_ffff, idx)
}

// ---------------------------------------------------------------------------
// Spherical geometry, mirroring the C++ formulas.

pub fn great_circle_dist(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    if lat1 == lat2 && lon1 == lon2 {
        return 0.0;
    }
    let mut sp = ((lat2 - lat1) * DEG_TO_ARC).cos()
        + (lat1 * DEG_TO_ARC).cos() * (lat2 * DEG_TO_ARC).cos() * (((lon2 - lon1) * DEG_TO_ARC).cos() - 1.0);
    if sp > 1.0 {
        sp = 1.0;
    }
    sp.acos() * ARC_TO_METER
}

#[derive(Clone, Copy, Debug)]
pub struct V3(pub f64, pub f64, pub f64);

impl V3 {
    pub fn dot(&self, o: &V3) -> f64 {
        self.0 * o.0 + self.1 * o.1 + self.2 * o.2
    }
    pub fn cross(&self, o: &V3) -> V3 {
        V3(self.1 * o.2 - self.2 * o.1, self.2 * o.0 - self.0 * o.2, self.0 * o.1 - self.1 * o.0)
    }
    pub fn add(&self, o: &V3) -> V3 {
        V3(self.0 + o.0, self.1 + o.1, self.2 + o.2)
    }
    pub fn scale(&self, a: f64) -> V3 {
        V3(self.0 * a, self.1 * a, self.2 * a)
    }
}

pub fn cartesian(lat: f64, lon: f64) -> V3 {
    let la = lat * DEG_TO_ARC;
    let lo = lon * DEG_TO_ARC;
    V3(la.sin(), la.cos() * lo.sin(), la.cos() * lo.cos())
}

/// Segment prepared for repeated distance and intersection tests.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub a: (f64, f64),
    pub b: (f64, f64),
    pub ca: V3,
    pub cb: V3,
    pub norm: V3,
}

impl Segment {
    pub fn new(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> Segment {
        let ca = cartesian(lat1, lon1);
        let cb = cartesian(lat2, lon2);
        Segment { a: (lat1, lon1), b: (lat2, lon2), ca, cb, norm: ca.cross(&cb) }
    }

    /// Distance from a point to the great circle through the segment.
    pub fn line_dist(&self, p: &V3) -> f64 {
        let nn = self.norm.dot(&self.norm);
        if nn == 0.0 {
            return f64::INFINITY;
        }
        let mut sp = p.dot(&self.norm).abs() / nn.sqrt();
        if sp > 1.0 {
            sp = 1.0;
        }
        sp.asin() * ARC_TO_METER
    }

    /// Whether the foot point of `p` on the great circle lies within the segment.
    pub fn foot_within(&self, p: &V3) -> bool {
        // p projected onto the segment plane lies between a and b iff it is on
        // the same side of the planes through (norm, a) and (norm, b).
        let sa = self.norm.cross(&self.ca).dot(p);
        let sb = self.cb.cross(&self.norm).dot(p);
        sa >= 0.0 && sb >= 0.0
    }

    /// Shortest distance from a point to the segment.
    pub fn dist(&self, lat: f64, lon: f64) -> f64 {
        let p = cartesian(lat, lon);
        let d_end = great_circle_dist(lat, lon, self.a.0, self.a.1).min(great_circle_dist(lat, lon, self.b.0, self.b.1));
        if self.norm.dot(&self.norm) > 0.0 && self.foot_within(&p) {
            d_end.min(self.line_dist(&p))
        } else {
            d_end
        }
    }

    /// Great-circle segment intersection test (C++ `intersect`).
    pub fn intersects(&self, o: &Segment) -> bool {
        let ip = self.norm.cross(&o.norm);
        let n = ip.dot(&ip).sqrt();
        if n == 0.0 || !n.is_finite() {
            return false;
        }
        let ip = ip.scale(1.0 / n);
        let asum = self.ca.add(&self.cb);
        let bsum = o.ca.add(&o.cb);
        asum.dot(&ip).abs() >= asum.dot(&self.ca) && bsum.dot(&ip).abs() >= bsum.dot(&o.ca)
    }
}

/// Whether `bbox_intersects` takes the segment across the antimeridian:
/// its ends lie more than 180 degrees of longitude apart.
pub fn bbox_across(from: (f64, f64), to: (f64, f64)) -> bool {
    (from.1 < 0.0 && to.1 - from.1 > 180.0) || (to.1 < 0.0 && from.1 - to.1 > 180.0)
}

/// C++ `Bbox_Double::intersects` for a segment, on unrounded doubles.
/// Segments spanning more than 180 degrees of longitude are taken across
/// the antimeridian (`bbox_across`); such a segment meets a box that does
/// not cross the antimeridian only when the box holds the longitudes of
/// both ends.
pub fn bbox_intersects(b: &BBox, from: (f64, f64), to: (f64, f64)) -> bool {
    let (s, n, w, e) = (b.south, b.north, b.west, b.east);
    let mut fl = from.1;
    let mut tl = to.1;
    if from.1 < 0.0 && to.1 - from.1 > 180.0 {
        fl += 360.0;
    }
    if to.1 < 0.0 && from.1 - to.1 > 180.0 {
        tl += 360.0;
    }
    let mut d = 0.0;
    if from.0 < s {
        if to.0 < s {
            return false;
        }
        d = (tl - fl) * (s - from.0) / (to.0 - from.0);
    } else if from.0 > n {
        if to.0 > n {
            return false;
        }
        d = (tl - fl) * (n - from.0) / (to.0 - from.0);
    }
    if to.0 < s {
        tl += (fl - tl) * (s - to.0) / (from.0 - to.0);
    } else if to.0 > n {
        tl += (fl - tl) * (n - to.0) / (from.0 - to.0);
    }
    fl += d;
    let lo = fl.min(tl);
    let hi = fl.max(tl);
    if w <= e {
        if hi < 180.0 {
            return lo <= e && hi >= w;
        }
        if lo > 180.0 {
            return lo - 360.0 <= e && hi - 360.0 >= w;
        }
        return lo <= e && hi - 360.0 >= w;
    }
    if hi < 180.0 {
        return lo <= e || hi >= w;
    }
    if lo > 180.0 {
        return lo - 360.0 <= e || hi - 360.0 >= w;
    }
    true
}

impl Segment {
    /// Latitude range of the great-circle arc, including its apex.
    pub fn lat_extent(&self) -> (f64, f64) {
        let mut lo = self.a.0.min(self.b.0);
        let mut hi = self.a.0.max(self.b.0);
        let nn = self.norm.dot(&self.norm);
        if nn > 0.0 {
            let n = self.norm.scale(1.0 / nn.sqrt());
            // Northernmost point of the great circle: the north axis minus its
            // component along the normal.
            let top = V3(1.0 - n.0 * n.0, -n.0 * n.1, -n.0 * n.2);
            let tl = top.dot(&top).sqrt();
            if tl > 0.0 {
                let top = top.scale(1.0 / tl);
                for (p, sign) in [(top, 1.0), (top.scale(-1.0), -1.0)] {
                    if self.foot_within(&p) {
                        let lat = p.0.clamp(-1.0, 1.0).asin() * 180.0 / PI;
                        if sign > 0.0 {
                            hi = hi.max(lat);
                        } else {
                            lo = lo.min(lat);
                        }
                    }
                }
            }
        }
        (lo, hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantization_roundtrip() {
        for (lat, lon) in [(51.25, 7.15), (-89.9999999, -179.9999999), (0.0, 0.0), (45.1234567, -12.7654321)] {
            let p = Pt::from_ll(lat, lon);
            assert!((p.lat() - lat).abs() < 1e-7);
            assert!((p.lon() - lon).abs() < 1e-7);
        }
    }

    // A way index meets a box when its cell shares a level-16 tile with it.
    #[test]
    fn cpp_index_meets_box() {
        let bb = |s, w, n, e| BBox { south: s, west: w, north: n, east: e };
        let p = Pt::from_ll(48.68, 6.18);
        let tile = tile_of(p); // a way within one tile has the tile as its index
        assert_eq!(cpp_calc_index(&[tile]), tile);
        assert!(cpp_index_meets(tile, &bb(48.679, 6.179, 48.681, 6.181)));
        assert!(!cpp_index_meets(tile, &bb(48.70, 6.20, 48.71, 6.21)));
        // a way over four tiles: a compound index, its cell is larger than the way
        let far = Pt::from_ll(48.68 + 0.003, 6.18 + 0.006);
        let idx = cpp_calc_index(&[tile, tile_of(far)]);
        assert!(idx & 0x8000_0000 != 0);
        let (y0, x0, y1, x1) = cpp_index_tiles(idx).unwrap();
        assert!(y0 <= p.y >> 16 && far.y >> 16 <= y1 && x0 <= p.x >> 16 && far.x >> 16 <= x1);
        assert!(cpp_index_meets(idx, &bb(48.6835, 6.1865, 48.6836, 6.1866)));
        assert!(!cpp_index_meets(idx, &bb(10.0, 10.0, 10.1, 10.1)));
        // the whole world, and a box across the antimeridian
        assert!(cpp_index_meets(0x8000_0080, &bb(-10.0, -10.0, -9.0, -9.0)));
        let east = tile_of(Pt::from_ll(0.0, 179.99));
        assert!(cpp_index_meets(east, &bb(-1.0, 179.0, 1.0, -179.0)));
        assert!(!cpp_index_meets(east, &bb(-1.0, -179.0, 1.0, 179.0 - 1.0)));
    }

    #[test]
    fn tile_matches_cpp_ll_upper() {
        // Reference: C++ ll_upper_(ilat(51.25), ilon(7.15)).
        fn cpp_ll_upper(mut ilat: u32, mut ilon: u32) -> u32 {
            ilat &= 0xffff0000;
            ilat |= ilat >> 8;
            ilat &= 0xff00ff00;
            ilat |= ilat >> 4;
            ilat &= 0xf0f0f0f0;
            ilat |= ilat >> 2;
            ilat &= 0xcccccccc;
            ilat |= ilat >> 1;
            ilat &= 0xaaaaaaaa;
            ilon &= 0xffff0000;
            ilon |= ilon >> 8;
            ilon &= 0xff00ff00;
            ilon |= ilon >> 4;
            ilon &= 0xf0f0f0f0;
            ilon |= ilon >> 2;
            ilon &= 0xcccccccc;
            ilon |= ilon >> 1;
            ilon &= 0xaaaaaaaa;
            (ilat | (ilon >> 1)) ^ 0x40000000
        }
        for (lat, lon) in [(51.25, 7.15), (-33.0, -70.5), (0.0, 0.0), (89.0, 179.9)] {
            let p = Pt::from_ll(lat, lon);
            assert_eq!(tile_of(p), cpp_ll_upper(ilat(lat), ilon(lon) as u32));
        }
    }

    #[test]
    fn morton_roundtrip() {
        for (a, b) in [(0u32, 0u32), (1, 2), (0xffff, 0x1234), (0x8000, 0x7fff)] {
            assert_eq!(unmorton(morton(a, b)), (a, b));
        }
    }

    #[test]
    fn anchor_cell_contains_rect() {
        let r = Rect { y0: 1_000_000_000, x0: 2_000_000_000, y1: 1_000_300_000, x1: 2_000_100_000 };
        let c = anchor_cell(&r);
        let lr = c.loose_rect();
        assert!(lr.y0 <= r.y0 && lr.x0 <= r.x0 && lr.y1 >= r.y1 && lr.x1 >= r.x1);
        // and the query ranges for r find the anchor
        let ranges = loose_ranges(&r);
        assert!(ranges.iter().any(|cr| cr.level == c.level && cr.lo <= c.code && c.code <= cr.hi));
    }

    #[test]
    fn ranges_cover_small_rect() {
        let p = Pt::from_ll(51.25, 7.15);
        let r = Rect::point(p);
        let rs = tile_ranges(&r);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].lo, tile_of(p));
        assert_eq!(rs[0].hi, tile_of(p));
    }

    #[test]
    fn distance_sane() {
        let d = great_circle_dist(0.0, 0.0, 0.0, 1.0);
        assert!((d - 111_111.1).abs() < 1.0, "{d}");
    }

    /// The great-circle arcs between points at most a cell of a level apart
    /// in longitude stay within `arc_bulge` of the latitudes of their ends,
    /// and ends at the latitude where the bulge is largest reach it.
    #[test]
    fn arc_bulge_bounds_great_circles() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        for level in 2..=MAX_LEVEL {
            let width = 1u64 << (32 - level as u32);
            let bulge = arc_bulge(width) as f64 / 1e7;
            for _ in 0..20_000 {
                let d = next() * (width as f64 / 1e7).min(179.999);
                // ends at about the same latitude half the time: an arc
                // between them reaches past them
                let la = next() * 170.0 - 85.0;
                let lb = if next() < 0.5 { la + (next() - 0.5) * d * 0.01 } else { next() * 170.0 - 85.0 };
                let lo = next() * (360.0 - d) - 180.0;
                let (s, n) = Segment::new(la, lo, lb, lo + d).lat_extent();
                let out = (la.min(lb) - s).max(n - la.max(lb));
                assert!(out <= bulge, "level {level}: {la} {lb} {d}: {out} > {bulge}");
            }
            // the largest bulge: ends `width` apart at tan(p) = sqrt(c), north and south
            let d = (width as f64 / 1e7).min(179.999);
            let p = (d / 2.0).to_radians().cos().sqrt().atan().to_degrees();
            for p in [p, -p] {
                let (s, n) = Segment::new(p, -d / 2.0, p, d / 2.0).lat_extent();
                let out = (p - s).max(n - p);
                assert!(out <= bulge && out >= bulge - 3e-7, "level {level}: {p} {d}: {out} {bulge}");
            }
        }
        assert_eq!(arc_bulge(0), 2);
        assert_eq!(arc_bulge(1 << 31), u32::MAX);
        assert_eq!(arc_bulge(1_800_000_000), u32::MAX);
    }

    /// `arc_box` holds the great-circle edges between the corners of a box,
    /// and empty boxes stay empty.
    #[test]
    fn arc_box_holds_the_edges() {
        for (la, lb, lo, d) in [(60.0, 60.0, 0.0, 20.0), (-60.0, -60.0, -20.0, 20.0), (48.4124, 48.4124, -7.65, 20.0), (10.0, 30.0, 100.0, 179.0), (0.0, 0.0, 5.0, 90.0), (45.0, 45.0, 1.0, 0.001)] {
            let (a, b) = (Pt::from_ll(la, lo), Pt::from_ll(lb, lo + d));
            let mut r = Rect::point(a);
            r.extend(b);
            let (s, n) = Segment::new(a.lat(), a.lon(), b.lat(), b.lon()).lat_extent();
            let arc = r.arc_box();
            assert!(arc.y0 as f64 <= (s + 91.0) * 1e7 && (n + 91.0) * 1e7 <= arc.y1 as f64, "{la} {lb} {lo} {d}");
            assert_eq!((arc.x0, arc.x1), (r.x0, r.x1));
        }
        assert!(Rect::EMPTY.arc_box().is_empty());
        assert!(Rect::EMPTY.grow_lat(5).is_empty());
    }
}
