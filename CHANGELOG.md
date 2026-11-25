# Changelog

The changes in each release of overpass-rust, newest first. "Overpass API"
here means the C++ server, version 0.7.62, whose answers overpass-rust
reproduces.

## 0.3.1 (2026-11-25)

### Faster

- `(poly:"...")` with large polygons. The edges of the polygon are sorted
  into rows by latitude, and the test of a point, a segment or a box looks
  only at the edges in the rows it touches; 0.3.0 compared every point and
  segment with every edge. A way or relation whose stored bounding box lies
  wholly inside the polygon passes, and one whose box lies wholly outside
  fails, without building its geometry. Queries with a country outline of
  2,500 to 10,000 points as the polygon, which took 25 s to over 3 minutes,
  take 0.1 to 1 s. Small polygons run as fast as on 0.3.0.

### Fixed

- A query with a polygon stops at its `[timeout:]`. The geometry tests of a
  batch of elements ran without a time check, so a query with a large polygon
  could run about 30 s past its timeout.

## 0.3.0 (2026-11-24)

### Faster

- On the planet, real queries give the same answers as on 0.2.9 and take
  less time. Those that take under 1 s use 1.6 times less time and 1.4 times
  less CPU. In a sample of real queries of the common shapes, up to 20 s,
  those that take 1 s or more on 0.2.9 use 3.4 times less time and 1.7 times
  less CPU; their median falls from 1.4 s to 0.15 s.
- `around` with a large set of ways as the source. Each way of the set is
  first known by its stored bounding box, and the positions of its nodes are
  read only for the ways near a candidate. The cells within the radius are
  found once per statement, a node shared by two segments of a way is tested
  once, and a long segment is covered in pieces along its path, where 0.2.9
  used one box around both ends. A point is compared with bounds of latitude
  and longitude before its distance on the sphere is computed. Examples: the
  forests of Switzerland to hotels within 300 m take 1.0 s (8.6 s on 0.2.9),
  the buildings of Zurich to relations within 10 m 0.8 s (9.5 s), and hiking
  routes within 300 m of the forests of Switzerland 6 to 8 s (over 100 s).
- `around` that finds relations reuses the nodes and ways the statement has
  read and reads the other members without their tags: `foreach` over 600
  peaks with `nwr(around.p:3000)` takes 6.5 s (11.1 s on 0.2.9).
- `(area)` over large areas. The cell ranges of an area are walked with one
  cursor through the database, where 0.2.9 made two searches per range. A
  large area is read as rows of tiles. For an area of more than 4,096 block
  tiles, a block tile is read only when a test needs it. The cell ranges of
  the areas of a query are kept for its later statements. Real area queries,
  weighted by how often each shape occurs, take 2.7 times less time and 1.5
  times less CPU.
- `out meta` finds the version, timestamp and user of an element in the cell
  of the element, without a lookup in the id index, and decodes them only for
  the elements the query keeps. Real bounding-box queries with `out meta`,
  the most frequent shape, take 1.5 times less time and 1.3 times less CPU.
- `node(bbox)` tests each node against the box while it decodes a chunk, so
  nodes outside the box are not kept and sorted. `way(bn)` tests the bounding
  box of a way before its nodes, and decodes tags only for the ways it keeps.
- Statements that scan a region with tag filters skip a chunk whose key table
  does not hold a key the filters need, before reading anything else of it.
  `[~"k"~"v"]` tests the key regex once per key of a chunk. Regex searches in
  the tag index test the first two bytes a match can start with before they
  run the regex. A case-insensitive regex on the names of shops in Russia
  takes 0.9 s (2.2 s on 0.2.9), and one on all names of the planet 1.4 s
  (2.1 s).
- A union moves a set that nothing else holds instead of copying it. A
  statement inside a union skips the elements the union already holds when
  nothing else reads its result.
- Grid statistics with `for` over chains of `?:` that compare `lat()` or
  `lon()` with numbers find the level of a chain by bisection and compare the
  coordinate as a number. They take about 3 times less time.
- Timestamps, the `out meta` attributes in XML, bounding boxes and centers are
  written straight into the output. Output buffers take their size from the
  previous batch, and large parts go to the connection without an extra copy.

### Changed

- `[maxsize:]` counts the nodes of `node(bbox)` after the box test, so it
  follows the memory the query holds. A query close to the limit can pass
  where 0.2.9 stopped with "out of memory"; a query that passed on 0.2.9
  passes on 0.3.0 too.

### Fixed

- An `around` statement that tests relations checks the time and memory
  limits while it tests their cells. On 0.2.9 such a statement could run well
  past `[timeout:]`.

## 0.2.9 (2026-11-24)

### Faster

- A light query does not have to wait for the parallel work of a heavy query
  that runs at the same time. Some parts of a query run on several threads
  (decoding, spatial and area tests, printing), and all queries of a server
  share one pool of threads for them (`--threads`, by default at most 8). In
  earlier versions such a part kept the pool until it was done, so a small
  part of another query could wait seconds behind it. A part that finds the
  pool busy starts on the thread of its own query, where a small part
  finishes. After 10 ms it queues for the pool, and the part that holds the
  pool stops taking new work once it has had the pool for 10 ms and lets the
  waiting part run. The pool threads take work in runs of about 1 ms, so one
  slow item (a chunk read from disk) holds up only one thread.
- Measured with a pool of 8 threads, beside a part of another query that runs
  for 3 s: a part with 40 ms of work takes 14 ms (2.9 s on 0.2.8), and one
  with 400 ms of work takes 100 ms (3.1 s). A query that takes 15 ms alone
  (`out meta` in a city) takes 29 ms in the median and at most 74 ms while a
  heavy `around` query runs on the planet; on 0.2.8 it took 81 ms and at most
  190 ms. A heavy query beside a stream of light ones takes about 6% longer,
  which is the pool time they get. Alone, a heavy query runs as fast as on
  0.2.8.
- On the planet, real queries that take under 1 s give the same answers as on
  0.2.8, with 2% less time and 5% less CPU.

## 0.2.8 (2026-11-23)

### Faster

- On the planet, real queries give the same answers as on 0.2.7 and run
  faster. Those that take under 1 s use 1.3 times less time and 1.5 times less
  CPU, and slower ones use 2.9 times less time and 2.3 times less CPU. The
  slowest, which took 28 to 43 s on 0.2.7, take about 5 s.
- A statement that reads a whole region and filters by tags
  (`nwr["name"](area.a)`, `node["shop"](box)`) tests the tags of an element
  before it decodes the element. It stops reading a chunk of the database (a
  block of stored elements) when a key that the filters need does not occur in
  it.
- `(area)` with an area from the database (`area[...]`) reads a smaller
  region. It used to read every block tile of the area (about 12 x 8 km each).
  It now reads the smaller node tiles (16 x 16 make one block tile) that the
  outline of the area passes or that lie inside it, so for a city the region
  read shrinks to about the city. Ways and relations in node tiles that lie
  wholly inside the area pass without reading any node position, and a way
  whose first node is inside passes after reading that one position. The
  member ways of relations are read only from the cells of the area, and in
  rounds, so that the later rounds skip a relation once one of its member ways
  passes.
- Statements in a row with the same `(area.set)` clause, as in
  `(rel(area.c)[name="x"];rel(area.c)["name:en"="x"];)`, build the polygons of
  the area and the list of cells to read once. The polygons and the list are
  kept for the next statement when they take at most 64 MiB beyond the
  database areas, and they count against the memory limit while kept. The
  database areas of a set are prepared once per query.
- A statement on input sets (`nwr.s[name~"..."]`) goes through the smallest of
  them without copying it, and copies only the elements that pass. A
  query into a set followed by a query that only filters that set by tags,
  `A->.s; B.s[tags];` (for example `nwr(area.c)[name]->.s; nwr.s[name~"x"];`),
  runs as one statement when nothing else reads `.s`. The tag filters then run
  before the spatial tests of A.
- `lat()` and `lon()` compared with a number are compared without first
  turning the coordinate into text. Copies of an element made by `make` or
  `convert` share its name and tags. JSON and XML output write numbers and
  escaped characters straight into the output.
- Decoding elements takes less work. The elements of a chunk are picked by
  runs of elements in the same cell, not one by one; the tags of an element go
  into a buffer that is allocated once, at its full size; node positions are
  read without decoding whole nodes; and the entries of the tag index are read
  without building a list for each entry.

### Changed

- When `A->.s; B.s[tags];` runs as one statement, `.s` is not stored, so with
  a very small `[maxsize:]` such a query answers where 0.2.7 ran out of
  memory.

## 0.2.7 (2026-11-23)

### Fixed: queries that could crash the server or run past their limits

- An expression of about 400 KB with a long chain of operators (`1+1+…`,
  `a||b||…`, `?:`) overflowed the stack and ended the server process, and with
  it every query the process was running. Expressions are limited to 1024
  nested operators and XML queries to 1023 levels of nesting; a deeper query
  is a parse error.
- Parsing an XML query took time that grew with the square of its length: 1 MB
  of `<print/>` took 45 s, outside any timeout. The parsing time grows
  linearly with the length.
- A tag filter such as `way["highway"]` collects a list of elements from the
  tag index, and that list did not count against the memory limit of the
  query, so one query could take tens of GB. The list counts against the
  limit, and such a query ends with "Query run out of memory".
- The evaluators `poly()` and `hull()` interpolated a polygon edge through a
  pole into millions of points that did not count against the memory limit.
  The points count against it before they are made.
- `union` held all its parts until its end without counting them, so thousands
  of parts could take tens of GB. The parts count against the memory limit,
  and they are merged early when there are many.
- `per_member`, `per_vertex`, the aggregators, `(if:)` and `for` checked
  neither the timeout nor the memory limit while they ran, so a query could
  run past both inside them. They check both now.
- `is_in` did not check the timeout, and `around` over a set checked it only
  between statements, so both could run past the `[timeout:]` of the query.
- Geometry tests (`poly`, `around`, and bounding boxes on ways and relations)
  built the geometry of all elements to test at once, and counted every node
  position until the statement ended. So a large region failed with "out of
  memory" where the same region given as a bounding box answered. The tests
  work in batches.
- CSV output with more than 10,000 columns is a parse error (400,000 columns
  used up the memory of the server). The regular expressions of a query may
  take at most 5 s to compile; past that the query is refused.
- `area(pivot.a)` and `area` with a recursion clause (`area(<)`) returned
  every area of the database. These statements do not return areas.
- `(area)` with a closed way that crosses the antimeridian, or goes around a
  pole, missed elements that `is_in` puts inside the way, because it looked
  for them only in the bounding box of its nodes. `(area)` now looks in the
  whole area of the way, which for a way around a pole is everything north of
  it, as with `is_in`.

### Fixed: server

- Queries that ran during the area rebuild (about two minutes a day) could see
  only part of the areas, because the rebuild committed its tables every
  1 GiB. The area tables are replaced in one transaction.
- The spool file that holds the unread part of an answer (see 0.2.6) grew to
  the size of the whole answer when the client kept reading, but more slowly
  than the query wrote. The parts the client has read are given back to the
  file system, and `serve --spool-max` counts the bytes that the files take on
  disk.
- An answer cut short, because the client read too slowly or the query failed,
  ended like a complete answer, so a client could take it for the whole
  answer. Such a response now ends with an error.
- A POST request with a query string in the URL ignored its body. The body is
  used, as in Overpass API.

## 0.2.6 (2026-11-18)

### Fixed: server

- A client that read the answer more slowly than the server wrote it got
  "runtime error: Query timed out" at the end of the answer once the client
  had taken longer than the `[timeout:]` of the query, even when the query
  itself needed a fraction of a second, because the time spent waiting for the
  client counted against the limit. The query also kept its slot
  (`--max-concurrent`) and its database snapshot until the client had read the
  answer. The part of the answer that the client has not read yet waits in a
  spool (1 MiB in memory per query, the rest in an unlinked file in `TMPDIR`).
  The query finishes and frees its slot and snapshot, and `[timeout:]` counts
  only the time of the query itself, as with Overpass API, whose web server
  buffers the answer.
- `serve --spool-max` (default 4 GiB) limits the spool files of all queries
  together. When that is used up, a query waits for its client until
  `[timeout:]` + 30 s. A client that does not read any data for 60 s, or has
  not read the whole answer 30 minutes after the query ended, loses the
  answer, and its memory and file are freed.

## 0.2.5 (2026-11-18)

### Fixed: replication

- Updates could stop for up to 10 minutes when the download of a state file or
  a diff stopped sending data, because only each single read had a time limit,
  and for longer when the download sent data very slowly. Each download now
  has a limit of 180 s (`replicate --timeout`) and may go at most 30 s without
  data; after that the attempt fails and is tried again.
- When replication does not get any further for 15 minutes, whatever the
  cause, the replication process exits. The start script of the container
  starts it again after 60 s, and the server goes on answering queries in the
  meantime.

## 0.2.4 (2026-11-18)

### Fixed: replication

- One element in a diff that could not be read (a number that does not parse,
  coordinates out of range, a node without a position, an unknown member type)
  stopped the updates, because the same diffs failed on every attempt. Such an
  element is left out, logged as `replicate: left out ...` and counted in the
  info key `replicate_skipped`, and the rest of the diff is applied.
- A downloaded file that was not one complete `<osmChange>` document (cut
  short, empty, or an error page) was applied as a diff with fewer changes,
  and its sequence counted as done. Such a file fails and is fetched again.
- A panic while applying diffs ended the replication process. A panic now ends
  only that attempt, with an error. The start script of the container starts a
  stopped replication again after 60 s, while the server goes on answering
  queries.
- When one diff holds two versions of an element, the higher version wins,
  also when the lower one comes later in the file.

### Changed: replication

- After a failed attempt, the wait before the next one starts at the interval
  (`replicate --interval`, 60 s by default) and doubles with each failure in
  a row, up to 10 minutes. The next
  batch then applies each diff in a transaction of its own, so the diffs
  before a failing one are kept.
- Of a batch of diffs, only the state file of the last one is read. When it is
  missing (a gap in the feed), the data timestamp is the newest element
  timestamp in the diffs.
- A feed that is behind the database is an error, logged on every attempt.
- A file `replicate.status` in the database directory is written after every
  attempt, for monitoring. It holds the time, the sequence number, the data
  timestamp, the number of failed attempts in a row, the last error and the
  number of elements left out.
- When stored data cannot be decoded, the error names the chunk or index
  entry.
- `overpass check-ways [--sample N] [--repair]` compares the stored bounding
  box and flags of ways with their nodes, and with `--repair` computes the
  ways with problems again.

## 0.2.3 (2026-11-15)

### Faster

- `around` and `poly` on a set of relations (`rel.a(around:...)`,
  `nwr.a(poly:...)`) start from the relations of the set, so eight such
  statements on two sets of hiking routes, with circles of 500 m and 2.5 km,
  take 0.07 s (0.73 s before). They used to start from every relation near the
  region and keep those of the set only at the end. When the relations of the
  set have more members than the region has nodes, the members are found from
  the region, as before.

## 0.2.2 (2026-11-13)

### Fixed: answers that differed from Overpass API

- Case-insensitive regular expressions (`,i`) matched 24 of the 2,927 letters
  with case pairs differently. They now compare letters as glibc does (both
  sides in upper case with `towupper`), also in bracket expressions. So `i`
  matches the Turkish dotless `ı` (`name~"diyarbakir",i` finds "Diyarbakır"),
  while `k` no longer matches the Kelvin sign and `ß` no longer matches `ẞ`.
  `[A-z]` matches as `[A-Z]`.
- Long regular expressions with many classes, such as word searches of the
  form `(^|[^[:alnum:]])word([^[:alnum:]]|$)|...` with 60 classes, were
  refused as invalid, because the compiled program was limited to 2 MB. The
  limit is 32 MB, about 600 such classes. The cache of the matcher (a lazy
  DFA) is 8 MB, so such expressions also match fast: 31,000 names in 2 ms,
  where a cache of 2 MB took 110 ms.
- Intervals in braces are read as glibc reads them: `a{,2}` is `a{0,2}` and
  `a{,}` is `a*` (both were refused), and `a{ 2}` with a space is refused (it
  was read as `a{2}`).
- Word boundaries `\b`, `\B`, `\<` and `\>` count only `[_[:alnum:]]` as word
  characters, as glibc does. So a combining mark, ZWNJ and ZWJ, Thai tone
  marks and connector punctuation other than `_` end a word, and `\bکتاب\b`
  finds "کتاب‌فروشی". `\<` matches only at the start of a word and `\>` only
  at the end (`a\<` matched "a b").
- For a way with a missing node, the evaluators `geom()`, `center()`,
  `length()`, `lat()`/`lon()`, `hull` and `trace` used the nodes that exist.
  They now follow the rule of `out geom`, so a way whose index in Overpass API
  does not store geometry does not have any, and relations leave such member
  ways out. `angle()` at a vertex next to a missing node is `NaN` (it was
  empty).
- An empty key in `make` or `convert` (`""=...`, `!""`) wrote a tag with an
  empty key. It is a static error, as in Overpass API.

### Fixed

- `around` near the poles missed elements within the radius.
  `node(around:2000,...)` at the Amundsen–Scott station finds the 6 named
  nodes within 1.1 km, where it found 1 before; Overpass API finds 1 there,
  and none at the pole itself. A circle that reaches a pole takes every
  longitude, and near a pole the longitude range of the circle is no longer
  computed as if at 89.9 degrees.

## 0.2.1 (2026-11-11)

### Faster

- `node["place"]["name"~"Springfield"]` on the planet takes 0.7 s (1.9 s when
  the index is read from disk) instead of 52 s, and
  `way["highway"]["name"~"..."]` under 2 s instead of over 100 s. This applies
  to a statement without a `key=value` filter, on the planet or over a large
  region, whose first filter is a key alone (`["place"]`) and which also has a
  regular expression. When the key has more than 500,000 elements there and
  the regular expression at most 500,000, the statement starts from the
  elements that match the regular expression, not from every element with the
  key.
- A filter with a regular expression, or a key alone, on a key with many
  values reads the tag index on 8 threads. The index of node names is read
  from disk in 1.9 s (16 s before), and `nwr["website"~"...",i]` takes 0.2 s
  (0.6 s before).

## 0.2.0 (2026-11-11)

### Fixed: answers that differed from Overpass API

- In regular expressions, the POSIX classes (`[[:alnum:]]`, `[[:upper:]]` and
  the others), `\w` and `\s` matched ASCII only. They now match the Unicode
  sets of glibc in the C.UTF-8 locale, so `ü`, `é` and `٣` are letters or
  digits. With `,i`, `[:upper:]` and `[:lower:]` match as `[:alpha:]`. `[.c.]`
  and `[=c=]` take one ASCII character.
- `around` and areas from closed ways missed the ways and relations that meet
  the region only along the great-circle arc of a long edge (ferry routes, sea
  boundaries).
- Ways and relations across the antimeridian could fail the quick checks that
  `around` and areas make before the exact test.
- A way that touched a bounding box only with a node on its edge was missed,
  although the node passed. A way now passes a bounding box when one of its
  nodes passes.

### Changed

- The generator in answers is `Overpass API via overpass-rust v.<version>`.
- A set name with a leading dot in an expression, as in `.a.count(nodes)`, is
  an error that names the fix (`a.count(nodes)`). Overpass API reads `.a`
  there as a set that does not exist, so the count is always 0.

### Faster

- Statements on the area of a large country work out the ranges of database
  cells to read in one pass: 0.05 s for Russia (4.6 s before), for each way or
  relation statement.
- Relations in an area or a polygon read only those of their members that are
  stored near it. A lookup by id reads a bucket of the id index only up to the
  last id it needs.
- `poly` and `around` on relations start from the relations near the region,
  and decide ways and relations by their stored bounding box where that is
  enough.
- `out center` and `out bb` take the stored bounding box of complete ways and
  relations.
- Tag index: `area` statements, like node, way and relation statements, start
  from the `key=value` filter with the fewest index entries. A regular
  expression anchored with `^` reads the index entries of its prefixes. With
  several filters, the elements they select in the index are intersected and
  tested on their tags before they are decoded.
- Chunk lookups step a cursor forward to nearby chunks instead of searching
  from the root of the tree each time. A query runs its parallel parts on
  several threads only when the run queue of the host leaves enough CPUs idle.

## 0.1.0 (2026-11-07)

First public release.
