//! Spanner-specific checks the planner applies to the nodes it lowers: the supported-function
//! allowlist and hint names (ported from cloud-spanner-emulator `query_validator.cc`, Apache-2.0).

use std::collections::HashSet;
use std::sync::LazyLock;

use googlesql_sys::proto::ResolvedOptionProto;
use spanbolt_storage::{Code, Error, Result};

static SUPPORTED: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    include_str!("supported_functions.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .collect()
});

/// Rejects GoogleSQL built-ins that Spanner does not support (BigQuery-only functions).
pub fn function(full_name: &str) -> Result<()> {
    let (group, name) = full_name
        .rsplit_once(':')
        .unwrap_or(("GoogleSQL", full_name));
    if group.eq_ignore_ascii_case("GoogleSQL") && !SUPPORTED.contains(name) {
        return Err(Error::new(
            Code::Unimplemented,
            format!(
                "Unsupported built-in function: {}",
                name.trim_start_matches('$').to_uppercase()
            ),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Node {
    TableScan,
    JoinScan,
    AggregateScan,
    ArrayScan,
    Insert,
    Update,
    Delete,
    Query,
    Subquery,
    SetOperation,
    FunctionCall,
}

fn spanner_hints(node: Node) -> &'static [&'static str] {
    match node {
        Node::TableScan => &[
            "force_index",
            "groupby_scan_optimization",
            "index_strategy",
            "scan_method",
        ],
        Node::JoinScan => &[
            "join_type",
            "join_method",
            "hash_join_build_side",
            "force_join_order",
            "hash_join_execution",
        ],
        Node::AggregateScan => &["group_type", "group_method"],
        Node::ArrayScan => &[
            "join_type",
            "join_method",
            "hash_join_build_side",
            "batch_mode",
            "force_join_order",
        ],
        Node::Insert | Node::Delete => &["lock_scanned_ranges"],
        Node::Update => &["lock_scanned_ranges", "pdml_max_parallelism"],
        Node::Query => &[
            "force_index",
            "index_strategy",
            "join_type",
            "join_method",
            "hash_join_build_side",
            "force_join_order",
            "constant_folding",
            "use_additional_parallelism",
            "enable_adaptive_plans",
            "lock_scanned_ranges",
            "parameter_sensitive",
            "hash_join_execution",
            "allow_search_indexes_in_transaction",
            "require_enhance_query",
            "enhance_query_timeout_ms",
            "scan_method",
            // Accepted by production Spanner (the emulator rejects them).
            "optimizer_version",
            "optimizer_statistics_package",
        ],
        Node::Subquery => &[
            "join_type",
            "join_method",
            "hash_join_build_side",
            "batch_mode",
            "force_join_order",
            "hash_join_execution",
        ],
        Node::SetOperation => &["join_method", "force_join_order"],
        Node::FunctionCall => &["disable_inline"],
    }
}

fn emulator_hints(node: Node) -> &'static [&'static str] {
    match node {
        Node::TableScan => &["disable_query_null_filtered_index_check"],
        Node::Query => &[
            "disable_query_partitionability_check",
            "disable_query_null_filtered_index_check",
            "ignore_unknown_hints",
        ],
        Node::Insert | Node::Update | Node::Delete => &["ignore_unknown_hints"],
        _ => &[],
    }
}

/// Checks the hint names on one node. Hints for other engines are ignored.
pub fn hints(node: Node, list: &[ResolvedOptionProto]) -> Result<()> {
    for h in list {
        let name = h.name.as_deref().unwrap_or("");
        let q = h.qualifier.as_deref().unwrap_or("");
        let allowed = if q.is_empty() || q.eq_ignore_ascii_case("spanner") {
            spanner_hints(node)
        } else if q.eq_ignore_ascii_case("spanner_emulator") {
            emulator_hints(node)
        } else {
            continue;
        };
        if !allowed.iter().any(|a| a.eq_ignore_ascii_case(name)) {
            return Err(Error::invalid_argument(format!(
                "Unsupported hint: {name}."
            )));
        }
    }
    Ok(())
}
