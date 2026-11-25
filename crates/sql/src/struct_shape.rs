//! Cloud Spanner's rules for the nested tables a query returns.
//!
//! Spanner returns an `ARRAY(SELECT AS STRUCT ...)` as a nested table unless the subquery yields
//! exactly one row whatever the data. A query fails with UNIMPLEMENTED when its result includes
//! the same nested table more than once (`conformance/cases/sql/struct_returned_twice.case`), or
//! when it returns a nested table and also reads a field of one of its structs. The rules are
//! mapped from probes against Cloud Spanner; where the probes leave a shape open, the
//! query is accepted.

use std::cell::Cell;
use std::collections::HashMap;

use googlesql_sys::proto::any_resolved_aggregate_scan_base_proto::Node as AggNode;
use googlesql_sys::proto::any_resolved_expr_proto::Node as E;
use googlesql_sys::proto::any_resolved_function_call_base_proto::Node as FB;
use googlesql_sys::proto::any_resolved_non_scalar_function_call_base_proto::Node as NS;
use googlesql_sys::proto::any_resolved_scan_proto::Node as S;
use googlesql_sys::proto::resolved_subquery_expr_enums::SubqueryType;
use googlesql_sys::proto::value_proto::Value as V;
use googlesql_sys::proto::*;
use spanbolt_storage::{Code, Error, Result};

use crate::plan::{call_base, computed, expr_type, fn_name, scan_columns};

/// Bounds the walk through column definitions.
const MAX_DEPTH: usize = 64;

/// A nested table: the address of its array subquery node.
type Origin = usize;

/// The [`Origin`] of the nested table subquery `s`: the address of its node.
fn origin_of(s: &ResolvedSubqueryExprProto) -> Origin {
    s as *const ResolvedSubqueryExprProto as Origin
}

/// How a column gets its value, for the columns the rule follows.
enum Def<'a> {
    Expr(&'a AnyResolvedExprProto),
    /// An element of this array (`UNNEST`).
    ElementOf(&'a AnyResolvedExprProto),
    /// The same value as one of these columns (a WITH reference, a set operation).
    Same(Vec<i64>),
}

/// What is asked of an expression: which nested tables' structs its value holds, which nested
/// tables it is, or which nested tables it is one struct of.
#[derive(Clone, Copy)]
enum Flow {
    Contained,
    Array,
    Element,
}

#[derive(Default)]
struct Shape<'a> {
    defs: HashMap<i64, Def<'a>>,
    /// WITH entries in scope while collecting.
    with: HashMap<String, &'a AnyResolvedScanProto>,
    /// The entry each WITH reference (by node address) reads.
    with_refs: HashMap<usize, &'a AnyResolvedScanProto>,
    /// The struct operand of every field access.
    accessed: Vec<&'a AnyResolvedExprProto>,
    /// Every subquery expression, by the address of its subquery node (an [`Origin`]).
    subqueries: HashMap<Origin, &'a AnyResolvedExprProto>,
    /// While set, values that only pass through `ARRAY_FILTER` contain no nested table.
    skip_filters: Cell<bool>,
}

pub fn check(q: &ResolvedQueryStmtProto) -> Result<()> {
    let Some(scan) = q.query.as_ref() else {
        return Ok(());
    };
    if !q.output_column_list.iter().any(|oc| {
        oc.column
            .as_ref()
            .and_then(|c| c.r#type.as_ref())
            .is_some_and(has_struct)
    }) {
        return Ok(());
    }
    let mut shape = Shape::default();
    shape.scan(scan);
    let returned = shape.returned(q);
    if returned.is_empty() {
        return Ok(());
    }
    shape.returned_twice(q, &returned)?;
    for base in &shape.accessed {
        let mut read = Vec::new();
        shape.element_of(base, 0, &mut read);
        if read.iter().any(|o| returned.contains(o)) {
            return Err(Error::new(
                Code::Unimplemented,
                "Unsupported query shape: Accessing fields of a struct value also present in the \
                 final result is not supported.",
            ));
        }
    }
    Ok(())
}

impl Shape<'_> {
    /// The nested tables the result includes, once per time it includes them.
    fn returned(&self, q: &ResolvedQueryStmtProto) -> Vec<Origin> {
        let mut out = Vec::new();
        for oc in &q.output_column_list {
            if let Some(c) = &oc.column
                && c.r#type.as_ref().is_some_and(has_struct)
            {
                self.column(Flow::Contained, c.column_id.unwrap_or(-1), 0, &mut out);
            }
        }
        out
    }

    /// A nested table the result includes more than once. Two `ARRAY_FILTER` calls over one
    /// nested table are accepted; the table next to a filter of it is not.
    fn returned_twice(&self, q: &ResolvedQueryStmtProto, returned: &[Origin]) -> Result<()> {
        let twice: Vec<Origin> = returned
            .iter()
            .enumerate()
            .filter(|(i, o)| returned[..*i].contains(o))
            .map(|(_, o)| *o)
            .collect();
        if twice.is_empty() {
            return Ok(());
        }
        self.skip_filters.set(true);
        let unfiltered = self.returned(q);
        self.skip_filters.set(false);
        let Some(origin) = twice.into_iter().find(|o| unfiltered.contains(o)) else {
            return Ok(());
        };
        let element = self
            .subqueries
            .get(&origin)
            .and_then(|e| expr_type(e))
            .and_then(element_type)
            .and_then(|t| crate::types::from_proto(t).ok())
            .map(|t| spanbolt_storage::value::type_name(&t))
            .unwrap_or_else(|| "STRUCT".into());
        Err(Error::new(
            Code::Unimplemented,
            format!(
                "Unsupported query shape: A nested table defined in a sub-query may not be \
                 included in the query result more than once. Rewrite your query using the \
                 nested table with sub-select. {element} is the type included more than once."
            ),
        ))
    }
}

fn kind(t: &TypeProto) -> Option<TypeKind> {
    t.type_kind.and_then(|k| TypeKind::try_from(k).ok())
}

fn element_type(t: &TypeProto) -> Option<&TypeProto> {
    t.array_type.as_ref()?.element_type.as_deref()
}

fn has_struct(t: &TypeProto) -> bool {
    match kind(t) {
        Some(TypeKind::TypeStruct) => true,
        Some(TypeKind::TypeArray) => element_type(t).is_some_and(has_struct),
        _ => false,
    }
}

pub(crate) fn is_struct_array(t: &TypeProto) -> bool {
    kind(t) == Some(TypeKind::TypeArray)
        && element_type(t).and_then(kind) == Some(TypeKind::TypeStruct)
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

/// An array known to hold exactly one element: `[x]`.
fn one_element(e: &AnyResolvedExprProto) -> bool {
    if let Some(V::ArrayValue(a)) = literal(e) {
        return a.element.len() == 1;
    }
    call_base(e).is_some_and(|b| fn_name(b) == "$make_array" && b.argument_list.len() == 1)
}

/// The value arguments of a call. A call that takes a lambda (`ARRAY_FILTER(a, e -> ...)`) has
/// all its arguments in `generic_argument_list`; this yields the ones that are not lambdas.
pub(crate) fn value_args(
    b: &ResolvedFunctionCallBaseProto,
) -> impl Iterator<Item = &AnyResolvedExprProto> {
    b.argument_list.iter().chain(
        b.generic_argument_list
            .iter()
            .filter_map(|g| g.expr.as_ref()),
    )
}

/// The lambda argument of a call, as in `ARRAY_TRANSFORM(a, e -> ...)`.
pub(crate) fn lambda(b: &ResolvedFunctionCallBaseProto) -> Option<&ResolvedInlineLambdaProto> {
    b.generic_argument_list
        .iter()
        .find_map(|g| g.inline_lambda.as_ref())
}

/// `ARRAY_TRANSFORM(a, e -> body)`: its elements are the body's values, not `a`'s.
pub(crate) fn transform_body(b: &ResolvedFunctionCallBaseProto) -> Option<&AnyResolvedExprProto> {
    if fn_name(b) != "array_transform" {
        return None;
    }
    lambda(b)?.body.as_ref()
}

impl<'a> Shape<'a> {
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
            }
            Some(S::ResolvedArrayScanNode(a)) => {
                self.input(&a.input_scan);
                for (array, element) in a.array_expr_list.iter().zip(&a.element_column_list) {
                    self.expr(array);
                    if let Some(id) = element.column_id {
                        self.defs.insert(id, Def::ElementOf(array));
                    }
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
                    // An aggregate's result is a new value: its column gets no definition.
                    for c in &base.aggregate_list {
                        if let Some(e) = computed(c).and_then(|c| c.expr.as_ref()) {
                            self.expr(e);
                        }
                    }
                }
            }
            Some(S::ResolvedOrderByScanNode(o)) => self.input(&o.input_scan),
            Some(S::ResolvedLimitOffsetScanNode(l)) => self.input(&l.input_scan),
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

    fn expr(&mut self, e: &'a AnyResolvedExprProto) {
        match e.node.as_ref() {
            Some(E::ResolvedFunctionCallBaseNode(b)) => {
                let base = match b.node.as_ref() {
                    Some(FB::ResolvedFunctionCallNode(f)) => f.parent.as_ref(),
                    Some(FB::ResolvedNonScalarFunctionCallBaseNode(n)) => match n.node.as_ref() {
                        Some(NS::ResolvedAggregateFunctionCallNode(a)) => {
                            a.parent.as_ref().and_then(|p| p.parent.as_ref())
                        }
                        _ => None,
                    },
                    None => None,
                };
                let Some(base) = base else {
                    return;
                };
                for a in value_args(base) {
                    self.expr(a);
                }
                // The lambda's first argument is an element of the array argument, as with UNNEST.
                if let Some(l) = lambda(base) {
                    if let (Some(array), Some(id)) = (
                        value_args(base).next(),
                        l.argument_list.first().and_then(|c| c.column_id),
                    ) {
                        self.defs.insert(id, Def::ElementOf(array));
                    }
                    if let Some(body) = l.body.as_ref() {
                        self.expr(body);
                    }
                }
            }
            Some(E::ResolvedCastNode(c)) => self.opt_expr(&c.expr),
            Some(E::ResolvedMakeStructNode(m)) => {
                for f in &m.field_list {
                    self.expr(f);
                }
            }
            Some(E::ResolvedGetStructFieldNode(g)) => {
                if let Some(base) = g.expr.as_deref() {
                    self.accessed.push(base);
                    self.expr(base);
                }
            }
            Some(E::ResolvedGetJsonFieldNode(g)) => self.opt_expr(&g.expr),
            Some(E::ResolvedSubqueryExprNode(s)) => {
                self.subqueries.insert(origin_of(s), e);
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

    // ------------------------------------------------------------ nested tables

    /// The nested table an array subquery makes, unless it yields exactly one row.
    fn origin(&self, e: &AnyResolvedExprProto, s: &ResolvedSubqueryExprProto) -> Option<Origin> {
        if subquery_type(s) != Some(SubqueryType::Array)
            || !expr_type(e).is_some_and(is_struct_array)
        {
            return None;
        }
        let outer: Vec<i64> = s
            .parameter_list
            .iter()
            .map(|p| column_id(&p.column))
            .collect();
        let scan = s.subquery.as_deref()?;
        (!self.single_row(scan, &outer, 0)).then_some(origin_of(s))
    }

    /// Whether Spanner takes `s` to yield exactly one row. `outer` holds the columns of the
    /// enclosing query that the subquery reads.
    fn single_row(&self, s: &AnyResolvedScanProto, outer: &[i64], depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        let single = |s: &Option<Box<AnyResolvedScanProto>>| {
            s.as_deref()
                .is_some_and(|s| self.single_row(s, outer, depth + 1))
        };
        let constant = |e: &Option<Box<AnyResolvedExprProto>>| {
            e.as_deref().is_none_or(|e| self.constant(e, outer, 0))
        };
        match s.node.as_ref() {
            Some(S::ResolvedSingleRowScanNode(_)) => true,
            Some(S::ResolvedProjectScanNode(p)) => single(&p.input_scan),
            Some(S::ResolvedOrderByScanNode(o)) => single(&o.input_scan),
            Some(S::ResolvedAggregateScanBaseNode(a)) => match a.node.as_ref() {
                Some(AggNode::ResolvedAggregateScanNode(x)) => x.parent.as_ref().is_some_and(|b| {
                    b.group_by_list.is_empty()
                        && b.grouping_set_list.is_empty()
                        && b.rollup_column_list.is_empty()
                }),
                _ => false,
            },
            Some(S::ResolvedArrayScanNode(a)) => {
                a.array_offset_column.is_none()
                    && matches!(a.array_expr_list.as_slice(), [array] if one_element(array))
                    && (a.input_scan.is_none() || single(&a.input_scan))
                    && constant(&a.join_expr)
            }
            Some(S::ResolvedJoinScanNode(j)) => {
                single(&j.left_scan) && single(&j.right_scan) && constant(&j.join_expr)
            }
            // Spanner folds a constant filter: one that is FALSE or NULL leaves no row.
            Some(S::ResolvedFilterScanNode(f)) => {
                single(&f.input_scan)
                    && constant(&f.filter_expr)
                    && f.filter_expr
                        .as_deref()
                        .is_none_or(|e| !matches!(self.eval(e, 0), Some(C::Null | C::Bool(false))))
            }
            Some(S::ResolvedLimitOffsetScanNode(l)) => {
                let int = |e: &Option<Box<AnyResolvedExprProto>>| match e.as_deref().map(literal) {
                    Some(Some(V::Int64Value(i))) => Some(*i),
                    _ => None,
                };
                single(&l.input_scan)
                    && int(&l.limit).is_none_or(|n| n >= 1)
                    && int(&l.offset).is_none_or(|n| n == 0)
            }
            Some(S::ResolvedWithScanNode(w)) => single(&w.query),
            Some(S::ResolvedWithRefScanNode(r)) => self
                .with_refs
                .get(&(r as *const _ as usize))
                .is_some_and(|sub| self.single_row(sub, outer, depth + 1)),
            _ => false,
        }
    }

    /// Whether `e` is the same for every row of the subquery and every outer row.
    fn constant(&self, e: &AnyResolvedExprProto, outer: &[i64], depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        match e.node.as_ref() {
            Some(E::ResolvedLiteralNode(_)) | Some(E::ResolvedParameterNode(_)) => true,
            Some(E::ResolvedColumnRefNode(c)) => {
                let id = column_id(&c.column);
                !outer.contains(&id)
                    && match self.defs.get(&id) {
                        Some(Def::Expr(x)) | Some(Def::ElementOf(x)) => {
                            self.constant(x, outer, depth + 1)
                        }
                        _ => false,
                    }
            }
            Some(E::ResolvedCastNode(c)) => c
                .expr
                .as_deref()
                .is_some_and(|x| self.constant(x, outer, depth + 1)),
            Some(E::ResolvedFunctionCallBaseNode(_)) => call_base(e).is_some_and(|b| {
                b.generic_argument_list.is_empty()
                    && b.argument_list
                        .iter()
                        .all(|a| self.constant(a, outer, depth + 1))
            }),
            _ => false,
        }
    }

    // ------------------------------------------------------------ value flow

    fn column(&self, flow: Flow, id: i64, depth: usize, out: &mut Vec<Origin>) {
        if depth > MAX_DEPTH {
            return;
        }
        match (self.defs.get(&id), flow) {
            (Some(Def::Expr(x)), _) => self.follow(flow, x, depth + 1, out),
            (Some(Def::ElementOf(x)), Flow::Contained) => self.contained_in(x, depth + 1, out),
            (Some(Def::ElementOf(x)), Flow::Element) => self.array_of(x, depth + 1, out),
            (Some(Def::Same(ids)), _) => {
                for &src in ids {
                    self.column(flow, src, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    fn follow(&self, flow: Flow, e: &'a AnyResolvedExprProto, depth: usize, out: &mut Vec<Origin>) {
        match flow {
            Flow::Contained => self.contained_in(e, depth, out),
            Flow::Array => self.array_of(e, depth, out),
            Flow::Element => self.element_of(e, depth, out),
        }
    }

    /// The first output column of a subquery expression.
    fn subquery_column(
        &self,
        flow: Flow,
        s: &'a ResolvedSubqueryExprProto,
        depth: usize,
        out: &mut Vec<Origin>,
    ) {
        if let Some(id) = s
            .subquery
            .as_deref()
            .and_then(|q| scan_columns(q).first().copied())
        {
            self.column(flow, id, depth + 1, out);
        }
    }

    /// The nested tables whose structs are part of `e`'s value.
    fn contained_in(&self, e: &'a AnyResolvedExprProto, depth: usize, out: &mut Vec<Origin>) {
        if depth > MAX_DEPTH || !expr_type(e).is_some_and(has_struct) {
            return;
        }
        match e.node.as_ref() {
            Some(E::ResolvedColumnRefNode(c)) => {
                self.column(Flow::Contained, column_id(&c.column), depth, out)
            }
            Some(E::ResolvedSubqueryExprNode(s)) => {
                out.extend(self.origin(e, s));
                self.subquery_column(Flow::Contained, s, depth, out);
            }
            // Scalar calls pass their arguments' structs on; an aggregate makes a new value.
            // ARRAY_TRANSFORM holds what its lambda returns.
            Some(E::ResolvedFunctionCallBaseNode(_)) => {
                if let Some(b) = call_base(e) {
                    if self.skip_filters.get() && fn_name(b) == "array_filter" {
                        return;
                    }
                    match transform_body(b) {
                        Some(body) => self.contained_in(body, depth + 1, out),
                        None => {
                            for a in value_args(b) {
                                self.contained_in(a, depth + 1, out);
                            }
                        }
                    }
                }
            }
            Some(E::ResolvedMakeStructNode(m)) => {
                for f in &m.field_list {
                    self.contained_in(f, depth + 1, out);
                }
            }
            Some(E::ResolvedGetStructFieldNode(g)) => {
                if let Some(x) = g.expr.as_deref() {
                    self.contained_in(x, depth + 1, out);
                }
            }
            Some(E::ResolvedCastNode(c)) => {
                if let Some(x) = c.expr.as_deref() {
                    self.contained_in(x, depth + 1, out);
                }
            }
            Some(E::ResolvedWithExprNode(w)) => {
                if let Some(x) = w.expr.as_deref() {
                    self.contained_in(x, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    /// The nested tables `e` is: the array itself, possibly through array functions.
    fn array_of(&self, e: &'a AnyResolvedExprProto, depth: usize, out: &mut Vec<Origin>) {
        if depth > MAX_DEPTH || !expr_type(e).is_some_and(is_struct_array) {
            return;
        }
        match e.node.as_ref() {
            Some(E::ResolvedColumnRefNode(c)) => {
                self.column(Flow::Array, column_id(&c.column), depth, out)
            }
            Some(E::ResolvedSubqueryExprNode(s)) => match self.origin(e, s) {
                Some(o) => out.push(o),
                None if subquery_type(s) == Some(SubqueryType::Scalar) => {
                    self.subquery_column(Flow::Array, s, depth, out)
                }
                None => {}
            },
            // ARRAY_FILTER(a, ...) is a's nested table; ARRAY_TRANSFORM makes a new array.
            Some(E::ResolvedFunctionCallBaseNode(_)) => {
                for a in call_base(e)
                    .filter(|b| transform_body(b).is_none())
                    .into_iter()
                    .flat_map(value_args)
                {
                    self.array_of(a, depth + 1, out);
                }
            }
            // `a[OFFSET(0)].x`: the array a struct field holds.
            Some(E::ResolvedGetStructFieldNode(g)) => self.field(Flow::Array, g, depth, out),
            Some(E::ResolvedCastNode(c)) => {
                if let Some(x) = c.expr.as_deref() {
                    self.array_of(x, depth + 1, out);
                }
            }
            Some(E::ResolvedWithExprNode(w)) => {
                if let Some(x) = w.expr.as_deref() {
                    self.array_of(x, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    /// The nested tables `e` is one struct of.
    fn element_of(&self, e: &'a AnyResolvedExprProto, depth: usize, out: &mut Vec<Origin>) {
        if depth > MAX_DEPTH || expr_type(e).and_then(kind) != Some(TypeKind::TypeStruct) {
            return;
        }
        match e.node.as_ref() {
            Some(E::ResolvedColumnRefNode(c)) => {
                self.column(Flow::Element, column_id(&c.column), depth, out)
            }
            Some(E::ResolvedSubqueryExprNode(s))
                if subquery_type(s) == Some(SubqueryType::Scalar) =>
            {
                self.subquery_column(Flow::Element, s, depth, out)
            }
            // `a[OFFSET(0)]`, `ARRAY_FIRST(a)`, `IFNULL(a[OFFSET(0)], a[OFFSET(1)])`.
            Some(E::ResolvedFunctionCallBaseNode(_)) => {
                for a in call_base(e).iter().flat_map(|b| &b.argument_list) {
                    self.array_of(a, depth + 1, out);
                    self.element_of(a, depth + 1, out);
                }
            }
            Some(E::ResolvedGetStructFieldNode(g)) => self.field(Flow::Element, g, depth, out),
            Some(E::ResolvedCastNode(c)) => {
                if let Some(x) = c.expr.as_deref() {
                    self.element_of(x, depth + 1, out);
                }
            }
            Some(E::ResolvedWithExprNode(w)) => {
                if let Some(x) = w.expr.as_deref() {
                    self.element_of(x, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ struct fields

    /// The value of field `g` of a struct, followed as `flow`.
    fn field(
        &self,
        flow: Flow,
        g: &'a ResolvedGetStructFieldProto,
        depth: usize,
        out: &mut Vec<Origin>,
    ) {
        let (Some(base), Some(idx)) = (g.expr.as_deref(), g.field_idx) else {
            return;
        };
        let mut fields = Vec::new();
        self.struct_field(base, idx, depth + 1, &mut fields);
        for f in fields {
            self.follow(flow, f, depth + 1, out);
        }
    }

    /// The expressions that can give field `idx` of struct `e`.
    fn struct_field(
        &self,
        e: &'a AnyResolvedExprProto,
        idx: i64,
        depth: usize,
        out: &mut Vec<&'a AnyResolvedExprProto>,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        match e.node.as_ref() {
            Some(E::ResolvedColumnRefNode(c)) => {
                self.column_field(column_id(&c.column), idx, false, depth, out)
            }
            Some(E::ResolvedMakeStructNode(m)) => {
                out.extend(usize::try_from(idx).ok().and_then(|i| m.field_list.get(i)));
            }
            Some(E::ResolvedSubqueryExprNode(s))
                if subquery_type(s) == Some(SubqueryType::Scalar) =>
            {
                if let Some(id) = s
                    .subquery
                    .as_deref()
                    .and_then(|q| scan_columns(q).first().copied())
                {
                    self.column_field(id, idx, false, depth + 1, out);
                }
            }
            // `a[OFFSET(0)]`, `ARRAY_FIRST(a)`, `IFNULL(s, t)`.
            Some(E::ResolvedFunctionCallBaseNode(_)) => {
                for a in call_base(e).iter().flat_map(|b| &b.argument_list) {
                    match expr_type(a).and_then(kind) {
                        Some(TypeKind::TypeArray) => self.element_field(a, idx, depth + 1, out),
                        Some(TypeKind::TypeStruct) => self.struct_field(a, idx, depth + 1, out),
                        _ => {}
                    }
                }
            }
            Some(E::ResolvedCastNode(c)) => {
                if let Some(x) = c.expr.as_deref() {
                    self.struct_field(x, idx, depth + 1, out);
                }
            }
            Some(E::ResolvedWithExprNode(w)) => {
                if let Some(x) = w.expr.as_deref() {
                    self.struct_field(x, idx, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    /// The expressions that can give field `idx` of the structs in array `e`.
    fn element_field(
        &self,
        e: &'a AnyResolvedExprProto,
        idx: i64,
        depth: usize,
        out: &mut Vec<&'a AnyResolvedExprProto>,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        match e.node.as_ref() {
            Some(E::ResolvedColumnRefNode(c)) => {
                self.column_field(column_id(&c.column), idx, true, depth, out)
            }
            Some(E::ResolvedSubqueryExprNode(s)) => {
                let Some(id) = s
                    .subquery
                    .as_deref()
                    .and_then(|q| scan_columns(q).first().copied())
                else {
                    return;
                };
                // ARRAY(SELECT AS STRUCT ...) holds the structs of its column; a scalar subquery
                // returns an array column.
                let elements = subquery_type(s) == Some(SubqueryType::Scalar);
                self.column_field(id, idx, elements, depth + 1, out);
            }
            Some(E::ResolvedFunctionCallBaseNode(_)) => {
                for a in call_base(e).iter().flat_map(|b| &b.argument_list) {
                    if expr_type(a).and_then(kind) == Some(TypeKind::TypeArray) {
                        self.element_field(a, idx, depth + 1, out);
                    }
                }
            }
            Some(E::ResolvedCastNode(c)) => {
                if let Some(x) = c.expr.as_deref() {
                    self.element_field(x, idx, depth + 1, out);
                }
            }
            Some(E::ResolvedWithExprNode(w)) => {
                if let Some(x) = w.expr.as_deref() {
                    self.element_field(x, idx, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    /// Field `idx` of column `id`'s struct, or with `elements` of the structs in its array.
    fn column_field(
        &self,
        id: i64,
        idx: i64,
        elements: bool,
        depth: usize,
        out: &mut Vec<&'a AnyResolvedExprProto>,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        match self.defs.get(&id) {
            Some(Def::Expr(x)) if elements => self.element_field(x, idx, depth + 1, out),
            Some(Def::Expr(x)) => self.struct_field(x, idx, depth + 1, out),
            Some(Def::ElementOf(x)) if !elements => self.element_field(x, idx, depth + 1, out),
            Some(Def::Same(ids)) => {
                for &src in ids {
                    self.column_field(src, idx, elements, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ constant folding

    /// The value of a constant expression, where it is simple enough to fold here.
    fn eval(&self, e: &AnyResolvedExprProto, depth: usize) -> Option<C> {
        if depth > MAX_DEPTH {
            return None;
        }
        match e.node.as_ref()? {
            E::ResolvedLiteralNode(_) => Some(match literal(e) {
                None => C::Null,
                Some(V::BoolValue(b)) => C::Bool(*b),
                Some(V::Int64Value(i)) => C::Int(*i),
                Some(V::StringValue(s)) => C::Str(s.clone()),
                Some(_) => return None,
            }),
            E::ResolvedColumnRefNode(c) => match self.defs.get(&column_id(&c.column))? {
                Def::Expr(x) => self.eval(x, depth + 1),
                Def::ElementOf(x) => self.only_element(x, depth + 1),
                Def::Same(_) => None,
            },
            E::ResolvedCastNode(c) => match self.eval(c.expr.as_deref()?, depth + 1)? {
                C::Null => Some(C::Null),
                _ => None,
            },
            E::ResolvedFunctionCallBaseNode(_) => {
                let b = call_base(e)?;
                let args: Vec<Option<C>> = b
                    .argument_list
                    .iter()
                    .map(|a| self.eval(a, depth + 1))
                    .collect();
                fold(fn_name(b), &args)
            }
            _ => None,
        }
    }

    /// The element of a one-element array.
    fn only_element(&self, e: &AnyResolvedExprProto, depth: usize) -> Option<C> {
        if let Some(V::ArrayValue(a)) = literal(e) {
            let [v] = a.element.as_slice() else {
                return None;
            };
            return Some(match v.value.as_ref() {
                None => C::Null,
                Some(V::BoolValue(b)) => C::Bool(*b),
                Some(V::Int64Value(i)) => C::Int(*i),
                Some(V::StringValue(s)) => C::Str(s.clone()),
                Some(_) => return None,
            });
        }
        let b = call_base(e)?;
        match b.argument_list.as_slice() {
            [x] if fn_name(b) == "$make_array" => self.eval(x, depth + 1),
            _ => None,
        }
    }
}

/// A folded constant.
#[derive(Clone, PartialEq, PartialOrd)]
enum C {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
}

/// Folds the boolean functions a filter is made of. `None`: not folded here.
fn fold(name: &str, args: &[Option<C>]) -> Option<C> {
    use std::cmp::Ordering;
    let cmp = |want: fn(Ordering) -> bool| match args {
        [Some(C::Null), Some(_)] | [Some(_), Some(C::Null)] => Some(C::Null),
        [Some(a), Some(b)] => match (a, b) {
            (C::Int(_), C::Int(_)) | (C::Str(_), C::Str(_)) | (C::Bool(_), C::Bool(_)) => {
                a.partial_cmp(b).map(|o| C::Bool(want(o)))
            }
            _ => None,
        },
        _ => None,
    };
    match name {
        "$equal" => cmp(Ordering::is_eq),
        "$not_equal" => cmp(Ordering::is_ne),
        "$less" => cmp(Ordering::is_lt),
        "$less_or_equal" => cmp(Ordering::is_le),
        "$greater" => cmp(Ordering::is_gt),
        "$greater_or_equal" => cmp(Ordering::is_ge),
        "$not" => match args {
            [Some(C::Bool(b))] => Some(C::Bool(!b)),
            [Some(C::Null)] => Some(C::Null),
            _ => None,
        },
        "$is_null" => match args {
            [Some(v)] => Some(C::Bool(*v == C::Null)),
            _ => None,
        },
        // Three-valued logic: FALSE wins AND, TRUE wins OR, else NULL if any is NULL.
        "$and" | "$or" => {
            let (wins, loses) = if name == "$and" {
                (false, true)
            } else {
                (true, false)
            };
            let mut null = false;
            for a in args {
                match a {
                    Some(C::Bool(b)) if *b == wins => return Some(C::Bool(wins)),
                    Some(C::Bool(_)) => {}
                    Some(C::Null) => null = true,
                    _ => return None,
                }
            }
            Some(if null { C::Null } else { C::Bool(loses) })
        }
        _ => None,
    }
}
