<p align="center">
  <a href="https://overpass.ch"><img src="logo.png" alt="overpass.ch" width="209"></a>
</p>

# overpass-rust

overpass-rust is a reimplementation of the [Overpass API](https://wiki.openstreetmap.org/wiki/Overpass_API)
in Rust. It answers queries on current OpenStreetMap data and gives the same answers as Overpass API
0.7.62, so Overpass Turbo, QGIS, OSMnx, overpy and other clients work with it without changes. It
serves the planet queries of [overpass.ch](https://overpass.ch).

In a test with the queries of overpass.ch customers, on the same hardware, overpass-rust was
typically 84 times faster than Overpass API 0.7.62.11 for answers under 100 KB, and 14 times faster
in total time ([how this was measured](#speed-on-the-planet)).

overpass-rust supports Overpass QL on current data, including areas and evaluators. Queries in the
XML query language are translated to QL, except those with evaluator elements. History
queries (`[date:]`, `[diff:]`, `[adiff:]` and the statements that go with them) are rejected with an
error, and `out:popup`, `out:custom` and `local` are not implemented.

## Speed on the planet

The test ran on an AMD Ryzen 9 3900 with 128 GB of RAM and NVMe disks, with overpass-rust and
Overpass API on the same machine and the same planet data. It used overpass-rust 0.2.0 and 0.2.1, in
November 2026. After overpass-rust answered a customer
query, the same query was sent to Overpass API, and the two answers were compared. Both programs
were timed until the last byte of the answer.

Response times on the anonymized customer queries:

| | median | 95th percentile | 99th percentile |
|---|---|---|---|
| Overpass API | 295 ms | 6.8 s | 20.5 s |
| overpass-rust | 4 ms | 0.22 s | 2.6 s |

The percentiles in the table are taken over the times of each program separately. Compared query by
query (the time of Overpass API divided by the time of overpass-rust for the same query), Overpass
API took 56 times as long at the median, 11 times at the 10th percentile and 689 times at the 90th.
For answers under 100 KB the median was 84 times. Over all queries, Overpass API needed 14 times the
total time, and overpass-rust was faster on 99.96% of them.

### Why the gap grows with the data

overpass-rust was also compared with Overpass API on smaller data. Each test used different
queries, so the rows can be compared only roughly. The figure in parentheses is the geometric mean
(Germany) or the median (planet) of the ratio per query.

| Data | Measure | overpass-rust faster by |
|---|---|---|
| synthetic, 640k nodes, one process per query | 17 queries, total | 2.5× |
| Luxembourg, 4.9 million elements | typical queries | 1.8× to 17× |
| Germany, 515 million elements, both in RAM | 212 queries, total (geometric mean) | 3.7× (12×) |
| planet, customer queries | total (median) | 14× (56×) |

On small data, most of the time goes into starting a process and parsing the query, in both
programs. On large data, most of the time goes into reading indexes and data. On the planet, the
Overpass API database for current data is more than three times the size of the RAM, so many queries
on Overpass API wait for the disk.

The Germany test used the Geofabrik extract of 1 November 2026 with metadata. It ran on a Xeon
E3-1275 v5 with 8 threads, 64 GB of RAM and NVMe disks, with overpass-rust 0.1.0 and Overpass API from the Docker image `wiktorn/overpass-api:v0.7.62.11`. Both were queried
over HTTP.

| | Overpass API | overpass-rust |
|---|---|---|
| import with metadata | 90 min | 17.5 min |
| database | 28 GB | 9.8 GB |
| 60 minutely planet diffs | 97 s | 7.9 s |
| 212 hand-written queries, warm, sum of medians | 393 s | 106 s |
| 500 generated multi-statement queries (`foreach`, `for`, `make`, `if`) | 775 s | 49 s |
| throughput, 8 clients, queries under 2 s | 13.7 q/s | 56.5 q/s |

## Why it is faster

Overpass API starts a new process for each request, and that process registers with the dispatcher
(the background process of Overpass API that coordinates queries and database updates) before it
reads any data. In the planet test, the quickest answers of Overpass API took about 50 ms.
overpass-rust answers every query inside one process, `overpass serve`. The database is an LMDB
file (LMDB is an embedded key-value database), which the process reads through a memory map. Each
query sees the data as it was when the query started, while a single writer applies the diffs, so
overpass-rust does not need a dispatcher. The quickest answers of overpass-rust took less than 1 ms.

The overpass-rust database is less than half the size of the Overpass API database. The planet with
metadata and areas takes 190 GiB with overpass-rust and 428 GiB with Overpass API, not counting
attic data (the old versions of elements that Overpass API keeps for history queries). Elements are
stored in chunks of up to 16 KiB, each compressed with lz4. Within a chunk, coordinates are stored
column by column, and each tag key and value is stored once, in a dictionary for the chunk. The
indexes are delta-coded (each number in an index list is stored as the difference to the one
before). On a machine with 128 GB of RAM, the size decides whether the parts of the database that
queries use stay in memory, in the page cache of the operating system, or have to be read from disk.

The map is divided into spatial cells (squares of a grid), and the data is sorted by cell and then
by id. The tag index lists, for each tag, the cell and id of every element with that tag, so the
elements found through a tag are read in the order in which they lie on disk. Every way and relation
stores its bounding box. For `(bbox)`, `around`, `poly` and areas, the box alone decides most cases
(those where it lies wholly inside or wholly outside the region), and then the coordinates of the
nodes are not loaded. `out center` and `out bb` are computed from the stored box.

For each statement, a planner picks where to start (ids, an input set, the tag index or the cells of
a region, whichever is cheapest) and begins with the most selective filter, the one that matches the
fewest elements. Tags are checked before the rest of an element is decoded. For an area,
overpass-rust works out which cells the area covers and reads only those. Within one query,
decoding, geometry tests, output and scans of the tag index run on up to 8 threads, while Overpass
API uses one thread per query.

The releases since the first public one made many queries faster ([CHANGELOG.md](CHANGELOG.md)).
Since 0.2.0, the cells of a large country are worked out in one pass
over the grid, which takes 0.05 s for Russia (4.6 s before), and to find the relations in an area,
overpass-rust reads only the members stored near the area. Since 0.2.1, a statement like
`node["place"]["name"~"Springfield"]`, where the key alone (`place`) matches many elements, starts
from the names that match the regular expression. This brings it from 52 s to 0.7 s on the planet.
Scans of the tag index for a regular expression run on 8 threads. Since 0.2.3, `around` and `poly`
on a set of relations start from the relations of the set, so eight such statements on two sets of
hiking routes take 0.07 s instead of 0.73 s. 0.2.8 tests the tags of an element before it decodes
the element, and reads a smaller region for an area from the database. On real planet queries,
those that took under 1 s on 0.2.7 use 1.3 times less time, slower ones 2.9 times less, and the
slowest went from 28 to 43 s down to about 5 s. Since 0.2.9, a light query does not wait behind the
parallel work of a heavy query that runs at the same time.

## Compatibility

overpass-rust follows the specifications in `docs/spec/`, which were extracted from the C++ sources
of Overpass API. Where details of the C++ code show in the output, such as number formatting, quirks
of `strtod` (the C function that reads numbers), iteration order or area geometry, overpass-rust
reproduces them.

Of 1,103 test queries over 13 datasets, 899 give byte-identical output. 142 return nothing in
both programs, 80 of them because both reject the query with the same error. 12 depend on timing
(Overpass API ran into the timeout of the query when its answers were recorded). The other 50 differ
on purpose, for example where Overpass API keeps outdated geometry for ways and relations after one
of their nodes changes, or finds nothing with `around` at a pole. On the Germany extract, both
programs applied 36 hours of diffs, and the answers stayed identical except where the Overpass API
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

For a server that stays up to date, run `serve` and `replicate` at the same time, and `make-areas`
from time to time to rebuild the areas. `overpass <command> --help` lists the options of each
command.

## Licence

overpass-rust is licensed under the GNU Affero General Public License, version 3 or later
(`LICENSE`). It is based on [Overpass API](https://github.com/drolbr/Overpass-API) by Roland
Olbricht et al., and parts of it are translated from the C++ sources; `NOTICE` lists them. If you
run a modified version for others over a network, the licence requires you to offer them its source.
