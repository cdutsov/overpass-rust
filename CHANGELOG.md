# Changelog

Changes of overpass-rust, newest first. "Overpass API" is the C++ server
(0.7.62) whose answers overpass-rust reproduces.

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
