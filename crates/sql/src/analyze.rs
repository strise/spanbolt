//! Calls the GoogleSQL analyser and maps its errors to Spanner's status codes.

use googlesql_sys::proto::any_resolved_statement_proto::Node as StmtNode;
use googlesql_sys::proto::{AnyResolvedStatementProto, ResolvedQueryStmtProto};
use spanbolt_storage::{Code, Error, Result};

use crate::catalog::SqlCatalog;

fn map_error(e: googlesql_sys::AnalyzeError) -> Error {
    // absl status codes share gRPC numbering.
    let code = match e.code {
        5 => Code::NotFound,
        6 => Code::AlreadyExists,
        9 => Code::FailedPrecondition,
        11 => Code::OutOfRange,
        12 => Code::Unimplemented,
        13 => Code::Internal,
        _ => Code::InvalidArgument,
    };
    // The emulator words a missing parameter binding this way.
    if let Some(rest) = e.message.strip_prefix("Query parameter '")
        && let Some((name, tail)) = rest.split_once('\'')
        && tail.starts_with(" not found")
    {
        return Error::invalid_argument(format!("Incomplete query parameters {name}"));
    }
    Error::new(code, e.message)
}

pub fn statement(cat: &SqlCatalog, sql: &str, params: &str) -> Result<AnyResolvedStatementProto> {
    cat.gsql.analyze(sql, params).map_err(map_error)
}

pub fn view(gsql: &googlesql_sys::Catalog, sql: &str) -> Result<ResolvedQueryStmtProto> {
    let stmt = gsql.analyze(sql, "{}").map_err(map_error)?;
    match stmt.node {
        Some(StmtNode::ResolvedQueryStmtNode(q)) => Ok(q),
        _ => Err(Error::invalid_argument("View definition is not a query.")),
    }
}
