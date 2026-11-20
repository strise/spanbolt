//! INFORMATION_SCHEMA rows derived from the schema, shaped like the emulator's (1.5.59). Column
//! order follows `info_schema_columns.tsv`.

use std::collections::HashMap;
use std::sync::Arc;

use spanbolt_ddl::names::{make_fingerprint, make_name};
use spanbolt_ddl::{ColumnType, ForeignKey, OnDelete, Schema, SortOrder, Table};
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
            for ix in managed_indexes(schema) {
                let (sch, tname) = split(&ix.table);
                out.push(vec![
                    s(""),
                    s(sch),
                    s(tname),
                    s(split(&ix.name).1),
                    s("INDEX"),
                    s(if ix.interleaved { tname } else { "" }),
                    b(ix.unique),
                    b(ix.null_filtered),
                    s("READ_WRITE"),
                    Value::Null,
                    b(true),
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
            for ix in managed_indexes(schema) {
                let Some(t) = schema.table(&ix.table) else {
                    continue;
                };
                let (sch, tname) = split(&t.name);
                for (pos, col) in ix.cols.iter().enumerate() {
                    let Some(c) = t.column(col) else { continue };
                    // A key column keeps its table ordering; every other column is ascending.
                    let order = t
                        .primary_key
                        .iter()
                        .find(|k| k.column.eq_ignore_ascii_case(col))
                        .map_or(&SortOrder::Asc, |k| &k.order);
                    out.push(index_col_row(
                        [sch, tname, split(&ix.name).1, "INDEX", &c.name],
                        Some((pos + 1, order)),
                        !c.not_null && !ix.null_filtered,
                        &ddl_type(&c.ty),
                    ));
                }
            }
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
        "PROPERTY_GRAPHS" => {
            let mut graphs: Vec<_> = cat.graphs.values().collect();
            graphs.sort_by(|a, b| a.name.cmp(&b.name));
            for g in graphs {
                let meta = crate::graph::metadata_json(g, &cat.storage);
                out.push(vec![
                    s(""),
                    s(""),
                    s(&g.name),
                    Value::Json(meta.to_string().into()),
                ]);
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

/// An index the emulator creates to back a foreign key (`SPANNER_IS_MANAGED`). Names follow the
/// emulator's `GlobalSchemaNames::GenerateManagedIndexName`.
struct ManagedIndex {
    /// Fully qualified, as the emulator names it: `mon.IDX_alerts_code_U_…` in a named schema.
    name: String,
    /// The indexed table, fully qualified.
    table: String,
    cols: Vec<String>,
    unique: bool,
    null_filtered: bool,
    /// The index is interleaved in its own table (the table's key is a prefix of its columns).
    interleaved: bool,
}

fn managed_index_name(table: &str, cols: &[String], null_filtered: bool, unique: bool) -> String {
    let (sch, tname) = split(table);
    let prefix = if sch.is_empty() {
        "IDX".to_string()
    } else {
        format!("{sch}.IDX")
    };
    let base = format!("{prefix}_{tname}_{}", cols.join("_"));
    // One code: U for unique indexes (null-filtered or not), else N for null-filtered ones.
    let codes = if unique {
        "U"
    } else if null_filtered {
        "N"
    } else {
        ""
    };
    if codes.is_empty() {
        make_name(&base, &make_fingerprint(&base))
    } else {
        let fp = make_fingerprint(&format!("{base}_{codes}"));
        make_name(&base, &format!("{codes}_{fp}"))
    }
}

fn managed_index(t: &Table, cols: Vec<String>, unique: bool) -> ManagedIndex {
    let null_filtered = cols
        .iter()
        .any(|c| t.column(c).is_some_and(|c| !c.not_null));
    let interleaved = t.primary_key.len() <= cols.len()
        && t.primary_key
            .iter()
            .zip(&cols)
            .all(|(k, c)| k.column.eq_ignore_ascii_case(c));
    ManagedIndex {
        name: managed_index_name(&t.name, &cols, null_filtered, unique),
        table: t.name.clone(),
        cols,
        unique,
        null_filtered,
        interleaved,
    }
}

/// The indexes the emulator creates for a foreign key of table `t`: a unique index on the
/// referenced columns unless they are exactly the referenced table's primary key, and (enforced
/// keys only) an index on the referencing columns unless they are a non-null prefix of `t`'s
/// primary key in the same order. Ports `CreateForeignKeyConstraint` and the
/// `EvaluateForeignKeyRefere*PrimaryKey` helpers of the emulator's schema updater.
fn fk_indexes(
    schema: &Schema,
    t: &Table,
    fk: &ForeignKey,
) -> (Option<ManagedIndex>, Option<ManagedIndex>) {
    let Some(rt) = schema.table(&fk.ref_table) else {
        return (None, None);
    };
    let n = fk.ref_columns.len();
    let pos = |name: &str| {
        fk.ref_columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
    };
    // Referenced columns in index order: the matching primary key prefix, then table order.
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut referenced_required = None;
    for k in &rt.primary_key {
        let Some(p) = pos(&k.column) else { break };
        order.push(p);
        if order.len() == n {
            referenced_required = Some(rt.primary_key.len() != n);
            break;
        }
    }
    let referenced_required = referenced_required.unwrap_or_else(|| {
        for c in &rt.columns {
            if let Some(p) = pos(&c.name)
                && !order.contains(&p)
            {
                order.push(p);
                if order.len() == n {
                    break;
                }
            }
        }
        order.len() == n
    });
    if order.len() != n || fk.columns.len() != n {
        return (None, None);
    }
    let referenced = referenced_required.then(|| {
        let cols = order.iter().map(|&p| fk.ref_columns[p].clone()).collect();
        managed_index(rt, cols, true)
    });
    if !fk.enforced {
        return (referenced, None);
    }
    let nullable = fk
        .columns
        .iter()
        .any(|c| t.column(c).is_some_and(|c| !c.not_null));
    let referencing_required = n > t.primary_key.len() || nullable || {
        let at = |name: &str| {
            fk.columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(name))
                .map(|i| order[i])
        };
        let matching = t
            .primary_key
            .iter()
            .enumerate()
            .take_while(|(i, k)| at(&k.column) == Some(*i))
            .count();
        matching != n
    };
    let referencing = referencing_required.then(|| {
        let cols = order.iter().map(|&p| fk.columns[p].clone()).collect();
        managed_index(t, cols, false)
    });
    (referenced, referencing)
}

/// Every foreign key's managed indexes, once each (keys with the same columns share one), in
/// schema order.
fn managed_indexes(schema: &Schema) -> Vec<ManagedIndex> {
    let mut out: Vec<ManagedIndex> = Vec::new();
    for t in schema.tables.values() {
        for fk in &t.foreign_keys {
            let (a, b) = fk_indexes(schema, t, fk);
            for ix in [a, b].into_iter().flatten() {
                if !out.iter().any(|o| o.name == ix.name) {
                    out.push(ix);
                }
            }
        }
    }
    out
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
            let (rsch, rname) = split(&fk.ref_table);
            // A name generated in a named schema carries the schema (`sch.FK_T_R_..._1`); the
            // views show the in-schema part.
            let fk_name = split(&fk.name).1;
            // A key onto the referenced primary key names it; any other key names the unique
            // index backing it, which is also a UNIQUE constraint of the referenced table. The
            // emulator lists that constraint once per foreign key using it, and names it with its
            // schema in TABLE_CONSTRAINTS and CONSTRAINT_TABLE_USAGE only.
            let unique = fk_indexes(schema, t, fk).0;
            let unique_name = unique
                .as_ref()
                .map_or_else(|| format!("PK_{rname}"), |ix| split(&ix.name).1.to_string());
            if let Some(ix) = &unique {
                let short = split(&ix.name).1;
                match which {
                    "TABLE_CONSTRAINTS" => out.push(vec![
                        s(""),
                        s(""),
                        s(&ix.name),
                        s(""),
                        s(rsch),
                        s(rname),
                        s("UNIQUE"),
                        s("NO"),
                        s("NO"),
                        s("YES"),
                    ]),
                    "KEY_COLUMN_USAGE" => {
                        for (p, c) in ix.cols.iter().enumerate() {
                            out.push(vec![
                                s(""),
                                s(rsch),
                                s(short),
                                s(""),
                                s(rsch),
                                s(rname),
                                s(c),
                                i(p + 1),
                                Value::Null,
                            ]);
                        }
                    }
                    "CONSTRAINT_TABLE_USAGE" => {
                        out.push(vec![s(""), s(rsch), s(rname), s(""), s(rsch), s(&ix.name)])
                    }
                    "CONSTRAINT_COLUMN_USAGE" => {
                        for c in &ix.cols {
                            out.push(vec![
                                s(""),
                                s(rsch),
                                s(rname),
                                s(c),
                                s(""),
                                s(rsch),
                                s(short),
                            ]);
                        }
                    }
                    _ => {}
                }
            }
            match which {
                "REFERENTIAL_CONSTRAINTS" => out.push(vec![
                    s(""),
                    s(sch),
                    s(fk_name),
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
                            s(fk_name),
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
                    s(fk_name),
                    s(""),
                    s(sch),
                    s(name),
                    s("FOREIGN KEY"),
                    s("NO"),
                    s("NO"),
                    s(if fk.enforced { "YES" } else { "NO" }),
                ]),
                _ => list.push(Constraint {
                    name: fk_name.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_index_names_match_the_emulator() {
        // Recorded from emulator 1.5.59 (conformance/cases/sql/information_schema_fk.case).
        let cols = |c: &[&str]| c.iter().map(|c| c.to_string()).collect::<Vec<_>>();
        assert_eq!(
            managed_index_name("Parents", &cols(&["Code"]), false, true),
            "IDX_Parents_Code_U_1138935A6D4FCC53"
        );
        assert_eq!(
            managed_index_name("mon.alerts", &cols(&["code"]), true, true),
            "mon.IDX_alerts_code_U_54EEBC5310A8EBE2"
        );
        assert_eq!(
            managed_index_name("mon.comments", &cols(&["alert_id"]), false, false),
            "mon.IDX_comments_alert_id_D5970CE6F9EF9329"
        );
        assert_eq!(
            managed_index_name("Children", &cols(&["PA", "PB"]), true, false),
            "IDX_Children_PA_PB_N_34E3181FE3AAD541"
        );
    }

    #[test]
    fn long_managed_index_names_are_cut_to_128_bytes() {
        let col = "C".repeat(120);
        let name = managed_index_name("T", &[col], true, true);
        assert_eq!(name.len(), spanbolt_ddl::names::MAX_IDENTIFIER);
        assert!(name.starts_with("IDX_T_CCC"));
        assert!(name.ends_with(&format!("_U_{}", &name[name.len() - 16..])));
    }
}
