//! Recursive descent parser for Overpass QL (see docs/spec/ql-grammar.md).

use super::ast::*;
use super::lexer::{Tok, Token, tokenize};
use crate::geo::BBox;
use crate::model::parse_timestamp;
use anyhow::{Result, anyhow, bail};

pub fn parse(src: &str) -> Result<Script> {
    let trimmed = src.trim_start();
    if trimmed.is_empty() {
        if src.is_empty() {
            bail!("encoding error: Your input is empty.");
        }
        bail!("encoding error: Your input contains only whitespace.");
    }
    if trimmed.starts_with('<') {
        // The XML query language becomes QL text first.
        let ql = super::xml::to_ql(src)?;
        return parse_ql(&ql);
    }
    parse_ql(src)
}

fn parse_ql(src: &str) -> Result<Script> {
    let toks = tokenize(src)?;
    let mut p = Parser { toks, pos: 0, global_bbox: false, expr_depth: 0, ectx: ECtx::Set };
    p.script()
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    global_bbox: bool,
    /// Current expression nesting, bounded to keep recursion off the stack limit.
    expr_depth: usize,
    /// What the expression being parsed can evaluate.
    ectx: ECtx,
}

const MAX_EXPR_DEPTH: usize = 256;

const AGGREGATORS: &[&str] = &["u", "set", "min", "max", "sum", "count", "gcat"];

/// Where an expression is evaluated (C++ `QL_Context`): on a set (`make`,
/// `if`), on an element (`convert`, `(if:)`, `for`, aggregator arguments) or
/// on a member of an element (`per_member`, `per_vertex`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ECtx {
    Set,
    Elem,
    Member,
}

/// Value type of an expression (C++ `Eval_Return_Type`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ty {
    Str,
    Geom,
    /// A list of strings: only `keys()`, accepted only by `for (...)`.
    Container,
}

impl Ty {
    fn name(self) -> &'static str {
        match self {
            Ty::Str => "string",
            Ty::Geom => "geometry",
            Ty::Container => "container",
        }
    }
}

/// Functions that need an element to evaluate (`t` stands for `t[...]`).
const ELEM_FUNCS: &[&str] = &[
    "t", "id", "type", "keys", "version", "timestamp", "changeset", "uid", "user", "is_closed", "geom", "length", "lat", "lon", "is_tag", "count_tags",
    "count_members", "count_distinct_members", "count_by_role", "count_distinct_by_role", "per_member", "per_vertex",
];

/// Functions that need a member of an element.
const MEMBER_FUNCS: &[&str] = &["pos", "mtype", "ref", "role", "angle"];

fn ty_of(e: &Expr) -> Ty {
    match e {
        Expr::Call(n, _) if matches!(n.as_str(), "geom" | "center" | "trace" | "hull" | "pt" | "lstr" | "poly") => Ty::Geom,
        Expr::Agg { name, .. } if name == "gcat" => Ty::Geom,
        Expr::Call(n, _) if n == "keys" => Ty::Container,
        Expr::Ternary(_, a, _) => ty_of(a),
        _ => Ty::Str,
    }
}

/// Name of the C++ evaluator for an expression, as type errors show it.
fn eval_name(e: &Expr) -> String {
    let s = match e {
        Expr::Lit(_) => "eval-fixed",
        Expr::Tag(_) => "eval-value",
        Expr::Generic => "eval-generic",
        Expr::SetProp { .. } => "eval-set-key",
        Expr::Call(n, _) => match n.as_str() {
            "t" => "eval-value",
            "keys" => "eval-all-keys",
            "is_tag" => "eval-is-tag",
            "count_tags" | "count_members" | "count_distinct_members" | "count_by_role" | "count_distinct_by_role" => "eval-prop-count",
            "is_closed" => "eval-is-closed",
            "geom" => "eval-geometry",
            "mtype" => "eval-membertype",
            "pt" => "eval-point",
            "lstr" => "eval-linestring",
            "poly" => "eval-polygon",
            "per_member" => "eval-per-member",
            "per_vertex" => "eval-per-vertex",
            "is_number" => "eval-is-number",
            "is_date" => "eval-is-date",
            other => return format!("eval-{}", other.replace('_', "-")),
        },
        Expr::Agg { name, .. } => match name.as_str() {
            "u" => "eval-union",
            "set" => "eval-set",
            "min" => "eval-min",
            "max" => "eval-umax",
            "sum" => "eval-sum",
            "gcat" => "eval-geom-concat",
            _ => "eval-set-count",
        },
        Expr::Unary(UnOp::Not, _) => "eval-not",
        Expr::Unary(UnOp::Neg, _) => "eval-negate",
        Expr::Binary(op, ..) => match op {
            BinOp::Or => "eval-or",
            BinOp::And => "eval-and",
            BinOp::Eq => "eval-equal",
            BinOp::Ne => "eval-not-equal",
            BinOp::Lt => "eval-less",
            BinOp::Le => "eval-less-equal",
            BinOp::Gt => "eval-greater",
            BinOp::Ge => "eval-greater-equal",
            BinOp::Add => "eval-plus",
            BinOp::Sub => "eval-minus",
            BinOp::Mul => "eval-times",
            BinOp::Div => "eval-divided-by",
        },
        Expr::Ternary(..) => "eval-ternary",
    };
    s.to_string()
}

fn parse_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// `strtoll(base 0)` followed by an optional binary or decimal suffix.
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let digits_end = s.find(|c: char| !(c.is_ascii_alphanumeric())).unwrap_or(s.len());
    let (num, rest) = s.split_at(digits_end);
    let (num, suffix) = match num.find(|c: char| !c.is_ascii_hexdigit() && c != 'x' && c != 'X') {
        Some(p) => (&num[..p], &num[p..]),
        None => (num, ""),
    };
    let suffix = format!("{}{}", suffix, rest.trim());
    let v: u64 = if let Some(h) = num.strip_prefix("0x").or_else(|| num.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()?
    } else if num.len() > 1 && num.starts_with('0') {
        u64::from_str_radix(&num[1..], 8).ok()?
    } else {
        num.parse().ok()?
    };
    let mult: u64 = match suffix.trim() {
        "" => 1,
        "ki" => 1 << 10,
        "Mi" => 1 << 20,
        "Gi" => 1 << 30,
        "k" => 1_000,
        "M" => 1_000_000,
        "G" => 1_000_000_000,
        _ => return None,
    };
    let v = v.checked_mul(mult)?;
    (v > 0).then_some(v)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }
    fn line(&self) -> usize {
        self.toks[self.pos].line
    }
    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn found(&self) -> String {
        match &self.toks[self.pos].tok {
            Tok::Word(w) => w.clone(),
            Tok::Str(s) => format!("\"{s}\""),
            Tok::Num(n) => n.clone(),
            Tok::P(p) => p.to_string(),
            Tok::Other(o) => o.clone(),
            Tok::Eof => "end of input".into(),
        }
    }
    fn err<T>(&self, msg: impl std::fmt::Display) -> Result<T> {
        if matches!(self.peek(), Tok::Eof) {
            bail!("line {}: parse error: {} Unexpected end of input.", self.line(), msg)
        }
        bail!("line {}: parse error: {}", self.line(), msg)
    }
    fn expected<T>(&self, what: &str) -> Result<T> {
        self.err(format!("'{what}' expected - '{}' found.", self.found()))
    }
    fn static_err<T>(&self, line: usize, msg: impl std::fmt::Display) -> Result<T> {
        bail!("line {line}: static error: {msg}")
    }
    fn is_p(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::P(x) if *x == p)
    }
    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Word(x) if x == w)
    }
    fn eat_p(&mut self, p: &str) -> bool {
        if self.is_p(p) {
            self.next();
            true
        } else {
            false
        }
    }
    fn expect_p(&mut self, p: &str) -> Result<()> {
        if self.eat_p(p) { Ok(()) } else { self.expected(p) }
    }
    fn ident(&mut self, what: &str) -> Result<String> {
        match self.peek().clone() {
            Tok::Word(w) => {
                self.next();
                Ok(w)
            }
            _ => self.err(format!("{what} expected - '{}' found.", self.found())),
        }
    }
    /// TEXT acceptor: string, word, or number token (verbatim).
    fn text(&mut self, what: &str) -> Result<String> {
        match self.peek().clone() {
            Tok::Word(w) => {
                self.next();
                Ok(w)
            }
            Tok::Str(s) => {
                self.next();
                Ok(s)
            }
            Tok::Num(n) => {
                self.next();
                Ok(n)
            }
            _ => self.err(format!("{what} expected - '{}' found.", self.found())),
        }
    }
    fn number(&mut self, what: &str) -> Result<f64> {
        let line = self.line();
        let neg = self.eat_p("-");
        let t = self.text(what)?;
        let v = parse_number(&t).ok_or_else(|| anyhow!("line {line}: parse error: {what} expected - '{t}' found."))?;
        Ok(if neg { -v } else { v })
    }
    /// `.name` set reference
    fn set_name(&mut self) -> Result<String> {
        self.expect_p(".")?;
        self.ident("Variable")
    }
    fn opt_into(&mut self) -> Result<String> {
        if self.eat_p("->") { self.set_name() } else { Ok("_".into()) }
    }

    fn script(&mut self) -> Result<Script> {
        let mut settings = Settings::default();
        if self.is_p("[") {
            while self.is_p("[") {
                self.setting(&mut settings)?;
            }
            self.expect_p(";")?;
        }
        self.global_bbox = settings.bbox.is_some();
        let mut stmts = Vec::new();
        while !matches!(self.peek(), Tok::Eof) {
            stmts.push(self.stmt(0)?);
        }
        Ok(Script { settings, stmts })
    }

    fn setting(&mut self, s: &mut Settings) -> Result<()> {
        self.expect_p("[")?;
        let line = self.line();
        let key = self.ident("Keyword")?;
        // C++ checks the syntax first: a colon, then the value(s), then ']';
        // an unknown name is a static error afterwards.
        if !self.eat_p(":") {
            return self.expected(":");
        }
        match key.as_str() {
            "out" => {
                let f = self.text("Value")?;
                s.out = match f.as_str() {
                    "xml" => OutFormat::Xml,
                    "json" => OutFormat::Json,
                    "csv" => OutFormat::Csv(self.csv_spec()?),
                    "custom" | "popup" => return self.err(format!("Unknown output format: {f}")),
                    _ => return self.err(format!("Unknown output format: {f}")),
                };
            }
            "timeout" | "maxsize" => {
                // Every token up to ']' is the value; a malformed one is the
                // C++ static error below.
                let mut v = self.text("Value")?;
                while !self.is_p("]") && !matches!(self.peek(), Tok::Eof) {
                    v.push(' ');
                    v.push_str(&self.found());
                    self.next();
                }
                let attr = if key == "timeout" { "timeout" } else { "element-limit" };
                let Some(n) = parse_size(&v) else {
                    return self.static_err(
                        line,
                        format!("For the attribute \"{attr}\" of the element \"osm-script\" the only allowed values are positive integers optionally with suffix \"ki\", \"Mi\", or \"Gi\"."),
                    );
                };
                if key == "timeout" {
                    s.timeout = n.min(u32::MAX as u64) as u32;
                } else {
                    s.maxsize = n;
                }
            }
            "bbox" => {
                let mut vals = vec![self.text("Value")?];
                while self.eat_p(",") {
                    vals.push(self.number_text()?);
                }
                if vals.len() != 4 {
                    return self.static_err(line, "A bounding box needs four comma-separated values.");
                }
                let nums: Vec<f64> = vals.iter().map(|v| parse_number(v).unwrap_or(0.0)).collect();
                let b = BBox { south: nums[0], west: nums[1], north: nums[2], east: nums[3] };
                if !(-90.0..=90.0).contains(&b.south) || !(-90.0..=90.0).contains(&b.north) {
                    return self.static_err(line, "Latitudes in bounding boxes must be between -90.0 and 90.0.");
                }
                if !(-180.0..=180.0).contains(&b.west) || !(-180.0..=180.0).contains(&b.east) {
                    return self.static_err(line, "Longitudes in bounding boxes must be between -180.0 and 180.0.");
                }
                s.bbox = Some(b);
            }
            "date" | "diff" | "adiff" => {
                return self.static_err(line, format!("The setting [{key}:...] needs attic data, which this server does not support yet."));
            }
            _ => {
                self.text("Value")?;
                self.expect_p("]")?;
                return self.static_err(line, format!("Unknown attribute \"{key}\" in element \"osm-script\"."));
            }
        }
        self.expect_p("]")
    }

    /// TEXT that may be a negative number.
    fn text_or_negative(&mut self) -> Result<String> {
        if self.eat_p("-") {
            return Ok(format!("-{}", self.text("Value")?));
        }
        self.text("Value")
    }

    /// TEXT possibly preceded by a separate minus token.
    fn number_text(&mut self) -> Result<String> {
        if self.eat_p("-") {
            Ok(format!("-{}", self.text("Value")?))
        } else {
            self.text("Value")
        }
    }

    fn csv_spec(&mut self) -> Result<CsvSpec> {
        let mut spec = CsvSpec { fields: Vec::new(), header: true, separator: "\t".into() };
        self.expect_p("(")?;
        loop {
            if self.eat_p("::") {
                let w = self.text("CSV format specification")?;
                spec.fields.push(format!("::{w}"));
            } else {
                spec.fields.push(self.text("CSV format specification")?);
            }
            if !self.eat_p(",") {
                break;
            }
        }
        if self.eat_p(";") {
            let h = self.text("CSV output header line (true or false)")?;
            spec.header = h != "false";
            if self.eat_p(";") {
                spec.separator = self.text("CSV separator character")?;
            }
        }
        self.expect_p(")")?;
        Ok(spec)
    }

    fn bbox_static(&self, line: usize, elem: &str, b: BBox) -> Result<BBox> {
        for (name, v) in [("s", b.south), ("n", b.north)] {
            if !(-90.0..=90.0).contains(&v) {
                return self.static_err(line, format!("For the attribute \"{name}\" of the element \"{elem}\" the only allowed values are floats between -90.0 and 90.0."));
            }
        }
        for (name, v) in [("w", b.west), ("e", b.east)] {
            if !(-180.0..=180.0).contains(&v) {
                return self.static_err(line, format!("For the attribute \"{name}\" of the element \"{elem}\" the only allowed values are floats between -180.0 and 180.0."));
            }
        }
        if b.south > b.north {
            return self.static_err(line, format!("The value of attribute \"n\" of the element \"{elem}\" must always be greater or equal than the value of attribute \"s\"."));
        }
        Ok(b)
    }

    fn block(&mut self, depth: usize) -> Result<Vec<Stmt>> {
        let close = if self.eat_p("{") {
            "}"
        } else if self.eat_p("(") {
            ")"
        } else {
            return self.err(format!("'{{' or '(' expected - '{}' found.", self.found()));
        };
        let mut body = Vec::new();
        while !self.is_p(close) {
            if matches!(self.peek(), Tok::Eof) {
                return self.expected(close);
            }
            body.push(self.stmt(depth + 1)?);
        }
        self.next();
        Ok(body)
    }

    fn end(&mut self) -> Result<()> {
        self.expect_p(";")
    }

    fn stmt(&mut self, depth: usize) -> Result<Stmt> {
        if depth >= 1023 {
            return self.err("Nesting of statements is limited to 1023 levels");
        }
        let line = self.line();
        if self.is_p("(") {
            return self.union(depth);
        }
        if let Tok::Word(w) = self.peek().clone() {
            match w.as_str() {
                "foreach" => {
                    self.next();
                    let input = if self.is_p(".") { self.set_name()? } else { "_".into() };
                    let var = if self.eat_p("->") { Some(self.set_name()?) } else { None };
                    let body = self.block(depth + 1)?;
                    self.eat_p(";");
                    return Ok(Stmt { kind: StmtKind::Foreach { input, var, body }, into: "_".into(), line });
                }
                "for" => {
                    self.next();
                    let input = if self.is_p(".") { self.set_name()? } else { "_".into() };
                    let var = if self.eat_p("->") { Some(self.set_name()?) } else { None };
                    self.expect_p("(")?;
                    let key = {
                        let saved = std::mem::replace(&mut self.ectx, ECtx::Elem);
                        let e = self.expr();
                        self.ectx = saved;
                        let e = e?;
                        if ty_of(&e) != Ty::Container {
                            self.want(&e, Ty::Str, "string or container")?;
                        }
                        e
                    };
                    self.expect_p(")")?;
                    let body = self.block(depth + 1)?;
                    self.eat_p(";");
                    return Ok(Stmt { kind: StmtKind::For { input, var, key, body }, into: "_".into(), line });
                }
                "complete" => {
                    self.next();
                    let limit = if self.eat_p("(") {
                        let t = self.text("maximum loop number")?;
                        self.expect_p(")")?;
                        let n: u64 = t.parse().unwrap_or(0);
                        if !(1..=1_048_576).contains(&n) {
                            return self.static_err(line, "For the attribute \"maxnum\" of the element \"complete\" the only allowed values are positive integers between 1 and 1048576.");
                        }
                        Some(n)
                    } else {
                        None
                    };
                    let input = if self.is_p(".") { self.set_name()? } else { "_".into() };
                    let into = self.opt_into()?;
                    let body = self.block(depth)?;
                    self.eat_p(";");
                    return Ok(Stmt { kind: StmtKind::Complete { input, var: Some(into.clone()), limit, body }, into, line });
                }
                "if" => {
                    self.next();
                    self.expect_p("(")?;
                    let cond = self.typed(ECtx::Set, Ty::Str, "string")?;
                    self.expect_p(")")?;
                    let then = self.block(depth)?;
                    let els = if self.is_word("else") {
                        self.next();
                        self.block(depth)?
                    } else {
                        Vec::new()
                    };
                    self.eat_p(";");
                    return Ok(Stmt { kind: StmtKind::If { cond, then, els }, into: "_".into(), line });
                }
                "retro" | "timeline" => {
                    return self.static_err(line, format!("The statement \"{w}\" needs attic data, which this server does not support yet."));
                }
                _ => {}
            }
        }
        let mut input: Option<String> = None;
        if self.is_p(".") {
            input = Some(self.set_name()?);
        }
        let inp = input.clone().unwrap_or_else(|| "_".into());
        match self.peek().clone() {
            Tok::P(op @ ("<" | "<<" | ">" | ">>")) => {
                self.next();
                let op = match op {
                    "<" => RecurseOp::Up,
                    "<<" => RecurseOp::UpRel,
                    ">" => RecurseOp::Down,
                    _ => RecurseOp::DownRel,
                };
                let into = self.opt_into()?;
                self.end()?;
                Ok(Stmt { kind: StmtKind::Recurse { op, input: inp }, into, line })
            }
            Tok::Word(w) if matches!(w.as_str(), "out" | "make" | "convert" | "is_in" | "map_to_area" | "local" | "compare") => {
                self.next();
                match w.as_str() {
                    "out" => {
                        let opts = self.out_opts()?;
                        let line = self.line();
                        self.end()?;
                        Ok(Stmt { kind: StmtKind::Out { input: inp, opts }, into: "_".into(), line })
                    }
                    "is_in" => {
                        let coord = if self.eat_p("(") {
                            let lat = self.number("Value")?;
                            if !self.eat_p(",") {
                                return self.static_err(line, "For the attribute \"lon\" of the element \"coord-query\" the only allowed values are floats between -180.0 and 180.0.");
                            }
                            let lon = self.number("Value")?;
                            self.expect_p(")")?;
                            if !(-90.0..=90.0).contains(&lat) {
                                return self.static_err(line, "For the attribute \"lat\" of the element \"coord-query\" the only allowed values are floats between -90.0 and 90.0.");
                            }
                            if !(-180.0..=180.0).contains(&lon) {
                                return self.static_err(line, "For the attribute \"lon\" of the element \"coord-query\" the only allowed values are floats between -180.0 and 180.0.");
                            }
                            Some((lat, lon))
                        } else {
                            None
                        };
                        let into = self.opt_into()?;
                        self.end()?;
                        Ok(Stmt { kind: StmtKind::IsIn { input: inp, coord }, into, line })
                    }
                    "map_to_area" => {
                        let into = self.opt_into()?;
                        self.end()?;
                        Ok(Stmt { kind: StmtKind::MapToArea { input: inp }, into, line })
                    }
                    "make" | "convert" => {
                        if w == "make" && input.is_some() {
                            return self.static_err(line, "Unknown attribute \"from\" in element \"make\".");
                        }
                        let name = match self.peek() {
                            Tok::Word(_) => self.ident("Element class name")?,
                            _ => return self.static_err(line, "The attribute type must be set to a nonempty string."),
                        };
                        let entries = self.make_entries(w == "convert")?;
                        let into = self.opt_into()?;
                        self.end()?;
                        let kind = if w == "make" { StmtKind::Make { name, entries } } else { StmtKind::Convert { input: inp, name, entries } };
                        Ok(Stmt { kind, into, line })
                    }
                    _ => self.static_err(line, format!("The statement \"{w}\" is not supported by this server yet.")),
                }
            }
            _ if input.is_some() => {
                // `.a;` or `.a->.b;`: set copy.
                let into = self.opt_into()?;
                self.end()?;
                Ok(Stmt { kind: StmtKind::Item { set: inp }, into, line })
            }
            Tok::Word(w) => {
                let Some(types) = Types::parse(&w) else {
                    return self.err(format!("Unknown type \"{w}\""));
                };
                self.next();
                let filters = self.query_filters(&types)?;
                if filters.is_empty() && !self.global_bbox {
                    return self.err("An empty query is not allowed");
                }
                let into = self.opt_into()?;
                self.end()?;
                Ok(Stmt { kind: StmtKind::Query { types, filters }, into, line })
            }
            _ => self.err(format!("Unknown type \"{}\"", self.found())),
        }
    }

    fn union(&mut self, depth: usize) -> Result<Stmt> {
        let line = self.line();
        self.expect_p("(")?;
        let mut items = Vec::new();
        let mut minus: Option<Stmt> = None;
        while !self.is_p(")") {
            if matches!(self.peek(), Tok::Eof) {
                return self.expected(")");
            }
            if !items.is_empty() && self.is_p("-") && minus.is_none() && items.len() == 1 {
                self.next();
                let m = self.stmt(depth + 2)?;
                if matches!(m.kind, StmtKind::Out { .. }) {
                    return self.static_err(m.line, "Element \"print\" cannot be subelement of element \"difference\".");
                }
                minus = Some(m);
                if !self.is_p(")") {
                    return self.err("difference always requires two operands");
                }
                break;
            }
            let s = self.stmt(depth + 2)?;
            if matches!(s.kind, StmtKind::Out { .. }) {
                return self.static_err(s.line, "Element \"print\" cannot be subelement of element \"union\".");
            }
            items.push(s);
        }
        self.expect_p(")")?;
        let into = self.opt_into()?;
        self.end()?;
        let kind = match minus {
            Some(m) => StmtKind::Difference(Box::new(items.remove(0)), Box::new(m)),
            None => StmtKind::Union(items),
        };
        Ok(Stmt { kind, into, line })
    }

    fn out_opts(&mut self) -> Result<OutOpts> {
        let mut o = OutOpts::default();
        loop {
            match self.peek().clone() {
                Tok::Word(w) => {
                    match w.as_str() {
                        "ids" => o.mode = OutMode::Ids,
                        "skel" => o.mode = OutMode::Skel,
                        "body" => o.mode = OutMode::Body,
                        "tags" => o.mode = OutMode::Tags,
                        "meta" => o.mode = OutMode::Meta,
                        "count" => o.count = true,
                        "noids" => o.noids = true,
                        "geom" => o.geom = GeomMode::Geom,
                        "bb" => o.geom = GeomMode::Bb,
                        "center" => o.geom = GeomMode::Center,
                        "qt" => o.order = Order::Quadtile,
                        "asc" => o.order = Order::Id,
                        "quirks" => {
                            return self.static_err(
                                self.line(),
                                "For the attribute \"mode\" of the element \"print\" the only allowed values are \"ids_only\", \"skeleton\", \"body\", \"tags\",  \"count\", or \"meta\".",
                            );
                        }
                        _ => return self.err(format!("Invalid parameter for print: \"{w}\"")),
                    }
                    if w != "count" && matches!(w.as_str(), "ids" | "skel" | "body" | "tags" | "meta") {
                        o.count = false;
                    }
                    self.next();
                }
                Tok::Num(n) => {
                    self.next();
                    let digits: String = n.chars().take_while(|c| c.is_ascii_digit()).collect();
                    o.limit = Some(digits.parse::<u64>().unwrap_or(u64::MAX).min(u32::MAX as u64));
                }
                Tok::P("(") => {
                    let line = self.line();
                    self.next();
                    let south = self.number("Value")?;
                    self.expect_p(",")?;
                    let west = self.number("Value")?;
                    self.expect_p(",")?;
                    let north = self.number("Value")?;
                    self.expect_p(",")?;
                    let east = self.number("Value")?;
                    self.expect_p(")")?;
                    o.geom_bbox = Some(self.bbox_static(line, "print", BBox { south, west, north, east })?);
                }
                Tok::P(";") | Tok::Eof => break,
                _ => return self.err(format!("Invalid parameter for print: \"{}\"", self.found())),
            }
        }
        Ok(o)
    }

    fn query_filters(&mut self, types: &Types) -> Result<Vec<Filter>> {
        let mut tags = Vec::new();
        let mut parens = Vec::new();
        if self.is_p(".") {
            tags.push(Filter::Input(self.set_name()?));
        }
        loop {
            if self.is_p("[") {
                if let Some(f) = self.tag_filter()? {
                    tags.push(f);
                }
            } else if self.is_p("(") {
                parens.push(self.paren_filter(types)?);
            } else if self.is_p(".") {
                tags.push(Filter::Input(self.set_name()?));
            } else {
                break;
            }
        }
        tags.extend(parens);
        Ok(tags)
    }

    fn tag_filter(&mut self) -> Result<Option<Filter>> {
        let line = self.line();
        self.expect_p("[")?;
        let key_regex = self.eat_p("~");
        if self.eat_p("!") {
            let k = self.text("Key")?;
            self.expect_p("]")?;
            if k.is_empty() {
                return self.static_err(line, "For the attribute \"k\" of the element \"has-kv\" the only allowed values are non-empty strings.");
            }
            return Ok(Some(Filter::NotKey(k)));
        }
        let key = self.text("Key")?;
        let mut neg = false;
        if self.eat_p("!") {
            neg = true;
        }
        let f = if self.is_p("]") {
            self.next();
            if key_regex {
                return self.err("A regular expression for a key can only be combined with a regular expression as value criterion");
            }
            Some(Filter::HasKey(key))
        } else if self.is_p("=") || self.is_p("!=") {
            if key_regex {
                return self.err("A regular expression for a key can only be combined with a regular expression as value criterion");
            }
            let ne = self.is_p("!=");
            self.next();
            if self.eat_p("]") {
                return Ok(None);
            }
            let v = self.text("Value")?;
            self.expect_p("]")?;
            if v.is_empty() {
                None
            } else if ne {
                Some(Filter::Ne(key, v))
            } else {
                Some(Filter::Eq(key, v))
            }
        } else if self.eat_p("~") {
            let re = self.text("Value")?;
            let mut ci = false;
            if self.eat_p(",") {
                match self.peek() {
                    Tok::Word(w) if w == "i" => {
                        self.next();
                        ci = true;
                    }
                    _ => return self.expected("i"),
                }
            }
            self.expect_p("]")?;
            if key_regex {
                if key.is_empty() {
                    return self.static_err(line, "For the attribute \"k\" of the element \"has-kv\" the only allowed values are non-empty strings.");
                }
                if neg {
                    return self.static_err(line, "In the element \"has-kv\" regular expressions on keys cannot be combined with negation.");
                }
                check_regex(line, &key)?;
                check_regex(line, &re)?;
                Some(Filter::KeyRegex { key, value: re, ci })
            } else if re.is_empty() {
                Some(Filter::HasKey(key))
            } else {
                check_regex(line, &re)?;
                Some(Filter::Regex { key, re, ci, neg })
            }
        } else {
            return self.err(format!("'!', '~', '=', '!=', or ']'  expected - '{}' found.", self.found()));
        };
        if let Some(Filter::HasKey(k) | Filter::Eq(k, _) | Filter::Ne(k, _) | Filter::Regex { key: k, .. }) = &f {
            if k.is_empty() {
                return self.static_err(line, "For the attribute \"k\" of the element \"has-kv\" the only allowed values are non-empty strings.");
            }
        }
        Ok(f)
    }

    /// C++ reads the numbers of a `(...)` clause as expressions: a word,
    /// number, string or one of `| & \\ ~` after a number is "Operator
    /// expected"; `, ) ; ] } ->` end the number and leave the error, if any,
    /// to the clause. (Arithmetic inside the clause is not evaluated here.)
    fn after_clause_number(&self) -> Result<()> {
        match self.peek() {
            Tok::Word(_) | Tok::Num(_) | Tok::Str(_) | Tok::P("|" | "&" | "\\" | "~") | Tok::Other(_) => {
                self.err(format!("Operator expected, but \"{}\"found.", self.found()))
            }
            _ => Ok(()),
        }
    }

    fn paren_filter(&mut self, types: &Types) -> Result<Filter> {
        let line = self.line();
        self.expect_p("(")?;
        let f = match self.peek().clone() {
            Tok::Num(_) | Tok::P("-") => {
                let first = self.number("Number")?;
                self.after_clause_number()?;
                if self.eat_p(")") {
                    if first <= 0.0 || first.fract() != 0.0 {
                        return self.static_err(line, "For the attribute \"ref\" of the element \"id-query\" the only allowed values are positive integers.");
                    }
                    return Ok(Filter::Ids(vec![first as u64]));
                }
                let mut vals = vec![first];
                while self.eat_p(",") {
                    vals.push(self.number("Number")?);
                    self.after_clause_number()?;
                }
                if !self.is_p(")") {
                    return self.expected(")");
                }
                if vals.len() != 4 {
                    return self.err("bbox requires four arguments");
                }
                Filter::BBox(self.bbox_static(line, "bbox-query", BBox { south: vals[0], west: vals[1], north: vals[2], east: vals[3] })?)
            }
            Tok::P(op @ ("<" | "<<" | ">" | ">>")) => {
                self.next();
                if self.is_p(".") {
                    self.set_name()?;
                }
                Filter::RecurseOp(match op {
                    "<" => RecurseOp::Up,
                    "<<" => RecurseOp::UpRel,
                    ">" => RecurseOp::Down,
                    _ => RecurseOp::DownRel,
                })
            }
            Tok::Word(w) => {
                self.next();
                match w.as_str() {
                    "id" => {
                        self.expect_p(":")?;
                        let mut ids = vec![self.number("OSM element id")?];
                        while self.eat_p(",") {
                            ids.push(self.number("OSM element id")?);
                        }
                        if types.derived {
                            return self.static_err(line, "For the attribute \"type\" of the element \"id-query\" the only allowed values are \"node\", \"way\", \"relation\", or \"area\".");
                        }
                        if ids.iter().any(|v| *v <= 0.0 || v.fract() != 0.0) {
                            return self.static_err(line, "For the attribute \"ref\" of the element \"id-query\" the only allowed values are positive integers.");
                        }
                        Filter::Ids(ids.into_iter().map(|v| v as u64).collect())
                    }
                    "around" => {
                        let input = if self.is_p(".") { Some(self.set_name()?) } else { None };
                        if !self.eat_p(":") {
                            return self.err("around requires the radius as first argument");
                        }
                        let radius = self.number("Value")?;
                        let mut coords = Vec::new();
                        while self.eat_p(",") {
                            coords.push(self.number("Value")?);
                        }
                        if coords.len() % 2 != 0 {
                            return self.err("around requires an odd number of arguments");
                        }
                        if radius < 0.0 {
                            return self.static_err(line, "For the attribute \"radius\" of the element \"around\" the only allowed values are nonnegative floats.");
                        }
                        let points: Vec<(f64, f64)> = coords.chunks(2).map(|c| (c[0], c[1])).collect();
                        for (lat, lon) in &points {
                            if !(-90.0..=90.0).contains(lat) {
                                return self.static_err(line, "For the attribute \"lat\" of the element \"around\" the only allowed values are floats between -90.0 and 90.0.");
                            }
                            if !(-180.0..=180.0).contains(lon) {
                                return self.static_err(line, "For the attribute \"lon\" of the element \"around\" the only allowed values are floats between -180.0 and 180.0.");
                            }
                        }
                        Filter::Around { input, radius, points }
                    }
                    "poly" => {
                        self.expect_p(":")?;
                        let s = self.text("Value")?;
                        let nums: Option<Vec<f64>> = s.split_whitespace().map(parse_number).collect();
                        let Some(nums) = nums.filter(|n| n.len() % 2 == 0 && n.len() >= 6) else {
                            return self.static_err(line, "For the attribute \"bounds\" of the element \"polygon-query\" the only allowed values are an even number of float values.");
                        };
                        let pts: Vec<(f64, f64)> = nums.chunks(2).map(|c| (c[0], c[1])).collect();
                        if pts.iter().any(|(lat, lon)| !(-90.0..=90.0).contains(lat) || !(-180.0..=180.0).contains(lon)) {
                            return self.static_err(line, "For the attribute \"bounds\" of the element \"polygon-query\" the only allowed values are pairs of latitudes and longitudes.");
                        }
                        Filter::Poly(pts)
                    }
                    "newer" => {
                        self.expect_p(":")?;
                        let s = self.text("Value")?;
                        let Some(ts) = parse_timestamp(&s) else {
                            return self.static_err(line, "The attribute \"than\" of the element \"newer\" must be a timestamp of the form \"YYYY-MM-DDThh:mm:ssZ\".");
                        };
                        Filter::Newer(ts)
                    }
                    "user" => {
                        self.expect_p(":")?;
                        let mut names = vec![self.text("Value")?];
                        while self.eat_p(",") {
                            names.push(self.text("Value")?);
                        }
                        Filter::User(names)
                    }
                    "uid" => {
                        self.expect_p(":")?;
                        let mut ids = vec![self.number("Value")? as u32];
                        while self.eat_p(",") {
                            ids.push(self.number("Value")? as u32);
                        }
                        Filter::Uid(ids)
                    }
                    "user_touched" | "uid_touched" | "changed" => {
                        return self.static_err(line, format!("The filter \"{w}\" needs attic data, which this server does not support yet."));
                    }
                    "area" => {
                        let set = if self.is_p(".") { self.set_name()? } else { "_".into() };
                        let id = if self.eat_p(":") {
                            // C++: atoll, then stored as a 32 bit area id.
                            let v = atoll(&self.text("Value")?);
                            if v <= 0 {
                                return self.static_err(line, "For the attribute \"ref\" of the element \"area-query\" the only allowed values are positive integers.");
                            }
                            Some(v as u64 & 0xffff_ffff)
                        } else {
                            None
                        };
                        Filter::Area { set, id }
                    }
                    "pivot" => Filter::Pivot(if self.is_p(".") { self.set_name()? } else { "_".into() }),
                    "if" => {
                        self.expect_p(":")?;
                        Filter::If(self.typed(ECtx::Elem, Ty::Str, "string")?)
                    }
                    "w" | "r" | "bn" | "bw" | "br" | "way_cnt" | "way_link" => {
                        let kind = match w.as_str() {
                            "w" => RecurseFilter::W,
                            "r" => RecurseFilter::R,
                            "bn" => RecurseFilter::Bn,
                            "bw" => RecurseFilter::Bw,
                            "way_cnt" => RecurseFilter::WayCnt,
                            "way_link" => RecurseFilter::WayLink,
                            _ => RecurseFilter::Br,
                        };
                        // Result types each recursion accepts, as the C++ factory checks them.
                        let is = |n: bool, w: bool, r: bool| types.node == n && types.way == w && types.relation == r && !types.area && !types.derived;
                        let (ok, msg) = match kind {
                            RecurseFilter::R => (!types.area && !types.derived && (types.node || types.way || types.relation), "'r' produces nodes, ways, or relations."),
                            RecurseFilter::W => (is(true, false, false), "'w' produces nodes."),
                            RecurseFilter::WayCnt => (is(true, false, false), "'way_cnt' produces nodes."),
                            RecurseFilter::WayLink => (is(true, false, false), "'way_link' produces nodes."),
                            RecurseFilter::Br => (is(false, false, true), "'br' produces relations."),
                            RecurseFilter::Bw => (is(false, false, true), "'bw' produces relations."),
                            RecurseFilter::Bn => (is(false, true, false) || is(false, false, true) || is(false, true, true), "'bn' produces ways or relations."),
                        };
                        if !ok {
                            return self.err(format!("A recursion of type {msg}"));
                        }
                        let input = if self.is_p(".") { self.set_name()? } else { "_".into() };
                        let (mut role, mut positions, mut count) = (None, None, None);
                        if matches!(kind, RecurseFilter::WayCnt | RecurseFilter::WayLink) {
                            // `:l`, `:l-u` or `:l-`; without a range nothing matches.
                            let (mut lower, mut upper) = (1i64, 0i64);
                            if self.eat_p(":") {
                                let l = self.text("Value")?;
                                lower = atoll(&l);
                                upper = lower;
                                if self.eat_p("-") {
                                    upper = if self.is_p(")") { u32::MAX as i64 } else { atoll(&self.text("Value")?) };
                                }
                                if lower <= 0 {
                                    return self.static_err(line, "lower must be an integer greater or equal 1");
                                }
                                if upper < lower {
                                    return self.static_err(line, "upper must be an integer greater or equal lower");
                                }
                            }
                            count = Some((lower.clamp(0, u32::MAX as i64) as u32, upper.clamp(0, u32::MAX as i64) as u32));
                        } else if self.eat_p(":") {
                            let mut vals = vec![self.text_or_negative()?];
                            while self.eat_p(",") {
                                vals.push(self.text_or_negative()?);
                            }
                            if kind == RecurseFilter::R || types.relation {
                                if kind == RecurseFilter::Bn && types.way {
                                    return self.err("A recursion of type 'bn' cannot have restrictions.");
                                }
                                // Only the first role is used, as in C++.
                                role = Some(vals.swap_remove(0));
                            } else {
                                let mut ps = Vec::with_capacity(vals.len());
                                for v in &vals {
                                    match atoll(v) {
                                        0 => return self.static_err(line, format!("Invalid pos '{v}' in positions list.")),
                                        p => ps.push(p),
                                    }
                                }
                                positions = Some(ps);
                            }
                        }
                        Filter::Recurse { kind, input, role, positions, count }
                    }
                    // `(bbox)` left over when no bbox parameter replaced it.
                    "bbox" => return self.err("bbox requires four arguments"),
                    _ => return self.err("Unknown query clause"),
                }
            }
            Tok::P(")") => bail!("line 0: parse error: Unknown query clause"),
            _ => return self.err("Unknown query clause"),
        };
        self.expect_p(")")?;
        Ok(f)
    }

    fn make_entries(&mut self, convert: bool) -> Result<Vec<MakeEntry>> {
        let ctx = if convert { ECtx::Elem } else { ECtx::Set };
        let mut out: Vec<MakeEntry> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        if self.is_p(";") || self.is_p("->") {
            return Ok(out);
        }
        loop {
            let line = self.line();
            let entry = if self.eat_p("!") {
                MakeEntry { key: MakeKey::Remove(self.text("Key")?), value: None }
            } else if self.eat_p("::") {
                let key = if self.is_p("=") {
                    if !convert {
                        return self.static_err(line, "The generic tag set property can only be called from convert");
                    }
                    MakeKey::Generic { from: None }
                } else {
                    match self.ident("Key")?.as_str() {
                        "id" => MakeKey::Id,
                        "geom" => MakeKey::Geom,
                        _ => return self.static_err(line, "The only allowed special properties are \"id\" and \"geometry\""),
                    }
                };
                self.expect_p("=")?;
                let t = if key == MakeKey::Geom { Ty::Geom } else { Ty::Str };
                MakeEntry { key, value: Some(self.rhs(ctx, t, t.name())?) }
            } else if matches!(self.peek(), Tok::Word(_)) && matches!(self.peek_at(1), Tok::P("::")) {
                let from = self.ident("Variable")?;
                self.expect_p("::")?;
                self.expect_p("=")?;
                MakeEntry { key: MakeKey::Generic { from: Some(from) }, value: Some(self.rhs(ctx, Ty::Str, "string")?) }
            } else {
                let k = self.text("Key")?;
                if !self.eat_p("=") {
                    if self.is_p(":") || self.is_p("-") || self.is_p(".") {
                        return self.static_err(line, format!("if special character \"{}\" is present in property name then the property name must be in quotes", self.found()));
                    }
                    return self.static_err(line, "To set a property it must have a value to the right of the equal sign");
                }
                MakeEntry { key: MakeKey::Tag(k), value: Some(self.rhs(ctx, Ty::Str, "string")?) }
            };
            if let MakeKey::Tag(k) | MakeKey::Remove(k) = &entry.key {
                if !seen.insert(k.clone()) {
                    return self.static_err(line, format!("A key cannot be added twice to an element: \"{k}\""));
                }
            }
            out.push(entry);
            if !self.eat_p(",") {
                break;
            }
        }
        Ok(out)
    }

    // --- Expressions (lowest precedence first); all binary operators are
    // left-associative, including the ternary.

    pub fn expr(&mut self) -> Result<Expr> {
        self.expr_depth += 1;
        if self.expr_depth > MAX_EXPR_DEPTH {
            return self.err(format!("Expressions are limited to {MAX_EXPR_DEPTH} nested levels"));
        }
        let r = self.expr_inner();
        self.expr_depth -= 1;
        r
    }

    /// An expression evaluated in context `ctx` that must have type `t`;
    /// `expectation` names the accepted types in the error.
    fn typed(&mut self, ctx: ECtx, t: Ty, expectation: &str) -> Result<Expr> {
        let saved = std::mem::replace(&mut self.ectx, ctx);
        let e = self.expr();
        self.ectx = saved;
        let e = e?;
        self.want(&e, t, expectation)?;
        Ok(e)
    }

    /// The value of a `make`/`convert` property. When it fails, C++ adds a
    /// second error for the property (one message per line here).
    fn rhs(&mut self, ctx: ECtx, t: Ty, expectation: &str) -> Result<Expr> {
        self.typed(ctx, t, expectation).map_err(|e| anyhow::anyhow!("{e}\nline {}: parse error: Property assignment needs a right-hand-side value", self.line()))
    }

    /// C++ static check of an evaluator's type.
    fn want(&self, e: &Expr, t: Ty, expectation: &str) -> Result<()> {
        let got = ty_of(e);
        if got != t {
            return self.static_err(self.line(), format!("Evaluator for {expectation} expected, but function \"{}\" is an evaluator for {}.", eval_name(e), got.name()));
        }
        Ok(())
    }

    /// Arguments parsed in another context.
    fn args_in(&mut self, ctx: ECtx) -> Result<Vec<Expr>> {
        let saved = std::mem::replace(&mut self.ectx, ctx);
        let r = self.args();
        self.ectx = saved;
        r
    }

    /// C++ check that a function can evaluate an element or member here.
    fn check_context(&self, name: &str) -> Result<()> {
        if MEMBER_FUNCS.contains(&name) && self.ectx != ECtx::Member {
            return self.err(format!("{name}(...) must be called in a context where it can evaluate an element's member"));
        }
        if ELEM_FUNCS.contains(&name) && self.ectx == ECtx::Set {
            let shown = if name == "t" { "" } else { name };
            return self.err(format!("{shown}(...) must be called in a context where it can evaluate an element"));
        }
        Ok(())
    }

    fn expr_inner(&mut self) -> Result<Expr> {
        let mut c = self.or_expr()?;
        while self.eat_p("?") {
            let a = self.or_expr()?;
            if !self.eat_p(":") {
                return self.err("Operator \"?\" must have a \":\" operator as right-hand-side argument");
            }
            let b = self.or_expr()?;
            self.want(&c, Ty::Str, "string")?;
            let t = ty_of(&a);
            self.want(&b, t, t.name())?;
            c = Expr::Ternary(Box::new(c), Box::new(a), Box::new(b));
        }
        Ok(c)
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut l = self.and_expr()?;
        while self.eat_p("||") {
            let r = self.and_expr()?;
            l = self.binary(BinOp::Or, l, r)?;
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut l = self.eq_expr()?;
        while self.eat_p("&&") {
            let r = self.eq_expr()?;
            l = self.binary(BinOp::And, l, r)?;
        }
        Ok(l)
    }

    fn eq_expr(&mut self) -> Result<Expr> {
        let mut l = self.rel_expr()?;
        loop {
            let op = if self.eat_p("==") {
                BinOp::Eq
            } else if self.eat_p("!=") {
                BinOp::Ne
            } else {
                break;
            };
            let r = self.rel_expr()?;
            l = self.binary(op, l, r)?;
        }
        Ok(l)
    }

    fn rel_expr(&mut self) -> Result<Expr> {
        let mut l = self.add_expr()?;
        loop {
            let op = if self.eat_p("<=") {
                BinOp::Le
            } else if self.eat_p(">=") {
                BinOp::Ge
            } else if self.eat_p("<") {
                BinOp::Lt
            } else if self.eat_p(">") {
                BinOp::Gt
            } else {
                break;
            };
            let r = self.add_expr()?;
            l = self.binary(op, l, r)?;
        }
        Ok(l)
    }

    /// Prefix minus binds like binary `+`/`-`: `-a*b` = `-(a*b)`, `-a+b` = `(-a)+b`.
    fn add_expr(&mut self) -> Result<Expr> {
        let mut l = if self.eat_p("-") {
            let e = self.add_operand()?;
            self.unary(UnOp::Neg, e)?
        } else {
            self.mul_expr()?
        };
        loop {
            let op = if self.eat_p("+") {
                BinOp::Add
            } else if self.eat_p("-") {
                BinOp::Sub
            } else if let Tok::Num(n) = self.peek().clone() {
                // `3-5` lexes as `3` `-5`: split back into a subtraction.
                if let Some(rest) = n.strip_prefix('-') {
                    self.toks[self.pos].tok = Tok::Num(rest.to_string());
                    BinOp::Sub
                } else {
                    break;
                }
            } else {
                break;
            };
            let r = self.mul_expr()?;
            l = self.binary(op, l, r)?;
        }
        Ok(l)
    }

    /// Operators take strings.
    fn binary(&self, op: BinOp, l: Expr, r: Expr) -> Result<Expr> {
        self.want(&l, Ty::Str, "string")?;
        self.want(&r, Ty::Str, "string")?;
        Ok(Expr::Binary(op, Box::new(l), Box::new(r)))
    }

    fn unary(&self, op: UnOp, e: Expr) -> Result<Expr> {
        self.want(&e, Ty::Str, "string")?;
        Ok(Expr::Unary(op, Box::new(e)))
    }

    fn add_operand(&mut self) -> Result<Expr> {
        let mut negs = 0;
        while self.eat_p("-") {
            negs += 1;
            if negs > MAX_EXPR_DEPTH {
                return self.err(format!("Expressions are limited to {MAX_EXPR_DEPTH} nested levels"));
            }
        }
        let mut e = self.mul_expr()?;
        for _ in 0..negs {
            e = self.unary(UnOp::Neg, e)?;
        }
        Ok(e)
    }

    fn mul_expr(&mut self) -> Result<Expr> {
        let mut l = self.unary_expr()?;
        loop {
            let op = if self.eat_p("*") {
                BinOp::Mul
            } else if self.eat_p("/") {
                BinOp::Div
            } else if self.is_p("%") {
                return self.err("\"%\" cannot be used as binary operator");
            } else {
                break;
            };
            let r = self.unary_expr()?;
            l = self.binary(op, l, r)?;
        }
        Ok(l)
    }

    fn unary_expr(&mut self) -> Result<Expr> {
        if self.eat_p("!") {
            // `!!x` is a C++ parse error; `!a.f()` would bind `!` to the set
            // name in C++, which is never what is meant.
            if self.is_p("!") {
                return self.err("\"!\" cannot be used as binary operator");
            }
            if matches!(self.peek(), Tok::Word(_)) && matches!(self.peek_at(1), Tok::P(".")) {
                return self.err("Put parentheses around a set function after \"!\", e.g. !(a.count(nodes))");
            }
            let e = self.primary()?;
            return self.unary(UnOp::Not, e);
        }
        self.primary()
    }

    fn args(&mut self) -> Result<Vec<Expr>> {
        self.expect_p("(")?;
        let mut args = Vec::new();
        if self.eat_p(")") {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            if !self.eat_p(",") {
                break;
            }
        }
        self.expect_p(")")?;
        Ok(args)
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Tok::Str(s) => {
                self.next();
                Ok(Expr::Lit(s))
            }
            Tok::Num(n) => {
                self.next();
                Ok(Expr::Lit(n))
            }
            Tok::P("(") => {
                self.next();
                if self.is_p(")") {
                    return self.static_err(self.line(), "Empty parentheses cannot be evaluated.");
                }
                let e = self.expr()?;
                self.expect_p(")")?;
                Ok(e)
            }
            Tok::P("::") => {
                self.next();
                if let Tok::Word(w) = self.peek() {
                    let w = w.clone();
                    return self.err(format!("\"::\" cannot be used as unary operator; use {w}() instead of ::{w}"));
                }
                Ok(Expr::Generic)
            }
            Tok::Word(w) => {
                self.next();
                if w == "t" && self.is_p("[") {
                    self.check_context("t")?;
                    self.next();
                    let k = self.expr()?;
                    self.want(&k, Ty::Str, "string")?;
                    self.expect_p("]")?;
                    return Ok(match k {
                        Expr::Lit(s) => Expr::Tag(s),
                        other => Expr::Call("t".into(), vec![other]),
                    });
                }
                if self.is_p("(") {
                    if AGGREGATORS.contains(&w.as_str()) {
                        let args = self.agg_args(&w)?;
                        return Ok(Expr::Agg { set: None, name: w, args });
                    }
                    self.check_context(&w)?;
                    if matches!(w.as_str(), "is_tag" | "count_members" | "count_distinct_members" | "count_by_role" | "count_distinct_by_role") {
                        // Literal arguments; bare words are allowed.
                        self.expect_p("(")?;
                        let mut args = Vec::new();
                        while !self.is_p(")") {
                            args.push(Expr::Lit(self.text("Value")?));
                            if !self.eat_p(",") {
                                break;
                            }
                        }
                        self.expect_p(")")?;
                        if args.is_empty() && matches!(w.as_str(), "count_by_role" | "count_distinct_by_role") {
                            return self.err(format!("{w}() must have the name of the wanted role as argument"));
                        }
                        return Ok(Expr::Call(w, args));
                    }
                    let line = self.line();
                    let member = matches!(w.as_str(), "per_member" | "per_vertex");
                    let args = if member { self.args_in(ECtx::Member)? } else { self.args()? };
                    check_call(line, &w, args.len())?;
                    let arg_ty = if matches!(w.as_str(), "center" | "trace" | "hull" | "lstr" | "poly") { Ty::Geom } else { Ty::Str };
                    for a in &args {
                        self.want(a, arg_ty, arg_ty.name())?;
                    }
                    return Ok(Expr::Call(w, args));
                }
                if self.is_p(".") && matches!(self.peek_at(1), Tok::Word(_)) {
                    self.next();
                    let name = self.ident("Keyword")?;
                    if self.is_p("(") {
                        if !AGGREGATORS.contains(&name.as_str()) {
                            return self.static_err(self.line(), format!("{name}(...) cannot have an input set"));
                        }
                        let args = self.agg_args(&name)?;
                        let set = if w == "_" { None } else { Some(w) };
                        return Ok(Expr::Agg { set, name, args });
                    }
                    return Ok(Expr::SetProp { set: w, prop: name });
                }
                bail!("line {}: parse error: Put quotation marks around \"{w}\" if it should be a constant and not a tag evaluation.", self.line())
            }
            // `.a.count(nodes)`: C++ reads it as a set that never exists (count 0,
            // keys empty), whatever `a` holds. The dot is a mistake for `a.count(nodes)`.
            Tok::P(".") if matches!((self.peek_at(1), self.peek_at(2)), (Tok::Word(_), Tok::P("."))) => {
                let Tok::Word(name) = self.peek_at(1).clone() else { unreachable!() };
                let prop = match (self.peek_at(3), self.peek_at(4)) {
                    (Tok::Word(w), Tok::P("(")) => format!("{w}(...)"),
                    (Tok::Word(w), _) => w.clone(),
                    _ => String::new(),
                };
                self.err(format!("Write {name}.{prop} instead of .{name}.{prop}; inside an expression a set name comes without the dot."))
            }
            _ => self.err(format!("Operator expected, but \"{}\"found.", self.found())),
        }
    }

    /// Aggregator arguments; `count(nodes)` takes a bare type word.
    fn agg_args(&mut self, name: &str) -> Result<Vec<Expr>> {
        if name == "count" {
            self.expect_p("(")?;
            let w = self.ident("Keyword")?;
            if !matches!(w.as_str(), "nodes" | "ways" | "relations" | "deriveds" | "nwr" | "nw" | "wr" | "nr") {
                // C++ reports this one as a parse error.
                bail!("line {}: parse error: \"count\" accepts only one of \"nodes\", \"ways\", \"relations\", \"deriveds\", \"nwr\", \"nw\", \"wr\", or \"nr\" as type argument.", self.line());
            }
            self.expect_p(")")?;
            return Ok(vec![Expr::Lit(w)]);
        }
        let args = self.args_in(ECtx::Elem)?;
        if args.is_empty() {
            return self.static_err(self.line(), format!("{name}(...) needs an argument"));
        }
        let t = if name == "gcat" { Ty::Geom } else { Ty::Str };
        for a in &args {
            self.want(a, t, t.name())?;
        }
        Ok(args)
    }
}

/// C `atoll`: leading whitespace, optional sign, digits; 0 if none.
fn atoll(s: &str) -> i64 {
    let t = s.trim_start();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let mut v: i64 = 0;
    for c in digits.bytes().take_while(|c| c.is_ascii_digit()) {
        v = v.saturating_mul(10).saturating_add((c - b'0') as i64);
    }
    if neg { -v } else { v }
}

/// Function names and arities as the C++ evaluator factories check them.
fn check_call(line: usize, name: &str, n: usize) -> Result<()> {
    let need = match name {
        "number" | "is_number" | "suffix" | "abs" | "date" | "is_date" | "lrs_min" | "lrs_max" | "center" | "trace" | "hull" | "per_member" | "per_vertex" => 1,
        "pt" | "lrs_in" | "lrs_isect" | "lrs_union" => 2,
        "lstr" | "poly" | "id" | "type" | "version" | "timestamp" | "changeset" | "uid" | "user" | "keys" | "lat" | "lon" | "is_closed" | "length" | "geom" | "count_tags" | "pos" | "mtype" | "ref" | "role" | "angle" => 0,
        _ => bail!("line {line}: parse error: Function \"{name}\" not known"),
    };
    if n < need {
        match (name, need) {
            ("per_member" | "per_vertex", _) => bail!("line {line}: parse error: {name}(...) must have one or more arguments"),
            (_, 2) => bail!("line {line}: parse error: {name}(...) needs two arguments"),
            _ => bail!("line {line}: parse error: {name}(...) needs an argument"),
        }
    }
    if need > 0 && n > need {
        bail!("line {line}: parse error: \",\" cannot be used as binary operator");
    }
    Ok(())
}

fn check_regex(line: usize, re: &str) -> Result<()> {
    if crate::exec::filter::compile_regex(re, false).is_err() {
        bail!("line {line}: static error: Invalid regular expression: \"{re}\"");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_query() {
        let s = parse("[out:json][timeout:25];node[amenity=cafe](50.6,7.0,50.8,7.3)->.a;.a out body;").unwrap();
        assert_eq!(s.settings.out, OutFormat::Json);
        assert_eq!(s.settings.timeout, 25);
        assert_eq!(s.stmts.len(), 2);
        match &s.stmts[0].kind {
            StmtKind::Query { types, filters } => {
                assert!(types.node);
                assert_eq!(filters[0], Filter::Eq("amenity".into(), "cafe".into()));
                assert!(matches!(filters[1], Filter::BBox(_)));
            }
            _ => panic!(),
        }
        assert_eq!(s.stmts[0].into, "a");
    }

    #[test]
    fn parses_union_difference_recurse() {
        let s = parse("(way(1); - node(2););(node(1);way(2););>;._;<<->.x;").unwrap();
        assert!(matches!(s.stmts[0].kind, StmtKind::Difference(..)));
        assert!(matches!(s.stmts[1].kind, StmtKind::Union(_)));
        assert!(matches!(s.stmts[2].kind, StmtKind::Recurse { op: RecurseOp::Down, .. }));
        assert_eq!(s.stmts[4].into, "x");
    }

    #[test]
    fn parses_expressions() {
        let s = parse("node(1)(if: t[\"a\"] == \"b\" && number(t[\"c\"]) > 3);make x n=count(nodes), v=u(t[\"k\"]), w=a.u(id());").unwrap();
        assert_eq!(s.stmts.len(), 2);
        let s = parse("make x v=1?2:3?4:5;").unwrap();
        match &s.stmts[0].kind {
            StmtKind::Make { entries, .. } => assert!(matches!(entries[0].value, Some(Expr::Ternary(ref c, _, _)) if matches!(**c, Expr::Ternary(..)))),
            _ => panic!(),
        }
    }

    #[test]
    fn set_name_with_a_dot_in_an_expression() {
        for (q, want) in [
            ("make x n=.a.count(nodes);", "line 1: parse error: Write a.count(...) instead of .a.count(...);"),
            ("node(1)->.c;\nif (.c.count(deriveds) == 0) { make x n=1; }", "line 2: parse error: Write c.count(...) instead of .c.count(...);"),
            ("make x n=.a.val;", "line 1: parse error: Write a.val instead of .a.val;"),
        ] {
            let e = parse(q).unwrap_err().to_string();
            assert!(e.starts_with(want), "{q}: {e}");
        }
        // without the second dot the old message stands
        assert!(parse("make x n=.a;").unwrap_err().to_string().contains("Operator expected, but \".\"found."));
        assert!(parse("make x n=a.count(nodes);").is_ok());
    }

    #[test]
    fn regex_escapes_decode_like_cpp() {
        let s = parse(r#"node["name"~"^\\d+$",i];"#).unwrap();
        match &s.stmts[0].kind {
            StmtKind::Query { filters, .. } => assert_eq!(filters[0], Filter::Regex { key: "name".into(), re: r"^\d+$".into(), ci: true, neg: false }),
            _ => panic!(),
        }
    }

    #[test]
    fn quirks() {
        assert!(parse("node;").is_err());
        assert!(parse("[bbox:1,2,3,4];node;").is_ok());
        let s = parse("node[a=\"\"][b];").unwrap();
        match &s.stmts[0].kind {
            StmtKind::Query { filters, .. } => assert_eq!(filters.len(), 1),
            _ => panic!(),
        }
        assert_eq!(parse("[timeout:010];node(1);").unwrap().settings.timeout, 8);
        assert_eq!(parse("[maxsize:2Mi];node(1);").unwrap().settings.maxsize, 2 << 20);
        // An empty XML script is valid, as in C++ (an empty result).
        assert!(parse("<osm-script/>").unwrap().stmts.is_empty());
        let s = parse("node[a!~b];").unwrap();
        match &s.stmts[0].kind {
            StmtKind::Query { filters, .. } => assert!(matches!(filters[0], Filter::Regex { neg: true, .. })),
            _ => panic!(),
        }
    }
}
