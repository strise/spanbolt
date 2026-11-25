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
    let bytes = cat.gsql.analyze_raw(sql, params).map_err(|e| {
        let e = unsupported(cat, map_error(e));
        if cat.emulator_compat {
            e
        } else {
            prod_analyzer_error(prod_proto_names(e))
        }
    })?;
    googlesql_sys::with_stack_for(sql.len(), || {
        crate::limits::check(&bytes, cat.emulator_compat)
    })?;
    AnyResolvedStatementProto::decode(bytes.as_slice())
        .map_err(|e| Error::internal(format!("decode AnyResolvedStatementProto: {e}")))
}

/// Production names PROTO_BUNDLE's type `proto2.FileDescriptorSet` in errors too
/// (`No matching signature for function LENGTH ... Argument types: proto2.FileDescriptorSet`).
fn prod_proto_names(mut e: Error) -> Error {
    let name = crate::types::FILE_DESCRIPTOR_SET;
    if e.message.contains(name) {
        e.message = e
            .message
            .replace(name, &crate::types::production_proto_name(name));
    }
    e
}

/// Production Spanner passes the analyser's located errors (`... [at 1:8]`) through absl's
/// `CEscape`: quotes and backslashes escaped, newlines as `\n`, bytes outside printable ASCII as
/// octal (`Unexpected identifier \"SELEC\"`, `No matching signature for function LENGTH\n
/// Argument types: ...`). A syntax error also accepts a `;` where a statement may end.
fn prod_analyzer_error(mut e: Error) -> Error {
    if e.code == Code::Unimplemented {
        return e;
    }
    if let Some(rest) = e.message.strip_prefix("Syntax error: ") {
        let rest = rest.replacen("Expected end of input", "Expected \";\" or end of input", 1);
        e.message = format!("Syntax error: {rest}");
    } else if !has_location(&e.message) {
        return e;
    }
    e.message = spanbolt_storage::wire::c_escape_bytes(e.message.as_bytes());
    e
}

/// True if the message carries the analyser's error location, `[at LINE:COLUMN]`.
fn has_location(message: &str) -> bool {
    message.match_indices("[at ").any(|(i, _)| {
        let rest = &message[i + 4..];
        let Some((pos, _)) = rest.split_once(']') else {
            return false;
        };
        pos.split_once(':').is_some_and(|(line, col)| {
            !line.is_empty()
                && !col.is_empty()
                && line.bytes().all(|b| b.is_ascii_digit())
                && col.bytes().all(|b| b.is_ascii_digit())
        })
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn located_errors_are_c_escaped_in_production() {
        let e = prod_analyzer_error(Error::invalid_argument(
            "No matching signature for function F\n  Argument types: 'é' [at 1:8]",
        ));
        assert_eq!(
            e.message,
            "No matching signature for function F\\n  Argument types: \\'\\303\\251\\' [at 1:8]"
        );
        let e = prod_analyzer_error(Error::invalid_argument(
            "Syntax error: Expected end of input but got \"\\\" [at 1:10]",
        ));
        assert_eq!(
            e.message,
            "Syntax error: Expected \\\";\\\" or end of input but got \\\"\\\\\\\" [at 1:10]"
        );
    }

    #[test]
    fn unlocated_errors_are_left_alone() {
        let m = "SEARCH function's first argument must be a TOKENLIST";
        assert_eq!(prod_analyzer_error(Error::invalid_argument(m)).message, m);
        let m = "it's [at x:1]";
        assert_eq!(prod_analyzer_error(Error::invalid_argument(m)).message, m);
    }
}
