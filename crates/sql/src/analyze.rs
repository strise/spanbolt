//! Calls the GoogleSQL analyser and maps its errors to Spanner's status codes.

use googlesql_sys::proto::any_resolved_statement_proto::Node as StmtNode;
use googlesql_sys::proto::{AnyResolvedStatementProto, ResolvedQueryStmtProto};
use prost::Message;
use spanbolt_storage::{Code, Error, Result};

use crate::catalog::SqlCatalog;

pub(crate) fn map_error(e: googlesql_sys::AnalyzeError) -> Error {
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
    // Spanner and the emulator suggest no table for a table that isn't found.
    if e.message.starts_with("Table not found: ")
        && let Some(at) = e.message.find("; Did you mean ")
        && let Some(q) = e.message[at..].find('?')
    {
        let mut message = e.message.clone();
        message.replace_range(at..at + q + 1, "");
        return Error::new(code, message);
    }
    Error::new(code, e.message)
}

/// Analyses one statement and checks it against Spanner's query size limits.
pub fn statement(cat: &SqlCatalog, sql: &str, params: &str) -> Result<AnyResolvedStatementProto> {
    crate::limits::check_query_string(sql)?;
    let bytes = cat
        .gsql
        .analyze_raw(sql, params)
        .map_err(|e| unsupported(cat, map_error(e)))?;
    crate::limits::check(&bytes, cat.emulator_compat)?;
    AnyResolvedStatementProto::decode(bytes.as_slice())
        .map_err(|e| Error::internal(format!("decode AnyResolvedStatementProto: {e}")))
}

/// Spanner SQL the catalog doesn't register, so the analyser reports it as missing: UNIMPLEMENTED
/// instead of the analyser's INVALID_ARGUMENT, because it is valid on Spanner.
fn unsupported(cat: &SqlCatalog, e: Error) -> Error {
    if e.code != Code::InvalidArgument {
        return e;
    }
    // Sequences are stored, but their counters aren't modelled.
    if let Some(rest) = e.message.strip_prefix("Function not found: ") {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let name = name.to_ascii_uppercase();
        if name == "GET_NEXT_SEQUENCE_VALUE" || name == "GET_INTERNAL_SEQUENCE_STATE" {
            return crate::types::unimplemented(name);
        }
    }
    // A change stream's READ_<stream> table-valued function.
    if let Some(rest) = e.message.strip_prefix("Table-valued function not found: ")
        && let Some(tvf) = rest.split([' ', ';']).next()
        && tvf
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case("READ_"))
        && let stream = &tvf[5..]
        && cat.schema.opaque.iter().any(|o| {
            o.kind.eq_ignore_ascii_case("CHANGE STREAM") && o.name.eq_ignore_ascii_case(stream)
        })
    {
        return crate::types::unimplemented("Reading a change stream");
    }
    e
}

pub fn view(gsql: &googlesql_sys::Catalog, sql: &str) -> Result<ResolvedQueryStmtProto> {
    let stmt = gsql.analyze(sql, "{}").map_err(map_error)?;
    match stmt.node {
        Some(StmtNode::ResolvedQueryStmtNode(q)) => Ok(q),
        _ => Err(Error::invalid_argument("View definition is not a query.")),
    }
}
