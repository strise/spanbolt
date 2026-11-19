//! Schema validation and mutation for parsed DDL statements.

use crate::DdlError;
use crate::lexer::{Tok, tokenize};
use crate::parser::{ChangeStream, CsTarget};
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

fn validate_column(table: &str, c: &Column) -> R<()> {
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
    if c.default.is_some() && c.generated.is_some() {
        return Err(inv(format!(
            "Cannot use DEFAULT with generated column `{}`.",
            c.name
        )));
    }
    if c.ty == ColumnType::TokenList && !c.hidden {
        return Err(inv(format!(
            "TOKENLIST column {table}.{} must be HIDDEN.",
            c.name
        )));
    }
    Ok(())
}

fn rdp_check(t: &Table, r: &RowDeletionPolicy) -> R<()> {
    match t.column(&r.column) {
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

    fn need_table(&self, name: &str) -> R<String> {
        self.table_key(name).ok_or_else(|| table_not_found(name))
    }

    fn opaque_find(&self, kind: &str, name: &str) -> Option<usize> {
        self.opaque
            .iter()
            .position(|o| o.kind == kind && o.name.eq_ignore_ascii_case(name))
    }

    /// True if `name` is used by any schema object sharing Spanner's name space.
    fn name_taken(&self, name: &str) -> bool {
        let k = key(name);
        self.tables.contains_key(&k)
            || self.indexes.contains_key(&k)
            || self.search_indexes.contains_key(&k)
            || self.views.contains_key(&k)
            || self.sequences.contains_key(&k)
            || self.tables.values().any(|t| {
                t.synonym
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case(name))
                    || t.foreign_keys
                        .iter()
                        .any(|f| f.name.eq_ignore_ascii_case(name))
                    || t.checks.iter().any(|c| c.name.eq_ignore_ascii_case(name))
            })
            || self.opaque.iter().any(|o| {
                matches!(
                    o.kind.as_str(),
                    "CHANGE STREAM" | "MODEL" | "PROPERTY GRAPH"
                ) && o.name.eq_ignore_ascii_case(name)
            })
    }

    fn check_new_name(&self, name: &str) -> R<()> {
        if self.name_taken(name) {
            return Err(dup(name));
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
    fn validate_fk(&self, table: &Table, fk: &ForeignKey) -> R<()> {
        let referenced = if fk.ref_table.eq_ignore_ascii_case(&table.name) {
            table
        } else {
            self.resolve_table(&fk.ref_table)
                .ok_or_else(|| table_not_found(&fk.ref_table))?
        };
        for c in &fk.columns {
            if table.column(c).is_none() {
                return Err(pre(format!(
                    "Column `{c}` not found for table `{}` in foreign key `{}`.",
                    table.name, fk.name
                )));
            }
        }
        for c in &fk.ref_columns {
            if referenced.column(c).is_none() {
                return Err(pre(format!(
                    "Column `{c}` not found for table `{}` in foreign key `{}`.",
                    referenced.name, fk.name
                )));
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

    /// Picks a free generated constraint name (`..._1`, `..._2`, ...).
    fn free_generated(&self, name: &str, taken: &[String]) -> String {
        let base = name.strip_suffix("_1").unwrap_or(name);
        let mut n = 1;
        loop {
            let cand = format!("{base}_{n}");
            if !self.name_taken(&cand) && !taken.iter().any(|t| t.eq_ignore_ascii_case(&cand)) {
                return cand;
            }
            n += 1;
        }
    }

    fn validate_interleave(&self, t: &Table) -> R<()> {
        let Some(il) = &t.interleave else {
            return Ok(());
        };
        let parent = self
            .resolve_table(&il.parent)
            .ok_or_else(|| table_not_found(&il.parent))?;
        for (i, pk) in parent.primary_key.iter().enumerate() {
            let pos = t
                .primary_key
                .iter()
                .position(|c| c.column.eq_ignore_ascii_case(&pk.column));
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

    fn create_table(&mut self, table: &Table, if_not_exists: bool) -> R<()> {
        if if_not_exists && self.table_key(&table.name).is_some() {
            return Ok(());
        }
        self.check_new_name(&table.name)?;
        let mut t = table.clone();
        for (i, c) in t.columns.iter().enumerate() {
            if t.columns[..i]
                .iter()
                .any(|o| o.name.eq_ignore_ascii_case(&c.name))
            {
                return Err(pre(format!("Duplicate column name {}.{}.", t.name, c.name)));
            }
            validate_column(&t.name, c)?;
        }
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
            match t.column(&k.column) {
                None => {
                    return Err(pre(format!(
                        "Table {} references nonexistent key column {}.",
                        t.name, k.column
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
                _ => {}
            }
        }
        self.validate_interleave(&t)?;
        let mut names: Vec<String> = Vec::new();
        for i in 0..t.foreign_keys.len() {
            if t.foreign_keys[i].generated_name {
                t.foreign_keys[i].name = self.free_generated(&t.foreign_keys[i].name, &names);
            }
            let fk = &t.foreign_keys[i];
            if (!fk.generated_name && self.name_taken(&fk.name))
                || names.iter().any(|n| n.eq_ignore_ascii_case(&fk.name))
                || fk.name.eq_ignore_ascii_case(&t.name)
            {
                return Err(dup(&fk.name));
            }
            self.validate_fk(&t, fk)?;
            names.push(fk.name.clone());
        }
        for i in 0..t.checks.len() {
            if t.checks[i].generated_name {
                t.checks[i].name = self.free_generated(&t.checks[i].name, &names);
            }
            let c = &t.checks[i];
            if self.name_taken(&c.name)
                || names.iter().any(|n| n.eq_ignore_ascii_case(&c.name))
                || c.name.eq_ignore_ascii_case(&t.name)
            {
                return Err(dup(&c.name));
            }
            names.push(c.name.clone());
        }
        if let Some(r) = &t.row_deletion_policy {
            rdp_check(&t, r)?;
        }
        if let Some(s) = &t.synonym
            && (self.name_taken(s) || s.eq_ignore_ascii_case(&t.name))
        {
            return Err(dup(s));
        }
        let k = key(&t.name);
        self.tables.insert(k.clone(), t);
        self.push_order(ObjectKind::Table, k);
        Ok(())
    }

    fn drop_table(&mut self, name: &str, if_exists: bool) -> R<()> {
        let Some(k) = self.table_key(name) else {
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
        self.tables.remove(&k);
        self.drop_order(ObjectKind::Table, &k);
        Ok(())
    }

    fn alter_table(&mut self, table: &str, action: &AlterTable) -> R<()> {
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
                if t.column(&column.name).is_some() {
                    if *if_not_exists {
                        return Ok(());
                    }
                    return Err(pre(format!(
                        "Duplicate column name {}.{}.",
                        tname, column.name
                    )));
                }
                validate_column(&tname, column)?;
                self.table_mut(&k)?.columns.push(column.clone());
            }
            AlterTable::DropColumn { name } => {
                let Some(c) = t.column(name) else {
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
                    return Err(pre(format!(
                        "Cannot drop column {tname}.{cname} . Column {tname}.{cname} is tracked by {}. Columns explicitly tracked by a Change Stream cannot be dropped. Please alter the Change Stream to stop tracking the column before dropping the column.",
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
            }
            AlterTable::AlterColumn { column } => {
                let Some(old) = t.column(&column.name) else {
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
                validate_column(&tname, column)?;
                let mut new = column.clone();
                new.name = old.name.clone();
                if !new.allow_commit_timestamp && new.ty == ColumnType::Timestamp {
                    new.allow_commit_timestamp = old.allow_commit_timestamp;
                }
                if new.generated.is_none() && old.generated.is_some() {
                    new.generated = old.generated.clone();
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
                if t.column(name).is_none() {
                    return Err(col_not_found(name));
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
            AlterTable::SetColumnOptions {
                name,
                allow_commit_timestamp,
            } => {
                let Some(c) = t.column(name) else {
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
                if let Some(v) = allow_commit_timestamp {
                    let t = self.table_mut(&k)?;
                    if let Some(c) = t
                        .columns
                        .iter_mut()
                        .find(|c| c.name.eq_ignore_ascii_case(name))
                    {
                        c.allow_commit_timestamp = *v;
                    }
                }
            }
            AlterTable::AddForeignKey(fk) => {
                let mut fk = fk.clone();
                if fk.generated_name {
                    fk.name = self.free_generated(&fk.name, &[]);
                } else if self.name_taken(&fk.name) {
                    return Err(dup(&fk.name));
                }
                self.validate_fk(t, &fk)?;
                self.table_mut(&k)?.foreign_keys.push(fk);
            }
            AlterTable::AddCheck(c) => {
                let mut c = c.clone();
                if c.generated_name {
                    c.name = self.free_generated(&c.name, &[]);
                } else if self.name_taken(&c.name) {
                    return Err(dup(&c.name));
                }
                self.table_mut(&k)?.checks.push(c);
            }
            AlterTable::DropConstraint { name } => {
                let has = t
                    .foreign_keys
                    .iter()
                    .any(|f| f.name.eq_ignore_ascii_case(name))
                    || t.checks.iter().any(|c| c.name.eq_ignore_ascii_case(name));
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
                if t.interleave.is_none() {
                    return Err(inv(format!(
                        "Cannot SET ON DELETE on table {tname} that does not have an INTERLEAVE clause."
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
                rdp_check(t, r)?;
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
                rdp_check(t, r)?;
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
                self.add_synonym(&k, s)?;
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
                self.rename_table(&k, to)?;
            }
            AlterTable::RenameWithSynonym { to, synonym } => {
                if let Some(s) = &t.synonym {
                    return Err(inv(format!(
                        "Cannot add a synonym to table {tname} because the table already has a synonym {s}."
                    )));
                }
                if !synonym.eq_ignore_ascii_case(&tname) && self.name_taken(synonym) {
                    return Err(dup(synonym));
                }
                let new_key = self.rename_table(&k, to)?;
                self.add_synonym(&new_key, synonym)?;
            }
        }
        Ok(())
    }

    fn table_mut(&mut self, k: &str) -> R<&mut Table> {
        self.tables.get_mut(k).ok_or_else(|| table_not_found(k))
    }

    fn add_synonym(&mut self, k: &str, s: &str) -> R<()> {
        let t = &self.tables[k];
        if let Some(old) = &t.synonym {
            return Err(inv(format!(
                "Cannot add a synonym to table {} because the table already has a synonym {old}.",
                t.name
            )));
        }
        if self.name_taken(s) {
            return Err(dup(s));
        }
        self.table_mut(k)?.synonym = Some(s.to_string());
        Ok(())
    }

    /// Renames a table and every reference to it; returns the new key.
    fn rename_table(&mut self, k: &str, to: &str) -> R<String> {
        let old_name = self.tables[k].name.clone();
        let synonym_is_target = self.tables[k]
            .synonym
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(to));
        if !synonym_is_target {
            self.check_new_name(to)?;
        }
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
        Ok(new_key)
    }

    fn create_index(&mut self, index: &Index, if_not_exists: bool) -> R<()> {
        if if_not_exists && self.indexes.contains_key(&key(&index.name)) {
            return Ok(());
        }
        let tk = self.need_table(&index.table)?;
        self.check_new_name(&index.name)?;
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
            inv(format!(
                "Index {n} specifies key column {c} which does not exist in the index's base table."
            ))
        };
        for (i, kp) in index.key.iter().enumerate() {
            let Some(c) = t.column(&kp.column) else {
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
            let Some(c) = t.column(s) else {
                return Err(missing(s));
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
        self.indexes.insert(key(&index.name), index.clone());
        Ok(())
    }

    fn create_search_index(&mut self, index: &SearchIndex) -> R<()> {
        let tk = self.need_table(&index.table)?;
        self.check_new_name(&index.name)?;
        let t = &self.tables[&tk];
        let n = &index.name;
        for c in index
            .columns
            .iter()
            .chain(&index.storing)
            .chain(&index.partition_by)
        {
            if t.column(c).is_none() {
                return Err(inv(format!(
                    "Index {n} specifies key column {c} which does not exist in the index's base table."
                )));
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
        self.search_indexes.insert(key(&index.name), index.clone());
        Ok(())
    }

    fn alter_index(&mut self, name: &str, action: &AlterIndex, emulator_compat: bool) -> R<()> {
        let k = key(name);
        let Some(idx) = self.indexes.get(&k) else {
            return Err(DdlError::NotFound(format!("Index not found: {name}")));
        };
        let t = self
            .resolve_table(&idx.table)
            .ok_or_else(|| table_not_found(&idx.table))?;
        match action {
            AlterIndex::AddStoredColumn(c) => {
                let Some(col) = t.column(c) else {
                    return Err(DdlError::NotFound(format!(
                        "Column not found in table {}: {c}",
                        t.name
                    )));
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

    fn opaque_stmt(&mut self, object: &OpaqueObject, drop: bool) -> R<()> {
        let kind = object.kind.as_str();
        let name = object.name.as_str();
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
            self.opaque.remove(i);
            return Ok(());
        }
        match kind {
            "CHANGE STREAM" => {
                self.check_new_name(name)?;
                let cs = ChangeStream::parse(&object.ddl)?;
                self.validate_change_stream(&cs)?;
                self.opaque.push(object.clone());
            }
            "ALTER CHANGE STREAM" => {
                let Some(i) = self.opaque_find("CHANGE STREAM", name) else {
                    return Err(DdlError::NotFound(format!(
                        "Change Stream not found: {name}"
                    )));
                };
                let mut cs = ChangeStream::parse(&self.opaque[i].ddl)?;
                cs.alter(&object.ddl)?;
                self.validate_change_stream(&cs)?;
                self.opaque[i].ddl = cs.to_ddl();
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
                self.check_new_name(name)?;
                self.opaque.push(OpaqueObject {
                    kind: kind.to_string(),
                    name: name.to_string(),
                    ddl,
                });
            }
            "ANALYZE" => {}
            _ => self.opaque.push(object.clone()),
        }
        Ok(())
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

    fn validate_change_stream(&self, cs: &ChangeStream) -> R<()> {
        if let CsTarget::Tables(ts) = &cs.target {
            for (t, cols) in ts {
                let Some(table) = self.resolve_table(t) else {
                    return Err(pre(format!(
                        "Change Stream {} cannot track {t} because either Change Streams do not track database objects of this type, or this Table does not exist.",
                        cs.name
                    )));
                };
                for c in cols.iter().flatten() {
                    if table.column(c).is_none() {
                        return Err(pre(format!(
                            "Change Stream {} cannot track column {c} because it does not exist in Table {}.",
                            cs.name, table.name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn apply_inner(&mut self, stmt: &Statement) -> R<()> {
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
                        Err(dup(name))
                    };
                }
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
            } => self.create_table(table, *if_not_exists)?,
            Statement::DropTable { name, if_exists } => self.drop_table(name, *if_exists)?,
            Statement::AlterTable { table, action } => self.alter_table(table, action)?,
            Statement::RenameTables { renames } => {
                let mut next = self.clone();
                for (from, to) in renames {
                    let k = next.need_table(from)?;
                    next.rename_table(&k, to)?;
                }
                let version = self.version;
                *self = next;
                self.version = version;
            }
            Statement::CreateIndex {
                index,
                if_not_exists,
            } => self.create_index(index, *if_not_exists)?,
            Statement::DropIndex { name, if_exists } => {
                if self.indexes.remove(&key(name)).is_none() && !if_exists {
                    return Err(DdlError::NotFound(format!("Index not found: {name}")));
                }
            }
            Statement::AlterIndex { name, action } => self.alter_index(name, action, false)?,
            Statement::CreateSearchIndex { index } => self.create_search_index(index)?,
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
                self.check_new_name(&sequence.name)?;
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
                if self.sequences.remove(&k).is_none() {
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
                        return Err(dup(&view.name));
                    }
                    self.views.insert(k, view.clone());
                } else {
                    self.check_new_name(&view.name)?;
                    self.views.insert(k.clone(), view.clone());
                    self.push_order(ObjectKind::View, k);
                }
            }
            Statement::DropView { name, if_exists } => {
                let k = key(name);
                if self.views.remove(&k).is_none() {
                    if *if_exists {
                        return Ok(());
                    }
                    return Err(DdlError::NotFound(format!("View not found: {name}")));
                }
                self.drop_order(ObjectKind::View, &k);
            }
            Statement::AlterDatabase { options, name } => {
                self.database_name = Some(name.clone());
                merge_options(&mut self.options, options);
            }
            Statement::Opaque { object, drop } => self.opaque_stmt(object, *drop)?,
        }
        Ok(())
    }

    /// Apply a parsed statement, validating it against the current schema. On error the schema
    /// is unchanged.
    pub fn apply(&mut self, stmt: &Statement) -> Result<(), DdlError> {
        self.apply_inner(stmt)?;
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
            _ => self.apply(stmt),
        }
    }

    /// Checks on existing rows that `stmt` needs before it can be applied; call before `apply`.
    pub fn data_checks(&self, stmt: &Statement) -> Vec<DataCheck> {
        let mut out = Vec::new();
        match stmt {
            Statement::AlterTable { table, action } => {
                let Some(t) = self.resolve_table(table) else {
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
                        let Some(old) = t.column(&column.name) else {
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
