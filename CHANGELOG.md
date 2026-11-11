# Changelog

Changes of overpass-rust, newest first. "Overpass API" is the C++ server
(0.7.62) whose answers overpass-rust reproduces.

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
