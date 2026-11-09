//! Committed state of one table: the primary-key index into the commit log
//! and, while the table fits the memory budget, a decoded copy of its rows
//! stored column by column. Each column is one contiguous array indexed by
//! slot, so a scan walks memory in order and a filter reads only its own
//! column.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use super::expr::{holds, ColPred, PredValue};
use crate::error::DbResult;
use crate::sql::ast::CmpOp;
use crate::storage::catalog::TableDef;
use crate::storage::log::{LogStore, RowLoc};
use crate::storage::row::decode_row;
use crate::types::{DataType, Value};

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

/// Read-only view of one row, either materialized values or a slot of the
/// row cache.
#[derive(Clone, Copy)]
pub struct RowView<'a> {
    pub pk: i64,
    src: Src<'a>,
}

#[derive(Clone, Copy)]
enum Src<'a> {
    Values(&'a [Value]),
    Cache(&'a RowCache, usize),
}

impl<'a> RowView<'a> {
    pub fn new(pk: i64, values: &'a [Value]) -> RowView<'a> {
        RowView { pk, src: Src::Values(values) }
    }

    #[inline]
    pub fn get(&self, col: usize) -> ValRef<'a> {
        match self.src {
            Src::Values(values) => match &values[col] {
                Value::Int(i) => ValRef::Int(*i),
                Value::Str(s) => ValRef::Str(s),
                Value::Vector(v) => ValRef::Vector(v),
            },
            Src::Cache(cache, slot) => cache.get(slot, col),
        }
    }

    pub fn to_values(self) -> Vec<Value> {
        let n = match self.src {
            Src::Values(values) => values.len(),
            Src::Cache(cache, _) => cache.cols.len(),
        };
        (0..n)
            .map(|i| match self.get(i) {
                ValRef::Int(x) => Value::Int(x),
                ValRef::Str(s) => Value::Str(s.to_string()),
                ValRef::Vector(v) => Value::Vector(v.to_vec()),
            })
            .collect()
    }
}

/// Rows by slot. Freed slots are reused by later inserts.
struct RowCache {
    pks: Vec<i64>,
    live: Vec<bool>,
    cols: Vec<ColData>,
    free: Vec<u32>,
}

enum ColData {
    /// The primary key column, read from `RowCache::pks`.
    Pk,
    Int(Vec<i64>),
    Str(StrCol),
    Vector { dim: usize, data: Vec<f32> },
}

/// Strings of one column, back to back in one buffer.
#[derive(Default)]
struct StrCol {
    /// (offset, length) of each slot's string in `arena`.
    spans: Vec<(usize, usize)>,
    arena: String,
    /// Bytes of `arena` that no slot refers to any more.
    garbage: usize,
}

impl StrCol {
    #[inline]
    fn get(&self, slot: usize) -> &str {
        let (off, len) = self.spans[slot];
        &self.arena[off..off + len]
    }

    fn set(&mut self, slot: usize, s: &str) {
        self.clear(slot);
        self.spans[slot] = (self.arena.len(), s.len());
        self.arena.push_str(s);
    }

    /// Forgets the slot's string. Garbage is kept below half the arena (or
    /// 4 KiB), so the arena stays within about twice the live strings.
    fn clear(&mut self, slot: usize) {
        self.garbage += self.spans[slot].1;
        self.spans[slot] = (0, 0);
        if self.garbage > 4096 && self.garbage * 2 > self.arena.len() {
            self.compact();
        }
    }

    /// Rewrites the arena with only the strings still referenced.
    fn compact(&mut self) {
        let mut arena = String::with_capacity(self.arena.len() - self.garbage);
        for span in &mut self.spans {
            let (off, len) = *span;
            *span = (arena.len(), len);
            arena.push_str(&self.arena[off..off + len]);
        }
        self.arena = arena;
        self.garbage = 0;
    }

    fn bytes(&self) -> usize {
        self.spans.capacity() * size_of::<(usize, usize)>() + self.arena.capacity()
    }
}

impl ColData {
    fn grow(&mut self) {
        match self {
            ColData::Pk => {}
            ColData::Int(v) => v.push(0),
            ColData::Str(s) => s.spans.push((0, 0)),
            ColData::Vector { dim, data } => data.resize(data.len() + *dim, 0.0),
        }
    }

    fn set(&mut self, slot: usize, value: Value) {
        match (self, value) {
            (ColData::Pk, _) => {}
            (ColData::Int(c), Value::Int(x)) => c[slot] = x,
            (ColData::Str(c), Value::Str(s)) => c.set(slot, &s),
            (ColData::Vector { dim, data }, Value::Vector(v)) if v.len() == *dim => {
                data[slot * *dim..(slot + 1) * *dim].copy_from_slice(&v);
            }
            _ => debug_assert!(false, "row value does not match its column type"),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            ColData::Pk => 0,
            ColData::Int(v) => v.capacity() * 8,
            ColData::Str(s) => s.bytes(),
            ColData::Vector { data, .. } => data.capacity() * 4,
        }
    }
}

impl RowCache {
    fn new(def: &TableDef) -> RowCache {
        let cols = def
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| match c.ty {
                _ if i == def.pk => ColData::Pk,
                DataType::BigInt => ColData::Int(Vec::new()),
                DataType::Varchar(_) => ColData::Str(StrCol::default()),
                DataType::Vector(dim) => ColData::Vector { dim: dim as usize, data: Vec::new() },
            })
            .collect();
        RowCache { pks: Vec::new(), live: Vec::new(), cols, free: Vec::new() }
    }

    fn insert(&mut self, pk: i64, row: Vec<Value>) -> u32 {
        let slot = match self.free.pop() {
            Some(s) => s as usize,
            None => {
                self.pks.push(0);
                self.live.push(false);
                for c in &mut self.cols {
                    c.grow();
                }
                self.pks.len() - 1
            }
        };
        self.pks[slot] = pk;
        self.live[slot] = true;
        for (c, v) in self.cols.iter_mut().zip(row) {
            c.set(slot, v);
        }
        slot as u32
    }

    fn remove(&mut self, slot: u32) {
        let s = slot as usize;
        if std::mem::replace(&mut self.live[s], false) {
            for c in &mut self.cols {
                if let ColData::Str(sc) = c {
                    sc.clear(s);
                }
            }
            self.free.push(slot);
        }
    }

    #[inline]
    fn get(&self, slot: usize, col: usize) -> ValRef<'_> {
        match &self.cols[col] {
            ColData::Pk => ValRef::Int(self.pks[slot]),
            ColData::Int(v) => ValRef::Int(v[slot]),
            ColData::Str(s) => ValRef::Str(s.get(slot)),
            ColData::Vector { dim, data } => ValRef::Vector(&data[slot * dim..(slot + 1) * dim]),
        }
    }

    fn view(&self, slot: usize) -> RowView<'_> {
        RowView { pk: self.pks[slot], src: Src::Cache(self, slot) }
    }

    fn bytes(&self) -> usize {
        self.pks.capacity() * 8
            + self.live.capacity()
            + self.free.capacity() * 4
            + self.cols.iter().map(ColData::bytes).sum::<usize>()
    }
}

/// A column predicate bound to the cached column it reads.
enum Probe<'a> {
    Int(&'a [i64], CmpOp, i64),
    Str(&'a StrCol, CmpOp, &'a [u8]),
}

impl<'a> Probe<'a> {
    fn new(cache: &'a RowCache, p: &ColPred<'a>) -> Option<Probe<'a>> {
        Some(match (cache.cols.get(p.col)?, &p.value) {
            (ColData::Pk, PredValue::Int(v)) => Probe::Int(&cache.pks, p.op, *v),
            (ColData::Int(c), PredValue::Int(v)) => Probe::Int(c, p.op, *v),
            (ColData::Str(c), PredValue::Str(v)) => Probe::Str(c, p.op, v.as_bytes()),
            _ => return None,
        })
    }

    #[inline]
    fn holds(&self, slot: usize) -> bool {
        match self {
            Probe::Int(c, op, v) => holds(*op, c[slot].cmp(v)),
            Probe::Str(c, op, v) => {
                let (off, len) = c.spans[slot];
                holds(*op, c.arena.as_bytes()[off..off + len].cmp(v))
            }
        }
    }
}

pub struct Table {
    pub def: TableDef,
    pub rows: BTreeMap<i64, Entry>,
    /// When present, holds every row of `rows`.
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
        self.cache.as_ref().map_or(0, RowCache::bytes)
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
                cache.insert(pk, row)
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
            return Ok(Some(cache.view(e.slot as usize).to_values()));
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
        let mut buf = Vec::new();
        for (&pk, e) in self.rows.range(range) {
            let go = match (&self.cache, e.slot != NO_SLOT) {
                (Some(cache), true) => f(cache.view(e.slot as usize))?,
                _ => {
                    log.read(e.loc, &mut buf)?;
                    let values = decode_row(&self.def, pk, &buf)?;
                    f(RowView::new(pk, &values))?
                }
            };
            if !go {
                break;
            }
        }
        Ok(())
    }

    /// Like `scan` restricted to rows where every predicate holds, in no
    /// particular order. A cached table scanned over the whole key range is
    /// walked slot by slot without the index, and the predicates run as
    /// loops over their column arrays before any row is looked at.
    pub fn scan_unordered(
        &self,
        log: &LogStore,
        range: RangeInclusive<i64>,
        preds: &[ColPred<'_>],
        mut f: impl FnMut(RowView<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let whole = *range.start() == i64::MIN && *range.end() == i64::MAX;
        let probes = match &self.cache {
            Some(cache) if whole => preds.iter().map(|p| Probe::new(cache, p)).collect::<Option<Vec<_>>>(),
            _ => None,
        };
        let (Some(cache), Some(probes)) = (&self.cache, probes) else {
            return self.scan(log, range, |row| if preds.iter().all(|p| p.test(&row)) { f(row) } else { Ok(true) });
        };
        let Some((first, others)) = probes.split_first() else {
            for (slot, &live) in cache.live.iter().enumerate() {
                if live && !f(cache.view(slot))? {
                    break;
                }
            }
            return Ok(());
        };
        let mut sel: Vec<u32> =
            (0..cache.live.len()).filter(|&s| cache.live[s] && first.holds(s)).map(|s| s as u32).collect();
        for p in others {
            sel.retain(|&s| p.holds(s as usize));
        }
        for s in sel {
            if !f(cache.view(s as usize))? {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::catalog::Column;

    fn def() -> TableDef {
        TableDef {
            id: 1,
            name: "t".into(),
            columns: vec![
                Column { name: "s".into(), ty: DataType::Varchar(8) },
                Column { name: "id".into(), ty: DataType::BigInt },
                Column { name: "v".into(), ty: DataType::Vector(2) },
                Column { name: "n".into(), ty: DataType::BigInt },
            ],
            pk: 1,
            vector_index: None,
        }
    }

    fn row(pk: i64, s: &str, n: i64) -> Vec<Value> {
        vec![Value::Str(s.into()), Value::Int(pk), Value::Vector(vec![pk as f32, -1.0]), Value::Int(n)]
    }

    /// Random puts and deletes against a model map, with string sizes that
    /// force arena compaction and slot reuse.
    #[test]
    fn cache_matches_model_through_updates_deletes_and_compaction() {
        let def = def();
        let mut cache = RowCache::new(&def);
        let mut slots: BTreeMap<i64, u32> = BTreeMap::new();
        let mut model: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
        let mut x = 12345u64;
        for step in 0..20_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let pk = ((x >> 33) % 300) as i64 - 150;
            if (x >> 20).is_multiple_of(4) {
                if let Some(slot) = slots.remove(&pk) {
                    cache.remove(slot);
                }
                model.remove(&pk);
            } else {
                let s = "é".repeat(((x >> 40) % 6) as usize) + &step.to_string();
                if let Some(slot) = slots.remove(&pk) {
                    cache.remove(slot);
                }
                slots.insert(pk, cache.insert(pk, row(pk, &s, step)));
                model.insert(pk, row(pk, &s, step));
            }
        }
        for (pk, slot) in &slots {
            let view = cache.view(*slot as usize);
            assert_eq!(view.pk, *pk);
            assert_eq!(view.to_values(), model[pk]);
        }
        assert_eq!(cache.live.iter().filter(|l| **l).count(), model.len());
        let ColData::Str(s) = &cache.cols[0] else { panic!() };
        let live_bytes: usize = model.values().map(|r| if let Value::Str(s) = &r[0] { s.len() } else { 0 }).sum();
        assert!(s.arena.len() <= 2 * live_bytes + 4096 + 64, "arena {} for {} live bytes", s.arena.len(), live_bytes);
    }
}
