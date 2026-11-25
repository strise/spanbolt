//! The GoogleSQL catalog of one schema version: tables, synonyms, views and INFORMATION_SCHEMA,
//! plus the cache of prepared statements analysed against it.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use googlesql_sys::proto::ResolvedQueryStmtProto;
use parking_lot::Mutex;
use spanbolt_ddl::{ObjectKind, Schema};
use spanbolt_storage::{Catalog as StorageCatalog, Error, Result, TableId};

use crate::analyze;
use crate::info_schema::InfoRows;
use crate::prepared::Prepared;
use crate::types::{self, Type};

/// What a catalog table name refers to.
#[derive(Clone)]
pub enum Relation {
    Table(TableId),
    View(Arc<ViewDef>),
    Info(Arc<InfoTable>),
}

pub struct ViewDef {
    pub name: String,
    pub query: ResolvedQueryStmtProto,
}

pub struct InfoTable {
    pub name: String,
    pub columns: Vec<(String, Type, bool)>,
}

/// Prepared statements are cached per (SQL, parameter types).
type PlanKey = (String, String);
const MAX_PLANS: usize = 4096;

pub struct SqlCatalog {
    pub gsql: googlesql_sys::Catalog,
    pub schema: Arc<Schema>,
    pub storage: Arc<StorageCatalog>,
    /// Keyed by lowercase full name (`schema.table`, `information_schema.tables`).
    relations: HashMap<String, Relation>,
    plans: Mutex<HashMap<PlanKey, Arc<Prepared>>>,
    /// Property graphs, keyed by lowercase name.
    pub graphs: HashMap<String, Arc<crate::graph::GraphDef>>,
    /// Graph adjacency built on first use, per element table and table contents.
    pub graph_index: crate::graph_exec::IndexCache,
    /// Emulator-shaped INFORMATION_SCHEMA (`--emulator-compat`) rather than production's.
    pub emulator_compat: bool,
    /// The proto bundle's types, when it has any.
    pub protos: Option<Arc<crate::protos::Bundle>>,
    /// INFORMATION_SCHEMA rows, built on first use. They depend on the schema alone.
    info_rows: Mutex<HashMap<String, Arc<InfoRows>>>,
    pub hits: std::sync::atomic::AtomicU64,
    pub misses: std::sync::atomic::AtomicU64,
}

fn view_of(r: Option<&Relation>) -> Option<Arc<ViewDef>> {
    match r? {
        Relation::View(v) => Some(v.clone()),
        _ => None,
    }
}

impl SqlCatalog {
    /// The declared names of the tables and INFORMATION_SCHEMA tables (not views or synonyms).
    pub fn table_names(&self) -> Vec<String> {
        self.relations
            .iter()
            .filter_map(|(key, r)| match r {
                Relation::Table(id) => {
                    let name = &self.storage.table(*id).name;
                    (name.to_ascii_lowercase() == *key).then(|| name.clone())
                }
                Relation::Info(it) => Some(format!("INFORMATION_SCHEMA.{}", it.name)),
                Relation::View(_) => None,
            })
            .collect()
    }

    /// The view `full_name` names, if it names one.
    pub fn view(&self, full_name: &str) -> Option<Arc<ViewDef>> {
        view_of(self.relation(full_name))
    }

    pub fn relation(&self, full_name: &str) -> Option<&Relation> {
        self.relations.get(&full_name.to_ascii_lowercase())
    }

    pub fn relations(&self) -> impl Iterator<Item = &Relation> {
        self.relations.values()
    }

    pub fn cached(&self, key: &PlanKey) -> Option<Arc<Prepared>> {
        let p = self.plans.lock().get(key).cloned();
        let counter = if p.is_some() {
            &self.hits
        } else {
            &self.misses
        };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        p
    }

    pub fn plan_count(&self) -> usize {
        self.plans.lock().len()
    }

    /// The rows of `INFORMATION_SCHEMA.<table>`, built once per catalog (schema version).
    pub fn info_rows(&self, table: &str) -> Result<Arc<InfoRows>> {
        if let Some(r) = self.info_rows.lock().get(table) {
            return Ok(r.clone());
        }
        let rows = crate::info_schema::rows(self, table)?;
        let key = match self.relation(&format!("information_schema.{table}")) {
            Some(Relation::Info(it)) => it.columns.iter().position(|(n, ..)| n == "TABLE_NAME"),
            _ => None,
        };
        let rows = Arc::new(InfoRows::new(rows, key));
        Ok(self
            .info_rows
            .lock()
            .entry(table.to_string())
            .or_insert(rows)
            .clone())
    }

    pub fn store(&self, key: PlanKey, p: Arc<Prepared>) {
        let mut plans = self.plans.lock();
        if plans.len() >= MAX_PLANS {
            plans.clear();
        }
        plans.insert(key, p);
    }
}

/// The INFORMATION_SCHEMA tables, parsed once per process: the emulator's with
/// `--emulator-compat`, production's otherwise.
pub(crate) fn info_tables(emulator_compat: bool) -> &'static [Arc<InfoTable>] {
    static EMULATOR: std::sync::LazyLock<Vec<Arc<InfoTable>>> =
        std::sync::LazyLock::new(|| parse_info_tables(include_str!("info_schema_columns.tsv")));
    static SPANNER: std::sync::LazyLock<Vec<Arc<InfoTable>>> = std::sync::LazyLock::new(|| {
        parse_info_tables(include_str!("info_schema_columns_spanner.tsv"))
    });
    if emulator_compat { &EMULATOR } else { &SPANNER }
}

/// `STRING(MAX)`, `STRING(100)`, `ARRAY<STRING(MAX)>`, `BOOL`, `INT64`, `JSON`, and
/// `PROTO<proto2.FileDescriptorSet>` (GoogleSQL's `google.protobuf.FileDescriptorSet`).
fn info_type(ty: &str) -> Type {
    if let Some(e) = ty.strip_prefix("ARRAY<").and_then(|t| t.strip_suffix('>')) {
        return Type::Array(Box::new(info_type(e)));
    }
    if let Some(m) = ty
        .strip_prefix("PROTO<proto2.")
        .and_then(|t| t.strip_suffix('>'))
    {
        return Type::Proto(format!("google.protobuf.{m}"));
    }
    match ty {
        "BOOL" => Type::Bool,
        "INT64" => Type::Int64,
        "JSON" => Type::Json,
        t => Type::String(
            t.strip_prefix("STRING(")
                .and_then(|n| n.strip_suffix(')'))
                .and_then(|n| n.parse().ok()),
        ),
    }
}

fn parse_info_tables(tsv: &str) -> Vec<Arc<InfoTable>> {
    let mut out: Vec<InfoTable> = Vec::new();
    for line in tsv.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        let [table, column, ty, nullable] = parts[..] else {
            continue;
        };
        if out.last().is_none_or(|t| t.name != table) {
            out.push(InfoTable {
                name: table.to_string(),
                columns: Vec::new(),
            });
        }
        if let Some(t) = out.last_mut() {
            t.columns
                .push((column.to_string(), info_type(ty), nullable == "YES"));
        }
    }
    out.into_iter().map(Arc::new).collect()
}

/// GoogleSQL tables shared by every catalog whose schema has the same table definition, keyed by
/// that definition (the table's JSON). A DDL then builds only the tables it changed.
/// Also the analysed views, which a DDL that leaves their tables alone doesn't re-analyse.
#[derive(Default)]
pub struct TableCache {
    tables: Mutex<HashMap<String, Weak<googlesql_sys::Table>>>,
    views: Mutex<HashMap<String, Arc<AnalysedView>>>,
    /// The last proto bundle built, for the next schema version with the same bundle.
    bundle: Mutex<Weak<crate::protos::Bundle>>,
}

const MAX_VIEWS: usize = 1024;

/// A view's analysis, valid for any catalog in which its SQL's identifiers name the same tables.
struct AnalysedView {
    deps: Vec<(String, Weak<googlesql_sys::Table>)>,
    query: ResolvedQueryStmtProto,
    columns: Vec<(String, Type)>,
}

/// The relations `sql` could refer to: every relation in the catalog with a name part (schema or
/// table) spelled by an identifier of `sql`, with its GoogleSQL table. A superset of what the
/// analyser looks up, so equal dependencies mean the view analyses to the same result; a new or
/// changed table with one of those names changes them.
fn view_deps(
    sql: &str,
    names: &[String],
    tables: &[Arc<googlesql_sys::Table>],
) -> Vec<(String, Weak<googlesql_sys::Table>)> {
    let words: std::collections::HashSet<String> = sql
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut deps: Vec<_> = names
        .iter()
        .zip(tables)
        .filter(|(n, _)| n.split('.').any(|part| words.contains(part)))
        .map(|(n, t)| (n.clone(), Arc::downgrade(t)))
        .collect();
    deps.sort_by(|a, b| a.0.cmp(&b.0));
    deps
}

fn same_deps(
    a: &[(String, Weak<googlesql_sys::Table>)],
    b: &[(String, Weak<googlesql_sys::Table>)],
) -> bool {
    // A Weak keeps its allocation, so an equal pointer is the same table, never a reused address.
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|((na, ta), (nb, tb))| na == nb && Weak::ptr_eq(ta, tb))
}

impl TableCache {
    /// The table for `def`; `protos` when its columns use the bundle's types, which then key it
    /// too (a table refers to the bundle's GoogleSQL types).
    fn get(
        &self,
        def: String,
        protos: Option<&Arc<googlesql_sys::Protos>>,
    ) -> Result<Arc<googlesql_sys::Table>> {
        let key = match protos {
            // The table holds its Protos, so the address names one bundle while the table lives.
            Some(p) => format!("{:p}\n{def}", Arc::as_ptr(p)),
            None => def,
        };
        if let Some(t) = self.tables.lock().get(&key).and_then(Weak::upgrade) {
            return Ok(t);
        }
        let json = match protos {
            Some(_) => &key[key.find('\n').map_or(0, |i| i + 1)..],
            None => key.as_str(),
        };
        let built = match protos {
            Some(p) => googlesql_sys::Table::with_protos(json, p),
            None => googlesql_sys::Table::new(json),
        };
        let t = Arc::new(
            built.map_err(|e| Error::internal(format!("cannot build a GoogleSQL table: {e}")))?,
        );
        let mut tables = self.tables.lock();
        // Tables no catalog uses any more; amortised over the inserts.
        if tables.len() >= 1024 && tables.len().is_power_of_two() {
            tables.retain(|_, w| w.strong_count() > 0);
        }
        // A concurrent build may have made the same table; either copy works.
        tables.insert(key, Arc::downgrade(&t));
        Ok(t)
    }

    /// The types of `schema`'s proto bundle, if it has any.
    fn bundle(&self, schema: &Schema) -> Result<Option<Arc<crate::protos::Bundle>>> {
        let Some(b) = schema.proto_bundle.as_ref().filter(|b| !b.types.is_empty()) else {
            return Ok(None);
        };
        if let Some(cached) = self.bundle.lock().upgrade()
            && Arc::ptr_eq(&cached.descriptors, &b.descriptors)
            && cached.types.iter().eq(b.types.keys())
        {
            return Ok(Some(cached));
        }
        let built = Arc::new(crate::protos::Bundle::new(b)?);
        *self.bundle.lock() = Arc::downgrade(&built);
        Ok(Some(built))
    }

    /// Definitions held, live or not yet pruned.
    pub fn len(&self) -> usize {
        self.tables.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct Builder<'a> {
    cache: &'a TableCache,
    features: Vec<&'static str>,
    protos: Option<Arc<crate::protos::Bundle>>,
    tables: Vec<Arc<googlesql_sys::Table>>,
    /// Lowercase names of `tables`.
    names: Vec<String>,
    relations: HashMap<String, Relation>,
}

impl Builder<'_> {
    fn add(&mut self, name: &str, columns: &[Col], pk: &[usize], rel: Relation) -> Result<()> {
        let protos = self
            .protos
            .as_ref()
            .filter(|_| columns.iter().any(|c| types::is_bundle_type(c.1)))
            .map(|b| &b.gsql);
        self.tables
            .push(self.cache.get(table_json(name, columns, pk), protos)?);
        let lower = name.to_ascii_lowercase();
        self.names.push(lower.clone());
        self.relations.insert(lower, rel);
        Ok(())
    }

    /// The view's analysis from an earlier catalog with the same dependencies, or a fresh one.
    fn analyse_view(&self, gsql: &googlesql_sys::Catalog, sql: &str) -> Result<Arc<AnalysedView>> {
        let deps = view_deps(sql, &self.names, &self.tables);
        if let Some(v) = self.cache.views.lock().get(sql)
            && same_deps(&v.deps, &deps)
        {
            return Ok(v.clone());
        }
        let query = analyze::view(gsql, sql)?;
        let mut columns = Vec::new();
        for oc in &query.output_column_list {
            let ty = oc
                .column
                .as_ref()
                .and_then(|c| c.r#type.as_ref())
                .ok_or_else(|| Error::internal("view column without type"))?;
            columns.push((oc.name.clone().unwrap_or_default(), types::from_proto(ty)?));
        }
        let v = Arc::new(AnalysedView {
            deps,
            query,
            columns,
        });
        let mut views = self.cache.views.lock();
        if views.len() >= MAX_VIEWS {
            views.clear();
        }
        views.insert(sql.to_string(), v.clone());
        Ok(v)
    }

    fn gsql(&self) -> Result<googlesql_sys::Catalog> {
        googlesql_sys::Catalog::from_tables_with_protos(
            self.tables.clone(),
            &self.features,
            self.protos.as_ref().map(|b| b.gsql.clone()),
        )
        .map_err(|e| Error::internal(format!("cannot build the GoogleSQL catalog: {e}")))
    }
}

/// A column of a GoogleSQL table: name, type, hidden (excluded from `SELECT *`), writable.
type Col<'a> = (&'a str, &'a Type, bool, bool);

fn push_json_str(out: &mut String, s: &str) {
    // Infallible: a str always serialises.
    out.push_str(&serde_json::to_string(s).unwrap_or_default());
}

/// The shim's table JSON (`googlesql_shim.h`), also the table's cache key.
fn table_json(name: &str, columns: &[Col], pk: &[usize]) -> String {
    let mut out = String::with_capacity(64 + columns.len() * 64);
    out.push_str("{\"name\":");
    push_json_str(&mut out, name);
    out.push_str(",\"columns\":[");
    for (i, (n, ty, hidden, writable)) in columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        push_json_str(&mut out, n);
        out.push_str(",\"type\":");
        push_json_str(&mut out, &types::gsql_name(ty));
        out.push_str(if *hidden {
            ",\"hidden\":true"
        } else {
            ",\"hidden\":false"
        });
        out.push_str(if *writable {
            ",\"writable\":true}"
        } else {
            ",\"writable\":false}"
        });
    }
    out.push(']');
    if !pk.is_empty() {
        out.push_str(",\"primary_key\":");
        out.push_str(&serde_json::to_string(pk).unwrap_or_default());
    }
    out.push('}');
    out
}

/// Builds the catalog for `schema`, reusing the GoogleSQL tables in `cache`. Views are analysed in
/// creation order so they can refer to earlier views.
pub fn build(
    schema: Arc<Schema>,
    storage: Arc<StorageCatalog>,
    cache: &TableCache,
    features: &'static [&'static str],
    emulator_compat: bool,
) -> Result<SqlCatalog> {
    let protos = cache.bundle(&schema)?;
    let mut features = features.to_vec();
    if protos.is_some() {
        features.extend_from_slice(crate::protos::FEATURES);
    }
    let mut b = Builder {
        cache,
        features,
        protos: protos.clone(),
        tables: Vec::with_capacity(storage.tables.len() + 64),
        names: Vec::with_capacity(storage.tables.len() + 64),
        relations: HashMap::new(),
    };
    for (id, tm) in storage.tables.iter().enumerate() {
        let st = schema.table(&tm.name);
        let cols: Vec<Col> = tm
            .columns
            .iter()
            .map(|c| {
                let hidden = st
                    .and_then(|t| t.column(&c.name))
                    .is_some_and(|sc| sc.hidden);
                (c.name.as_str(), &c.ty, hidden, !c.generated)
            })
            .collect();
        b.add(&tm.name, &cols, &tm.pk, Relation::Table(id))?;
        if let Some(syn) = st.and_then(|t| t.synonym.as_ref()) {
            b.add(syn, &cols, &tm.pk, Relation::Table(id))?;
        }
    }
    for it in info_tables(emulator_compat) {
        let cols: Vec<Col> = it
            .columns
            .iter()
            .map(|(n, t, _)| (n.as_str(), t, false, false))
            .collect();
        let name = format!("INFORMATION_SCHEMA.{}", it.name);
        b.add(&name, &cols, &[], Relation::Info(it.clone()))?;
    }
    let views: Vec<_> = schema
        .creation_order
        .iter()
        .filter(|(k, _)| *k == ObjectKind::View)
        .filter_map(|(_, n)| schema.views.get(n))
        .collect();
    let mut gsql = b.gsql()?;
    let mut pending = views;
    while !pending.is_empty() {
        let mut next = Vec::new();
        let mut added = false;
        for v in pending {
            match b.analyse_view(&gsql, &v.sql) {
                Ok(a) => {
                    let cols: Vec<Col> = a
                        .columns
                        .iter()
                        .map(|(n, t)| (n.as_str(), t, false, false))
                        .collect();
                    let def = Arc::new(ViewDef {
                        name: v.name.clone(),
                        query: a.query.clone(),
                    });
                    b.add(&v.name, &cols, &[], Relation::View(def))?;
                    added = true;
                }
                Err(_) => next.push(v),
            }
        }
        if !added {
            break;
        }
        gsql = b.gsql()?;
        pending = next;
    }
    let mut graphs = HashMap::new();
    let views = |name: &str| view_of(b.relations.get(&name.to_ascii_lowercase()));
    for g in schema.opaque.iter().filter(|o| o.kind == "PROPERTY GRAPH") {
        // A graph the current tables no longer support is left out; DDL rejects creating one.
        let Ok(def) = crate::graph::analyse(&gsql, &storage, &views, &g.ddl) else {
            continue;
        };
        if gsql
            .add_property_graph(&crate::graph::shim_json(&def))
            .is_ok()
        {
            graphs.insert(def.name.to_ascii_lowercase(), Arc::new(def));
        }
    }
    Ok(SqlCatalog {
        gsql,
        schema,
        storage,
        graphs,
        graph_index: Default::default(),
        relations: b.relations,
        plans: Mutex::new(HashMap::new()),
        emulator_compat,
        protos,
        info_rows: Mutex::new(HashMap::new()),
        hits: Default::default(),
        misses: Default::default(),
    })
}

/// Catalogs per schema, shared by every database (and clone) that uses the same `Arc<Schema>`.
#[derive(Default)]
pub struct CatalogCache {
    entries: Mutex<Vec<(Weak<Schema>, Arc<SqlCatalog>)>>,
    tables: TableCache,
}

/// Whether anything besides the cached catalog itself still holds `cat`'s schema (a database
/// version). The catalog's own `schema` field is a strong reference, so a `Weak<Schema>` alone never
/// expires: testing it kept every schema version ever queried alive.
fn in_use(cat: &SqlCatalog) -> bool {
    Arc::strong_count(&cat.schema) > 1
}

impl CatalogCache {
    /// Catalogs held, live or not yet pruned.
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// GoogleSQL table definitions held for reuse across schema versions.
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// Cached catalogs and the prepared statements they hold.
    pub fn sizes(&self) -> (usize, usize) {
        let entries = self.entries.lock();
        (
            entries.len(),
            entries.iter().map(|(_, c)| c.plan_count()).sum(),
        )
    }

    /// The catalog for `schema`. `features` are the extra language features (fixed per engine).
    pub fn get(
        &self,
        schema: &Arc<Schema>,
        storage: &Arc<StorageCatalog>,
        features: &'static [&'static str],
        emulator_compat: bool,
    ) -> Result<Arc<SqlCatalog>> {
        {
            let mut entries = self.entries.lock();
            entries.retain(|(_, c)| in_use(c));
            if let Some((_, c)) = entries
                .iter()
                .find(|(w, _)| std::ptr::eq(w.as_ptr(), Arc::as_ptr(schema)))
            {
                return Ok(c.clone());
            }
        }
        let built = Arc::new(build(
            schema.clone(),
            storage.clone(),
            &self.tables,
            features,
            emulator_compat,
        )?);
        let mut entries = self.entries.lock();
        if let Some((_, c)) = entries
            .iter()
            .find(|(w, _)| std::ptr::eq(w.as_ptr(), Arc::as_ptr(schema)))
        {
            return Ok(c.clone());
        }
        entries.push((Arc::downgrade(schema), built.clone()));
        Ok(built)
    }
}
