//! Schema helpers for tests.

use spanbolt_ddl::{
    Column, ColumnType, Index, Interleave, KeyPart, OnDelete, Schema, SortOrder, Table,
};

pub fn col(name: &str, ty: ColumnType) -> Column {
    Column {
        name: name.into(),
        ty,
        not_null: false,
        default: None,
        generated: None,
        allow_commit_timestamp: false,
        hidden: false,
        vector_length: None,
    }
}

pub fn not_null(mut c: Column) -> Column {
    c.not_null = true;
    c
}

pub fn commit_ts(mut c: Column) -> Column {
    c.allow_commit_timestamp = true;
    c
}

pub fn asc(c: &str) -> KeyPart {
    KeyPart {
        column: c.into(),
        order: SortOrder::Asc,
    }
}

pub fn desc(c: &str) -> KeyPart {
    KeyPart {
        column: c.into(),
        order: SortOrder::Desc,
    }
}

pub fn table(name: &str, columns: Vec<Column>, pk: Vec<KeyPart>) -> Table {
    Table {
        name: name.into(),
        columns,
        primary_key: pk,
        interleave: None,
        foreign_keys: Vec::new(),
        checks: Vec::new(),
        row_deletion_policy: None,
        synonym: None,
    }
}

pub fn interleave(mut t: Table, parent: &str, on_delete: OnDelete) -> Table {
    t.interleave = Some(Interleave {
        parent: parent.into(),
        on_delete,
        in_parent: true,
    });
    t
}

pub fn index(name: &str, table: &str, key: Vec<KeyPart>, storing: &[&str]) -> Index {
    Index {
        name: name.into(),
        table: table.into(),
        key,
        storing: storing.iter().map(|s| s.to_string()).collect(),
        unique: false,
        null_filtered: false,
        interleave_in: None,
        filter: None,
    }
}

/// Adds tables and indexes to a schema the way `Schema::apply` would key them.
pub fn schema(tables: Vec<Table>, indexes: Vec<Index>) -> Schema {
    let mut s = Schema::default();
    for t in tables {
        s.tables.insert(t.name.to_ascii_lowercase(), t);
    }
    for i in indexes {
        s.indexes.insert(i.name.to_ascii_lowercase(), i);
    }
    s.version = 1;
    s
}

/// Builds a schema from DDL statements; panics on invalid DDL (tests only).
pub fn ddl(statements: &[&str]) -> Schema {
    let owned: Vec<String> = statements.iter().map(|s| s.to_string()).collect();
    match crate::snapshot::schema_from_ddl(&owned) {
        Ok(s) => s,
        Err(e) => panic!("invalid test DDL: {e}"),
    }
}
