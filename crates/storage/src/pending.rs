//! Reads of cells written with `PENDING_COMMIT_TIMESTAMP()` earlier in the same read-write
//! transaction. Their value is the commit timestamp, unknown until commit, so Spanner rejects
//! reading the column, and an index keyed on it, until then.

use crate::catalog::{Catalog, IndexId, TableId};
use crate::error::{Error, Result};

/// The error for reading `subject` (`Column C` or `Table <index table>`) while it is pending.
pub fn pending_read_error(subject: &str, prod_messages: bool) -> Error {
    Error::invalid_argument(if prod_messages {
        format!(
            "{subject} cannot be accessed because 1. it, or its associated index, has a pending CommitTimestamp, or 2. change stream tracking {subject} can't access the pending commit timestamp to record the old value."
        )
    } else {
        format!(
            "{subject} cannot be accessed because it, or its associated index, has a pending CommitTimestamp"
        )
    })
}

/// The error for reading column `c` of `table`.
pub fn pending_column_error(cat: &Catalog, table: TableId, c: usize, prod: bool) -> Error {
    pending_read_error(
        &format!("Column {}", cat.table(table).columns[c].name),
        prod,
    )
}

/// Rejects a read of `columns` of `table` (through `index`, when given) that touches a pending
/// `(table, column)`. An index is unreadable when a pending column is part of its key.
pub fn check_pending_read(
    cat: &Catalog,
    pending: &[(TableId, usize)],
    table: TableId,
    index: Option<IndexId>,
    columns: &[usize],
    prod: bool,
) -> Result<()> {
    let is_pending = |c: usize| pending.contains(&(table, c));
    if let Some(ix) = index {
        let im = cat.index(ix);
        if im.key.iter().any(|&c| is_pending(c)) {
            let name = if prod {
                format!("_Index_{}", im.name)
            } else {
                format!("_index_data_table_{}", im.name)
            };
            return Err(pending_read_error(&format!("Table {name}"), prod));
        }
    }
    match columns.iter().find(|&&c| is_pending(c)) {
        Some(&c) => Err(pending_column_error(cat, table, c, prod)),
        None => Ok(()),
    }
}
