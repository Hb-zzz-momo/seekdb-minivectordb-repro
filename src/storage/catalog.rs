use super::codec::{self, Reader};
use crate::error::DbResult;
use crate::types::DataType;
use crate::{bail, db_err};

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub ty: DataType,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableDef {
    pub id: u32,
    pub name: String,
    pub columns: Vec<Column>,
    /// Position of the BIGINT primary key column.
    pub pk: usize,
    /// Name of the vector index, if one was created.
    pub vector_index: Option<String>,
}

impl TableDef {
    /// Column position by name; names compare case-insensitively.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Position of the (at most one) vector column.
    pub fn vector_column(&self) -> Option<usize> {
        self.columns.iter().position(|c| matches!(c.ty, DataType::Vector(_)))
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        codec::put_u32(buf, self.id);
        codec::put_str(buf, &self.name);
        codec::put_varint(buf, self.columns.len() as u64);
        for c in &self.columns {
            codec::put_str(buf, &c.name);
            buf.push(c.ty.tag());
            codec::put_u32(buf, c.ty.param());
        }
        codec::put_varint(buf, self.pk as u64);
        match &self.vector_index {
            Some(name) => {
                buf.push(1);
                codec::put_str(buf, name);
            }
            None => buf.push(0),
        }
    }

    pub fn decode(r: &mut Reader<'_>) -> DbResult<TableDef> {
        let id = r.u32()?;
        let name = r.str()?.to_string();
        let n = r.varint()? as usize;
        let mut columns = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let cname = r.str()?.to_string();
            let tag = r.u8()?;
            let param = r.u32()?;
            let ty = DataType::from_parts(tag, param).ok_or_else(|| db_err!(Corrupt, "unknown column type {}", tag))?;
            columns.push(Column { name: cname, ty });
        }
        let pk = r.varint()? as usize;
        if pk >= columns.len() {
            bail!(Corrupt, "primary key position {} out of range", pk);
        }
        let vector_index = match r.u8()? {
            0 => None,
            1 => Some(r.str()?.to_string()),
            t => bail!(Corrupt, "bad vector index flag {}", t),
        };
        Ok(TableDef { id, name, columns, pk, vector_index })
    }
}
