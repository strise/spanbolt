//! Schema validation and mutation for parsed DDL statements.

use crate::DdlError;
use crate::lexer::{Tok, tokenize};
use crate::names;
use crate::parser::{ChangeStream, CsTarget, GrantStmt, parse_grant};
use crate::schema::*;

type R<T> = Result<T, DdlError>;

fn key(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn dup(name: &str) -> DdlError {
    DdlError::AlreadyExists(format!("Duplicate name in schema: {name}."))
}

fn table_not_found(name: &str) -> DdlError {
    DdlError::NotFound(format!("Table not found: {name}"))
}

/// A kind of named schema object, for the "name not valid" errors.
#[derive(Clone, Copy)]
enum Obj {
    Table,
    Synonym,
    Index,
    SearchIndex,
    ForeignKey,
    Check,
    View,
    Sequence,
    ChangeStream,
    Schema,
    Model,
    PropertyGraph,
}

impl Obj {
    /// How production (`compat == false`) or the emulator names the kind in errors.
    fn label(self, compat: bool) -> &'static str {
        match (self, compat) {
            (Obj::Table, true) => "Table",
            (Obj::Table | Obj::Synonym, false) => "table",
            (Obj::Synonym, true) => "Synonym",
            (Obj::Index | Obj::SearchIndex, true) => "Index",
            (Obj::Index, false) => "index",
            (Obj::SearchIndex, false) => "search index",
            (Obj::ForeignKey, true) => "Foreign Key",
            (Obj::ForeignKey, false) => "foreign key",
            (Obj::Check, true) => "",
            (Obj::Check, false) => "check constraint",
            (Obj::View, _) => "VIEW",
            (Obj::Sequence, _) => "Sequence",
            (Obj::ChangeStream, true) => "Change Stream",
            (Obj::ChangeStream, false) => "change stream",
            (Obj::Schema, true) => "Schema",
            (Obj::Schema, false) => "schema",
            (Obj::Model, _) => "Model",
            (Obj::PropertyGraph, _) => "Property Graph",
        }
    }
}

/// `name` as production prints it in errors: bytes outside printable ASCII as octal escapes.
fn c_escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_graphic() || b == b' ' {
            out.push(b as char);
        } else {
            out.push_str(&format!("\\{b:03o}"));
        }
    }
    out
}

fn name_not_valid(label: &str, name: &str) -> DdlError {
    let lead = if label.is_empty() {
        String::new()
    } else {
        format!("{label} ")
    };
    inv(format!("{lead}name not valid: {name}."))
}

/// Rejects a name production Spanner doesn't allow for a `kind` object. The emulator doesn't
/// validate view names.
fn check_name(kind: Obj, name: &str, compat: bool) -> R<()> {
    if names::valid_name(name) || (compat && matches!(kind, Obj::View)) {
        Ok(())
    } else if compat {
        Err(name_not_valid(kind.label(true), name))
    } else {
        Err(name_not_valid(kind.label(false), &c_escape(name)))
    }
}

/// Production allows column names other objects may have (a letter first, then letters, digits
/// and underscores) and names them with their table; the emulator only rejects a leading
/// underscore and overlong names.
fn check_column_name(table: &str, name: &str, compat: bool) -> R<()> {
    if compat && !names::valid_column_name(name) {
        Err(inv(format!("Column name not valid: {name}.")))
    } else if !compat && !names::valid_name(name) {
        Err(inv(format!(
            "Column name not valid: {table}.{}.",
            c_escape(name)
        )))
    } else {
        Ok(())
    }
}

/// A foreign key or check constraint name: a valid name without a prefix Spanner reserves.
fn check_constraint_name(kind: Obj, name: &str, compat: bool) -> R<()> {
    check_name(kind, name, compat)?;
    let part = name.rsplit_once('.').map_or(name, |(_, n)| n);
    for prefix in ["PK_", "CK_IS_NOT_NULL_"] {
        if part
            .get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
        {
            return Err(inv(format!(
                "Invalid {} name: {name}. Prefix {prefix} cannot be used.",
                kind.label(true)
            )));
        }
    }
    Ok(())
}

/// A column named like an existing one ignoring case. Production words a clash that differs only
/// in case differently from an exact one.
fn dup_column(table: &str, old: &str, new: &str, compat: bool) -> DdlError {
    pre(if !compat && old != new {
        format!("Column names in table {table} differ only in case: {old}, {new}.")
    } else {
        format!("Duplicate column name {table}.{new}.")
    })
}

fn pre(msg: String) -> DdlError {
    DdlError::Precondition(msg)
}

fn inv(msg: String) -> DdlError {
    DdlError::Syntax(msg)
}

/// Base type name as the emulator prints it in errors (`STRING`, `ARRAY`, ...).
fn base_type(ty: &ColumnType) -> &'static str {
    match ty {
        ColumnType::Bool => "BOOL",
        ColumnType::Int64 => "INT64",
        ColumnType::Float32 => "FLOAT32",
        ColumnType::Float64 => "FLOAT64",
        ColumnType::Numeric => "NUMERIC",
        ColumnType::String(_) => "STRING",
        ColumnType::Bytes(_) => "BYTES",
        ColumnType::Date => "DATE",
        ColumnType::Timestamp => "TIMESTAMP",
        ColumnType::Json => "JSON",
        ColumnType::Interval => "INTERVAL",
        ColumnType::Uuid => "UUID",
        ColumnType::TokenList => "TOKENLIST",
        ColumnType::Array(_) => "ARRAY",
        ColumnType::Struct(_) => "STRUCT",
        ColumnType::Proto(_) => "PROTO",
    }
}

fn same_base(a: &ColumnType, b: &ColumnType) -> bool {
    match (a, b) {
        (ColumnType::Array(x), ColumnType::Array(y)) => same_base(x, y),
        _ => base_type(a) == base_type(b),
    }
}

fn not_keyable(ty: &ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Array(_) | ColumnType::Json | ColumnType::TokenList | ColumnType::Struct(_)
    )
}

const MAX_STRING_LEN: u64 = 2_621_440;
const MAX_BYTES_LEN: u64 = 10_485_760;

/// Production's schema limits (the emulator's `limits.h`).
const MAX_KEY_COLUMNS: usize = 16;
const MAX_COLUMNS_PER_TABLE: usize = 1024;
const MAX_INDEXES_PER_TABLE: usize = 128;
const MAX_TABLES_PER_DATABASE: usize = 5000;
const MAX_INDEXES_PER_DATABASE: usize = 2 * MAX_TABLES_PER_DATABASE;
const MAX_NON_KEY_COLUMNS_SIZE: u64 = 2000 << 20;

/// The size a column counts towards the non-key column limit: its declared maximum (4 bytes per
/// STRING character), 10 MiB for JSON, TOKENLIST and arrays, nothing for fixed-width types.
fn declared_size(ty: &ColumnType) -> u64 {
    match ty {
        ColumnType::Array(_) | ColumnType::Json | ColumnType::TokenList => MAX_BYTES_LEN,
        ColumnType::String(n) => 4 * n.unwrap_or(MAX_STRING_LEN),
        ColumnType::Bytes(n) => n.unwrap_or(MAX_BYTES_LEN),
        ColumnType::Uuid => 16,
        _ => 0,
    }
}

/// Total declared size of a table's non-key columns.
fn non_key_size(t: &Table) -> u64 {
    t.columns
        .iter()
        .filter(|c| {
            !t.primary_key
                .iter()
                .any(|k| k.column.eq_ignore_ascii_case(&c.name))
        })
        .map(|c| declared_size(&c.ty))
        .sum()
}

fn too_many_columns(t: &Table) -> R<()> {
    if t.columns.len() > MAX_COLUMNS_PER_TABLE {
        return Err(pre(format!(
            "Table {} has too many columns; the limit is {MAX_COLUMNS_PER_TABLE}.",
            t.name
        )));
    }
    Ok(())
}

fn validate_column(table: &str, c: &Column, compat: bool) -> R<()> {
    fn check_len(table: &str, c: &Column, ty: &ColumnType) -> R<()> {
        let (n, max) = match ty {
            ColumnType::String(Some(n)) => (*n, MAX_STRING_LEN),
            ColumnType::Bytes(Some(n)) => (*n, MAX_BYTES_LEN),
            ColumnType::Array(inner) => return check_len(table, c, inner),
            _ => return Ok(()),
        };
        if n == 0 || n > max {
            return Err(inv(format!(
                "Bad length for column {table}.{}: {n} : Allowed length range: [1, {max}].",
                c.name
            )));
        }
        Ok(())
    }
    if let ColumnType::Array(inner) = &c.ty
        && matches!(**inner, ColumnType::Array(_))
    {
        return Err(inv(
            "Array of arrays type is not supported by the schema.".into()
        ));
    }
    check_len(table, c, &c.ty)?;
    if matches!(c.generated, Some(Generated::Identity { .. })) && c.ty != ColumnType::Int64 {
        return Err(inv(format!(
            "The type of an identity column {} is invalid. Currently, only INT64 is supported.",
            c.name
        )));
    }
    fn unsupported(ty: &ColumnType) -> Option<&'static str> {
        match ty {
            ColumnType::Struct(_) => Some("STRUCT"),
            ColumnType::Interval => Some("INTERVAL"),
            ColumnType::Array(t) => unsupported(t),
            _ => None,
        }
    }
    if let Some(ty) = unsupported(&c.ty) {
        return Err(inv(format!(
            "Column {table}.{} has type {ty}, which is not supported as a column type.",
            c.name
        )));
    }
    if c.allow_commit_timestamp && c.ty != ColumnType::Timestamp {
        return Err(pre(format!(
            "Column {table}.{} has invalid allow_commit_timestamp option.  Option only allowed on TIMESTAMP columns.",
            c.name
        )));
    }
    if c.vector_length.is_some()
        && !matches!(&c.ty, ColumnType::Array(t) if matches!(**t, ColumnType::Float32 | ColumnType::Float64))
    {
        return Err(inv(format!(
            "`vector_length` can only be applied on ARRAY<FLOAT32>or ARRAY<FLOAT64>, but it is applied on {table}.{}.",
            c.name
        )));
    }
    if let Some(u) = &c.on_update
        && !is_pending_commit_timestamp(u)
    {
        return Err(on_update_not_pct(table, &c.name, u, compat));
    }
    if c.default
        .as_deref()
        .is_some_and(is_pending_commit_timestamp)
        && !c.allow_commit_timestamp
    {
        return Err(pre(format!(
            "Cannot use default value expression PENDING_COMMIT_TIMESTAMP() on column `{}` without the allow_commit_timestamp column option.",
            c.name
        )));
    }
    if c.on_update.is_some()
        && !c
            .default
            .as_deref()
            .is_some_and(is_pending_commit_timestamp)
    {
        return Err(if compat {
            inv(format!(
                "The ON UPDATE expression of column `{}` does not match the DEFAULT value expression.",
                c.name
            ))
        } else {
            default_not_pct(&c.name)
        });
    }
    if c.default.is_some() && c.generated.is_some() {
        return Err(inv(format!(
            "Cannot use DEFAULT with generated column `{}`.",
            c.name
        )));
    }
    check_virtual_not_null(table, c)?;
    if c.ty == ColumnType::TokenList && !c.hidden {
        return Err(inv(format!(
            "TOKENLIST column {table}.{} must be HIDDEN.",
            c.name
        )));
    }
    Ok(())
}

fn on_update_not_pct(table: &str, column: &str, expr: &str, compat: bool) -> DdlError {
    inv(if compat {
        "PENDING_COMMIT_TIMESTAMP() is the only supported ON UPDATE expression.".into()
    } else {
        format!(
            "Error parsing ON UPDATE expression '{expr}' from column '{column}' in table '{table}': PENDING_COMMIT_TIMESTAMP() and PENDING_COMMIT_TIMESTAMP_INT64() are the only supported ON UPDATE expressions."
        )
    })
}

fn default_not_pct(column: &str) -> DdlError {
    pre(format!(
        "The only default value expression permitted for commit timestamp column `{column}` is PENDING_COMMIT_TIMESTAMP()."
    ))
}

fn on_update_needs_default(column: &str) -> DdlError {
    inv(format!(
        "Column `{column}` with an ON UPDATE expression must have an identical DEFAULT value expression."
    ))
}

/// Only STORED generated columns can be NOT NULL (emulator 1.5.51 and production).
fn check_virtual_not_null(table: &str, c: &Column) -> R<()> {
    if c.not_null && matches!(c.generated, Some(Generated::Expr { stored: false, .. })) {
        return Err(inv(format!(
            "Generated column `{table}.{}` that are not marked as STORED cannot be specified as NOT NULL.",
            c.name
        )));
    }
    Ok(())
}

/// Rejects generated columns that read each other in a cycle.
fn check_generated_cycles(t: &Table) -> R<()> {
    let reads = |c: &Column| -> Vec<usize> {
        let Some((e, _)) = c.generated.as_ref().and_then(Generated::expr) else {
            return Vec::new();
        };
        let ids = crate::lexer::column_references(e).unwrap_or_default();
        t.columns
            .iter()
            .enumerate()
            .filter(|(_, o)| o.generated.as_ref().and_then(Generated::expr).is_some())
            .filter(|(_, o)| ids.iter().any(|w| w.eq_ignore_ascii_case(&o.name)))
            .map(|(i, _)| i)
            .collect()
    };
    let edges: Vec<Vec<usize>> = t.columns.iter().map(reads).collect();
    // 0 = unvisited, 1 = on the current path, 2 = done.
    fn visit(
        c: usize,
        edges: &[Vec<usize>],
        mark: &mut [u8],
        path: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        match mark[c] {
            2 => return None,
            1 => {
                let at = path.iter().position(|&p| p == c).unwrap_or(0);
                return Some(path[at..].to_vec());
            }
            _ => {}
        }
        mark[c] = 1;
        path.push(c);
        for &d in &edges[c] {
            if let Some(cycle) = visit(d, edges, mark, path) {
                return Some(cycle);
            }
        }
        path.pop();
        mark[c] = 2;
        None
    }
    let mut mark = vec![0u8; t.columns.len()];
    for c in 0..t.columns.len() {
        if let Some(cycle) = visit(c, &edges, &mut mark, &mut Vec::new()) {
            let names: Vec<&str> = cycle.iter().map(|&i| t.columns[i].name.as_str()).collect();
            return Err(pre(format!(
                "Cycle detected while analysing generated column, which include objects ({})",
                names.join(",")
            )));
        }
    }
    Ok(())
}

/// Production finds the policy's column by its exact spelling, the emulator ignoring case.
fn rdp_check(t: &Table, r: &RowDeletionPolicy, compat: bool) -> R<()> {
    let column = if compat {
        t.column(&r.column)
    } else {
        t.column_exact(&r.column)
    };
    match column {
        None => Err(inv(format!(
            "Cannot create ROW DELETION POLICY because there is no column named {} in table {}.",
            r.column, t.name
        ))),
        Some(c) if c.ty != ColumnType::Timestamp => Err(inv(format!(
            "Cannot create ROW DELETION POLICY because column {} in table {} is not of type TIMESTAMP.",
            r.column, t.name
        ))),
        _ => Ok(()),
    }
}

/// Lists names as `` `a`, `b` ``.
fn backquoted(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Rows-dependent work the storage layer must do before (or instead of) accepting a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataCheck {
    /// The table must have no rows, otherwise fail with `message` (FAILED_PRECONDITION).
    TableEmpty { table: String, message: String },
    /// The column must contain no NULLs.
    NoNulls { table: String, column: String },
    /// STRING/BYTES values (or array elements) in the column must fit the new length `limit`.
    MaxLength {
        table: String,
        column: String,
        limit: u64,
    },
    /// Existing rows must not violate the new unique index.
    UniqueIndex { index: String },
    /// Existing rows must satisfy the new foreign key on `table`.
    ForeignKey { table: String, name: String },
    /// Existing rows must satisfy the new check constraint on `table`.
    Check { table: String, name: String },
}

impl Schema {
    fn table_key(&self, name: &str) -> Option<String> {
        let k = key(name);
        if self.tables.contains_key(&k) {
            return Some(k);
        }
        self.tables
            .iter()
            .find(|(_, t)| {
                t.synonym
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case(name))
            })
            .map(|(k, _)| k.clone())
    }

    /// Resolves a table by name or synonym.
    pub fn resolve_table(&self, name: &str) -> Option<&Table> {
        self.table_key(name).and_then(|k| self.tables.get(&k))
    }

    /// The key of the table (or synonym) spelled exactly `name`. Spanner detects duplicate names
    /// ignoring case, but DDL must refer to an object by its declared spelling.
    fn table_key_exact(&self, name: &str) -> Option<String> {
        let k = self.table_key(name)?;
        let t = &self.tables[&k];
        (t.name == name || t.synonym.as_deref() == Some(name)).then_some(k)
    }

    /// The table (or synonym) spelled exactly `name`.
    fn table_exact(&self, name: &str) -> Option<&Table> {
        self.table_key_exact(name).and_then(|k| self.tables.get(&k))
    }

    fn need_table(&self, name: &str) -> R<String> {
        self.table_key_exact(name)
            .ok_or_else(|| table_not_found(name))
    }

    fn opaque_find(&self, kind: &str, name: &str) -> Option<usize> {
        self.opaque
            .iter()
            .position(|o| o.kind == kind && o.name.eq_ignore_ascii_case(name))
    }

    /// The declared spelling of the schema object whose name equals `name` ignoring case, among
    /// the objects sharing Spanner's name space.
    fn taken_spelling(&self, name: &str) -> Option<&str> {
        let k = key(name);
        let direct = [
            self.tables.get(&k).map(|t| t.name.as_str()),
            self.indexes.get(&k).map(|i| i.name.as_str()),
            self.search_indexes.get(&k).map(|i| i.name.as_str()),
            self.views.get(&k).map(|v| v.name.as_str()),
            self.sequences.get(&k).map(|s| s.name.as_str()),
        ];
        if let Some(n) = direct.into_iter().flatten().next() {
            return Some(n);
        }
        let same = |n: &&str| n.eq_ignore_ascii_case(name);
        self.tables
            .values()
            .flat_map(|t| {
                t.synonym
                    .as_deref()
                    .into_iter()
                    .chain(t.foreign_keys.iter().map(|f| f.name.as_str()))
                    .chain(t.checks.iter().map(|c| c.name.as_str()))
            })
            .find(same)
            .or_else(|| {
                self.opaque
                    .iter()
                    .filter(|o| {
                        matches!(
                            o.kind.as_str(),
                            "CHANGE STREAM" | "MODEL" | "PROPERTY GRAPH"
                        )
                    })
                    .map(|o| o.name.as_str())
                    .find(same)
            })
            .or_else(|| {
                self.locality_group(name)
                    .map(|i| self.locality_groups[i].name.as_str())
            })
    }

    fn locality_group(&self, name: &str) -> Option<usize> {
        self.locality_groups
            .iter()
            .position(|g| g.name.eq_ignore_ascii_case(name))
    }

    /// The locality groups a statement's tables, columns and indexes name must exist.
    fn check_locality_refs(&self, stmt: &Statement, compat: bool) -> R<()> {
        let cols = |cs: &[Column]| -> Vec<String> {
            cs.iter().filter_map(|c| c.locality_group.clone()).collect()
        };
        let refs: Vec<String> = match stmt {
            Statement::CreateTable { table, .. } => {
                let mut v = cols(&table.columns);
                v.extend(table.locality_group.clone());
                v
            }
            Statement::CreateIndex { index, .. } => index.locality_group.iter().cloned().collect(),
            Statement::AlterTable { action, .. } => match action {
                AlterTable::AddColumn { column, .. } | AlterTable::AlterColumn { column } => {
                    cols(std::slice::from_ref(column))
                }
                AlterTable::SetColumnOptions {
                    locality_group: Some(Some(g)),
                    ..
                }
                | AlterTable::SetOptions {
                    locality_group: Some(g),
                } => vec![g.clone()],
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        for g in refs {
            if !g.eq_ignore_ascii_case(DEFAULT_LOCALITY_GROUP) && self.locality_group(&g).is_none()
            {
                // Production answers NOT_FOUND without a message.
                let msg = if compat {
                    format!("Locality group not found: {g}")
                } else {
                    String::new()
                };
                return Err(DdlError::NotFound(msg));
            }
        }
        Ok(())
    }

    /// Production only: spilling to HDD needs SSD storage first.
    fn check_locality_storage(g: &LocalityGroup) -> R<()> {
        let opt = |k: &str| {
            g.options
                .iter()
                .find(|(n, v)| n.eq_ignore_ascii_case(k) && v != "NULL")
                .map(|(_, v)| v.as_str())
        };
        if opt("storage") == Some("'hdd'") && opt("ssd_to_hdd_spill_timespan").is_some() {
            return Err(pre(format!(
                "Locality Group {} has invalid storage option: hdd. If the ssd_to_hdd_spill_timespan is set, then the storage option is required to be \"ssd\".",
                g.name
            )));
        }
        Ok(())
    }

    fn locality_group_referenced(&self, name: &str) -> bool {
        let is = |g: &Option<String>| g.as_deref().is_some_and(|g| g.eq_ignore_ascii_case(name));
        self.tables
            .values()
            .any(|t| is(&t.locality_group) || t.columns.iter().any(|c| is(&c.locality_group)))
            || self.indexes.values().any(|i| is(&i.locality_group))
    }

    fn create_locality_group(
        &mut self,
        group: &LocalityGroup,
        if_not_exists: bool,
        compat: bool,
    ) -> R<()> {
        let name = &group.name;
        if self.locality_group(name).is_some() {
            // Production refuses even with IF NOT EXISTS.
            return match (compat, if_not_exists) {
                (true, true) => Ok(()),
                (true, false) => Err(dup(name)),
                (false, _) => Err(inv(format!(
                    "Could not create locality group because it already exists: {name}"
                ))),
            };
        }
        self.check_new_name(name, compat)?;
        let mut group = group.clone();
        if !compat {
            group.options.retain(|(_, v)| v != "NULL");
            Self::check_locality_storage(&group)?;
        }
        self.locality_groups.push(group);
        Ok(())
    }

    fn alter_locality_group(
        &mut self,
        name: &str,
        options: &[(String, String)],
        compat: bool,
    ) -> R<()> {
        let i = match self.locality_group(name) {
            Some(i) => i,
            // `default` exists without being created.
            None if name.eq_ignore_ascii_case(DEFAULT_LOCALITY_GROUP) => {
                self.locality_groups.push(LocalityGroup {
                    name: DEFAULT_LOCALITY_GROUP.to_string(),
                    options: Vec::new(),
                });
                self.locality_groups.len() - 1
            }
            None if compat => {
                return Err(DdlError::NotFound(format!(
                    "Locality group not found: {name}"
                )));
            }
            None => return Err(DdlError::NotFound(String::new())),
        };
        let mut group = self.locality_groups[i].clone();
        if compat {
            // The emulator replaces the options rather than merging them.
            group.options = options.to_vec();
        } else {
            merge_options(&mut group.options, options);
            Self::check_locality_storage(&group)?;
        }
        self.locality_groups[i] = group;
        Ok(())
    }

    fn drop_locality_group(&mut self, name: &str, if_exists: bool, compat: bool) -> R<()> {
        let Some(i) = self.locality_group(name) else {
            // Production refuses even with IF EXISTS.
            return match (compat, if_exists) {
                (true, true) => Ok(()),
                (true, false) => Err(DdlError::NotFound(format!(
                    "Locality group not found: {name}"
                ))),
                (false, _) => Err(inv(format!(
                    "Cannot drop locality group {name} - it does not exist."
                ))),
            };
        };
        if self.locality_group_referenced(name) {
            return Err(if compat {
                pre(format!(
                    "Locality group {name} cannot be dropped as it is referenced."
                ))
            } else {
                inv(format!(
                    "Cannot drop locality group {name} - it is still referenced."
                ))
            });
        }
        self.locality_groups.remove(i);
        Ok(())
    }

    /// True if `name` is used by any schema object sharing Spanner's name space.
    fn name_taken(&self, name: &str) -> bool {
        self.taken_spelling(name).is_some()
    }

    /// The duplicate-name error for `name`. Production words a clash that differs only in case
    /// differently from an exact one.
    fn dup(&self, name: &str, compat: bool) -> DdlError {
        match self.taken_spelling(name) {
            Some(old) if !compat && old != name => DdlError::AlreadyExists(format!(
                "Schema object names differ only in case: {old}, {name}."
            )),
            _ => dup(name),
        }
    }

    fn check_new_name(&self, name: &str, compat: bool) -> R<()> {
        if self.name_taken(name) {
            return Err(self.dup(name, compat));
        }
        if let Some((schema, _)) = name.rsplit_once('.')
            && !self.schemas.contains_key(&key(schema))
        {
            return Err(DdlError::NotFound(format!("Schema not found: {schema}")));
        }
        Ok(())
    }

    fn push_order(&mut self, kind: ObjectKind, k: String) {
        self.creation_order.push((kind, k));
    }

    fn drop_order(&mut self, kind: ObjectKind, k: &str) {
        self.creation_order
            .retain(|(ok, n)| !(*ok == kind && n == k));
    }

    /// Foreign keys in other tables (or this one) referencing `table`.
    fn fks_referencing<'a>(
        &'a self,
        table: &'a str,
    ) -> impl Iterator<Item = (&'a Table, &'a ForeignKey)> + 'a {
        self.tables.values().flat_map(move |t| {
            t.foreign_keys
                .iter()
                .filter(move |f| f.ref_table.eq_ignore_ascii_case(table))
                .map(move |f| (t, f))
        })
    }

    /// Validates a foreign key declared on `table` (which may be a table being created).
    fn validate_fk(&self, table: &Table, fk: &ForeignKey, compat: bool) -> R<()> {
        let referenced = if fk.ref_table == table.name {
            table
        } else {
            self.table_exact(&fk.ref_table)
                .ok_or_else(|| table_not_found(&fk.ref_table))?
        };
        for (t, cols) in [(table, &fk.columns), (referenced, &fk.ref_columns)] {
            if let Some(c) = cols.iter().find(|c| t.column_exact(c).is_none()) {
                let msg = format!(
                    "Column `{c}` not found for table `{}` in foreign key `{}`.",
                    t.name, fk.name
                );
                return Err(if compat { pre(msg) } else { inv(msg) });
            }
        }
        for (t, cols) in [(table, &fk.columns), (referenced, &fk.ref_columns)] {
            for c in cols.iter().filter_map(|c| t.column(c)) {
                if not_keyable(&c.ty) {
                    return Err(pre(format!(
                        "Column `{}` for foreign key `{}` on table `{}` has an unsupported type.",
                        c.name, fk.name, t.name
                    )));
                }
            }
        }
        if fk.columns.len() != fk.ref_columns.len() {
            return Err(pre(format!(
                "The number of columns are different for table `{}` and table `{}` in foreign key `{}`.",
                table.name, referenced.name, fk.name
            )));
        }
        for (a, b) in fk.columns.iter().zip(&fk.ref_columns) {
            let (ca, cb) = (table.column(a), referenced.column(b));
            if let (Some(ca), Some(cb)) = (ca, cb)
                && (!same_base(&ca.ty, &cb.ty) || not_keyable(&ca.ty))
            {
                return Err(pre(format!(
                    "The column types are different for column `{}` of table `{}` and column `{}` of table `{}` in foreign key `{}`.",
                    ca.name, table.name, cb.name, referenced.name, fk.name
                )));
            }
        }
        if fk.enforced
            && fk.on_delete == OnDelete::NoAction
            && referenced.row_deletion_policy.is_some()
            && !std::ptr::eq(referenced, table)
        {
            return Err(inv(format!(
                "Cannot add a row deletion policy to table `{}`. It is referenced by one or more foreign keys: `{}`.",
                referenced.name, fk.name
            )));
        }
        Ok(())
    }

    /// The emulator's name for an unnamed constraint: `FK_<table>_<referenced table>` (or
    /// `CK_<table>` for a check) with a fingerprint and the first sequence number whose name is
    /// free in the schema and in `taken` (names already given out by the same statement).
    fn generated_name(&self, table: &Table, fk: Option<&ForeignKey>, taken: &[String]) -> String {
        let free =
            |n: &str| !self.name_taken(n) && !taken.iter().any(|t| t.eq_ignore_ascii_case(n));
        match fk {
            Some(fk) => {
                let referenced = if fk.ref_table.eq_ignore_ascii_case(&table.name) {
                    table.name.as_str()
                } else {
                    self.resolve_table(&fk.ref_table)
                        .map_or(fk.ref_table.as_str(), |r| r.name.as_str())
                };
                names::foreign_key_name(&table.name, referenced, |n| !free(n))
            }
            None => names::check_name(&table.name, |n| !free(n)),
        }
    }

    fn validate_interleave(&self, t: &Table) -> R<()> {
        let Some(il) = &t.interleave else {
            return Ok(());
        };
        let parent = self
            .table_exact(&il.parent)
            .ok_or_else(|| table_not_found(&il.parent))?;
        for (i, pk) in parent.primary_key.iter().enumerate() {
            let pos = t.primary_key.iter().position(|c| c.column == pk.column);
            let child_part = match pos {
                Some(j) if j == i => &t.primary_key[j],
                Some(j) => {
                    return Err(pre(format!(
                        "Table {} references parent key column {} at incorrect position {j}.",
                        t.name, pk.column
                    )));
                }
                None => {
                    return Err(pre(format!(
                        "Table {} does not reference parent key column {}.",
                        t.name, pk.column
                    )));
                }
            };
            let (Some(pc), Some(cc)) = (parent.column(&pk.column), t.column(&child_part.column))
            else {
                continue;
            };
            if !same_base(&pc.ty, &cc.ty) {
                return Err(pre(format!(
                    "Table {} references parent key column {} with incorrect type {} (should be {}).",
                    t.name,
                    pk.column,
                    base_type(&cc.ty),
                    base_type(&pc.ty)
                )));
            }
            if pk.order != child_part.order {
                let o = if pk.order == SortOrder::Desc {
                    "DESC"
                } else {
                    "ASC"
                };
                return Err(pre(format!(
                    "Table {} references parent key column {} with incorrect order {o}.",
                    t.name, pk.column
                )));
            }
            if pc.not_null && !cc.not_null {
                return Err(pre(format!(
                    "Table {} references parent key column {} that is not null, but child key is nullable.",
                    t.name, pk.column
                )));
            }
            if !pc.not_null && cc.not_null {
                return Err(pre(format!(
                    "Table {} references parent key column {} that is nullable, but child key is not null.",
                    t.name, pk.column
                )));
            }
        }
        Ok(())
    }

    fn create_table(&mut self, table: &Table, if_not_exists: bool, compat: bool) -> R<()> {
        if if_not_exists && self.table_key(&table.name).is_some() {
            return Ok(());
        }
        self.check_new_name(&table.name, compat)?;
        check_name(Obj::Table, &table.name, compat)?;
        if let Some(s) = &table.synonym {
            check_name(Obj::Synonym, s, compat)?;
        }
        let mut t = table.clone();
        if !compat {
            // Production only: the emulator keys rows by a virtual generated column.
            for k in &t.primary_key {
                if let Some(c) = t.column(&k.column)
                    && matches!(c.generated, Some(Generated::Expr { stored: false, .. }))
                {
                    return Err(inv(format!(
                        "Generated column `{}.{}` must be STORED to be part of the primary key.",
                        t.name, c.name
                    )));
                }
            }
        }
        for (i, c) in t.columns.iter().enumerate() {
            if let Some(o) = t.columns[..i]
                .iter()
                .find(|o| o.name.eq_ignore_ascii_case(&c.name))
            {
                return Err(dup_column(&t.name, &o.name, &c.name, compat));
            }
            check_column_name(&t.name, &c.name, compat)?;
            validate_column(&t.name, c, compat)?;
        }
        check_generated_cycles(&t)?;
        for (i, k) in t.primary_key.iter().enumerate() {
            if t.primary_key[..i]
                .iter()
                .any(|o| o.column.eq_ignore_ascii_case(&k.column))
            {
                return Err(inv(format!(
                    "Table {} references key column {} more than once.",
                    t.name, k.column
                )));
            }
            match t.column_exact(&k.column) {
                None => {
                    // Production Spanner starts this one in lowercase.
                    return Err(pre(format!(
                        "{} {} references nonexistent key column {}.",
                        if compat { "Table" } else { "table" },
                        t.name,
                        k.column
                    )));
                }
                Some(c) if not_keyable(&c.ty) => {
                    return Err(inv(format!(
                        "Column {}.{} has type {}, but is part of the primary key.",
                        t.name,
                        c.name,
                        base_type(&c.ty)
                    )));
                }
                Some(c) if c.on_update.is_some() => {
                    return Err(inv(format!(
                        "Column {}.{} with ON UPDATE cannot be a part of or the whole primary key.",
                        t.name, c.name
                    )));
                }
                _ => {}
            }
        }
        too_many_columns(&t)?;
        if t.primary_key.len() > MAX_KEY_COLUMNS {
            return Err(inv(format!(
                "Table {} has too many keys ({}); the limit is {MAX_KEY_COLUMNS}.",
                t.name,
                t.primary_key.len()
            )));
        }
        let size = non_key_size(&t);
        if size > MAX_NON_KEY_COLUMNS_SIZE {
            return Err(pre(format!(
                "Total size of non-key columns in Table {} {size} exceeds the limit of {MAX_NON_KEY_COLUMNS_SIZE}.",
                t.name
            )));
        }
        if self.tables.len() >= MAX_TABLES_PER_DATABASE {
            return Err(pre(format!(
                "Cannot add Table {} : too many tables (limit {MAX_TABLES_PER_DATABASE} per database).",
                t.name
            )));
        }
        self.validate_interleave(&t)?;
        let mut names: Vec<String> = Vec::new();
        for i in 0..t.foreign_keys.len() {
            if t.foreign_keys[i].generated_name {
                t.foreign_keys[i].name = self.generated_name(&t, Some(&t.foreign_keys[i]), &names);
            }
            let fk = &t.foreign_keys[i];
            if !fk.generated_name {
                check_constraint_name(Obj::ForeignKey, &fk.name, compat)?;
            }
            if (!fk.generated_name && self.name_taken(&fk.name))
                || names.iter().any(|n| n.eq_ignore_ascii_case(&fk.name))
                || fk.name.eq_ignore_ascii_case(&t.name)
            {
                return Err(self.dup(&fk.name, compat));
            }
            self.validate_fk(&t, fk, compat)?;
            names.push(fk.name.clone());
        }
        for i in 0..t.checks.len() {
            if t.checks[i].generated_name {
                t.checks[i].name = self.generated_name(&t, None, &names);
            }
            let c = &t.checks[i];
            check_name_in_default_schema(&c.name)?;
            if !c.generated_name {
                check_constraint_name(Obj::Check, &c.name, compat)?;
            }
            if self.name_taken(&c.name)
                || names.iter().any(|n| n.eq_ignore_ascii_case(&c.name))
                || c.name.eq_ignore_ascii_case(&t.name)
            {
                return Err(self.dup(&c.name, compat));
            }
            names.push(c.name.clone());
        }
        if let Some(r) = &t.row_deletion_policy {
            rdp_check(&t, r, compat)?;
        }
        if let Some(s) = &t.synonym
            && (self.name_taken(s) || s.eq_ignore_ascii_case(&t.name))
        {
            return Err(self.dup(s, compat));
        }
        let k = key(&t.name);
        self.tables.insert(k.clone(), t);
        self.push_order(ObjectKind::Table, k);
        Ok(())
    }

    fn drop_table(&mut self, name: &str, if_exists: bool) -> R<()> {
        let Some(k) = self.table_key_exact(name) else {
            return if if_exists {
                Ok(())
            } else {
                Err(table_not_found(name))
            };
        };
        let t = &self.tables[&k];
        let children: Vec<&str> = self
            .tables
            .values()
            .filter(|c| {
                c.interleave
                    .as_ref()
                    .is_some_and(|i| i.parent.eq_ignore_ascii_case(&t.name))
            })
            .map(|c| c.name.as_str())
            .collect();
        if !children.is_empty() {
            return Err(pre(format!(
                "Cannot drop table {} with interleaved tables: {}.",
                t.name,
                children.join(", ")
            )));
        }
        let mut idx: Vec<&str> = self
            .indexes_on(&t.name)
            .map(|i| i.name.as_str())
            .chain(
                self.search_indexes
                    .values()
                    .filter(|i| i.table.eq_ignore_ascii_case(&t.name))
                    .map(|i| i.name.as_str()),
            )
            .collect();
        idx.sort();
        if !idx.is_empty() {
            return Err(pre(format!(
                "Cannot drop table {} with indices: {}.",
                t.name,
                idx.join(", ")
            )));
        }
        let fks: Vec<&str> = self
            .fks_referencing(&t.name)
            .filter(|(owner, _)| !owner.name.eq_ignore_ascii_case(&t.name))
            .map(|(_, f)| f.name.as_str())
            .collect();
        if !fks.is_empty() {
            return Err(pre(format!(
                "Cannot drop table `{}`. It is referenced by one or more foreign keys: {}. You must drop the foreign keys before dropping the table.",
                t.name,
                backquoted(&fks)
            )));
        }
        let streams = self.streams_tracking(&t.name, None);
        if !streams.is_empty() {
            return Err(pre(format!(
                "Cannot drop table {0}. Table {0} is tracked by {1}. Tables explicitly tracked by a Change Stream cannot be dropped. Please drop the Change Stream or modify its FOR clause to stop tracking the table explicitly before dropping the table.",
                t.name,
                count_streams(&streams)
            )));
        }
        let views: Vec<&str> = self
            .views
            .values()
            .filter(|v| sql_mentions(&v.sql, &t.name))
            .map(|v| v.name.as_str())
            .collect();
        if !views.is_empty() {
            return Err(pre(format!(
                "Cannot drop TABLE `{}` on which there are dependent views: {}.",
                t.name,
                views.join(", ")
            )));
        }
        let name = t.name.clone();
        self.tables.remove(&k);
        self.drop_order(ObjectKind::Table, &k);
        self.grants
            .retain(|g| !matches!(&g.object, GrantObject::Table { table, .. } if *table == name));
        Ok(())
    }

    fn alter_table(&mut self, table: &str, action: &AlterTable, compat: bool) -> R<()> {
        let k = self.need_table(table)?;
        let t = &self.tables[&k];
        let tname = t.name.clone();
        let col_not_found =
            |c: &str| DdlError::NotFound(format!("Column not found in table {tname}: {c}"));
        match action {
            AlterTable::AddColumn {
                column,
                if_not_exists,
            } => {
                if let Some(old) = t.column(&column.name) {
                    if *if_not_exists {
                        return Ok(());
                    }
                    return Err(dup_column(&tname, &old.name, &column.name, compat));
                }
                check_column_name(&tname, &column.name, compat)?;
                validate_column(&tname, column, compat)?;
                if column.generated.is_some() {
                    let mut next = t.clone();
                    next.columns.push(column.clone());
                    check_generated_cycles(&next)?;
                }
                let mut next = t.clone();
                next.columns.push(column.clone());
                if !compat && next.columns.len() > MAX_COLUMNS_PER_TABLE {
                    return Err(pre(format!(
                        "Cannot add column {} to table {tname} because it would have too many columns; the limit is {MAX_COLUMNS_PER_TABLE}.",
                        column.name
                    )));
                }
                too_many_columns(&next)?;
                let size = non_key_size(&next);
                if size > MAX_NON_KEY_COLUMNS_SIZE {
                    return Err(pre(format!(
                        "Cannot add column {}. Total size of non-key columns in table {tname} {size} exceeds the limit of {MAX_NON_KEY_COLUMNS_SIZE}.",
                        column.name
                    )));
                }
                self.table_mut(&k)?.columns.push(column.clone());
            }
            AlterTable::DropColumn { name } => {
                let Some(c) = t.column_exact(name) else {
                    return Err(col_not_found(name));
                };
                let cname = c.name.clone();
                if t.primary_key
                    .iter()
                    .any(|p| p.column.eq_ignore_ascii_case(name))
                {
                    return Err(inv(format!(
                        "Cannot drop key column {cname} from table {tname}."
                    )));
                }
                let rdp_on = |n: &str| {
                    t.row_deletion_policy
                        .as_ref()
                        .is_some_and(|r| r.column.eq_ignore_ascii_case(n))
                };
                if rdp_on(name) {
                    return Err(rdp_break(&cname, &tname));
                }
                let streams = self.streams_tracking(&tname, Some(name));
                if !streams.is_empty() {
                    // The emulator names the column with its table and puts a space before the dot.
                    let (gap, column) = if compat {
                        (" ", format!("{tname}.{cname}"))
                    } else {
                        ("", cname.clone())
                    };
                    return Err(pre(format!(
                        "Cannot drop column {tname}.{cname}{gap}. Column {column} is tracked by {}. Columns explicitly tracked by a Change Stream cannot be dropped. Please alter the Change Stream to stop tracking the column before dropping the column.",
                        count_streams(&streams)
                    )));
                }
                let mut users: Vec<&str> = self
                    .indexes_on(&tname)
                    .filter(|i| {
                        i.key.iter().any(|p| p.column.eq_ignore_ascii_case(name))
                            || i.storing.iter().any(|s| s.eq_ignore_ascii_case(name))
                    })
                    .map(|i| i.name.as_str())
                    .chain(
                        self.search_indexes
                            .values()
                            .filter(|i| {
                                i.table.eq_ignore_ascii_case(&tname)
                                    && i.columns
                                        .iter()
                                        .chain(&i.storing)
                                        .chain(&i.partition_by)
                                        .chain(i.order_by.iter().map(|o| &o.column))
                                        .any(|s| s.eq_ignore_ascii_case(name))
                            })
                            .map(|i| i.name.as_str()),
                    )
                    .collect();
                users.sort();
                if let Some(i) = users.first() {
                    return Err(pre(format!(
                        "Cannot drop column {cname} from table {tname} because it is used by index {i}."
                    )));
                }
                let fks: Vec<&str> = t
                    .foreign_keys
                    .iter()
                    .filter(|f| f.columns.iter().any(|c| c.eq_ignore_ascii_case(name)))
                    .map(|f| f.name.as_str())
                    .chain(
                        self.fks_referencing(&tname)
                            .filter(|(_, f)| {
                                f.ref_columns.iter().any(|c| c.eq_ignore_ascii_case(name))
                            })
                            .map(|(_, f)| f.name.as_str()),
                    )
                    .collect();
                if !fks.is_empty() {
                    return Err(pre(format!(
                        "Cannot drop column `{cname}` from table `{tname}`. It is used by one or more foreign keys: {}. You must drop the foreign keys before dropping the column.",
                        backquoted(&fks)
                    )));
                }
                self.table_mut(&k)?
                    .columns
                    .retain(|c| !c.name.eq_ignore_ascii_case(name));
                self.grants.retain(|g| {
                    !matches!(&g.object, GrantObject::Table { table, column: Some(c), .. }
                        if *table == tname && *c == cname)
                });
            }
            AlterTable::AlterColumn { column } => {
                let Some(old) = t.column_exact(&column.name) else {
                    return Err(col_not_found(&column.name));
                };
                if column.ty != ColumnType::Timestamp
                    && t.row_deletion_policy
                        .as_ref()
                        .is_some_and(|r| r.column.eq_ignore_ascii_case(&column.name))
                {
                    return Err(rdp_break(&old.name, &tname));
                }
                if !type_change_ok(&old.ty, &column.ty) {
                    return Err(inv(format!(
                        "Cannot change type of column `{}` from `{}` to `{}`",
                        old.name,
                        base_type(&old.ty),
                        base_type(&column.ty)
                    )));
                }
                if t.primary_key
                    .iter()
                    .any(|p| p.column.eq_ignore_ascii_case(&column.name))
                {
                    // Spanner allows type/length changes on a key column that keeps its
                    // nullability, unless interleaved children share the key.
                    if old.not_null != column.not_null {
                        return Err(inv(format!(
                            "Cannot change key column {tname}.{}.",
                            old.name
                        )));
                    }
                    let has_children = self.tables.values().any(|c| {
                        c.interleave
                            .as_ref()
                            .is_some_and(|i| i.parent.eq_ignore_ascii_case(&tname))
                    });
                    if has_children {
                        return Err(inv(format!(
                            "Requested change to key column {tname}.{} could not be made.",
                            old.name
                        )));
                    }
                }
                // A key column of an index keeps its nullability, unique or not; STORING
                // columns and NULL_FILTERED indexes don't count (emulator and production agree).
                if old.not_null != column.not_null
                    && let Some(ix) = self.indexes.values().find(|ix| {
                        !ix.null_filtered
                            && ix.table.eq_ignore_ascii_case(&tname)
                            && ix
                                .key
                                .iter()
                                .any(|p| p.column.eq_ignore_ascii_case(&column.name))
                    })
                {
                    return Err(pre(format!(
                        "Changing NOT NULL constraints on column {} is not allowed because it affects index {}.",
                        old.name, ix.name
                    )));
                }
                let mut new = column.clone();
                new.name = old.name.clone();
                if !new.allow_commit_timestamp && new.ty == ColumnType::Timestamp {
                    new.allow_commit_timestamp = old.allow_commit_timestamp;
                }
                validate_column(&tname, &new, compat)?;
                let is_key = t
                    .primary_key
                    .iter()
                    .any(|p| p.column.eq_ignore_ascii_case(&new.name));
                if !is_key && declared_size(&new.ty) > declared_size(&old.ty) {
                    let size = non_key_size(t) - declared_size(&old.ty) + declared_size(&new.ty);
                    if size > MAX_NON_KEY_COLUMNS_SIZE {
                        return Err(pre(format!(
                            "Cannot increase length of column {}. Total size of non-key columns in table {tname} is {size} which exceeds the limit of {MAX_NON_KEY_COLUMNS_SIZE}.",
                            new.name
                        )));
                    }
                }
                if new.generated.is_none() && old.generated.is_some() {
                    new.generated = old.generated.clone();
                }
                check_virtual_not_null(&tname, &new)?;
                if new.generated.is_some() {
                    let mut next = t.clone();
                    if let Some(c) = next
                        .columns
                        .iter_mut()
                        .find(|c| c.name.eq_ignore_ascii_case(&new.name))
                    {
                        *c = new.clone();
                    }
                    check_generated_cycles(&next)?;
                }
                let t = self.table_mut(&k)?;
                if let Some(c) = t
                    .columns
                    .iter_mut()
                    .find(|c| c.name.eq_ignore_ascii_case(&new.name))
                {
                    *c = new;
                }
            }
            AlterTable::SetColumnDefault { name, default } => {
                let Some(c) = t.column_exact(name) else {
                    return Err(col_not_found(name));
                };
                if c.on_update.is_some() {
                    match default {
                        None => return Err(on_update_needs_default(&c.name)),
                        Some(d) if !is_pending_commit_timestamp(d) => {
                            return Err(default_not_pct(&c.name));
                        }
                        Some(_) => {}
                    }
                }
                let t = self.table_mut(&k)?;
                if let Some(c) = t
                    .columns
                    .iter_mut()
                    .find(|c| c.name.eq_ignore_ascii_case(name))
                {
                    c.default = default.clone();
                }
            }
            AlterTable::SetColumnOnUpdate { name, on_update } => {
                let Some(c) = t.column(name) else {
                    return Err(col_not_found(name));
                };
                if let Some(u) = on_update {
                    if !is_pending_commit_timestamp(u) {
                        return Err(on_update_not_pct(&tname, &c.name, u, compat));
                    }
                    if !compat && c.ty != ColumnType::Timestamp {
                        // Cloud Spanner type-checks the expression first; its message escapes
                        // the quotes.
                        return Err(inv(format!(
                            "Error parsing ON UPDATE expression \\'{u}\\' from column \\'{}\\' in table \\'{tname}\\': Expected type {}; found TIMESTAMP [at 1:1]",
                            c.name,
                            base_type(&c.ty)
                        )));
                    }
                    if !c
                        .default
                        .as_deref()
                        .is_some_and(is_pending_commit_timestamp)
                    {
                        return Err(on_update_needs_default(&c.name));
                    }
                    if t.primary_key
                        .iter()
                        .any(|k| k.column.eq_ignore_ascii_case(&c.name))
                    {
                        return Err(inv(format!(
                            "Column {tname}.{} with ON UPDATE cannot be a part of or the whole primary key.",
                            c.name
                        )));
                    }
                }
                let t = self.table_mut(&k)?;
                if let Some(c) = t
                    .columns
                    .iter_mut()
                    .find(|c| c.name.eq_ignore_ascii_case(name))
                {
                    c.on_update = on_update.clone();
                }
            }
            AlterTable::SetColumnOptions {
                name,
                allow_commit_timestamp,
                locality_group,
            } => {
                // The emulator finds the column ignoring case; production by its spelling.
                let c = if compat {
                    t.column(name)
                } else {
                    t.column_exact(name)
                };
                let Some(c) = c else {
                    return Err(col_not_found(name));
                };
                let fks: Vec<&str> = t
                    .foreign_keys
                    .iter()
                    .filter(|f| f.columns.iter().any(|c| c.eq_ignore_ascii_case(name)))
                    .map(|f| f.name.as_str())
                    .chain(
                        self.fks_referencing(&tname)
                            .filter(|(_, f)| {
                                f.ref_columns.iter().any(|c| c.eq_ignore_ascii_case(name))
                            })
                            .map(|(_, f)| f.name.as_str()),
                    )
                    .collect();
                if allow_commit_timestamp.is_some() && !fks.is_empty() {
                    return Err(pre(format!(
                        "Cannot set the commit_timestamp option for column `{}` of table `{tname}`. It is used by one or more foreign keys: {}.",
                        c.name,
                        backquoted(&fks)
                    )));
                }
                if *allow_commit_timestamp == Some(true) && c.ty != ColumnType::Timestamp {
                    return Err(pre(format!(
                        "Column {tname}.{} has invalid allow_commit_timestamp option.  Option only allowed on TIMESTAMP columns.",
                        c.name
                    )));
                }
                let t = self.table_mut(&k)?;
                if let Some(c) = t
                    .columns
                    .iter_mut()
                    .find(|c| c.name.eq_ignore_ascii_case(name))
                {
                    if let Some(v) = allow_commit_timestamp {
                        c.allow_commit_timestamp = *v;
                    }
                    if let Some(g) = locality_group {
                        c.locality_group = g.clone();
                    }
                }
            }
            AlterTable::SetOptions { locality_group } => {
                self.table_mut(&k)?.locality_group = locality_group.clone();
            }
            AlterTable::AddForeignKey(fk) => {
                let mut fk = fk.clone();
                if fk.generated_name {
                    fk.name = self.generated_name(t, Some(&fk), &[]);
                } else if self.name_taken(&fk.name) {
                    return Err(self.dup(&fk.name, compat));
                } else {
                    check_constraint_name(Obj::ForeignKey, &fk.name, compat)?;
                }
                self.validate_fk(t, &fk, compat)?;
                self.table_mut(&k)?.foreign_keys.push(fk);
            }
            AlterTable::AddCheck(c) => {
                let mut c = c.clone();
                if c.generated_name {
                    c.name = self.generated_name(t, None, &[]);
                } else if self.name_taken(&c.name) {
                    return Err(self.dup(&c.name, compat));
                }
                check_name_in_default_schema(&c.name)?;
                if !c.generated_name {
                    check_constraint_name(Obj::Check, &c.name, compat)?;
                }
                self.table_mut(&k)?.checks.push(c);
            }
            AlterTable::DropConstraint { name } => {
                let same = |n: &str| {
                    if compat {
                        n.eq_ignore_ascii_case(name)
                    } else {
                        n == name
                    }
                };
                let has = t.foreign_keys.iter().any(|f| same(&f.name))
                    || t.checks.iter().any(|c| same(&c.name));
                if !has {
                    return Err(DdlError::NotFound(format!(
                        "{name} is not a constraint in {tname}"
                    )));
                }
                let t = self.table_mut(&k)?;
                t.foreign_keys
                    .retain(|f| !f.name.eq_ignore_ascii_case(name));
                t.checks.retain(|c| !c.name.eq_ignore_ascii_case(name));
            }
            AlterTable::SetOnDelete(o) => {
                let Some(il) = &t.interleave else {
                    return Err(inv(format!(
                        "Cannot SET ON DELETE on table {tname} that does not have an INTERLEAVE clause."
                    )));
                };
                if !il.in_parent {
                    return Err(inv(format!(
                        "Cannot SET ON DELETE on an INTERLEAVE IN table {tname}."
                    )));
                }
                if let Some(il) = self.table_mut(&k)?.interleave.as_mut() {
                    il.on_delete = o.clone();
                }
            }
            AlterTable::AddRowDeletionPolicy(r) => {
                if let Some(old) = &t.row_deletion_policy {
                    return Err(inv(format!(
                        "Cannot create ROW DELETION POLICY because there is already one on column named {} in table {tname}.",
                        old.column
                    )));
                }
                rdp_check(t, r, compat)?;
                let fks: Vec<&str> = self
                    .fks_referencing(&tname)
                    .filter(|(o, f)| {
                        f.enforced
                            && f.on_delete == OnDelete::NoAction
                            && !o.name.eq_ignore_ascii_case(&tname)
                    })
                    .map(|(_, f)| f.name.as_str())
                    .collect();
                if !fks.is_empty() {
                    return Err(inv(format!(
                        "Cannot add a row deletion policy to table `{tname}`. It is referenced by one or more foreign keys: {}.",
                        backquoted(&fks)
                    )));
                }
                self.table_mut(&k)?.row_deletion_policy = Some(r.clone());
            }
            AlterTable::ReplaceRowDeletionPolicy(r) => {
                if t.row_deletion_policy.is_none() {
                    return Err(inv(format!(
                        "ROW DELETION POLICY does not exist on table {tname}."
                    )));
                }
                rdp_check(t, r, compat)?;
                self.table_mut(&k)?.row_deletion_policy = Some(r.clone());
            }
            AlterTable::DropRowDeletionPolicy => {
                if t.row_deletion_policy.is_none() {
                    return Err(inv(format!(
                        "ROW DELETION POLICY does not exist on table {tname}."
                    )));
                }
                self.table_mut(&k)?.row_deletion_policy = None;
            }
            AlterTable::AddSynonym(s) => {
                self.add_synonym(&k, s, compat)?;
            }
            AlterTable::DropSynonym(s) => {
                if !t
                    .synonym
                    .as_deref()
                    .is_some_and(|x| x.eq_ignore_ascii_case(s))
                {
                    return Err(inv(format!("SYNONYM {s} does not exist on table {tname}.")));
                }
                self.table_mut(&k)?.synonym = None;
            }
            AlterTable::Rename { to } => {
                self.rename_table(&k, to, compat)?;
            }
            AlterTable::RenameWithSynonym { to, synonym } => {
                if let Some(s) = &t.synonym {
                    return Err(inv(format!(
                        "Cannot add a synonym to table {tname} because the table already has a synonym {s}."
                    )));
                }
                if !synonym.eq_ignore_ascii_case(&tname) && self.name_taken(synonym) {
                    return Err(self.dup(synonym, compat));
                }
                let new_key = self.rename_table(&k, to, compat)?;
                self.add_synonym(&new_key, synonym, compat)?;
            }
        }
        Ok(())
    }

    fn table_mut(&mut self, k: &str) -> R<&mut Table> {
        self.tables.get_mut(k).ok_or_else(|| table_not_found(k))
    }

    fn add_synonym(&mut self, k: &str, s: &str, compat: bool) -> R<()> {
        let t = &self.tables[k];
        if let Some(old) = &t.synonym {
            return Err(inv(format!(
                "Cannot add a synonym to table {} because the table already has a synonym {old}.",
                t.name
            )));
        }
        if self.name_taken(s) {
            return Err(self.dup(s, compat));
        }
        check_name(Obj::Synonym, s, compat)?;
        self.table_mut(k)?.synonym = Some(s.to_string());
        Ok(())
    }

    /// Renames a table and every reference to it; returns the new key.
    fn rename_table(&mut self, k: &str, to: &str, compat: bool) -> R<String> {
        let old_name = self.tables[k].name.clone();
        let synonym_is_target = self.tables[k]
            .synonym
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(to));
        if !synonym_is_target {
            self.check_new_name(to, compat)?;
        }
        check_name(Obj::Table, to, compat)?;
        let mut t = self.tables.remove(k).ok_or_else(|| table_not_found(k))?;
        if synonym_is_target {
            t.synonym = None;
        }
        t.name = to.to_string();
        let new_key = key(to);
        self.tables.insert(new_key.clone(), t);
        for (kind, n) in &mut self.creation_order {
            if *kind == ObjectKind::Table && n == k {
                *n = new_key.clone();
            }
        }
        let same = |s: &str| s.eq_ignore_ascii_case(&old_name);
        for g in &mut self.grants {
            if let GrantObject::Table { table, .. } = &mut g.object
                && same(table)
            {
                *table = to.to_string();
            }
        }
        for t in self.tables.values_mut() {
            if let Some(il) = t.interleave.as_mut()
                && same(&il.parent)
            {
                il.parent = to.to_string();
            }
            for f in &mut t.foreign_keys {
                if same(&f.ref_table) {
                    f.ref_table = to.to_string();
                }
            }
        }
        for i in self.indexes.values_mut() {
            if same(&i.table) {
                i.table = to.to_string();
            }
            if i.interleave_in.as_deref().is_some_and(same) {
                i.interleave_in = Some(to.to_string());
            }
        }
        for i in self.search_indexes.values_mut() {
            if same(&i.table) {
                i.table = to.to_string();
            }
        }
        let rename = |ddl: &mut String| {
            let Ok(mut cs) = ChangeStream::parse(ddl) else {
                return;
            };
            if let CsTarget::Tables(ts) = &mut cs.target
                && ts.iter().any(|(t, _)| same(t))
            {
                for (t, _) in ts.iter_mut().filter(|(t, _)| same(t)) {
                    *t = to.to_string();
                }
                *ddl = cs.to_ddl();
            }
        };
        for o in self.opaque.iter_mut().filter(|o| o.kind == "CHANGE STREAM") {
            rename(&mut o.ddl);
            if let Some(p) = o.printed.as_mut() {
                rename(p);
            }
        }
        Ok(new_key)
    }

    /// Production's index limits: 128 per table and 10,000 per database.
    fn check_index_counts(&self, index: &str, table: &str) -> R<()> {
        let on_table = self.indexes_on(table).count()
            + self
                .search_indexes
                .values()
                .filter(|i| i.table.eq_ignore_ascii_case(table))
                .count();
        if on_table >= MAX_INDEXES_PER_TABLE {
            return Err(pre(format!(
                "Cannot add index {index} to table {table}: too many indices (limit {MAX_INDEXES_PER_TABLE} per table)."
            )));
        }
        if self.indexes.len() + self.search_indexes.len() >= MAX_INDEXES_PER_DATABASE {
            return Err(pre(format!(
                "Cannot add Index {index} : too many indexes (limit {MAX_INDEXES_PER_DATABASE} per database)."
            )));
        }
        Ok(())
    }

    fn create_index(&mut self, index: &Index, if_not_exists: bool, compat: bool) -> R<()> {
        if if_not_exists && self.indexes.contains_key(&key(&index.name)) {
            return Ok(());
        }
        let tk = self.need_table(&index.table)?;
        self.check_new_name(&index.name, compat)?;
        check_name(Obj::Index, &index.name, compat)?;
        let t = &self.tables[&tk];
        let n = &index.name;
        let schema_of = |s: &str| s.rsplit_once('.').map(|(a, _)| a.to_ascii_lowercase());
        if schema_of(n) != schema_of(&t.name) {
            return Err(pre(format!(
                "Index {n} does not match the schema of the indexed table {}.",
                t.name
            )));
        }
        let missing = |c: &str| {
            let msg = format!(
                "Index {n} specifies key column {c} which does not exist in the index's base table."
            );
            if compat { inv(msg) } else { pre(msg) }
        };
        for (i, kp) in index.key.iter().enumerate() {
            let Some(c) = t.column_exact(&kp.column) else {
                return Err(missing(&kp.column));
            };
            if index.key[..i]
                .iter()
                .any(|o| o.column.eq_ignore_ascii_case(&kp.column))
            {
                return Err(inv(format!(
                    "Index {n} specifies key column {} twice.",
                    c.name
                )));
            }
            if not_keyable(&c.ty) {
                return Err(pre(format!(
                    "Cannot reference {} {} in the creation of index {n}.",
                    base_type(&c.ty),
                    c.name
                )));
            }
        }
        for s in &index.storing {
            let Some(c) = t.column_exact(s) else {
                return Err(if compat {
                    missing(s)
                } else {
                    pre(format!(
                        "Index {n} specifies nonexistent stored column {s}."
                    ))
                });
            };
            if index.key.iter().any(|k| k.column.eq_ignore_ascii_case(s)) {
                return Err(inv(format!(
                    "Index {n} specifies stored column {} already specified as primary key.",
                    c.name
                )));
            }
            if t.primary_key
                .iter()
                .any(|k| k.column.eq_ignore_ascii_case(s))
            {
                return Err(pre(format!(
                    "Index {n} specifies stored column {} which is a key of table {}.",
                    c.name, t.name
                )));
            }
        }
        if let Some(f) = &index.filter {
            for c in crate::parser::null_filter_columns(f)? {
                if t.column(&c).is_none() {
                    return Err(pre(format!(
                        "Index {n} specifies a null filter on a nonexistent column {c}."
                    )));
                }
                // Prod and the emulator both reject a filter column the index doesn't carry: an
                // index key, a STORING column or a key of the base table.
                let in_index = index.key.iter().any(|k| k.column.eq_ignore_ascii_case(&c))
                    || index.storing.iter().any(|s| s.eq_ignore_ascii_case(&c))
                    || t.primary_key
                        .iter()
                        .any(|k| k.column.eq_ignore_ascii_case(&c));
                if !in_index {
                    return Err(DdlError::Unimplemented(format!(
                        "Cannot null-filter column {c} that is not in the index {n}."
                    )));
                }
            }
        }
        if let Some(p) = &index.interleave_in {
            let Some(parent) = self.resolve_table(p) else {
                return Err(pre(format!(
                    "Cannot interleave index {n} within nonexistent table {p}."
                )));
            };
            let not_prefix = || {
                pre(format!(
                    "Index {n} specifies keys that are not a prefix of the keys of table {}, yet is to be interleaved in {}.",
                    parent.name, parent.name
                ))
            };
            for (i, pk) in parent.primary_key.iter().enumerate() {
                let Some(kp) = index.key.get(i) else {
                    return Err(not_prefix());
                };
                let (Some(pc), Some(ic)) = (parent.column(&pk.column), t.column(&kp.column)) else {
                    return Err(not_prefix());
                };
                if !same_base(&pc.ty, &ic.ty) {
                    return Err(not_prefix());
                }
                if pc.not_null && !ic.not_null && !index.null_filtered {
                    return Err(pre(format!(
                        "Index {n} references parent key column {} that is not null, but child key is nullable.",
                        pc.name
                    )));
                }
            }
        }
        let keys = index.key.len()
            + t.primary_key
                .iter()
                .filter(|p| {
                    !index
                        .key
                        .iter()
                        .any(|k| k.column.eq_ignore_ascii_case(&p.column))
                })
                .count();
        if keys > MAX_KEY_COLUMNS {
            return Err(inv(format!(
                "Index {n} has too many keys ({keys}); the limit is {MAX_KEY_COLUMNS}."
            )));
        }
        self.check_index_counts(n, &t.name)?;
        self.indexes.insert(key(&index.name), index.clone());
        Ok(())
    }

    fn create_search_index(&mut self, index: &SearchIndex, compat: bool) -> R<()> {
        let tk = self.need_table(&index.table)?;
        self.check_new_name(&index.name, compat)?;
        check_name(Obj::SearchIndex, &index.name, compat)?;
        let t = &self.tables[&tk];
        let n = &index.name;
        for c in index
            .columns
            .iter()
            .chain(&index.storing)
            .chain(&index.partition_by)
        {
            if t.column_exact(c).is_none() {
                let msg = format!(
                    "Index {n} specifies key column {c} which does not exist in the index's base table."
                );
                return Err(if compat { inv(msg) } else { pre(msg) });
            }
        }
        for o in &index.order_by {
            match t.column(&o.column) {
                None => {
                    return Err(inv(format!(
                        "Index {n} specifies key column {} which does not exist in the index's base table.",
                        o.column
                    )));
                }
                Some(c) if !c.not_null => {
                    return Err(inv(format!(
                        "Column {} in search index {n} must be NOT NULL.",
                        c.name
                    )));
                }
                Some(c) if c.ty != ColumnType::Int64 => {
                    return Err(inv(format!(
                        "Search index {n} ordered by column {} with invalid type {}",
                        c.name,
                        base_type(&c.ty)
                    )));
                }
                _ => {}
            }
        }
        self.check_index_counts(n, &t.name)?;
        self.search_indexes.insert(key(&index.name), index.clone());
        Ok(())
    }

    /// Production's ALTER SEARCH INDEX (the emulator doesn't parse it).
    fn alter_search_index(&mut self, name: &str, action: &AlterSearchIndex) -> R<()> {
        let k = key(name);
        let Some(idx) = self.search_indexes.get(&k) else {
            return Err(DdlError::NotFound(format!("Index not found: {name}")));
        };
        let t = self
            .resolve_table(&idx.table)
            .ok_or_else(|| table_not_found(&idx.table))?;
        let n = &idx.name;
        let has = |list: &[String], c: &str| list.iter().any(|x| x.eq_ignore_ascii_case(c));
        let not_found = |c: &str| DdlError::NotFound(format!("Column not found in table {n}: {c}"));
        let mut next = idx.clone();
        match action {
            AlterSearchIndex::AddColumn(c) => {
                if has(&idx.columns, c) {
                    return Err(inv(format!("Index {n} specifies key column {c} twice.")));
                }
                let Some(col) = t.column(c) else {
                    return Err(pre(format!(
                        "Index {n} specifies key column {c} which does not exist in the index's base table."
                    )));
                };
                if col.ty != ColumnType::TokenList {
                    return Err(pre(format!(
                        "Cannot alter non-TOKENLIST key column {} in search index {n}. You can use STORED COLUMN in ALTER SEARCH INDEX statement if you want to modify a stored column in this search index.",
                        col.name
                    )));
                }
                next.columns.push(col.name.clone());
            }
            AlterSearchIndex::DropColumn(c) => {
                if !has(&idx.columns, c) {
                    return Err(not_found(c));
                }
                if idx.columns.len() == 1 {
                    return Err(pre(format!(
                        "Cannot drop the only key {} in search index {n}. You can use DROP SEARCH INDEX to completely drop this search index.",
                        idx.columns[0]
                    )));
                }
                next.columns.retain(|x| !x.eq_ignore_ascii_case(c));
            }
            AlterSearchIndex::AddStoredColumn(c) => {
                if has(&idx.storing, c) {
                    return Err(inv(format!("Index {n} specifies stored column {c} twice.")));
                }
                let Some(col) = t.column(c) else {
                    return Err(pre(format!(
                        "Index {n} specifies stored column {c} which does not exist in the index's base table."
                    )));
                };
                next.storing.push(col.name.clone());
            }
            AlterSearchIndex::DropStoredColumn(c) => {
                if !has(&idx.storing, c) {
                    return Err(not_found(c));
                }
                next.storing.retain(|x| !x.eq_ignore_ascii_case(c));
            }
        }
        self.search_indexes.insert(k, next);
        Ok(())
    }

    fn alter_index(&mut self, name: &str, action: &AlterIndex, emulator_compat: bool) -> R<()> {
        let k = key(name);
        let Some(idx) = self
            .indexes
            .get(&k)
            .filter(|i| emulator_compat || i.name == name)
        else {
            return Err(DdlError::NotFound(format!("Index not found: {name}")));
        };
        let t = self
            .resolve_table(&idx.table)
            .ok_or_else(|| table_not_found(&idx.table))?;
        match action {
            AlterIndex::AddStoredColumn(c) => {
                let Some(col) = t.column_exact(c) else {
                    // Production looks the column up by its exact spelling; the emulator finds
                    // it ignoring case and then fails validating the index.
                    return Err(if !emulator_compat {
                        pre(format!(
                            "Index {} specifies nonexistent stored column {c}.",
                            idx.name
                        ))
                    } else if t.column(c).is_some() {
                        inv(format!(
                            "Index {} specifies key column {c} which does not exist in the index's base table.",
                            idx.name
                        ))
                    } else {
                        DdlError::NotFound(format!("Column not found in table {}: {c}", t.name))
                    });
                };
                let present = idx.storing.iter().any(|s| s.eq_ignore_ascii_case(c))
                    || idx.key.iter().any(|s| s.column.eq_ignore_ascii_case(c))
                    || t.primary_key
                        .iter()
                        .any(|s| s.column.eq_ignore_ascii_case(c));
                if present {
                    return Err(pre(format!(
                        "Column {} is already existed in index {}.",
                        col.name, idx.name
                    )));
                }
                let col = col.name.clone();
                if let Some(i) = self.indexes.get_mut(&k) {
                    i.storing.push(col);
                }
            }
            AlterIndex::DropStoredColumn(c) => {
                if !idx.storing.iter().any(|s| s.eq_ignore_ascii_case(c)) {
                    return Err(DdlError::NotFound(format!(
                        "Index {} does not have stored column {c}.",
                        idx.name
                    )));
                }
                let filter_cols = match &idx.filter {
                    Some(f) => crate::parser::null_filter_columns(f)?,
                    None => Vec::new(),
                };
                let (dropped, kept): (Vec<String>, Vec<String>) = filter_cols
                    .into_iter()
                    .partition(|f| f.eq_ignore_ascii_case(c));
                // Prod refuses to drop a column the WHERE clause filters on. The emulator drops
                // the column's `IS NOT NULL` conjunct with it (and the WHERE clause when none is
                // left); spanbolt then also backfills, since storage rebuilds an index whose null
                // filter changes.
                if let Some(col) = dropped.first()
                    && !emulator_compat
                {
                    return Err(DdlError::Syntax(format!(
                        "Cannot drop null filtered column {col} in index {}.",
                        idx.name
                    )));
                }
                let filter = (!kept.is_empty()).then(|| crate::parser::null_filter_sql(&kept));
                if let Some(i) = self.indexes.get_mut(&k) {
                    i.storing.retain(|s| !s.eq_ignore_ascii_case(c));
                    i.filter = filter;
                }
            }
        }
        Ok(())
    }

    fn opaque_stmt(&mut self, object: &OpaqueObject, drop: bool, compat: bool) -> R<()> {
        let kind = object.kind.as_str();
        let name = object.name.as_str();
        match kind {
            "ROLE" => return self.role_stmt(object, drop, compat),
            "GRANT" | "REVOKE" => return self.grant_stmt(&object.ddl, compat),
            _ => {}
        }
        if drop {
            let Some(i) = self.opaque_find(kind, name) else {
                if object.ddl == "IF EXISTS" || kind == "ROLE" {
                    return Ok(());
                }
                let what = match kind {
                    "CHANGE STREAM" => "Change Stream",
                    "PROPERTY GRAPH" => "Property Graph",
                    "MODEL" => "Model",
                    _ => kind,
                };
                return Err(DdlError::NotFound(format!("{what} not found: {name}")));
            };
            let removed = self.opaque.remove(i);
            if kind == "CHANGE STREAM" {
                let tvf = format!("READ_{}", removed.name);
                self.grants.retain(|g| match &g.object {
                    GrantObject::ChangeStream(c) => *c != removed.name,
                    GrantObject::TableFunction(f) => *f != tvf,
                    _ => true,
                });
            }
            return Ok(());
        }
        match kind {
            "CHANGE STREAM" => {
                let (_, if_not_exists, ddl) = normalize_create_head(&object.ddl, kind);
                if if_not_exists && self.opaque_find(kind, name).is_some() {
                    return Ok(());
                }
                self.check_new_name(name, compat)?;
                check_name(Obj::ChangeStream, name, compat)?;
                let cs = ChangeStream::parse(&ddl)?;
                self.validate_change_stream(&cs, compat)?;
                self.opaque.push(OpaqueObject {
                    kind: kind.to_string(),
                    name: name.to_string(),
                    ddl,
                    printed: None,
                });
            }
            "ALTER CHANGE STREAM" => {
                let Some(i) = self.opaque_find("CHANGE STREAM", name) else {
                    return Err(DdlError::NotFound(format!(
                        "Change Stream not found: {name}"
                    )));
                };
                let mut cs = ChangeStream::parse(&self.opaque[i].ddl)?;
                let written = cs.alter(&object.ddl, compat)?;
                self.validate_change_stream(&cs, compat)?;
                let o = &mut self.opaque[i];
                // The emulator prints the option list last written; its effective options merge.
                let printed_options = match (written, &o.printed) {
                    (Some(w), _) if compat => Some(w),
                    (None, Some(p)) => ChangeStream::parse(p).ok().map(|p| p.options),
                    _ => None,
                };
                o.printed = printed_options.map(|options| {
                    ChangeStream {
                        options,
                        ..cs.clone()
                    }
                    .to_ddl()
                });
                o.ddl = cs.to_ddl();
            }
            "PROPERTY GRAPH" | "MODEL" => {
                let (or_replace, if_not_exists, ddl) = normalize_create_head(&object.ddl, kind);
                if let Some(i) = self.opaque_find(kind, name) {
                    if if_not_exists {
                        return Ok(());
                    }
                    if or_replace {
                        self.opaque[i].ddl = ddl;
                        return Ok(());
                    }
                }
                self.check_new_name(name, compat)?;
                check_name(
                    if kind == "MODEL" {
                        Obj::Model
                    } else {
                        Obj::PropertyGraph
                    },
                    name,
                    compat,
                )?;
                self.opaque.push(OpaqueObject {
                    kind: kind.to_string(),
                    name: name.to_string(),
                    ddl,
                    printed: None,
                });
            }
            "ANALYZE" => {}
            _ => self.opaque.push(object.clone()),
        }
        Ok(())
    }

    /// `CREATE ROLE` and `DROP ROLE`. The emulator accepts both without checking them.
    fn role_stmt(&mut self, object: &OpaqueObject, drop: bool, compat: bool) -> R<()> {
        let name = object.name.as_str();
        let found = self.opaque_find("ROLE", name);
        if drop {
            let Some(i) = found else {
                if compat || object.ddl == "IF EXISTS" {
                    return Ok(());
                }
                return Err(pre(format!("Role not found: {name}.")));
            };
            let role = self.opaque[i].name.clone();
            // Roles granted to the role don't count.
            let privileged = self
                .grants
                .iter()
                .any(|g| g.grantee == role && !matches!(g.object, GrantObject::Role(_)));
            if !compat && privileged {
                return Err(inv(format!(
                    "Cannot drop role `{name}` which has active privileges. All privileges must be revoked from a role before it can be dropped."
                )));
            }
            self.opaque.remove(i);
            self.grants
                .retain(|g| g.grantee != role && g.object != GrantObject::Role(role.clone()));
            return Ok(());
        }
        let error = if name.eq_ignore_ascii_case("public") {
            inv(format!(
                "Invalid role name. `{name}` is a reserved system role name, ignoring case."
            ))
        } else if found.is_some() {
            pre(format!("Duplicate name in schema: {name}."))
        } else {
            self.opaque.push(object.clone());
            return Ok(());
        };
        if compat { Ok(()) } else { Err(error) }
    }

    /// `GRANT` and `REVOKE`. The emulator accepts any it can parse and checks nothing, so under
    /// `compat` one that names a missing role or object is accepted and not stored.
    fn grant_stmt(&mut self, ddl: &str, compat: bool) -> R<()> {
        let stmt = parse_grant(ddl)?;
        let (roles, objects) = match self.resolve_grant(&stmt) {
            Ok(r) => r,
            Err(_) if compat => return Ok(()),
            Err(e) => return Err(e),
        };
        for grantee in roles {
            for object in &objects {
                let held = |g: &Grant| g.grantee == grantee && g.object == *object;
                if stmt.revoke {
                    self.grants.retain(|g| !held(g));
                } else if !self.grants.iter().any(held) {
                    self.grants.push(Grant {
                        grantee: grantee.clone(),
                        object: object.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// The grantees and objects of a grant, under their names in the schema.
    fn resolve_grant(&self, stmt: &GrantStmt) -> R<(Vec<String>, Vec<GrantObject>)> {
        let role = |name: &str, system: bool| -> R<String> {
            if let Some(i) = self.opaque_find("ROLE", name) {
                return Ok(self.opaque[i].name.clone());
            }
            if system && let Some(r) = SYSTEM_ROLES.iter().find(|r| r.eq_ignore_ascii_case(name)) {
                return Ok(r.to_string());
            }
            Err(pre(format!("Role not found: {name}.")))
        };
        let roles = stmt
            .roles
            .iter()
            .map(|r| role(r, false))
            .collect::<R<Vec<_>>>()?;
        let stream = |name: &str| {
            self.opaque_find("CHANGE STREAM", name)
                .map(|i| self.opaque[i].name.clone())
        };
        let mut objects = Vec::new();
        for object in &stmt.objects {
            objects.push(match object {
                GrantObject::Table {
                    table,
                    privilege,
                    column,
                } => {
                    let t = &self.tables[&self.need_table(table)?];
                    let column = match column {
                        None => None,
                        Some(c) => Some(
                            t.columns
                                .iter()
                                .find(|x| x.name.eq_ignore_ascii_case(c))
                                .map(|x| x.name.clone())
                                .ok_or_else(|| {
                                    DdlError::NotFound(format!(
                                        "Column not found in table {}: {c}",
                                        t.name
                                    ))
                                })?,
                        ),
                    };
                    GrantObject::Table {
                        table: t.name.clone(),
                        privilege: *privilege,
                        column,
                    }
                }
                GrantObject::View(v) => GrantObject::View(
                    self.views
                        .get(&key(v))
                        .map(|x| x.name.clone())
                        .ok_or_else(|| DdlError::NotFound(format!("VIEW not found: {v}")))?,
                ),
                GrantObject::ChangeStream(c) => {
                    GrantObject::ChangeStream(stream(c).ok_or_else(|| {
                        DdlError::NotFound(format!("Change Stream not found: {c}"))
                    })?)
                }
                GrantObject::TableFunction(f) => GrantObject::TableFunction(
                    f.get(..5)
                        .filter(|p| p.eq_ignore_ascii_case("READ_"))
                        .and_then(|_| stream(&f[5..]))
                        .map(|c| format!("READ_{c}"))
                        .ok_or_else(|| {
                            DdlError::NotFound(format!("TABLE_FUNCTION not found: {f}"))
                        })?,
                ),
                GrantObject::Role(r) => GrantObject::Role(role(r, true)?),
            });
        }
        Ok((roles, objects))
    }

    /// Change streams explicitly tracking `table` (or `column` of it).
    fn streams_tracking(&self, table: &str, column: Option<&str>) -> Vec<String> {
        self.opaque
            .iter()
            .filter(|o| o.kind == "CHANGE STREAM")
            .filter_map(|o| ChangeStream::parse(&o.ddl).ok())
            .filter(|cs| match &cs.target {
                CsTarget::Tables(ts) => ts.iter().any(|(t, cols)| {
                    t.eq_ignore_ascii_case(table)
                        && match column {
                            None => true,
                            Some(c) => cols.iter().flatten().any(|x| x.eq_ignore_ascii_case(c)),
                        }
                }),
                _ => false,
            })
            .map(|cs| cs.name)
            .collect()
    }

    /// Production resolves tracked tables and columns by their exact spelling and answers
    /// INVALID_ARGUMENT; the emulator ignores case and answers FAILED_PRECONDITION.
    fn validate_change_stream(&self, cs: &ChangeStream, compat: bool) -> R<()> {
        check_change_stream_options(cs, compat)?;
        let err = |msg: String| if compat { pre(msg) } else { inv(msg) };
        if let CsTarget::Tables(ts) = &cs.target {
            for (t, cols) in ts {
                let table = if compat {
                    self.resolve_table(t)
                } else {
                    self.table_exact(t)
                };
                let Some(table) = table else {
                    return Err(err(format!(
                        "Change Stream {} cannot track {t} because either Change Streams do not track database objects of this type, or this Table does not exist.",
                        cs.name
                    )));
                };
                for c in cols.iter().flatten() {
                    let found = if compat {
                        table.column(c)
                    } else {
                        table.column_exact(c)
                    };
                    if found.is_none() {
                        return Err(err(format!(
                            "Change Stream {} cannot track column {c} because it does not exist in Table {}.",
                            cs.name, table.name
                        )));
                    }
                }
            }
        }
        self.check_change_stream_limit(cs, compat)
    }

    /// The tables, non-key columns (as `table.column`) and ALL that `cs` tracks, in the order
    /// Spanner checks them against the limit.
    fn tracked_objects(&self, cs: &ChangeStream) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let non_key = |t: &Table| -> Vec<String> {
            t.columns
                .iter()
                .filter(|c| {
                    !t.primary_key
                        .iter()
                        .any(|k| k.column.eq_ignore_ascii_case(&c.name))
                })
                .map(|c| c.name.clone())
                .collect()
        };
        let whole = |t: &Table, out: &mut Vec<(String, String)>| {
            out.push((t.name.clone(), String::new()));
            for c in non_key(t) {
                out.push((t.name.clone(), c));
            }
        };
        match &cs.target {
            CsTarget::None => {}
            CsTarget::All => {
                out.push((String::new(), String::new()));
                for (_, n) in &self.creation_order {
                    if let Some(t) = self.tables.get(n) {
                        whole(t, &mut out);
                    }
                }
            }
            CsTarget::Tables(ts) => {
                for (t, cols) in ts {
                    let Some(table) = self.resolve_table(t) else {
                        continue;
                    };
                    match cols {
                        None => whole(table, &mut out),
                        Some(cols) => {
                            for c in cols {
                                if let Some(col) = table.column(c) {
                                    out.push((table.name.clone(), col.name.clone()));
                                }
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// At most three change streams may track the same table, non-key column, or ALL.
    fn check_change_stream_limit(&self, cs: &ChangeStream, compat: bool) -> R<()> {
        let others: Vec<Vec<(String, String)>> = self
            .opaque
            .iter()
            .filter(|o| o.kind == "CHANGE STREAM" && !o.name.eq_ignore_ascii_case(&cs.name))
            .filter_map(|o| ChangeStream::parse(&o.ddl).ok())
            .map(|o| self.tracked_objects(&o))
            .collect();
        let same = |a: &(String, String), b: &(String, String)| {
            a.0.eq_ignore_ascii_case(&b.0) && a.1.eq_ignore_ascii_case(&b.1)
        };
        for obj in self.tracked_objects(cs) {
            let count = others
                .iter()
                .filter(|o| o.iter().any(|x| same(x, &obj)))
                .count();
            if count < 3 {
                continue;
            }
            let what = match (obj.0.as_str(), obj.1.as_str()) {
                ("", _) => "ALL".to_string(),
                (t, "") => t.to_string(),
                (_, c) if compat => c.to_string(),
                (t, c) => format!("{t}.{c}"),
            };
            let sep = if compat { " :" } else { "" };
            return Err(pre(format!(
                "Failed to create or alter Change Stream {}{sep} because it is not allowed to have more than 3 Change Streams tracking the same table or non-key column or ALL: {what}.",
                cs.name
            )));
        }
        Ok(())
    }

    fn apply_inner(&mut self, stmt: &Statement, compat: bool) -> R<()> {
        self.check_locality_refs(stmt, compat)?;
        match stmt {
            Statement::CreateSchema {
                name,
                if_not_exists,
            } => {
                let k = key(name);
                if self.schemas.contains_key(&k) {
                    return if *if_not_exists {
                        Ok(())
                    } else {
                        Err(self.dup(name, compat))
                    };
                }
                if names::reserved_schema_name(name) {
                    return Err(name_not_valid(Obj::Schema.label(compat), name));
                }
                check_name(Obj::Schema, name, compat)?;
                self.schemas.insert(k.clone(), name.clone());
                self.push_order(ObjectKind::Schema, k);
            }
            Statement::DropSchema { name } => {
                let k = key(name);
                if !self.schemas.contains_key(&k) {
                    return Err(DdlError::NotFound(format!("Schema not found: {name}")));
                }
                let prefix = format!("{k}.");
                let in_schema = |n: &&String| n.starts_with(&prefix);
                let mut msg = String::new();
                let groups: [(&str, Vec<&str>); 4] = [
                    (
                        "tables",
                        self.tables
                            .iter()
                            .filter(|(n, _)| in_schema(n))
                            .map(|(_, t)| t.name.as_str())
                            .collect(),
                    ),
                    (
                        "views",
                        self.views
                            .iter()
                            .filter(|(n, _)| in_schema(n))
                            .map(|(_, v)| v.name.as_str())
                            .collect(),
                    ),
                    (
                        "indexes",
                        self.indexes
                            .iter()
                            .filter(|(n, _)| in_schema(n))
                            .map(|(_, i)| i.name.as_str())
                            .chain(
                                self.search_indexes
                                    .iter()
                                    .filter(|(n, _)| in_schema(n))
                                    .map(|(_, i)| i.name.as_str()),
                            )
                            .collect(),
                    ),
                    (
                        "sequences",
                        self.sequences
                            .iter()
                            .filter(|(n, _)| in_schema(n))
                            .map(|(_, q)| q.name.as_str())
                            .collect(),
                    ),
                ];
                for (what, names) in groups {
                    if !names.is_empty() {
                        msg.push_str(&format!("\nDependent {what}:{}", names.join(",")));
                    }
                }
                if !msg.is_empty() {
                    return Err(pre(format!(
                        "Cannot drop schema {name} while it has dependencies.{msg}"
                    )));
                }
                self.schemas.remove(&k);
                self.drop_order(ObjectKind::Schema, &k);
            }
            Statement::CreateTable {
                table,
                if_not_exists,
            } => self.create_table(table, *if_not_exists, compat)?,
            Statement::DropTable { name, if_exists } => self.drop_table(name, *if_exists)?,
            Statement::AlterTable {
                table,
                action,
                if_exists,
            } => {
                if *if_exists && self.table_exact(table).is_none() {
                    return Ok(());
                }
                self.alter_table(table, action, compat)?
            }
            Statement::RenameTables { renames } => {
                // Spanner renames one pair after the other, so a chain can go through a new name;
                // the emulator looks every source up in the schema before the statement.
                if compat {
                    for (from, _) in renames {
                        self.need_table(from)?;
                    }
                }
                let mut next = self.clone();
                for (from, to) in renames {
                    let k = next.need_table(from)?;
                    next.rename_table(&k, to, compat)?;
                }
                let version = self.version;
                *self = next;
                self.version = version;
            }
            Statement::CreateIndex {
                index,
                if_not_exists,
            } => self.create_index(index, *if_not_exists, compat)?,
            Statement::DropIndex { name, if_exists } => {
                let k = key(name);
                let exact = self.indexes.get(&k).is_some_and(|i| i.name == *name);
                if exact {
                    self.indexes.remove(&k);
                } else if !if_exists {
                    return Err(DdlError::NotFound(format!("Index not found: {name}")));
                }
            }
            Statement::AlterIndex { name, action } => self.alter_index(name, action, false)?,
            Statement::CreateSearchIndex { index } => self.create_search_index(index, compat)?,
            Statement::AlterSearchIndex { name, action } => {
                self.alter_search_index(name, action)?
            }
            Statement::DropSearchIndex { name, if_exists } => {
                if self.search_indexes.remove(&key(name)).is_none() && !if_exists {
                    return Err(DdlError::NotFound(format!("Index not found: {name}")));
                }
            }
            Statement::CreateSequence {
                sequence,
                if_not_exists,
            } => {
                let k = key(&sequence.name);
                if *if_not_exists && self.sequences.contains_key(&k) {
                    return Ok(());
                }
                self.check_new_name(&sequence.name, compat)?;
                check_name(Obj::Sequence, &sequence.name, compat)?;
                let has_kind = sequence
                    .options
                    .iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case("sequence_kind") && v != "NULL")
                    || self
                        .options
                        .iter()
                        .any(|(k, _)| k.eq_ignore_ascii_case("default_sequence_kind"));
                if !has_kind {
                    return Err(inv(
                        "The sequence does not have a valid sequence kind. Please specify the sequence kind explicitly or set the database option `default_sequence_kind`.".to_string(),
                    ));
                }
                self.sequences.insert(k.clone(), sequence.clone());
                self.push_order(ObjectKind::Sequence, k);
            }
            Statement::AlterSequence { name, options } => {
                let Some(seq) = self.sequences.get_mut(&key(name)) else {
                    return Err(DdlError::NotFound(format!("Sequence not found: {name}")));
                };
                merge_options(&mut seq.options, options);
            }
            Statement::DropSequence { name, if_exists } => {
                let k = key(name);
                // Production drops a sequence only by its declared spelling, and names it
                // alone when it doesn't.
                let exact = self.sequences.get(&k).is_some_and(|q| q.name == *name);
                if !compat && !exact && !*if_exists && self.sequences.contains_key(&k) {
                    return Err(DdlError::NotFound(name.clone()));
                }
                if !(exact || compat) || self.sequences.remove(&k).is_none() {
                    if *if_exists {
                        return Ok(());
                    }
                    return Err(DdlError::NotFound(format!("Sequence not found: {name}")));
                }
                self.drop_order(ObjectKind::Sequence, &k);
            }
            Statement::CreateView { view, or_replace } => {
                let k = key(&view.name);
                if self.views.contains_key(&k) {
                    if !or_replace {
                        return Err(self.dup(&view.name, compat));
                    }
                    self.views.insert(k, view.clone());
                } else {
                    self.check_new_name(&view.name, compat)?;
                    check_name(Obj::View, &view.name, compat)?;
                    self.views.insert(k.clone(), view.clone());
                    self.push_order(ObjectKind::View, k);
                }
            }
            Statement::DropView { name, if_exists } => {
                let k = key(name);
                // The emulator drops a view only by its declared spelling; production ignores case.
                let exact = self.views.get(&k).is_some_and(|v| v.name == *name);
                let view = if compat && !exact {
                    None
                } else {
                    self.views.remove(&k)
                };
                let Some(view) = view else {
                    if *if_exists {
                        return Ok(());
                    }
                    return Err(DdlError::NotFound(format!("View not found: {name}")));
                };
                self.drop_order(ObjectKind::View, &k);
                self.grants
                    .retain(|g| g.object != GrantObject::View(view.name.clone()));
            }
            Statement::AlterDatabase { options, name } => {
                self.database_name = Some(name.clone());
                merge_options(&mut self.options, options);
            }
            Statement::CreateLocalityGroup {
                group,
                if_not_exists,
            } => self.create_locality_group(group, *if_not_exists, compat)?,
            Statement::AlterLocalityGroup { name, options } => {
                self.alter_locality_group(name, options, compat)?
            }
            Statement::DropLocalityGroup { name, if_exists } => {
                self.drop_locality_group(name, *if_exists, compat)?
            }
            Statement::Opaque { object, drop } => self.opaque_stmt(object, *drop, compat)?,
        }
        Ok(())
    }

    /// Apply a parsed statement, validating it against the current schema. On error the schema
    /// is unchanged.
    pub fn apply(&mut self, stmt: &Statement) -> Result<(), DdlError> {
        self.apply_inner(stmt, false)?;
        self.version += 1;
        Ok(())
    }

    /// `apply`, following the official emulator where it disagrees with production Spanner
    /// (`--emulator-compat`). `apply` follows production.
    pub fn apply_emulator_compat(&mut self, stmt: &Statement) -> Result<(), DdlError> {
        match stmt {
            Statement::AlterIndex { name, action } => {
                self.alter_index(name, action, true)?;
                self.version += 1;
                Ok(())
            }
            _ => {
                self.apply_inner(stmt, true)?;
                self.version += 1;
                Ok(())
            }
        }
    }

    /// Checks on existing rows that `stmt` needs before it can be applied; call before `apply`.
    pub fn data_checks(&self, stmt: &Statement) -> Vec<DataCheck> {
        let mut out = Vec::new();
        match stmt {
            Statement::AlterTable { table, action, .. } => {
                let Some(t) = self.table_exact(table) else {
                    return out;
                };
                match action {
                    AlterTable::AddColumn { column, .. }
                        if column.not_null
                            && column.default.is_none()
                            && column.generated.is_none()
                            && t.column(&column.name).is_none() =>
                    {
                        out.push(DataCheck::TableEmpty {
                            table: t.name.clone(),
                            message: format!(
                                "Cannot add NOT NULL column {}.{} to existing table {}.",
                                t.name, column.name, t.name
                            ),
                        });
                    }
                    AlterTable::AlterColumn { column } => {
                        let Some(old) = t.column_exact(&column.name) else {
                            return out;
                        };
                        if column.not_null && !old.not_null {
                            out.push(DataCheck::NoNulls {
                                table: t.name.clone(),
                                column: old.name.clone(),
                            });
                        }
                        if let Some(limit) = max_len(&column.ty)
                            && max_len(&old.ty).is_none_or(|o| o > limit)
                        {
                            out.push(DataCheck::MaxLength {
                                table: t.name.clone(),
                                column: old.name.clone(),
                                limit,
                            });
                        }
                    }
                    AlterTable::AddForeignKey(fk) if fk.enforced => {
                        out.push(DataCheck::ForeignKey {
                            table: t.name.clone(),
                            name: fk.name.clone(),
                        })
                    }
                    AlterTable::AddCheck(c) => out.push(DataCheck::Check {
                        table: t.name.clone(),
                        name: c.name.clone(),
                    }),
                    _ => {}
                }
            }
            Statement::CreateIndex { index, .. } if index.unique => {
                if !self.indexes.contains_key(&key(&index.name)) {
                    out.push(DataCheck::UniqueIndex {
                        index: index.name.clone(),
                    });
                }
            }
            _ => {}
        }
        out
    }
}

/// Declared STRING/BYTES length, also of array elements; `None` for MAX and other types.
fn max_len(ty: &ColumnType) -> Option<u64> {
    match ty {
        ColumnType::String(n) | ColumnType::Bytes(n) => *n,
        ColumnType::Array(e) => max_len(e),
        _ => None,
    }
}

fn rdp_break(column: &str, table: &str) -> DdlError {
    inv(format!(
        "ROW DELETION POLICY will break if drop/alter column named {column} in table {table}. Row Deletion Policy must be set on column of type TIMESTAMP."
    ))
}

fn count_streams(names: &[String]) -> String {
    let noun = if names.len() == 1 {
        "Change Stream"
    } else {
        "Change Streams"
    };
    format!("{} {noun}: {}", names.len(), names.join(", "))
}

fn type_change_ok(old: &ColumnType, new: &ColumnType) -> bool {
    use ColumnType::*;
    match (old, new) {
        (String(_) | Bytes(_), String(_) | Bytes(_)) => true,
        (Array(a), Array(b)) => type_change_ok(a, b),
        (a, b) => a == b,
    }
}

fn merge_options(into: &mut Vec<(String, String)>, from: &[(String, String)]) {
    for (k, v) in from {
        match into.iter_mut().find(|(ek, _)| ek.eq_ignore_ascii_case(k)) {
            Some(e) => e.1 = v.clone(),
            None => into.push((k.clone(), v.clone())),
        }
    }
    into.retain(|(_, v)| v != "NULL");
}

/// Strips `OR REPLACE` / `IF NOT EXISTS` from a `CREATE <kind>` statement.
fn normalize_create_head(ddl: &str, kind: &str) -> (bool, bool, String) {
    let Ok(toks) = tokenize(ddl) else {
        return (false, false, ddl.to_string());
    };
    let words: Vec<String> = toks
        .iter()
        .take(8)
        .map(|t| match &t.tok {
            Tok::Ident(w) => w.to_ascii_uppercase(),
            _ => String::new(),
        })
        .collect();
    let mut i = 1;
    let or_replace = words.get(1).is_some_and(|w| w == "OR");
    if or_replace {
        i += 2;
    }
    i += kind.split(' ').count();
    let if_not_exists = words.get(i).is_some_and(|w| w == "IF");
    if if_not_exists {
        i += 3;
    }
    let rest = toks.get(i).map_or("", |t| &ddl[t.start..]);
    (or_replace, if_not_exists, format!("CREATE {kind} {rest}"))
}

/// True if the SQL text mentions `name` as an identifier path.
fn sql_mentions(sql: &str, name: &str) -> bool {
    let Ok(toks) = tokenize(sql) else {
        return false;
    };
    let parts: Vec<&str> = name.split('.').collect();
    let ident = |t: &Tok| match t {
        Tok::Ident(w) | Tok::QuotedIdent(w) => Some(w.clone()),
        _ => None,
    };
    let mut i = 0;
    while i < toks.len() {
        let ok = parts.iter().enumerate().all(|(j, p)| {
            let at = i + 2 * j;
            ident(&toks.get(at).map_or(Tok::Eof, |t| t.tok.clone()))
                .is_some_and(|w| w.eq_ignore_ascii_case(p))
                && (j + 1 == parts.len()
                    || matches!(toks.get(at + 1).map(|t| &t.tok), Some(Tok::Sym("."))))
        });
        let preceded_by_dot = i > 0 && matches!(toks[i - 1].tok, Tok::Sym("."));
        if ok && !preceded_by_dot {
            return true;
        }
        i += 1;
    }
    false
}

/// The emulator rejects a CHECK constraint whose name carries a schema, which is every unnamed one
/// on a table in a named schema (`sch.CK_T_..._1`). Its message has no object kind.
fn check_name_in_default_schema(name: &str) -> R<()> {
    if name.contains('.') {
        return Err(DdlError::Unimplemented(format!(
            " not yet supported in named schema: {name}."
        )));
    }
    Ok(())
}

/// Seconds in a change stream `retention_period` (`36h`, `1440m`, `2d`, `86400s`).
fn duration_secs(v: &str) -> Option<u64> {
    let unit = match v.chars().last()? {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return None,
    };
    let n = &v[..v.len() - 1];
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse::<u64>().ok()?.checked_mul(unit)
}

/// `retention_period` and `value_capture_type` values. The emulator allows up to 7 days of
/// retention, production 30.
fn check_change_stream_options(cs: &ChangeStream, compat: bool) -> R<()> {
    let opt = |k: &str| {
        cs.options
            .iter()
            .find(|(o, _)| o == k)
            .and_then(|(_, v)| unquote(v))
    };
    let name = &cs.name;
    let retention = opt("retention_period").and_then(|v| {
        let max_days = if compat { 7 } else { 30 };
        match duration_secs(&v) {
            None if compat => Some(format!("Invalid time duration format: {v}.")),
            None => Some(format!(
                "ChangeStream {name} has invalid retention_period option: {v}. Invalid time duration format"
            )),
            Some(s) if !(86400..=max_days * 86400).contains(&s) => Some(if compat {
                format!(
                    "Invalid retention_period: {v}. Change Streams retention period must be a value in the range [24h, 7d]."
                )
            } else {
                format!(
                    "ChangeStream {name} has invalid retention_period option: {v}. Change Streams retention period must be a value in the range [24h, 30d]"
                )
            }),
            Some(_) => None,
        }
    });
    let capture = opt("value_capture_type")
        .filter(|v| {
            !matches!(
                v.as_str(),
                "OLD_AND_NEW_VALUES" | "NEW_ROW" | "NEW_VALUES" | "NEW_ROW_AND_OLD_VALUES"
            )
        })
        .map(|v| {
            if compat {
                format!(
                    "Invalid value_capture_type: {v}. Change Streams only support value capture types in OLD_AND_NEW_VALUES, NEW_ROW, and NEW_VALUES."
                )
            } else {
                format!(
                    "ChangeStream {name} has invalid value_capture_type option: {v}. Invalid value_capture_type"
                )
            }
        });
    // The emulator stops at the first bad option; Spanner lists them all, value_capture_type first.
    let errors: Vec<String> = if compat {
        retention.or(capture).into_iter().collect()
    } else {
        capture.into_iter().chain(retention).collect()
    };
    if errors.is_empty() {
        Ok(())
    } else {
        Err(pre(errors.join(", ")))
    }
}

/// A string option literal's value; `None` for NULL and other literals.
fn unquote(v: &str) -> Option<String> {
    match tokenize(v).ok()?.first().map(|t| &t.tok) {
        Some(Tok::Str(s)) => Some(s.clone()),
        _ => None,
    }
}
