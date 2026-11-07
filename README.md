<p align="center">
  <a href="https://overpass.ch"><img src="logo.png" alt="overpass.ch" width="209"></a>
</p>

# overpass-rust

A Rust reimplementation of the [Overpass API](https://wiki.openstreetmap.org/wiki/Overpass_API),
the read-only query server for OpenStreetMap data. It aims to be compatible with clients of the
C++ Overpass API 0.7.62 (Overpass Turbo, iD, JOSM, osmnx, overpy).

Deployed on [overpass.ch](https://overpass.ch). On live queries, it is typically 25 times faster than Overpass API
0.7.62.11 on queries with answers under 100 KB, and 13 times faster in total time over all queries.

## Status

| Area | State |
|---|---|
| Storage, import (OSM XML, PBF), OsmChange updates, QL parser, queries, recursion, `out` modes, XML/JSON output, HTTP server | done |
| `make`, `convert`, `(if:)`, `if`, `for`, evaluator functions, CSV, areas (`area`, `(area)`, `is_in`, `map_to_area`, `pivot`, `make-areas`) | done |
| Attic data (`[date:]`, `[diff:]`, `[adiff:]`, `retro`, `timeline`, `changed`, `compare`) | not supported; rejected with an error |
| XML query language: translated to QL (`query`, `has-kv`, `bbox-query`, `around`, `polygon-query`, `id-query`, `item`, `recurse`, `area-query`, `coord-query`, `map-to-area`, `union`, `difference`, `foreach`, `print`), with the C++ error messages and line numbers; evaluator elements are rejected | done |
| Dropped: `out:popup`, `out:custom`, `local` | — |

## Build and run

```sh
cargo build --release
B=target/release/overpass

$B --db data/db import region.osm.pbf          # also .osm/.osc, .gz/.bz2, `-` for stdin
$B --db data/db make-areas                     # areas from multipolygon/boundary relations
$B --db data/db replicate                      # apply replication diffs, keep running
$B --db data/db update changes.osc.gz          # or apply OsmChange files by hand
$B --db data/db info                           # data timestamp, replication state, size
$B --db data/db query -q '[out:json];node[amenity=cafe](50.7,7.1,50.8,7.2);out;'
$B --db data/db serve --listen 0.0.0.0:8080    # /api/interpreter, /api/status, /api/timestamp
```

For a server with an extract or the whole planet, kept up to date, run `serve` and `replicate` side
by side and `make-areas` from time to time.

- **Import.** Into an empty database, an OSM file sorted as usual (nodes, ways, relations) is
  bulk loaded: one pass over the input, node positions and way boxes in lookup files on disk,
  every table sorted on disk and written in key order. Memory stays within `--memory` (MiB,
  default 1024) whatever the input size, so this works for the planet; temporary files go to
  `--tmp` (default: inside the database directory). `--meta no` drops versions, timestamps,
  changesets and users. A `.pbf` header with replication fields (planet and Geofabrik files)
  sets the data timestamp and the replication start. Into a non-empty database, files are
  applied as changes.
- **Updates.** `replicate` downloads minutely, hourly or daily diffs (`--url`, default from the
  `.pbf` header) and applies them; the sequence number is stored in the same transaction as the
  data. Without a recorded sequence, the start is found from the data timestamp. `--once` applies
  what is available and exits. `update` applies OsmChange files in one transaction.
- **Areas.** `make-areas` rebuilds all areas from the current data and replaces the stored ones
  (areas whose relation no longer qualifies are removed; C++ keeps them). It reads short
  snapshots and writes in one transaction at the end, so it can run next to `replicate`.
- **Queries.** `--threads N` (or `OVERPASS_THREADS`) sets the worker threads used inside one
  query (default: half the hardware threads, at most 8). A query runs its parallel sections
  sequentially while other queries, in the same process or in other processes, keep the cores
  busy. `serve --max-concurrent N --max-timeout S --max-maxsize B` bounds concurrent queries and
  their `[timeout:]` (default 1800 s) and `[maxsize:]` (default 4 GiB); `--max-space B` bounds
  the sum of `[maxsize:]` over running queries like the `--space` of the C++ dispatcher. While a
  statement decodes data, its memory is checked against the C++ address-space limit
  (2 × maxsize + 1 GiB), so a query stops with "Query run out of memory" before it can exhaust
  the server. `query` and `serve`
  open the database read-only and see each committed update. A query stops when its client
  disconnects.
- **Maintenance.** `compact` rewrites the file without free pages; run it only while no server
  or update uses the database.

## Architecture

```
src/geo.rs            coordinate quantization (as in C++), quad tiles, loose-quadtree cells, sphere math
src/model.rs          nodes, ways, relations, areas, compact tag storage
src/store/            LMDB tables: cell blocks, meta blocks, id index, tag index, relation members, area blocks
src/update/           OSM XML / PBF / OsmChange readers, change application with cascades, make-areas
src/ql/               Overpass QL lexer and recursive-descent parser
src/exec/             query engine: sets and frames, query planner, recursion, evaluators, areas, `out`
src/output/           XML, JSON and CSV writers
src/server.rs         axum HTTP server, streaming output, concurrency limit
```

- **Storage.** Elements are ordered by spatial cell, then id: nodes by the same 16-bit quad tile
  as the C++ code, ways and relations in a loose quadtree (each element stored once). They are
  packed into lz4-compressed chunks of up to 16 KiB that may span many cells, with columnar
  coordinates and a per-chunk tag dictionary; meta data lives in parallel chunks. The id index
  and the tag index `(key, value) -> (cell, id)` are delta-coded and grouped over many cells, the
  relation member index likewise. LMDB gives every query a consistent snapshot while one writer
  applies updates, which replaces the C++ dispatcher.
- **Queries.** Each query statement picks the cheapest candidate source (ids, input sets,
  recursion, tag index, spatial cells) and checks the remaining clauses. Ways and relations carry
  their bounding box, which decides most spatial tests without loading node coordinates. Decoding,
  geometry tests and output formatting run in parallel for large inputs.
- **Fidelity.** Semantics follow the specs in `docs/spec/`, which were extracted from the C++
  sources. Where C++ behaviour is observable (number formatting `%.14g`, `strtoll`/`strtod`
  quirks, iteration order via the C++ storage index, area geometry with its integer arithmetic,
  RHR polygon normalization), it is ported, not approximated.

## Compatibility testing

Tested against a corpus of 598 queries over ten datasets and the recorded output of the C++ binary:
synthetic C++ test data (also with metadata and an OsmChange diff), the C++ implicit-area
fixtures, a multipolygon dataset, a Monaco extract, a 10 degree area, antimeridian and
missing-node edge cases, long tag keys and values, and nodes created after the ways using them.

Current result: 528 queries match the C++ output byte for byte and 41 are rejected by both (with
the same first error message, including the C++ static type and context checks of evaluator
expressions). The rest are 12 timing-dependent queries (C++ hit `[timeout:1]`) and 17 intended
differences: dropped formats, stale state the C++ code keeps for
ways and relations when a node they use is deleted or created later, and closed ways that C++
`is_in(lat,lon)` reports although the point lies outside them.

Regular expressions are translated from glibc POSIX ERE (GNU escapes such as `\w`, `\<`; other
escaped characters are literal; `.` matches newlines) to the Rust `regex` crate; back-references
are rejected. Error message wording for runtime errors may differ from C++.

## Performance

A benchmark compares the C++ `osm3s_query` with this implementation on the same data
(C++ test generator, size 400: 640k nodes, 240k ways). Every query is a fresh process, median of
several runs; outputs were identical for all queries. The machine was shared, so throughput
numbers vary between runs (C++ measured 92–108 q/s); the ratios are from the same run.

| | C++ | Rust, 1 thread | Rust, default threads |
|---|---|---|---|
| 17 queries, total latency | 1226 ms | 492 ms (2.5x) | 500 ms (2.5x) |
| throughput, 16 clients, process per query | 92 q/s | 120 q/s (1.3x) | 118 q/s (1.3x) |
| throughput, 16 clients, `overpass serve` | — | — | 145 q/s (1.6x) |

A 30 000-element OsmChange applies in 0.6 s (C++ 1.5 s). On a real extract (Luxembourg, 4.9
million elements with metadata) the bulk import takes 3.6 s (C++ 18 s, from XML with ids
renumbered densely for its id maps) and typical queries run 1.8 to 17 times faster with
identical output.

Database size:

| | C++ | Rust |
|---|---|---|
| synthetic, size 400 (no metadata) | 26 MB | 19 MB |
| Luxembourg, with metadata, dense ids | 116 MB | 95 MB |
| Luxembourg, with metadata, original OSM ids | — | 103 MB |
| Monaco, with metadata, original OSM ids | 601 MB | 2.3 MB |

The C++ id-to-index maps grow with the largest id, which makes extracts with real OSM ids large.

### Germany extract against C++ 0.7.62.11

Geofabrik Germany of 1 November 2026 (4.9 GB `.pbf`, 515 million elements, meta data), the C++
server as the `wiktorn/overpass-api:v0.7.62.11` image (dispatcher, nginx, fcgiwrap) and this
server side by side on one machine (Xeon E3-1275 v5, 8 threads, 64 GB,
NVMe), both over HTTP.

| | C++ | Rust |
|---|---|---|
| import with meta data | 90 min (2 cores; mostly one thread) | 17.5 min (5 cores) |
| areas | 223–266 s | 42–75 s |
| database | 28 GB | 9.8 GB (11.3 GB after 36 h of minutely diffs) |
| 60 minutely planet diffs | 97 s | 7.9 s |
| 212 hand-written queries, warm, sum of medians | 393 s | 106 s (geometric mean of the per-query ratio: 12×) |
| 400 generated queries (types, tags, bbox/around/poly/area, recursion, all `out` modes, JSON/XML/CSV) | 206 s | 43 s |
| 500 generated multi-statement queries (`foreach`, `for`, `make`/`convert`, `if`, `complete`, set operations) | 775 s | 49 s |
| throughput, 4 / 8 clients, queries under 2 s | 10.4 / 13.7 q/s | 42.9 / 56.5 q/s |
| 12 clients, heavy country-wide queries, 3 min | 0.29 q/s, peak 3.7 GB | 0.9 q/s, peak 2.7 GB |

Answers were byte-identical for every query that returns data, before and after applying 36 hours
of planet minutely diffs to both, except where the C++ database itself became inconsistent: ways
and relations whose index C++ fixed before their nodes or members arrived (C++ then reports
foreign ways and an Angolan municipality for a point in Berlin, or runs out of memory on area
queries), relation member ways missing from C++ `>` and `way(r)` after a member way moved, and
an area C++ keeps after its relation lost the name. Not supported: attic data (`[date:]`,
`[diff:]`, `[adiff:]`, `retro`, ...), which C++ answers from an attic database.

## Licence

GNU Affero General Public License, version 3 or later (`LICENSE`). overpass-rust is based on
[Overpass API](https://github.com/drolbr/Overpass-API) by Roland Olbricht et al., and parts of it
are translated from the C++ sources; `NOTICE` lists them. If you run a modified version for others
over a network, the licence requires you to offer them its source.
