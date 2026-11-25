//! What `CREATE VIEW` checks of a view's query, mapped from the emulator and Cloud Spanner
//! (`conformance/cases/ddl/view_definitions.case`): the query analyses against the other objects,
//! has no query parameters and no WITH clause, and names each column once. Cloud Spanner resolves
//! table names in a view case-sensitively (views and columns are not); the emulator ignores case.
//!
//! Both analyse the view in strict name resolution mode, which the analyser shim doesn't expose:
//! `SELECT *` and `t.*` are checked here, unqualified column names are accepted.

use googlesql_sys::proto::any_resolved_scan_proto::Node as S;
use spanbolt_ddl::{SqlToken, sql_tokens};
use spanbolt_storage::{Error, Result};

/// The absl (and gRPC) status code.
const UNIMPLEMENTED: i32 = 12;

use crate::catalog::SqlCatalog;

/// The view's query without the leading comments and whitespace, which Spanner drops.
pub fn body(sql: &str) -> &str {
    let mut rest = sql.trim_start();
    loop {
        if rest.starts_with("--") || rest.starts_with('#') {
            rest = rest.find('\n').map_or("", |i| &rest[i + 1..]).trim_start();
        } else if let Some(r) = rest.strip_prefix("/*")
            && let Some(end) = r.find("*/")
        {
            rest = r[end + 2..].trim_start();
        } else {
            return rest;
        }
    }
}

/// Checks the query `body` of a view against `cat`, which holds every object but the view. The
/// error message is the reason, with its `[at L:C]` position in `body`.
pub fn check(cat: &SqlCatalog, body: &str, strict_case: bool) -> Result<()> {
    let toks = sql_tokens(body);
    if strict_case && let Some(toks) = &toks {
        wrong_case_table(cat, body, toks)?;
    }
    let stmt = cat.gsql.analyze(body, "{}").map_err(|e| {
        let at = e.message.rfind(" [at ").map_or("", |i| &e.message[i..]);
        if e.message.starts_with("Query parameter '") {
            Error::invalid_argument(format!(
                "Query parameters cannot be used inside SQL view bodies{at}"
            ))
        } else {
            let mut e = crate::analyze::map_error(e);
            // The emulator suggests no function either.
            if e.message.starts_with("Function not found: ")
                && let Some(at) = e.message.find("; Did you mean ")
                && let Some(q) = e.message[at..].find('?')
            {
                e.message.replace_range(at..at + q + 1, "");
            }
            e
        }
    })?;
    use googlesql_sys::proto::any_resolved_statement_proto::Node as StmtNode;
    let Some(StmtNode::ResolvedQueryStmtNode(q)) = stmt.node else {
        return Err(Error::invalid_argument("View definition is not a query."));
    };
    if matches!(
        q.query.as_ref().and_then(|s| s.node.as_ref()),
        Some(S::ResolvedWithScanNode(_))
    ) {
        return Err(Error::invalid_argument(
            "WITH clauses are unsupported in view definitions.",
        ));
    }
    if let Some(toks) = &toks {
        stars(body, toks)?;
    }
    let mut seen: Vec<String> = Vec::new();
    for (i, oc) in q.output_column_list.iter().enumerate() {
        let name = oc.name.as_deref().unwrap_or_default();
        if name.is_empty() || name.starts_with('$') {
            return Err(Error::invalid_argument(format!(
                "CREATE VIEW columns must be named, but column {} has no name [at 1:1]",
                i + 1
            )));
        }
        if seen.iter().any(|s| s.eq_ignore_ascii_case(name)) {
            return Err(Error::invalid_argument(format!(
                "CREATE VIEW has columns with duplicate name {name} [at 1:1]"
            )));
        }
        seen.push(name.to_string());
    }
    Ok(())
}

/// `[at L:C]` of byte offset `at` in `text`.
fn position(text: &str, at: usize) -> String {
    let before = &text[..at.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before
        .rfind('\n')
        .map_or(before, |i| &before[i + 1..])
        .chars()
        .count()
        + 1;
    format!("[at {line}:{col}]")
}

fn ident(t: &SqlToken) -> Option<&str> {
    match t {
        SqlToken::Ident { text, .. } => Some(text),
        _ => None,
    }
}

/// `SELECT *` and `t.*`, which strict name resolution rejects.
fn stars(body: &str, toks: &[(SqlToken, usize)]) -> Result<()> {
    for (i, (t, at)) in toks.iter().enumerate() {
        if *t != SqlToken::Sym("*") || i == 0 {
            continue;
        }
        let prev = &toks[i - 1].0;
        if ident(prev).is_some_and(|w| {
            ["SELECT", "DISTINCT", "ALL"]
                .iter()
                .any(|k| k.eq_ignore_ascii_case(w))
        }) && !matches!(prev, SqlToken::Ident { quoted: true, .. })
        {
            return Err(Error::invalid_argument(format!(
                "SELECT * is not allowed in strict name resolution mode {}",
                position(body, *at)
            )));
        }
        if *prev == SqlToken::Sym(".") {
            // The position of the path the star expands.
            let mut start = i - 1;
            while start >= 1 && ident(&toks[start - 1].0).is_some() {
                start -= 1;
                if start >= 1 && toks[start - 1].0 == SqlToken::Sym(".") {
                    start -= 1;
                } else {
                    break;
                }
            }
            return Err(Error::invalid_argument(format!(
                "Dot-star is not allowed in strict name resolution mode {}",
                position(body, toks[start].1)
            )));
        }
    }
    Ok(())
}

/// Cloud Spanner finds a table named in a view only by its declared spelling. When the query
/// spells a table name with other case, it is analysed without those tables, which is what Spanner
/// sees as long as no identifier spells them as declared (one could be a reference Spanner finds):
/// then only a `Table not found` for another spelling is taken as Spanner's answer.
fn wrong_case_table(cat: &SqlCatalog, body: &str, toks: &[(SqlToken, usize)]) -> Result<()> {
    // Identifier paths of one or two parts, as written.
    let mut paths: Vec<String> = Vec::new();
    for (i, (t, _)) in toks.iter().enumerate() {
        let Some(w) = ident(t) else { continue };
        paths.push(w.to_string());
        if i + 2 < toks.len()
            && toks[i + 1].0 == SqlToken::Sym(".")
            && let Some(w2) = ident(&toks[i + 2].0)
        {
            paths.push(format!("{w}.{w2}"));
        }
    }
    let mut wrong: Vec<String> = Vec::new();
    let mut exact = false;
    for name in cat.table_names() {
        let spelled: Vec<&String> = paths
            .iter()
            .filter(|p| p.eq_ignore_ascii_case(&name))
            .collect();
        if spelled.iter().any(|p| **p != name) {
            exact |= spelled.iter().any(|p| **p == name);
            wrong.push(name);
        }
    }
    if wrong.is_empty() {
        return Ok(());
    }
    let without: Vec<String> = wrong.iter().map(|n| n.to_ascii_lowercase()).collect();
    let gsql = cat.gsql_without(&without)?;
    match gsql.analyze_raw(body, "{}") {
        Err(e) => {
            let missing = e
                .message
                .strip_prefix("Table not found: ")
                .and_then(|r| r.split([' ', ';']).next())
                .unwrap_or_default();
            let other_spelling = wrong
                .iter()
                .any(|n| n.eq_ignore_ascii_case(missing) && n != missing);
            if other_spelling || (!exact && e.code != UNIMPLEMENTED) {
                return Err(crate::analyze::map_error(e));
            }
            Ok(())
        }
        Ok(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_drops_leading_comments() {
        assert_eq!(body("  /* c */ -- x\n# y\n SELECT 1"), "SELECT 1");
        assert_eq!(body("SELECT 1 /* c */"), "SELECT 1 /* c */");
        assert_eq!(body("/* open"), "/* open");
    }

    #[test]
    fn positions_count_lines_and_columns() {
        assert_eq!(position("SELECT a\nFROM t", 14), "[at 2:6]");
        assert_eq!(position("SELECT *", 7), "[at 1:8]");
    }

    #[test]
    fn stars_follow_strict_mode() {
        let check = |sql: &str| stars(sql, &sql_tokens(sql).unwrap()).map_err(|e| e.message);
        assert_eq!(
            check("SELECT * FROM T"),
            Err("SELECT * is not allowed in strict name resolution mode [at 1:8]".into())
        );
        assert_eq!(
            check("SELECT T.* FROM T"),
            Err("Dot-star is not allowed in strict name resolution mode [at 1:8]".into())
        );
        assert!(check("SELECT COUNT(*) AS n, T.a * 2 AS b FROM T").is_ok());
    }
}
