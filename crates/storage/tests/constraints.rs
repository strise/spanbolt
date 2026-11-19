//! Commit-level constraints: unique indexes, foreign keys, and the DDL checks that guard them.

use prost_types::{ListValue, value::Kind};
use spanbolt_ddl::{DataCheck, parse_statement};
use spanbolt_storage::testing::ddl;
use spanbolt_storage::{
    Code, DbState, KeySet, Mutation, Target, Timestamp, Value, WriteKind, Writer,
};

fn s(v: &str) -> prost_types::Value {
    prost_types::Value {
        kind: Some(Kind::StringValue(v.into())),
    }
}

fn base() -> DbState {
    let sch = ddl(&[
        "CREATE TABLE P (P INT64 NOT NULL, Email STRING(MAX), Code STRING(MAX)) PRIMARY KEY (P)",
        "CREATE UNIQUE INDEX PByEmail ON P(Email)",
        "CREATE TABLE R (R INT64 NOT NULL, P INT64, CONSTRAINT FkP FOREIGN KEY (P) REFERENCES P (P)) PRIMARY KEY (R)",
        "CREATE TABLE C (C INT64 NOT NULL, P INT64, \
         CONSTRAINT FkC FOREIGN KEY (P) REFERENCES P (P) ON DELETE CASCADE) PRIMARY KEY (C)",
        "CREATE TABLE ByCode (K INT64 NOT NULL, Code STRING(MAX), \
         CONSTRAINT FkCode FOREIGN KEY (Code) REFERENCES P (Code)) PRIMARY KEY (K)",
    ]);
    DbState::new(sch).unwrap_or_default()
}

fn write(st: &DbState, kind: WriteKind, t: &str, cols: &[&str], rows: &[&[&str]]) -> Mutation {
    let cols: Vec<String> = cols.iter().map(|c| c.to_string()).collect();
    let rows: Vec<ListValue> = rows
        .iter()
        .map(|r| ListValue {
            values: r.iter().map(|v| s(v)).collect(),
        })
        .collect();
    Mutation::write(&st.catalog, kind, t, &cols, &rows).unwrap_or_else(|e| panic!("{e}"))
}

fn delete(st: &DbState, t: &str, key: i64) -> Mutation {
    Mutation::delete(&st.catalog, t, KeySet::keys(vec![vec![Value::Int64(key)]]))
        .unwrap_or_else(|e| panic!("{e}"))
}

fn apply(st: &DbState, muts: &[Mutation]) -> spanbolt_storage::Result<DbState> {
    let mut w = Writer::new(st, Timestamp::default());
    for m in muts {
        w.apply(m)?;
    }
    Ok(w.finish()?.0)
}

fn seeded() -> DbState {
    let st = base();
    let m = write(
        &st,
        WriteKind::Insert,
        "P",
        &["P", "Email", "Code"],
        &[&["1", "a@x", "c1"], &["2", "b@x", "c2"]],
    );
    apply(&st, &[m]).unwrap_or_else(|e| panic!("{e}"))
}

fn count(st: &DbState, t: &str) -> usize {
    st.rows(st.catalog.table_id(t).unwrap_or_default()).len()
}

#[test]
fn unique_swap_in_one_commit() {
    let st = seeded();
    let swap = [
        write(
            &st,
            WriteKind::Update,
            "P",
            &["P", "Email"],
            &[&["1", "b@x"]],
        ),
        write(
            &st,
            WriteKind::Update,
            "P",
            &["P", "Email"],
            &[&["2", "a@x"]],
        ),
    ];
    assert!(apply(&st, &swap).is_ok());
    let err = apply(&st, &swap[..1]).err();
    assert_eq!(err.map(|e| e.code), Some(Code::AlreadyExists));
}

#[test]
fn foreign_key_checked_at_commit() {
    let st = seeded();
    let missing = write(&st, WriteKind::Insert, "R", &["R", "P"], &[&["1", "42"]]);
    let err = apply(&st, std::slice::from_ref(&missing)).err();
    assert_eq!(
        err.map(|e| e.message),
        Some(
            "Foreign key `FkP` constraint violation on table `R`. Cannot find referenced key `{Int64(42)}` in table `P`."
                .into()
        )
    );
    // Inserting the referenced row later in the same commit satisfies it.
    let parent = write(&st, WriteKind::Insert, "P", &["P"], &[&["42"]]);
    assert!(apply(&st, &[missing, parent]).is_ok());
}

#[test]
fn foreign_key_blocks_delete_unless_reinserted() {
    let st = seeded();
    let child = write(&st, WriteKind::Insert, "R", &["R", "P"], &[&["1", "2"]]);
    let st = apply(&st, &[child]).unwrap_or_else(|e| panic!("{e}"));
    let err = apply(&st, &[delete(&st, "P", 2)]).err();
    assert_eq!(err.map(|e| e.code), Some(Code::FailedPrecondition));
    let again = write(&st, WriteKind::Insert, "P", &["P"], &[&["2"]]);
    assert!(apply(&st, &[delete(&st, "P", 2), again]).is_ok());
}

#[test]
fn foreign_key_on_delete_cascade() {
    let st = seeded();
    let kids = write(
        &st,
        WriteKind::Insert,
        "C",
        &["C", "P"],
        &[&["1", "1"], &["2", "1"], &["3", "2"]],
    );
    let st = apply(&st, &[kids]).unwrap_or_else(|e| panic!("{e}"));
    let st = apply(&st, &[delete(&st, "P", 1)]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(count(&st, "C"), 1);
}

#[test]
fn foreign_key_on_non_key_columns_uses_a_managed_index() {
    let st = seeded();
    let fk = st
        .catalog
        .foreign_keys
        .iter()
        .find(|f| f.name == "FkCode")
        .unwrap_or_else(|| panic!("no FkCode"));
    let Target::Index(ix) = fk.parent_lookup.target else {
        panic!("expected an index lookup");
    };
    assert!(st.catalog.index(ix).managed);
    assert!(st.catalog.index_id(&st.catalog.index(ix).name).is_none());
    let ok = write(
        &st,
        WriteKind::Insert,
        "ByCode",
        &["K", "Code"],
        &[&["1", "c2"]],
    );
    assert!(apply(&st, &[ok]).is_ok());
    let bad = write(
        &st,
        WriteKind::Insert,
        "ByCode",
        &["K", "Code"],
        &[&["1", "zz"]],
    );
    assert_eq!(
        apply(&st, &[bad]).err().map(|e| e.code),
        Some(Code::FailedPrecondition)
    );
}

fn alter(st: &DbState, sql: &str) -> (DbState, Vec<DataCheck>) {
    let mut schema = (*st.schema).clone();
    let stmt = parse_statement(sql).unwrap_or_else(|e| panic!("{e}"));
    let checks = schema.data_checks(&stmt);
    schema.apply(&stmt).unwrap_or_else(|e| panic!("{e}"));
    (
        st.with_schema(schema).unwrap_or_else(|e| panic!("{e}")),
        checks,
    )
}

#[test]
fn adding_a_foreign_key_validates_existing_rows() {
    let (st, _) = alter(&seeded(), "ALTER TABLE R DROP CONSTRAINT FkP");
    let rows = write(
        &st,
        WriteKind::Insert,
        "R",
        &["R", "P"],
        &[&["1", "7"], &["2", "1"]],
    );
    let st = apply(&st, &[rows]).unwrap_or_else(|e| panic!("{e}"));

    let (next, checks) = alter(
        &st,
        "ALTER TABLE ByCode ADD CONSTRAINT FkR FOREIGN KEY (K) REFERENCES R (R)",
    );
    assert!(matches!(checks.as_slice(), [DataCheck::ForeignKey { .. }]));
    assert!(next.run_data_check(&checks[0]).is_ok());

    let (next, checks) = alter(
        &st,
        "ALTER TABLE R ADD CONSTRAINT FkP2 FOREIGN KEY (P) REFERENCES P (P)",
    );
    let err = next.run_data_check(&checks[0]).err();
    assert_eq!(err.map(|e| e.code), Some(Code::FailedPrecondition));
}

#[test]
fn defaults_without_an_evaluator_are_unimplemented() {
    let st = DbState::new(ddl(&[
        "CREATE TABLE D (K INT64 NOT NULL, V STRING(MAX) DEFAULT ('x')) PRIMARY KEY (K)",
    ]))
    .unwrap_or_default();
    let omit = write(&st, WriteKind::Insert, "D", &["K"], &[&["1"]]);
    assert_eq!(
        apply(&st, &[omit]).err().map(|e| e.code),
        Some(Code::Unimplemented)
    );
    let explicit = write(&st, WriteKind::Insert, "D", &["K", "V"], &[&["1", "y"]]);
    assert!(apply(&st, &[explicit]).is_ok());
}

#[test]
fn tables_without_primary_key_get_a_hidden_rowid() {
    let st = DbState::new(ddl(&["CREATE TABLE X (K INT64)"])).unwrap_or_default();
    let m = write(&st, WriteKind::Insert, "X", &["K"], &[&["1"], &["1"]]);
    let st = apply(&st, &[m]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(count(&st, "X"), 2);
}
