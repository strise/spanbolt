//! Physical operators.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use spanbolt_storage::{Error, Result, Value};

use crate::exec::{BoxOp, Cx, Env, Eval, Op, Row, RowIter, indent};
use crate::value_ops::{Key, order_cmp, out_of_range, truthy};

fn line(out: &mut String, depth: usize, s: &str) {
    indent(out, depth);
    out.push_str(s);
    out.push('\n');
}

pub struct SingleRow;

impl Op for SingleRow {
    fn open<'a>(&'a self, _: &'a Cx<'a>, _: &'a Env<'a>) -> Result<RowIter<'a>> {
        Ok(Box::new(std::iter::once(Ok(Vec::new()))))
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(out, d, "SingleRow");
    }
}

pub struct Filter {
    pub input: BoxOp,
    pub pred: Eval,
}

impl Op for Filter {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let it = self.input.open(cx, env)?;
        Ok(Box::new(it.filter_map(move |r| match r {
            Ok(row) => {
                let e = Env {
                    row: &row,
                    up: Some(env),
                };
                match (self.pred)(cx, &e) {
                    Ok(v) if truthy(&v) => Some(Ok(row)),
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                }
            }
            Err(e) => Some(Err(e)),
        })))
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(out, d, "Filter");
        self.input.describe(out, d + 1);
    }
}

/// Appends computed columns to each input row.
pub struct Compute {
    pub input: BoxOp,
    pub exprs: Vec<Eval>,
}

impl Op for Compute {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let it = self.input.open(cx, env)?;
        Ok(Box::new(it.map(move |r| {
            let mut row = r?;
            let n = row.len();
            row.reserve(self.exprs.len());
            for ex in &self.exprs {
                // Later expressions only see the input columns.
                let v = {
                    let e = Env {
                        row: &row[..n],
                        up: Some(env),
                    };
                    ex(cx, &e)?
                };
                row.push(v);
            }
            Ok(row)
        })))
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(out, d, &format!("Compute({})", self.exprs.len()));
        self.input.describe(out, d + 1);
    }
}

/// Keeps the columns at `positions`, in that order.
pub struct Select {
    pub input: BoxOp,
    pub positions: Vec<usize>,
}

impl Op for Select {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let it = self.input.open(cx, env)?;
        Ok(Box::new(it.map(move |r| {
            let row = r?;
            Ok(self
                .positions
                .iter()
                .map(|&p| row.get(p).cloned().unwrap_or(Value::Null))
                .collect())
        })))
    }
    fn describe(&self, out: &mut String, d: usize) {
        self.input.describe(out, d);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

/// Nested-loop join. A dependent right side is reopened per left row with the left row as its
/// outer environment (index nested loop, lateral joins); otherwise it is materialised once.
pub struct NestedLoop {
    pub left: BoxOp,
    pub right: BoxOp,
    pub kind: JoinKind,
    pub cond: Option<Eval>,
    pub dependent: bool,
    pub left_width: usize,
    pub right_width: usize,
}

impl Op for NestedLoop {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let left = self.left.open(cx, env)?;
        let build = if self.dependent {
            Build::Dependent(&*self.right)
        } else {
            Build::All(self.right.open(cx, env)?.collect::<Result<_>>()?)
        };
        Ok(Box::new(JoinIter::new(
            cx,
            env,
            JoinShape {
                kind: self.kind,
                cond: self.cond.as_ref(),
                left_width: self.left_width,
                right_width: self.right_width,
            },
            left,
            build,
        )))
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(
            out,
            d,
            &format!(
                "NestedLoopJoin({:?}{})",
                self.kind,
                if self.dependent { ", dependent" } else { "" }
            ),
        );
        self.left.describe(out, d + 1);
        self.right.describe(out, d + 1);
    }
}

/// A correlated scan decorrelated into a hash lookup: the input does not depend on the outer row,
/// so it runs once per statement and its rows are bucketed by the inner key columns. Each open
/// returns the bucket whose key equals the outer probe values. The predicate above still checks
/// every row, so the bucket only has to contain all rows that can match (NULL probes match none).
pub struct HashLookup {
    pub input: BoxOp,
    /// Positions of the inner key columns in the input rows.
    pub build: Vec<usize>,
    /// Outer values, evaluated against the environment the scan is opened with.
    pub probe: Vec<Eval>,
}

impl Op for HashLookup {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let mut key = Vec::with_capacity(self.probe.len());
        for p in &self.probe {
            let v = p(cx, env)?;
            if v.is_null() {
                return Ok(Box::new(std::iter::empty()));
            }
            key.push(v);
        }
        let id = self as *const Self as *const () as usize;
        let cached = cx.memo.borrow().get(&id).cloned();
        let memo = match cached {
            Some(m) => m,
            None => {
                let mut buckets: HashMap<Key, Vec<Row>> = HashMap::new();
                for r in self.input.open(cx, env)? {
                    let r = r?;
                    let k: Vec<Value> = self
                        .build
                        .iter()
                        .map(|&i| r.get(i).cloned().unwrap_or(Value::Null))
                        .collect();
                    if k.iter().any(Value::is_null) {
                        continue;
                    }
                    buckets.entry(Key(k)).or_default().push(r);
                }
                let m = Rc::new(crate::exec::Memo::Buckets(
                    buckets.into_iter().map(|(k, v)| (k, Rc::new(v))).collect(),
                ));
                cx.memo.borrow_mut().insert(id, m.clone());
                m
            }
        };
        let crate::exec::Memo::Buckets(b) = memo.as_ref() else {
            return Err(Error::internal("bad hash lookup memo"));
        };
        let Some(rows) = b.get(&Key(key)).cloned() else {
            return Ok(Box::new(std::iter::empty()));
        };
        Ok(Box::new((0..rows.len()).map(move |i| Ok(rows[i].clone()))))
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(
            out,
            d,
            &format!("HashLookup (decorrelated, {} keys)", self.build.len()),
        );
        self.input.describe(out, d + 1);
    }
}

/// Equi-join: builds a hash table on the right side.
pub struct HashJoin {
    pub left: BoxOp,
    pub right: BoxOp,
    pub kind: JoinKind,
    pub left_keys: Vec<Eval>,
    pub right_keys: Vec<Eval>,
    pub residual: Option<Eval>,
    pub left_width: usize,
    pub right_width: usize,
}

fn eval_key(cx: &Cx, env: &Env, row: &[Value], keys: &[Eval]) -> Result<Option<Key>> {
    let e = Env { row, up: Some(env) };
    let mut k = Vec::with_capacity(keys.len());
    for f in keys {
        let v = f(cx, &e)?;
        if v.is_null() {
            return Ok(None);
        }
        k.push(v);
    }
    Ok(Some(Key(k)))
}

impl Op for HashJoin {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let rows: Vec<Row> = self.right.open(cx, env)?.collect::<Result<_>>()?;
        // Chains of build rows per key, in build order: `heads[key]`, then `next[i]`.
        let mut heads: HashMap<Key, usize> = HashMap::with_capacity(rows.len());
        let mut next = vec![NONE; rows.len()];
        for (i, r) in rows.iter().enumerate().rev() {
            if let Some(k) = eval_key(cx, env, r, &self.right_keys)? {
                next[i] = heads.insert(k, i).unwrap_or(NONE);
            }
        }
        let left = self.left.open(cx, env)?;
        Ok(Box::new(JoinIter::new(
            cx,
            env,
            JoinShape {
                kind: self.kind,
                cond: self.residual.as_ref(),
                left_width: self.left_width,
                right_width: self.right_width,
            },
            left,
            Build::Hash {
                rows,
                heads,
                next,
                left_keys: &self.left_keys,
            },
        )))
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(out, d, &format!("HashJoin({:?})", self.kind));
        self.left.describe(out, d + 1);
        self.right.describe(out, d + 1);
    }
}

const NONE: usize = usize::MAX;

/// What a join pairs each probe (left) row with.
enum Build<'a> {
    /// Every build row, materialised once (cross and theta joins).
    All(Vec<Row>),
    /// Build rows chained per equi-join key.
    Hash {
        rows: Vec<Row>,
        heads: HashMap<Key, usize>,
        next: Vec<usize>,
        left_keys: &'a [Eval],
    },
    /// The right side reopened per left row, with the left row as its outer environment.
    Dependent(&'a dyn Op),
}

struct JoinShape<'a> {
    kind: JoinKind,
    cond: Option<&'a Eval>,
    left_width: usize,
    right_width: usize,
}

/// The left row being probed. `pos` is the next candidate: an index into the build rows (or
/// into `own` for a dependent right side), or a chain position for a hash build.
struct Probe {
    left: Row,
    own: Vec<Row>,
    pos: usize,
    any: bool,
}

/// Streams join output: one output row per `next`, holding only the build side. RIGHT and
/// FULL joins emit the unmatched build rows after the probe side runs out.
struct JoinIter<'a> {
    cx: &'a Cx<'a>,
    env: &'a Env<'a>,
    shape: JoinShape<'a>,
    left: RowIter<'a>,
    build: Build<'a>,
    matched: Vec<bool>,
    probe: Option<Probe>,
    /// Next build row to check for the unmatched tail, once the probe side is done.
    tail: Option<usize>,
    done: bool,
}

impl<'a> JoinIter<'a> {
    fn new(
        cx: &'a Cx<'a>,
        env: &'a Env<'a>,
        shape: JoinShape<'a>,
        left: RowIter<'a>,
        build: Build<'a>,
    ) -> Self {
        let tracked = match &build {
            Build::All(rows) | Build::Hash { rows, .. }
                if matches!(shape.kind, JoinKind::Right | JoinKind::Full) =>
            {
                rows.len()
            }
            _ => 0,
        };
        JoinIter {
            cx,
            env,
            shape,
            left,
            build,
            matched: vec![false; tracked],
            probe: None,
            tail: None,
            done: false,
        }
    }

    /// Starts probing a left row.
    fn start(&self, left: Row) -> Result<Probe> {
        let (own, pos) = match &self.build {
            Build::All(_) => (Vec::new(), 0),
            Build::Hash {
                heads, left_keys, ..
            } => {
                let pos = match eval_key(self.cx, self.env, &left, left_keys)? {
                    Some(k) => heads.get(&k).copied().unwrap_or(NONE),
                    None => NONE,
                };
                (Vec::new(), pos)
            }
            Build::Dependent(op) => {
                let e = Env {
                    row: &left,
                    up: Some(self.env),
                };
                let own = op.open(self.cx, &e)?.collect::<Result<Vec<_>>>()?;
                (own, 0)
            }
        };
        Ok(Probe {
            left,
            own,
            pos,
            any: false,
        })
    }

    /// The next output row for the current probe, or `None` when its candidates run out.
    fn advance(&mut self) -> Result<Option<Row>> {
        let Some(p) = self.probe.as_mut() else {
            return Ok(None);
        };
        loop {
            let (i, r) = match &self.build {
                Build::All(rows) => match rows.get(p.pos) {
                    Some(r) => {
                        p.pos += 1;
                        (p.pos - 1, r)
                    }
                    None => return Ok(None),
                },
                Build::Hash { rows, next, .. } => {
                    if p.pos == NONE {
                        return Ok(None);
                    }
                    let i = p.pos;
                    p.pos = next[i];
                    (i, &rows[i])
                }
                Build::Dependent(_) => match p.own.get(p.pos) {
                    Some(r) => {
                        p.pos += 1;
                        (NONE, r)
                    }
                    None => return Ok(None),
                },
            };
            let mut row = Vec::with_capacity(p.left.len() + r.len());
            row.extend_from_slice(&p.left);
            row.extend_from_slice(r);
            let ok = match self.shape.cond {
                None => true,
                Some(c) => {
                    let e = Env {
                        row: &row,
                        up: Some(self.env),
                    };
                    truthy(&c(self.cx, &e)?)
                }
            };
            if ok {
                p.any = true;
                if let Some(m) = self.matched.get_mut(i) {
                    *m = true;
                }
                return Ok(Some(row));
            }
        }
    }

    fn step(&mut self) -> Result<Option<Row>> {
        if let Some(t) = self.tail.as_mut() {
            let rows = match &self.build {
                Build::All(rows) | Build::Hash { rows, .. } => rows,
                Build::Dependent(_) => return Ok(None),
            };
            while let Some(m) = self.matched.get(*t) {
                let i = *t;
                *t += 1;
                if !m {
                    let mut row = vec![Value::Null; self.shape.left_width];
                    row.extend_from_slice(&rows[i]);
                    return Ok(Some(row));
                }
            }
            return Ok(None);
        }
        loop {
            if let Some(row) = self.advance()? {
                return Ok(Some(row));
            }
            if let Some(p) = self.probe.take()
                && !p.any
                && matches!(self.shape.kind, JoinKind::Left | JoinKind::Full)
            {
                let mut row = p.left;
                row.extend(std::iter::repeat_n(Value::Null, self.shape.right_width));
                return Ok(Some(row));
            }
            match self.left.next() {
                Some(l) => self.probe = Some(self.start(l?)?),
                None => {
                    self.tail = Some(0);
                    return self.step();
                }
            }
        }
    }
}

impl Iterator for JoinIter<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        if self.done {
            return None;
        }
        let r = self.step().transpose();
        if !matches!(r, Some(Ok(_))) {
            self.done = true;
        }
        r
    }
}

/// UNNEST: one row per element, appended to the input row (or alone without input).
pub struct Unnest {
    pub input: Option<BoxOp>,
    pub array: Eval,
    pub with_offset: bool,
    pub outer: bool,
    pub cond: Option<Eval>,
}

impl Op for Unnest {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let mut inputs: RowIter<'a> = match &self.input {
            Some(i) => i.open(cx, env)?,
            None => Box::new(std::iter::once(Ok(Vec::new()))),
        };
        // The input row being expanded, its array, the next element and whether any matched.
        let mut cur: Option<(Row, Value, usize, bool)> = None;
        let mut done = false;
        Ok(Box::new(std::iter::from_fn(move || {
            if done {
                return None;
            }
            let r = (|| -> Result<Option<Row>> {
                loop {
                    if let Some((base, arr, i, any)) = cur.as_mut() {
                        let elems: &[Value] = match arr {
                            Value::Array(a) => a,
                            _ => &[],
                        };
                        while let Some(v) = elems.get(*i) {
                            let mut row = Vec::with_capacity(base.len() + 2);
                            row.extend_from_slice(base);
                            row.push(v.clone());
                            if self.with_offset {
                                row.push(Value::Int64(*i as i64));
                            }
                            *i += 1;
                            if let Some(c) = &self.cond {
                                let e = Env {
                                    row: &row,
                                    up: Some(env),
                                };
                                if !truthy(&c(cx, &e)?) {
                                    continue;
                                }
                            }
                            *any = true;
                            return Ok(Some(row));
                        }
                        if let Some((mut row, _, _, false)) = cur.take()
                            && self.outer
                        {
                            row.push(Value::Null);
                            if self.with_offset {
                                row.push(Value::Null);
                            }
                            return Ok(Some(row));
                        }
                    }
                    let Some(base) = inputs.next().transpose()? else {
                        return Ok(None);
                    };
                    let arr = {
                        let e = Env {
                            row: &base,
                            up: Some(env),
                        };
                        (self.array)(cx, &e)?
                    };
                    cur = Some((base, arr, 0, false));
                }
            })()
            .transpose();
            if !matches!(r, Some(Ok(_))) {
                done = true;
            }
            r
        })))
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(out, d, "Unnest");
        if let Some(i) = &self.input {
            i.describe(out, d + 1);
        }
    }
}

pub struct SortKey {
    pub expr: Eval,
    pub desc: bool,
    pub nulls_first: bool,
}

pub struct Sort {
    pub input: BoxOp,
    pub keys: Vec<SortKey>,
}

pub fn compare_keys(keys: &[SortKey], a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    for (k, (x, y)) in keys.iter().zip(a.iter().zip(b)) {
        let o = match (x.is_null(), y.is_null()) {
            (true, true) => Equal,
            (true, false) => {
                if k.nulls_first {
                    Less
                } else {
                    Greater
                }
            }
            (false, true) => {
                if k.nulls_first {
                    Greater
                } else {
                    Less
                }
            }
            _ => {
                let o = order_cmp(x, y);
                if k.desc { o.reverse() } else { o }
            }
        };
        if o != Equal {
            return o;
        }
    }
    Equal
}

impl Op for Sort {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let mut rows: Vec<(Vec<Value>, Row)> = Vec::new();
        for r in self.input.open(cx, env)? {
            let r = r?;
            let e = Env {
                row: &r,
                up: Some(env),
            };
            let k = self
                .keys
                .iter()
                .map(|k| (k.expr)(cx, &e))
                .collect::<Result<Vec<_>>>()?;
            rows.push((k, r));
        }
        rows.sort_by(|a, b| compare_keys(&self.keys, &a.0, &b.0));
        Ok(Box::new(rows.into_iter().map(|(_, r)| Ok(r))))
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(out, d, "Sort");
        self.input.describe(out, d + 1);
    }
}

pub struct Limit {
    pub input: BoxOp,
    pub limit: Option<Eval>,
    pub offset: Option<Eval>,
}

fn count_arg(cx: &Cx, env: &Env, e: &Option<Eval>, what: &str) -> Result<Option<usize>> {
    let Some(e) = e else { return Ok(None) };
    match e(cx, env)? {
        Value::Int64(n) if n >= 0 => Ok(Some(n as usize)),
        Value::Int64(_) => Err(out_of_range(format!(
            "{what} expects a non-negative integer literal or parameter"
        ))),
        Value::Null => Err(out_of_range(format!("{what} must not be null"))),
        _ => Err(Error::invalid_argument(format!(
            "{what} must be an integer"
        ))),
    }
}

impl Op for Limit {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let limit = count_arg(cx, env, &self.limit, "LIMIT")?;
        let offset = count_arg(cx, env, &self.offset, "OFFSET")?.unwrap_or(0);
        let it = self.input.open(cx, env)?.skip(offset);
        Ok(match limit {
            Some(n) => Box::new(it.take(n)),
            None => Box::new(it),
        })
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(out, d, "Limit");
        self.input.describe(out, d + 1);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetKind {
    UnionAll,
    UnionDistinct,
    IntersectAll,
    IntersectDistinct,
    ExceptAll,
    ExceptDistinct,
}

pub struct SetOp {
    pub kind: SetKind,
    pub inputs: Vec<BoxOp>,
}

impl Op for SetOp {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let Some((first, others)) = self.inputs.split_first() else {
            return Ok(Box::new(std::iter::empty()));
        };
        Ok(match self.kind {
            // Inputs are opened one after the other, as the previous one runs out.
            SetKind::UnionAll => {
                Box::new(self.inputs.iter().map(move |i| i.open(cx, env)).flat_map(
                    |r| -> RowIter<'a> {
                        match r {
                            Ok(it) => it,
                            Err(e) => Box::new(std::iter::once(Err(e))),
                        }
                    },
                ))
            }
            SetKind::UnionDistinct => {
                let mut seen = HashSet::new();
                Box::new(
                    self.inputs
                        .iter()
                        .map(move |i| i.open(cx, env))
                        .flat_map(|r| -> RowIter<'a> {
                            match r {
                                Ok(it) => it,
                                Err(e) => Box::new(std::iter::once(Err(e))),
                            }
                        })
                        .filter(move |r| match r {
                            Ok(r) => seen.insert(Key(r.clone())),
                            Err(_) => true,
                        }),
                )
            }
            // The other inputs are materialised as counts; the first one streams through them
            // in order, as `(A op B) op C`.
            SetKind::IntersectAll | SetKind::ExceptAll => {
                let mut counts: Vec<HashMap<Key, usize>> = Vec::with_capacity(others.len());
                for o in others {
                    let mut c: HashMap<Key, usize> = HashMap::new();
                    for r in o.open(cx, env)? {
                        *c.entry(Key(r?)).or_default() += 1;
                    }
                    counts.push(c);
                }
                let intersect = self.kind == SetKind::IntersectAll;
                Box::new(first.open(cx, env)?.filter(move |r| {
                    let Ok(r) = r else { return true };
                    let k = Key(r.clone());
                    counts.iter_mut().all(|c| match c.get_mut(&k) {
                        Some(n) if *n > 0 => {
                            *n -= 1;
                            intersect
                        }
                        _ => !intersect,
                    })
                }))
            }
            SetKind::IntersectDistinct | SetKind::ExceptDistinct => {
                let mut sets: Vec<HashSet<Key>> = Vec::with_capacity(others.len());
                for o in others {
                    sets.push(
                        o.open(cx, env)?
                            .map(|r| r.map(Key))
                            .collect::<Result<_>>()?,
                    );
                }
                let intersect = self.kind == SetKind::IntersectDistinct;
                let mut seen = HashSet::new();
                Box::new(first.open(cx, env)?.filter(move |r| {
                    let Ok(r) = r else { return true };
                    let k = Key(r.clone());
                    let keep = if intersect {
                        sets.iter().all(|o| o.contains(&k))
                    } else {
                        sets.iter().all(|o| !o.contains(&k))
                    };
                    keep && seen.insert(k)
                }))
            }
        })
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(out, d, &format!("SetOp({:?})", self.kind));
        for i in &self.inputs {
            i.describe(out, d + 1);
        }
    }
}

pub struct With {
    pub entries: Vec<(String, BoxOp)>,
    pub body: BoxOp,
}

impl Op for With {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        // Entries are materialised in order; each sees the ones before it.
        let depth = cx.with.borrow().len();
        let mut frames = Vec::with_capacity(self.entries.len());
        let mut entries = || -> Result<()> {
            for (name, op) in &self.entries {
                let rows: Vec<Row> = op.open(cx, env)?.collect::<Result<_>>()?;
                let frame = (name.clone(), Rc::new(rows));
                cx.with.borrow_mut().push(frame.clone());
                frames.push(frame);
            }
            Ok(())
        };
        let r = entries();
        let body = r.and_then(|_| self.body.open(cx, env));
        cx.with.borrow_mut().truncate(depth);
        // The body streams. Operators below it may open WITH references lazily, so the entries
        // are in scope again for every step. Enclosing WITHs are on the call stack, so their
        // entries sit below these ones.
        let mut body = body?;
        Ok(Box::new(std::iter::from_fn(move || {
            let depth = cx.with.borrow().len();
            cx.with.borrow_mut().extend(frames.iter().cloned());
            let r = body.next();
            cx.with.borrow_mut().truncate(depth);
            r
        })))
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(out, d, "With");
        for (_, e) in &self.entries {
            e.describe(out, d + 1);
        }
        self.body.describe(out, d + 1);
    }
}

pub struct WithRef {
    pub name: String,
}

impl Op for WithRef {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, _: &'a Env<'a>) -> Result<RowIter<'a>> {
        let rows = cx
            .with
            .borrow()
            .iter()
            .rev()
            .find(|(n, _)| n == &self.name)
            .map(|(_, r)| r.clone())
            .ok_or_else(|| Error::internal(format!("WITH entry {} not found", self.name)))?;
        Ok(Box::new((0..rows.len()).map(move |i| Ok(rows[i].clone()))))
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(out, d, &format!("WithRef({})", self.name));
    }
}

/// The rows of an INFORMATION_SCHEMA table, built once per catalog. `lookup` (a constant
/// compared with `TABLE_NAME`, case-insensitively) narrows them to one table's rows; the filter
/// above still applies the exact predicate.
pub struct Generated {
    pub name: String,
    pub lookup: Option<Eval>,
}

impl Op for Generated {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let info = cx.cat.info_rows(&self.name)?;
        if let Some(ev) = &self.lookup {
            match ev(cx, env)? {
                Value::Null => return Ok(Box::new(std::iter::empty())),
                Value::String(name) => {
                    if let Some(hits) = info.by_table_name(&name) {
                        let hits = hits.to_vec();
                        return Ok(Box::new(
                            hits.into_iter().map(move |i| Ok(info.rows[i].clone())),
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(Box::new(
            (0..info.rows.len()).map(move |i| Ok(info.rows[i].clone())),
        ))
    }
    fn describe(&self, out: &mut String, d: usize) {
        let by = if self.lookup.is_some() {
            " by TABLE_NAME"
        } else {
            ""
        };
        line(out, d, &format!("Generated({}){by}", self.name));
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleMethod {
    Bernoulli,
    Reservoir,
}

/// TABLESAMPLE. BERNOULLI keeps each row with probability `size / 100`; RESERVOIR keeps `size`
/// rows chosen uniformly (Algorithm R), or every row when there are no more than that. `size` is a
/// literal or a parameter, checked when the scan opens as the emulator does. With `WITH WEIGHT`,
/// each row gets its scaling weight appended: `100 / percent` for BERNOULLI, `rows / size` for
/// RESERVOIR (1 when every row was kept).
pub struct Sample {
    pub input: BoxOp,
    pub method: SampleMethod,
    pub size: Eval,
    pub weight: bool,
}

/// SplitMix64, seeded from the OS once per scan. Sampling needs no cryptographic quality.
struct SampleRng(u64);

impl SampleRng {
    fn new() -> Result<Self> {
        let mut buf = [0u8; 8];
        getrandom::fill(&mut buf).map_err(|e| Error::internal(format!("random: {e}")))?;
        Ok(SampleRng(u64::from_le_bytes(buf)))
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [0, n).
    fn below(&mut self, n: u64) -> u64 {
        ((self.next() as u128 * n as u128) >> 64) as u64
    }
}

impl Op for Sample {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let size = match (self.size)(cx, env)? {
            Value::Null => return Err(out_of_range("SampleScan requires non-null size")),
            v => v,
        };
        let mut rng = SampleRng::new()?;
        match self.method {
            SampleMethod::Bernoulli => {
                let percent = match size {
                    Value::Int64(n) => n as f64,
                    Value::Float64(f) => f,
                    v => return Err(Error::internal(format!("TABLESAMPLE PERCENT {v:?}"))),
                };
                // NaN passes the range check and keeps no rows, as in the emulator.
                if !percent.is_nan() && !(0.0..=100.0).contains(&percent) {
                    return Err(out_of_range("PERCENT value must be in the range [0, 100]"));
                }
                let weight = self.weight.then(|| Value::Float64(100.0 / percent));
                let it = self.input.open(cx, env)?;
                Ok(Box::new(it.filter_map(move |r| match r {
                    Ok(mut row) => (rng.unit() * 100.0 < percent).then(|| {
                        row.extend(weight.clone());
                        Ok(row)
                    }),
                    Err(e) => Some(Err(e)),
                })))
            }
            SampleMethod::Reservoir => {
                let k = match size {
                    Value::Int64(n) if n >= 0 => n as u64,
                    Value::Int64(_) => {
                        return Err(out_of_range("SampleScan requires non-negative size"));
                    }
                    v => return Err(Error::internal(format!("TABLESAMPLE ROWS {v:?}"))),
                };
                // No capacity up front: `k` can be far larger than the input.
                let mut kept: Vec<Row> = Vec::new();
                let mut seen = 0u64;
                for r in self.input.open(cx, env)? {
                    let r = r?;
                    seen += 1;
                    if (kept.len() as u64) < k {
                        kept.push(r);
                    } else {
                        let j = rng.below(seen);
                        if j < k {
                            kept[j as usize] = r;
                        }
                    }
                }
                if self.weight {
                    let w = if seen > k {
                        seen as f64 / k as f64
                    } else {
                        1.0
                    };
                    for row in &mut kept {
                        row.push(Value::Float64(w));
                    }
                }
                Ok(Box::new(kept.into_iter().map(Ok)))
            }
        }
    }
    fn describe(&self, out: &mut String, d: usize) {
        line(out, d, &format!("Sample {:?}", self.method));
        self.input.describe(out, d + 1);
    }
}
