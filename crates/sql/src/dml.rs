//! INSERT, UPDATE and DELETE (with THEN RETURN), applied to the transaction's private view.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use googlesql_sys::proto::any_resolved_expr_proto::Node as E;
use googlesql_sys::proto::any_resolved_statement_proto::Node as StmtNode;
use googlesql_sys::proto::resolved_insert_stmt_enums::InsertMode;
use googlesql_sys::proto::*;
use spanbolt_proto::spanner as pb;
use spanbolt_storage::key::{encode_key, prefix_end};
use spanbolt_storage::{
    Code, Error, IndexId, KeySet, Mutation, Result, TableId, TableMeta, Timestamp, Value,
    WriteKind, index_prefix,
};
use spanbolt_txn::ReadWriteTxn;

use crate::catalog::SqlCatalog;
use crate::engine::metadata;
use crate::exec::{BoxOp, Cx, Env, Eval, Row};
use crate::plan::{Planner, Scope, call_base, col_type, fn_name};
use crate::prepared::Prepared;
use crate::types::{Type, unimplemented};
use crate::validate::Node;

/// A value written by DML: an expression, the commit timestamp, or NULL (DEFAULT without one).
enum Val {
    Expr(Eval),
    CommitTs,
    /// The DEFAULT keyword: the column's default, or NULL without one.
    Default,
}

enum Source {
    Values(Vec<Vec<Val>>),
    Query { op: BoxOp, positions: Vec<usize> },
}

/// The conflicts an INSERT skips instead of failing (INSERT OR IGNORE, ON CONFLICT DO NOTHING).
#[derive(Default)]
struct Skip {
    /// A row whose primary key exists.
    key: bool,
    /// A row whose key in one of these unique indexes exists.
    indexes: Vec<IndexId>,
}

enum Kind {
    Insert {
        mode: WriteKind,
        ignore: Skip,
        columns: Vec<usize>,
        source: Source,
    },
    Update {
        scan: BoxOp,
        sets: Vec<(usize, Val)>,
    },
    Delete {
        scan: BoxOp,
    },
}

struct Returning {
    /// The target table's columns the expressions read (pending commit timestamps fail them).
    columns: Vec<usize>,
    exprs: Vec<Eval>,
    /// Positions in `row ++ computed ++ [action]`.
    positions: Vec<usize>,
}

pub struct DmlPlan {
    table: TableId,
    kind: Kind,
    returning: Option<Returning>,
    /// The statement reads other rows through subqueries (not partitionable).
    has_subquery: bool,
}

fn nid(c: &Option<ResolvedColumnProto>) -> i64 {
    c.as_ref().and_then(|c| c.column_id).unwrap_or(-1)
}

fn is_pct(e: &AnyResolvedExprProto) -> bool {
    call_base(e).is_some_and(|b| fn_name(b) == "pending_commit_timestamp")
}

fn value(p: &mut Planner, dv: Option<&ResolvedDmlValueProto>, scope: &Scope) -> Result<Val> {
    let e = dv
        .and_then(|d| d.value.as_ref())
        .ok_or_else(|| Error::internal("DML value without expression"))?;
    if is_pct(e) {
        return Ok(Val::CommitTs);
    }
    if let Some(E::ResolvedDmldefaultNode(_)) = e.node {
        return Ok(Val::Default);
    }
    Ok(Val::Expr(p.expr(e, scope)?))
}

fn has_default(p: &Planner, table: TableId, col: usize) -> bool {
    let tm = p.cat.storage.table(table);
    p.cat
        .schema
        .table(&tm.name)
        .and_then(|t| t.columns.get(col))
        .is_some_and(|c| c.default.is_some() || c.generated.is_some())
}

fn table_layout(ts: &ResolvedTableScanProto, n: usize) -> Vec<i64> {
    let mut layout: Vec<i64> = (0..n).map(|c| -1_000_000 - c as i64).collect();
    if let Some(base) = &ts.parent {
        for (c, &i) in base.column_list.iter().zip(&ts.column_index_list) {
            if let Some(slot) = layout.get_mut(i as usize) {
                *slot = c.column_id.unwrap_or(-1);
            }
        }
    }
    layout
}

type Fields = Vec<(String, Type)>;

fn returning(
    p: &mut Planner,
    r: &Option<ResolvedReturningClauseProto>,
    ts: &ResolvedTableScanProto,
    layout: &[i64],
) -> Result<(Option<Returning>, Fields)> {
    let Some(r) = r else {
        return Ok((None, Vec::new()));
    };
    let read: Vec<&AnyResolvedExprProto> = r
        .expr_list
        .iter()
        .filter_map(|cc| cc.expr.as_deref())
        .collect();
    let mut columns = crate::plan::columns_read(&read, ts, layout);
    // Plain column references are output columns without an expression.
    for oc in &r.output_column_list {
        if let Some(c) = layout.iter().position(|&l| l == nid(&oc.column))
            && !columns.contains(&c)
        {
            columns.push(c);
        }
    }
    let scope = Scope {
        cols: layout,
        up: Some(&Scope::TOP),
    };
    let mut ids: Vec<i64> = layout.to_vec();
    let mut exprs = Vec::new();
    for cc in &r.expr_list {
        let e = cc
            .expr
            .as_ref()
            .ok_or_else(|| Error::internal("returning without expr"))?;
        exprs.push(p.expr(e, &scope)?);
        ids.push(nid(&cc.column));
    }
    if let Some(a) = &r.action_column {
        ids.push(nid(&a.column));
    }
    let mut positions = Vec::new();
    let mut fields = Vec::new();
    for oc in &r.output_column_list {
        let c = oc
            .column
            .as_ref()
            .ok_or_else(|| Error::internal("output without column"))?;
        let id = c.column_id.unwrap_or(-1);
        positions.push(
            ids.iter()
                .position(|&x| x == id)
                .ok_or_else(|| Error::internal("THEN RETURN column not found"))?,
        );
        fields.push((oc.name.clone().unwrap_or_default(), col_type(c)?));
    }
    Ok((
        Some(Returning {
            columns,
            exprs,
            positions,
        }),
        fields,
    ))
}

fn insert(p: &mut Planner, ins: &ResolvedInsertStmtProto) -> Result<(DmlPlan, Fields)> {
    if let Some(b) = &ins.parent {
        p.statement_hints(Node::Insert, &b.hint_list)?;
    }
    let ts = ins
        .table_scan
        .as_ref()
        .ok_or_else(|| Error::internal("insert without table"))?;
    let (_, _, table) = p.dml_scan(ts, None)?;
    let ncols = p.cat.storage.table(table).columns.len();
    let layout = table_layout(ts, ncols);
    let columns: Vec<usize> = ins
        .insert_column_list
        .iter()
        .map(|c| {
            let id = c.column_id.unwrap_or(-1);
            layout
                .iter()
                .position(|&l| l == id)
                .ok_or_else(|| Error::internal("insert column not in table"))
        })
        .collect::<Result<_>>()?;
    let (mode, key) = match ins.insert_mode.and_then(|m| InsertMode::try_from(m).ok()) {
        Some(InsertMode::OrIgnore) => (WriteKind::Insert, true),
        Some(InsertMode::OrUpdate) => (WriteKind::InsertOrUpdate, false),
        Some(InsertMode::OrReplace) => (WriteKind::Replace, false),
        _ => (WriteKind::Insert, false),
    };
    let ignore = match &ins.on_conflict_clause {
        Some(oc) => on_conflict(p, oc, table, &layout)?,
        None => Skip {
            key,
            indexes: Vec::new(),
        },
    };
    let source = if let Some(q) = &ins.query {
        let planned = p.scan(q, &Scope::TOP)?;
        let ids: Vec<i64> = ins
            .query_output_column_list
            .iter()
            .map(|c| c.column_id.unwrap_or(-1))
            .collect();
        let positions = crate::plan::positions(&planned.layout, &ids)?;
        Source::Query {
            op: planned.op,
            positions,
        }
    } else {
        let mut rows = Vec::new();
        for r in &ins.row_list {
            let mut vals = Vec::new();
            for (i, v) in r.value_list.iter().enumerate() {
                let _ = i;
                vals.push(value(p, Some(v), &Scope::TOP)?);
            }
            rows.push(vals);
        }
        Source::Values(rows)
    };
    let (ret, fields) = returning(p, &ins.returning, ts, &layout)?;
    Ok((
        DmlPlan {
            table,
            kind: Kind::Insert {
                mode,
                ignore,
                columns,
                source,
            },
            returning: ret,
            has_subquery: false,
        },
        fields,
    ))
}

/// A unique index and the encoded index key (without the primary key suffix) a row would have.
type IndexKey = (IndexId, Vec<u8>);

/// The keys each row would have in the unique indexes ON CONFLICT DO NOTHING checks. Two rows
/// with the same key are UNIMPLEMENTED: which one Cloud Spanner keeps follows no rule we found
/// (sometimes the first, sometimes the last, sometimes neither).
fn conflict_index_keys(
    state: &spanbolt_storage::DbState,
    tm: &TableMeta,
    indexes: &[IndexId],
    columns: &[usize],
    rows: &[Vec<Value>],
    defaults: &[(usize, usize)],
) -> Result<Vec<Vec<IndexKey>>> {
    if indexes.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen: HashSet<IndexKey> = HashSet::new();
    let mut out = Vec::with_capacity(rows.len());
    for (r, row) in rows.iter().enumerate() {
        let mut full = vec![Value::Null; tm.columns.len()];
        let mut supplied = vec![false; tm.columns.len()];
        for (i, &c) in columns.iter().enumerate() {
            if !defaults.contains(&(r, i)) {
                full[c] = row[i].clone();
                supplied[c] = true;
            }
        }
        let mut keys = Vec::new();
        for &ix in indexes {
            let im = state.catalog.index(ix);
            if let Some(&c) = im.key.iter().find(|&&c| {
                let cm = &tm.columns[c];
                !supplied[c] && (cm.default.is_some() || cm.generated || cm.identity)
            }) {
                return Err(unimplemented(format!(
                    "ON CONFLICT DO NOTHING on index {} with a default or generated value for column {}",
                    im.name, tm.columns[c].name
                )));
            }
            let Some(pf) = index_prefix(im, &full)? else {
                continue;
            };
            if !seen.insert((ix, pf.clone())) {
                return Err(unimplemented(format!(
                    "ON CONFLICT DO NOTHING with two rows that have the same key in unique index {}",
                    im.name
                )));
            }
            keys.push((ix, pf));
        }
        out.push(keys);
    }
    Ok(out)
}

/// `ON CONFLICT [target] DO NOTHING`: without a target Cloud Spanner skips rows that clash on
/// the primary key or any unique index; a target names the primary key or one unique index.
fn on_conflict(
    p: &Planner,
    oc: &ResolvedOnConflictClauseProto,
    table: TableId,
    layout: &[i64],
) -> Result<Skip> {
    use googlesql_sys::proto::resolved_on_conflict_clause_enums::ConflictAction;
    if oc
        .conflict_action
        .and_then(|a| ConflictAction::try_from(a).ok())
        != Some(ConflictAction::Nothing)
    {
        return Err(unimplemented("INSERT ... ON CONFLICT DO UPDATE"));
    }
    let storage = &p.cat.storage;
    let tm = storage.table(table);
    let unique: Vec<IndexId> = tm
        .indexes
        .iter()
        .copied()
        .filter(|&ix| {
            let im = storage.index(ix);
            im.unique && !im.managed
        })
        .collect();
    let compat = p.opts.emulator_compat;
    let no_match = || {
        Error::new(
            if compat {
                Code::FailedPrecondition
            } else {
                Code::InvalidArgument
            },
            "There is no unique constraint matching the ON CONFLICT specification",
        )
    };
    // Neither Spanner nor the emulator takes a null-filtered unique index as the target; Spanner
    // also refuses one named by ON UNIQUE CONSTRAINT, and an empty target when the table has one.
    let null_filtered = |ix: IndexId| -> Result<()> {
        let im = storage.index(ix);
        if !im.null_filtered {
            return Ok(());
        }
        Err(Error::new(
            Code::Unimplemented,
            if compat {
                format!(
                    "Null filtered index as conflict target is not supported: {}",
                    im.name
                )
            } else {
                format!(
                    "ON CONFLICT clause with null filtering index as conflict target is not supported. Index: {}",
                    im.name
                )
            },
        ))
    };
    if let Some(name) = oc
        .unique_constraint_name
        .as_deref()
        .filter(|n| !n.is_empty())
    {
        let ix = unique
            .iter()
            .copied()
            .find(|&ix| storage.index(ix).name.eq_ignore_ascii_case(name))
            .ok_or_else(no_match)?;
        // The emulator only checks a target column list, not a named constraint.
        if !compat {
            null_filtered(ix)?;
        }
        return Ok(Skip {
            key: false,
            indexes: vec![ix],
        });
    }
    if oc.conflict_target_column_list.is_empty() {
        if compat {
            return Err(Error::new(
                Code::Unimplemented,
                "ON CONFLICT clause with empty conflict target in INSERT statement is not supported in Emulator",
            ));
        }
        for &ix in &unique {
            null_filtered(ix)?;
        }
        return Ok(Skip {
            key: true,
            indexes: unique,
        });
    }
    let mut target: Vec<usize> = oc
        .conflict_target_column_list
        .iter()
        .map(|c| {
            let id = c.column_id.unwrap_or(-1);
            layout
                .iter()
                .position(|&l| l == id)
                .ok_or_else(|| Error::internal("conflict target not in table"))
        })
        .collect::<Result<_>>()?;
    target.sort_unstable();
    target.dedup();
    let same = |cols: &[usize]| {
        let mut c = cols.to_vec();
        c.sort_unstable();
        c == target
    };
    if same(&tm.pk) {
        return Ok(Skip {
            key: true,
            indexes: Vec::new(),
        });
    }
    let ix = unique
        .into_iter()
        .find(|&ix| same(&storage.index(ix).key))
        .ok_or_else(no_match)?;
    null_filtered(ix)?;
    Ok(Skip {
        key: false,
        indexes: vec![ix],
    })
}

fn update(p: &mut Planner, up: &ResolvedUpdateStmtProto) -> Result<(DmlPlan, Fields)> {
    if let Some(b) = &up.parent {
        p.statement_hints(Node::Update, &b.hint_list)?;
    }
    if up.from_scan.is_some() || up.array_offset_column.is_some() {
        return Err(unimplemented("UPDATE with FROM or nested updates"));
    }
    let ts = up
        .table_scan
        .as_ref()
        .ok_or_else(|| Error::internal("update without table"))?;
    let (scan, layout, table) = p.dml_scan(ts, up.where_expr.as_deref())?;
    let tm = p.cat.storage.table(table).clone();
    let scope = Scope {
        cols: &layout,
        up: Some(&Scope::TOP),
    };
    let mut sets = Vec::new();
    for item in &up.update_item_list {
        if !item.update_item_element_list.is_empty()
            || !item.delete_list.is_empty()
            || !item.update_list.is_empty()
            || !item.insert_list.is_empty()
        {
            return Err(unimplemented("Nested UPDATE items"));
        }
        let target = item
            .target
            .as_ref()
            .ok_or_else(|| Error::internal("update without target"))?;
        let id = match target.node.as_ref() {
            Some(E::ResolvedColumnRefNode(c)) => nid(&c.column),
            _ => return Err(unimplemented("UPDATE of a nested field")),
        };
        let col = layout
            .iter()
            .position(|&l| l == id)
            .ok_or_else(|| Error::internal("update target not in table"))?;
        if tm.is_key_column(col) {
            return Err(Error::invalid_argument(
                "Cannot modify a primary key column with UPDATE",
            ));
        }
        let val = value(p, item.set_value.as_ref(), &scope)?;
        if matches!(val, Val::Default) && has_default(p, table, col) {
            return Err(unimplemented("DEFAULT values in DML"));
        }
        if let Some(e) = item.set_value.as_ref().and_then(|v| v.value.as_ref()) {
            let read = crate::plan::columns_read(&[e], ts, &layout);
            p.reads.push(crate::prepared::ScanRead {
                table,
                index: None,
                columns: read,
            });
        }
        sets.push((col, val));
    }
    let (ret, fields) = returning(p, &up.returning, ts, &layout)?;
    Ok((
        DmlPlan {
            table,
            kind: Kind::Update { scan, sets },
            returning: ret,
            has_subquery: false,
        },
        fields,
    ))
}

fn delete(p: &mut Planner, del: &ResolvedDeleteStmtProto) -> Result<(DmlPlan, Fields)> {
    if let Some(b) = &del.parent {
        p.statement_hints(Node::Delete, &b.hint_list)?;
    }
    if del.using_scan.is_some() || del.array_offset_column.is_some() {
        return Err(unimplemented("DELETE with USING or nested deletes"));
    }
    let ts = del
        .table_scan
        .as_ref()
        .ok_or_else(|| Error::internal("delete without table"))?;
    let (scan, layout, table) = p.dml_scan(ts, del.where_expr.as_deref())?;
    let (ret, fields) = returning(p, &del.returning, ts, &layout)?;
    Ok((
        DmlPlan {
            table,
            kind: Kind::Delete { scan },
            returning: ret,
            has_subquery: false,
        },
        fields,
    ))
}

pub fn plan(p: &mut Planner, node: &StmtNode) -> Result<(DmlPlan, Fields)> {
    let before = p.subqueries;
    let (mut plan, fields) = match node {
        StmtNode::ResolvedInsertStmtNode(ins) => insert(p, ins)?,
        StmtNode::ResolvedUpdateStmtNode(up) => update(p, up)?,
        StmtNode::ResolvedDeleteStmtNode(del) => delete(p, del)?,
        _ => return Err(unimplemented("This statement kind")),
    };
    plan.has_subquery = p.subqueries > before;
    Ok((plan, fields))
}

/// Cloud Spanner rejects an UPDATE that has to rewrite an index entry holding a column the
/// transaction wrote with PENDING_COMMIT_TIMESTAMP() (the emulator allows it since 1.5.57). The
/// error is a constraint error: the commit fails with it too.
fn check_index_rewrite(
    cat: &spanbolt_storage::Catalog,
    tm: &TableMeta,
    table: TableId,
    sets: &[(usize, Val)],
    pending: &[(TableId, usize)],
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    for &ix in &tm.indexes {
        let im = cat.index(ix);
        if !sets.iter().any(|(c, _)| im.entry_columns.contains(c)) {
            continue;
        }
        if let Some(&c) = im
            .entry_columns
            .iter()
            .find(|&&c| pending.contains(&(table, c)) && !sets.iter().any(|(s, _)| *s == c))
        {
            let e = spanbolt_storage::pending::pending_column_error(cat, table, c, true);
            return Err(Error::constraint(e.code, e.message));
        }
    }
    Ok(())
}

fn check_commit_ts(tm: &TableMeta, col: usize) -> Result<()> {
    if tm.columns[col].allow_commit_timestamp {
        return Ok(());
    }
    Err(Error::constraint(
        Code::FailedPrecondition,
        format!(
            "Cannot write commit timestamp because the allow_commit_timestamp column option is not set to true for column {}.{}, or for all corresponding shared key columns in this table's interleaved table hierarchy.",
            tm.name, tm.columns[col].name
        ),
    ))
}

#[allow(clippy::too_many_arguments)]
/// Evaluates one row's written values, appending them to `row` (whose length is the cell
/// offset) and recording commit-timestamp cells as `(r, cell)`.
fn eval_vals(
    cx: &Cx,
    env: &Env,
    vals: &[(usize, &Val)],
    tm: &TableMeta,
    r: usize,
    row: &mut Vec<Value>,
    ts_cells: &mut Vec<(usize, usize)>,
    defaults: &mut Vec<(usize, usize)>,
) -> Result<()> {
    for (col, v) in vals {
        match v {
            Val::Expr(e) => row.push(e(cx, env)?),
            Val::Default => {
                defaults.push((r, row.len()));
                row.push(Value::Null);
            }
            Val::CommitTs => {
                check_commit_ts(tm, *col)?;
                ts_cells.push((r, row.len()));
                row.push(Value::Null);
            }
        }
    }
    Ok(())
}

fn pk_debug(tm: &TableMeta, key: &[Value]) -> String {
    let parts: Vec<String> = tm
        .pk
        .iter()
        .zip(key)
        .map(|(&c, v)| {
            let shown = match v {
                Value::String(s) => format!("\"{s}\""),
                Value::Null => "NULL".to_string(),
                other => crate::cast::to_string(other)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
            };
            format!("pk#{}:{shown}", tm.columns[c].name)
        })
        .collect();
    format!("({{{}}})", parts.join(", "))
}

/// An INSERT's rows with their keys. The key columns and the columns their DEFAULT or generation
/// reads are computed per row as storage stores them; those the statement leaves to a DEFAULT or an
/// identity (absent, or written as `DEFAULT`) are written explicitly, so the key checks, THEN RETURN
/// and the stored row agree on one value (`GENERATE_UUID()` differs on every call). Other omitted
/// columns stay omitted: INSERT OR UPDATE keeps them on an existing row.
struct KeyedRows {
    columns: Vec<usize>,
    rows: Vec<Vec<Value>>,
    keys: Vec<Vec<Value>>,
    /// `(row, position in columns)` of the cells written as `DEFAULT` that storage fills.
    defaults: Vec<(usize, usize)>,
}

fn keyed_rows(
    tm: &TableMeta,
    columns: &[usize],
    mut rows: Vec<Vec<Value>>,
    defaults: Vec<(usize, usize)>,
    now: Timestamp,
) -> Result<KeyedRows> {
    use spanbolt_storage::expr::{compute_row, mentions};
    let fillable = |c: usize| tm.columns[c].default.is_some() || tm.columns[c].identity;
    let key_reads = |c: usize| {
        tm.pk.iter().any(|&k| {
            let km = &tm.columns[k];
            [&km.default, &km.expr]
                .into_iter()
                .flatten()
                .any(|e| mentions(e, &tm.columns[c].name))
        })
    };
    let needed: Vec<bool> = (0..tm.columns.len())
        .map(|c| tm.pk.contains(&c) || key_reads(c))
        .collect();
    let computes = tm.pk.iter().any(|k| !columns.contains(k))
        || defaults.iter().any(|&(_, i)| needed[columns[i]]);
    if !computes {
        let keys = rows
            .iter()
            .map(|row| {
                tm.pk
                    .iter()
                    .map(|k| {
                        columns
                            .iter()
                            .position(|c| c == k)
                            .map_or(Value::Null, |i| row[i].clone())
                    })
                    .collect()
            })
            .collect();
        return Ok(KeyedRows {
            columns: columns.to_vec(),
            rows,
            keys,
            defaults,
        });
    }
    let absent: Vec<usize> = (0..tm.columns.len())
        .filter(|&c| needed[c] && fillable(c) && !columns.contains(&c))
        .collect();
    let mut defaulted: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    for &(r, i) in &defaults {
        defaulted[r].push(i);
    }
    let mut keys = Vec::with_capacity(rows.len());
    for (row, defaulted) in rows.iter_mut().zip(&defaulted) {
        let mut full = vec![Value::Null; tm.columns.len()];
        let mut supplied = vec![false; tm.columns.len()];
        for (i, &c) in columns.iter().enumerate() {
            if !defaulted.contains(&i) {
                full[c] = row[i].clone();
                supplied[c] = true;
            }
        }
        compute_row(tm, &mut full, now, &|c| needed[c] && !supplied[c], &|c| {
            needed[c]
        })?;
        for &i in defaulted.iter().filter(|&&i| needed[columns[i]]) {
            row[i] = full[columns[i]].clone();
        }
        row.extend(absent.iter().map(|&c| full[c].clone()));
        keys.push(tm.pk.iter().map(|&k| full[k].clone()).collect());
    }
    Ok(KeyedRows {
        columns: [columns, &absent].concat(),
        rows,
        keys,
        defaults: defaults
            .into_iter()
            .filter(|&(_, i)| !needed[columns[i]])
            .collect(),
    })
}

pub fn execute(
    cat: &SqlCatalog,
    d: &DmlPlan,
    p: &Prepared,
    params: &[Value],
    rw: &ReadWriteTxn,
    now: Timestamp,
    partitioned: bool,
) -> Result<pb::ResultSet> {
    if partitioned {
        if let Kind::Insert { .. } = d.kind {
            return Err(Error::invalid_argument(
                "INSERT is not supported for Partitioned DML",
            ));
        }
        if d.has_subquery {
            return Err(Error::invalid_argument(
                "Partitioned DML only supports fully partitionable statements.",
            ));
        }
    }
    let state = rw.current_state()?;
    let tm = state.catalog.table(d.table).clone();
    let cx = Cx {
        state: &state,
        cat,
        params,
        now,
        rw: Some(rw),
        with: RefCell::new(Vec::new()),
        memo: RefCell::new(HashMap::new()),
    };
    // THEN RETURN can't read a column the transaction (this statement included) wrote with
    // PENDING_COMMIT_TIMESTAMP().
    let check = |pending: &[(TableId, usize)]| -> Result<()> {
        match &d.returning {
            Some(ret) if ret.columns.iter().any(|&c| pending.contains(&(d.table, c))) => Err(
                Error::invalid_argument(crate::plan::PENDING_COMMIT_TIMESTAMP_USE),
            ),
            _ => Ok(()),
        }
    };
    let top = Env::EMPTY;
    let pk_of = |row: &[Value]| -> Vec<Value> { tm.pk.iter().map(|&c| row[c].clone()).collect() };
    // THEN RETURN rows: deleted rows as they were; inserted/updated ones read back afterwards.
    let mut affected: Vec<(Vec<Value>, &'static str)> = Vec::new();
    let mut keys_after: Vec<(Vec<Value>, &'static str)> = Vec::new();
    let count;
    match &d.kind {
        Kind::Insert {
            mode,
            ignore,
            columns,
            source,
        } => {
            let mut rows: Vec<Vec<Value>> = Vec::new();
            let mut ts_cells = Vec::new();
            let mut defaults: Vec<(usize, usize)> = Vec::new();
            match source {
                Source::Values(vrows) => {
                    for (r, vr) in vrows.iter().enumerate() {
                        let vals: Vec<(usize, &Val)> =
                            columns.iter().copied().zip(vr.iter()).collect();
                        let mut row = Vec::with_capacity(vals.len());
                        eval_vals(
                            &cx,
                            &top,
                            &vals,
                            &tm,
                            r,
                            &mut row,
                            &mut ts_cells,
                            &mut defaults,
                        )?;
                        rows.push(row);
                    }
                }
                Source::Query { op, positions } => {
                    for r in op.open(&cx, &top)? {
                        let r = r?;
                        rows.push(
                            positions
                                .iter()
                                .map(|&i| r.get(i).cloned().unwrap_or(Value::Null))
                                .collect(),
                        );
                    }
                }
            }
            let KeyedRows {
                columns,
                rows,
                keys,
                defaults,
            } = keyed_rows(&tm, columns, rows, defaults, now)?;
            let columns = &columns;
            let mut seen: HashSet<Vec<u8>> = HashSet::new();
            let index_keys =
                conflict_index_keys(&state, &tm, &ignore.indexes, columns, &rows, &defaults)?;
            let mut kept_rows = Vec::new();
            let mut kept_cells = Vec::new();
            let mut kept_defaults: Vec<Vec<usize>> = Vec::new();
            for (r, (row, key)) in rows.into_iter().zip(keys).enumerate() {
                let enc = encode_key(&key, &tm.pk_desc)?;
                let repeated = seen.contains(&enc);
                let exists = repeated || state.tables[d.table].get(&enc[..]).is_some();
                if exists && *mode == WriteKind::Insert {
                    if ignore.key {
                        continue;
                    }
                    return Err(Error::constraint(
                        Code::AlreadyExists,
                        format!(
                            "Failed to insert row with primary key {} due to previously {} row",
                            pk_debug(&tm, &key),
                            if repeated { "inserted" } else { "existing" }
                        ),
                    ));
                }
                if let Some(pfs) = index_keys.get(r)
                    && pfs.iter().any(|(ix, pf)| {
                        let end = prefix_end(pf);
                        state.indexes[*ix]
                            .range::<_, [u8]>((
                                std::ops::Bound::Included(&pf[..]),
                                std::ops::Bound::Excluded(&end[..]),
                            ))
                            .next()
                            .is_some()
                    })
                {
                    continue;
                }
                seen.insert(enc);
                let action = if exists { "UPDATE" } else { "INSERT" };
                let new_r = kept_rows.len();
                kept_cells.extend(
                    ts_cells
                        .iter()
                        .filter(|(cr, _)| *cr == r)
                        .map(|&(_, c)| (new_r, c)),
                );
                keys_after.push((key, action));
                kept_defaults.push(
                    defaults
                        .iter()
                        .filter(|(dr, _)| *dr == r)
                        .map(|&(_, c)| c)
                        .collect(),
                );
                kept_rows.push(row);
            }
            count = kept_rows.len();
            if kept_defaults.iter().all(|d| d.is_empty()) {
                if !kept_rows.is_empty() {
                    rw.apply_dml_checked(
                        vec![Mutation::Write {
                            kind: *mode,
                            table: d.table,
                            columns: columns.clone(),
                            rows: kept_rows,
                            commit_ts_cells: kept_cells,
                        }],
                        &check,
                    )?;
                }
            } else {
                // Rows naming DEFAULT omit those columns so storage fills the defaults.
                let mut muts = Vec::new();
                for (r, (row, defs)) in kept_rows.into_iter().zip(kept_defaults).enumerate() {
                    let keep: Vec<usize> =
                        (0..columns.len()).filter(|i| !defs.contains(i)).collect();
                    let cells = kept_cells
                        .iter()
                        .filter(|(cr, _)| *cr == r)
                        .filter_map(|&(_, c)| keep.iter().position(|&k| k == c).map(|p| (0, p)))
                        .collect();
                    muts.push(Mutation::Write {
                        kind: *mode,
                        table: d.table,
                        columns: keep.iter().map(|&i| columns[i]).collect(),
                        rows: vec![keep.iter().map(|&i| row[i].clone()).collect()],
                        commit_ts_cells: cells,
                    });
                }
                rw.apply_dml_checked(muts, &check)?;
            }
        }
        Kind::Update { scan, sets } => {
            if state.prod_messages {
                check_index_rewrite(&state.catalog, &tm, d.table, sets, &rw.pending_commit_ts())?;
            }
            let olds: Vec<Row> = scan.open(&cx, &top)?.collect::<Result<_>>()?;
            let mut columns: Vec<usize> = tm.pk.clone();
            columns.extend(sets.iter().map(|(c, _)| *c));
            let vals: Vec<(usize, &Val)> = sets.iter().map(|(c, v)| (*c, v)).collect();
            let mut rows = Vec::with_capacity(olds.len());
            let mut ts_cells = Vec::new();
            for (r, old) in olds.iter().enumerate() {
                let env = Env {
                    row: old,
                    up: Some(&top),
                };
                let mut row = pk_of(old);
                eval_vals(
                    &cx,
                    &env,
                    &vals,
                    &tm,
                    r,
                    &mut row,
                    &mut ts_cells,
                    &mut Vec::new(),
                )?;
                keys_after.push((pk_of(old), "UPDATE"));
                rows.push(row);
            }
            count = rows.len();
            if !rows.is_empty() {
                rw.apply_dml_checked(
                    vec![Mutation::Write {
                        kind: WriteKind::Update,
                        table: d.table,
                        columns,
                        rows,
                        commit_ts_cells: ts_cells,
                    }],
                    &check,
                )?;
            }
        }
        Kind::Delete { scan } => {
            let olds: Vec<Row> = scan.open(&cx, &top)?.collect::<Result<_>>()?;
            count = olds.len();
            if !olds.is_empty() {
                let keys = olds.iter().map(|o| pk_of(o)).collect();
                rw.apply_dml_checked(
                    vec![Mutation::Delete {
                        table: d.table,
                        keys: KeySet::keys(keys),
                    }],
                    &check,
                )?;
            }
            affected = olds.into_iter().map(|o| (o, "DELETE")).collect();
        }
    }
    let mut out_rows = Vec::new();
    if let Some(ret) = &d.returning {
        if !keys_after.is_empty() {
            let after = rw.current_state()?;
            for (key, action) in keys_after {
                let enc = encode_key(&key, &tm.pk_desc)?;
                if let Some(row) = after.tables[d.table].get(&enc[..]) {
                    affected.push((row.to_vec(), action));
                }
            }
        }
        for (row, action) in affected {
            let env = Env {
                row: &row,
                up: Some(&top),
            };
            let mut ext = row.clone();
            for e in &ret.exprs {
                ext.push(e(&cx, &env)?);
            }
            ext.push(Value::String(Arc::from(action)));
            out_rows.push(prost_types::ListValue {
                values: ret
                    .positions
                    .iter()
                    .map(|&i| ext.get(i).cloned().unwrap_or(Value::Null).to_proto())
                    .collect(),
            });
        }
    }
    let rc = if partitioned {
        pb::result_set_stats::RowCount::RowCountLowerBound(count as i64)
    } else {
        pb::result_set_stats::RowCount::RowCountExact(count as i64)
    };
    Ok(pb::ResultSet {
        metadata: Some(metadata(&p.fields)),
        rows: out_rows,
        stats: Some(pb::ResultSetStats {
            query_plan: None,
            query_stats: None,
            row_count: Some(rc),
        }),
        ..Default::default()
    })
}
