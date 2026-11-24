//! Immutable database states and the writer that derives new states from them.

use std::sync::Arc;

use imbl::{OrdMap, Vector};
use spanbolt_ddl::{ColumnType, DataCheck, OnDelete, Schema};

use crate::catalog::{Catalog, FkId, IndexId, IndexMeta, Lookup, TableId, TableMeta};
use crate::error::{Code, Error, Result};
use crate::key::{
    Key, KeySet, PointMode, Span, encode_key, encode_keyset, encode_part, prefix_end,
};
use crate::value::{COMMIT_TIMESTAMP_SENTINEL, Timestamp, Value};

/// A row: one value per table column, in schema order. For index entries: one value per
/// `IndexMeta::entry_columns`.
pub type Row = Arc<[Value]>;
pub type RowMap = OrdMap<Key, Row>;

/// What a key in a read or write set refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    Table(TableId),
    Index(IndexId),
}

/// One immutable database snapshot. Cloning is O(1); commits derive new states by structural sharing.
#[derive(Clone)]
pub struct DbState {
    pub schema: Arc<Schema>,
    pub catalog: Arc<Catalog>,
    pub tables: Vector<RowMap>,
    pub indexes: Vector<RowMap>,
    /// Constraint violations use production Spanner's message texts instead of the emulator's.
    /// Derived states (commits, schema changes) inherit it.
    pub prod_messages: bool,
}

impl std::fmt::Debug for DbState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbState")
            .field("tables", &self.catalog.tables.len())
            .field("indexes", &self.catalog.indexes.len())
            .finish()
    }
}

impl Default for DbState {
    fn default() -> Self {
        DbState {
            schema: Arc::new(Schema::default()),
            catalog: Arc::new(Catalog::default()),
            tables: Vector::new(),
            indexes: Vector::new(),
            prod_messages: false,
        }
    }
}

pub fn key_debug(values: &[Value], types: &[&ColumnType]) -> String {
    let mut s = String::from("{");
    for (i, (v, t)) in values.iter().zip(types).enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        v.debug_typed(t, &mut s);
    }
    s.push('}');
    s
}

/// Iterator over the rows of a map that fall in a list of spans.
pub struct SpanIter<'a> {
    map: &'a RowMap,
    spans: std::slice::Iter<'a, Span>,
    cur: Option<imbl::ordmap::RangedIter<'a, Key, Row, imbl::shared_ptr::DefaultSharedPtr>>,
}

impl<'a> Iterator for SpanIter<'a> {
    type Item = (&'a Key, &'a Row);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(it) = &mut self.cur {
                if let Some(x) = it.next() {
                    return Some(x);
                }
                self.cur = None;
            }
            match self.spans.next()? {
                Span::Point(k) => {
                    if let Some(row) = self.map.get(&**k) {
                        return Some((k, row));
                    }
                }
                s @ Span::Range { .. } => self.cur = Some(self.map.range::<_, [u8]>(s.bounds())),
            }
        }
    }
}

pub fn scan<'a>(map: &'a RowMap, spans: &'a [Span]) -> SpanIter<'a> {
    SpanIter {
        map,
        spans: spans.iter(),
        cur: None,
    }
}

impl DbState {
    pub fn new(schema: Schema) -> Result<DbState> {
        let catalog = Catalog::build(&schema)?;
        Ok(DbState {
            tables: (0..catalog.tables.len()).map(|_| RowMap::new()).collect(),
            indexes: (0..catalog.indexes.len()).map(|_| RowMap::new()).collect(),
            schema: Arc::new(schema),
            catalog: Arc::new(catalog),
            prod_messages: false,
        })
    }

    pub fn rows(&self, t: TableId) -> &RowMap {
        &self.tables[t]
    }

    pub fn index_entries(&self, i: IndexId) -> &RowMap {
        &self.indexes[i]
    }

    pub fn map(&self, target: Target) -> &RowMap {
        match target {
            Target::Table(t) => &self.tables[t],
            Target::Index(i) => &self.indexes[i],
        }
    }

    /// Encodes a `KeySet` for a table (exact point keys) or an index (point keys are prefixes).
    pub fn spans(&self, target: Target, ks: &KeySet) -> Result<Vec<Span>> {
        match target {
            Target::Table(t) => {
                let tm = self.catalog.table(t);
                encode_keyset(ks, &tm.pk_desc, PointMode::Exact, &tm.name)
            }
            Target::Index(i) => {
                let im = self.catalog.index(i);
                encode_keyset(ks, &im.key_desc, PointMode::Prefix, &im.name)
            }
        }
    }

    pub fn row_count(&self) -> usize {
        self.tables.iter().map(|m| m.len()).sum()
    }

    /// Runs a row-dependent DDL check (`Schema::data_checks`) against this state's rows. Run it
    /// on the state migrated to the schema that includes the statement.
    pub fn run_data_check(&self, check: &DataCheck) -> Result<()> {
        let rows = |table: &str| self.catalog.table_id(table).map(|t| (t, &self.tables[t]));
        match check {
            DataCheck::TableEmpty { table, message } => match rows(table) {
                Some((_, m)) if !m.is_empty() => Err(Error::failed_precondition(message.clone())),
                _ => Ok(()),
            },
            DataCheck::NoNulls { table, column } => {
                let Some((t, m)) = rows(table) else {
                    return Ok(());
                };
                let tm = self.catalog.table(t);
                let Some(c) = tm.column(column) else {
                    return Ok(());
                };
                match m.values().find(|r| r[c].is_null()) {
                    Some(r) => Err(Error::failed_precondition(format!(
                        "Cannot specify a null value for column: {} in table: {} referenced by key: {}",
                        tm.columns[c].name,
                        tm.name,
                        pk_debug(tm, r)
                    ))),
                    None => Ok(()),
                }
            }
            DataCheck::MaxLength {
                table,
                column,
                limit,
            } => {
                let Some((t, m)) = rows(table) else {
                    return Ok(());
                };
                let tm = self.catalog.table(t);
                let Some(c) = tm.column(column) else {
                    return Ok(());
                };
                for r in m.values() {
                    if let Some(len) = longest(&r[c])
                        && len > *limit
                    {
                        return Err(Error::failed_precondition(format!(
                            "Reducing the length of column {} to {limit} is not allowed because it has a value of length {len} at key: {}",
                            tm.columns[c].name,
                            pk_debug(tm, r)
                        )));
                    }
                }
                Ok(())
            }
            // Enforced by the index backfill in `with_schema`.
            DataCheck::UniqueIndex { .. } => Ok(()),
            DataCheck::ForeignKey { table, name } => {
                let Some(fk) = self
                    .catalog
                    .foreign_keys
                    .iter()
                    .position(|f| f.name.eq_ignore_ascii_case(name))
                else {
                    return Ok(());
                };
                let child = self.catalog.foreign_keys[fk].table;
                if rows(table).is_none() {
                    return Ok(());
                }
                for row in self.tables[child].values() {
                    self.check_fk_child(fk, row)?;
                }
                Ok(())
            }
            DataCheck::Check { table, name } => {
                let Some((t, m)) = rows(table) else {
                    return Ok(());
                };
                if m.is_empty() {
                    return Ok(());
                }
                let tm = self.catalog.table(t);
                let Some(ev) = crate::expr::expr_evaluator() else {
                    return Err(crate::expr::no_evaluator(crate::expr::CHECKS));
                };
                let Some(ck) = tm.checks.iter().find(|c| c.name.eq_ignore_ascii_case(name)) else {
                    return Ok(());
                };
                for r in m.values() {
                    if ev.eval(&ck.expr, tm, r, &ColumnType::Bool)? == Value::Bool(false) {
                        // OUT_OF_RANGE, as for the same violation in a mutation.
                        return Err(Error::new(
                            Code::OutOfRange,
                            format!(
                                "Check constraint `{}`.`{}` is violated for key {}",
                                tm.name,
                                ck.name,
                                pk_debug(tm, r)
                            ),
                        ));
                    }
                }
                Ok(())
            }
        }
    }

    /// Whether `lookup` finds a row for `values` (in lookup-key order).
    fn lookup_hit(&self, lookup: &Lookup, values: &[Value]) -> Result<bool> {
        let enc = encode_key(values, &lookup.desc)?;
        let map = self.map(lookup.target);
        if lookup.point {
            return Ok(map.get(&enc[..]).is_some());
        }
        let end = prefix_end(&enc);
        Ok(map
            .range::<_, [u8]>((
                std::ops::Bound::Included(&enc[..]),
                std::ops::Bound::Excluded(&end[..]),
            ))
            .next()
            .is_some())
    }

    /// A referencing row must find its referenced row (unless a foreign key column is NULL).
    fn check_fk_child(&self, fk: FkId, row: &[Value]) -> Result<()> {
        let f = &self.catalog.foreign_keys[fk];
        let vals: Vec<Value> = f
            .parent_lookup
            .from_other
            .iter()
            .map(|&c| row[c].clone())
            .collect();
        if vals.iter().any(Value::is_null) || self.lookup_hit(&f.parent_lookup, &vals)? {
            return Ok(());
        }
        let ct = self.catalog.table(f.table);
        let rt = self.catalog.table(f.ref_table);
        let shown: Vec<Value> = f.columns.iter().map(|&c| row[c].clone()).collect();
        let types: Vec<&ColumnType> = f.columns.iter().map(|&c| &ct.columns[c].ty).collect();
        Err(Error::constraint(
            Code::FailedPrecondition,
            format!(
                "Foreign key `{}` constraint violation on table `{}`. Cannot find referenced key `{}` in table `{}`.",
                f.name,
                ct.name,
                key_debug(&shown, &types),
                rt.name
            ),
        ))
    }

    /// A referenced row that is gone must not be referenced any more.
    fn check_fk_parent(&self, fk: FkId, old: &[Value]) -> Result<()> {
        let f = &self.catalog.foreign_keys[fk];
        let parent_vals: Vec<Value> = f
            .parent_lookup
            .cols
            .iter()
            .map(|&c| old[c].clone())
            .collect();
        if parent_vals.iter().any(Value::is_null)
            || self.lookup_hit(&f.parent_lookup, &parent_vals)?
        {
            return Ok(());
        }
        let child_vals: Vec<Value> = f
            .child_lookup
            .from_other
            .iter()
            .map(|&c| old[c].clone())
            .collect();
        if !self.lookup_hit(&f.child_lookup, &child_vals)? {
            return Ok(());
        }
        let ct = self.catalog.table(f.table);
        let rt = self.catalog.table(f.ref_table);
        let shown: Vec<Value> = f.ref_columns.iter().map(|&c| old[c].clone()).collect();
        let types: Vec<&ColumnType> = f.ref_columns.iter().map(|&c| &rt.columns[c].ty).collect();
        if self.prod_messages {
            return Err(Error::constraint(
                Code::FailedPrecondition,
                format!(
                    "Foreign key constraint violation when deleting or updating referenced row(s): referencing row(s) found in table `{}`.",
                    ct.name
                ),
            ));
        }
        Err(Error::constraint(
            Code::FailedPrecondition,
            format!(
                "Foreign key `{}` constraint violation when deleting or updating referenced key `{}` in table `{}`. A row with that key exists in the referencing table `{}`.",
                f.name,
                key_debug(&shown, &types),
                rt.name,
                ct.name
            ),
        ))
    }

    /// Recomputes every index from the table rows.
    pub fn rebuild_indexes(&mut self) -> Result<()> {
        let mut indexes = Vector::new();
        for ix in 0..self.catalog.indexes.len() {
            indexes.push_back(backfill(self, ix)?);
        }
        self.indexes = indexes;
        Ok(())
    }

    /// Moves the data to a new schema: columns are matched by name, indexes are reused when their
    /// definition is unchanged and backfilled otherwise.
    pub fn with_schema(&self, schema: Schema) -> Result<DbState> {
        let catalog = Catalog::build(&schema)?;
        let backfill_ts = Timestamp::from_micros(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_micros() as i64),
        );
        let mut tables = Vector::new();
        for tm in &catalog.tables {
            let Some(old_id) = self.catalog.table_id(&tm.name) else {
                tables.push_back(RowMap::new());
                continue;
            };
            let old = self.catalog.table(old_id);
            let mapping: Vec<Option<usize>> =
                tm.columns.iter().map(|c| old.column(&c.name)).collect();
            let same_layout = mapping.len() == old.columns.len()
                && mapping.iter().enumerate().all(|(i, m)| *m == Some(i));
            if same_layout && old.pk_desc == tm.pk_desc && old.pk.len() == tm.pk.len() {
                tables.push_back(self.tables[old_id].clone());
                continue;
            }
            if tm.pk.iter().any(|&c| mapping[c].is_none()) {
                return Err(Error::failed_precondition(format!(
                    "Cannot change the primary key of table {}",
                    tm.name
                )));
            }
            let added = |c: usize| mapping[c].is_none();
            let fill = tm.columns.iter().enumerate().any(|(c, cm)| {
                added(c) && (cm.default.is_some() || cm.expr.is_some() || cm.identity)
            });
            let mut map = RowMap::new();
            for (_, row) in self.tables[old_id].iter() {
                let mut new_row: Vec<Value> = mapping
                    .iter()
                    .map(|m| m.map_or(Value::Null, |i| row[i].clone()))
                    .collect();
                if fill {
                    crate::expr::compute_row(tm, &mut new_row, &added, &added)?;
                    // Spanner backfills a new `DEFAULT (PENDING_COMMIT_TIMESTAMP())` column with
                    // the schema change's time (the emulator writes 9999-12-31, a bug).
                    for (c, cm) in tm.columns.iter().enumerate() {
                        if cm.default_commit_ts && added(c) {
                            new_row[c] = Value::Timestamp(backfill_ts);
                        }
                    }
                }
                let new_row: Row = Arc::from(new_row);
                let key = encode_key(
                    &tm.pk
                        .iter()
                        .map(|&c| new_row[c].clone())
                        .collect::<Vec<_>>(),
                    &tm.pk_desc,
                )?;
                map.insert(Arc::from(key), new_row);
            }
            tables.push_back(map);
        }
        let mut state = DbState {
            schema: Arc::new(schema),
            catalog: Arc::new(catalog),
            tables,
            indexes: Vector::new(),
            prod_messages: self.prod_messages,
        };
        for (ix_id, im) in state.catalog.indexes.iter().enumerate() {
            let reusable = self.catalog.index_id_any(&im.name).filter(|&old_ix| {
                let o = self.catalog.index(old_ix);
                let ot = self.catalog.table(o.table);
                let nt = state.catalog.table(im.table);
                ot.name.eq_ignore_ascii_case(&nt.name)
                    && o.unique == im.unique
                    && o.null_filtered == im.null_filtered
                    && names(ot, &o.null_filter) == names(nt, &im.null_filter)
                    && o.key_desc == im.key_desc
                    && names(ot, &o.entry_columns) == names(nt, &im.entry_columns)
                    && names(ot, &o.key) == names(nt, &im.key)
                    && self.tables[o.table].ptr_eq(&state.tables[im.table])
            });
            let entries = match reusable {
                Some(old_ix) => self.indexes[old_ix].clone(),
                None => backfill(&state, ix_id)?,
            };
            state.indexes.push_back(entries);
        }
        Ok(state)
    }
}

/// The primary key of `row` as the emulator prints it in errors.
fn pk_debug(tm: &TableMeta, row: &[Value]) -> String {
    let vals: Vec<Value> = tm.pk.iter().map(|&c| row[c].clone()).collect();
    let types: Vec<&ColumnType> = tm.pk.iter().map(|&c| &tm.columns[c].ty).collect();
    key_debug(&vals, &types)
}

/// Length of a STRING (characters) or BYTES value, or of the longest array element.
fn longest(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => Some(s.chars().count() as u64),
        Value::Bytes(b) => Some(b.len() as u64),
        Value::Array(items) => items.iter().filter_map(longest).max(),
        _ => None,
    }
}

fn names(tm: &TableMeta, cols: &[usize]) -> Vec<String> {
    cols.iter()
        .map(|&c| tm.columns[c].name.to_ascii_lowercase())
        .collect()
}

fn backfill(state: &DbState, ix: IndexId) -> Result<RowMap> {
    let im = state.catalog.index(ix);
    let tm = state.catalog.table(im.table);
    let mut entries = RowMap::new();
    let mut buf = Vec::new();
    for (key, row) in state.tables[im.table].iter() {
        if let Some(prefix_len) = index_key(im, row, key, &mut buf)? {
            if im.unique {
                let prefix = &buf[..prefix_len];
                if let Some((_, other)) = entries
                    .range::<_, [u8]>((
                        std::ops::Bound::Included(prefix),
                        std::ops::Bound::Excluded(&prefix_end(prefix)[..]),
                    ))
                    .next()
                {
                    if state.prod_messages {
                        let other: Vec<Value> = other[..im.key.len()]
                            .iter()
                            .cloned()
                            .chain(entry_pk(im, tm, other))
                            .collect();
                        let this: Vec<Value> = im
                            .key
                            .iter()
                            .chain(&tm.pk)
                            .map(|&c| row[c].clone())
                            .collect();
                        return Err(Error::failed_precondition(format!(
                            "Found uniqueness violation: {} {}.",
                            prod_key(&other),
                            prod_key(&this)
                        )));
                    }
                    let vals: Vec<Value> = im.key.iter().map(|&c| row[c].clone()).collect();
                    let types: Vec<&ColumnType> =
                        im.key.iter().map(|&c| &tm.columns[c].ty).collect();
                    return Err(Error::failed_precondition(format!(
                        "Found uniqueness violation on index {}, duplicate key: {}",
                        im.name,
                        key_debug(&vals, &types)
                    )));
                }
            }
            entries.insert(Arc::from(&buf[..]), index_row(im, row));
        }
    }
    Ok(entries)
}

/// Writes the entry key of `row` into `buf`; returns the length of the index-column prefix, or
/// `None` when the index leaves the row out (a NULL in a `null_filter` column).
fn index_key(
    im: &IndexMeta,
    row: &[Value],
    table_key: &[u8],
    buf: &mut Vec<u8>,
) -> Result<Option<usize>> {
    buf.clear();
    if im.null_filter.iter().any(|&c| row[c].is_null()) {
        return Ok(None);
    }
    for (&c, &desc) in im.key.iter().zip(&im.key_desc) {
        encode_part(buf, &row[c], desc)?;
    }
    let n = buf.len();
    buf.extend_from_slice(table_key);
    Ok(Some(n))
}

/// The base table's primary key values of an index entry row.
fn entry_pk(im: &IndexMeta, tm: &TableMeta, entry: &[Value]) -> Vec<Value> {
    tm.pk
        .iter()
        .map(|c| {
            im.entry_columns
                .iter()
                .position(|e| e == c)
                .map_or(Value::Null, |i| entry[i].clone())
        })
        .collect()
}

/// A key as production Spanner prints it in constraint errors: `[10,1]`.
fn prod_key(values: &[Value]) -> String {
    let parts: Vec<String> = values
        .iter()
        .map(|v| match v {
            Value::Null => "<null>".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Int64(i) => i.to_string(),
            Value::String(s) => s.to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    format!("[{}]", parts.join(","))
}

fn index_row(im: &IndexMeta, row: &[Value]) -> Row {
    im.entry_columns.iter().map(|&c| row[c].clone()).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Insert,
    Update,
    InsertOrUpdate,
    Replace,
}

/// A mutation resolved against a catalog.
#[derive(Debug, Clone)]
pub enum Mutation {
    Write {
        kind: WriteKind,
        table: TableId,
        columns: Vec<usize>,
        rows: Vec<Vec<Value>>,
        /// `(row, column)` cells that hold `spanner.commit_timestamp()`.
        commit_ts_cells: Vec<(usize, usize)>,
    },
    Delete {
        table: TableId,
        keys: KeySet,
    },
}

impl Mutation {
    /// Resolves a write mutation in wire form (column names, rows of `google.protobuf.Value`).
    pub fn write(
        catalog: &Catalog,
        kind: WriteKind,
        table: &str,
        columns: &[String],
        values: &[prost_types::ListValue],
    ) -> Result<Mutation> {
        let t = catalog.lookup_table(table)?;
        let tm = catalog.table(t);
        let mut cols = Vec::with_capacity(columns.len());
        for name in columns {
            let c = tm.column(name).ok_or_else(|| {
                Error::not_found(format!("Column not found in table {}: {name}", tm.name))
            })?;
            if tm.columns[c].expr.is_some() {
                return Err(Error::failed_precondition(format!(
                    "Cannot write into generated column `{}.{}`.",
                    tm.name, tm.columns[c].name
                )));
            }
            if cols.contains(&c) {
                return Err(Error::invalid_argument(format!(
                    "Multiple values for column {}.",
                    tm.columns[c].name
                )));
            }
            cols.push(c);
        }
        let mut rows = Vec::with_capacity(values.len());
        let mut commit_ts_cells = Vec::new();
        for (r, lv) in values.iter().enumerate() {
            if lv.values.len() != cols.len() {
                return Err(Error::invalid_argument(format!(
                    "Mutation column and value size mismatch: Found: {} columns and {} values",
                    cols.len(),
                    lv.values.len()
                )));
            }
            let mut row = Vec::with_capacity(cols.len());
            for (i, (pv, &c)) in lv.values.iter().zip(&cols).enumerate() {
                let cm = &tm.columns[c];
                if let (ColumnType::Timestamp, Some(prost_types::value::Kind::StringValue(s))) =
                    (&cm.ty, &pv.kind)
                    && s == COMMIT_TIMESTAMP_SENTINEL
                {
                    if !cm.allow_commit_timestamp {
                        return Err(Error::constraint(
                            Code::FailedPrecondition,
                            format!(
                                "Cannot write commit timestamp because the allow_commit_timestamp column option is not set to true for column {}.{}, or for all corresponding shared key columns in this table's interleaved table hierarchy.",
                                tm.name, cm.name
                            ),
                        ));
                    }
                    commit_ts_cells.push((r, i));
                    row.push(Value::Null);
                    continue;
                }
                row.push(crate::wire::decode(pv, &cm.ty)?);
            }
            rows.push(row);
        }
        Ok(Mutation::Write {
            kind,
            table: t,
            columns: cols,
            rows,
            commit_ts_cells,
        })
    }

    pub fn delete(catalog: &Catalog, table: &str, keys: KeySet) -> Result<Mutation> {
        Ok(Mutation::Delete {
            table: catalog.lookup_table(table)?,
            keys,
        })
    }

    pub fn table(&self) -> TableId {
        match self {
            Mutation::Write { table, .. } | Mutation::Delete { table, .. } => *table,
        }
    }
}

/// Decodes a wire point key, which must have one part per key column.
pub fn decode_point_key(
    list: &prost_types::ListValue,
    types: &[ColumnType],
    name: &str,
) -> Result<Vec<Value>> {
    if list.values.len() != types.len() {
        let mut e = crate::key::wrong_parts(name, types.len(), list.values.len());
        e.message.push_str(" Supplied key: ");
        e.message.push_str(&crate::wire::list_short_debug(list));
        return Err(e);
    }
    decode_key(list, types)
}

/// Decodes a wire key (`ListValue`) using the key column types. Extra parts are reported by the
/// key-set encoder, so they are decoded as NULL here.
pub fn decode_key(list: &prost_types::ListValue, types: &[ColumnType]) -> Result<Vec<Value>> {
    list.values
        .iter()
        .enumerate()
        .map(|(i, v)| match types.get(i) {
            Some(t) => crate::wire::decode(v, t),
            None => Ok(Value::Null),
        })
        .collect()
}

/// A constraint checked on the state at the end of the commit (or DML statement), so that
/// several mutations can pass through an intermediate state that violates it.
enum Deferred {
    /// A unique index entry was written.
    Unique {
        ix: IndexId,
        key: Key,
        prefix: usize,
    },
    /// A referencing row (by table key) was written.
    FkChild { fk: FkId, table: TableId, key: Key },
    /// A referenced row with these values was deleted or changed.
    FkParent { fk: FkId, old: Row },
}

/// Applies mutations to a private copy of a state, enforcing Spanner's constraints and keeping
/// indexes in sync. Records every key it writes.
pub struct Writer {
    state: DbState,
    commit_ts: Timestamp,
    /// Timestamps later than this in an `allow_commit_timestamp` column are "in the future".
    max_ts: Timestamp,
    written: Vec<(Target, Key)>,
    deferred: Vec<Deferred>,
    buf: Vec<u8>,
    mutations: usize,
    /// `(table, column)`s written with the commit timestamp.
    pending: Vec<(TableId, usize)>,
}

impl Writer {
    pub fn new(base: &DbState, commit_ts: Timestamp) -> Writer {
        Writer {
            state: base.clone(),
            commit_ts,
            max_ts: commit_ts,
            written: Vec::new(),
            deferred: Vec::new(),
            buf: Vec::with_capacity(64),
            mutations: 0,
            pending: Vec::new(),
        }
    }

    /// The `(table, column)`s written with the commit timestamp so far: explicitly, or by a
    /// `DEFAULT` or `ON UPDATE (PENDING_COMMIT_TIMESTAMP())`. DML staged before commit holds a
    /// placeholder there, which the transaction must not read back.
    pub fn pending_commit_ts(&self) -> &[(TableId, usize)] {
        &self.pending
    }

    fn mark_pending(&mut self, t: TableId, c: usize) {
        if !self.pending.contains(&(t, c)) {
            self.pending.push((t, c));
        }
    }

    /// Writes the commit timestamp into the columns of `row` the write leaves out whose
    /// `DEFAULT` (insert) or `ON UPDATE` (update) is `PENDING_COMMIT_TIMESTAMP()`.
    fn fill_commit_ts(
        &mut self,
        tm: &TableMeta,
        t: TableId,
        columns: &[usize],
        row: &mut [Value],
        update: bool,
    ) {
        for (c, cm) in tm.columns.iter().enumerate() {
            let fills = if update {
                cm.on_update_commit_ts
            } else {
                cm.default_commit_ts
            };
            if fills && !columns.contains(&c) {
                row[c] = Value::Timestamp(self.commit_ts);
                self.mark_pending(t, c);
            }
        }
    }

    /// Checks commit-timestamp columns against `max_ts` instead of the commit timestamp: DML
    /// staged before commit doesn't know its commit timestamp yet, only that it will be later
    /// than every `CURRENT_TIMESTAMP()` handed out so far.
    pub fn with_max_ts(mut self, max_ts: Timestamp) -> Writer {
        self.max_ts = max_ts;
        self
    }

    pub fn state(&self) -> &DbState {
        &self.state
    }

    /// Number of cell and row mutations applied, as `CommitStats.mutation_count` counts them.
    pub fn mutation_count(&self) -> usize {
        self.mutations
    }

    /// Checks the deferred constraints (unique indexes, foreign keys) on the final state and
    /// returns it with the keys written.
    pub fn finish(self) -> Result<(DbState, Vec<(Target, Key)>)> {
        self.validate()?;
        Ok((self.state, self.written))
    }

    fn validate(&self) -> Result<()> {
        let st = &self.state;
        for d in &self.deferred {
            match d {
                Deferred::Unique { ix, key, prefix } => {
                    let entries = &st.indexes[*ix];
                    let Some(row) = entries.get(&**key) else {
                        continue;
                    };
                    let p = &key[..*prefix];
                    let end = prefix_end(p);
                    let n = entries
                        .range::<_, [u8]>((
                            std::ops::Bound::Included(p),
                            std::ops::Bound::Excluded(&end[..]),
                        ))
                        .take(2)
                        .count();
                    if n > 1 {
                        let im = st.catalog.index(*ix);
                        let tm = st.catalog.table(im.table);
                        if st.prod_messages {
                            let written_here = |k: &[u8]| {
                                self.deferred.iter().any(|d| {
                                    matches!(d, Deferred::Unique { ix: i, key: dk, .. }
                                        if i == ix && **dk == *k)
                                })
                            };
                            // A clash with a row committed earlier names that row.
                            let committed = entries
                                .range::<_, [u8]>((
                                    std::ops::Bound::Included(p),
                                    std::ops::Bound::Excluded(&end[..]),
                                ))
                                .find(|(k, _)| !written_here(k));
                            if let Some((_, other)) = committed {
                                let pk = entry_pk(im, tm, other);
                                let index_key: Vec<Value> = other[..im.key.len()]
                                    .iter()
                                    .cloned()
                                    .chain(pk.iter().cloned())
                                    .collect();
                                return Err(Error::constraint(
                                    Code::AlreadyExists,
                                    format!(
                                        "Unique index violation on index {} at index key {}. It conflicts with row {} in table {}.",
                                        im.name,
                                        prod_key(&index_key),
                                        prod_key(&pk),
                                        tm.name
                                    ),
                                ));
                            }
                            // Otherwise the duplicate is the last entry with this key written in
                            // the transaction.
                            let dup = self
                                .deferred
                                .iter()
                                .rev()
                                .find_map(|d| match d {
                                    Deferred::Unique { ix: i, key: k, .. }
                                        if i == ix && k.starts_with(p) =>
                                    {
                                        entries.get(&**k)
                                    }
                                    _ => None,
                                })
                                .unwrap_or(row);
                            return Err(Error::constraint(
                                Code::AlreadyExists,
                                format!(
                                    "UNIQUE violation on index {}. Row {} in table {} duplicates key {} created in this transaction.",
                                    im.name,
                                    prod_key(&entry_pk(im, tm, dup)),
                                    tm.name,
                                    prod_key(&dup[..im.key.len()])
                                ),
                            ));
                        }
                        let vals: Vec<Value> = row[..im.key.len()].to_vec();
                        let types: Vec<&ColumnType> =
                            im.key.iter().map(|&c| &tm.columns[c].ty).collect();
                        return Err(Error::constraint(
                            Code::AlreadyExists,
                            format!(
                                "UNIQUE violation on index {},  duplicate key: {} in this transaction.",
                                im.name,
                                key_debug(&vals, &types)
                            ),
                        ));
                    }
                }
                Deferred::FkChild { fk, table, key } => {
                    if let Some(row) = st.tables[*table].get(&**key) {
                        st.check_fk_child(*fk, row)?;
                    }
                }
                Deferred::FkParent { fk, old } => st.check_fk_parent(*fk, old)?,
            }
        }
        Ok(())
    }

    pub fn apply(&mut self, m: &Mutation) -> Result<()> {
        match m {
            Mutation::Write {
                kind,
                table,
                columns,
                rows,
                commit_ts_cells,
            } => {
                let catalog = self.state.catalog.clone();
                let tm = catalog.table(*table);
                if tm.computed {
                    crate::expr::check_generated_key_inputs(tm, *kind, columns)?;
                }
                let key_pos: Vec<Option<usize>> = tm
                    .pk
                    .iter()
                    .map(|k| columns.iter().position(|c| c == k))
                    .collect();
                let mut ts_cells = commit_ts_cells.iter().peekable();
                for (r, row) in rows.iter().enumerate() {
                    let mut row: std::borrow::Cow<'_, [Value]> = std::borrow::Cow::Borrowed(row);
                    while let Some(&&(cr, cc)) = ts_cells.peek() {
                        if cr != r {
                            break;
                        }
                        row.to_mut()[cc] = Value::Timestamp(self.commit_ts);
                        self.mark_pending(*table, columns[cc]);
                        ts_cells.next();
                    }
                    self.write_row(tm, *table, *kind, columns, &key_pos, &row)?;
                }
                Ok(())
            }
            Mutation::Delete { table, keys } => {
                let spans = self.state.spans(Target::Table(*table), keys)?;
                let mut doomed: Vec<Key> = Vec::new();
                for (k, _) in scan(&self.state.tables[*table], &spans) {
                    doomed.push(k.clone());
                }
                self.mutations += 1;
                for k in doomed {
                    self.delete_row(*table, &k)?;
                }
                Ok(())
            }
        }
    }

    fn write_row(
        &mut self,
        tm: &TableMeta,
        t: TableId,
        kind: WriteKind,
        columns: &[usize],
        key_pos: &[Option<usize>],
        values: &[Value],
    ) -> Result<()> {
        for (v, &c) in values.iter().zip(columns) {
            self.check_value(tm, c, v)?;
        }
        self.mutations += columns.len();
        let mut computed_row = None;
        if tm.computed && key_pos.iter().any(Option::is_none) {
            // A key column the write leaves out (DEFAULT, identity or generated) gets its value
            // before the row is keyed. The whole new row is computed here, once, so a volatile
            // default such as GENERATE_UUID() is evaluated once per row and the key matches it.
            let mut row = vec![Value::Null; tm.columns.len()];
            for (v, &c) in values.iter().zip(columns) {
                row[c] = v.clone();
            }
            crate::expr::compute_row(tm, &mut row, &|c| !columns.contains(&c), &|_| true)?;
            self.fill_commit_ts(tm, t, columns, &mut row, false);
            computed_row = Some(row);
        }
        self.buf.clear();
        for ((kp, &desc), &c) in key_pos.iter().zip(&tm.pk_desc).zip(&tm.pk) {
            let v = match (kp, &computed_row) {
                (Some(p), _) => &values[*p],
                (None, Some(row)) => &row[c],
                (None, None) => &Value::Null,
            };
            encode_part(&mut self.buf, v, desc)?;
        }
        let key: Key = Arc::from(&self.buf[..]);
        let existing = self.state.tables[t].get(&key).cloned();
        let kind = match (kind, &existing) {
            (WriteKind::InsertOrUpdate, Some(_)) => WriteKind::Update,
            (WriteKind::InsertOrUpdate, None) => WriteKind::Insert,
            (k, _) => k,
        };
        match kind {
            WriteKind::Update => {
                let Some(old) = existing else {
                    return Err(Error::constraint(
                        Code::NotFound,
                        format!(
                            "Table {}: Row {} not found.",
                            tm.name,
                            row_key_debug(tm, key_pos, values)
                        ),
                    ));
                };
                let mut new_row = old.to_vec();
                for (v, &c) in values.iter().zip(columns) {
                    new_row[c] = v.clone();
                }
                self.fill_commit_ts(tm, t, columns, &mut new_row, true);
                if tm.computed {
                    let regenerate = crate::expr::expr_evaluator().is_some()
                        || tm
                            .generated_order
                            .iter()
                            .any(|&g| tm.inputs[g].iter().any(|c| columns.contains(c)));
                    crate::expr::compute_row(tm, &mut new_row, &|_| false, &|_| regenerate)?;
                    crate::expr::check_row(tm, &new_row, &|| pk_debug(tm, &new_row))?;
                }
                self.put_row(t, key, Some(old), Arc::from(new_row))
            }
            WriteKind::Insert | WriteKind::Replace => {
                if let Some(_old) = existing {
                    if kind == WriteKind::Insert {
                        return Err(Error::constraint(
                            Code::AlreadyExists,
                            format!(
                                "Table {}: Row {} already exists.",
                                tm.name,
                                row_key_debug(tm, key_pos, values)
                            ),
                        ));
                    }
                    self.delete_row(t, &key)?;
                }
                let precomputed = computed_row.is_some();
                let mut new_row = computed_row.unwrap_or_else(|| {
                    let mut row = vec![Value::Null; tm.columns.len()];
                    for (v, &c) in values.iter().zip(columns) {
                        row[c] = v.clone();
                    }
                    row
                });
                for (c, cm) in tm.columns.iter().enumerate() {
                    if cm.not_null && !cm.generated && cm.default.is_none() && !columns.contains(&c)
                    {
                        return Err(Error::constraint(
                            Code::FailedPrecondition,
                            format!(
                                "A new row in table {} does not specify a non-null value for NOT NULL column: {}",
                                tm.name, cm.name
                            ),
                        ));
                    }
                }
                if tm.computed {
                    let missing = |c: usize| !columns.contains(&c);
                    if !precomputed {
                        crate::expr::compute_row(tm, &mut new_row, &missing, &|_| true)?;
                        self.fill_commit_ts(tm, t, columns, &mut new_row, false);
                    }
                    for (c, cm) in tm.columns.iter().enumerate() {
                        if missing(c) && !cm.identity {
                            self.check_value(tm, c, &new_row[c])?;
                        }
                    }
                    crate::expr::check_row(tm, &new_row, &|| pk_debug(tm, &new_row))?;
                }
                self.check_parent(tm, &new_row)?;
                self.put_row(t, key, None, Arc::from(new_row))
            }
            WriteKind::InsertOrUpdate => Err(Error::internal("unresolved insert_or_update")),
        }
    }

    fn check_value(&self, tm: &TableMeta, c: usize, v: &Value) -> Result<()> {
        let cm = &tm.columns[c];
        match v {
            Value::Null if cm.not_null => Err(Error::constraint(
                Code::FailedPrecondition,
                format!(
                    "Cannot specify a null value for column: {}.{} in table: {}",
                    tm.name, cm.name, tm.name
                ),
            )),
            Value::String(s) => match cm.max_len {
                Some(limit) if s.len() as u64 > limit && s.chars().count() as u64 > limit => {
                    Err(too_long(tm, cm.name.as_str(), s.chars().count(), limit))
                }
                _ => Ok(()),
            },
            Value::Bytes(b) => match cm.max_len {
                Some(limit) if b.len() as u64 > limit => {
                    Err(too_long(tm, cm.name.as_str(), b.len(), limit))
                }
                _ => Ok(()),
            },
            Value::Timestamp(ts) if cm.allow_commit_timestamp && *ts > self.max_ts => {
                Err(Error::constraint(
                    Code::FailedPrecondition,
                    format!(
                        "Cannot write timestamps in the future, found: {}",
                        ts.format_offset()
                    ),
                ))
            }
            _ => Ok(()),
        }
    }

    fn check_parent(&mut self, tm: &TableMeta, row: &[Value]) -> Result<()> {
        let Some(parent) = tm.parent else {
            return Ok(());
        };
        let pm = self.state.catalog.table(parent);
        let n = pm.pk.len();
        let mut buf = Vec::with_capacity(32);
        for (&c, &desc) in tm.pk[..n].iter().zip(&tm.pk_desc[..n]) {
            encode_part(&mut buf, &row[c], desc)?;
        }
        if self.state.tables[parent].get(&buf[..]).is_some() {
            return Ok(());
        }
        let vals: Vec<Value> = tm.pk[..n].iter().map(|&c| row[c].clone()).collect();
        let types: Vec<&ColumnType> = tm.pk[..n].iter().map(|&c| &tm.columns[c].ty).collect();
        Err(Error::constraint(
            Code::NotFound,
            format!(
                "Insert failed because key was not found in parent table:  Parent Table: {}  Child Table: {}  Key: {}",
                pm.name,
                tm.name,
                key_debug(&vals, &types)
            ),
        ))
    }

    fn put_row(&mut self, t: TableId, key: Key, old: Option<Row>, new: Row) -> Result<()> {
        let catalog = self.state.catalog.clone();
        let tm = catalog.table(t);
        let mut new_key = Vec::with_capacity(64);
        for &ix in &tm.indexes {
            let im = catalog.index(ix);
            let old_entry = match &old {
                Some(o) => index_key(im, o, &key, &mut self.buf)?.map(|_| Key::from(&self.buf[..])),
                None => None,
            };
            let new_prefix = index_key(im, &new, &key, &mut new_key)?;
            let new_row = new_prefix.map(|_| index_row(im, &new));
            if let (Some(ok), Some(_)) = (&old_entry, new_prefix)
                && **ok == new_key[..]
            {
                let unchanged = self.state.indexes[ix]
                    .get(&**ok)
                    .zip(new_row.as_ref())
                    .is_some_and(|(a, b)| a == b);
                if !unchanged && let Some(r) = new_row {
                    self.state.indexes[ix].insert(ok.clone(), r);
                    self.written.push((Target::Index(ix), ok.clone()));
                }
                continue;
            }
            if let Some(ok) = old_entry {
                self.state.indexes[ix].remove(&*ok);
                self.written.push((Target::Index(ix), ok));
            }
            if let (Some(plen), Some(r)) = (new_prefix, new_row) {
                let k: Key = Arc::from(&new_key[..]);
                if im.unique {
                    self.deferred.push(Deferred::Unique {
                        ix,
                        key: k.clone(),
                        prefix: plen,
                    });
                }
                self.state.indexes[ix].insert(k.clone(), r);
                self.written.push((Target::Index(ix), k));
            }
        }
        let changed = |cols: &[usize]| match &old {
            Some(o) => cols.iter().any(|&c| o[c] != new[c]),
            None => true,
        };
        for &fk in &tm.fks_out {
            if changed(&catalog.foreign_keys[fk].columns) {
                self.deferred.push(Deferred::FkChild {
                    fk,
                    table: t,
                    key: key.clone(),
                });
            }
        }
        if let Some(o) = &old {
            for &fk in &tm.fks_in {
                if changed(&catalog.foreign_keys[fk].ref_columns) {
                    self.deferred
                        .push(Deferred::FkParent { fk, old: o.clone() });
                }
            }
        }
        self.state.tables[t].insert(key.clone(), new);
        self.written.push((Target::Table(t), key));
        Ok(())
    }

    /// Handles the foreign keys that reference a deleted row: ON DELETE CASCADE deletes the
    /// referencing rows now; NO ACTION is checked at the end.
    fn delete_referencing(&mut self, tm: &TableMeta, old: &Row) -> Result<()> {
        let catalog = self.state.catalog.clone();
        for &fk in &tm.fks_in {
            let f = &catalog.foreign_keys[fk];
            if f.on_delete == OnDelete::NoAction {
                self.deferred.push(Deferred::FkParent {
                    fk,
                    old: old.clone(),
                });
                continue;
            }
            let vals: Vec<Value> = f
                .child_lookup
                .from_other
                .iter()
                .map(|&c| old[c].clone())
                .collect();
            if vals.iter().any(Value::is_null) {
                continue;
            }
            let parent_vals: Vec<Value> = f
                .parent_lookup
                .cols
                .iter()
                .map(|&c| old[c].clone())
                .collect();
            if self.state.lookup_hit(&f.parent_lookup, &parent_vals)? {
                continue;
            }
            let prefix = encode_key(&vals, &f.child_lookup.desc)?;
            let end = prefix_end(&prefix);
            let bounds = (
                std::ops::Bound::Included(&prefix[..]),
                std::ops::Bound::Excluded(&end[..]),
            );
            let ct = catalog.table(f.table);
            let doomed: Vec<Key> = match f.child_lookup.target {
                Target::Table(t) => self.state.tables[t]
                    .range::<_, [u8]>(bounds)
                    .map(|(k, _)| k.clone())
                    .collect(),
                Target::Index(ix) => {
                    let im = catalog.index(ix);
                    let mut out = Vec::new();
                    for (_, entry) in self.state.indexes[ix].range::<_, [u8]>(bounds) {
                        let pk: Vec<Value> = ct
                            .pk
                            .iter()
                            .map(|c| {
                                im.entry_columns
                                    .iter()
                                    .position(|e| e == c)
                                    .map_or(Value::Null, |p| entry[p].clone())
                            })
                            .collect();
                        out.push(Arc::from(encode_key(&pk, &ct.pk_desc)?));
                    }
                    out
                }
            };
            for k in doomed {
                self.delete_row(f.table, &k)?;
            }
        }
        Ok(())
    }

    fn delete_row(&mut self, t: TableId, key: &Key) -> Result<()> {
        let Some(old) = self.state.tables[t].remove(&**key) else {
            return Ok(());
        };
        self.written.push((Target::Table(t), key.clone()));
        let catalog = self.state.catalog.clone();
        let tm = catalog.table(t);
        for &ix in &tm.indexes {
            let im = catalog.index(ix);
            if index_key(im, &old, key, &mut self.buf)?.is_some() {
                let k: Key = Arc::from(&self.buf[..]);
                self.state.indexes[ix].remove(&*k);
                self.written.push((Target::Index(ix), k));
            }
        }
        if !tm.fks_in.is_empty() {
            self.delete_referencing(tm, &old)?;
        }
        let end = prefix_end(key);
        for child in &tm.children {
            let bounds = (
                std::ops::Bound::Included(&key[..]),
                std::ops::Bound::Excluded(&end[..]),
            );
            let keys: Vec<Key> = self.state.tables[child.table]
                .range::<_, [u8]>(bounds)
                .map(|(k, _)| k.clone())
                .collect();
            if keys.is_empty() {
                continue;
            }
            if child.on_delete == OnDelete::NoAction {
                let ct = catalog.table(child.table);
                let vals: Vec<Value> = tm.pk.iter().map(|&c| old[c].clone()).collect();
                let types: Vec<&ColumnType> = tm.pk.iter().map(|&c| &tm.columns[c].ty).collect();
                return Err(Error::constraint(
                    Code::FailedPrecondition,
                    format!(
                        "Delete failed because one or more keys exist within the child table:  Parent Table: {}  Child Table: {}  Key: {}",
                        tm.name,
                        ct.name,
                        key_debug(&vals, &types)
                    ),
                ));
            }
            for ck in keys {
                self.delete_row(child.table, &ck)?;
            }
        }
        Ok(())
    }
}

fn too_long(tm: &TableMeta, col: &str, size: usize, limit: u64) -> Error {
    Error::constraint(
        Code::FailedPrecondition,
        format!(
            "New value exceeds the maximum size limit for this column: {}.{col}, size: {size}, limit: {limit}.",
            tm.name
        ),
    )
}

fn row_key_debug(tm: &TableMeta, key_pos: &[Option<usize>], values: &[Value]) -> String {
    let vals: Vec<Value> = key_pos
        .iter()
        .map(|p| p.map_or(Value::Null, |p| values[p].clone()))
        .collect();
    let types: Vec<&ColumnType> = tm.pk.iter().map(|&c| &tm.columns[c].ty).collect();
    key_debug(&vals, &types)
}
