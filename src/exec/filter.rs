//! Tag filters.

use crate::model::Tags;
use crate::ql::ast::Filter;
use anyhow::{Result, anyhow};
use regex::{Regex, RegexBuilder};

/// Translate a glibc POSIX extended regular expression into Rust syntax.
///
/// GNU escapes `\w \W \s \S \b \B` keep their meaning, `\<`/`\>` become
/// word boundaries, `` \` ``/`\'` anchor at text start/end; any other
/// escaped character is literal (so `\d` is the letter d). Inside a bracket
/// expression a backslash is an ordinary character. Perl extensions such as
/// `(?i)` are not ERE and are rejected.
pub fn translate_ere(re: &str) -> Option<String> {
    let chars: Vec<char> = re.chars().collect();
    let mut out = String::with_capacity(re.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                let Some(&n) = chars.get(i + 1) else { return None };
                match n {
                    'w' | 'W' | 's' | 'S' | 'b' | 'B' => {
                        out.push('\\');
                        out.push(n);
                    }
                    '<' | '>' => out.push_str("\\b"),
                    '`' => out.push_str("\\A"),
                    '\'' => out.push_str("\\z"),
                    '1'..='9' => return None,
                    other => {
                        let mut buf = [0u8; 4];
                        out.push_str(&regex::escape(other.encode_utf8(&mut buf)));
                    }
                }
                i += 2;
            }
            '[' => {
                // Copy a bracket expression; backslashes inside are literal.
                out.push('[');
                i += 1;
                if chars.get(i) == Some(&'^') {
                    out.push('^');
                    i += 1;
                }
                if chars.get(i) == Some(&']') {
                    out.push_str("\\]");
                    i += 1;
                }
                loop {
                    let Some(&d) = chars.get(i) else { return None };
                    if d == ']' {
                        out.push(']');
                        i += 1;
                        break;
                    }
                    if d == '[' && matches!(chars.get(i + 1), Some(':' | '.' | '=')) {
                        // POSIX class like [:alpha:]: copy through the closing ":]".
                        let close = chars.get(i + 1).copied().unwrap();
                        let start = i;
                        i += 2;
                        while i + 1 < chars.len() && !(chars[i] == close && chars[i + 1] == ']') {
                            i += 1;
                        }
                        if i + 1 >= chars.len() {
                            return None;
                        }
                        i += 2;
                        out.extend(&chars[start..i]);
                        continue;
                    }
                    if matches!(d, '\\' | '[' | '&' | '~' | '-') && !(d == '-' && (chars.get(i + 1) != Some(&']') && out.ends_with(|ch: char| ch != '[' && ch != '^'))) {
                        out.push('\\');
                    }
                    out.push(d);
                    i += 1;
                }
            }
            '(' if chars.get(i + 1) == Some(&'?') => return None,
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(out)
}

/// Compile a regex written for glibc POSIX ERE.
pub fn compile_regex(re: &str, ci: bool) -> Result<Regex> {
    let err = || anyhow!("Invalid regular expression: \"{re}\"");
    let translated = translate_ere(re).ok_or_else(err)?;
    RegexBuilder::new(&translated)
        .case_insensitive(ci)
        // POSIX `.` matches newlines.
        .dot_matches_new_line(true)
        .size_limit(2 << 20)
        .dfa_size_limit(2 << 20)
        .build()
        .map_err(|_| err())
}

/// Value prefixes up to this many bytes are always kept whole in the tag
/// index (see `store::index_part`).
const MAX_PREFIX: usize = 180;

/// Byte strings one of which every value matched by `re` (a translated
/// pattern, see `compile_regex`) starts with: for regexes anchored at the
/// start whose beginnings form a small set, e.g. `^Aldi` or
/// `^(primary|secondary)$`; None for the others. The tag index is then
/// read only at these prefixes.
pub fn anchored_prefixes(re: &str, ci: bool) -> Option<Vec<Vec<u8>>> {
    use regex_syntax::hir::Look;
    use regex_syntax::hir::literal::{ExtractKind, Extractor};
    let hir = regex_syntax::ParserBuilder::new().case_insensitive(ci).dot_matches_new_line(true).build().parse(re).ok()?;
    // `^` without multi-line mode: every match starts at the beginning of the value.
    if !hir.properties().look_set_prefix().contains(Look::Start) {
        return None;
    }
    let seq = Extractor::new().kind(ExtractKind::Prefix).extract(&hir);
    let mut lits: Vec<Vec<u8>> = seq.literals()?.iter().map(|l| l.as_bytes()[..l.as_bytes().len().min(MAX_PREFIX)].to_vec()).collect();
    // An empty prefix restricts nothing; NUL bytes are stored differently.
    if lits.is_empty() || lits.iter().any(|l| l.is_empty() || l.contains(&0)) {
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

/// A tag filter ready to match.
#[derive(Clone, Debug)]
pub enum TagFilter {
    HasKey(String),
    NotKey(String),
    Eq(String, String),
    Ne(String, String),
    /// `prefixes`: see `anchored_prefixes` (for the tag index).
    Regex { key: String, re: Regex, neg: bool, prefixes: Option<Vec<Vec<u8>>> },
    KeyRegex { key: Regex, value: Regex, value_prefixes: Option<Vec<Vec<u8>>> },
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
                let prefixes = anchored_prefixes(re.as_str(), *ci);
                TagFilter::Regex { key: key.clone(), re, neg: *neg, prefixes }
            }
            Filter::KeyRegex { key, value, ci } => {
                let value = compile_regex(value, *ci)?;
                let value_prefixes = anchored_prefixes(value.as_str(), *ci);
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
    }

    fn pre(re: &str, ci: bool) -> Option<Vec<String>> {
        let r = compile_regex(re, ci).unwrap();
        anchored_prefixes(r.as_str(), ci).map(|v| v.into_iter().map(|b| String::from_utf8(b).unwrap()).collect())
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
            let ps = anchored_prefixes(r.as_str(), false).unwrap();
            for s in vals {
                if r.is_match(s) {
                    assert!(ps.iter().any(|p| s.as_bytes().starts_with(p)), "{re} {s}");
                }
            }
        }
    }
}
