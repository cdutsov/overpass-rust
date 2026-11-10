# Evaluators, `make`/`convert`, `(if:)`, `if`, `for`, `local` — implementation spec

Source of truth: C++ tree `Overpass-API/src/overpass_api` (release 0.7.62 line). All `file:line`
citations are relative to that directory. Attic/history (`timeline`, `retro`, `compare`, attic
elements) is out of scope; wherever the C++ handles `Attic<…>` it does exactly what the current
variant does, and you may ignore it.

Notation: `INT(s)`, `DBL(s)`, `PDBL(s)` are the three number recognisers of §1.2; `G14(x)`,
`FIX(x,p)`, `I(n)` are the three formatters of §1.3; `TRUE(s)` is truthiness (§1.4).

---------------------------------------------------------------------------------------------------

## 1. Value model

### 1.1 Types

Every evaluator has a static *return type* checked at parse time:

| type | produced by | consumed by |
|---|---|---|
| `string` | almost everything | all operators/functions, tag values, `if`, `(if:)`, `for` |
| `geometry` | `geom()`, `pt()`, `lstr()`, `poly()`, `center()`, `trace()`, `hull()`, `gcat()`, `?:` with geometry branches | `::geom=`, geometry functions, `gcat()` |
| `container` (list of strings) | only `keys()` | only `for (...)` |

There is no number or boolean type: every runtime value is a byte string. "Numbers" are strings
that the recognisers below accept, results are always re-serialised to strings. Strings are
arbitrary bytes (UTF‑8 in practice); all string comparisons are **byte-wise lexicographic**
(`std::string` operator<, same as Rust `str`/`[u8]` `Ord`).

A geometry-typed evaluator used where a string is expected is a parse error. (If you ever must
stringify one, C++ yields the literals `<Opaque_Geometry>` for `geom()`, `<Point_Geometry>`,
`<Linestring_Geometry>`, `<Polygon_Geometry>` — `statements/item_geometry.h:165`,
`explicit_geometry.h:121,226,340`. Not reachable from valid QL.)

### 1.2 Number recognition (`data/utils.h:64-121`)

All recognisers use the C library (C locale, decimal point `.`). Leading whitespace
(`' ' \t \n \v \f \r`) is skipped by the C functions; **trailing whitespace is NOT accepted** by the
full-match recognisers.

| name | C++ | accepts | value |
|---|---|---|---|
| `INT(s)` | `try_int64`: `s != ""` and `strtoll(s, &end, 0)` consumed the **whole** string | optional ws, optional `+`/`-`, then base‑0 integer: `0x`/`0X` hex, leading `0` = **octal**, else decimal. glibc ≥ 2.38 (C23 mode, the default for g++) also accepts `0b`/`0B` binary — environment dependent, see §13 | `i64`; **overflow saturates** to `i64::MAX`/`i64::MIN` and still counts as success (errno ignored) |
| `DBL(s)` | `try_double`: `s != ""` and `strtod` consumed the whole string | optional ws, sign, decimal with optional fraction/exponent (`1.`, `.5`, `1e3`, `1E-3`), hex floats (`0x1p3`, `0x1A`), `inf`, `infinity`, `nan`, `nan(chars)` — all case-insensitive | `f64`; overflow → ±inf, underflow → 0/denormal, still success (errno ignored) |
| `PDBL(s)` | `try_starts_with_double`: `s != ""`, `strtod` consumed ≥1 char **and errno == 0** | any *prefix* of the above; rest ignored | `f64`; **fails on ERANGE** (`1e400`, `1e-400`) |

Consequences that must be reproduced:

* `INT("010") = 8`, `INT("0x10") = 16`, `INT("08")` fails (but `DBL("08") = 8`), `INT("099")` fails (`DBL` = 99),
  `INT(" 7") = 7`, `INT("7 ")` fails, `INT("+5") = 5`, `INT("1e3")` fails (`DBL` = 1000),
  `INT("99999999999999999999") = 9223372036854775807`.
* `DBL("NaN")`, `DBL("nan")`, `DBL("inf")`, `DBL("-Infinity")` succeed. Hence the evaluator's own
  output `"NaN"` **is a number** for every later operation (`number("x") + 1` → `"nan"`).
  `"NaD"`, `"NaW"` are not numbers.
* `PDBL("information") = inf` (consumes `inf`), `PDBL("nano") = nan`, `PDBL("5.5m") = 5.5`,
  `PDBL("1e") = 1`, `PDBL("-.")` fails, `PDBL(".e1")` fails.

Suffix extraction `double_suffix(s)` (`utils.h:90`): if `s==""` → `""`. Run `strtod`; if errno==0 and
≥1 char consumed: skip whitespace after the number and return the remainder; otherwise `""`.

### 1.3 Number formatting (critical for output compatibility)

| name | C++ | rule |
|---|---|---|
| `I(n)` | `to_string(int64/uint64/uint32)` (`utils.h:46`; `setprecision` has no effect on integers) | plain decimal, `-` sign for negatives, no grouping |
| `G14(x)` | `to_string(double)` = `ostream << setprecision(14) << x` ≡ `printf("%.14g")` | see below |
| `FIX(x,p)` | `fixed_to_string(x,p)` (`utils.h:55`) ≡ `printf("%.{p}f")` | exactly `p` decimals; used with p=3 (length, angle) and p=7 (lat/lon) |

`G14(x)` algorithm (glibc `%.14g`):

1. NaN → `"nan"`, or `"-nan"` if the sign bit is set. ±∞ → `"inf"` / `"-inf"`.
2. Zero → `"0"`, negative zero → `"-0"`.
3. Round the **exact binary value** of `x` to 14 significant decimal digits, ties-to-even
   (glibc semantics). Let `e` be the decimal exponent of the rounded value (`d.ddd…×10^e`).
4. If `-4 <= e < 14`: fixed notation with `13-e` fraction digits; else scientific `d.ddddddddddddde±XX`
   (exponent sign always present, at least 2 exponent digits).
5. Strip trailing zeros of the fraction, then a trailing `.`.

`FIX` also rounds the exact binary value ties-to-even; NaN → `nan`/`-nan`, inf → `inf`;
`-0.0` → `"-0.000"`.

Verified examples (g++/glibc x86‑64):

| value | `G14` | | value | `G14` |
|---|---|---|---|---|
| 8/9 | `0.88888888888889` | | 1e14 | `1e+14` |
| 0.1+0.2 | `0.3` | | 123456789012345 | `1.2345678901234e+14` |
| 6/3 | `2` | | 99999999999999.5 | `1e+14` |
| 1.5+1.5 | `3` | | 12345678901234.5 | `12345678901234` |
| 1e12 | `1000000000000` | | 1e-4 | `0.0001` |
| 3.14159265358979 | `3.1415926535898` | | 1e-5 | `1e-05` |
| (1/3)*3 via strings | `0.99999999999999` | | 5e-324 | `4.9406564584125e-324` |
| -(0.0) | `-0` | | DBL_MAX | `1.7976931348623e+308` |

`FIX`: `FIX(0.0625,3)="0.062"`, `FIX(2.5,0)="2"`, `FIX(3.5,0)="4"`, `FIX(1.0005,3)="1.000"`.
An implementation has to match these tie cases.

Integer arithmetic results (`+ - *`, negation, sums) are formatted with `I`; anything going through
a double with `G14`. A value that "looks integral" can therefore print either way:
`"2"*"3"` → `6` (int path) vs `"2.0"*"3"` → `6` (double path, `G14(6.0)`), but
`"100000000000000000"+"1"` → `100000000000000001` (int) whereas `"1e17"+"1"` → `1e+17` (double).

### 1.4 Truthiness

`TRUE(s)` (`utils.h:124`, used by `!`, `&&`, `||`, `?:`, `(if:)`; `if` uses the identical formula in
`statements/if.cc:69`):

```
if DBL(s) succeeds: return value != 0.0      // NaN != 0 → true
else:               return s != ""
```

| s | TRUE | | s | TRUE |
|---|---|---|---|---|
| `""` | false | | `"false"` | **true** |
| `"0"`, `"0.0"`, `"-0"`, `" 0"`, `"0x0"`, `"0e5"` | false | | `"0 "` (trailing space) | **true** |
| `"1"`, `"abc"`, `"NaN"`, `"nan"`, `"inf"` | true | | `"no"` | true |

Boolean results are always exactly `"1"` or `"0"`.

### 1.5 Dates (`statements/string_endomorphisms.cc:104-175`)

`date(s)` scans `s` for up to six maximal runs of ASCII digits (`'0'..'9'`), skipping any non-digit
bytes before each run: `year, month, day, hour, minute, second`. Missing runs are 0. Runs are
accumulated into `u32` with wrapping (`v = v*10 + d`). Invalid if
`year < 1000 || month > 12 || day > 31 || hour > 23 || minute > 59 || second > 60` → `"NaD"`.
(Month/day 0 are accepted.) Valid →

```
G14( year + month/16 + day/(16*32) + hour/(16*32*32) + minute/(16*32*32*64) + second/(16*32*32*64*64) )
```

computed in `f64` left to right exactly as written. Examples: `date("2006")="2006"`,
`date("2012-09-13")="2012.587890625"`, `date("2013-01-02T12:30:45Z")="2013.0671679527"`,
`date("christmas_day")="NaD"`. `is_date(s)` applies the same parse and returns `"1"`/`"0"`
(`"" → "0"`). Date comparison is just numeric comparison of these encoded doubles
(`date(t["a"]) < date("2015-01-01")`). There is no date arithmetic. `timestamp()` returns
`YYYY-MM-DDTHH:MM:SSZ` strings which compare correctly as strings.

### 1.6 Sentinel strings

| string | produced by |
|---|---|
| `NaN` | `- * /` with non-numbers, unary `-` / `number()` / `abs()` of non-numbers, `sum()` with a non-number, `lat()`/`lon()` without centre, `angle()` degenerate |
| `nan`, `-nan`, `inf`, `-inf` | `G14` of non-finite doubles (e.g. `1/0="inf"`, `0/0="-nan"` on x86‑64) |
| `NaD` | `date()` of an invalid date |
| `NaW` | `is_closed()` on a non-way |
| `< multiple values found >` | `u()` with ≥2 distinct non-empty values |

---------------------------------------------------------------------------------------------------

## 2. Evaluation contexts

### 2.1 Parse contexts (`statements/statement.h:132`)

| context | where | element functions (`t[]`, `id()`, …) | member functions (`pos()`, …) |
|---|---|---|---|
| `generic` → RHS gets `evaluator_expected` | `make` property RHS, `if (...)` condition | **parse error** ("… must be called in a context where it can evaluate an element") | error |
| `in_convert` → RHS gets `elem_eval_possible` | `convert` property RHS | yes | error |
| `elem_eval_possible` | `(if: …)`, `for (…)`, argument of every aggregator (`u`, `set`, `min`, `max`, `sum`, `gcat`) | yes | error |
| `member_eval_possible` | argument of `per_member(…)` / `per_vertex(…)` | yes | yes |

Aggregators, constants, operators, string/list/geometry functions, `<set>.<key>`, `count(…)` are
allowed in every context. Element/member functions take **no arguments and no set prefix**
(`a.id()` and `id(1)` are parse errors). Unknown function name → parse error
`Function "X" not known`. Functions that do **not** exist in this tree (return parse error):
`debug`, `lower`, `upper`, `replace`, `area`, string slicing, etc.

### 2.2 Runtime element context

When an element is "in context" the evaluator sees: element kind, the skeleton (id, way `nds`,
relation `members`), its tags (vector of `(key,value)` in storage order), its geometry (§10.1, only
loaded if some sub-expression needs geometry), its metadata (only for nodes/ways/relations and only
if needed) and the user name. **Areas and deriveds have no metadata; areas have no geometry**
(`statements/evaluator.cc:431-444`). Derived elements' geometry is their stored `::geom` (may be
absent).

### 2.3 The generic key `::`

`::` evaluates to the value of the tag whose key is the *current key*. The current key is only set
while evaluating the RHS of a generic property (`::=…`, §11.1/11.2). Elsewhere `::` yields `""`.
Aggregators pass the current key through to their argument (`u(::)` in `_::=u(::)` works per key).

### 2.4 Aggregators are evaluated eagerly

Aggregators (`u`, `set`, `min`, `max`, `sum`, `gcat`), `count(…)` and `<set>.<key>` are computed
**once, when the statement prepares its tasks**, against the named set as it exists at that moment,
and then behave as constants (`statements/aggregators.cc:78-107`). Inside `convert`/`(if:)`, an
aggregator over `_` therefore sees the whole input set, not the current element.

### 2.5 Missing sets and "null tasks"

If an aggregator names a set that does not exist (never assigned in the current scope chain —
note that **`_` does not exist before the first statement writes it**), the aggregator produces *no
task at all* (`aggregators.cc:80-91`). Consequences:

* as the direct RHS of a `make`/`convert` property: **the tag is omitted entirely** (`set_prop.cc:323,346`);
* as `::geom=` RHS: no geometry;
* as an operand of an operator/function: the operand is treated as `""` (binary/unary/function
  wrappers substitute `""` for a missing child; ternary with missing condition returns `"0"`);
* as the whole `if (…)` condition: C++ dereferences null (crash). Treat as `""` (false).

`count(…)` on a missing set returns `"0"`; `<set>.<key>` returns `""`. An existing but empty set
behaves normally (`u()` → `""`, `sum()` → `"0"`, …).

A set name written with a dot inside an expression (`.a.count(nodes)`, `.a.val`) is taken by C++ as
such a missing set: `count` gives `"0"` and a key `""`, whatever `a` holds, so `if (.a.count(nodes) == 0)`
always takes its first branch. Rust rejects it: `Write a.count(...) instead of .a.count(...); inside an
expression a set name comes without the dot.`

### 2.6 Element iteration order

Every place that iterates a set (aggregators, `convert`, `(if:)`, `for`, `set()`, `gcat`) visits:
nodes, ways, relations, areas, deriveds (attic kinds interleaved after their current kind) and,
within a kind, ascending **storage index** (`Uint32_Index` quad-tile index for nodes,
`Uint31_Index` for ways/relations/areas; all `make`/`convert` deriveds sit at index 0), then
ascending id inside an index bucket (sets are sorted when stored, `dispatch/resource_manager.cc:452`).
This order is observable: derived-id assignment in `convert`, component order of `gcat`, and
floating-point rounding of `sum`.

---------------------------------------------------------------------------------------------------

## 3. Literals and expression syntax

* **Constants** (`tag_value.cc:28-50`): a token that is quoted (`"…"` or `'…'`, JSON-style escapes
  `\n \t \uXXXX \<c>`) or that passes `INT` or `DBL` unquoted. Unquoted numeric tokens are kept
  **verbatim** (`v=1.50` → `"1.50"`, `v=007` → `"007"`; not normalised). Any other bare word is a
  parse error ("Put quotation marks around …"); so `t[name]` is invalid, use `t["name"]`.
  A token like `-5` directly after an operand is split into binary minus + `5`; otherwise it is the
  literal `"-5"` (`frontend/tokenizer_utils.cc:273-300`).
* **Precedence** (`frontend/tokenizer_utils.cc:200-236`), low → high; all binary operators are
  **left-associative**, including `?:`:

| prio | operators |
|---|---|
| 1 | `,` (argument separator) |
| 3 / 4 | `?` / `:` |
| 5 | `\|\|` |
| 6 | `&&` |
| 7 | `==` `!=` |
| 8 | `<` `<=` `>` `>=` |
| 10 | `+` `-` (binary; also prefix `-`) |
| 11 | `*` `/` |
| 12 | `.` (set prefix: `a.u(…)`, `a.val`) |
| 13 | prefix `!` |
| 15 | `::` |

  Quirk: `c1 ? a : c2 ? b : d` parses as `(c1 ? a : c2) ? b : d`. A prefix `-` binds with
  priority 10 (`-a*b` = `-(a*b)`, `a*-b` with a non-literal `b` is a parse error).
* Ternary: condition must be `string`; the type of the `:`-LHS determines the type of the whole
  expression and the RHS must match (so geometry ternaries are allowed).

---------------------------------------------------------------------------------------------------

## 4. Operators (`statements/binary_operators.cc`, `unary_operators.cc`, `ternary_operator.cc`)

Both operands are always evaluated (no short circuit; no side effects exist anyway). Missing
operand → `""`.

| op | rule (L,R are the operand strings) | cite |
|---|---|---|
| `!R` | `TRUE(R) ? "0" : "1"` | `unary_operators.cc:52` |
| `-R` | `INT(R)` → `I(-r)` (wrapping; `-i64::MIN = i64::MIN`); else `DBL(R)` → `G14(-r)` (`-"0.0"` → `"-0"`, `-"0"` → `"0"`); else `"NaN"` | `:65` |
| `L && R` | `TRUE(L) && TRUE(R) ? "1":"0"` | `:176` |
| `L \|\| R` | `TRUE(L) \|\| TRUE(R) ? "1":"0"` | `:189` |
| `== != < <= > >=` | if `INT(L)` and `INT(R)`: compare as i64; else if `DBL(L)` and `DBL(R)`: compare as f64 (IEEE: any comparison with NaN is false, so `NaN==NaN` → `"0"`, `NaN!=NaN` → `"1"`); else byte-wise string comparison. Result `"1"`/`"0"` | `:202-330` |
| `L + R` | both INT → `I(l+r)` (wrapping); both DBL → `G14(l+r)`; else **string concatenation** `L ++ R` | `:340` |
| `L - R` | both INT → `I(l-r)`; both DBL → `G14(l-r)`; else `"NaN"` | `:363` |
| `L * R` | both INT → `I(l*r)` (wrapping, e.g. `1e10`-ints squared → `7766279631452241920`); both DBL → `G14(l*r)`; else `"NaN"` | `:386` |
| `L / R` | **never integer**: both DBL → `G14(l/r)`; else `"NaN"`. `1/0="inf"`, `-1/0="-inf"`, `0/0="-nan"` | `:409` |
| `C ? A : B` | `TRUE(C)` → evaluate and return `A`, else `B` (lazy). Missing branch → `""`; missing condition → `"0"` | `ternary_operator.cc:110` |

"Both INT" means **both** sides pass; mixing an int with a non-int double falls to the double path.
Non-transitive equality results from octal parsing: `"010"=="8"` → `1`, `"010"=="10"` → `0`,
`"010"=="10.0"` → `1`, `"99"=="099"` → `1`; saturation: `"99999999999999999999"=="99999999999999999998"` → `1`.

NaN sign: x86‑64 produces a *negative* default NaN for invalid operations on non-NaN operands
(`0/0`, `inf-inf`, `0*inf`) and propagates an existing NaN operand unchanged (first operand wins if
both NaN). `G14` prints the sign. Strings parsed from `"nan"`/`"NaN"` are positive NaN.

Test vectors (from `make.test.cc` / `osm-backend/generate_test_file.cc`):

| expr | result | | expr | result |
|---|---|---|---|---|
| `"1" + "0 "` | `10 ` | | `"9.5" == "9.50"` | `1` |
| `" 1" + "10"` | `11` | | `"nine" == "nine "` | `0` |
| `" 1" + "2_"` | ` 12_` | | `"99" == "99 "` | `0` |
| `"5.5" + "3.5"` | `9` | | `"9.5" < "10"` | `1` |
| `"100000000000000000"+"1"` | `100000000000000001` | | `"1" < "a"` | `1` |
| `"2" * "6.5"` | `13` | | `" " < "1"` | `1` |
| `"_2" * "7"` | `NaN` | | `"a" <= "a.0"` | `1` |
| `"2" - "5"` | `-3` | | `"9.0" <= "9"` | `1` |
| `-"-3."` | `3` | | `"8" / "9"` | `0.88888888888889` |
| `-"one"` | `NaN` | | `"true" && ""` | `0` |
| `"false" && "false"` | `1` | | `!"false"` | `0` |
| `"" \|\| ""` | `0` | | `!""` | `1` |

---------------------------------------------------------------------------------------------------

## 5. Element functions

They need an element in context (parse error otherwise, §2.1). If one is ever evaluated without an
element it yields `""` (`count_*` yield `"0"`).

| function | node | way | relation | area | derived | cite |
|---|---|---|---|---|---|---|
| `t[<expr>]` | value of the **first** tag whose key == `<expr>` (evaluated in the same context), else `""` | same | same | same | same | `tag_value.cc:117,137,216` |
| `is_tag(<literal>)` | `"1"` if any tag has that key else `"0"`. Argument must be a single literal token (quoted or bare), not an expression | same | same | same | same | `tag_value.cc:267,287` |
| `::` | `t[current key]` or `""` (§2.3) | | | | | `tag_value.h:383` |
| `keys()` | container: all keys, sorted byte-wise, deduplicated | | | | | `tag_value.cc:329` |
| `id()` | `I(id)` | `I(id)` | `I(id)` | `I(area id)` | `I(u64 id)` | `tag_value.h:110` |
| `type()` | `node` | `way` | `relation` | `area` | its type name (e.g. `stat`) | `tag_value.h:162` |
| `version()` | `I(version)` | ← | ← | `""` | `""` | `tag_value.h:551` |
| `timestamp()` | `YYYY-MM-DDTHH:MM:SSZ` (zero padded) | ← | ← | `""` | `""` | `core/datatypes.h:681` |
| `changeset()` | `I(changeset)` | ← | ← | `""` | `""` | |
| `uid()` | `I(uid)` | ← | ← | `""` | `""` | |
| `user()` | user name for uid, `""` if unknown | ← | ← | `""` | `""` | |
| `count_tags()` | number of tag entries (`I`) | ← | ← | ← | ← | `tag_value.cc:645` |
| `count_members()` | `"0"` | `I(nds.len())` | `I(members.len())` | `"0"` | `"0"` | `:662,733` |
| `count_members(nodes\|ways\|relations\|rels)` | `"0"` | **`"0"`** (type filter unsupported for ways) | count of members of that type | `"0"` | `"0"` | |
| `count_distinct_members()` | `"0"` | number of distinct node ids | see below | `"0"` | `"0"` | |
| `count_distinct_members(type)` | `"0"` | **`"0"`** | see below | `"0"` | `"0"` | |
| `count_by_role("r")` / `count_by_role("r", type)` | `"0"` | `"0"` | members with role `r` (and type) | `"0"` | `"0"` | `:700` |
| `count_distinct_by_role("r"[, type])` | `"0"` | `"0"` | see below | `"0"` | `"0"` | |
| `is_closed()` | `NaW` | `"1"` iff `nds` non-empty and `nds.first == nds.last` (a 1-node way is closed) else `"0"` | `NaW` | `NaW` | `NaW` | `item_geometry.h:51` |
| `geom()` | geometry (§10.1) | ← | ← | Null geometry | its geometry or Null | `item_geometry.h:113` |
| `length()` | `"0"` | `FIX(length(geom),3)` (or `"0"` if no geometry) | `FIX(…,3)` | `"0"` | **`"0"` always** | `item_geometry.h:184` |
| `lat()` / `lon()` | `FIX(center_lat/lon,7)` if geometry has a centre, else `"NaN"` | ← | ← | `"NaN"` | ← | `item_geometry.h:255-348` |

Distinct-member counting for relations (`tag_value.cc:733-780`): filter members by criterion
(role id must equal the role's id for `*_by_role`; member type if given); sort by `(type, ref)`;
count runs of adjacent entries that are **equal in `(ref, type, role)`**. Thus the same object
referenced with two different roles counts twice. C++ uses `std::sort` (unstable); for ≤16 members
libstdc++ is effectively stable — implement as a *stable* sort by `(type,ref)` then count adjacent
distinct `(type,ref,role)`. Role lookup: a role string never seen in the database has no id → count
`0`. `count_*` keyword arguments are bare words `nodes`, `ways`, `relations`, `rels`; the role must
be a single literal.

Note: `t[...]` inside `per_member` evaluates its key expression without member position.

---------------------------------------------------------------------------------------------------

## 6. Member functions (`statements/per_member.cc`, `per_member.h`)

`per_member(E)` / `per_vertex(E)` evaluate `E` once per position and join the results with `;`
(no sorting, no dedup, empty results kept, e.g. `";;"` possible).

| enumerator | node / area / derived | way (`n = nds.len()`) | relation (`n = members.len()`) |
|---|---|---|---|
| `per_member(E)` | `""` (E not evaluated) | positions `0..n-1` (`""` if `n==0`) | positions `0..n-1` |
| `per_vertex(E)` | `""` | if `n > 2`: positions `1..n-2`, plus `n-1` if `nds.first == nds.last`; else `""` | `""` |

Inside `E` (position `p`, 0-based):

| function | way | relation |
|---|---|---|
| `pos()` | `I(p+1)` | `I(p+1)` |
| `mtype()` | `node` | `node`/`way`/`relation` (`invalid` for corrupt types) |
| `ref()` | `I(nds[p])` | `I(members[p].ref)` |
| `role()` | `""` | role string of `members[p]` |
| `angle()` | see below | `""` |

Operators/functions/ternaries forward the position to their children; element functions ignore it.

`angle()` (`per_member.cc:242-301`), way only, needs geometry:
* no geometry → `""`. If `p+1 < n` or the way is closed → compute, else `""`. `p == 0` → `""`.
* Cache: if the way geometry is a single linestring (`has_line_geometry`), `C[i] = cart(lat_i, lon_i)`
  for every way position (invalid positions → `(0,0,0)`), else `C` is empty (C++ then reads out of
  bounds — return `""`). `cart(lat,lon) = (sin φ, cos φ·sin λ, cos φ·cos λ)` with
  `deg→rad = acos(0)/90`.
* `prev=C[p-1]`, `mid=C[p]`, `next = C[p+1]` if `p+1 < len(C)` else `C[1]`.
* `in = mid × prev`, `out = next × mid` (standard cross product). If `|in| < 1e-10` or
  `|out| < 1e-10` → `"NaN"`.
* `prod = (in·out)/|in|/|out|`; `cw = (mid−prev)·out`.
* `|prod| > 1` → `"0.000"`; else `FIX(copysign(acos(prod)/acos(0)*90, cw), 3)` (may print `-0.000`).

---------------------------------------------------------------------------------------------------

## 7. String functions (`statements/string_endomorphisms.cc`) — one argument each

| function | result |
|---|---|
| `number(s)` | `INT(s)` → `I(v)` (so `number("010")="8"`, `number("0x10")="16"`, `number(" 12")="12"`); else `PDBL(s)` → `G14(v)` (`"3."`→`3`, `"1e12"`→`1000000000000`, `"5.5m"`→`5.5`, `"12 "`→`12`, `"information"`→`inf`, `"nano"`→`nan`); else `"NaN"` (`""`, `"-."`, `"1e400"`, `"1e-400"`) |
| `is_number(s)` | same tests → `"1"`/`"0"` (`is_number("5 apples")="1"`, `is_number("")="0"`) |
| `suffix(s)` | `double_suffix(s)` (§1.2): `""`→`""`, `"1e100"`→`""`, `"5.5m"`→`m`, `"200 "`→`""`, `"40 m/s"`→`m/s`, `"4 2"`→`2`, `"3,14"`→`,14`, `"1e"`→`e`, `"9 3/4"`→`3/4`, `"abc"`→`""` |
| `abs(s)` | like `number` but absolute value (`abs(i64::MIN)` stays negative): `"-3.14"`→`3.14`, `"-1e12"`→`1000000000000`, `"-42"`→`42`, `"-."`→`NaN` |
| `date(s)` / `is_date(s)` | §1.5 |

### 7.1 List-represented sets (`statements/set_list_operators.cc`)

`trim(x)` (`:24`) — **buggy, reproduce exactly**: `from` = index of first non-space byte
(`isspace`, C locale); if none (including `x==""`) return `" "` (a single space). `to` = index of
last non-space byte. Return `x.substr(from, to+1)` i.e. `to+1` bytes starting at `from`, clamped
to the end. Hence `" a "` → `"a "`, `"  a  "` → `"a  "`, `" a"` → `"a"`, `"a "` → `"a"`.

`members(L)`: `""` → empty list; else split on every `;`, `trim` each piece (empty pieces become
`" "`), sort byte-wise, dedup.

| function | rule |
|---|---|
| `lrs_in(x, L)` | `L == ""` → `"0"`. Else `t = trim(x)`; split `L` on `;`, `"1"` if any `trim(piece) == t`, else `"0"` (no dedup/sort needed). `lrs_in("", "a;")` → `"1"` (both trim to `" "`) |
| `lrs_isect(A, B)` | sorted set intersection of `members(A)`, `members(B)`, joined with `;` (`""` if empty) |
| `lrs_union(A, B)` | sorted set union, joined with `;`. `lrs_union("a;;b","")` → `" ;a;b"` |
| `lrs_max(L)` / `lrs_min(L)` | `L == ""` → `""`. Else fold over `trim`med pieces (not deduped) with the min/max state machine of §9.2 but starting in state *int* (not void); result as in §9.2 |

Test vectors (`make.test.cc` lrs_test → `generate_test_file.cc`):

| expr | result | | expr | result |
|---|---|---|---|---|
| `lrs_max("  foo")` | `foo` | | `lrs_max("9;10")` / `lrs_min("9;10")` | `10` / `9` |
| `lrs_max("1e3")` | `1000` | | `lrs_max("1e-2;1e-1")` / `lrs_min(…)` | `0.1` / `0.01` |
| `lrs_max("100000000000000001")` | `100000000000000001` | | `lrs_max("9 bis;10 bis")` / `lrs_min(…)` | `9 bis` / `10 bis` |
| `lrs_max(".;9;10")` | `9` (string order) | | `lrs_min("a;9;10")` | `10` (string order) |
| `lrs_isect("\t","\n")` | ` ` (one space) | | `lrs_isect("a;;b","a; ")` | ` ;a` |
| `lrs_union("a;b","a; ")` | ` ;a;b` | | `lrs_in("  ","\t")` / `lrs_in("  ","")` | `1` / `0` |
| `lrs_in("","a;  ;c")` | `1` | | `lrs_isect(" foo"," foo")` | `foo` |

---------------------------------------------------------------------------------------------------

## 8. Set properties: `<set>.<key>` (`statements/runtime_value.cc`)

`a.val` (set name and key are bare identifiers; set is mandatory, `_.val` for the default set) →
the string stored under key `<key>` for set `<set>`, `""` if none. The only writer is `for`
(key `val`, §11.5). Set key-values are **erased whenever the set is reassigned** by any statement
(`resource_manager.cc:139-146`): after `make`/query/etc. writes `_`, `_.val` becomes `""`.

---------------------------------------------------------------------------------------------------

## 9. Aggregators (`statements/aggregators.cc`, `aggregators.h`)

Syntax: `f(E)` (set `_`) or `<set>.f(E)` (`a.u(t["name"])`; set name without leading dot).
`E` is evaluated for every element of the set in the order of §2.6, with the current generic key
passed through. Result computed eagerly (§2.4). Missing set → null task (§2.5).

### 9.1 `u`, `set`

| | rule | empty set / all-empty |
|---|---|---|
| `u(E)` | ignore `""` values; first non-empty value is kept; a different non-empty value turns the result into `< multiple values found >` (permanently) (`:199`) | `""` |
| `set(E)` | distinct non-empty values, sorted byte-wise, joined by `;` (no escaping of `;` inside values) (`:349-361`) | `""` |

### 9.2 `min`, `max` (`:213-293`)

State: `type ∈ {void<int<double<string}` = void; `l` = `i64::MAX` (min) / `i64::MIN` (max);
`d` = `f64::MAX` (min) / `-f64::MAX` (max); `s = ""`. For **every** value `v` (including `""`):

```
if type == void: type = int
if type <= int:    if INT(v): l = min(l, v_int)    else type = double
if type <= double: if DBL(v): d = min(d, v_dbl)    else type = string
if v != "":        s = (s == "") ? v : min_bytewise(s, v)
```
(max analogous). `min(d,x)` is C++ `std::min` = `x < d ? x : d` (a NaN `x` never replaces `d`;
`std::max(d,x)` = `d < x ? x : d`). Result: void → `""`; int → `I(l)`; double → `G14(d)`;
string → `s`.

Key consequence: **a single element without the tag (value `""`) switches the result to
lexicographic comparison** of the non-empty values: `min` over `{"10","9",""}` is `"10"`. Values
`{"10","9"}` give `"9"`. All-NaN doubles give `G14(f64::MAX)` = `1.7976931348623e+308`.

### 9.3 `sum` (`:309-336`)

State: `type = int`, `l = 0`, `d = 0.0`. For each `v`:
```
if type == int:              if INT(v): l += v (wrapping)  else type = double
if type in {int, double}:    if DBL(v): d += v             else type = string
```
Result: int → `I(l)`; double → `G14(d)`; string → `"NaN"`. Empty set → `"0"`. Any `""` value → `"NaN"`.
(`d` accumulates all values in iteration order, so rounding follows §2.6 order.)

### 9.4 `count(<type>)` (`:370-505`)

`count(T)` / `<set>.count(T)` with `T` a bare word: `nodes`, `ways`, `relations`, `deriveds`,
`nwr`, `nw`, `wr`, `nr` (no `rels`, no `areas`). Counts elements of those kinds in the set
(attic included); **`deriveds` counts areas + deriveds**. `I(count)`. Missing set → `"0"`.
Test: set {1 node, 1 way, 1 relation, 1 derived} → nodes 1, ways 1, relations 1, deriveds 1,
nwr 3, nw 2, wr 2, nr 2.

### 9.5 `gcat(G)` (geometry; `:519-535`)

Evaluates geometry `G` per element; every non-null result (note: `geom()` of an area/derived
without geometry returns a *Null geometry object*, which is non-null and therefore included as an
empty component) is appended, in iteration order, to a new Compound geometry. If no element
yielded a non-null geometry the result is *null* (no geometry). Duplicates are kept.

---------------------------------------------------------------------------------------------------

## 10. Geometry

### 10.1 Geometry model and element geometries (`core/geometry.h`, `statements/evaluator.cc:220-444`)

Variants (all coordinates are `(lat, lon)` doubles):

| variant | centre | bbox | "line" | "multiline" | components |
|---|---|---|---|---|---|
| Null | no | no | – | – | – |
| Point | the point | point | – | – | – |
| Linestring(pts) | bbox centre (always "has centre", even when empty → invalid bbox ⇒ centre (100,200)) | yes | pts | – | – |
| PartialWay(pts with placeholders `(100,200)`) | bbox centre of valid pts, only if ≥1 valid | same | the single valid run if exactly one | list of maximal runs of valid points | – |
| RHR polygon(rings) | bbox centre (always "has centre") | yes | – | rings | – |
| Compound(comps) | bbox centre iff merged bbox valid | merged | – | – | comps |
| PartialRelation(comps) | as Compound, but only if ≥1 valid coordinate | | – | – | comps |

Element geometries (computed from the database, not clipped):
* node → Point at its stored coordinate.
* way → if all node coordinates are known: Linestring of them (empty way → empty Linestring, so
  `lat()` of a node-less way is `"100.0000000"` and `lon()` `"200.0000000"`); otherwise PartialWay
  with `(100,200)` placeholders for missing nodes.
* relation (`evaluator.cc:284-360`) → one coordinate list per member: node member → 1 coordinate
  (invalid if the node is missing), way member → its node coordinates (empty if the way is missing),
  **relation member → empty list**. If no list is empty and all coordinates are valid: Compound with,
  per member, Point (list of length 1, i.e. node or 1-node way) or Linestring (≥2). Otherwise
  PartialRelation with, per member: empty list → Null; length-1 list with valid coordinate → Point;
  anything else → PartialWay (invalid coordinates become `(100,200)` placeholders; a missing node
  member thus becomes a PartialWay with a single placeholder — renders like Null). Relation without
  members → Compound with no components (no centre).
* area → none (null). derived → its `::geom` or none.

Bounding boxes (`core/geometry.cc:123-170, 371-425, 1686-1737`):
* point list: min/max over valid points (lat < 100). None valid → invalid bbox
  `(s,w,n,e)=(100,200,100,200)`. If `east - west > 180`: recompute
  `ww = min lon over lon > 0` (init 180), `we = max lon over lon <= 0` (init −180); if `ww − we > 180`
  return `(s, ww, n, we)` (date-line crossing, west > east) else `(s, −180, n, 180)`.
* compound: merge component bboxes (components with a bbox only); `wrapped` if any component has
  `east < west`. Nothing → invalid. If not wrapped and `east−west <= 180` → plain merge. Else
  `west=200, east=−200`; for each component: if `c.east <= 0`: `east = max(east, c.east)` and if
  `c.west >= 0` also `west = min(west, c.west)`; elif `c.west >= 0`: `west = min(west, c.west)`;
  else return `(s,−180,n,180)`. Finally `west − east > 180` → `(s,west,n,east)` else `(s,−180,n,180)`.
* bbox valid ⇔ `−90 <= s <= n <= 90` and `w, e ∈ [−180,180]`.
* centre: `lat = (s+n)/2`; `lon = (w+e)/2` if `w <= e`, else `(w+e)/2 + 180` if `(w+e)/2 <= 0`,
  else `(w+e)/2 − 180`.

### 10.2 Length (`core/geometry.cc:2104-2145`)

```
gc(lat1,lon1,lat2,lon2):
  if lat1==lat2 && lon1==lon2: return 0
  k = acos(0)/90                                   // deg → rad
  sp = cos((lat2-lat1)k) + cos(lat1 k)·cos(lat2 k)·(cos((lon2-lon1)k) − 1)
  if sp > 1: sp = 1
  return acos(sp) · (10_000_000 / acos(0))         // metres; sphere of 40 000 km circumference
length(g): components → sum of length(component);
           has line → sum of gc over consecutive points;
           else faithful way geometry (PartialWay with >1 run) → sum over consecutive positions where
                both are valid; otherwise 0.
```
`length()` prints `FIX(length, 3)`. Polygon/Point → 0.

### 10.3 Geometry functions

| function | rule |
|---|---|
| `pt(lat, lon)` (two string args, evaluated with no generic key) | `DBL(lat)` ∈ [−90,90] and `DBL(lon)` ∈ [−180,180] → Point, else Null (`explicit_geometry.cc:22`) |
| `lstr(g1, g2, …)` | Linestring of the centres of those args that are non-null and have a centre (others skipped). Zero/one point allowed |
| `poly(l1, l2, …)` | each arg with a *line* geometry and ≥2 points becomes a ring (closed by appending its first point if needed); then RHR normalisation (§10.4) |
| `center(g)` | null → null; has centre → Point(centre); else Null |
| `trace(g)` | null → null; else §10.5 (always a Compound) |
| `hull(g)` | null → null; else §10.6 |
| `geom()` | element geometry (clone) or Null (§5) |
| `gcat(g)` | §9.5 |
| `c ? g1 : g2` | geometry ternary; condition evaluated with no generic key |

### 10.4 RHR polygon normalisation (`core/geometry.cc:635-1585`)

Used by `poly()` and `hull()` (≥3 hull vertices). Observable behaviour (port the algorithm
verbatim; it deserves its own sub-spec):
1. Every segment whose |Δlat| or |Δlon| ≥ 0.0065536° is densified along the great circle
   (`interpolate_segment`, recursive refinement near the vertex of maximal latitude; segments
   spanning ≥ 180° of longitude are routed via the pole). Inserted vertices appear in the output.
2. Segments are split at mutual intersections; rings are re-assembled by following, at each shared
   point, the next segment in angular (`atan2(Δlon, Δlat)`) order; consecutive near-duplicate
   vertices (|Δ| < 1e-7) are merged; rings with ≤ 2 points are dropped; each ring is closed
   (last == first).
3. Each ring is reversed if necessary so that the polygon interior lies on the right-hand side
   (outer rings clockwise), decided by an inside test at the ring's second vertex.
Ring output order follows the segment index order of the assembly loop.

### 10.5 `trace` (`core/geometry.cc:2147-2285`)

1. Collect recursively: components → recurse; line geometry → one linestring; multiline → each run
   as a linestring; else if it has a centre → one *node* point.
2. Nodes: sort by `(lat, lon)`, dedup exact duplicates.
3. `cnt[p]`: +1 per distinct node; per non-empty linestring: first and last point +2, every interior
   point +1.
4. Output Compound: first one Point per node (sorted order); then for each linestring in
   collection order: split at every interior point with `cnt > 1` (the split point ends one piece and
   starts the next); append each piece as a Linestring unless an equal piece was already appended,
   where equality is "same length and same point sequence after orienting each piece so it starts at
   the smaller endpoint (by `(lat,lon)`)". A 1-point linestring yields piece `[p, p]`.

### 10.6 `hull` (`core/geometry.cc:2318-2533`)

1. Collect all points recursively: components → recurse; line → all its points; multiline → all
   points of all runs/rings; else centre if any.
2. Sort by `(lon, lat)`, dedup exact duplicates. 0 → Null; 1 → Point; 2 → Linestring `[a, b, a]`.
3. `S` = first point (in sorted order) with the strictly smallest lat; `N` = first with the
   strictly largest lat. Largest longitude gap: start with `gap = P[0].lon + 360 − P[last].lon`,
   `W = P[0]`, `E = P[last]`; for `i = 1..`: if `P[i].lon − P[i−1].lon > gap`: `gap = that`,
   `W = P[i]`, `E = P[i−1]`.
4. Spherical vectors `v(p) = (sin φ, cos φ sin λ, cos φ cos λ)`; `X(a,b)` = normalised `a × b`.
   Hull = cyclic list of segments `(pt, v, edge)`. Init: if `W.lon != E.lon`: `[W, E]`; elif
   `N.lat != S.lat`: `[S, N]`; else empty. If non-empty: `seg1.edge = X(v0, v1)`, `seg0.edge = X(v1, v0)`.
5. For every point `p` in sorted order, `enhance(p)` with `s = v(p)`, `vis(i) = seg[i].edge·s >= 0`:
   ```
   i = first index with seg[i].edge·s >= 1e-8; if none: return
   if vis(0):
       j = last; while j != 0 and vis(j): j--;  erase seg[j+1 ..]
       k = 0; while k < len and vis(k): k++;  k--
       seg[k].edge = X(s, seg[k].v)
       if k == 0: insert new(p) at 0
       else: erase seg[1 .. k-1]; seg[0] = new(p)            // new at index 0
   else:
       t = i; while t < len and vis(t): t++;  t--
       f = i; while f != 0 and vis(f): f--;  f++
       seg[t].edge = X(s, seg[t].v)
       if f == t: insert new(p) at f
       else: erase seg[f+1 .. t-1]; seg[f] = new(p)
   new.edge = X(prev.v, s) where prev = cyclic predecessor of new (last element if new is at 0)
   ```
   ("erase a..b" is inclusive; empty when b < a.)
6. Vertex list = `pt` of segments in order. `< 3` → Linestring(list); else RHR polygon of the single
   ring (closed, densified, oriented per §10.4).

---------------------------------------------------------------------------------------------------

## 11. Statements

### 11.1 `make` (`statements/make.cc`, parser `frontend/map_ql_parser.cc:821-872`)

```
make <Type> [<prop> {, <prop>}] [->.<set>];
<prop> ::= <key> = <expr>        tag (key: identifier or quoted string, JSON-decoded)
         | !<key>                declare key without value ("shrek": excluded from generic copy)
         | <Set>::= <expr>       generic: one tag per key found in <Set>
         | ::id = <expr>
         | ::geom = <geometry expr>
```
* Parse context `generic`: element functions are errors; aggregators/constants/operators/`count`/
  `<set>.val` work. Static errors: same key twice, >1 `::id`, >1 `::geom`, >1 generic, empty type.
  `::=` without a set name is an error in `make` ("…can only be called from convert").
* Execution: build one derived element `{type: <Type>, id: 0, tags: [], geometry: none}`; process
  the props **in source order**:
  * `k = e` → append `(k, value)` — always appended, **also when the value is `""`** (printed as
    `v=""`); omitted only if the RHS is a null task (§2.5).
  * `!k` → nothing.
  * `S::=e` → for every distinct key occurring on any element of set `S` (iteration order irrelevant;
    keys sorted byte-wise) **except declared keys** (all keys named by `k=…` or `!k` in this
    statement), append `(key, e evaluated with current key = key)` in sorted key order. Missing set → nothing.
  * `::id=e` → if `INT(value)` succeeds: id = that value reinterpreted as u64 (`-5` →
    `18446744073709551611`); otherwise ignored.
  * `::geom=g` → geometry = result (null → none).
* If no valid `::id`, id = next value of the **global derived-id counter** (per request, starts at 0,
  pre-incremented → first id is 1; shared by all `make`, `convert`, `local` statements in the
  request; not incremented when `::id` succeeded) (`core/parsed_query.h:47`).
* Output set (`_` or `->.x`) is replaced by a set containing exactly this one derived.

Examples (expected test output): `make one;` → `<one id="1"/>`;
`make with-tags not="in", alphabetic="order";` → tags in source order;
`make stat nodes=count(nodes), …` → tags `nodes,ways,…` in order written.

### 11.2 `convert` (`statements/convert.cc`)

```
[.in] convert <Type> <prop> {, <prop>} [->.out];
<prop> as for make, plus  ::= <expr>   (generic over the current element's own keys)
```
* Parse context `in_convert`: RHS may use element functions (§5) of the current element.
* For each element of the input set in §2.6 order (nodes, ways, relations, areas, deriveds): create
  a derived `{type, id 0}` and process props in source order:
  * `k = e` → append `(k, e(elem))` (empty values kept).
  * `::=e` → the element's keys, sorted byte-wise, deduplicated, minus declared keys; append
    `(key, e(elem) with current key)` each. So `::=::` copies all (non-declared) tags in sorted key
    order; `!k` removes `k` from the copy.
  * `S::=e` (with set) → like make's generic over set `S`, but each value evaluated with the
    element in context.
  * `::id=e`, `::geom=g` as in `make`, evaluated per element (`::geom=geom()` copies the element's
    geometry; for areas this is a Null geometry).
  * Then assign a dispensed id if none was set.
* Ids are therefore dispensed in **processing order (kind, storage index, id)**, not in element-id
  order. The output set contains only the new deriveds. Missing input set → empty output.
* Test: `convert rewrite ::=::, !node_key, extra_key="extra_value"` on a node with
  `{node_key, node_key_7}` → tags `node_key_7, extra_key`.

### 11.3 `(if: <expr>)` query filter (`statements/filter.cc`)

A query clause; `<expr>` is evaluated (context `elem_eval_possible`, no generic key) for every
candidate produced by the other clauses of the query; elements with `TRUE(result)` are kept,
relative order preserved. It never selects data on its own (constraint strategy `ids_required`);
the query spec defines how candidates are produced. Aggregators inside refer to named sets as of
the start of the query.

### 11.4 `if` / `else` statement (`statements/if.cc`)

```
if (<expr>) { … } [else { … }]
```
`<expr>` in context `evaluator_expected` (no element functions), evaluated once; `TRUE` → run the
then-block else the else-block. No new scope: sets assigned inside are ordinary assignments.

### 11.5 `for` (`statements/for.cc:182-266`)

```
[.in] for [->.loop] (<expr>) { … }        // .in and ->.loop default to _
```
1. Evaluate `<expr>` (context `elem_eval_possible`, string or container) for each element of `in`
   (§2.6 order). String: the element goes to group `value` (**including the empty string**).
   Container (`keys()`): the element goes to every group named by a returned value; elements without
   tags are in no group.
2. Before looping, the set `loop` **in the outer scope is replaced by an empty set** (its old content
   is the input if `in == loop`). A new scope frame is pushed.
3. For each group in **byte-wise ascending order of the value** (empty value first): assign the
   group's elements (sorted) to `loop` in the inner frame, set `loop.val = value`, execute the body.
4. After the loop, every set assigned in the inner frame except `loop` is moved to the outer frame
   (values from the last iteration win); the frame is popped. **Hence after `for`, `loop` (usually
   `_`) is empty**, and `loop.val` is unset.
5. Remember §8: once a body statement writes to `loop`, `loop.val` reads `""` for the rest of that
   iteration — read it before (`make x name=_.val` works because evaluation precedes the write).

`foreach` (contrast, `statements/foreach.cc:84-129`): same frame handling and emptied output set,
but iterates single elements in §2.6 order (`loop` = one-element set), no grouping, no `val`.

### 11.6 `local` (`statements/localize.cc`) — summary

`[.in] local [l|ll|llb] [(s,w,n,e)] [->.out];` turns the input into "localized" deriveds (tags of
the source copied, keys starting with `_` get an extra `_` prefixed; tags of every produced element
sorted by `(key,value)` at creation; then special `_…` tags appended):
* `vertex` (Point) per input node that has tags (`llb`: every node): tags + `_local_id` (if several
  nodes share the exact coordinate: 1-based rank among them by `(ll_lower, id)`) + `_node_ref`
  (`llb` only).
* `link` (Linestring) per section of each input way (tagged, or all for `llb`): ways are split at
  nodes that are used more than once (way ends count twice, tagged/input nodes count) or carry a
  `_local_id`; tags + `_local_from`/`_local_to` (endpoint `_local_id`s) + `_local_id` (rank among
  parallel sections between the same endpoints) + `_way_ref` (`llb`). With `ll`/`llb`: node-less
  ways become `link` with Null geometry.
* `trigraph` per relation member (node → Point, way → its link sections, oriented to connect to the
  previous/next member; relation members only with `ll`/`llb`, Null geometry), chained by
  `_previous`/`_next` tags holding derived ids; `_local_ref`, `_vertex_local_from/_to`,
  `_child_node_ref`/`_child_way_ref`/`_child_relation_ref`, `_relation_ref` (`llb`).
* Optional bbox drops elements/sections not touching it. Ids come from the global counter in
  creation order (vertices, links, loose links, trigraphs). C++ keeps a raw pointer to the previous
  trigraph inside a growing vector (undefined behaviour; `_next` may be lost after reallocation) —
  do not try to emulate; this statement is low priority for byte-exact parity.

---------------------------------------------------------------------------------------------------

## 12. Printing derived elements (`out`)

* Default `out` order is ascending **u64 id** (unstable among equal ids); `out qt` uses storage
  order (all `make`/`convert` deriveds at index 0, i.e. their stored order).
* XML (`output_formats/output_xml.cc:555`): `<{type} id="{id}">` (every `out` mode includes the
  id); tags (modes with TAGS: body/tags/meta) `<tag k="…" v="…"/>` in stored order, XML-escaped, empty values
  printed; self-closing `<{type} id="…"/>` when nothing inside. Areas printed via the same path use
  type `area` and no geometry. Derived elements have no metadata (`out meta` prints nothing extra).
* Geometry (`output_xml.cc:191-284`), first matching branch, indent starts at 4 spaces, nested +2:
  1. `GEOMETRY` flag (`out geom`) and components → for each non-null component
     `<group>` … recursive … `</group>` (Null components give an empty group);
  2. `GEOMETRY` and line → `<vertex lat="FIX7" lon="FIX7"/>` per point;
  3. `GEOMETRY` and multiline (polygons, multi-run PartialWay) → per ring `<linestring>` + vertices +
     `</linestring>`;
  4. `GEOMETRY` and centre (Point; also empty Linestring!) → `<point lat lon/>`;
  5. `BOUNDS` flag and bbox → `<bounds minlat minlon maxlat maxlon/>` (always 4-space indent);
  6. `CENTER` flag (`out center`) and centre → `<center lat lon/>`.
  Any branch taken opens the element with `>` even if it prints no children (e.g. an empty polygon
  gives `<t id="1">\n  </t>`). `out geom` = GEOMETRY|BOUNDS, `out bb` = BOUNDS, `out center` = CENTER.
  Geometry is printed before tags.
* JSON (`output_formats/output_json.cc:282-404`): `{"type": "<type>", "id": <id>, "geometry": …,
  "tags": …}`; with GEOMETRY and the top geometry having a centre → GeoJSON-like object:
  components → `GeometryCollection` (only components that have a centre), line → `LineString`,
  multiline → `Polygon` (rings), else `Point`; coordinates `[lon, lat]` with `FIX7`. BOUNDS without
  that → 5-point `Polygon` of the bbox; CENTER → `Point`.

---------------------------------------------------------------------------------------------------

## 13. Quirk checklist / environment notes

1. `G14` everywhere for doubles, `I` for ints, `FIX3` for `length`/`angle`, `FIX7` for `lat`/`lon`.
2. Octal (`010`=8) and hex (`0x10`=16) integers in all int paths (`+ - * ==`, `number`, `sum`,
   `min/max`, `::id`). Binary `0b101`=5 **only if** the reference binary was built against glibc ≥ 2.38
   in C23 mode — probe the reference with `make t v="0b11"+"0";` (`3` vs `0b110`).
3. Integer overflow: parsing saturates, arithmetic wraps.
4. `"NaN"`/`"nan"`/`"inf"` strings are numbers; `"NaD"`, `"NaW"` are not; `"NaN"=="NaN"` is `"0"`.
5. `+` concatenates non-numbers; `- * /` give `NaN`; `/` is always floating point.
6. Truthiness: `"false"` and `"0 "` are true; `" 0"` and `"0.0"` are false.
7. `min`/`max` degrade to string comparison as soon as one value is `""` or non-numeric; `sum`
   becomes `NaN`; `u`/`set` ignore `""`.
8. `count(deriveds)` includes areas; `count_members(type)` on ways is `"0"`; `is_closed` of a 1-node
   way is `"1"`; `length()` of deriveds is `"0"`.
9. `trim` in `lrs_*` keeps trailing spaces when there are leading spaces; empty items become `" "`.
10. `?:` is left-associative.
11. Aggregators are pre-computed; referencing a never-assigned set (including `_` at script start)
    drops the tag entirely.
12. Derived ids: one global counter per request starting at 1; `convert` dispenses in storage order;
    explicit `::id` does not consume a counter value.
13. After `for`/`foreach`, the loop's output set (default `_`) is empty.
14. Keys in generic copy (`::=`) are sorted byte-wise; explicit tags keep source order; empty values
    are emitted.
