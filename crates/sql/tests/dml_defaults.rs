//! INSERTs that leave key columns to their DEFAULT.

use std::sync::Arc;

use prost_types::value::Kind;
use spanbolt_ddl::{Schema, parse_statement};
use spanbolt_proto::spanner as pb;
use spanbolt_sql::{Engine, SchemaExprs, Txn};
use spanbolt_storage::DbState;
use spanbolt_txn::{Clock, Database, ReadOnlyTxn, ReadWriteTxn};

const DDL: &[&str] = &[
    "CREATE TABLE Singers (Id STRING(36) NOT NULL DEFAULT (GENERATE_UUID()), Name STRING(MAX)) PRIMARY KEY (Id)",
];

fn database() -> Arc<Database> {
    spanbolt_storage::expr::set_expr_evaluator(Some(Arc::new(SchemaExprs::new().expect("exprs"))));
    let mut schema = Schema::default();
    for s in DDL {
        schema
            .apply(&parse_statement(s).expect("parse"))
            .expect("apply");
    }
    Arc::new(Database::new(
        Arc::new(Clock::default()),
        DbState::new(schema).expect("state"),
    ))
}

fn texts(rs: &pb::ResultSet) -> Vec<Vec<String>> {
    rs.rows
        .iter()
        .map(|r| {
            r.values
                .iter()
                .map(|v| match &v.kind {
                    Some(Kind::StringValue(s)) => s.clone(),
                    _ => "NULL".into(),
                })
                .collect()
        })
        .collect()
}

fn dml(e: &Engine, db: &Arc<Database>, sql: &str) -> Vec<Vec<String>> {
    let txn = Arc::new(ReadWriteTxn::new(db.clone()));
    let req = pb::ExecuteSqlRequest {
        sql: sql.into(),
        ..Default::default()
    };
    let rs = e.execute_sql(db, Txn::ReadWrite(&txn), &req).expect("dml");
    txn.commit(&txn.catalog(), &[]).expect("commit");
    texts(&rs)
}

fn stored(e: &Engine, db: &Arc<Database>) -> Vec<Vec<String>> {
    let txn = ReadOnlyTxn::new(db.latest());
    let req = pb::ExecuteSqlRequest {
        sql: "SELECT Id, Name FROM Singers ORDER BY Name".into(),
        ..Default::default()
    };
    texts(&e.execute_sql(db, Txn::ReadOnly(&txn), &req).expect("query"))
}

#[test]
fn then_return_reads_back_rows_whose_key_is_a_default() {
    let (e, db) = (Engine::default(), database());

    let returned = dml(
        &e,
        &db,
        "INSERT INTO Singers (Name) VALUES ('Ada'), ('Bo') THEN RETURN Id, Name",
    );
    let explicit = dml(
        &e,
        &db,
        "INSERT INTO Singers (Id, Name) VALUES (DEFAULT, 'Cy') THEN RETURN Id, Name",
    );

    let all = [returned, explicit].concat();
    assert_eq!(
        all.iter().map(|r| r[1].as_str()).collect::<Vec<_>>(),
        ["Ada", "Bo", "Cy"]
    );
    assert!(all.iter().all(|r| r[0].len() == 36), "{all:?}");
    assert_eq!(stored(&e, &db), all);
}
