//! Lowers graph queries (`GRAPH ... MATCH`, `GRAPH_TABLE`) to `GraphMatch` operators and the
//! expressions over graph elements.

use std::collections::HashMap;
use std::sync::Arc;

use googlesql_sys::proto::any_resolved_expr_proto::Node as E;
use googlesql_sys::proto::any_resolved_graph_element_scan_proto::Node as ElemNode;
use googlesql_sys::proto::any_resolved_graph_path_scan_base_proto::Node as PathNode;
use googlesql_sys::proto::any_resolved_graph_scan_base_proto::Node as GraphNode;
use googlesql_sys::proto::resolved_graph_edge_scan_enums::EdgeOrientation;
use googlesql_sys::proto::resolved_graph_path_mode_enums::PathMode;
use googlesql_sys::proto::*;
use spanbolt_storage::{Error, Result, Value};

use super::{
    Planned, Planner, Scope, call_base, conjuncts, fn_name, hash_key_type, nid, refs_any, type_of,
};
use crate::exec::{Cx, Env, Eval};
use crate::graph::GraphDef;
use crate::graph_exec::{
    Anchor, ElemPat, GraphMatch, GraphRt, Item, Mode, Orient, Path, Quant, SlotSeek, decode,
    same_element,
};
use crate::types::{Type, unimplemented};

/// The graph a `GraphTableScan` reads, while its body is planned.
pub(crate) struct GraphCx {
    def: Arc<GraphDef>,
    rt: Arc<GraphRt>,
    /// Compiled property definitions, by property name and element table slot.
    props: HashMap<String, Arc<Vec<Option<Eval>>>>,
}

pub(crate) fn is_graph_type(t: Option<&TypeProto>) -> bool {
    matches!(
        t.and_then(|t| t.type_kind)
            .and_then(|k| TypeKind::try_from(k).ok()),
        Some(TypeKind::TypeGraphElement) | Some(TypeKind::TypeGraphPath)
    )
}

fn scan_cols(s: Option<&ResolvedScanProto>) -> Vec<i64> {
    s.map(|s| {
        s.column_list
            .iter()
            .map(|c| c.column_id.unwrap_or(-1))
            .collect()
    })
    .unwrap_or_default()
}

fn elem_base(
    e: &AnyResolvedGraphElementScanProto,
) -> Option<(&ResolvedGraphElementScanProto, Option<i32>)> {
    match e.node.as_ref()? {
        ElemNode::ResolvedGraphNodeScanNode(n) => Some((n.parent.as_deref()?, None)),
        ElemNode::ResolvedGraphEdgeScanNode(n) => {
            Some((n.parent.as_deref()?, Some(n.orientation.unwrap_or(0))))
        }
    }
}

fn elem_cols(b: &ResolvedGraphElementScanProto) -> Vec<i64> {
    scan_cols(b.parent.as_ref().and_then(|p| p.parent.as_ref()))
}

fn path_cols(p: &ResolvedGraphPathScanProto) -> Vec<i64> {
    scan_cols(p.parent.as_ref().and_then(|p| p.parent.as_ref()))
}

fn add(layout: &mut Vec<i64>, id: i64) {
    if !layout.contains(&id) {
        layout.push(id);
    }
}

/// Every column a path binds, including those inside quantified sub-paths.
fn collect_cols(p: &ResolvedGraphPathScanProto, out: &mut Vec<i64>) {
    for id in path_cols(p) {
        add(out, id);
    }
    for c in [&p.head, &p.tail].into_iter().flatten() {
        add(out, c.column_id.unwrap_or(-1));
    }
    for g in &p.group_variable_list {
        add(out, nid(&g.element));
        add(out, nid(&g.array));
    }
    for s in &p.input_scan_list {
        match s.node.as_ref() {
            Some(PathNode::ResolvedGraphElementScanNode(e)) => {
                if let Some((b, _)) = elem_base(e) {
                    for id in elem_cols(b) {
                        add(out, id);
                    }
                }
            }
            Some(PathNode::ResolvedGraphPathScanNode(p)) => collect_cols(p, out),
            None => {}
        }
    }
}

fn mode_of(p: &ResolvedGraphPathScanProto) -> Result<Mode> {
    match p
        .path_mode
        .as_ref()
        .and_then(|m| m.path_mode)
        .and_then(|m| PathMode::try_from(m).ok())
    {
        None | Some(PathMode::Unspecified) | Some(PathMode::Walk) => Ok(Mode::Walk),
        Some(PathMode::Trail) => Ok(Mode::Trail),
        Some(PathMode::Acyclic) => Ok(Mode::Acyclic),
        Some(PathMode::Simple) => Err(unimplemented("The SIMPLE path mode")),
    }
}

fn check_path(p: &ResolvedGraphPathScanProto) -> Result<()> {
    if p.path.is_some() {
        return Err(unimplemented("Graph path variables"));
    }
    if p.search_prefix.is_some() {
        return Err(unimplemented("ANY and SHORTEST path search"));
    }
    if p.path_cost.is_some() {
        return Err(unimplemented("Path costs"));
    }
    Ok(())
}

fn strip_cast(e: &AnyResolvedExprProto) -> &AnyResolvedExprProto {
    match e.node.as_ref() {
        Some(E::ResolvedCastNode(c)) if is_graph_type(super::expr_type(e)) => {
            c.expr.as_deref().map_or(e, strip_cast)
        }
        _ => e,
    }
}

fn col_of(e: &AnyResolvedExprProto) -> Option<i64> {
    match strip_cast(e).node.as_ref()? {
        E::ResolvedColumnRefNode(c) => Some(nid(&c.column)),
        _ => None,
    }
}

/// `x.prop` over column `x`: (column, property name).
fn property_of(e: &AnyResolvedExprProto) -> Option<(i64, &str)> {
    match e.node.as_ref()? {
        E::ResolvedGraphGetElementPropertyNode(g) => Some((
            col_of(g.expr.as_deref()?)?,
            g.property.as_ref()?.name.as_deref()?,
        )),
        _ => None,
    }
}

fn equal_args(e: &AnyResolvedExprProto) -> Option<(&AnyResolvedExprProto, &AnyResolvedExprProto)> {
    let b = call_base(e)?;
    if fn_name(b) != "$equal" || b.argument_list.len() != 2 {
        return None;
    }
    Some((&b.argument_list[0], &b.argument_list[1]))
}

fn quant_bound(e: Option<&AnyResolvedExprProto>) -> Result<Option<usize>> {
    let Some(e) = e else { return Ok(None) };
    match e.node.as_ref() {
        Some(E::ResolvedLiteralNode(l)) => {
            match crate::literal::value(
                l.value.as_ref().and_then(|v| v.value.as_ref()),
                &Type::Int64,
            )? {
                Value::Int64(n) if n >= 0 => Ok(Some(n as usize)),
                _ => Err(Error::invalid_argument(
                    "Path quantifier bounds must be non-negative",
                )),
            }
        }
        _ => Err(unimplemented("Path quantifiers that are not literals")),
    }
}

/// A top-level node or edge pattern of a path, for choosing where matching starts.
struct Top<'a> {
    item: usize,
    col: i64,
    slots: Vec<usize>,
    filter: Option<&'a AnyResolvedExprProto>,
}

impl Planner<'_> {
    pub(super) fn graph_table(
        &mut self,
        g: &ResolvedGraphTableScanProto,
        outer: &Scope,
    ) -> Result<Planned> {
        let name = g
            .property_graph
            .as_ref()
            .and_then(|p| p.full_name.as_deref())
            .unwrap_or("");
        let def = self
            .cat
            .graphs
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, d)| d.clone())
            .ok_or_else(|| Error::not_found(format!("Property graph not found: {name}")))?;
        let rt = Arc::new(GraphRt::new(&def));
        let saved = self.graph.replace(GraphCx {
            def,
            rt,
            props: HashMap::new(),
        });
        let input = g
            .input_scan
            .as_deref()
            .ok_or_else(|| Error::internal("graph table without input"));
        let r = input
            .and_then(|i| self.graph_base(i, outer))
            .and_then(|p| self.graph_columns(p, &g.shape_expr_list, outer));
        self.graph = saved;
        r
    }

    /// `GRAPH_TABLE(... COLUMNS (...))`: the output columns, computed over the matched rows.
    fn graph_columns(
        &mut self,
        p: Planned,
        shape: &[ResolvedComputedColumnProto],
        outer: &Scope,
    ) -> Result<Planned> {
        if shape.is_empty() {
            return Ok(p);
        }
        let scope = Scope {
            cols: &p.layout,
            up: Some(outer),
        };
        let mut exprs = Vec::new();
        let mut layout = p.layout.clone();
        for cc in shape {
            let e = cc
                .expr
                .as_ref()
                .ok_or_else(|| Error::internal("graph column without expr"))?;
            exprs.push(self.expr(e, &scope)?);
            layout.push(nid(&cc.column));
        }
        Ok(Planned {
            op: Box::new(crate::ops::Compute { input: p.op, exprs }),
            layout,
        })
    }

    pub(super) fn graph_base(
        &mut self,
        b: &AnyResolvedGraphScanBaseProto,
        outer: &Scope,
    ) -> Result<Planned> {
        match b.node.as_ref() {
            Some(GraphNode::ResolvedGraphScanNode(s)) => self.graph_scan(s, outer),
            Some(GraphNode::ResolvedGraphLinearScanNode(l)) => {
                // Each statement reads the previous one's rows through a GraphRefScan.
                let mut prev = self.graph_ref.take();
                for s in &l.scan_list {
                    self.graph_ref = prev.take();
                    let p = self.scan(s, outer);
                    self.graph_ref = None;
                    prev = Some(p?);
                }
                prev.ok_or_else(|| Error::internal("empty graph linear scan"))
            }
            None => Err(Error::internal("empty graph scan")),
        }
    }

    fn graph_cx(&self) -> Result<(Arc<GraphDef>, Arc<GraphRt>)> {
        self.graph
            .as_ref()
            .map(|g| (g.def.clone(), g.rt.clone()))
            .ok_or_else(|| Error::internal("graph scan outside a graph query"))
    }

    fn slots(&self, def: &GraphDef, refs: &[GraphElementTableRefProto]) -> Result<Vec<usize>> {
        refs.iter()
            .map(|r| {
                let name = r.name.as_deref().unwrap_or("");
                match def.node_or_edge(name) {
                    Some((false, i)) => Ok(i),
                    Some((true, i)) => Ok(def.nodes.len() + i),
                    None => Err(Error::internal(format!("unknown element table {name}"))),
                }
            })
            .collect()
    }

    fn graph_scan(&mut self, gs: &ResolvedGraphScanProto, outer: &Scope) -> Result<Planned> {
        // GRAPH_TABLE's MATCH has no input: it starts from one empty row.
        let input = match gs.input_scan.as_deref() {
            Some(i) => self.scan(i, outer)?,
            None => Planned {
                op: Box::new(crate::ops::SingleRow),
                layout: Vec::new(),
            },
        };
        let (def, rt) = self.graph_cx()?;
        let mut layout = input.layout.clone();
        for p in &gs.input_scan_list {
            let mut cols = Vec::new();
            collect_cols(p, &mut cols);
            for id in cols {
                if input.layout.contains(&id) {
                    return Err(unimplemented("This graph pattern"));
                }
                add(&mut layout, id);
            }
        }
        let scope = Scope {
            cols: &layout,
            up: Some(outer),
        };
        let mut scan_conj = Vec::new();
        if let Some(f) = gs.filter_expr.as_deref() {
            conjuncts(f, &mut scan_conj);
        }
        let mut assigned = vec![false; scan_conj.len()];
        let mut bound: Vec<i64> = input.layout.clone();
        let mut paths = Vec::new();
        let mut describe = Vec::new();
        for (pi, ps) in gs.input_scan_list.iter().enumerate() {
            check_path(ps)?;
            let mut filters = Vec::new();
            let mut tops = Vec::new();
            let mut items = Vec::new();
            self.graph_items(ps, &def, &scope, &mut items, &mut filters, Some(&mut tops))?;
            let mut mine = Vec::new();
            collect_cols(ps, &mut mine);
            let unbound: Vec<i64> = layout
                .iter()
                .copied()
                .filter(|c| !bound.contains(c))
                .collect();
            let (anchor_at, anchor, how) =
                self.graph_anchor(&def, &tops, &scan_conj, &unbound, &scope)?;
            describe.push(how);
            bound.extend(mine);
            let unbound: Vec<i64> = layout
                .iter()
                .copied()
                .filter(|c| !bound.contains(c))
                .collect();
            let last = pi + 1 == gs.input_scan_list.len();
            let mut after = Vec::new();
            for (i, c) in scan_conj.iter().enumerate() {
                if !assigned[i] && (last || !refs_any(c, &unbound)) {
                    assigned[i] = true;
                    after.push(self.expr(c, &scope)?);
                }
            }
            paths.push(Path {
                items,
                anchor_at,
                anchor,
                filters,
                mode: mode_of(ps)?,
                after,
            });
        }
        let optional = gs.optional == Some(true);
        let describe = format!(
            "Graph{}Match({}: {})",
            if optional { "Optional" } else { "" },
            def.name,
            describe.join(", ")
        );
        Ok(Planned {
            op: Box::new(GraphMatch {
                input: input.op,
                width: layout.len(),
                paths,
                optional,
                graph: rt,
                describe,
            }),
            layout,
        })
    }

    fn graph_items<'p>(
        &mut self,
        ps: &'p ResolvedGraphPathScanProto,
        def: &GraphDef,
        scope: &Scope,
        items: &mut Vec<Item>,
        filters: &mut Vec<Eval>,
        mut tops: Option<&mut Vec<Top<'p>>>,
    ) -> Result<()> {
        let pos = |id: i64| {
            scope
                .cols
                .iter()
                .position(|&c| c == id)
                .ok_or_else(|| Error::internal(format!("graph column {id} not in layout")))
        };
        for s in &ps.input_scan_list {
            match s.node.as_ref() {
                Some(PathNode::ResolvedGraphElementScanNode(e)) => {
                    let (b, orient) =
                        elem_base(e).ok_or_else(|| Error::internal("empty element scan"))?;
                    let id = *elem_cols(b)
                        .first()
                        .ok_or_else(|| Error::internal("element scan without column"))?;
                    let slots = self.slots(def, &b.target_element_table_list)?;
                    let mut fs = Vec::new();
                    if let Some(f) = b.filter_expr.as_deref() {
                        fs.push(self.expr(f, scope)?);
                    }
                    if let Some(t) = tops.as_deref_mut() {
                        t.push(Top {
                            item: items.len(),
                            col: id,
                            slots: slots.clone(),
                            filter: b.filter_expr.as_deref(),
                        });
                    }
                    let pat = ElemPat {
                        col: pos(id)?,
                        slots,
                        filters: fs,
                    };
                    items.push(match orient {
                        None => Item::Node(pat),
                        Some(o) => Item::Edge(
                            pat,
                            match EdgeOrientation::try_from(o) {
                                Ok(EdgeOrientation::Right) => Orient::Right,
                                Ok(EdgeOrientation::Left) => Orient::Left,
                                _ => Orient::Any,
                            },
                        ),
                    });
                }
                Some(PathNode::ResolvedGraphPathScanNode(p)) => {
                    check_path(p)?;
                    match p.quantifier.as_deref() {
                        None => {
                            if mode_of(p)? != Mode::Walk {
                                return Err(unimplemented("Path modes on a sub-path"));
                            }
                            self.graph_items(p, def, scope, items, filters, tops.as_deref_mut())?;
                        }
                        Some(q) => {
                            let lo = quant_bound(q.lower_bound.as_deref())?.unwrap_or(0);
                            let hi = quant_bound(q.upper_bound.as_deref())?
                                .ok_or_else(|| unimplemented("Unbounded path quantifiers"))?;
                            let mut inner = Vec::new();
                            let mut qf = Vec::new();
                            self.graph_items(p, def, scope, &mut inner, &mut qf, None)?;
                            let groups = p
                                .group_variable_list
                                .iter()
                                .map(|g| Ok((pos(nid(&g.element))?, pos(nid(&g.array))?)))
                                .collect::<Result<Vec<_>>>()?;
                            let head = p.head.as_ref().and_then(|c| c.column_id).unwrap_or(-1);
                            let tail = p.tail.as_ref().and_then(|c| c.column_id).unwrap_or(-1);
                            items.push(Item::Quant(Box::new(Quant {
                                items: inner,
                                lo,
                                hi,
                                head: pos(head)?,
                                tail: pos(tail)?,
                                groups,
                                filters: qf,
                                mode: mode_of(p)?,
                            })));
                        }
                    }
                }
                None => return Err(Error::internal("empty path element")),
            }
        }
        if let Some(f) = ps.filter_expr.as_deref() {
            filters.push(self.expr(f, scope)?);
        }
        Ok(())
    }

    /// Chooses where matching a path starts: an element bound by an earlier path, else one found
    /// by key or column values known up front, else the first node.
    fn graph_anchor(
        &mut self,
        def: &GraphDef,
        tops: &[Top],
        scan_conj: &[&AnyResolvedExprProto],
        unbound: &[i64],
        scope: &Scope,
    ) -> Result<(usize, Anchor, String)> {
        let mut best: Option<(usize, usize, Vec<SlotSeek>)> = None;
        for (ti, t) in tops.iter().enumerate() {
            let mut conj = Vec::new();
            if let Some(f) = t.filter {
                conjuncts(f, &mut conj);
            }
            conj.extend_from_slice(scan_conj);
            // A variable declared again: `x#4 = x#1`.
            for c in &conj {
                let Some((a, b)) = equal_args(c) else {
                    continue;
                };
                for (x, other) in [(a, b), (b, a)] {
                    if col_of(x) == Some(t.col)
                        && is_graph_type(super::expr_type(other))
                        && !refs_any(other, unbound)
                    {
                        let ev = self.expr(other, scope)?;
                        return Ok((t.item, Anchor::Seed(ev), format!("seed {}", t.col)));
                    }
                }
            }
            // `x.prop = <known value>`, per element table.
            let mut per: Vec<SlotSeek> = Vec::new();
            let mut any_seek = false;
            for &slot in &t.slots {
                let et = if slot < def.nodes.len() {
                    &def.nodes[slot]
                } else {
                    &def.edges[slot - def.nodes.len()]
                };
                let mut choice: Option<Option<(Vec<usize>, Vec<Eval>)>> = None;
                for c in &conj {
                    let Some((a, b)) = equal_args(c) else {
                        continue;
                    };
                    for (x, rhs) in [(a, b), (b, a)] {
                        let Some((col, prop)) = property_of(x) else {
                            continue;
                        };
                        if col != t.col || refs_any(rhs, unbound) {
                            continue;
                        }
                        let (Ok(lt), Ok(rt)) = (type_of(x), type_of(rhs)) else {
                            continue;
                        };
                        if lt != rt || !hash_key_type(&lt) {
                            continue;
                        }
                        let Some(pd) = et
                            .properties
                            .iter()
                            .find(|p| p.name.eq_ignore_ascii_case(prop))
                        else {
                            // The property is NULL on this table: the condition never holds.
                            choice = Some(None);
                            break;
                        };
                        if let Some(id) = col_of(&pd.expr)
                            && let Some(p) = et.layout.iter().position(|&c| c == id)
                        {
                            let ev = self.expr(rhs, scope)?;
                            choice = Some(Some((vec![p], vec![ev])));
                            break;
                        }
                    }
                    if choice.is_some() {
                        break;
                    }
                }
                match choice {
                    Some(None) => {}
                    Some(Some(s)) => {
                        any_seek = true;
                        per.push((slot, Some(s)));
                    }
                    None => per.push((slot, None)),
                }
            }
            let scans = per.iter().filter(|(_, s)| s.is_none()).count();
            if (any_seek || per.is_empty()) && best.as_ref().is_none_or(|b| scans < b.1) {
                best = Some((ti, scans, per));
            }
        }
        if let Some((ti, scans, per)) = best {
            let t = &tops[ti];
            let how = if scans == 0 { "seek" } else { "partial seek" };
            return Ok((t.item, Anchor::Seek(per), format!("{how} {}", t.col)));
        }
        let t = tops
            .first()
            .ok_or_else(|| Error::internal("graph path without elements"))?;
        Ok((t.item, Anchor::Scan, format!("scan {}", t.col)))
    }

    /// Property definitions compiled over base table rows, one per element table slot.
    fn property_evals(&mut self, name: &str) -> Result<Arc<Vec<Option<Eval>>>> {
        let key = name.to_ascii_lowercase();
        if let Some(e) = self.graph.as_ref().and_then(|g| g.props.get(&key)) {
            return Ok(e.clone());
        }
        let (def, _) = self.graph_cx()?;
        let mut evals = Vec::new();
        for t in def.nodes.iter().chain(&def.edges) {
            match t
                .properties
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(name))
            {
                Some(p) => {
                    let s = Scope {
                        cols: &t.layout,
                        up: None,
                    };
                    evals.push(Some(self.expr(&p.expr, &s)?));
                }
                None => evals.push(None),
            }
        }
        let evals = Arc::new(evals);
        if let Some(g) = self.graph.as_mut() {
            g.props.insert(key, evals.clone());
        }
        Ok(evals)
    }

    pub(super) fn graph_property(
        &mut self,
        g: &ResolvedGraphGetElementPropertyProto,
        scope: &Scope,
    ) -> Result<Eval> {
        let name = g
            .property
            .as_ref()
            .and_then(|p| p.name.as_deref())
            .ok_or_else(|| unimplemented("Dynamic graph properties"))?;
        let x = self.expr(
            g.expr
                .as_deref()
                .ok_or_else(|| Error::internal("property without element"))?,
            scope,
        )?;
        let evals = self.property_evals(name)?;
        Ok(Arc::new(move |cx: &Cx, env: &Env| {
            let v = x(cx, env)?;
            let Some(el) = decode(&v) else {
                return Ok(Value::Null);
            };
            match evals.get(el.slot).and_then(|e| e.as_ref()) {
                Some(f) => f(
                    cx,
                    &Env {
                        row: el.row,
                        up: None,
                    },
                ),
                None => Ok(Value::Null),
            }
        }))
    }

    /// Graph functions and comparisons of graph elements; `None` for any other call.
    pub(super) fn graph_call(
        &mut self,
        name: &str,
        b: &ResolvedFunctionCallBaseProto,
        scope: &Scope,
    ) -> Result<Option<Eval>> {
        let graph_arg = b
            .argument_list
            .first()
            .is_some_and(|a| is_graph_type(super::expr_type(a)));
        if !graph_arg {
            return Ok(None);
        }
        let args = b
            .argument_list
            .iter()
            .map(|a| self.expr(a, scope))
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(match (name, &args[..]) {
            ("labels", [x]) => {
                let (_, rt) = self.graph_cx()?;
                let x = x.clone();
                Arc::new(move |cx: &Cx, env: &Env| {
                    let v = x(cx, env)?;
                    Ok(match decode(&v) {
                        Some(el) => rt
                            .slots
                            .get(el.slot)
                            .map_or(Value::Null, |s| Value::Array(s.labels.clone())),
                        None => Value::Null,
                    })
                })
            }
            ("$equal" | "$not_equal", [a, c]) => {
                let (a, c) = (a.clone(), c.clone());
                let eq = name == "$equal";
                Arc::new(move |cx: &Cx, env: &Env| {
                    let (x, y) = (a(cx, env)?, c(cx, env)?);
                    Ok(match same_element(&x, &y) {
                        Some(s) => Value::Bool(s == eq),
                        None => Value::Null,
                    })
                })
            }
            ("$is_null", [x]) => {
                let x = x.clone();
                Arc::new(move |cx: &Cx, env: &Env| Ok(Value::Bool(x(cx, env)?.is_null())))
            }
            _ => return Err(unimplemented(format!("Function {name} on graph elements"))),
        }))
    }
}
