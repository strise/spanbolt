//! What `CREATE VIEW` checks of a view's query, mapped from the emulator and Cloud Spanner
//! (`conformance/cases/ddl/view_definitions.case`): the query analyses against the other objects,
//! has no query parameters and no WITH clause, and names each column once. Both analyse it in
//! strict name resolution mode (`conformance/cases/ddl/view_names.case`): column names need a
//! qualifier, and `SELECT *` is rejected. Cloud Spanner also finds a table only by its declared
//! spelling (views, synonyms and columns ignore case); the emulator ignores case everywhere.

use googlesql_sys::proto::any_resolved_scan_proto::Node as S;
use spanbolt_storage::{Error, Result};

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
    let exact = if strict_case {
        serde_json::to_string(&cat.table_names()).unwrap_or_default()
    } else {
        String::new()
    };
    let stmt = cat.gsql.analyze_view(body, &exact).map_err(|e| {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_drops_leading_comments() {
        assert_eq!(body("  /* c */ -- x\n# y\n SELECT 1"), "SELECT 1");
        assert_eq!(body("SELECT 1 /* c */"), "SELECT 1 /* c */");
        assert_eq!(body("/* open"), "/* open");
    }
}
