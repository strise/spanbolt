//! INFORMATION_SCHEMA rows derived from the schema, shaped like the emulator's (1.5.59). Column
//! order follows `info_schema_columns.tsv`.

use std::collections::HashMap;
use std::sync::Arc;

use spanbolt_ddl::{ColumnType, OnDelete, Schema, SortOrder, Table};
use spanbolt_storage::{Result, Value};

use crate::catalog::{Relation, SqlCatalog};
use crate::exec::Row;

fn s(v: &str) -> Value {
    Value::String(Arc::from(v))
}

fn opt(v: Option<&str>) -> Value {
    v.map_or(Value::Null, s)
}

fn b(v: bool) -> Value {
    Value::Bool(v)
}

fn i(v: usize) -> Value {
    Value::Int64(v as i64)
}

/// `schema.table` → (`schema`, `table`).
fn split(name: &str) -> (&str, &str) {
    name.split_once('.').unwrap_or(("", name))
}

/// DDL spelling of a column type: `STRING(MAX)`, `ARRAY<BYTES(16)>`.
pub fn ddl_type(t: &ColumnType) -> String {
    let len = |n: &Option<u64>| n.map_or("MAX".to_string(), |n| n.to_string());
    match t {
        ColumnType::String(n) => format!("STRING({})", len(n)),
        ColumnType::Bytes(n) => format!("BYTES({})", len(n)),
        ColumnType::Array(e) => format!("ARRAY<{}>", ddl_type(e)),
        other => crate::types::spanner_name(other),
    }
}

fn delete_rule(o: &OnDelete) -> &'static str {
    match o {
        OnDelete::Cascade => "CASCADE",
        OnDelete::NoAction => "NO ACTION",
    }
}

type InfoCols = Vec<(String, ColumnType, bool)>;

fn info_tables(cat: &SqlCatalog) -> Vec<(String, InfoCols)> {
    let mut v: Vec<_> = cat
        .relations()
        .filter_map(|r| match r {
            Relation::Info(it) => Some((it.name.clone(), it.columns.clone())),
            _ => None,
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// Columns a CHECK expression mentions (identifiers that are column names).
fn check_columns<'t>(t: &'t Table, expr: &str) -> Vec<&'t str> {
    let words: Vec<&str> = expr
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect();
    t.columns
        .iter()
        .filter(|c| words.iter().any(|w| w.eq_ignore_ascii_case(&c.name)))
        .map(|c| c.name.as_str())
        .collect()
}

/// An INFORMATION_SCHEMA table's rows, with row positions by ASCII-lowercased `TABLE_NAME` when
/// the table has that column. Spanner names are ASCII, so every case-insensitive comparison
/// (`=`, `LOWER(..) =`, `UPPER(..) =`) that matches a row finds it under the lowercased key.
pub struct InfoRows {
    pub rows: Vec<Row>,
    by_table_name: Option<HashMap<String, Vec<usize>>>,
}

impl InfoRows {
    pub fn new(rows: Vec<Row>, table_name_col: Option<usize>) -> Self {
        let by_table_name = table_name_col.map(|c| {
            let mut m: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, r) in rows.iter().enumerate() {
                if let Some(Value::String(n)) = r.get(c) {
                    m.entry(n.to_ascii_lowercase()).or_default().push(i);
                }
            }
            m
        });
        InfoRows {
            rows,
            by_table_name,
        }
    }

    /// Positions of the rows whose `TABLE_NAME` equals `name` ignoring ASCII case; `None` when
    /// the table has no `TABLE_NAME` column.
    pub fn by_table_name(&self, name: &str) -> Option<&[usize]> {
        let m = self.by_table_name.as_ref()?;
        Some(
            m.get(&name.to_ascii_lowercase())
                .map_or(&[][..], |v| v.as_slice()),
        )
    }
}

fn null_row(n: usize) -> Row {
    vec![Value::Null; n]
}

pub fn rows(cat: &SqlCatalog, table: &str) -> Result<Vec<Row>> {
    let schema = &cat.schema;
    let mut out: Vec<Row> = Vec::new();
    match table {
        "TABLES" => {
            for t in schema.tables.values() {
                let (sch, name) = split(&t.name);
                let (parent, on_delete, kind) = match &t.interleave {
                    Some(il) => (
                        Some(split(&il.parent).1.to_string()),
                        Some(delete_rule(&il.on_delete)),
                        Some(if il.in_parent { "IN PARENT" } else { "IN" }),
                    ),
                    None => (None, None, None),
                };
                let rdp = t
                    .row_deletion_policy
                    .as_ref()
                    .map(|p| format!("OLDER_THAN({}, INTERVAL {} DAY)", p.column, p.days));
                out.push(vec![
                    s(""),
                    s(sch),
                    s(name),
                    opt(parent.as_deref()),
                    opt(on_delete),
                    s("BASE TABLE"),
                    s("COMMITTED"),
                    opt(kind),
                    opt(rdp.as_deref()),
                ]);
            }
            for v in schema.views.values() {
                let (sch, name) = split(&v.name);
                let mut r = null_row(9);
                r[0] = s("");
                r[1] = s(sch);
                r[2] = s(name);
                r[5] = s("VIEW");
                out.push(r);
            }
            let mut sys: Vec<(String, String)> = info_tables(cat)
                .into_iter()
                .map(|(n, _)| ("INFORMATION_SCHEMA".to_string(), n))
                .collect();
            sys.push(("SPANNER_SYS".into(), "SUPPORTED_OPTIMIZER_VERSIONS".into()));
            for (sch, name) in sys {
                let mut r = null_row(9);
                r[0] = s("");
                r[1] = s(&sch);
                r[2] = s(&name);
                r[5] = s("VIEW");
                r[7] = s("");
                out.push(r);
            }
        }
        "COLUMNS" => {
            for t in schema.tables.values() {
                let (sch, name) = split(&t.name);
                for (pos, c) in t.columns.iter().enumerate() {
                    let (generated, expr, stored) = match &c.generated {
                        Some(spanbolt_ddl::Generated::Expr { expr, stored }) => (
                            "ALWAYS",
                            Some(expr.as_str()),
                            Some(if *stored { "YES" } else { "NO" }),
                        ),
                        Some(spanbolt_ddl::Generated::Identity { .. }) | None => {
                            ("NEVER", None, None)
                        }
                    };
                    let identity =
                        matches!(c.generated, Some(spanbolt_ddl::Generated::Identity { .. }));
                    let mut r = column_row(
                        sch,
                        name,
                        &c.name,
                        pos,
                        &ddl_type(&c.ty),
                        !c.not_null,
                        Some("COMMITTED"),
                    );
                    r[5] = opt(c.default.as_deref());
                    r[9] = s(generated);
                    r[10] = b(c.hidden);
                    r[11] = opt(expr);
                    r[13] = opt(stored);
                    if identity {
                        r[15] = s("YES");
                    }
                    out.push(r);
                }
            }
            for rel in cat.relations() {
                let Relation::View(v) = rel else { continue };
                let (sch, name) = split(&v.name);
                for (pos, oc) in v.query.output_column_list.iter().enumerate() {
                    let ty = oc
                        .column
                        .as_ref()
                        .and_then(|c| c.r#type.as_ref())
                        .and_then(|t| crate::types::from_proto(t).ok())
                        .map(|t| ddl_type(&t))
                        .unwrap_or_default();
                    let col = oc.name.as_deref().unwrap_or("");
                    out.push(column_row(
                        sch,
                        name,
                        col,
                        pos,
                        &ty,
                        true,
                        Some("COMMITTED"),
                    ));
                }
            }
            for (name, cols) in info_tables(cat) {
                for (pos, (c, ty, nullable)) in cols.iter().enumerate() {
                    let ty = ddl_type(ty);
                    out.push(column_row(
                        "INFORMATION_SCHEMA",
                        &name,
                        c,
                        pos,
                        &ty,
                        *nullable,
                        None,
                    ));
                }
            }
        }
        "INDEXES" => {
            let pk = |sch: &str, name: &str| {
                vec![
                    s(""),
                    s(sch),
                    s(name),
                    s("PRIMARY_KEY"),
                    s("PRIMARY_KEY"),
                    s(""),
                    b(true),
                    b(false),
                    Value::Null,
                    Value::Null,
                    b(false),
                    Value::Null,
                    Value::Null,
                ]
            };
            for t in schema.tables.values() {
                let (sch, name) = split(&t.name);
                out.push(pk(sch, name));
            }
            for ix in schema.indexes.values() {
                let (sch, tname) = split(&ix.table);
                out.push(vec![
                    s(""),
                    s(sch),
                    s(tname),
                    s(split(&ix.name).1),
                    s("INDEX"),
                    s(ix.interleave_in.as_deref().map_or("", |p| split(p).1)),
                    b(ix.unique),
                    b(ix.null_filtered),
                    s("READ_WRITE"),
                    opt(ix.filter.as_deref()),
                    b(false),
                    Value::Null,
                    Value::Null,
                ]);
            }
            for si in schema.search_indexes.values() {
                let (sch, tname) = split(&si.table);
                let arr = |v: Vec<String>| {
                    Value::Array(Arc::from(v.iter().map(|x| s(x)).collect::<Vec<_>>()))
                };
                out.push(vec![
                    s(""),
                    s(sch),
                    s(tname),
                    s(split(&si.name).1),
                    s("SEARCH"),
                    s(si.interleave_in.as_deref().map_or("", |p| split(p).1)),
                    b(false),
                    b(false),
                    s("READ_WRITE"),
                    opt(si.filter.as_deref()),
                    b(false),
                    arr(si.partition_by.clone()),
                    arr(si.order_by.iter().map(|k| k.column.clone()).collect()),
                ]);
            }
            for (name, _) in info_tables(cat) {
                out.push(pk("INFORMATION_SCHEMA", &name));
            }
        }
        "INDEX_COLUMNS" => {
            for t in schema.tables.values() {
                let (sch, name) = split(&t.name);
                for (pos, k) in t.primary_key.iter().enumerate() {
                    let Some(c) = t.column(&k.column) else {
                        continue;
                    };
                    out.push(index_col_row(
                        [sch, name, "PRIMARY_KEY", "PRIMARY_KEY", &c.name],
                        Some((pos + 1, &k.order)),
                        !c.not_null,
                        &ddl_type(&c.ty),
                    ));
                }
            }
            for ix in schema.indexes.values() {
                let Some(t) = schema.table(&ix.table) else {
                    continue;
                };
                let (sch, tname) = split(&t.name);
                let iname = split(&ix.name).1;
                // A filtered index's `WHERE c IS NOT NULL` columns are NOT NULL in the index.
                let filtered: Vec<String> = ix
                    .filter
                    .as_deref()
                    .and_then(|f| spanbolt_ddl::null_filter_columns(f).ok())
                    .unwrap_or_default();
                let in_filter = |n: &str| filtered.iter().any(|f| f.eq_ignore_ascii_case(n));
                for (pos, k) in ix.key.iter().enumerate() {
                    let Some(c) = t.column(&k.column) else {
                        continue;
                    };
                    out.push(index_col_row(
                        [sch, tname, iname, "INDEX", &c.name],
                        Some((pos + 1, &k.order)),
                        !c.not_null && !ix.null_filtered && !in_filter(&c.name),
                        &ddl_type(&c.ty),
                    ));
                }
                for st in &ix.storing {
                    let Some(c) = t.column(st) else { continue };
                    out.push(index_col_row(
                        [sch, tname, iname, "INDEX", &c.name],
                        None,
                        !c.not_null && !in_filter(&c.name),
                        &ddl_type(&c.ty),
                    ));
                }
            }
        }
        "SCHEMATA" => {
            let mut names: Vec<&str> = vec!["", "INFORMATION_SCHEMA", "SPANNER_SYS"];
            for n in schema.schemas.values() {
                names.push(n);
            }
            for n in names {
                out.push(vec![s(""), s(n), Value::Int64(0), s("")]);
            }
        }
        "TABLE_CONSTRAINTS"
        | "CONSTRAINT_TABLE_USAGE"
        | "KEY_COLUMN_USAGE"
        | "CONSTRAINT_COLUMN_USAGE"
        | "CHECK_CONSTRAINTS"
        | "REFERENTIAL_CONSTRAINTS" => constraints(schema, table, &mut out),
        "COLUMN_OPTIONS" => {
            for t in schema.tables.values() {
                let (sch, name) = split(&t.name);
                for c in t.columns.iter().filter(|c| c.allow_commit_timestamp) {
                    out.push(vec![
                        s(""),
                        s(sch),
                        s(name),
                        s(&c.name),
                        s("allow_commit_timestamp"),
                        s("BOOL"),
                        s("TRUE"),
                    ]);
                }
            }
        }
        "DATABASE_OPTIONS" => {
            out.push(vec![
                s(""),
                s(""),
                s("database_dialect"),
                s("STRING"),
                s("GOOGLE_STANDARD_SQL"),
            ]);
            for (k, v) in &schema.options {
                let (ty, val) = option_value(v);
                out.push(vec![s(""), s(""), s(k), s(ty), s(&val)]);
            }
        }
        "VIEWS" => {
            for v in schema.views.values() {
                let (sch, name) = split(&v.name);
                let security = if v.sql_security_invoker {
                    "INVOKER"
                } else {
                    "DEFINER"
                };
                out.push(vec![s(""), s(sch), s(name), s(&v.sql), s(security)]);
            }
        }
        "SEQUENCES" => {
            for q in schema.sequences.values() {
                let (sch, name) = split(&q.name);
                out.push(vec![s(""), s(sch), s(name), s("INT64")]);
            }
        }
        "SEQUENCE_OPTIONS" => {
            for q in schema.sequences.values() {
                let (sch, name) = split(&q.name);
                for (k, v) in &q.options {
                    let (ty, val) = option_value(v);
                    out.push(vec![s(""), s(sch), s(name), s(k), s(ty), s(&val)]);
                }
            }
        }
        "CHANGE_STREAMS" => {
            for o in &schema.opaque {
                if o.kind.eq_ignore_ascii_case("CHANGE STREAM") {
                    let all = o.ddl.to_ascii_uppercase().contains("FOR ALL");
                    let (sch, name) = split(&o.name);
                    out.push(vec![s(""), s(sch), s(name), b(all)]);
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// DDL option literal → (type, value): `'7d'` → STRING 7d, `true` → BOOL TRUE, `5` → INT64.
fn option_value(v: &str) -> (&'static str, String) {
    let t = v.trim();
    let unquoted = t
        .strip_prefix('\'')
        .and_then(|x| x.strip_suffix('\''))
        .or_else(|| t.strip_prefix('"').and_then(|x| x.strip_suffix('"')));
    if let Some(inner) = unquoted {
        ("STRING", inner.to_string())
    } else if t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("false") {
        ("BOOL", t.to_ascii_uppercase())
    } else if t.parse::<i64>().is_ok() {
        ("INT64", t.to_string())
    } else {
        ("STRING", t.to_string())
    }
}

fn column_row(
    sch: &str,
    table: &str,
    col: &str,
    pos: usize,
    ty: &str,
    nullable: bool,
    state: Option<&str>,
) -> Row {
    let mut r = null_row(21);
    r[0] = s("");
    r[1] = s(sch);
    r[2] = s(table);
    r[3] = s(col);
    r[4] = i(pos + 1);
    r[7] = s(if nullable { "YES" } else { "NO" });
    r[8] = s(ty);
    r[9] = s("NEVER");
    r[10] = b(false);
    r[14] = opt(state);
    r[15] = s("NO");
    r
}

/// `names` = schema, table, index, index type, column.
fn index_col_row(
    names: [&str; 5],
    key: Option<(usize, &SortOrder)>,
    nullable: bool,
    ty: &str,
) -> Row {
    let [sch, table, index, kind, col] = names;
    vec![
        s(""),
        s(sch),
        s(table),
        s(index),
        s(kind),
        s(col),
        key.map_or(Value::Null, |(p, _)| i(p)),
        key.map_or(Value::Null, |(_, o)| {
            s(if *o == SortOrder::Desc { "DESC" } else { "ASC" })
        }),
        s(if nullable { "YES" } else { "NO" }),
        s(ty),
    ]
}

/// A constraint: name, type, the table it uses (FK: referenced table) and its columns there.
struct Constraint {
    name: String,
    kind: &'static str,
    used: String,
    cols: Vec<String>,
}

fn constraints(schema: &Schema, which: &str, out: &mut Vec<Row>) {
    for t in schema.tables.values() {
        let (sch, name) = split(&t.name);
        let mut list: Vec<Constraint> = vec![Constraint {
            name: format!("PK_{name}"),
            kind: "PRIMARY KEY",
            used: t.name.clone(),
            cols: t.primary_key.iter().map(|k| k.column.clone()).collect(),
        }];
        for c in t.columns.iter().filter(|c| c.not_null) {
            let n = format!("CK_IS_NOT_NULL_{name}_{}", c.name);
            if which == "CHECK_CONSTRAINTS" {
                let clause = format!("{} IS NOT NULL", c.name);
                out.push(vec![s(""), s(sch), s(&n), s(&clause), s("COMMITTED")]);
            }
            list.push(Constraint {
                name: n,
                kind: "CHECK",
                used: t.name.clone(),
                cols: vec![c.name.clone()],
            });
        }
        for ck in &t.checks {
            if which == "CHECK_CONSTRAINTS" {
                out.push(vec![
                    s(""),
                    s(sch),
                    s(&ck.name),
                    s(&ck.expr),
                    s("COMMITTED"),
                ]);
            }
            list.push(Constraint {
                name: ck.name.clone(),
                kind: "CHECK",
                used: t.name.clone(),
                cols: check_columns(t, &ck.expr)
                    .into_iter()
                    .map(String::from)
                    .collect(),
            });
        }
        for fk in &t.foreign_keys {
            let ref_pk: Vec<String> = schema
                .table(&fk.ref_table)
                .map(|r| {
                    r.primary_key
                        .iter()
                        .map(|k| k.column.to_ascii_lowercase())
                        .collect()
                })
                .unwrap_or_default();
            let refs: Vec<String> = fk
                .ref_columns
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .collect();
            let (rsch, rname) = split(&fk.ref_table);
            let unique_name = if refs == ref_pk {
                format!("PK_{rname}")
            } else {
                format!("IDX_{rname}_{}_U", fk.ref_columns.join("_"))
            };
            match which {
                "REFERENTIAL_CONSTRAINTS" => out.push(vec![
                    s(""),
                    s(sch),
                    s(&fk.name),
                    s(""),
                    s(rsch),
                    s(&unique_name),
                    s("SIMPLE"),
                    s("NO ACTION"),
                    s(delete_rule(&fk.on_delete)),
                    s("COMMITTED"),
                ]),
                "KEY_COLUMN_USAGE" => {
                    for (p, c) in fk.columns.iter().enumerate() {
                        out.push(vec![
                            s(""),
                            s(sch),
                            s(&fk.name),
                            s(""),
                            s(sch),
                            s(name),
                            s(c),
                            i(p + 1),
                            i(p + 1),
                        ]);
                    }
                }
                "TABLE_CONSTRAINTS" => out.push(vec![
                    s(""),
                    s(sch),
                    s(&fk.name),
                    s(""),
                    s(sch),
                    s(name),
                    s("FOREIGN KEY"),
                    s("NO"),
                    s("NO"),
                    s(if fk.enforced { "YES" } else { "NO" }),
                ]),
                _ => list.push(Constraint {
                    name: fk.name.clone(),
                    kind: "FOREIGN KEY",
                    used: fk.ref_table.clone(),
                    cols: fk.ref_columns.clone(),
                }),
            }
        }
        for c in list {
            let (usch, uname) = split(&c.used);
            match which {
                "TABLE_CONSTRAINTS" => out.push(vec![
                    s(""),
                    s(sch),
                    s(&c.name),
                    s(""),
                    s(sch),
                    s(name),
                    s(c.kind),
                    s("NO"),
                    s("NO"),
                    s("YES"),
                ]),
                "CONSTRAINT_TABLE_USAGE" => {
                    out.push(vec![s(""), s(usch), s(uname), s(""), s(sch), s(&c.name)])
                }
                "CONSTRAINT_COLUMN_USAGE" => {
                    for col in &c.cols {
                        out.push(vec![
                            s(""),
                            s(usch),
                            s(uname),
                            s(col),
                            s(""),
                            s(sch),
                            s(&c.name),
                        ]);
                    }
                }
                "KEY_COLUMN_USAGE" if c.kind == "PRIMARY KEY" => {
                    for (p, col) in c.cols.iter().enumerate() {
                        out.push(vec![
                            s(""),
                            s(sch),
                            s(&c.name),
                            s(""),
                            s(sch),
                            s(name),
                            s(col),
                            i(p + 1),
                            Value::Null,
                        ]);
                    }
                }
                _ => {}
            }
        }
    }
}
