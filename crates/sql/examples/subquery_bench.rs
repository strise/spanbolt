//! Correlated subqueries at 10k x 10k rows, in process (no gRPC):
//! `source scripts/googlesql-env.sh && cargo run --release -p spanbolt-sql --example subquery_bench`
//!
//! `Orders.CustomerRef` is not a key or index column of `Orders`, so a correlated predicate on
//! it cannot become a key lookup. `Rows` is a per-query override of the row count.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use prost_types::value::Kind;
use spanbolt_ddl::{Schema, parse_statement};
use spanbolt_proto::spanner as pb;
use spanbolt_sql::{Engine, Txn};
use spanbolt_storage::{DbState, Mutation, WriteKind};
use spanbolt_txn::{Clock, Database, ReadOnlyTxn, ReadWriteTxn};

fn s(v: &str) -> prost_types::Value {
    prost_types::Value {
        kind: Some(Kind::StringValue(v.into())),
    }
}

fn main() {
    let rows: i64 = std::env::var("ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let mut schema = Schema::default();
    for ddl in [
        "CREATE TABLE Customers (CustomerId INT64 NOT NULL, Region STRING(MAX)) PRIMARY KEY (CustomerId)",
        "CREATE TABLE Orders (OrderId INT64 NOT NULL, CustomerRef INT64, Amount INT64) PRIMARY KEY (OrderId)",
    ] {
        schema
            .apply(&parse_statement(ddl).expect("ddl"))
            .expect("apply");
    }
    let db = Arc::new(Database::new(
        Arc::new(Clock::default()),
        DbState::new(schema).expect("state"),
    ));
    let txn = ReadWriteTxn::new(db.clone());
    let cat = txn.catalog();
    let lv = |v: Vec<prost_types::Value>| prost_types::ListValue { values: v };
    let customers: Vec<_> = (0..rows)
        .map(|i| {
            lv(vec![
                s(&i.to_string()),
                s(if i % 2 == 0 { "N" } else { "S" }),
            ])
        })
        .collect();
    // Every third customer has no orders; the others have one or two.
    let orders: Vec<_> = (0..rows)
        .map(|i| {
            let c = (i * 3 / 2) % rows;
            let c = if c % 3 == 2 { (c + 1) % rows } else { c };
            lv(vec![
                s(&i.to_string()),
                s(&c.to_string()),
                s(&(i % 100).to_string()),
            ])
        })
        .collect();
    let cols = |c: &[&str]| c.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let m = vec![
        Mutation::write(
            &cat,
            WriteKind::Insert,
            "Customers",
            &cols(&["CustomerId", "Region"]),
            &customers,
        )
        .expect("mutation"),
        Mutation::write(
            &cat,
            WriteKind::Insert,
            "Orders",
            &cols(&["OrderId", "CustomerRef", "Amount"]),
            &orders,
        )
        .expect("mutation"),
    ];
    txn.commit(&cat, &m).expect("commit");

    let engine = Engine::default();
    let queries: &[(&str, &str)] = &[
        (
            "EXISTS",
            "SELECT COUNT(*) FROM Customers c WHERE EXISTS (SELECT 1 FROM Orders o WHERE o.CustomerRef = c.CustomerId)",
        ),
        (
            "NOT EXISTS",
            "SELECT COUNT(*) FROM Customers c WHERE NOT EXISTS (SELECT 1 FROM Orders o WHERE o.CustomerRef = c.CustomerId AND o.Amount > 10)",
        ),
        (
            "IN (correlated)",
            "SELECT COUNT(*) FROM Customers c WHERE c.CustomerId IN (SELECT o.CustomerRef FROM Orders o WHERE o.CustomerRef = c.CustomerId)",
        ),
        (
            "scalar COUNT(*)",
            "SELECT SUM((SELECT COUNT(*) FROM Orders o WHERE o.CustomerRef = c.CustomerId)) FROM Customers c",
        ),
        (
            "scalar SUM in SELECT",
            "SELECT c.CustomerId, (SELECT SUM(o.Amount) FROM Orders o WHERE o.CustomerRef = c.CustomerId) FROM Customers c",
        ),
        (
            "ARRAY subquery",
            "SELECT c.CustomerId, ARRAY(SELECT o.OrderId FROM Orders o WHERE o.CustomerRef = c.CustomerId) FROM Customers c",
        ),
    ];
    println!("{rows} customers x {rows} orders");
    for (label, sql) in queries {
        let req = pb::ExecuteSqlRequest {
            sql: sql.to_string(),
            params: Some(prost_types::Struct {
                fields: BTreeMap::new(),
            }),
            ..Default::default()
        };
        let ro = ReadOnlyTxn {
            version: db.latest(),
        };
        // Warm-up plans and caches the statement.
        let t = Instant::now();
        let rs = engine
            .execute_sql(&db, Txn::ReadOnly(&ro), &req)
            .expect("query");
        let first = t.elapsed();
        let mut best = first;
        let reps = if first.as_secs_f64() > 2.0 { 0 } else { 3 };
        for _ in 0..reps {
            let t = Instant::now();
            engine
                .execute_sql(&db, Txn::ReadOnly(&ro), &req)
                .expect("query");
            best = best.min(t.elapsed());
        }
        println!(
            "{label:<22} rows={:<6} best={:>10.2} ms",
            rs.rows.len(),
            best.as_secs_f64() * 1e3
        );
    }
}
