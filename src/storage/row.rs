//! Row encoding inside log records. The primary key is stored in the record
//! header, so the row body holds the other columns in schema order:
//! BIGINT as 8 bytes LE, VARCHAR as varint length + UTF-8, VECTOR(n) as n
//! float32 LE.

use super::catalog::TableDef;
use super::codec::{self, Reader};
use crate::error::DbResult;
use crate::types::{DataType, Value};
use crate::{bail, db_err};

pub fn encode_row(def: &TableDef, row: &[Value], out: &mut Vec<u8>) -> DbResult<()> {
    if row.len() != def.columns.len() {
        bail!(Internal, "row has {} values, table {} has {} columns", row.len(), def.name, def.columns.len());
    }
    for (i, (col, v)) in def.columns.iter().zip(row).enumerate() {
        if i == def.pk {
            continue;
        }
        match (col.ty, v) {
            (DataType::BigInt, Value::Int(x)) => codec::put_i64(out, *x),
            (DataType::Varchar(_), Value::Str(s)) => codec::put_str(out, s),
            (DataType::Vector(dim), Value::Vector(v)) if v.len() == dim as usize => {
                out.reserve(v.len() * 4);
                for f in v {
                    out.extend_from_slice(&f.to_le_bytes());
                }
            }
            _ => bail!(Internal, "value for column {} does not match type {}", col.name, col.ty),
        }
    }
    Ok(())
}

pub fn decode_row(def: &TableDef, pk: i64, bytes: &[u8]) -> DbResult<Vec<Value>> {
    let mut r = Reader::new(bytes);
    let mut row = Vec::with_capacity(def.columns.len());
    for (i, col) in def.columns.iter().enumerate() {
        if i == def.pk {
            row.push(Value::Int(pk));
            continue;
        }
        row.push(match col.ty {
            DataType::BigInt => Value::Int(r.i64()?),
            DataType::Varchar(_) => Value::Str(r.str()?.to_string()),
            DataType::Vector(dim) => {
                let raw = r.take(dim as usize * 4)?;
                Value::Vector(raw.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
            }
        });
    }
    if r.remaining() != 0 {
        return Err(db_err!(Corrupt, "{} trailing bytes in row of table {}", r.remaining(), def.name));
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::catalog::Column;

    fn docs() -> TableDef {
        TableDef {
            id: 1,
            name: "docs".into(),
            columns: vec![
                Column { name: "id".into(), ty: DataType::BigInt },
                Column { name: "category".into(), ty: DataType::Varchar(32) },
                Column { name: "embedding".into(), ty: DataType::Vector(3) },
                Column { name: "n".into(), ty: DataType::BigInt },
            ],
            pk: 0,
            vector_index: None,
        }
    }

    #[test]
    fn roundtrip() {
        let def = docs();
        let row = vec![
            Value::Int(42),
            Value::Str("向量索引".into()),
            Value::Vector(vec![1.0, -0.5, 3.25]),
            Value::Int(i64::MIN),
        ];
        let mut buf = Vec::new();
        encode_row(&def, &row, &mut buf).unwrap();
        assert_eq!(buf.len(), 1 + 12 + 12 + 8);
        assert_eq!(decode_row(&def, 42, &buf).unwrap(), row);
        assert!(decode_row(&def, 42, &buf[..buf.len() - 1]).is_err());
    }

    #[test]
    fn rejects_mismatched_values() {
        let def = docs();
        let mut buf = Vec::new();
        let bad_dim = vec![Value::Int(1), Value::Str("a".into()), Value::Vector(vec![1.0]), Value::Int(0)];
        assert!(encode_row(&def, &bad_dim, &mut buf).is_err());
        assert!(encode_row(&def, &bad_dim[..2], &mut buf).is_err());
    }
}
