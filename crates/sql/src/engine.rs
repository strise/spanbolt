//! The entry point the server calls: ExecuteSql, ExecuteBatchDml and PartitionQuery.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use googlesql_sys::proto::any_resolved_statement_proto::Node as StmtNode;
use prost_types::value::Kind;
use spanbolt_ddl::Schema;
use spanbolt_proto::spanner as pb;
use spanbolt_storage::expr::SchemaChange;
use spanbolt_storage::{Code, DbState, Error, Result, Value};
use spanbolt_txn::{Database, ReadOnlyTxn, ReadWriteTxn};

use crate::catalog::{CatalogCache, SqlCatalog};
use crate::exec::{Cx, Env};
use crate::plan::{Options, Planner, Scope, hint_bool, positions};
use crate::prepared::{Body, Prepared, SearchUse};
use crate::types::{self, Type};

/// The transaction a statement runs in.
pub enum Txn<'a> {
    ReadOnly(&'a ReadOnlyTxn),
    ReadWrite(&'a Arc<ReadWriteTxn>),
    PartitionedDml,
}

/// Receives a statement's result as the executor produces it: the metadata once, then each row.
/// An error stops execution and is returned from [`Engine::execute_sql_into`].
pub trait RowSink {
    fn metadata(&mut self, m: pb::ResultSetMetadata) -> Result<()>;
    fn row(&mut self, values: Vec<prost_types::Value>) -> Result<()>;
}

/// Collects a whole result set (unary ExecuteSql).
#[derive(Default)]
struct Collect {
    metadata: Option<pb::ResultSetMetadata>,
    rows: Vec<prost_types::ListValue>,
}

impl RowSink for Collect {
    fn metadata(&mut self, m: pb::ResultSetMetadata) -> Result<()> {
        self.metadata = Some(m);
        Ok(())
    }
    fn row(&mut self, values: Vec<prost_types::Value>) -> Result<()> {
        self.rows.push(prost_types::ListValue { values });
        Ok(())
    }
}

/// Hands a finished result set (DML) to a sink.
fn drain(rs: pb::ResultSet, sink: &mut dyn RowSink) -> Result<Option<pb::ResultSetStats>> {
    sink.metadata(rs.metadata.unwrap_or_default())?;
    for r in rs.rows {
        sink.row(r.values)?;
    }
    Ok(rs.stats)
}

#[derive(Default)]
pub struct Engine {
    cache: CatalogCache,
    pub opts: Options,
}

/// A statement with its parameters, from ExecuteSql or one ExecuteBatchDml entry.
pub struct Stmt<'a> {
    pub sql: &'a str,
    pub params: Option<&'a prost_types::Struct>,
    pub param_types: &'a HashMap<String, pb::Type>,
}

impl Engine {
    pub fn new(opts: Options) -> Engine {
        Engine {
            cache: CatalogCache::default(),
            opts,
        }
    }

    pub fn catalog(&self, state: &DbState) -> Result<Arc<SqlCatalog>> {
        self.cache.get(
            &state.schema,
            &state.catalog,
            self.opts.extra_features(),
            self.opts.emulator_compat,
        )
    }

    /// Checks the property graph `name` of `schema` against its tables, as `CREATE PROPERTY GRAPH`
    /// does: the analyser's error is the statement's error. Returns the definition to keep:
    /// Cloud Spanner's printed form, with `ALL COLUMNS` expanded once, now.
    pub fn validate_property_graph(
        &self,
        schema: &Arc<Schema>,
        name: &str,
    ) -> Result<Option<String>> {
        let Some(g) = schema
            .opaque
            .iter()
            .find(|o| o.kind == "PROPERTY GRAPH" && o.name.eq_ignore_ascii_case(name))
        else {
            return Ok(None);
        };
        // Analysed against the other objects, as the graph itself is not defined yet.
        let mut others = (**schema).clone();
        others
            .opaque
            .retain(|o| !(o.kind == "PROPERTY GRAPH" && o.name.eq_ignore_ascii_case(name)));
        let others = Arc::new(others);
        let storage = Arc::new(spanbolt_storage::Catalog::build(&others)?);
        let cat = self.cache.get(
            &others,
            &storage,
            self.opts.extra_features(),
            self.opts.emulator_compat,
        )?;
        let views = |n: &str| cat.view(n);
        let analyse = |ddl: &str| crate::graph::analyse(&cat.gsql, &storage, &views, ddl);
        let compat = self.opts.emulator_compat;
        let input = |t: &str| -> Option<crate::graph_ddl::Input> {
            if let Some(t) = others.table(t) {
                return Some(crate::graph_ddl::Input {
                    columns: t.columns.iter().map(|c| c.name.clone()).collect(),
                    key: t.primary_key.iter().map(|k| k.column.clone()).collect(),
                });
            }
            let v = cat.view(t)?;
            Some(crate::graph_ddl::Input {
                columns: v
                    .query
                    .output_column_list
                    .iter()
                    .map(|o| o.name.clone().unwrap_or_default())
                    .collect(),
                key: Vec::new(),
            })
        };
        let printed = crate::graph_ddl::parse(&g.ddl).map(|p| p.print(!compat, &input));
        if compat {
            // The emulator reports errors in the statement as written.
            analyse(&g.ddl)?;
            return Ok(printed.filter(|p| analyse(p).is_ok()));
        }
        // Cloud Spanner analyses its printed form, and says which graph it was.
        let text = printed.unwrap_or_else(|| g.ddl.clone());
        analyse(&text).map_err(|e| {
            if e.code == spanbolt_storage::Code::Unimplemented {
                return e;
            }
            Error::new(
                e.code,
                format!(
                    "Error analyzing the definition of graph `{}`: {}",
                    g.name, e.message
                ),
            )
        })?;
        Ok(Some(text))
    }

    /// The property graphs of `state` as emulator 1.5.59's GetDatabaseDdl prints them, by name.
    pub fn emulator_graph_ddl(&self, state: &DbState) -> Vec<(String, String)> {
        let Ok(cat) = self.catalog(state) else {
            return Vec::new();
        };
        cat.graphs
            .values()
            .map(|g| (g.name.clone(), crate::graph::emulator_ddl(g)))
            .collect()
    }

    /// Checks the view `name` of `schema` as `CREATE VIEW` does; `replacing` when `CREATE OR
    /// REPLACE` changes an existing view. The error is the statement's.
    pub fn validate_view(&self, schema: &Arc<Schema>, name: &str, replacing: bool) -> Result<()> {
        let key = name.to_ascii_lowercase();
        let Some(view) = schema.views.get(&key) else {
            return Ok(());
        };
        // Analysed against the other objects, as the view itself is not defined yet.
        let mut others = (**schema).clone();
        others.views.remove(&key);
        let others = Arc::new(others);
        let storage = Arc::new(spanbolt_storage::Catalog::build(&others)?);
        let cat = self.cache.get(
            &others,
            &storage,
            self.opts.extra_features(),
            self.opts.emulator_compat,
        )?;
        let compat = self.opts.emulator_compat;
        let body = crate::view_check::body(&view.sql);
        let Err(e) = crate::view_check::check(&cat, body, !compat) else {
            return Ok(());
        };
        if e.code == spanbolt_storage::Code::Unimplemented {
            return Err(e);
        }
        // Positions are in the statement each prints the view as: the emulator always quotes the
        // name, Spanner only when it must.
        let quoted = if compat {
            format!("`{}`", view.name)
        } else {
            spanbolt_ddl::quote_ident(&view.name)
        };
        let prefix = format!("CREATE VIEW {quoted} SQL SECURITY INVOKER AS ");
        let reason = shift_first_line(&e.message, prefix.chars().count());
        let analysing = format!("Error analyzing the definition of view `{}`: ", view.name);
        Err(match (replacing, compat) {
            (false, _) => Error::invalid_argument(format!("{analysing}{reason}")),
            (true, true) => Error::new(
                spanbolt_storage::Code::FailedPrecondition,
                format!(
                    "Cannot replace VIEW `{}` because new definition is invalid with the \
                     following diagnostic message: {reason}",
                    view.name
                ),
            ),
            // Spanner's message has the two escaped newlines in it.
            (true, false) => Error::new(
                spanbolt_storage::Code::FailedPrecondition,
                format!(
                    "Cannot replace VIEW `{}` because new definition is invalid with the \
                     following diagnostic message:\\n\\n{analysing}{reason}",
                    view.name
                ),
            ),
        })
    }

    /// Rejects `change` (from `before` to `after`) when a property graph depends on it, with
    /// Cloud Spanner's errors. The emulator has no such check and aborts on the next query of
    /// the graph, so `--emulator-compat` rejects it too.
    pub fn check_graph_dependents(
        &self,
        before: &Arc<Schema>,
        after: &Arc<Schema>,
        change: &SchemaChange,
    ) -> Result<()> {
        let storage = Arc::new(spanbolt_storage::Catalog::build(before)?);
        let old = self.cache.get(
            before,
            &storage,
            self.opts.extra_features(),
            self.opts.emulator_compat,
        )?;
        // Graphs in the order the schema created them.
        let graphs: Vec<&Arc<crate::graph::GraphDef>> = before
            .opaque
            .iter()
            .filter(|o| o.kind == "PROPERTY GRAPH")
            .filter_map(|o| old.graphs.get(&o.name.to_ascii_lowercase()))
            .collect();
        let reads = |g: &crate::graph::GraphDef, view: bool, name: &str| {
            g.nodes.iter().chain(&g.edges).any(|e| {
                matches!(e.input, crate::graph::ElementInput::View(_)) == view
                    && e.table.eq_ignore_ascii_case(name)
            })
        };
        let names = |gs: &[&Arc<crate::graph::GraphDef>], sep: &str, quote: bool| {
            gs.iter()
                .map(|g| {
                    if quote {
                        format!("`{}`", g.name)
                    } else {
                        g.name.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(sep)
        };
        let (table, view) = match change {
            SchemaChange::DropTable(t) => {
                let using: Vec<_> = graphs.into_iter().filter(|g| reads(g, false, t)).collect();
                if using.is_empty() {
                    return Ok(());
                }
                return Err(Error::failed_precondition(format!(
                    "Cannot drop TABLE `{t}` on which there are dependent property graphs: {}.",
                    names(&using, ", ", false)
                )));
            }
            SchemaChange::DropView(v) => {
                let using: Vec<_> = graphs.into_iter().filter(|g| reads(g, true, v)).collect();
                if using.is_empty() {
                    return Ok(());
                }
                return Err(Error::failed_precondition(format!(
                    "Cannot drop VIEW `{v}` on which there are dependent property_graphs: {}.",
                    names(&using, ", ", false)
                )));
            }
            SchemaChange::DropColumn { table, column } => {
                let using: Vec<_> = graphs
                    .into_iter()
                    .filter(|g| graph_reads_column(g, table, column))
                    .collect();
                if using.is_empty() {
                    return Ok(());
                }
                return Err(Error::failed_precondition(format!(
                    "Cannot drop column `{column}` from the table `{table}` because it is used by the property graph(s) {}.",
                    names(&using, ", ", true)
                )));
            }
            SchemaChange::AlterColumn { table, .. } => (Some(table), None),
            SchemaChange::ReplaceView(v) => (None, Some(v)),
        };
        let using: Vec<_> = graphs
            .into_iter()
            .filter(|g| match (table, view) {
                (Some(t), _) => reads(g, false, t),
                (_, Some(v)) => reads(g, true, v),
                _ => false,
            })
            .collect();
        if using.is_empty() {
            return Ok(());
        }
        // Each graph is analysed again against the changed schema without any graph.
        let mut others = (**after).clone();
        others.opaque.retain(|o| o.kind != "PROPERTY GRAPH");
        let others = Arc::new(others);
        let storage = Arc::new(spanbolt_storage::Catalog::build(&others)?);
        let cat = self.cache.get(
            &others,
            &storage,
            self.opts.extra_features(),
            self.opts.emulator_compat,
        )?;
        for g in using {
            let Some(ddl) = before
                .opaque
                .iter()
                .find(|o| o.kind == "PROPERTY GRAPH" && o.name == g.name)
                .map(|o| o.ddl.as_str())
            else {
                continue;
            };
            let analysed = crate::graph::analyse(&cat.gsql, &storage, &|n| cat.view(n), ddl);
            let message = match analysed {
                Err(e) => {
                    let message = format!(
                        "Error analyzing the definition of graph `{}`: {}",
                        g.name, e.message
                    );
                    match view {
                        Some(_) => message,
                        // The emulator accepts a column change that breaks a graph's key
                        // references (it fails on the graph's next use).
                        None if self.opts.emulator_compat => continue,
                        None => return Err(Error::new(e.code, message)),
                    }
                }
                Ok(new) => {
                    let Some((p, ty)) = g.properties.iter().find_map(|p| {
                        let q = new.property(&p.name)?;
                        (q.ty != p.ty).then_some((p, &q.ty))
                    }) else {
                        continue;
                    };
                    if view.is_none() {
                        // Cloud Spanner answers a column type change that a graph property
                        // can't follow with an empty message.
                        return Err(Error::invalid_argument(String::new()));
                    }
                    format!(
                        "The schema change implicitly changes the type of the property `{}` for property graph `{}` from `{}` to `{}`.\\nTip: Give explicit type to the property definition before making this change by adding casts/conversions to the intended final output type.",
                        p.name,
                        g.name,
                        types::gsql_name(&p.ty),
                        types::gsql_name(ty)
                    )
                }
            };
            let v = view.map(String::as_str).unwrap_or_default();
            return Err(Error::failed_precondition(format!(
                "Cannot replace VIEW `{v}`. The new definition causes the definition of PROPERTY GRAPH `{}` to become invalid with the following diagnostic message:\\n\\n{message}",
                g.name
            )));
        }
        Ok(())
    }

    /// GoogleSQL catalogs this engine holds (pruned when the next catalog is looked up).
    pub fn cached_catalogs(&self) -> usize {
        self.cache.len()
    }

    /// Analysis-cache hits and misses across every catalog this engine built.
    pub fn prepare(&self, cat: &SqlCatalog, st: &Stmt) -> Result<Arc<Prepared>> {
        let mut decl: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        if let Some(p) = st.params {
            for name in p.fields.keys() {
                decl.insert(name.clone(), serde_json::Value::Null);
            }
        }
        for (name, t) in st.param_types {
            decl.insert(
                name.clone(),
                serde_json::Value::String(types::gsql_name(&types::from_spanner(t)?)),
            );
        }
        let params_json = serde_json::to_string(&decl).unwrap_or_else(|_| "{}".into());
        let p = self.prepare_sql(cat, st.sql, params_json)?;
        // Schema expressions (generated TOKENLIST columns) may produce TOKENLIST; queries may not.
        if matches!(p.body, Body::Query { .. })
            && p.fields.iter().any(|(_, t)| matches!(t, Type::TokenList))
        {
            return Err(Error::invalid_argument(
                "TOKENLIST is an internal-only type and cannot be returned to the user by SQL query.",
            ));
        }
        Ok(p)
    }

    /// Prepares `sql` with parameter types given as the shim's JSON (`{"name": "TYPE"}`).
    pub(crate) fn prepare_sql(
        &self,
        cat: &SqlCatalog,
        sql: &str,
        params_json: String,
    ) -> Result<Arc<Prepared>> {
        let key = (sql.to_string(), params_json);
        if let Some(p) = cat.cached(&key) {
            return Ok(p);
        }
        let p = Arc::new(self.plan(cat, sql, &key.1)?);
        cat.store(key, p.clone());
        Ok(p)
    }

    fn plan(&self, cat: &SqlCatalog, sql: &str, params_json: &str) -> Result<Prepared> {
        let stmt = crate::analyze::statement(cat, sql, params_json)?;
        let mut planner = Planner::new(cat, self.opts);
        match stmt.node {
            Some(StmtNode::ResolvedQueryStmtNode(q)) => {
                let hints = q
                    .parent
                    .as_ref()
                    .map(|p| p.hint_list.as_slice())
                    .unwrap_or(&[]);
                planner.statement_hints(crate::validate::Node::Query, hints)?;
                planner.stmt_disable_nf_check = hint_bool(
                    hints,
                    "spanner_emulator",
                    "disable_query_null_filtered_index_check",
                );
                let scan = q
                    .query
                    .as_ref()
                    .ok_or_else(|| Error::internal("query without scan"))?;
                let mut fields = Vec::new();
                let mut ids = Vec::new();
                for oc in &q.output_column_list {
                    let c = oc
                        .column
                        .as_ref()
                        .ok_or_else(|| Error::internal("output column without column"))?;
                    if c.r#type.as_ref().is_some_and(crate::types::has_graph) {
                        return Err(crate::types::unimplemented(
                            "Returning graph elements or paths from a query",
                        ));
                    }
                    // Proto fields of the integer types Spanner lacks: the emulator can't
                    // name the result type, production fails inside.
                    if let Some(name) = c.r#type.as_ref().and_then(crate::types::narrow_int) {
                        return Err(if self.opts.emulator_compat {
                            Error::new(
                                spanbolt_storage::Code::InvalidArgument,
                                format!("Type not found: {name}"),
                            )
                        } else {
                            Error::new(
                                spanbolt_storage::Code::Internal,
                                "Internal error encountered.",
                            )
                        });
                    }
                    ids.push(c.column_id.unwrap_or(-1));
                    // Anonymous columns (`$col1`) have no name on the wire.
                    let name = oc
                        .name
                        .clone()
                        .filter(|n| !n.starts_with('$'))
                        .unwrap_or_default();
                    let ty = crate::plan::col_type(c)?;
                    fields.push((
                        name,
                        crate::types::result_type(ty, self.opts.emulator_compat),
                    ));
                }
                if q.is_value_table == Some(true) {
                    // The emulator runs a top-level SELECT AS VALUE (and rejects AS STRUCT for
                    // its STRUCT column); Spanner rejects both
                    // (`conformance/cases/sql/value_tables_top_level.case`).
                    if !self.opts.emulator_compat {
                        return Err(Error::new(
                            Code::Unimplemented,
                            "Unsupported query shape: Top level queries may not specify value \
                             tables. 'SELECT AS STRUCT' and 'SELECT AS VALUE' are only supported \
                             in subqueries, not at the top level.",
                        ));
                    }
                    for f in &mut fields {
                        f.0.clear();
                    }
                }
                check_no_struct_columns(&fields)?;
                let p = planner.scan(scan, &Scope::TOP)?;
                planner.finish_search()?;
                if !self.opts.emulator_compat {
                    // Spanner reports a NULL-able or constructed ARRAY<STRUCT> result before a
                    // field read of a returned struct.
                    crate::struct_result::check(&q)?;
                    crate::struct_shape::check(&q)?;
                }
                let positions = positions(&p.layout, &ids)?;
                Ok(Prepared {
                    body: Body::Query {
                        op: p.op,
                        positions,
                    },
                    params: planner.param_types,
                    fields,
                    search: SearchUse {
                        allowed_in_txn: hint_bool(hints, "", "allow_search_indexes_in_transaction"),
                        ..planner.search
                    },
                    subplans: planner.subplans,
                    reads: planner.reads,
                    reads_info_schema: planner.reads_info_schema,
                })
            }
            Some(node) => {
                let (dml, fields) = crate::dml::plan(&mut planner, &node)?;
                planner.finish_search()?;
                check_no_struct_columns(&fields)?;
                Ok(Prepared {
                    body: Body::Dml(dml),
                    params: planner.param_types,
                    fields,
                    search: planner.search,
                    subplans: planner.subplans,
                    reads: planner.reads,
                    reads_info_schema: planner.reads_info_schema,
                })
            }
            None => Err(Error::internal("empty statement")),
        }
    }

    /// Decodes the request's parameter values for the prepared statement's parameters.
    pub fn bind(&self, p: &Prepared, st: &Stmt) -> Result<Vec<Value>> {
        let mut out = Vec::with_capacity(p.params.len());
        for param in &p.params {
            let v = st.params.and_then(|s| {
                s.fields.get(&param.name).or_else(|| {
                    s.fields
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(&param.name))
                        .map(|(_, v)| v)
                })
            });
            let Some(v) = v else {
                return Err(Error::invalid_argument(format!(
                    "No parameter found for binding: {}",
                    param.name
                )));
            };
            let declared = st.param_types.get(&param.name).or_else(|| {
                st.param_types
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&param.name))
                    .map(|(_, t)| t)
            });
            let val = match declared {
                Some(_) => {
                    Value::from_proto(v, &param.ty).map_err(|m| param_error(v, &param.ty, m))?
                }
                None => untyped(v, &param.ty)?,
            };
            out.push(val);
        }
        Ok(out)
    }

    pub fn execute_sql(
        &self,
        db: &Arc<Database>,
        txn: Txn<'_>,
        req: &pb::ExecuteSqlRequest,
    ) -> Result<pb::ResultSet> {
        let mut c = Collect::default();
        let stats = self.execute_sql_into(db, txn, req, &mut c)?;
        Ok(pb::ResultSet {
            metadata: c.metadata,
            rows: c.rows,
            stats,
            ..Default::default()
        })
    }

    /// Runs a statement, handing a query's rows to `sink` as the executor produces them.
    /// Returns the result's stats.
    pub fn execute_sql_into(
        &self,
        db: &Arc<Database>,
        txn: Txn<'_>,
        req: &pb::ExecuteSqlRequest,
        sink: &mut dyn RowSink,
    ) -> Result<Option<pb::ResultSetStats>> {
        let st = Stmt {
            sql: &req.sql,
            params: req.params.as_ref(),
            param_types: &req.param_types,
        };
        let mode = pb::execute_sql_request::QueryMode::try_from(req.query_mode)
            .unwrap_or(pb::execute_sql_request::QueryMode::Normal);
        match txn {
            Txn::ReadOnly(t) => {
                let state = t.version.state.clone();
                let cat = self.catalog(&state)?;
                let p = self.prepare(&cat, &st)?;
                if let Body::Dml(_) = p.body {
                    use pb::transaction_selector::Selector;
                    // Without a selector the emulator treats the request as a read-only
                    // transaction, not a single-use one, and says so.
                    let single_use = matches!(
                        req.transaction.as_ref().and_then(|t| t.selector.as_ref()),
                        Some(Selector::SingleUse(_))
                    );
                    return Err(Error::invalid_argument(if single_use {
                        "DML statements may not be performed in single-use transactions, to avoid replay."
                    } else {
                        "DML statements can only be performed in a read-write or partitioned-dml transaction. Current transaction type is ReadOnly."
                    }));
                }
                t.check_expired()?;
                let params = self.bind(&p, &st)?;
                self.run_query(
                    &cat,
                    &state,
                    &p,
                    &params,
                    None,
                    db.clock().now(),
                    mode,
                    sink,
                )
            }
            Txn::ReadWrite(rw) => {
                let state = rw.current_state()?;
                let cat = self.catalog(&state)?;
                let p = self.prepare(&cat, &st)?;
                p.check_read_write(self.opts.emulator_compat)?;
                p.check_pending_reads(rw, &state.catalog, self.opts.emulator_compat)?;
                let params = self.bind(&p, &st)?;
                match &p.body {
                    Body::Query { .. } => self.run_query(
                        &cat,
                        &state,
                        &p,
                        &params,
                        Some(rw),
                        db.clock().statement_ts(),
                        mode,
                        sink,
                    ),
                    Body::Dml(d) => drain(
                        crate::dml::execute(
                            &cat,
                            d,
                            &p,
                            &params,
                            rw,
                            db.clock().statement_ts(),
                            false,
                        )
                        .inspect_err(|e| rw.dml_failed(e))?,
                        sink,
                    ),
                }
            }
            Txn::PartitionedDml => {
                let mut last = None;
                for _ in 0..10 {
                    let rw = Arc::new(ReadWriteTxn::new(db.clone()));
                    let state = rw.current_state()?;
                    let cat = self.catalog(&state)?;
                    let p = self.prepare(&cat, &st)?;
                    let Body::Dml(d) = &p.body else {
                        return Err(Error::invalid_argument(
                            "Partitioned DML transactions can only execute DML statements.",
                        ));
                    };
                    let params = self.bind(&p, &st)?;
                    let rs = crate::dml::execute(
                        &cat,
                        d,
                        &p,
                        &params,
                        &rw,
                        db.clock().statement_ts(),
                        true,
                    )?;
                    match rw.commit(&state.catalog, &[]) {
                        Ok(_) => return drain(rs, sink),
                        Err(e) if e.code == Code::Aborted => last = Some(e),
                        Err(e) => return Err(e),
                    }
                }
                Err(last.unwrap_or_else(|| Error::aborted("Partitioned DML aborted")))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_query(
        &self,
        cat: &SqlCatalog,
        state: &DbState,
        p: &Prepared,
        params: &[Value],
        rw: Option<&ReadWriteTxn>,
        now: spanbolt_storage::Timestamp,
        mode: pb::execute_sql_request::QueryMode,
        sink: &mut dyn RowSink,
    ) -> Result<Option<pb::ResultSetStats>> {
        use pb::execute_sql_request::QueryMode;
        let Body::Query { op, positions } = &p.body else {
            return Err(Error::internal("not a query"));
        };
        sink.metadata(metadata(&p.fields))?;
        if mode == QueryMode::Plan {
            return Ok(Some(stats(None)));
        }
        let cx = Cx {
            state,
            cat,
            params,
            now,
            rw,
            with: RefCell::new(Vec::new()),
            memo: RefCell::new(HashMap::new()),
        };
        for r in op.open(&cx, &Env::EMPTY)? {
            let r = r?;
            sink.row(
                positions
                    .iter()
                    .map(|&i| {
                        r.get(i).map_or(
                            prost_types::Value {
                                kind: Some(Kind::NullValue(0)),
                            },
                            Value::to_proto,
                        )
                    })
                    .collect(),
            )?;
        }
        Ok((mode == QueryMode::Profile).then(|| stats(None)))
    }

    pub fn execute_batch_dml(
        &self,
        db: &Arc<Database>,
        rw: &Arc<ReadWriteTxn>,
        req: &pb::ExecuteBatchDmlRequest,
    ) -> Result<pb::ExecuteBatchDmlResponse> {
        if req.statements.is_empty() {
            return Err(Error::invalid_argument(
                "No statements in batch DML request.",
            ));
        }
        let mut result_sets = Vec::new();
        for (n, s) in req.statements.iter().enumerate() {
            let st = Stmt {
                sql: &s.sql,
                params: s.params.as_ref(),
                param_types: &s.param_types,
            };
            let run = || -> Result<pb::ResultSet> {
                if !looks_like_dml(&s.sql) {
                    return Err(Error::invalid_argument(format!(
                        "Statement {n}: '{}' is not valid DML.",
                        s.sql
                    )));
                }
                let state = rw.current_state()?;
                let cat = self.catalog(&state)?;
                let p = self.prepare(&cat, &st)?;
                p.check_read_write(self.opts.emulator_compat)?;
                p.check_pending_reads(rw, &state.catalog, self.opts.emulator_compat)?;
                let params = self.bind(&p, &st)?;
                match &p.body {
                    Body::Dml(d) => crate::dml::execute(
                        &cat,
                        d,
                        &p,
                        &params,
                        rw,
                        db.clock().statement_ts(),
                        false,
                    ),
                    Body::Query { .. } => Err(Error::invalid_argument(
                        "ExecuteBatchDml only supports DML statements.",
                    )),
                }
            };
            match run() {
                Ok(mut rs) => {
                    rs.rows.clear();
                    if !result_sets.is_empty() {
                        rs.metadata = None;
                    }
                    result_sets.push(rs);
                }
                Err(e) => {
                    rw.dml_failed(&e);
                    return Ok(pb::ExecuteBatchDmlResponse {
                        result_sets,
                        status: Some(rpc_status(&e)),
                        precommit_token: None,
                    });
                }
            }
        }
        Ok(pb::ExecuteBatchDmlResponse {
            result_sets,
            status: Some(spanbolt_proto::google::rpc::Status::default()),
            precommit_token: None,
        })
    }

    /// The physical plan of a query, one operator per line (for tests and debugging).
    pub fn explain(&self, state: &DbState, req: &pb::ExecuteSqlRequest) -> Result<String> {
        let cat = self.catalog(state)?;
        let st = Stmt {
            sql: &req.sql,
            params: req.params.as_ref(),
            param_types: &req.param_types,
        };
        let p = self.prepare(&cat, &st)?;
        match &p.body {
            Body::Query { op, .. } => Ok(crate::plan::explain(op.as_ref(), &p.subplans)),
            Body::Dml(_) => Ok("DML".into()),
        }
    }

    /// Validates the query and returns it as a single partition.
    pub fn partition_query(
        &self,
        db: &Arc<Database>,
        req: &pb::PartitionQueryRequest,
    ) -> Result<Vec<pb::Partition>> {
        let state = db.latest().state.clone();
        let cat = self.catalog(&state)?;
        let st = Stmt {
            sql: &req.sql,
            params: req.params.as_ref(),
            param_types: &req.param_types,
        };
        let p = self.prepare(&cat, &st)?;
        if let Body::Dml(_) = p.body {
            return Err(Error::invalid_argument(
                "PartitionQuery only supports queries, not DML statements.",
            ));
        }
        p.search.check_partitioned()?;
        Ok(vec![pb::Partition {
            partition_token: b"spanbolt:query".to_vec(),
        }])
    }

    /// Entry counts of the process's SQL caches, for soak tests.
    pub fn cache_sizes(&self) -> Vec<(&'static str, usize)> {
        let (catalogs, plans) = self.cache.sizes();
        let (postings, queries) = crate::search::cache_sizes();
        vec![
            ("catalogs", catalogs),
            ("plans", plans),
            ("gsql_tables", self.cache.table_count()),
            ("search_postings", postings),
            ("search_queries", queries),
        ]
    }

    /// Analysis-cache counters, summed over live catalogs.
    pub fn cache_stats(&self, state: &DbState) -> (u64, u64) {
        match self.catalog(state) {
            Ok(c) => (
                c.hits.load(std::sync::atomic::Ordering::Relaxed),
                c.misses.load(std::sync::atomic::Ordering::Relaxed),
            ),
            Err(_) => (0, 0),
        }
    }
}

pub fn rpc_status(e: &Error) -> spanbolt_proto::google::rpc::Status {
    let code = match e.code {
        Code::InvalidArgument => 3,
        Code::NotFound => 5,
        Code::AlreadyExists => 6,
        Code::FailedPrecondition => 9,
        Code::OutOfRange => 11,
        Code::Aborted => 10,
        Code::Unimplemented => 12,
        Code::Internal => 13,
        Code::DeadlineExceeded => 4,
        Code::ResourceExhausted => 8,
    };
    spanbolt_proto::google::rpc::Status {
        code,
        message: e.message.clone(),
        details: Vec::new(),
    }
}

pub fn metadata(fields: &[(String, Type)]) -> pb::ResultSetMetadata {
    pb::ResultSetMetadata {
        row_type: Some(pb::StructType {
            fields: fields
                .iter()
                .map(|(n, t)| pb::struct_type::Field {
                    name: n.clone(),
                    r#type: Some(types::spanner_type(t)),
                })
                .collect(),
        }),
        ..Default::default()
    }
}

pub fn stats(row_count: Option<pb::result_set_stats::RowCount>) -> pb::ResultSetStats {
    pb::ResultSetStats {
        query_plan: Some(pb::QueryPlan::default()),
        query_stats: Some(prost_types::Struct::default()),
        row_count,
    }
}

/// An untyped parameter: the client sent a JSON value and no type; coerce it to the type the
/// analyser inferred.
fn untyped(v: &prost_types::Value, ty: &Type) -> Result<Value> {
    match &v.kind {
        None | Some(Kind::NullValue(_)) => Ok(Value::Null),
        Some(Kind::StringValue(s)) => crate::cast::cast(
            &Value::String(Arc::from(s.as_str())),
            &Type::String(None),
            ty,
        ),
        _ => Value::from_proto(v, ty).map_err(Error::invalid_argument),
    }
}

/// Whether the statement starts with INSERT, UPDATE or DELETE (after comments and hints).
fn looks_like_dml(sql: &str) -> bool {
    let mut rest = sql.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("--").or_else(|| rest.strip_prefix('#')) {
            rest = r.split_once('\n').map_or("", |(_, t)| t).trim_start();
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, t)| t).trim_start();
        } else if let Some(r) = rest.strip_prefix("@{") {
            rest = r.split_once('}').map_or("", |(_, t)| t).trim_start();
        } else {
            break;
        }
    }
    let word: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(word.as_str(), "INSERT" | "UPDATE" | "DELETE")
}

/// Parameter decode errors; INT64 is worded differently from mutations.
fn param_error(v: &prost_types::Value, ty: &Type, msg: String) -> Error {
    match (ty, &v.kind) {
        (Type::Int64, Some(Kind::StringValue(s))) => {
            Error::failed_precondition(format!("Could not parse {s} as an integer"))
        }
        _ => Error::failed_precondition(msg),
    }
}

/// Spanner cannot return a STRUCT as a result column value (ARRAY<STRUCT> is fine). The emulator and
/// production reject the shape before execution, also for `THEN RETURN` and value tables.
fn check_no_struct_columns(fields: &[(String, Type)]) -> Result<()> {
    if fields.iter().any(|(_, t)| matches!(t, Type::Struct(_))) {
        return Err(Error::new(
            Code::Unimplemented,
            "Unsupported query shape: A struct value cannot be returned as a column value. \
             Rewrite the query to flatten the struct fields in the result.",
        ));
    }
    Ok(())
}

/// Moves a trailing `[at 1:C]` position right by `by` columns; other lines keep theirs.
fn shift_first_line(msg: &str, by: usize) -> String {
    if let Some(i) = msg.rfind("[at 1:")
        && let Some(end) = msg[i..].find(']')
        && let Ok(col) = msg[i + 6..i + end].parse::<usize>()
    {
        return format!("{}[at 1:{}]{}", &msg[..i], col + by, &msg[i + end + 1..]);
    }
    msg.to_string()
}

/// Whether an element table of `g` over `table` reads `column`: as a key, an edge's source or
/// destination column, a node column an edge references, or in a property definition.
fn graph_reads_column(g: &crate::graph::GraphDef, table: &str, column: &str) -> bool {
    let over = |e: &crate::graph::ElementTable| {
        matches!(e.input, crate::graph::ElementInput::Table(_))
            && e.table.eq_ignore_ascii_case(table)
    };
    let named = |e: &crate::graph::ElementTable, cols: &[usize]| {
        cols.iter().any(|&i| {
            e.columns
                .get(i)
                .is_some_and(|c| c.eq_ignore_ascii_case(column))
        })
    };
    let elements = g.nodes.iter().chain(&g.edges);
    for e in elements.filter(|e| over(e)) {
        let refs = [&e.source, &e.destination];
        if named(e, &e.key)
            || refs
                .iter()
                .flat_map(|r| r.iter())
                .any(|r| named(e, &r.edge_columns))
            || e.properties.iter().any(|p| {
                spanbolt_storage::expr::column_references(&p.sql)
                    .iter()
                    .any(|w| w.eq_ignore_ascii_case(column))
            })
        {
            return true;
        }
    }
    // Node columns edges reference.
    g.edges.iter().any(|e| {
        [&e.source, &e.destination]
            .into_iter()
            .flatten()
            .any(|r| over(&g.nodes[r.node]) && named(&g.nodes[r.node], &r.node_columns))
    })
}
