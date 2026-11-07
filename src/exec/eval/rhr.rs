// Contains code translated from C++ to Rust in 2026 from Overpass API,
// Copyright 2008-2018 Roland Olbricht et al. Modified by Chavdar Dutsov.
// GNU Affero General Public License, version 3 or later. See LICENSE and NOTICE.

//! Right-hand-rule polygon normalisation used by `poly()` and `hull()`
//! (docs/spec/evaluators.md §10.4).
//!
//! Faithful port of the C++ `RHR_Polygon_Geometry(const Free_Polygon_Geometry&)`
//! constructor (`core/geometry.cc:635-1585`) and of the `Four_Field_Index`
//! (`core/four_field_index.cc`) it uses for the inside test. Arithmetic keeps
//! the C++ operation order because the output is compared bytewise after
//! `%.7f` formatting; integer conversions mimic what gcc emits on x86-64.
//! Where the C++ code has undefined behaviour (out-of-bounds reads, endless
//! loops) this port stops early instead.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};

/// Point as (lat, lon), compared exactly like the C++ `Point_Double`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct P {
    lat: f64,
    lon: f64,
}

impl P {
    fn new(lat: f64, lon: f64) -> P {
        P { lat, lon }
    }

    fn eps_eq(&self, o: &P) -> bool {
        (self.lat - o.lat).abs() < 1e-7 && (self.lon - o.lon).abs() < 1e-7
    }
}

const MASK: u32 = 0xffff_0000;

/// `acos(0)/90.` (the double nearest pi/2, divided by 90).
fn deg_to_arc() -> f64 {
    std::f64::consts::FRAC_PI_2 / 90.0
}

/// `std::min` / `std::max` (NaN behaviour differs from `f64::min`/`max`).
fn cmin(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

fn cmax(a: f64, b: f64) -> f64 {
    if a < b { b } else { a }
}

/// double -> int32 as `cvttsd2si`: out of range and NaN give INT_MIN.
fn to_i32(x: f64) -> i32 {
    if x > -2147483649.0 && x < 2147483648.0 { x as i32 } else { i32::MIN }
}

/// double -> uint32 as gcc emits it (64-bit `cvttsd2si`, low 32 bits).
fn to_u32(x: f64) -> u32 {
    if x > -9.2e18 && x < 9.2e18 { (x as i64) as u32 } else { 0 }
}

fn ilat(lat: f64) -> u32 {
    to_u32((lat + 91.0) * 10000000.0 + 0.5)
}

fn ilon(lon: f64) -> i32 {
    to_i32(lon * 10000000.0 + if lon > 0.0 { 0.5 } else { -0.5 })
}

fn lat_of(ilat: u32) -> f64 {
    (ilat as f64) / 10000000.0 - 91.0
}

fn lon_of(ilon: i32) -> f64 {
    (ilon as f64) / 10000000.0
}

/// Index cell of a point: upper 16 bits of ilat and of ilon.
fn cell(p: &P) -> u32 {
    (ilat(p.lat) & MASK) | ((ilon(p.lon) as u32) >> 16)
}

// ---------------------------------------------------------------------------
// libstdc++ `std::sort` (introsort + final insertion sort). The order of
// elements that compare equal must match the C++ binary.

const S_THRESHOLD: usize = 16;

fn cpp_sort<T: Copy, F: Fn(&T, &T) -> bool>(v: &mut [T], lt: &F) {
    let n = v.len();
    if n == 0 {
        return;
    }
    let lg = (usize::BITS - 1 - n.leading_zeros()) as usize;
    introsort_loop(v, 0, n, lg * 2, lt);
    final_insertion_sort(v, 0, n, lt);
}

fn introsort_loop<T: Copy, F: Fn(&T, &T) -> bool>(v: &mut [T], first: usize, mut last: usize, mut depth: usize, lt: &F) {
    while last - first > S_THRESHOLD {
        if depth == 0 {
            make_heap(v, first, last, lt);
            sort_heap(v, first, last, lt);
            return;
        }
        depth -= 1;
        let cut = unguarded_partition_pivot(v, first, last, lt);
        introsort_loop(v, cut, last, depth, lt);
        last = cut;
    }
}

fn unguarded_partition_pivot<T: Copy, F: Fn(&T, &T) -> bool>(v: &mut [T], first: usize, last: usize, lt: &F) -> usize {
    let mid = first + (last - first) / 2;
    move_median_to_first(v, first, first + 1, mid, last - 1, lt);
    unguarded_partition(v, first + 1, last, first, lt)
}

fn move_median_to_first<T: Copy, F: Fn(&T, &T) -> bool>(v: &mut [T], result: usize, a: usize, b: usize, c: usize, lt: &F) {
    if lt(&v[a], &v[b]) {
        if lt(&v[b], &v[c]) {
            v.swap(result, b);
        } else if lt(&v[a], &v[c]) {
            v.swap(result, c);
        } else {
            v.swap(result, a);
        }
    } else if lt(&v[a], &v[c]) {
        v.swap(result, a);
    } else if lt(&v[b], &v[c]) {
        v.swap(result, c);
    } else {
        v.swap(result, b);
    }
}

fn unguarded_partition<T: Copy, F: Fn(&T, &T) -> bool>(
    v: &mut [T],
    mut first: usize,
    mut last: usize,
    pivot: usize,
    lt: &F,
) -> usize {
    let end = last;
    loop {
        while first < end && lt(&v[first], &v[pivot]) {
            first += 1;
        }
        last -= 1;
        while last > 0 && lt(&v[pivot], &v[last]) {
            last -= 1;
        }
        if first >= last {
            return first;
        }
        v.swap(first, last);
        first += 1;
    }
}

fn final_insertion_sort<T: Copy, F: Fn(&T, &T) -> bool>(v: &mut [T], first: usize, last: usize, lt: &F) {
    if last - first > S_THRESHOLD {
        insertion_sort(v, first, first + S_THRESHOLD, lt);
        for i in first + S_THRESHOLD..last {
            unguarded_linear_insert(v, i, lt);
        }
    } else {
        insertion_sort(v, first, last, lt);
    }
}

fn insertion_sort<T: Copy, F: Fn(&T, &T) -> bool>(v: &mut [T], first: usize, last: usize, lt: &F) {
    if first == last {
        return;
    }
    for i in first + 1..last {
        if lt(&v[i], &v[first]) {
            let val = v[i];
            v.copy_within(first..i, first + 1);
            v[first] = val;
        } else {
            unguarded_linear_insert(v, i, lt);
        }
    }
}

fn unguarded_linear_insert<T: Copy, F: Fn(&T, &T) -> bool>(v: &mut [T], mut last: usize, lt: &F) {
    let val = v[last];
    while last > 0 && lt(&val, &v[last - 1]) {
        v[last] = v[last - 1];
        last -= 1;
    }
    v[last] = val;
}

fn adjust_heap<T: Copy, F: Fn(&T, &T) -> bool>(v: &mut [T], first: usize, mut hole: usize, len: usize, value: T, lt: &F) {
    let top = hole;
    let mut second = hole;
    while second < (len - 1) / 2 {
        second = 2 * (second + 1);
        if lt(&v[first + second], &v[first + second - 1]) {
            second -= 1;
        }
        v[first + hole] = v[first + second];
        hole = second;
    }
    if len >= 2 && (len & 1) == 0 && second == (len - 2) / 2 {
        second = 2 * (second + 1);
        v[first + hole] = v[first + second - 1];
        hole = second - 1;
    }
    // __push_heap
    let mut parent = (hole as isize - 1) / 2;
    while hole > top && lt(&v[first + parent as usize], &value) {
        v[first + hole] = v[first + parent as usize];
        hole = parent as usize;
        parent = (hole as isize - 1) / 2;
    }
    v[first + hole] = value;
}

fn make_heap<T: Copy, F: Fn(&T, &T) -> bool>(v: &mut [T], first: usize, last: usize, lt: &F) {
    if last - first < 2 {
        return;
    }
    let len = last - first;
    let mut parent = (len - 2) / 2;
    loop {
        let value = v[first + parent];
        adjust_heap(v, first, parent, len, value, lt);
        if parent == 0 {
            return;
        }
        parent -= 1;
    }
}

fn sort_heap<T: Copy, F: Fn(&T, &T) -> bool>(v: &mut [T], first: usize, mut last: usize, lt: &F) {
    while last - first > 1 {
        last -= 1;
        let value = v[last];
        v[last] = v[first];
        adjust_heap(v, first, 0, last - first, value, lt);
    }
}

// ---------------------------------------------------------------------------
// Densification along great circles (`interpolation_point`,
// `Interpolation_Collector`, `interpolate_segment`).

fn interpolation_point(ox: f64, oy: f64, oz: f64, lgx: f64, lgz: f64, factor: f64, lhs_lon: f64) -> P {
    let d = deg_to_arc();
    let sin_factor = factor.sin();
    let cos_factor = factor.cos();
    let new_x = sin_factor * ox + cos_factor * lgx;
    let new_y = sin_factor * oy;
    let new_z = sin_factor * oz + cos_factor * lgz;

    let new_lat = new_x.asin() / d;
    let mut new_lon = new_y.atan2(new_z) / d + lhs_lon;
    if new_lon < -180.0 {
        new_lon += 360.0;
    } else if new_lon > 180.0 {
        new_lon -= 360.0;
    }
    P::new(new_lat, new_lon)
}

struct Collector<'a> {
    ox: f64,
    oy: f64,
    oz: f64,
    lgx: f64,
    lgz: f64,
    lhs_lon: f64,
    center: f64,
    ex_gc_pt_lat: f64,
    acceptable_max_length: f64,
    target: &'a mut Vec<P>,
}

impl<'a> Collector<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(ox: f64, oy: f64, oz: f64, lgx: f64, lgz: f64, dist: f64, lhs_lon: f64, target: &'a mut Vec<P>) -> Self {
        let d = deg_to_arc();
        let center = if lgx == 0.0 { dist } else { (ox / lgx).atan() };
        let ex_pt = interpolation_point(ox, oy, oz, lgx, lgz, cmax(cmin(center, dist), 0.0), lhs_lon);
        let mut ex_gc_pt_lat = ex_pt.lat;
        let acceptable_max_length = 0.0065536 * (ex_pt.lat * d).cos();

        let bounded_center = cmax(cmin(center, dist), 0.0);
        if acceptable_max_length < 0.0016384 && center != bounded_center {
            ex_gc_pt_lat = interpolation_point(ox, oy, oz, lgx, lgz, center, lhs_lon).lat;
        }
        Collector {
            ox,
            oy,
            oz,
            lgx,
            lgz,
            lhs_lon,
            center: bounded_center,
            ex_gc_pt_lat,
            acceptable_max_length,
            target,
        }
    }

    fn point(&self, dist: f64) -> P {
        interpolation_point(self.ox, self.oy, self.oz, self.lgx, self.lgz, dist, self.lhs_lon)
    }

    fn single_point(&mut self, dist: f64) {
        let p = self.point(dist);
        self.target.push(p);
    }

    fn sequence(&mut self, from: f64, to: f64, max_step: f64) {
        let num_sections = to_i32((to - from) / max_step).wrapping_add(1);
        for j in 1..num_sections {
            let p = self.point(from + (to - from) * (j as f64) / (num_sections as f64));
            self.target.push(p);
        }
    }

    fn collect_center(&mut self, from: f64, to: f64, divisor: i32) {
        let d = deg_to_arc();
        let max_length_threshold = 0.0065536 / (divisor as f64);
        if self.acceptable_max_length < max_length_threshold && divisor < 65536 {
            let dist_ex_s =
                ((1.0 - 1.0 / (divisor as f64) / (divisor as f64)).sqrt() / (self.ex_gc_pt_lat * d).sin()).acos();
            let bound_l = cmax(cmin(self.center - dist_ex_s, to), from);
            let bound_r = cmax(cmin(self.center + dist_ex_s, to), from);

            self.sequence(from, bound_l, max_length_threshold * d);
            if bound_l > 0.0 && bound_l < to {
                self.single_point(bound_l);
            }

            self.collect_center(bound_l, bound_r, divisor.wrapping_mul(4));

            if bound_r > 0.0 && bound_r < to {
                self.single_point(bound_r);
            }
            self.sequence(bound_r, to, max_length_threshold * d);
        } else {
            if self.acceptable_max_length < 1e-9 {
                self.acceptable_max_length = 1e-9;
            }
            self.acceptable_max_length *= d;

            let (center, step) = (self.center, self.acceptable_max_length);
            self.sequence(from, center, step);
            if center > from && center < to {
                self.single_point(center);
            }
            self.sequence(center, to, step);
        }
    }
}

fn interpolate_segment(lhs_lat: f64, lhs_lon: f64, rhs_lat: f64, mut rhs_lon: f64, target: &mut Vec<P>) {
    let d = deg_to_arc();

    if (rhs_lat - lhs_lat).abs() < 0.0065536 && (rhs_lon - lhs_lon).abs() < 0.0065536 {
        target.push(P::new(rhs_lat, rhs_lon));
        return;
    }

    if (rhs_lon - lhs_lon).abs() > 180.0 {
        rhs_lon = if lhs_lon < 0.0 { rhs_lon - 360.0 } else { rhs_lon + 360.0 };
    }

    if (rhs_lon - lhs_lon).abs() > 179.999999 {
        let pole_lat = if lhs_lat + rhs_lat >= 0.0 { 90.0 } else { -90.0 };

        let n = to_i32((pole_lat - lhs_lat).abs() / 0.0065536).wrapping_add(1);
        for j in 1..n {
            target.push(P::new(lhs_lat + (pole_lat - lhs_lat) * (j as f64) / (n as f64), lhs_lon));
        }
        target.push(P::new(pole_lat, lhs_lon));

        let n = to_i32((rhs_lon - lhs_lon).abs() / 0.0065536).wrapping_add(1);
        for j in 1..n {
            target.push(P::new(pole_lat, lhs_lon + (rhs_lon - lhs_lon) * (j as f64) / (n as f64)));
        }
        target.push(P::new(pole_lat, rhs_lon));

        let n = to_i32((rhs_lat - pole_lat).abs() / 0.0065536).wrapping_add(1);
        for j in 1..n {
            target.push(P::new(pole_lat + (rhs_lat - pole_lat) * (j as f64) / (n as f64), rhs_lon));
        }
    } else {
        let lhs_cos = (lhs_lat * d).cos();
        let lhs_gc_x = (lhs_lat * d).sin();
        let lhs_gc_z = lhs_cos;

        let rhs_cos = (rhs_lat * d).cos();
        let rhs_gc_x = (rhs_lat * d).sin();
        let rhs_gc_y = ((rhs_lon - lhs_lon) * d).sin() * rhs_cos;
        let rhs_gc_z = ((rhs_lon - lhs_lon) * d).cos() * rhs_cos;

        let prod = lhs_gc_x * rhs_gc_x + lhs_gc_z * rhs_gc_z;
        let dist = prod.acos();

        let mut orth_x = rhs_gc_x - prod * lhs_gc_x;
        let mut orth_y = rhs_gc_y;
        let mut orth_z = rhs_gc_z - prod * lhs_gc_z;
        let lg_orth = (orth_x * orth_x + orth_y * orth_y + orth_z * orth_z).sqrt();
        orth_x /= lg_orth;
        orth_y /= lg_orth;
        orth_z /= lg_orth;

        Collector::new(orth_x, orth_y, orth_z, lhs_gc_x, lhs_gc_z, dist, lhs_lon, target).collect_center(0.0, dist, 4);
    }

    if rhs_lon < -180.0 {
        rhs_lon += 360.0;
    } else if rhs_lon > 180.0 {
        rhs_lon -= 360.0;
    }
    target.push(P::new(rhs_lat, rhs_lon));
}

// ---------------------------------------------------------------------------
// Segment index per cell (`std::map< uint32, std::vector< unsigned int > >`).
// `entry().or_default()` reproduces `operator[]`, which also creates empty
// cells; those matter for the inside test.

type Idx = BTreeMap<u32, Vec<usize>>;

fn add_segment(m: &mut Idx, from: P, to: P, pos: usize) {
    let lhs_ilat = ilat(from.lat);
    let lhs_ilon = ilon(from.lon) as u32;
    let rhs_ilat = ilat(to.lat);
    let rhs_ilon = ilon(to.lon) as u32;

    m.entry((lhs_ilat & MASK) | (lhs_ilon >> 16)).or_default().push(pos);
    if (lhs_ilon & MASK) != (rhs_ilon & MASK) {
        m.entry((lhs_ilat & MASK) | (rhs_ilon >> 16)).or_default().push(pos);
    }
    if (lhs_ilat & MASK) != (rhs_ilat & MASK) {
        m.entry((rhs_ilat & MASK) | (lhs_ilon >> 16)).or_default().push(pos);
        if (lhs_ilon & MASK) != (rhs_ilon & MASK) {
            m.entry((rhs_ilat & MASK) | (rhs_ilon >> 16)).or_default().push(pos);
        }
    }
}

fn replace_in(segs: &mut [usize], old_pos: usize, new_pos: usize) {
    for s in segs.iter_mut() {
        if *s == old_pos {
            *s = new_pos;
        }
    }
}

fn replace_segment(m: &mut Idx, from: P, via: P, to: P, old_pos: usize, mut new_pos: usize) {
    let lhs_ilat = ilat(from.lat) & MASK;
    let lhs_ilon = ilon(from.lon) as u32 & MASK;
    let rhs_ilat = ilat(to.lat) & MASK;
    let rhs_ilon = ilon(to.lon) as u32 & MASK;
    let via_ilat = ilat(via.lat) & MASK;
    let via_ilon = ilon(via.lon) as u32 & MASK;
    let via_is_lhs = via_ilat == lhs_ilat && via_ilon == lhs_ilon;
    let via_is_rhs = via_ilat == rhs_ilat && via_ilon == rhs_ilon;

    // The upper part is the one that possibly crosses the index boundary
    if via_is_lhs {
        new_pos += 1;
    }

    replace_in(m.entry(lhs_ilat | (lhs_ilon >> 16)).or_default(), old_pos, new_pos);
    if lhs_ilon != rhs_ilon {
        replace_in(m.entry(lhs_ilat | (rhs_ilon >> 16)).or_default(), old_pos, new_pos);
    }
    if lhs_ilat != rhs_ilat {
        replace_in(m.entry(rhs_ilat | (lhs_ilon >> 16)).or_default(), old_pos, new_pos);
        if lhs_ilon != rhs_ilon {
            if via_is_lhs || via_is_rhs {
                replace_in(m.entry(rhs_ilat | (rhs_ilon >> 16)).or_default(), old_pos, new_pos);
            } else {
                m.entry(lhs_ilat | (rhs_ilon >> 16)).or_default().push(new_pos + 1);
                m.entry(rhs_ilat | (lhs_ilon >> 16)).or_default().push(new_pos + 1);
                replace_in(m.entry(rhs_ilat | (rhs_ilon >> 16)).or_default(), old_pos, new_pos + 1);
            }
        }
    }

    if via_is_lhs {
        m.entry(lhs_ilat | (lhs_ilon >> 16)).or_default().push(new_pos - 1);
    } else if via_is_rhs {
        m.entry(rhs_ilat | (rhs_ilon >> 16)).or_default().push(new_pos + 1);
    }
}

fn try_intersect(lhs_from: P, lhs_to: P, rhs_from: P, rhs_to: P, isect: &mut P) -> bool {
    let rfmlt_lat = rhs_from.lat - lhs_to.lat;
    let rfmlt_lon = rhs_from.lon - lhs_to.lon;

    // The two segments are connected by a vertex
    if rfmlt_lat == 0.0 && rfmlt_lon == 0.0 {
        return false;
    }
    if lhs_from == rhs_to || lhs_from == rhs_from || lhs_to == rhs_to {
        return false;
    }

    let lfmlt_lat = lhs_from.lat - lhs_to.lat;
    let lfmlt_lon = lhs_from.lon - lhs_to.lon;
    let rfmrt_lat = rhs_from.lat - rhs_to.lat;
    let rfmrt_lon = rhs_from.lon - rhs_to.lon;
    let det = lfmlt_lat * rfmrt_lon - rfmrt_lat * lfmlt_lon;

    if det == 0.0 {
        // Segments on parallel but distinct beams
        if lfmlt_lat * rfmlt_lon - lfmlt_lon * rfmlt_lat != 0.0 {
            return false;
        }

        // Overlapping collinear segments: compare along the dominant axis.
        let key = |p: &P| if rfmlt_lat.abs() > rfmlt_lon.abs() { p.lat } else { p.lon };
        let (lf, lt, rf, rt) = (key(&lhs_from), key(&lhs_to), key(&rhs_from), key(&rhs_to));
        if lf < lt {
            if rf < rt {
                if lt < rf || rt < lf {
                    return false;
                }
                *isect = if lf < rf { rhs_from } else { lhs_from };
            } else {
                if lt < rt || rf < lf {
                    return false;
                }
                *isect = if lf < rt { rhs_to } else { lhs_from };
            }
        } else if rf < rt {
            if lf < rf || rt < lt {
                return false;
            }
            *isect = if lt < rf { rhs_from } else { lhs_to };
        } else {
            if lf < rt || rf < lt {
                return false;
            }
            *isect = if lt < rt { rhs_to } else { lhs_to };
        }
        return true;
    }

    let x = (rfmrt_lon * rfmlt_lat - rfmrt_lat * rfmlt_lon) / det;
    let y = (lfmlt_lat * rfmlt_lon - lfmlt_lon * rfmlt_lat) / det;

    if x <= -1e-7 || x >= 1.0 + 1e-7 || y <= -1e-7 || y >= 1.0 + 1e-7 {
        return false;
    }

    isect.lat = lhs_to.lat + x * lfmlt_lat;
    isect.lon = lhs_to.lon + x * lfmlt_lon;
    true
}

/// Split segment `s` at `isect` (one half of the C++ `split_segments` body).
fn split_at(a: &mut Vec<P>, gaps: &mut Vec<usize>, m: &mut Idx, s: usize, isect: P) {
    if isect != a[s] && isect != a[s + 1] {
        let (from, to) = (a[s], a[s + 1]);
        a.push(from);
        a.push(isect);
        a.push(to);
        let at = gaps.partition_point(|&g| g < s + 1);
        gaps.insert(at, s + 1);
        gaps.push(a.len());

        replace_segment(m, from, isect, to, s, a.len() - 3);
    }
}

fn split_segments(a: &mut Vec<P>, gaps: &mut Vec<usize>, m: &mut Idx) {
    // Iterate like a std::map iterator: cells inserted behind the current one
    // during the loop are visited as well.
    let mut cur: Option<u32> = None;
    let mut ticks = 0u32;
    loop {
        let next = match cur {
            None => m.keys().next().copied(),
            Some(k) => m.range((Excluded(k), Unbounded)).next().map(|(k, _)| *k),
        };
        let Some(key) = next else { break };
        cur = Some(key);

        let mut isect = P::new(100.0, 0.0);
        // Local copy of the cell's segments, refreshed after every split.
        let mut segs: Vec<usize> = m[&key].clone();
        let mut i = 1;
        while i < segs.len() {
            ticks += 1;
            if ticks % 256 == 0 && crate::exec::fetch::thread_expired() {
                return;
            }
            for j in 0..i {
                let sj = segs[j];
                let si = segs[i];
                if !try_intersect(a[sj], a[sj + 1], a[si], a[si + 1], &mut isect) {
                    continue;
                }
                if cell(&isect) != key {
                    // Ensure that the same intersection is processed only in one index
                    continue;
                }
                // Avoid rounding artifacts
                for &sk in &segs {
                    if isect.eps_eq(&a[sk]) {
                        isect = a[sk];
                    }
                    if isect.eps_eq(&a[sk + 1]) {
                        isect = a[sk + 1];
                    }
                }

                split_at(a, gaps, m, sj, isect);
                let si = m[&key][i];
                split_at(a, gaps, m, si, isect);
                segs.clear();
                segs.extend_from_slice(&m[&key]);
            }
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Ring assembly (`collect_divertions`, `assemble_linestrings`).

#[derive(Clone, Copy)]
struct IdxPt {
    pt: P,
    idx: usize,
    remote: usize,
}

fn idxpt_lt(a: &IdxPt, b: &IdxPt) -> bool {
    if a.pt.lat != b.pt.lat {
        return a.pt.lat < b.pt.lat;
    }
    if a.pt.lon != b.pt.lon {
        return a.pt.lon < b.pt.lon;
    }
    a.idx < b.idx
}

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Div {
    to_idx: u32,
    to_remote: u32,
}

impl Div {
    const INVALID: Div = Div { to_idx: NONE, to_remote: NONE };

    fn to(p: &IdxPt) -> Div {
        Div { to_idx: p.idx as u32, to_remote: p.remote as u32 }
    }
}

fn line_divertion_idx(p: &IdxPt) -> usize {
    (p.idx as i64 * 2 + ((p.remote as i64 - p.idx as i64 - 1) / 2)) as usize
}

fn collect_divertions(a: &[P], key: u32, segs: &[usize], divs: &mut [Div]) {
    let mut pos_per_pt: Vec<IdxPt> = Vec::new();
    for &s in segs {
        if cell(&a[s]) == key {
            pos_per_pt.push(IdxPt { pt: a[s], idx: s, remote: s + 1 });
        }
        if cell(&a[s + 1]) == key {
            pos_per_pt.push(IdxPt { pt: a[s + 1], idx: s + 1, remote: s });
        }
    }
    if pos_per_pt.is_empty() {
        return;
    }
    cpp_sort(&mut pos_per_pt, &idxpt_lt);

    let ppp = &pos_per_pt;
    let set = |from: usize, to: usize, divs: &mut [Div]| {
        divs[line_divertion_idx(&ppp[from])] = Div::to(&ppp[to]);
    };

    let mut same_since = 0;
    let mut i = 0;
    while i <= ppp.len() {
        if i == ppp.len() || ppp[i].pt != ppp[same_since].pt {
            if i - same_since == 2 {
                set(same_since, same_since + 1, divs);
                set(same_since + 1, same_since, divs);
            } else {
                let mut lpg: Vec<(f64, usize)> = (same_since..i)
                    .map(|j| {
                        let (r, s) = (a[ppp[j].remote], a[ppp[j].idx]);
                        ((r.lon - s.lon).atan2(r.lat - s.lat), j)
                    })
                    .collect();
                cpp_sort(&mut lpg, &|x: &(f64, usize), y: &(f64, usize)| {
                    x.0 < y.0 || (!(y.0 < x.0) && x.1 < y.1)
                });

                let mut j = 0;
                while j < lpg.len() {
                    if j + 3 < lpg.len() && lpg[j + 1].0 == lpg[j + 2].0 {
                        set(lpg[j].1, lpg[j + 3].1, divs);
                        set(lpg[j + 1].1, lpg[j + 2].1, divs);
                        set(lpg[j + 2].1, lpg[j + 1].1, divs);
                        set(lpg[j + 3].1, lpg[j].1, divs);
                        j += 2;
                    } else {
                        if j + 1 >= lpg.len() {
                            break; // out of bounds in C++
                        }
                        set(lpg[j].1, lpg[j + 1].1, divs);
                        set(lpg[j + 1].1, lpg[j].1, divs);
                    }
                    j += 2;
                }
            }
            same_since = i;
        }
        i += 1;
    }
}

fn assemble_linestrings(a: &[P], divs: &mut [Div], out: &mut Vec<Vec<P>>) {
    let n = a.len();
    let mut pos: usize = 0;
    let mut count = 0usize;
    for i in (0..divs.len()).step_by(2) {
        count += (divs[i + 1].to_idx == NONE) as usize;
    }

    // C++ loops forever when no progress is possible; stop instead.
    let mut idle = 0usize;
    while count < n {
        let mut steps = 0usize;
        while divs[2 * pos].to_idx == NONE {
            pos += 1;
            if pos == n - 1 {
                pos = 0;
            }
            steps += 1;
            if steps > n {
                return;
            }
        }

        let mut cur: Vec<P> = Vec::new();
        let before = count;
        let mut dir: i64 = 1;
        loop {
            let k = 2 * pos as i64 + dir;
            if k < 0 || k as usize >= divs.len() || divs[k as usize].to_idx == NONE {
                break;
            }
            if cur.len() >= 2 && a[pos].eps_eq(&cur[cur.len() - 2]) {
                // Remove pairs of equal segments
                cur.pop();
            } else if cur.is_empty() || !a[pos].eps_eq(cur.last().unwrap()) {
                cur.push(a[pos]);
            }

            let dv = divs[(2 * pos as i64 + (3 * dir - 1) / 2) as usize];
            dir = dv.to_remote.wrapping_sub(dv.to_idx) as i32 as i64;
            pos = dv.to_idx as usize;

            count += 1;
            divs[k as usize].to_idx = NONE;
        }

        // `pos` is an unsigned int in C++: NONE + 1 wraps to 0.
        pos = (pos as u32).wrapping_add(1) as usize;
        if pos == n - 1 {
            pos = 0;
        }

        if cur.len() > 2 {
            cur.push(cur[0]);
            out.push(cur);
        }

        if count == before {
            idle += 1;
            if idle > n {
                return;
            }
        } else {
            idle = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Inside test (`toggle_if_inside`, `RHR_Polygon_Area_Oracle`, `Four_Field_Index`).

#[allow(clippy::too_many_arguments)]
fn toggle_if_inside(
    is_inside: &mut bool,
    on_vertex: &mut bool,
    on_segment: &mut bool,
    border_lat: f64,
    border_lon: f64,
    pt: P,
    mut lhs: P,
    mut rhs: P,
) {
    if pt.lon < -179.9 {
        lhs.lon -= if lhs.lon > 0.0 { 360.0 } else { 0.0 };
        rhs.lon -= if rhs.lon > 0.0 { 360.0 } else { 0.0 };
    } else if pt.lon > 179.9 {
        lhs.lon += if lhs.lon < 0.0 { 360.0 } else { 0.0 };
        rhs.lon += if rhs.lon < 0.0 { 360.0 } else { 0.0 };
    }

    let at_vertex = (pt.lat == lhs.lat && pt.lon == lhs.lon) || (pt.lat == rhs.lat && pt.lon == rhs.lon);

    if lhs.lat == rhs.lat {
        if lhs.lat < pt.lat {
            *is_inside ^= (lhs.lon - pt.lon) * (rhs.lon - pt.lon) < 0.0;
        } else if lhs.lat == pt.lat {
            if pt.lon == lhs.lon || pt.lon == rhs.lon {
                *on_vertex = true;
            } else {
                *on_segment |= (lhs.lon - pt.lon) * (rhs.lon - pt.lon) < 0.0;
            }
        }
        // no else required -- such a segment does not play a role
    } else if lhs.lon == rhs.lon {
        if at_vertex {
            *on_vertex = true;
        } else if pt.lon == lhs.lon && (pt.lat - lhs.lat) * (pt.lat - rhs.lat) < 0.0 {
            *on_segment = true;
        } else if lhs.lon < pt.lon && (lhs.lat < border_lat || rhs.lat < border_lat) {
            *is_inside = !*is_inside;
        }
        // no else required -- such a segment does not play a role
    } else if (lhs.lat - rhs.lat) * (lhs.lon - rhs.lon) < 0.0 {
        // The segment runs from nw to se or se to nw
        if at_vertex {
            *on_vertex = true;
        } else if (pt.lon - lhs.lon) * (pt.lon - rhs.lon) < 0.0 {
            let isect_lat = lhs.lat + (rhs.lat - lhs.lat) * (pt.lon - lhs.lon) / (rhs.lon - lhs.lon);
            if isect_lat < pt.lat {
                *is_inside = !*is_inside;
            } else if isect_lat == pt.lat {
                *on_segment = true;
            }
        } else if pt.lon == lhs.lon && rhs.lon < pt.lon {
            if lhs.lat < pt.lat {
                *is_inside = !*is_inside;
            }
        } else if pt.lon == rhs.lon && lhs.lon < pt.lon {
            if rhs.lat < pt.lat {
                *is_inside = !*is_inside;
            }
        } else if (border_lat - lhs.lat) * (border_lat - rhs.lat) < 0.0 {
            let isect_lon = lhs.lon + (rhs.lon - lhs.lon) * (border_lat - lhs.lat) / (rhs.lat - lhs.lat);
            if border_lon < isect_lon && isect_lon < pt.lon {
                *is_inside = !*is_inside;
            }
        }
    } else {
        // The segment runs from sw to ne or ne to sw
        if (border_lat - lhs.lat) * (border_lat - rhs.lat) < 0.0 {
            let isect_lon = lhs.lon + (rhs.lon - lhs.lon) * (border_lat - lhs.lat) / (rhs.lat - lhs.lat);
            if (lhs.lon <= pt.lon || rhs.lon <= pt.lon) && border_lon < isect_lon {
                *is_inside = !*is_inside;
            }
        }
        if at_vertex {
            *on_vertex = true;
            return;
        }

        if (pt.lon - lhs.lon) * (pt.lon - rhs.lon) < 0.0 {
            let isect_lat = lhs.lat + (rhs.lat - lhs.lat) * (pt.lon - lhs.lon) / (rhs.lon - lhs.lon);
            if isect_lat < pt.lat {
                *is_inside = !*is_inside;
            } else if isect_lat == pt.lat {
                *on_segment = true;
            }
        } else if pt.lon == lhs.lon && rhs.lon < pt.lon {
            if lhs.lat < pt.lat {
                *is_inside = !*is_inside;
            }
        } else if pt.lon == rhs.lon && lhs.lon < pt.lon {
            if rhs.lat < pt.lat {
                *is_inside = !*is_inside;
            }
        }
    }
}

struct Oracle<'a> {
    a: &'a [P],
    m: &'a Idx,
    inside_corners: BTreeSet<u32>,
}

impl Oracle<'_> {
    /// Toggles `*corner` once for every segment of cell `value` that crosses
    /// the given border of the cell.
    fn build_area(&mut self, sw: bool, value: i32, se: Option<&mut bool>, nw: Option<&mut bool>) {
        let Some(segs) = self.m.get(&(value as u32)) else { return };
        let a = self.a;
        if sw {
            self.inside_corners.insert(value as u32);
        }
        let vshl = value << 16;
        let vlat = value as u32 & MASK;
        let west = lon_of(((value as u32) << 16) as i32);
        let sub360 = |lon: f64| lon - if lon > 0.0 { 360.0 } else { 0.0 };

        if west > -179.99 {
            if let Some(nw) = nw {
                *nw = sw;
                for &s in segs {
                    let lhs_ilon = (ilon(a[s].lon) as u32 & MASK) as i32;
                    let rhs_ilon = (ilon(a[s + 1].lon) as u32 & MASK) as i32;
                    if (lhs_ilon < vshl && rhs_ilon == vshl) || (lhs_ilon == vshl && rhs_ilon < vshl) {
                        let isect_lat =
                            a[s].lat + (a[s + 1].lat - a[s].lat) * (west - a[s].lon) / (a[s + 1].lon - a[s].lon);
                        if ilat(isect_lat) & MASK == vlat {
                            *nw = !*nw;
                        }
                    }
                }
            }
            if let Some(se) = se {
                *se = sw;
                for &s in segs {
                    let lhs_ilat = ilat(a[s].lat) & MASK;
                    let rhs_ilat = ilat(a[s + 1].lat) & MASK;
                    if (lhs_ilat < vlat && rhs_ilat == vlat) || (lhs_ilat == vlat && rhs_ilat < vlat) {
                        let isect_lon =
                            a[s].lon + (a[s + 1].lon - a[s].lon) * (lat_of(vlat) - a[s].lat) / (a[s + 1].lat - a[s].lat);
                        if ilon(isect_lon) as u32 & MASK == (value as u32) << 16 {
                            *se = !*se;
                        }
                    }
                }
            }
        } else {
            if let Some(nw) = nw {
                *nw = sw;
                for &s in segs {
                    let lhs_lon = sub360(a[s].lon);
                    let rhs_lon = sub360(a[s + 1].lon);
                    if west > -180.0 {
                        let lhs_ilon = (ilon(lhs_lon) as u32 & MASK) as i32;
                        let rhs_ilon = (ilon(rhs_lon) as u32 & MASK) as i32;
                        if (lhs_ilon < vshl && rhs_ilon == vshl) || (lhs_ilon == vshl && rhs_ilon < vshl) {
                            let isect_lat =
                                a[s].lat + (a[s + 1].lat - a[s].lat) * (west - a[s].lon) / (a[s + 1].lon - a[s].lon);
                            if ilat(isect_lat) & MASK == vlat {
                                *nw = !*nw;
                            }
                        }
                    } else if (lhs_lon <= -180.0 && rhs_lon > -180.0) || (rhs_lon <= -180.0 && lhs_lon > -180.0) {
                        let isect_lat =
                            a[s].lat + (a[s + 1].lat - a[s].lat) * (-180.0 - a[s].lon) / (a[s + 1].lon - a[s].lon);
                        if ilat(isect_lat) & MASK == vlat {
                            *nw = !*nw;
                        }
                    }
                }
            }
            if let Some(se) = se {
                *se = sw;
                for &s in segs {
                    let lhs_ilat = ilat(a[s].lat) & MASK;
                    let rhs_ilat = ilat(a[s + 1].lat) & MASK;
                    if (lhs_ilat < vlat && rhs_ilat == vlat) || (lhs_ilat == vlat && rhs_ilat < vlat) {
                        let isect_lon =
                            a[s].lon + (a[s + 1].lon - a[s].lon) * (lat_of(vlat) - a[s].lat) / (a[s + 1].lat - a[s].lat);
                        if isect_lon >= -180.0 && ilon(isect_lon) as u32 & MASK == (value as u32) << 16 {
                            *se = !*se;
                        }
                    }
                }
            }
        }
    }

    fn get_point_status(&self, value: i32, lat: f64, lon: f64) -> i32 {
        if value == 1 {
            return 1;
        }
        let Some(segs) = self.m.get(&(value as u32)) else { return 0 };

        let border_lon = cmax(lon_of(value << 16), -180.0);
        let border_lat = lat_of(value as u32 & MASK);

        let mut on_vertex = false;
        let mut on_segment = false;
        // is_inside is now true iff the sw corner is inside the area
        let mut is_inside = self.inside_corners.contains(&(value as u32));

        let pt = P::new(lat, lon);
        for &s in segs {
            toggle_if_inside(
                &mut is_inside,
                &mut on_vertex,
                &mut on_segment,
                border_lat,
                border_lon,
                pt,
                self.a[s],
                self.a[s + 1],
            );
        }

        if on_vertex {
            return 0x20 + 2 * is_inside as i32;
        }
        if on_segment {
            return 0x10 + 2 * is_inside as i32;
        }
        2 * is_inside as i32
    }
}

#[derive(Clone, Copy, Default)]
struct Entry {
    sw: i32,
    se: i32,
    nw: i32,
    ne: i32,
}

impl Entry {
    fn quad(&mut self, north: bool, east: bool) -> &mut i32 {
        match (north, east) {
            (true, true) => &mut self.ne,
            (true, false) => &mut self.nw,
            (false, true) => &mut self.se,
            (false, false) => &mut self.sw,
        }
    }
}

struct FourFieldIndex {
    tree: Vec<Entry>,
    base_lat: u32,
    base_lon: i32,
    base_bits: i32,
    min_lon: i32,
}

impl FourFieldIndex {
    fn new() -> Self {
        FourFieldIndex { tree: Vec::new(), base_lat: 0, base_lon: 2000000000, base_bits: 0, min_lon: ilon(-180.0) }
    }

    fn add_point(&mut self, lat: f64, lon: f64, value: i32) -> i32 {
        if lat < -90.0 || lat > 90.0 || lon < -180.0 || lon > 180.0 {
            return 0;
        }
        let ilat = ilat(lat);
        let ilon = ilon(lon) as u32;
        let pos = self.make_available(ilat & 0xfffe_0000, (ilon & 0xfffe_0000) as i32, 15);
        let slot = self.tree[pos].quad(ilat & 0x0001_0000 != 0, ilon & 0x0001_0000 != 0);
        std::mem::replace(slot, value)
    }

    fn make_available(&mut self, lat: u32, lon: i32, significant_bits: i32) -> usize {
        if self.tree.is_empty() {
            self.base_lat = lat;
            self.base_lon = lon;
            self.base_bits = significant_bits;
            self.tree.push(Entry::default());
            return 0;
        }

        let lon = lon as u32;
        let mut bitmask = !(0xffff_ffffu32 >> self.base_bits);
        while (lat & bitmask) != (self.base_lat & bitmask) || (lon & bitmask) != (self.base_lon as u32 & bitmask) {
            self.base_bits -= 1;

            let old_root = self.tree[0];
            self.tree.push(old_root);
            self.tree[0] = Entry::default();
            let bit = 0x8000_0000u32 >> self.base_bits;
            let link = -(self.tree.len() as i32) + 1;
            *self.tree[0].quad(self.base_lat & bit != 0, self.base_lon as u32 & bit != 0) = link;

            bitmask = !(0xffff_ffffu32 >> self.base_bits);
            self.base_lat &= bitmask;
            self.base_lon = (self.base_lon as u32 & bitmask) as i32;
        }

        let mut cur_bits = self.base_bits;
        let mut cur_pos = 0usize;
        while cur_bits < significant_bits {
            let bit = 0x8000_0000u32 >> cur_bits;
            let len = self.tree.len();
            let slot = self.tree[cur_pos].quad(lat & bit != 0, lon & bit != 0);
            if *slot < 0 {
                cur_pos = (-*slot) as usize;
            } else {
                *slot = -(len as i32);
                cur_pos = len;
                self.tree.push(Entry::default());
            }
            cur_bits += 1;
        }
        cur_pos
    }

    fn compute_inside_parts(&mut self, oracle: &mut Oracle) {
        if !self.tree.is_empty() {
            self.compute(oracle, self.base_lat, self.base_lon, self.base_bits, 0, false, None, None);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn compute(
        &mut self,
        oracle: &mut Oracle,
        lat: u32,
        lon: i32,
        significant_bits: i32,
        pos: usize,
        sw: bool,
        r_se: Option<&mut bool>,
        r_nw: Option<&mut bool>,
    ) {
        let half = 0x8000_0000u32 >> significant_bits;
        let east_lon = lon.wrapping_add(half as i32);
        let north_lat = lat.wrapping_add(half);
        let bits = significant_bits + 1;

        if significant_bits == 0 || self.min_lon <= east_lon {
            let mut se = false;
            let mut nw = false;
            let mut ne = false;

            let v = self.tree[pos].sw;
            if v < 0 {
                self.compute(oracle, lat, lon, bits, (-v) as usize, sw, Some(&mut se), Some(&mut nw));
            } else if v > 0 {
                oracle.build_area(sw, v, Some(&mut se), Some(&mut nw));
            } else {
                self.tree[pos].sw = sw as i32;
                se = sw;
                nw = sw;
            }

            let v = self.tree[pos].se;
            if v < 0 {
                self.compute(oracle, lat, east_lon, bits, (-v) as usize, se, r_se, None);
            } else if v > 0 {
                oracle.build_area(se, v, r_se, None);
            } else {
                self.tree[pos].se = se as i32;
                if let Some(r) = r_se {
                    *r = se;
                }
            }

            let v = self.tree[pos].nw;
            if v < 0 {
                self.compute(oracle, north_lat, lon, bits, (-v) as usize, nw, Some(&mut ne), r_nw);
            } else if v > 0 {
                oracle.build_area(nw, v, Some(&mut ne), r_nw);
            } else {
                self.tree[pos].nw = nw as i32;
                ne = nw;
                if let Some(r) = r_nw {
                    *r = nw;
                }
            }

            let v = self.tree[pos].ne;
            if v < 0 {
                self.compute(oracle, north_lat, east_lon, bits, (-v) as usize, ne, None, None);
            } else if v > 0 {
                oracle.build_area(ne, v, None, None);
            } else {
                self.tree[pos].ne = ne as i32;
            }
        } else {
            let mut ne = false;

            let v = self.tree[pos].se;
            if v < 0 {
                self.compute(oracle, lat, east_lon, bits, (-v) as usize, sw, r_se, Some(&mut ne));
            } else if v > 0 {
                oracle.build_area(sw, v, r_se, Some(&mut ne));
            } else {
                self.tree[pos].se = sw as i32;
                if let Some(r) = r_se {
                    *r = sw;
                }
            }

            let v = self.tree[pos].ne;
            if v < 0 {
                self.compute(oracle, north_lat, east_lon, bits, (-v) as usize, ne, None, r_nw);
            } else if v > 0 {
                oracle.build_area(ne, v, None, r_nw);
            } else {
                self.tree[pos].ne = ne as i32;
                if let Some(r) = r_nw {
                    *r = ne;
                }
            }
        }
    }

    fn get_point_status(&self, oracle: &Oracle, lat: f64, lon: f64) -> i32 {
        if self.tree.is_empty() {
            return 0;
        }
        let ilat = ilat(lat);
        let ilon = ilon(lon) as u32;

        let mut cur_bits = self.base_bits;
        let mut cur_pos = 0usize;
        while cur_bits < 32 {
            let bit = 0x8000_0000u32 >> cur_bits;
            let mut e = self.tree[cur_pos];
            let v = *e.quad(ilat & bit != 0, ilon & bit != 0);
            if v < 0 {
                cur_pos = (-v) as usize;
            } else if v > 0 {
                return oracle.get_point_status(v, lat, lon);
            } else {
                return 0;
            }
            cur_bits += 1;
        }
        0
    }
}

fn strictly_west_of(lhs: f64, rhs: f64) -> bool {
    if lhs - rhs > 180.0 {
        return lhs - 360.0 < rhs;
    }
    lhs < rhs
}

fn weakly_west_of(lhs: f64, rhs: f64) -> bool {
    if lhs - rhs > 180.0 {
        return lhs - 360.0 <= rhs;
    }
    lhs <= rhs
}

/// RHR normalisation of a free polygon. `rings` are the linestrings as
/// `poly()` collects them; like `Free_Polygon_Geometry::add_linestring`,
/// rings with fewer than 2 points are skipped and open rings are closed.
/// Returns the closed rings in C++ output order.
pub fn rhr_polygon(rings: &[Vec<(f64, f64)>]) -> Vec<Vec<(f64, f64)>> {
    let mut input: Vec<Vec<P>> = Vec::new();
    for r in rings {
        if r.len() < 2 {
            continue;
        }
        let mut l: Vec<P> = r.iter().map(|&(lat, lon)| P::new(lat, lon)).collect();
        if l[0] != l[l.len() - 1] {
            l.push(l[0]);
        }
        input.push(l);
    }

    let mut a: Vec<P> = Vec::new();
    let mut gaps: Vec<usize> = vec![0];
    for l in &input {
        a.push(l[0]);
        for i in 1..l.len() {
            interpolate_segment(l[i - 1].lat, l[i - 1].lon, l[i].lat, l[i].lon, &mut a);
        }
        gaps.push(a.len());
    }

    let mut m: Idx = BTreeMap::new();
    let mut gap_it = 0;
    for i in 0..a.len() {
        if gap_it < gaps.len() && gaps[gap_it] == i {
            gap_it += 1;
        } else {
            add_segment(&mut m, a[i - 1], a[i], i - 1);
        }
    }
    split_segments(&mut a, &mut gaps, &mut m);

    // Gap positions keep the default (invalid) divertions.
    let mut divs = vec![Div::INVALID; 2 * a.len()];
    for (key, segs) in &m {
        collect_divertions(&a, *key, segs, &mut divs);
    }

    let mut lines: Vec<Vec<P>> = Vec::new();
    assemble_linestrings(&a, &mut divs, &mut lines);

    let mut oracle = Oracle { a: &a, m: &m, inside_corners: BTreeSet::new() };
    let mut ffi = FourFieldIndex::new();
    for &key in m.keys() {
        ffi.add_point(lat_of(key | 0x8000), lon_of(((key << 16) | 0x8000) as i32), key as i32);
    }
    ffi.compute_inside_parts(&mut oracle);

    for l in lines.iter_mut() {
        if l.len() <= 2 {
            continue;
        }
        let (p0, p1, p2) = (l[0], l[1], l[2]);
        let inside = ffi.get_point_status(&oracle, p1.lat, p1.lon) & 0x3 != 0;
        let slope_turn = || (p1.lat - p0.lat) / (p1.lon - p0.lon) < (p2.lat - p1.lat) / (p2.lon - p1.lon);
        let reverse = if strictly_west_of(p0.lon, p1.lon) {
            if weakly_west_of(p1.lon, p2.lon) { inside } else { inside ^ slope_turn() }
        } else if strictly_west_of(p2.lon, p1.lon) {
            !inside
        } else if p0.lon == p1.lon {
            inside ^ (p0.lat < p1.lat)
        } else if p2.lon == p1.lon {
            inside ^ (p1.lat < p2.lat)
        } else {
            inside ^ slope_turn()
        };
        if reverse {
            l.reverse();
        }
    }

    lines.into_iter().map(|l| l.into_iter().map(|p| (p.lat, p.lon)).collect()).collect()
}
