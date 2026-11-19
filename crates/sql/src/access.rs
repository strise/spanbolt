//! Table and index scans with key ranges computed at open time.

use std::sync::Arc;

use spanbolt_storage::key::encode_part;
use spanbolt_storage::{
    IndexId, Key, KeyRange, KeySet, Result, Row, RowMap, Span, TableId, Target, Value,
};

use crate::exec::{Cx, Env, Eval, Op, RowIter, indent};

/// Values one key column is constrained to.
pub enum KeyPart {
    /// `col = e` for any of the expressions (`IN (a, b)`).
    Eq(Vec<Eval>),
    /// `col IN UNNEST(e)`.
    InArray(Eval),
    /// `col IS NULL`.
    IsNull,
}

pub struct Bound {
    pub expr: Eval,
    pub inclusive: bool,
}

pub struct RangePart {
    pub lo: Option<Bound>,
    pub hi: Option<Bound>,
    pub desc: bool,
}

pub enum Access {
    Full,
    /// `SEARCH(column, query)` over a column in a search index: candidate rows from its inverted
    /// index (a full scan when the query gives no restriction).
    Search {
        column: usize,
        query: Eval,
        /// Parse the query with the emulator's grammar.
        emulator: bool,
    },
    Key {
        parts: Vec<KeyPart>,
        range: Option<RangePart>,
    },
}

pub enum Source {
    Table,
    /// Index entries; `entry_pos[i]` is the entry position of output column i when covered.
    Index {
        index: IndexId,
        /// Entry positions of the table's primary key columns, for the back-join.
        pk_entry_pos: Vec<usize>,
        /// `Some` when every output column is stored in the index.
        covered: Option<Vec<usize>>,
    },
}

pub struct TableScan {
    pub table: TableId,
    pub name: String,
    pub source: Source,
    pub access: Access,
    /// Table column positions to output.
    pub columns: Vec<usize>,
    pub key_len: usize,
}

/// Rows of a map within owned spans, in span order.
struct SpanRows<'a> {
    map: &'a RowMap,
    spans: Vec<Span>,
    next: usize,
    cur: Option<imbl::ordmap::RangedIter<'a, Key, Row, imbl::shared_ptr::DefaultSharedPtr>>,
}

impl<'a> Iterator for SpanRows<'a> {
    type Item = &'a Row;
    fn next(&mut self) -> Option<&'a Row> {
        loop {
            if let Some(it) = &mut self.cur {
                if let Some((_, r)) = it.next() {
                    return Some(r);
                }
                self.cur = None;
            }
            let s = self.spans.get(self.next)?;
            self.next += 1;
            match s {
                Span::Point(k) => {
                    if let Some(r) = self.map.get(&**k) {
                        return Some(r);
                    }
                }
                Span::Range { .. } => self.cur = Some(self.map.range::<_, [u8]>(s.bounds())),
            }
        }
    }
}

const MAX_POINTS: usize = 100_000;

/// Expands the per-column value lists into key prefixes; `None` = matches nothing.
fn prefixes(cx: &Cx, env: &Env, parts: &[KeyPart]) -> Result<Vec<Vec<Value>>> {
    let mut out: Vec<Vec<Value>> = vec![Vec::new()];
    for p in parts {
        let vals: Vec<Value> = match p {
            KeyPart::IsNull => vec![Value::Null],
            KeyPart::Eq(es) => {
                let mut v = Vec::with_capacity(es.len());
                for e in es {
                    let x = e(cx, env)?;
                    if !x.is_null() {
                        v.push(x);
                    }
                }
                v
            }
            KeyPart::InArray(e) => match e(cx, env)? {
                Value::Array(a) => a.iter().filter(|x| !x.is_null()).cloned().collect(),
                _ => Vec::new(),
            },
        };
        let mut vals = vals;
        vals.sort_by(crate::value_ops::order_cmp);
        vals.dedup_by(|a, b| crate::value_ops::group_eq(a, b));
        let mut next = Vec::with_capacity(out.len() * vals.len());
        for pre in &out {
            for v in &vals {
                let mut k = pre.clone();
                k.push(v.clone());
                next.push(k);
            }
        }
        out = next;
        if out.len() > MAX_POINTS {
            return Err(spanbolt_storage::Error::new(
                spanbolt_storage::Code::ResourceExhausted,
                "Too many key combinations in the query's key predicates.",
            ));
        }
    }
    Ok(out)
}

impl TableScan {
    fn target(&self) -> Target {
        match &self.source {
            Source::Table => Target::Table(self.table),
            Source::Index { index, .. } => Target::Index(*index),
        }
    }

    fn keyset(&self, cx: &Cx, env: &Env) -> Result<KeySet> {
        let Access::Key { parts, range } = &self.access else {
            return Ok(KeySet::all());
        };
        let pres = prefixes(cx, env, parts)?;
        let mut ks = KeySet::default();
        let Some(r) = range else {
            let is_index = matches!(self.source, Source::Index { .. });
            for p in pres {
                // Index entries may share a prefix (non-unique, or a partial key), so index
                // lookups are closed prefix ranges, never point keys.
                if !is_index && p.len() == self.key_len {
                    ks.keys.push(p);
                } else {
                    ks.ranges.push(KeyRange {
                        start: p.clone(),
                        start_closed: true,
                        end: p,
                        end_closed: true,
                    });
                }
            }
            return Ok(ks);
        };
        let eval = |b: &Option<Bound>| -> Result<Option<Option<(Value, bool)>>> {
            match b {
                None => Ok(Some(None)),
                Some(b) => {
                    let v = (b.expr)(cx, env)?;
                    if v.is_null() {
                        Ok(None)
                    } else {
                        Ok(Some(Some((v, b.inclusive))))
                    }
                }
            }
        };
        let (Some(lo), Some(hi)) = (eval(&r.lo)?, eval(&r.hi)?) else {
            return Ok(ks);
        };
        // In key order a DESC column runs from the high bound to the low bound.
        let (start, end) = if r.desc { (hi, lo) } else { (lo, hi) };
        for p in pres {
            let (mut s, sc) = (p.clone(), true);
            let (s, start_closed) = match &start {
                Some((v, inc)) => {
                    s.push(v.clone());
                    (s, *inc)
                }
                None => (s, sc),
            };
            let (e, end_closed) = match &end {
                Some((v, inc)) => {
                    let mut e = p.clone();
                    e.push(v.clone());
                    (e, *inc)
                }
                None => (p, true),
            };
            ks.ranges.push(KeyRange {
                start: s,
                start_closed,
                end: e,
                end_closed,
            });
        }
        Ok(ks)
    }
}

impl Op for TableScan {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let target = self.target();
        if let (
            Access::Search {
                column,
                query,
                emulator,
            },
            Source::Table,
        ) = (&self.access, &self.source)
        {
            let q = query(cx, env)?;
            if let Some(keys) =
                crate::search::search_keys(cx.state, self.table, *column, &q, *emulator)
            {
                // The candidates depend on every row's tokens: the read covers the table.
                let spans = cx.state.spans(target, &KeySet::all())?;
                cx.record(target, &spans);
                let table = cx.state.rows(self.table);
                return Ok(Box::new(keys.into_iter().filter_map(move |k| {
                    table
                        .get(&k)
                        .map(|r| Ok(self.columns.iter().map(|&c| r[c].clone()).collect()))
                })));
            }
        }
        let ks = self.keyset(cx, env)?;
        let spans = cx.state.spans(target, &ks)?;
        cx.record(target, &spans);
        let map = cx.state.map(target);
        let entries = SpanRows {
            map,
            spans: spans.to_vec(),
            next: 0,
            cur: None,
        };
        match &self.source {
            Source::Table => {
                Ok(Box::new(entries.map(move |r| {
                    Ok(self.columns.iter().map(|&c| r[c].clone()).collect())
                })))
            }
            Source::Index {
                covered: Some(pos), ..
            } => {
                Ok(Box::new(entries.map(move |r| {
                    Ok(pos.iter().map(|&p| r[p].clone()).collect())
                })))
            }
            Source::Index {
                pk_entry_pos,
                covered: None,
                ..
            } => {
                let tm = cx.state.catalog.table(self.table);
                let table = cx.state.rows(self.table);
                let mut buf = Vec::with_capacity(32);
                Ok(Box::new(entries.filter_map(move |entry| {
                    buf.clear();
                    for (&p, &desc) in pk_entry_pos.iter().zip(&tm.pk_desc) {
                        if let Err(e) = encode_part(&mut buf, &entry[p], desc) {
                            return Some(Err(e));
                        }
                    }
                    cx.record(
                        Target::Table(self.table),
                        &[Span::Point(Arc::from(&buf[..]))],
                    );
                    table
                        .get(&buf[..])
                        .map(|row| Ok(self.columns.iter().map(|&c| row[c].clone()).collect()))
                })))
            }
        }
    }

    fn describe(&self, out: &mut String, d: usize) {
        indent(out, d);
        let src = match &self.source {
            Source::Table => format!("Table({})", self.name),
            Source::Index { index, covered, .. } => format!(
                "Index({}#{index}{})",
                self.name,
                if covered.is_some() { "" } else { ", backjoin" }
            ),
        };
        let acc = match &self.access {
            Access::Full => "full scan".to_string(),
            Access::Search { .. } => "search index".to_string(),
            Access::Key { parts, range } => format!(
                "key prefix {}{}",
                parts.len(),
                if range.is_some() { " + range" } else { "" }
            ),
        };
        out.push_str(&format!("Scan {src}: {acc}\n"));
    }
}
