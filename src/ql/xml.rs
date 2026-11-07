//! The Overpass XML query language, translated to Overpass QL.
//!
//! Both languages build the same statements in C++, so an `<osm-script>` is
//! rewritten element by element into QL text and parsed by the QL parser.
//! Each statement is placed on the line C++ reports for it, so runtime and
//! static errors name the same line. Covered: `query` with `has-kv`,
//! `bbox-query`, `around`, `polygon-query`, `item`, `recurse`, `id-query`,
//! `area-query`, `newer`, `user`, `pivot`; the standalone forms of those;
//! `union`, `difference`, `print`, `foreach`, `coord-query`,
//! `map-to-area`, and the `osm-script` settings. Evaluators (`make`,
//! `convert`, `eval-*`, `if`, `for`, `complete`) are rejected.

use anyhow::{Result, bail};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

struct El {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<El>,
    /// Line as C++ counts it (see `line_offset`).
    line: usize,
}

impl El {
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }
    /// A set attribute, `_` when empty or absent.
    fn set(&self, k: &str) -> &str {
        self.attr(k).filter(|s| !s.is_empty()).unwrap_or("_")
    }
    fn into(&self) -> String {
        match self.set("into") {
            "_" => String::new(),
            s => format!("->.{s}"),
        }
    }
    fn err<T>(&self, msg: impl std::fmt::Display) -> Result<T> {
        bail!("line {}: static error: {msg}", self.line)
    }
    fn allowed<T>(&self, attr: &str, values: &str) -> Result<T> {
        self.err(format!("For the attribute \"{attr}\" of the element \"{}\" the only allowed values are {values}.", self.name))
    }
}

/// A QL string literal.
fn lit(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The number C's `strtod` reads at the start of `v` (0 if none), as QL text.
fn fnum(v: &str) -> String {
    let t = v.trim_start();
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == ds || (i == ds + 1 && b[ds] == b'.') {
        return "0".into();
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    t[..i].trim_start_matches('+').to_string()
}

/// The integer C's `atoll` reads at the start of `v` (0 if none).
fn inum(v: &str) -> String {
    let t = v.trim_start();
    let neg = t.starts_with('-');
    let digits: String = t.trim_start_matches(['+', '-']).chars().take_while(|c| c.is_ascii_digit()).collect();
    match (digits.is_empty(), neg) {
        (true, _) => "0".into(),
        (false, true) => format!("-{digits}"),
        (false, false) => digits,
    }
}

/// Lines C++ adds before the query: an XML declaration unless the query has
/// one, and an `<osm-script>` start tag unless the query has that root.
fn line_offset(src: &str) -> usize {
    let t = src.trim_start();
    let decl = t.starts_with("<?xml");
    let body = if decl { t.find("?>").map(|i| t[i + 2..].trim_start()).unwrap_or("") } else { t };
    usize::from(!decl) + usize::from(!body.starts_with("<osm-script"))
}

/// Parse into elements, with expat's messages for the common errors. An
/// unfinished document is closed at its end, as C++ runs what it read.
fn parse_tree(src: &str) -> Result<Vec<El>> {
    let src = src.trim_start();
    let offset = line_offset(src);
    let decl = src.starts_with("<?xml");
    let has_root = usize::from(!decl) == offset;
    // The reader stands before the whitespace that precedes an element.
    let line_at = |pos: usize| {
        let b = src.as_bytes();
        let mut p = pos.min(b.len());
        while p < b.len() && b[p].is_ascii_whitespace() {
            p += 1;
        }
        src[..p].bytes().filter(|c| *c == b'\n').count() + 1 + offset
    };
    let mut reader = Reader::from_str(src);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<El> = vec![El { name: String::new(), attrs: vec![], children: vec![], line: 1 }];
    let mut root_closed = false;
    let start = |b: &BytesStart, line: usize| -> Result<El> {
        let mut attrs = Vec::new();
        for a in b.attributes() {
            let a = a.map_err(|_| anyhow::anyhow!("line {line}: parse error: not well-formed (invalid token)"))?;
            let v = a.unescape_value().map_err(|_| anyhow::anyhow!("line {line}: parse error: undefined entity"))?;
            attrs.push((String::from_utf8_lossy(a.key.as_ref()).into_owned(), v.into_owned()));
        }
        Ok(El { name: String::from_utf8_lossy(b.name().as_ref()).into_owned(), attrs, children: vec![], line })
    };
    loop {
        let pos = reader.buffer_position() as usize;
        let ev = reader.read_event();
        let line = line_at(pos);
        if root_closed && has_root && matches!(ev, Ok(Event::Start(_) | Event::Empty(_))) {
            bail!("line {line}: parse error: junk after document element");
        }
        match ev {
            Ok(Event::Start(b)) => stack.push(start(&b, line)?),
            Ok(Event::Empty(b)) => {
                let el = start(&b, line)?;
                stack.last_mut().unwrap().children.push(el);
                root_closed |= stack.len() == 1;
            }
            Ok(Event::End(_)) => {
                let el = stack.pop().unwrap();
                stack.last_mut().unwrap().children.push(el);
                root_closed |= stack.len() == 1;
            }
            Ok(Event::Eof) => break,
            Ok(Event::Text(t)) => {
                if t.unescape().is_err() {
                    bail!("line {line}: parse error: undefined entity");
                }
                if !t.iter().all(|b| b.is_ascii_whitespace()) {
                    bail!("line {line}: parse error: not well-formed (invalid token)");
                }
            }
            Ok(_) => {}
            Err(quick_xml::Error::IllFormed(_)) => bail!("line {line}: parse error: mismatched tag"),
            Err(quick_xml::Error::Escape(_)) => bail!("line {line}: parse error: undefined entity"),
            Err(_) => bail!("line {line}: parse error: not well-formed (invalid token)"),
        }
    }
    while stack.len() > 1 {
        let el = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(el);
    }
    Ok(stack.pop().unwrap().children)
}

/// Set names are copied into the QL text: only QL identifiers pass.
fn check_sets(e: &El) -> Result<()> {
    if e.name != "osm-script" {
        for (k, v) in &e.attrs {
            let is_set = matches!(k.as_str(), "into" | "set" | "from");
            let ok = v.is_empty() || (v.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
            if is_set && !ok {
                return e.err(format!("For the attribute \"{k}\" of the element \"{}\" the only allowed values are set names.", e.name));
            }
        }
    }
    e.children.iter().try_for_each(check_sets)
}

/// Translate an XML query into Overpass QL.
pub fn to_ql(src: &str) -> Result<String> {
    let top = parse_tree(src)?;
    top.iter().try_for_each(check_sets)?;
    let wrapped;
    let script = match top.as_slice() {
        [e] if e.name == "osm-script" => e,
        _ => {
            wrapped = El { name: "osm-script".into(), attrs: vec![], children: top, line: 1 };
            &wrapped
        }
    };
    let mut ql = settings(script)?;
    for c in &script.children {
        stmt(c, &mut ql)?;
    }
    Ok(ql)
}

fn settings(s: &El) -> Result<String> {
    let mut out = String::new();
    for (k, v) in &s.attrs {
        match k.as_str() {
            "output" => match v.as_str() {
                "xml" | "json" => out.push_str(&format!("[out:{v}]")),
                "csv" => out.push_str(&format!("[out:csv({})]", s.attr("output-config").unwrap_or("::id"))),
                other => return s.err(format!("Unknown output format: {other}")),
            },
            "output-config" | "name" | "augmented" => {}
            "timeout" => out.push_str(&format!("[timeout:{}]", inum(v))),
            "element-limit" => out.push_str(&format!("[maxsize:{}]", inum(v))),
            "bbox" => {
                let p: Vec<String> = v.split(',').map(fnum).collect();
                if p.len() == 4 {
                    out.push_str(&format!("[bbox:{}]", p.join(",")));
                }
            }
            "date" if s.attr("from").is_none() => out.push_str(&format!("[date:{}]", lit(v))),
            "date" => {}
            "from" => {
                let kind = if s.attr("augmented").is_some() { "adiff" } else { "diff" };
                match s.attr("date") {
                    Some(d) => out.push_str(&format!("[{kind}:{},{}]", lit(v), lit(d))),
                    None => out.push_str(&format!("[{kind}:{}]", lit(v))),
                }
            }
            other => return s.err(format!("Unknown attribute \"{other}\" in element \"osm-script\".")),
        }
    }
    if !out.is_empty() {
        out.push(';');
    }
    Ok(out)
}

/// Pad with newlines so that the next statement starts on `line`.
fn to_line(ql: &mut String, line: usize) {
    let cur = ql.bytes().filter(|b| *b == b'\n').count() + 1;
    for _ in cur..line {
        ql.push('\n');
    }
}

const QUERY_TYPES: &str = "\"node\", \"way\", \"relation\", \"nwr\", \"nw\", \"wr\", \"nr\", or \"area\"";
const RECURSE_TYPES: &str = "\"relation-relation\", \"relation-backwards\",\"relation-way\", \"relation-node\", \"way-node\", \"way-relation\",\"node-relation\", \"node-way\", \"down\", \"down-rel\", \"up\", or \"up-rel\"";

/// One statement.
fn stmt(e: &El, ql: &mut String) -> Result<()> {
    to_line(ql, e.line);
    match e.name.as_str() {
        "query" => {
            let ty = match e.attr("type").unwrap_or("") {
                "node" => "node",
                "way" => "way",
                "relation" => "rel",
                t @ ("nwr" | "nw" | "wr" | "nr" | "area") => t,
                _ => return e.allowed("type", QUERY_TYPES),
            };
            let mut items = String::new();
            let mut filters = String::new();
            for c in &e.children {
                if c.name == "item" {
                    items.push_str(&format!(".{}", c.set("set")));
                } else {
                    filters.push_str(&criterion(c)?);
                }
            }
            ql.push_str(&format!("{ty}{items}{filters}{};", e.into()));
        }
        "union" => {
            ql.push('(');
            for c in &e.children {
                stmt(c, ql)?;
            }
            ql.push_str(&format!("){};", e.into()));
        }
        "difference" => {
            if e.children.len() != 2 {
                return e.err("A difference statement needs exactly two substatements.");
            }
            ql.push('(');
            stmt(&e.children[0], ql)?;
            ql.push_str(" - ");
            stmt(&e.children[1], ql)?;
            ql.push_str(&format!("){};", e.into()));
        }
        "item" => {
            // `set` is input and output at once; otherwise `from` and `into`.
            let (from, into) = match e.attr("set").filter(|s| !s.is_empty()) {
                Some(s) => {
                    if e.attr("from").is_some_and(|v| !v.is_empty()) || e.attr("into").is_some_and(|v| !v.is_empty()) {
                        return e.err("The attribute \"set\" of the element \"item\" can only be set if the attributes \"from\" and \"into\" are both empty.");
                    }
                    (s, s)
                }
                None => (e.set("from"), e.set("into")),
            };
            ql.push_str(&format!(".{from}->.{into};"));
        }
        "print" => {
            let mode = match e.attr("mode").unwrap_or("body") {
                "ids_only" => "ids",
                "skeleton" => "skel",
                m @ ("body" | "tags" | "meta" | "count") => m,
                _ => return e.allowed("mode", "\"ids_only\", \"skeleton\", \"body\", \"tags\",  \"count\", or \"meta\""),
            };
            let order = match e.attr("order").unwrap_or("id") {
                "id" => "",
                "quadtile" => " qt",
                _ => return e.allowed("order", "\"id\" or \"quadtile\""),
            };
            let limit = match e.attr("limit").filter(|l| !l.is_empty()) {
                Some(l) => format!(" {}", inum(l)),
                None => String::new(),
            };
            let geom = match e.attr("geometry").unwrap_or("skeleton") {
                "skeleton" => String::new(),
                "full" => match (e.attr("s"), e.attr("w"), e.attr("n"), e.attr("e")) {
                    (Some(s), Some(w), Some(n), Some(x)) => format!(" geom({},{},{},{})", fnum(s), fnum(w), fnum(n), fnum(x)),
                    _ => " geom".into(),
                },
                "bounds" => " bb".into(),
                "center" => " center".into(),
                _ => return e.allowed("geometry", "\"skeleton\", \"full\", \"bounds\", or \"center\""),
            };
            ql.push_str(&format!(".{} out {mode}{geom}{order}{limit};", e.set("from")));
        }
        "foreach" => {
            let into = e.attr("into").filter(|i| !i.is_empty()).map(|i| format!("->.{i}")).unwrap_or_default();
            ql.push_str(&format!("foreach.{}{into}{{", e.set("from")));
            for c in &e.children {
                stmt(c, ql)?;
            }
            ql.push('}');
        }
        "coord-query" => match (e.attr("lat"), e.attr("lon")) {
            (Some(lat), Some(lon)) => ql.push_str(&format!("is_in({},{}){};", fnum(lat), fnum(lon), e.into())),
            _ => ql.push_str(&format!(".{} is_in{};", e.set("from"), e.into())),
        },
        "map-to-area" => ql.push_str(&format!(".{} map_to_area{};", e.set("from"), e.into())),
        "recurse" => match e.attr("type").unwrap_or("") {
            "down" => ql.push_str(&format!(".{} >{};", e.set("from"), e.into())),
            "down-rel" => ql.push_str(&format!(".{} >>{};", e.set("from"), e.into())),
            "up" => ql.push_str(&format!(".{} <{};", e.set("from"), e.into())),
            "up-rel" => ql.push_str(&format!(".{} <<{};", e.set("from"), e.into())),
            t => {
                let (ty, _) = recurse_kind(e, t)?;
                ql.push_str(&format!("{ty}{}{};", criterion(e)?, e.into()));
            }
        },
        "id-query" => {
            let ty = match e.attr("type").unwrap_or("") {
                "node" => "node",
                "way" => "way",
                "relation" => "rel",
                "area" => "area",
                _ => return e.allowed("type", "\"node\", \"way\", \"relation\", or \"area\""),
            };
            ql.push_str(&format!("{ty}{}{};", criterion(e)?, e.into()));
        }
        // Standalone criteria select nodes, as in C++.
        "bbox-query" | "around" | "polygon-query" | "area-query" | "newer" | "user" | "pivot" => {
            ql.push_str(&format!("node{}{};", criterion(e)?, e.into()));
        }
        "make" | "convert" | "if" | "for" | "complete" | "localize" | "timeline" | "retro" | "compare" | "changed" => {
            return e.err(format!("The XML element \"{}\" is not supported by this server; please use Overpass QL.", e.name));
        }
        other => return e.err(format!("Unknown tag \"{other}\" in line {}.", e.line)),
    }
    Ok(())
}

/// QL type and filter word of a `recurse` type between element kinds.
fn recurse_kind(e: &El, t: &str) -> Result<(&'static str, &'static str)> {
    Ok(match t {
        "way-node" => ("node", "w"),
        "relation-node" => ("node", "r"),
        "relation-way" => ("way", "r"),
        "relation-relation" => ("rel", "r"),
        "node-way" => ("way", "bn"),
        "node-relation" => ("rel", "bn"),
        "way-relation" => ("rel", "bw"),
        "relation-backwards" => ("rel", "br"),
        _ => return e.allowed("type", RECURSE_TYPES),
    })
}

/// A query clause.
fn criterion(c: &El) -> Result<String> {
    Ok(match c.name.as_str() {
        "has-kv" => {
            let not = c.attr("modv") == Some("not");
            let ci = if c.attr("case") == Some("ignore") { ",i" } else { "" };
            match (c.attr("regk").filter(|s| !s.is_empty()), c.attr("regv").filter(|s| !s.is_empty())) {
                (Some(rk), Some(rv)) => format!("[~{}~{}{ci}]", lit(rk), lit(rv)),
                (Some(_), None) => return c.err("In the element \"has-kv\" regular expressions on keys must be combined with a regular expression on the value."),
                (None, Some(rv)) => format!("[{}{}{}{ci}]", lit(c.attr("k").unwrap_or("")), if not { "!~" } else { "~" }, lit(rv)),
                (None, None) => {
                    let k = lit(c.attr("k").unwrap_or(""));
                    match c.attr("v").filter(|v| !v.is_empty()) {
                        Some(v) => format!("[{k}{}{}]", if not { "!=" } else { "=" }, lit(v)),
                        // modv applies to a value only: without one, C++
                        // tests for the key (`regv="." modv="not"` is "no key").
                        None => format!("[{k}]"),
                    }
                }
            }
        }
        "bbox-query" => format!(
            "({},{},{},{})",
            fnum(c.attr("s").unwrap_or("")),
            fnum(c.attr("w").unwrap_or("")),
            fnum(c.attr("n").unwrap_or("")),
            fnum(c.attr("e").unwrap_or(""))
        ),
        "around" => {
            let r = fnum(c.attr("radius").unwrap_or(""));
            match (c.attr("lat"), c.attr("lon")) {
                (Some(lat), Some(lon)) => format!("(around:{r},{},{})", fnum(lat), fnum(lon)),
                _ => format!("(around.{}:{r})", c.set("from")),
            }
        }
        "polygon-query" => {
            let pts: Vec<String> = c.attr("bounds").unwrap_or("").split_whitespace().map(fnum).collect();
            format!("(poly:{})", lit(&pts.join(" ")))
        }
        "id-query" => {
            if c.attr("lower").is_some() || c.attr("upper").is_some() {
                return c.err("id-query with lower and upper is not supported by this server.");
            }
            format!("({})", inum(c.attr("ref").unwrap_or("")))
        }
        "area-query" => match c.attr("ref").filter(|r| !r.is_empty()) {
            Some(r) => format!("(area:{})", inum(r)),
            None => format!("(area.{})", c.set("from")),
        },
        "newer" => format!("(newer:{})", lit(c.attr("than").unwrap_or(""))),
        "user" => match (c.attr("name"), c.attr("uid")) {
            (Some(n), _) => format!("(user:{})", lit(n)),
            (None, Some(u)) => format!("(uid:{})", inum(u)),
            _ => return c.err("Element \"user\" needs \"name\" or \"uid\"."),
        },
        "pivot" => format!("(pivot.{})", c.set("from")),
        "recurse" => {
            let (_, word) = recurse_kind(c, c.attr("type").unwrap_or(""))?;
            let role = match c.attr("role") {
                Some(r) => format!(":{}", lit(r)),
                None => String::new(),
            };
            format!("({word}.{}{role})", c.set("from"))
        }
        "item" => format!(".{}", c.set("set")),
        "make" | "convert" | "eval" | "changed" => return c.err(format!("The XML element \"{}\" is not supported by this server; please use Overpass QL.", c.name)),
        other => return c.err(format!("Unknown tag \"{other}\" in line {}.", c.line)),
    })
}

#[cfg(test)]
mod tests {
    use super::to_ql;

    fn check(xml: &str, ql: &str) {
        assert_eq!(to_ql(xml).unwrap(), ql);
        crate::ql::parse(xml).unwrap();
    }

    #[test]
    fn translates_common_queries() {
        // C++ counts a declaration line and an <osm-script> line before a
        // one-line query: its statements are on line 2 (3 without root).
        check(
            r#"<osm-script output="json" timeout="25"><query type="node"><has-kv k="amenity" v="cafe"/><bbox-query s="1" w="2" n="3" e="4"/></query><print/></osm-script>"#,
            "[out:json][timeout:25];\nnode[\"amenity\"=\"cafe\"](1,2,3,4);._ out body;",
        );
        check(
            r#"<union><query type="way"><has-kv k="highway" regv="^(primary|secondary)$" case="ignore"/><bbox-query s="1" w="2" n="3x" e="4"/></query><recurse type="way-node"/></union><print mode="meta" geometry="center" order="quadtile" limit="5"/>"#,
            "\n\n(way[\"highway\"~\"^(primary|secondary)$\",i](1,2,3,4);node(w._););._ out meta center qt 5;",
        );
        check(
            "<?xml version=\"1.0\"?>\n<osm-script>\n<query type=\"relation\" into=\"r\"><has-kv k=\"route\" v=\"bus\"/><around radius=\"100\" lat=\"1.5\" lon=\"2.5\"/></query>\n<difference into=\"d\"><recurse type=\"relation-way\" from=\"r\"/><query type=\"way\"><item set=\"r\"/><has-kv k=\"name\" modv=\"not\"/></query></difference>\n<print from=\"d\" mode=\"ids_only\"/></osm-script>",
            "\n\nrel[\"route\"=\"bus\"](around:100,1.5,2.5)->.r;\n(way(r.r); - way.r[\"name\"];)->.d;\n.d out ids;",
        );
        check(r#"<item set="a"/><print from="a"/>"#, "\n\n.a->.a;.a out body;");
    }

    #[test]
    fn rejects_injection_and_unsupported() {
        assert!(to_ql(r#"<osm-script><make type="x"/></osm-script>"#).is_err());
        assert!(to_ql(r#"<query type="node" into="a;out"><bbox-query s="1" w="2" n="3" e="4"/></query>"#).is_err());
        assert_eq!(to_ql(r#"<osm-script><bbox-query s="1;out;" w="2" n="3" e="4"/></osm-script>"#).unwrap(), "\nnode(1,2,3,4);");
        assert!(to_ql(r#"<query type="node"><has-kv k="a&quot;]" v="b"/></query><print/>"#).is_ok());
        let e = to_ql(r#"<osm-script><print mode="ids"/></osm-script>"#).unwrap_err().to_string();
        assert!(e.starts_with("line 2: static error: For the attribute \"mode\" of the element \"print\""), "{e}");
        let e = to_ql(r#"<osm-script></query></osm-script>"#).unwrap_err().to_string();
        assert_eq!(e, "line 2: parse error: mismatched tag");
    }
}
