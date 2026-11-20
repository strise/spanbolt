//! The `ExprEvaluator` seam: once an evaluator is registered, DEFAULT values, stored generated
//! columns and CHECK constraints take effect. Its own test binary, since the registry is global.

use std::sync::Arc;

use prost_types::{ListValue, value::Kind};
use spanbolt_ddl::ColumnType;
use spanbolt_storage::expr::{ExprEvaluator, set_expr_evaluator};
use spanbolt_storage::testing::ddl;
use spanbolt_storage::{
    Code, DbState, KeySet, Mutation, Result, TableMeta, Timestamp, Value, WriteKind, Writer, scan,
};

/// Understands `<col> * <int>`, `<col> > <int>`, integer and string literals, and `NEXT()`, a
/// volatile counter standing in for GENERATE_UUID().
struct Toy;

static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(100);

impl ExprEvaluator for Toy {
    fn eval(&self, expr: &str, t: &TableMeta, row: &[Value], _ty: &ColumnType) -> Result<Value> {
        if expr == "NEXT()" {
            return Ok(Value::Int64(
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ));
        }
        let col = |n: &str| t.column(n.trim()).map(|c| row[c].clone());
        let int = |n: &str| n.trim().parse::<i64>().ok();
        if let Some(s) = expr.strip_prefix('\'').and_then(|e| e.strip_suffix('\'')) {
            return Ok(Value::string(s));
        }
        if let Some(i) = int(expr) {
            return Ok(Value::Int64(i));
        }
        for (op, f) in [
            (
                "*",
                (|a: i64, b: i64| Value::Int64(a * b)) as fn(i64, i64) -> Value,
            ),
            (">", |a, b| Value::Bool(a > b)),
        ] {
            if let Some((l, r)) = expr.split_once(op) {
                return Ok(match (col(l), int(r)) {
                    (Some(Value::Int64(a)), Some(b)) => f(a, b),
                    _ => Value::Null,
                });
            }
        }
        Ok(Value::Null)
    }
}

fn sv(v: &str) -> prost_types::Value {
    prost_types::Value {
        kind: Some(Kind::StringValue(v.into())),
    }
}

fn insert(st: &DbState, cols: &[&str], vals: &[&str]) -> Result<DbState> {
    let cols: Vec<String> = cols.iter().map(|c| c.to_string()).collect();
    let rows = [ListValue {
        values: vals.iter().map(|v| sv(v)).collect(),
    }];
    let m = Mutation::write(&st.catalog, WriteKind::Insert, "T", &cols, &rows)?;
    let mut w = Writer::new(st, Timestamp::default());
    w.apply(&m)?;
    Ok(w.finish()?.0)
}

#[test]
fn evaluator_fills_defaults_generated_columns_and_checks() {
    let st = DbState::new(ddl(&[
        "CREATE TABLE T (K INT64 NOT NULL, Qty INT64, Doubled INT64 AS (Qty * 2) STORED, \
         Status STRING(MAX) DEFAULT ('new'), CONSTRAINT Positive CHECK (Qty > 0)) PRIMARY KEY (K)",
    ]))
    .unwrap_or_default();
    assert_eq!(
        insert(&st, &["K", "Qty"], &["1", "21"])
            .err()
            .map(|e| e.code),
        Some(Code::Unimplemented)
    );

    set_expr_evaluator(Some(Arc::new(Toy)));
    let st = insert(&st, &["K", "Qty"], &["1", "21"]).unwrap_or_else(|e| panic!("{e}"));
    let t = st.catalog.table_id("T").unwrap_or_default();
    let spans = st
        .spans(spanbolt_storage::Target::Table(t), &KeySet::all())
        .unwrap_or_default();
    let rows: Vec<Vec<Value>> = scan(st.rows(t), &spans).map(|(_, r)| r.to_vec()).collect();
    assert_eq!(
        rows,
        vec![vec![
            Value::Int64(1),
            Value::Int64(21),
            Value::Int64(42),
            Value::string("new")
        ]]
    );
    let err = insert(&st, &["K", "Qty"], &["2", "0"]).err();
    assert_eq!(
        err.map(|e| (e.code, e.message)),
        Some((
            Code::OutOfRange,
            "Check constraint `T`.`Positive` is violated for key {Int64(2)}".into()
        ))
    );

    // A key column with a volatile DEFAULT that the writes leave out: each row evaluates it once,
    // and is keyed by the value it stores (one commit, several mutations and rows).
    let st = DbState::new(ddl(&[
        "CREATE TABLE U (Id INT64 NOT NULL DEFAULT (NEXT()), Source STRING(MAX)) PRIMARY KEY (Id)",
    ]))
    .unwrap_or_default();
    let mut w = Writer::new(&st, Timestamp::default());
    for rows in [&["a", "b"][..], &["c"], &["d"]] {
        let rows: Vec<ListValue> = rows
            .iter()
            .map(|v| ListValue {
                values: vec![sv(v)],
            })
            .collect();
        let m = Mutation::write(
            &st.catalog,
            WriteKind::InsertOrUpdate,
            "U",
            &["Source".to_string()],
            &rows,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        w.apply(&m).unwrap_or_else(|e| panic!("{e}"));
    }
    let st = w.finish().unwrap_or_else(|e| panic!("{e}")).0;
    let t = st.catalog.table_id("U").unwrap_or_default();
    let spans = st
        .spans(spanbolt_storage::Target::Table(t), &KeySet::all())
        .unwrap_or_default();
    let rows: Vec<Vec<Value>> = scan(st.rows(t), &spans).map(|(_, r)| r.to_vec()).collect();
    assert_eq!(rows.len(), 4);
    let ids: Vec<&Value> = rows.iter().map(|r| &r[0]).collect();
    assert!(ids.windows(2).all(|w| w[0] != w[1]), "{rows:?}");
    // Point lookups by the stored id find each row, so the key matches the value.
    for r in &rows {
        let key = KeySet::keys(vec![vec![r[0].clone()]]);
        let spans = st
            .spans(spanbolt_storage::Target::Table(t), &key)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(scan(st.rows(t), &spans).count(), 1, "{r:?}");
    }
    set_expr_evaluator(None);
}
