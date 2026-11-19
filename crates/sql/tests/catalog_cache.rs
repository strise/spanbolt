//! The per-schema catalog cache lets go of a catalog once no database state uses its schema.

use spanbolt_sql::Engine;
use spanbolt_storage::DbState;

fn catalogs(e: &Engine) -> usize {
    e.cache_sizes()
        .iter()
        .find(|(k, _)| *k == "catalogs")
        .map_or(0, |(_, n)| *n)
}

#[test]
fn catalog_of_a_dropped_schema_is_evicted() {
    let engine = Engine::default();
    let a = DbState::default();
    let clone = a.clone();
    let ca = engine.catalog(&a).expect("catalog");
    assert!(std::sync::Arc::ptr_eq(
        &ca,
        &engine.catalog(&clone).expect("catalog")
    ));
    let b = DbState::default();
    engine.catalog(&b).expect("catalog");
    assert_eq!(catalogs(&engine), 2);

    // A clone still uses a's schema: its catalog stays.
    drop(a);
    engine.catalog(&b).expect("catalog");
    assert_eq!(catalogs(&engine), 2);

    // In-flight users of the catalog don't keep it cached once the schema is gone.
    drop(clone);
    engine.catalog(&b).expect("catalog");
    assert_eq!(catalogs(&engine), 1);
    drop(ca);
}

fn state(ddl: &[&str]) -> DbState {
    let mut schema = spanbolt_ddl::Schema::default();
    for s in ddl {
        let st = spanbolt_ddl::parse_statement(s).expect("ddl parses");
        schema.apply(&st).expect("ddl applies");
    }
    DbState::new(schema).expect("state")
}

fn size(e: &Engine, key: &str) -> usize {
    e.cache_sizes()
        .iter()
        .find(|(k, _)| *k == key)
        .map_or(0, |(_, n)| *n)
}

fn view_type(e: &Engine, s: &DbState) -> i32 {
    let db = std::sync::Arc::new(spanbolt_txn::Database::new(
        std::sync::Arc::new(spanbolt_txn::Clock::default()),
        s.clone(),
    ));
    let ro = spanbolt_txn::ReadOnlyTxn {
        version: db.latest(),
    };
    let req = spanbolt_proto::spanner::ExecuteSqlRequest {
        sql: "SELECT v FROM VW".into(),
        ..Default::default()
    };
    let rs = e
        .execute_sql(&db, spanbolt_sql::Txn::ReadOnly(&ro), &req)
        .expect("query");
    let field = &rs
        .metadata
        .expect("metadata")
        .row_type
        .expect("row type")
        .fields[0];
    field.r#type.as_ref().expect("type").code
}

const T: &str = "CREATE TABLE T (k INT64 NOT NULL, v INT64) PRIMARY KEY (k)";
const V: &str = "CREATE VIEW VW SQL SECURITY INVOKER AS SELECT T.v FROM T";

#[test]
fn a_ddl_builds_only_the_tables_it_changes() {
    let engine = Engine::default();
    let a = state(&[T, V]);
    engine.catalog(&a).expect("catalog");
    let before = size(&engine, "gsql_tables");
    // One new table: the other tables (T, the view, INFORMATION_SCHEMA) are reused.
    let b = state(&[T, V, "CREATE TABLE U (k INT64 NOT NULL) PRIMARY KEY (k)"]);
    engine.catalog(&b).expect("catalog");
    assert_eq!(size(&engine, "gsql_tables"), before + 1);
    assert_eq!(view_type(&engine, &b), 2); // INT64
}

#[test]
fn a_view_is_reanalysed_when_its_tables_change() {
    let engine = Engine::default();
    let a = state(&[T, V]);
    assert_eq!(view_type(&engine, &a), 2); // INT64
    // Same view SQL over a T whose v is a STRING.
    let b = state(&[
        "CREATE TABLE T (k INT64 NOT NULL, v STRING(MAX)) PRIMARY KEY (k)",
        V,
    ]);
    assert_eq!(view_type(&engine, &b), 6); // STRING
    assert_eq!(view_type(&engine, &a), 2);
}
