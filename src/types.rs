use std::fmt;

use crate::error::DbResult;
use crate::{bail, db_err};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    BigInt,
    /// Maximum length in Unicode characters.
    Varchar(u32),
    /// Fixed dimension of float32 elements.
    Vector(u32),
}

impl DataType {
    pub fn tag(self) -> u8 {
        match self {
            DataType::BigInt => 1,
            DataType::Varchar(_) => 2,
            DataType::Vector(_) => 3,
        }
    }

    pub fn param(self) -> u32 {
        match self {
            DataType::BigInt => 0,
            DataType::Varchar(n) | DataType::Vector(n) => n,
        }
    }

    pub fn from_parts(tag: u8, param: u32) -> Option<DataType> {
        match tag {
            1 => Some(DataType::BigInt),
            2 => Some(DataType::Varchar(param)),
            3 => Some(DataType::Vector(param)),
            _ => None,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::BigInt => write!(f, "BIGINT"),
            DataType::Varchar(n) => write!(f, "VARCHAR({n})"),
            DataType::Vector(n) => write!(f, "VECTOR({n})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Str(String),
    Vector(Vec<f32>),
}

/// Length of a VARCHAR value as the schema counts it: Unicode scalar values.
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Parses a vector literal such as `[1, 0.5, -2e3]`. Every element must be a
/// plain decimal number that rounds to a finite float32; NaN, infinities and
/// float32 overflow are rejected. Dimension is checked by the caller.
pub fn parse_vector(text: &str) -> DbResult<Vec<f32>> {
    if let Some(v) = parse_vector_fast(text.as_bytes()) {
        return Ok(v);
    }
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| db_err!(InvalidValue, "invalid vector literal '{}'", text))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|e| parse_f32_strict(e.trim(), text))
        .collect()
}

/// Single-pass path for the usual shape `[d.ddd,-d.ddd,...]`. It accepts
/// only ASCII decimals whose digits form a significand of at most 2^24 and
/// whose decimal exponent is at most 10 in magnitude. Both factors are then
/// exact float32 values, so one float32 multiply or divide rounds exactly
/// as `str::parse::<f32>` does (Clinger's fast path). Any other input,
/// malformed ones included, returns None and takes the general path.
fn parse_vector_fast(b: &[u8]) -> Option<Vec<f32>> {
    const POW10: [f32; 11] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10];
    let skip_ws = |mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        i
    };
    // Appends the digits at `i` to `m`; returns the new position and count.
    let digits = |mut i: usize, m: &mut u64| {
        let start = i;
        while let Some(d) = b.get(i).map(|c| c.wrapping_sub(b'0')).filter(|&d| d <= 9) {
            *m = m.wrapping_mul(10).wrapping_add(u64::from(d));
            i += 1;
        }
        (i, i - start)
    };
    let mut i = skip_ws(0);
    if b.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    let mut out = Vec::with_capacity(b.len() / 8 + 1);
    loop {
        i = skip_ws(i);
        let neg = b.get(i) == Some(&b'-');
        if neg || b.get(i) == Some(&b'+') {
            i += 1;
        }
        let mut m = 0u64;
        let (j, int_digits) = digits(i, &mut m);
        let (mut frac_digits, mut exp) = (0, 0i32);
        i = j;
        if b.get(i) == Some(&b'.') {
            (i, frac_digits) = digits(i + 1, &mut m);
            exp = -(frac_digits as i32);
        }
        // 19 digits cannot wrap a u64, so m is exact whenever it is checked.
        let n = int_digits + frac_digits;
        if n == 0 || n > 19 || m > 1 << 24 {
            return None;
        }
        if matches!(b.get(i), Some(b'e' | b'E')) {
            i += 1;
            let eneg = b.get(i) == Some(&b'-');
            if eneg || b.get(i) == Some(&b'+') {
                i += 1;
            }
            let mut e = 0u64;
            let (j, n) = digits(i, &mut e);
            if n == 0 || n > 3 {
                return None;
            }
            i = j;
            exp += if eneg { -(e as i32) } else { e as i32 };
        }
        let p = *POW10.get(exp.unsigned_abs() as usize)?;
        let mag = if exp >= 0 { m as f32 * p } else { m as f32 / p };
        out.push(if neg { -mag } else { mag });
        i = skip_ws(i);
        match b.get(i) {
            Some(b',') => i += 1,
            Some(b']') => break,
            _ => return None,
        }
    }
    (skip_ws(i + 1) == b.len()).then_some(out)
}

fn parse_f32_strict(elem: &str, whole: &str) -> DbResult<f32> {
    if !is_plain_number(elem) {
        bail!(InvalidValue, "invalid vector element '{}' in '{}'", elem, whole);
    }
    let v: f32 = elem
        .parse()
        .map_err(|_| db_err!(InvalidValue, "invalid vector element '{}' in '{}'", elem, whole))?;
    if !v.is_finite() {
        bail!(InvalidValue, "vector element '{}' is out of float32 range", elem);
    }
    Ok(v)
}

/// `[+-] digits [. digits] [(e|E) [+-] digits]` with at least one mantissa
/// digit. Rejects words like `nan`/`inf` that `str::parse` would accept.
fn is_plain_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[test]
    fn vector_literals() {
        assert_eq!(parse_vector("[1, 0, 0.5, -2]").unwrap(), vec![1.0, 0.0, 0.5, -2.0]);
        assert_eq!(parse_vector(" [ 1e2 ,+.5, 5. ] ").unwrap(), vec![100.0, 0.5, 5.0]);
        assert_eq!(parse_vector("[]").unwrap(), Vec::<f32>::new());
        assert_eq!(parse_vector("[3.4028235e38]").unwrap(), vec![f32::MAX]);
        assert_eq!(parse_vector("[1e-50]").unwrap(), vec![0.0]);
    }

    #[test]
    fn vector_rejects() {
        for bad in [
            "[nan]", "[NaN]", "[inf]", "[-Infinity]", "[1e39]", "[-3.5e38]", "[1,,2]", "[1 2]",
            "1,2", "[1,2", "[0x10]", "[1e]", "[.]", "[1, 'a']", "[1,]",
        ] {
            let e = parse_vector(bad).unwrap_err();
            assert_eq!(e.kind, ErrorKind::InvalidValue, "{bad}");
        }
    }

    #[test]
    fn fast_path_equals_general_parse() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let mut fast = 0;
        for _ in 0..200_000 {
            let mut e = String::new();
            match next(3) {
                0 => e.push('-'),
                1 if next(4) == 0 => e.push('+'),
                _ => {}
            }
            for _ in 0..next(4) {
                e.push((b'0' + next(10) as u8) as char);
            }
            if next(4) != 0 {
                e.push('.');
                for _ in 0..next(10) {
                    e.push((b'0' + next(10) as u8) as char);
                }
            }
            if next(5) == 0 {
                e.push_str(["e", "E", "e-", "e+"][next(4) as usize]);
                e.push_str(&next(14).to_string());
            }
            let text = format!("[{e}]");
            let general = parse_f32_strict(&e, &text);
            if let Some(v) = parse_vector_fast(text.as_bytes()) {
                fast += 1;
                let g = general.unwrap_or_else(|err| panic!("fast path accepted {text}: {err}"));
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].to_bits(), g.to_bits(), "{text}");
            }
        }
        assert!(fast > 100_000, "fast path taken {fast} times");
    }

    #[test]
    fn fast_path_covers_the_benchmark_format() {
        let text = "[-0.123456,0.999999,-0.000000,0.000001,1.000000, -0.5 ]";
        let v = parse_vector_fast(text.as_bytes()).expect("fast path");
        let want: Vec<f32> = ["-0.123456", "0.999999", "-0.000000", "0.000001", "1.000000", "-0.5"]
            .iter()
            .map(|e| e.parse().unwrap())
            .collect();
        assert_eq!(v.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), want.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
        for slow in ["[]", "[ ]", "[1,]", "[1e]", "[0.12345678901]", "[1e11]", "[1\u{a0}]", "[1]x"] {
            assert!(parse_vector_fast(slow.as_bytes()).is_none(), "{slow}");
        }
    }

    #[test]
    fn varchar_counts_chars() {
        assert_eq!(char_len("事务与恢复"), 5);
        assert_eq!(char_len("ab😀"), 3);
    }
}
