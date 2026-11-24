//! Spanner GoogleSQL DDL: parser, schema model and canonical printer.

mod apply;
mod lexer;
pub mod names;
mod parser;
mod printer;
mod retention;
mod schema;

pub use apply::DataCheck;
pub use lexer::{column_references, identifiers, is_reserved};
pub use parser::{
    ChangeStream, CsTarget, is_ddl, null_filter_columns, parse_statement,
    parse_statement_emulator_compat, quote_ident, quote_path, quote_string,
};
pub use printer::print_statement;
pub use retention::{DEFAULT_VERSION_RETENTION, VERSION_RETENTION_PERIOD};
pub use schema::*;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum DdlError {
    /// Maps to gRPC INVALID_ARGUMENT.
    #[error("{0}")]
    Syntax(String),
    /// Maps to gRPC FAILED_PRECONDITION (e.g. dropping a table that has indexes).
    #[error("{0}")]
    Precondition(String),
    /// Maps to gRPC NOT_FOUND.
    #[error("{0}")]
    NotFound(String),
    /// Maps to gRPC ALREADY_EXISTS (Spanner reports duplicates as FAILED_PRECONDITION; keep the message).
    #[error("{0}")]
    AlreadyExists(String),
    /// Maps to gRPC UNIMPLEMENTED (a statement Spanner parses but doesn't support).
    #[error("{0}")]
    Unimplemented(String),
}

/// Splits a script into statements on top-level `;`, skipping comments and empty statements.
pub fn split_statements(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'#' || (c == b'-' && b.get(i + 1) == Some(&b'-')) {
            cur.push_str(&src[last..i]);
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            last = i;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            cur.push_str(&src[last..i]);
            i = src[i + 2..].find("*/").map_or(b.len(), |e| i + 4 + e);
            last = i;
            continue;
        }
        if c == b'\'' || c == b'"' || c == b'`' {
            let triple = c != b'`' && b.get(i + 1) == Some(&c) && b.get(i + 2) == Some(&c);
            i += if triple { 3 } else { 1 };
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == c && (!triple || (b.get(i + 1) == Some(&c) && b.get(i + 2) == Some(&c)))
                {
                    i += if triple { 3 } else { 1 };
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == b';' {
            cur.push_str(&src[last..i]);
            let s = cur.trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
            cur.clear();
            i += 1;
            last = i;
            continue;
        }
        i += 1;
    }
    cur.push_str(&src[last.min(src.len())..]);
    let s = cur.trim();
    if !s.is_empty() {
        out.push(s.to_string());
    }
    out
}
