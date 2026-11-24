//! Evaluates schema expressions (DEFAULT, stored generated columns, CHECK) for storage, through
//! the same analyser and executor as queries: `SELECT CAST((expr) AS T) FROM UNNEST(@__row)`
//! with the row bound as a one-element array of STRUCT, so column names resolve to its fields.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use spanbolt_ddl::ColumnType;
use spanbolt_storage::expr::{ExprEvaluator, ExprKind};
use spanbolt_storage::{DbState, Error, Result, TableMeta, Value};

use crate::catalog::SqlCatalog;
use crate::engine::Engine;
use crate::exec::{Cx, Env};
use crate::prepared::Body;
use crate::types;

pub struct SchemaExprs {
    engine: Engine,
    state: DbState,
    cat: Arc<SqlCatalog>,
}

impl SchemaExprs {
    pub fn new() -> Result<SchemaExprs> {
        SchemaExprs::with_options(crate::Options::default())
    }

    /// With the server's options, so generated TOKENLIST columns follow `--emulator-compat`.
    pub fn with_options(opts: crate::Options) -> Result<SchemaExprs> {
        let engine = Engine::new(opts);
        let state = DbState::default();
        let cat = engine.catalog(&state)?;
        Ok(SchemaExprs { engine, state, cat })
    }
}

fn row_param(table: &TableMeta) -> String {
    let fields: Vec<(Option<String>, ColumnType)> = table
        .columns
        .iter()
        .map(|c| (Some(c.name.clone()), c.ty.clone()))
        .collect();
    let row_type = types::gsql_name(&ColumnType::Array(Box::new(ColumnType::Struct(fields))));
    serde_json::json!({ "__row": row_type }).to_string()
}

/// Shifts `[at 1:N]` locations in an analyser message by `-offset` columns.
fn relocate(msg: &str, offset: usize) -> String {
    let Some(i) = msg.rfind("[at 1:") else {
        return msg.to_string();
    };
    let rest = &msg[i + 6..];
    let Some(end) = rest.find(']') else {
        return msg.to_string();
    };
    match rest[..end].parse::<usize>() {
        Ok(col) => format!(
            "{}[at 1:{}]{}",
            &msg[..i],
            col.saturating_sub(offset).max(1),
            &rest[end + 1..]
        ),
        Err(_) => msg.to_string(),
    }
}

/// Types a DEFAULT value may have for a column of type `col` (literal coercions).
fn assignable(found: &ColumnType, col: &ColumnType) -> bool {
    types::same(found, col)
        || matches!(
            (found, col),
            (
                ColumnType::Int64,
                ColumnType::Float64 | ColumnType::Float32 | ColumnType::Numeric
            ) | (ColumnType::Float64, ColumnType::Float32)
        )
}

impl ExprEvaluator for SchemaExprs {
    fn validate_property_graph(
        &self,
        schema: &Arc<spanbolt_ddl::Schema>,
        name: &str,
    ) -> Result<()> {
        self.engine.validate_property_graph(schema, name)
    }

    fn validate(&self, expr: &str, table: &TableMeta, kind: &ExprKind) -> Result<()> {
        // Generated columns are analysed with their parentheses, as the emulator reports them.
        let text = match kind {
            ExprKind::Generated { .. } => format!("({expr})"),
            _ => expr.to_string(),
        };
        const PREFIX: &str = "SELECT ";
        let sql = format!("{PREFIX}{text} FROM UNNEST(@__row)");
        let wrap = |msg: &str| {
            let msg = relocate(msg, PREFIX.len());
            let t = &table.name;
            Error::invalid_argument(match kind {
                ExprKind::Check { name } => format!(
                    "Error parsing expression '{expr}' from check constraint '{name}' in table '{t}': {msg}"
                ),
                ExprKind::Generated { column } => format!(
                    "Error parsing the definition of generated column `{t}`.`{column}`: {msg}"
                ),
                ExprKind::Default { column, .. } => {
                    format!("Error parsing the default value of column `{t}`.`{column}`: {msg}")
                }
            })
        };
        let stmt = match crate::analyze::statement(&self.cat, &sql, &row_param(table)) {
            Ok(s) => s,
            Err(e) => return Err(wrap(&e.message)),
        };
        // PENDING_COMMIT_TIMESTAMP() analyses anywhere; as a DEFAULT it must be the whole value.
        if let ExprKind::Default { column, .. } = kind
            && !spanbolt_ddl::is_pending_commit_timestamp(expr)
            && spanbolt_storage::expr::mentions(expr, "pending_commit_timestamp")
        {
            let msg = crate::plan::PENDING_COMMIT_TIMESTAMP_USE;
            return Err(if self.engine.opts.emulator_compat {
                wrap(msg)
            } else {
                Error::invalid_argument(format!(
                    "Error parsing default value expression '{expr}' from column '{column}' in table '{}': {msg}",
                    table.name
                ))
            });
        }
        if let ExprKind::Default { ty, .. } = kind
            && !expr.trim().eq_ignore_ascii_case("NULL")
        {
            use googlesql_sys::proto::any_resolved_statement_proto::Node as StmtNode;
            let found = match &stmt.node {
                Some(StmtNode::ResolvedQueryStmtNode(q)) => q
                    .output_column_list
                    .first()
                    .and_then(|o| o.column.as_ref())
                    .and_then(|c| c.r#type.as_ref())
                    .and_then(|t| types::from_proto(t).ok()),
                _ => None,
            };
            if let Some(found) = found
                && !assignable(&found, ty)
            {
                return Err(wrap(&format!(
                    "Expected type {}; found {} [at 1:1]",
                    types::spanner_name(ty),
                    types::spanner_name(&found)
                )));
            }
        }
        Ok(())
    }

    fn eval(&self, expr: &str, table: &TableMeta, row: &[Value], ty: &ColumnType) -> Result<Value> {
        let params = row_param(table);
        let sql = format!(
            "SELECT CAST(({expr}) AS {}) FROM UNNEST(@__row)",
            types::gsql_name(ty)
        );
        let p = self.engine.prepare_sql(&self.cat, &sql, params)?;
        let Body::Query { op, positions } = &p.body else {
            return Err(Error::internal("schema expression is not a query"));
        };
        let arg = Value::Array(Arc::from(vec![Value::Struct(Arc::from(row.to_vec()))]));
        let params = vec![arg];
        let cx = Cx {
            state: &self.state,
            cat: &self.cat,
            params: &params,
            now: spanbolt_storage::Timestamp::from_nanos(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos() as i128),
            ),
            rw: None,
            with: RefCell::new(Vec::new()),
            memo: RefCell::new(HashMap::new()),
        };
        let mut it = op.open(&cx, &Env::EMPTY)?;
        let first = it.next().transpose()?;
        Ok(first
            .and_then(|r| positions.first().and_then(|&i| r.get(i).cloned()))
            .unwrap_or(Value::Null))
    }
}
