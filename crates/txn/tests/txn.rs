use std::sync::Arc;
use std::time::Duration;

use prost_types::{ListValue, value::Kind};
use spanbolt_ddl::ColumnType;
use spanbolt_storage::testing::*;
use spanbolt_storage::{
    Code, DbState, KeyRange, KeySet, Mutation, Target, Timestamp, Value, WriteKind, scan,
};
use spanbolt_txn::{Clock, Database, ReadOnlyTxn, ReadWriteTxn, TimestampBound};

fn sv(s: &str) -> prost_types::Value {
    prost_types::Value {
        kind: Some(Kind::StringValue(s.into())),
    }
}

fn db() -> Arc<Database> {
    let sch = schema(
        vec![table(
            "T",
            vec![
                not_null(col("K", ColumnType::Int64)),
                col("V", ColumnType::String(None)),
            ],
            vec![asc("K")],
        )],
        vec![index("TByV", "T", vec![asc("V")], &[])],
    );
    let state = DbState::new(sch).unwrap_or_else(|e| panic!("{e}"));
    Arc::new(Database::new(Arc::new(Clock::default()), state))
}

fn put(db: &Database, k: i64, v: &str) -> Mutation {
    let cat = db.latest().state.catalog.clone();
    Mutation::write(
        &cat,
        WriteKind::InsertOrUpdate,
        "T",
        &["K".into(), "V".into()],
        &[ListValue {
            values: vec![sv(&k.to_string()), sv(v)],
        }],
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

fn ins(db: &Database, k: i64, v: &str) -> Mutation {
    let cat = db.latest().state.catalog.clone();
    Mutation::write(
        &cat,
        WriteKind::Insert,
        "T",
        &["K".into(), "V".into()],
        &[ListValue {
            values: vec![sv(&k.to_string()), sv(v)],
        }],
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

fn table_target(c: &spanbolt_storage::Catalog) -> spanbolt_storage::Result<Target> {
    c.lookup_table("T").map(Target::Table)
}

fn read_v(view: &spanbolt_txn::ReadView) -> Vec<Value> {
    let t = view.state.catalog.table_id("T").unwrap_or(0);
    scan(view.state.rows(t), &view.spans)
        .map(|(_, r)| r[1].clone())
        .collect()
}

fn key(k: i64) -> KeySet {
    KeySet::keys(vec![vec![Value::Int64(k)]])
}

fn commit(txn: &ReadWriteTxn, muts: &[Mutation]) -> spanbolt_storage::Result<Timestamp> {
    txn.commit(&txn.catalog(), muts).map(|c| c.ts)
}

#[test]
fn non_conflicting_transactions_both_commit() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    let b = ReadWriteTxn::new(db.clone());
    a.read(|c| Ok((table_target(c)?, (key(1)).clone())))
        .unwrap_or_else(|e| panic!("{e}"));
    b.read(|c| Ok((table_target(c)?, (key(2)).clone())))
        .unwrap_or_else(|e| panic!("{e}"));
    let ta = commit(&a, &[put(&db, 1, "a")]).unwrap_or_else(|e| panic!("{e}"));
    let tb = commit(&b, &[put(&db, 2, "b")]).unwrap_or_else(|e| panic!("{e}"));
    assert!(tb > ta);
}

#[test]
fn read_write_conflict_aborts_second_committer() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    let b = ReadWriteTxn::new(db.clone());
    a.read(|c| Ok((table_target(c)?, (key(1)).clone())))
        .unwrap_or_else(|e| panic!("{e}"));
    b.read(|c| Ok((table_target(c)?, (key(1)).clone())))
        .unwrap_or_else(|e| panic!("{e}"));
    commit(&a, &[put(&db, 1, "a")]).unwrap_or_else(|e| panic!("{e}"));
    let err = commit(&b, &[put(&db, 1, "b")]).err().map(|e| e.code);
    assert_eq!(err, Some(Code::Aborted));
    // The aborted transaction stays aborted.
    assert_eq!(
        b.read(|c| Ok((table_target(c)?, (key(1)).clone())))
            .err()
            .map(|e| e.code),
        Some(Code::Aborted)
    );
}

#[test]
fn range_reads_detect_phantoms() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    let range = KeySet {
        ranges: vec![KeyRange::closed_open(
            vec![Value::Int64(10)],
            vec![Value::Int64(20)],
        )],
        ..KeySet::default()
    };
    a.read(|c| Ok((table_target(c)?, (range).clone())))
        .unwrap_or_else(|e| panic!("{e}"));
    let b = ReadWriteTxn::new(db.clone());
    commit(&b, &[put(&db, 15, "phantom")]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        commit(&a, &[put(&db, 1, "x")]).err().map(|e| e.code),
        Some(Code::Aborted)
    );

    // An insert outside the range does not conflict.
    let c = ReadWriteTxn::new(db.clone());
    c.read(|c| Ok((table_target(c)?, (range).clone())))
        .unwrap_or_else(|e| panic!("{e}"));
    let d = ReadWriteTxn::new(db.clone());
    commit(&d, &[put(&db, 25, "outside")]).unwrap_or_else(|e| panic!("{e}"));
    assert!(commit(&c, &[put(&db, 2, "y")]).is_ok());
}

#[test]
fn index_reads_conflict_on_index_writes() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    let by_v = |c: &spanbolt_storage::Catalog| {
        c.index_id("TByV")
            .map(Target::Index)
            .ok_or_else(|| spanbolt_storage::Error::not_found("idx"))
    };
    a.read(|c| Ok((by_v(c)?, KeySet::keys(vec![vec![Value::string("x")]]))))
        .unwrap_or_else(|e| panic!("{e}"));
    let b = ReadWriteTxn::new(db.clone());
    commit(&b, &[put(&db, 7, "x")]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        commit(&a, &[put(&db, 8, "z")]).err().map(|e| e.code),
        Some(Code::Aborted)
    );
}

#[test]
fn blind_writes_never_abort() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    let b = ReadWriteTxn::new(db.clone());
    commit(&b, &[put(&db, 1, "b")]).unwrap_or_else(|e| panic!("{e}"));
    assert!(commit(&a, &[put(&db, 1, "a")]).is_ok());
}

#[test]
fn blind_insert_race_returns_already_exists() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    let b = ReadWriteTxn::new(db.clone());
    commit(&a, &[ins(&db, 1, "a")]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        commit(&b, &[ins(&db, 1, "b")]).err().map(|e| e.code),
        Some(Code::AlreadyExists)
    );
}

#[test]
fn read_then_insert_race_aborts_loser() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    let b = ReadWriteTxn::new(db.clone());
    a.read(|c| Ok((table_target(c)?, key(1))))
        .unwrap_or_else(|e| panic!("{e}"));
    b.read(|c| Ok((table_target(c)?, key(1))))
        .unwrap_or_else(|e| panic!("{e}"));
    commit(&a, &[ins(&db, 1, "a")]).unwrap_or_else(|e| panic!("{e}"));
    let err = commit(&b, &[ins(&db, 1, "b")]).err().map(|e| e.code);
    assert_eq!(err, Some(Code::Aborted));
}

#[test]
fn write_write_conflict_after_read() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    a.read(|c| Ok((table_target(c)?, (key(99)).clone())))
        .unwrap_or_else(|e| panic!("{e}"));
    let b = ReadWriteTxn::new(db.clone());
    commit(&b, &[put(&db, 1, "b")]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        commit(&a, &[put(&db, 1, "a")]).err().map(|e| e.code),
        Some(Code::Aborted)
    );
}

#[test]
fn read_your_writes_through_dml() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    a.apply_dml(vec![put(&db, 3, "mine")])
        .unwrap_or_else(|e| panic!("{e}"));
    let view = a
        .read(|c| Ok((table_target(c)?, (key(3)).clone())))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(read_v(&view), vec![Value::string("mine")]);
    // Not visible to others before commit.
    let ro = ReadOnlyTxn {
        version: db.latest(),
    };
    assert!(
        read_v(
            &ro.read(|c| Ok((table_target(c)?, (key(3)).clone())))
                .unwrap_or_else(|e| panic!("{e}"))
        )
        .is_empty()
    );
    commit(&a, &[]).unwrap_or_else(|e| panic!("{e}"));
    let ro = ReadOnlyTxn {
        version: db.latest(),
    };
    assert_eq!(
        read_v(
            &ro.read(|c| Ok((table_target(c)?, (key(3)).clone())))
                .unwrap_or_else(|e| panic!("{e}"))
        ),
        vec![Value::string("mine")]
    );
}

#[test]
fn mvcc_snapshots_and_stale_reads() {
    let db = db();
    let t1 = commit(&ReadWriteTxn::new(db.clone()), &[put(&db, 1, "v1")])
        .unwrap_or_else(|e| panic!("{e}"));
    let pinned = ReadOnlyTxn {
        version: db
            .snapshot(TimestampBound::Strong)
            .unwrap_or_else(|e| panic!("{e}")),
    };
    let t2 = commit(&ReadWriteTxn::new(db.clone()), &[put(&db, 1, "v2")])
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(t2 > t1);
    assert_eq!(
        read_v(
            &pinned
                .read(|c| Ok((table_target(c)?, (key(1)).clone())))
                .unwrap_or_else(|e| panic!("{e}"))
        ),
        vec![Value::string("v1")]
    );
    let at = |ts| ReadOnlyTxn {
        version: db
            .snapshot(TimestampBound::ReadTimestamp(ts))
            .unwrap_or_else(|e| panic!("{e}")),
    };
    assert_eq!(
        read_v(
            &at(t1)
                .read(|c| Ok((table_target(c)?, (key(1)).clone())))
                .unwrap_or_else(|e| panic!("{e}"))
        ),
        vec![Value::string("v1")]
    );
    assert_eq!(
        read_v(
            &at(t2)
                .read(|c| Ok((table_target(c)?, (key(1)).clone())))
                .unwrap_or_else(|e| panic!("{e}"))
        ),
        vec![Value::string("v2")]
    );
    let before = Timestamp::from_micros(t1.as_micros() - 1);
    assert!(
        read_v(
            &at(before)
                .read(|c| Ok((table_target(c)?, (key(1)).clone())))
                .unwrap_or_else(|e| panic!("{e}"))
        )
        .is_empty()
    );
    // A read in the future pins that timestamp: later commits get larger ones.
    let future = Timestamp::from_micros(t2.as_micros() + 10_000_000);
    at(future);
    let t3 = commit(&ReadWriteTxn::new(db.clone()), &[put(&db, 1, "v3")])
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(t3 > future);
    // Reads more than the version retention period (1h) in the past are rejected.
    let stale = db.snapshot(TimestampBound::ExactStaleness(Duration::from_secs(3600)));
    assert!(matches!(stale, Err(e) if e.code == spanbolt_storage::Code::FailedPrecondition));
}

#[test]
fn clone_is_isolated_from_source() {
    let db = db();
    commit(&ReadWriteTxn::new(db.clone()), &[put(&db, 1, "seed")])
        .unwrap_or_else(|e| panic!("{e}"));
    let clone = Arc::new(db.clone_database());
    commit(
        &ReadWriteTxn::new(clone.clone()),
        &[put(&clone, 1, "clone"), put(&clone, 2, "new")],
    )
    .unwrap_or_else(|e| panic!("{e}"));
    commit(&ReadWriteTxn::new(db.clone()), &[put(&db, 3, "source")])
        .unwrap_or_else(|e| panic!("{e}"));
    let all = KeySet::all();
    let src = read_v(
        &ReadOnlyTxn {
            version: db.latest(),
        }
        .read(|c| Ok((table_target(c)?, (all).clone())))
        .unwrap_or_else(|e| panic!("{e}")),
    );
    let cl = read_v(
        &ReadOnlyTxn {
            version: clone.latest(),
        }
        .read(|c| Ok((table_target(c)?, (all).clone())))
        .unwrap_or_else(|e| panic!("{e}")),
    );
    assert_eq!(src, vec![Value::string("seed"), Value::string("source")]);
    assert_eq!(cl, vec![Value::string("clone"), Value::string("new")]);
}

#[test]
fn injected_aborts_and_frozen_clock() {
    let db = db();
    db.clock().set(Some(Timestamp {
        secs: 2_000_000_000,
        nanos: 0,
    }));
    db.inject_aborts(2);
    for _ in 0..2 {
        let e = commit(&ReadWriteTxn::new(db.clone()), &[put(&db, 1, "x")])
            .err()
            .map(|e| e.code);
        assert_eq!(e, Some(Code::Aborted));
    }
    let a = commit(&ReadWriteTxn::new(db.clone()), &[put(&db, 1, "x")])
        .unwrap_or_else(|e| panic!("{e}"));
    let b = commit(&ReadWriteTxn::new(db.clone()), &[put(&db, 2, "y")])
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(a.secs >= 2_000_000_000 && b > a);
    assert_eq!(b.as_micros() - a.as_micros(), 1);
}

#[test]
fn schema_change_aborts_older_transactions() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    a.read(|c| Ok((table_target(c)?, (key(1)).clone())))
        .unwrap_or_else(|e| panic!("{e}"));
    let muts = [put(&db, 1, "x")];
    db.update_schema(|s| {
        let mut s = (*s.schema).clone();
        s.indexes.remove("tbyv");
        Ok(s)
    })
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        a.commit(&a.catalog(), &muts).err().map(|e| e.code),
        Some(Code::Aborted)
    );
}

#[test]
fn constraint_error_invalidates_transaction() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    a.apply_dml(vec![put(&db, 2, "two")])
        .unwrap_or_else(|e| panic!("{e}"));
    let dup = spanbolt_storage::Error::constraint(Code::AlreadyExists, "duplicate");
    a.dml_failed(&dup);
    let err = commit(&a, &[]).err();
    assert_eq!(err.as_ref().map(|e| e.code), Some(Code::AlreadyExists));
    assert_eq!(err.as_ref().map(|e| e.message.as_str()), Some("duplicate"));
    assert!(err.is_some_and(|e| !e.constraint));
    assert!(a.current_state().is_err());
    // Nothing was written.
    let ro = ReadOnlyTxn {
        version: db.latest(),
    };
    assert!(
        read_v(
            &ro.read(|c| Ok((table_target(c)?, (key(2)).clone())))
                .unwrap_or_else(|e| panic!("{e}"))
        )
        .is_empty()
    );
}

#[test]
fn non_constraint_error_keeps_transaction_usable() {
    let db = db();
    let a = ReadWriteTxn::new(db.clone());
    a.apply_dml(vec![put(&db, 2, "two")])
        .unwrap_or_else(|e| panic!("{e}"));
    a.dml_failed(&spanbolt_storage::Error::invalid_argument("bad column"));
    commit(&a, &[]).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn history_keeps_a_bounded_number_of_schemas() {
    let db = db();
    let first = commit(&ReadWriteTxn::new(db.clone()), &[put(&db, 1, "v1")])
        .unwrap_or_else(|e| panic!("{e}"));
    let first_schema = Arc::downgrade(&db.latest().state.schema);
    let mut changes = Vec::new();
    for i in 0..40 {
        let ts = db
            .update_schema(|s| {
                let mut s = (*s.schema).clone();
                s.version += 1;
                s.options.push((format!("o{i}"), String::new()));
                Ok(s)
            })
            .unwrap_or_else(|e| panic!("{e}"));
        changes.push(ts);
    }
    // Every schema change holds a whole schema; old ones go once enough newer ones exist.
    assert_eq!(first_schema.strong_count(), 0);
    let gc = db.snapshot(TimestampBound::ReadTimestamp(first));
    assert!(matches!(gc, Err(e) if e.code == Code::FailedPrecondition));
    let recent = db
        .snapshot(TimestampBound::ReadTimestamp(changes[30]))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(recent.ts, changes[30]);
    // Data commits share the schema and do not count against the bound.
    for i in 0..100 {
        commit(&ReadWriteTxn::new(db.clone()), &[put(&db, i, "x")])
            .unwrap_or_else(|e| panic!("{e}"));
    }
    let still = db
        .snapshot(TimestampBound::ReadTimestamp(changes[30]))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(still.ts, changes[30]);
}
