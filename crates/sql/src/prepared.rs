//! A statement analysed, validated and planned once per (schema, SQL, parameter types).

use crate::dml::DmlPlan;
use crate::exec::BoxOp;
use crate::plan::Param;
use crate::types::Type;

pub enum Body {
    Query {
        op: BoxOp,
        /// Positions of the output columns in the plan's rows.
        positions: Vec<usize>,
    },
    Dml(DmlPlan),
}

pub struct Prepared {
    pub body: Body,
    pub params: Vec<Param>,
    /// Result columns (query output or THEN RETURN).
    pub fields: Vec<(String, Type)>,
    pub search: SearchUse,
    /// Subquery plans, for `explain`.
    pub subplans: Vec<(String, std::sync::Arc<BoxOp>)>,
    /// The base-table columns the statement reads, for the pending commit timestamp check.
    pub reads: Vec<ScanRead>,
}

/// The columns one table scan reads, and the index it is forced through.
#[derive(Debug, Clone)]
pub struct ScanRead {
    pub table: spanbolt_storage::TableId,
    pub index: Option<spanbolt_storage::IndexId>,
    pub columns: Vec<usize>,
}

impl Prepared {
    /// Rejects the statement in a read-write transaction whose DML wrote, with
    /// `PENDING_COMMIT_TIMESTAMP()`, a column it reads (or the key of an index it is forced
    /// through, outside `--emulator-compat`).
    pub fn check_pending_reads(
        &self,
        rw: &spanbolt_txn::ReadWriteTxn,
        cat: &spanbolt_storage::Catalog,
        emulator_compat: bool,
    ) -> spanbolt_storage::Result<()> {
        if self.reads.is_empty() {
            return Ok(());
        }
        let pending = rw.pending_commit_ts();
        if pending.is_empty() {
            return Ok(());
        }
        for r in &self.reads {
            // The emulator only checks the columns a query reads.
            let index = r.index.filter(|_| !emulator_compat);
            spanbolt_storage::pending::check_pending_read(
                cat,
                &pending,
                r.table,
                index,
                &r.columns,
                !emulator_compat,
            )?;
        }
        Ok(())
    }
}

/// What the emulator restricts in read-write transactions and partitioned queries.
#[derive(Default, Clone)]
pub struct SearchUse {
    /// The first of SEARCH, SEARCH_SUBSTRING, SCORE or SNIPPET the statement calls (lower case).
    pub function: Option<String>,
    /// `--emulator-compat`: the emulator's error messages.
    pub emulator: bool,
    /// A search index named in FORCE_INDEX.
    pub forced_index: Option<String>,
    /// The `allow_search_indexes_in_transaction` statement hint.
    pub allowed_in_txn: bool,
}

impl SearchUse {
    pub fn check_read_write(&self) -> spanbolt_storage::Result<()> {
        if self.allowed_in_txn {
            return Ok(());
        }
        let message = match (&self.forced_index, &self.function) {
            (Some(i), _) if self.emulator => format!(
                "The index {i} cannot be used because it is a SEARCH index type which is not supported for transactional queries by default"
            ),
            (Some(i), _) => {
                format!("The search index {i} cannot be used in transactional queries by default.")
            }
            (None, Some(_)) if self.emulator => "Invalid use of search related function: SQL Search functions are not supported for transactional queries by default".to_string(),
            (None, Some(f)) => format!(
                "{} cannot be used in transactional queries by default.",
                f.to_ascii_uppercase()
            ),
            (None, None) => return Ok(()),
        };
        Err(spanbolt_storage::Error::invalid_argument(message))
    }

    pub fn check_partitioned(&self) -> spanbolt_storage::Result<()> {
        if let Some(i) = &self.forced_index {
            return Err(spanbolt_storage::Error::invalid_argument(format!(
                "The index {i} cannot be used because it is a SEARCH index type which is not supported for partitioned queries"
            )));
        }
        if self.function.is_some() {
            return Err(spanbolt_storage::Error::invalid_argument(
                "Invalid use of search related function: SQL Search functions are not supported for partitioned queries",
            ));
        }
        Ok(())
    }
}
