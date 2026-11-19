//! Runtime: the execution context, row environments and the operator trait.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use spanbolt_storage::{DbState, Result, Span, Target, Timestamp, Value};
use spanbolt_txn::ReadWriteTxn;

use crate::catalog::SqlCatalog;

pub type Row = Vec<Value>;
pub type RowIter<'a> = Box<dyn Iterator<Item = Result<Row>> + 'a>;

/// A compiled expression.
pub type Eval = Arc<dyn Fn(&Cx, &Env) -> Result<Value> + Send + Sync>;

/// Values visible to an expression: the current row and the rows of enclosing scopes
/// (correlated subqueries, index nested-loop joins).
#[derive(Clone, Copy)]
pub struct Env<'a> {
    pub row: &'a [Value],
    pub up: Option<&'a Env<'a>>,
}

impl<'a> Env<'a> {
    pub const EMPTY: Env<'static> = Env { row: &[], up: None };

    pub fn get(&self, depth: usize, idx: usize) -> &Value {
        let mut e = self;
        for _ in 0..depth {
            match e.up {
                Some(u) => e = u,
                None => return &Value::Null,
            }
        }
        e.row.get(idx).unwrap_or(&Value::Null)
    }
}

/// Per-execution state shared by every operator of one statement.
pub struct Cx<'a> {
    pub state: &'a DbState,
    pub cat: &'a SqlCatalog,
    pub params: &'a [Value],
    pub now: Timestamp,
    /// Read-write transactions record what the statement reads.
    pub rw: Option<&'a ReadWriteTxn>,
    /// Materialised WITH entries, innermost last.
    pub with: RefCell<Vec<(String, Rc<Vec<Row>>)>>,
    /// Results of uncorrelated subqueries and decorrelated scans, keyed by node address.
    pub memo: RefCell<HashMap<usize, Rc<Memo>>>,
}

pub enum Memo {
    Value(Value),
    Set {
        keys: std::collections::HashSet<crate::value_ops::Key>,
        has_null: bool,
        empty: bool,
    },
    /// Rows of a decorrelated scan, by key (`HashLookup`).
    Buckets(HashMap<crate::value_ops::Key, Rc<Vec<Row>>>),
}

impl Cx<'_> {
    pub fn record(&self, target: Target, spans: &[Span]) {
        if let Some(rw) = self.rw {
            rw.record_read(target, spans);
        }
    }
}

/// A physical operator. `open` starts a scan; correlated operators read `env`.
pub trait Op: Send + Sync {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>>;
    /// One-line description for plan tests and debugging.
    fn describe(&self, out: &mut String, depth: usize);
}

pub type BoxOp = Box<dyn Op>;

pub fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}
