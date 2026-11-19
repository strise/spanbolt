//! The seam where SQL execution plugs in. The server resolves sessions and transactions
//! (including inline `begin`) and hands the engine a transaction view.

use std::sync::Arc;

use spanbolt_proto::spanner as pb;
use spanbolt_txn::{Database, ReadOnlyTxn, ReadWriteTxn};
use tonic::Status;

/// The transaction a statement runs in.
pub enum SqlTxn<'a> {
    /// Single-use or read-only transaction: a pinned version.
    ReadOnly(&'a ReadOnlyTxn),
    /// Read-write: reads go through `ReadWriteTxn::read`/`current_state` (recording the read set);
    /// DML effects through `ReadWriteTxn::apply_dml`.
    ReadWrite(&'a Arc<ReadWriteTxn>),
    /// Partitioned DML: the engine commits independent batches on the database.
    PartitionedDml,
}

/// Receives a statement's result as it is produced: the metadata once, then each row.
/// An error (the client went away) stops execution.
pub trait RowSink {
    fn metadata(&mut self, m: pb::ResultSetMetadata) -> Result<(), Status>;
    fn row(&mut self, values: Vec<prost_types::Value>) -> Result<(), Status>;
}

pub trait SqlEngine: Send + Sync + 'static {
    /// Runs a query or DML statement. The server fills `metadata.transaction` for inline begins.
    fn execute_sql(
        &self,
        db: &Arc<Database>,
        txn: SqlTxn<'_>,
        req: &pb::ExecuteSqlRequest,
    ) -> Result<pb::ResultSet, Status>;

    /// Runs a statement, handing rows to `sink` as the executor produces them. Returns the stats
    /// for the last PartialResultSet.
    fn execute_sql_into(
        &self,
        db: &Arc<Database>,
        txn: SqlTxn<'_>,
        req: &pb::ExecuteSqlRequest,
        sink: &mut dyn RowSink,
    ) -> Result<Option<pb::ResultSetStats>, Status> {
        let rs = self.execute_sql(db, txn, req)?;
        sink.metadata(rs.metadata.unwrap_or_default())?;
        for r in rs.rows {
            sink.row(r.values)?;
        }
        Ok(rs.stats)
    }

    fn execute_batch_dml(
        &self,
        db: &Arc<Database>,
        txn: &Arc<ReadWriteTxn>,
        req: &pb::ExecuteBatchDmlRequest,
    ) -> Result<pb::ExecuteBatchDmlResponse, Status>;

    /// Entry counts of the engine's caches (soak tests).
    fn cache_sizes(&self) -> Vec<(&'static str, usize)> {
        Vec::new()
    }

    fn partition_query(
        &self,
        _db: &Arc<Database>,
        _req: &pb::PartitionQueryRequest,
    ) -> Result<Vec<pb::Partition>, Status> {
        Err(not_supported("PartitionQuery"))
    }
}

fn not_supported(rpc: &str) -> Status {
    Status::unimplemented(format!(
        "{rpc} is not supported by spanbolt yet: SQL execution is not implemented. Use Read/StreamingRead and Commit mutations."
    ))
}

/// The engine used until SQL lands: every statement is UNIMPLEMENTED.
pub struct NoSql;

impl SqlEngine for NoSql {
    fn execute_sql(
        &self,
        _db: &Arc<Database>,
        _txn: SqlTxn<'_>,
        _req: &pb::ExecuteSqlRequest,
    ) -> Result<pb::ResultSet, Status> {
        Err(not_supported("ExecuteSql"))
    }

    fn execute_batch_dml(
        &self,
        _db: &Arc<Database>,
        _txn: &Arc<ReadWriteTxn>,
        _req: &pb::ExecuteBatchDmlRequest,
    ) -> Result<pb::ExecuteBatchDmlResponse, Status> {
        Err(not_supported("ExecuteBatchDml"))
    }
}

/// The GoogleSQL engine (`--features googlesql`).
#[cfg(feature = "googlesql")]
impl SqlEngine for spanbolt_sql::Engine {
    fn execute_sql(
        &self,
        db: &Arc<Database>,
        txn: SqlTxn<'_>,
        req: &pb::ExecuteSqlRequest,
    ) -> Result<pb::ResultSet, Status> {
        spanbolt_sql::Engine::execute_sql(self, db, engine_txn(txn), req)
            .map_err(crate::status::to_status)
    }

    fn execute_sql_into(
        &self,
        db: &Arc<Database>,
        txn: SqlTxn<'_>,
        req: &pb::ExecuteSqlRequest,
        sink: &mut dyn RowSink,
    ) -> Result<Option<pb::ResultSetStats>, Status> {
        /// Carries the sink's Status through the engine's error type.
        struct Adapter<'a> {
            sink: &'a mut dyn RowSink,
            failed: Option<Status>,
        }
        impl Adapter<'_> {
            fn keep(&mut self, r: Result<(), Status>) -> spanbolt_storage::Result<()> {
                r.map_err(|s| {
                    let e = spanbolt_storage::Error::internal(s.message().to_string());
                    self.failed = Some(s);
                    e
                })
            }
        }
        impl spanbolt_sql::RowSink for Adapter<'_> {
            fn metadata(&mut self, m: pb::ResultSetMetadata) -> spanbolt_storage::Result<()> {
                let r = self.sink.metadata(m);
                self.keep(r)
            }
            fn row(&mut self, values: Vec<prost_types::Value>) -> spanbolt_storage::Result<()> {
                let r = self.sink.row(values);
                self.keep(r)
            }
        }
        let mut a = Adapter { sink, failed: None };
        let r = spanbolt_sql::Engine::execute_sql_into(self, db, engine_txn(txn), req, &mut a);
        match (r, a.failed) {
            (Err(_), Some(s)) => Err(s),
            (r, _) => r.map_err(crate::status::to_status),
        }
    }

    fn execute_batch_dml(
        &self,
        db: &Arc<Database>,
        txn: &Arc<ReadWriteTxn>,
        req: &pb::ExecuteBatchDmlRequest,
    ) -> Result<pb::ExecuteBatchDmlResponse, Status> {
        spanbolt_sql::Engine::execute_batch_dml(self, db, txn, req)
            .map_err(crate::status::to_status)
    }

    fn partition_query(
        &self,
        db: &Arc<Database>,
        req: &pb::PartitionQueryRequest,
    ) -> Result<Vec<pb::Partition>, Status> {
        spanbolt_sql::Engine::partition_query(self, db, req).map_err(crate::status::to_status)
    }

    fn cache_sizes(&self) -> Vec<(&'static str, usize)> {
        spanbolt_sql::Engine::cache_sizes(self)
    }
}

#[cfg(feature = "googlesql")]
fn engine_txn(txn: SqlTxn<'_>) -> spanbolt_sql::Txn<'_> {
    match txn {
        SqlTxn::ReadOnly(t) => spanbolt_sql::Txn::ReadOnly(t),
        SqlTxn::ReadWrite(t) => spanbolt_sql::Txn::ReadWrite(t),
        SqlTxn::PartitionedDml => spanbolt_sql::Txn::PartitionedDml,
    }
}

/// The default engine: GoogleSQL when built with the `googlesql` feature.
pub fn default_engine(emulator_compat: bool) -> Arc<dyn SqlEngine> {
    #[cfg(feature = "googlesql")]
    {
        // DEFAULT values, stored generated columns and CHECK constraints evaluate through SQL.
        if let Ok(ev) =
            spanbolt_sql::SchemaExprs::with_options(spanbolt_sql::Options { emulator_compat })
        {
            spanbolt_storage::expr::set_expr_evaluator(Some(Arc::new(ev)));
        }
        Arc::new(spanbolt_sql::Engine::new(spanbolt_sql::Options {
            emulator_compat,
        }))
    }
    #[cfg(not(feature = "googlesql"))]
    {
        let _ = emulator_compat;
        Arc::new(NoSql)
    }
}
