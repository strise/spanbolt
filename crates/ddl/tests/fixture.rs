//! The generated schema fixture (`crates/ddl/examples/gen_schema.rs`): every statement parses,
//! applies, prints back identically and round-trips.

use spanbolt_ddl::{Schema, parse_statement};

const FIXTURE: &str = include_str!("../../../fixtures/schema.json");

fn list(key: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn fixture() -> Vec<String> {
    list("ddl")
}

#[test]
fn fixture_parses_applies_and_prints_identically() {
    let stmts = fixture();
    assert_eq!(stmts.len(), 524);
    let mut schema = Schema::default();
    for (i, s) in stmts.iter().enumerate() {
        let stmt = parse_statement(s).unwrap_or_else(|e| panic!("parse #{i}: {e}\n{s}"));
        schema
            .apply(&stmt)
            .unwrap_or_else(|e| panic!("apply #{i}: {e}\n{s}"));
    }
    assert_eq!(schema.version, 524);
    // Roles and grants apply; the emulator's GetDatabaseDdl leaves them out.
    for s in list("access_ddl") {
        let stmt = parse_statement(&s).unwrap_or_else(|e| panic!("parse {s}: {e}"));
        schema
            .apply(&stmt)
            .unwrap_or_else(|e| panic!("apply {s}: {e}"));
    }
    let printed = schema.to_ddl_emulator_compat();
    for (i, (a, b)) in printed.iter().zip(&stmts).enumerate() {
        assert_eq!(a, b, "statement #{i} differs");
    }
    assert_eq!(printed.len(), stmts.len());
    // Production's form keeps them, and a snapshot of it loads back with every grant.
    assert!(!schema.grants.is_empty());
    let full = schema.to_ddl();
    let mut reloaded = Schema::default();
    for s in &full {
        let stmt = parse_statement(s).unwrap_or_else(|e| panic!("parse {s}: {e}"));
        reloaded
            .apply_emulator_compat(&stmt)
            .unwrap_or_else(|e| panic!("reload {s}: {e}"));
    }
    assert_eq!(reloaded.grants.len(), schema.grants.len());
    assert_eq!(reloaded.to_ddl(), full);
}

/// The fixture keeps covering every statement kind and DDL feature it was generated for.
#[test]
fn fixture_covers_the_ddl_features() {
    let stmts = fixture();
    let kinds = [
        "CREATE SCHEMA ",
        "CREATE SEQUENCE ",
        "CREATE TABLE ",
        "CREATE INDEX ",
        "CREATE UNIQUE INDEX ",
        "CREATE NULL_FILTERED INDEX ",
        "CREATE UNIQUE NULL_FILTERED INDEX ",
        "CREATE SEARCH INDEX ",
        "CREATE CHANGE STREAM ",
        "CREATE VIEW ",
        "CREATE PROPERTY GRAPH ",
    ];
    for k in kinds {
        assert!(stmts.iter().any(|s| s.starts_with(k)), "no {k}statement");
    }
    let features = [
        "INTERLEAVE IN PARENT",
        "ON DELETE CASCADE",
        "FOREIGN KEY(",
        "CHECK(",
        ") STORED,",
        "DEFAULT (",
        "GET_NEXT_SEQUENCE_VALUE",
        "allow_commit_timestamp = true",
        "ROW DELETION POLICY",
        "TOKENLIST AS (TOKENIZE_FULLTEXT(",
        "TOKENIZE_NGRAMS(",
        "TOKENLIST AS (TOKEN(",
        " HIDDEN,",
        " STORING (",
        " PARTITION BY ",
        ", INTERLEAVE IN ",
        " WHERE ",
        "SQL SECURITY INVOKER",
        "UNION ALL",
        "ARRAY<STRING(MAX)>",
        " JSON",
        " NUMERIC",
        " BYTES(MAX)",
        " DATE",
    ];
    for f in features {
        assert!(stmts.iter().any(|s| s.contains(f)), "no statement has {f}");
    }
    let access = list("access_ddl");
    assert!(access.iter().any(|s| s.starts_with("CREATE ROLE ")));
    assert!(access.iter().any(|s| s.starts_with("GRANT ")));
}

#[test]
fn fixture_round_trips() {
    for (i, s) in fixture().iter().enumerate() {
        let stmt = parse_statement(s).unwrap();
        let mut schema = Schema::default();
        let _ = &mut schema;
        let printed = print_one(&stmt);
        let again =
            parse_statement(&printed).unwrap_or_else(|e| panic!("reparse #{i}: {e}\n{printed}"));
        assert_eq!(again, stmt, "round trip #{i}\n{printed}");
    }
}

fn print_one(stmt: &spanbolt_ddl::Statement) -> String {
    spanbolt_ddl::print_statement(stmt)
}
