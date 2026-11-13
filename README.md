<p align="center">
  <a href="https://overpass.ch"><img src="logo.png" alt="overpass.ch" width="209"></a>
</p>

# overpass-rust

overpass-rust is a reimplementation of the [Overpass API](https://wiki.openstreetmap.org/wiki/Overpass_API)
in Rust. It answers queries on current OpenStreetMap data and gives the same answers as the Overpass
API 0.7.62, so Overpass Turbo, QGIS, OSMnx, overpy and other clients work with it unchanged. It
serves the planet queries of [overpass.ch](https://overpass.ch).

In a test on the queries of overpass.ch customers, overpass-rust was typically 84 times faster than
Overpass API 0.7.62.11 on the same hardware for answers under 100 KB, and 14 times faster in total
time ([how this was measured](#speed-on-the-planet)).

Overpass QL works on current data, areas and evaluators included. XML queries are translated to QL,
except for evaluator elements. History queries (`[date:]`, `[diff:]`, `[adiff:]` and the statements that go with them) are rejected
with an error, and `out:popup`, `out:custom` and `local` are left out.

## Why it is faster

Overpass API starts a new process for each request, and that process registers with the dispatcher
before it reads any data. In the test below, its quickest answers took about 50 ms. The Rust engine
answers every query inside one `overpass serve` process that reads a memory-mapped LMDB file. Each
query gets its own snapshot while a single writer applies the diffs, and the quickest answers took
less than 1 ms.

The database is less than half the size. Elements are packed into lz4-compressed chunks of up to 16
KiB, with coordinates stored column by column and a tag dictionary in each chunk, and the indexes are
delta-coded. The planet with metadata and areas takes 190 GiB in Rust and 428 GiB in the C++ engine,
attic data not counted. On a machine with 128 GB of RAM, this decides whether the parts of the
database in use stay in the page cache or are read from disk.

Data is sorted by spatial cell and then by id, and the tag index points to `(cell, id)`, so the
elements found through a tag are read in the order they lie on disk. Every way and relation keeps
its bounding box. For `(bbox)`, `around`, `poly` and areas the box settles most cases without
loading node coordinates, and `out center` and `out bb` come straight from it.

A planner picks where each statement starts (ids, an input set, the tag index or spatial cells,
whichever is cheapest) and begins with the most selective filter. Tags are checked before an element
is decoded. An area is turned into ranges of cells, and only those cells are read. Decoding,
geometry tests, output and scans of the tag index run on up to 8 threads within one query, where
Overpass API uses one thread per query.

The releases since the first public one made area and regex queries faster
([CHANGELOG.md](CHANGELOG.md)). Since 0.2.0, the cell ranges of a large country are worked out in one
walk, which takes 0.05 s for Russia (4.6 s before), and relations in an area are read only from
members stored near it. Since 0.2.1, a statement like `node["place"]["name"~"Springfield"]` starts
from the regex when the key is common, which brings it from 52 s to 0.7 s on the planet, and regex
scans run on 8 threads.

## Speed on the planet

The test ran on an AMD Ryzen 9 3900 with 128 GB of RAM and NVMe disks, with `overpass-rust` and
Overpass API on the same machine and the same planet data. After Rust answered a query, the same
query was sent to the Overpass API container, and the two answers were compared. Both sides were
timed until the last byte of the answer.

Comparison of response time over anonymized user queries:

| | median | 95th percentile | 99th percentile |
|---|---|---|---|
| Overpass API | 295 ms | 6.8 s | 20.5 s |
| overpass-rust | 4 ms | 0.22 s | 2.6 s |

> [!NOTE]
> Compared query by query, the C++ engine took 56 times as long as `overpass-rust` at the median,
> 11 times at the 10th percentile and 689 times at the 90th. For answers under 100 KB the median was
> 84 times. Over all queries the C++ code needed 14 times the total time, and Rust was faster on
> 99.96% of them.

### Why the gap grows with the data

| Data | Measure | overpass-rust faster by |
|---|---|---|
| synthetic, 640k nodes, one process per query | 17 queries, total | 2.5× |
| Luxembourg, 4.9 million elements | typical queries | 1.8× to 17× |
| Germany, 515 million elements, both in RAM | 212 queries, total (geometric mean) | 3.7× (12×) |
| planet, customer queries | total (median) | 14× (56×) |

On small data, most of the time goes into starting a process and parsing the query, on both sides. On
large data it goes into reading indexes and data. On the planet the Overpass API database for current
data is more than three times the size of the RAM, so many C++ queries wait for the disk. The rows use
different queries and compare only roughly.

The Germany run used the Geofabrik extract of 1 November 2026 with metadata, on a Xeon E3-1275 v5
with 8 threads, 64 GB of RAM and NVMe: `overpass-rust` before 0.2.0 versus the
`wiktorn/overpass-api:v0.7.62.11` image, and both servers were queried over HTTP.

| | Overpass API | overpass-rust |
|---|---|---|
| import with metadata | 90 min | 17.5 min |
| database | 28 GB | 9.8 GB |
| 60 minutely planet diffs | 97 s | 7.9 s |
| 212 hand-written queries, warm, sum of medians | 393 s | 106 s |
| 500 generated multi-statement queries (`foreach`, `for`, `make`, `if`) | 775 s | 49 s |
| throughput, 8 clients, queries under 2 s | 13.7 q/s | 56.5 q/s |

## Compatibility

The semantics of overpass-rust follow the specs in `docs/spec/`, which were extracted from the C++
sources. Where the behaviour of the C++ code shows in the output, as with number formatting, `strtod`
quirks, iteration order or area geometry, it is ported. Of 1,096 test queries over twelve datasets,
899 give byte-identical output and 142 give no output on either side, 80 of them rejected by both
with the same error. 12 depend on timing. The other 43 differ on purpose, for example where
Overpass API keeps stale state after a node changes, or finds nothing around a pole. On the Germany
extract, the answers stayed identical through 36 hours of diffs, except where the Overpass API
database itself became inconsistent.

## Build and run

```sh
cargo build --release
B=target/release/overpass

$B --db data/db import region.osm.pbf          # also .osm/.osc, .gz/.bz2; bulk load scales to the planet
$B --db data/db make-areas                     # areas from multipolygon/boundary relations
$B --db data/db replicate                      # apply replication diffs, keep running
$B --db data/db query -q '[out:json];node[amenity=cafe](50.7,7.1,50.8,7.2);out;'
$B --db data/db serve --listen 0.0.0.0:8080    # /api/interpreter, /api/status, /api/timestamp
```

Run `serve` and `replicate` side by side and `make-areas` from time to time. `overpass <command>
--help` lists the options.

## Licence

GNU Affero General Public License, version 3 or later (`LICENSE`). overpass-rust is based on
[Overpass API](https://github.com/drolbr/Overpass-API) by Roland Olbricht et al., and parts of it
are translated from the C++ sources; `NOTICE` lists them. If you run a modified version for others
over a network, the licence requires you to offer them its source.
