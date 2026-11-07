# Output format specification (XML, JSON, CSV)

Target: reproduce the byte stream of the C++ Overpass API **0.7.62** (`source_hash` `7c32a7d5d2eb05d3078b2b6736d177ed7d71cd69`).
Derived from `statements/print.cc`, `output_formats/output_{xml,json,csv}*.cc`, `frontend/{web_output,console_output,basic_formats,user_interface,cgi-helper}.cc`, `dispatch/{web_query,osm3s_query,dispatcher_stub,scripting_core}.cc`, `data/{geometry_from_quad_coords,way_geometry_store,relation_geometry_store,tag_store}.cc`, `core/geometry.cc`, `expat/escape_{xml,json}.cc`.

Out of scope: `out:popup`, `out:custom`, `[diff:]`/`[adiff:]` (action/old/new blocks, `visible=` attributes), attic (`[date:]`) specifics, the XML query language, `--dump-*` modes, `--clone`.

Note on test fixtures: `osm-3s_testing/expected/osm3s_query_*` (CLI, XML) and `output_csv_*` (HTTP CGI, CSV) exist; there are no `print_*` fixtures (print tests are generated on the fly by `generate_test_file`). The osm3s_query test runner normalizes the generator with `sed 's/Overpass API [^ ]* [a-f0-9]*/Overpass API/g'`, so fixtures show `generator="Overpass API"`.

---

## 0. Conventions

* Output is a single byte stream on stdout. Line terminator is always `\n` (never `\r\n`).
* Strings from the database (tag keys/values, roles, user names) are raw bytes (normally UTF-8). They are emitted unchanged apart from the escaping rules in §4. No Unicode validation/normalization, no `\uXXXX` escapes anywhere.
* Constants:
  * `VERSION = "0.7.62"`, `HASH8 = "7c32a7d5"` (first 8 chars of source hash)
  * `GENERATOR = "Overpass API " + VERSION + " " + HASH8` → `Overpass API 0.7.62 7c32a7d5`
  * The Rust server prints `Overpass API 0.7.62 compatible` in place of this generator: the one intended difference in the bytes of an answer. The examples below show the C++ value.
  * `COPYRIGHT = "The data included in this document is from www.openstreetmap.org. The data is made available under ODbL."`
* In this document `␤` is never used; literal blocks show exact bytes, an empty line in a block is an empty line in the output.

---

## 1. Output handler model and lifecycle

Every query has exactly one output handler (format), chosen at parse time by `[out:xml]` (default), `[out:json]`, `[out:csv(...)]`. Unknown name → parse error `Unknown output format: <name>`. The handler exposes:

| call | XML | JSON | CSV |
|---|---|---|---|
| `http_headers()` | `Content-type: application/osm3s+xml` | `Content-type: application/json` | `Content-type: text/csv` |
| `payload_header(ts, area_ts)` | §8.1 | §9.1 | header line (§10) |
| `print_global_bbox(bbox)` | §8.2 | nothing | nothing |
| `print_item(elem, geom, tags, meta, mode)` | §8 | §9 | §10 |
| `display_remark(t)` / `display_error(t)` | `<remark> t </remark>\n` immediately | append `t` to a message buffer | ignored |
| `footer()` | `\n</osm>\n` | §9.1 | nothing |

A frontend wrapper (C++ `Web_Output`) holds a state `NotYet → {Payload | Html} → Final`:

```
write_payload_header(ts, area_ts, with_http):  if state != NotYet: return
    state = Payload
    if with_http: emit CORS headers (§2.2), handler.http_headers(), "\n"
                  if method is HEAD or OPTIONS: return          # no payload header
    handler.payload_header(ts, area_ts)
write_html_header(status):                     if state != NotYet: return
    state = Html; emit HTML preamble (§2.3) with the given status
display_error(text, status):                   # status = 400 for encoding/parse/static, 200 for runtime
    if state == NotYet: write_html_header(status)
    if method is HEAD/OPTIONS: return
    Html    → "<p><strong style=\"color:#FF0000\">Error</strong>: " + escape_xml(text) + " </p>\n"
    Payload → handler.display_error(text)
display_remark(text): same as display_error with status 200 and
    "<p><strong style=\"color:#00BB00\">Remark</strong>: " + escape_xml(text) + " </p>\n"
write_footer():   Html → "\n</body>\n</html>\n";  Payload → handler.footer();  state = Final
```

`write_footer` is always executed at process end (it runs from the destructor), **including after timeouts and other runtime errors**. Therefore every payload document is closed properly even if truncated.

Execution order: payload header is written once **before** any statement executes (so `osm_base` is the DB timestamp at query start); then the root script prints the global bbox (XML only, §8.2); then each `out` statement appends elements as it executes; then the footer.

---

## 2. Framing per entry point

### 2.1 CLI (`osm3s_query`)

* stdout = `handler.payload_header` + [global bbox] + elements + `handler.footer`. **No HTTP headers, no remarks.** Errors/remarks never appear in stdout (no `<remark>`, no JSON `"remark"`); they go to stderr via the console reporter.
* JSONP never applies (no CGI params).
* If parsing fails (encoding/parse/static error) stdout is **empty** and the exit code is 0.
* If a runtime error happens during execution, stdout still contains a well-formed document (header, whatever was printed, footer) and the message goes to stderr.
* stderr line formats (each terminated by `\n`); default log level is ASSISTING; `--quiet` suppresses all, `--concise` shows only errors, `--progress` = errors+progress, `--verbose` = everything:

| kind | text | shown at |
|---|---|---|
| encoding error | `encoding error: <msg>` | ≠quiet |
| parse error | `line <n>: parse error: <msg>` | ≠quiet |
| static error | `line <n>: static error: <msg>` | ≠quiet |
| runtime error | `runtime error: <msg>` | ≠quiet |
| encoding/parse/static/runtime remark | `encoding remark: <msg>` / `line <n>: parse remark: <msg>` / `line <n>: static remark: <msg>` / `runtime remark: <msg>` | ≥ ASSISTING (default) |
| progress | `After <h>h<m>m<s>s: in "<stmt>", part <p>, on line <n>.[ Stack: <a> of <b>[ <a> of <b>...]]` | ≥ PROGRESS (default) |

  Remarks always emitted by default: `encoding remark: Please enter your query and terminate it with CTRL+D.` (only when reading stdin, i.e. no `--request=`), `runtime remark: Timeout is <t> and maxsize is <bytes>.`
* Exit codes: 0 success, parse failure, or `std::bad_alloc` (message `Query run out of memory using about <MB> MB of RAM.`); 1 `File_Error` (e.g. `runtime error: The dispatcher (i.e. the database management system) is turned off.`; other: `open64: <errno> <strerror> <file> <origin>` or, if errno==0, `open64: <file> <origin>`); 2 timeout/out of memory; 3 `Context error: <msg>`; 4 exit error or `Query failed with the exception: <what>`.
* CLI runtime messages: timeout `Query timed out in "<stmt>" at line <n> after <secs> seconds.`; memory `Query run out of memory in "<stmt>" at line <n> using about <MB> MB of RAM.` (MB = bytes / 1048576, integer division).

Example (CLI, timeout in statement on line 8): stdout

```
<?xml version="1.0" encoding="UTF-8"?>
<osm version="0.6" generator="Overpass API 0.7.62 7c32a7d5">
<note>The data included in this document is from www.openstreetmap.org. The data is made available under ODbL.</note>
<meta osm_base="mock-up-init"/>


</osm>
```
stderr: `runtime error: Query timed out in "query" at line 8 after 2 seconds.`

### 2.2 HTTP (`interpreter`, CGI semantics)

The C++ program is a CGI: it writes header lines `Name: value\n`, a blank line, then the body. A `Status: <code> <reason>` header sets the HTTP status (absent → 200). The Rust server must emit the equivalent HTTP response. Header names are written exactly as `Content-type` (case-insensitive in HTTP).

**Success / payload path** (header lines in this order):

```
Access-Control-Allow-Headers: <value>      ← only if request header Access-Control-Request-Headers is non-empty (echoed verbatim)
Access-Control-Allow-Origin: *             ← only if request header Origin is non-empty
Access-Control-Max-Age: 600                ← same condition as previous line
Access-Control-Allow-Methods: GET, POST, OPTIONS   ← only for OPTIONS
Content-Length: 0                          ← only for OPTIONS
Content-type: application/osm3s+xml        ← per format (§1 table)
                                           ← blank line
<payload>
```

* Status 200, no `Status:` line. Content type has no charset parameter for any format.
* HEAD / OPTIONS: the query is parsed but not executed (OPTIONS with empty query string uses dummy query `out;`). Headers as above (content type of the requested format), and the payload header is skipped. (C++ still writes the format footer, e.g. `\n</osm>\n`, as body; a correct server drops HEAD bodies anyway — recommended: send no body.)
* **Runtime errors after the payload header was written** (query timeout, out of memory, missing meta files, …) keep status 200 and are embedded: XML `<remark> runtime error: <msg> </remark>` line at the current position; JSON appended to the `"remark"` member in the footer; CSV silently dropped.

**Errors before the payload header** produce an HTML page (§2.3) regardless of the requested format:

| condition | status line | message(s) in `<p>Error</p>` |
|---|---|---|
| empty input / only whitespace / too long / bad `jsonp` | `Status: 400 Bad Request` | `encoding error: Your input is empty.` / `encoding error: Your input contains only whitespace.` / `encoding error: Input Input too long (length: <n>). The maximum query length is <mb> MB.` / `encoding error: Parameter "jsonp" must contain only letters, digits, or the underscore.` |
| QL parse error | `Status: 400 Bad Request` | `line <n>: parse error: <msg>` (one `<p>` per error, in order) |
| static (semantic) error | `Status: 400 Bad Request` | `line <n>: static error: <msg>` |
| dispatcher slot timeout (origin ends `::timeout`) | `Status: 504 Gateway Timeout` | `runtime error: open64: <errno> <strerror> <file> <origin>. The server is probably too busy to handle your request.` |
| rate limited (origin ends `::rate_limited`) | `Status: 429 Too Many Requests` | `runtime error: open64: <errno> <strerror> <file> <origin>. Please check <server_name>status for the quota of your IP address.` (`<server_name>` = first line of file `server_name` in the working dir, default `/api`, with `/` appended if missing → `/api/status`) |
| dispatcher not running | `Status: 504 Gateway Timeout` | `runtime error: The dispatcher (i.e. the database management system) is turned off.` |
| any other error before payload (e.g. `make-area` without rules: `runtime error: Specify --rules to execute a rule. make-area can only appear in rules.`; duplicate query) | none (200) | `runtime error: <msg>` |

Typical real messages: `runtime error: open64: 0 Success /osm3s_osm_base Dispatcher_Client::request_read_and_idx::timeout. The server is probably too busy to handle your request.`

HTTP runtime message texts (differ from CLI!): timeout `Query timed out in "<stmt>" at line <n> after <secs> seconds.`; memory `Query ran out of memory in "<stmt>" at line <n>. It would need at least <MB> MB of RAM to continue.`; `bad_alloc` → `Query run out of memory using about <MB> MB of RAM.`; other exception `Query failed with the exception: <what>`; missing meta `Tried to use meta file but no meta files available on this instance.`; missing attic `Tried to use museum file but no museum files available on this instance.` (the triggering File_Error is then additionally reported as `open64: <errno> <strerror> <file> <origin>` → two remarks).

The HTTP frontend runs at log level ASSISTING, so **remarks are never shown** (they require VERBOSE); only errors are.

**JSONP**: CGI parameter `jsonp=<name>` (only with `[out:json]`). Allowed chars: ASCII alnum, `_`, `.`; otherwise encoding error above and padding disabled (C++ then keeps executing and appends raw JSON elements after the HTML page — do not replicate; stop after the error page). Effect: body is `<name>(` + JSON document + `);\n` instead of JSON document + `\n` (§9.1). Content type stays `application/json`.

### 2.3 HTML error page (exact)

```
[CORS lines as in 2.2]
Status: 400 Bad Request                       ← omitted when status is 200; 504 → "504 Gateway Timeout", 429 → "429 Too Many Requests", other non-200 → "Status: <code>"
Content-type: text/html; charset=utf-8

<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN"
    "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd">
<html xmlns="http://www.w3.org/1999/xhtml" xml:lang="en" lang="en">
<head>
  <meta http-equiv="content-type" content="text/html; charset=utf-8" lang="en"/>
  <title>OSM3S Response</title>
</head>
<body>

<p>The data included in this document is from www.openstreetmap.org. The data is made available under ODbL.</p>
<p><strong style="color:#FF0000">Error</strong>: line 1: parse error: Unknown type &quot;nod&quot; </p>

</body>
</html>
```

* Error text is `escape_xml`-ed (§4.4) — note `&quot;`. One space before `</p>`.
* `<p>Data included until: …</p>` is never produced on error pages (timestamp is empty there).
* For HEAD/OPTIONS the C++ writes only `Content-type: text/html; charset=utf-8` without `Status:` and without the HTML body; treat as an edge case.

---

## 3. Header timestamps

* `ts` (osm_base) = first line of `<db_dir>/osm_base_version`, de-escaped (`\n`→LF, `\t`→TAB, `\x`→`x`), emitted **verbatim** (no escaping, no validation; fixtures contain `mock-up-init`). Normally `YYYY-MM-DDThh:mm:ssZ`.
* `area_ts` is non-empty only when the query uses areas ("area level" > 0). Area level is 1 iff the query contains any of: `is_in` (coord-query), an area filter `(area...)` (area-query), `map_to_area`, a query of type `area` (`area[...]`), or an id query of type area (`area(<id>)`). CLI `--rules` → level 2. Level 1: `area_ts` = first line of `area_version` in the area DB dir (de-escaped). **CLI with `--db-dir` quirk:** `area_ts` = `ts` (C++ reads area_version but assigns the base timestamp). Level 2: `area_ts` = `ts`.
* Area level > 0 also enables printing of area elements and the `areas` count tag (§6, §11).

---

## 4. Primitive formatting

### 4.1 Integers
Ids, versions, changesets, uids, member refs, nd refs: plain base-10, no padding, no sign for unsigned. Derived/area ids are unsigned 64-bit.

### 4.2 Coordinates
* All floating-point output (lat, lon, bounds, center, vertices) uses C `printf("%.7f")` semantics: fixed notation, exactly 7 fractional digits, trailing zeros kept, correctly rounded from the exact binary value (ties-to-even on exact ties), `-` sign kept for negative values including `-0.0000000`. Rust `format!("{:.7}", x)` matches on current toolchains.
* Stored node coordinates are integers: `ilat: u32 = round((lat+91)·1e7)`, `ilon: i32 = round(lon·1e7)` (see data-model spec). Convert back **exactly** as C++ does: `lat = (ilat as f64) / 10000000.0 - 91.0`, `lon = (ilon as f64) / 10000000.0`. Do not use any other arithmetic order (affects last-digit rounding).
* The "missing coordinate" sentinel `Quad_Coord(0,0)` decodes to `lat = -91.0`, `lon = -214.7483648` (ilon = i32::MIN). It is relevant for the quirks in §7.5.
* Examples: `51.2500000`, `7.1500000`, `-0.0050000`, `10.0125000`.

### 4.3 Timestamps
Meta timestamps: `YYYY-MM-DDThh:mm:ssZ`, each field zero-padded (year width 4). Packed storage: `year<<26 | month<<22 | day<<17 | hour<<12 | minute<<6 | second`. Same format in XML, JSON and CSV.

### 4.4 XML escape (`escape_xml`) — byte-wise
| byte | output |
|---|---|
| `&` | `&amp;` |
| `"` | `&quot;` |
| `<` | `&lt;` |
| `>` | `&gt;` |
| `\n` (0x0A) | `&#x0a;` |
| `\t` (0x09) | `&#x09;` |
| `\r` (0x0D) | `&#x0d;` |
| other 0x00–0x1F | `?` |
| everything else (incl. `'`, 0x7F, all bytes ≥ 0x80) | unchanged |

Applied to: tag keys and values, member roles, user names, HTML error texts. **Not** applied to: XML `<remark>` text, timestamps in `<meta>`, derived type names (element names).

### 4.5 JSON escape (`escape_cstr`) — byte-wise
`"`→`\"`, `\`→`\\`, LF→`\n`, TAB→`\t`, CR→`\r`, other 0x00–0x1F → `?`; everything else unchanged (`/` not escaped, no `\u`). Applied to: tag keys/values, roles, user names, the footer `"remark"`. **Not** applied to: header timestamps, derived `"type"` values, jsonp name.

### 4.6 CSV escape (`escape_csv`, "RFC 4180"-ish)
If the value contains `\n`, `,` or `"`: double every `"` and wrap the whole value in `"…"`. Otherwise unchanged. **The separator itself and `\r` do not trigger quoting** (a tab inside a value with the default tab separator is emitted raw). Applied to: tag values in data rows and field names in the header line. **Not** applied to `::user`, `::type`, or any other `::` field.

### 4.7 Tag order
Tags of nodes/ways/relations/areas are emitted in the order delivered by the tag store: **ascending by key, bytewise** (`memcmp` order, shorter prefix first). Keys are unique per element. Derived elements (`make`/`convert`, `out count`) emit tags in their stored order (see evaluator spec; count: §11).

### 4.8 Lookups
* Member role: role-id → string table; unknown id → `???`.
* User name: uid → name table; if the uid is unknown the `user` attribute/member/CSV value is omitted/empty; `uid` is always printed when meta is present.
* Member type names: `node`, `way`, `relation` (any other value → `invalid`).

---

## 5. `out` statement → mode flags

QL: `out [tokens…];` tokens in any order, last one wins per category:

| category | tokens | default |
|---|---|---|
| verbosity | `ids`, `skel`, `body`, `tags`, `meta`, `count` (`quirks` → static error) | `body` |
| order | `asc` (by id), `qt` (quadtile) | `asc` |
| geometry | `geom`, `bb`, `center` | none |
| ids | `noids` | ids shown |
| limit | any token starting with a digit (parsed with `atoll`, truncated to u32) | unlimited |
| clip bbox | `(s,w,n,e)` anywhere (normally after `geom`) | none |

Other tokens → parse error `Invalid parameter for print: "<tok>"`. Input set is `.name out …;` (default `_`). Missing (never assigned) set → prints nothing (count → all zeros).

Flags (bitmask): `ID 0x1, COORDS 0x2, NDS 0x4, MEMBERS 0x8, TAGS 0x10, VERSION 0x20, META 0x40, GEOMETRY 0x80, BOUNDS 0x100, CENTER 0x200, COUNT 0x400`.

| verbosity | flags |
|---|---|
| `ids` | ID |
| `skel` | ID COORDS NDS MEMBERS |
| `body` | ID COORDS NDS MEMBERS TAGS |
| `tags` | ID TAGS |
| `meta` | ID COORDS NDS MEMBERS TAGS VERSION META |
| `count` | ID COUNT (+TAGS added at execution) |

Geometry adds: `geom` → GEOMETRY|BOUNDS; `bb` → BOUNDS; `center` → CENTER. `noids` clears ID.

Clip bbox validation (static errors, exact texts): `For the attribute "s" of the element "print" the only allowed values are floats between -90.0 and 90.0.` (same for `n`; `w`/`e` with `-180.0 and 180.0`), and `The value of attribute "n" of the element "print" must always be greater or equal than the value of attribute "s".` `w > e` is allowed (crosses the antimeridian). Without a bbox, clipping is disabled.

Data fetched per flags: tags only if TAGS (otherwise elements have no tags at all, in every format); meta only if META; geometry stores only if GEOMETRY|BOUNDS|CENTER.

---

## 6. Element order, grouping, limit

Each `out` prints, in this fixed group order: **nodes, ways, relations, areas, derived elements**. Areas are printed only if area level > 0 (§3); otherwise skipped even if present.

* **`asc` (default)**: within each group, ascending by id (nodes/derived: u64, ways/relations/areas: u32). `std::sort` is unstable: derived elements with equal ids (possible with `convert`) have unspecified relative order — keep input order (stable sort) as best effort.
* **`qt`**: within each group, iterate buckets by ascending index, then elements in bucket order (buckets are kept sorted by id). Node bucket index = `Uint32_Index` = `ll_upper` (bit-interleaved upper 16 bits of `ilat`/`ilon`, see data-model spec), compared as plain u32. Way/relation bucket index = stored `Uint31_Index`, compared by `(v & 0x7fffffff)` first, then full `v`. Derived elements live in bucket 0 in insertion order.
* **Limit `N`**: a single counter across all groups of one `out`; elements are printed while `count < N`, i.e. the first N elements of the overall sequence (nodes first). `out 0` prints nothing. The `out count` element counts toward the limit.
* Multiple `out` statements simply append to the same document (JSON: same `elements` array, comma handling continues across statements; CSV: header only once at the top). A `foreach` body containing `out` prints once per iteration.

---

## 7. Geometry semantics

Each printed element is paired with a geometry object; the formats then ask `has_center/center`, `has_bbox/bounds`, per-vertex validity, etc.

### 7.1 Nodes
Always a point (lat, lon) from the stored coordinates. Nodes never get `bounds`/`center` children; coordinates are printed as attributes/members when any of COORDS|GEOMETRY|BOUNDS|CENTER is set (so `out ids center` and `out tags bb` print node coordinates; `out ids`, `out tags` do not).

### 7.2 Ways
Vertex list `V` = coordinates of `nds` in order. If **any** referenced node is missing from the DB, `V` is empty (whole geometry unknown).

Clipping (only with an `out … (s,w,n,e)` bbox): vertex `i` is **kept** iff it is inside the bbox (inclusive integer comparison on `ilat`/`ilon`; if `w > e` longitude matches when `lon ≥ w || lon ≤ e`), or one of its incident segments `(i-1,i)`, `(i,i+1)` intersects the bbox (`Bbox_Double::intersects`, which includes segments with an endpoint inside). Dropped vertices become the missing sentinel (`Quad_Coord(0,0)`).

* **`geom`**: if `V` has no sentinel (including empty `V`) → *Linestring* (all positions valid). Otherwise → *PartialWay*: positions with sentinel are invalid; `has_center/has_bbox` iff at least one valid vertex. Bounds/center from `calc_bounds` (7.4) over valid vertices. Empty-`V` quirk: Linestring with zero points ⇒ `has_bbox` is true with the invalid box `(100,200,100,200)` ⇒ XML/JSON print `minlat 100.0000000 minlon 200.0000000 maxlat 100.0000000 maxlon 200.0000000`, no nd coordinates, JSON `"geometry": [` + `\n  ]`.
* **`bb`**: plain min/max over **all** entries of `V` (no antimeridian logic, sentinels included — see 7.5) → box `(minlat,minlon,maxlat,maxlon)`; if `V` empty → no geometry.
* **`center`**: same min/max, center = `((minlat+maxlat)/2, (minlon+maxlon)/2)`; if `V` empty → none.

### 7.3 Relations
Per member `i`:
* node member: its coordinate, or the sentinel if the node is missing or (with clip bbox) outside the bbox. With a clip bbox only nodes in the bbox's tile ranges are loaded at all.
* way member: the way's `V` (unclipped `Way_Geometry_Store`; empty if the way or any of its nodes is missing; with clip bbox only ways whose index overlaps the bbox ranges are loaded). With a clip bbox a vertex becomes the sentinel iff it **and both neighbours** are outside the bbox (first vertex: itself and next; last: itself and previous; single-vertex way: itself) — no segment-intersection test here.
* relation member: empty.

* **`geom`**: if no member entry is empty and no coordinate is the sentinel → *Compound*: node → point, way with 1 vertex → point, way with ≥2 vertices → linestring. Otherwise → *PartialRelation*: empty entry → null component; 1-entry valid → point; anything else → partial way (sentinels = invalid positions). Bounds = component-wise `calc_bounds` (7.4); `has_bbox`/`has_center` iff resulting box is valid (and, for Partial, at least one valid coordinate exists).
  Member-level accessors used by printers: node member valid iff its component has a center; way member `way_size` = number of points of a linestring/partial-way component (a point component — 1-vertex way — has way_size 0; null has 0); position valid iff not sentinel.
* **`bb` / `center`**: min/max over every coordinate of node members and way members (sentinels included!). If `maxlon - minlon >= 180`: recompute `minlon` = min over coordinates with `lon >= 0`, `maxlon` = max over coordinates with `lon < 0` (initial values 200 / −200 remain if none). `bb` → box with these raw values (may have `minlon > maxlon`); `center` → `((minlat+maxlat)/2, (minlon+maxlon)/2)` (no wrap correction). No coordinates at all → no geometry.

### 7.4 `calc_bounds` and bbox center (used by `geom` geometries and derived geometries)
Points (linestring / partial way; invalid points skipped): `s,w,n,e` = min lat, min lon, max lat, max lon. None → invalid box `(100,200,100,200)`. If `e - w > 180`: `ww` = min lon over points with `lon > 0` (init 180), `we` = max lon over points with `lon <= 0` (init −180); if `ww - we > 180` → box `(s, ww, n, we)` (crosses antimeridian), else `(s, -180, n, 180)`.

Components (compound/partial relation): over components that have a bbox, `s,n,w,e` = min/max of their south/north/west/east; `wrapped` = any component with `east < west`. None → invalid. If `!wrapped && e - w <= 180` → `(s,w,n,e)`. Else recompute: for each component with bbox: if `east <= 0`: `e = max(e, east)`, and if also `west >= 0`: `w = min(w, west)`; else if `west >= 0`: `w = min(w, west)`; else → return `(s,-180,n,180)`. Finally if `w - e > 180` → `(s,w,n,e)` else `(s,-180,n,180)`. (w init 200, e init −200.)

Box validity: `-90 <= s <= n <= 90` and `w,e ∈ [-180,180]`.
Box center: `lat = (s+n)/2`; `lon = (w+e)/2` if `w <= e`, else `(w+e)/2 + 180` if `(w+e)/2 <= 0`, else `(w+e)/2 - 180`.
Point geometry: bbox = the point itself; center = the point.

### 7.5 C++ quirks to reproduce (or consciously deviate from)
1. `out bb(…)`/`out center(…)` on ways and relations, and `out bb`/`out center` on relations with missing node members: sentinel coordinates `(-91.0, -214.7483648)` enter the min/max. Clean planet data without clip bbox never triggers this.
2. Relation `center` across the antimeridian is `(minlon+maxlon)/2` after the recompute, e.g. members at ±170 → center lon `0.0000000`.
3. Ways never get antimeridian handling for `bb`/`center`, but do for `geom` (via 7.4).

---

## 8. XML format

### 8.1 Document framing

```
<?xml version="1.0" encoding="UTF-8"?>
<osm version="0.6" generator="Overpass API 0.7.62 7c32a7d5">
<note>The data included in this document is from www.openstreetmap.org. The data is made available under ODbL.</note>
<meta osm_base="2024-05-01T12:00:00Z" areas="2024-05-01T11:00:00Z"/>

…elements…

</osm>
```
Bytes: header ends with `<meta osm_base="TS"/>\n\n` (the ` areas="ATS"` attribute only if `area_ts` non-empty); footer is `\n</osm>\n`. Empty result therefore yields `…/>`, empty line, empty line, `</osm>`.

### 8.2 Global bbox
If the query has a valid `[bbox:s,w,n,e]` (or `bbox=` CGI param substituted into `[bbox]`), at the start of execution (right after the header, before any element) XML writes:
```
  <bounds minlat="50.0000000" minlon="7.0000000" maxlat="51.0000000" maxlon="8.0000000"/>

```
(i.e. the line followed by an empty line). JSON/CSV write nothing.

### 8.3 Remarks
`<remark> ` + text + ` </remark>\n` (text NOT escaped; leading and trailing space), at the current output position, e.g.
```
  <node id="1"/>
<remark> runtime error: Query timed out in "query" at line 3 after 2 seconds. </remark>

</osm>
```

### 8.4 Element templates
Opening line is indented 2 spaces. If the element has no child lines it is closed with `/>\n`; otherwise the opening tag ends with `>\n`, children (indented 4 spaces) follow, then `  </name>\n`. "Has children" is decided lazily: the first child printed emits the `>\n`.

**Attributes** (order fixed): `id` (if ID) → [node only: `lat`,`lon` if COORDS|GEOMETRY|BOUNDS|CENTER] → meta (if META and meta present): `version`, `timestamp`, `changeset`, `uid`, `user` (user only if known; escaped).

**Children order**: way/relation: bounds-or-center, then nds/members, then tags. Node: tags only.

| child | condition | text |
|---|---|---|
| bounds | BOUNDS and geometry has bbox | `    <bounds minlat="S" minlon="W" maxlat="N" maxlon="E"/>` |
| center | else CENTER and geometry has center | `    <center lat="LAT" lon="LON"/>` |
| nd | NDS, one per node ref | `    <nd ref="ID"/>`; with valid way geometry position i (only `geom`): `    <nd ref="ID" lat="LAT" lon="LON"/>` |
| member | MEMBERS, one per member | `    <member type="T" ref="ID" role="ROLE"/>` (+ see below) |
| tag | TAGS and tags exist | `    <tag k="K" v="V"/>` |

Relation members with relation geometry (`geom`):
* node member with valid position: `    <member type="node" ref="1" role="stop" lat="51.0000000" lon="7.0000000"/>`; invalid → no lat/lon.
* way member: if at least one of its positions is valid:
  ```
      <member type="way" ref="2" role="outer">
        <nd lat="51.0000000" lon="7.0000000"/>
        <nd/>
        <nd lat="51.1000000" lon="7.1000000"/>
      </member>
  ```
  (`<nd/>` for invalid positions, 6-space indent); if none valid (or way_size 0) → self-closing `<member …/>`.
* relation member: always self-closing.

Examples:
```
  <node id="1" lat="10.0125000" lon="1.0125000" version="3" timestamp="2001-01-01T00:00:01Z" changeset="12" uid="11" user="User_11">
    <tag k="even" v="yes"/>
  </node>
  <node id="2"/>
  <way id="7">
    <center lat="10.0125000" lon="1.1750000"/>
    <nd ref="7"/>
    <nd ref="8"/>
    <tag k="foo" v="bar"/>
  </way>
  <way id="8">
    <bounds minlat="51.0000000" minlon="7.0000000" maxlat="51.1000000" maxlon="7.1000000"/>
    <nd ref="15" lat="51.0000000" lon="7.0000000"/>
    <nd ref="16" lat="51.1000000" lon="7.1000000"/>
  </way>
  <relation id="6">
    <member type="way" ref="1" role="two"/>
    <member type="node" ref="1" role="one"/>
    <tag k="relation_key" v="relation_few"/>
  </relation>
```
`out ids geom` on a way prints `<bounds>` but no `<nd>` (NDS not set); `out tags center` on a way prints `<center>` + tags.

### 8.5 Areas and derived elements
Areas are printed as derived elements of type `area` with no geometry and no meta. Derived element (`make`, `convert`, `out count`): `  <TYPE` + ` id="ID"` (if ID) — TYPE is the raw type name, unescaped. Children: geometry (below, base indent 4 spaces) then tags (`    <tag …/>`). Closing `  </TYPE>`.

Derived geometry, first matching rule (`ind` = current indent, starts at 4 spaces):
1. GEOMETRY and compound geometry → per component (every non-null pointer, even null geometries): `ind<group>`, recurse with `ind+2`, `ind</group>`.
2. GEOMETRY and linestring (or partial way with exactly one valid run) → per point `ind<vertex lat="…" lon="…"/>`.
3. GEOMETRY and multi-linestring (polygon / partial way) → per ring: `ind<linestring>`, `ind  <vertex …/>`…, `ind</linestring>`.
4. GEOMETRY and has center (point) → `ind<point lat="…" lon="…"/>`.
5. BOUNDS and has bbox → `    <bounds minlat=… minlon=… maxlat=… maxlon=…/>` (always 4 spaces).
6. CENTER and has center → `ind<center lat="…" lon="…"/>`.
Note: plain `out`/`out body` prints **no** geometry for derived elements in XML.

```
  <area id="3600062422">
    <tag k="admin_level" v="4"/>
    <tag k="name" v="Berlin"/>
  </area>
  <center id="1">
    <point lat="48.0000000" lon="11.0000000"/>
  </center>
  <trace id="2">
    <group>
      <point lat="51.5010000" lon="7.0005000"/>
    </group>
  </trace>
```

---

## 9. JSON format

### 9.1 Document framing
Header (`P(` prefix only with JSONP name `P`):
```
{
  "version": 0.6,
  "generator": "Overpass API 0.7.62 7c32a7d5",
  "osm3s": {
    "timestamp_osm_base": "TS",
    "timestamp_areas_base": "ATS",
    "copyright": "The data included in this document is from www.openstreetmap.org. The data is made available under ODbL."
  },
  "elements": [

```
(`timestamp_areas_base` line only if `area_ts` non-empty; the header ends with `[\n\n`.)

Elements: each element is written as `{` … `\n}`; before every element except the first of the whole document `,\n` is written. Footer:
```
"\n\n  ]" + (msgs != "" ? ",\n\"remark\": \"" + escape_cstr(msgs) + "\"" : "") + "\n}" + (jsonp ? ");\n" : "\n")
```
`msgs` is the plain concatenation of all display_error/display_remark texts (no separator!), e.g. `runtime error: Query timed out in "query" at line 3 after 2 seconds.`.

Empty result:
```
{
  …header…
  "elements": [



  ]
}
```
(three empty lines). Two elements and a remark:
```
  "elements": [

{
  "type": "node",
  "id": 1
},
{
  "type": "node",
  "id": 2
}

  ],
"remark": "runtime error: Query timed out in \"query\" at line 3 after 2 seconds."
}
```

### 9.2 Element templates
Members are written as `",\n  \"name\": value"` after the first line `{\n  "type": "T"`; element ends with `\n}`. Member order:

* **node**: `type`, `id` (ID), `lat`, `lon` (if COORDS|GEOMETRY|BOUNDS|CENTER), meta (§9.3), `tags`.
* **way**: `type`, `id`, meta, `bounds` | `center`, `nodes` (NDS and non-empty), `geometry` (GEOMETRY and way geometry is linestring/partial — independent of NDS), `tags`.
* **relation**: `type`, `id`, meta, `bounds` | `center`, `members` (MEMBERS and non-empty), `tags`.
* **derived/area**: `type` (raw type name, unescaped), `id`, `geometry` (§9.4), `tags`.

```
{
  "type": "node",
  "id": 1,
  "lat": 10.0125000,
  "lon": 1.0125000,
  "timestamp": "2001-01-01T00:00:01Z",
  "version": 3,
  "changeset": 12,
  "user": "User_11",
  "uid": 11,
  "tags": {
    "even": "yes",
    "foo": "bar"
  }
}
```
bounds / center (same text for ways and relations):
```
  "bounds": {
    "minlat": 51.0000000,
    "minlon": 7.0000000,
    "maxlat": 51.1000000,
    "maxlon": 7.1000000
  },
```
```
  "center": {
    "lat": 51.0500000,
    "lon": 7.0500000
  },
```
nodes and way geometry (`null` for invalid positions; 4-space indent; comma directly after each item except the last):
```
  "nodes": [
    15,
    16
  ],
  "geometry": [
    { "lat": 51.0000000, "lon": 7.0000000 },
    null
  ],
```
(Zero positions → `"geometry": [` + `\n  ]`.)
members (comma before each item except the first, placed right after the previous `}`):
```
  "members": [
    {
      "type": "node",
      "ref": 1,
      "role": "stop",
      "lat": 51.0000000,
      "lon": 7.0000000
    },
    {
      "type": "way",
      "ref": 2,
      "role": "",
      "geometry": [
         { "lat": 51.0000000, "lon": 7.0000000 },
         null
      ]
    },
    {
      "type": "relation",
      "ref": 3,
      "role": ""
    }
  ]
```
Node member `lat`/`lon` only with relation geometry and valid position. Way member `geometry` is printed whenever the relation has relation geometry (`geom`) — even if the way is missing (`"geometry": [` + `\n      ]`); item indent is 9 spaces. Relation members never get geometry.

`tags`: `"tags": {` then `\n    "K": "V"` items joined by `,`, then `\n  }`. Tag values are always JSON strings.

### 9.3 Meta (only if META and meta present)
Order: `"timestamp": "…"`, `"version": N`, `"changeset": N`, `"user": "…"` (only if known), `"uid": N`. (Differs from XML order!)

### 9.4 Derived geometry (GeoJSON-ish), first matching rule
1. GEOMETRY and has center: `,\n  "geometry": ` + `G(geom, "  ")`
2. else BOUNDS and has bbox (note: a flat list of 5 positions, *not* nested as a GeoJSON ring — reproduce as is):
```
  "geometry": {
    "type": "Polygon",
    "coordinates": [
      [W, S],
      [E, S],
      [E, N],
      [W, N],
      [W, S]
    ]
  }
```
3. else CENTER and has center: `{` `\n    "type": "Point",` `\n    "coordinates": [ LON, LAT ]` `\n  }`.

`G(g, ind)`:
* compound/partial relation → `{\n{ind}  "type": "GeometryCollection",\n{ind}  "geometries": [` + for each component **with a center** (comma before all but the first): `\n{ind}    ` + `G(c, ind+"    ")` + `\n{ind}  ]\n{ind}}`
* linestring → `{\n{ind}  "type": "LineString",\n{ind}  "coordinates": [` + items `(","?)\n{ind}    [LON, LAT]` + `\n{ind}  ]\n{ind}}`
* multi-linestring (polygon, partial way) → `"type": "Polygon"`, rings `(","?)\n{ind}    [` + points `(","?)\n{ind}      [LON, LAT]` + `\n{ind}    ]`, then `\n{ind}  ]\n{ind}}`
* point → `{\n{ind}  "type": "Point",\n{ind}  "coordinates": [ LON, LAT ]\n{ind}}`

Note the asymmetric spacing: Point uses `[ LON, LAT ]`, LineString/Polygon items use `[LON, LAT]`. Coordinates are `[lon, lat]` order.

---

## 10. CSV format

Syntax: `[out:csv( field[, field…] [; headerline [; "separator"]] )]`.
* field: a tag key (bare identifier or quoted string, `\n`, `\t`, `\uXXXX` escapes decoded) or `::` + name (special field). Names containing `:` must be quoted: `::"count:nodes"`.
* headerline: anything except the literal `false` means true (default true).
* separator: any string, default TAB; may be empty or multi-character (`"---"`).

**Header line** (if enabled), written as payload header: for each field `(special ? "@" : "") + escape_csv(name)`, joined by separator, then `\n`. E.g. `@type\t@id\tname\n`. Fields are not deduplicated (`::id,::id` → two columns).

**Data line** per printed element: values joined by separator, then `\n`. Value of each field:

| field | value |
|---|---|
| tag key `k` | `escape_csv(v)` of the tag `k` if tags were loaded (TAGS flag) and present, else empty |
| `::id` | id if ID flag (empty with `noids`) |
| `::otype` | `1` node, `2` way, `3` relation, `4` area/derived/count |
| `::type` | `node` / `way` / `relation` / derived type name (`area`, `count`, make-type), raw |
| `::lat`, `::lon` | `%.7f` of geometry center if any of COORDS|GEOMETRY|BOUNDS|CENTER is set **and** the geometry has a center; else empty |
| `::version`, `::timestamp`, `::changeset`, `::uid` | from meta (only with `out meta`), else empty |
| `::user` | raw user name (no escaping) if meta present and uid known, else empty |
| `::count`, `::"count:nodes"`, `::"count:ways"`, `::"count:relations"`, `::"count:areas"` | only for elements whose type name is `count`: value of tag `total`/`nodes`/`ways`/`relations`/`areas`, or `0` if that tag is absent (e.g. `areas` without area level); empty for other elements |
| any other `::x` | empty |

Coordinates per mode: nodes have lat/lon in `skel`/`body`/`meta` and with any geometry modifier; ways/relations only with `center` (simple center 7.2/7.3), `bb` (box center 7.4 rule) or `geom` (center of `calc_bounds` box); derived elements with a geometry also in plain `body` (COORDS). No footer, no remarks, no error output inside CSV.

Examples (tab shown as `→`):
```
@type→@id→@lat→@lon→@version→@timestamp→@changeset→@user→@uid
node→1→10.0125000→1.0125000→3→2001-01-01T00:00:01Z→12→User_11→11
way→1→→→4→2002-01-01T00:00:01Z→13→User_12→12
```
`[out:csv("even",::id,"foo";true;",")]` with `out meta`:
```
even,@id,foo
,1,
yes,2,
,7,bar
```
`out center` gives ways/relations coordinates: `way→1→10.0125000→1.0250000→…`. `out ids`: `node→1→→→…` (no coordinates, no tags). Separator `""` concatenates (`even@idfoo`, `yes14bar`).

---

## 11. `out count`

Produces exactly one derived element: type `count`, id `0`, no geometry, tags in this order:
`nodes`, `ways`, `relations`, [`areas` — only if area level > 0], `total` (= sum of the preceding). Values are decimal strings of the element counts of the input set (current + attic). Flags ID|COUNT|TAGS (+ geometry/noids/limit/order modifiers apply as usual).

XML:
```
  <count id="0">
    <tag k="nodes" v="4"/>
    <tag k="ways" v="4"/>
    <tag k="relations" v="4"/>
    <tag k="total" v="12"/>
  </count>
```
JSON:
```
{
  "type": "count",
  "id": 0,
  "tags": {
    "nodes": "4",
    "ways": "4",
    "relations": "4",
    "total": "12"
  }
}
```
CSV (`[out:csv(::type,::id,::count,::"count:nodes")]`): header `@type→@id→@count→@count:nodes`, row `count→0→12→4`.

---

## 12. Complete examples

### 12.1 HTTP, `[out:json];node(1);out;` (no Origin header)
```
Content-type: application/json

{
  "version": 0.6,
  "generator": "Overpass API 0.7.62 7c32a7d5",
  "osm3s": {
    "timestamp_osm_base": "2024-05-01T12:00:00Z",
    "copyright": "The data included in this document is from www.openstreetmap.org. The data is made available under ODbL."
  },
  "elements": [

{
  "type": "node",
  "id": 1,
  "lat": 10.0125000,
  "lon": 1.0125000
}

  ]
}
```

### 12.2 HTTP, CSV fixture `output_csv_8` (body after headers)
```
Content-type: text/csv

even,@id,foo
,1,
yes,2,
,7,bar
yes,14,bar
,1,
…
```

### 12.3 CLI, XML, `node(1);out ids;`
```
<?xml version="1.0" encoding="UTF-8"?>
<osm version="0.6" generator="Overpass API 0.7.62 7c32a7d5">
<note>The data included in this document is from www.openstreetmap.org. The data is made available under ODbL.</note>
<meta osm_base="mock-up-init"/>

  <node id="1"/>

</osm>
```

### 12.4 XML, two outs: `node(1);out ids; way(1);out ids;`
```
<meta osm_base="…"/>

  <node id="1"/>
  <way id="1"/>

</osm>
```
JSON equivalent: `{…"id": 1\n},\n{\n  "type": "way",…` (single array).

---

## 13. Implementation checklist (easy to get wrong)

1. XML meta order `version timestamp changeset uid user`; JSON `timestamp version changeset user uid`.
2. Node coordinates appear whenever any of COORDS/GEOMETRY/BOUNDS/CENTER is set (`out ids center` → node lat/lon).
3. JSON way `geometry` does not require NDS; XML nd coordinates do (they live on `<nd>`).
4. `bounds` takes precedence over `center`; `out geom` always implies bounds.
5. XML `<remark>` text unescaped, padded with single spaces; JSON remark messages concatenated without separator and placed in the footer; CSV drops them.
6. Errors before the payload → HTML page with 400/429/504 even for JSON/CSV; after the payload → status 200 with embedded remark.
7. CSV escaping ignores the separator and `\r`; `::user` is never escaped; `@` prefix only in the header.
8. Every float is `%.7f`; coordinates are computed `ilat/1e7 - 91.0`, `ilon/1e7`; centers `(a+b)/2`.
9. Order is nodes → ways → relations → areas (only with area level) → derived; `out N` limit spans all groups.
10. JSON elements of consecutive `out` statements share one array; the first element has no leading comma, all others `,\n`.
11. XML document always ends `\n</osm>\n` and JSON `\n\n  ]…\n}\n`, also after runtime errors (footer is unconditional).
12. Tags are sorted by key (bytewise) for OSM elements and areas; derived/count tags keep their own order.
