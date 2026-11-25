//! Negative cases; expected texts were taken from emulator 1.5.59, or from Cloud Spanner where
//! production mode (`Schema::apply`) follows it.

use spanbolt_ddl::{
    DataCheck, DdlError, Schema, parse_statement, parse_statement_emulator_compat, print_statement,
};

const BASE: &[&str] = &[
    "CREATE TABLE P (id INT64 NOT NULL, s STRING(MAX), ts TIMESTAMP, a ARRAY<INT64>, j JSON) PRIMARY KEY (id)",
    "CREATE TABLE C (id INT64 NOT NULL, c INT64 NOT NULL, v STRING(10)) PRIMARY KEY (id, c), INTERLEAVE IN PARENT P ON DELETE CASCADE",
    "CREATE INDEX pi ON P(s)",
    "CREATE TABLE F (id INT64 NOT NULL, pid INT64, CONSTRAINT fkf FOREIGN KEY (pid) REFERENCES P (id)) PRIMARY KEY (id)",
    "CREATE TABLE R (id INT64 NOT NULL, x INT64, FOREIGN KEY (x) REFERENCES P (id)) PRIMARY KEY (id)",
    "CREATE TABLE T (id INT64 NOT NULL, ts TIMESTAMP) PRIMARY KEY (id), ROW DELETION POLICY (OLDER_THAN(ts, INTERVAL 1 DAY))",
    "CREATE TABLE G (id INT64 NOT NULL, x INT64 NOT NULL) PRIMARY KEY (id)",
    "ALTER TABLE G ADD SYNONYM GS",
    "CREATE TABLE H (id INT64 NOT NULL, a STRING(MAX), t TOKENLIST AS (TOKENIZE_FULLTEXT(a)) HIDDEN, n INT64) PRIMARY KEY (id)",
    "CREATE SEARCH INDEX hs ON H(t)",
    "CREATE CHANGE STREAM cs FOR G, H(a)",
    "CREATE SCHEMA s",
    "CREATE TABLE s.T (id INT64 NOT NULL) PRIMARY KEY (id)",
    "CREATE VIEW v SQL SECURITY INVOKER AS SELECT F.id FROM F",
];

fn base() -> Schema {
    let mut s = Schema::default();
    for stmt in BASE {
        s.apply(&parse_statement(stmt).unwrap())
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
    s
}

#[derive(Debug)]
enum K {
    Inv,
    Pre,
    Nf,
    Dup,
    Unimpl,
}

fn kind(e: &DdlError) -> (&'static str, &str) {
    match e {
        DdlError::Syntax(m) => ("Inv", m),
        DdlError::Precondition(m) => ("Pre", m),
        DdlError::NotFound(m) => ("Nf", m),
        DdlError::AlreadyExists(m) | DdlError::Exists(m) => ("Dup", m),
        DdlError::Internal(m) => ("Internal", m),
        DdlError::Unimplemented(m) => ("Unimpl", m),
    }
}

#[test]
fn rejects_like_the_emulator() {
    use K::*;
    let cases: &[(&str, K, &str)] = &[
        (
            "CREATE TABLE p (id INT64 NOT NULL) PRIMARY KEY (id)",
            Dup,
            "Schema object names differ only in case: P, p.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL, id STRING(MAX)) PRIMARY KEY (id)",
            Pre,
            "Duplicate column name D.id.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL) PRIMARY KEY (nope)",
            Pre,
            // Production's lowercase; `--emulator-compat` says `Table D`.
            "table D references nonexistent key column nope.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL) PRIMARY KEY (id, id)",
            Inv,
            "Table D references key column id more than once.",
        ),
        (
            "CREATE TABLE D (id ARRAY<INT64> NOT NULL) PRIMARY KEY (id)",
            Inv,
            "Column D.id has type ARRAY, but is part of the primary key.",
        ),
        (
            "CREATE TABLE D (id JSON NOT NULL) PRIMARY KEY (id)",
            Inv,
            "Column D.id has type JSON, but is part of the primary key.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL) PRIMARY KEY (id), INTERLEAVE IN PARENT Nope",
            Nf,
            "Table not found: Nope",
        ),
        (
            "CREATE TABLE D (x INT64 NOT NULL) PRIMARY KEY (x), INTERLEAVE IN PARENT P",
            Pre,
            "Table D does not reference parent key column id.",
        ),
        (
            "CREATE TABLE D (id STRING(MAX) NOT NULL) PRIMARY KEY (id), INTERLEAVE IN PARENT P",
            Pre,
            "Table D references parent key column id with incorrect type STRING (should be INT64).",
        ),
        (
            "CREATE TABLE D (id INT64) PRIMARY KEY (id), INTERLEAVE IN PARENT P",
            Pre,
            "Table D references parent key column id that is not null, but child key is nullable.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL, x INT64, CONSTRAINT fk FOREIGN KEY (x) REFERENCES Nope (id)) PRIMARY KEY (id)",
            Nf,
            "Table not found: Nope",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL, x STRING(MAX), CONSTRAINT fk FOREIGN KEY (x) REFERENCES P (id)) PRIMARY KEY (id)",
            Pre,
            "The column types are different for column `x` of table `D` and column `id` of table `P` in foreign key `fk`.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL, x INT64, CONSTRAINT fk FOREIGN KEY (y) REFERENCES P (id)) PRIMARY KEY (id)",
            Inv,
            "Column `y` not found for table `D` in foreign key `fk`.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL, x INT64, CONSTRAINT fk FOREIGN KEY (x, id) REFERENCES P (id)) PRIMARY KEY (id)",
            Pre,
            "The number of columns are different for table `D` and table `P` in foreign key `fk`.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL, x INT64, CONSTRAINT fkf FOREIGN KEY (x) REFERENCES P (id)) PRIMARY KEY (id)",
            Dup,
            "Duplicate name in schema: fkf.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL, j JSON, CONSTRAINT fj FOREIGN KEY (j) REFERENCES P (id)) PRIMARY KEY (id)",
            Pre,
            "Column `j` for foreign key `fj` on table `D` has an unsupported type.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL, ts STRING(MAX)) PRIMARY KEY (id), ROW DELETION POLICY (OLDER_THAN(ts, INTERVAL 1 DAY))",
            Inv,
            "Cannot create ROW DELETION POLICY because column ts in table D is not of type TIMESTAMP.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL, x INT64 OPTIONS (allow_commit_timestamp = true)) PRIMARY KEY (id)",
            Pre,
            "Column D.x has invalid allow_commit_timestamp option.  Option only allowed on TIMESTAMP columns.",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL, x STRING(0)) PRIMARY KEY (id)",
            Inv,
            "Bad length for column D.x: 0 : Allowed length range: [1, 2621440].",
        ),
        (
            "CREATE TABLE D (id INT64 NOT NULL, t TOKENLIST) PRIMARY KEY (id)",
            Inv,
            "TOKENLIST column D.t must be HIDDEN.",
        ),
        (
            "CREATE TABLE nos.T (id INT64 NOT NULL) PRIMARY KEY (id)",
            Nf,
            "Schema not found: nos",
        ),
        (
            "CREATE INDEX pi ON P(ts)",
            Dup,
            "Duplicate name in schema: pi.",
        ),
        ("CREATE INDEX pi2 ON Nope(ts)", Nf, "Table not found: Nope"),
        (
            "CREATE INDEX pi2 ON P(nope)",
            Pre,
            "Index pi2 specifies key column nope which does not exist in the index's base table.",
        ),
        (
            "CREATE INDEX pi2 ON P(a)",
            Pre,
            "Cannot reference ARRAY a in the creation of index pi2.",
        ),
        (
            "CREATE INDEX pi2 ON P(s) STORING (s)",
            Inv,
            "Index pi2 specifies stored column s already specified as primary key.",
        ),
        (
            "CREATE INDEX pi2 ON P(s) STORING (id)",
            Pre,
            "Index pi2 specifies stored column id which is a key of table P.",
        ),
        (
            "CREATE INDEX pi2 ON P(s, s)",
            Inv,
            "Index pi2 specifies key column s twice.",
        ),
        (
            "CREATE INDEX pi2 ON C(v), INTERLEAVE IN Nope",
            Pre,
            "Cannot interleave index pi2 within nonexistent table Nope.",
        ),
        (
            "CREATE INDEX pi2 ON C(v), INTERLEAVE IN C",
            Pre,
            "Index pi2 specifies keys that are not a prefix of the keys of table C, yet is to be interleaved in C.",
        ),
        (
            "CREATE INDEX ti2 ON s.T(id)",
            Pre,
            "Index ti2 does not match the schema of the indexed table s.T.",
        ),
        (
            "CREATE SEARCH INDEX hs2 ON H(t) ORDER BY n",
            Inv,
            "Column n in search index hs2 must be NOT NULL.",
        ),
        (
            "DROP TABLE P",
            Pre,
            "Cannot drop table P with interleaved tables: C.",
        ),
        ("DROP TABLE Nope", Nf, "Table not found: Nope"),
        (
            "DROP TABLE G",
            Pre,
            "Cannot drop table G. Table G is tracked by 1 Change Stream: cs. Tables explicitly tracked by a Change Stream cannot be dropped. Please drop the Change Stream or modify its FOR clause to stop tracking the table explicitly before dropping the table.",
        ),
        (
            "DROP TABLE F",
            Pre,
            "Cannot drop TABLE `F` on which there are dependent views: v.",
        ),
        ("DROP INDEX Nope", Nf, "Index not found: Nope"),
        (
            "ALTER TABLE P DROP COLUMN s",
            Pre,
            "Cannot drop column s from table P because it is used by index pi.",
        ),
        (
            "ALTER TABLE P DROP COLUMN id",
            Inv,
            "Cannot drop key column id from table P.",
        ),
        (
            "ALTER TABLE P DROP COLUMN nope",
            Nf,
            "Column not found in table P: nope",
        ),
        (
            "ALTER TABLE H DROP COLUMN a",
            Pre,
            "Cannot drop column H.a. Column a is tracked by 1 Change Stream: cs. Columns explicitly tracked by a Change Stream cannot be dropped. Please alter the Change Stream to stop tracking the column before dropping the column.",
        ),
        (
            "ALTER TABLE T DROP COLUMN ts",
            Inv,
            "ROW DELETION POLICY will break if drop/alter column named ts in table T. Row Deletion Policy must be set on column of type TIMESTAMP.",
        ),
        (
            "ALTER TABLE P ADD COLUMN s STRING(MAX)",
            Pre,
            "Duplicate column name P.s.",
        ),
        (
            "ALTER TABLE P ALTER COLUMN s INT64",
            Inv,
            "Cannot change type of column `s` from `STRING` to `INT64`",
        ),
        (
            "ALTER TABLE P ALTER COLUMN id INT64",
            Inv,
            "Cannot change key column P.id.",
        ),
        (
            "ALTER TABLE P DROP CONSTRAINT nope",
            Nf,
            "nope is not a constraint in P",
        ),
        (
            "ALTER TABLE P DROP ROW DELETION POLICY",
            Inv,
            "ROW DELETION POLICY does not exist on table P.",
        ),
        (
            "ALTER TABLE T ADD ROW DELETION POLICY (OLDER_THAN(ts, INTERVAL 2 DAY))",
            Inv,
            "Cannot create ROW DELETION POLICY because there is already one on column named ts in table T.",
        ),
        (
            "ALTER TABLE P SET ON DELETE CASCADE",
            Inv,
            "Cannot SET ON DELETE on table P that does not have an INTERLEAVE clause.",
        ),
        (
            "ALTER TABLE G ADD SYNONYM GS3",
            Inv,
            "Cannot add a synonym to table G because the table already has a synonym GS.",
        ),
        (
            "ALTER TABLE G DROP SYNONYM nope",
            Inv,
            "SYNONYM nope does not exist on table G.",
        ),
        (
            "ALTER TABLE R ALTER COLUMN x SET OPTIONS (allow_commit_timestamp = true)",
            Pre,
            "Cannot set the commit_timestamp option for column `x` of table `R`. It is used by one or more foreign keys: ",
        ),
        (
            "ALTER INDEX pi ADD STORED COLUMN id",
            Pre,
            "Column id is already existed in index pi.",
        ),
        (
            "ALTER INDEX pi DROP STORED COLUMN ts",
            Nf,
            "Index pi does not have stored column ts.",
        ),
        (
            "ALTER INDEX nope ADD STORED COLUMN ts",
            Nf,
            "Index not found: nope",
        ),
        ("RENAME TABLE G TO P", Dup, "Duplicate name in schema: P."),
        (
            "CREATE VIEW v SQL SECURITY INVOKER AS SELECT 1 AS x",
            Dup,
            "Duplicate name in schema: v.",
        ),
        (
            "CREATE VIEW v2 AS SELECT 1 AS x",
            Inv,
            "View `v2` is missing the SQL SECURITY clause.",
        ),
        ("DROP VIEW nope", Nf, "View not found: nope"),
        (
            "DROP SCHEMA s",
            Pre,
            "Cannot drop schema s while it has dependencies.\nDependent tables:s.T",
        ),
        (
            "CREATE SEQUENCE sq",
            Inv,
            "The sequence does not have a valid sequence kind. Please specify the sequence kind explicitly or set the database option `default_sequence_kind`.",
        ),
        ("DROP SEQUENCE nope", Nf, "Sequence not found: nope"),
        (
            "CREATE CHANGE STREAM cs2 FOR nope",
            Inv,
            "Change Stream cs2 cannot track nope because either Change Streams do not track database objects of this type, or this Table does not exist.",
        ),
        (
            "CREATE CHANGE STREAM cs2 FOR G(nope)",
            Inv,
            "Change Stream cs2 cannot track column nope because it does not exist in Table G.",
        ),
        (
            "DROP CHANGE STREAM nope",
            Nf,
            "Change Stream not found: nope",
        ),
        (
            "CREATE TABLE SELECT (id INT64 NOT NULL) PRIMARY KEY (id)",
            Inv,
            "Error parsing Spanner DDL statement: CREATE TABLE SELECT (id INT64 NOT NULL) PRIMARY KEY (id) : Syntax error on line 1, column 14: Encountered 'SELECT' while parsing: identifier",
        ),
        (
            "CREATE TABLE K (id INT64 NOT NULL) PRIMARY KEY (id) garbage",
            Inv,
            "Error parsing Spanner DDL statement: CREATE TABLE K (id INT64 NOT NULL) PRIMARY KEY (id) garbage : Syntax error on line 1, column 53: Expecting 'EOF' but found 'garbage'",
        ),
        (
            "CREATE INDEX px ON P(s) WHERE ts IS NOT NULL",
            Unimpl,
            "Cannot null-filter column ts that is not in the index px.",
        ),
        (
            "CREATE INDEX ci ON C(c2) STORING ()",
            Inv,
            "Encountered ')' while parsing: identifier",
        ),
    ];
    let schema = base();
    let mut failures = Vec::new();
    for (sql, want_kind, want_msg) in cases {
        let mut s = schema.clone();
        let res = parse_statement(sql).and_then(|st| s.apply(&st));
        match res {
            Ok(()) => failures.push(format!("{sql}: expected error, got OK")),
            Err(e) => {
                let (k, m) = kind(&e);
                if k != format!("{want_kind:?}") || !m.contains(want_msg) {
                    failures.push(format!(
                        "{sql}:\n  want {want_kind:?} {want_msg}\n  got  {k} {m}"
                    ));
                }
                assert_eq!(s, schema, "failed statement changed the schema: {sql}");
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn version_bumps_only_on_success() {
    let mut s = base();
    let v = s.version;
    assert!(
        s.apply(&parse_statement("DROP TABLE Nope").unwrap())
            .is_err()
    );
    assert_eq!(s.version, v);
    s.apply(&parse_statement("DROP TABLE IF EXISTS Nope").unwrap())
        .unwrap();
    assert_eq!(s.version, v + 1);
}

#[test]
fn add_not_null_column_is_a_data_check() {
    let s = base();
    let stmt = parse_statement("ALTER TABLE G ADD COLUMN x2 INT64 NOT NULL").unwrap();
    assert_eq!(
        s.data_checks(&stmt),
        vec![DataCheck::TableEmpty {
            table: "G".into(),
            message: "Cannot add NOT NULL column G.x2 to existing table G.".into(),
        }]
    );
    let with_default =
        parse_statement("ALTER TABLE G ADD COLUMN x2 INT64 NOT NULL DEFAULT (0)").unwrap();
    assert!(s.data_checks(&with_default).is_empty());
}

#[test]
fn unnamed_constraints_get_generated_names() {
    let s = base();
    let r = s.table("R").unwrap();
    assert!(r.foreign_keys[0].generated_name);
    assert!(r.foreign_keys[0].name.starts_with("FK_R_P_"));
    let mut s2 = s.clone();
    let name = r.foreign_keys[0].name.clone();
    s2.apply(&parse_statement(&format!("ALTER TABLE R DROP CONSTRAINT {name}")).unwrap())
        .unwrap();
    assert!(s2.table("R").unwrap().foreign_keys.is_empty());
}

#[test]
fn rename_with_synonym_updates_references() {
    let mut s = base();
    s.apply(&parse_statement("ALTER TABLE P RENAME TO P2, ADD SYNONYM P").unwrap())
        .unwrap();
    assert_eq!(s.table("P2").unwrap().synonym.as_deref(), Some("P"));
    assert_eq!(
        s.table("C").unwrap().interleave.as_ref().unwrap().parent,
        "P2"
    );
    assert_eq!(s.table("F").unwrap().foreign_keys[0].ref_table, "P2");
    assert_eq!(s.index("pi").unwrap().table, "P2");
    assert_eq!(s.resolve_table("p").unwrap().name, "P2");
    s.apply(&parse_statement("ALTER TABLE P2 DROP SYNONYM P").unwrap())
        .unwrap();
    assert!(s.resolve_table("P").is_none());
}

#[test]
fn null_filter_on_index_key_storing_or_table_key() {
    let mut s = base();
    for stmt in [
        "CREATE INDEX f1 ON P(s) WHERE s IS NOT NULL",
        "CREATE INDEX f2 ON P(s) STORING (ts) WHERE ts IS NOT NULL",
        "CREATE INDEX f3 ON P(s) WHERE id IS NOT NULL",
    ] {
        s.apply(&parse_statement(stmt).unwrap())
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
}

#[test]
fn drop_stored_null_filter_column() {
    let mut s = base();
    let create = "CREATE INDEX f1 ON P(s) STORING (ts, a) WHERE ts IS NOT NULL AND a IS NOT NULL";
    s.apply(&parse_statement(create).unwrap()).unwrap();
    let before = s.index("f1").unwrap().clone();
    // Production rejects it and leaves the index alone.
    let drop_a = parse_statement("ALTER INDEX f1 DROP STORED COLUMN a").unwrap();
    let e = s.apply(&drop_a).unwrap_err();
    assert_eq!(
        kind(&e),
        ("Inv", "Cannot drop null filtered column a in index f1.")
    );
    assert_eq!(s.index("f1"), Some(&before));
    // The emulator drops the column's conjunct with it, then the WHERE clause.
    s.apply_emulator_compat(&drop_a).unwrap();
    let f1 = s.index("f1").unwrap();
    assert_eq!(f1.storing, ["ts"]);
    assert_eq!(f1.filter.as_deref(), Some("ts IS NOT NULL"));
    s.apply_emulator_compat(&parse_statement("ALTER INDEX f1 DROP STORED COLUMN ts").unwrap())
        .unwrap();
    let f1 = s.index("f1").unwrap();
    assert!(f1.storing.is_empty());
    assert_eq!(f1.filter, None);
}

/// Spanner DDL spanbolt doesn't model is UNIMPLEMENTED; DDL Spanner doesn't have stays a syntax
/// error.
#[test]
fn unsupported_ddl_is_unimplemented() {
    for (sql, what) in [
        (
            "CREATE QUEUE Q (Id INT64, Payload BYTES(MAX) NOT NULL) PRIMARY KEY (Id)",
            "CREATE QUEUE",
        ),
        ("DROP QUEUE Q", "DROP QUEUE"),
        (
            "CREATE PLACEMENT p OPTIONS (instance_partition = 'p1')",
            "CREATE PLACEMENT",
        ),
        (
            "CREATE FUNCTION F(x INT64) RETURNS INT64 AS (x + 1)",
            "CREATE FUNCTION",
        ),
        (
            "CREATE OR REPLACE FUNCTION F(x INT64) RETURNS INT64 AS (x + 1)",
            "CREATE FUNCTION",
        ),
        ("DROP FUNCTION F", "DROP FUNCTION"),
        (
            "CREATE VECTOR INDEX V ON T(E) OPTIONS (distance_type = 'COSINE')",
            "CREATE VECTOR INDEX",
        ),
        (
            "ALTER VECTOR INDEX V ADD STORED COLUMN X",
            "ALTER VECTOR INDEX",
        ),
        ("DROP VECTOR INDEX V", "DROP VECTOR INDEX"),
        (
            "CREATE INDEX I ON P((JSON_VALUE(j, '$.city')))",
            "An index key expression",
        ),
        (
            "CREATE INDEX I ON P(s, (LOWER(s)))",
            "An index key expression",
        ),
    ] {
        let e = parse_statement(sql).expect_err(sql);
        assert_eq!(
            kind(&e),
            (
                "Unimpl",
                format!("{what} is not supported by spanbolt yet.").as_str()
            ),
            "{sql}"
        );
    }
    for (sql, msg) in [
        (
            "CREATE WIDGET W",
            "Error parsing Spanner DDL statement: CREATE WIDGET W : Syntax error on line 1, column 8: Encountered 'WIDGET' while parsing: create_statement",
        ),
        (
            "ALTER WIDGET W",
            "Error parsing Spanner DDL statement: ALTER WIDGET W : Syntax error on line 1, column 7: Encountered 'WIDGET' while parsing: alter_statement",
        ),
        (
            "DROP WIDGET W",
            "Error parsing Spanner DDL statement: DROP WIDGET W : Syntax error on line 1, column 6: Encountered 'WIDGET' while parsing: drop_statement",
        ),
    ] {
        assert_eq!(kind(&parse_statement(sql).expect_err(sql)), ("Inv", msg));
    }
}

fn applied(stmts: &[&str]) -> Result<Schema, DdlError> {
    let mut s = base();
    for stmt in stmts {
        s.apply(&parse_statement(stmt)?)?;
    }
    Ok(s)
}

#[test]
fn column_keyword_is_optional() {
    let s = applied(&[
        "ALTER TABLE G ADD y INT64",
        "ALTER TABLE G ALTER y INT64 NOT NULL",
        "ALTER TABLE G ALTER y SET DEFAULT (7)",
        "ALTER TABLE G DROP x",
    ])
    .unwrap();
    let g = s.table("G").unwrap();
    let names: Vec<_> = g.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "y"]);
    assert_eq!(g.columns[1].default.as_deref(), Some("7"));
    // ADD still reads constraints, synonyms and row deletion policies first.
    applied(&[
        "ALTER TABLE G ADD CHECK (x > 0)",
        "ALTER TABLE G ADD CONSTRAINT ck CHECK (x < 9)",
    ])
    .unwrap();
    let e = parse_statement("ALTER TABLE G ADD IF NOT EXISTS y INT64").unwrap_err();
    assert!(
        e.to_string()
            .ends_with("column 19: Encountered 'IF' while parsing: identifier"),
        "{e}"
    );
}

#[test]
fn primary_key_inside_the_table_body() {
    for (sql, key) in [
        (
            "CREATE TABLE X (Id INT64 NOT NULL PRIMARY KEY, V STRING(MAX))",
            "PRIMARY KEY(Id)",
        ),
        (
            "CREATE TABLE X (Id INT64, V STRING(MAX), PRIMARY KEY (Id, V DESC))",
            "PRIMARY KEY(Id, V DESC)",
        ),
        (
            "CREATE TABLE X (PRIMARY KEY (Id), Id INT64)",
            "PRIMARY KEY(Id)",
        ),
        ("CREATE TABLE X (Id INT64, PRIMARY KEY ())", "PRIMARY KEY()"),
        (
            "CREATE TABLE X (id INT64 NOT NULL, Id2 INT64 NOT NULL, PRIMARY KEY (id, Id2)), INTERLEAVE IN PARENT P ON DELETE CASCADE",
            "PRIMARY KEY(id, Id2)",
        ),
    ] {
        let stmt = parse_statement(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let printed = print_statement(&stmt);
        assert!(printed.contains(key), "{sql} printed {printed}");
        applied(&[sql]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for (sql, msg) in [
        (
            "CREATE TABLE X (Id INT64, PRIMARY KEY (Id)) PRIMARY KEY (Id)",
            "Can only specify PRIMARY KEY once.",
        ),
        (
            "CREATE TABLE X (Id INT64 PRIMARY KEY, J INT64 PRIMARY KEY)",
            "Multiple columns declared as PRIMARY KEY",
        ),
        (
            "CREATE TABLE X (Id INT64 PRIMARY KEY, J INT64, PRIMARY KEY (J))",
            "Can only specify PRIMARY KEY once.",
        ),
        (
            "CREATE TABLE X (Id INT64 PRIMARY KEY, J INT64) PRIMARY KEY (J)",
            "Can only specify PRIMARY KEY once.",
        ),
        (
            "CREATE TABLE X (Id INT64, PRIMARY KEY (Id), PRIMARY KEY (Id))",
            "Can only specify PRIMARY KEY once.",
        ),
        (
            "ALTER TABLE G ADD COLUMN y INT64 PRIMARY KEY",
            "Unexpected PRIMARY KEY clause",
        ),
    ] {
        let e = parse_statement(sql).expect_err(sql);
        assert_eq!(
            kind(&e),
            (
                "Inv",
                format!("Error parsing Spanner DDL statement: {sql} : {msg}").as_str()
            )
        );
    }
}

#[test]
fn if_exists_forms() {
    let s = applied(&[
        "ALTER TABLE IF EXISTS Nope ADD COLUMN y INT64",
        "ALTER TABLE IF EXISTS G ADD COLUMN y INT64",
        "CREATE CHANGE STREAM IF NOT EXISTS cs FOR ALL",
        "CREATE CHANGE STREAM IF NOT EXISTS cs2 FOR G",
    ])
    .unwrap();
    assert!(s.table("G").unwrap().column("y").is_some());
    let streams: Vec<_> = s
        .opaque
        .iter()
        .filter(|o| o.kind == "CHANGE STREAM")
        .map(|o| o.ddl.as_str())
        .collect();
    assert_eq!(
        streams,
        [
            "CREATE CHANGE STREAM cs FOR G, H(a)",
            "CREATE CHANGE STREAM cs2 FOR G"
        ]
    );
    let e = applied(&["ALTER TABLE IF EXISTS G DROP COLUMN Nope"]).unwrap_err();
    assert_eq!(kind(&e), ("Nf", "Column not found in table G: Nope"));
    let stmt = parse_statement("ALTER TABLE IF EXISTS G ADD COLUMN y INT64").unwrap();
    assert_eq!(
        print_statement(&stmt),
        "ALTER TABLE IF EXISTS G ADD COLUMN y INT64"
    );
}

#[test]
fn alter_search_index() {
    let s = applied(&[
        "ALTER TABLE H ADD COLUMN u TOKENLIST AS (TOKENIZE_FULLTEXT(a)) HIDDEN",
        "ALTER SEARCH INDEX hs ADD COLUMN u",
        "ALTER SEARCH INDEX hs ADD STORED COLUMN n",
    ])
    .unwrap();
    let hs = &s.search_indexes["hs"];
    assert_eq!(hs.columns, ["t", "u"]);
    assert_eq!(hs.storing, ["n"]);
    let s2 = {
        let mut s2 = s.clone();
        s2.apply(&parse_statement("ALTER SEARCH INDEX hs DROP COLUMN t").unwrap())
            .unwrap();
        s2.apply(&parse_statement("ALTER SEARCH INDEX hs DROP STORED COLUMN n").unwrap())
            .unwrap();
        s2
    };
    assert_eq!(s2.search_indexes["hs"].columns, ["u"]);
    assert!(s2.search_indexes["hs"].storing.is_empty());
    for (sql, k, msg) in [
        (
            "ALTER SEARCH INDEX hs ADD COLUMN t",
            "Inv",
            "Index hs specifies key column t twice.",
        ),
        (
            "ALTER SEARCH INDEX hs ADD COLUMN nope",
            "Pre",
            "Index hs specifies key column nope which does not exist in the index's base table.",
        ),
        (
            "ALTER SEARCH INDEX hs ADD COLUMN a",
            "Pre",
            "Cannot alter non-TOKENLIST key column a in search index hs. You can use STORED COLUMN in ALTER SEARCH INDEX statement if you want to modify a stored column in this search index.",
        ),
        (
            "ALTER SEARCH INDEX hs ADD STORED COLUMN n",
            "Inv",
            "Index hs specifies stored column n twice.",
        ),
        (
            "ALTER SEARCH INDEX hs DROP STORED COLUMN a",
            "Nf",
            "Column not found in table hs: a",
        ),
        (
            "ALTER SEARCH INDEX nope ADD COLUMN u",
            "Nf",
            "Index not found: nope",
        ),
    ] {
        let mut s3 = s.clone();
        let e = s3.apply(&parse_statement(sql).unwrap()).expect_err(sql);
        assert_eq!(kind(&e), (k, msg), "{sql}");
    }
    let e = applied(&["ALTER SEARCH INDEX hs DROP COLUMN t"]).unwrap_err();
    assert_eq!(
        kind(&e),
        (
            "Pre",
            "Cannot drop the only key t in search index hs. You can use DROP SEARCH INDEX to completely drop this search index."
        )
    );
    let sql = "ALTER SEARCH INDEX hs ADD u";
    assert_eq!(
        kind(&parse_statement(sql).unwrap_err()),
        (
            "Inv",
            "Error parsing Spanner DDL statement: ALTER SEARCH INDEX hs ADD u : Syntax error on line 1, column 23: Encountered 'ADD' while parsing: alter_search_index_statement"
        )
    );
    // The emulator's grammar has no ALTER SEARCH INDEX.
    let sql = "ALTER SEARCH INDEX hs ADD COLUMN u";
    assert_eq!(
        kind(&parse_statement_emulator_compat(sql).unwrap_err()),
        (
            "Inv",
            "Error parsing Spanner DDL statement: ALTER SEARCH INDEX hs ADD COLUMN u : Syntax error on line 1, column 7: Encountered 'SEARCH' while parsing: alter_statement"
        )
    );
}

#[test]
fn interleave_in_rejects_on_delete() {
    let sql = "CREATE TABLE D (id INT64 NOT NULL, c INT64 NOT NULL) PRIMARY KEY (id, c), INTERLEAVE IN P ON DELETE CASCADE";
    assert_eq!(
        kind(&parse_statement(sql).unwrap_err()),
        (
            "Inv",
            "table D uses INTERLEAVE IN and ON DELETE, which is not allowed since there is no parent/child relationship between rows of the two tables. Did you mean to use INTERLEAVE IN PARENT?"
        )
    );
    assert_eq!(
        kind(&parse_statement_emulator_compat(sql).unwrap_err()),
        ("Inv", "Cannot SET ON DELETE on an INTERLEAVE IN table D.")
    );
    let e = applied(&[
        "CREATE TABLE D (id INT64 NOT NULL, c INT64 NOT NULL) PRIMARY KEY (id, c), INTERLEAVE IN P",
        "ALTER TABLE D SET ON DELETE CASCADE",
    ])
    .unwrap_err();
    assert_eq!(
        kind(&e),
        ("Inv", "Cannot SET ON DELETE on an INTERLEAVE IN table D.")
    );
}

#[test]
fn locality_groups_print_and_reload() {
    let mut s = Schema::default();
    for sql in [
        "CREATE LOCALITY GROUP lg OPTIONS (storage = 'ssd', ssd_to_hdd_spill_timespan = '10d')",
        "ALTER LOCALITY GROUP `default` SET OPTIONS (storage = 'hdd')",
        "CREATE TABLE P (id INT64 NOT NULL, ts TIMESTAMP OPTIONS (allow_commit_timestamp = true, locality_group = 'lg')) PRIMARY KEY (id), OPTIONS (locality_group = 'lg')",
        "CREATE TABLE C (id INT64 NOT NULL, c INT64 NOT NULL) PRIMARY KEY (id, c), INTERLEAVE IN PARENT P ON DELETE CASCADE, OPTIONS (locality_group = 'lg')",
        "CREATE INDEX ci ON C(c) OPTIONS (locality_group = 'lg')",
    ] {
        s.apply(&parse_statement(sql).unwrap())
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let ddl = s.to_ddl();
    assert_eq!(
        ddl,
        [
            // By name, `default` included (conformance/cases/ddl/object_order.case).
            "ALTER LOCALITY GROUP `default` SET OPTIONS (\n  storage = 'hdd'\n)",
            "CREATE LOCALITY GROUP lg OPTIONS (\n  storage = 'ssd',\n  ssd_to_hdd_spill_timespan = '10d'\n)",
            "CREATE TABLE P (\n  id INT64 NOT NULL,\n  ts TIMESTAMP OPTIONS (\n    locality_group = 'lg',\n    allow_commit_timestamp = true\n  ),\n) PRIMARY KEY(id), OPTIONS (\n  locality_group = 'lg'\n)",
            "CREATE TABLE C (\n  id INT64 NOT NULL,\n  c INT64 NOT NULL,\n) PRIMARY KEY(id, c),\n  INTERLEAVE IN PARENT P ON DELETE CASCADE, OPTIONS (\n  locality_group = 'lg'\n)",
            "CREATE INDEX ci ON C(c) OPTIONS (\n  locality_group = 'lg'\n)",
        ]
    );
    // Snapshots store `to_ddl` and load it back statement by statement.
    let mut back = Schema::default();
    for sql in &ddl {
        back.apply_emulator_compat(&parse_statement(sql).unwrap())
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(back.to_ddl(), ddl);
    assert_eq!(
        s.to_ddl_emulator_compat(),
        [
            "CREATE TABLE P (\n  id INT64 NOT NULL,\n  ts TIMESTAMP OPTIONS (\n    allow_commit_timestamp = true\n  ),\n) PRIMARY KEY(id)",
            "CREATE TABLE C (\n  id INT64 NOT NULL,\n  c INT64 NOT NULL,\n) PRIMARY KEY(id, c),\n  INTERLEAVE IN PARENT P ON DELETE CASCADE",
            "CREATE INDEX ci ON C(c)",
            "CREATE LOCALITY GROUP lg OPTIONS ( storage = 'ssd', ssd_to_hdd_spill_timespan = '10d' )",
            "CREATE LOCALITY GROUP `default` OPTIONS ( storage = 'hdd' )",
        ]
    );
    // print_statement output parses back to the same statement.
    for sql in [
        "CREATE LOCALITY GROUP IF NOT EXISTS lg OPTIONS (storage = 'hdd')",
        "ALTER LOCALITY GROUP lg SET OPTIONS (ssd_to_hdd_spill_timespan = NULL)",
        "DROP LOCALITY GROUP IF EXISTS lg",
        "ALTER TABLE P SET OPTIONS (locality_group = NULL)",
        "ALTER TABLE P ALTER COLUMN ts SET OPTIONS (allow_commit_timestamp = false, locality_group = 'lg')",
    ] {
        let stmt = parse_statement(sql).unwrap();
        assert_eq!(
            parse_statement(&print_statement(&stmt)).unwrap(),
            stmt,
            "{sql}"
        );
    }
}

#[test]
fn locality_group_errors_follow_the_mode() {
    let prod = |stmts: &[&str]| {
        let mut s = Schema::default();
        for sql in stmts {
            s.apply(&parse_statement(sql)?)?;
        }
        Ok::<_, DdlError>(s)
    };
    let compat = |stmts: &[&str]| {
        let mut s = Schema::default();
        for sql in stmts {
            s.apply_emulator_compat(&parse_statement_emulator_compat(sql)?)?;
        }
        Ok::<_, DdlError>(s)
    };
    type Outcome<'a> = (&'a str, &'a str);
    let cases: &[(&[&str], Outcome, Outcome)] = &[
        (
            &[
                "CREATE LOCALITY GROUP lg",
                "CREATE LOCALITY GROUP IF NOT EXISTS lg",
            ],
            (
                "Inv",
                "Could not create locality group because it already exists: lg",
            ),
            ("ok", ""),
        ),
        (
            &["DROP LOCALITY GROUP IF EXISTS lg"],
            ("Inv", "Cannot drop locality group lg - it does not exist."),
            ("ok", ""),
        ),
        (
            &["ALTER LOCALITY GROUP lg SET OPTIONS (storage = 'hdd')"],
            ("Nf", ""),
            ("Nf", "Locality group not found: lg"),
        ),
        (
            &[
                "CREATE LOCALITY GROUP lg",
                "CREATE TABLE T (id INT64 OPTIONS (locality_group = 'lg')) PRIMARY KEY (id)",
                "DROP LOCALITY GROUP lg",
            ],
            (
                "Inv",
                "Cannot drop locality group lg - it is still referenced.",
            ),
            (
                "Pre",
                "Locality group lg cannot be dropped as it is referenced.",
            ),
        ),
        (
            &[
                "CREATE LOCALITY GROUP lg OPTIONS (storage = 'hdd', ssd_to_hdd_spill_timespan = '1d')",
            ],
            (
                "Pre",
                "Locality Group lg has invalid storage option: hdd. If the ssd_to_hdd_spill_timespan is set, then the storage option is required to be \"ssd\".",
            ),
            ("ok", ""),
        ),
        (
            &["CREATE TABLE T (id INT64) PRIMARY KEY (id), OPTIONS (locality_group = 'nope')"],
            ("Nf", ""),
            ("Nf", "Locality group not found: nope"),
        ),
    ];
    let outcome = |r: Result<Schema, DdlError>| match r {
        Ok(_) => ("ok".to_string(), String::new()),
        Err(e) => {
            let (k, m) = kind(&e);
            (k.to_string(), m.to_string())
        }
    };
    for (stmts, p, c) in cases {
        let want = |(k, m): (&str, &str)| (k.to_string(), m.to_string());
        assert_eq!(outcome(prod(stmts)), want(*p), "prod {stmts:?}");
        assert_eq!(outcome(compat(stmts)), want(*c), "compat {stmts:?}");
    }
    // Unquoted `default` is a keyword in production's grammar, a name in the emulator's.
    let sql = "ALTER LOCALITY GROUP default SET OPTIONS (storage = 'hdd')";
    assert!(parse_statement(sql).is_err());
    assert!(parse_statement_emulator_compat(sql).is_ok());
}

/// Production's per-database object counts (5000 tables, 10000 indexes), which a conformance case
/// can't reach cheaply. The schema is filled directly: applying thousands of statements one by one
/// would make the test slow.
#[test]
fn database_object_count_limits() {
    let table = |name: &str| {
        let Ok(st) = parse_statement(&format!(
            "CREATE TABLE {name} (K INT64 NOT NULL, V INT64) PRIMARY KEY (K)"
        )) else {
            panic!("parse")
        };
        let spanbolt_ddl::Statement::CreateTable { table, .. } = st else {
            panic!("not a table")
        };
        table
    };
    let mut s = Schema::default();
    for i in 0..4999 {
        let t = table(&format!("T{i}"));
        s.tables.insert(t.name.to_ascii_lowercase(), t);
    }
    let create = |s: &mut Schema, sql: &str| s.apply(&parse_statement(sql).unwrap());
    create(&mut s, "CREATE TABLE Last (K INT64) PRIMARY KEY (K)").unwrap();
    let err = create(&mut s, "CREATE TABLE OneTooMany (K INT64) PRIMARY KEY (K)").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Cannot add Table OneTooMany : too many tables (limit 5000 per database)."
    );

    let index = |name: &str, table: &str| {
        let Ok(spanbolt_ddl::Statement::CreateIndex { index, .. }) =
            parse_statement(&format!("CREATE INDEX {name} ON {table}(V)"))
        else {
            panic!("not an index")
        };
        index
    };
    for i in 0..9999 {
        let ix = index(&format!("I{i}"), &format!("T{}", i / 100));
        s.indexes.insert(ix.name.to_ascii_lowercase(), ix);
    }
    create(&mut s, "CREATE INDEX LastIndex ON T200(V)").unwrap();
    let err = create(&mut s, "CREATE INDEX OneTooMany ON T201(V)").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Cannot add Index OneTooMany : too many indexes (limit 10000 per database)."
    );
}

/// Package `m`: messages `Info { optional Genre genre = 1; }` and `Other`, enum `Genre`.
fn music_descriptors() -> std::sync::Arc<Vec<u8>> {
    use prost::Message;
    use prost_types::{
        DescriptorProto, EnumDescriptorProto, EnumValueDescriptorProto, FieldDescriptorProto,
        FileDescriptorProto, FileDescriptorSet,
    };
    let message = |name: &str, field: Vec<FieldDescriptorProto>| DescriptorProto {
        name: Some(name.into()),
        field,
        ..Default::default()
    };
    let set = FileDescriptorSet {
        file: vec![FileDescriptorProto {
            name: Some("m.proto".into()),
            package: Some("m".into()),
            message_type: vec![
                message(
                    "Info",
                    vec![FieldDescriptorProto {
                        name: Some("genre".into()),
                        number: Some(1),
                        type_name: Some(".m.Genre".into()),
                        ..Default::default()
                    }],
                ),
                message("Other", vec![]),
            ],
            enum_type: vec![EnumDescriptorProto {
                name: Some("Genre".into()),
                value: vec![EnumValueDescriptorProto {
                    name: Some("A".into()),
                    number: Some(0),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    std::sync::Arc::new(set.encode_to_vec())
}

/// Applies `stmts` (bundle statements with `descriptors`), production rules unless `compat`.
fn with_bundle(stmts: &[&str], descriptors: &[u8], compat: bool) -> Result<Schema, DdlError> {
    let mut s = Schema::default();
    for sql in stmts {
        let mut stmt = parse_statement(sql)?;
        if let spanbolt_ddl::Statement::CreateProtoBundle { descriptors: d, .. }
        | spanbolt_ddl::Statement::AlterProtoBundle { descriptors: d, .. } = &mut stmt
        {
            *d = Some(std::sync::Arc::new(descriptors.to_vec()));
        }
        if compat {
            s.apply_emulator_compat(&stmt)?;
        } else {
            s.apply(&stmt)?;
        }
    }
    Ok(s)
}

#[test]
fn proto_bundle_rules_follow_the_mode() {
    let d = music_descriptors();
    let setup = [
        "CREATE PROTO BUNDLE (m.Info, m.Genre)",
        "CREATE TABLE T (K INT64 NOT NULL, I m.Info, G m.Genre, GS ARRAY<m.Genre>) PRIMARY KEY (K)",
    ];
    let s = with_bundle(&setup, &d, false).unwrap_or_else(|e| panic!("{e}"));
    let t = s.table("T").expect("table");
    assert_eq!(
        t.columns.iter().map(|c| c.ty.clone()).collect::<Vec<_>>(),
        [
            spanbolt_ddl::ColumnType::Int64,
            spanbolt_ddl::ColumnType::Proto("m.Info".into()),
            spanbolt_ddl::ColumnType::Enum("m.Genre".into()),
            spanbolt_ddl::ColumnType::Array(Box::new(spanbolt_ddl::ColumnType::Enum(
                "m.Genre".into()
            ))),
        ]
    );
    let err = |extra: &str, compat: bool| {
        let mut stmts = setup.to_vec();
        stmts.push(extra);
        let e = with_bundle(&stmts, &d, compat).expect_err(extra);
        let (k, m) = kind(&e);
        (k, m.to_string())
    };
    let cases: &[(&str, bool, (&str, &str))] = &[
        (
            "CREATE TABLE U (K INT64 NOT NULL, O m.Other) PRIMARY KEY (K)",
            false,
            ("Nf", "Type not found: m.Other."),
        ),
        (
            "ALTER PROTO BUNDLE DELETE (m.Genre)",
            false,
            (
                "Nf",
                "Type m.Info references type m.Genre which is to be removed from the proto bundle",
            ),
        ),
        (
            "ALTER PROTO BUNDLE DELETE (m.Info)",
            false,
            (
                "Inv",
                "Cannot set new proto bundle: removed type m.Info is still used by 1 columns",
            ),
        ),
        (
            "DROP PROTO BUNDLE",
            false,
            (
                "Inv",
                "Cannot set new proto bundle: removed type m.Genre is still used by 2 columns",
            ),
        ),
        (
            "DROP PROTO BUNDLE",
            true,
            (
                "Pre",
                "Removed type `m.Info` is still being used by column T.I",
            ),
        ),
        (
            "ALTER PROTO BUNDLE INSERT (m.Genre)",
            false,
            ("Dup", "Type already exists: m.Genre"),
        ),
        (
            "ALTER PROTO BUNDLE UPDATE (m.Other)",
            false,
            ("Nf", "Message type not found in proto bundle: m.Other"),
        ),
        (
            "CREATE PROTO BUNDLE (m.Other)",
            false,
            ("Inv", "Proto bundle already exists; cannot create it"),
        ),
    ];
    for (sql, compat, want) in cases {
        assert_eq!(err(sql, *compat), (want.0, want.1.to_string()), "{sql}");
    }
    // Inserting a type no column uses, then deleting it again, is fine.
    let mut stmts = setup.to_vec();
    stmts.extend([
        "ALTER PROTO BUNDLE INSERT (m.Other)",
        "ALTER PROTO BUNDLE DELETE (m.Other)",
    ]);
    let s = with_bundle(&stmts, &d, false).unwrap_or_else(|e| panic!("{e}"));
    let b = s.proto_bundle.as_ref().expect("bundle");
    assert_eq!(b.types.keys().collect::<Vec<_>>(), ["m.Genre", "m.Info"]);
    assert_eq!(&b.enum_numbers["m.Genre"][..], [0]);
    // Garbage descriptors.
    let e = with_bundle(&setup[..1], &[0xff, 0xff], false).expect_err("garbage");
    assert_eq!(
        kind(&e),
        (
            "Inv",
            "Failed to deserialize proto descriptors into google.protobuf.FileDescriptorSet. Error:"
        )
    );
}
