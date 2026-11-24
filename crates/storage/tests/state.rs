use std::sync::Arc;

use prost_types::{ListValue, value::Kind};
use spanbolt_ddl::ColumnType;
use spanbolt_storage::testing::*;
use spanbolt_storage::{
    Code, DbState, KeyRange, KeySet, Mutation, Target, Timestamp, Value, WriteKind, Writer, scan,
    snapshot,
};

fn s(v: &str) -> prost_types::Value {
    prost_types::Value {
        kind: Some(Kind::StringValue(v.into())),
    }
}
fn null() -> prost_types::Value {
    prost_types::Value {
        kind: Some(Kind::NullValue(0)),
    }
}
fn row(vals: Vec<prost_types::Value>) -> ListValue {
    ListValue { values: vals }
}

fn base() -> DbState {
    let sch = ddl(&[
        "CREATE TABLE Singers (SingerId INT64 NOT NULL, Name STRING(10), Email STRING(MAX)) PRIMARY KEY (SingerId)",
        "CREATE TABLE Albums (SingerId INT64 NOT NULL, AlbumId INT64 NOT NULL, Title STRING(MAX), \
         Updated TIMESTAMP OPTIONS (allow_commit_timestamp = true)) PRIMARY KEY (SingerId, AlbumId DESC), \
         INTERLEAVE IN PARENT Singers ON DELETE CASCADE",
        "CREATE TABLE Songs (SingerId INT64 NOT NULL, AlbumId INT64 NOT NULL, SongId INT64 NOT NULL) \
         PRIMARY KEY (SingerId, AlbumId DESC, SongId), INTERLEAVE IN PARENT Albums ON DELETE CASCADE",
        "CREATE TABLE Tours (SingerId INT64 NOT NULL, TourId INT64 NOT NULL) PRIMARY KEY (SingerId, TourId), \
         INTERLEAVE IN PARENT Singers ON DELETE NO ACTION",
        "CREATE INDEX SingersByName ON Singers(Name) STORING (Email)",
        "CREATE UNIQUE NULL_FILTERED INDEX SingersByEmail ON Singers(Email)",
        "CREATE INDEX AlbumsByTitle ON Albums(Title DESC)",
    ]);
    DbState::new(sch).unwrap_or_default()
}

fn apply(state: &DbState, muts: &[Mutation]) -> spanbolt_storage::Result<DbState> {
    let mut w = Writer::new(
        state,
        Timestamp {
            secs: 1_700_000_000,
            nanos: 0,
        },
    );
    for m in muts {
        w.apply(m)?;
    }
    Ok(w.finish()?.0)
}

fn write(
    state: &DbState,
    kind: WriteKind,
    t: &str,
    cols: &[&str],
    rows: Vec<ListValue>,
) -> Mutation {
    let cols: Vec<String> = cols.iter().map(|c| c.to_string()).collect();
    match Mutation::write(&state.catalog, kind, t, &cols, &rows) {
        Ok(m) => m,
        Err(e) => panic!("{e}"),
    }
}

fn ok(r: spanbolt_storage::Result<DbState>) -> DbState {
    match r {
        Ok(s) => s,
        Err(e) => panic!("{e}"),
    }
}

fn seeded() -> DbState {
    let st = base();
    let m = vec![
        write(
            &st,
            WriteKind::Insert,
            "Singers",
            &["SingerId", "Name", "Email"],
            vec![
                row(vec![s("1"), s("Marc"), s("m@x")]),
                row(vec![s("2"), s("Ann"), null()]),
                row(vec![s("3"), s("Bo"), s("b@x")]),
            ],
        ),
        write(
            &st,
            WriteKind::Insert,
            "Albums",
            &["SingerId", "AlbumId", "Title"],
            vec![
                row(vec![s("1"), s("1"), s("A")]),
                row(vec![s("1"), s("2"), s("B")]),
                row(vec![s("2"), s("1"), s("C")]),
            ],
        ),
        write(
            &st,
            WriteKind::Insert,
            "Songs",
            &["SingerId", "AlbumId", "SongId"],
            vec![row(vec![s("1"), s("2"), s("9")])],
        ),
    ];
    ok(apply(&st, &m))
}

fn index_values(st: &DbState, name: &str, col: usize) -> Vec<Value> {
    let ix = st.catalog.index_id(name).unwrap_or(usize::MAX);
    st.index_entries(ix)
        .values()
        .map(|r| r[col].clone())
        .collect()
}

#[test]
fn index_maintenance_on_insert_update_delete() {
    let st = seeded();
    assert_eq!(
        index_values(&st, "SingersByName", 0),
        vec![
            Value::string("Ann"),
            Value::string("Bo"),
            Value::string("Marc")
        ]
    );
    // NULL_FILTERED skips Ann's NULL email.
    assert_eq!(
        index_values(&st, "SingersByEmail", 0),
        vec![Value::string("b@x"), Value::string("m@x")]
    );
    // DESC index on Title.
    assert_eq!(
        index_values(&st, "AlbumsByTitle", 0),
        vec![Value::string("C"), Value::string("B"), Value::string("A")]
    );

    // Update the indexed column and the STORING column.
    let st2 = ok(apply(
        &st,
        &[write(
            &st,
            WriteKind::Update,
            "Singers",
            &["SingerId", "Name", "Email"],
            vec![row(vec![s("1"), s("Zed"), s("z@x")])],
        )],
    ));
    assert_eq!(
        index_values(&st2, "SingersByName", 0),
        vec![
            Value::string("Ann"),
            Value::string("Bo"),
            Value::string("Zed")
        ]
    );
    assert_eq!(
        index_values(&st2, "SingersByName", 1),
        vec![Value::Null, Value::string("b@x"), Value::string("z@x")]
    );
    // Storing-only change rewrites the entry in place.
    let st3 = ok(apply(
        &st2,
        &[write(
            &st2,
            WriteKind::Update,
            "Singers",
            &["SingerId", "Email"],
            vec![row(vec![s("2"), s("a@x")])],
        )],
    ));
    assert_eq!(
        index_values(&st3, "SingersByName", 1),
        vec![
            Value::string("a@x"),
            Value::string("b@x"),
            Value::string("z@x")
        ]
    );
    assert_eq!(index_values(&st3, "SingersByEmail", 0).len(), 3);

    let del = Mutation::delete(
        &st3.catalog,
        "Singers",
        KeySet::keys(vec![vec![Value::Int64(3)]]),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let st4 = ok(apply(&st3, &[del]));
    assert_eq!(
        index_values(&st4, "SingersByName", 0),
        vec![Value::string("Ann"), Value::string("Zed")]
    );
    assert_eq!(
        index_values(&st4, "SingersByEmail", 0),
        vec![Value::string("a@x"), Value::string("z@x")]
    );
    // The source state is untouched (structural sharing).
    assert_eq!(index_values(&st, "SingersByName", 0).len(), 3);
}

#[test]
fn unique_index_violation() {
    let st = seeded();
    let err = apply(
        &st,
        &[write(
            &st,
            WriteKind::Insert,
            "Singers",
            &["SingerId", "Email"],
            vec![row(vec![s("4"), s("m@x")])],
        )],
    )
    .err();
    let err = err.unwrap_or_else(|| panic!("expected violation"));
    assert_eq!(err.code, Code::AlreadyExists);
    assert_eq!(
        err.message,
        "UNIQUE violation on index SingersByEmail,  duplicate key: {String(\"m@x\")} in this transaction."
    );
    // NULLs are filtered, so two NULL emails are fine.
    ok(apply(
        &st,
        &[write(
            &st,
            WriteKind::Insert,
            "Singers",
            &["SingerId"],
            vec![row(vec![s("5")]), row(vec![s("6")])],
        )],
    ));
}

#[test]
fn constraints_match_emulator_messages() {
    let st = seeded();
    let cases: Vec<(Mutation, Code, &str)> = vec![
        (
            write(
                &st,
                WriteKind::Insert,
                "Singers",
                &["SingerId"],
                vec![row(vec![s("1")])],
            ),
            Code::AlreadyExists,
            "Table Singers: Row {Int64(1)} already exists.",
        ),
        (
            write(
                &st,
                WriteKind::Update,
                "Singers",
                &["SingerId"],
                vec![row(vec![s("9")])],
            ),
            Code::NotFound,
            "Table Singers: Row {Int64(9)} not found.",
        ),
        (
            write(
                &st,
                WriteKind::Insert,
                "Singers",
                &["Name"],
                vec![row(vec![s("x")])],
            ),
            Code::FailedPrecondition,
            "A new row in table Singers does not specify a non-null value for NOT NULL column: SingerId",
        ),
        (
            write(
                &st,
                WriteKind::Insert,
                "Singers",
                &["SingerId"],
                vec![row(vec![null()])],
            ),
            Code::FailedPrecondition,
            "Cannot specify a null value for column: Singers.SingerId in table: Singers",
        ),
        (
            write(
                &st,
                WriteKind::Insert,
                "Singers",
                &["SingerId", "Name"],
                vec![row(vec![s("7"), s("01234567890")])],
            ),
            Code::FailedPrecondition,
            "New value exceeds the maximum size limit for this column: Singers.Name, size: 11, limit: 10.",
        ),
        (
            write(
                &st,
                WriteKind::Insert,
                "Albums",
                &["SingerId", "AlbumId"],
                vec![row(vec![s("7"), s("1")])],
            ),
            Code::NotFound,
            "Insert failed because key was not found in parent table:  Parent Table: Singers  Child Table: Albums  Key: {Int64(7)}",
        ),
        (
            write(
                &st,
                WriteKind::Insert,
                "Albums",
                &["SingerId", "AlbumId", "Updated"],
                vec![row(vec![s("1"), s("5"), s("2999-01-01T00:00:00Z")])],
            ),
            Code::FailedPrecondition,
            "Cannot write timestamps in the future, found: 2999-01-01T00:00:00+00:00",
        ),
    ];
    for (m, code, msg) in cases {
        let err = apply(&st, &[m])
            .err()
            .unwrap_or_else(|| panic!("expected error: {msg}"));
        assert_eq!((err.code, err.message.as_str()), (code, msg));
    }
    let cols = vec!["Nope".to_string()];
    let e = Mutation::write(&st.catalog, WriteKind::Insert, "Singers", &cols, &[])
        .err()
        .map(|e| e.message);
    assert_eq!(
        e.as_deref(),
        Some("Column not found in table Singers: Nope")
    );
    let e = Mutation::write(&st.catalog, WriteKind::Insert, "Nope", &cols, &[])
        .err()
        .map(|e| e.message);
    assert_eq!(e.as_deref(), Some("Table not found: Nope"));
}

#[test]
fn cascade_delete_and_no_action() {
    let st = seeded();
    let albums = st.catalog.table_id("Albums").unwrap_or(0);
    let songs = st.catalog.table_id("songs").unwrap_or(0);
    let del = |st: &DbState, k: i64| {
        Mutation::delete(
            &st.catalog,
            "Singers",
            KeySet::keys(vec![vec![Value::Int64(k)]]),
        )
        .unwrap_or_else(|e| panic!("{e}"))
    };
    let st2 = ok(apply(&st, &[del(&st, 1)]));
    assert_eq!(st2.rows(albums).len(), 1);
    assert_eq!(st2.rows(songs).len(), 0);
    assert_eq!(
        index_values(&st2, "AlbumsByTitle", 0),
        vec![Value::string("C")]
    );

    let st3 = ok(apply(
        &st2,
        &[write(
            &st2,
            WriteKind::Insert,
            "Tours",
            &["SingerId", "TourId"],
            vec![row(vec![s("2"), s("1")])],
        )],
    ));
    let err = apply(&st3, &[del(&st3, 2)])
        .err()
        .unwrap_or_else(|| panic!("expected NO ACTION failure"));
    assert_eq!(err.code, Code::FailedPrecondition);
    assert_eq!(
        err.message,
        "Delete failed because one or more keys exist within the child table:  Parent Table: Singers  Child Table: Tours  Key: {Int64(2)}"
    );

    // Replace on a parent cascades to children too.
    let st4 = ok(apply(
        &st,
        &[write(
            &st,
            WriteKind::Replace,
            "Singers",
            &["SingerId"],
            vec![row(vec![s("1")])],
        )],
    ));
    assert_eq!(st4.rows(albums).len(), 1);
    let singers = st4.catalog.table_id("Singers").unwrap_or(0);
    assert_eq!(st4.rows(singers).len(), 3);
}

#[test]
fn ranges_and_desc_keys() {
    let st = seeded();
    let albums = st.catalog.table_id("Albums").unwrap_or(0);
    // Prefix range [1] .. [1] covers all of singer 1's albums, AlbumId DESC.
    let ks = KeySet {
        ranges: vec![KeyRange {
            start: vec![Value::Int64(1)],
            start_closed: true,
            end: vec![Value::Int64(1)],
            end_closed: true,
        }],
        ..KeySet::default()
    };
    let spans = st.spans(Target::Table(albums), &ks).unwrap_or_default();
    let ids: Vec<Value> = scan(st.rows(albums), &spans)
        .map(|(_, r)| r[1].clone())
        .collect();
    assert_eq!(ids, vec![Value::Int64(2), Value::Int64(1)]);
    // Range delete with a prefix.
    let del = Mutation::delete(&st.catalog, "Albums", ks).unwrap_or_else(|e| panic!("{e}"));
    let st2 = ok(apply(&st, &[del]));
    assert_eq!(st2.rows(albums).len(), 1);
    // Wrong number of key parts.
    let e = st
        .spans(
            Target::Table(albums),
            &KeySet::keys(vec![vec![Value::Int64(1)]]),
        )
        .err()
        .map(|e| e.message);
    assert_eq!(
        e.as_deref(),
        Some("Wrong number of key parts for Albums. Expected 2 key parts, found 1 key parts.")
    );
}

#[test]
fn commit_timestamp_sentinel() {
    let st = seeded();
    let m = write(
        &st,
        WriteKind::InsertOrUpdate,
        "Albums",
        &["SingerId", "AlbumId", "Updated"],
        vec![row(vec![s("1"), s("1"), s("spanner.commit_timestamp()")])],
    );
    let st2 = ok(apply(&st, &[m]));
    let albums = st2.catalog.table_id("Albums").unwrap_or(0);
    let spans = st2
        .spans(
            Target::Table(albums),
            &KeySet::keys(vec![vec![Value::Int64(1), Value::Int64(1)]]),
        )
        .unwrap_or_default();
    let got: Vec<Value> = scan(st2.rows(albums), &spans)
        .map(|(_, r)| r[3].clone())
        .collect();
    assert_eq!(
        got,
        vec![Value::Timestamp(Timestamp {
            secs: 1_700_000_000,
            nanos: 0
        })]
    );
    let cols = vec!["SingerId".to_string(), "Name".to_string()];
    let bad = Mutation::write(
        &st.catalog,
        WriteKind::Insert,
        "Singers",
        &cols,
        &[row(vec![s("8"), s("spanner.commit_timestamp()")])],
    );
    assert!(bad.is_ok(), "STRING columns take the sentinel literally");
}

#[test]
fn snapshot_round_trip() {
    let st = seeded();
    let img = snapshot::InstanceImage {
        name: "projects/p/instances/i".into(),
        config: "projects/p/instanceConfigs/emulator-config".into(),
        display_name: "I".into(),
        node_count: 1,
        processing_units: 1000,
        labels: vec![("a".into(), "b".into())],
        databases: vec![snapshot::DatabaseImage {
            name: "db".into(),
            commit_ts: Timestamp { secs: 5, nanos: 6 },
            state: Arc::new(st.clone()),
        }],
    };
    let mut buf = Vec::new();
    snapshot::write_to(&mut buf, std::slice::from_ref(&img)).unwrap_or_else(|e| panic!("{e}"));
    let loaded = snapshot::read_from(&buf[..]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(loaded.len(), 1);
    let l = &loaded[0];
    assert_eq!(
        (l.name.as_str(), l.labels.clone(), l.processing_units),
        (img.name.as_str(), img.labels.clone(), 1000)
    );
    let db = &l.databases[0];
    assert_eq!(db.commit_ts, Timestamp { secs: 5, nanos: 6 });
    assert_eq!(db.state.schema.to_ddl(), st.schema.to_ddl());
    for t in 0..st.catalog.tables.len() {
        assert_eq!(st.rows(t), db.state.rows(t));
    }
    for i in 0..st.catalog.indexes.len() {
        assert_eq!(st.index_entries(i), db.state.index_entries(i));
    }
    assert!(snapshot::read_from(&b"SPANBOLTgarbage"[..]).is_err());
}

#[test]
fn schema_migration_keeps_rows_and_backfills_indexes() {
    let st = seeded();
    let mut sch = (*st.schema).clone();
    if let Some(t) = sch.tables.get_mut("singers") {
        t.columns
            .insert(1, col("Country", ColumnType::String(None)));
    }
    sch.indexes.insert(
        "singersbycountry".into(),
        index("SingersByCountry", "Singers", vec![asc("Country")], &[]),
    );
    let st2 = st.with_schema(sch).unwrap_or_else(|e| panic!("{e}"));
    let singers = st2.catalog.table_id("Singers").unwrap_or(0);
    assert_eq!(st2.rows(singers).len(), 3);
    let first = st2
        .rows(singers)
        .values()
        .next()
        .cloned()
        .unwrap_or_else(|| Arc::from(vec![]));
    assert_eq!(
        &first[..],
        &[
            Value::Int64(1),
            Value::Null,
            Value::string("Marc"),
            Value::string("m@x")
        ]
    );
    assert_eq!(index_values(&st2, "SingersByCountry", 0).len(), 3);
    assert_eq!(index_values(&st2, "SingersByName", 0).len(), 3);
}

#[test]
fn prod_messages_follow_production_spanner() {
    let mut st = seeded();
    st.prod_messages = true;
    let err = apply(
        &st,
        &[write(
            &st,
            WriteKind::Insert,
            "Singers",
            &["SingerId", "Email"],
            vec![row(vec![s("8"), s("q@x")]), row(vec![s("9"), s("q@x")])],
        )],
    )
    .err()
    .unwrap_or_else(|| panic!("expected violation"));
    assert_eq!(err.code, Code::AlreadyExists);
    assert_eq!(
        err.message,
        "UNIQUE violation on index SingersByEmail. Row [9] in table Singers duplicates key [q@x] created in this transaction."
    );
    // A clash with a committed row names that row.
    let err = apply(
        &st,
        &[write(
            &st,
            WriteKind::Insert,
            "Singers",
            &["SingerId", "Email"],
            vec![row(vec![s("4"), s("m@x")])],
        )],
    )
    .err()
    .unwrap_or_else(|| panic!("expected violation"));
    assert_eq!(err.code, Code::AlreadyExists);
    assert_eq!(
        err.message,
        "Unique index violation on index SingersByEmail at index key [m@x,1]. It conflicts with row [1] in table Singers."
    );
    // A duplicate primary key: against a committed row, and against one this commit wrote.
    let insert = |id: &str| {
        write(
            &st,
            WriteKind::Insert,
            "Singers",
            &["SingerId"],
            vec![row(vec![s(id)])],
        )
    };
    let err = apply(&st, &[insert("1")])
        .err()
        .unwrap_or_else(|| panic!("expected duplicate"));
    assert_eq!(err.code, Code::AlreadyExists);
    assert_eq!(err.message, "Row [1] in table Singers already exists");
    let err = apply(&st, &[insert("7"), insert("7")])
        .err()
        .unwrap_or_else(|| panic!("expected duplicate"));
    assert_eq!(err.code, Code::InvalidArgument);
    assert_eq!(
        err.message,
        "Row [7] in Singers was already inserted in this transaction."
    );
    // Derived states keep the flag.
    let next = apply(&st, &[]).unwrap_or_else(|e| panic!("{e}"));
    assert!(next.prod_messages);
}
