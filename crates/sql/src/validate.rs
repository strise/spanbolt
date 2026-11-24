//! Spanner-specific checks the planner applies to the nodes it lowers: the supported-function
//! allowlist and hint names (ported from cloud-spanner-emulator `query_validator.cc`, Apache-2.0).

use std::collections::HashSet;
use std::sync::LazyLock;

use googlesql_sys::proto::ResolvedOptionProto;
use spanbolt_storage::{Code, Error, Result, Value};

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

/// Hint names the emulator and Cloud Spanner both accept on a node.
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

/// Hint names Cloud Spanner accepts on top of `spanner_hints` (the emulator rejects them).
fn production_hints(node: Node) -> &'static [&'static str] {
    match node {
        Node::TableScan => &["seekable_key_size"],
        Node::Query => &[
            "optimizer_version",
            "optimizer_statistics_package",
            "allow_distributed_merge",
            "execution_method",
            "use_unenforced_foreign_key",
            "allow_timestamp_predicate_pushdown",
        ],
        Node::Insert | Node::Update => &["optimizer_version", "optimizer_statistics_package"],
        Node::Delete => &[
            "optimizer_version",
            "optimizer_statistics_package",
            "pdml_max_parallelism",
        ],
        _ => &[],
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

/// The newest query optimizer version Cloud Spanner runs.
const MAX_OPTIMIZER_VERSION: i64 = 9;

#[derive(Clone, Copy, PartialEq)]
enum Ty {
    Bool,
    Int64,
    String,
    /// Checked by the hint's own rule.
    Any,
}

fn hint_type(name: &str, emulator: bool) -> Ty {
    match name.to_ascii_lowercase().as_str() {
        "batch_mode" if emulator => Ty::Bool,
        "force_join_order"
        | "use_additional_parallelism"
        | "constant_folding"
        | "groupby_scan_optimization"
        | "enable_adaptive_plans"
        | "disable_inline"
        | "allow_search_indexes_in_transaction"
        | "require_enhance_query"
        | "allow_distributed_merge"
        | "use_unenforced_foreign_key"
        | "allow_timestamp_predicate_pushdown" => Ty::Bool,
        "enhance_query_timeout_ms" | "pdml_max_parallelism" | "seekable_key_size" => Ty::Int64,
        "batch_mode" | "optimizer_version" => Ty::Any,
        _ => Ty::String,
    }
}

/// How the hints of one statement are checked: by Cloud Spanner's rules, or by the emulator's
/// (`--emulator-compat`), where `@{spanner_emulator.ignore_unknown_hints=true}` on the statement
/// skips hint names neither knows.
#[derive(Clone, Copy)]
pub struct HintRules {
    emulator: bool,
    ignore_unknown: bool,
}

/// A hint's value as the emulator prints it (`Value::DebugString`).
fn debug_value(v: &Option<Value>) -> String {
    match v {
        None | Some(Value::Null) => "NULL".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Int64(i)) => i.to_string(),
        Some(Value::String(s)) => format!("{s:?}"),
        Some(_) => "?".into(),
    }
}

fn literal_type_name(v: &Option<Value>) -> &'static str {
    match v {
        Some(Value::Bool(_)) => "BOOL",
        Some(Value::String(_)) => "STRING",
        _ => "INT64",
    }
}

fn invalid(msg: String) -> Error {
    Error::invalid_argument(msg)
}

impl HintRules {
    pub fn new(emulator: bool) -> Self {
        HintRules {
            emulator,
            ignore_unknown: false,
        }
    }

    /// The rules for a statement whose own hints are `list`. In emulator mode this reads
    /// `spanner_emulator.ignore_unknown_hints`, which must be a BOOL literal.
    pub fn statement(emulator: bool, list: &[ResolvedOptionProto]) -> Result<Self> {
        let mut rules = HintRules::new(emulator);
        if !emulator {
            return Ok(rules);
        }
        for h in list {
            let name = h.name.as_deref().unwrap_or("");
            let q = h.qualifier.as_deref().unwrap_or("");
            if !(q.eq_ignore_ascii_case("spanner_emulator")
                && name.eq_ignore_ascii_case("ignore_unknown_hints"))
            {
                continue;
            }
            let v = h.value.as_ref().and_then(crate::plan::literal_of);
            match v {
                Some(Value::Bool(b)) => rules.ignore_unknown = b,
                _ => {
                    return Err(invalid(format!(
                        "Invalid hint value for emulator-only hint spanner_emulator.{name}: Literal(type={}, value={})\n.",
                        literal_type_name(&v),
                        debug_value(&v)
                    )));
                }
            }
        }
        Ok(rules)
    }

    /// Checks the hints on one node: names, repeats, then values. Hints for other engines are
    /// ignored; Cloud Spanner also ignores `spanner_emulator` hints.
    pub fn check(&self, node: Node, list: &[ResolvedOptionProto]) -> Result<()> {
        let mut seen: Vec<(&str, Option<Value>)> = Vec::new();
        let mut seen_emulator: Vec<(&str, Option<Value>)> = Vec::new();
        for h in list {
            let name = h.name.as_deref().unwrap_or("");
            let q = h.qualifier.as_deref().unwrap_or("");
            let has = |names: &[&str]| names.iter().any(|a| a.eq_ignore_ascii_case(name));
            let (known, unknown_msg, into) = if q.is_empty() || q.eq_ignore_ascii_case("spanner") {
                let known =
                    has(spanner_hints(node)) || (!self.emulator && has(production_hints(node)));
                (known, format!("Unsupported hint: {name}."), &mut seen)
            } else if q.eq_ignore_ascii_case("spanner_emulator") && self.emulator {
                (
                    has(emulator_hints(node)),
                    format!("Invalid emulator-only hint: {name}."),
                    &mut seen_emulator,
                )
            } else {
                continue;
            };
            if !known {
                if self.ignore_unknown {
                    continue;
                }
                return Err(invalid(unknown_msg));
            }
            // The emulator keys its hint map by the name as written; Spanner ignores case.
            let repeated = into.iter().any(|(n, _)| {
                if self.emulator {
                    *n == name
                } else {
                    n.eq_ignore_ascii_case(name)
                }
            });
            if repeated {
                return Err(invalid(if self.emulator {
                    format!("Multiple values for hint: {name}")
                } else {
                    format!(
                        "Providing the same hint multiple times is not supported: {}.",
                        name.to_ascii_lowercase()
                    )
                }));
            }
            into.push((name, h.value.as_ref().and_then(crate::plan::literal_of)));
        }
        for (name, v) in &seen {
            self.check_value(node, name, v, &seen)?;
        }
        for (name, v) in &seen_emulator {
            if name.to_ascii_lowercase().starts_with("disable_")
                && !matches!(v, Some(Value::Bool(_)))
            {
                return Err(invalid(format!(
                    "Invalid hint value for emulator-only hint {name}: {}.",
                    debug_value(v)
                )));
            }
        }
        Ok(())
    }

    fn check_value(
        &self,
        node: Node,
        name: &str,
        v: &Option<Value>,
        all: &[(&str, Option<Value>)],
    ) -> Result<()> {
        let lower = name.to_ascii_lowercase();
        let emulator_bad = || {
            invalid(format!(
                "Invalid hint value for: {name} hint: {}.",
                debug_value(v)
            ))
        };
        let type_ok = matches!(
            (hint_type(name, self.emulator), v),
            (Ty::Any, _)
                | (Ty::Bool, Some(Value::Bool(_)))
                | (Ty::Int64, Some(Value::Int64(_)))
                | (Ty::String, Some(Value::String(_)))
        );
        if !type_ok {
            return Err(if self.emulator {
                emulator_bad()
            } else {
                invalid(format!("Invalid hint value for: {lower}."))
            });
        }
        let s = match v {
            Some(Value::String(s)) => &**s,
            _ => "",
        };
        let one_of = |allowed: &[&str]| allowed.iter().any(|a| a.eq_ignore_ascii_case(s));
        // Cloud Spanner's message for a value outside the hint's set.
        let prod_bad: Option<String> = match lower.as_str() {
            "force_index" => {
                if node == Node::Query && !s.eq_ignore_ascii_case("_base_table") {
                    return Err(invalid(if self.emulator {
                        format!("Invalid statement-level hint value: {name}={}.", debug_value(v))
                    } else {
                        format!("Unsupported statement-level hint value: force_index={s}.")
                    }));
                }
                None
            }
            "join_method" | "join_type" => (!one_of(&[
                "apply_join",
                "hash_join",
                "merge_join",
                "push_broadcast_hash_join",
                "loop_join",
            ]))
            .then(|| format!("Invalid value for join_type/join_method hint: {s}.")),
            "parameter_sensitive" if self.emulator => {
                (!one_of(&["always", "auto", "never"])).then(String::new)
            }
            "hash_join_build_side" => {
                let hash = all.iter().any(|(n, hv)| {
                    // The emulator looks the join hint up by its lower-case name only.
                    let named = if self.emulator {
                        *n == "join_method" || *n == "join_type"
                    } else {
                        n.eq_ignore_ascii_case("join_method") || n.eq_ignore_ascii_case("join_type")
                    };
                    named
                        && matches!(hv, Some(Value::String(j)) if j.eq_ignore_ascii_case("hash_join"))
                });
                if !hash {
                    return Err(invalid(
                        "hash_join_build_side hint is only supported for HASH joins.".into(),
                    ));
                }
                (!one_of(&["build_left", "build_right"]))
                    .then(|| format!("Invalid value for hash_join_build_side hint: {s}."))
            }
            "hash_join_execution" => (!one_of(&["one_pass", "multi_pass"]))
                .then(|| format!("Invalid value for hash_join_execution hint: {s}.")),
            "group_method" | "group_type" => (!one_of(&["hash_group", "stream_group"]))
                .then(|| format!("Invalid hint value for: group_type/group_method hint: {s}.")),
            "lock_scanned_ranges" => (!one_of(&["exclusive", "shared"]))
                .then(|| format!("Invalid hint value for: {s} hint: lock_scanned_ranges.")),
            "index_strategy" => (!one_of(&["force_index_union"]))
                .then(|| format!("Invalid hint value for: index_strategy hint: {s}.")),
            "scan_method" => {
                let ok = one_of(&["batch", "row"]) || (!self.emulator && one_of(&["columnar"]));
                (!ok).then(|| format!("Invalid hint value for: scan_method hint: {s}."))
            }
            "execution_method" => (!one_of(&["batch", "row"]))
                .then(|| format!("Invalid hint value for: execution_method hint: {s}.")),
            "seekable_key_size" => match v {
                Some(Value::Int64(n)) if *n < 0 => {
                    Some(format!("Invalid hint value for: seekable_key_size hint: {n}."))
                }
                _ => None,
            },
            "optimizer_version" => match v {
                Some(Value::Int64(n)) if (1..=i64::from(i32::MAX)).contains(n) => {
                    (*n > MAX_OPTIMIZER_VERSION)
                        .then(|| format!("Query optimizer version: {n} is not supported"))
                }
                Some(Value::String(_)) if one_of(&["latest", "latest_version", "default_version"]) => None,
                _ => Some(
                    "The value of optimizer_version hint has to be latest or a positive int32 value"
                        .into(),
                ),
            },
            _ => None,
        };
        match prod_bad {
            None => Ok(()),
            Some(_) if self.emulator => Err(emulator_bad()),
            Some(m) => Err(invalid(m)),
        }
    }
}
