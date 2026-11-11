//! INSERT, UPDATE, DELETE and SELECT against the rows visible to the current
//! transaction (committed rows with the transaction's own changes on top).

use std::cmp::Ordering;
use std::ops::RangeInclusive;

use super::expr::{compile, compile_filter, literal_value, pk_range, split_filter, CExpr, ColPred, Kind, Scope};
use super::result::{cmp_cells, Cell, RowSink};
use super::table::{RowView, Table, ValRef};
use super::topk::TopK;
use super::txn::{Changes, Txn};
use super::Tables;
use crate::error::DbResult;
use crate::sql::ast::{Delete, Expr, Insert, Literal, Select, SelectItem, Update};
use crate::storage::catalog::TableDef;
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

pub(super) fn insert(tables: &Tables, txn: &mut Txn, ins: &Insert) -> DbResult<u64> {
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
    Ok(ins.rows.len() as u64)
}

/// A resolved UPDATE: the columns to set, each with its value, and the
/// filter. Resolving it reports every error the statement can have.
pub(super) struct UpdatePlan {
    pub sets: Vec<(usize, Value)>,
    filter: Option<CExpr>,
}

pub(super) fn plan_update(def: &TableDef, up: &Update) -> DbResult<UpdatePlan> {
    let mut sets = Vec::with_capacity(up.sets.len());
    for (name, expr) in &up.sets {
        let col = def.column(name).ok_or_else(|| db_err!(NoSuchColumn, "unknown column '{}'", name))?;
        if col == def.pk {
            bail!(Unsupported, "updating the primary key is not supported");
        }
        sets.push((col, literal_value(expr, &def.columns[col])?));
    }
    let filter = compile_filter(up.filter.as_ref(), &Scope { def, alias: None })?;
    Ok(UpdatePlan { sets, filter })
}

/// Whether storing `sets` would change the row.
fn differs(row: &RowView<'_>, sets: &[(usize, Value)]) -> bool {
    sets.iter().any(|(col, v)| match (row.get(*col), v) {
        (ValRef::Int(a), Value::Int(b)) => a != *b,
        (ValRef::Str(a), Value::Str(b)) => a != b,
        (ValRef::Vector(a), Value::Vector(b)) => a.floats(&mut Vec::new()) != b.as_slice(),
        _ => true,
    })
}

/// For an UPDATE outside a transaction: the number of committed rows its
/// WHERE clause matches and, in key order, the keys of those whose values
/// it would change.
pub(super) fn update_targets(log: &LogStore, table: &Table, plan: &UpdatePlan) -> DbResult<(u64, Vec<i64>)> {
    let (mut matched, mut keys) = (0, Vec::new());
    if let Some(range) = pk_range(plan.filter.as_ref(), table.def.pk) {
        table.scan(log, range, |row| {
            if plan.filter.as_ref().is_none_or(|f| f.test(&row)) {
                matched += 1;
                if differs(&row, &plan.sets) {
                    keys.push(row.pk);
                }
            }
            Ok(true)
        })?;
    }
    Ok((matched, keys))
}

/// UPDATE inside a transaction. Returns the rows the WHERE clause matched
/// and, of those, the rows whose values changed. Only the changed rows are
/// written.
pub(super) fn update(log: &LogStore, tables: &Tables, txn: &mut Txn, up: &Update) -> DbResult<(u64, u64)> {
    let id = tables.lookup(&up.table)?;
    let table = tables.table(id)?;
    let def = &table.def;
    let UpdatePlan { sets, filter } = plan_update(def, up)?;
    let mut matched = Vec::new();
    if let Some(range) = pk_range(filter.as_ref(), def.pk) {
        scan_visible(table, log, txn.changes(id), range, &mut |row| {
            if filter.as_ref().is_none_or(|f| f.test(&row)) {
                matched.push(row.to_values());
            }
            Ok(true)
        })?;
    }
    let (matched_rows, mut changed) = (matched.len() as u64, 0);
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
    Ok((matched_rows, changed))
}

/// The keys of the rows a DELETE removes, in key order, among the rows
/// visible with `changes` on top of the committed ones.
pub(super) fn delete_targets(log: &LogStore, table: &Table, changes: Option<&Changes>, del: &Delete) -> DbResult<Vec<i64>> {
    let filter = compile_filter(del.filter.as_ref(), &Scope { def: &table.def, alias: None })?;
    let mut doomed = Vec::new();
    if let Some(range) = pk_range(filter.as_ref(), table.def.pk) {
        scan_visible(table, log, changes, range, &mut |row| {
            if filter.as_ref().is_none_or(|f| f.test(&row)) {
                doomed.push(row.pk);
            }
            Ok(true)
        })?;
    }
    Ok(doomed)
}

/// DELETE inside a transaction.
pub(super) fn delete(log: &LogStore, tables: &Tables, txn: &mut Txn, del: &Delete) -> DbResult<u64> {
    let id = tables.lookup(&del.table)?;
    let doomed = delete_targets(log, tables.table(id)?, txn.changes(id), del)?;
    for &pk in &doomed {
        txn.write(id, pk, None);
    }
    Ok(doomed.len() as u64)
}

/// Runs a SELECT, sending rows to `sink` as they are produced. Only TopK
/// (its K row ids) and ORDER BY on anything but the primary key (sort keys
/// and row ids) hold state for the whole result; no path keeps output rows.
pub(super) fn select(log: &LogStore, tables: &Tables, txn: &Txn, sel: &Select, sink: &mut dyn RowSink) -> DbResult<()> {
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
    sink.columns(&columns)?;
    let Some(range) = pk_range(filter.as_ref(), def.pk) else {
        return Ok(());
    };
    if limit == Some(0) {
        return Ok(());
    }
    let mut cells = Vec::with_capacity(outputs.len());
    let mut emit = |row: &RowView<'_>| -> DbResult<()> {
        cells.clear();
        cells.extend(outputs.iter().map(|e| e.cell(row)));
        sink.row(&cells)
    };
    let mut emit_pk = |pk: i64| -> DbResult<()> {
        let values = get_visible(table, log, changes, pk)?
            .ok_or_else(|| db_err!(Internal, "row {} vanished during a query", pk))?;
        emit(&RowView::new(pk, &values))
    };
    let passes = |row: &RowView<'_>| filter.as_ref().is_none_or(|f| f.test(row));
    // For unordered scans: column predicates go to the scan, the rest is
    // tested per row.
    let (preds, rest) = split_filter(filter.as_ref());
    let passes_rest = |row: &RowView<'_>| rest.iter().all(|e| e.test(row));

    // TopK: ORDER BY L2_DISTANCE(...) ASC LIMIT k; ties go to the smaller pk.
    if let ([(CExpr::Dist { col, query }, false)], Some(limit)) = (order.as_slice(), limit) {
        let mut top = TopK::new(query, offset.saturating_add(limit));
        let mut floats = Vec::new();
        let whole = *range.start() == i64::MIN && *range.end() == i64::MAX;
        if whole && table.is_cached() {
            // Uncommitted rows first: they are few and fill the heap early.
            for (&pk, change) in changes.into_iter().flatten() {
                if let Some(values) = change {
                    let row = RowView::new(pk, values);
                    if preds.iter().all(|p| p.test(&row)) && passes_rest(&row) {
                        if let ValRef::Vector(v) = row.get(*col) {
                            top.offer(pk, v.floats(&mut floats));
                        }
                    }
                }
            }
            let overlaid = |pk: i64| changes.is_some_and(|c| c.contains_key(&pk));
            table.topk(&preds, overlaid, passes_rest, &mut top)?;
        } else {
            scan_visible_unordered(table, log, changes, range, &preds, |row| {
                if passes_rest(&row) {
                    if let ValRef::Vector(v) = row.get(*col) {
                        top.offer(row.pk, v.floats(&mut floats));
                    }
                }
                Ok(true)
            })?;
        }
        for pk in top.into_sorted().into_iter().skip(offset) {
            emit_pk(pk)?;
        }
        return Ok(());
    }

    // No ORDER BY, or ORDER BY the primary key ascending: rows stream out of
    // the scan in pk order, which stops once LIMIT is satisfied.
    if order.is_empty() || matches!(order.as_slice(), [(CExpr::Col(c), false)] if *c == def.pk) {
        let (mut skip, mut left) = (offset, limit.unwrap_or(usize::MAX));
        return scan_visible(table, log, changes, range, &mut |row| {
            if !passes(&row) {
                return Ok(true);
            }
            if skip > 0 {
                skip -= 1;
                return Ok(true);
            }
            emit(&row)?;
            left -= 1;
            Ok(left > 0)
        });
    }

    // Any other ORDER BY: sort the matching rows' keys (pk breaks ties, so
    // the scan may visit rows in any order), then fetch the rows to output.
    let mut found: Vec<(Vec<Cell>, i64)> = Vec::new();
    scan_visible_unordered(table, log, changes, range, &preds, |row| {
        if passes_rest(&row) {
            found.push((order.iter().map(|(e, _)| e.cell(&row)).collect(), row.pk));
        }
        Ok(true)
    })?;
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
    for (_, pk) in found.into_iter().skip(offset).take(limit.unwrap_or(usize::MAX)) {
        emit_pk(pk)?;
    }
    Ok(())
}
