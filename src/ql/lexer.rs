//! Tokenizer for Overpass QL.

use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    /// Identifier or bare word (`node`, `out`, `name`, `addr:street` parts).
    Word(String),
    /// Quoted string, escapes already decoded.
    Str(String),
    /// Number literal kept verbatim.
    Num(String),
    /// Punctuation or operator.
    P(&'static str),
    /// Any other single character (non-ASCII or unknown punctuation); only
    /// valid inside quotes, so the parser rejects it.
    Other(String),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub line: usize,
    pub col: usize,
    /// Whether whitespace or a comment precedes the token.
    pub space_before: bool,
}

const PUNCT: &[&str] = &[
    "->", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "::", "(", ")", "[", "]", "{", "}", ";", ",", ".", ":", "=", "~", "!", "<", ">",
    "+", "-", "*", "/", "%", "?",
];

pub fn tokenize(src: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut line, mut col) = (0usize, 1usize, 1usize);
    let mut space = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            col = 1;
            i += 1;
            space = true;
            continue;
        }
        // C `isspace`: other Unicode spaces are not whitespace in QL.
        if matches!(c, ' ' | '\t' | '\r' | '\u{0b}' | '\u{0c}') {
            i += 1;
            col += 1;
            space = true;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            space = true;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            col += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                if chars[i] == '\n' {
                    line += 1;
                    col = 1;
                } else {
                    col += 1;
                }
                i += 1;
            }
            if i >= chars.len() {
                // Like the C++ tokenizer, an unterminated comment swallows the rest.
                break;
            }
            i += 2;
            col += 2;
            space = true;
            continue;
        }
        let (start_line, start_col) = (line, col);
        let tok = if c == '"' || c == '\'' {
            let quote = c;
            i += 1;
            col += 1;
            let mut s = String::new();
            loop {
                let Some(&ch) = chars.get(i) else { bail!("line {start_line}: unterminated string") };
                i += 1;
                col += 1;
                if ch == quote {
                    break;
                }
                if ch == '\n' {
                    line += 1;
                    col = 1;
                }
                if ch == '\\' {
                    let Some(&e) = chars.get(i) else { bail!("line {start_line}: unterminated string") };
                    i += 1;
                    col += 1;
                    match e {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        'u' => {
                            // One to four hex digits, greedy.
                            let mut v = 0u32;
                            let mut n = 0;
                            while n < 4 && chars.get(i).is_some_and(|c| c.is_ascii_hexdigit()) {
                                v = v * 16 + chars[i].to_digit(16).unwrap();
                                i += 1;
                                col += 1;
                                n += 1;
                            }
                            if v < 0x20 {
                                bail!("line 0: parse error: Invalid UTF-8 character (value below 32) in escape sequence.");
                            }
                            s.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                        }
                        other => s.push(other),
                    }
                } else {
                    s.push(ch);
                }
            }
            Tok::Str(s)
        } else if c.is_ascii_digit() || (c == '-' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit() || *d == '.') && number_context(&out)) {
            // C++ number token: [0-9]+ ("." [0-9]*)? ("e" "-"? [0-9]*)?, or a
            // leading "-" followed by the same with optional integer digits.
            let st = i;
            if c == '-' {
                i += 1;
            }
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if chars.get(i) == Some(&'.') {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if chars.get(i) == Some(&'e') {
                i += 1;
                if chars.get(i) == Some(&'-') {
                    i += 1;
                }
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let n: String = chars[st..i].iter().collect();
            col += i - st;
            Tok::Num(n)
        } else if is_word_start(c) {
            let st = i;
            while i < chars.len() && is_word_char(chars[i]) {
                i += 1;
            }
            col += i - st;
            Tok::Word(chars[st..i].iter().collect())
        } else {
            let mut found = None;
            for p in PUNCT {
                let pc: Vec<char> = p.chars().collect();
                if chars[i..].starts_with(&pc) {
                    found = Some(*p);
                    break;
                }
            }
            match found {
                Some(p) => {
                    i += p.chars().count();
                    col += p.chars().count();
                    Tok::P(p)
                }
                None => {
                    i += 1;
                    col += 1;
                    Tok::Other(c.to_string())
                }
            }
        };
        out.push(Token { tok, line: start_line, col: start_col, space_before: space });
        space = false;
    }
    out.push(Token { tok: Tok::Eof, line, col, space_before: space });
    Ok(out)
}

/// A leading minus starts a number only where an operand is expected.
fn number_context(prev: &[Token]) -> bool {
    match prev.last().map(|t| &t.tok) {
        None => true,
        Some(Tok::P(p)) => !matches!(*p, ")" | "]"),
        Some(Tok::Num(_)) | Some(Tok::Str(_)) | Some(Tok::Word(_)) | Some(Tok::Other(_)) => false,
        Some(Tok::Eof) => true,
    }
}

fn is_word_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}
