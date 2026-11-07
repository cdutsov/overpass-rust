//! Syntax tree of Overpass QL.

use crate::geo::BBox;
use crate::model::Id;

#[derive(Clone, Debug, PartialEq)]
pub struct Script {
    pub settings: Settings,
    pub stmts: Vec<Stmt>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OutFormat {
    Xml,
    Json,
    Csv(CsvSpec),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CsvSpec {
    pub fields: Vec<String>,
    pub header: bool,
    pub separator: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub out: OutFormat,
    pub timeout: u32,
    pub maxsize: u64,
    pub bbox: Option<BBox>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { out: OutFormat::Xml, timeout: 180, maxsize: 512 * 1024 * 1024, bbox: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    /// Output set name (`->.x`), `_` by default.
    pub into: String,
    pub line: usize,
}

/// Which element types a query statement selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Types {
    pub node: bool,
    pub way: bool,
    pub relation: bool,
    pub area: bool,
    pub derived: bool,
}

impl Types {
    pub fn parse(s: &str) -> Option<Types> {
        let mut t = Types::default();
        match s {
            "node" => t.node = true,
            "way" => t.way = true,
            "rel" | "relation" => t.relation = true,
            "area" => t.area = true,
            "derived" => t.derived = true,
            "nwr" => {
                t.node = true;
                t.way = true;
                t.relation = true;
            }
            "nw" => {
                t.node = true;
                t.way = true;
            }
            "wr" => {
                t.way = true;
                t.relation = true;
            }
            "nr" => {
                t.node = true;
                t.relation = true;
            }
            _ => return None,
        }
        Some(t)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecurseOp {
    /// `<`: ways and relations using the input
    Up,
    /// `<<`: transitive closure upwards
    UpRel,
    /// `>`: members of the input
    Down,
    /// `>>`: transitive closure downwards
    DownRel,
}

/// Recurse filters inside a query: `(w)`, `(r)`, `(bn)`, ...
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecurseFilter {
    /// `(w)`: nodes of ways
    W,
    /// `(way_cnt:l-u)`: nodes that belong to between l and u of the ways
    WayCnt,
    /// `(way_link:l-u)`: nodes with between l and u way segments attached
    WayLink,
    /// `(r)`: relation members
    R,
    /// `(bn)`: backwards from nodes
    Bn,
    /// `(bw)`: backwards from ways
    Bw,
    /// `(br)`: backwards from relations
    Br,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Filter {
    /// `[k]`
    HasKey(String),
    /// `[!k]`
    NotKey(String),
    /// `[k=v]`
    Eq(String, String),
    /// `[k!=v]`
    Ne(String, String),
    /// `[k~v]`, `[k~v,i]`
    Regex { key: String, re: String, ci: bool, neg: bool },
    /// `[~k~v]`
    KeyRegex { key: String, value: String, ci: bool },
    BBox(BBox),
    Ids(Vec<Id>),
    Around { input: Option<String>, radius: f64, points: Vec<(f64, f64)> },
    Poly(Vec<(f64, f64)>),
    Newer(i64),
    User(Vec<String>),
    Uid(Vec<u32>),
    /// `(area)`, `(area.s)`, `(area:id)`
    Area { set: String, id: Option<Id> },
    Pivot(String),
    /// `(<)`, `(<<)`, `(>)`, `(>>)` on the default set
    RecurseOp(RecurseOp),
    If(Expr),
    /// `(w:1,-1)` and `way(bn:2)` carry vertex positions instead of a role.
    /// `way_cnt`/`way_link` carry the inclusive range in `count`.
    Recurse { kind: RecurseFilter, input: String, role: Option<String>, positions: Option<Vec<i64>>, count: Option<(u32, u32)> },
    /// `.a` input set
    Input(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutOpts {
    pub mode: OutMode,
    pub geom: GeomMode,
    pub order: Order,
    pub limit: Option<u64>,
    pub geom_bbox: Option<BBox>,
    pub count: bool,
    pub noids: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutMode {
    Ids,
    Skel,
    Body,
    Tags,
    Meta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeomMode {
    None,
    Geom,
    Bb,
    Center,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Id,
    Quadtile,
}

impl Default for OutOpts {
    fn default() -> Self {
        OutOpts { mode: OutMode::Body, geom: GeomMode::None, order: Order::Id, limit: None, geom_bbox: None, count: false, noids: false }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MakeEntry {
    /// Tag key; `None` together with `wildcard` for `::=`.
    pub key: MakeKey,
    pub value: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MakeKey {
    Tag(String),
    /// `!key`: remove a tag (convert only)
    Remove(String),
    /// `::id`
    Id,
    /// `::geom`
    Geom,
    /// `:: = e` (convert) or `S:: = e`: one tag per key of the source,
    /// with `::` in `e` standing for the current key's value
    Generic { from: Option<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    Query { types: Types, filters: Vec<Filter> },
    Union(Vec<Stmt>),
    Difference(Box<Stmt>, Box<Stmt>),
    Recurse { op: RecurseOp, input: String },
    Item { set: String },
    Out { input: String, opts: OutOpts },
    Foreach { input: String, var: Option<String>, body: Vec<Stmt> },
    For { input: String, var: Option<String>, key: Expr, body: Vec<Stmt> },
    If { cond: Expr, then: Vec<Stmt>, els: Vec<Stmt> },
    Complete { input: String, var: Option<String>, limit: Option<u64>, body: Vec<Stmt> },
    IsIn { input: String, coord: Option<(f64, f64)> },
    MapToArea { input: String },
    Make { name: String, entries: Vec<MakeEntry> },
    Convert { input: String, name: String, entries: Vec<MakeEntry> },
    /// Statements reserved for later phases; executing them fails.
    Unsupported(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Lit(String),
    /// `t["k"]` or bare tag access in make/convert context
    Tag(String),
    /// `::` inside a generic make/convert property: the current key's value
    Generic,
    /// Function call on the current element: `id()`, `is_tag("k")`, ...
    Call(String, Vec<Expr>),
    /// Aggregator over a set: `a.count(nodes)`, `u(t["k"])`, `set(...)`.
    Agg { set: Option<String>, name: String, args: Vec<Expr> },
    /// `_.val`: key stored on a set (written by `for`)
    SetProp { set: String, prop: String },
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
}
