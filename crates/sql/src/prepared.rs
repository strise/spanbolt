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
