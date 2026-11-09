use std::cmp::Ordering;

use crate::types::Value;

/// One output value.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Int(i64),
    Float(f64),
    Str(String),
    Vector(Vec<f32>),
}

impl From<Value> for Cell {
    fn from(v: Value) -> Cell {
        match v {
            Value::Int(i) => Cell::Int(i),
            Value::Str(s) => Cell::Str(s),
            Value::Vector(v) => Cell::Vector(v),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecResult {
    /// Statement without a result set; `affected` counts changed rows.
    Ok { affected: u64 },
    Rows { columns: Vec<String>, rows: Vec<Vec<Cell>> },
}

impl ExecResult {
    pub fn ok(affected: u64) -> ExecResult {
        ExecResult::Ok { affected }
    }
}

/// Exact comparison of an integer with a float.
pub fn cmp_int_float(i: i64, f: f64) -> Ordering {
    if f >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    if f < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    let t = f.trunc();
    match i.cmp(&(t as i64)) {
        Ordering::Equal if f > t => Ordering::Less,
        Ordering::Equal if f < t => Ordering::Greater,
        o => o,
    }
}

/// Sort order for ORDER BY keys: numbers numerically, strings by code point.
pub fn cmp_cells(a: &Cell, b: &Cell) -> Ordering {
    match (a, b) {
        (Cell::Int(x), Cell::Int(y)) => x.cmp(y),
        (Cell::Float(x), Cell::Float(y)) => x.total_cmp(y),
        (Cell::Int(x), Cell::Float(y)) => cmp_int_float(*x, *y),
        (Cell::Float(x), Cell::Int(y)) => cmp_int_float(*y, *x).reverse(),
        (Cell::Str(x), Cell::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_float_comparison_is_exact() {
        assert_eq!(cmp_int_float(1, 1.5), Ordering::Less);
        assert_eq!(cmp_int_float(2, 1.5), Ordering::Greater);
        assert_eq!(cmp_int_float(-1, -1.5), Ordering::Greater);
        assert_eq!(cmp_int_float(-2, -1.5), Ordering::Less);
        assert_eq!(cmp_int_float(3, 3.0), Ordering::Equal);
        assert_eq!(cmp_int_float(i64::MAX, 9.3e18), Ordering::Less);
        assert_eq!(cmp_int_float(i64::MIN, -9.3e18), Ordering::Greater);
        assert_eq!(cmp_int_float(9_007_199_254_740_993, 9_007_199_254_740_992.0), Ordering::Greater);
    }
}
