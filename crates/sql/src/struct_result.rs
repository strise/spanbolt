//! Cloud Spanner's rules for the ARRAY<STRUCT> values a query returns.
//!
//! Spanner returns an ARRAY<STRUCT> as a nested table, and rejects two shapes of it before running
//! the query, both with UNIMPLEMENTED: an array built with array constructor syntax
//! (`[STRUCT(...)]`), and one that can be NULL. Nullability is static: a parameter, an aggregate
//! (in a grouped query only with ORDER BY or LIMIT, or a nullable argument), a NULL literal, a CASE without ELSE, a column on the nullable side of an outer join and a scalar
//! subquery that can return no row can all be NULL; COALESCE/IFNULL with a non-null argument and
//! `ARRAY(SELECT ...)` cannot. Both rules also apply to the ARRAY<STRUCT> fields of the structs an
//! array subquery returns. The emulator implements neither. The rules are mapped from probes against
//! Cloud Spanner (`conformance/cases/sql/struct_array_results.case`); where they leave a shape
//! open, the query is accepted.

use std::collections::HashMap;

use googlesql_sys::proto::any_resolved_aggregate_scan_base_proto::Node as AggNode;
use googlesql_sys::proto::any_resolved_expr_proto::Node as E;
use googlesql_sys::proto::any_resolved_function_call_base_proto::Node as FB;
use googlesql_sys::proto::any_resolved_non_scalar_function_call_base_proto::Node as NS;
use googlesql_sys::proto::any_resolved_scan_proto::Node as S;
use googlesql_sys::proto::resolved_join_scan_enums::JoinType;
use googlesql_sys::proto::resolved_subquery_expr_enums::SubqueryType;
use googlesql_sys::proto::value_proto::Value as V;
use googlesql_sys::proto::*;
use spanbolt_storage::{Code, Error, Result};

use crate::plan::{computed, expr_type, fn_name, scan_columns};
use crate::struct_shape::{is_struct_array, lambda, transform_body, value_args};

/// Bounds the walk through column definitions and nested tables.
const MAX_DEPTH: usize = 64;

const CONSTRUCTED: &str = "Unsupported query shape: This query returns a STRUCT value constructed \
    by a scalar subquery or struct construction syntax. Spanner does not yet support returning \
    STRUCT except as arrays-of-structs. Rephrase your query to produce the struct value with an \
    array subquery: ARRAY(SELECT AS STRUCT expr ...) or flatten the structs fields into the result.";

/// How a column gets its value.
enum Def<'a> {
    Expr(&'a AnyResolvedExprProto),
    /// An aggregate, and whether the query groups (every group has a row).
    Aggregate(&'a AnyResolvedExprProto, bool),
    /// The same value as one of these columns (a WITH reference, a set operation).
    Same(Vec<i64>),
}

/// Why a value can be NULL. Spanner words the error differently for the nullable side of a
/// LEFT/RIGHT join (a FULL join gets the plain message).
#[derive(Clone, Copy)]
enum Null {
    Value,
    OuterJoin,
}

#[derive(Default)]
struct Results<'a> {
    defs: HashMap<i64, Def<'a>>,
    /// Columns on the nullable side of an outer join, as seen above the join.
    outer: HashMap<i64, Null>,
    /// WITH entries in scope while collecting.
    with: HashMap<String, &'a AnyResolvedScanProto>,
    /// The entry each WITH reference (by node address) reads.
    with_refs: HashMap<usize, &'a AnyResolvedScanProto>,
}

pub fn check(q: &ResolvedQueryStmtProto) -> Result<()> {
    let Some(scan) = q.query.as_ref() else {
        return Ok(());
    };
    if !q.output_column_list.iter().any(|oc| {
        oc.column
            .as_ref()
            .and_then(|c| c.r#type.as_ref())
            .is_some_and(is_struct_array)
    }) {
        return Ok(());
    }
    let mut r = Results::default();
    r.scan(scan);
    for oc in &q.output_column_list {
        if let Some(c) = &oc.column
            && let Some(t) = c.r#type.as_ref().filter(|t| is_struct_array(t))
        {
            r.check_column(c.column_id.unwrap_or(-1), t, 0)?;
        }
    }
    Ok(())
}

fn type_name(t: &TypeProto) -> String {
    crate::types::from_proto(t)
        .map(|t| spanbolt_storage::value::type_name(&t))
        .unwrap_or_else(|_| "ARRAY<STRUCT>".into())
}

fn null_error(t: &TypeProto, why: Null) -> Error {
    let cause = match why {
        Null::Value => "",
        Null::OuterJoin => {
            " A likely cause for this error is a sub-SELECT in a FULL OUTER JOIN, or on the \
             nullable side of a LEFT/RIGHT OUTER JOIN."
        }
    };
    Error::new(
        Code::Unimplemented,
        format!(
            "Unsupported query shape: This query can return a null-valued array of struct, which \
             is not supported by Spanner.{cause} {} is the type that might contain null values. \
             Use IFNULL/COALESCE to convert potentially null values to non-null values.",
            type_name(t)
        ),
    )
}

fn column_id(c: &Option<ResolvedColumnProto>) -> i64 {
    c.as_ref().and_then(|c| c.column_id).unwrap_or(-1)
}

fn subquery_type(s: &ResolvedSubqueryExprProto) -> Option<SubqueryType> {
    s.subquery_type.and_then(|t| SubqueryType::try_from(t).ok())
}

fn literal(e: &AnyResolvedExprProto) -> Option<&V> {
    match e.node.as_ref()? {
        E::ResolvedLiteralNode(l) => l.value.as_ref()?.value.as_ref()?.value.as_ref(),
        _ => None,
    }
}

/// A scalar or aggregate function call.
fn function(e: &AnyResolvedExprProto) -> Option<&ResolvedFunctionCallBaseProto> {
    match e.node.as_ref()? {
        E::ResolvedFunctionCallBaseNode(b) => match b.node.as_ref()? {
            FB::ResolvedFunctionCallNode(f) => f.parent.as_ref(),
            FB::ResolvedNonScalarFunctionCallBaseNode(n) => match n.node.as_ref()? {
                NS::ResolvedAggregateFunctionCallNode(a) => a.parent.as_ref()?.parent.as_ref(),
                _ => None,
            },
        },
        _ => None,
    }
}

fn aggregate(e: &AnyResolvedExprProto) -> Option<&ResolvedAggregateFunctionCallProto> {
    match e.node.as_ref()? {
        E::ResolvedFunctionCallBaseNode(b) => match b.node.as_ref()? {
            FB::ResolvedNonScalarFunctionCallBaseNode(n) => match n.node.as_ref()? {
                NS::ResolvedAggregateFunctionCallNode(a) => Some(a),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn is_make_struct(e: &AnyResolvedExprProto) -> bool {
    matches!(e.node.as_ref(), Some(E::ResolvedMakeStructNode(_)))
}

/// A struct value that comes from `STRUCT(...)` through scalar functions.
fn built_struct(e: &AnyResolvedExprProto, depth: usize) -> bool {
    if depth > MAX_DEPTH
        || expr_type(e).and_then(|t| t.type_kind) != Some(TypeKind::TypeStruct as i32)
    {
        return false;
    }
    is_make_struct(e)
        || matches!(e.node.as_ref(), Some(E::ResolvedFunctionCallBaseNode(_)))
            && function(e)
                .is_some_and(|b| b.argument_list.iter().any(|a| built_struct(a, depth + 1)))
}

fn struct_array(e: &AnyResolvedExprProto) -> bool {
    expr_type(e).is_some_and(is_struct_array)
}

/// The struct-array arguments of a function call, lambda calls included.
fn struct_array_args(
    b: &ResolvedFunctionCallBaseProto,
) -> impl Iterator<Item = &AnyResolvedExprProto> {
    value_args(b).filter(|a| struct_array(a))
}

/// A struct built through scalar functions without being one `STRUCT(...)`: as an aggregate's
/// argument (`ARRAY_AGG(IF(c, STRUCT(...), NULL))`) or what a lambda returns
/// (`ARRAY_TRANSFORM(a, e -> IF(c, STRUCT(...), NULL))`), Spanner takes it for constructor syntax.
fn wrapped_struct(e: &AnyResolvedExprProto) -> bool {
    !is_make_struct(e) && built_struct(e, 0)
}

/// An array known to hold exactly one element: `[x]`.
fn one_element(e: &AnyResolvedExprProto) -> bool {
    if let Some(V::ArrayValue(a)) = literal(e) {
        return a.element.len() == 1;
    }
    function(e).is_some_and(|b| fn_name(b) == "$make_array" && b.argument_list.len() == 1)
}

impl<'a> Results<'a> {
    // ------------------------------------------------------------ collecting

    fn scan(&mut self, s: &'a AnyResolvedScanProto) {
        match s.node.as_ref() {
            Some(S::ResolvedProjectScanNode(p)) => {
                self.input(&p.input_scan);
                for c in &p.expr_list {
                    self.define(c);
                }
            }
            Some(S::ResolvedFilterScanNode(f)) => {
                self.input(&f.input_scan);
                self.opt_expr(&f.filter_expr);
            }
            Some(S::ResolvedJoinScanNode(j)) => {
                self.input(&j.left_scan);
                self.input(&j.right_scan);
                self.opt_expr(&j.join_expr);
                let (left, right) = match j.join_type.and_then(|t| JoinType::try_from(t).ok()) {
                    Some(JoinType::Left) => (None, Some(Null::OuterJoin)),
                    Some(JoinType::Right) => (Some(Null::OuterJoin), None),
                    Some(JoinType::Full) => (Some(Null::Value), Some(Null::Value)),
                    _ => (None, None),
                };
                for (side, why) in [(&j.left_scan, left), (&j.right_scan, right)] {
                    if let (Some(side), Some(why)) = (side.as_deref(), why) {
                        for id in scan_columns(side) {
                            self.outer.insert(id, why);
                        }
                    }
                }
            }
            Some(S::ResolvedArrayScanNode(a)) => {
                self.input(&a.input_scan);
                for e in &a.array_expr_list {
                    self.expr(e);
                }
                self.opt_expr(&a.join_expr);
            }
            Some(S::ResolvedAggregateScanBaseNode(a)) => {
                if let Some(AggNode::ResolvedAggregateScanNode(x)) = a.node.as_ref()
                    && let Some(base) = &x.parent
                {
                    self.input(&base.input_scan);
                    for g in &base.group_by_list {
                        self.define(g);
                    }
                    let grouped = !base.group_by_list.is_empty();
                    for c in &base.aggregate_list {
                        if let Some(c) = computed(c)
                            && let Some(e) = c.expr.as_ref()
                        {
                            self.expr(e);
                            self.defs
                                .insert(column_id(&c.column), Def::Aggregate(e, grouped));
                        }
                    }
                }
            }
            Some(S::ResolvedOrderByScanNode(o)) => self.input(&o.input_scan),
            Some(S::ResolvedLimitOffsetScanNode(l)) => self.input(&l.input_scan),
            Some(S::ResolvedAnalyticScanNode(a)) => self.input(&a.input_scan),
            Some(S::ResolvedSetOperationScanNode(so)) => {
                let out = scan_columns(s);
                let mut sources = vec![Vec::new(); out.len()];
                for item in &so.input_item_list {
                    if let Some(sc) = &item.scan {
                        self.scan(sc);
                    }
                    for (src, c) in sources.iter_mut().zip(&item.output_column_list) {
                        src.push(c.column_id.unwrap_or(-1));
                    }
                }
                for (id, src) in out.into_iter().zip(sources) {
                    self.defs.insert(id, Def::Same(src));
                }
            }
            Some(S::ResolvedWithScanNode(w)) => {
                let mut shadowed = Vec::new();
                for e in &w.with_entry_list {
                    if let Some(sub) = &e.with_subquery {
                        self.scan(sub);
                        let name = e.with_query_name.clone().unwrap_or_default();
                        shadowed.push((name.clone(), self.with.insert(name, sub)));
                    }
                }
                self.input(&w.query);
                for (name, previous) in shadowed.into_iter().rev() {
                    match previous {
                        Some(p) => self.with.insert(name, p),
                        None => self.with.remove(&name),
                    };
                }
            }
            Some(S::ResolvedWithRefScanNode(r)) => {
                let name = r.with_query_name.as_deref().unwrap_or_default();
                if let Some(&sub) = self.with.get(name) {
                    self.with_refs.insert(r as *const _ as usize, sub);
                    for (id, src) in scan_columns(s).into_iter().zip(scan_columns(sub)) {
                        self.defs.insert(id, Def::Same(vec![src]));
                    }
                }
            }
            _ => {}
        }
    }

    fn input(&mut self, s: &'a Option<Box<AnyResolvedScanProto>>) {
        if let Some(s) = s.as_deref() {
            self.scan(s);
        }
    }

    fn define(&mut self, c: &'a ResolvedComputedColumnProto) {
        if let Some(e) = c.expr.as_ref() {
            self.expr(e);
            self.defs.insert(column_id(&c.column), Def::Expr(e));
        }
    }

    fn opt_expr(&mut self, e: &'a Option<Box<AnyResolvedExprProto>>) {
        if let Some(e) = e.as_deref() {
            self.expr(e);
        }
    }

    /// Collects the scans of the subqueries inside `e`.
    fn expr(&mut self, e: &'a AnyResolvedExprProto) {
        match e.node.as_ref() {
            Some(E::ResolvedFunctionCallBaseNode(_)) => {
                let Some(b) = function(e) else {
                    return;
                };
                for a in value_args(b) {
                    self.expr(a);
                }
                if let Some(body) = lambda(b).and_then(|l| l.body.as_ref()) {
                    self.expr(body);
                }
            }
            Some(E::ResolvedCastNode(c)) => self.opt_expr(&c.expr),
            Some(E::ResolvedMakeStructNode(m)) => {
                for f in &m.field_list {
                    self.expr(f);
                }
            }
            Some(E::ResolvedGetStructFieldNode(g)) => self.opt_expr(&g.expr),
            Some(E::ResolvedGetJsonFieldNode(g)) => self.opt_expr(&g.expr),
            Some(E::ResolvedSubqueryExprNode(s)) => {
                self.opt_expr(&s.in_expr);
                self.input(&s.subquery);
            }
            Some(E::ResolvedWithExprNode(w)) => {
                for a in &w.assignment_list {
                    self.define(a);
                }
                self.opt_expr(&w.expr);
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ checking

    /// Both rules for a returned ARRAY<STRUCT> column of type `t`, then for the nested tables it
    /// is made of.
    fn check_column(&self, id: i64, t: &TypeProto, depth: usize) -> Result<()> {
        if self.constructed_column(id, depth) {
            return Err(Error::new(Code::Unimplemented, CONSTRUCTED));
        }
        if let Some(why) = self.nullable_column(id, depth) {
            return Err(null_error(t, why));
        }
        let mut tables = Vec::new();
        self.tables_column(id, depth, &mut tables);
        self.check_tables(&tables, depth)
    }

    fn check_expr(&self, e: &'a AnyResolvedExprProto, depth: usize) -> Result<()> {
        let Some(t) = expr_type(e).filter(|t| is_struct_array(t)) else {
            return Ok(());
        };
        if self.constructed(e, depth) {
            return Err(Error::new(Code::Unimplemented, CONSTRUCTED));
        }
        if let Some(why) = self.nullable(e, depth) {
            return Err(null_error(t, why));
        }
        let mut tables = Vec::new();
        self.tables(e, depth, &mut tables);
        self.check_tables(&tables, depth)
    }

    /// The ARRAY<STRUCT> fields of the structs each nested table returns.
    fn check_tables(&self, tables: &[&'a ResolvedSubqueryExprProto], depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Ok(());
        }
        for s in tables {
            let Some(id) = s
                .subquery
                .as_deref()
                .and_then(|q| scan_columns(q).first().copied())
            else {
                continue;
            };
            if let Some(Def::Expr(x)) = self.defs.get(&id)
                && let Some(E::ResolvedMakeStructNode(m)) = x.node.as_ref()
            {
                for f in &m.field_list {
                    self.check_expr(f, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    /// Whether the value is or is made from an array built with constructor syntax.
    fn constructed_column(&self, id: i64, depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        match self.defs.get(&id) {
            Some(Def::Expr(x)) | Some(Def::Aggregate(x, _)) => self.constructed(x, depth + 1),
            Some(Def::Same(ids)) => ids.iter().any(|&i| self.constructed_column(i, depth + 1)),
            None => false,
        }
    }

    fn constructed(&self, e: &'a AnyResolvedExprProto, depth: usize) -> bool {
        if depth > MAX_DEPTH || !struct_array(e) {
            return false;
        }
        match e.node.as_ref() {
            Some(E::ResolvedLiteralNode(_)) => {
                matches!(literal(e), Some(V::ArrayValue(a)) if !a.element.is_empty())
            }
            Some(E::ResolvedColumnRefNode(c)) => {
                self.constructed_column(column_id(&c.column), depth)
            }
            // ARRAY_TRANSFORM's elements come from its lambda: `e -> STRUCT(...)` is fine.
            Some(E::ResolvedFunctionCallBaseNode(_)) => {
                function(e).is_some_and(|b| match transform_body(b) {
                    Some(body) => wrapped_struct(body),
                    None => {
                        (fn_name(b) == "$make_array" && !b.argument_list.is_empty())
                        || struct_array_args(b).any(|a| self.constructed(a, depth + 1))
                        // `ARRAY_AGG(IF(c, STRUCT(...), NULL))`; `ARRAY_AGG(STRUCT(...))` is fine.
                        || (aggregate(e).is_some() && b.argument_list.iter().any(wrapped_struct))
                    }
                })
            }
            Some(E::ResolvedCastNode(c)) => c
                .expr
                .as_deref()
                .is_some_and(|x| self.constructed(x, depth + 1)),
            Some(E::ResolvedWithExprNode(w)) => w
                .expr
                .as_deref()
                .is_some_and(|x| self.constructed(x, depth + 1)),
            Some(E::ResolvedSubqueryExprNode(s))
                if subquery_type(s) == Some(SubqueryType::Scalar) =>
            {
                self.first_column(s)
                    .is_some_and(|id| self.constructed_column(id, depth + 1))
            }
            _ => false,
        }
    }

    fn first_column(&self, s: &'a ResolvedSubqueryExprProto) -> Option<i64> {
        s.subquery
            .as_deref()
            .and_then(|q| scan_columns(q).first().copied())
    }

    /// Why the value can be NULL, if Spanner takes it to be nullable.
    fn nullable_column(&self, id: i64, depth: usize) -> Option<Null> {
        if depth > MAX_DEPTH {
            return None;
        }
        if let Some(&why) = self.outer.get(&id) {
            return Some(why);
        }
        match self.defs.get(&id)? {
            Def::Expr(x) => self.nullable(x, depth + 1),
            // An aggregate is NULL on no input; in a grouped query only with ORDER BY or LIMIT,
            // or when its argument can be NULL.
            Def::Aggregate(x, grouped) => {
                let a = aggregate(x);
                if !grouped
                    || a.is_none_or(|a| !a.order_by_item_list.is_empty() || a.limit.is_some())
                {
                    return Some(Null::Value);
                }
                struct_array_args(function(x)?).find_map(|a| self.nullable(a, depth + 1))
            }
            Def::Same(ids) => ids.iter().find_map(|&i| self.nullable_column(i, depth + 1)),
        }
    }

    fn nullable(&self, e: &'a AnyResolvedExprProto, depth: usize) -> Option<Null> {
        if depth > MAX_DEPTH || !struct_array(e) {
            return None;
        }
        let arg = |a: &'a AnyResolvedExprProto| self.nullable(a, depth + 1);
        match e.node.as_ref()? {
            E::ResolvedLiteralNode(l) => l
                .value
                .as_ref()
                .is_none_or(|v| v.value.as_ref().is_none_or(|x| x.value.is_none()))
                .then_some(Null::Value),
            E::ResolvedParameterNode(_) => Some(Null::Value),
            E::ResolvedColumnRefNode(c) => self.nullable_column(column_id(&c.column), depth),
            E::ResolvedFunctionCallBaseNode(_) => {
                let b = function(e)?;
                let args = &b.argument_list;
                match fn_name(b) {
                    "coalesce" | "ifnull" => {
                        let mut first = None;
                        for a in args {
                            first = first.or(Some(arg(a)?));
                        }
                        first
                    }
                    "if" => args.iter().skip(1).find_map(arg),
                    // `CASE WHEN c THEN x ... [ELSE y] END`: without ELSE it can be NULL.
                    "$case_no_value" => {
                        let no_else = args.len() % 2 == 0;
                        args.iter()
                            .skip(1)
                            .step_by(2)
                            .chain(if no_else { None } else { args.last() })
                            .find_map(arg)
                            .or(no_else.then_some(Null::Value))
                    }
                    // `CASE v WHEN w THEN x ... [ELSE y] END`.
                    "$case_with_value" => {
                        let no_else = args.len() % 2 == 1;
                        args.iter()
                            .skip(2)
                            .step_by(2)
                            .chain(if no_else { None } else { args.last() })
                            .find_map(arg)
                            .or(no_else.then_some(Null::Value))
                    }
                    _ => struct_array_args(b).find_map(arg),
                }
            }
            E::ResolvedCastNode(c) => arg(c.expr.as_deref()?),
            E::ResolvedWithExprNode(w) => arg(w.expr.as_deref()?),
            E::ResolvedSubqueryExprNode(s) if subquery_type(s) == Some(SubqueryType::Scalar) => {
                let scan = s.subquery.as_deref()?;
                if self.may_be_empty(scan, depth + 1) {
                    return Some(Null::Value);
                }
                self.nullable_column(self.first_column(s)?, depth + 1)
            }
            _ => None,
        }
    }

    /// Whether Spanner takes `s` to possibly return no row.
    fn may_be_empty(&self, s: &AnyResolvedScanProto, depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return true;
        }
        let empty = |s: &Option<Box<AnyResolvedScanProto>>| {
            s.as_deref().is_none_or(|s| self.may_be_empty(s, depth + 1))
        };
        match s.node.as_ref() {
            Some(S::ResolvedSingleRowScanNode(_)) => false,
            Some(S::ResolvedProjectScanNode(p)) => empty(&p.input_scan),
            Some(S::ResolvedOrderByScanNode(o)) => empty(&o.input_scan),
            Some(S::ResolvedAnalyticScanNode(a)) => empty(&a.input_scan),
            Some(S::ResolvedAggregateScanBaseNode(a)) => match a.node.as_ref() {
                Some(AggNode::ResolvedAggregateScanNode(x)) => x.parent.as_ref().is_none_or(|b| {
                    !b.group_by_list.is_empty()
                        || !b.grouping_set_list.is_empty()
                        || !b.rollup_column_list.is_empty()
                }),
                _ => true,
            },
            Some(S::ResolvedArrayScanNode(a)) => {
                let input = a.input_scan.is_some() && empty(&a.input_scan);
                let one = a.join_expr.is_none()
                    && matches!(a.array_expr_list.as_slice(), [array] if one_element(array));
                input || !(one || (a.is_outer == Some(true) && a.input_scan.is_some()))
            }
            Some(S::ResolvedLimitOffsetScanNode(l)) => {
                let int = |e: &Option<Box<AnyResolvedExprProto>>| match e.as_deref().map(literal) {
                    Some(Some(V::Int64Value(i))) => Some(*i),
                    _ => None,
                };
                // `LIMIT 1` keeps a row; a parameter or `LIMIT 0` may not, nor may an OFFSET.
                let limit_keeps = l.limit.is_none() || int(&l.limit).is_some_and(|n| n >= 1);
                let offset_keeps = l.offset.is_none() || int(&l.offset) == Some(0);
                empty(&l.input_scan) || !limit_keeps || !offset_keeps
            }
            Some(S::ResolvedWithScanNode(w)) => empty(&w.query),
            Some(S::ResolvedWithRefScanNode(r)) => self
                .with_refs
                .get(&(r as *const _ as usize))
                .is_none_or(|sub| self.may_be_empty(sub, depth + 1)),
            _ => true,
        }
    }

    /// The nested tables (array subqueries) the value is made of.
    fn tables_column(&self, id: i64, depth: usize, out: &mut Vec<&'a ResolvedSubqueryExprProto>) {
        if depth > MAX_DEPTH {
            return;
        }
        match self.defs.get(&id) {
            Some(Def::Expr(x)) | Some(Def::Aggregate(x, _)) => self.tables(x, depth + 1, out),
            Some(Def::Same(ids)) => {
                for &i in ids {
                    self.tables_column(i, depth + 1, out);
                }
            }
            None => {}
        }
    }

    fn tables(
        &self,
        e: &'a AnyResolvedExprProto,
        depth: usize,
        out: &mut Vec<&'a ResolvedSubqueryExprProto>,
    ) {
        if depth > MAX_DEPTH || !struct_array(e) {
            return;
        }
        match e.node.as_ref() {
            Some(E::ResolvedColumnRefNode(c)) => {
                self.tables_column(column_id(&c.column), depth, out)
            }
            Some(E::ResolvedFunctionCallBaseNode(_)) => {
                for a in function(e)
                    .filter(|b| transform_body(b).is_none())
                    .into_iter()
                    .flat_map(struct_array_args)
                {
                    self.tables(a, depth + 1, out);
                }
            }
            Some(E::ResolvedCastNode(c)) => {
                if let Some(x) = c.expr.as_deref() {
                    self.tables(x, depth + 1, out);
                }
            }
            Some(E::ResolvedWithExprNode(w)) => {
                if let Some(x) = w.expr.as_deref() {
                    self.tables(x, depth + 1, out);
                }
            }
            Some(E::ResolvedSubqueryExprNode(s)) => match subquery_type(s) {
                Some(SubqueryType::Array) => out.push(s),
                Some(SubqueryType::Scalar) => {
                    if let Some(id) = self.first_column(s) {
                        self.tables_column(id, depth + 1, out);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}
