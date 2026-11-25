//! The search-index access path returns what a full scan returns, across writes.

use std::sync::Arc;

use spanbolt_ddl::{Schema, parse_statement};
use spanbolt_proto::spanner as pb;
use spanbolt_sql::{Engine, SchemaExprs, Txn};
use spanbolt_storage::DbState;
use spanbolt_txn::{Clock, Database, ReadOnlyTxn, ReadWriteTxn};

const DDL: &[&str] = &[
    "CREATE TABLE Plain (Id INT64 NOT NULL, Name STRING(MAX), Tags ARRAY<STRING(MAX)>, T TOKENLIST AS (TOKENIZE_FULLTEXT(Name)) HIDDEN, A TOKENLIST AS (TOKENIZE_FULLTEXT(Tags)) HIDDEN) PRIMARY KEY (Id)",
    "CREATE TABLE Indexed (Id INT64 NOT NULL, Name STRING(MAX), Tags ARRAY<STRING(MAX)>, T TOKENLIST AS (TOKENIZE_FULLTEXT(Name)) HIDDEN, A TOKENLIST AS (TOKENIZE_FULLTEXT(Tags)) HIDDEN) PRIMARY KEY (Id)",
    "CREATE SEARCH INDEX IndexedSearch ON Indexed(T, A)",
];

const QUERIES: &[&str] = &[
    "acme",
    "acme beta",
    "acme | gamma",
    "acme -beta",
    "-acme",
    "\"acme beta\"",
    "beta-acme",
    "acme AROUND(1) gamma",
    "zzz",
    "zzz OR acme",
    "",
    "!!",
];

fn sql(table: &str, col: &str) -> String {
    format!("SELECT Id FROM {table} WHERE SEARCH({col}, @q) ORDER BY Id")
}

fn rows(e: &Engine, db: &Arc<Database>, sql: &str, q: &str) -> Vec<String> {
    let mut req = pb::ExecuteSqlRequest {
        sql: sql.into(),
        ..Default::default()
    };
    req.params = Some(prost_types::Struct {
        fields: [(
            "q".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(q.into())),
            },
        )]
        .into(),
    });
    req.param_types.insert(
        "q".into(),
        pb::Type {
            code: pb::TypeCode::String as i32,
            ..Default::default()
        },
    );
    let txn = ReadOnlyTxn::new(db.latest());
    let rs = e.execute_sql(db, Txn::ReadOnly(&txn), &req).expect("query");
    rs.rows
        .iter()
        .map(|r| format!("{:?}", r.values[0].kind))
        .collect()
}

fn dml(e: &Engine, db: &Arc<Database>, stmts: &[String]) {
    let txn = Arc::new(ReadWriteTxn::new(db.clone()));
    for s in stmts {
        let req = pb::ExecuteSqlRequest {
            sql: s.clone(),
            ..Default::default()
        };
        e.execute_sql(db, Txn::ReadWrite(&txn), &req).expect("dml");
    }
    txn.commit(&txn.catalog(), &[]).expect("commit");
}

fn both(stmt: &str) -> Vec<String> {
    ["Plain", "Indexed"]
        .iter()
        .map(|t| stmt.replace("{T}", t))
        .collect()
}

#[test]
fn search_index_matches_full_scan_across_writes() {
    spanbolt_storage::expr::set_expr_evaluator(Some(Arc::new(SchemaExprs::new().expect("exprs"))));
    let mut schema = Schema::default();
    for s in DDL {
        schema
            .apply(&parse_statement(s).expect("parse"))
            .expect("apply");
    }
    let db = Arc::new(Database::new(
        Arc::new(Clock::default()),
        DbState::new(schema).expect("state"),
    ));
    // Cloud Spanner rejects SEARCH over a column no search index covers; the emulator's mode runs
    // it as a full scan, which is what this test compares the index path against.
    let e = Engine::new(spanbolt_sql::Options {
        emulator_compat: true,
    });
    let plan = e
        .explain(
            &db.latest().state,
            &pb::ExecuteSqlRequest {
                sql: "SELECT Id FROM Indexed WHERE SEARCH(T, 'acme')".into(),
                ..Default::default()
            },
        )
        .expect("plan");
    assert!(plan.contains("search index"), "{plan}");
    let check = |round: &str| {
        for col in ["T", "A"] {
            for q in QUERIES {
                assert_eq!(
                    rows(&e, &db, &sql("Plain", col), q),
                    rows(&e, &db, &sql("Indexed", col), q),
                    "{round}: SEARCH({col}, {q:?})"
                );
            }
        }
    };
    let mut stmts = Vec::new();
    for i in 0..200 {
        let name = match i % 5 {
            0 => "Acme Beta",
            1 => "beta acme gamma",
            2 => "Acme x gamma",
            3 => "Gamma",
            _ => "",
        };
        let name = if i % 7 == 0 {
            "NULL".to_string()
        } else {
            format!("'{name}'")
        };
        stmts.extend(both(&format!(
            "INSERT INTO {{T}} (Id, Name, Tags) VALUES ({i}, {name}, ['acme', 'beta {i}'])"
        )));
    }
    dml(&e, &db, &stmts);
    check("insert");
    dml(
        &e,
        &db,
        &[
            both("UPDATE {T} SET Name = 'gamma acme' WHERE Id < 20"),
            both("DELETE FROM {T} WHERE Id >= 150"),
            both("UPDATE {T} SET Tags = NULL WHERE Id BETWEEN 30 AND 40"),
        ]
        .concat(),
    );
    check("update");
    dml(
        &e,
        &db,
        &both("INSERT INTO {T} (Id, Name) VALUES (1000, 'acme beta acme')"),
    );
    check("insert again");
}
