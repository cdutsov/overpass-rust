# Areas — implementation spec (Overpass API 0.7.62, master @ a0db4f39)

Scope: everything about areas except attic/history (attic code paths exist next to every
current-data path below. They are out of scope, so ignore any `attic_*` set). C++ paths are relative to
`Overpass-API/src/`. "Verified" means the behaviour was checked against a C++ build of this
revision (osm3s_query/update_database built from this tree, `--db-dir` mode).

There are **two unrelated area models**, with different geometry code, tile sizes and border rules:

| | DB areas ("area objects") | Implicit areas (closed ways) |
|---|---|---|
| Source | `make-area` in `--rules` runs, stored in the area DB | any way in a set whose first node id == last node id |
| Ids | 32-bit: way+2400000000, relation+3600000000, node id | the way id (they stay ways) |
| Tile grid | "block tile" = 2^20 × 2^20 coordinate units (~0.105°) | node tile = 2^16 × 2^16 units (~0.0066°) |
| Point test | `check_area_block`: ray cast **south** inside each block tile, plus synthetic "carry" segments | L-shaped path from the tile's SW corner, with an `sw_is_inside` flag swept **eastwards** |
| Border tolerance | exact integer equality (and an integer-truncated intersection) | ±0.5 coordinate units |
| Used by | `is_in`, `(area)`, `area` queries, `map_to_area`, `pivot` | `is_in`, `(area)`, `area` queries (as ways), `map_to_area` (as passthrough) |

Relations are **never** implicit areas. A relation can act as an area only through its DB area
(`map_to_area`).

---

## 1. Coordinate primitives (shared)

All geometry is integer. For a node: `ilat = (u32)((lat + 91.0) * 1e7 + 0.5)` and
`ilon = (i32)(lon * 1e7 + (lon > 0 ? 0.5 : -0.5))` (truncation toward zero, `core/index_computations.h:1257-1266`).
Doubles that users supply (`is_in(lat,lon)`) are converted the same way. Back-conversion:
`lat = ilat/1e7 - 91.0` and `lon = ilon/1e7`. A double round trip of a node coordinate is exact,
so you can work on the integers directly (`area_query.cc:608-611` does the round trip, which is harmless).

- `ll_upper(ilat, ilon)`: interleave the **high 16 bits** of ilat (odd bit positions) and ilon (even
  positions). `node_idx = ll_upper(ilat, ilon) ^ 0x40000000` (the XOR flips the lon sign bit to
  offset-binary). Bit 31 is always 0.
- `ll_lower(ilat, ilon)`: the same interleave of the **low 16 bits**.
- Decoding: `ilat(upper, lower)` and `ilon(upper, lower)` invert this, and `ilon` re-applies `^0x40000000` to `upper`.
- **Node tile** = `node_idx` (cell 0x10000 × 0x10000 units). SW corner: `south = ilat(idx,0)` and `west = ilon(idx,0)`.
- **Block tile** = `node_idx & 0xffffff00` (cell 0x100000 × 0x100000 units; it contains 256 node tiles,
  and its node-index range is `[t, t+0x100)`). The SW corner is `(ilat & 0xfff00000, ilon & 0xfff00000)`, using i32
  two's-complement masking (so negative lons floor toward −∞).
- For a fixed lon cell, a larger lat gives a larger tile id. For a fixed lat cell, a larger lon gives a larger id.
  Both facts are relied on below.

## 2. Area DB data model and storage

Logical content per area (`core/type_area.h`):

- `Area_Skeleton { id: u32, used_indices: Vec<u32> }`. `used_indices` is the sorted list of block tiles
  that hold ≥1 block of this area.
- `Area index` (key in the AREAS file) is `calc_index(used_indices)`: the same compound-index function that
  computes a way's index from its node indices (see the index spec). It matters only for `out qt`
  ordering and for the `is_in` lookup (§8). If `used_indices` is empty, the index is 0 and the area is **not stored**.
- `Area_Block { id: u32, coors: Vec<(ilat, ilon)> }` is a polyline stored under one block tile. Every point of a
  block lies inside that tile, except the pole/carry rules in §3.4.
- Tags: AREA_TAGS_LOCAL `(area_index & 0xffffff00, key, value) → area id` and AREA_TAGS_GLOBAL `(key, value) → area id`.
  These use the same filter semantics as element tags.

C++ on-disk layout, for reference only: `areas.bin` holds Uint31 index → `[id u32][n u32][n × u32]`. `area_blocks.bin`
holds Uint31 tile → `[id u32][n u16][n × 5 bytes]`. Each 5-byte point is `ll_lower` (u32 LE) plus the low byte of
`node_idx`, and the tile supplies the upper 24 bits (`type_area.h:251-363`).

Update semantics (`osm-backend/area_updater.cc`):
- `set_area(id)` **replaces** an area: the old location, all of its blocks, and its tags are deleted, then the new ones are inserted
  (`update_area_ids` 83-115, `update_area_tags_*`).
- Areas are **never deleted** otherwise. A relation that stops matching the rules, or that becomes invalid, keeps
  its old area forever. Nothing calls `set_id_deleted`.
- Writes are buffered and flushed when more than 512Ki blocks are pending, at the end of the run, and before `print`/`is_in`.

## 3. Creating areas

### 3.1 Rules scripts and loops
`rules/areas.osm3s` is run by `bin/rules_loop.sh` as `osm3s_query --progress --rules`, every 3 s, forever:

```
union of relations with: (type=multipolygon & name) | (type=boundary & name) | (admin_level & name)
                       | postal_code | addr:postcode
foreach (into pivot):
   union( recurse relation-way from pivot ; recurse way-node )   // ways of ANY role + their nodes
   make-area pivot=pivot return-area=no                         // from "_" = the union
```
- Only **relation** areas are produced. Way rules were removed in commit a2dce9b2 (2021), so closed
  ways are expected to be used as implicit areas instead. Roles are ignored. Nested relation members are
  not followed. Node members are ignored.
- `rules/areas_delta.osm3s` is identical except that each query has `<changed since=X until=X/>`, with
  `X` = the content of `$DB_DIR/area_version`. That selects relations changed after X (this needs attic, so it is out of scope).
  `rules_delta_loop.sh` runs the delta script hourly, or the full script if `area_version` is missing or `init`.
- `--rules` sets area_level=2: the area DB is writable and `area_version` is written with the current
  `osm_base_version` content. In dispatcher mode it is written to `area_version.shadow` and renamed on commit
  (`dispatch/dispatcher_stub.cc:201-223,282-287,394-405`). `--rules` also disables the query timeout (`osm3s_query.cc:83-86`).
- `make-area` outside `--rules` is a hard error. The text is: `Specify --rules to execute a rule. make-area can only appear in rules.`
  (`dispatch/scripting_core.cc:76-83`). `make-area` exists only in XML. QL has no syntax for it.

### 3.2 `<make-area from="_" into="_" pivot="" return-area="yes"/>` (`statements/make_area.cc`)
- If `return-area` is not `yes`/`no`, this static error is raised: `For the attribute "return-area" of the element "make-area" the only allowed values are "yes" or "no", default is "yes".`
- Execution (`execute` 296-423):
  1. If the pivot set does not exist, the output is the empty set (no message).
  2. **Pivot detection** (72-111): the first non-empty node bucket in index order (its first element), else the first way,
     else the first relation. Only ONE element is used. If no element is found, the output is empty.
  3. **Tags**: all current tags of the pivot element (from the local tag index). They are stored sorted by (key, value)
     bytewise, because the tag index is ordered that way. Areas therefore always print their tags key-sorted (verified).
  4. **Id**: `u32(pivot_id)` for a node, `(u32)(way_id + 2400000000)` for a way, and `(u32)(rel_id + 3600000000)` for a relation,
     all in **u32 wrapping arithmetic** (347-350). Node ids ≥ 2^32 are truncated. Way ids > 1 894 967 295 wrap.
  5. If the `from` set does not exist, the output is empty.
  6. **Parity check** (113-137, 359-367): over every way in `from` with ≥2 node refs, toggle the first and last node id
     in a set. If the set is non-empty, emit the runtime remark
     `make-area: Node <smallest odd node id> is contained in an odd number of ways.`. Then **no area** is created,
     the output is empty, and an existing area with that id is left untouched. Example (verified): an open
     way 51 with nodes 201→202 gives `Node 201`.
  7. The remark `...referred by way ... is not contained in set` is dead code (`create_area_blocks` always returns 0, line 162).
     A way whose node refs are not all present in `from` is **silently skipped** (`make_geometry` returns an empty list,
     `data/collect_members.cc:840-857`). That can leave unbalanced rings. In C++ that is UB (see §3.4). Treat it as a bad geometry.
  8. Build the blocks (§3.3). If any segment crossed the antimeridian an odd total number of times, add the
     south-pole line (§3.4). Then add the carry segments (§3.4).
  9. If more than 384×1024 tiles are touched, all blocks are dropped (186-190) and the index is 0, so **no area** is created
     (no message).
  10. Store the area (replace semantics). If `return-area=yes`, the output set = `{Area_Skeleton(id, used_indices)}` at
     its index, otherwise empty.
- Ring semantics: plain **even-odd (XOR)** over all segments of all ways. Inner/outer roles and orientation do not matter.
  Holes work automatically. Overlapping outer rings cancel each other. A way used twice is used once, because sets are deduplicated.

### 3.3 Block construction (`data/collect_members.cc:796-837`, `core/type_area.h:49-216`)
For each way, take its vertex list `P[0..n)` (global ints). Ways with <2 resolvable vertices contribute nothing.
```
cur_tile = None; poly = []
for v in P:
  t = block_tile(v)
  if cur_tile is Some(c) and t != c:
      if len(poly) > 1: blocks[c].push(Block(id, poly))
      for piece in aligned_segments(poly.last, v): blocks[tile(piece.a)].push(Block(id, [piece.a, piece.b]))
      poly = []
  cur_tile = t; poly.push(v)
if len(poly) > 1: blocks[cur_tile].push(Block(id, poly))
```
So a tile holds the polylines of consecutive in-tile vertices, plus one 2-point block per piece of each
tile-crossing segment.

`proportion(clow, cmid, cup, low, up)` = `low` if `cup == clow`, else
`((i64)up - low) * ((i64)cmid - clow) / ((i64)cup - clow) + low`. Division is i64 truncation toward zero, the result is cast to i32,
and all arguments are cast to i32 first.

`aligned_segments(F, T)`:
- If `F.lon < -900000000 && T.lon > 900000000` (antimeridian crossing), emit
  `vert((T.lat,T.lon) → (proportion(T.lon, 1800000000, F.lon+3600000000, T.lat, F.lat), 1800000000))` and
  `vert((proportion(T.lon-3600000000, -1800000000, F.lon, T.lat, F.lat), -1800000000) → (F.lat,F.lon))`, and
  flag "wrapped". The mirror case (`T.lon < -9e8 && F.lon > 9e8`) is symmetric with F and T exchanged. Otherwise emit `vert(F→T)`.
- `vert(A→B)` splits at lon cell boundaries (multiples of 0x100000). If both points are in the same lon cell, emit `horiz(A→B)`.
  Otherwise order the pieces west→east (swap if needed, so W is the west point and E the east point):
  `s = (W.lon & 0xfff00000) + 0x100000`. Emit `horiz(W → (proportion(W.lon, s-1, E.lon, W.lat, E.lat), s-1))`. Then,
  while `s < (E.lon & 0xfff00000)`, emit `horiz((p(s), s) → (p(s+0xfffff), s+0xfffff))` and set `s += 0x100000`. Finally emit
  `horiz((p(s), s) → E)`, where `p(x) = proportion(W.lon, x, E.lon, W.lat, E.lat)`.
- `horiz(A→B)` splits at lat cell boundaries in the same way, south→north (S is the south point and N the north point):
  `s = (S.lat & 0xfff00000) + 0x100000`. Emit the piece `S → (s-1, q(s))` (**note: lon evaluated at `s`, lat at `s-1`**). While
  `s < (N.lat & 0xfff00000)`, emit `(s, q(s)) → (s+0xfffff, q(s+0x100000))` and set `s += 0x100000`. Finally emit `(s, q(s)) → N`, where
  `q(y) = proportion(S.lat, y, N.lat, S.lon, N.lon)`. If both points are in the same lat cell, emit the single piece `A → B`.
- Each piece is stored in the tile of its first point. Pieces never share a boundary point: they end at
  `boundary-1` and the next piece starts at `boundary`. Interpolation is integer-truncated, so the stored polygon is
  not exactly the straight segment.

### 3.4 South-pole line and carry segments
- **South-pole line** (`make_area.cc:244-293`). This only applies if "wrapped" (XOR over all segments) is true. Count tiles
  with south-corner lat < 0 as +1 and others as −1. If the sum is ≥ 0, add a horizontal block at `ilat = 10000000` (−90°) in every
  lon cell: `[(1e7, c), (1e7, c+0xfffff)]` for each cell start `c`. The westmost cell runs from −1800000000, and the eastmost
  cell ends at 1800000000.
- **Carry segments** (`add_segment_blocks` 177-241) let a south-going ray stop at its own tile's southern edge.
  Process the tiles in ascending id order, including tiles created during the loop:
  ```
  L = {}                                   // set of i32 lons, toggle semantics
  for block in tile T: toggle(L, block.first.lon); toggle(L, block.last.lon)
  if L empty: continue
  y = south(T) + 0x100000 ; N = block_tile(y, west(T))     // the northern neighbour
  for (l1, l2), (l3, l4), ... in sorted(L): blocks[N].push(Block(id, [(y, l1), (y, l2)]))
  ```
  If `|L|` is odd the C++ is UB. That cannot happen for balanced rings.
  Every tile in the interior of a ring therefore receives a carry block, and `used_indices` includes those tiles.

## 4. Point/way/relation tests against DB areas

### 4.1 `check_area_block(block, P)` → `HIT=1`, `TOGGLE_EAST=2`, `TOGGLE_WEST=4` (`coord_query.cc:90-185`)
Ray from P straight south. For consecutive block points `(la,lo)` (prev) → `(lb,lob)` (cur):
```
if lo < lob:      // eastward
   if lob < P.lon or lo > P.lon: continue
   if lob == P.lon: { if lb < P.lat: st ^= WEST elif lb == P.lat: return HIT; continue }
   if lo  == P.lon: { if la < P.lat: st ^= EAST elif la == P.lat: return HIT; continue }
elif lo > lob:    // westward
   if lob > P.lon or lo < P.lon: continue
   if lob == P.lon: { if lb < P.lat: st ^= EAST elif lb == P.lat: return HIT; continue }
   if lo  == P.lon: { if la < P.lat: st ^= WEST elif la == P.lat: return HIT; continue }
else:             // vertical
   if lob == P.lon and P.lat in [min(la,lb), max(la,lb)]: return HIT
   continue
y = (u32)((i64)lb + ((i64)P.lon - lob) * ((i64)la - lb) / ((i64)lo - lob))   // i64, truncating
if P.lat > y: st ^= EAST|WEST  elif P.lat == y: return HIT
return st
```
The WEST bit is the parity of a ray infinitesimally west of P, and the EAST bit is the parity just east of P. Blocks are evaluated
independently, and a HIT ends only that block's evaluation (any toggles collected so far in that block are lost).

### 4.2 Aggregation per (point, area, block tile)
The blocks of one area within the point's block tile are evaluated in storage order:
- **is_in / `node(area)` nodes** (`add_border = true`): if any block returns HIT, the point is inside. Otherwise XOR the results.
  The point is inside if the result is ≠ 0 (`area_query.cc:612-634`, `coord_query.cc:290-300`). So **the border is inside**,
  and so is a point that lies inside on either side of a vertical line through a vertex.
- **Relation node members** (`add_border = false`): XOR all results, so HIT toggles bit 0. The point is inside if the result is ≠ 0.
  Whether a border point counts therefore depends on how many blocks report HIT. Verified on a square
  with a hole: members on the outer edges, the hole edges and the hole vertices count. A member exactly on the outer corner
  (where two blocks meet, with no carry segment below) does **not** count. Implement the exact rule.
- A point in a block tile that has no blocks of the area is outside.

### 4.3 Ways against DB areas (`collect_ways`, `area_query.cc:824-979`; always `add_border=false`)
Candidate ways are split with the §3.3 procedure (id = way id) into pieces per block tile. For each block tile T
that has blocks of a requested area (ascending T), with `areas_T` = requested area id → its blocks in T, ascending id:
1. **Vertices** (actual way nodes only) lying in T: for each area, `st = 0`. For each block, `c = check_area_block`. If `c == HIT`,
   set `st = HIT` and stop, otherwise `st ^= c`. If `st & (EAST|WEST) != 0`, the way is inside. Border vertices never count.
2. **Pieces** of candidate ways stored in T. For each piece, `status = {}` (keyed by area id). For each area (stop as soon as
   the way is already inside), for each block b:
   `r = intersects_inner(piece, b)`. If `r == INTERSECT`, the way is inside: stop this area. If `r == HIT`, set `status[aid] = HIT`
   and stop this area. Otherwise run `has_inner_points(piece, b, &status[aid])`.
   After all areas: if any `status` value is ≠ 0 and has no HIT bit, the way is inside.
3. `has_inner_points` (802-821): for each sub-segment (p,q) of the piece, the midpoint is
   `m = (south(T) + ⌊(p.lat_l+q.lat_l)/2⌋, west(T) + ⌊(p.lon_l+q.lon_l)/2⌋)`, where `x_l = x & 0xfffff` is tile-local.
   Then `c = check_area_block(b, m)`. If `c & HIT`, set `st = HIT` (this overwrites the earlier value), else `st ^= c`. All midpoints of a piece and all
   blocks of the area are XORed into one value.
4. `intersects_inner(W, B)` (649-799) works on tile-local u32 coordinates `lat_l = ilat & 0xfffff` and
   `lon_l = 0x80000000 + (ilon & 0xfffff)` (the f64 rounding depends on these absolute values). For each sub-segment
   a of W: if `a` goes east, test it against each sub-segment b of B with `ordered_a(a0,a1,b)`. If it goes west, swap the ends first. If it is vertical,
   use `longitude_a`. Return the first non-zero result.
   - `ordered_a` (requires a0.lon < a1.lon): if `b0.lon<b1.lon` and `a0.lon<b1.lon && b0.lon<a1.lon`, return
     `ordered(a0,a1,b0,b1)`. Mirror this for `b1.lon<b0.lon` with b reversed. If b is vertical and `a0.lon<b.lon<a1.lon`:
     `y = (f64(b.lon)-a0.lon)/(f64(a1.lon)-a0.lon)*(f64(a1.lat)-a0.lat)+a0.lat`. Return INTERSECT if y is strictly
     between b0.lat and b1.lat. Otherwise return 0.
   - `ordered(a0,a1,b0,b1)`:
     `det = (f64(a1.lat)-a0.lat)*(f64(b1.lon)-b0.lon) - (f64(b1.lat)-b0.lat)*(f64(a1.lon)-a0.lon)`.
     If `det != 0`: `t = f64(b0.lat.wrapping_sub(a0.lat))` (**u32 wrap**, a C++ bug: when the area segment's west end is
     south of the way segment's west end, the crossing is effectively never found and only the midpoint test remains),
     `t += f64(a0.lon)/(f64(a1.lon)-a0.lon)*(f64(a1.lat)-a0.lat)`, `t -= f64(b0.lon)/(f64(b1.lon)-b0.lon)*(f64(b1.lat)-b0.lat)`,
     `x = t/det*(f64(a1.lon)-a0.lon)*(f64(b1.lon)-b0.lon)`. If `a0.lon<x<a1.lon && b0.lon<=x<=b1.lon`, return
     INTERSECT if `a0≠b0 && a1≠b1`, else 0. If `det == 0` (parallel): return HIT if
     `|a0.lat - (f64(a0.lon)-b0.lon)/(f64(b1.lon)-b0.lon)*(f64(b1.lat)-b0.lat) - b0.lat| <= 1`, or the same check with a1.
     Otherwise return 0.
   - `longitude_a(lat0, lon, lat1, b)`: if b is not vertical and `lon` is strictly inside b's lon range, take
     `y = (f64(lon)-b0.lon)/(f64(b1.lon)-b0.lon)*(f64(b1.lat)-b0.lat)+b0.lat` and return INTERSECT if y is strictly between lat0 and lat1.
     If b is vertical at the same lon and the open lat intervals overlap, return HIT. Otherwise return 0.
- Net effect (verified): a way is inside if it has a vertex strictly inside, or it properly crosses into the interior.
  It is outside if it only touches the boundary: at a vertex, along an edge, ending on an edge, or being the ring itself.

## 5. Implicit areas (closed ways) — `data/tilewise_geometry.h`
A way qualifies if `nds` is non-empty and `nds.first == nds.last` (node ids). There are no tag checks (`area=no`,
highway rings etc. all qualify) and no validity checks. Its geometry comes from current node
positions. If any node is missing, the way is ignored. Every qualifying way is its own polygon (even-odd within that way).
A point is inside the *union* if it is inside any of the ways.

### 5.1 Segmentation into node tiles
`Segment` stores `(latW, lonW, latE, lonE)`, with W = the endpoint with the smaller lon. If the lons are equal, W = the second endpoint.
For each consecutive vertex pair (p,q) of the way (`make_entries` 188-215):
- If they share a node tile, push the segment there.
- Else, let `e = touched_index(p,q)` (117-146). Tile coordinates are `(ilat>>16, ilon>>16)` with an arithmetic shift. If the points are in the same lat row and
  their columns differ by ≤1, or in the same column and their rows differ by ≤1, then `e = 0`. If they are diagonal neighbours: `lonB = max(p.lon,q.lon) & 0xffff0000`,
  `latB = (u32)((f64(q.lat)-p.lat)*(lonB-p.lon)/(q.lon-p.lon) + p.lat)`. Then `missing_lon` = (if `p.lat<q.lat`:
  `latB < (q.lat & 0xffff0000) ? q.lon : p.lon`, else `latB < (p.lat & 0xffff0000) ? p.lon : q.lon`), and
  `e = node_idx(latB, missing_lon)`. Anything else gives `e = 0xff`.
  - If `e ≠ 0xff`: push the **unsplit** segment into tile(p), into tile(q), and into e if `e ≠ 0`.
  - If `e = 0xff`, run `aux(p,q)` (218-330). If p and q are in the same lon column (`lon & 0xffff0000` equal): `sn(p→q)`. Otherwise, with `gc =
    great circle(p,q)` (`core/geometry.h:603-651`, `lat_of(lon)` in degrees) and W/E as the west/east point
    (case |Δlon| < 180°): `x = W.lon`, `y = ilat(gc.lat_of(deg((x&0xffff0000)+0x10000)))`, `sn(W → (y, (x&0xffff0000)+0x10000))`.
    Then for `x = (x&0xffff0000)+0x10000` while `x+0x10000 < E.lon`: `y' = ilat(gc.lat_of(deg(x+0x10000)))`,
    `sn((y,x) → (y', x+0x10000))`, `y = y'`, `x += 0x10000`. Finally `sn((y,x) → E)`. Here `ilat(f64)` uses the §1 rounding.
    The Δlon == 180° and antimeridian (Δlon > 180°) branches route via ±90° or ±180° and push a marker segment
    `(a,1800000000)→(a,1800000001)`. Transcribe 255-328 verbatim if you need them.
  - `sn(A→B)` (333-371): if both are in the same lat row, push into `node_idx(A.lat, A.lon)`. Else order the points S→N, `m = min(S.lon,N.lon)`,
    `y = (S.lat&0xffff0000)+0x10000`, `x(y) = (i32)(S.lon + (f64(y)-S.lat)/((i32)N.lat-(i32)S.lat)*(N.lon-S.lon))`. Push
    `S→(y,x(y))` into `node_idx(S.lat, m)`. While `y+0x10000 < N.lat`, push `(y,x)→(y+0x10000, x(y+0x10000))` into `node_idx(y, m)`
    and advance. Push `(y,x)→N` into `node_idx(y, m)`. Pieces here **share** boundary points (unlike §3.3).

### 5.2 Tile state and point classification
Each tile and way entry has `segments` and `sw_is_inside` (initially false). Tiles are processed in ascending id order.
After a tile is processed, for each way entry: if `eastern_sw(entry)` holds, set (and create if absent) the entry of tile
`node_idx(south, west+0x10000)` to `sw_is_inside = true`. In the westmost column (`west < -1800000000`), also do the same
northwards, toggling for segments with `lonW == -1800000000 && lonE != -1800000000` (651-662, 959-975).
`eastern_sw`: start from `sw`. For each segment with `(latW<=south) != (latE<=south)`,
`xs = lonW + (f64(south)-latW)*(lonE-lonW)/((i32)latE-(i32)latW)`. If `west <= xs < west+0x10000`, toggle.
A tile/way entry may therefore exist with no segments. In that case the tile is fully inside, through the propagated flag. (The is_in variant skips propagation outside the
query rows. This is only an optimisation, except as noted in §8.)

`relpos(P, entry, south, west)` → INSIDE/OUTSIDE/BORDER (665-810). This is the parity of the path SW corner → east along `lat=south` →
north to P. Let `y(lon) = latW + (f64(lon)-lonW)*((i32)latE-(i32)latW)/(lonE-lonW)`:
```
inside = entry.sw
for s in entry.segments:
  if s.latW > south and s.latE > south:                          // (A) north of the south line
     if s.lonW == P.lon: { if s.latW == P.lat: BORDER
                           if s.lonW == s.lonE: { if P.lat in [min,max] of lats: BORDER }
                           else inside ^= (s.latW < P.lat) }
     elif s.lonE == P.lon: { if s.latE == P.lat: BORDER }
     elif s.lonW < P.lon < s.lonE: { v=y(P.lon); if v-.5 < P.lat: { if P.lat < v+.5: BORDER; inside=!inside } }
  elif (s.latW <= south) != (s.latE <= south):                    // (B) meets the south line
     xs = lonW + (f64(south)-latW)*(lonE-lonW)/((i32)latE-(i32)latW)
     if west <= xs < west+0x10000:
        if s.latW <= south:
           if s.lonE <= P.lon: { if s.lonE == P.lon: { if s.latE == P.lat: BORDER
                                                      if s.lonW == s.lonE and P.lat <= s.latE: BORDER }
                                 inside = !inside }
           elif xs < P.lon: { v=y(P.lon); if P.lat < v+.5: { if v-.5 < P.lat: BORDER; inside=!inside } }
        else:
           if xs <= P.lon: { if xs == P.lon: { if P.lat == south: BORDER
                                               if xs == s.lonW and P.lat <= s.latW: BORDER }
                             inside = !inside }
           elif s.lonW <= P.lon: { v=y(P.lon); if v-.5 < P.lat: { if P.lat < v+.5: BORDER; inside=!inside } }
     else: same as (A) but without the vertical sub-case
  elif s.latW == south and s.latE == south:                       // (C) on the south line
     if P.lat == south and s.lonW <= P.lon <= s.lonE: BORDER
     if s.lonW == -1800000000: inside = !inside
  // segments wholly south of the line are ignored
return inside ? INSIDE : OUTSIDE
```
Point query against the union (`rel_pos_ilat_ilon` 1002-1023): if P is not inside the current tile (`P.lat-south ∉ [0,65535]`
or `P.lon-west ∉ [0,65535]`), return OUTSIDE. Otherwise, for each way entry in ascending order: INSIDE returns INSIDE. BORDER returns BORDER
only if `accept_border` is set, otherwise the loop continues. If nothing matched, return OUTSIDE.

Segment vs entry (`rel_position(idx, seg, entry)` 813-880), returning INSIDE/OUTSIDE:
```
touch = []
for a in entry.segments with a.lonW <= seg.lonE and seg.lonW <= a.lonE:
   sAW = (f64(a.latW)-seg.latW)*(seg.lonE-seg.lonW) - (a.lonW-seg.lonW)*(f64(seg.latE)-seg.latW)   // i32 diffs then f64
   sAE = same with a.latE/a.lonE;  if sAW*sAE > 0: continue
   sSW = (f64(seg.latW)-a.latW)*(a.lonE-a.lonW) - (seg.lonW-a.lonW)*(f64(a.latE)-a.latW)
   sSE = same with seg.latE/seg.lonE; if sSW*sSE > 0: continue
   if sAW == 0: touch += a.W; if sAE == 0: touch += a.E
   elif sAE == 0: touch += a.E
   elif sSW == 0: touch += seg.W
   elif sSE != 0: return INSIDE                                  // proper crossing
   // (sSE == 0 alone adds nothing)
if touch: touch += seg.W, seg.E; sort by (lat, lon); dedup
   for consecutive (p,q): m = ((u64(p.lat)+q.lat)/2, (i64(p.lon)+q.lon)/2)    // integer division
      if relpos(m, entry, south, west) == INSIDE: return INSIDE   // m is not tile-checked
return OUTSIDE
```
Way vs union (909-932): for each of the way's segments in this tile, in order: if the W endpoint is INSIDE (with
`accept_border=false`), or the E endpoint is, return INSIDE. Otherwise, for each area entry, if `rel_position(seg, entry)` is not OUTSIDE, return it.

### 5.3 Observed semantics (fixtures `osm-3s_testing/input/area/*.ql`, `expected/area/*.out`; all verified)
- Nodes (`node(area)`): border **included**. For square way 101, the vertices and the nodes on its edges are all returned (`way_101.out`).
- Relations: node members on the border **excluded**. Rel 10104 (only a border node) is excluded, while 10105 (border+inside) is included
  (`way_101_r.out`). Way members follow the way rule.
- Ways: inside if a vertex is strictly inside or a segment properly crosses. Ways that only touch (one corner, one vertex, an endpoint on an edge, or lying
  entirely on the boundary) are excluded (`way_101_w.out`: 10101-10104 and 10151-10153 in, 10161-10163 and 10171 out).
- A closed way is normally *not* inside itself, but tile-splitting rounding makes some shapes self-contained
  (verified: of the fixture ways, only 313 and 323 return themselves for `way(N);way(area)`).
- Antimeridian-crossing rectangles 204/205 work (`is_in_204_205`, `way_204/205`).

## 6. The `(area)` filter (`statements/area_query.cc`)
Syntax: `(area)`, `(area.s)`, `(area:ID)`, `(area.s:ID)`. The XML form is `<area-query from="_" ref=""/>`. `ref` is parsed with `atoll`.
If it is ≤0 and not empty, this static error is raised: `For the attribute "ref" of the element "area-query" the only allowed values are positive integers.`
The ref is truncated to u32. It cannot be standalone in QL: `node(area)` etc. are queries. XML `<area-query/>` standalone returns **nodes only**.
Its QL dump is `node(area...)`.

Polygon sources:
- **Without ref**: every Area_Skeleton in the input set (ids plus their `used_indices`, taken from the set, not re-read), plus every
  closed way in the input set (implicit). Relations and open ways in the input are ignored (verified: `rel(1);node(area)` → empty).
- **With ref**: the DB area with that id. Its tiles are read from the AREAS file by a full scan, and an unknown id gives nothing. **Quirk**: the input
  set (default `_`) is *still* fetched, and its closed ways also act as implicit areas (line 265 is unconditional).
  Verified: `way(950); node(area:3600000001)` returns way 950's nodes even though 950 lies outside that area.
  Node, way and relation candidates are range-restricted to the DB area's block tiles, so the extra matches only happen inside those tiles.

Per element type (expensive filter, 262-392):
- Nodes: those inside an implicit area with `accept_border=true` are kept. The rest are tested against the DB areas with
  `add_border=true` (§4.2). Result = union.
- Ways: implicit areas use §5.2 (ways_contained_in). The rest use the DB areas with §4.3.
- Relations: first a coarse pre-filter by ranges. Then node members (current node positions) are tested with border **excluded** for implicit areas
  and `add_border=false` for DB areas. Way members are tested as ways. A relation is kept if ≥1 node member or ≥1 way member
  passes. Relations in the input set are not used as polygons.
- Areas in the filtered query (`area(...)(area.x)`) are **not filtered** (TODO in code; verified: `area[name](area.a)` returns
  every named area).
- Candidate ranges (coarse, no semantic effect apart from the ref quirk): block-tile node ranges `[t, t+0x100)` of the DB areas,
  plus (from input only) the index ranges covered by all input ways. Ways and relations whose indices fall outside the parents of those ranges are
  dropped early.
- Strategy hint only: with fewer than 12 tiles (ref) or ≤12 used indices (input), ranges are preferred, otherwise ids.

## 7. `area` queries and area ids (`statements/query.cc`, `statements/id_query.cc`)
- `area…` / `<query type="area">` has type = `QUERY_AREA | QUERY_WAY | QUERY_CLOSED_WAY` (query.cc:72-76). It returns **both** DB
  areas matching the filters and **closed ways** matching the same filters (tags of the way). Verified: `area[name]` returns
  way 900 (closed, `name=Closed`) and area 3600000001. Open ways never appear. Output order: ways before areas (§11).
- Area tag filters use AREA_TAGS_GLOBAL/LOCAL with the usual tag-filter semantics. A missing area file (ENOENT) is
  treated as "no areas" (query.cc:998-1003, 1257-1261).
- **No spatial filter applies to areas**: `bbox`, `around`, `poly`, `(area)` and the global `[bbox:]` do not filter areas
  (`data/bbox_filter.cc:102,252` TODO). Verified: `area(50,50,51,51)` returns every area in the DB, and only the closed
  ways inside the bbox. `area(bbox)` with no tag filter = **all areas**.
- "Filters too weak in query statement: specify in addition a bbox, a tag filter, or similar." is **not** emitted for the
  area half (the check at 1250 is unreachable since the 2022 `Id_Constraint` refactor). It *is* emitted by the closed-way half
  when the way half has no index-able constraint, e.g. `area(if:id()>3000000000)`: the runtime error is printed, then
  all areas still pass through the `if` (verified). `expected/area/area_bbox.err` still lists that error. That file is stale:
  the current binary prints only the way 602 output, with no error.
- `area(ID)` / `area(id:A,B,…)` in QL is a query with an id constraint (id-query is not standalone for `area`). It returns DB areas with
  those ids and **closed ways with the same numeric id**. Verified: `area(900)` → way 900, `area(2)` (open way) → nothing.
  Ids > u32::MAX are dropped for both.
- XML `<id-query type="area" ref=…/>` standalone (id_query.cc:409-413) returns areas with those ids and **any** way (current)
  with that id, without a closed check. Verified: the open way 2 is returned.
- `area._[…]` / `area.s` item-style filters select the areas *and the closed ways* of the input set.
- Evaluators: `type()` is `"area"` for areas, and `id()`, `t[...]`, `count_tags()` work. `count(areas)` is not a valid
  argument.

## 8. `is_in` (`<coord-query>`, `statements/coord_query.cc`)
Syntax: `is_in;`, `.s is_in;`, `is_in(lat,lon);`, `… ->.t;`. XML: `<coord-query from lat lon into/>`. If `lat` or `lon` is given, both
are parsed with `atof` and checked: `For the attribute "lat" of the element "coord-query" the only allowed values are floats between -90.0 and 90.0.`
(and the same for lon with -180.0 and 180.0). An empty attribute fails as well. lat=100 is the internal "absent" sentinel.

Input points: the (lat,lon) pair if given (the input set is then ignored), else every node of the input set (ways, relations and areas in the
input are ignored). Output = **DB areas** + **closed ways** (current) covering any input point:
- DB areas: for each point's block tile, evaluate every block in the tile (all areas). If any block returns HIT, the area is found.
  Otherwise XOR per (point, area), and the area is found if the result is ≠ 0. **Border counts.** The found ids are then looked up in AREAS under the
  parent indices of the point tiles. The output contains `Area_Skeleton`s, i.e. areas whose blocks exist but whose location record is missing are dropped.
- Closed ways: candidates are current closed ways whose index is a parent of a point's node tile, classified per §5.2 with
  `status != OUTSIDE` (**border counts**). Each way is reported once.
  - Verified: `is_in(10.25,20.25)` inside a multipolygon's hole returns the inner-ring **way** (closed) but not the area.
  - The lat/lon variant evaluates the first tile with id ≥ the point's tile, without checking equality. If the point's own tile
    has no entry, a later tile's entry is evaluated against the point (the L-path test is not tile-checked). This can produce
    rare false positives. The node-input variant checks the tiles exactly.
- Fixtures: `is_in(51.2501,7.1501)` → ways 101, 201, 202 (`is_in_101_et_al.out`). `node(10114);is_in` gives the same.

## 9. `map_to_area` (`statements/map_to_area.cc`)
Syntax: `map_to_area;`, `.s map_to_area->.t;`. XML: `<map-to-area from into/>`. Output:
- the areas from the AREAS file whose id ∈ {u32(way_id+2400000000) for input ways} ∪ {u32(rel_id+3600000000) for input
  relations} (full scan, missing ids ignored, nodes ignored);
- **plus** the closed ways of the input (passthrough, so they can serve as implicit areas). Relations do not pass through.
  Verified: `way(900);way(1);rel(1);rel(2);map_to_area` → way 900 + area 3600000001 (rel 2 has no area).

## 10. `pivot` (`statements/pivot.cc`)
Area id → element: `id < 2400000000` → node `id`; `2400000000 < id < 3600000000` → way `id-2400000000`;
`id > 3600000000` → relation `id-3600000000`. The exact values 2400000000 and 3600000000 map to nothing. Elements are looked up in
the current DB, and missing ones are skipped.
- `way(pivot[.s])` returns the pivot ways **plus all ways of the input set** (open or closed; verified: `way(1);…;way(pivot)`
  → ways 1 and 900). `rel(pivot)` returns the pivot relations. `nwr(pivot)` = the union per type. As a filter combined with others,
  the result is restricted to the pivot ids (ways: or input ways). Attic, areas and deriveds are cleared.
- **Quirk**: `node(pivot.s)` *alone* is the standalone pivot statement (`can_standalone("node")`). It returns pivot nodes **and** pivot
  ways (plus input ways) **and** pivot relations. Verified: `area(id:101,2400000900,3600000001)->.a;node(pivot.a)` → node 101,
  way 900, relation 1. Combined with another filter, `node(pivot.s)[…]` returns nodes only.
- XML `<pivot from into/>` is the standalone statement. QL has no bare `pivot;` (`Unknown type "pivot"`).
- Fixtures: `way(101);way(pivot)` → way 101 (input passthrough), and `way.b(pivot.a)` with an empty `.b` → empty.

## 11. Output of areas
Areas are printed after nodes, ways and relations, and before deriveds, in the same `out` (`statements/print.cc:832-853`). They are printed
as a derived-like object with no geometry and no metadata (print.cc:315-321):
- XML: `  <area id="N"/>` without tags, or `  <area id="N">` + `    <tag k=".." v=".."/>` lines + `  </area>`.
  `out ids|skel` → the id only. `out` / `body` / `tags` / `meta` / `geom` / `center` / `bb` → id + tags (no bounds, center or meta attributes).
  Tags are in key-sorted order (§3.2).
- JSON: `{\n  "type": "area",\n  "id": N` + optional `,\n  "tags": {\n    "k": "v", …\n  }` + `\n}`.
- CSV: `::type` = `area`, `::id` = id, tag columns. `::lat`/`::lon` etc. are empty.
- Order: `out` sorts by id, while `out qt` sorts by area index and then storage order. `limit` counts areas.
- `out count` adds the tag `areas` (between `relations` and `total`) **only if the query has area access** (area_level > 0),
  and `total` includes the areas (print.cc:758-773). Verified: `node(101);out count;` has no `areas` tag.

## 12. Area timestamp and area_level
- `area_level` (`scripting_core.cc:74-92`): 2 with `--rules`. Otherwise it is 1 if the parsed script contains `is_in`/coord-query,
  `(area)`/area-query, `map_to_area`, a query of type `area`, or `<id-query type="area">`. The flags are set at parse time (an unexecuted
  branch still counts). `pivot` does not set it. Otherwise it is 0.
- The header attribute is printed only if area_level > 0 and the value is non-empty. XML: `<meta osm_base="T" areas="A"/>`.
  JSON: `"timestamp_areas_base": "A",` after `timestamp_osm_base`.
- A = the first line of `area_version` in the area dispatcher's DB dir (dispatcher mode, area_level 1). In **`--db-dir` mode
  A = the osm_base timestamp** (bug at `dispatcher_stub.cc:280`, `area_timestamp = de_escape(timestamp)`; verified after a diff
  had advanced `osm_base_version` past `area_version`). With `--rules`, A = the osm_base timestamp.
- Missing area files: `is_in`, `(area)` and `map_to_area` fail with a runtime error (`open64: 2 No such file or directory …area_blocks.bin|areas.bin`),
  while `area` queries treat them as empty (verified). The C++ fixtures `touch area_blocks.bin area_tags_local.bin`.

## 13. Precision and quantization summary
- All tests are integer, at 1e-7° units. DB-area tile splits use **truncated linear interpolation** (`proportion`) and end at
  `boundary-1` / start at `boundary`. Points on cell boundaries can therefore classify differently from an ideal straight edge.
- DB-area point-on-edge = exact equality with the truncated i64 intersection lat (`check_area_block`). The ray goes south, so
  "inside" means that the west or east side parity is odd.
- Implicit areas: the border tolerance is `|lat - y| < 0.5` (f64), and tile-column crossings use **great-circle** latitudes
  rounded with `ilat()`. Within a column, row crossings use truncated linear interpolation.
- Midpoints use integer floor division (DB areas: tile-local; implicit: `(a+b)/2` on u64/i64).
- The u32 wrap in `ordered()` (§4.3) and the u32/i32 casts listed above must be reproduced for bit-identical results.

## 14. Quick reference: verified results on a synthetic DB
Relation 1 (multipolygon, name=Square) uses outer ways 1 (n1→n2→n3) and 2 (n3→n4→n1), with corners (10.0,20.0) (10.0,20.5) (10.5,20.5)
(10.5,20.0), and an inner closed way 3 spanning (10.2–10.3, 20.2–20.3). Rel 2 = way 1 only gives the parity remark (`Node 1`), so no area.
- `area[name=Square]` → area 3600000001, tags a, b, name, type (sorted). JSON type "area".
- `node(area.a)`: corners, all edge points (N/S/E/W edges), hole vertices and hole-edge points are in. The hole interior (10.25,20.25)
  and (9.9,20.1) are out.
- `way(area.a)`: in: E-W crossing (no vertex inside), N-S crossing, inner way, closed way 900 inside. Out: rings 1, 2 and 3, a way
  ending on the corner, a way lying along the south edge, a way ending on the south edge.
- `rel(area.a)` with single node members: on the south, north, east or west edge → in; hole vertex → in; hole edge → in; outer corner (10.0,20.0) → **out**;
  in the hole → out. With a way member lying along the border → out.
- `is_in(10.0,20.25)` → the area (border). `is_in(10.06,20.07)` → way 900 + every DB area covering the point.

## 15. Quirk checklist (all intentional fidelity targets)
1. `area` queries return closed ways too, and `area(ID)` returns the closed way with that numeric id.
2. bbox/around/poly/(area)/global bbox never filter areas, so `area(s,w,n,e)` = all areas.
3. `(area:ID)` also uses the closed ways of `_` (or `.s`) as implicit areas.
4. Relations are never implicit areas. The default rules create only relation areas.
5. Border: DB-area nodes in; DB-area relation node members depend on the block HIT parity; implicit relation node members out;
   ways that only touch are out in both models; `is_in` includes the border in both models.
6. `node(pivot.s)` standalone returns nodes, ways and relations. `way(pivot)` passes all input ways through.
7. `--db-dir` area timestamp = osm_base timestamp.
8. Area ids are u32 and wrap. Node-pivot areas use the raw node id.
9. Stale areas are never deleted. A failed `make-area` keeps any old area with that id.
10. `make-area` uses only the first pivot element. Missing nodes silently drop a way. Tiles > 393216 silently mean no area.
