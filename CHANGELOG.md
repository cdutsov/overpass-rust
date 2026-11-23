# Changelog

Changes of overpass-rust, newest first. "Overpass API" is the C++ server
(0.7.62) whose answers overpass-rust reproduces.

## 0.2.8 (2026-11-23)

Faster
- Real queries on the planet, against 0.2.7: 23,085 that take under 1 s use
  1.3 times less time and 1.5 times less CPU, 263 slower ones 2.9 and 2.3
  times less; the slowest, 28 to 43 s on 0.2.7, take about 5 s. The answers
  are the same.
- A region scan with tag filters (`nwr["name"](area.a)`, `node["shop"](box)`)
  tests the tags before it decodes an element, and does not read on in a
  chunk of the database where a key the filters need does not occur.
- `(area)`: the region read for a database area follows the node tiles its
  outline passes, not its block tiles (about 12 x 8 km), so the region of a
  city shrinks to the city. Ways and relations in node tiles inside the area
  pass without node positions; a way whose first vertex is inside passes on
  that vertex. The member ways of relations are read in rounds, only from the
  cells of the area.
- Statements that follow each other with the same `(area.set)` clause build
  its polygons and cell ranges once, while they take at most 64 MiB beyond
  the database areas; they count against the memory limit while kept. The
  database areas of a set are prepared once per query.
- A statement on input sets (`nwr.s[name~"..."]`) walks the smallest set in
  place and copies only what passes. `A->.s; B.s[tags];` runs as one
  statement when nothing else reads `.s`: the tag filters run before the
  spatial tests of A.
- `lat()` and `lon()` compared with a number do not build strings; the
  elements of `make` and `convert` share their name and tags; JSON and XML
  output write numbers and escapes directly.
- Decoding: elements of a chunk are picked by cell runs, tag buffers are
  allocated once, node positions are read without whole nodes, and tag index
  entries are visited without lists per entry.

Changed
- `A->.s; B.s[tags];` as one statement does not store `.s`: with a very small
  `[maxsize:]` such a query answers where 0.2.7 ran out of memory.

## 0.2.7 (2026-11-23)

Fixed: queries that stopped the server or escaped its limits
- An expression with a long chain of operators (`1+1+…`, `a||b||…`, `?:`) of
  about 400 KB overflowed the stack and ended the server process, with every
  query it was running. Expressions are limited to 1024 nested operators and
  XML queries to 1023 levels of nesting; deeper ones are a parse error.
- Parsing an XML query took time growing with the square of its length (1 MB
  of `<print/>`: 45 s, outside any timeout). It is linear.
- The candidates a tag index read collects (`way["highway"]`) did not count
  against the memory limit of the query, so one query could take tens of GB.
  They count, and such a query ends with "Query run out of memory".
- Polygon edges through a pole were interpolated into millions of points that
  did not count against the memory limit. They count.
- `union` held all its parts until the end without counting them: thousands of
  parts could take tens of GB. The parts count against the memory limit and
  merge early when there are many.
- `per_member`, `per_vertex`, the aggregators, `(if:)` and `for` checked
  neither the timeout nor the memory limit while they ran. They do.
- `is_in` did not check the timeout, and `around` over a set checked it only
  between statements.
- Geometry tests (`poly`, `around`, and boxes on ways and relations) built the
  geometry of all candidates at once and counted every node position until the
  statement ended: a large region failed with "out of memory" where the same
  region as a box answered. They work in batches.
- CSV output with more than 10,000 columns is a parse error (400,000 columns
  used up the memory of the server). Compiling the regular expressions of a
  query may take 5 s at most.
- `area(pivot.a)` and `area` with a recursion clause (`area(<)`) returned
  every area of the database. They return no areas.
- `(area)` with a closed way that crosses the antimeridian, or goes around a
  pole, missed elements inside it that `is_in` puts inside: its candidates
  were taken from the box of its vertices. They cover its whole area; for a
  way around a pole that is everything north of it, as with `is_in`.

Fixed: server
- The area rebuild committed its tables every 1 GiB, so queries during the
  rebuild (about two minutes a day) could see part of the areas. The tables
  are replaced in one transaction.
- The spool file of a client that kept reading, slower than the query wrote,
  grew to the size of the whole answer. Parts the client has read are given
  back to the file system, and `serve --spool-max` counts the bytes on disk.
- An answer cut short because the client was too slow, or because the query
  failed, ended like a complete one. The response ends with an error.
- A POST with a query string in the URL ignored its body. The body is used, as
  in the C++ server.

## 0.2.6 (2026-11-18)

Fixed: server
- An answer to a client that reads slower than the server writes ended with
  "runtime error: Query timed out" once the client had taken longer than the
  `[timeout:]` of the query, even when the query itself needed a fraction of a
  second: the time spent waiting for the client counted against the limit. The
  query also held its slot and its database snapshot until the client had read
  the answer. The part of the answer a client has not taken yet now waits in a
  spool (1 MiB in memory per query, the rest in an unlinked file in `TMPDIR`);
  the query finishes, frees its slot and snapshot, and `[timeout:]` counts only
  its own time, as with the C++ server, whose web server buffers the answer.
- Limits of the spool: `serve --spool-max` (default 4 GiB) bounds the spool
  files of all queries; when it is used up, a query waits for its client until
  `[timeout:]` + 30 s. A client that does not take any data for 60 s, or has
  not read the answer 30 minutes after the query ended, loses it, and its
  memory and file are freed.

## 0.2.5 (2026-11-18)

Fixed: replication
- A download of a state file or a diff that stops sending data held up the
  updates for up to 10 minutes (only each read had a time limit), and one that
  sends data very slowly could hold them up for longer. Each download now has a
  limit of 180 s (`replicate --timeout`) with at most 30 s without data; then
  the attempt fails and is tried again.
- When the replication loop makes no progress for 15 minutes, whatever the
  cause, the process exits; the start script of the container starts it again
  after 60 s while the server goes on answering queries.

## 0.2.4 (2026-11-18)

Fixed: replication
- An element of a diff that cannot be read (a number that does not parse,
  coordinates out of range, a node without a position, an unknown member type)
  is left out, logged as `replicate: left out ...` and counted in the info key
  `replicate_skipped`; the rest of the diff is applied. Such an element stopped
  the updates: the same diffs failed on every attempt.
- A file that is not one complete `<osmChange>` document (cut short, empty, an
  error page) fails and is fetched again. It was applied as a diff with fewer
  changes, and its sequence counted as done.
- A panic while applying diffs ends that attempt with an error, not the
  replication process; the start script of the container starts a stopped
  replication again after 60 s, while the server goes on answering queries.
- Within one diff the higher version of an element wins, also when a lower
  version follows it in the file.

Changed: replication
- After a failed attempt the wait doubles from the interval (30 s) up to 10
  minutes, and the diffs of the next batch go in one transaction each, so the
  diffs before a failing one are kept.
- Only the state file of the last diff of a batch is read. When it is missing
  (a gap in the feed), the newest element timestamp of the diffs is the data
  timestamp.
- A feed that is behind the database is an error, logged on every attempt.
- `replicate.status` in the database directory, written after every attempt
  for monitoring: time, sequence, data timestamp, failed attempts in a row, the
  last error and the number of elements left out.
- An error when stored data cannot be decoded names the chunk or index entry.
- `overpass check-ways [--sample N] [--repair]` compares the box and the flags
  of ways with their nodes and computes the ways with problems again.

## 0.2.3 (2026-11-15)

Faster
- `around` and `poly` on a set of relations (`rel.a(around:...)`,
  `nwr.a(poly:...)`) start from the relations of the set. They started from
  every relation near the region and kept those of the set at the end: eight
  such statements on two sets of hiking routes, with circles of 500 m and
  2.5 km, take 0.07 s, 0.73 s before. When the relations of the set have more
  members than the region has nodes, the members are found from the region as
  before.

## 0.2.2 (2026-11-13)

Fixed: answers that differed from Overpass API
- Regular expressions with `,i` compare letters as glibc does (both sides in
  upper case with `towupper`), also in bracket expressions: `i` matches the
  Turkish dotless `ı` (`name~"diyarbakir",i` finds "Diyarbakır"), while `k` no
  longer matches the Kelvin sign and `ß` no longer matches `ẞ`. 24 of the 2,927
  letters with case pairs matched differently. `[A-z]` matches as `[A-Z]`.
- Long regular expressions with many classes, such as word searches of the
  form `(^|[^[:alnum:]])word([^[:alnum:]]|$)|...` with 60 classes, were
  refused as invalid (a limit of 2 MB for the compiled program). The limit is
  32 MB, about 600 such classes, and the lazy DFA cache 8 MB, so they also
  match fast (31,000 names in 2 ms, 110 ms with the cache of 2 MB).
- Intervals as glibc reads them: `a{,2}` is `a{0,2}` and `a{,}` is `a*`
  (both were refused); `a{ 2}` with a space is refused (it was read as `a{2}`).
- Word boundaries `\b`, `\B`, `\<` and `\>` count `[_[:alnum:]]` as word
  characters, as glibc does: a combining mark, ZWNJ and ZWJ, Thai tone marks
  and connector punctuation other than `_` end a word (`\bکتاب\b` finds
  "کتاب‌فروشی"). `\<` is only the start and `\>` only the end of a word
  (`a\<` matched "a b").
- Ways with a missing node in evaluators: `geom()`, `center()`, `length()`,
  `lat()`/`lon()`, `hull` and `trace` follow the rule of `out geom`, so a way
  whose C++ index does not store geometry has none (it used the nodes that
  exist), and relations leave such member ways out. `angle()` at a vertex next
  to a missing node is `NaN` (it was empty).
- An empty key in `make` or `convert` (`""=...`, `!""`) is a static error, as
  in Overpass API; it wrote a tag with an empty key.

Fixed
- `around` near the poles finds everything within the radius: a circle that
  reaches a pole takes every longitude, and near a pole the longitudes are no
  longer capped at the scale of 89.9 degrees. `node(around:2000,...)` at the
  Amundsen–Scott station finds the 6 named nodes within 1.1 km, 1 before.
  Overpass API finds 1 there, and none at the pole itself.

## 0.2.1 (2026-11-11)

Faster
- A statement without a `key=value` filter, on the planet or over a large
  region, whose first filter is a key alone (`["place"]`) and which also has a
  regular expression, takes its candidates from the regular expression when
  the key has more than 500,000 elements there and the expression at most
  500,000. `node["place"]["name"~"Springfield"]` on the planet: 0.7 s (1.9 s
  with the index on disk), 52 s before; `way["highway"]["name"~"..."]`: under
  2 s, over 100 s before.
- A regular expression or a key alone on a key with many values reads the tag
  index on 8 threads: the node names in 1.9 s from disk, 16 s before;
  `nwr["website"~"...",i]` in 0.2 s, 0.6 s before.

## 0.2.0 (2026-11-11)

Fixed: answers that differed from Overpass API
- Regular expressions: the POSIX classes (`[[:alnum:]]`, `[[:upper:]]` and the
  others), `\w` and `\s` match the Unicode sets of glibc in the C.UTF-8 locale,
  so `ü`, `é` and `٣` are letters or digits. With `,i`, `[:upper:]` and
  `[:lower:]` match as `[:alpha:]`. `[.c.]` and `[=c=]` take one ASCII
  character.
- `around` and areas from closed ways find the ways and relations that meet the
  region only along the great-circle arc of a long edge (ferry routes, sea
  boundaries).
- Ways and relations across the antimeridian pass the pre-checks of `around`
  and areas.
- Bounding boxes: a way passes when one of its nodes passes, also for a node on
  the edge of the box.

Changed
- The generator in answers is `Overpass API via overpass-rust v.<version>`.
- A set name with a leading dot in an expression, as in `.a.count(nodes)`, is
  an error that names the fix (`a.count(nodes)`). Overpass API evaluates it on
  a set that does not exist.

Faster
- Statements on the area of a large country compute the cell ranges of the
  area in one walk: 0.05 s for Russia, 4.6 s before, for each way or relation
  statement.
- Relations in an area or a polygon read only the members stored near it; id
  lookups read a bucket only up to the last id they need.
- `poly` and `around` on relations start from the candidate relations and
  decide ways and relations by their stored box where it suffices.
- `out center` and `out bb` take the stored box of complete ways and relations.
- Tag index: area statements start from the most selective `key=value` filter;
  an anchored regex reads the index entries of its prefixes; the candidates of
  several filters are intersected and tested on their tags before they are
  decoded.
- Chunk lookups step a cursor to nearby chunks; parallel sections compare the
  run queue with the number of CPUs.

## 0.1.0 (2026-11-07)

First public release.
