//! INFORMATION_SCHEMA queries Liquibase and the JDBC driver send, over a large schema, in process:
//! `source scripts/googlesql-env.sh && cargo run --release -p spanbolt-sql --example info_schema_bench`
//!
//! `DDL=<file>` loads the schema from a file of DDL statements separated by `;` at line ends (a
//! `GetDatabaseDdl` dump); without it, a synthetic schema of 550 tables x 12 columns with one index each.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use prost_types::value::Kind;
use spanbolt_ddl::{Schema, parse_statement};
use spanbolt_proto::spanner as pb;
use spanbolt_sql::{Engine, Txn};
use spanbolt_storage::DbState;
use spanbolt_txn::{Clock, Database, ReadOnlyTxn};

fn s(v: &str) -> prost_types::Value {
    prost_types::Value {
        kind: Some(Kind::StringValue(v.into())),
    }
}

fn statements() -> Vec<String> {
    if let Ok(path) = std::env::var("DDL") {
        let text = std::fs::read_to_string(&path).expect("read DDL");
        return text
            .split(";\n")
            .map(|s| s.trim().trim_end_matches(';').trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    let mut v = Vec::new();
    for t in 0..550 {
        let cols: Vec<String> = (0..12)
            .map(|c| format!("c{c} STRING(MAX) DEFAULT ('x{c}')"))
            .collect();
        v.push(format!(
            "CREATE TABLE t{t} (id STRING(36) NOT NULL, {}) PRIMARY KEY (id)",
            cols.join(", ")
        ));
        v.push(format!("CREATE INDEX t{t}_c1 ON t{t} (c1)"));
    }
    v
}

fn main() {
    let mut schema = Schema::default();
    let (mut ok, mut failed) = (0, 0);
    for ddl in statements() {
        match parse_statement(&ddl).map(|st| schema.apply(&st)) {
            Ok(Ok(_)) => ok += 1,
            _ => failed += 1,
        }
    }
    let db = Arc::new(Database::new(
        Arc::new(Clock::default()),
        DbState::new(schema).expect("state"),
    ));
    let engine = Engine::default();
    let (table, column) = if std::env::var("DDL").is_ok() {
        ("databasechangelog", "id")
    } else {
        ("t300", "c4")
    };
    let p = |v: &[&str]| {
        let mut fields = BTreeMap::new();
        let mut types = std::collections::HashMap::new();
        for (i, x) in v.iter().enumerate() {
            fields.insert(format!("p{}", i + 1), s(x));
            types.insert(
                format!("p{}", i + 1),
                pb::Type {
                    code: pb::TypeCode::String as i32,
                    ..Default::default()
                },
            );
        }
        (prost_types::Struct { fields }, types)
    };
    let queries: Vec<(&str, String, Vec<&str>)> = vec![
        (
            "COLUMN_DEF (Liquibase)",
            "SELECT DISTINCT COLUMN_DEFAULT AS COLUMN_DEF FROM INFORMATION_SCHEMA.COLUMNS WHERE LOWER(TABLE_CATALOG) = @p1 AND LOWER(TABLE_SCHEMA) = @p2 AND LOWER(TABLE_NAME) = @p3 AND LOWER(COLUMN_NAME) = @p4".into(),
            vec!["", "", table, column],
        ),
        (
            "TABLES LIKE (JDBC)",
            "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE UPPER(TABLE_CATALOG) LIKE @p1 AND UPPER(TABLE_SCHEMA) LIKE @p2 AND UPPER(TABLE_NAME) LIKE @p3".into(),
            vec!["%", "", "%"],
        ),
        (
            "INDEXES eq",
            format!("SELECT COUNT(*) FROM INFORMATION_SCHEMA.INDEXES WHERE TABLE_SCHEMA = '' AND TABLE_NAME = '{table}' AND INDEX_NAME = 'PRIMARY_KEY'"),
            vec![],
        ),
        (
            "COLUMNS count",
            "SELECT COUNT(*) FROM INFORMATION_SCHEMA.COLUMNS".into(),
            vec![],
        ),
    ];
    println!("{ok} DDL statements applied, {failed} skipped");
    for (label, sql, args) in &queries {
        let (params, param_types) = p(args);
        let req = pb::ExecuteSqlRequest {
            sql: sql.clone(),
            params: Some(params),
            param_types,
            ..Default::default()
        };
        let ro = ReadOnlyTxn::new(db.latest());
        let t = Instant::now();
        let rs = engine
            .execute_sql(&db, Txn::ReadOnly(&ro), &req)
            .expect("query");
        let first = t.elapsed();
        let n = 200;
        let t = Instant::now();
        for _ in 0..n {
            engine
                .execute_sql(&db, Txn::ReadOnly(&ro), &req)
                .expect("query");
        }
        let mean = t.elapsed() / n;
        println!(
            "{label:<24} rows={:<5} first={:>9.2} ms  mean={:>8.3} ms",
            rs.rows.len(),
            first.as_secs_f64() * 1e3,
            mean.as_secs_f64() * 1e3
        );
    }
}
