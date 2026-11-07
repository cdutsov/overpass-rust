//! Number recognition and formatting with the C library semantics the C++
//! evaluators rely on (`strtoll` base 0, `strtod`, `%.14g`, `%.Nf`).
//! See docs/spec/evaluators.md §1.

fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `strtoll(s, &end, 0)` consuming the whole string; overflow saturates.
pub fn int(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.is_empty() {
        return None;
    }
    let mut i = 0;
    while i < b.len() && is_c_space(b[i]) {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let (base, mut j) = if b.get(i) == Some(&b'0') && matches!(b.get(i + 1), Some(b'x' | b'X')) && b.get(i + 2).is_some_and(|c| c.is_ascii_hexdigit()) {
        (16u32, i + 2)
    } else if b.get(i) == Some(&b'0') && matches!(b.get(i + 1), Some(b'b' | b'B')) && matches!(b.get(i + 2), Some(b'0' | b'1')) {
        // glibc >= 2.38 (C23) accepts binary literals; the reference build does.
        (2, i + 2)
    } else if b.get(i) == Some(&b'0') {
        (8, i)
    } else {
        (10, i)
    };
    let start = j;
    let mut v: u128 = 0;
    while j < b.len() {
        let Some(d) = (b[j] as char).to_digit(base) else { break };
        v = (v * base as u128 + d as u128).min(1 << 64);
        j += 1;
    }
    if j == start || j != b.len() {
        return None;
    }
    Some(if neg {
        if v > 1u128 << 63 { i64::MIN } else { (v as i128).wrapping_neg() as i64 }
    } else if v > i64::MAX as u128 {
        i64::MAX
    } else {
        v as i64
    })
}

/// Longest prefix accepted by `strtod`: (value, bytes consumed, range error).
fn strtod_prefix(s: &str) -> Option<(f64, usize, bool)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_c_space(b[i]) {
        i += 1;
    }
    let num_start = i;
    let mut neg = false;
    if matches!(b.get(i), Some(b'+' | b'-')) {
        neg = b[i] == b'-';
        i += 1;
    }
    let rest = &s[i..];
    let lower: String = rest.chars().take(8).collect::<String>().to_ascii_lowercase();
    if lower.starts_with("inf") {
        let n = if lower.starts_with("infinity") { 8 } else { 3 };
        return Some((if neg { f64::NEG_INFINITY } else { f64::INFINITY }, i + n, false));
    }
    if lower.starts_with("nan") {
        let mut n = i + 3;
        if b.get(n) == Some(&b'(') {
            let mut k = n + 1;
            while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'_') {
                k += 1;
            }
            if b.get(k) == Some(&b')') {
                n = k + 1;
            }
        }
        let v = if neg { -f64::NAN } else { f64::NAN };
        return Some((v, n, false));
    }
    if b.get(i) == Some(&b'0') && matches!(b.get(i + 1), Some(b'x' | b'X')) {
        let mut k = i + 2;
        let mut mant: f64 = 0.0;
        let mut digits = 0;
        let mut exp: i32 = 0;
        while k < b.len() && b[k].is_ascii_hexdigit() {
            mant = mant * 16.0 + (b[k] as char).to_digit(16).unwrap() as f64;
            k += 1;
            digits += 1;
        }
        if b.get(k) == Some(&b'.') {
            k += 1;
            while k < b.len() && b[k].is_ascii_hexdigit() {
                mant = mant * 16.0 + (b[k] as char).to_digit(16).unwrap() as f64;
                exp -= 4;
                k += 1;
                digits += 1;
            }
        }
        if digits > 0 {
            if matches!(b.get(k), Some(b'p' | b'P')) {
                let mut m = k + 1;
                let eneg = match b.get(m) {
                    Some(b'-') => {
                        m += 1;
                        true
                    }
                    Some(b'+') => {
                        m += 1;
                        false
                    }
                    _ => false,
                };
                let es = m;
                let mut e: i32 = 0;
                while m < b.len() && b[m].is_ascii_digit() {
                    e = e.saturating_mul(10).saturating_add((b[m] - b'0') as i32);
                    m += 1;
                }
                if m > es {
                    exp += if eneg { -e } else { e };
                    k = m;
                }
            }
            let v = mant * 2f64.powi(exp);
            return Some((if neg { -v } else { v }, k, v.is_infinite()));
        }
        // "0x" without digits: only the "0" is consumed.
        return Some((if neg { -0.0 } else { 0.0 }, i + 1, false));
    }
    let mut k = i;
    let mut digits = 0;
    let mut nonzero = false;
    while k < b.len() && b[k].is_ascii_digit() {
        nonzero |= b[k] != b'0';
        k += 1;
        digits += 1;
    }
    if b.get(k) == Some(&b'.') {
        k += 1;
        while k < b.len() && b[k].is_ascii_digit() {
            nonzero |= b[k] != b'0';
            k += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if matches!(b.get(k), Some(b'e' | b'E')) {
        let mut m = k + 1;
        if matches!(b.get(m), Some(b'+' | b'-')) {
            m += 1;
        }
        let es = m;
        while m < b.len() && b[m].is_ascii_digit() {
            m += 1;
        }
        if m > es {
            k = m;
        }
    }
    let text = &s[num_start..k];
    let t = text.trim_start_matches('+');
    let v: f64 = t.parse().ok()?;
    let erange = v.is_infinite() || (nonzero && (v == 0.0 || v.is_subnormal()));
    Some((v, k, erange))
}

/// `strtod` consuming the whole string (range errors ignored).
pub fn dbl(s: &str) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let (v, n, _) = strtod_prefix(s)?;
    (n == s.len()).then_some(v)
}

/// `strtod` on a prefix, failing on range errors.
pub fn pdbl(s: &str) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let (v, n, erange) = strtod_prefix(s)?;
    (n > 0 && !erange).then_some(v)
}

/// Text after a leading number, without the whitespace that follows it.
pub fn suffix(s: &str) -> String {
    match strtod_prefix(s) {
        Some((_, n, false)) if n > 0 => s[n..].trim_start_matches(|c: char| c.is_ascii() && is_c_space(c as u8)).to_string(),
        _ => String::new(),
    }
}

fn nan_inf(x: f64) -> Option<String> {
    if x.is_nan() {
        return Some(if x.is_sign_negative() { "-nan".into() } else { "nan".into() });
    }
    if x.is_infinite() {
        return Some(if x > 0.0 { "inf".into() } else { "-inf".into() });
    }
    None
}

/// `printf("%.14g", x)`.
pub fn g14(x: f64) -> String {
    if let Some(s) = nan_inf(x) {
        return s;
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let sci = format!("{:.13e}", x);
    let (mant, exp) = sci.split_once('e').unwrap();
    let e: i32 = exp.parse().unwrap();
    if !(-4..14).contains(&e) {
        let m = strip_zeros(mant);
        return format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    }
    strip_zeros(&format!("{:.*}", (13 - e) as usize, x)).to_string()
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s }
}

/// `printf("%.{p}f", x)`.
pub fn fix(x: f64, p: usize) -> String {
    if let Some(s) = nan_inf(x) {
        return s;
    }
    format!("{:.*}", p, x)
}

/// Truthiness of an evaluator value.
pub fn truthy(s: &str) -> bool {
    match dbl(s) {
        Some(v) => v != 0.0,
        None => !s.is_empty(),
    }
}

/// `date()` encoding; `None` for invalid dates ("NaD").
pub fn date(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut parts = [0u32; 6];
    let mut i = 0;
    for p in parts.iter_mut() {
        while i < b.len() && !b[i].is_ascii_digit() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let mut v: u32 = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            v = v.wrapping_mul(10).wrapping_add((b[i] - b'0') as u32);
            i += 1;
        }
        *p = v;
    }
    let [y, mo, d, h, mi, se] = parts;
    if y < 1000 || mo > 12 || d > 31 || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(
        y as f64 + mo as f64 / 16.0 + d as f64 / (16.0 * 32.0) + h as f64 / (16.0 * 32.0 * 32.0) + mi as f64 / (16.0 * 32.0 * 32.0 * 64.0)
            + se as f64 / (16.0 * 32.0 * 32.0 * 64.0 * 64.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_semantics() {
        assert_eq!(int("010"), Some(8));
        assert_eq!(int("0x10"), Some(16));
        assert_eq!(int("08"), None);
        assert_eq!(int("099"), None);
        assert_eq!(int(" 7"), Some(7));
        assert_eq!(int("7 "), None);
        assert_eq!(int("+5"), Some(5));
        assert_eq!(int("1e3"), None);
        assert_eq!(int("99999999999999999999"), Some(i64::MAX));
        assert_eq!(int("-99999999999999999999"), Some(i64::MIN));
        assert_eq!(int("0"), Some(0));
        assert_eq!(int(""), None);
        assert_eq!(int("-"), None);
    }

    #[test]
    fn dbl_semantics() {
        assert_eq!(dbl("08"), Some(8.0));
        assert_eq!(dbl("1e3"), Some(1000.0));
        assert!(dbl("NaN").unwrap().is_nan());
        assert_eq!(dbl("-Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(dbl("1."), Some(1.0));
        assert_eq!(dbl(".5"), Some(0.5));
        assert_eq!(dbl("0x1p3"), Some(8.0));
        assert_eq!(dbl("NaD"), None);
        assert_eq!(dbl("7 "), None);
        assert_eq!(pdbl("information"), Some(f64::INFINITY));
        assert!(pdbl("nano").unwrap().is_nan());
        assert_eq!(pdbl("5.5m"), Some(5.5));
        assert_eq!(pdbl("1e"), Some(1.0));
        assert_eq!(pdbl("-."), None);
        assert_eq!(pdbl(".e1"), None);
        assert_eq!(pdbl("1e400"), None);
    }

    #[test]
    fn suffix_cases() {
        for (i, o) in [("", ""), ("1e100", ""), ("5.5m", "m"), ("200 ", ""), ("40 m/s", "m/s"), ("4 2", "2"), ("3,14", ",14"), ("1e", "e"), ("9 3/4", "3/4"), ("abc", "")] {
            assert_eq!(suffix(i), o, "{i}");
        }
    }

    #[test]
    fn g14_cases() {
        for (v, o) in [
            (8.0 / 9.0, "0.88888888888889"),
            (1e14, "1e+14"),
            (0.1 + 0.2, "0.3"),
            (123456789012345.0, "1.2345678901234e+14"),
            (2.0, "2"),
            (99999999999999.5, "1e+14"),
            (12345678901234.5, "12345678901234"),
            (1e12, "1000000000000"),
            (1e-4, "0.0001"),
            (3.14159265358979, "3.1415926535898"),
            (1e-5, "1e-05"),
            (5e-324, "4.9406564584125e-324"),
            (-0.0, "-0"),
            (f64::MAX, "1.7976931348623e+308"),
        ] {
            assert_eq!(g14(v), o, "{v}");
        }
        assert_eq!(fix(0.0625, 3), "0.062");
        assert_eq!(fix(2.5, 0), "2");
        assert_eq!(fix(3.5, 0), "4");
        assert_eq!(fix(1.0005, 3), "1.000");
    }

    #[test]
    fn truthiness() {
        for s in ["", "0", "0.0", "-0", " 0", "0x0", "0e5"] {
            assert!(!truthy(s), "{s:?}");
        }
        for s in ["false", "0 ", "1", "abc", "NaN", "inf", "no"] {
            assert!(truthy(s), "{s:?}");
        }
    }

    #[test]
    fn dates() {
        assert_eq!(date("2006").map(g14), Some("2006".into()));
        assert_eq!(date("2012-09-13").map(g14), Some("2012.587890625".into()));
        assert_eq!(date("2013-01-02T12:30:45Z").map(g14), Some("2013.0671679527".into()));
        assert_eq!(date("christmas_day"), None);
    }
}
