# Overpass QL — Grammar and Parsing Behaviour

Status: normative for the Rust port's parser. Reference implementation: Overpass API 0.7.62 (git `a0db4f39`).
All `file:line` citations are relative to `Overpass-API/src/`. Behaviour marked **[verified]** was confirmed by
running the reference `osm3s_query --dump-compact-ql` / `--dump-xml` / execution mode against the quoted input.

Scope:
- QL only. The XML query language is dropped (see §1 for how XML input is detected).
- Attic/history features are **recognized but unsupported**: `[date:]`, `[diff:]`, `[adiff:]` (and their raw
  aliases `[from:]`, `[augmented:]`), `retro`, `timeline`, `compare`, `(changed…)`, `(user_touched:…)`,
  `(uid_touched:…)`. The parser MUST recognize their syntax exactly as described here (so that error messages and
  recovery stay aligned), then reject the query with a dedicated "unsupported" static error (wording is ours).
- Semantics (what a statement computes) are out of scope; §11 maps every construct to the internal statement name
  so that semantics specs can be cross-referenced.

Conventions: `IDENT`, `TEXT`, `NUMTOK` etc. are defined in §2.5. "Expect `x`" means the error-recovering helper of
§3.3. Quoted error messages are exact (including typos, doubled spaces, missing spaces).

---

## 1. Pipeline

1. **Input normalisation** (frontend, `overpass_api/frontend/user_interface.cc:37-105`): empty input →
   `encoding error: Your input is empty.`; whitespace-only → `encoding error: Your input contains only whitespace.`
   The console frontend appends `\n` after every input line (`frontend/cgi-helper.cc:64-73`), so errors detected at
   end of input report the line *after* the last line.
2. **Dialect detection** (`dispatch/scripting_core.cc:150-154`): skip leading whitespace; if the first character
   is `<` the input is XML, otherwise QL. Consequence: a QL program cannot start with the `<`/`<<` recurse statement
   (tests 132/133 prefix `._;`). The port MUST keep this rule and reject such input as "XML not supported".
3. **Lexing** (§2): comment stripping → whitespace compression → tokenization, all streaming.
4. **Parsing** (§4–§9): settings block, then statements until end of input. The parser never stops at the first
   error; it records errors and resynchronises (§3.3). Statement objects are constructed *during* parsing, and
   construction performs attribute validation that emits **static errors**, interleaved with parse errors.
5. **Decision** (`dispatch/scripting_core.cc:230-239`): if any encoding, parse, or static error was recorded, the
   query is rejected and nothing executes. Remarks never reject.

---

## 2. Lexical structure

Three stacked filters in `expat/map_ql_input.cc`. Each tracks quote state independently (`plainfile` /
`single_quot` / `double_quot`); inside quotes a backslash makes the next byte opaque (it can't close the string
or start a comment).

### 2.1 Comments (`Comment_Replacer`, map_ql_input.cc:55-171)
Outside quotes only:
- `//` … up to and including the next `\n` → one space.
- `/*` … first `*/` → one space. Not nested. An unterminated block comment swallows the rest of the input
  silently (then normal EOF errors follow). `/*/` does not close.
- A `/` not followed by `/` or `*` is an ordinary character.
- Comment markers inside `"…"` / `'…'` are literal **[verified]** (`node["a//b"="c/*d*/"]` keeps both).

### 2.2 Whitespace (`Whitespace_Compressor`, :199-273)
Outside quotes, any run of C `isspace` bytes (SP, HT, LF, VT, FF, CR) becomes one space, which the tokenizer
then discards. Inside quotes whitespace (including newlines) is preserved verbatim. Whitespace is never
significant except as a token separator.

### 2.3 Tokens (`Tokenizer::get`, :385-542)
After each token one optional space is skipped. Token classes, by first byte:

| First byte | Token | Rule |
|---|---|---|
| `A-Za-z_` | identifier | `[A-Za-z_][A-Za-z0-9_]*` (ASCII `isalpha`/`isalnum` only) |
| `0-9` | number | `[0-9]+ ( "." [0-9]* )? ( "e" "-"? [0-9]* )?` — lowercase `e` only, no `+`, `E` not accepted; `1.`, `1e`, `1e-` are valid tokens |
| `-` followed by digit or `.` | number | `-` then the same rule (fraction may be empty): `-5`, `-.5`, **`-.`** |
| `-` otherwise | `->` if next is `>`, else `-` | |
| `"` / `'` | string | up to the matching unescaped quote; `\x` skips `x`; token **includes** the quotes. Unterminated → token runs to end of input |
| `:` | `::` or `:` | greedy 2-char probe |
| `=` | `==` or `=` | |
| `!` | `!=` or `!` | `!~` is **two** tokens `!` `~` |
| `\|` | `\|\|` or `\|` | |
| `&` | `&&` or `&` | |
| `<` | `<<`, `<=` or `<` | |
| `>` | `>>`, `>=` or `>` | |
| any other ASCII | 1-char token | `( ) [ ] { } . , ; ~ ? + * / % @ #` … |
| byte ≥ 0x80 | one UTF-8 sequence | 2/3/4 bytes by lead byte; invalid lead → 1 byte |

Tokenization pitfalls (all **[verified]**):
- `( .a; -.b; )` lexes `-.` `b` → `Unknown type "-."`. Write `- .b`.
- `(if:::id…)` lexes `if` `::` `:` … (greedy `::`).
- `[k=Foo-Bar]` → `Foo` `-` `Bar` → `']' expected - '-' found.`
- `.5` is `.` `5`. `12abc` is `12` `abc`. `café` unquoted is `caf` `é`.
- `a-1` lexes `a` `-1`; expression trees re-split it into binary minus (§8.1).

### 2.4 Positions, EOF
- Each token carries the 1-based line of its first byte. Newlines inside comments are counted.
- At end of input the tokenizer's "current token" keeps its last value while `good()` turns false; error messages
  produced at EOF therefore quote the **last real token** (e.g. `node[amenity]` with no `;` gives
  `';' expected - ']' found.` then `Unexpected end of input.`) **[verified]**.

### 2.5 Token acceptors (`frontend/tokenizer_utils.cc`)

| Name | Accepts | On failure (token NOT consumed) |
|---|---|---|
| `TEXT` (`get_text_token`, :22-69) | a string token (decoded, §2.6); or a token starting with `A-Za-z0-9_` (verbatim); or `-` + digit (verbatim) | `<what> expected - '<tok>' found.` |
| `IDENT` (`get_identifier_token`, :72-94) | token starting with `A-Za-z_` | `<what> expected - '<tok>' found.` |
| `NUMTOK` | any token whose first char is a digit, or `-` + digit (used by filter dispatch §6.4) | — |

`<what>` is one of: `Keyword`, `Value`, `Key`, `Variable`, `Number`, `maximum loop number`, `OSM base type`,
`OSM element id`, `Element class name`, `CSV format specification`, `CSV output header line (true or false)`,
`CSV separator character`. Unquoted TEXT values are **not normalised**: `[k=1e5]`, `[k=007]`, `[k=-12.5]` keep
that exact text **[verified]**.

### 2.6 String decoding — two decoders
Both strip the first byte and **the last byte** (assumed closing quote; for an unterminated string this eats the
last real character). Escapes: `\n`→LF, `\t`→TAB, `\uH…` (1–4 hex digits, greedy) → code point as UTF-8 (1–3
bytes; UTF-16 surrogates are encoded individually, **not** paired — `"😀"` yields 6 bytes of CESU-8
**[verified]**), any other `\c` → `c` (so `\"`, `\\`, `\'`, `\q`→`q`). A `\u` value < 0x20 (incl. zero hex digits)
emits parse error `Invalid UTF-8 character (value below 32) in escape sequence.` with **line 0** and appends nothing.

| Decoder | Used for | Difference |
|---|---|---|
| A: `get_text_token` (tokenizer_utils.cc:30-52 + `decode_to_utf8` decode_text.cc:98-146) | every `TEXT` position: tag keys/values, settings values, CSV fields, `out(…)` bbox, `is_in`, `local`, `timeline`, `complete` | reference behaviour |
| B: `decode_json` (decode_text.cc:22-95) | evaluator string literals, `(around:R)`, `(poly:…)`, `(newer:…)`, `(user:…)`, recurse roles, `is_tag(k)`, set-prop keys, `count_by_role` role; unquoted tokens pass through verbatim | bug: a non-hex char inside the 4-char window multiplies the value by 16 once more: `\u41x` → U+0410 (B) vs `A` (A) **[verified]** |

Port: implement decoder A everywhere; decoder B's `\u` bug need not be reproduced.

---

## 3. Errors

### 3.1 Format
`line N: parse error: MSG`, `line N: static error: MSG`, `encoding error: MSG`; remarks analogous
(`line N: static remark: MSG`). No column. Console writes to stderr; the web frontend returns HTTP 400 with the
message embedded (`frontend/console_output.cc:33-45`, `frontend/web_output.cc:42-63`).

Line `N`: for parse errors, the line of the current token at detection (§2.4); for static errors, the statement's
recorded line: the first token of the statement for most statements, but `out` uses the line of its terminating
`;` (map_ql_parser.cc:797), `make`/`convert` likewise the line of their terminating `;` (:862), and `has-kv`
the line of the token following the operator (:1081-1135). A few errors always report line 0 (decoder `\u` error; empty filter `node();` →
`line 0: parse error: Unknown query clause` **[verified]**).

### 3.2 Rejection
Any parse or static error rejects the whole query. Ordering of messages is detection order; the port SHOULD
reproduce message texts for the cases in §10 and MUST accept/reject exactly the same inputs.

### 3.3 The `expect` helper (`clear_until_after`, tokenizer_utils.cc:109-197)
`expect(t1[,t2…], consume=true)`: if the current token is not one of the targets, emit
`'t1' expected - 'X' found.` / `'t1' or 't2' expected - …` / `'t1', 't2', or 't3'  expected - …` (note two
spaces before `expected` for 3+ targets) / `'t1', 't2', 't3', or 't4'  expected - …` /
`'t1', 't2', 't3', 't4', or 't5'  expected - …`, then **skip tokens until a target is found** (or EOF). Then, if
at EOF emit `Unexpected end of input.`, else consume the target iff `consume`. This skip-ahead is the only
recovery mechanism; it can swallow large parts of the query (e.g. `[out:csv]` skips to the next `(`).

---

## 4. Grammar (EBNF)

Terminals in quotes are exact tokens. `SET` = `IDENT` unless noted. Every statement is terminated as stated; a
stray `;` where a statement is expected is **not** an empty statement: `node(1);;` → `Unknown type ";"` +
`An empty query is not allowed` **[verified]**.

```
program      = [ setting { setting } ";" ] { statement } ;          (* map_ql_parser.cc:1361-1406 *)
setting      = "[" IDENT ( ":" | "]" ) TEXT setting-tail "]" ;       (* §5 *)

statement    = union | foreach | for | complete | if | retro | timeline        (* keyword-dispatched first *)
             | [ "." ANYTOK ] prefixed                                         (* :1304-1330 *)
             | query ;
prefixed     = out | make | convert | recurse-op | is-in | map-to-area | local | compare ;
into         = "->" "." SET ;                                                   (* probe_into :46-57 *)

union        = "(" [ statement [ "-" statement ] ] { statement } ")" [ into ] ";" ;   (* §6.6 *)
block        = "{" { statement } "}" | "(" { statement } ")" ;                  (* :71-92 *)
foreach      = "foreach" [ "." SET ] [ into ] block [ ";" ] ;
for          = "for" [ "." SET ] [ into ] "(" expr ")" block [ ";" ] ;
complete     = "complete" [ "(" TEXT ")" ] [ "." SET ] [ into ] block [ ";" ] ;
if           = "if" "(" expr ")" block [ "else" block ] [ ";" ] ;
retro        = "retro" "(" expr ")" block [ ";" ] ;                             (* unsupported *)
timeline     = "timeline" "(" TEXT "," TEXT [ "," TEXT ] ")" [ into ] ";" ;     (* unsupported *)
compare      = "compare" [ "(" [ "delta" ":" expr ] ")" ] [ into ]
               ( block [ ";" ] | ";" ) ;                                        (* unsupported *)

out          = "out" { out-param } ";" ;                                        (* §7 *)
recurse-op   = ( "<" | "<<" | ">" | ">>" ) [ into ] ";" ;
is-in        = "is_in" [ "(" TEXT [ "," TEXT ] ")" ] [ into ] ";" ;
map-to-area  = "map_to_area" [ into ] ";" ;
local        = "local" [ ANYTOK-except(";" "->" "(") ]
               [ "(" TEXT [ "," TEXT [ "," TEXT [ "," TEXT ] ] ] ")" ] [ into ] ";" ;
make         = "make"    [ IDENT ] [ prop { "," prop } ] [ into ] ";" ;        (* §9 *)
convert      = "convert" [ IDENT ] [ prop { "," prop } ] [ into ] ";" ;

query        = TYPE [ "." ANYTOK ] { filter } [ into ] ";"                      (* §6 *)
             | (* after a leading "." SET: *) [ into ] ";" ;                    (* set copy: item *)
TYPE         = "node" | "way" | "rel" | "relation" | "nwr" | "nw" | "wr" | "nr" | "area" | "derived" ;
filter       = "[" tag-filter "]" | "(" paren-filter ")" | "." SET ;
```

Notes:
- `ANYTOK`: the set name after a statement-initial `.` and directly after `TYPE` is taken **raw** (any single
  token, no IDENT check: `node.1` is accepted **[verified]**). Everywhere else (`foreach.`, `for.`,
  `complete.`, `->.`, `.x` among filters) it must be `IDENT` (`Variable expected - …`). Port: require IDENT
  everywhere except keep the lenient form behind a compat note.
- `.` and the following name may be separated by whitespace (`node . a`, `-> . x`) **[verified]**.
- `_` is the default set; `->._` is the same as omitting `into`.
- `into` is at most one: `node(1)->.a->.b;` → `';' expected - '->' found.` **[verified]**.
- In `for`, `if`, `retro` the `(`…`)` is mandatory and parsed as one expression (`parse_value_tree` with
  `parenthesis_expected=true`: the tree stops at the first unmatched closer, `;` or `->`).
- `else` must immediately follow the `if` block; an empty else-block is dropped. A dangling `else` with no block is
  an error.
- Nesting depth: each `(`…`)` union, `foreach` or `for` body adds 2 to the depth counter, each `complete` / `if` /
  `retro` / `compare` body adds 1 (:84, :106, :573, :598). A statement parsed at depth ≥ 1024 →
  `Nesting of statements is limited to 1023 levels` (:1282-1287) and is dropped; e.g. 520 nested unions fail
  **[verified]**, so the practical limit is ~511 nested unions.

---

## 5. Settings block (`parse_setup`, map_ql_parser.cc:388-464; validation `statements/osm_script.cc:60-215`)

- Recognized only at program start: `while token == "["`. Each setting: `[`, keyword (`IDENT`,
  `Keyword expected - …`), expect `:` or `]` (consumed), value (`TEXT`, `Value expected - …`), keyword-specific tail,
  then `]`.
- After ≥1 setting, `;` is **required** (`expect ";"`, :1395-1396): `[out:json]node(1);` →
  `';' expected - 'node' found.` **[verified]**. No settings ⇒ no `;`.
- Duplicate settings: last wins (`[out:json][out:xml]` → xml).
- Settings become attributes of the root `osm-script` statement; unknown keywords therefore produce
  `static error: Unknown attribute "<kw>" in element "osm-script".` **[verified]**.

| Setting | Tail syntax | Attribute | Validation (static errors) / default |
|---|---|---|---|
| `[out:F]` | format-specific config (below) | `output`, `output-config` | `F ∈ {xml, json, csv, custom, popup}`, else **parse** error `Unknown output format: F`. Default `xml`. `F` may be quoted. |
| `[timeout:V]` | all tokens up to `]` appended with single spaces | `timeout` | default `180`. `V` → `strtoll(base 0)` then optional suffix `ki`/`Mi`/`Gi` (×2^10/2^20/2^30) or `k`/`M`/`G` (×10^3/10^6/10^9), optionally after one space; anything else or ≤0 → `For the attribute "timeout" of the element "osm-script" the only allowed values are positive integers optionally with suffix "ki", "Mi", or "Gi".` Quirk: leading `0` = **octal** (`[timeout:010]` = 8 s **[verified]**). |
| `[maxsize:V]` | same as timeout | `element-limit` | default `536870912`; message `… "element-limit" of the element "osm-script" …`. `[maxsize:2Mi]` lexes `2` `Mi` → `"2 Mi"` → OK. |
| `[bbox:s,w,n,e]` | up to three `, TEXT` | `bbox` (joined with `,`) | fewer than 4 → `A bounding box needs four comma-separated values.`; `Latitudes in bounding boxes must be between -90.0 and 90.0.`; `Longitudes in bounding boxes must be between -180.0 and 180.0.` |
| `[date:T]` | — | `date` | **unsupported** (attic). Format `yyyy-mm-ddThh:mm:ssZ`. |
| `[diff:T1(,T2)?]` | optional `, TEXT` | `from`=T1, `date`=T2 | **unsupported** |
| `[adiff:T1(,T2)?]` | optional `, TEXT` | as diff + `augmented=deletions` | **unsupported** |
| `[from:…]`, `[augmented:…]`, `[output:…]` | none | raw attribute | accidental aliases (they are osm-script attribute names); port SHOULD reject as unknown. |

Effects of `[bbox:…]` at parse time (osm-script is constructed before the first statement is parsed, :1392):
an empty query `TYPE;` becomes legal (§6.5) and every *query-wrapper* statement gets an implicit bbox constraint
(`statements/query.cc:86-90`) — note: not the standalone forms of §6.5 (semantic quirk, flag for semantics spec).

### 5.1 `[out:csv(…)]` (`output_formats/output_csv_factory.cc`)
```
csv-config = "(" csv-field { "," csv-field } [ ";" TEXT [ ";" TEXT ] ] ")" ;
csv-field  = [ "::" ] TEXT ;          (* "::" marks a special field: ::id, ::type, ::lat, … *)
```
- Parentheses are mandatory: `[out:csv]` → `'(' expected - ']' found.` and the skip-ahead runs into the query
  **[verified]**.
- Field names are TEXT: `addr:street` must be quoted. `::"lat"` ≡ `::lat`.
- Header flag: the TEXT `false` disables the header; anything else (including `true`, `maybe`, or omitted) enables it.
- Separator: TEXT (decoded: `"\t"`, `","`, `""`, `"---"` all legal). Default TAB.
- Special-field names are not validated by the parser.

`[out:popup(…)]` has its own mini-grammar (`output_popup_factory.cc`); `custom`/`popup` are web-frontend formats —
port may reject them with `Unknown output format`. `xml`/`json` take no config; `[out:json(…)]` →
`']' expected - '(' found.`

---

## 6. Query statements (`parse_statement` :1275-1358, `parse_query` :1031-1272)

### 6.1 Dispatch order in `parse_statement`
1. `(` → union/difference. 2. Keywords `for`, `foreach`, `complete`, `if`, `retro`, `timeline`.
3. Optional `"." ANYTOK` (input set). 4. Keywords `out`, `convert`, `make`, `<` `<<` `>` `>>`, `is_in`,
`map_to_area`, `local`, `compare` (these receive the input set). 5. Otherwise: if no input set was given, the
current token is taken as TYPE (`rel` → `relation`); if not in the TYPE list → parse error `Unknown type "X"` but
parsing continues with that type (later static errors, e.g. `nodes(1);` gives 3 errors **[verified]**). Then an
optional `"." ANYTOK` input set, then `parse_query`.

Consequences: keywords are case-sensitive (`NODE` is an unknown type); `.a foreach{…}` is an error
(`';' expected - 'foreach' found.`) — the input set of `foreach`/`for`/`complete` is written after the keyword;
`.a[x];` (input set without type) → type `""` → static error on `query` **[verified]**; `.a make x;` →
`static error: Unknown attribute "from" in element "make".` **[verified]**.

### 6.2 Filters
`parse_query` loops while the token is `[`, `(` or `.`:
- `[` → tag filter (§6.3) → clause.
- `(` → the contents up to the matching `)` are parsed as an **expression tree** (§8.1, `Token_Tree(…, true)`),
  then `expect ")"`. Dispatch on the tree: §6.4. `(` at EOF → `':' or '.' expected.`
- `.` → `IDENT` → item clause (intersection with that set). Any number, anywhere in the chain.
Then optional `into`, then `expect ";"`.

### 6.3 Tag filters (`[` … `]`, :1042-1151)

| Syntax | Clause | has-kv attributes |
|---|---|---|
| `[K]` | key exists | `k=K` |
| `[K=V]` | equals | `k, v` |
| `[K!=V]` (single token `!=`) | not equal | `k, v, modv=not` |
| `[K~R]` | value regex | `k, regv` |
| `[K!~R]` | value regex negated | `k, regv, modv=not` |
| `[K~R,i]`, `[K!~R,i]` | case-insensitive | + `case=ignore` |
| `[~KR~R]`, `[~KR~R,i]` | key regex + value regex | `regk, regv` (+`case=ignore`) |
| `[!K]` | key absent | `k, regv=".*", modv=not` (dumps as `["K"!~".*"]`) |

`K`, `V`, `R`, `KR` are TEXT. State machine after `[`:
1. Optional `~` (key-regex flag). 2. If `!`: TEXT key, `expect "]"`, emit `[!K]`. (`[!K=V]` →
   `']' expected - '=' found.`, filter still becomes `[!K]` **[verified]**.)
3. TEXT key; `expect("!", "~", "=", "!=", "]", consume=false)`.
4. If `!`: negate, consume, `expect("~", "=", "]", consume=false)`.
5. `]` → exists. `=`/`!=` → negation is **reset** to `token == "!="`; then if next is `]`, consume it and emit
   **nothing**; else TEXT value, emit only if the decoded value is non-empty, `expect "]"`.
   `~` → TEXT regex; `expect(",", "]", consume=false)`; if `,`: consume, `expect "i"`, `expect("]", consume=false)`;
   consume `]`.
6. Key-regex flag with `]`, `=`, `!=` → parse error
   `A regular expression for a key can only be combined with a regular expression as value criterion`; the clause
   is still built as the plain form.

Quirks (all **[verified]**; port SHOULD reproduce since they silently change results):
- `[K=]`, `[K=""]`, `[K!=""]` are **dropped** without error (test 124). If that leaves `TYPE;` it becomes an empty
  query error.
- `[K!]` ≡ `[K]` (negation without operator is ignored at query build time; `query.cc:100-139`).
- `[K ! = V]` ≡ `[K=V]` (step 5 resets negation); `[K!=V]` must use the single token `!=`.
- `[K~""]` and `[K!~""]` ≡ `[K]` (empty regex means "no regex").
- `,i` is only accepted after `~` (`[K=V,i]` → `']' expected - ',' found.`); the flag must be lowercase `i`
  (`'i' expected - 'I' found.` and the skip-ahead runs to the next `i` token).
- `[~KR!~R]` → static error `In the element "has-kv" regular expressions on keys cannot be combined with negation.`
- Invalid regex → static error `Invalid regular expression: "<re>"`. Empty key → static error
  `For the attribute "k" of the element "has-kv" the only allowed values are non-empty strings.`
- Unquoted values: `[k=Foo-Bar]` fails; `[k=.5]` fails (`Value expected - '.' found.`); `[k=-12.5]`, `[1=2]` work.

### 6.4 Parenthesised filters — dispatch (`Factory::create_criterion`, statements/statement.cc:315-340)
The tree is built with the full operator table (§8.2: `,`<`:`<`.`). The criterion is chosen by the **leftmost
leaf** of the tree; if that leaf is a `NUMTOK`, it is `bbox` when the root is `,`, else `id`. Unknown → parse error
`Unknown query clause`. Each criterion then pattern-matches the tree; tokens are taken raw unless "decoded"
(decoder B).

| Surface syntax | Leftmost | Tree shape accepted | Internal stmt | Errors / quirks |
|---|---|---|---|---|
| `(S,W,N,E)` | NUMTOK | `,(,(,(S,W),N),E)` | `bbox-query` s,w,n,e | not exactly 4 → `bbox requires four arguments`. W/N/E are `atof`'d raw tokens: non-numeric silently → 0 (`(50,abc,51,8)` accepted **[verified]**); quoted first value → `Unknown query clause`. Range checks are static: `For the attribute "s" of the element "bbox-query" the only allowed values are floats between -90.0 and 90.0.` (same for n; w/e ±180), `The value of attribute "n" … must always be greater or equal than the value of attribute "s".` |
| `(ID)` | NUMTOK | leaf | `id-query` type, ref | `(1,2)` → `bbox requires four arguments` — lists need `id:` |
| `(id:ID{,ID})` | `id` | `,(…,:(id,ID1)…)` | `id-query` ref, ref_1… | ids via `atoll`; `0`/non-numeric → `For the attribute "ref" of the element "id-query" the only allowed values are positive integers.` (negative ids wrap to huge uint64 and are accepted **[verified]** — port SHOULD reject); TYPE `derived` → `… "type" of the element "id-query" the only allowed values are "node", "way", "relation", or "area".` |
| `(around[.SET]:R)` | `around` | `:(around \| .(around,SET), R)` | `around` from, radius | R decoded (quotes OK). Missing `:` → `around requires the radius as first argument` |
| `(around[.SET]:R,LAT,LON)` | | `, ` chain | `around` lat, lon | |
| `(around[.SET]:R,LAT1,LON1,…,LATn,LONn)` n≥2 | | | `around` polyline | even number of trailing values → `around requires an odd number of arguments`. A set together with coordinates is accepted. Static: radius ≥ 0, lat/lon ranges |
| `(poly:"lat lon lat lon …")` | `poly` | `:(poly,STR)` | `polygon-query` bounds | decoded; whitespace-separated floats, even count (static). **`(poly)` without `:` yields no statement and no error** **[verified]** |
| `(area)`, `(area.SET)`, `(area:ID)`, `(area.SET:ID)` | `area` | | `area-query` from, ref | ref via atoll (non-positive → static error) |
| `(pivot)`, `(pivot.SET)` | `pivot` | | `pivot` from | |
| `(newer:"T")` | `newer` | `:(newer,STR)` | `newer` than | decoded; bad timestamp → static. No `:` → silently nothing. Inside a union/if/foreach/complete body as a standalone statement: `"newer" can appear only inside "query" statements.` |
| `(user:N{,N})`, `(uid:U{,U})` | `user`/`uid` | | `user` name/uid (+`name_i`/`uid_i`) | names decoded, uids raw. No values → `Exactly one of the two attributes "name" and "uid" must be set.` |
| `(user_touched:…)`, `(uid_touched:…)` | | | `user` criterion=touched | **unsupported** (reads attic meta, user.cc:118-140) |
| `(changed)`, `(changed:"T")`, `(changed:"T1","T2")` | `changed` | | `changed` since, until | **unsupported**. `Date required after "changed" with colon`, `Unexpected token "X" after "changed"` |
| `(if:EXPR)` | `if` | `:(if,EXPR)`; if a lower-priority operator (`,` `=` `?`) captured the colon, the `:(if,x)` node is spliced out (filter.cc:111-119) | `filter` + evaluator | context elem_eval_possible, must be string (§8.3) |
| `(w)`,`(r)`,`(bn)`,`(bw)`,`(br)`,`(way_link…)`,`(way_cnt…)` | name | `[:( ] [.(NAME,SET)] [, roles…]` | `recurse` | §6.4.1 |
| `(<)`, `(<<)`, `(>)`, `(>>)` | operator | leaf | `recurse` up/up-rel/down/down-rel | **`(<.a)` silently drops `.a`** (`<` is a tree operator, so `.a` lands in its rhs; recurse.cc:213-246) **[verified]** |
| `()` | `""` | | — | `line 0: parse error: Unknown query clause` |

#### 6.4.1 Recurse filters (recurse.cc:61-210)
`(NAME[.SET][:ARG{,ARG}])`. `NAME` and result TYPE select the recurse type; mismatch → parse error, which also
leads to a static error on `recurse` (2 errors **[verified]** for `way(w)`):

| NAME | TYPE → recurse type | Error if TYPE wrong |
|---|---|---|
| `r` | node→relation-node, way→relation-way, relation→relation-relation, nwr/nw/wr/nr→relation-nwr/-nw/-wr/-nr | `A recursion of type 'r' produces nodes, ways, or relations.` |
| `w` | node→way-node | `A recursion of type 'w' produces nodes.` |
| `way_link`, `way_cnt` | node→way-link / way-count | `… 'way_link' produces nodes.` |
| `bn` | way→node-way, relation→node-relation, wr→node-wr | `A recursion of type 'bn' produces ways or relations.` |
| `bw` | relation→way-relation | `… 'bw' produces relations.` |
| `br` | relation→relation-backwards | `… 'br' produces relations.` |

ARG interpretation (in this priority order):
- `NAME == r` or TYPE == relation: role filter = **first** ARG only (decoded; extra roles silently ignored:
  `(r:"a","b")` ≡ `(r:"a")` **[verified]**). Unquoted roles allowed.
- else `NAME == w` or TYPE == way: ARGs are vertex positions (`pos`, e.g. `(w:1,-1)`). `way(bn:"x")` lands here →
  static `Invalid pos 'x' in positions list.` **[verified]**.
- else `way_cnt`/`way_link`: `:N` (lower=upper=N) or `:L-U` / `:L-` (tree `-(L,U)`).
- else → `A recursion of type '<NAME>' cannot have restrictions.`
- A compound role (`(r:"a"+"b")`) → `Simple role token expected, structured syntax tree found.`

### 6.5 Statement synthesis (:1173-1268)
Let C = tag/item clauses, P = paren filters, F = input set after TYPE.

| Case | Result |
|---|---|
| C, P empty, F empty | global bbox set → `query(type)` with no conditions; else parse error `An empty query is not allowed` (no statement) |
| C, P empty, F set, no TYPE (`.a;`, `.a->.b;`) | `item(from=F, into)` (set copy) |
| C, P empty, F set, TYPE | `query(type)` + `item(F)` |
| exactly one tag clause, F/P empty | `query(type)` + `has-kv` |
| exactly one paren filter, C/F empty | the criterion **itself** carries `into` (no query wrapper) iff `can_standalone(type)`: bbox, around, pivot, poly → node only; id → node/way/relation; recurse names → any TYPE except nwr; `<`/`>`, area, if, newer, user, changed → never. Otherwise `query(type)` + criterion. If the criterion failed to build → **no statement** |
| otherwise | `query(type, into)` + `item(F)` + clauses in source order + paren filters in source order (paren filters always after all `[…]`/`.x` clauses) |

The standalone/wrapped distinction is invisible except for the global-bbox quirk in §5.

### 6.6 Union and difference (`parse_union` :467-490, `collect_substatements_and_probe` :94-139)
`( s1 s2 … )[->.x];` → `union` (empty `();` legal). `( s1 - s2 )` → `difference`: after the first statement, a
`-` token marks a difference; then exactly one more statement must follow before `)`, else parse error
`difference always requires two operands` + `expect(")", consume=false)` (which may resync inside a later
statement **[verified]**). Statements without a result set (e.g. `out`) inside a union →
static `Element "print" cannot be subelement of element "union".` **[verified]**.

---

## 7. Output statement `out` (`parse_output` :724-809; statements/print.cc)

`[.SET] out {param} ;` — params in **any order**, each a single token; later params in the same group override
earlier ones (`out ids skel` → skel; `out 5 7` → limit 7 **[verified]**). No `into` allowed (`->` →
`Invalid parameter for print: "->"`, also for `.` and the name).

| Group | Tokens → value | Default |
|---|---|---|
| mode | `ids`→ids_only, `skel`→skeleton, `body`, `tags`, `meta`, `count`, `quirks` | body |
| order | `asc`→id, `qt`→quadtile | id |
| geometry | `geom`→full, `bb`→bounds, `center` | skeleton |
| ids | `noids`→no | yes |
| limit | any token whose first char is a digit; `atoll` (`out 1e3` → 1) | none |
| bbox | `(` TEXT `,` TEXT `,` TEXT `,` TEXT `)` = (s, w, n, e) — may stand alone; normally written `geom(s,w,n,e)` | none |

- `quirks` passes the parser but `print` rejects it: static `For the attribute "mode" of the element "print" the
  only allowed values are "ids_only", "skeleton", "body", "tags",  "count", or "meta".` (two spaces before `"count"`).
- Any other token → `Invalid parameter for print: "X"` (parse error, token skipped).
- bbox static checks as for bbox-query but with element name `print`; a partial bbox (e.g. `(50,7)`) cascades
  `',' expected …` errors.

---

## 8. Expressions (evaluators)

### 8.1 Tree construction (`Token_Tree`, tokenizer_utils.cc:239-323)
Operator-precedence parse over the token stream, stopping at `;`, `->`, end of input, or (when called for `(…)`
contents) at the first unmatched closer, which is not consumed.
- Brackets `(` `[` `{` open a node whose **lhs is the operand immediately before it** (so `f(x)` = `((f, x)`,
  `t["k"]` = `[(t, "k")`, plain grouping `(x)` = `((∅, x)`). Any closer closes the nearest opener of **any** kind
  (`(a]` is accepted). Unmatched closer → `Unmatched X` (only in make/convert lists); unclosed opener at the end →
  `Left X not closed.`
- Binary operator with priority p: pop the stack while the top's priority ≥ p (⇒ **all operators are
  left-associative**, including `?:`), new node lhs = current rightmost operand.
- An operator with no operand before it becomes **prefix** (lhs empty): `!x`, `-x`, also `::x`, `+x` (the latter
  rejected later).
- Operand after an operand → `Operator expected, but "X"found.` (no space before `found`).
- A number token starting with `-` (e.g. `-1`) that follows a complete operand is split into binary `-` and `1`
  (`3-5`, `a -5` subtract; `f(-5)`, `a==-5` keep the literal) **[verified]**.

### 8.2 Precedence (lowest → highest)

| Prio | Tokens | Arity / role |
|---|---|---|
| 1 | `,` | argument / item separator |
| 2 | `=` | make/convert assignment only |
| 3 | `?` | ternary condition; rhs must be a `:` node |
| 4 | `:` | ternary branches; criterion `name:value` |
| 5 | `\|\|` | binary |
| 6 | `&&` | binary |
| 7 | `==` `!=` | binary |
| 8 | `<` `<=` `>` `>=` | binary |
| 10 | `+` `-` | binary; prefix `-` also gets 10 while building the tree |
| 11 | `*` `/` `%` | binary (`%` has **no** evaluator: `"%" cannot be used as binary operator`) |
| 12 | `.` (`->`) | set member / set-prefixed function |
| 13 | `!` | prefix |
| 15 | `::` | pseudo-key |

Consequences **[verified]**: `1?2:3?4:5` = `(1?2:3)?4:5`; `2<3<4` = `(2<3)<4`; `!a==b` = `(!a)==b`;
`-a*b` = `-(a*b)`; `-a+b` = `(-a)+b`; `a*-b` fails (`"*" needs a right hand side argument`) but `a*-5` works
(literal); `- -1` = negate(-1).

### 8.3 Contexts and type checks
Evaluator creation (`Factory::create_evaluator`, statement.cc:219-312) depends on the *context*:

| Context | Where |
|---|---|
| generic | top level of each `make` item (only set-prop forms valid) |
| in_convert | top level of each `convert` item |
| evaluator_expected | `if (…)`, `retro (…)`, RHS of a `make` property |
| elem_eval_possible | `(if:…)`, `for (…)`, `compare(delta:…)`, RHS of a `convert` property, **arguments of aggregators** (always, regardless of outer context) |
| member_eval_possible | argument of `per_member(…)` / `per_vertex(…)` |

"element" functions require elem_eval_possible or member_eval_possible; "member" functions require
member_eval_possible. Operators and `S.key` silently fail in generic/in_convert (so `make x 1+2;` errors).
Required result type: `if`/`retro`/`(if:)`/`compare delta`/operator operands/most function args → string;
`for` → string or container; `::geom=` RHS → geometry. Mismatch → static
`Evaluator for <exp> expected, but function "<stmt-name>" is an evaluator for <actual>.`

### 8.4 Leaves
- **Literal** (`eval-fixed`): a quoted string (decoder B) or a token that parses fully as int64 or double
  (`12`, `-3.5`, `1.5e3`, `007`). Any other bare leaf (`abc`, `id`) →
  `Put quotation marks around "abc" if it should be a constant and not a tag evaluation.` followed by
  `Token "abc" has not been recognized as statement`.
- `::` alone → `eval-generic` (value of the "current key" in `S::=` / `::=` assignments; "" otherwise).
- **`::id`, `::type`, `::version` … are NOT valid expressions** in this version (`"::" cannot be used as unary
  operator` + 2 more errors **[verified]**). Use `id()`, `type()`, `version()`. `::id`/`::geom` exist only on the
  LHS of make/convert properties (§9).
- `S.key` (`eval-set-key`, runtime_value.h): `.` whose rhs is not a function call: value of key `key` stored on
  set `S`; only `for` writes such a key (`val`), so the idiom is `_.val`. Not valid in generic/in_convert.

### 8.5 Functions and operators
Call forms: `f()`, `f(arg)`, `f(a, b, …)` (left-nested `,` tree), `S.f(arg)` (set prefix is a **bare IDENT,
no leading dot**). `.a.u(x)` is accepted but uses a set literally named `.` (dumps `..u(x)`) **[verified]** — port
SHOULD reject it. `_.u(x)` ≡ `u(x)`. Generic call-shape errors: `Function "f" not known`,
`f(...) cannot have an input set`, `f(...) does not accept any arguments`, `f(...) must have one or more arguments`,
`f(...) must be called in a context where it can evaluate an element` (`… an element's member`),
`Empty parentheses cannot be evaluated.` (static, `()`), `"X" cannot be used as binary operator` /
`"X" needs a right hand side argument` / `"X" cannot be used as unary operator` (then parses the operand anyway).
Multi-argument calls to one-argument functions fail via `"," cannot be used as binary operator`.

| Form | Internal name | Args → result | Ctx | Notes |
|---|---|---|---|---|
| `t[k]` | eval-value | string → string | elem | lhs of `[` must be leaf `t` (`Tag evaulation needs function name "t" before the left bracket` sic), key any string expr (`t[a]` fails: bare `a`) |
| `is_tag(k)` | eval-is-tag | leaf → string | elem | `is_tag(key) needs a simple string as argument`; bare `is_tag(a)` OK |
| `id()` `type()` `version()` `timestamp()` `changeset()` `uid()` `user()` | eval-id … eval-user | none → string | elem | no args, no set |
| `keys()` | eval-all-keys | none → container | elem | |
| `count_tags()` | eval-prop-count | none → string | elem | |
| `count_members([w])`, `count_distinct_members([w])` | eval-prop-count | w ∈ nodes/ways/relations/rels | elem | |
| `count_by_role(r[,w])`, `count_distinct_by_role(r[,w])` | eval-prop-count | literal role (+w) | elem | `… must have the name of the wanted role as argument` |
| `is_closed()` `length()` `lat()` `lon()` | eval-is-closed, eval-length, eval-lat, eval-lon | → string | elem | |
| `geom()` | eval-geometry | → geometry | elem | |
| `per_member(e)`, `per_vertex(e)` | eval-per-member/-vertex | string → string | elem | e in member ctx |
| `pos()` `mtype()` `ref()` `role()` `angle()` | eval-pos, eval-membertype, eval-ref, eval-role, eval-angle | → string | member | |
| `[S.]u(e)` `set(e)` `min(e)` `max(e)` `sum(e)` | eval-union, eval-set, eval-min, eval-umax (sic), eval-sum | string → string | any | arg in elem ctx; `u(...) needs an argument` |
| `[S.]gcat(e)` | eval-geom-concat | geometry → geometry | any | |
| `[S.]count(w)` | eval-set-count | w ∈ nodes ways relations deriveds nwr nw wr nr (raw token; **not** `rels`) | any | `"count" accepts only one of "nodes", "ways", "relations", "deriveds", "nwr", "nw", "wr", or "nr" as type argument.` |
| `number(e)` `is_number(e)` `suffix(e)` `abs(e)` `date(e)` `is_date(e)` | eval-number, eval-is-number, eval-suffix, eval-abs, eval-date, eval-is-date | string → string | any | exactly 1 |
| `lrs_min(e)` `lrs_max(e)` | eval-lrs-min/-max | string → string | any | exactly 1 |
| `lrs_in(a,b)` `lrs_isect(a,b)` `lrs_union(a,b)` | eval-lrs-in/-isect/-union | 2 strings → string | any | exactly 2; `<f>(...) needs two arguments` |
| `pt(lat,lon)` | eval-point | 2 strings → geometry | any | exactly 2 |
| `lstr(g,…)`, `poly(g,…)` | eval-linestring, eval-polygon | ≥1 geometry → geometry | any | (`poly` here is the function, distinct from the `(poly:…)` filter) |
| `center(g)` `trace(g)` `hull(g)` | eval-center/-trace/-hull | geometry → geometry | any | exactly 1 |
| `!a` | eval-not | string → string | non-generic | prio 13 |
| `-a` | eval-negate | string → string | non-generic | |
| `a\|\|b` `a&&b` | eval-or, eval-and | | | |
| `==` `!=` `<` `<=` `>` `>=` | eval-equal, eval-not-equal, eval-less, eval-less-equal, eval-greater, eval-greater-equal | | | |
| `+` `-` `*` `/` | eval-plus, eval-minus, eval-times, eval-divided-by | | | |
| `c?a:b` | eval-ternary | c string; a,b same type (any) → that type | | `Operator "?" must have a ":" operator as right-hand-side argument` |

Operator operand errors: `Operator "<op>" needs a left-hand-side argument` / `… right-hand-side argument`
(operand present but invalid).

---

## 9. `make` / `convert` properties (`parse_make` :822-872; statements/set_prop.cc)

`make TYPE items [->.x];`, `[.S] convert TYPE items [->.x];`. `TYPE` is an IDENT (element class name; required:
`make;` → static `The attribute type must be set to a nonempty string.`). The rest up to `;`/`->` is **one**
expression tree, split on top-level `,` into items (source order kept). Each item is evaluated in generic (make) /
in_convert (convert) context, where only set-prop forms are valid:

| Item | keytype | Allowed | RHS ctx / type |
|---|---|---|---|
| `KEY = e` (KEY a single leaf: IDENT, number or string; decoded) | tag | both | make: evaluator_expected; convert: elem_eval_possible; string |
| `!KEY` | tag, no value (delete/omit) | both | — |
| `::id = e` | id | both | string |
| `::geom = e` | geometry | both | geometry |
| `S:: = e` | generic, from=S | both | `::` inside e = current key's value |
| `:: = e` | generic | **convert only** (`The generic tag set property can only be called from convert`) | |

Errors: compound key (`k:v=1`, `"a"="b"="c"`) → `if special character "<op>" is present in property name then the
property name must be in quotes` (+ `"=" cannot be used as binary operator`, test 134); `::x=…` other than id/geom →
`The only allowed special properties are "id" and "geometry"` (the keyword is actually `geom`);
`To set a property it must have a name to the left of the equal sign`;
`To set a property it must have a value to the right of the equal sign`; RHS failed →
`Property assignment needs a right-hand-side value`. A bare expression (`make x 1+2`) → `"+" cannot be used …`.
Statement-level static errors: `A key cannot be added twice to an element: "k"` (counts `!k` too); at most one
`::id`, one `::geom`, one generic item per statement (`A make statement can have at most one …`).
`make` with an input set → `Unknown attribute "from" in element "make".`

---

## 10. Parse-error catalogue (exact texts; static ones listed in context above)

| Message | Origin |
|---|---|
| `<what> expected - '<tok>' found.` | TEXT/IDENT acceptors §2.5 |
| `'x' expected - 'y' found.` (+ 2–5 target variants) / `Unexpected end of input.` | expect §3.3 |
| `Unknown type "X"` · `An empty query is not allowed` · `Nesting of statements is limited to 1023 levels` | statements §6 |
| `Unknown output format: X` | settings |
| `difference always requires two operands` | union |
| `Invalid parameter for print: "X"` | out |
| `A regular expression for a key can only be combined with a regular expression as value criterion` | tag filter |
| `':' or '.' expected.` | `(` at EOF in a query |
| `Unknown query clause` · `bbox requires four arguments` · `around requires an odd number of arguments` · `around requires the radius as first argument` · recurse messages §6.4.1 · `Date required after "changed" with colon` · `Unexpected token "X" after "changed"` | filters |
| `Operator expected, but "X"found.` · `Unmatched X` · `Left X not closed.` | expression tree |
| `Invalid UTF-8 character (value below 32) in escape sequence.` (line 0) | decoders |
| evaluator messages | §8.4–§9 |

---

## 11. QL construct → internal statement (for cross-referencing semantics specs)

| QL | Statement (XML name) | Attributes / children |
|---|---|---|
| settings | `osm-script` | timeout, element-limit, output, output-config, bbox, date, from, augmented |
| `TYPE…;` (wrapped) | `query` | type, into; children has-kv / item / criteria |
| `[…]` | `has-kv` | k \| regk, v \| regv, modv (""/not), case (sensitive/ignore) |
| `.S;`, `.S->.x;`, `node.S` | `item` | from, into |
| `(s,w,n,e)` | `bbox-query` | s, w, n, e, into |
| `(ID)`, `(id:…)` | `id-query` | type, ref, ref_1…, into |
| `(around…)` | `around` | from, radius, lat, lon, polyline, into |
| `(poly:…)` | `polygon-query` | bounds, into |
| `(area…)` | `area-query` | from, ref, into |
| `(pivot…)` | `pivot` | from, into |
| `(newer:…)` | `newer` | than |
| `(user…)`, `(uid…)` | `user` | type, name/uid (+_i), criterion |
| `(changed…)` | `changed` | since, until |
| `(if:…)` | `filter` | one evaluator |
| `(w)`…, `(<)`…, `<`/`<<`/`>`/`>>` statements | `recurse` | type, from, into, role, role-restricted, pos, lower, upper |
| `(…)` / `(a;-b;)` | `union` / `difference` | into |
| `foreach` · `for` · `complete` | `foreach` · `for` (+evaluator) · `complete` | from, into (+maxnum, default 4096, range 1..1048576) |
| `if … else …` | `if` + marker `else` | evaluator first |
| `out` | `print` | from, mode, order, limit, geometry, ids, s, n, w, e |
| `is_in` | `coord-query` | from, into, lat, lon (lat-only → static lon error) |
| `map_to_area` | `map-to-area` | from, into |
| `local [l\|ll\|llb]` | `localize` | type (default l; else static `Localize must have one of the values "l", "ll", or "llb". "l" is the default value.`), from, into, s, n, w, e |
| `make` / `convert` | `make` / `convert` + `set-prop` children | type, into (+from for convert); set-prop: keytype, k, from |
| `retro` · `timeline` · `compare` | `retro` · `timeline` · `compare` | unsupported |

Canonical dump notes (for golden tests against `--dump-compact-ql`): `rel`→`relation`; keys/values always
double-quoted with only `"`/`\` escaped; numbers reformatted (`50.0`→`50`); defaults omitted (`->._`, `body`,
`asc`); settings printed in alphabetical attribute order (`bbox`, `maxsize`, `out`, `timeout`, …); `foreach(…);`
→ `foreach{…}`; paren filters after tag filters. Known dump bugs (do **not** use as parse oracle): `t[""k""]`,
`count(node)`, `::id=`→`::=`, `version("")`, lost parentheses for equal-priority right operands, missing commas in
`lrs_*`/`lstr`/`poly`.

---

## 12. Compatibility decisions for the port

MUST reproduce (silently change results or acceptance): §2.3 tokenization rules incl. `-.`/`:::` pitfalls; XML
detection by leading `<`; mandatory `;` after settings and after every non-block statement; stray `;` is an error;
dropped empty-value filters; `[K!]`≡`[K]`; `[K ! = V]`≡`[K=V]`; `[K~""]`≡`[K]`; left-associative ternary;
first-role-only for `(r:…)`; `(<.S)` ignores `S`; octal `[timeout:010]`; header flag "anything but `false`";
filter dispatch by leftmost token (quoted first bbox value fails).

SHOULD fix but flag (treat as errors in the port, document divergence): `(poly)`/`(newer)` without `:` silently
yielding nothing; non-numeric bbox/around coordinates `atof`→0; `.a.u(x)` set named `.`; `node.1` raw set names;
decoder-B `\u` bug; unpaired surrogates; `[from:]`/`[augmented:]`/`[output:]` aliases; `custom`/`popup` formats.

Unsupported (parse, then reject with a static error): `[date:]`, `[diff:]`, `[adiff:]`, `retro`, `timeline`,
`compare`, `(changed…)`, `(user_touched:…)`, `(uid_touched:…)`.
