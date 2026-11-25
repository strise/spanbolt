//! Lowers the resolved AST to physical operators and compiled expressions.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use googlesql_sys::proto::any_resolved_aggregate_scan_base_proto::Node as AggNode;
use googlesql_sys::proto::any_resolved_computed_column_base_proto::Node as CcBase;
use googlesql_sys::proto::any_resolved_computed_column_impl_proto::Node as CcImpl;
use googlesql_sys::proto::any_resolved_expr_proto::Node as E;
use googlesql_sys::proto::any_resolved_function_call_base_proto::Node as FB;
use googlesql_sys::proto::any_resolved_non_scalar_function_call_base_proto::Node as NS;
use googlesql_sys::proto::any_resolved_scan_proto::Node as S;
use googlesql_sys::proto::resolved_function_call_base_enums::ErrorMode;
use googlesql_sys::proto::resolved_join_scan_enums::JoinType;
use googlesql_sys::proto::resolved_non_scalar_function_call_base_enums::NullHandlingModifier;
use googlesql_sys::proto::resolved_order_by_item_enums::NullOrderMode;
use googlesql_sys::proto::resolved_set_operation_scan_enums::SetOperationType;
use googlesql_sys::proto::resolved_subquery_expr_enums::SubqueryType;
use googlesql_sys::proto::*;
use spanbolt_storage::{Code, Error, IndexId, Result, TableId, Value};

use crate::access::{Access, Bound, KeyPart, RangePart, Source, TableScan};
use crate::agg::{AggKind, AggSpec, Aggregate};
use crate::catalog::{Relation, SqlCatalog};
use crate::exec::{BoxOp, Cx, Env, Eval, Memo, Op};
use crate::func::{self, Sig};
use crate::ops::*;
use crate::types::{self, Type, unimplemented};
use crate::validate::{HintRules, Node};
use crate::value_ops::{Key, truthy};

#[path = "graph_plan.rs"]
mod graph_plan;

/// Columns visible at one level, innermost first.
pub struct Scope<'p> {
    pub cols: &'p [i64],
    pub up: Option<&'p Scope<'p>>,
}

impl Scope<'_> {
    pub const TOP: Scope<'static> = Scope {
        cols: &[],
        up: None,
    };

    fn find(&self, id: i64) -> Option<(usize, usize)> {
        let mut s = Some(self);
        let mut depth = 0;
        while let Some(sc) = s {
            if let Some(i) = sc.cols.iter().position(|&c| c == id) {
                return Some((depth, i));
            }
            s = sc.up;
            depth += 1;
        }
        None
    }
}

pub struct Planned {
    pub op: BoxOp,
    pub layout: Vec<i64>,
}

pub struct Param {
    pub name: String,
    pub ty: Type,
}

#[derive(Clone, Copy, Default)]
pub struct Options {
    /// Follow the emulator instead of production Spanner: honour
    /// `spanner_emulator.disable_query_null_filtered_index_check`, and analyse with the emulator's
    /// language options (window functions and NULLS FIRST/LAST rejected by the analyser), and
    /// use the emulator's full-text search (see `crate::search`).
    pub emulator_compat: bool,
}

impl Options {
    /// Language features enabled on top of the shim's. Both the emulator and Spanner parse
    /// `INSERT ... ON CONFLICT`. Production Spanner also analyses window functions and NULLS
    /// FIRST/LAST, then rejects them while planning, and accepts pipe syntax (`FROM t |> WHERE
    /// ...`), which the emulator rejects. Pipe operators resolve to the usual scans; any that
    /// don't plan are UNIMPLEMENTED.
    pub fn extra_features(&self) -> &'static [&'static str] {
        if self.emulator_compat {
            &["FEATURE_INSERT_ON_CONFLICT_CLAUSE"]
        } else {
            &[
                "FEATURE_INSERT_ON_CONFLICT_CLAUSE",
                "FEATURE_ANALYTIC_FUNCTIONS",
                "FEATURE_NULLS_FIRST_LAST_IN_ORDER_BY",
                "FEATURE_PIPES",
            ]
        }
    }
}

/// Production Spanner's error for an ORDER BY item with a non-default NULLS order: only
/// `ASC NULLS FIRST` and `DESC NULLS LAST` are accepted.
fn check_null_order(it: &ResolvedOrderByItemProto) -> Result<bool> {
    let desc = it.is_descending == Some(true);
    match it.null_order.and_then(|n| NullOrderMode::try_from(n).ok()) {
        Some(NullOrderMode::NullsFirst) if desc => Err(non_default_null_order()),
        Some(NullOrderMode::NullsLast) if !desc => Err(non_default_null_order()),
        _ => Ok(!desc),
    }
}

fn non_default_null_order() -> Error {
    Error::invalid_argument(
        "Non default sort order of NULL values is not supported. Sort expressions in `ORDER BY` clause should use `NULLS FIRST` option with ascending order and `NULLS LAST` option with descending order.",
    )
}

/// Production Spanner's error for a window function: it analyses them, then fails in planning.
fn analytic_scan_error(a: &ResolvedAnalyticScanProto) -> Error {
    let calls: Vec<&AnyResolvedComputedColumnBaseProto> = a
        .function_group_list
        .iter()
        .flat_map(|g| &g.analytic_function_list)
        .collect();
    if calls.len() != 1 {
        return Error::new(
            Code::Unimplemented,
            "Unsupported analytic scan: multiple analytic functions aren't supported",
        );
    }
    let name = computed(calls[0])
        .and_then(|c| match c.expr.as_ref()?.node.as_ref()? {
            E::ResolvedFunctionCallBaseNode(b) => match b.node.as_ref()? {
                FB::ResolvedNonScalarFunctionCallBaseNode(n) => match n.node.as_ref()? {
                    NS::ResolvedAnalyticFunctionCallNode(a) => a.parent.as_ref()?.parent.as_ref(),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .map(fn_name)
        .unwrap_or("");
    let name = match name {
        "$count_star" => "count",
        n => n,
    };
    Error::new(
        Code::Unimplemented,
        format!(
            "Unsupported built-in function: {}.",
            name.to_ascii_uppercase()
        ),
    )
}

pub struct Planner<'c> {
    pub cat: &'c SqlCatalog,
    pub params: HashMap<String, usize>,
    pub param_types: Vec<Param>,
    pub opts: Options,
    /// Statement-level `disable_query_null_filtered_index_check`.
    pub stmt_disable_nf_check: bool,
    /// How hints are checked in this statement (set from the statement's own hints).
    pub hint_rules: HintRules,
    /// Subqueries compiled so far (partitioned DML must have none).
    pub subqueries: usize,
    /// Search functions and search indexes the statement uses.
    pub search: crate::prepared::SearchUse,
    /// Cloud Spanner's placement rules for search functions (not in `--emulator-compat`).
    pub search_rules: SearchRules,
    /// Subquery plans, for `explain` (kind and operator tree).
    pub subplans: Vec<(String, Arc<BoxOp>)>,
    /// The property graph of the graph query being planned.
    graph: Option<graph_plan::GraphCx>,
    /// The previous statement's rows, for the next `GraphRefScan` of a linear graph query.
    graph_ref: Option<Planned>,
    /// The base-table columns each table scan reads.
    pub reads: Vec<crate::prepared::ScanRead>,
    /// The statement reads an INFORMATION_SCHEMA table.
    pub reads_info_schema: bool,
}

/// What Cloud Spanner checks about where search functions appear in a statement.
#[derive(Default)]
pub struct SearchRules {
    /// Compiling a SELECT list (a project scan's computed columns).
    select_list: bool,
    /// (table, column) of TOKENLIST columns that SEARCH reads.
    search_cols: std::collections::HashSet<(String, String)>,
    /// The column each SCORE reads (`None`: not a column).
    score_cols: Vec<(String, String)>,
}

fn escorer() -> Error {
    Error::invalid_argument(
        "ESCORER_SCORE: This function may only be used with a compatible SEARCH predicate",
    )
}

fn not_supported_in_query(function: &str) -> Error {
    Error::invalid_argument(format!(
        "{} is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{{force_index}} hint to the query.",
        function.to_ascii_uppercase()
    ))
}

/// Column references in a TOKENLIST argument (a column, or `TOKENLIST_CONCAT([a, b])`).
fn tokenlist_refs<'e>(e: &'e AnyResolvedExprProto, out: &mut Vec<&'e AnyResolvedExprProto>) {
    if matches!(e.node.as_ref(), Some(E::ResolvedColumnRefNode(_))) {
        out.push(e);
    } else if let Some(b) = call_base(e) {
        for a in &b.argument_list {
            tokenlist_refs(a, out);
        }
    }
}

/// Search function calls in a predicate (not inside subqueries): (name, TOKENLIST argument).
fn search_calls<'e>(
    e: &'e AnyResolvedExprProto,
    out: &mut Vec<(&'e str, &'e AnyResolvedExprProto)>,
) {
    if let Some(E::ResolvedCastNode(c)) = e.node.as_ref() {
        if let Some(x) = c.expr.as_deref() {
            search_calls(x, out);
        }
        return;
    }
    let Some(b) = call_base(e) else { return };
    let name = fn_name(b);
    if matches!(name, "search" | "search_substring" | "search_ngrams")
        && let Some(a) = b.argument_list.first()
    {
        out.push((name, a));
    }
    for a in &b.argument_list {
        search_calls(a, out);
    }
}

/// Lower-cased (table, column) pairs.
type ColumnSet = std::collections::HashSet<(String, String)>;

/// Columns a WHERE clause fixes to constants (`=`, `IN`, `IN UNNEST`, a bare BOOL column) and
/// columns it requires to be non-NULL (any comparison, `IS NOT NULL`).
fn fixed_columns(conj: &[&AnyResolvedExprProto]) -> (ColumnSet, ColumnSet) {
    let (mut fixed, mut not_null) = (
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
    );
    for c in conj {
        if let Some(k) = tokenlist_column(Some(c)) {
            fixed.insert(k.clone());
            not_null.insert(k);
            continue;
        }
        let Some(b) = call_base(c) else { continue };
        let a = &b.argument_list;
        let col = |i: usize| tokenlist_column(a.get(i));
        let others_const = |skip: usize| {
            a.iter()
                .enumerate()
                .all(|(i, x)| i == skip || is_const(x, &[]))
        };
        match fn_name(b) {
            "$equal" | "$in" | "$in_array" => {
                let equal = fn_name(b) == "$equal";
                for i in 0..a.len().min(if equal { 2 } else { 1 }) {
                    // `IN` fixes the column whatever the list is (also a subquery).
                    if let Some(k) = col(i)
                        && (!equal || others_const(i))
                    {
                        fixed.insert(k.clone());
                        not_null.insert(k);
                    }
                }
            }
            "$not" => {
                if let Some(inner) = a.first().and_then(call_base)
                    && fn_name(inner) == "$is_null"
                    && let Some(k) = tokenlist_column(inner.argument_list.first())
                {
                    not_null.insert(k);
                } else if let Some(k) = col(0) {
                    fixed.insert(k.clone());
                    not_null.insert(k);
                }
            }
            "$not_equal" | "$less" | "$less_or_equal" | "$greater" | "$greater_or_equal"
            | "$between" => {
                for i in 0..a.len() {
                    if let Some(k) = col(i) {
                        not_null.insert(k);
                    }
                }
            }
            _ => {}
        }
    }
    (fixed, not_null)
}

/// Columns a search index's `WHERE` filter requires non-NULL (`a IS NOT NULL AND b IS NOT NULL`).
/// `None` when the filter has another shape.
fn filter_not_null(filter: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let upper = filter.to_ascii_uppercase();
    let mut rest = (filter, upper.as_str());
    loop {
        let (part, next) = match rest.1.find(" AND ") {
            Some(i) => (&rest.0[..i], Some(i + 5)),
            None => (rest.0, None),
        };
        let p = part.trim().trim_matches(|c| c == '(' || c == ')').trim();
        let col = p
            .to_ascii_uppercase()
            .strip_suffix("IS NOT NULL")
            .map(|c| p[..c.len()].trim().trim_matches('`').to_ascii_lowercase())?;
        if col.is_empty() || col.contains(char::is_whitespace) {
            return None;
        }
        out.push(col);
        match next {
            Some(i) => rest = (&rest.0[i..], &rest.1[i..]),
            None => return Some(out),
        }
    }
}

fn tokenlist_column(e: Option<&AnyResolvedExprProto>) -> Option<(String, String)> {
    match e?.node.as_ref()? {
        E::ResolvedColumnRefNode(c) => {
            let c = c.column.as_ref()?;
            Some((
                c.table_name
                    .clone()
                    .unwrap_or_default()
                    .to_ascii_lowercase(),
                c.name.clone().unwrap_or_default().to_ascii_lowercase(),
            ))
        }
        _ => None,
    }
}

fn nid(c: &Option<ResolvedColumnProto>) -> i64 {
    c.as_ref().and_then(|c| c.column_id).unwrap_or(-1)
}

pub fn col_type(c: &ResolvedColumnProto) -> Result<Type> {
    types::column_from_proto(
        c.r#type
            .as_ref()
            .ok_or_else(|| Error::internal("column without type"))?,
    )
}

fn node_name<T: std::fmt::Debug>(n: &T) -> String {
    let s = format!("{n:?}");
    let end = s.find(['(', ' ', '{']).unwrap_or(s.len());
    s[..end]
        .trim_end_matches("Node")
        .trim_start_matches("Resolved")
        .to_string()
}

pub fn expr_type(e: &AnyResolvedExprProto) -> Option<&TypeProto> {
    let base: Option<&ResolvedExprProto> = match e.node.as_ref()? {
        E::ResolvedLiteralNode(x) => x.parent.as_ref(),
        E::ResolvedParameterNode(x) => x.parent.as_ref(),
        E::ResolvedColumnRefNode(x) => x.parent.as_ref(),
        E::ResolvedFunctionCallBaseNode(b) => match b.node.as_ref()? {
            FB::ResolvedFunctionCallNode(f) => f.parent.as_ref()?.parent.as_ref(),
            FB::ResolvedNonScalarFunctionCallBaseNode(n) => match n.node.as_ref()? {
                NS::ResolvedAggregateFunctionCallNode(a) => {
                    a.parent.as_ref()?.parent.as_ref()?.parent.as_ref()
                }
                _ => None,
            },
        },
        E::ResolvedCastNode(x) => x.parent.as_ref(),
        E::ResolvedMakeStructNode(x) => x.parent.as_ref(),
        E::ResolvedGetStructFieldNode(x) => x.parent.as_ref(),
        E::ResolvedGetJsonFieldNode(x) => x.parent.as_ref(),
        E::ResolvedSubqueryExprNode(x) => x.parent.as_ref(),
        E::ResolvedWithExprNode(x) => x.parent.as_ref(),
        E::ResolvedDmldefaultNode(x) => x.parent.as_ref(),
        E::ResolvedGraphGetElementPropertyNode(x) => x.parent.as_ref(),
        _ => None,
    };
    base.and_then(|b| b.r#type.as_ref())
}

pub fn type_of(e: &AnyResolvedExprProto) -> Result<Type> {
    types::from_proto(expr_type(e).ok_or_else(|| Error::internal("expression without type"))?)
}

/// A function call's base fields.
/// The emulator's error for a NULL ARRAY<STRUCT> literal and for a lambda call returning one.
const EMULATOR_NULL_STRUCT_ARRAY: &str = "Unsupported query shape: Spanner does not support array \
    constructor syntax for a null-valued array of struct.";

pub fn call_base(e: &AnyResolvedExprProto) -> Option<&ResolvedFunctionCallBaseProto> {
    match e.node.as_ref()? {
        E::ResolvedFunctionCallBaseNode(b) => match b.node.as_ref()? {
            FB::ResolvedFunctionCallNode(f) => f.parent.as_ref(),
            _ => None,
        },
        _ => None,
    }
}

pub fn full_fn_name(b: &ResolvedFunctionCallBaseProto) -> &str {
    b.function
        .as_ref()
        .and_then(|f| f.name.as_deref())
        .unwrap_or("")
}

pub fn fn_name(b: &ResolvedFunctionCallBaseProto) -> &str {
    let full = b
        .function
        .as_ref()
        .and_then(|f| f.name.as_deref())
        .unwrap_or("");
    full.rsplit_once(':').map_or(full, |(_, n)| n)
}

fn conjuncts<'e>(e: &'e AnyResolvedExprProto, out: &mut Vec<&'e AnyResolvedExprProto>) {
    if let Some(b) = call_base(e)
        && fn_name(b) == "$and"
    {
        for a in &b.argument_list {
            conjuncts(a, out);
        }
        return;
    }
    out.push(e);
}

fn column_ref(e: &AnyResolvedExprProto) -> Option<i64> {
    match e.node.as_ref()? {
        E::ResolvedColumnRefNode(c) => Some(nid(&c.column)),
        _ => None,
    }
}

/// Expressions that are constant for one scan of a table: literals, parameters, references to
/// columns outside `ids`, and casts/calls over those.
fn is_const(e: &AnyResolvedExprProto, ids: &[i64]) -> bool {
    match e.node.as_ref() {
        Some(E::ResolvedLiteralNode(_)) | Some(E::ResolvedParameterNode(_)) => true,
        Some(E::ResolvedColumnRefNode(c)) => !ids.contains(&nid(&c.column)),
        Some(E::ResolvedCastNode(c)) => c.expr.as_deref().is_some_and(|x| is_const(x, ids)),
        Some(E::ResolvedFunctionCallBaseNode(_)) => call_base(e).is_some_and(|b| {
            !matches!(fn_name(b), "rand" | "generate_uuid" | "new_uuid")
                && b.generic_argument_list.is_empty()
                && b.argument_list.iter().all(|a| is_const(a, ids))
        }),
        _ => false,
    }
}

/// True when every column `e` references is in `ids` (and `e` is a simple expression).
fn refs_only(e: &AnyResolvedExprProto, ids: &[i64]) -> bool {
    match e.node.as_ref() {
        Some(E::ResolvedLiteralNode(_)) | Some(E::ResolvedParameterNode(_)) => true,
        Some(E::ResolvedColumnRefNode(c)) => ids.contains(&nid(&c.column)),
        Some(E::ResolvedCastNode(c)) => c.expr.as_deref().is_some_and(|x| refs_only(x, ids)),
        Some(E::ResolvedGetStructFieldNode(g)) => {
            g.expr.as_deref().is_some_and(|x| refs_only(x, ids))
        }
        Some(E::ResolvedFunctionCallBaseNode(_)) => call_base(e).is_some_and(|b| {
            b.generic_argument_list.is_empty() && b.argument_list.iter().all(|a| refs_only(a, ids))
        }),
        _ => false,
    }
}

/// PENDING_COMMIT_TIMESTAMP() anywhere but as a DML value (also a THEN RETURN of its column).
pub(crate) const PENDING_COMMIT_TIMESTAMP_USE: &str = "The PENDING_COMMIT_TIMESTAMP() function may only be used as a value for INSERT or UPDATE of an appropriately typed column. It cannot be used in SELECT, or as the input to any other scalar expression.";

/// The columns of the DML target table `ts` (positions in `layout`) that `es` refer to. Among
/// the columns the statement names, so an expression too complex to analyse counts only those.
pub(crate) fn columns_read(
    es: &[&AnyResolvedExprProto],
    ts: &ResolvedTableScanProto,
    layout: &[i64],
) -> Vec<usize> {
    ts.column_index_list
        .iter()
        .map(|&i| i as usize)
        .filter(|&c| {
            layout
                .get(c)
                .is_some_and(|id| es.iter().any(|e| refs_any(e, std::slice::from_ref(id))))
        })
        .collect()
}

fn refs_any(e: &AnyResolvedExprProto, ids: &[i64]) -> bool {
    match e.node.as_ref() {
        Some(E::ResolvedColumnRefNode(c)) => ids.contains(&nid(&c.column)),
        Some(E::ResolvedGraphGetElementPropertyNode(g)) => {
            g.expr.as_deref().is_none_or(|x| refs_any(x, ids))
        }
        Some(E::ResolvedCastNode(c)) => c.expr.as_deref().is_some_and(|x| refs_any(x, ids)),
        Some(E::ResolvedGetStructFieldNode(g)) => {
            g.expr.as_deref().is_some_and(|x| refs_any(x, ids))
        }
        Some(E::ResolvedFunctionCallBaseNode(_)) => match call_base(e) {
            Some(b) if b.generic_argument_list.is_empty() => {
                b.argument_list.iter().any(|a| refs_any(a, ids))
            }
            _ => true,
        },
        Some(E::ResolvedLiteralNode(_)) | Some(E::ResolvedParameterNode(_)) => false,
        _ => true,
    }
}

/// Whether `e` reads a column outside `ids` (an outer row), or is something not analysed here.
fn refs_outside(e: &AnyResolvedExprProto, ids: &[i64]) -> bool {
    match e.node.as_ref() {
        Some(E::ResolvedLiteralNode(_)) | Some(E::ResolvedParameterNode(_)) => false,
        Some(E::ResolvedColumnRefNode(c)) => !ids.contains(&nid(&c.column)),
        Some(E::ResolvedCastNode(c)) => c.expr.as_deref().is_none_or(|x| refs_outside(x, ids)),
        Some(E::ResolvedGetStructFieldNode(g)) => {
            g.expr.as_deref().is_none_or(|x| refs_outside(x, ids))
        }
        Some(E::ResolvedFunctionCallBaseNode(_)) => match call_base(e) {
            Some(b) if b.generic_argument_list.is_empty() => {
                b.argument_list.iter().any(|a| refs_outside(a, ids))
            }
            _ => true,
        },
        _ => true,
    }
}

/// Types whose grouping key (`Key`) never separates two values SQL `=` calls equal.
fn hash_key_type(t: &Type) -> bool {
    matches!(
        t,
        Type::Bool
            | Type::Int64
            | Type::Float32
            | Type::Float64
            | Type::Numeric
            | Type::String(_)
            | Type::Bytes(_)
            | Type::Date
            | Type::Timestamp
            | Type::Uuid
    )
}

/// `SAFE.f(...)`: evaluation errors become NULL.
fn safe_call(ev: Eval) -> Eval {
    Arc::new(move |cx: &Cx, env: &Env| match ev(cx, env) {
        Ok(v) => Ok(v),
        Err(e) if matches!(e.code, Code::OutOfRange | Code::InvalidArgument) => Ok(Value::Null),
        Err(e) => Err(e),
    })
}

/// A constant that `TABLE_NAME` (column `id`) must equal, ignoring case, for a conjunct to hold:
/// `TABLE_NAME = k`, `LOWER(TABLE_NAME) = k` or `UPPER(TABLE_NAME) = k`, either side.
fn table_name_lookup<'e>(
    conj: &[&'e AnyResolvedExprProto],
    id: i64,
    ids: &[i64],
) -> Option<&'e AnyResolvedExprProto> {
    let is_key = |e: &AnyResolvedExprProto| {
        column_ref(e) == Some(id)
            || call_base(e).is_some_and(|b| {
                matches!(fn_name(b), "lower" | "upper")
                    && b.argument_list.len() == 1
                    && column_ref(&b.argument_list[0]) == Some(id)
            })
    };
    conj.iter().find_map(|c| {
        let b = call_base(c)?;
        if fn_name(b) != "$equal" {
            return None;
        }
        let [x, y] = &b.argument_list[..] else {
            return None;
        };
        let is_str = |e: &AnyResolvedExprProto| matches!(type_of(e), Ok(Type::String(_)));
        if is_key(x) && is_const(y, ids) && is_str(y) {
            Some(y)
        } else if is_key(y) && is_const(x, ids) && is_str(x) {
            Some(x)
        } else {
            None
        }
    })
}

/// One usable key predicate on a table column.
enum Pred<'e> {
    Eq(Vec<&'e AnyResolvedExprProto>),
    InArray(&'e AnyResolvedExprProto),
    IsNull,
    Lo(&'e AnyResolvedExprProto, bool),
    Hi(&'e AnyResolvedExprProto, bool),
}

fn key_preds<'e>(conj: &[&'e AnyResolvedExprProto], ids: &[i64]) -> Vec<(i64, Pred<'e>)> {
    let mut out = Vec::new();
    for c in conj {
        let Some(b) = call_base(c) else { continue };
        let a = &b.argument_list;
        let col = |i: usize| a.get(i).and_then(column_ref).filter(|id| ids.contains(id));
        let konst = |i: usize| a.get(i).filter(|x| is_const(x, ids));
        let same_type = |ci: usize, vi: usize| match (a.get(ci), a.get(vi)) {
            (Some(x), Some(y)) => match (type_of(x), type_of(y)) {
                (Ok(t1), Ok(t2)) => types::same(&t1, &t2),
                _ => false,
            },
            _ => false,
        };
        match fn_name(b) {
            "$equal" => {
                if let (Some(id), Some(v)) = (col(0), konst(1))
                    && same_type(0, 1)
                {
                    out.push((id, Pred::Eq(vec![v])));
                } else if let (Some(id), Some(v)) = (col(1), konst(0))
                    && same_type(1, 0)
                {
                    out.push((id, Pred::Eq(vec![v])));
                }
            }
            "$in" => {
                if let Some(id) = col(0)
                    && a.len() > 1
                    && (1..a.len()).all(|i| konst(i).is_some() && same_type(0, i))
                {
                    out.push((id, Pred::Eq(a[1..].iter().collect())));
                }
            }
            "$in_array" => {
                if let (Some(id), Some(v)) = (col(0), konst(1)) {
                    let elem_ok = match (type_of(&a[0]), type_of(&a[1])) {
                        (Ok(t), Ok(Type::Array(e))) => types::same(&t, &e),
                        _ => false,
                    };
                    if elem_ok {
                        out.push((id, Pred::InArray(v)));
                    }
                }
            }
            "$is_null" => {
                if let Some(id) = col(0) {
                    out.push((id, Pred::IsNull));
                }
            }
            op @ ("$less" | "$less_or_equal" | "$greater" | "$greater_or_equal") => {
                let inclusive = op.ends_with("equal");
                let upper = op.starts_with("$less");
                if let (Some(id), Some(v)) = (col(0), konst(1))
                    && same_type(0, 1)
                {
                    out.push((
                        id,
                        if upper {
                            Pred::Hi(v, inclusive)
                        } else {
                            Pred::Lo(v, inclusive)
                        },
                    ));
                } else if let (Some(id), Some(v)) = (col(1), konst(0))
                    && same_type(1, 0)
                {
                    out.push((
                        id,
                        if upper {
                            Pred::Lo(v, inclusive)
                        } else {
                            Pred::Hi(v, inclusive)
                        },
                    ));
                }
            }
            "$between" => {
                if let (Some(id), Some(lo), Some(hi)) = (col(0), konst(1), konst(2))
                    && same_type(0, 1)
                    && same_type(0, 2)
                {
                    out.push((id, Pred::Lo(lo, true)));
                    out.push((id, Pred::Hi(hi, true)));
                }
            }
            _ => {}
        }
    }
    out
}

/// Columns a conjunct proves non-NULL, by the rule real Spanner applies to NULL_FILTERED indexes
/// (verified on Spanner with PLAN queries): `IS NOT NULL`, comparisons and IN lists (NULL never
/// compares true), and `IN UNNEST` over a literal array without NULLs. `IN UNNEST(@param)` does
/// not count, because the array may contain NULL.
fn not_null_ids(conj: &[&AnyResolvedExprProto]) -> Vec<i64> {
    let mut out = Vec::new();
    for c in conj {
        let Some(b) = call_base(c) else { continue };
        let a = &b.argument_list;
        match fn_name(b) {
            "$not" => {
                if let Some(inner) = a.first().and_then(call_base)
                    && fn_name(inner) == "$is_null"
                    && let Some(id) = inner.argument_list.first().and_then(column_ref)
                {
                    out.push(id);
                }
            }
            "$equal" | "$not_equal" | "$less" | "$less_or_equal" | "$greater"
            | "$greater_or_equal" => out.extend(a.iter().filter_map(column_ref)),
            "$between" | "$in" | "$like" => out.extend(a.first().and_then(column_ref)),
            "$in_array" => {
                let literal_without_nulls = a.get(1).and_then(literal_of).is_some_and(
                    |v| matches!(&v, Value::Array(items) if items.iter().all(|x| !x.is_null())),
                );
                if literal_without_nulls {
                    out.extend(a.first().and_then(column_ref));
                }
            }
            _ => {}
        }
    }
    out
}

/// One side of a key range: the bound expression and whether it is inclusive.
type RangeBound<'e> = Option<(&'e AnyResolvedExprProto, bool)>;

/// The key parts an access path binds, then an optional range on the next key column.
struct KeyChoice<'e> {
    parts: Vec<Pred<'e>>,
    range: Option<(RangeBound<'e>, RangeBound<'e>)>,
}

fn choose_key<'e>(key_ids: &[Option<i64>], preds: &mut [(i64, Pred<'e>)]) -> KeyChoice<'e> {
    let mut parts = Vec::new();
    let mut range = None;
    for kid in key_ids {
        let Some(kid) = kid else { break };
        let eq = preds.iter().position(|(id, p)| {
            id == kid && matches!(p, Pred::Eq(_) | Pred::InArray(_) | Pred::IsNull)
        });
        if let Some(i) = eq {
            let p = std::mem::replace(&mut preds[i].1, Pred::IsNull);
            parts.push(p);
            continue;
        }
        let lo = preds.iter().find_map(|(id, p)| match p {
            Pred::Lo(e, inc) if id == kid => Some((*e, *inc)),
            _ => None,
        });
        let hi = preds.iter().find_map(|(id, p)| match p {
            Pred::Hi(e, inc) if id == kid => Some((*e, *inc)),
            _ => None,
        });
        if lo.is_some() || hi.is_some() {
            range = Some((lo, hi));
        }
        break;
    }
    KeyChoice { parts, range }
}

fn hint_value<'h>(
    hints: &'h [ResolvedOptionProto],
    qualifier: &str,
    name: &str,
) -> Option<&'h Value2> {
    hints
        .iter()
        .find(|h| {
            h.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
                && h.qualifier
                    .as_deref()
                    .unwrap_or("")
                    .eq_ignore_ascii_case(qualifier)
        })
        .and_then(|h| h.value.as_ref())
}

type Value2 = AnyResolvedExprProto;

pub(crate) fn literal_of(e: &AnyResolvedExprProto) -> Option<Value> {
    match e.node.as_ref()? {
        E::ResolvedLiteralNode(l) => {
            let v = l.value.as_ref()?;
            let ty = types::from_proto(v.r#type.as_ref()?).ok()?;
            crate::literal::value(v.value.as_ref(), &ty).ok()
        }
        _ => None,
    }
}

pub fn hint_bool(hints: &[ResolvedOptionProto], qualifier: &str, name: &str) -> bool {
    hint_value(hints, qualifier, name)
        .and_then(literal_of)
        .is_some_and(|v| matches!(v, Value::Bool(true)))
}

fn scan_base(s: &AnyResolvedScanProto) -> Option<&ResolvedScanProto> {
    match s.node.as_ref()? {
        S::ResolvedTableScanNode(x) => x.parent.as_ref(),
        S::ResolvedFilterScanNode(x) => x.parent.as_ref(),
        S::ResolvedProjectScanNode(x) => x.parent.as_ref(),
        S::ResolvedJoinScanNode(x) => x.parent.as_ref(),
        S::ResolvedArrayScanNode(x) => x.parent.as_ref(),
        S::ResolvedSingleRowScanNode(x) => x.parent.as_ref(),
        S::ResolvedOrderByScanNode(x) => x.parent.as_ref(),
        S::ResolvedLimitOffsetScanNode(x) => x.parent.as_ref(),
        S::ResolvedSetOperationScanNode(x) => x.parent.as_ref(),
        S::ResolvedWithScanNode(x) => x.parent.as_ref(),
        S::ResolvedWithRefScanNode(x) => x.parent.as_ref(),
        S::ResolvedSampleScanNode(x) => x.parent.as_ref(),
        S::ResolvedAggregateScanBaseNode(a) => match a.node.as_ref()? {
            AggNode::ResolvedAggregateScanNode(x) => x.parent.as_ref()?.parent.as_ref(),
            _ => None,
        },
        _ => None,
    }
}

pub fn scan_columns(s: &AnyResolvedScanProto) -> Vec<i64> {
    scan_base(s)
        .map(|b| {
            b.column_list
                .iter()
                .map(|c| c.column_id.unwrap_or(-1))
                .collect()
        })
        .unwrap_or_default()
}

pub fn computed(c: &AnyResolvedComputedColumnBaseProto) -> Option<&ResolvedComputedColumnProto> {
    match c.node.as_ref()? {
        CcBase::ResolvedComputedColumnImplNode(i) => match i.node.as_ref()? {
            CcImpl::ResolvedComputedColumnNode(c) => Some(c),
            _ => None,
        },
    }
}

pub fn positions(layout: &[i64], ids: &[i64]) -> Result<Vec<usize>> {
    ids.iter()
        .map(|id| {
            layout
                .iter()
                .position(|l| l == id)
                .ok_or_else(|| Error::internal(format!("column {id} not produced by input")))
        })
        .collect()
}

/// Plans `e` and returns its column/table restrictions for a scan being planned.
struct ScanCtx<'e> {
    conj: Vec<&'e AnyResolvedExprProto>,
}

impl<'c> Planner<'c> {
    pub fn new(cat: &'c SqlCatalog, opts: Options) -> Self {
        Planner {
            cat,
            params: HashMap::new(),
            param_types: Vec::new(),
            opts,
            stmt_disable_nf_check: false,
            hint_rules: HintRules::new(opts.emulator_compat),
            subqueries: 0,
            search: crate::prepared::SearchUse {
                emulator: opts.emulator_compat,
                ..Default::default()
            },
            search_rules: SearchRules::default(),
            subplans: Vec::new(),
            graph: None,
            graph_ref: None,
            reads: Vec::new(),
            reads_info_schema: false,
        }
    }

    /// Checks a statement's own hints and sets the rules for the hints inside it.
    pub fn statement_hints(&mut self, node: Node, hints: &[ResolvedOptionProto]) -> Result<()> {
        self.hint_rules = HintRules::statement(self.opts.emulator_compat, hints)?;
        self.hint_rules.check(node, hints)
    }

    /// Statement-level search checks once the whole statement is planned: on Cloud Spanner,
    /// SCORE needs a SEARCH predicate over the same TOKENLIST column.
    pub fn finish_search(&self) -> Result<()> {
        if self.opts.emulator_compat {
            return Ok(());
        }
        let r = &self.search_rules;
        if let Some((table, column)) = r.score_cols.iter().find(|c| !r.search_cols.contains(*c)) {
            // Without a SEARCH no WHERE clause picks a partition: a partitioned or filtered
            // search index on the column cannot serve SCORE at all.
            let indexes: Vec<_> = self
                .cat
                .schema
                .search_indexes
                .values()
                .filter(|si| {
                    si.table.eq_ignore_ascii_case(table)
                        && si.columns.iter().any(|c| c.eq_ignore_ascii_case(column))
                })
                .collect();
            if !indexes.is_empty()
                && indexes
                    .iter()
                    .all(|si| !si.partition_by.is_empty() || si.filter.is_some())
            {
                return Err(not_supported_in_query("score"));
            }
            return Err(escorer());
        }
        Ok(())
    }

    // ---------------------------------------------------------------- scans

    pub fn scan(&mut self, s: &AnyResolvedScanProto, outer: &Scope) -> Result<Planned> {
        self.scan_with(s, outer, ScanCtx { conj: Vec::new() })
    }

    fn scan_with(
        &mut self,
        s: &AnyResolvedScanProto,
        outer: &Scope,
        sc: ScanCtx,
    ) -> Result<Planned> {
        let node = s
            .node
            .as_ref()
            .ok_or_else(|| Error::internal("empty scan"))?;
        match node {
            S::ResolvedSingleRowScanNode(_) => Ok(Planned {
                op: Box::new(SingleRow),
                layout: Vec::new(),
            }),
            S::ResolvedTableScanNode(t) => self.table_scan(t, outer, &sc.conj),
            S::ResolvedFilterScanNode(f) => {
                let input = f
                    .input_scan
                    .as_deref()
                    .ok_or_else(|| Error::internal("filter without input"))?;
                let pred = f
                    .filter_expr
                    .as_deref()
                    .ok_or_else(|| Error::internal("filter without predicate"))?;
                let mut conj = sc.conj;
                conjuncts(pred, &mut conj);
                self.check_search_index(&conj)?;
                let p = self.scan_with(input, outer, ScanCtx { conj })?;
                let scope = Scope {
                    cols: &p.layout,
                    up: Some(outer),
                };
                let select_list = std::mem::replace(&mut self.search_rules.select_list, false);
                let pred = self.expr(pred, &scope);
                self.search_rules.select_list = select_list;
                let pred = pred?;
                Ok(Planned {
                    op: Box::new(Filter { input: p.op, pred }),
                    layout: p.layout,
                })
            }
            S::ResolvedProjectScanNode(p) => {
                let input = p
                    .input_scan
                    .as_deref()
                    .ok_or_else(|| Error::internal("project without input"))?;
                let inp = self.scan(input, outer)?;
                if p.expr_list.is_empty() {
                    return Ok(inp);
                }
                let scope = Scope {
                    cols: &inp.layout,
                    up: Some(outer),
                };
                let mut exprs = Vec::new();
                let mut layout = inp.layout.clone();
                let select_list = std::mem::replace(&mut self.search_rules.select_list, true);
                for cc in &p.expr_list {
                    let e = cc
                        .expr
                        .as_ref()
                        .ok_or_else(|| Error::internal("computed column without expr"));
                    match e.and_then(|e| self.expr(e, &scope)) {
                        Ok(e) => exprs.push(e),
                        Err(e) => {
                            self.search_rules.select_list = select_list;
                            return Err(e);
                        }
                    }
                    layout.push(nid(&cc.column));
                }
                self.search_rules.select_list = select_list;
                Ok(Planned {
                    op: Box::new(Compute {
                        input: inp.op,
                        exprs,
                    }),
                    layout,
                })
            }
            S::ResolvedJoinScanNode(j) => self.join(j, outer, sc.conj),
            S::ResolvedArrayScanNode(a) => self.array_scan(a, outer),
            S::ResolvedAggregateScanBaseNode(a) => match a.node.as_ref() {
                Some(AggNode::ResolvedAggregateScanNode(x)) => {
                    let base = x
                        .parent
                        .as_ref()
                        .ok_or_else(|| Error::internal("aggregate without base"))?;
                    self.aggregate(base, outer)
                }
                _ => Err(unimplemented("This aggregation")),
            },
            S::ResolvedOrderByScanNode(o) => {
                let input = o
                    .input_scan
                    .as_deref()
                    .ok_or_else(|| Error::internal("order by without input"))?;
                let inp = self.scan(input, outer)?;
                let scope = Scope {
                    cols: &inp.layout,
                    up: Some(outer),
                };
                let keys = self.order_keys(&o.order_by_item_list, &scope)?;
                Ok(Planned {
                    op: Box::new(Sort {
                        input: inp.op,
                        keys,
                    }),
                    layout: inp.layout,
                })
            }
            S::ResolvedLimitOffsetScanNode(l) => {
                let input = l
                    .input_scan
                    .as_deref()
                    .ok_or_else(|| Error::internal("limit without input"))?;
                let inp = self.scan(input, outer)?;
                let limit = l
                    .limit
                    .as_deref()
                    .map(|e| self.expr(e, outer))
                    .transpose()?;
                let offset = l
                    .offset
                    .as_deref()
                    .map(|e| self.expr(e, outer))
                    .transpose()?;
                Ok(Planned {
                    op: Box::new(Limit {
                        input: inp.op,
                        limit,
                        offset,
                    }),
                    layout: inp.layout,
                })
            }
            S::ResolvedSetOperationScanNode(so) => {
                if let Some(b) = &so.parent {
                    self.hint_rules.check(Node::SetOperation, &b.hint_list)?;
                }
                let kind = match so.op_type.and_then(|t| SetOperationType::try_from(t).ok()) {
                    Some(SetOperationType::UnionAll) => SetKind::UnionAll,
                    Some(SetOperationType::UnionDistinct) => SetKind::UnionDistinct,
                    Some(SetOperationType::IntersectAll) => SetKind::IntersectAll,
                    Some(SetOperationType::IntersectDistinct) => SetKind::IntersectDistinct,
                    Some(SetOperationType::ExceptAll) => SetKind::ExceptAll,
                    Some(SetOperationType::ExceptDistinct) => SetKind::ExceptDistinct,
                    None => return Err(unimplemented("This set operation")),
                };
                let mut inputs = Vec::new();
                for item in &so.input_item_list {
                    let sc = item
                        .scan
                        .as_ref()
                        .ok_or_else(|| Error::internal("set item without scan"))?;
                    let p = self.scan(sc, outer)?;
                    let ids: Vec<i64> = item
                        .output_column_list
                        .iter()
                        .map(|c| c.column_id.unwrap_or(-1))
                        .collect();
                    let pos = positions(&p.layout, &ids)?;
                    inputs.push(Box::new(Select {
                        input: p.op,
                        positions: pos,
                    }) as BoxOp);
                }
                Ok(Planned {
                    op: Box::new(SetOp { kind, inputs }),
                    layout: scan_columns(s),
                })
            }
            S::ResolvedWithScanNode(w) => {
                if w.recursive == Some(true) {
                    return Err(unimplemented("WITH RECURSIVE"));
                }
                let mut entries = Vec::new();
                for e in &w.with_entry_list {
                    let sub = e
                        .with_subquery
                        .as_ref()
                        .ok_or_else(|| Error::internal("with entry without query"))?;
                    let p = self.scan(sub, outer)?;
                    let pos = positions(&p.layout, &scan_columns(sub))?;
                    entries.push((
                        e.with_query_name.clone().unwrap_or_default(),
                        Box::new(Select {
                            input: p.op,
                            positions: pos,
                        }) as BoxOp,
                    ));
                }
                let body = w
                    .query
                    .as_deref()
                    .ok_or_else(|| Error::internal("with without body"))?;
                let b = self.scan(body, outer)?;
                Ok(Planned {
                    op: Box::new(With {
                        entries,
                        body: b.op,
                    }),
                    layout: b.layout,
                })
            }
            S::ResolvedWithRefScanNode(r) => Ok(Planned {
                op: Box::new(WithRef {
                    name: r.with_query_name.clone().unwrap_or_default(),
                }),
                layout: scan_columns(s),
            }),
            S::ResolvedSampleScanNode(x) => self.sample_scan(x, outer),
            S::ResolvedAnalyticScanNode(a) => Err(analytic_scan_error(a)),
            S::ResolvedGraphTableScanNode(g) => self.graph_table(g, outer),
            S::ResolvedGraphScanBaseNode(b) => self.graph_base(b, outer),
            S::ResolvedGraphRefScanNode(_) => self
                .graph_ref
                .take()
                .ok_or_else(|| unimplemented("This graph query shape")),
            other => Err(unimplemented(format!(
                "Query operator {}",
                node_name(other)
            ))),
        }
    }

    fn order_keys(
        &mut self,
        items: &[ResolvedOrderByItemProto],
        scope: &Scope,
    ) -> Result<Vec<SortKey>> {
        let mut keys = Vec::new();
        for it in items {
            let id = it.column_ref.as_ref().map(|c| nid(&c.column)).unwrap_or(-1);
            let (d, i) = scope
                .find(id)
                .ok_or_else(|| Error::internal("ORDER BY column not found"))?;
            let desc = it.is_descending == Some(true);
            let nulls_first = check_null_order(it)?;
            keys.push(SortKey {
                expr: Arc::new(move |_: &Cx, e: &Env| Ok(e.get(d, i).clone())),
                desc,
                nulls_first,
            });
        }
        Ok(keys)
    }

    /// TABLESAMPLE. Rejects what the emulator's query validator rejects, or in default mode what
    /// production Spanner rejects, in its words (and WITH WEIGHT, which it doesn't support); the
    /// analyser already checks the size's type and a literal's range, and rejects PARTITION BY.
    /// Filters above the sample are not pushed into its input: they apply to the sampled rows.
    fn sample_scan(&mut self, x: &ResolvedSampleScanProto, outer: &Scope) -> Result<Planned> {
        let emulator = self.opts.emulator_compat;
        if x.repeatable_argument.is_some() {
            return Err(Error::invalid_argument(if emulator {
                "REPEATABLE is not supported"
            } else {
                "REPEATABLE TABLESAMPLE is not supported."
            }));
        }
        let name = x.method.as_deref().unwrap_or("").to_ascii_lowercase();
        let unit = x.unit.unwrap_or(0);
        let rows = unit == resolved_sample_scan_enums::SampleUnit::Rows as i32;
        let method = match (name.as_str(), rows) {
            ("bernoulli", false) => Some(SampleMethod::Bernoulli),
            ("reservoir", true) => Some(SampleMethod::Reservoir),
            _ => None,
        };
        let Some(method) = method else {
            if !emulator {
                let unit = if rows { "ROWS" } else { "PERCENT" };
                return Err(Error::invalid_argument(format!(
                    "The specified sampling algorithm ({name}/{unit}) is not supported (check the spelling)."
                )));
            }
            return Err(Error::invalid_argument(match name.as_str() {
                "bernoulli" => "BERNOULLI/ROWS is not supported".to_string(),
                "reservoir" => "RESERVOIR/PERCENT is not supported".to_string(),
                "system" => "SYSTEM sampling is not supported".to_string(),
                m => format!("Unknown scan method {m}"),
            }));
        };
        if !emulator && x.weight_column.is_some() {
            return Err(Error::invalid_argument(
                "TABLESAMPLE WITH WEIGHT is not supported.",
            ));
        }
        if !x.partition_by_list.is_empty() {
            return Err(unimplemented("TABLESAMPLE with PARTITION BY"));
        }
        let input = x
            .input_scan
            .as_deref()
            .ok_or_else(|| Error::internal("sample without input"))?;
        let size = x
            .size
            .as_deref()
            .ok_or_else(|| Error::internal("sample without size"))?;
        let size = self.expr(size, outer)?;
        let p = self.scan(input, outer)?;
        let mut layout = p.layout;
        let weight = x.weight_column.as_ref().map(|w| nid(&w.column));
        layout.extend(weight);
        Ok(Planned {
            op: Box::new(Sample {
                input: p.op,
                method,
                size,
                weight: weight.is_some(),
                emulator,
            }),
            layout,
        })
    }

    fn table_scan(
        &mut self,
        t: &ResolvedTableScanProto,
        outer: &Scope,
        conj: &[&AnyResolvedExprProto],
    ) -> Result<Planned> {
        let base = t
            .parent
            .as_ref()
            .ok_or_else(|| Error::internal("table scan without base"))?;
        let layout: Vec<i64> = base
            .column_list
            .iter()
            .map(|c| c.column_id.unwrap_or(-1))
            .collect();
        let tref = t
            .table
            .as_ref()
            .ok_or_else(|| Error::internal("table scan without table"))?;
        let full = tref
            .full_name
            .as_deref()
            .or(tref.name.as_deref())
            .unwrap_or("");
        let rel = self
            .cat
            .relation(full)
            .cloned()
            .ok_or_else(|| Error::not_found(format!("Table not found: {full}")))?;
        let idx: Vec<usize> = t.column_index_list.iter().map(|&i| i as usize).collect();
        self.hint_rules.check(Node::TableScan, &base.hint_list)?;
        if t.for_system_time_expr.is_some() {
            return Err(unimplemented("FOR SYSTEM_TIME AS OF"));
        }
        match rel {
            Relation::Table(tid) => {
                let op = self.base_table(tid, t, &base.hint_list, &layout, &idx, outer, conj)?;
                Ok(Planned { op, layout })
            }
            Relation::View(v) => {
                let q = v
                    .query
                    .query
                    .as_ref()
                    .ok_or_else(|| Error::internal("view without query"))?;
                let p = self.scan(q, &Scope::TOP)?;
                let out_ids: Vec<i64> = v
                    .query
                    .output_column_list
                    .iter()
                    .map(|o| nid(&o.column))
                    .collect();
                let pos = positions(&p.layout, &out_ids)?;
                let pick: Vec<usize> = idx
                    .iter()
                    .map(|&i| pos.get(i).copied().unwrap_or(usize::MAX))
                    .collect();
                Ok(Planned {
                    op: Box::new(Select {
                        input: p.op,
                        positions: pick,
                    }),
                    layout,
                })
            }
            Relation::Info(it) => {
                self.reads_info_schema = true;
                let key = it
                    .columns
                    .iter()
                    .position(|(n, ..)| n == "TABLE_NAME")
                    .and_then(|c| idx.iter().position(|&i| i == c))
                    .map(|p| layout[p]);
                let lookup = match key.and_then(|id| table_name_lookup(conj, id, &layout)) {
                    Some(e) => Some(self.expr(e, outer)?),
                    None => None,
                };
                let generated = Generated {
                    name: it.name.clone(),
                    lookup,
                };
                Ok(Planned {
                    op: Box::new(Select {
                        input: Box::new(generated),
                        positions: idx,
                    }),
                    layout,
                })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn base_table(
        &mut self,
        tid: TableId,
        t: &ResolvedTableScanProto,
        hints: &[ResolvedOptionProto],
        layout: &[i64],
        idx: &[usize],
        outer: &Scope,
        conj: &[&AnyResolvedExprProto],
    ) -> Result<BoxOp> {
        let storage = self.cat.storage.clone();
        let tm = storage.table(tid);
        let col_of = |id: i64| layout.iter().position(|&l| l == id).map(|p| idx[p]);
        let mut preds = key_preds(conj, layout);
        let forced = hint_value(hints, "", "FORCE_INDEX").and_then(literal_of);
        let read_at = self.reads.len();
        self.reads.push(crate::prepared::ScanRead {
            table: tid,
            index: None,
            columns: idx.to_vec(),
        });
        let forced = match forced {
            Some(Value::String(name)) => {
                let is_search = self.cat.schema.search_indexes.values().any(|si| {
                    si.name.eq_ignore_ascii_case(&name) && si.table.eq_ignore_ascii_case(&tm.name)
                });
                if is_search && self.search.forced_index.is_none() {
                    self.search.forced_index = Some(name.to_string());
                }
                // Production also takes the undocumented PRIMARY_KEY for the base table; the
                // emulator rejects it (GoogleCloudPlatform/cloud-spanner-emulator#214).
                let primary_key =
                    !self.opts.emulator_compat && name.eq_ignore_ascii_case("PRIMARY_KEY");
                if name.eq_ignore_ascii_case("_BASE_TABLE") || primary_key || is_search {
                    Some(None)
                } else {
                    let ix = storage
                        .index_id(&name)
                        .filter(|&i| storage.index(i).table == tid)
                        .ok_or_else(|| {
                            if !self.opts.emulator_compat {
                                return Error::invalid_argument(format!(
                                    "The table {} does not have an index called {name}",
                                    tm.name
                                ));
                            }
                            let shown = if storage.index_id(&name).is_some() {
                                tm.name.as_str()
                            } else {
                                ""
                            };
                            Error::invalid_argument(format!(
                                "The table {shown} does not have a secondary index called {name}"
                            ))
                        })?;
                    Some(Some(ix))
                }
            }
            _ => None,
        };
        // Cloud Spanner takes SEEKABLE_KEY_SIZE from 1 up to one less than the key columns of
        // the forced index (its own key plus the primary key) or of the table.
        if let Some(Value::Int64(n)) =
            hint_value(hints, "", "SEEKABLE_KEY_SIZE").and_then(literal_of)
        {
            let key_len = match forced {
                Some(Some(ix)) => {
                    let ik = &storage.index(ix).key;
                    ik.len() + tm.pk.iter().filter(|c| !ik.contains(c)).count()
                }
                _ => tm.pk.len(),
            };
            if n == 0 || n >= key_len as i64 {
                return Err(Error::invalid_argument(
                    "Request contains an invalid argument.",
                ));
            }
        }
        let table_key: Vec<Option<i64>> = tm
            .pk
            .iter()
            .map(|&c| {
                layout
                    .iter()
                    .zip(idx)
                    .find(|(_, i)| **i == c)
                    .map(|(id, _)| *id)
            })
            .collect();
        let index_key = |ix: IndexId| -> Vec<Option<i64>> {
            storage
                .index(ix)
                .key
                .iter()
                .map(|&c| {
                    layout
                        .iter()
                        .zip(idx)
                        .find(|(_, i)| **i == c)
                        .map(|(id, _)| *id)
                })
                .collect()
        };
        let (source_ix, choice): (Option<IndexId>, KeyChoice) = match forced {
            Some(Some(ix)) => {
                self.check_null_filtered(ix, tid, t, hints, conj, &col_of)?;
                self.reads[read_at].index = Some(ix);
                (Some(ix), choose_key(&index_key(ix), &mut preds))
            }
            Some(None) => (None, choose_key(&table_key, &mut preds)),
            None => {
                let mut pk_preds: Vec<(i64, Pred)> = key_preds(conj, layout);
                let pk = choose_key(&table_key, &mut pk_preds);
                if !pk.parts.is_empty() || pk.range.is_some() {
                    (None, pk)
                } else {
                    let mut best: Option<(IndexId, KeyChoice)> = None;
                    for &ix in &tm.indexes {
                        let im = storage.index(ix);
                        let mut p = key_preds(conj, layout);
                        let ch = choose_key(&index_key(ix), &mut p);
                        // Every column whose NULL leaves rows out of the index must be a key
                        // part the chosen predicates prove non-NULL.
                        let usable = !ch.parts.is_empty()
                            && im.null_filter.iter().all(|c| {
                                im.key[..ch.parts.len()]
                                    .iter()
                                    .zip(&ch.parts)
                                    .any(|(k, p)| k == c && !matches!(p, Pred::IsNull))
                            });
                        if usable
                            && best
                                .as_ref()
                                .is_none_or(|(_, b)| ch.parts.len() > b.parts.len())
                        {
                            best = Some((ix, ch));
                        }
                    }
                    match best {
                        Some((ix, ch)) => (Some(ix), ch),
                        None => (
                            None,
                            KeyChoice {
                                parts: Vec::new(),
                                range: None,
                            },
                        ),
                    }
                }
            }
        };
        let source = match source_ix {
            None => Source::Table,
            Some(ix) => {
                let im = storage.index(ix);
                let covered: Option<Vec<usize>> = idx
                    .iter()
                    .map(|&c| im.entry_columns.iter().position(|&e| e == c))
                    .collect();
                let pk_entry_pos: Vec<usize> = tm
                    .pk
                    .iter()
                    .map(|&c| im.entry_columns.iter().position(|&e| e == c).unwrap_or(0))
                    .collect();
                Source::Index {
                    index: ix,
                    pk_entry_pos,
                    covered,
                }
            }
        };
        let key_desc: Vec<bool> = match source_ix {
            None => tm.pk_desc.clone(),
            Some(ix) => storage.index(ix).key_desc.clone(),
        };
        // A key or range bound that reads the outer row makes the scan a per-row lookup already.
        let access_correlated = choice.parts.iter().any(|p| match p {
            Pred::Eq(es) => es.iter().any(|e| refs_outside(e, layout)),
            Pred::InArray(e) => refs_outside(e, layout),
            _ => false,
        }) || choice.range.as_ref().is_some_and(|(lo, hi)| {
            [lo, hi]
                .iter()
                .any(|b| b.is_some_and(|(e, _)| refs_outside(e, layout)))
        });
        let mut parts = Vec::new();
        for p in choice.parts {
            parts.push(match p {
                Pred::Eq(es) => KeyPart::Eq(
                    es.iter()
                        .map(|e| self.expr(e, outer))
                        .collect::<Result<_>>()?,
                ),
                Pred::InArray(e) => KeyPart::InArray(self.expr(e, outer)?),
                Pred::IsNull => KeyPart::IsNull,
                _ => return Err(Error::internal("range predicate in key prefix")),
            });
        }
        let range = match choice.range {
            None => None,
            Some((lo, hi)) => {
                let desc = key_desc.get(parts.len()).copied().unwrap_or(false);
                let mut b = |x: Option<(&AnyResolvedExprProto, bool)>| -> Result<Option<Bound>> {
                    x.map(|(e, inclusive)| {
                        Ok(Bound {
                            expr: self.expr(e, outer)?,
                            inclusive,
                        })
                    })
                    .transpose()
                };
                Some(RangePart {
                    lo: b(lo)?,
                    hi: b(hi)?,
                    desc,
                })
            }
        };
        let access = if parts.is_empty() && range.is_none() {
            match self.search_access(tid, source_ix.is_none(), layout, &col_of, outer, conj)? {
                Some(a) => a,
                None => Access::Full,
            }
        } else {
            Access::Key { parts, range }
        };
        let search = matches!(access, Access::Search { .. });
        let scan: BoxOp = Box::new(TableScan {
            table: tid,
            name: tm.name.clone(),
            source,
            access,
            columns: idx.to_vec(),
            key_len: tm.pk.len(),
        });
        if access_correlated || search {
            return Ok(scan);
        }
        self.decorrelate(scan, layout, outer, conj)
    }

    /// Turns a scan that a correlated equality (`inner_col = outer_col`) filters, but that does
    /// not read the outer row itself, into a hash lookup built once per statement. Without it a
    /// correlated subquery or dependent join over a non-key column rescans the table per outer
    /// row. The predicates above the scan still run on every returned row.
    fn decorrelate(
        &mut self,
        scan: BoxOp,
        layout: &[i64],
        outer: &Scope,
        conj: &[&AnyResolvedExprProto],
    ) -> Result<BoxOp> {
        let mut build = Vec::new();
        let mut probe = Vec::new();
        for c in conj {
            let Some(b) = call_base(c) else { continue };
            if fn_name(b) != "$equal" || b.argument_list.len() != 2 {
                continue;
            }
            let a = &b.argument_list;
            for (i, o) in [(0, 1), (1, 0)] {
                let (Some(inner), Some(outer_id)) = (column_ref(&a[i]), column_ref(&a[o])) else {
                    continue;
                };
                let Some(pos) = layout.iter().position(|&l| l == inner) else {
                    continue;
                };
                if layout.contains(&outer_id) || outer.find(outer_id).is_none() {
                    continue;
                }
                let hashable = match (type_of(&a[i]), type_of(&a[o])) {
                    (Ok(t1), Ok(t2)) => types::same(&t1, &t2) && hash_key_type(&t1),
                    _ => false,
                };
                if !hashable || build.contains(&pos) {
                    continue;
                }
                build.push(pos);
                probe.push(self.expr(&a[o], outer)?);
                break;
            }
        }
        if build.is_empty() {
            return Ok(scan);
        }
        Ok(Box::new(HashLookup {
            input: scan,
            build,
            probe,
        }))
    }

    /// A `SEARCH(col, query)` conjunct over a TOKENIZE_FULLTEXT column of a search index on the
    /// table, with a query that does not depend on the scanned row.
    fn search_access(
        &mut self,
        tid: TableId,
        base: bool,
        layout: &[i64],
        col_of: &dyn Fn(i64) -> Option<usize>,
        outer: &Scope,
        conj: &[&AnyResolvedExprProto],
    ) -> Result<Option<Access>> {
        if !base {
            return Ok(None);
        }
        let tm = self.cat.storage.table(tid);
        for c in conj {
            let Some(b) = call_base(c) else { continue };
            let a = &b.argument_list;
            if fn_name(b) != "search"
                || a.len() < 2
                || a.get(4).is_some_and(|d| literal_of(d) != Some(Value::Null))
            {
                continue;
            }
            let Some(col) = column_ref(&a[0]).and_then(col_of) else {
                continue;
            };
            let cm = &tm.columns[col];
            let fulltext = cm.expr.as_deref().is_some_and(|e| {
                e.trim_start()
                    .to_ascii_uppercase()
                    .starts_with("TOKENIZE_FULLTEXT")
            });
            let indexed = self.cat.schema.search_indexes.values().any(|si| {
                si.table.eq_ignore_ascii_case(&tm.name)
                    && si.columns.iter().any(|n| n.eq_ignore_ascii_case(&cm.name))
            });
            if !fulltext || !indexed || !is_const(&a[1], layout) {
                continue;
            }
            let query = self.expr(&a[1], outer)?;
            return Ok(Some(Access::Search {
                column: col,
                query,
                emulator: self.opts.emulator_compat,
            }));
        }
        Ok(None)
    }

    /// The rows a DML statement targets: every column of the table, in schema order, filtered by
    /// `where_expr`. The layout gives table-scan column ids to the columns the statement names.
    pub fn dml_scan(
        &mut self,
        ts: &ResolvedTableScanProto,
        where_expr: Option<&AnyResolvedExprProto>,
    ) -> Result<(BoxOp, Vec<i64>, TableId)> {
        let base = ts
            .parent
            .as_ref()
            .ok_or_else(|| Error::internal("table scan without base"))?;
        let tref = ts
            .table
            .as_ref()
            .ok_or_else(|| Error::internal("table scan without table"))?;
        let full = tref
            .full_name
            .as_deref()
            .or(tref.name.as_deref())
            .unwrap_or("");
        let Some(Relation::Table(tid)) = self.cat.relation(full).cloned() else {
            return Err(unimplemented(format!("DML on {full}")));
        };
        let n = self.cat.storage.table(tid).columns.len();
        let mut layout: Vec<i64> = (0..n).map(|c| -1_000_000 - c as i64).collect();
        for (c, &i) in base.column_list.iter().zip(&ts.column_index_list) {
            if let Some(slot) = layout.get_mut(i as usize) {
                *slot = c.column_id.unwrap_or(-1);
            }
        }
        let idx: Vec<usize> = (0..n).collect();
        let mut conj = Vec::new();
        if let Some(w) = where_expr {
            conjuncts(w, &mut conj);
        }
        self.hint_rules.check(Node::TableScan, &base.hint_list)?;
        let read_at = self.reads.len();
        let mut op =
            self.base_table(tid, ts, &base.hint_list, &layout, &idx, &Scope::TOP, &conj)?;
        // The scan reads the whole row, but the statement reads only the columns its WHERE
        // names (SET values and THEN RETURN are checked by the caller).
        self.reads[read_at] = crate::prepared::ScanRead {
            table: tid,
            index: None,
            columns: where_expr.map_or_else(Vec::new, |w| columns_read(&[w], ts, &layout)),
        };
        if let Some(w) = where_expr {
            let pred = self.expr(
                w,
                &Scope {
                    cols: &layout,
                    up: Some(&Scope::TOP),
                },
            )?;
            op = Box::new(Filter { input: op, pred });
        }
        Ok((op, layout, tid))
    }

    /// Real Spanner rejects FORCE_INDEX on a NULL_FILTERED or filtered index unless the WHERE
    /// clause proves every column whose NULL leaves rows out non-NULL (see `not_null_ids`).
    fn check_null_filtered(
        &self,
        ix: IndexId,
        _tid: TableId,
        _t: &ResolvedTableScanProto,
        hints: &[ResolvedOptionProto],
        conj: &[&AnyResolvedExprProto],
        col_of: &dyn Fn(i64) -> Option<usize>,
    ) -> Result<()> {
        let storage = &self.cat.storage;
        let im = storage.index(ix);
        if im.null_filter.is_empty() {
            return Ok(());
        }
        if self.opts.emulator_compat
            && (self.stmt_disable_nf_check
                || hint_bool(
                    hints,
                    "spanner_emulator",
                    "disable_query_null_filtered_index_check",
                ))
        {
            return Ok(());
        }
        let filtered: Vec<usize> = not_null_ids(conj).into_iter().filter_map(col_of).collect();
        match im.null_filter.iter().find(|c| !filtered.contains(c)) {
            None => Ok(()),
            Some(&c) => Err(Error::invalid_argument(format!(
                "The index {} cannot be used because it filters nulls that are required to answer the query for column {}.",
                im.name,
                storage.table(im.table).columns[c].name
            ))),
        }
    }

    /// `pushed`: conjuncts of filters above the join, used only to narrow key ranges below it.
    fn join(
        &mut self,
        j: &ResolvedJoinScanProto,
        outer: &Scope,
        pushed: Vec<&AnyResolvedExprProto>,
    ) -> Result<Planned> {
        if let Some(b) = &j.parent {
            self.hint_rules.check(Node::JoinScan, &b.hint_list)?;
        }
        let kind = match j.join_type.and_then(|t| JoinType::try_from(t).ok()) {
            Some(JoinType::Left) => JoinKind::Left,
            Some(JoinType::Right) => JoinKind::Right,
            Some(JoinType::Full) => JoinKind::Full,
            _ => JoinKind::Inner,
        };
        let ls = j
            .left_scan
            .as_deref()
            .ok_or_else(|| Error::internal("join without left"))?;
        let rs = j
            .right_scan
            .as_deref()
            .ok_or_else(|| Error::internal("join without right"))?;
        let left_ids = scan_columns(ls);
        let right_ids = scan_columns(rs);
        let keeps_left = matches!(kind, JoinKind::Inner | JoinKind::Left);
        let to_left: Vec<&AnyResolvedExprProto> = if keeps_left {
            pushed
                .iter()
                .copied()
                .filter(|c| refs_only(c, &left_ids))
                .collect()
        } else {
            Vec::new()
        };
        let left = self.scan_with(ls, outer, ScanCtx { conj: to_left })?;
        let mut conj = Vec::new();
        if let Some(c) = j.join_expr.as_deref() {
            conjuncts(c, &mut conj);
        }
        if kind == JoinKind::Inner {
            let both: Vec<i64> = left_ids.iter().chain(&right_ids).copied().collect();
            conj.extend(
                pushed
                    .iter()
                    .copied()
                    .filter(|c| refs_only(c, &both) && refs_any(c, &right_ids)),
            );
        }
        let lateral = !j.parameter_list.is_empty() || j.is_lateral == Some(true);
        // Index nested loop: the right side is a table whose key the ON clause binds to left columns.
        let index_nl = matches!(kind, JoinKind::Inner | JoinKind::Left)
            && self.right_uses_left_key(rs, &conj, &left.layout);
        if index_nl || lateral {
            if matches!(kind, JoinKind::Right | JoinKind::Full) {
                return Err(unimplemented(
                    "RIGHT/FULL JOIN with a correlated right side",
                ));
            }
            let lscope = Scope {
                cols: &left.layout,
                up: Some(outer),
            };
            let right = self.scan_with(
                rs,
                &lscope,
                ScanCtx {
                    conj: if index_nl { conj.clone() } else { Vec::new() },
                },
            )?;
            let mut layout = left.layout.clone();
            layout.extend(&right.layout);
            let cond = match j.join_expr.as_deref() {
                Some(c) => Some(self.expr(
                    c,
                    &Scope {
                        cols: &layout,
                        up: Some(outer),
                    },
                )?),
                None => None,
            };
            return Ok(Planned {
                op: Box::new(NestedLoop {
                    left_width: left.layout.len(),
                    right_width: right.layout.len(),
                    left: left.op,
                    right: right.op,
                    kind,
                    cond,
                    dependent: true,
                }),
                layout,
            });
        }
        let right_only: Vec<&AnyResolvedExprProto> = conj
            .iter()
            .copied()
            .filter(|c| refs_only(c, &right_ids))
            .collect();
        let right = self.scan_with(rs, outer, ScanCtx { conj: right_only })?;
        let mut layout = left.layout.clone();
        layout.extend(&right.layout);
        let both = Scope {
            cols: &layout,
            up: Some(outer),
        };
        let cond = match j.join_expr.as_deref() {
            Some(c) => Some(self.expr(c, &both)?),
            None => None,
        };
        // Equi-join keys.
        let mut lk = Vec::new();
        let mut rk = Vec::new();
        for c in &conj {
            if let Some(b) = call_base(c)
                && fn_name(b) == "$equal"
                && b.argument_list.len() == 2
            {
                let (a0, a1) = (&b.argument_list[0], &b.argument_list[1]);
                let pair = if refs_only(a0, &left.layout)
                    && refs_only(a1, &right_ids)
                    && refs_any(a0, &left.layout)
                    && refs_any(a1, &right_ids)
                {
                    Some((a0, a1))
                } else if refs_only(a1, &left.layout)
                    && refs_only(a0, &right_ids)
                    && refs_any(a1, &left.layout)
                    && refs_any(a0, &right_ids)
                {
                    Some((a1, a0))
                } else {
                    None
                };
                if let Some((l, r)) = pair {
                    lk.push(self.expr(
                        l,
                        &Scope {
                            cols: &left.layout,
                            up: Some(outer),
                        },
                    )?);
                    rk.push(self.expr(
                        r,
                        &Scope {
                            cols: &right.layout,
                            up: Some(outer),
                        },
                    )?);
                }
            }
        }
        let op: BoxOp = if !lk.is_empty() {
            Box::new(HashJoin {
                left_width: left.layout.len(),
                right_width: right.layout.len(),
                left: left.op,
                right: right.op,
                kind,
                left_keys: lk,
                right_keys: rk,
                residual: cond,
            })
        } else {
            Box::new(NestedLoop {
                left_width: left.layout.len(),
                right_width: right.layout.len(),
                left: left.op,
                right: right.op,
                kind,
                cond,
                dependent: false,
            })
        };
        Ok(Planned { op, layout })
    }

    /// Whether the right scan is a (filtered) base table whose PK or an index has its leading
    /// column bound by an equality to the left side.
    fn right_uses_left_key(
        &self,
        rs: &AnyResolvedScanProto,
        conj: &[&AnyResolvedExprProto],
        left: &[i64],
    ) -> bool {
        let mut all = conj.to_vec();
        let mut cur = rs;
        loop {
            match cur.node.as_ref() {
                Some(S::ResolvedFilterScanNode(f)) => {
                    if let Some(p) = f.filter_expr.as_deref() {
                        conjuncts(p, &mut all);
                    }
                    match f.input_scan.as_deref() {
                        Some(i) => cur = i,
                        None => return false,
                    }
                }
                Some(S::ResolvedTableScanNode(t)) => {
                    let Some(base) = t.parent.as_ref() else {
                        return false;
                    };
                    let layout: Vec<i64> = base
                        .column_list
                        .iter()
                        .map(|c| c.column_id.unwrap_or(-1))
                        .collect();
                    let idx: Vec<usize> = t.column_index_list.iter().map(|&i| i as usize).collect();
                    let full = t
                        .table
                        .as_ref()
                        .and_then(|r| r.full_name.as_deref().or(r.name.as_deref()))
                        .unwrap_or("");
                    let Some(Relation::Table(tid)) = self.cat.relation(full) else {
                        return false;
                    };
                    let tm = self.cat.storage.table(*tid);
                    let preds = key_preds(&all, &layout);
                    let binds_left = |id: i64| {
                        preds.iter().any(|(pid, p)| {
                            *pid == id
                                && match p {
                                    Pred::Eq(es) => es.iter().any(|e| refs_any(e, left)),
                                    Pred::InArray(e) => refs_any(e, left),
                                    _ => false,
                                }
                        })
                    };
                    let id_of = |c: usize| {
                        layout
                            .iter()
                            .zip(&idx)
                            .find(|(_, i)| **i == c)
                            .map(|(id, _)| *id)
                    };
                    if let Some(id) = tm.pk.first().and_then(|&c| id_of(c))
                        && binds_left(id)
                    {
                        return true;
                    }
                    return tm.indexes.iter().any(|&ix| {
                        let im = self.cat.storage.index(ix);
                        im.null_filter.is_empty()
                            && im
                                .key
                                .first()
                                .and_then(|&c| id_of(c))
                                .is_some_and(binds_left)
                    });
                }
                _ => return false,
            }
        }
    }

    fn array_scan(&mut self, a: &ResolvedArrayScanProto, outer: &Scope) -> Result<Planned> {
        if let Some(b) = &a.parent {
            self.hint_rules.check(Node::ArrayScan, &b.hint_list)?;
        }
        if a.array_expr_list.len() != 1 {
            return Err(unimplemented("UNNEST of more than one array"));
        }
        let input = match a.input_scan.as_deref() {
            Some(i) => Some(self.scan(i, outer)?),
            None => None,
        };
        let in_layout: Vec<i64> = input.as_ref().map(|p| p.layout.clone()).unwrap_or_default();
        let scope = Scope {
            cols: &in_layout,
            up: Some(outer),
        };
        let array = self.expr(&a.array_expr_list[0], &scope)?;
        let mut layout = in_layout.clone();
        layout.push(
            a.element_column_list
                .first()
                .and_then(|c| c.column_id)
                .unwrap_or(-1),
        );
        let with_offset = a.array_offset_column.is_some();
        if let Some(o) = &a.array_offset_column {
            layout.push(nid(&o.column));
        }
        let cond = match a.join_expr.as_deref() {
            Some(c) => Some(self.expr(
                c,
                &Scope {
                    cols: &layout,
                    up: Some(outer),
                },
            )?),
            None => None,
        };
        Ok(Planned {
            op: Box::new(Unnest {
                input: input.map(|p| p.op),
                array,
                with_offset,
                outer: a.is_outer == Some(true),
                cond,
            }),
            layout,
        })
    }

    fn aggregate(&mut self, a: &ResolvedAggregateScanBaseProto, outer: &Scope) -> Result<Planned> {
        if let Some(b) = &a.parent {
            self.hint_rules.check(Node::AggregateScan, &b.hint_list)?;
        }
        if !a.grouping_set_list.is_empty() || !a.rollup_column_list.is_empty() {
            return Err(unimplemented("GROUP BY ROLLUP/CUBE/GROUPING SETS"));
        }
        let input = a
            .input_scan
            .as_deref()
            .ok_or_else(|| Error::internal("aggregate without input"))?;
        let inp = self.scan(input, outer)?;
        let scope = Scope {
            cols: &inp.layout,
            up: Some(outer),
        };
        let mut layout = Vec::new();
        let mut group = Vec::new();
        for g in &a.group_by_list {
            let e = g
                .expr
                .as_ref()
                .ok_or_else(|| Error::internal("group by without expr"))?;
            group.push(self.expr(e, &scope)?);
            layout.push(nid(&g.column));
        }
        let mut aggs = Vec::new();
        for c in &a.aggregate_list {
            let cc = computed(c).ok_or_else(|| unimplemented("This aggregate column"))?;
            let e = cc
                .expr
                .as_ref()
                .ok_or_else(|| Error::internal("aggregate without expr"))?;
            aggs.push(self.agg_spec(e, &scope, outer)?);
            layout.push(nid(&cc.column));
        }
        Ok(Planned {
            op: Box::new(Aggregate {
                input: inp.op,
                group,
                aggs,
            }),
            layout,
        })
    }

    fn agg_spec(
        &mut self,
        e: &AnyResolvedExprProto,
        scope: &Scope,
        outer: &Scope,
    ) -> Result<AggSpec> {
        let call = match e.node.as_ref() {
            Some(E::ResolvedFunctionCallBaseNode(b)) => match b.node.as_ref() {
                Some(FB::ResolvedNonScalarFunctionCallBaseNode(n)) => match n.node.as_ref() {
                    Some(NS::ResolvedAggregateFunctionCallNode(a)) => a,
                    _ => return Err(unimplemented("Window (analytic) functions")),
                },
                _ => return Err(Error::internal("aggregate column is not an aggregate call")),
            },
            _ => return Err(Error::internal("aggregate column is not an aggregate call")),
        };
        let ns = call
            .parent
            .as_ref()
            .ok_or_else(|| Error::internal("aggregate without base"))?;
        let base = ns
            .parent
            .as_ref()
            .ok_or_else(|| Error::internal("aggregate without base"))?;
        if call.having_modifier.is_some() {
            return Err(unimplemented("HAVING MAX/MIN in aggregates"));
        }
        if ns.where_expr.is_some() || !call.group_by_list.is_empty() {
            return Err(unimplemented("This aggregate modifier"));
        }
        crate::validate::function(full_fn_name(base))?;
        let kind = AggKind::parse(fn_name(base))?;
        let mut args = Vec::new();
        for a in &base.argument_list {
            args.push(self.expr(a, scope)?);
        }
        let mut order = Vec::new();
        for it in &call.order_by_item_list {
            let id = it.column_ref.as_ref().map(|c| nid(&c.column)).unwrap_or(-1);
            let (d, i) = scope
                .find(id)
                .ok_or_else(|| Error::internal("aggregate ORDER BY column not found"))?;
            let desc = it.is_descending == Some(true);
            let nulls_first = check_null_order(it)?;
            order.push(SortKey {
                expr: Arc::new(move |_: &Cx, e: &Env| Ok(e.get(d, i).clone())),
                desc,
                nulls_first,
            });
        }
        let limit = call
            .limit
            .as_deref()
            .map(|l| self.expr(l, outer))
            .transpose()?;
        let ret = type_of(e)?;
        Ok(AggSpec {
            kind,
            args,
            distinct: ns.distinct == Some(true),
            ignore_nulls: ns
                .null_handling_modifier
                .and_then(|m| NullHandlingModifier::try_from(m).ok())
                == Some(NullHandlingModifier::IgnoreNulls),
            order,
            limit,
            ret,
        })
    }

    // ---------------------------------------------------------------- expressions

    /// Registers a parameter the statement uses; its type is the analysed one.
    pub fn param_index(&mut self, name: &str, ty: Type) -> usize {
        let key = name.to_ascii_lowercase();
        if let Some(&i) = self.params.get(&key) {
            return i;
        }
        let i = self.param_types.len();
        self.param_types.push(Param {
            name: name.to_string(),
            ty,
        });
        self.params.insert(key, i);
        i
    }

    pub fn expr(&mut self, e: &AnyResolvedExprProto, scope: &Scope) -> Result<Eval> {
        let node = e
            .node
            .as_ref()
            .ok_or_else(|| Error::internal("empty expression"))?;
        Ok(match node {
            E::ResolvedLiteralNode(l) => {
                let v = l
                    .value
                    .as_ref()
                    .ok_or_else(|| Error::internal("literal without value"))?;
                let tp = v
                    .r#type
                    .as_ref()
                    .ok_or_else(|| Error::internal("literal without type"))?;
                // The emulator's query validator rejects any NULL ARRAY<STRUCT> literal, as in
                // `CASE ... ELSE NULL END`. Production rejects only result columns that can be a
                // NULL ARRAY<STRUCT>, wherever the NULL comes from (`struct_result`).
                if self.opts.emulator_compat
                    && v.value.as_ref().is_none_or(|x| x.value.is_none())
                    && crate::struct_shape::is_struct_array(tp)
                {
                    return Err(Error::new(Code::Unimplemented, EMULATOR_NULL_STRUCT_ARRAY));
                }
                let ty = types::from_proto(tp)?;
                let val = crate::literal::value(v.value.as_ref(), &ty)?;
                Arc::new(move |_: &Cx, _: &Env| Ok(val.clone()))
            }
            E::ResolvedParameterNode(p) => {
                let name = p.name.clone().unwrap_or_default();
                let i = self.param_index(&name, type_of(e)?);
                Arc::new(move |cx: &Cx, _: &Env| {
                    Ok(cx.params.get(i).cloned().unwrap_or(Value::Null))
                })
            }
            E::ResolvedColumnRefNode(c) => {
                let id = nid(&c.column);
                let (d, i) = scope
                    .find(id)
                    .ok_or_else(|| Error::internal(format!("column {id} not in scope")))?;
                if d == 0 {
                    Arc::new(move |_: &Cx, e: &Env| {
                        Ok(e.row.get(i).cloned().unwrap_or(Value::Null))
                    })
                } else {
                    Arc::new(move |_: &Cx, e: &Env| Ok(e.get(d, i).clone()))
                }
            }
            E::ResolvedFunctionCallBaseNode(b) => match b.node.as_ref() {
                Some(FB::ResolvedFunctionCallNode(fc)) => {
                    let base = fc
                        .parent
                        .as_ref()
                        .ok_or_else(|| Error::internal("call without base"))?;
                    self.call(base, scope)?
                }
                _ => {
                    return Err(unimplemented(
                        "Aggregate or window functions in this position",
                    ));
                }
            },
            E::ResolvedCastNode(c) => {
                if c.format.is_some() || c.time_zone.is_some() {
                    return Err(unimplemented("CAST with FORMAT or time zone"));
                }
                let inner = c
                    .expr
                    .as_deref()
                    .ok_or_else(|| Error::internal("cast without expr"))?;
                if graph_plan::is_graph_type(expr_type(e)) {
                    return self.expr(inner, scope);
                }
                let from = type_of(inner)?;
                let to = type_of(e)?;
                let x = self.expr(inner, scope)?;
                let safe = c.return_null_on_error == Some(true);
                // Validate the cast pair at plan time where possible.
                Arc::new(move |cx: &Cx, env: &Env| {
                    let v = x(cx, env)?;
                    match crate::cast::cast(&v, &from, &to) {
                        Ok(r) => Ok(r),
                        Err(e) if safe && e.code != Code::Unimplemented => Ok(Value::Null),
                        Err(e) => Err(e),
                    }
                })
            }
            E::ResolvedMakeStructNode(m) => {
                let fields = m
                    .field_list
                    .iter()
                    .map(|f| self.expr(f, scope))
                    .collect::<Result<Vec<_>>>()?;
                Arc::new(move |cx: &Cx, env: &Env| {
                    let mut v = Vec::with_capacity(fields.len());
                    for f in &fields {
                        v.push(f(cx, env)?);
                    }
                    Ok(Value::Struct(Arc::from(v)))
                })
            }
            E::ResolvedGetStructFieldNode(g) => {
                let inner = g
                    .expr
                    .as_deref()
                    .ok_or_else(|| Error::internal("field access without expr"))?;
                let x = self.expr(inner, scope)?;
                let i = g.field_idx.unwrap_or(0) as usize;
                Arc::new(move |cx: &Cx, env: &Env| match x(cx, env)? {
                    Value::Struct(f) => Ok(f.get(i).cloned().unwrap_or(Value::Null)),
                    _ => Ok(Value::Null),
                })
            }
            E::ResolvedGetJsonFieldNode(g) => {
                let inner = g
                    .expr
                    .as_deref()
                    .ok_or_else(|| Error::internal("json field without expr"))?;
                let x = self.expr(inner, scope)?;
                let name = g.field_name.clone().unwrap_or_default();
                Arc::new(move |cx: &Cx, env: &Env| match x(cx, env)? {
                    Value::Json(j) => Ok(crate::func_json::member(&j, &name)),
                    _ => Ok(Value::Null),
                })
            }
            E::ResolvedSubqueryExprNode(s) => self.subquery(s, scope)?,
            E::ResolvedGraphGetElementPropertyNode(g) => self.graph_property(g, scope)?,
            E::ResolvedWithExprNode(w) => {
                let ids: Vec<i64> = w.assignment_list.iter().map(|a| nid(&a.column)).collect();
                let inner = Scope {
                    cols: &ids,
                    up: Some(scope),
                };
                let mut assigns = Vec::new();
                for a in &w.assignment_list {
                    let ex = a
                        .expr
                        .as_ref()
                        .ok_or_else(|| Error::internal("WITH assignment without expr"))?;
                    assigns.push(self.expr(ex, &inner)?);
                }
                let body = w
                    .expr
                    .as_deref()
                    .ok_or_else(|| Error::internal("WITH without expr"))?;
                let body = self.expr(body, &inner)?;
                let n = assigns.len();
                Arc::new(move |cx: &Cx, env: &Env| {
                    let mut vals = vec![Value::Null; n];
                    for (i, a) in assigns.iter().enumerate() {
                        let v = {
                            let e = Env {
                                row: &vals,
                                up: Some(env),
                            };
                            a(cx, &e)?
                        };
                        vals[i] = v;
                    }
                    let e = Env {
                        row: &vals,
                        up: Some(env),
                    };
                    body(cx, &e)
                })
            }
            other => return Err(unimplemented(format!("Expression {}", node_name(other)))),
        })
    }

    /// Cloud Spanner allows SEARCH, SEARCH_SUBSTRING and SEARCH_NGRAMS only as predicates, not
    /// in a SELECT list, and SCORE only next to a SEARCH over the same column.
    /// Cloud Spanner evaluates SEARCH, SEARCH_SUBSTRING and SEARCH_NGRAMS only through a search
    /// index on the column, and uses a search index only when the WHERE clause fixes every
    /// partition column and implies the index's `WHERE ... IS NOT NULL` filter.
    fn check_search_index(&self, conj: &[&AnyResolvedExprProto]) -> Result<()> {
        if self.opts.emulator_compat {
            return Ok(());
        }
        let mut calls = Vec::new();
        for c in conj {
            search_calls(c, &mut calls);
        }
        if calls.is_empty() {
            return Ok(());
        }
        let (fixed, not_null) = fixed_columns(conj);
        let mut refs = Vec::new();
        for (name, arg) in calls {
            refs.clear();
            tokenlist_refs(arg, &mut refs);
            for r in &refs {
                self.check_search_column(name, r, &fixed, &not_null)?;
            }
        }
        Ok(())
    }

    fn check_search_column(
        &self,
        name: &str,
        arg: &AnyResolvedExprProto,
        fixed: &std::collections::HashSet<(String, String)>,
        not_null: &std::collections::HashSet<(String, String)>,
    ) -> Result<()> {
        let schema = &self.cat.schema;
        {
            let (Some((table, column)), Some(E::ResolvedColumnRefNode(cr))) =
                (tokenlist_column(Some(arg)), arg.node.as_ref())
            else {
                return Ok(());
            };
            // Columns of WITH entries, views and subqueries are not table columns.
            let Some(tm) = schema
                .tables
                .iter()
                .find(|(t, _)| t.eq_ignore_ascii_case(&table))
                .map(|(_, t)| t)
            else {
                return Ok(());
            };
            // A column from the wrong tokenizer fails on its own, before the index is chosen.
            let tokenizer = tm
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&column))
                .and_then(|c| match &c.generated {
                    Some(spanbolt_ddl::Generated::Expr { expr, .. }) => {
                        Some(expr.trim_start().to_ascii_uppercase())
                    }
                    _ => None,
                })
                .unwrap_or_default();
            let fits = match name {
                "search" => tokenizer.starts_with("TOKENIZE_FULLTEXT"),
                "search_substring" => tokenizer.starts_with("TOKENIZE_SUBSTRING"),
                _ => {
                    tokenizer.starts_with("TOKENIZE_NGRAMS")
                        || tokenizer.starts_with("TOKENIZE_SUBSTRING")
                }
            };
            if !fits {
                return Ok(());
            }
            let indexes: Vec<_> = schema
                .search_indexes
                .values()
                .filter(|si| {
                    si.table.eq_ignore_ascii_case(&table)
                        && si.columns.iter().any(|c| c.eq_ignore_ascii_case(&column))
                })
                .collect();
            if indexes.is_empty() {
                let col = cr
                    .column
                    .as_ref()
                    .and_then(|c| c.name.clone())
                    .unwrap_or(column);
                return Err(Error::invalid_argument(format!(
                    "{} cannot be used because there is no search index indexing the column {col}, or the query is executed with base table only hint.",
                    name.to_ascii_uppercase()
                )));
            }
            let key = |c: &str| (table.clone(), c.to_ascii_lowercase());
            let usable = indexes.iter().any(|si| {
                si.partition_by.iter().all(|p| fixed.contains(&key(p)))
                    && si
                        .filter
                        .as_deref()
                        .and_then(filter_not_null)
                        .unwrap_or_default()
                        .iter()
                        .all(|c| not_null.contains(&key(c)) || fixed.contains(&key(c)))
            });
            if !usable {
                return Err(not_supported_in_query(name));
            }
        }
        Ok(())
    }

    /// A misplaced call over a computed TOKENLIST (`SEARCH(TOKENIZE_FULLTEXT('…'), …)`) returns
    /// its error to raise after the arguments, whose own errors Cloud Spanner reports first.
    fn search_placement(
        &mut self,
        name: &str,
        b: &ResolvedFunctionCallBaseProto,
    ) -> Result<Option<Error>> {
        match name {
            "search" | "search_substring" | "search_ngrams" => {
                if self.search_rules.select_list {
                    let e = Error::invalid_argument(format!(
                        "The function {} can only be used in a WHERE clause.",
                        name.to_ascii_uppercase()
                    ));
                    if b.argument_list.first().and_then(column_ref).is_some() {
                        return Err(e);
                    }
                    return Ok(Some(e));
                }
                if name == "search"
                    && let Some(c) = tokenlist_column(b.argument_list.first())
                {
                    self.search_rules.search_cols.insert(c);
                }
            }
            "score" => {
                // A BYTES query fails on its own.
                let bytes = b
                    .argument_list
                    .get(1)
                    .and_then(|a| type_of(a).ok())
                    .is_some_and(|t| matches!(t, Type::Bytes(..)));
                if !bytes {
                    match tokenlist_column(b.argument_list.first()) {
                        Some(c) => self.search_rules.score_cols.push(c),
                        None => return Ok(Some(escorer())),
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn call(&mut self, b: &ResolvedFunctionCallBaseProto, scope: &Scope) -> Result<Eval> {
        self.hint_rules.check(Node::FunctionCall, &b.hint_list)?;
        crate::validate::function(full_fn_name(b))?;
        let name = fn_name(b).to_string();
        if crate::search::is_search_function(&name) && self.search.function.is_none() {
            self.search.function = Some(name.clone());
        }
        let misplaced = if self.opts.emulator_compat {
            None
        } else {
            self.search_placement(&name, b)?
        };
        if let Some(ev) = self.graph_call(&name, b, scope)? {
            return Ok(ev);
        }
        let safe = b.error_mode.and_then(|m| ErrorMode::try_from(m).ok())
            == Some(ErrorMode::SafeErrorMode);
        let ret = types::from_proto(
            b.parent
                .as_ref()
                .and_then(|p| p.r#type.as_ref())
                .ok_or_else(|| Error::internal("call without type"))?,
        )?;
        if !b.generic_argument_list.is_empty() {
            let ev = self.lambda_call(&name, b, scope)?;
            return Ok(if safe { safe_call(ev) } else { ev });
        }
        let mut args = Vec::with_capacity(b.argument_list.len());
        let mut arg_types = Vec::with_capacity(b.argument_list.len());
        for a in &b.argument_list {
            args.push(self.expr(a, scope)?);
            arg_types.push(type_of(a)?);
        }
        if let Some(e) = misplaced {
            return Ok(Arc::new(move |cx: &Cx, env: &Env| {
                for a in &args {
                    a(cx, env)?;
                }
                Err(e.clone())
            }));
        }
        let ev: Eval = match name.as_str() {
            "$and" => Arc::new(move |cx: &Cx, env: &Env| {
                let mut unknown = false;
                for a in &args {
                    match a(cx, env)? {
                        Value::Bool(false) => return Ok(Value::Bool(false)),
                        Value::Null => unknown = true,
                        _ => {}
                    }
                }
                Ok(if unknown {
                    Value::Null
                } else {
                    Value::Bool(true)
                })
            }),
            "$or" => Arc::new(move |cx: &Cx, env: &Env| {
                let mut unknown = false;
                for a in &args {
                    match a(cx, env)? {
                        Value::Bool(true) => return Ok(Value::Bool(true)),
                        Value::Null => unknown = true,
                        _ => {}
                    }
                }
                Ok(if unknown {
                    Value::Null
                } else {
                    Value::Bool(false)
                })
            }),
            "$case_no_value" => Arc::new(move |cx: &Cx, env: &Env| {
                let mut i = 0;
                while i + 1 < args.len() {
                    if truthy(&args[i](cx, env)?) {
                        return args[i + 1](cx, env);
                    }
                    i += 2;
                }
                if i < args.len() {
                    args[i](cx, env)
                } else {
                    Ok(Value::Null)
                }
            }),
            "$case_with_value" => Arc::new(move |cx: &Cx, env: &Env| {
                let v = args[0](cx, env)?;
                let mut i = 1;
                while i + 1 < args.len() {
                    let w = args[i](cx, env)?;
                    if crate::value_ops::sql_eq(&v, &w) == Some(true) {
                        return args[i + 1](cx, env);
                    }
                    i += 2;
                }
                if i < args.len() {
                    args[i](cx, env)
                } else {
                    Ok(Value::Null)
                }
            }),
            "if" => Arc::new(move |cx: &Cx, env: &Env| {
                if truthy(&args[0](cx, env)?) {
                    args[1](cx, env)
                } else {
                    args[2](cx, env)
                }
            }),
            "coalesce" | "ifnull" => Arc::new(move |cx: &Cx, env: &Env| {
                for a in &args {
                    let v = a(cx, env)?;
                    if !v.is_null() {
                        return Ok(v);
                    }
                }
                Ok(Value::Null)
            }),
            "nullif" => Arc::new(move |cx: &Cx, env: &Env| {
                let a = args[0](cx, env)?;
                let b = args[1](cx, env)?;
                Ok(if crate::value_ops::sql_eq(&a, &b) == Some(true) {
                    Value::Null
                } else {
                    a
                })
            }),
            "iferror" => Arc::new(move |cx: &Cx, env: &Env| match args[0](cx, env) {
                Ok(v) => Ok(v),
                Err(e) if e.code != Code::Unimplemented => args[1](cx, env),
                Err(e) => Err(e),
            }),
            "pending_commit_timestamp" => {
                return Err(Error::invalid_argument(PENDING_COMMIT_TIMESTAMP_USE));
            }
            _ => {
                let sig = Sig {
                    name: &name,
                    args: &arg_types,
                    ret: &ret,
                    emulator_compat: self.opts.emulator_compat,
                };
                let f = func::lookup(&sig).ok_or_else(|| match name.as_str() {
                    // The emulator's own wording for calls it filters out.
                    "to_json_string" => Error::new(
                        Code::Unimplemented,
                        format!(
                            "TO_JSON_STRING is not supported on values of type {}",
                            arg_types
                                .first()
                                .map(types::spanner_name)
                                .unwrap_or_default()
                        ),
                    ),
                    "to_base32" | "from_base32" => Error::new(
                        Code::Unimplemented,
                        format!("Unsupported built-in function: {name}"),
                    ),
                    // The emulator's check before any vector index lookup (which spanbolt lacks).
                    "approx_cosine_distance"
                    | "approx_euclidean_distance"
                    | "approx_dot_product"
                        if arg_types.len() == 2 =>
                    {
                        Error::invalid_argument(format!(
                            "Argument `options` is required for function {}.",
                            name.to_uppercase()
                        ))
                    }
                    _ => unimplemented(format!(
                        "Function {}",
                        name.trim_start_matches('$').to_uppercase()
                    )),
                })?;
                match args.len() {
                    1 => {
                        let a0 = args.remove(0);
                        Arc::new(move |cx: &Cx, env: &Env| {
                            let v = [a0(cx, env)?];
                            f(cx, &v)
                        })
                    }
                    2 => {
                        let a1 = args.remove(1);
                        let a0 = args.remove(0);
                        Arc::new(move |cx: &Cx, env: &Env| {
                            let v = [a0(cx, env)?, a1(cx, env)?];
                            f(cx, &v)
                        })
                    }
                    _ => Arc::new(move |cx: &Cx, env: &Env| {
                        let mut v = Vec::with_capacity(args.len());
                        for a in &args {
                            v.push(a(cx, env)?);
                        }
                        f(cx, &v)
                    }),
                }
            }
        };
        Ok(if safe { safe_call(ev) } else { ev })
    }

    /// A call whose arguments include an inline lambda (`ARRAY_FILTER(a, e -> ...)`): the
    /// lambda's arguments are bound as a scope of their own, like a WITH expression's.
    fn lambda_call(
        &mut self,
        name: &str,
        b: &ResolvedFunctionCallBaseProto,
        scope: &Scope,
    ) -> Result<Eval> {
        let unsupported = || {
            unimplemented(format!(
                "Function {} with lambda or named arguments",
                name.to_uppercase()
            ))
        };
        // The emulator rejects every lambda call that makes an ARRAY<STRUCT>, wherever it is.
        if self.opts.emulator_compat
            && b.parent
                .as_ref()
                .and_then(|p| p.r#type.as_ref())
                .is_some_and(crate::struct_shape::is_struct_array)
        {
            return Err(Error::new(Code::Unimplemented, EMULATOR_NULL_STRUCT_ARRAY));
        }
        let [arr, lam] = b.generic_argument_list.as_slice() else {
            return Err(unsupported());
        };
        let (Some(arr), Some(lam)) = (arr.expr.as_ref(), lam.inline_lambda.as_ref()) else {
            return Err(unsupported());
        };
        let arr = self.expr(arr, scope)?;
        let ids: Vec<i64> = lam
            .argument_list
            .iter()
            .map(|c| c.column_id.unwrap_or(-1))
            .collect();
        let inner = Scope {
            cols: &ids,
            up: Some(scope),
        };
        let body = lam
            .body
            .as_ref()
            .ok_or_else(|| Error::internal("lambda without body"))?;
        let body = self.expr(body, &inner)?;
        let with_index = ids.len() > 1;
        let lambda: crate::func_array::Lambda =
            Arc::new(move |cx: &Cx, env: &Env, x: &Value, i: usize| {
                let vals = if with_index {
                    vec![x.clone(), Value::Int64(i as i64)]
                } else {
                    vec![x.clone()]
                };
                body(
                    cx,
                    &Env {
                        row: &vals,
                        up: Some(env),
                    },
                )
            });
        crate::func_array::with_lambda(name, arr, lambda).ok_or_else(unsupported)
    }

    fn subquery(&mut self, s: &ResolvedSubqueryExprProto, scope: &Scope) -> Result<Eval> {
        self.hint_rules.check(Node::Subquery, &s.hint_list)?;
        self.subqueries += 1;
        let kind = s
            .subquery_type
            .and_then(|t| SubqueryType::try_from(t).ok())
            .unwrap_or(SubqueryType::Scalar);
        let sub = s
            .subquery
            .as_deref()
            .ok_or_else(|| Error::internal("subquery without scan"))?;
        let correlated = !s.parameter_list.is_empty();
        let p = self.scan(sub, scope)?;
        let out_col = scan_columns(sub).first().copied();
        let pos = match out_col {
            Some(id) => p.layout.iter().position(|&l| l == id).unwrap_or(0),
            None => 0,
        };
        let op: Arc<BoxOp> = Arc::new(p.op);
        let key = Arc::as_ptr(&op) as *const () as usize;
        self.subplans.push((
            format!("{kind:?}{}", if correlated { ", correlated" } else { "" }),
            op.clone(),
        ));
        match kind {
            SubqueryType::Scalar | SubqueryType::Array | SubqueryType::Exists => {
                let compute = move |cx: &Cx, env: &Env| -> Result<Value> {
                    let mut it = op.open(cx, env)?;
                    match kind {
                        SubqueryType::Exists => Ok(Value::Bool(it.next().transpose()?.is_some())),
                        SubqueryType::Array => {
                            let mut v = Vec::new();
                            for r in it {
                                v.push(r?.get(pos).cloned().unwrap_or(Value::Null));
                            }
                            Ok(Value::Array(Arc::from(v)))
                        }
                        _ => {
                            let first = it.next().transpose()?;
                            if it.next().transpose()?.is_some() {
                                return Err(crate::value_ops::out_of_range(
                                    "More than one element",
                                ));
                            }
                            Ok(first
                                .and_then(|r| r.get(pos).cloned())
                                .unwrap_or(Value::Null))
                        }
                    }
                };
                if correlated {
                    Ok(Arc::new(compute))
                } else {
                    Ok(Arc::new(move |cx: &Cx, env: &Env| {
                        if let Some(m) = cx.memo.borrow().get(&key)
                            && let Memo::Value(v) = m.as_ref()
                        {
                            return Ok(v.clone());
                        }
                        let v = compute(cx, env)?;
                        cx.memo
                            .borrow_mut()
                            .insert(key, Rc::new(Memo::Value(v.clone())));
                        Ok(v)
                    }))
                }
            }
            SubqueryType::In => {
                let lhs = s
                    .in_expr
                    .as_deref()
                    .ok_or_else(|| Error::internal("IN subquery without lhs"))?;
                let lhs = self.expr(lhs, scope)?;
                let build = move |cx: &Cx, env: &Env| -> Result<Rc<Memo>> {
                    let mut keys = std::collections::HashSet::new();
                    let mut has_null = false;
                    let mut empty = true;
                    for r in op.open(cx, env)? {
                        empty = false;
                        let v = r?.get(pos).cloned().unwrap_or(Value::Null);
                        if v.is_null() {
                            has_null = true;
                        } else {
                            keys.insert(Key(vec![v]));
                        }
                    }
                    Ok(Rc::new(Memo::Set {
                        keys,
                        has_null,
                        empty,
                    }))
                };
                Ok(Arc::new(move |cx: &Cx, env: &Env| {
                    let set = if correlated {
                        build(cx, env)?
                    } else {
                        let cached = cx.memo.borrow().get(&key).cloned();
                        match cached {
                            Some(m) => m,
                            None => {
                                let m = build(cx, env)?;
                                cx.memo.borrow_mut().insert(key, m.clone());
                                m
                            }
                        }
                    };
                    let Memo::Set {
                        keys,
                        has_null,
                        empty,
                    } = set.as_ref()
                    else {
                        return Err(Error::internal("bad IN memo"));
                    };
                    if *empty {
                        return Ok(Value::Bool(false));
                    }
                    let v = lhs(cx, env)?;
                    if v.is_null() {
                        return Ok(Value::Null);
                    }
                    if keys.contains(&Key(vec![v])) {
                        Ok(Value::Bool(true))
                    } else if *has_null {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::Bool(false))
                    }
                }))
            }
            other => Err(unimplemented(format!("Subquery {other:?}"))),
        }
    }
}

/// Wraps an operator so `describe` shows the whole tree, then each subquery's tree.
pub fn explain(op: &dyn Op, subplans: &[(String, Arc<BoxOp>)]) -> String {
    let mut s = String::new();
    op.describe(&mut s, 0);
    for (kind, sub) in subplans {
        s.push_str(&format!("Subquery {kind}:\n"));
        sub.describe(&mut s, 1);
    }
    s
}
