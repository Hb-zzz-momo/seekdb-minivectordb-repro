//! INSERT, UPDATE, DELETE and SELECT against the rows visible to the current
//! transaction (committed rows with the transaction's own changes on top).

use std::cmp::Ordering;
use std::ops::RangeInclusive;

use super::expr::{compile, compile_filter, literal_value, pk_range, split_filter, CExpr, ColPred, Kind, Scope};
use super::result::{cmp_cells, Cell, ExecResult};
use super::table::{RowView, Table, ValRef};
use super::topk::TopK;
use super::txn::{Changes, Txn};
use super::Tables;
use crate::error::DbResult;
use crate::sql::ast::{Delete, Expr, Insert, Literal, Select, SelectItem, Update};
use crate::storage::log::LogStore;
use crate::types::Value;
use crate::{bail, db_err};

/// Visits visible rows with pk in `range`, in pk order, until `f` returns
/// false.
fn scan_visible(
    table: &Table,
    log: &LogStore,
    changes: Option<&Changes>,
    range: RangeInclusive<i64>,
    f: &mut dyn FnMut(RowView<'_>) -> DbResult<bool>,
) -> DbResult<()> {
    let Some(changes) = changes else {
        return table.scan(log, range, f);
    };
    let mut pending = changes.range(range.clone()).peekable();
    let mut stopped = false;
    table.scan(log, range, |row| {
        // Emit pending changes ordered before (or replacing) this row.
        while let Some(&(&pk, change)) = pending.peek() {
            if pk > row.pk {
                break;
            }
            pending.next();
            if let Some(values) = change {
                if !f(RowView::new(pk, values))? {
                    stopped = true;
                    return Ok(false);
                }
            }
            if pk == row.pk {
                return Ok(true);
            }
        }
        let go = f(row)?;
        stopped = !go;
        Ok(go)
    })?;
    if !stopped {
        for (&pk, change) in pending {
            if let Some(values) = change {
                if !f(RowView::new(pk, values))? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Like `scan_visible` restricted to rows where every predicate holds, in no
/// particular order, which lets a cached table be walked slot by slot. For
/// queries whose output order does not depend on the visiting order.
fn scan_visible_unordered(
    table: &Table,
    log: &LogStore,
    changes: Option<&Changes>,
    range: RangeInclusive<i64>,
    preds: &[ColPred<'_>],
    mut f: impl FnMut(RowView<'_>) -> DbResult<bool>,
) -> DbResult<()> {
    let Some(changes) = changes else {
        return table.scan_unordered(log, range, preds, f);
    };
    let mut stopped = false;
    table.scan_unordered(log, range.clone(), preds, |row| {
        if changes.contains_key(&row.pk) {
            return Ok(true);
        }
        let go = f(row)?;
        stopped = !go;
        Ok(go)
    })?;
    if !stopped {
        for (&pk, change) in changes.range(range) {
            if let Some(values) = change {
                let row = RowView::new(pk, values);
                if preds.iter().all(|p| p.test(&row)) && !f(row)? {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn get_visible(table: &Table, log: &LogStore, changes: Option<&Changes>, pk: i64) -> DbResult<Option<Vec<Value>>> {
    match changes.and_then(|c| c.get(&pk)) {
        Some(change) => Ok(change.clone()),
        None => table.get(log, pk),
    }
}

fn exists_visible(table: &Table, changes: Option<&Changes>, pk: i64) -> bool {
    match changes.and_then(|c| c.get(&pk)) {
        Some(change) => change.is_some(),
        None => table.rows.contains_key(&pk),
    }
}

fn pk_of(values: &[Value], pk: usize) -> DbResult<i64> {
    match values.get(pk) {
        Some(Value::Int(v)) => Ok(*v),
        _ => bail!(Internal, "row without an integer primary key"),
    }
}

pub(super) fn insert(tables: &Tables, txn: &mut Txn, ins: &Insert) -> DbResult<ExecResult> {
    let id = tables.lookup(&ins.table)?;
    let table = tables.table(id)?;
    let def = &table.def;
    let positions: Vec<usize> = match &ins.columns {
        Some(cols) => {
            let mut pos = Vec::with_capacity(cols.len());
            for c in cols {
                let i = def.column(c).ok_or_else(|| db_err!(NoSuchColumn, "unknown column '{}'", c))?;
                if pos.contains(&i) {
                    bail!(Schema, "column '{}' specified twice", c);
                }
                pos.push(i);
            }
            if let Some(missing) = (0..def.columns.len()).find(|i| !pos.contains(i)) {
                bail!(NoDefault, "field '{}' doesn't have a default value", def.columns[missing].name);
            }
            pos
        }
        None => (0..def.columns.len()).collect(),
    };
    if let Some(i) = ins.rows.iter().position(|r| r.len() != positions.len()) {
        bail!(ColumnCount, "column count doesn't match value count at row {}", i + 1);
    }
    for row in &ins.rows {
        let mut values = vec![Value::Int(0); def.columns.len()];
        for (expr, &col) in row.iter().zip(&positions) {
            values[col] = literal_value(expr, &def.columns[col])?;
        }
        let pk = pk_of(&values, def.pk)?;
        if exists_visible(table, txn.changes(id), pk) {
            bail!(DuplicateKey, "duplicate entry '{}' for key 'PRIMARY'", pk);
        }
        txn.write(id, pk, Some(values));
    }
    Ok(ExecResult::ok(ins.rows.len() as u64))
}

pub(super) fn update(log: &LogStore, tables: &Tables, txn: &mut Txn, up: &Update) -> DbResult<ExecResult> {
    let id = tables.lookup(&up.table)?;
    let table = tables.table(id)?;
    let def = &table.def;
    let mut sets = Vec::with_capacity(up.sets.len());
    for (name, expr) in &up.sets {
        let col = def.column(name).ok_or_else(|| db_err!(NoSuchColumn, "unknown column '{}'", name))?;
        if col == def.pk {
            bail!(Unsupported, "updating the primary key is not supported");
        }
        sets.push((col, literal_value(expr, &def.columns[col])?));
    }
    let scope = Scope { def, alias: None };
    let filter = compile_filter(up.filter.as_ref(), &scope)?;
    let mut matched = Vec::new();
    if let Some(range) = pk_range(filter.as_ref(), def.pk) {
        scan_visible(table, log, txn.changes(id), range, &mut |row| {
            if filter.as_ref().is_none_or(|f| f.test(&row)) {
                matched.push(row.to_values());
            }
            Ok(true)
        })?;
    }
    let mut changed = 0;
    for mut values in matched {
        let mut diff = false;
        for (col, v) in &sets {
            if values[*col] != *v {
                values[*col] = v.clone();
                diff = true;
            }
        }
        if diff {
            let pk = pk_of(&values, def.pk)?;
            txn.write(id, pk, Some(values));
            changed += 1;
        }
    }
    Ok(ExecResult::ok(changed))
}

pub(super) fn delete(log: &LogStore, tables: &Tables, txn: &mut Txn, del: &Delete) -> DbResult<ExecResult> {
    let id = tables.lookup(&del.table)?;
    let table = tables.table(id)?;
    let scope = Scope { def: &table.def, alias: None };
    let filter = compile_filter(del.filter.as_ref(), &scope)?;
    let mut doomed = Vec::new();
    if let Some(range) = pk_range(filter.as_ref(), table.def.pk) {
        scan_visible(table, log, txn.changes(id), range, &mut |row| {
            if filter.as_ref().is_none_or(|f| f.test(&row)) {
                doomed.push(row.pk);
            }
            Ok(true)
        })?;
    }
    for &pk in &doomed {
        txn.write(id, pk, None);
    }
    Ok(ExecResult::ok(doomed.len() as u64))
}

pub(super) fn select(log: &LogStore, tables: &Tables, txn: &Txn, sel: &Select) -> DbResult<ExecResult> {
    let id = tables.lookup(&sel.table)?;
    let table = tables.table(id)?;
    let def = &table.def;
    let scope = Scope { def, alias: sel.table_alias.as_deref() };

    let mut columns = Vec::new();
    let mut outputs = Vec::new();
    let mut aliases: Vec<(String, usize)> = Vec::new();
    for item in &sel.items {
        match item {
            SelectItem::Star => {
                for (i, c) in def.columns.iter().enumerate() {
                    columns.push(c.name.clone());
                    outputs.push(CExpr::Col(i));
                }
            }
            SelectItem::Expr { expr, alias, text } => {
                let c = compile(expr, &scope)?;
                if c.kind(def) == Kind::Bool {
                    bail!(Unsupported, "conditions in the select list are not supported");
                }
                if let Some(a) = alias {
                    aliases.push((a.clone(), outputs.len()));
                }
                columns.push(match (alias, expr) {
                    (Some(a), _) => a.clone(),
                    (None, Expr::Column { name, .. }) => name.clone(),
                    (None, _) => text.clone(),
                });
                outputs.push(c);
            }
        }
    }

    let filter = compile_filter(sel.filter.as_ref(), &scope)?;

    let mut order = Vec::with_capacity(sel.order_by.len());
    for o in &sel.order_by {
        let e = match &o.expr {
            Expr::Column { table: None, name } if aliases.iter().any(|(a, _)| a.eq_ignore_ascii_case(name)) => {
                let i = aliases.iter().find(|(a, _)| a.eq_ignore_ascii_case(name)).map_or(0, |(_, i)| *i);
                outputs[i].clone()
            }
            Expr::Lit(Literal::Int(n)) => {
                let i = usize::try_from(*n).ok().filter(|&i| i >= 1 && i <= outputs.len());
                match i {
                    Some(i) => outputs[i - 1].clone(),
                    None => bail!(NoSuchColumn, "unknown column '{}' in order clause", n),
                }
            }
            e => compile(e, &scope)?,
        };
        match e.kind(def) {
            Kind::Vector => bail!(Unsupported, "ORDER BY a vector column is not supported"),
            Kind::Bool => bail!(Unsupported, "ORDER BY a condition is not supported"),
            _ => {}
        }
        order.push((e, o.desc));
    }

    let offset = usize::try_from(sel.offset.unwrap_or(0)).unwrap_or(usize::MAX);
    let limit = sel.limit.map(|l| usize::try_from(l).unwrap_or(usize::MAX));
    let changes = txn.changes(id);
    let Some(range) = pk_range(filter.as_ref(), def.pk) else {
        return Ok(ExecResult::Rows { columns, rows: Vec::new() });
    };
    if limit == Some(0) {
        return Ok(ExecResult::Rows { columns, rows: Vec::new() });
    }
    let passes = |row: &RowView<'_>| filter.as_ref().is_none_or(|f| f.test(row));
    // For unordered scans: column predicates go to the scan, the rest is
    // tested per row.
    let (preds, rest) = split_filter(filter.as_ref());
    let passes_rest = |row: &RowView<'_>| rest.iter().all(|e| e.test(row));

    // TopK: ORDER BY L2_DISTANCE(...) ASC LIMIT k; ties go to the smaller pk.
    if let ([(CExpr::Dist { col, query }, false)], Some(limit)) = (order.as_slice(), limit) {
        let mut top = TopK::new(query, offset.saturating_add(limit));
        scan_visible_unordered(table, log, changes, range, &preds, |row| {
            if passes_rest(&row) {
                if let ValRef::Vector(v) = row.get(*col) {
                    top.offer(row.pk, v);
                }
            }
            Ok(true)
        })?;
        let mut rows = Vec::new();
        for pk in top.into_sorted().into_iter().skip(offset) {
            let values = get_visible(table, log, changes, pk)?
                .ok_or_else(|| db_err!(Internal, "row {} vanished during a query", pk))?;
            let view = RowView::new(pk, &values);
            rows.push(outputs.iter().map(|e| e.cell(&view)).collect());
        }
        return Ok(ExecResult::Rows { columns, rows });
    }

    // General path. Without ORDER BY rows come in pk order and we can stop
    // as soon as LIMIT is satisfied; with it, the sort (pk breaks ties)
    // fixes the order, so the scan may visit rows in any order.
    let stop_after = if order.is_empty() { limit.map(|l| offset.saturating_add(l)) } else { None };
    let mut found: Vec<(Vec<Cell>, i64, Vec<Cell>)> = Vec::new();
    let mut keep = |row: RowView<'_>| {
        let keys = order.iter().map(|(e, _)| e.cell(&row)).collect();
        found.push((keys, row.pk, outputs.iter().map(|e| e.cell(&row)).collect()));
        Ok(stop_after.is_none_or(|n| found.len() < n))
    };
    if order.is_empty() {
        scan_visible(table, log, changes, range, &mut |row| if passes(&row) { keep(row) } else { Ok(true) })?;
    } else {
        scan_visible_unordered(table, log, changes, range, &preds, |row| if passes_rest(&row) { keep(row) } else { Ok(true) })?;
    }
    if !order.is_empty() {
        found.sort_by(|a, b| {
            for (i, (_, desc)) in order.iter().enumerate() {
                let o = cmp_cells(&a.0[i], &b.0[i]);
                let o = if *desc { o.reverse() } else { o };
                if o != Ordering::Equal {
                    return o;
                }
            }
            a.1.cmp(&b.1)
        });
    }
    let rows = found.into_iter().skip(offset).take(limit.unwrap_or(usize::MAX)).map(|(_, _, out)| out).collect();
    Ok(ExecResult::Rows { columns, rows })
}
