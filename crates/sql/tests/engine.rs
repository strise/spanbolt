//! Planner pushdown and expression semantics, in process.

use std::collections::HashMap;
use std::sync::Arc;

use prost_types::value::Kind;
use spanbolt_ddl::{Schema, parse_statement};
use spanbolt_proto::spanner as pb;
use spanbolt_sql::{Engine, Options, Txn};
use spanbolt_storage::{Code, DbState, Mutation, WriteKind};
use spanbolt_txn::{Clock, Database, ReadOnlyTxn, ReadWriteTxn};

const DDL: &[&str] = &[
    "CREATE TABLE Singers (SingerId INT64 NOT NULL, Name STRING(MAX), Team STRING(MAX), Score FLOAT64) PRIMARY KEY (SingerId)",
    "CREATE TABLE Albums (SingerId INT64 NOT NULL, AlbumId INT64 NOT NULL, Title STRING(MAX)) PRIMARY KEY (SingerId, AlbumId), INTERLEAVE IN PARENT Singers ON DELETE CASCADE",
    "CREATE INDEX SingersByTeam ON Singers(Team) STORING (Name)",
    "CREATE NULL_FILTERED INDEX SingersByName ON Singers(Name)",
    "CREATE INDEX AlbumsByTitleDesc ON Albums(Title DESC)",
    "CREATE INDEX SingersByTeamScored ON Singers(Team) STORING (Score) WHERE Score IS NOT NULL",
];

fn database() -> Arc<Database> {
    let mut schema = Schema::default();
    for s in DDL {
        let st = parse_statement(s).expect("ddl parses");
        schema.apply(&st).expect("ddl applies");
    }
    let db = Arc::new(Database::new(
        Arc::new(Clock::default()),
        DbState::new(schema).expect("state"),
    ));
    let txn = ReadWriteTxn::new(db.clone());
    let cat = txn.catalog();
    let lv = |vals: Vec<prost_types::Value>| prost_types::ListValue { values: vals };
    let s = |x: &str| prost_types::Value {
        kind: Some(Kind::StringValue(x.into())),
    };
    let null = prost_types::Value {
        kind: Some(Kind::NullValue(0)),
    };
    let num = |x: f64| prost_types::Value {
        kind: Some(Kind::NumberValue(x)),
    };
    let singers = vec![
        lv(vec![s("1"), s("Ann"), s("red"), num(1.5)]),
        lv(vec![s("2"), s("Bob"), s("blue"), num(2.5)]),
        lv(vec![s("3"), null.clone(), s("red"), null.clone()]),
        lv(vec![s("4"), s("Dan"), null.clone(), num(0.5)]),
    ];
    let albums = vec![
        lv(vec![s("1"), s("1"), s("A")]),
        lv(vec![s("1"), s("2"), s("B")]),
        lv(vec![s("2"), s("1"), s("C")]),
    ];
    let cols = |c: &[&str]| c.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let m = vec![
        Mutation::write(
            &cat,
            WriteKind::Insert,
            "Singers",
            &cols(&["SingerId", "Name", "Team", "Score"]),
            &singers,
        )
        .expect("mutation"),
        Mutation::write(
            &cat,
            WriteKind::Insert,
            "Albums",
            &cols(&["SingerId", "AlbumId", "Title"]),
            &albums,
        )
        .expect("mutation"),
    ];
    txn.commit(&cat, &m).expect("commit");
    db
}

fn request(
    sql: &str,
    params: &[(&str, pb::TypeCode, prost_types::Value)],
) -> pb::ExecuteSqlRequest {
    let mut fields = std::collections::BTreeMap::new();
    let mut types = HashMap::new();
    for (n, t, v) in params {
        fields.insert(n.to_string(), v.clone());
        types.insert(
            n.to_string(),
            pb::Type {
                code: *t as i32,
                ..Default::default()
            },
        );
    }
    pb::ExecuteSqlRequest {
        sql: sql.into(),
        params: Some(prost_types::Struct { fields }),
        param_types: types,
        ..Default::default()
    }
}

fn int(i: i64) -> prost_types::Value {
    prost_types::Value {
        kind: Some(Kind::StringValue(i.to_string())),
    }
}

fn text(v: &prost_types::Value) -> String {
    match &v.kind {
        Some(Kind::StringValue(s)) => s.clone(),
        Some(Kind::NumberValue(n)) => n.to_string(),
        Some(Kind::BoolValue(b)) => b.to_string(),
        Some(Kind::ListValue(l)) => format!(
            "[{}]",
            l.values.iter().map(text).collect::<Vec<_>>().join(",")
        ),
        _ => "NULL".into(),
    }
}

fn run(
    engine: &Engine,
    db: &Arc<Database>,
    req: &pb::ExecuteSqlRequest,
) -> Result<Vec<String>, spanbolt_storage::Error> {
    let txn = ReadOnlyTxn {
        version: db.latest(),
    };
    let rs = engine.execute_sql(db, Txn::ReadOnly(&txn), req)?;
    Ok(rs
        .rows
        .iter()
        .map(|r| r.values.iter().map(text).collect::<Vec<_>>().join("|"))
        .collect())
}

fn plan(
    engine: &Engine,
    db: &Arc<Database>,
    sql: &str,
    params: &[(&str, pb::TypeCode, prost_types::Value)],
) -> String {
    engine
        .explain(&db.latest().state, &request(sql, params))
        .expect("plan")
}

#[test]
fn primary_key_point_lookup() {
    let (e, db) = (Engine::default(), database());
    let p = plan(
        &e,
        &db,
        "SELECT Name FROM Singers WHERE SingerId = @id",
        &[("id", pb::TypeCode::Int64, int(2))],
    );
    assert!(p.contains("Scan Table(Singers): key prefix 1"), "{p}");
    let r = run(
        &e,
        &db,
        &request(
            "SELECT Name FROM Singers WHERE SingerId = @id",
            &[("id", pb::TypeCode::Int64, int(2))],
        ),
    )
    .expect("run");
    assert_eq!(r, vec!["Bob"]);
}

#[test]
fn in_unnest_and_range_use_key_ranges() {
    let (e, db) = (Engine::default(), database());
    let p = plan(
        &e,
        &db,
        "SELECT SingerId FROM Singers WHERE SingerId IN UNNEST([1, 3])",
        &[],
    );
    assert!(p.contains("key prefix 1"), "{p}");
    let p = plan(
        &e,
        &db,
        "SELECT SingerId FROM Singers WHERE SingerId > 1 AND SingerId <= 3",
        &[],
    );
    assert!(p.contains("key prefix 0 + range"), "{p}");
    let r = run(
        &e,
        &db,
        &request(
            "SELECT SingerId FROM Singers WHERE SingerId > 1 AND SingerId <= 3 ORDER BY 1",
            &[],
        ),
    )
    .expect("run");
    assert_eq!(r, vec!["2", "3"]);
    let p = plan(
        &e,
        &db,
        "SELECT AlbumId FROM Albums WHERE SingerId = 1",
        &[],
    );
    assert!(p.contains("Scan Table(Albums): key prefix 1"), "{p}");
    let p = plan(&e, &db, "SELECT SingerId FROM Singers", &[]);
    assert!(p.contains("full scan"), "{p}");
}

#[test]
fn secondary_index_equality_and_covering() {
    let (e, db) = (Engine::default(), database());
    let p = plan(&e, &db, "SELECT Name FROM Singers WHERE Team = 'red'", &[]);
    assert!(
        p.contains("Index(Singers#") && p.contains("key prefix 1") && !p.contains("backjoin"),
        "{p}"
    );
    let p = plan(&e, &db, "SELECT Score FROM Singers WHERE Team = 'red'", &[]);
    assert!(p.contains("backjoin"), "{p}");
    let r = run(
        &e,
        &db,
        &request(
            "SELECT SingerId, Score FROM Singers WHERE Team = 'red' ORDER BY 1",
            &[],
        ),
    )
    .expect("run");
    assert_eq!(r, vec!["1|1.5", "3|NULL"]);
    // DESC index range.
    let r = run(&e, &db, &request("SELECT Title FROM Albums@{FORCE_INDEX=AlbumsByTitleDesc} WHERE Title >= 'B' ORDER BY Title", &[])).expect("run");
    assert_eq!(r, vec!["B", "C"]);
}

#[test]
fn join_uses_index_nested_loop() {
    let (e, db) = (Engine::default(), database());
    let sql =
        "SELECT s.Name, a.Title FROM Singers s JOIN Albums a ON a.SingerId = s.SingerId ORDER BY 2";
    let p = plan(&e, &db, sql, &[]);
    assert!(
        p.contains("NestedLoopJoin(Inner, dependent)")
            && p.contains("Scan Table(Albums): key prefix 1"),
        "{p}"
    );
    let r = run(&e, &db, &request(sql, &[])).expect("run");
    assert_eq!(r, vec!["Ann|A", "Ann|B", "Bob|C"]);
    let left = "SELECT s.SingerId, a.AlbumId FROM Singers s LEFT JOIN Albums a ON a.SingerId = s.SingerId ORDER BY 1, 2";
    let r = run(&e, &db, &request(left, &[])).expect("run");
    assert_eq!(r, vec!["1|1", "1|2", "2|1", "3|NULL", "4|NULL"]);
}

#[test]
fn filtered_index_leaves_out_rows_with_null_filter_columns() {
    let (e, db) = (Engine::default(), database());
    // Singer 3 (Team red, Score NULL) has no entry in SingersByTeamScored.
    let forced = "SELECT SingerId FROM Singers@{FORCE_INDEX=SingersByTeamScored} WHERE Score IS NOT NULL AND Team = 'red'";
    assert_eq!(run(&e, &db, &request(forced, &[])).expect("run"), vec!["1"]);
    let unproven =
        "SELECT SingerId FROM Singers@{FORCE_INDEX=SingersByTeamScored} WHERE Team = 'red'";
    let err = run(&e, &db, &request(unproven, &[])).expect_err("rejected");
    assert_eq!(err.code, Code::InvalidArgument);
    assert!(
        err.message.ends_with("for column Score."),
        "{}",
        err.message
    );
    // Without a hint the planner must not pick the filtered index for a Team lookup.
    let auto = "SELECT SingerId FROM Singers WHERE Team = 'red' ORDER BY SingerId";
    assert_eq!(
        run(&e, &db, &request(auto, &[])).expect("run"),
        vec!["1", "3"]
    );
}

#[test]
fn null_filtered_force_index_is_strict() {
    let (e, db) = (Engine::default(), database());
    let bad = run(
        &e,
        &db,
        &request(
            "SELECT SingerId FROM Singers@{FORCE_INDEX=SingersByName} WHERE SingerId = 1",
            &[],
        ),
    );
    let err = bad.expect_err("rejected");
    assert_eq!(err.code, Code::InvalidArgument);
    assert!(
        err.message
            .contains("cannot be used because it filters nulls"),
        "{}",
        err.message
    );
    let hinted = "SELECT SingerId FROM Singers@{FORCE_INDEX=SingersByName, spanner_emulator.disable_query_null_filtered_index_check=true} WHERE SingerId = 1";
    assert!(run(&e, &db, &request(hinted, &[])).is_err());
    let ok = run(&e, &db, &request("SELECT SingerId FROM Singers@{FORCE_INDEX=SingersByName} WHERE Name IS NOT NULL AND Name = 'Ann'", &[])).expect("run");
    assert_eq!(ok, vec!["1"]);
    // An equality comparison proves the key column non-NULL, as on production Spanner.
    let eq = run(
        &e,
        &db,
        &request(
            "SELECT SingerId FROM Singers@{FORCE_INDEX=SingersByName} WHERE Name = 'Ann'",
            &[],
        ),
    )
    .expect("run");
    assert_eq!(eq, vec!["1"]);
    let compat = Engine::new(Options {
        emulator_compat: true,
    });
    assert_eq!(
        run(&compat, &db, &request(hinted, &[])).expect("run"),
        vec!["1"]
    );
}

#[test]
fn three_valued_logic_and_nulls() {
    let (e, db) = (Engine::default(), database());
    let r = run(&e, &db, &request(
        "SELECT CAST(NULL AS INT64) = 1, CAST(NULL AS BOOL) AND FALSE, CAST(NULL AS BOOL) OR TRUE, 1 IN (NULL, 2), 2 IN (NULL, 2), NULL IS NULL, COALESCE(NULL, 3)",
        &[],
    )).expect("run");
    assert_eq!(r, vec!["NULL|false|true|NULL|true|true|3"]);
    let r = run(
        &e,
        &db,
        &request(
            "SELECT COUNT(*), COUNT(Name), SUM(Score), MAX(Name) FROM Singers",
            &[],
        ),
    )
    .expect("run");
    assert_eq!(r, vec!["4|3|4.5|Dan"]);
    let r = run(
        &e,
        &db,
        &request("SELECT SUM(Score) FROM Singers WHERE FALSE", &[]),
    )
    .expect("run");
    assert_eq!(r, vec!["NULL"]);
    let r = run(
        &e,
        &db,
        &request("SELECT Name FROM Singers ORDER BY Name", &[]),
    )
    .expect("run");
    assert_eq!(r[0], "NULL");
    let r = run(
        &e,
        &db,
        &request("SELECT Name FROM Singers ORDER BY Name DESC", &[]),
    )
    .expect("run");
    assert_eq!(r[3], "NULL");
}

#[test]
fn integer_overflow_is_out_of_range() {
    let (e, db) = (Engine::default(), database());
    let err = run(
        &e,
        &db,
        &request(
            "SELECT @big + 1",
            &[("big", pb::TypeCode::Int64, int(i64::MAX))],
        ),
    )
    .expect_err("overflow");
    assert_eq!(err.code, Code::OutOfRange);
    let err = run(
        &e,
        &db,
        &request(
            "SELECT -@small",
            &[("small", pb::TypeCode::Int64, int(i64::MIN))],
        ),
    )
    .expect_err("overflow");
    assert_eq!(err.code, Code::OutOfRange);
    let r = run(
        &e,
        &db,
        &request("SELECT SAFE_CAST('x' AS INT64), CAST('42' AS INT64)", &[]),
    )
    .expect("run");
    assert_eq!(r, vec!["NULL|42"]);
    let err = run(&e, &db, &request("SELECT CAST('x' AS INT64)", &[])).expect_err("bad cast");
    assert_eq!(err.code, Code::OutOfRange);
}

#[test]
fn unsupported_functions_are_unimplemented() {
    let (e, db) = (Engine::default(), database());
    let err =
        run(&e, &db, &request("SELECT JSON_EXTRACT(JSON '1', '$')", &[])).expect_err("unsupported");
    assert_eq!(err.code, Code::Unimplemented);
    let err = run(&e, &db, &request("SELECT 1 FROM Singers@{NOPE=1}", &[])).expect_err("hint");
    assert_eq!(err.message, "Unsupported hint: NOPE.");
}

#[test]
fn dml_reads_its_own_writes() {
    let (e, db) = (Engine::default(), database());
    let txn = Arc::new(ReadWriteTxn::new(db.clone()));
    let exec = |sql: &str| {
        e.execute_sql(&db, Txn::ReadWrite(&txn), &request(sql, &[]))
            .expect("dml")
    };
    let rs = exec("INSERT INTO Singers (SingerId, Name) VALUES (10, 'Eve'), (11, 'Fay')");
    assert!(matches!(
        rs.stats.and_then(|s| s.row_count),
        Some(pb::result_set_stats::RowCount::RowCountExact(2))
    ));
    let rs = exec("UPDATE Singers SET Score = 9 WHERE SingerId >= 10 THEN RETURN SingerId, Score");
    assert_eq!(rs.rows.len(), 2);
    let rs = exec("SELECT COUNT(*) FROM Singers WHERE Score = 9");
    assert_eq!(text(&rs.rows[0].values[0]), "2");
    let rs = exec("DELETE FROM Singers WHERE SingerId = 1");
    assert!(matches!(
        rs.stats.and_then(|s| s.row_count),
        Some(pb::result_set_stats::RowCount::RowCountExact(1))
    ));
    let rs = exec("SELECT COUNT(*) FROM Albums WHERE SingerId = 1");
    assert_eq!(text(&rs.rows[0].values[0]), "0");
    txn.commit(&txn.catalog(), &[]).expect("commit");
    let r = run(&e, &db, &request("SELECT COUNT(*) FROM Singers", &[])).expect("run");
    assert_eq!(r, vec!["5"]);
}

fn rows(e: &Engine, db: &Arc<Database>, sql: &str) -> Vec<String> {
    run(e, db, &request(sql, &[])).unwrap_or_else(|err| panic!("{sql}: {err:?}"))
}

#[test]
fn correlated_subqueries_on_non_key_columns_use_a_hash_lookup() {
    let (e, db) = (Engine::default(), database());
    // Albums.AlbumId is not a key prefix: the subquery scans Albums once and probes a hash.
    let exists = "SELECT SingerId FROM Singers s WHERE EXISTS (SELECT 1 FROM Albums a WHERE a.AlbumId = s.SingerId) ORDER BY 1";
    let p = plan(&e, &db, exists, &[]);
    assert!(p.contains("Subquery Exists, correlated:"), "{p}");
    assert!(
        p.contains("HashLookup (decorrelated, 1 keys)\n        Scan Table(Albums): full scan"),
        "{p}"
    );
    assert_eq!(rows(&e, &db, exists), vec!["1", "2"]);
    let not_exists = "SELECT SingerId FROM Singers s WHERE NOT EXISTS (SELECT 1 FROM Albums a WHERE a.AlbumId = s.SingerId AND a.Title = 'C') ORDER BY 1";
    assert!(plan(&e, &db, not_exists, &[]).contains("HashLookup"));
    assert_eq!(rows(&e, &db, not_exists), vec!["2", "3", "4"]);
    let count = "SELECT SingerId, (SELECT COUNT(*) FROM Albums a WHERE a.AlbumId = s.SingerId) FROM Singers s ORDER BY 1";
    assert!(plan(&e, &db, count, &[]).contains("HashLookup"));
    assert_eq!(rows(&e, &db, count), vec!["1|2", "2|1", "3|0", "4|0"]);
    let array = "SELECT SingerId, ARRAY(SELECT a.Title FROM Albums a WHERE s.SingerId = a.AlbumId ORDER BY a.Title) FROM Singers s ORDER BY 1";
    assert!(plan(&e, &db, array, &[]).contains("HashLookup"));
    assert_eq!(
        rows(&e, &db, array),
        vec!["1|[A,C]", "2|[B]", "3|[]", "4|[]"]
    );
}

#[test]
fn decorrelated_in_subquery_keeps_null_semantics() {
    let (e, db) = (Engine::default(), database());
    // Score has no index. Singer 3 has a NULL Score (no bucket: empty subquery, FALSE);
    // singer 4 has a NULL Team (NULL IN non-empty set is NULL).
    let sql = "SELECT s.SingerId, s.Team IN (SELECT o.Team FROM Singers o WHERE o.Score = s.Score) FROM Singers s ORDER BY 1";
    let p = plan(&e, &db, sql, &[]);
    assert!(p.contains("Subquery In, correlated:"), "{p}");
    assert!(p.contains("HashLookup"), "{p}");
    assert_eq!(
        rows(&e, &db, sql),
        vec!["1|true", "2|true", "3|false", "4|NULL"]
    );
    // The bucket holds a NULL Team: a non-matching value gives NULL, not FALSE.
    let sql = "SELECT s.SingerId, 'x' IN (SELECT o.Team FROM Singers o WHERE o.Score = s.Score) FROM Singers s ORDER BY 1";
    assert_eq!(
        rows(&e, &db, sql),
        vec!["1|false", "2|false", "3|false", "4|NULL"]
    );
}

#[test]
fn correlated_key_lookups_stay_per_row() {
    let (e, db) = (Engine::default(), database());
    let sql = "SELECT SingerId FROM Singers s WHERE EXISTS (SELECT 1 FROM Albums a WHERE a.SingerId = s.SingerId) ORDER BY 1";
    let p = plan(&e, &db, sql, &[]);
    assert!(!p.contains("HashLookup"), "{p}");
    assert!(p.contains("Scan Table(Albums): key prefix 1"), "{p}");
    assert_eq!(rows(&e, &db, sql), vec!["1", "2"]);
    // An index on the correlated column also keeps the per-row lookup.
    let sql = "SELECT s.SingerId FROM Singers s WHERE EXISTS (SELECT 1 FROM Singers o WHERE o.Team = s.Team AND o.SingerId != s.SingerId) ORDER BY 1";
    let p = plan(&e, &db, sql, &[]);
    assert!(!p.contains("HashLookup"), "{p}");
    assert_eq!(rows(&e, &db, sql), vec!["1", "3"]);
}

#[test]
fn outer_joins_emit_unmatched_rows() {
    let (e, db) = (Engine::default(), database());
    // Hash join (equality) and nested loop (theta), each with every join kind.
    for on in ["l.x = r.y", "l.x <= r.y AND l.x >= r.y"] {
        let q = |kind: &str| {
            format!(
                "SELECT l.x, r.y FROM (SELECT x FROM UNNEST([1, 2, 2, 3, NULL]) x) l {kind} JOIN \
                 (SELECT y FROM UNNEST([2, 3, 3, 4, NULL]) y) r ON {on} ORDER BY 1, 2"
            )
        };
        assert_eq!(
            rows(&e, &db, &q("INNER")),
            vec!["2|2", "2|2", "3|3", "3|3"],
            "{on}"
        );
        assert_eq!(
            rows(&e, &db, &q("LEFT")),
            vec!["NULL|NULL", "1|NULL", "2|2", "2|2", "3|3", "3|3"],
            "{on}"
        );
        assert_eq!(
            rows(&e, &db, &q("RIGHT")),
            vec!["NULL|NULL", "NULL|4", "2|2", "2|2", "3|3", "3|3"],
            "{on}"
        );
        assert_eq!(
            rows(&e, &db, &q("FULL")),
            vec![
                "NULL|NULL",
                "NULL|NULL",
                "NULL|4",
                "1|NULL",
                "2|2",
                "2|2",
                "3|3",
                "3|3"
            ],
            "{on}"
        );
    }
}

#[test]
fn unnest_streams_with_offset_and_outer() {
    let (e, db) = (Engine::default(), database());
    assert_eq!(
        rows(
            &e,
            &db,
            "SELECT a, x, o FROM UNNEST([1, 2]) a LEFT JOIN UNNEST(IF(a = 1, [10, 20], [])) x \
             WITH OFFSET o ORDER BY 1, 3"
        ),
        vec!["1|10|0", "1|20|1", "2|NULL|NULL"]
    );
    assert_eq!(
        rows(
            &e,
            &db,
            "SELECT s.SingerId, t FROM Singers s, UNNEST([s.Name, s.Team]) t \
             WHERE t IS NOT NULL ORDER BY 1, 2"
        ),
        vec!["1|Ann", "1|red", "2|Bob", "2|blue", "3|red", "4|Dan"]
    );
}

#[test]
fn set_operations_stream_the_first_input() {
    let (e, db) = (Engine::default(), database());
    let q = |op: &str| {
        format!(
            "SELECT x FROM ((SELECT x FROM UNNEST([1, 1, 1, 2, 3, NULL, NULL]) x) {op} \
             (SELECT x FROM UNNEST([1, 2, 2, NULL, 4]) x) {op} \
             (SELECT x FROM UNNEST([1, 1, 2, NULL, NULL]) x)) ORDER BY 1"
        )
    };
    assert_eq!(
        rows(&e, &db, &q("UNION DISTINCT")),
        vec!["NULL", "1", "2", "3", "4"]
    );
    assert_eq!(rows(&e, &db, &q("UNION ALL")).len(), 17);
    assert_eq!(
        rows(&e, &db, &q("INTERSECT DISTINCT")),
        vec!["NULL", "1", "2"]
    );
    assert_eq!(rows(&e, &db, &q("INTERSECT ALL")), vec!["NULL", "1", "2"]);
    assert_eq!(rows(&e, &db, &q("EXCEPT DISTINCT")), vec!["3"]);
    // {1,1,1,2,3,N,N} - {1,2,2,N,4} = {1,1,3,N}; minus {1,1,2,N,N} = {3}.
    assert_eq!(rows(&e, &db, &q("EXCEPT ALL")), vec!["3"]);
}

#[test]
fn with_entries_stay_visible_to_lazy_scans() {
    let (e, db) = (Engine::default(), database());
    // The WITH reference sits on the dependent side of a lateral join, so it is opened while
    // the body streams, after With::open has returned.
    assert_eq!(
        rows(
            &e,
            &db,
            "WITH t AS (SELECT x FROM UNNEST([1, 2, 3]) x) \
             SELECT s.SingerId, (SELECT COUNT(*) FROM t WHERE t.x <= s.SingerId) \
             FROM Singers s ORDER BY 1"
        ),
        vec!["1|1", "2|2", "3|3", "4|3"]
    );
}

#[test]
fn joins_produce_rows_lazily() {
    let (e, db) = (Engine::default(), database());
    // 10^12 output rows: these only finish if the joins stop at the LIMIT. The probe side is
    // itself a 10^8-row cross join; the build side has 10^4 rows.
    let big = "UNNEST(GENERATE_ARRAY(1, 10000))";
    let probe = format!("(SELECT a, c FROM {big} a, {big} c)");
    let build = format!("(SELECT b FROM {big} b)");
    for sql in [
        format!("SELECT a, b FROM {big} a, {big} c, {big} b LIMIT 3"),
        format!("SELECT a, b FROM {probe} JOIN {build} ON a < b LIMIT 3"),
        format!("SELECT a, b FROM {probe} JOIN {build} ON MOD(a, 2) = MOD(b, 2) LIMIT 3"),
        format!("SELECT a, b FROM {probe} FULL JOIN {build} ON a = b + c LIMIT 3"),
    ] {
        assert_eq!(rows(&e, &db, &sql).len(), 3, "{sql}");
    }
}

#[test]
fn folded_aggregates_match_buffered_semantics() {
    let (e, db) = (Engine::default(), database());
    let q = |sql: &str| rows(&e, &db, sql);
    // Score is FLOAT64 with one NULL; Team has one NULL group.
    assert_eq!(
        q(
            "SELECT Team, COUNT(*), COUNT(Score), SUM(Score), AVG(Score), MIN(Name), MAX(Name), \
           ANY_VALUE(Name) FROM Singers GROUP BY Team ORDER BY Team"
        ),
        vec![
            "NULL|1|1|0.5|0.5|Dan|Dan|Dan",
            "blue|1|1|2.5|2.5|Bob|Bob|Bob",
            "red|2|1|1.5|1.5|Ann|Ann|Ann",
        ]
    );
    // Empty input: COUNT is 0, everything else NULL.
    assert_eq!(
        q(
            "SELECT COUNT(x), COUNTIF(x > 0), SUM(x), AVG(x), MIN(x), MAX(x), ANY_VALUE(x), \
           LOGICAL_AND(x > 0), BIT_OR(x) FROM UNNEST(ARRAY<INT64>[]) x"
        ),
        vec!["0|0|NULL|NULL|NULL|NULL|NULL|NULL|NULL"]
    );
    // All-NULL input behaves like empty input.
    assert_eq!(
        q(
            "SELECT COUNT(x), SUM(x), AVG(x), MIN(x), ANY_VALUE(x), LOGICAL_OR(x > 0), BIT_AND(x) \
           FROM UNNEST([CAST(NULL AS INT64), NULL]) x"
        ),
        vec!["0|NULL|NULL|NULL|NULL|NULL|NULL"]
    );
    assert_eq!(
        q(
            "SELECT COUNTIF(x > 1), LOGICAL_AND(x > 0), LOGICAL_OR(x > 2), BIT_AND(x), BIT_OR(x), \
           BIT_XOR(x), AVG(x) FROM UNNEST([1, NULL, 2, 3]) x"
        ),
        vec!["2|true|true|0|3|0|2"]
    );
    // NaN wins MIN and MAX; NUMERIC SUM and AVG stay exact.
    assert_eq!(
        q("SELECT MIN(x), MAX(x) FROM UNNEST([1.0, CAST('nan' AS FLOAT64), 3.0]) x"),
        vec!["NaN|NaN"]
    );
    assert_eq!(
        q("SELECT SUM(x), AVG(x) FROM UNNEST([NUMERIC '1.5', NUMERIC '2', NULL]) x"),
        vec!["3.5|1.75"]
    );
    // SUM overflow is OUT_OF_RANGE even when a later value would bring it back in range.
    let err = run(
        &e,
        &db,
        &request(
            "SELECT SUM(x) FROM UNNEST([@big, 1, -2]) x",
            &[("big", pb::TypeCode::Int64, int(i64::MAX))],
        ),
    )
    .expect_err("overflow");
    assert_eq!(err.code, Code::OutOfRange);
    // DISTINCT and ORDER BY still buffer.
    assert_eq!(
        q(
            "SELECT COUNT(DISTINCT x), SUM(DISTINCT x), ARRAY_AGG(x ORDER BY x DESC LIMIT 2) \
           FROM UNNEST([1, 2, 2, 3]) x"
        ),
        vec!["3|6|[3,2]"]
    );
}

#[test]
fn catalogs_of_dropped_schemas_are_released() {
    let e = Engine::new(Options::default());
    let mut schema = Schema::default();
    let mut released = Vec::new();
    for s in DDL {
        schema
            .apply(&parse_statement(s).expect("ddl parses"))
            .expect("ddl applies");
        let state = DbState::new(schema.clone()).expect("state");
        e.catalog(&state).expect("catalog");
        released.push(Arc::downgrade(&state.schema));
    }
    // The next lookup prunes every catalog whose schema nothing else holds.
    let live = DbState::new(schema).expect("state");
    e.catalog(&live).expect("catalog");
    assert_eq!(e.cached_catalogs(), 1);
    assert!(released.iter().all(|w| w.strong_count() == 0));
}

#[test]
fn information_schema_table_name_lookup() {
    let (e, db) = (Engine::default(), database());
    let s = |x: &str| prost_types::Value {
        kind: Some(Kind::StringValue(x.into())),
    };
    let null = prost_types::Value {
        kind: Some(Kind::NullValue(0)),
    };
    let st = pb::TypeCode::String;
    let q = |sql: &str, params: &[(&str, pb::TypeCode, prost_types::Value)]| {
        run(&e, &db, &request(sql, params)).unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    };
    let lower = "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.COLUMNS WHERE LOWER(TABLE_NAME) = @t AND TABLE_SCHEMA = '' ORDER BY ORDINAL_POSITION";
    let p = plan(&e, &db, lower, &[("t", st, s("albums"))]);
    assert!(p.contains("Generated(COLUMNS) by TABLE_NAME"), "{p}");
    assert_eq!(
        q(lower, &[("t", st, s("albums"))]),
        vec!["SingerId", "AlbumId", "Title"]
    );
    // The exact predicate still applies to the case-insensitive candidates.
    assert!(q(lower, &[("t", st, s("Albums"))]).is_empty());
    assert!(q(lower, &[("t", st, null)]).is_empty());
    let upper = "SELECT COUNT(*) FROM INFORMATION_SCHEMA.COLUMNS WHERE @t = UPPER(TABLE_NAME)";
    assert_eq!(q(upper, &[("t", st, s("SINGERS"))]), vec!["4"]);
    let exact =
        "SELECT INDEX_NAME FROM INFORMATION_SCHEMA.INDEXES WHERE TABLE_NAME = 'Singers' ORDER BY 1";
    assert!(plan(&e, &db, exact, &[]).contains("by TABLE_NAME"));
    assert_eq!(
        q(exact, &[]),
        vec![
            "PRIMARY_KEY",
            "SingersByName",
            "SingersByTeam",
            "SingersByTeamScored"
        ]
    );
    assert!(
        q(
            "SELECT 1 FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_NAME = 'singers'",
            &[]
        )
        .is_empty()
    );
    // Correlated: the lookup key comes from the outer row.
    let corr = "SELECT t.TABLE_NAME, (SELECT COUNT(*) FROM INFORMATION_SCHEMA.COLUMNS c WHERE c.TABLE_NAME = t.TABLE_NAME AND c.TABLE_SCHEMA = '') FROM INFORMATION_SCHEMA.TABLES t WHERE t.TABLE_SCHEMA = '' ORDER BY 1";
    assert_eq!(q(corr, &[]), vec!["Albums|3", "Singers|4"]);
    // Rows are cached per schema version: DDL makes a new catalog with fresh rows.
    db.update_schema(|st| {
        let mut schema = (*st.schema).clone();
        schema
            .apply(&parse_statement("ALTER TABLE Albums ADD COLUMN Year INT64").expect("ddl"))
            .expect("apply");
        Ok(schema)
    })
    .expect("update schema");
    assert_eq!(
        q(lower, &[("t", st, s("albums"))]),
        vec!["SingerId", "AlbumId", "Title", "Year"]
    );
}

/// Production Spanner analyses window functions and NULLS FIRST/LAST, then rejects them;
/// `emulator_compat` keeps the emulator's analyser errors.
#[test]
fn window_functions_and_null_order_follow_prod() {
    let (e, db) = (Engine::default(), database());
    let err = |e: &Engine, sql: &str| run(e, &db, &request(sql, &[])).expect_err(sql);
    let one = err(
        &e,
        "SELECT ROW_NUMBER() OVER (ORDER BY SingerId) FROM Singers",
    );
    assert_eq!(one.code, Code::Unimplemented);
    assert_eq!(one.message, "Unsupported built-in function: ROW_NUMBER.");
    let count = err(&e, "SELECT COUNT(*) OVER () FROM Singers");
    assert_eq!(count.message, "Unsupported built-in function: COUNT.");
    let two = err(
        &e,
        "SELECT RANK() OVER (ORDER BY Score), SUM(Score) OVER (PARTITION BY Team) FROM Singers",
    );
    assert_eq!(two.code, Code::Unimplemented);
    assert_eq!(
        two.message,
        "Unsupported analytic scan: multiple analytic functions aren't supported"
    );
    for sql in [
        "SELECT SingerId FROM Singers ORDER BY Name NULLS LAST",
        "SELECT SingerId FROM Singers ORDER BY Name DESC NULLS FIRST",
        "SELECT ARRAY_AGG(SingerId ORDER BY Name ASC NULLS LAST) FROM Singers",
    ] {
        let n = err(&e, sql);
        assert_eq!(n.code, Code::InvalidArgument, "{sql}");
        assert!(
            n.message
                .starts_with("Non default sort order of NULL values is not supported."),
            "{sql}: {}",
            n.message
        );
    }
    // The default spellings are accepted and order as without them.
    let asc = run(
        &e,
        &db,
        &request(
            "SELECT SingerId FROM Singers ORDER BY Name ASC NULLS FIRST",
            &[],
        ),
    )
    .expect("run");
    assert_eq!(asc, vec!["3", "1", "2", "4"]);
    let desc = run(
        &e,
        &db,
        &request(
            "SELECT ARRAY_AGG(SingerId ORDER BY Name DESC NULLS LAST) FROM Singers",
            &[],
        ),
    )
    .expect("run");
    assert_eq!(desc, vec!["[4,2,1,3]"]);

    let compat = Engine::new(Options {
        emulator_compat: true,
    });
    let w = err(
        &compat,
        "SELECT ROW_NUMBER() OVER (ORDER BY SingerId) FROM Singers",
    );
    assert_eq!(w.code, Code::InvalidArgument);
    assert!(w.message.starts_with("Analytic functions not supported"));
    let n = err(
        &compat,
        "SELECT SingerId FROM Singers ORDER BY Name ASC NULLS FIRST",
    );
    assert!(
        n.message
            .starts_with("NULLS FIRST and NULLS LAST are not supported")
    );
}

/// Production Spanner cannot read a field of an ARRAY<STRUCT> subquery value the query also
/// returns, unless the subquery yields exactly one row. The emulator accepts it.
#[test]
fn reading_a_field_of_a_returned_struct_array_follows_prod() {
    let (e, db) = (Engine::default(), database());
    let albums = "ARRAY(SELECT AS STRUCT a.AlbumId, a.Title FROM Albums a \
                  WHERE a.SingerId = s.SingerId ORDER BY a.AlbumId DESC LIMIT 1)";
    let singers = format!("(SELECT s.SingerId, {albums} AS last FROM Singers s)");
    let wrap =
        |array: &str| format!("SELECT a, a[SAFE_OFFSET(0)].x FROM (SELECT ARRAY({array}) AS a)");
    let rejected = [
        format!("SELECT c.SingerId, c.last, c.last[SAFE_OFFSET(0)].Title FROM {singers} c"),
        format!("SELECT c.last FROM {singers} c WHERE c.last[SAFE_OFFSET(0)].AlbumId = 2"),
        format!("SELECT c.last FROM {singers} c ORDER BY c.last[SAFE_OFFSET(0)].Title"),
        format!(
            "SELECT c.last, ARRAY(SELECT AS STRUCT b.Title FROM Albums b WHERE b.SingerId = c.SingerId \
             AND b.AlbumId <= c.last[SAFE_OFFSET(0)].AlbumId) FROM {singers} c"
        ),
        format!("SELECT c.last, t.Title FROM {singers} c, UNNEST(c.last) t"),
        format!(
            "WITH c AS {singers} SELECT c.last, ARRAY_FIRST(c.last).Title FROM c WHERE c.SingerId = 1"
        ),
        format!("SELECT ARRAY_REVERSE(c.last), c.last[SAFE_OFFSET(0)].Title FROM {singers} c"),
        wrap("SELECT AS STRUCT v AS x FROM UNNEST([1, 2]) v"),
        wrap("SELECT AS STRUCT v AS x FROM UNNEST([1, 1, 2]) v GROUP BY v"),
        wrap("SELECT AS STRUCT d.x FROM (SELECT 1 AS x UNION ALL SELECT 2 AS x) d"),
        wrap("SELECT AS STRUCT COUNT(*) AS x FROM Singers s GROUP BY s.Team"),
        wrap("SELECT AS STRUCT MAX(s.Name) AS x FROM Singers s HAVING COUNT(*) > 0"),
        wrap("SELECT AS STRUCT 1 AS x FROM UNNEST([1]) v LIMIT 0"),
    ];
    for sql in &rejected {
        let err = run(&e, &db, &request(sql, &[])).expect_err(sql);
        assert_eq!(err.code, Code::Unimplemented, "{sql}");
        assert_eq!(
            err.message,
            "Unsupported query shape: Accessing fields of a struct value also present in the final result is not supported.",
            "{sql}"
        );
    }
    let accepted = [
        format!("SELECT c.SingerId, c.last FROM {singers} c"),
        format!("SELECT c.SingerId, c.last[SAFE_OFFSET(0)].Title FROM {singers} c"),
        format!("SELECT c.last, ARRAY_LENGTH(c.last), c.last[SAFE_OFFSET(0)] IS NULL FROM {singers} c"),
        format!(
            "SELECT d.n, d.t FROM (SELECT ARRAY_LENGTH(c.last) AS n, c.last[SAFE_OFFSET(0)].Title AS t \
             FROM {singers} c) d"
        ),
        format!("SELECT c.last, d.last[SAFE_OFFSET(0)].Title FROM {singers} c JOIN {singers} d USING (SingerId)"),
        format!("SELECT ANY_VALUE(c.last) FROM {singers} c GROUP BY c.last[SAFE_OFFSET(0)].Title"),
        "SELECT a, a[OFFSET(0)].x FROM (SELECT ARRAY(SELECT AS STRUCT s.Name AS x) AS a FROM Singers s)".to_string(),
        wrap("SELECT AS STRUCT 1 AS x"),
        wrap("SELECT AS STRUCT v AS x FROM UNNEST([1]) v WHERE v > 0"),
        wrap("SELECT AS STRUCT MAX(s.Name) AS x FROM Singers s"),
        wrap("SELECT AS STRUCT d.x FROM (SELECT 1 AS x) d CROSS JOIN (SELECT 2 AS y) f"),
    ];
    for sql in &accepted {
        run(&e, &db, &request(sql, &[])).unwrap_or_else(|err| panic!("{sql}: {}", err.message));
    }
    let compat = Engine::new(Options {
        emulator_compat: true,
    });
    for sql in &rejected {
        run(&compat, &db, &request(sql, &[]))
            .unwrap_or_else(|err| panic!("{sql}: {}", err.message));
    }
}
