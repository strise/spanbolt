//! Graph query execution: graph element values, adjacency lookups and the pattern matcher that
//! `GraphScan` lowers to.
//!
//! A graph element is `STRUCT<slot INT64, key BYTES, row STRUCT>`: the element table it came from
//! (node tables first, then edge tables), its encoded primary key and the base table row.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use spanbolt_storage::{
    Code, DbState, Error, Result, Row as StoredRow, RowMap, Span, TableId, Target, Value,
};

use crate::exec::{BoxOp, Cx, Env, Eval, Op, Row, RowIter, indent};
use crate::value_ops::{Key, truthy};
use spanbolt_storage::key::encode_key;

// ------------------------------------------------------------------ element values

pub fn element(slot: usize, key: &Arc<[u8]>, row: &StoredRow) -> Value {
    Value::Struct(Arc::from(vec![
        Value::Int64(slot as i64),
        Value::Bytes(key.clone()),
        Value::Struct(row.clone()),
    ]))
}

pub struct Elem<'v> {
    pub slot: usize,
    pub key: &'v Arc<[u8]>,
    pub row: &'v StoredRow,
}

pub fn decode(v: &Value) -> Option<Elem<'_>> {
    let Value::Struct(f) = v else { return None };
    match &f[..] {
        [Value::Int64(s), Value::Bytes(key), Value::Struct(row)] => Some(Elem {
            slot: *s as usize,
            key,
            row,
        }),
        _ => None,
    }
}

/// Graph element equality: the same row of the same element table.
pub fn same_element(a: &Value, b: &Value) -> Option<bool> {
    let (a, b) = (decode(a)?, decode(b)?);
    Some(a.slot == b.slot && a.key == b.key)
}

// ------------------------------------------------------------------ graph shape at runtime

/// The element tables of one graph, by slot.
pub struct GraphRt {
    pub slots: Vec<SlotRt>,
}

pub struct SlotRt {
    pub table: TableId,
    pub labels: Arc<[Value]>,
    /// Edges only.
    pub source: Option<EndRt>,
    pub destination: Option<EndRt>,
}

pub struct EndRt {
    pub node: usize,
    pub edge_columns: Vec<usize>,
    pub node_columns: Vec<usize>,
}

impl GraphRt {
    pub fn new(g: &crate::graph::GraphDef) -> GraphRt {
        let n = g.nodes.len();
        let end = |r: &Option<crate::graph::NodeRef>| {
            r.as_ref().map(|r| EndRt {
                node: r.node,
                edge_columns: r.edge_columns.clone(),
                node_columns: r.node_columns.clone(),
            })
        };
        let slots = g
            .nodes
            .iter()
            .chain(&g.edges)
            .enumerate()
            .map(|(i, t)| SlotRt {
                table: t.table_id,
                labels: t.labels.iter().map(|l| Value::string(l)).collect(),
                source: if i >= n { end(&t.source) } else { None },
                destination: if i >= n { end(&t.destination) } else { None },
            })
            .collect();
        GraphRt { slots }
    }
}

// ------------------------------------------------------------------ lookups

type Index = HashMap<Key, Vec<(Arc<[u8]>, StoredRow)>>;
type Indexes = HashMap<(TableId, Vec<usize>), (RowMap, Arc<Index>)>;
/// A base table row found for an element table slot: (slot, primary key, row).
type Found = (usize, Arc<[u8]>, StoredRow);

/// Hash indexes over base table columns, rebuilt when the table changes. Edges are found by
/// their source or destination columns, which are rarely a key.
#[derive(Default)]
pub struct IndexCache {
    inner: Mutex<Indexes>,
}

impl IndexCache {
    fn get(&self, state: &DbState, table: TableId, cols: &[usize]) -> Arc<Index> {
        let rows = state.rows(table);
        let k = (table, cols.to_vec());
        if let Ok(m) = self.inner.lock()
            && let Some((snap, idx)) = m.get(&k)
            && snap.ptr_eq(rows)
        {
            return idx.clone();
        }
        let mut idx: Index = HashMap::new();
        for (key, row) in rows.iter() {
            let vals: Vec<Value> = cols.iter().map(|&c| row[c].clone()).collect();
            if vals.iter().any(Value::is_null) {
                continue;
            }
            idx.entry(Key(vals))
                .or_default()
                .push((key.clone(), row.clone()));
        }
        let idx = Arc::new(idx);
        if let Ok(mut m) = self.inner.lock() {
            m.insert(k, (rows.clone(), idx.clone()));
        }
        idx
    }
}

/// Rows of `table` whose `cols` equal `vals` (SQL equality: NULL matches nothing).
fn lookup(
    cx: &Cx,
    table: TableId,
    cols: &[usize],
    vals: Vec<Value>,
    out: &mut Vec<(Arc<[u8]>, StoredRow)>,
) -> Result<()> {
    if vals.iter().any(Value::is_null) {
        return Ok(());
    }
    let tm = cx.state.catalog.table(table);
    if tm.pk == cols {
        let Ok(key) = encode_key(&vals, &tm.pk_desc) else {
            return Ok(());
        };
        let key: Arc<[u8]> = Arc::from(key);
        cx.record(Target::Table(table), &[Span::Point(key.clone())]);
        if let Some((k, row)) = cx.state.rows(table).get_key_value(&key) {
            out.push((k.clone(), row.clone()));
        }
        return Ok(());
    }
    record_all(cx, table);
    let idx = cx.cat.graph_index.get(cx.state, table, cols);
    if let Some(rows) = idx.get(&Key(vals)) {
        out.extend(rows.iter().cloned());
    }
    Ok(())
}

fn record_all(cx: &Cx, table: TableId) {
    cx.record(
        Target::Table(table),
        &[Span::Range {
            lo: Arc::from(&[][..]),
            hi: None,
        }],
    );
}

// ------------------------------------------------------------------ patterns

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Orient {
    Right,
    Left,
    Any,
}

impl Orient {
    fn flip(self, dir: Dir) -> Orient {
        match (self, dir) {
            (o, Dir::Fwd) | (o @ Orient::Any, _) => o,
            (Orient::Right, Dir::Back) => Orient::Left,
            (Orient::Left, Dir::Back) => Orient::Right,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Walk,
    Trail,
    Acyclic,
}

/// One node or edge pattern: the working-row column it binds and the element tables it ranges
/// over.
pub struct ElemPat {
    pub col: usize,
    pub slots: Vec<usize>,
    pub filters: Vec<Eval>,
}

pub enum Item {
    Node(ElemPat),
    Edge(ElemPat, Orient),
    Quant(Box<Quant>),
}

pub struct Quant {
    pub items: Vec<Item>,
    pub lo: usize,
    pub hi: usize,
    pub head: usize,
    pub tail: usize,
    /// (inner column, array column).
    pub groups: Vec<(usize, usize)>,
    pub filters: Vec<Eval>,
    pub mode: Mode,
}

/// How one element table is searched: by these columns equal to these values, else scanned.
pub type SlotSeek = (usize, Option<(Vec<usize>, Vec<Eval>)>);

pub enum Anchor {
    /// Every element of the anchor's tables.
    Scan,
    /// An element bound before this path (a variable declared again).
    Seed(Eval),
    /// Elements whose columns equal values known before this path, per slot (`None`: scan).
    Seek(Vec<SlotSeek>),
}

pub struct Path {
    pub items: Vec<Item>,
    pub anchor_at: usize,
    pub anchor: Anchor,
    pub filters: Vec<Eval>,
    pub mode: Mode,
    /// Conditions of the enclosing MATCH that can run once this path is bound.
    pub after: Vec<Eval>,
}

/// `MATCH`: binds every path for each input row.
pub struct GraphMatch {
    pub input: BoxOp,
    pub width: usize,
    pub paths: Vec<Path>,
    pub optional: bool,
    pub graph: Arc<GraphRt>,
    pub describe: String,
}

impl Op for GraphMatch {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let it = self.input.open(cx, env)?;
        Ok(Box::new(it.flat_map(move |r| {
            let rows = r.and_then(|input| {
                let n = input.len();
                let mut row = input;
                row.resize(self.width, Value::Null);
                let mut m = Matcher {
                    cx,
                    env,
                    g: &self.graph,
                    out: Vec::new(),
                    trace: Vec::new(),
                    steps: 0,
                };
                m.paths(&self.paths, 0, &mut row)?;
                if m.out.is_empty() && self.optional {
                    row[n..].fill(Value::Null);
                    m.out.push(row);
                }
                Ok(m.out)
            });
            let v: Vec<Result<Row>> = match rows {
                Ok(rows) => rows.into_iter().map(Ok).collect(),
                Err(e) => vec![Err(e)],
            };
            v
        })))
    }

    fn describe(&self, out: &mut String, d: usize) {
        indent(out, d);
        out.push_str(&self.describe);
        out.push('\n');
        self.input.describe(out, d + 1);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Fwd,
    Back,
}

/// A traversed element, for path modes: (is edge, slot, key).
type Visit = (bool, usize, Arc<[u8]>);

struct Matcher<'a> {
    cx: &'a Cx<'a>,
    env: &'a Env<'a>,
    g: &'a GraphRt,
    out: Vec<Row>,
    trace: Vec<Visit>,
    steps: u64,
}

/// Element bindings one input row of a MATCH may try. A cancelled query keeps its thread until
/// the current row is done, so a runaway pattern must end on its own.
const MAX_STEPS: u64 = 50_000_000;

type K<'k> = dyn FnMut(&mut Matcher, &mut Vec<Value>, &Value) -> Result<()> + 'k;

fn visit(edge: bool, v: &Value) -> Option<Visit> {
    decode(v).map(|e| (edge, e.slot, e.key.clone()))
}

fn unique(trace: &[Visit], edges: bool) -> bool {
    let mut seen: Vec<&Visit> = trace.iter().filter(|v| v.0 == edges).collect();
    let n = seen.len();
    seen.sort_by(|a, b| (a.1, &a.2).cmp(&(b.1, &b.2)));
    seen.dedup();
    seen.len() == n
}

fn mode_ok(mode: Mode, trace: &[Visit]) -> bool {
    match mode {
        Mode::Walk => true,
        Mode::Trail => unique(trace, true),
        Mode::Acyclic => unique(trace, false),
    }
}

impl<'a> Matcher<'a> {
    fn pass(&self, filters: &[Eval], row: &[Value]) -> Result<bool> {
        let e = Env {
            row,
            up: Some(self.env),
        };
        for f in filters {
            if !truthy(&f(self.cx, &e)?) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn eval(&self, f: &Eval, row: &[Value]) -> Result<Value> {
        f(
            self.cx,
            &Env {
                row,
                up: Some(self.env),
            },
        )
    }

    fn paths(&mut self, paths: &[Path], pi: usize, row: &mut Vec<Value>) -> Result<()> {
        let Some(p) = paths.get(pi) else {
            self.out.push(row.clone());
            return Ok(());
        };
        let start = self.trace.len();
        let mut done = |m: &mut Matcher, row: &mut Vec<Value>, _: &Value| -> Result<()> {
            if !mode_ok(p.mode, &m.trace[start..])
                || !m.pass(&p.filters, row)?
                || !m.pass(&p.after, row)?
            {
                return Ok(());
            }
            m.paths(paths, pi + 1, row)
        };
        let (pat, edge) = match &p.items[p.anchor_at] {
            Item::Node(e) => (e, None),
            Item::Edge(e, o) => (e, Some(*o)),
            Item::Quant(_) => return Err(Error::internal("graph anchor on a quantified path")),
        };
        for (slot, key, base) in self.candidates(pat, &p.anchor, row)? {
            let v = element(slot, &key, &base);
            row[pat.col] = v.clone();
            if !self.pass(&pat.filters, row)? {
                continue;
            }
            let a = p.anchor_at;
            match edge {
                None => {
                    self.trace.push((false, slot, key.clone()));
                    self.both_ways(&p.items, a, &v, &v, row, &mut done)?;
                    self.trace.pop();
                }
                Some(o) => {
                    let Some((src, dst)) = self.endpoints(slot, &base)? else {
                        continue;
                    };
                    let mut ends = Vec::new();
                    if o != Orient::Left {
                        ends.push((src.clone(), dst.clone()));
                    }
                    if o != Orient::Right
                        && !(o == Orient::Any && same_element(&src, &dst) == Some(true))
                    {
                        ends.push((dst, src));
                    }
                    for (l, r) in ends {
                        let t = self.trace.len();
                        self.trace.push((true, slot, key.clone()));
                        self.trace.extend(visit(false, &l));
                        self.trace.extend(visit(false, &r));
                        self.both_ways(&p.items, a, &l, &r, row, &mut done)?;
                        self.trace.truncate(t);
                    }
                }
            }
        }
        Ok(())
    }

    /// Extends a path right of item `a` from node `right`, then left of it from node `left`.
    fn both_ways(
        &mut self,
        items: &[Item],
        a: usize,
        left: &Value,
        right: &Value,
        row: &mut Vec<Value>,
        k: &mut K,
    ) -> Result<()> {
        let left = left.clone();
        self.walk(
            items,
            a as isize + 1,
            Dir::Fwd,
            right,
            row,
            &mut |m, row, _| m.walk(items, a as isize - 1, Dir::Back, &left, row, k),
        )
    }

    fn candidates(&self, pat: &ElemPat, anchor: &Anchor, row: &[Value]) -> Result<Vec<Found>> {
        let mut out = Vec::new();
        match anchor {
            Anchor::Seed(f) => {
                let v = self.eval(f, row)?;
                if let Some(e) = decode(&v)
                    && pat.slots.contains(&e.slot)
                {
                    out.push((e.slot, e.key.clone(), e.row.clone()));
                }
            }
            Anchor::Scan => {
                for &s in &pat.slots {
                    self.scan(s, &mut out);
                }
            }
            Anchor::Seek(per) => {
                for (s, seek) in per {
                    match seek {
                        None => self.scan(*s, &mut out),
                        Some((cols, exprs)) => {
                            let vals = exprs
                                .iter()
                                .map(|f| self.eval(f, row))
                                .collect::<Result<Vec<_>>>()?;
                            let mut rows = Vec::new();
                            lookup(self.cx, self.g.slots[*s].table, cols, vals, &mut rows)?;
                            out.extend(rows.into_iter().map(|(k, r)| (*s, k, r)));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn scan(&self, slot: usize, out: &mut Vec<Found>) {
        let t = self.g.slots[slot].table;
        record_all(self.cx, t);
        out.extend(
            self.cx
                .state
                .rows(t)
                .iter()
                .map(|(k, r)| (slot, k.clone(), r.clone())),
        );
    }

    /// The node at one end of an edge, if it exists.
    fn end_node(&self, end: &EndRt, edge_row: &StoredRow) -> Result<Option<Value>> {
        let vals = end
            .edge_columns
            .iter()
            .map(|&c| edge_row[c].clone())
            .collect();
        let mut rows = Vec::new();
        lookup(
            self.cx,
            self.g.slots[end.node].table,
            &end.node_columns,
            vals,
            &mut rows,
        )?;
        Ok(rows.first().map(|(k, r)| element(end.node, k, r)))
    }

    fn endpoints(&self, slot: usize, edge_row: &StoredRow) -> Result<Option<(Value, Value)>> {
        let s = &self.g.slots[slot];
        let (Some(src), Some(dst)) = (&s.source, &s.destination) else {
            return Ok(None);
        };
        let (Some(a), Some(b)) = (self.end_node(src, edge_row)?, self.end_node(dst, edge_row)?)
        else {
            return Ok(None);
        };
        Ok(Some((a, b)))
    }

    /// Edges of `slot` leaving node `from` towards `o`, with the node at their other end.
    fn edges(
        &self,
        slot: usize,
        o: Orient,
        from: &Elem,
        out: &mut Vec<(Value, Value)>,
    ) -> Result<()> {
        let s = &self.g.slots[slot];
        let (Some(src), Some(dst)) = (&s.source, &s.destination) else {
            return Ok(());
        };
        let sides: &[(&EndRt, &EndRt)] = match o {
            Orient::Right => &[(src, dst)],
            Orient::Left => &[(dst, src)],
            Orient::Any => &[(src, dst), (dst, src)],
        };
        for (i, (near, far)) in sides.iter().enumerate() {
            if near.node != from.slot {
                continue;
            }
            let vals = near
                .node_columns
                .iter()
                .map(|&c| from.row[c].clone())
                .collect();
            let mut rows = Vec::new();
            lookup(self.cx, s.table, &near.edge_columns, vals, &mut rows)?;
            for (k, r) in rows {
                let Some(next) = self.end_node(far, &r)? else {
                    continue;
                };
                // An undirected self-loop is one match, not two.
                if i == 1
                    && same_element(&next, &element(from.slot, from.key, from.row)) == Some(true)
                {
                    continue;
                }
                out.push((element(slot, &k, &r), next));
            }
        }
        Ok(())
    }

    fn walk(
        &mut self,
        items: &[Item],
        idx: isize,
        dir: Dir,
        cur: &Value,
        row: &mut Vec<Value>,
        k: &mut K,
    ) -> Result<()> {
        if idx < 0 || idx as usize >= items.len() {
            return k(self, row, cur);
        }
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(Error::new(
                Code::ResourceExhausted,
                "The graph pattern matches too many paths. Add filters or lower the path quantifier's upper bound.",
            ));
        }
        let next = if dir == Dir::Fwd { idx + 1 } else { idx - 1 };
        match &items[idx as usize] {
            Item::Node(p) => {
                let Some(e) = decode(cur) else { return Ok(()) };
                if !p.slots.contains(&e.slot) {
                    return Ok(());
                }
                row[p.col] = cur.clone();
                if !self.pass(&p.filters, row)? {
                    return Ok(());
                }
                self.walk(items, next, dir, cur, row, k)
            }
            Item::Edge(p, o) => {
                let Some(from) = decode(cur) else {
                    return Ok(());
                };
                let o = o.flip(dir);
                let mut steps = Vec::new();
                for &s in &p.slots {
                    self.edges(s, o, &from, &mut steps)?;
                }
                for (edge, node) in steps {
                    row[p.col] = edge.clone();
                    if !self.pass(&p.filters, row)? {
                        continue;
                    }
                    let t = self.trace.len();
                    self.trace.extend(visit(true, &edge));
                    self.trace.extend(visit(false, &node));
                    self.walk(items, next, dir, &node, row, k)?;
                    self.trace.truncate(t);
                }
                Ok(())
            }
            Item::Quant(q) => {
                let mut acc: Vec<Vec<Value>> = vec![Vec::new(); q.groups.len()];
                let start = self.trace.len();
                self.repeat(
                    q,
                    0,
                    dir,
                    cur,
                    cur,
                    start,
                    row,
                    &mut acc,
                    &mut |m, row, c| m.walk(items, next, dir, c, row, k),
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repeat(
        &mut self,
        q: &Quant,
        n: usize,
        dir: Dir,
        start: &Value,
        cur: &Value,
        trace_start: usize,
        row: &mut Vec<Value>,
        acc: &mut Vec<Vec<Value>>,
        k: &mut K,
    ) -> Result<()> {
        if n >= q.lo {
            let ok = q.mode == Mode::Walk || {
                let mut seg: Vec<Visit> = visit(false, start).into_iter().collect();
                seg.extend_from_slice(&self.trace[trace_start..]);
                mode_ok(q.mode, &seg)
            };
            if ok {
                let (head, tail) = match dir {
                    Dir::Fwd => (start, cur),
                    Dir::Back => (cur, start),
                };
                row[q.head] = head.clone();
                row[q.tail] = tail.clone();
                for (vals, &(_, arr)) in acc.iter().zip(&q.groups) {
                    let mut vals = vals.clone();
                    if dir == Dir::Back {
                        vals.reverse();
                    }
                    row[arr] = Value::Array(Arc::from(vals));
                }
                k(self, row, cur)?;
            }
        }
        if n >= q.hi {
            return Ok(());
        }
        let first = match dir {
            Dir::Fwd => 0,
            Dir::Back => q.items.len() as isize - 1,
        };
        self.walk(&q.items, first, dir, cur, row, &mut |m, row, c| {
            if !m.pass(&q.filters, row)? {
                return Ok(());
            }
            for (vals, &(inner, _)) in acc.iter_mut().zip(&q.groups) {
                vals.push(row[inner].clone());
            }
            let r = m.repeat(q, n + 1, dir, start, c, trace_start, row, acc, k);
            for vals in acc.iter_mut() {
                vals.pop();
            }
            r
        })
    }
}
