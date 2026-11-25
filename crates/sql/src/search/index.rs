//! The search-index access path: an inverted index (token -> row keys) of a TOKENIZE_FULLTEXT
//! column, kept per table version and updated incrementally from the row map's structural diff.
//! It yields a superset of the rows SEARCH can match; the WHERE clause still filters them.

use std::sync::{Arc, LazyLock, Mutex};

use imbl::ordmap::DiffItem;
use spanbolt_storage::{DbState, Key, RowMap, TableId, Value};

use super::eval::compiled_for;
use super::rquery::Node;
use super::tokens::{GAP, is_signature, iter};

type Set = imbl::OrdSet<Key>;

struct Postings {
    rows: RowMap,
    map: imbl::HashMap<Vec<u8>, Set>,
}

struct Entry {
    table: String,
    column: usize,
    postings: Arc<Postings>,
}

const CACHE_SIZE: usize = 32;

/// Most recently used last.
static CACHE: LazyLock<Mutex<Vec<Entry>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub(super) fn cache_len() -> usize {
    CACHE.lock().map_or(0, |c| c.len())
}

fn tokens_of(v: &Value) -> impl Iterator<Item = &[u8]> {
    let b: &[u8] = match v {
        Value::TokenList(b) => b,
        _ => &[],
    };
    iter(b)
        .filter_map(|t| t.ok())
        .filter(|t| !is_signature(t) && *t != GAP)
        .map(|t| t.strip_prefix(&[super::tokens::ALT][..]).unwrap_or(t))
}

fn add(map: &mut imbl::HashMap<Vec<u8>, Set>, v: &Value, k: &Key) {
    for t in tokens_of(v) {
        map.entry(t.to_vec()).or_default().insert(k.clone());
    }
}

fn remove(map: &mut imbl::HashMap<Vec<u8>, Set>, v: &Value, k: &Key) {
    for t in tokens_of(v) {
        if let Some(s) = map.get_mut(t) {
            s.remove(k);
            if s.is_empty() {
                map.remove(t);
            }
        }
    }
}

fn postings(state: &DbState, table: TableId, column: usize) -> Arc<Postings> {
    let name = state.catalog.table(table).name.clone();
    let rows = state.rows(table);
    let base = {
        let Ok(mut c) = CACHE.lock() else {
            return Arc::new(build(None, rows, column));
        };
        let hit = c
            .iter()
            .rposition(|e| e.table == name && e.column == column);
        match hit {
            Some(i) if c[i].postings.rows.ptr_eq(rows) => {
                let e = c.remove(i);
                let p = e.postings.clone();
                c.push(e);
                return p;
            }
            Some(i) => Some(c[i].postings.clone()),
            None => None,
        }
    };
    let p = Arc::new(build(base.as_deref(), rows, column));
    if let Ok(mut c) = CACHE.lock() {
        if c.len() >= CACHE_SIZE {
            c.remove(0);
        }
        c.push(Entry {
            table: name,
            column,
            postings: p.clone(),
        });
    }
    p
}

fn build(base: Option<&Postings>, rows: &RowMap, column: usize) -> Postings {
    let empty = RowMap::new();
    let (old_rows, mut map) = match base {
        Some(b) => (&b.rows, b.map.clone()),
        None => (&empty, imbl::HashMap::new()),
    };
    let col = |r: &spanbolt_storage::Row| r.get(column).cloned().unwrap_or(Value::Null);
    for d in old_rows.diff(rows) {
        match d {
            DiffItem::Add(k, r) => add(&mut map, &col(r), k),
            DiffItem::Remove(k, r) => remove(&mut map, &col(r), k),
            DiffItem::Update {
                old: (k, o),
                new: (_, n),
            } => {
                let (o, n) = (col(o), col(n));
                if o != n {
                    remove(&mut map, &o, k);
                    add(&mut map, &n, k);
                }
            }
        }
    }
    Postings {
        rows: rows.clone(),
        map,
    }
}

/// Rows that may match; `None` means every row (no restriction derivable).
fn candidates(p: &Postings, n: &Node) -> Option<Set> {
    let all = |c: &[Node]| {
        let mut acc: Option<Set> = None;
        for c in c {
            if let Some(s) = candidates(p, c) {
                acc = Some(match acc {
                    None => s,
                    Some(a) => a.intersection(s),
                });
            }
        }
        acc
    };
    match n {
        Node::Term(t) => Some(p.map.get(t.as_slice()).cloned().unwrap_or_default()),
        Node::Number(_) | Node::Not(_) => None,
        Node::And(c) | Node::Phrase(c) | Node::Around(c) | Node::InOrder(c) => all(c),
        Node::Or(c) => {
            let mut acc = Set::new();
            for c in c {
                acc = acc.union(candidates(p, c)?);
            }
            Some(acc)
        }
    }
}

/// NOT inside AROUND fails at evaluation; such queries must reach every row.
fn not_in_around(n: &Node, around: bool) -> bool {
    match n {
        Node::Not(c) => around || not_in_around(c, around),
        Node::Around(c) | Node::InOrder(c) => c.iter().any(|c| not_in_around(c, true)),
        Node::And(c) | Node::Or(c) | Node::Phrase(c) => c.iter().any(|c| not_in_around(c, around)),
        Node::Term(_) | Node::Number(_) => false,
    }
}

/// Keys of the rows `SEARCH(column, query)` can match, in key order, or `None` to scan the table.
pub fn search_keys(
    state: &DbState,
    table: TableId,
    column: usize,
    query: &Value,
    emulator: bool,
) -> Option<Vec<Key>> {
    let q = match query {
        Value::String(q) => q,
        // SEARCH(.., NULL) is NULL for every row.
        _ => return Some(Vec::new()),
    };
    if q.is_empty() {
        return Some(Vec::new());
    }
    let c = compiled_for(q, emulator);
    let node = match &c.parse {
        Ok(Some(n)) => n,
        Ok(None) => return Some(Vec::new()),
        // The filter reports the parse error.
        Err(_) => return None,
    };
    if not_in_around(node, false) {
        return None;
    }
    let p = postings(state, table, column);
    candidates(&p, node).map(|s| s.into_iter().collect())
}
