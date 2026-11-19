//! Printing of constructs the fixture lacks; expected text is emulator 1.5.59 GetDatabaseDdl output.

use spanbolt_ddl::{Schema, parse_statement, print_statement};

const SCRIPT: &[&str] = &[
    r#"ALTER DATABASE db1 SET OPTIONS (version_retention_period = '2d')"#,
    r#"CREATE SCHEMA zeta"#,
    r#"CREATE SCHEMA alpha"#,
    r#"CREATE SEQUENCE seq OPTIONS (start_with_counter = 5, sequence_kind='bit_reversed_positive')"#,
    r#"CREATE TABLE B (id INT64 NOT NULL, x STRING(MAX)) PRIMARY KEY (id)"#,
    r#"create table A (id int64 not null, b int64, z string(10) default ('a'), ts timestamp options (allow_commit_timestamp=true), CHECK (b > 0), constraint ck2 check(b <    100), synonym(AX)) primary key (id desc)"#,
    r#"CREATE INDEX zz ON A(b)"#,
    r#"CREATE INDEX Aa ON A(b DESC) STORING (z)"#,
    r#"CREATE UNIQUE NULL_FILTERED INDEX a_ ON A(id, z)"#,
    r#"ALTER TABLE A ADD CONSTRAINT fk1 FOREIGN KEY (b) REFERENCES B (id) ON DELETE NO ACTION"#,
    r#"ALTER TABLE B ADD COLUMN aid INT64"#,
    r#"ALTER TABLE B ADD FOREIGN KEY (aid) REFERENCES B (id) ON DELETE CASCADE"#,
    r#"CREATE TABLE C (id INT64 NOT NULL, c2 INT64 NOT NULL, ts TIMESTAMP) PRIMARY KEY (id DESC, c2), INTERLEAVE IN PARENT A, ROW DELETION POLICY (OLDER_THAN(ts, INTERVAL 7 DAY))"#,
    r#"CREATE TABLE D (id INT64 NOT NULL, c2 INT64 NOT NULL) PRIMARY KEY (id DESC, c2), INTERLEAVE IN A"#,
    r#"CREATE TABLE H (id INT64 NOT NULL, n STRING(MAX), k INT64 NOT NULL, t TOKENLIST AS (TOKENIZE_FULLTEXT(n)) HIDDEN, g STRING(MAX) AS (UPPER(n)) STORED, v INT64 AS (k + 1), af ARRAY<FLOAT64>(vector_length=>3)) PRIMARY KEY (id)"#,
    r#"CREATE SEARCH INDEX si ON H(t) STORING (n) PARTITION BY k ORDER BY k DESC OPTIONS (sort_order_sharding = true)"#,
    r#"CREATE INDEX hf ON H(n, g) WHERE n is not null and g IS not NULL"#,
    r#"CREATE TABLE alpha.T (`limit` INT64 NOT NULL, `Order` STRING(MAX)) PRIMARY KEY (`limit`)"#,
    r#"CREATE INDEX alpha.ti ON alpha.T(`Order`)"#,
    r#"CREATE CHANGE STREAM cs1 FOR A(z), B OPTIONS (retention_period='36h', value_capture_type = 'NEW_ROW')"#,
    r#"CREATE CHANGE STREAM cs0 FOR ALL"#,
    r#"CREATE VIEW vz SQL SECURITY DEFINER AS SELECT 1 AS a"#,
    r#"CREATE VIEW va SQL SECURITY INVOKER AS   SELECT A.id FROM A"#,
    r#"CREATE ROLE r1"#,
    r#"GRANT SELECT ON TABLE A TO ROLE r1"#,
];

const EXPECTED: &[&str] = &[
    r#"ALTER DATABASE db1 SET OPTIONS (version_retention_period = '2d')"#,
    r#"CREATE SCHEMA zeta"#,
    r#"CREATE SCHEMA alpha"#,
    r#"CREATE SEQUENCE seq OPTIONS (
  sequence_kind = 'bit_reversed_positive',
  start_with_counter = 5)"#,
    r#"CREATE TABLE B (
  id INT64 NOT NULL,
  x STRING(MAX),
  aid INT64,
  FOREIGN KEY(aid) REFERENCES B(id) ON DELETE CASCADE,
) PRIMARY KEY(id)"#,
    r#"CREATE TABLE A (
  id INT64 NOT NULL,
  b INT64,
  z STRING(10) DEFAULT ('a'),
  ts TIMESTAMP OPTIONS (
    allow_commit_timestamp = true
  ),
  CONSTRAINT fk1 FOREIGN KEY(b) REFERENCES B(id) ON DELETE NO ACTION,
  CHECK(b > 0),
  CONSTRAINT ck2 CHECK(b <    100),
  SYNONYM(AX),
) PRIMARY KEY(id DESC)"#,
    r#"CREATE INDEX Aa ON A(b DESC) STORING (z)"#,
    r#"CREATE UNIQUE NULL_FILTERED INDEX a_ ON A(id, z)"#,
    r#"CREATE INDEX zz ON A(b)"#,
    r#"CREATE TABLE C (
  id INT64 NOT NULL,
  c2 INT64 NOT NULL,
  ts TIMESTAMP,
) PRIMARY KEY(id DESC, c2),
  INTERLEAVE IN PARENT A ON DELETE NO ACTION, ROW DELETION POLICY (OLDER_THAN(ts, INTERVAL 7 DAY))"#,
    r#"CREATE TABLE D (
  id INT64 NOT NULL,
  c2 INT64 NOT NULL,
) PRIMARY KEY(id DESC, c2),
  INTERLEAVE IN A"#,
    r#"CREATE TABLE H (
  id INT64 NOT NULL,
  n STRING(MAX),
  k INT64 NOT NULL,
  t TOKENLIST AS (TOKENIZE_FULLTEXT(n)) HIDDEN,
  g STRING(MAX) AS (UPPER(n)) STORED,
  v INT64 AS (k + 1),
  af ARRAY<FLOAT64>(vector_length=>3),
) PRIMARY KEY(id)"#,
    r#"CREATE INDEX hf ON H(n, g) WHERE n IS NOT NULL AND g IS NOT NULL"#,
    r#"CREATE SEARCH INDEX si ON H(t) STORING (n) PARTITION BY k ORDER BY k DESC OPTIONS (sort_order_sharding = true)"#,
    r#"CREATE TABLE alpha.T (
  `limit` INT64 NOT NULL,
  `Order` STRING(MAX),
) PRIMARY KEY(`limit`)"#,
    r#"CREATE INDEX alpha.ti ON alpha.T(`Order`)"#,
    r#"CREATE CHANGE STREAM cs1 FOR A(z), B OPTIONS ( retention_period = '36h', value_capture_type = 'NEW_ROW' )"#,
    r#"CREATE CHANGE STREAM cs0 FOR ALL"#,
    r#"CREATE VIEW vz SQL SECURITY DEFINER AS SELECT 1 AS a"#,
    r#"CREATE VIEW va SQL SECURITY INVOKER AS SELECT A.id FROM A"#,
];

#[test]
fn prints_like_the_emulator() {
    let mut schema = Schema::default();
    for s in SCRIPT {
        schema
            .apply(&parse_statement(s).unwrap())
            .unwrap_or_else(|e| panic!("{s}: {e}"));
    }
    let got = schema.to_ddl();
    for (g, e) in got.iter().zip(EXPECTED) {
        assert_eq!(g, e);
    }
    assert_eq!(got.len(), EXPECTED.len());
    for s in &got {
        let stmt = parse_statement(s).unwrap();
        assert_eq!(&print_statement(&stmt), s);
    }
}

#[test]
fn table_without_primary_key_gets_hidden_rowid() {
    let expected = "CREATE TABLE X (\n  K INT64,\n  rowid INT64 NOT NULL GENERATED BY DEFAULT AS IDENTITY (BIT_REVERSED_POSITIVE) HIDDEN,\n) PRIMARY KEY(rowid)";
    let mut schema = Schema::default();
    let stmt = parse_statement("CREATE TABLE X (K INT64)").unwrap_or_else(|e| panic!("{e}"));
    schema.apply(&stmt).unwrap_or_else(|e| panic!("{e}"));
    let printed = schema.to_ddl();
    assert_eq!(printed, vec![expected.to_string()]);
    // The printed form parses back to the same table.
    let again = parse_statement(expected).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(print_statement(&again), expected);
}
