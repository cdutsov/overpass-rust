# Element selection semantics (query, filters, recursion, block statements)

Target: the C++ Overpass API **0.7.62** at `../Overpass-API` (HEAD `a0db4f39`, source `7c32a7d5`).
Paths are relative to `src/overpass_api/` unless they start with `src/`. `file:line` refers to that tree.

Derived from `statements/{query,bbox_query,around,polygon_query,id_query,recurse,union,difference,foreach,complete,newer,user,item,osm_script,statement}.cc`,
`data/{bbox_filter,collect_members,way_geometry_store,tags_global_reader,filter_ids_by_tags,abstract_processing,regular_expression,utils}.*`,
`core/{index_computations.h,type_*.h,geometry.cc,datatypes.h,basic_types.h}`, `dispatch/resource_manager.cc`, `frontend/map_ql_parser.cc`.

Out of scope (other specs): attic/diff (`[date:]`, `[diff:]`, `retro`, `timeline`, `changed`, `compare`, all `attic_*` code paths), areas (`area` type, `(area)`, `is_in`, `map_to_area`, `make_area`, `pivot`), evaluators (`make`, `convert`, `(if:)`, `if`, `for`, `lrs_*` set-list operators), output formatting (`docs/spec/output.md`).
Where this spec says "the areas spec" it means the shared point-in-polygon machinery that `poly` reuses.

Conventions: `S` = the set a statement reads (`_` unless `.name` given), `R` = the set it writes (`_` unless `->.name`). "Exists" = present in the current database.

---

## 1. Data model

### 1.1 Element types and ids

| type | id type | notes |
|---|---|---|
| node | `u64` | ids ≥ 1 |
| way | `u32` (`type_way.h:37`) | ids > 2^32−1 cannot exist; `way(5000000000)` matches nothing (`id_query.cc:112` drops ids > `u32::MAX`) |
| relation | `u32` (`type_relation.h:56`) | same |

Relation members: `(ref: u64, type ∈ {NODE=1, WAY=2, RELATION=3}, role_id)` (`type_relation.h:36-43`). Way member refs are truncated to 32 bit when looked up (`ref32()`). Roles are an interned string table; role comparison is exact byte equality on the role string (`collect_members.cc:781`). A role string that never occurs in the database has no id: every role-restricted recursion on it returns ∅ (but see §12 quirk Q-R2).

Members/nodes that are referenced but do not exist are silently skipped by every recursion and by every spatial test (no error, no placeholder).

### 1.2 Coordinate quantization (exact; must be bit-identical)

All coordinates are stored as integers in units of 1e-7 degree (`index_computations.h:1218-1263`):

```
ilat(lat: f64) -> u32 = ((lat + 91.0) * 10000000.0 + 0.5) as u32          // add 91 first, then scale; truncate
ilon(lon: f64) -> i32 = (lon * 10000000.0 + (if lon > 0.0 { 0.5 } else { -0.5 })) as i32   // round half away from 0
lat(ilat: u32) -> f64 = (ilat as f64) / 10000000.0 - 91.0                  // index_computations.h:1289
lon(ilon: i32) -> f64 = (ilon as f64) / 10000000.0
```
Use exactly this f64 operation order when converting user-supplied doubles (bbox, around centre, poly vertices); boundary decisions depend on it.
Output prints `lat()`/`lon()` with `%.7f` (output spec). Because the f64 error is ≪ 0.5e-7, that equals exact decimal formatting of the integers: `lat = (ilat − 910000000)·1e-7`, `lon = ilon·1e-7` (zero prints `0.0000000`, never `-0.0000000`).

### 1.3 Node tile index (`Uint32_Index`, also called `ll_upper`)

```
ll_upper(ilat, ilon) = interleave16(ilat >> 16, (ilon as u32) >> 16) ^ 0x40000000
   interleave16(a, b): bit i of a -> bit 2i+1, bit i of b -> bit 2i   (i = 0..15)
ll_lower(ilat, ilon) = interleave16(ilat & 0xffff, (ilon as u32) & 0xffff)
```
(`index_computations.h:59-87, 1237`). Bit 31 is always 0 for nodes (ilat < 2^31). The XOR flips the sign bit of `ilon`, so ascending index order is monotone in signed longitude within a latitude band. A tile is 65536 × 1e-7° = 0.0065536° square.
A 16×16-tile *block* is `ll_upper & 0xffffff00` (0.1048576° square); `poly` works per block.

### 1.4 Way/relation index (`Uint31_Index`) — needed only to reproduce iteration order

`way index = calc_index([ll_upper of each existing node in nd order])` (`osm-backend/way_updater.cc:94`);
`relation index = calc_index(sorted-unique [ll_upper of existing member nodes] ++ [index of existing member ways])` (relation members ignored, `relation_updater.cc:551`).
`calc_index` (`index_computations.h:117-311`): empty input → `0xFE`. Otherwise compute the tile bounding box of all inputs (a compound input contributes its whole extent; input `0x80000080` → return `0x80000080`). If it is a single tile → that tile's `ll_upper`. Else, with `ilat_*`/`ilon_*` the 16-bit tile coordinates of the bbox corners, return the first matching level:

| test on both lat and lon (`(max & M) − (min & M) < D`) | result |
|---|---|
| M=0xfffe, D=4 | `(sw_corner & 0xfffffffc) \| 0x80000001` |
| M=0xfff8, D=0x10 | `(sw & 0xffffffc0) \| 0x80000002` |
| M=0xffe0, D=0x40 | `(sw & 0xfffffc00) \| 0x80000004` |
| M=0xff80, D=0x100 | `(sw & 0xffffc000) \| 0x80000008` |
| M=0xfe00, D=0x400 | `(sw & 0xfffc0000) \| 0x80000010` |
| M=0xf800, D=0x1000 | `(sw & 0xffc00000) \| 0x80000020` |
| M=0xe000, D=0x4000 | `(sw & 0xfc000000) \| 0x80000040` |
| otherwise | `0x80000080` |

`sw_corner` = interleaved lat_min|lon_min (already in `ll_upper` bit layout). Ordering of `Uint31_Index` is **not** plain `u32`: compare `(idx & 0x7fffffff, idx)` lexicographically (`basic_types.h:139`), so compound indexes sort right after the simple tile with the same low 31 bits.
The Rust storage uses a different cell scheme; this only matters for `foreach` order and `out qt`.

### 1.5 Way geometry used by spatial filters

`geometry(way)` = coordinates of its `nds` in order (duplicates kept). If **any** referenced node does not exist, the geometry is empty (`collect_members.cc:840-856`) and the way never matches `bbox`, `around` or `poly`, never contributes to an `around.set` source. (Large ways whose compound index has bit0 = 0 store their geometry at update time, skipping missing nodes; with consistent data both rules agree.)

---

## 2. Sets and the runtime stack

### 2.1 Set content, deduplication, ordering
A set holds nodes, ways, relations (plus areas and derived elements — other specs). Each type is a map `index → Vec<skeleton>`; whenever a set is stored (`Resource_Manager::swap_set`, `resource_manager.cc:457`) every vector is sorted by id. Union is by `(index, id)` (`abstract_processing.h:347`); since an element has one index, a set is a true set of `(type, id)`.
**Internal order** = nodes by `(ll_upper, id)`, ways by `(Uint31 key, id)`, relations by `(Uint31 key, id)`. Nothing in selection depends on order except `foreach` iteration (§14). `out` re-sorts (output spec).
A set that was never written and an empty set behave identically for every statement in this spec.

### 2.2 Frames
Sets live in a stack of frames (`Runtime_Stack_Frame`). Lookup of `name` = current frame, then parent, … (first hit wins). Every statement writes its result `R` into the **current (top) frame**, replacing it. Block statements (§13–15) push a frame and define how inner sets leak out. The root frame persists for the whole script.

### 2.3 Who reads/writes what

| statement | reads | writes |
|---|---|---|
| query `T.s1.s2[...](...)->.R` | `s1`,`s2` (item clauses), sets named inside criteria (`(r.x)`, `(around.x:…)`, `(w.x)`, `(>.x)`…) | `R` |
| standalone recursion `.S >->.R`, `node(w.S)->.R`… | `S` | `R` |
| item `.S->.R` | `S` | `R` (no-op if `S == R`, `item.cc:202`) |
| union / difference | see §13 | `R` + every set written inside (incl. `_`) |
| foreach `.S->.V{…}` | `S` | sets written inside; `V` ends **empty** (§14) |
| complete `.S->.V{…}` | `S` | `S` (accumulated!), `V`, sets written inside |

---

## 3. How a QL query clause becomes a statement (matters for global bbox and quirks)

`frontend/map_ql_parser.cc:1036-1270`. Given `TYPE.from CLAUSES ->.R`:

1. `.S;` / `.S->.R;` (no type) → **item** statement.
2. No clauses, no `.from`: if a valid global `[bbox:]` exists → query with only the global bbox (all elements of TYPE in it); else parse error `An empty query is not allowed`.
3. Exactly one `(...)` criterion, no `[...]`, no `.from`, and the criterion "can stand alone" for TYPE → the criterion's **own statement**, executed standalone:
   - `node(s,w,n,e)`, `node(around…)`, `node(poly:…)` (node only);
   - `node(ID)`, `way(ID)`, `rel(ID)`, `…(id:…)` (node/way/relation only);
   - recursion criteria `w r bn bw br way_cnt way_link` for every TYPE except `nwr` (`recurse.h:49`).
4. Everything else → **query statement** with one constraint per clause; a `.from` becomes an item constraint added first; a valid global bbox becomes an extra constraint (added in the constructor, `query.cc:86-90`).

**Global `[bbox:s,w,n,e]` applies only to query statements.** Standalone statements of rule 3, item, and standalone `> >> < <<` ignore it. `[bbox:…]; node(1); out;` returns node 1 even outside the bbox; `node(1)[name]` or `nwr(1)` would be filtered. If `s > n` the global bbox is invalid and silently never applied (`osm_script.cc:177-181`, `geometry.h` `valid()`).
`(bbox)`/`[bbox]` in the query text are replaced by the URL `bbox=w,s,e,n` parameter at the CGI layer before parsing (`frontend/user_interface.cc:205-233`).

Query TYPE values: `node way rel(ation) nwr nw wr nr derived area`. `derived` and `area` belong to other specs (`area` internally also selects closed ways, `query.cc:74`; on `derived` the tag filters `[~k~v]` are ignored, `query.cc:738-769`).

### 3.1 Criterion forms (inside `( … )`)

The criterion kind is the leftmost token of the parenthesised expression (`statement.cc:315-338`).

| form | kind | input set |
|---|---|---|
| `s,w,n,e` (first token numeric, top operator `,`) | bbox | – |
| `N` / `-N` (numeric, no `,`) | id | – |
| `id:N1,N2,…` | id | – |
| `around:R` / `around.S:R` / `around:R,lat,lon[,lat,lon…]` | around | `S` (default `_`) when no coordinates |
| `poly:"lat lon …"` | poly | – |
| `newer:"T"` | newer | – |
| `user:"a","b"`, `uid:1,2`, `user_touched:…`, `uid_touched:…` | user | – |
| `w`, `w.S`, `w:1,-1`, `w.S:2` | recurse way-node | `S` |
| `r`, `r.S`, `r:"role"`, `r.S:"role"` | recurse relation-member | `S` |
| `bn`, `bw`, `br` (+ `.S`, `:"role"` / `:pos` for `way(bn)`) | recurse backwards | `S` |
| `way_cnt:a`, `way_cnt:a-b`, `way_cnt:a-`, `way_link:…` (+ `.S`) | recurse counting | `S` |
| `>`, `>>`, `<`, `<<` (+ `.S`, e.g. `(>.S)`) | recurse constraint (never standalone) | `S` |
| `area…`, `pivot…`, `if:…`, `changed…` | other specs | |

Unknown leftmost token → parse error `Unknown query clause`.

---

## 4. Query statement: overall semantics

Result = all elements of the requested types that satisfy **every** clause (logical AND), written to `R`. For `nwr`/`nw`/`wr`/`nr` each type is evaluated independently with the same clauses. Clause order does not matter, except for the quirks in §4.2/§12.

### 4.1 Tag filters

`[...]` clauses are `has-kv` statements (`query.cc:1294-1402`, parser `map_ql_parser.cc:1043-1157`). QL string literals are unescaped **before** regex compilation: `\n`, `\t`, `\uXXXX` are decoded, any other `\c` becomes `c` (`frontend/tokenizer_utils.cc:30-51`). So `"\."` is the regex `.`, and `"\\."` is `\.`.

| QL | kind | element passes iff |
|---|---|---|
| `[k]` | key | it has key `k` (any value, also `""`) |
| `[k=v]` (v ≠ `""`) | key-value | it has tag `k` with value exactly `v` (byte equality) |
| `[k=""]`, `[k!=""]` | — | **clause silently dropped by the parser** (`map_ql_parser.cc:1104`); if it was the only clause → rule 2 of §3 |
| `[k!=v]` | not key-value | it does **not** have tag `k=v` (elements without `k` pass) |
| `[k~re]` | key + value regex | it has `k` and `re` matches its value |
| `[k~""]` | key | identical to `[k]` (empty `regv` is ignored, `query.cc:1357`) |
| `[k!~re]` | not key+regex | NOT (has `k` and `re` matches value); elements without `k` pass |
| `[!k]` | `[k!~".*"]` | it has no key `k` (`map_ql_parser.cc:1050`) |
| `[~kre~vre]` | key regex + value regex | ∃ one tag `(k', v')` with `kre` matching `k'` **and** `vre` matching `v'` |
| `[~kre!~vre]` | — | static error `In the element "has-kv" regular expressions on keys cannot be combined with negation.` |
| `[~kre=v]`, `[~kre]` | — | parse error `A regular expression for a key can only be combined with a regular expression as value criterion` |
| `…,i]` after a `~`/`!~` value | case-insensitive | `REG_ICASE` for the value regex and, in `[~kre~vre,i]`, also the key regex |
| key `""` (`[""]`, `[~""~"x"]`) | — | static error `For the attribute "k" of the element "has-kv" the only allowed values are non-empty strings.` |

Several filters on the same element are ANDed; several `[~kre~vre]` filters may be satisfied by different tags. `[a=b][a=c]` → ∅ (but see Q-T1). Keys and values are compared as raw bytes.

**Regex engine (C++)**: glibc POSIX `regcomp(REG_EXTENDED | REG_NOSUB [| REG_ICASE])` after `setlocale(LC_ALL, "C.UTF-8")`, matched with `regexec` = unanchored search, boolean result only (`data/regular_expression.h:43-72`). Two patterns bypass the library: exactly `.*` → always true (even for `""`), exactly `.` → true iff value non-empty. Compile failure → static error `Invalid regular expression: "<pattern>"`.
The Rust server uses the `regex` crate in its place. Known divergences: `\d`, `\D` are literal `d`/`D` in glibc ERE but digit classes in Rust; inside `[...]` a backslash is literal in POSIX (`[\.]` = `\` or `.`); glibc accepts back-references `\1` (Rust: error); glibc GNU escapes are `\w \W \s \S \b \B \< \> \` \'` (Rust supports the first six), every other `\c` is the literal `c`; `{` handling differs. POSIX classes are ASCII-only in the crate, while glibc in C.UTF-8 uses Unicode sets (`ü` and `٣` are `alnum`, non-ASCII digits are `alpha` and not `digit`, NBSP is `punct` and not `space`) and reads `upper`/`lower` as `alpha` under REG_ICASE; glibc `\w` is `[_[:alnum:]]` and `\s` is `[[:space:]]`, where the crate takes all marks and NBSP in; `[.c.]` and `[=c=]` take a single ASCII character only. The Rust server translates each class into the glibc set (`translate_ere`); `\b`/`\B` still use the crate word characters, which include combining marks. Case folding is Unicode-simple in Rust vs `towlower` in glibc C.UTF-8.

### 4.2 Execution strategy (informative; explains errors and Q-T1)

`query.cc:921-1290`. Each constraint reports a strategy `ids_required < ids_useful < prefer_ranges` (bbox: `prefer_ranges` if `(n−s)·|e−w| < 1` else `ids_useful`; around: `prefer_ranges` if radius < 2000 m else `ids_useful`; poly: `prefer_ranges` unless its bbox area > 1 deg²; id, item, recursion, user-touched: `prefer_ranges`; user-last, newer: `ids_required`). `check_keys_late = max(all)` (starting at `ids_required`).
Per element type:
1. **Global tag index** (`tags_global_reader.h:764-813`): if there is any `[k=v]`, or (`check_keys_late != prefer_ranges` and any `[k]`, `[k~re]`, `[~k~v]`) → collect matching ids (intersection over those filters), then remove ids that carry a negated tag (`[k!=v]`, `[k!~re]`). If only negative filters exist and `check_keys_late != prefer_ranges` → id list = "all except those having the negated tags". A positive id list that is empty → result ∅ immediately.
2. Constraints may *collect* data directly (item, recursion) or contribute index ranges (bbox, around, poly, id, user-last, recursion) and id lists (id, user-touched).
3. Elements are read from the intersection of ranges, then filtered by all constraints' cheap and expensive filters; if `check_keys_late == prefer_ranges` the local tag index re-checks **all** tag filters (`query.cc:888-905`).

The result equals the AND of all clause predicates, except:

**Q-T1** (`tags_global_reader.h:302`, `query.cc:480`): with `check_keys_late ∈ {prefer_ranges, ids_useful}` (i.e. the query also has a bbox/around/poly/id/recursion/item/touched clause) and a **first** `[k=v]` that matches ≥ 1 048 576 elements globally, the global id list is abandoned and only the local check runs; the local check keeps one value per key (`key_union[k] = last v`), so `[k=v1][k=v2](bbox)` behaves like `[k=v2](bbox)`. Irrelevant for small test databases; recommended Rust behaviour: plain AND (∅).

### 4.3 "Filters too weak"

`query.cc:1150-1192` (nodes, ways), `1250` (area). If for nodes or ways no constraint delivered data, the intersected ranges are unrestricted, and the id list is "all except an **empty** list", the statement emits the **non-fatal** runtime error
`Filters too weak in query statement: specify in addition a bbox, a tag filter, or similar.`
and then **continues with a full scan** of that element type (applying the remaining filters). Typical triggers: `node(newer:…)` alone, `way(if:…)` alone, `node[!k]`/`node[k!=v]`/`node[k!~re]` where no node carries the negated tag. Relations never trigger it (silent full scan). If the negated tag *does* exist, there is no message, just the full scan. Rust may reject instead of scanning, with a different error text, but must still emit an error in exactly these cases.

### 4.4 Item clauses inside a query
`node.a`, `nwr.a.b`, `way.a[x](…)`: the candidates are the elements of the named set(s) of the requested types; multiple items intersect (`item.cc:142-171`). A missing set acts as ∅.

---

## 5. `bbox` — `(s,w,n,e)`

Parsing: `atof` on each value. Static errors (statement not executed): `s`/`n` outside [-90,90] or empty, `n < s`, `w`/`e` outside [-180,180] or empty (`bbox_query.cc:150-195`; messages `For the attribute "s" of the element "bbox-query" the only allowed values are floats between -90.0 and 90.0.` etc., `The value of attribute "n" of the element "bbox-query" must always be greater or equal than the value of attribute "s".`). `w > e` is allowed and means the box crosses the antimeridian.

**Nodes** (integer test, `bbox_filter.cc:39-58`), inclusive on all sides:
```
S=ilat(s) N=ilat(n) W=ilon(w) E=ilon(e)
pass = S <= node.ilat <= N && ((W <= node.ilon <= E) || (E < W && (node.ilon >= W || node.ilon <= E)))
```
**Ways** (`Bbox_Filter::matches`, `bbox_filter.cc:106-125`) — on decoded f64 coordinates and the **unrounded** f64 bbox (so a node exactly on an edge given with >7 decimals can be in for the node test and out for the way test):
- empty geometry → false; one coordinate → `contains(p)`;
- else true iff some consecutive segment satisfies `intersects(a, b)`. A way crossing the box with no node inside matches.
- Rust: a way also passes when one of its nodes passes the integer node test above, checked before the segments. A node on an edge can decode 1e-14 outside the f64 box (`ilat/1e7 - 91.0`), and an edge given with more than 7 decimals rounds onto the node in the integer test; in both cases the way passes as its node does. The live C++ returns these ways as well. Member nodes of relations use the integer node test too. The ends of a segment taken across the antimeridian (below) do not count: C++ tests such a segment only by `intersects`, which for a box with `w <= e` passes only when the box holds the longitudes of both ends, so a way whose only segment runs from 175 to −170 does not match `(5,165,20,180)` although its node at 175 lies inside.
```
contains(p): s <= p.lat <= n && (if e >= w { w <= p.lon <= e } else { p.lon >= w || p.lon <= e })   // geometry.cc:48
intersects(from, to):                                                                                 // geometry.cc:60-112
  fl = from.lon; tl = to.lon
  if from.lon < 0 && to.lon - from.lon > 180 { fl += 360 }      // segment taken across the antimeridian
  if to.lon < 0 && from.lon - to.lon > 180 { tl += 360 }
  d = 0
  if from.lat < s { if to.lat < s { return false }; d = (tl-fl)*(s-from.lat)/(to.lat-from.lat) }
  else if from.lat > n { if to.lat > n { return false }; d = (tl-fl)*(n-from.lat)/(to.lat-from.lat) }
  if to.lat < s { tl += (fl-tl)*(s-to.lat)/(from.lat-to.lat) }        // uses fl before += d
  else if to.lat > n { tl += (fl-tl)*(n-to.lat)/(from.lat-to.lat) }
  fl += d
  lo = min(fl,tl); hi = max(fl,tl)
  if w <= e { if hi < 180 { return lo <= e && hi >= w }
              if lo > 180 { return lo-360 <= e && hi-360 >= w }
              return lo <= e && hi-360 >= w }
  if hi < 180 { return lo <= e || hi >= w }
  if lo > 180 { return lo-360 <= e || hi-360 >= w }
  return true
```
(Linear interpolation in lat/lon, not great circles.)
**Relations** (`bbox_filter.cc:151-196, 211-232`): match iff some existing member **node** satisfies `contains` (f64 test, not the integer one) or some existing member **way** satisfies the way test. Member relations are never considered. A relation whose only spatial members are sub-relations never matches.

Standalone `node(s,w,n,e)` = node test only (`bbox_query.cc:227-240`).

---

## 6. `around`

Syntax → attributes (`around.cc:388-438`): `(around:R)` / `(around.S:R)` → source = set `S` (default `_`); `(around:R,lat,lon)` → one point; `(around:R,lat1,lon1,…,latk,lonk)` (k ≥ 2) → open polyline. Wrong arity → parse error `around requires an odd number of arguments`; missing radius → `around requires the radius as first argument`.
Static errors: radius < 0 or empty (`…the only allowed values are nonnegative floats.`), lat/lon out of range (only the **last** polyline point is range-checked, `around.cc:493-498`).

**Constants** (`core/geometry.cc:2104`, `around.cc:530-700`):
```
DEG(x) = x * (acos(0)/90.0)            // used by great_circle_dist
R_M    = 10000000.0 / acos(0)          // = 2e7/π ≈ 6366197.724 m ("10 000 km per quarter meridian")
gcd(a, b):                             // great-circle distance in metres
  if a.lat == b.lat && a.lon == b.lon { return 0 }
  c = cos(DEG(b.lat - a.lat)) + cos(DEG(a.lat))*cos(DEG(b.lat))*(cos(DEG(b.lon - a.lon)) - 1)
  return acos(min(c, 1.0)) * R_M
cart(lat, lon) = (sin(lat/90.0*acos(0)), cos(lat/90.0*acos(0))*sin(lon/90.0*acos(0)), cos(lat/90.0*acos(0))*cos(lon/90.0*acos(0)))
line_dist(seg a→b, p) = asin(min(1, |cart(p)·n| / sqrt(n·n))) * R_M,   n = cart(a) × cart(b)   // distance to the great circle
seg_intersect(A, B):  ip = normalize(nA × nB); as = A.a+A.b; bs = B.a+B.b (cartesian)
                      return |as·ip| >= as·A.a && |bs·ip| >= bs·B.a          // around.cc:634
```
(Zero-length or parallel segments yield NaN → comparisons false.)

**Sources.** Points: the single point / every polyline vertex / for `.S`: every node, every vertex of every way (geometry §1.5), every existing member node and every vertex of every existing member way of every relation in `S` (sub-relations ignored). Segments: consecutive polyline vertices / consecutive vertices of those ways.

**Point test** `inside(p)` (`around.cc:873-903`):
```
for c in source points whose "radius box" covers tile(p):
    if (r > 0 && gcd(c, p) <= r) || (|c.lat-p.lat| < 1e-7 && |c.lon-p.lon| < 1e-7): return true
for each source segment (a,b):
    if line_dist((a,b), p) <= r:
        lim = sqrt(gcd(a,b)^2 + r^2)
        if gcd(p,a) <= lim && gcd(p,b) <= lim: return true
return false
```
Radius box of a source point (`add_coord`, `around.cc:698`): `dlat = r*360/4e7`; `south = lat−dlat`, `north = lat+dlat`; `scl = lat > 0 ? north : south`, clamped to 89.9 if `|scl| ≥ 89.9`; `dlon = dlat / cos(scl/90*acos(0))`; the point is registered for every node tile intersecting `[south,north]×[lon−dlon, lon+dlon]` (no antimeridian wrap). This box contains the whole circle except within ~0.1° of a pole or across the antimeridian, so `inside` ≈ "gcd ≤ r to some source point or within r of some source segment".
**Radius 0**: points match only if coincident within 1e-7° in both lat and lon; segment matches require `line_dist ≤ 0` (numerically rare). Ways still match via an exactly shared node position.

**Segment test** `inside(a→b)` (target way segments, `around.cc:906-932`): true if some source **point** c (all of them, no tile restriction) has `line_dist((a,b), c) ≤ r` and `gcd(c,a) ≤ lim` and `gcd(c,b) ≤ lim` with `lim = sqrt(gcd(a,b)^2 + r^2)`; or some source segment `seg_intersect(source, (a,b))`.

**Targets.** Node: `inside(p)`. Way: empty geometry → false; any vertex `inside` → true; else any consecutive segment `inside(a→b)`. Relation: some existing member node `inside`, or some existing member way matches as a way (`around.cc:268-311`).
Candidates are first restricted to tiles derived from the sources' tiles expanded by the radius (`expand`, `around.cc:103-123`); this prefilter is conservative except near poles/antimeridian. A missing or empty source set → ∅.
C++ restricts target ways and relations by their index (a coarse block that holds all their nodes), so a target whose segment reaches the radius only on its great circle, north or south of the box of its nodes, still matches (an edge 5 degrees long passes up to about 3 km off that box). Rust grows a stored box by the largest bulge its width allows (`Rect::arc_box`, `arc_ranges`) before it uses it.
Standalone `node(around…)` = node test (`around.cc:969-982`).

---

## 7. `poly` — `(poly:"lat lon lat lon …")`

Parsing (`polygon_query.cc:270-318`): whitespace-separated doubles read with `istream >>`. Errors: odd count or any unparsable token → `For the attribute "bounds" of the element "polygon-query" an even number of float values must be provided.` (note: **trailing whitespace after the last number also triggers this**, because the loop runs while the stream is `good()`); fewer than 3 pairs → `… at least 3 lat/lon float value pairs must be provided.`; lat/lon out of range → range errors. The ring is closed implicitly (last → first).

Geometry: vertices are quantized (§1.2); each edge (including the closing one) is cut into "aligned segments" at multiples of 0x100000 in ilon and then ilat with integer interpolation (`Area::calc_aligned_segments`, `core/type_area.h:64-216`). An edge whose endpoints satisfy `from_ilon < −90°` and `to_ilon > +90°` (or vice versa) is routed **across the antimeridian**, not through 0°. `add_segment_blocks` (`polygon_query.cc:157-207`) then adds horizontal segments at the southern edge of the next block north for every block whose segment endpoints have odd multiplicity, propagating northwards, so every block inside the polygon has segments. A node in a block without segments is outside.
Point test per block (`Coord_Query_Statement::check_area_block`, `coord_query.cc:90-183`, shared with areas): vertical ray to the south on integer coordinates; result `HIT` (point on a segment, endpoints inclusive), or XOR of toggle bits (`TOGGLE_EAST|TOGGLE_WEST` for a proper crossing south of the point; only one of them when the ray passes exactly through a segment endpoint).

- **Nodes** (top level, `add_border = true`): inside iff any segment of its block returns `HIT`, or the XOR of all segment results ≠ 0. Boundary counts as inside.
- **Ways** (top level `add_border = true`; as relation members `false`): same routine as area queries (`polygon_query.cc:399-528`, helpers `intersects_inner`/`has_inner_points` in `area_query.cc:707-818`): matches if a vertex is **strictly** inside (a vertex exactly on the border does not count), or a segment properly crosses the border, or a segment midpoint is strictly inside; with `add_border` also if a segment runs along / touches the border (`HIT`). Empty geometry never matches.
- **Relations**: match iff some existing member node passes the node test with `add_border = false` (inside iff XOR of all results ≠ 0, where `HIT` contributes bit 1 — so a node on an edge interior still counts, a node on a vertex shared by two edges cancels), or some existing member way passes the way test with `add_border = false` (`polygon_query.cc:93-127`, `collect_members.h:693`).

Standalone `node(poly:…)` = node test.

---

## 8. `id` — `(N)`, `(id:N1,N2,…)`

`node(123)`, `way(id:1,2,3)`, `nwr(5)`. Each value parsed with `atoll` and kept if `> 0` as `u64` (`id_query.cc:330-361`); list is sorted and deduplicated. `0` alone → static error `For the attribute "ref" of the element "id-query" the only allowed values are positive integers.` A leading digit or `-digit` makes the parser treat a criterion as `id` (one value) or `bbox` (comma list) (`statement.cc:322-325`), so `node(1,2)` is a malformed bbox; use `id:`.
Result: the elements of the requested type(s) with those ids that exist. For way/relation, ids > u32::MAX are dropped. Inside a query it intersects with other clauses; standalone it ignores the global bbox (§3).

---

## 9. `newer`, `user`, `uid`

**`(newer:"T")`** (`newer.cc:44-105, 151-163`): passes iff the element's **current** metadata timestamp `>= T` (inclusive). Elements without metadata fail. Only valid inside a query (elsewhere: static error `"newer" can appear only inside "query" statements.`). Never standalone, never restricts candidates (alone → §4.3).
Timestamp parsing (`core/datatypes.h:644-660`): string must have ≥ 19 chars with ASCII digits at positions 0-3, 5-6, 8-9, 11-12, 14-15, 17-18; separators and trailing characters are **not** checked; result packed as `year<<26 | month<<22 | day<<17 | hour<<12 | min<<6 | sec` (fields masked to 14/4/5/5/6/6 bits) and compared as an integer. Failure → static error `The attribute "than" must contain a timestamp exactly in the form yyyy-mm-ddThh:mm:ssZ.`

**User filters** (`user.cc:257-343`):

| QL | criterion | passes iff |
|---|---|---|
| `(user:"n1","n2",…)` | last | uid of the element's **current** version ∈ uids of those names |
| `(uid:1,2,…)` | last | uid of current version ∈ list |
| `(user_touched:"n",…)`, `(uid_touched:…)` | touched | some version (current or historic metadata) has uid in the set |

Names are matched exactly (case-sensitive) against the user table; all uids whose stored name equals any given name are used; unknown names → ∅ result. `uid` values via `atoll` → u32; zeros ignored; neither or both kinds given → static error `Exactly one of the two attributes "name" and "uid" must be set.` Elements without metadata fail. Areas are removed. Never standalone.

---

## 10. Item

`.S;` = copy `S` into `_`; `.S->.R;` copy into `R`; if `S` does not exist, `R` becomes ∅. **`.S->.S` / `._;` writes nothing** (`item.cc:202`). In a union this matters: `(._; …)` contributes the current `_` (§13).

---

## 11. Recursion statements

Input `S` = `.S` prefix or `_`. A missing input set → result ∅. Results contain only existing elements; duplicates removed.

### 11.1 Member/parent recursions (`(…)` criteria; standalone unless TYPE = `nwr`)

| QL | result |
|---|---|
| `node(w)` | nodes referenced by ways in S |
| `node(w:p1,p2,…)` | nodes at the given positions of each way: `p > 0` → `nds[p−1]` if `p ≤ len`; `p < 0` → `nds[len+p]` if `len+p ≥ 0` (−1 = last); out-of-range ignored; `0` → static error `Invalid pos '0' in positions list.` (`collect_members.cc:28-52`) |
| `node(r)` / `way(r)` / `rel(r)` | node / way / relation members of relations in S (the input relations themselves only if they are members) |
| `nw(r)`, `wr(r)`, `nr(r)`, `nwr(r)` | union of the respective member kinds |
| `…(r:"role")` | only members whose role string equals `role` exactly; `(r:"")` = empty role |
| `way(bn)` | ways containing ≥ 1 node of S |
| `way(bn:p…)` | ways whose node at position p (same position rules) is in S; for `bn` with TYPE `way` the `:` argument is a **position list**, so `way(bn:"x")` → `Invalid pos 'x' …` |
| `rel(bn)` / `rel(bw)` / `rel(br)` | relations having a node / way / relation member in S |
| `rel(bn:"role")`, `rel(bw:"role")`, `rel(br:"role")` | … with that role |
| `wr(bn)` | `way(bn)` ∪ `rel(bn)` (standalone only; see Q-R3) |
| `node(way_cnt:a)` / `(way_cnt:a-b)` / `(way_cnt:a-)` | nodes n with `a ≤ c(n) ≤ b` (b = ∞ if omitted, b = a if single), `c(n)` = number of distinct ways in S whose node list contains n (`collect_members.cc:128-154`) |
| `node(way_link:…)` | same bounds on `L(n) = 2·occ(n) − ends(n)`; `occ` = occurrences of n in all node lists of S (repeats counted), `ends` = number of times n is the first node of a way plus number of times it is the last node (a closed way's start node: occ 2, ends 2 → 2; one-node way → 0) (`collect_members.cc:157-199`) |

Static errors: `lower must be an integer greater or equal 1`, `upper must be an integer greater or equal lower`; `node(way_cnt)` without argument silently returns ∅. Wrong target type → parse errors `A recursion of type 'w' produces nodes.`, `… 'r' produces nodes, ways, or relations.`, `… 'bn' produces ways or relations.`, `… 'bw' produces relations.`, `… 'br' produces relations.`; restrictions on other kinds → `A recursion of type '<t>' cannot have restrictions.` For `r` only the **first** role after `:` is used.

### 11.2 `>`, `>>`, `<`, `<<` (standalone forms, `recurse.cc:1295-1452`)

Let N, W, Rl be the nodes/ways/relations of S.
- **`>`** (`collect_members.cc:1063-1085`): ways = member ways of Rl; nodes = nodes of (W ∪ member ways of Rl) ∪ member nodes of Rl; relations = ∅. Input elements are not included (unless reached as members).
- **`>>`**: `C` = closure of Rl under "relation member of" **including Rl itself** (`relations_loop`, `recurse.cc:450-470`: repeat `C ∪= relation members of C` until the count stops growing). Result relations = C; ways = member ways of C (input ways **not** included); nodes = member nodes of C ∪ nodes of (W ∪ member ways of C).
- **`<`**: ways = ways containing a node of N (input ways **not** included); relations = **Rl** ∪ relations with a way member in (W ∪ those ways) ∪ relations with a node member in N. Including Rl is new in 0.7.61+ (`recurse.cc:1439`; 0.7.60 excluded it). Relations that merely contain a relation of Rl are not included.
- **`<<`**: ways as in `<`; relations = upward closure (`relations_up_loop`, `recurse.cc:473-488`: repeat `C ∪= relations having a relation member in C`) starting from the `<` relation set (so Rl included).

### 11.3 Recursion criteria inside a query (`Recurse_Constraint`, `recurse.cc:556-1055`)
Used when combined with other clauses, a `.from`, or TYPE `nwr`. Semantics = standalone result ∩ requested type ∩ other clauses, **except**:
- **Q-R1** `node(>)`, `node(>>)`: only nodes that are members of the relations (closure for `>>`) or of their member ways. **Nodes of ways in S are not included** (`recurse.cc:515-532`, `962-997`), unlike standalone `>`/`>>`. `way(>)`, `way(>>)`, `rel(>>)` as standalone; `rel(>)` = ∅.
- `rel(<)`: relations with a way member in (W ∪ ways of N) or node member in N — **without** Rl (unlike standalone `<`). `rel(<<)` includes Rl and its upward closure. `way(<)`/`way(<<)` = ways containing a node of N. `node(<)`/`node(<<)` = ∅.
- **Q-R2** role that does not exist in the database: the data-collection step yields ∅, but the filter step skips filtering (`recurse.cc:842`); if another clause collected the candidates (an item clause, e.g. `node.a(r:"nosuchrole")`), the result is all of them instead of ∅.
- **Q-R3** `wr(bn)` combined with anything (e.g. `wr(bn)[name]`) → always ∅ (type `node-wr` is not handled by the constraint, `recurse.cc:780-825`).

---

## 12. Collected quirk checklist (for oracle comparisons)
- Q-T1 same-key multi-value collapse with ≥ 1 048 576 globally matching first `[k=v]` (§4.2).
- `[k=""]`/`[k!=""]` dropped; `[k~""]` = `[k]`.
- Global bbox ignored by standalone statements (§3).
- Node bbox test uses rounded integers, way/relation tests unrounded doubles (§5).
- "Filters too weak" is a non-fatal message followed by a full scan (§4.3).
- `poly` string with trailing whitespace → static error.
- Standalone `<` includes input relations; `rel(<)` does not; `node(>)` excludes nodes of input ways (Q-R1); Q-R2; Q-R3.
- After `foreach`, the loop-variable set is empty; sets written in union/foreach/complete bodies (including `_`) leak out (§13-15).

---

## 13. Union `( … );` and difference `( A; - B; )`

Union (`union.cc:66-82`, frame ops `resource_manager.cc:236-295`):
```
union(stmts, R):
  F = push_frame()
  F[R] = take(R):  find the nearest frame (starting at the caller's frame) holding R; if it is the caller's
                   frame, MOVE it (caller[R] becomes ∅), else COPY it (∅ if nowhere)
  for st in stmts: run st (writes go to F, reads fall back outward); caller[R] ∪= lookup(st.result)
  for (name, set) in F, name != R: caller[name] = set      # leak, overwriting the caller's sets
  pop F
```
Consequences: the result is exactly the union of the substatements' results. Each substatement sees the sets as left by the previous substatements **inside** the union (e.g. in `node(1); (way(bn); node(w););` the `node(w)` reads the ways just produced, and node 1 is in the result only if it belongs to one of those ways). Inside the union `R` initially holds its pre-union value (`(._; way(bn);)` = old `_` ∪ its parent ways). Any set assigned inside leaks out; in particular with a named result `(…)->.c;` the caller's `_` is replaced by the inner `_` (the last value written to `_` inside). A substatement without a result (e.g. `out`) → static error. `newer` → static error.

Difference (`difference.cc:62-91`): no initial move of `R`.
```
F = push_frame(); run A; caller[R] = copy(lookup(A.result))
run B; caller[R] = caller[R] − lookup(B.result)       # by (type, id)
for (name,set) in F, name != R: caller[name] = set; pop F
```
More than two substatements → static error `A set difference always requires exactly two substatements: …`. With one substatement the result is `A`.

---

## 14. `foreach.S->.V { … }`

`foreach.cc:103-128`, default `S = V = _`.
```
saved = caller[V]; caller[V] = ∅                     # V is emptied in the caller's frame first
F = push_frame()
src = (S == V) ? saved : lookup(S)                   # snapshot; body writes never change src
for elem in order(src):                              # nodes, ways, relations (then areas, deriveds); see §2.1
    F[V] = { elem }
    run body                                          # other sets in F persist across iterations
for (name,set) in F, name != V: caller[name] = set   # leak (e.g. `_` if V != `_`)
pop F
```
After the loop `V` is **empty** in the caller (e.g. `node(1); foreach{}; out;` prints nothing; `foreach->.e{…}` leaves `_` as last written inside the body). Iteration order = internal set order (§2.1): nodes by tile index then id, etc. — not globally by id. `newer` in the body → static error.

---

## 15. `complete(N).S->.V { … }`

`complete.cc:69-93`, default `S = V = _`, `N = 4096` (static error unless 1 ≤ N ≤ 1048576).
```
F = push_frame(); cnt = 0
loop {
  F[V] = copy(lookup_from_caller(S))     # current accumulated S
  run body
  grown = (caller[S] ∪= lookup(S))       # lookup from F: the body's S if it wrote one, else the caller's S
  if !grown || ++cnt > N { break }       # at most N+1 body executions
}
F[V] = copy(lookup_from_caller(S))
for (name,set) in F: caller[name] = set  # all, including V
pop F
```
So the body sees `V` = everything accumulated so far and must leave the new elements in `S`; iteration stops at the fixpoint. When `S != V`, the caller's `S` is also updated to the accumulated set. "Grown" counts any element type.

---

## 16. Errors and resource limits

### 16.1 Static vs runtime
Static errors (bad attribute values listed above) prevent execution of the whole script (parser/dispatcher spec). Runtime errors from selection statements: only "Filters too weak" (non-fatal, §4.3) and the resource errors below (fatal: execution stops; output already produced stays, footer still written — output spec §1).

### 16.2 Settings (`osm_script.cc:33-119`)
`[timeout:T]` default `180` (seconds), `[maxsize:M]` (= `element-limit`) default `536870912` bytes. Both parsed by `strtoll(…, base 0)` (so `0x…` hex and leading-`0` octal work), optional suffix `ki`/`Mi`/`Gi` (×2^10/2^20/2^30) or `k`/`M`/`G` (×10^3/10^6/10^9), optionally preceded by one space; any other suffix or value ≤ 0 → static error `For the attribute "timeout" of the element "osm-script" the only allowed values are positive integers optionally with suffix "ki", "Mi", or "Gi".` (resp. `"element-limit"`).

### 16.3 Checks (`resource_manager.cc:632-697`)
`health_check` runs at the end of almost every statement (not after `item`) and periodically inside long loops (tag-index scans, element reads, recursion collection). It fails when
- **timeout**: `elapsed > timeout`, `elapsed = wall-clock seconds since the request started` (`time(NULL)` granularity), or
- **oversize**: `size > maxsize`, with
  `size = Σ over all frames, all live sets: eval_set(set) + transient`,
  `eval_set = Σ_types Σ_index-buckets (64 + count·w)`, `w` = 8 (node), 128 (way), 192 (relation), 128 (area); derived elements are not counted (`resource_manager.cc:27-102`). `transient` = the same formula applied to the partial result currently being read (only inside reads). Sets are re-measured when stored/moved; `out` buffers are not counted.

Messages (prefixed `runtime error: ` by the output layer):
- timeout: `Query timed out in "<stmt>" at line <n> after <s> seconds.`
- oversize (HTTP, `dispatch/web_query.cc:177`): `Query ran out of memory in "<stmt>" at line <n>. It would need at least <size/2^20> MB of RAM to continue.`
- oversize (CLI `osm3s_query`, `osm3s_query.cc:324`): `Query run out of memory in "<stmt>" at line <n> using about <size/2^20> MB of RAM.`

`<stmt>` is the statement name (`query`, `recurse`, `union`, `foreach`, `complete`, `item`, `bbox-query`, `around`, `polygon-query`, `id-query`, `print`, …). Exact parity of *when* an oversize/timeout triggers is not achievable; the Rust engine should apply the same accounting at statement boundaries so that clearly-too-large queries fail with an equivalent message. The dispatcher may also refuse to start a query under load (server spec).

---

## 17. Worked examples (expected C++ results)

Assume a database where way 1 has nodes 10, 11, 12; relation 5 has members node 20 (role `stop`), way 1 (role `""`), relation 6; relation 6 has member way 2 (nodes 30, 31); relation 7 has member relation 5. All exist.

| script | result (`out` prints) | why |
|---|---|---|
| `way(1); >; out;` | nodes 10,11,12 | §11.2 `>` |
| `way(1); node(>); out;` | nothing | Q-R1 |
| `rel(5); >; out;` | node 20 (member), nodes 10-12 (of way 1), way 1 | ways only from relations; rel 6 not included |
| `rel(5); >>; out;` | rels 5, 6; ways 1, 2; nodes 10-12, 20, 30, 31 | closure incl. input |
| `node(20); <; out;` | rel 5 | no ways contain node 20 |
| `rel(5); <; out;` | rel 5 (input kept) | 0.7.61+ behaviour; rel 7 **not** included |
| `rel(5); rel(<); out;` | nothing | in-query `<` excludes input relations; nodes/ways of input are none |
| `rel(5); <<; out;` | rels 5, 7 | upward closure incl. input |
| `rel(5); node(r:"stop"); out;` | node 20 | role match |
| `rel(5); way(r:""); out;` | way 1 | empty role |
| `rel(5); node(r:"nosuchrole"); out;` | nothing | unknown role |
| `node(10)->.a; rel(5); node.a(r:"nosuchrole"); out;` | node 10 (should logically be ∅; with `r:"stop"` it is ∅) | Q-R2 |
| `node(10); way(bn); out;` | way 1 | |
| `node(12); way(bn:1); out;` / `way(bn:-1)` | nothing / way 1 | positions |
| `way(1); node(w:1,-1); out;` | nodes 10, 12 | |
| `node(10); foreach { }; out;` | nothing | loop variable `_` emptied |
| `node(10); foreach->.e { way(bn.e); }; out;` | way 1 | body's `_` leaks out |
| `(node(10); way(1);)->.c; out;` | way 1 | inner `_` leaks; `.c` = node 10 + way 1 |
| `node(10); (._; way(bn););` then `out;` | node 10, way 1 | `_` pre-loaded into the union frame |
| `(way(1); - node(10););` | way 1 | difference by (type,id) |
| `rel(5); complete { rel(br); }; out;` | rels 5, 7 | fixpoint of parents |
| `[bbox:0,0,1,1]; node(10); out;` (node 10 outside) | node 10 | standalone id query ignores global bbox |
| `[bbox:0,0,1,1]; node(10)[name]; out;` | nothing | query statement applies it |
| `node[name=""]; out;` | parse error `An empty query is not allowed` | clause dropped |
| `node(around:0,LAT,LON)` with node at exactly LAT/LON | that node | 1e-7° coincidence rule |

---

## 18. Implementation guidance (non-normative)

The C++ planner (§4.2) is an optimisation; the observable semantics are "AND of exact clause predicates" plus the listed quirks. A Rust engine can therefore:
1. Build the candidate set per element type from the cheapest clause: an item clause (`.from`) or recursion result (exact), an id list, a positive tag-index lookup, or the spatial cells of bbox/around/poly/user-last. If none applies, raise the §4.3 error (same trigger condition: no item/recursion clause, no spatial/id/user clause, and no positive tag filter — negative tag filters count as "none" only if they match nothing).
2. Apply every clause as an exact predicate: tags (§4.1), bbox (§5), around (§6), poly (§7), id (§8), newer/user (§9), item membership (§4.4), recursion membership (§11.3 rules, including Q-R1).
3. Store the result sorted by `(cell, id)` or simply by id; only `foreach` order (§14) and `out qt` observe the difference.

Predicates that need extra data:
- way geometry for bbox/around/poly (all nodes must exist, §1.5);
- relation member nodes and member-way geometries for bbox/around/poly on relations (members of type relation never count);
- current metadata (timestamp, uid) for `newer`/`user`, historic metadata for `*_touched`;
- the role string table for role-restricted recursion.

Decide explicitly (and record in the compat notes) whether to reproduce or fix: Q-T1, Q-R1, Q-R2, Q-R3, the `<` input-relation rule, the foreach/union set-leak rules (these two are relied on by real scripts — reproduce them), the poly trailing-whitespace error, and the "too weak" full scan.
