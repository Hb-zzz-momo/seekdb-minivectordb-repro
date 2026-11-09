//! Committed state of one table: the primary-key index into the commit log
//! and, while the table fits the memory budget, a decoded copy of its rows
//! with vectors packed into one contiguous array for distance scans.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use crate::error::DbResult;
use crate::storage::catalog::TableDef;
use crate::storage::log::{LogStore, RowLoc};
use crate::storage::row::decode_row;
use crate::types::Value;

const NO_SLOT: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub loc: RowLoc,
    slot: u32,
}

pub enum ValRef<'a> {
    Int(i64),
    Str(&'a str),
    Vector(&'a [f32]),
}

/// Read-only view of one row. For cached rows the vector column is read from
/// the cache's packed array rather than from `values`.
#[derive(Clone, Copy)]
pub struct RowView<'a> {
    pub pk: i64,
    values: &'a [Value],
    vec_col: usize,
    vec: &'a [f32],
}

impl<'a> RowView<'a> {
    /// View of a fully materialized row.
    pub fn new(pk: i64, values: &'a [Value], vec_col: Option<usize>) -> RowView<'a> {
        let vec = match vec_col.and_then(|i| values.get(i)) {
            Some(Value::Vector(v)) => v.as_slice(),
            _ => &[],
        };
        RowView { pk, values, vec_col: vec_col.unwrap_or(usize::MAX), vec }
    }

    pub fn get(&self, col: usize) -> ValRef<'a> {
        if col == self.vec_col {
            return ValRef::Vector(self.vec);
        }
        match &self.values[col] {
            Value::Int(i) => ValRef::Int(*i),
            Value::Str(s) => ValRef::Str(s),
            Value::Vector(v) => ValRef::Vector(v),
        }
    }

    pub fn to_values(self) -> Vec<Value> {
        (0..self.values.len())
            .map(|i| match self.get(i) {
                ValRef::Int(x) => Value::Int(x),
                ValRef::Str(s) => Value::Str(s.to_string()),
                ValRef::Vector(v) => Value::Vector(v.to_vec()),
            })
            .collect()
    }
}

struct RowCache {
    vec_col: Option<usize>,
    dim: usize,
    /// Row values per slot, vector column emptied; `None` marks a free slot.
    values: Vec<Option<Vec<Value>>>,
    vecs: Vec<f32>,
    free: Vec<u32>,
    bytes: usize,
}

impl RowCache {
    fn new(def: &TableDef) -> RowCache {
        let vec_col = def.vector_column();
        let dim = vec_col.map_or(0, |i| def.columns[i].ty.param() as usize);
        RowCache { vec_col, dim, values: Vec::new(), vecs: Vec::new(), free: Vec::new(), bytes: 0 }
    }

    fn insert(&mut self, mut row: Vec<Value>) -> u32 {
        let vec = match self.vec_col {
            Some(i) => match std::mem::replace(&mut row[i], Value::Vector(Vec::new())) {
                Value::Vector(v) => v,
                _ => Vec::new(),
            },
            None => Vec::new(),
        };
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                self.values.push(None);
                self.vecs.resize(self.vecs.len() + self.dim, 0.0);
                (self.values.len() - 1) as u32
            }
        };
        let s = slot as usize;
        let dst = &mut self.vecs[s * self.dim..(s + 1) * self.dim];
        if vec.len() == dst.len() {
            dst.copy_from_slice(&vec);
        }
        self.bytes += row_bytes(&row) + self.dim * 4;
        self.values[s] = Some(row);
        slot
    }

    fn remove(&mut self, slot: u32) {
        if let Some(row) = self.values[slot as usize].take() {
            self.bytes -= row_bytes(&row) + self.dim * 4;
            self.free.push(slot);
        }
    }

    fn view(&self, slot: u32, pk: i64) -> RowView<'_> {
        let s = slot as usize;
        RowView {
            pk,
            values: self.values[s].as_deref().unwrap_or(&[]),
            vec_col: self.vec_col.unwrap_or(usize::MAX),
            vec: &self.vecs[s * self.dim..(s + 1) * self.dim],
        }
    }
}

/// Rough heap footprint of a cached row, for the memory budget.
fn row_bytes(row: &[Value]) -> usize {
    64 + row
        .iter()
        .map(|v| {
            24 + match v {
                Value::Str(s) => s.capacity(),
                _ => 0,
            }
        })
        .sum::<usize>()
}

pub struct Table {
    pub def: TableDef,
    pub rows: BTreeMap<i64, Entry>,
    cache: Option<RowCache>,
}

impl Table {
    pub fn new(def: TableDef) -> Table {
        let cache = Some(RowCache::new(&def));
        Table { def, rows: BTreeMap::new(), cache }
    }

    pub fn is_cached(&self) -> bool {
        self.cache.is_some()
    }

    pub fn cache_bytes(&self) -> usize {
        self.cache.as_ref().map_or(0, |c| c.bytes + c.vecs.capacity() * 4)
    }

    /// Switches the table to reading rows from the log.
    pub fn drop_cache(&mut self) {
        self.cache = None;
        for e in self.rows.values_mut() {
            e.slot = NO_SLOT;
        }
    }

    /// Records a committed insert or update. `row` is required while the
    /// table is cached; without it the cache is given up.
    pub fn apply_put(&mut self, pk: i64, loc: RowLoc, row: Option<Vec<Value>>) {
        if row.is_none() && self.cache.is_some() {
            self.drop_cache();
        }
        let old = self.rows.get(&pk).map_or(NO_SLOT, |e| e.slot);
        let slot = match (&mut self.cache, row) {
            (Some(cache), Some(row)) => {
                if old != NO_SLOT {
                    cache.remove(old);
                }
                cache.insert(row)
            }
            _ => NO_SLOT,
        };
        self.rows.insert(pk, Entry { loc, slot });
    }

    pub fn apply_delete(&mut self, pk: i64) {
        if let Some(e) = self.rows.remove(&pk) {
            if let (Some(cache), true) = (&mut self.cache, e.slot != NO_SLOT) {
                cache.remove(e.slot);
            }
        }
    }

    /// Committed row by primary key.
    pub fn get(&self, log: &LogStore, pk: i64) -> DbResult<Option<Vec<Value>>> {
        let Some(e) = self.rows.get(&pk) else {
            return Ok(None);
        };
        if let (Some(cache), true) = (&self.cache, e.slot != NO_SLOT) {
            return Ok(Some(cache.view(e.slot, pk).to_values()));
        }
        let mut buf = Vec::new();
        log.read(e.loc, &mut buf)?;
        Ok(Some(decode_row(&self.def, pk, &buf)?))
    }

    /// Visits committed rows with pk in `range` in pk order until `f`
    /// returns false.
    pub fn scan(
        &self,
        log: &LogStore,
        range: RangeInclusive<i64>,
        mut f: impl FnMut(RowView<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let vec_col = self.def.vector_column();
        let mut buf = Vec::new();
        for (&pk, e) in self.rows.range(range) {
            let go = match (&self.cache, e.slot != NO_SLOT) {
                (Some(cache), true) => f(cache.view(e.slot, pk))?,
                _ => {
                    log.read(e.loc, &mut buf)?;
                    let values = decode_row(&self.def, pk, &buf)?;
                    f(RowView::new(pk, &values, vec_col))?
                }
            };
            if !go {
                break;
            }
        }
        Ok(())
    }
}
