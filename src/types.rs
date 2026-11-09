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
    fn varchar_counts_chars() {
        assert_eq!(char_len("事务与恢复"), 5);
        assert_eq!(char_len("ab😀"), 3);
    }
}
