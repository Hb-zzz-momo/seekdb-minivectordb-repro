//! Statement execution on top of the commit log: catalog, transactions,
//! DDL and DML.
//!
//! Every statement writes into the transaction overlay (`txn`). Outside
//! BEGIN ... COMMIT the overlay is committed right after the statement. A
//! commit turns the overlay into one log frame, then applies it to the
//! in-memory tables. DDL and BEGIN implicitly commit an open transaction.

pub mod expr;
mod ivf;
mod query;
pub mod result;
mod table;
mod topk;
mod txn;

use std::collections::HashMap;
use std::path::Path;

use crate::error::DbResult;
use crate::sql::ast::{CreateTable, Statement};
use crate::storage::catalog::{Column, TableDef};
use crate::storage::log::{LogOptions, LogStore, Op, OpView};
use crate::storage::row::{decode_row, encode_row};
use crate::types::DataType;
use crate::{bail, db_err};
use result::ExecResult;
use table::Table;
use txn::Txn;

pub struct EngineOptions {
    pub log: LogOptions,
    /// Bytes of decoded rows kept in memory across all tables.
    pub cache_budget: usize,
    /// Tables with at least this many rows get a vector index for TopK.
    pub ivf_min_rows: usize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        // MVDB_DROP_CACHE=0 is for benchmarks only.
        let drop_cache = std::env::var("MVDB_DROP_CACHE").map_or(true, |v| v != "0");
        EngineOptions {
            log: LogOptions { drop_cache, ..LogOptions::default() },
            cache_budget: default_cache_budget(),
            ivf_min_rows: default_ivf_min_rows(),
        }
    }
}

/// `MVDB_IVF=0` disables the vector index (for comparisons);
/// `MVDB_IVF_MIN_ROWS` changes the table size from which it is used (tests
/// set it low to exercise the index on small tables).
pub fn default_ivf_min_rows() -> usize {
    if std::env::var("MVDB_IVF").is_ok_and(|v| v == "0") {
        return usize::MAX;
    }
    std::env::var("MVDB_IVF_MIN_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(4096)
}

/// `MVDB_CACHE_BYTES`, else a quarter of the cgroup memory limit, else 256 MiB.
pub fn default_cache_budget() -> usize {
    if let Some(n) = std::env::var("MVDB_CACHE_BYTES").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    let limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max").ok().and_then(|s| s.trim().parse::<u64>().ok());
    match limit {
        Some(l) => ((l / 4) as usize).clamp(4 << 20, 1 << 30),
        None => 256 << 20,
    }
}

/// Committed catalog and table state.
struct Tables {
    by_id: HashMap<u32, Table>,
    by_name: HashMap<String, u32>,
    next_id: u32,
    cache_budget: usize,
    ivf_min_rows: usize,
}

impl Tables {
    fn lookup(&self, name: &str) -> DbResult<u32> {
        self.by_name
            .get(&name.to_ascii_lowercase())
            .copied()
            .ok_or_else(|| db_err!(NoSuchTable, "table '{}' doesn't exist", name))
    }

    fn table(&self, id: u32) -> DbResult<&Table> {
        self.by_id.get(&id).ok_or_else(|| db_err!(Internal, "table id {} missing", id))
    }

    fn create(&mut self, def: TableDef) {
        self.next_id = self.next_id.max(def.id + 1);
        self.by_name.insert(def.name.to_ascii_lowercase(), def.id);
        self.by_id.insert(def.id, Table::new(def));
    }

    fn drop(&mut self, id: u32) {
        if let Some(t) = self.by_id.remove(&id) {
            let key = t.def.name.to_ascii_lowercase();
            if self.by_name.get(&key) == Some(&id) {
                self.by_name.remove(&key);
            }
        }
    }

    fn replay(&mut self, ops: &[OpView<'_>]) -> DbResult<()> {
        for op in ops {
            match op {
                OpView::CreateTable(def) => self.create(def.clone()),
                OpView::DropTable { table } => self.drop(*table),
                OpView::CreateIndex { table, name } => {
                    if let Some(t) = self.by_id.get_mut(table) {
                        t.def.vector_index = Some(name.clone());
                    }
                }
                OpView::Put { table, pk, row, loc } => {
                    if let Some(t) = self.by_id.get_mut(table) {
                        let values = if t.is_cached() { Some(decode_row(&t.def, *pk, row)?) } else { None };
                        t.apply_put(*pk, *loc, values);
                    }
                }
                OpView::Delete { table, pk } => {
                    if let Some(t) = self.by_id.get_mut(table) {
                        t.apply_delete(*pk);
                    }
                }
            }
        }
        self.enforce_budget();
        Ok(())
    }

    /// Drops row caches, largest first, until they fit the budget.
    fn enforce_budget(&mut self) {
        loop {
            let total: usize = self.by_id.values().map(Table::cache_bytes).sum();
            if total <= self.cache_budget {
                return;
            }
            let Some(t) = self.by_id.values_mut().filter(|t| t.is_cached()).max_by_key(|t| t.cache_bytes()) else {
                return;
            };
            eprintln!(
                "engine: table {} no longer fits the {} byte row cache; reading it from disk",
                t.def.name, self.cache_budget
            );
            t.drop_cache();
        }
    }
}

pub struct Engine {
    log: LogStore,
    tables: Tables,
    txn: Txn,
    vars: HashMap<String, String>,
}

impl Engine {
    /// Opens the database in `dir`, recovering committed state from the log.
    pub fn open(dir: &Path, opts: EngineOptions) -> DbResult<Engine> {
        let mut tables = Tables {
            by_id: HashMap::new(),
            by_name: HashMap::new(),
            next_id: 1,
            cache_budget: opts.cache_budget,
            ivf_min_rows: opts.ivf_min_rows,
        };
        let log = LogStore::open(dir, opts.log, |_, ops| tables.replay(ops))?;
        // Index builds are part of recovery, before the database reports
        // ready, so the first query does not pay for one.
        for t in tables.by_id.values_mut() {
            t.prepare_index(opts.ivf_min_rows);
        }
        tables.enforce_budget();
        Ok(Engine { log, tables, txn: Txn::default(), vars: HashMap::new() })
    }

    pub fn in_transaction(&self) -> bool {
        self.txn.explicit
    }

    /// Session variable set with `SET name = value` (name lower-cased).
    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// Discards the open transaction, e.g. after an internal error.
    pub fn abort_transaction(&mut self) {
        self.txn.clear();
        self.txn.explicit = false;
    }

    pub fn execute(&mut self, stmt: &Statement) -> DbResult<ExecResult> {
        match stmt {
            Statement::Begin => {
                let r = self.commit();
                self.txn.explicit = r.is_ok();
                r.map(|_| ExecResult::ok(0))
            }
            Statement::Commit => {
                let r = self.commit();
                self.txn.explicit = false;
                r.map(|_| ExecResult::ok(0))
            }
            Statement::Rollback => {
                self.abort_transaction();
                Ok(ExecResult::ok(0))
            }
            Statement::CreateTable(_) | Statement::DropTable { .. } | Statement::CreateVectorIndex(_) => {
                let r = self.commit();
                self.txn.explicit = false;
                r?;
                self.ddl(stmt)
            }
            Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => {
                self.txn.begin_statement();
                let r = match stmt {
                    Statement::Insert(s) => query::insert(&self.tables, &mut self.txn, s),
                    Statement::Update(s) => query::update(&self.log, &self.tables, &mut self.txn, s),
                    Statement::Delete(s) => query::delete(&self.log, &self.tables, &mut self.txn, s),
                    _ => unreachable!(),
                };
                match r {
                    Ok(res) => {
                        if !self.txn.explicit {
                            self.commit()?;
                        }
                        Ok(res)
                    }
                    Err(e) => {
                        self.txn.undo_statement();
                        Err(e)
                    }
                }
            }
            Statement::Select(s) => {
                // A likely TopK: build the vector index first if the table
                // has outgrown the one it has (a no-op otherwise).
                if s.order_by.len() == 1 && s.limit.is_some() {
                    if let Some(t) = self.tables.lookup(&s.table).ok().and_then(|id| self.tables.by_id.get_mut(&id)) {
                        t.prepare_index(self.tables.ivf_min_rows);
                    }
                }
                query::select(&self.log, &self.tables, &self.txn, s)
            }
            Statement::Set { name, value } => {
                use crate::sql::ast::{Expr, Literal};
                let v = match value {
                    Expr::Lit(Literal::Str(s)) | Expr::Lit(Literal::Num(s)) => s.clone(),
                    Expr::Lit(Literal::Int(i)) => i.to_string(),
                    Expr::Column { table: None, name } => name.clone(),
                    _ => bail!(Unsupported, "unsupported value for SET {}", name),
                };
                self.vars.insert(name.to_ascii_lowercase(), v);
                Ok(ExecResult::ok(0))
            }
        }
    }

    /// Makes the transaction's changes durable as one log frame, then applies
    /// them. On error the changes are discarded.
    fn commit(&mut self) -> DbResult<()> {
        let changes = self.txn.take_changes();
        let mut ops = Vec::new();
        for (&id, rows) in &changes {
            let table = self.tables.table(id)?;
            for (&pk, row) in rows {
                match row {
                    Some(values) => {
                        let mut buf = Vec::new();
                        encode_row(&table.def, values, &mut buf)?;
                        ops.push(Op::Put { table: id, pk, row: buf });
                    }
                    None if table.rows.contains_key(&pk) => ops.push(Op::Delete { table: id, pk }),
                    None => {}
                }
            }
        }
        if ops.is_empty() {
            return Ok(());
        }
        let mut locs = self.log.append(&ops)?.into_iter();
        for (id, rows) in changes {
            let Some(table) = self.tables.by_id.get_mut(&id) else { continue };
            for (pk, row) in rows {
                match row {
                    Some(values) => match locs.next() {
                        Some(loc) => table.apply_put(pk, loc, Some(values)),
                        None => bail!(Internal, "commit lost track of row locations"),
                    },
                    None => table.apply_delete(pk),
                }
            }
        }
        self.tables.enforce_budget();
        Ok(())
    }

    fn ddl(&mut self, stmt: &Statement) -> DbResult<ExecResult> {
        match stmt {
            Statement::CreateTable(ct) => {
                if self.tables.by_name.contains_key(&ct.name.to_ascii_lowercase()) {
                    if ct.if_not_exists {
                        return Ok(ExecResult::ok(0));
                    }
                    bail!(TableExists, "table '{}' already exists", ct.name);
                }
                let def = table_def(ct, self.tables.next_id)?;
                self.log.append(&[Op::CreateTable(def.clone())])?;
                self.tables.create(def);
            }
            Statement::DropTable { name, if_exists } => {
                let id = match self.tables.lookup(name) {
                    Ok(id) => id,
                    Err(_) if *if_exists => return Ok(ExecResult::ok(0)),
                    Err(e) => return Err(e),
                };
                self.log.append(&[Op::DropTable { table: id }])?;
                self.tables.drop(id);
            }
            Statement::CreateVectorIndex(ix) => {
                let id = self.tables.lookup(&ix.table)?;
                let def = &self.tables.table(id)?.def;
                let col = def.column(&ix.column).ok_or_else(|| db_err!(NoSuchColumn, "unknown column '{}'", ix.column))?;
                if !matches!(def.columns[col].ty, DataType::Vector(_)) {
                    bail!(Schema, "column '{}' is not a vector column", ix.column);
                }
                if let Some(existing) = &def.vector_index {
                    bail!(Schema, "table '{}' already has vector index '{}'", def.name, existing);
                }
                self.log.append(&[Op::CreateIndex { table: id, name: ix.name.clone() }])?;
                if let Some(t) = self.tables.by_id.get_mut(&id) {
                    t.def.vector_index = Some(ix.name.clone());
                    t.prepare_index(self.tables.ivf_min_rows);
                }
                self.tables.enforce_budget();
            }
            _ => bail!(Internal, "not a DDL statement"),
        }
        Ok(ExecResult::ok(0))
    }
}

fn table_def(ct: &CreateTable, id: u32) -> DbResult<TableDef> {
    let mut columns: Vec<Column> = Vec::with_capacity(ct.columns.len());
    let mut pk = None;
    for (i, c) in ct.columns.iter().enumerate() {
        if columns.iter().any(|x| x.name.eq_ignore_ascii_case(&c.name)) {
            bail!(Schema, "duplicate column name '{}'", c.name);
        }
        if c.primary_key {
            if pk.is_some() {
                bail!(Schema, "multiple primary keys defined");
            }
            pk = Some(i);
        }
        columns.push(Column { name: c.name.clone(), ty: c.ty });
    }
    if let Some(cols) = &ct.primary_key {
        if pk.is_some() {
            bail!(Schema, "multiple primary keys defined");
        }
        if cols.len() != 1 {
            bail!(Unsupported, "composite primary keys are not supported");
        }
        let i = columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&cols[0]))
            .ok_or_else(|| db_err!(NoSuchColumn, "key column '{}' doesn't exist in table", cols[0]))?;
        pk = Some(i);
    }
    let Some(pk) = pk else {
        bail!(Schema, "table '{}' needs a BIGINT primary key", ct.name);
    };
    if columns[pk].ty != DataType::BigInt {
        bail!(Schema, "primary key column '{}' must be BIGINT", columns[pk].name);
    }
    if columns.iter().filter(|c| matches!(c.ty, DataType::Vector(_))).count() > 1 {
        bail!(Schema, "a table can have at most one vector column");
    }
    Ok(TableDef { id, name: ct.name.clone(), columns, pk, vector_index: None })
}
