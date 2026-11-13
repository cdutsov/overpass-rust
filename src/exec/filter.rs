//! Tag filters.

use crate::model::Tags;
use crate::ql::ast::Filter;
use anyhow::{Result, anyhow};
use regex::{Regex, RegexBuilder};
use regex_syntax::hir::{ClassUnicode, ClassUnicodeRange};

/// The Rust class (a bracket expression) of the POSIX class `[:name:]` as
/// glibc defines it in the C.UTF-8 locale: Unicode letters and digits count,
/// so `ü` is in `[:alnum:]`. Non-ASCII decimal digits are in `alpha`, not in
/// `digit`. With REG_ICASE glibc reads `upper` and `lower` as `alpha`.
/// None for an unknown name (glibc: invalid character class).
fn posix_class(name: &str, ci: bool) -> Option<&'static str> {
    const ALPHA: &str = r"[[\p{Alphabetic}\p{Nd}]--0-9]";
    Some(match name {
        "alnum" => r"[\p{Alphabetic}\p{Nd}]",
        "alpha" => ALPHA,
        "upper" | "lower" if ci => ALPHA,
        "upper" => r"[\p{Uppercase}\p{Lt}]",
        "lower" => r"[\p{Lowercase}\x{1C5}\x{1C8}\x{1CB}\x{1F2}]",
        "digit" => "[0-9]",
        "xdigit" => "[0-9A-Fa-f]",
        "space" => r"[\t-\r \x{1680}\x{2000}-\x{2006}\x{2008}-\x{200A}\x{2028}\x{2029}\x{205F}\x{3000}]",
        "blank" => r"[\t \x{1680}\x{2000}-\x{2006}\x{2008}-\x{200A}\x{205F}\x{3000}]",
        "cntrl" => r"[\x00-\x1F\x7F-\x9F\x{2028}\x{2029}]",
        "print" => r"[^\p{Cn}\p{Cc}\p{Zl}\p{Zp}]",
        "graph" => r"[[^\p{Cn}\p{Cc}\p{Zs}\p{Zl}\p{Zp}]\x{A0}\x{2007}\x{202F}]",
        "punct" => r"[[[^\p{Cn}\p{Cc}\p{Zs}\p{Zl}\p{Zp}]--[\p{Alphabetic}\p{Nd}]]\x{A0}\x{2007}\x{202F}]",
        _ => return None,
    })
}

/// `towupper` of glibc in C.UTF-8 (see `towupper.rs`).
fn glibc_upper(c: char) -> char {
    use super::towupper::TOWUPPER;
    match TOWUPPER.binary_search_by_key(&(c as u32), |&(a, _)| a) {
        Ok(i) => char::from_u32(TOWUPPER[i].1).unwrap_or(c),
        Err(_) => c,
    }
}

/// The characters that `c` matches under REG_ICASE: glibc compares
/// `towupper` of the text with `towupper` of the pattern, so `i` matches
/// `I` and the dotless `ı`, while `k` does not match the Kelvin sign.
fn ci_chars(c: char) -> Vec<char> {
    use super::towupper::TOWUPPER;
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static LOWER: OnceLock<HashMap<char, Vec<char>>> = OnceLock::new();
    let lower = LOWER.get_or_init(|| {
        let mut m: HashMap<char, Vec<char>> = HashMap::new();
        for &(a, b) in TOWUPPER {
            if let (Some(a), Some(b)) = (char::from_u32(a), char::from_u32(b)) {
                m.entry(b).or_default().push(a);
            }
        }
        m
    });
    let u = glibc_upper(c);
    let mut v: Vec<char> = lower.get(&u).cloned().unwrap_or_default();
    if glibc_upper(u) == u {
        v.push(u);
    }
    v.sort_unstable();
    v
}

/// The characters x with `towupper(x)` in `set`: what a bracket expression
/// matches under REG_ICASE, where glibc uppercases its characters and range
/// ends and tests `towupper` of the text against it (`[a-z]` matches `ſ`
/// and `ı`, `[A-z]` does not match `_`). Only the characters `towupper`
/// changes can move in or out.
fn ci_preimage(set: &ClassUnicode) -> ClassUnicode {
    use super::towupper::TOWUPPER;
    use std::sync::OnceLock;
    static CHANGED: OnceLock<ClassUnicode> = OnceLock::new();
    let changed = CHANGED.get_or_init(|| ClassUnicode::new(TOWUPPER.iter().filter_map(|&(a, _)| char::from_u32(a)).map(|c| ClassUnicodeRange::new(c, c))));
    let contains = |c: char| range_of(set, c).is_some();
    let mut m = set.clone();
    m.difference(changed);
    let add = ClassUnicode::new(
        TOWUPPER
            .iter()
            .filter_map(|&(a, b)| Some((char::from_u32(a)?, char::from_u32(b)?)))
            .filter(|&(_, b)| contains(b))
            .map(|(a, _)| ClassUnicodeRange::new(a, a)),
    );
    m.union(&add);
    m
}

/// The set of a Rust class such as `[a-c\p{Nd}]` (a single character too).
fn class_of(text: &str) -> Option<ClassUnicode> {
    use regex_syntax::hir::{Class, HirKind};
    let hir = regex_syntax::Parser::new().parse(text).ok()?;
    match hir.kind() {
        HirKind::Class(Class::Unicode(c)) => Some(c.clone()),
        HirKind::Class(Class::Bytes(b)) if b.ranges().is_empty() => Some(ClassUnicode::empty()),
        HirKind::Literal(l) => {
            let mut it = std::str::from_utf8(&l.0).ok()?.chars();
            let c = it.next()?;
            it.next().is_none().then(|| ClassUnicode::new([ClassUnicodeRange::new(c, c)]))
        }
        _ => None,
    }
}

/// A class as Rust syntax, every character as `\x{..}`.
fn class_text(set: &ClassUnicode) -> String {
    use std::fmt::Write;
    if set.ranges().is_empty() {
        return r"[^\x{0}-\x{10FFFF}]".into();
    }
    let mut s = String::from("[");
    for r in set.ranges() {
        let _ = write!(s, "\\x{{{:X}}}", r.start() as u32);
        if r.end() != r.start() {
            let _ = write!(s, "-\\x{{{:X}}}", r.end() as u32);
        }
    }
    s.push(']');
    s
}

/// `[...]` as `[^...]` when `neg`.
fn negated(neg: bool, class: &str) -> String {
    if neg { format!("[^{}", &class[1..]) } else { class.to_string() }
}

/// A class under REG_ICASE: `class` as written, `upper` with its characters
/// uppercased. The class as written when it already matches the same.
fn ci_class(class: &str, upper: &str, neg: bool) -> Option<String> {
    let mut m = ci_preimage(&class_of(upper)?);
    if class_of(class)? == m {
        return Some(negated(neg, class));
    }
    if neg {
        m.negate();
    }
    Some(class_text(&m))
}

/// A literal character; under REG_ICASE the class of the characters it
/// matches; with `words`, a word-difference character as its substitute.
fn literal(c: char, ci: bool, words: bool, out: &mut String) {
    let mut set = if ci { ci_chars(c) } else { vec![c] };
    if words {
        for x in set.iter_mut() {
            *x = word_sub(*x).unwrap_or(*x);
        }
    }
    if set.len() > 1 {
        out.push('[');
        for x in set {
            out.push_str(&regex::escape(x.encode_utf8(&mut [0u8; 4])));
        }
        out.push(']');
    } else {
        out.push_str(&regex::escape(set[0].encode_utf8(&mut [0u8; 4])));
    }
}

/// Characters the regex crate counts as word characters for `\b` and glibc
/// does not: marks that are not letters (U+0301, Thai tone marks, variation
/// selectors), connector punctuation other than `_`, ZWJ and ZWNJ. glibc
/// counts `[_[:alnum:]]`, the regex crate Unicode `\w`. With the start rank
/// of each range.
fn word_diff() -> &'static (ClassUnicode, Vec<u32>) {
    static D: std::sync::OnceLock<(ClassUnicode, Vec<u32>)> = std::sync::OnceLock::new();
    D.get_or_init(|| {
        let d = class_of(r"[[\p{M}\p{Pc}\p{Join_Control}]--[\p{Alphabetic}\p{Nd}_]]").expect("word difference class");
        let mut rank = Vec::with_capacity(d.ranges().len());
        let mut n = 0u32;
        for r in d.ranges() {
            rank.push(n);
            n += r.end() as u32 - r.start() as u32 + 1;
        }
        (d, rank)
    })
}

/// While a regex with word boundaries runs, each word-difference character
/// stands at an unassigned code point from U+40000 (plane 4), which the
/// regex crate counts as a non-word character.
const WORD_SUB: u32 = 0x40000;

/// The index of the range of `set` that holds `c`.
fn range_of(set: &ClassUnicode, c: char) -> Option<usize> {
    set.ranges()
        .binary_search_by(|r| {
            if r.end() < c {
                std::cmp::Ordering::Less
            } else if r.start() > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .ok()
}

/// The substitute of a word-difference character, None for the others.
fn word_sub(c: char) -> Option<char> {
    let (d, rank) = word_diff();
    let i = range_of(d, c)?;
    char::from_u32(WORD_SUB + rank[i] + (c as u32 - d.ranges()[i].start() as u32))
}

/// A class of a regex with word boundaries: its word-difference characters
/// at their substitutes, the substitutes of the others out.
fn word_class(set: &ClassUnicode) -> ClassUnicode {
    let (d, rank) = word_diff();
    let last = d.ranges().last().map_or(0, |r| rank[rank.len() - 1] + r.end() as u32 - r.start() as u32);
    let subs = ClassUnicode::new([ClassUnicodeRange::new(char::from_u32(WORD_SUB).unwrap(), char::from_u32(WORD_SUB + last).unwrap())]);
    let mut inside = set.clone();
    inside.intersect(d);
    let mut m = set.clone();
    m.difference(d);
    m.difference(&subs);
    m.union(&ClassUnicode::new(inside.ranges().iter().filter_map(|r| Some(ClassUnicodeRange::new(word_sub(r.start())?, word_sub(r.end())?)))));
    m
}

/// Append a class; with `words` through `word_class`.
fn put_class(out: &mut String, class: &str, words: bool) -> Option<()> {
    if words {
        out.push_str(&class_text(&word_class(&class_of(class)?)));
    } else {
        out.push_str(class);
    }
    Some(())
}

/// Translate the bracket expression that starts at `chars[*i]` and move
/// `*i` past it: whether it is negated, and the Rust class without the `^`.
/// Backslashes inside are literal. `upper`: characters and range ends
/// uppercased with `towupper`, as glibc reads the pattern under REG_ICASE.
fn bracket(chars: &[char], i: &mut usize, ci: bool, upper: bool) -> Option<(bool, String)> {
    let up = |c: char| if upper { glibc_upper(c) } else { c };
    let mut out = String::from("[");
    *i += 1;
    let neg = chars.get(*i) == Some(&'^');
    if neg {
        *i += 1;
    }
    if chars.get(*i) == Some(&']') {
        out.push_str("\\]");
        *i += 1;
    }
    loop {
        let &d = chars.get(*i)?;
        if d == ']' {
            out.push(']');
            *i += 1;
            break;
        }
        if d == '[' && matches!(chars.get(*i + 1), Some(':' | '.' | '=')) {
            // `[:alpha:]`, `[.x.]` or `[=x=]`: up to the closing ":]".
            let close = chars[*i + 1];
            let start = *i;
            *i += 2;
            while *i + 1 < chars.len() && !(chars[*i] == close && chars[*i + 1] == ']') {
                *i += 1;
            }
            if *i + 1 >= chars.len() {
                return None;
            }
            let inner = &chars[start + 2..*i];
            if close == ':' {
                let name: String = inner.iter().collect();
                out.push_str(posix_class(&name, ci)?);
            } else {
                // C.UTF-8 has single ASCII characters only: `[.-.]`, `[=a=]`.
                let [ch] = inner else { return None };
                if !ch.is_ascii() {
                    return None;
                }
                out.push_str(&regex::escape(up(*ch).encode_utf8(&mut [0u8; 4])));
            }
            *i += 2;
            continue;
        }
        if matches!(d, '\\' | '[' | '&' | '~' | '-') && !(d == '-' && (chars.get(*i + 1) != Some(&']') && out.ends_with(|ch: char| ch != '[' && ch != '^'))) {
            out.push('\\');
        }
        out.push(up(d));
        *i += 1;
    }
    Some((neg, out))
}

/// Translate a glibc POSIX extended regular expression into Rust syntax;
/// `ci`: the regex is case-insensitive (REG_ICASE).
///
/// GNU escapes `\w \W \s \S` match the glibc classes `[_[:alnum:]]` and
/// `[[:space:]]` and their complements, `\b \B` keep their meaning,
/// `\<`/`\>` are the start and the end of a word, `` \` ``/`\'` anchor at
/// text start/end; any other escaped character is literal (so `\d` is the
/// letter d). Inside a bracket expression a backslash is an ordinary
/// character. Perl extensions such as `(?i)` are not ERE and are rejected.
/// Under REG_ICASE every letter and class becomes the explicit set of the
/// characters glibc matches (`ci_chars`, `ci_preimage`): the Unicode case
/// folding of the regex crate differs for 24 characters. With word
/// boundaries the word-difference characters stand at their substitutes
/// (`word_diff`); `Ere::is_match` replaces them in the text too.
pub fn translate_ere(re: &str, ci: bool) -> Option<String> {
    translate(re, ci).map(|t| t.0)
}

/// The translation and whether it has word boundaries.
fn translate(re: &str, ci: bool) -> Option<(String, bool)> {
    let (t, words) = translate_with(re, ci, false)?;
    if words { translate_with(re, ci, true) } else { Some((t, false)) }
}

fn translate_with(re: &str, ci: bool, words: bool) -> Option<(String, bool)> {
    let chars: Vec<char> = re.chars().collect();
    let mut out = String::with_capacity(re.len() + 8);
    let mut i = 0;
    let mut bounds = false;
    let class = |class: String, neg: bool| if ci { ci_class(&class, &class, neg) } else { Some(negated(neg, &class)) };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                let Some(&n) = chars.get(i + 1) else { return None };
                match n {
                    'w' | 'W' => put_class(&mut out, &class(format!("[_{}]", posix_class("alnum", ci)?), n == 'W')?, words)?,
                    's' | 'S' => put_class(&mut out, &class(format!("[{}]", posix_class("space", ci)?), n == 'S')?, words)?,
                    'b' | 'B' | '<' | '>' => {
                        bounds = true;
                        out.push_str(match n {
                            'b' => "\\b",
                            'B' => "\\B",
                            '<' => "\\b{start}",
                            _ => "\\b{end}",
                        });
                    }
                    '`' => out.push_str("\\A"),
                    '\'' => out.push_str("\\z"),
                    '1'..='9' => return None,
                    other => literal(other, ci, words, &mut out),
                }
                i += 2;
            }
            '[' => {
                let start = i;
                let (neg, written) = bracket(&chars, &mut i, ci, false)?;
                let class = if ci {
                    let (_, upper) = bracket(&chars, &mut { start }, ci, true)?;
                    ci_class(&written, &upper, neg)?
                } else {
                    negated(neg, &written)
                };
                put_class(&mut out, &class, words)?;
            }
            '(' if chars.get(i + 1) == Some(&'?') => return None,
            // An interval: glibc reads `{,n}` as `{0,n}` and `{,}` as `{0,}`, and
            // refuses white space inside. Other braces go to the regex crate,
            // which refuses the same forms as glibc (`{}`, `{z}`, `{2`).
            '{' => {
                let close = chars[i + 1..].iter().position(|&d| d == '}').map(|p| i + 1 + p);
                match close {
                    Some(j) if chars[i + 1..j].iter().all(|d| d.is_ascii_digit() || *d == ',' || d.is_whitespace()) => {
                        let inner: String = chars[i + 1..j].iter().collect();
                        if inner.contains(char::is_whitespace) {
                            return None;
                        }
                        out.push('{');
                        if inner.starts_with(',') {
                            out.push('0');
                        }
                        out.push_str(&inner);
                        out.push('}');
                        i = j + 1;
                    }
                    _ => {
                        out.push('{');
                        i += 1;
                    }
                }
            }
            // Operators and other characters without case pairs as they are.
            _ if (ci && ci_chars(c) != [c]) || (words && word_sub(c).is_some()) => {
                literal(c, ci, words, &mut out);
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some((out, bounds))
}

/// A regex written for glibc POSIX ERE, compiled. With word boundaries, the
/// characters the regex crate counts as word characters and glibc does not
/// are replaced by their substitutes in the text before it is matched.
#[derive(Clone, Debug)]
pub struct Ere {
    re: Regex,
    words: bool,
}

impl Ere {
    pub fn is_match(&self, s: &str) -> bool {
        if self.words && !s.is_ascii() && s.chars().any(|c| word_sub(c).is_some()) {
            let t: String = s.chars().map(|c| word_sub(c).unwrap_or(c)).collect();
            return self.re.is_match(&t);
        }
        self.re.is_match(s)
    }

    /// The translated pattern.
    pub fn as_str(&self) -> &str {
        self.re.as_str()
    }
}

/// Compile a regex written for glibc POSIX ERE. Case-insensitive regexes
/// are translated into explicit classes (`translate_ere`), so the regex
/// crate itself compares case-sensitively.
///
/// Each Unicode class such as `[^[:alnum:]]` takes about 50 KB of the
/// program: client word searches with 60 of them need 3 MB, 32 MB holds
/// about 600. The lazy DFA needs a cache of 8 MB for them; with 2 MB it
/// falls back to a slower engine (31,000 names: 110 ms against 2 ms).
pub fn compile_regex(re: &str, ci: bool) -> Result<Ere> {
    let err = || anyhow!("Invalid regular expression: \"{re}\"");
    let (translated, words) = translate(re, ci).ok_or_else(err)?;
    let re = RegexBuilder::new(&translated)
        // POSIX `.` matches newlines.
        .dot_matches_new_line(true)
        .size_limit(32 << 20)
        .dfa_size_limit(8 << 20)
        .build()
        .map_err(|_| err())?;
    Ok(Ere { re, words })
}

/// Value prefixes up to this many bytes are always kept whole in the tag
/// index (see `store::index_part`).
const MAX_PREFIX: usize = 180;

/// Byte strings one of which every value matched by `re` (a translated
/// pattern, see `compile_regex`) starts with: for regexes anchored at the
/// start whose beginnings form a small set, e.g. `^Aldi` or
/// `^(primary|secondary)$`; None for the others. The tag index is then
/// read only at these prefixes.
pub fn anchored_prefixes(re: &str) -> Option<Vec<Vec<u8>>> {
    use regex_syntax::hir::Look;
    use regex_syntax::hir::literal::{ExtractKind, Extractor};
    let hir = regex_syntax::ParserBuilder::new().dot_matches_new_line(true).build().parse(re).ok()?;
    // `^` without multi-line mode: every match starts at the beginning of the value.
    if !hir.properties().look_set_prefix().contains(Look::Start) {
        return None;
    }
    let seq = Extractor::new().kind(ExtractKind::Prefix).extract(&hir);
    let mut lits: Vec<Vec<u8>> = seq.literals()?.iter().map(|l| l.as_bytes()[..l.as_bytes().len().min(MAX_PREFIX)].to_vec()).collect();
    // An empty prefix restricts nothing; NUL bytes are stored differently;
    // substitutes of word-difference characters (lead byte F1) are not in values.
    if lits.is_empty() || lits.iter().any(|l| l.is_empty() || l.contains(&0) || l.contains(&0xF1)) {
        return None;
    }
    lits.sort();
    lits.dedup();
    // A prefix that extends another one adds no values.
    let mut out: Vec<Vec<u8>> = Vec::new();
    for l in lits {
        if !out.last().is_some_and(|p| l.starts_with(p)) {
            out.push(l);
        }
    }
    Some(out)
}

/// Whether a literal set tells enough for a prefilter: at most 16
/// spellings, each at least 3 bytes. The dotless `ı`, which `,i` matches for
/// `i` as glibc does, counts as `i`: `mini` under `,i` has 36 spellings and
/// counts 16, as before that rule. Longer sets, such as most words of 5
/// letters and more under `,i`, give no prefilter.
fn literals_fit(v: &[Vec<u8>]) -> bool {
    if v.iter().any(|l| l.len() < 3) {
        return false;
    }
    let mut counted: Vec<Vec<u8>> = v
        .iter()
        .map(|l| {
            let mut out = Vec::with_capacity(l.len());
            let mut i = 0;
            while i < l.len() {
                if l[i..].starts_with("ı".as_bytes()) {
                    out.push(b'i');
                    i += 2;
                } else {
                    out.push(l[i]);
                    i += 1;
                }
            }
            out
        })
        .collect();
    counted.sort();
    counted.dedup();
    counted.len() <= 16
}

/// Byte strings one of which every value matched by `re` (a translated
/// pattern, see `compile_regex`) contains: the literals every match starts
/// or ends with, e.g. `Zürich` for `Zürich` or `(Bahnhof|Station)`. None
/// when they would not tell much (`literals_fit`).
pub fn required_literals(re: &str) -> Option<Vec<Vec<u8>>> {
    use regex_syntax::hir::literal::{ExtractKind, Extractor};
    let hir = regex_syntax::ParserBuilder::new().dot_matches_new_line(true).build().parse(re).ok()?;
    for kind in [ExtractKind::Prefix, ExtractKind::Suffix] {
        let seq = Extractor::new().kind(kind).extract(&hir);
        let Some(lits) = seq.literals() else { continue };
        // Substitutes of word-difference characters (lead byte F1) are not in values.
        if lits.is_empty() || lits.iter().any(|l| l.as_bytes().contains(&0xF1)) {
            continue;
        }
        let mut v: Vec<Vec<u8>> = lits.iter().map(|l| l.as_bytes().to_vec()).collect();
        v.sort();
        v.dedup();
        if literals_fit(&v) {
            return Some(v);
        }
    }
    None
}

/// A tag filter ready to match.
#[derive(Clone, Debug)]
pub enum TagFilter {
    HasKey(String),
    NotKey(String),
    Eq(String, String),
    Ne(String, String),
    /// `prefixes`: see `anchored_prefixes` (for the tag index);
    /// `literals`: see `required_literals`.
    Regex { key: String, re: Ere, neg: bool, prefixes: Option<Vec<Vec<u8>>>, literals: Option<Vec<Vec<u8>>> },
    KeyRegex { key: Ere, value: Ere, value_prefixes: Option<Vec<Vec<u8>>> },
}

fn get<'a>(tags: &'a Tags, key: &str) -> Option<&'a str> {
    crate::model::tag(tags, key)
}

impl TagFilter {
    pub fn from_ast(f: &Filter) -> Result<Option<TagFilter>> {
        Ok(Some(match f {
            Filter::HasKey(k) => TagFilter::HasKey(k.clone()),
            Filter::NotKey(k) => TagFilter::NotKey(k.clone()),
            Filter::Eq(k, v) => TagFilter::Eq(k.clone(), v.clone()),
            Filter::Ne(k, v) => TagFilter::Ne(k.clone(), v.clone()),
            Filter::Regex { key, re, ci, neg } => {
                let re = compile_regex(re, *ci)?;
                let prefixes = anchored_prefixes(re.as_str());
                let literals = required_literals(re.as_str());
                TagFilter::Regex { key: key.clone(), re, neg: *neg, prefixes, literals }
            }
            Filter::KeyRegex { key, value, ci } => {
                let value = compile_regex(value, *ci)?;
                let value_prefixes = anchored_prefixes(value.as_str());
                TagFilter::KeyRegex { key: compile_regex(key, *ci)?, value, value_prefixes }
            }
            _ => return Ok(None),
        }))
    }

    pub fn matches(&self, tags: &Tags) -> bool {
        match self {
            TagFilter::HasKey(k) => get(tags, k).is_some(),
            TagFilter::NotKey(k) => get(tags, k).is_none(),
            TagFilter::Eq(k, v) => get(tags, k) == Some(v.as_str()),
            TagFilter::Ne(k, v) => get(tags, k) != Some(v.as_str()),
            TagFilter::Regex { key, re, neg, .. } => {
                let m = get(tags, key).is_some_and(|v| re.is_match(v));
                m != *neg
            }
            TagFilter::KeyRegex { key, value, .. } => tags.iter().any(|(k, v)| key.is_match(k) && value.is_match(v)),
        }
    }

    /// Byte strings one of which the value of every element that passes
    /// the filter contains (at least 3 bytes each), or None.
    pub fn value_literals(&self) -> Option<Vec<&[u8]>> {
        match self {
            TagFilter::Eq(_, v) if v.len() >= 3 => Some(vec![v.as_bytes()]),
            TagFilter::Regex { neg: false, literals: Some(l), .. } => Some(l.iter().map(|l| l.as_slice()).collect()),
            _ => None,
        }
    }

    /// Whether the filter can generate candidates from the tag index.
    pub fn is_positive(&self) -> bool {
        matches!(self, TagFilter::HasKey(_) | TagFilter::Eq(..) | TagFilter::Regex { neg: false, .. } | TagFilter::KeyRegex { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(re: &str, s: &str) -> bool {
        compile_regex(re, false).unwrap().is_match(s)
    }

    fn mi(re: &str, s: &str) -> bool {
        compile_regex(re, true).unwrap().is_match(s)
    }

    /// Expected values from glibc `regexec` in C.UTF-8, as the C++ server runs it.
    #[test]
    fn posix_classes_as_glibc() {
        assert!(m("^[[:alnum:]]$", "ü"));
        assert!(m("^[[:alnum:]]$", "٣"));
        assert!(!m("^[[:alnum:]]$", "_"));
        assert!(m("^[[:alpha:]]$", "٣"));
        assert!(!m("^[[:digit:]]$", "٣"));
        assert!(m("^[[:alpha:]]$", "\u{e31}"));
        assert!(!m("^[[:alpha:]]$", "\u{301}"));
        // A word on its own: "Grüne" does not contain the word "ne".
        let word = "(^|[^[:alnum:]])ne([^[:alnum:]]|$)";
        assert!(!mi(word, "Grüne Aue"));
        assert!(!mi(word, "Domäne"));
        assert!(mi(word, "so 'ne Art Salon"));
        assert!(m("^[[:upper:]]$", "ǅ") && m("^[[:lower:]]$", "ǅ"));
        assert!(m("^[[:upper:]]$", "Ⅳ"));
        assert!(!m("^[[:upper:]]$", "a"));
        // REG_ICASE reads upper and lower as alpha.
        assert!(mi("^[[:upper:]]$", "a") && mi("^[[:upper:]]$", "ß") && mi("^[[:lower:]]$", "ª"));
        assert!(!mi("^[^[:upper:]]$", "a"));
        assert!(mi("^[[:upper:][:digit:]]+$", "a1"));
        assert!(!m("^[[:space:]]$", "\u{a0}") && m("^[[:space:]]$", "\u{2003}"));
        assert!(m("^[^[:space:]]$", "\u{85}") && m("^[[:cntrl:]]$", "\u{85}"));
        assert!(m("^[[:cntrl:]]$", "\u{2028}") && m("^[[:blank:]]$", "\u{3000}"));
        assert!(m("^[[:punct:]]$", "€") && m("^[[:punct:]]$", "\u{a0}"));
        assert!(m("^[[:graph:]]$", "\u{a0}") && m("^[[:print:]]$", "\u{a0}"));
        assert!(!m("^[[:xdigit:]]+$", "Ａ"));
        assert!(m("^[[:alpha:]-]+$", "a-b"));
        assert!(compile_regex("^[[:foo:]]$", false).is_err());
        assert!(compile_regex("[[:alpha:]", false).is_err());
        // GNU escapes: \w is [_[:alnum:]], \s is [[:space:]].
        assert!(m("^\\w$", "ü") && m("^\\w$", "_") && mi("^\\w$", "Ü"));
        assert!(!m("^\\w$", "\u{301}") && !m("^\\w$", "‿") && m("^\\W$", "\u{301}"));
        assert!(!m("^\\s$", "\u{a0}") && m("^\\S$", "\u{a0}"));
        // Collating elements and equivalence classes: one ASCII character.
        assert!(m("^[[=a=]]$", "a") && !m("^[[=a=]]$", "á") && mi("^[[=a=]]$", "A"));
        assert!(m("^[[.-.]a]$", "-") && m("^[[.].]]$", "]") && m("^[[=[=]]$", "["));
        assert!(compile_regex("[[=ab=]]", false).is_err());
        assert!(compile_regex("[[.ü.]]", false).is_err());
        // Many Unicode classes stay within the size limits.
        let many = "([^[:alnum:]]*x[^[:alpha:]]*y[^[:alnum:]]*){8}";
        assert!(compile_regex(many, true).is_ok());
        // A client word search with 120 classes (6 MB), as glibc compiles it.
        let words: Vec<String> = (0..60).map(|k| format!("(^|[^[:alnum:]])word{k}([^[:alnum:]]|$)")).collect();
        let words = format!("({})", words.join("|"));
        for ci in [false, true] {
            let r = compile_regex(&words, ci).unwrap();
            assert!(r.is_match("a word42") && !r.is_match("a word42x") && !r.is_match("aword7"));
        }
        assert!(mi(&words, "WORD59 Bar"));
        // An expression too large for 32 MB is refused, not compiled.
        assert!(compile_regex("([^[:alnum:]]x){1000}", false).is_err());
    }

    /// REG_ICASE as glibc `regexec` in C.UTF-8 answers it: `towupper` of the
    /// text against the uppercased pattern, also in bracket expressions.
    #[test]
    fn case_insensitive_as_glibc() {
        // The Turkish dotless i: towupper gives I.
        assert!(mi("^diyarbakir$", "Diyarbakır") && mi("^DIYARBAKIR$", "Diyarbakır"));
        assert!(mi("^ı$", "I") && mi("^ı$", "i") && mi("^I$", "ı"));
        // The dotted capital İ stays itself, so it matches only itself.
        assert!(!mi("^i$", "İ") && mi("^İ$", "İ"));
        // Signs and capitals that only Unicode case folding pairs.
        assert!(!mi("^k$", "\u{212a}") && !mi("^\u{212a}$", "K") && mi("^k$", "K"));
        assert!(!mi("^ω$", "\u{2126}") && !mi("^å$", "\u{212b}") && !mi("^θ$", "\u{3f4}"));
        assert!(!mi("^ß$", "ẞ") && !mi("^ẞ$", "ß") && mi("^ß$", "ß"));
        // Pairs both have.
        assert!(mi("^σ$", "ς") && mi("^ς$", "Σ") && mi("^µ$", "Μ") && mi("^ǅ$", "ǆ") && mi("^s$", "ſ"));
        assert!(mi("^école$", "ÉCOLE") && mi("^москва$", "МОСКВА") && mi("^ϑ$", "θ"));
        // Bracket expressions: characters and range ends uppercased.
        assert!(mi("^[a-z]$", "ſ") && mi("^[a-z]$", "ı") && mi("^[A-Z]$", "a") && !mi("^[a-z]$", "\u{212a}"));
        assert!(mi("^[A-z]$", "q") && !mi("^[A-z]$", "_") && !mi("^[A-z]$", "["));
        assert!(!mi("^[^A-Z]$", "ı") && mi("^[^A-Z]$", "\u{212a}") && !mi("^[^a]$", "A"));
        assert!(mi("^[ik]$", "ı") && mi("^[IK]$", "k") && !mi("^[ß]$", "ẞ") && mi("^[é]$", "É"));
        assert!(mi("^[0-9a-f]+$", "A1f") && !mi("^[0-9a-f]$", "g"));
        assert!(mi("^\\w+$", "Diyarbakır_1") && !mi("^\\W$", "_") && mi("^\\W$", " "));
        assert!(mi("^[[:lower:]_]$", "_") && !mi("^[^[:upper:]]$", "ı"));
        // Operators keep their meaning; escaped letters are literal letters.
        assert!(mi("^(ab|cd)+$", "ABcD") && mi("^a{2}$", "aA") && mi("^\\d$", "D"));
        // The tag index reads the case variants as prefixes.
        assert_eq!(pre("^ki", true), Some(vec!["KI".into(), "Ki".into(), "Kı".into(), "kI".into(), "ki".into(), "kı".into()]));
    }

    /// Word boundaries as glibc `regexec` answers them: word characters are
    /// `[_[:alnum:]]`, so a combining mark, ZWNJ, ZWJ, a Thai tone mark and
    /// connector punctuation other than `_` end a word.
    #[test]
    fn word_boundaries_as_glibc() {
        assert!(m("Cafe\\b", "Cafe\u{301}") && !m("Cafe\\B", "Cafe\u{301}") && m("\\<Cafe\\>", "Cafe\u{301}"));
        assert!(m("\\bکتاب\\b", "کتاب\u{200c}ها") && m("x\\b", "x\u{203f}y") && m("x\\b", "x\u{200d}y"));
        assert!(m("น\\b", "น\u{e49}ำ") && !m("\\B", "x\u{203f}y") && m("\\B", "\u{301}"));
        assert!(m("\\b\u{301}", "e\u{301}") && !m("\u{301}\\b", "e\u{301}") && m("\u{301}\\b", "b\u{301}c"));
        assert!(m("[\u{301}]\\b", "b\u{301}c") && !m("e.\\b", "e\u{301}") && !m("\\b[^a]\\b", "🍺\u{fe0f} Bar"));
        // \< is the start and \> the end of a word, not any boundary.
        assert!(!m("a\\<", "a b") && !m("\\>a", "a b") && m("\\<b", "a b") && m("b\\>", "b\u{301}c"));
        assert!(mi("\\bCAFE\\b", "cafe\u{301}") && mi("\\bDIYARBAKIR\\b", "Diyarbakır\u{200c}"));
        // Without boundaries the text is matched as it is.
        assert!(m("^e\u{301}$", "e\u{301}") && m("^[^[:alnum:]]$", "\u{301}") && m("^.$", "\u{200c}"));
        // The tag index prefixes leave out the substitutes.
        assert_eq!(pre("^Cafe\u{301}\\b", false), None);
        assert_eq!(pre("^Cafe\\b", false), Some(vec!["Cafe".to_string()]));
    }

    #[test]
    fn posix_ere_semantics() {
        assert!(m("\\d", "d"));
        assert!(!m("\\d", "5"));
        assert!(m("a.b", "a\nb"));
        assert!(m("[\\.]", "\\"));
        assert!(m("[\\.]", "."));
        assert!(m("[[:digit:]]+", "42"));
        assert!(m("^\\<ab\\>$", "ab"));
        assert!(m("a-b", "a-b"));
        assert!(m("[a-c]", "b"));
        assert!(m("[]x]", "]"));
        assert!(compile_regex("(?i)x", false).is_err());
        assert!(m("\\.", "."));
        assert!(!m("\\.", "x"));
        // Intervals as glibc reads them.
        assert!(m("^a{,2}$", "") && m("^a{,2}$", "aa") && !m("^a{,2}$", "aaa") && m("^(ab){,2}$", "abab"));
        assert!(m("^a{,}$", "aaa") && m("^a{,0}$", "") && !m("^a{,0}$", "a") && m("^a{,2}b$", "aab"));
        assert!(m("^x{1}{2}$", "xx") && mi("^A{,2}$", "aA"));
        for bad in ["^a{ 2}$", "^a{2 }$", "^a{}$", "^a{2$", "^{$", "{z}", "^a{x}$", "^a{2,1}$", "^a{,2"] {
            assert!(compile_regex(bad, false).is_err(), "{bad}");
        }
    }

    fn pre(re: &str, ci: bool) -> Option<Vec<String>> {
        let r = compile_regex(re, ci).unwrap();
        anchored_prefixes(r.as_str()).map(|v| v.into_iter().map(|b| String::from_utf8(b).unwrap()).collect())
    }

    fn lits(re: &str, ci: bool) -> Option<Vec<String>> {
        let r = compile_regex(re, ci).unwrap();
        required_literals(r.as_str()).map(|v| v.into_iter().map(|b| String::from_utf8_lossy(&b).into_owned()).collect())
    }

    #[test]
    fn required_literals_of_common_regexes() {
        let v = |xs: &[&str]| Some(xs.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(lits("Zürich", false), v(&["Zürich"]));
        assert_eq!(lits("(Bahnhof|Station)", false), v(&["Bahnhof", "Station"]));
        assert_eq!(lits("ab", false), None);
        assert_eq!(lits("[[:alpha:]]+x", false), None);
        let many: Vec<String> = (10..27).map(|k| format!("w{k}x")).collect();
        assert_eq!(lits(&many.join("|"), false), None);
        assert_eq!(lits(&many[..16].join("|"), false).map(|v| v.len()), Some(16));
        assert_eq!(lits("rue", true).map(|v| v.len()), Some(8));
        // mini under ,i: 36 spellings (i also as ı), counted as 16; all kept.
        let mini = lits("Mini", true).unwrap();
        assert!(mini.len() == 36 && mini.contains(&"mını".to_string()) && mini.contains(&"MINI".to_string()), "{mini:?}");
        for s in ["Minigolf", "MINI pitch", "mını", "Kalamini"] {
            assert!(mini.iter().any(|x| s.contains(x.as_str())), "{s}");
        }
        // Longer words under ,i: none (128 spellings before the rule, 192 now).
        assert_eq!(lits("Peletier", true), None);
        // The extractor cuts long alternatives itself: as before, 8 prefixes.
        assert_eq!(lits("^(улица Ленина|Ленина|Ленина улица)$", true).map(|v| v.len()), Some(8));
    }

    #[test]
    fn anchored_prefixes_of_common_regexes() {
        let v = |xs: &[&str]| Some(xs.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(pre("^Aldi", false), v(&["Aldi"]));
        assert_eq!(pre("^(primary|secondary)$", false), v(&["primary", "secondary"]));
        assert_eq!(pre("^(a|ab|b)", false), v(&["a", "b"]));
        assert_eq!(pre("^ab", true), v(&["AB", "Ab", "aB", "ab"]));
        assert_eq!(pre("^Stra(ss|ß)e", false), v(&["Strasse", "Straße"]));
        // Not anchored, anchored in one branch only, or no fixed start.
        assert_eq!(pre("Aldi", false), None);
        assert_eq!(pre("^a|b", false), None);
        assert_eq!(pre("^.*x", false), None);
        assert_eq!(pre("^", false), None);
        assert_eq!(pre("^[[:alpha:]]", false), None);
        // Every value the regex matches starts with a prefix.
        for (re, vals) in [("^Al(di|ber)", &["Aldi Süd", "Alberto", "Al"][..]), ("^(x|y)z?$", &["x", "yz", "q"])] {
            let r = compile_regex(re, false).unwrap();
            let ps = anchored_prefixes(r.as_str()).unwrap();
            for s in vals {
                if r.is_match(s) {
                    assert!(ps.iter().any(|p| s.as_bytes().starts_with(p)), "{re} {s}");
                }
            }
        }
    }
}
