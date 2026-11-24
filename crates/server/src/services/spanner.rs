use std::sync::Arc;

use prost::bytes::Bytes;
use spanbolt_proto::spanner as pb;
use spanbolt_proto::spanner::spanner_server::Spanner;
use spanbolt_storage::{Catalog, Error, KeySet, Mutation, Target, WriteKind, scan};
use spanbolt_txn::{ReadOnlyTxn, ReadView, ReadWriteTxn, TimestampBound};
use tonic::{Request, Response, Status};

use crate::convert::{self, spanner_type};
use crate::emulator::{DatabaseEntry, Emulator, Session, TxnKind};
use crate::sql::{RowSink, SqlTxn};
use crate::status::{to_status, unimplemented};
use crate::stream::{self, PartialWriter, ResultStream, ResumeToken};
use crate::timing;

pub struct SpannerService {
    pub emu: Arc<Emulator>,
}

const NOT_READ_WRITE: &str =
    "Cannot commit or rollback a read-only or partitioned-dml transaction.";

/// The transaction a read or query runs in, after resolving its selector.
enum Reader {
    ReadOnly(ReadOnlyTxn),
    ReadWrite(Arc<ReadWriteTxn>),
    PartitionedDml,
}

struct Resolved {
    reader: Reader,
    /// Set when the selector began a transaction inline; returned in the result metadata.
    begun: Option<pb::Transaction>,
    /// The snapshot of a single-use read-only read, which a resume token must pin.
    read_ts: Option<spanbolt_storage::Timestamp>,
}

fn bound(ro: &pb::transaction_options::ReadOnly) -> Result<TimestampBound, Status> {
    use pb::transaction_options::read_only::TimestampBound as B;
    Ok(match &ro.timestamp_bound {
        None | Some(B::Strong(_)) => TimestampBound::Strong,
        Some(B::MinReadTimestamp(t)) => TimestampBound::MinReadTimestamp(convert::timestamp(t)?),
        Some(B::ReadTimestamp(t)) => TimestampBound::ReadTimestamp(convert::timestamp(t)?),
        Some(B::MaxStaleness(d)) => TimestampBound::MaxStaleness(convert::duration(d)?),
        Some(B::ExactStaleness(d)) => TimestampBound::ExactStaleness(convert::duration(d)?),
    })
}

fn precommit(
    session: &Session,
    emu: &Emulator,
    id: &[u8],
) -> Option<pb::MultiplexedSessionPrecommitToken> {
    session
        .multiplexed
        .then(|| pb::MultiplexedSessionPrecommitToken {
            precommit_token: id.to_vec(),
            seq_num: emu.next_id() as i32,
        })
}

fn session_proto(emu: &Emulator, s: &Session) -> pb::Session {
    pb::Session {
        name: s.name.clone(),
        labels: s.labels.clone(),
        create_time: Some(s.create_time.to_proto()),
        approximate_last_use_time: Some(emu.last_use_time(s).to_proto()),
        creator_role: s.creator_role.clone(),
        multiplexed: s.multiplexed,
    }
}

impl SpannerService {
    fn new_session(&self, entry: &DatabaseEntry, template: Option<&pb::Session>) -> Arc<Session> {
        let t = template.cloned().unwrap_or_default();
        self.emu.add_session(Session {
            name: format!("{}/sessions/{}", entry.name, self.emu.opaque_id()),
            database: entry.name.clone(),
            db: entry.db.clone(),
            multiplexed: t.multiplexed,
            create_time: self.emu.clock.now(),
            labels: t.labels,
            creator_role: t.creator_role,
            current_txn: parking_lot::Mutex::new(None),
            last_used: Default::default(),
        })
    }

    fn begin(
        &self,
        session: &Session,
        opts: &pb::TransactionOptions,
    ) -> Result<(Reader, pb::Transaction), Status> {
        use pb::transaction_options::Mode;
        match &opts.mode {
            Some(Mode::ReadWrite(_)) => {
                let txn = Arc::new(ReadWriteTxn::new(session.db.clone()));
                let id = self
                    .emu
                    .add_txn(session, TxnKind::ReadWrite(txn.clone()))
                    .to_be_bytes()
                    .to_vec();
                let precommit_token = precommit(session, &self.emu, &id);
                Ok((
                    Reader::ReadWrite(txn),
                    pb::Transaction {
                        id,
                        precommit_token,
                        ..Default::default()
                    },
                ))
            }
            Some(Mode::ReadOnly(ro)) => {
                let bound = bound(ro)?;
                let single_use_only = match bound {
                    TimestampBound::MaxStaleness(_) => Some("max_staleness"),
                    TimestampBound::MinReadTimestamp(_) => Some("min_read_timestamp"),
                    _ => None,
                };
                if let Some(b) = single_use_only {
                    return Err(Status::invalid_argument(format!(
                        "Timestamp bound: {b} for read can be only be used for a single use transaction."
                    )));
                }
                let version = session.db.snapshot(bound).map_err(to_status)?;
                let read_timestamp = ro.return_read_timestamp.then(|| version.ts.to_proto());
                let txn = ReadOnlyTxn { version };
                let id = self
                    .emu
                    .add_txn(session, TxnKind::ReadOnly(txn.clone()))
                    .to_be_bytes()
                    .to_vec();
                Ok((
                    Reader::ReadOnly(txn),
                    pb::Transaction {
                        id,
                        read_timestamp,
                        ..Default::default()
                    },
                ))
            }
            Some(Mode::PartitionedDml(_)) => {
                let id = self
                    .emu
                    .add_txn(session, TxnKind::PartitionedDml)
                    .to_be_bytes()
                    .to_vec();
                Ok((
                    Reader::PartitionedDml,
                    pb::Transaction {
                        id,
                        ..Default::default()
                    },
                ))
            }
            None => Err(Status::invalid_argument(
                "Transaction options must specify a mode.",
            )),
        }
    }

    fn resolve(
        &self,
        session: &Session,
        sel: Option<&pb::TransactionSelector>,
    ) -> Result<Resolved, Status> {
        self.resolve_resumed(session, sel, None)
    }

    /// Like `resolve`; a single-use read resumed from `resume` reads the token's snapshot.
    fn resolve_resumed(
        &self,
        session: &Session,
        sel: Option<&pb::TransactionSelector>,
        resume: Option<ResumeToken>,
    ) -> Result<Resolved, Status> {
        use pb::transaction_selector::Selector;
        let pinned = resume.and_then(|t| t.read_ts);
        let single_use = |bound: TimestampBound| match pinned {
            Some(ts) => session.db.snapshot(TimestampBound::ReadTimestamp(ts)),
            None => session.db.snapshot(bound),
        };
        match sel.and_then(|s| s.selector.as_ref()) {
            None => {
                let version = single_use(TimestampBound::Strong).map_err(to_status)?;
                Ok(Resolved {
                    read_ts: Some(version.ts),
                    reader: Reader::ReadOnly(ReadOnlyTxn { version }),
                    begun: None,
                })
            }
            Some(Selector::SingleUse(opts)) => match &opts.mode {
                Some(pb::transaction_options::Mode::ReadOnly(ro)) => {
                    let version = single_use(bound(ro)?).map_err(to_status)?;
                    // Clients read the timestamp of a single-use read from the result metadata.
                    let begun = ro.return_read_timestamp.then(|| pb::Transaction {
                        read_timestamp: Some(version.ts.to_proto()),
                        ..Default::default()
                    });
                    Ok(Resolved {
                        read_ts: Some(version.ts),
                        reader: Reader::ReadOnly(ReadOnlyTxn { version }),
                        begun,
                    })
                }
                _ => Err(Status::invalid_argument(
                    "Single-use transactions for reads and queries must be read-only.",
                )),
            },
            Some(Selector::Id(id)) => {
                let entry = self.emu.txn(session, id)?;
                let reader = match &entry.kind {
                    TxnKind::ReadWrite(t) => Reader::ReadWrite(t.clone()),
                    TxnKind::ReadOnly(t) => Reader::ReadOnly(t.clone()),
                    TxnKind::PartitionedDml => Reader::PartitionedDml,
                };
                Ok(Resolved {
                    reader,
                    begun: None,
                    read_ts: None,
                })
            }
            Some(Selector::Begin(opts)) => {
                let (reader, txn) = self.begin(session, opts)?;
                Ok(Resolved {
                    reader,
                    begun: Some(txn),
                    read_ts: None,
                })
            }
        }
    }

    fn execute_read(&self, req: &pb::ReadRequest) -> Result<pb::ResultSet, Status> {
        let session = self.emu.session(&req.session)?;
        let resolved = self.resolve(&session, req.transaction.as_ref())?;
        let compat = self.emu.emulator_compat;
        let view = read_view(&resolved.reader, req, compat)?;
        let mut c = Collect::default();
        project(&view, &req.columns, req.limit, compat, &mut c)?;
        let mut metadata = c.metadata;
        if let Some(meta) = metadata.as_mut() {
            meta.transaction = resolved.begun.clone();
        }
        Ok(pb::ResultSet {
            metadata,
            rows: c.rows,
            precommit_token: read_precommit(&self.emu, &session, &resolved, req),
            ..Default::default()
        })
    }

    fn rw_for_commit(
        &self,
        session: &Session,
        txn: Option<&pb::commit_request::Transaction>,
    ) -> Result<(Arc<ReadWriteTxn>, Option<Vec<u8>>), Status> {
        use pb::commit_request::Transaction as T;
        match txn {
            Some(T::TransactionId(id)) => {
                let entry = self.emu.txn(session, id)?;
                match &entry.kind {
                    TxnKind::ReadWrite(t) => Ok((t.clone(), Some(id.clone()))),
                    TxnKind::ReadOnly(_) | TxnKind::PartitionedDml => {
                        Err(Status::failed_precondition(NOT_READ_WRITE))
                    }
                }
            }
            Some(T::SingleUseTransaction(opts)) => match &opts.mode {
                Some(pb::transaction_options::Mode::ReadWrite(_)) => {
                    Ok((Arc::new(ReadWriteTxn::new(session.db.clone())), None))
                }
                _ => Err(Status::invalid_argument(
                    "Single-use commit transactions must be read-write.",
                )),
            },
            None => Err(Status::invalid_argument("Commit requires a transaction.")),
        }
    }

    fn commit_mutations(
        &self,
        txn: &ReadWriteTxn,
        mutations: &[pb::Mutation],
    ) -> Result<spanbolt_txn::CommitOutcome, Status> {
        let catalog = txn.catalog();
        let muts = mutations
            .iter()
            .map(|m| mutation(&catalog, m, !self.emu.emulator_compat))
            .collect::<Result<Vec<_>, _>>()?;
        txn.commit(&catalog, &muts).map_err(to_status)
    }
}

fn read_target(
    cat: &Catalog,
    table: &str,
    index: &str,
    ks: Option<&pb::KeySet>,
    emulator_compat: bool,
) -> spanbolt_storage::Result<(Target, KeySet)> {
    let t = cat.lookup_table(table)?;
    if index.is_empty() {
        let tm = cat.table(t);
        let types = tm.key_types();
        return Ok((
            Target::Table(t),
            convert::keyset(ks, &types, &tm.name, !emulator_compat)?,
        ));
    }
    let ix = cat.index_id(index).ok_or_else(|| {
        Error::not_found(if emulator_compat {
            format!("No index '{index}' was found for table '{table}'.")
        } else {
            format!("Index not found on table {table}: {index}")
        })
    })?;
    let im = cat.index(ix);
    if im.table != t {
        return Err(Error::failed_precondition(format!(
            "Index '{}' indexes '{}' which does not match the given base table '{}'.",
            im.name,
            cat.table(im.table).name,
            cat.table(t).name
        )));
    }
    let tm = cat.table(t);
    let types: Vec<_> = im.key.iter().map(|&c| tm.columns[c].ty.clone()).collect();
    Ok((
        Target::Index(ix),
        convert::keyset(ks, &types, &im.name, !emulator_compat)?,
    ))
}

/// Collects a whole result (unary Read).
#[derive(Default)]
struct Collect {
    metadata: Option<pb::ResultSetMetadata>,
    rows: Vec<prost_types::ListValue>,
}

impl RowSink for Collect {
    fn metadata(&mut self, m: pb::ResultSetMetadata) -> Result<(), Status> {
        self.metadata = Some(m);
        Ok(())
    }
    fn row(&mut self, values: Vec<prost_types::Value>) -> Result<(), Status> {
        self.rows.push(prost_types::ListValue { values });
        Ok(())
    }
}

/// Resolves the read's table, index and key set in its transaction.
fn read_view(
    reader: &Reader,
    req: &pb::ReadRequest,
    emulator_compat: bool,
) -> Result<ReadView, Status> {
    let resolve = |cat: &Catalog| {
        read_target(
            cat,
            &req.table,
            &req.index,
            req.key_set.as_ref(),
            emulator_compat,
        )
    };
    match reader {
        Reader::ReadOnly(t) => t.read(resolve),
        Reader::ReadWrite(t) => t.read(resolve).and_then(|view| {
            check_pending_read(t, &view, &req.columns, emulator_compat)?;
            Ok(view)
        }),
        Reader::PartitionedDml => {
            return Err(Status::failed_precondition(
                "Partitioned DML transactions cannot be used for reads.",
            ));
        }
    }
    .map_err(to_status)
}

/// Rejects reading columns (or an index keyed on them) that DML in this transaction wrote with
/// `PENDING_COMMIT_TIMESTAMP()`.
fn check_pending_read(
    t: &ReadWriteTxn,
    view: &ReadView,
    columns: &[String],
    emulator_compat: bool,
) -> spanbolt_storage::Result<()> {
    let pending = t.pending_commit_ts();
    if pending.is_empty() {
        return Ok(());
    }
    let cat = &view.state.catalog;
    let (table, index) = match view.target {
        Target::Table(t) => (t, None),
        Target::Index(ix) => (cat.index(ix).table, Some(ix)),
    };
    let tm = cat.table(table);
    let cols: Vec<usize> = columns.iter().filter_map(|c| tm.column(c)).collect();
    spanbolt_storage::pending::check_pending_read(
        cat,
        &pending,
        table,
        index,
        &cols,
        !emulator_compat,
    )
}

/// The precommit token a read in a multiplexed session's read-write transaction returns.
fn read_precommit(
    emu: &Emulator,
    session: &Session,
    resolved: &Resolved,
    req: &pb::ReadRequest,
) -> Option<pb::MultiplexedSessionPrecommitToken> {
    if !matches!(resolved.reader, Reader::ReadWrite(_)) || !session.multiplexed {
        return None;
    }
    let id = match (&resolved.begun, &req.transaction) {
        (Some(t), _) => t.id.clone(),
        (
            None,
            Some(pb::TransactionSelector {
                selector: Some(pb::transaction_selector::Selector::Id(id)),
            }),
        ) => id.clone(),
        _ => Vec::new(),
    };
    precommit(session, emu, &id)
}

/// Column projection, metadata and `limit`, handing rows to `sink` as the scan produces them.
fn project(
    view: &ReadView,
    columns: &[String],
    limit: i64,
    emulator_compat: bool,
    sink: &mut dyn RowSink,
) -> Result<(), Status> {
    let cat = &view.state.catalog;
    let (tm, positions): (_, Vec<(usize, usize)>) = match view.target {
        Target::Table(t) => {
            let tm = cat.table(t);
            let pos = columns
                .iter()
                .map(|c| {
                    tm.column(c).map(|i| (i, i)).ok_or_else(|| {
                        Status::not_found(format!("Column not found in table {}: {c}", tm.name))
                    })
                })
                .collect::<Result<_, _>>()?;
            (tm, pos)
        }
        Target::Index(ix) => {
            let im = cat.index(ix);
            let tm = cat.table(im.table);
            let pos = columns
                .iter()
                .map(|c| match (im.column(c), tm.column(c)) {
                    (Some(p), _) => Ok((p, im.entry_columns[p])),
                    (None, Some(_)) if emulator_compat => Err(Status::not_found(format!(
                        "Index '{}' which indexes '{}' does not have a column named {c}. Reads with an index can only query columns present in the index (see https://cloud.google.com/spanner/docs/secondary-indexes#read-with-index).",
                        im.name, tm.name
                    ))),
                    (None, Some(_)) => Err(Status::unimplemented(format!(
                        "Reading non-indexed columns using an index is not supported. Consider adding {c} to the index using a STORING clause."
                    ))),
                    (None, None) => Err(Status::not_found(format!(
                        "Column not found in table {}: {c}",
                        tm.name
                    ))),
                })
                .collect::<Result<_, _>>()?;
            (tm, pos)
        }
    };
    let fields = positions
        .iter()
        .map(|&(_, tc)| pb::struct_type::Field {
            name: tm.columns[tc].name.clone(),
            r#type: Some(spanner_type(&tm.columns[tc].ty)),
        })
        .collect();
    let limit = if limit > 0 {
        limit as usize
    } else {
        usize::MAX
    };
    sink.metadata(pb::ResultSetMetadata {
        row_type: Some(pb::StructType { fields }),
        ..Default::default()
    })?;
    for (_, row) in scan(view.state.map(view.target), &view.spans).take(limit) {
        sink.row(positions.iter().map(|&(p, _)| row[p].to_proto()).collect())?;
    }
    Ok(())
}

/// `prod_messages` words key errors as production Spanner does.
fn mutation(cat: &Catalog, m: &pb::Mutation, prod_messages: bool) -> Result<Mutation, Status> {
    use pb::mutation::Operation as Op;
    let write =
        |kind, w: &pb::mutation::Write| Mutation::write(cat, kind, &w.table, &w.columns, &w.values);
    match &m.operation {
        Some(Op::Insert(w)) => write(WriteKind::Insert, w),
        Some(Op::Update(w)) => write(WriteKind::Update, w),
        Some(Op::InsertOrUpdate(w)) => write(WriteKind::InsertOrUpdate, w),
        Some(Op::Replace(w)) => write(WriteKind::Replace, w),
        Some(Op::Delete(d)) => cat.lookup_table(&d.table).and_then(|t| {
            let tm = cat.table(t);
            let ks = convert::keyset(d.key_set.as_ref(), &tm.key_types(), &tm.name, prod_messages)?;
            Mutation::delete(cat, &d.table, ks)
        }),
        Some(_) => return Err(unimplemented("Queue mutations (send/ack)")),
        None => return Err(Status::invalid_argument("Mutation has no operation.")),
    }
    .map_err(to_status)
}

fn sql_txn<'a>(r: &'a Reader) -> SqlTxn<'a> {
    match r {
        Reader::ReadOnly(t) => SqlTxn::ReadOnly(t),
        Reader::ReadWrite(t) => SqlTxn::ReadWrite(t),
        Reader::PartitionedDml => SqlTxn::PartitionedDml,
    }
}

/// A Read or StreamingRead in the timing log: `READ table(columns) [index]`.
pub(crate) fn read_label(req: &pb::ReadRequest) -> String {
    let mut s = format!("READ {}({})", req.table, req.columns.join(", "));
    if !req.index.is_empty() {
        s.push_str(" INDEX ");
        s.push_str(&req.index);
    }
    s
}

/// A commit's mutations in the timing log: each distinct `OP table`, sorted.
fn mutations_label(ms: &[pb::Mutation]) -> String {
    use pb::mutation::Operation as Op;
    let mut parts: Vec<String> = ms
        .iter()
        .filter_map(|m| {
            Some(match m.operation.as_ref()? {
                Op::Insert(w) => format!("INSERT {}", w.table),
                Op::Update(w) => format!("UPDATE {}", w.table),
                Op::InsertOrUpdate(w) => format!("INSERT_OR_UPDATE {}", w.table),
                Op::Replace(w) => format!("REPLACE {}", w.table),
                Op::Delete(d) => format!("DELETE {}", d.table),
                #[allow(unreachable_patterns)]
                _ => "OTHER".to_string(),
            })
        })
        .collect();
    parts.sort();
    parts.dedup();
    format!("COMMIT {}", parts.join(", "))
}

impl SpannerService {
    /// ExecuteStreamingSql, with each PartialResultSet already encoded (see [`crate::wire`]).
    pub(crate) async fn execute_streaming_sql_encoded(
        &self,
        req: pb::ExecuteSqlRequest,
    ) -> Result<ResultStream<Bytes>, Status> {
        let session = self.emu.session(&req.session)?;
        let resume = ResumeToken::decode(&req.resume_token)?;
        let resolved = self.resolve_resumed(&session, req.transaction.as_ref(), resume)?;
        let emu = self.emu.clone();
        stream::spawn(move |emit| {
            let mut w = PartialWriter::new(emit)
                .begun(resolved.begun)
                .resume(resume, resolved.read_ts);
            let stats =
                emu.sql
                    .execute_sql_into(&session.db, sql_txn(&resolved.reader), &req, &mut w)?;
            w.finish(stats, None)
        })
        .await
    }

    /// StreamingRead, with each PartialResultSet already encoded (see [`crate::wire`]).
    pub(crate) async fn streaming_read_encoded(
        &self,
        req: pb::ReadRequest,
    ) -> Result<ResultStream<Bytes>, Status> {
        let session = self.emu.session(&req.session)?;
        let resume = ResumeToken::decode(&req.resume_token)?;
        let resolved = self.resolve_resumed(&session, req.transaction.as_ref(), resume)?;
        // Errors in the table, index, columns or key set fail the RPC before it streams.
        let compat = self.emu.emulator_compat;
        let view = read_view(&resolved.reader, &req, compat)?;
        let precommit_token = read_precommit(&self.emu, &session, &resolved, &req);
        stream::spawn(move |emit| {
            let mut w = PartialWriter::new(emit)
                .begun(resolved.begun)
                .resume(resume, resolved.read_ts);
            project(&view, &req.columns, req.limit, compat, &mut w)?;
            w.finish(None, precommit_token)
        })
        .await
    }
}

#[tonic::async_trait]
impl Spanner for SpannerService {
    async fn create_session(
        &self,
        req: Request<pb::CreateSessionRequest>,
    ) -> Result<Response<pb::Session>, Status> {
        let req = req.into_inner();
        let entry = self.emu.database(&req.database)?;
        Ok(Response::new(session_proto(
            &self.emu,
            &self.new_session(&entry, req.session.as_ref()),
        )))
    }

    async fn batch_create_sessions(
        &self,
        req: Request<pb::BatchCreateSessionsRequest>,
    ) -> Result<Response<pb::BatchCreateSessionsResponse>, Status> {
        let req = req.into_inner();
        if req.session_count <= 0 {
            return Err(Status::invalid_argument("session_count must be positive."));
        }
        let entry = self.emu.database(&req.database)?;
        let session = (0..req.session_count.min(100))
            .map(|_| {
                session_proto(
                    &self.emu,
                    &self.new_session(&entry, req.session_template.as_ref()),
                )
            })
            .collect();
        Ok(Response::new(pb::BatchCreateSessionsResponse { session }))
    }

    async fn get_session(
        &self,
        req: Request<pb::GetSessionRequest>,
    ) -> Result<Response<pb::Session>, Status> {
        let name = &req.get_ref().name;
        crate::names::validate_session_name(name)?;
        let session = self.emu.session(name)?;
        Ok(Response::new(session_proto(&self.emu, &session)))
    }

    async fn list_sessions(
        &self,
        req: Request<pb::ListSessionsRequest>,
    ) -> Result<Response<pb::ListSessionsResponse>, Status> {
        let req = req.get_ref();
        self.emu.database(&req.database)?;
        let (sessions, next_page_token) =
            self.emu
                .sessions_page(&req.database, req.page_size, &req.page_token);
        let sessions = sessions
            .iter()
            .map(|s| session_proto(&self.emu, s))
            .collect();
        Ok(Response::new(pb::ListSessionsResponse {
            sessions,
            next_page_token,
        }))
    }

    async fn delete_session(
        &self,
        req: Request<pb::DeleteSessionRequest>,
    ) -> Result<Response<()>, Status> {
        self.emu.delete_session(&req.get_ref().name)?;
        Ok(Response::new(()))
    }

    async fn execute_sql(
        &self,
        req: Request<pb::ExecuteSqlRequest>,
    ) -> Result<Response<pb::ResultSet>, Status> {
        timing::note(req.extensions(), || req.get_ref().sql.clone(), None);
        let req = req.into_inner();
        let session = self.emu.session(&req.session)?;
        let resolved = self.resolve(&session, req.transaction.as_ref())?;
        let mut rs = self
            .emu
            .sql
            .execute_sql(&session.db, sql_txn(&resolved.reader), &req)?;
        if let Some(t) = resolved.begun {
            rs.metadata.get_or_insert_with(Default::default).transaction = Some(t);
        }
        Ok(Response::new(rs))
    }

    type ExecuteStreamingSqlStream = ResultStream<pb::PartialResultSet>;

    async fn execute_streaming_sql(
        &self,
        req: Request<pb::ExecuteSqlRequest>,
    ) -> Result<Response<Self::ExecuteStreamingSqlStream>, Status> {
        timing::note(req.extensions(), || req.get_ref().sql.clone(), None);
        let out = self.execute_streaming_sql_encoded(req.into_inner()).await?;
        Ok(Response::new(stream::decoded(out)))
    }

    async fn execute_batch_dml(
        &self,
        req: Request<pb::ExecuteBatchDmlRequest>,
    ) -> Result<Response<pb::ExecuteBatchDmlResponse>, Status> {
        timing::note(
            req.extensions(),
            || {
                let s = &req.get_ref().statements;
                s.iter()
                    .map(|s| s.sql.as_str())
                    .collect::<Vec<_>>()
                    .join(";\n")
            },
            Some(req.get_ref().statements.len() as u64),
        );
        let req = req.into_inner();
        let session = self.emu.session(&req.session)?;
        let resolved = self.resolve(&session, req.transaction.as_ref())?;
        let Reader::ReadWrite(txn) = &resolved.reader else {
            return Err(Status::failed_precondition(
                "ExecuteBatchDml requires a read-write transaction.",
            ));
        };
        let mut resp = self.emu.sql.execute_batch_dml(&session.db, txn, &req)?;
        if let (Some(t), Some(first)) = (resolved.begun, resp.result_sets.first_mut()) {
            first
                .metadata
                .get_or_insert_with(Default::default)
                .transaction = Some(t);
        }
        Ok(Response::new(resp))
    }

    async fn read(&self, req: Request<pb::ReadRequest>) -> Result<Response<pb::ResultSet>, Status> {
        timing::note(req.extensions(), || read_label(req.get_ref()), None);
        Ok(Response::new(self.execute_read(req.get_ref())?))
    }

    type StreamingReadStream = ResultStream<pb::PartialResultSet>;

    async fn streaming_read(
        &self,
        req: Request<pb::ReadRequest>,
    ) -> Result<Response<Self::StreamingReadStream>, Status> {
        timing::note(req.extensions(), || read_label(req.get_ref()), None);
        let out = self.streaming_read_encoded(req.into_inner()).await?;
        Ok(Response::new(stream::decoded(out)))
    }

    async fn begin_transaction(
        &self,
        req: Request<pb::BeginTransactionRequest>,
    ) -> Result<Response<pb::Transaction>, Status> {
        let req = req.into_inner();
        let session = self.emu.session(&req.session)?;
        let opts = req
            .options
            .ok_or_else(|| Status::invalid_argument("Missing transaction options."))?;
        Ok(Response::new(self.begin(&session, &opts)?.1))
    }

    async fn commit(
        &self,
        req: Request<pb::CommitRequest>,
    ) -> Result<Response<pb::CommitResponse>, Status> {
        timing::note(
            req.extensions(),
            || mutations_label(&req.get_ref().mutations),
            Some(req.get_ref().mutations.len() as u64),
        );
        let req = req.into_inner();
        let session = self.emu.session(&req.session)?;
        let (txn, id) = self.rw_for_commit(&session, req.transaction.as_ref())?;
        let result = self.commit_mutations(&txn, &req.mutations);
        if let Some(id) = &id {
            self.emu.end_txn(id);
        }
        let outcome = result?;
        Ok(Response::new(pb::CommitResponse {
            commit_timestamp: Some(outcome.ts.to_proto()),
            commit_stats: req
                .return_commit_stats
                .then_some(pb::commit_response::CommitStats {
                    mutation_count: outcome.mutation_count as i64,
                }),
            ..Default::default()
        }))
    }

    async fn rollback(&self, req: Request<pb::RollbackRequest>) -> Result<Response<()>, Status> {
        let req = req.into_inner();
        let session = self.emu.session(&req.session)?;
        let entry = self.emu.txn(&session, &req.transaction_id)?;
        match &entry.kind {
            TxnKind::ReadWrite(t) => t.rollback(),
            TxnKind::ReadOnly(_) | TxnKind::PartitionedDml => {
                return Err(Status::failed_precondition(NOT_READ_WRITE));
            }
        }
        self.emu.end_txn(&req.transaction_id);
        Ok(Response::new(()))
    }

    async fn partition_query(
        &self,
        req: Request<pb::PartitionQueryRequest>,
    ) -> Result<Response<pb::PartitionResponse>, Status> {
        let req = req.into_inner();
        let session = self.emu.session(&req.session)?;
        let partitions = self.emu.sql.partition_query(&session.db, &req)?;
        let resolved = self.resolve(&session, req.transaction.as_ref())?;
        Ok(Response::new(pb::PartitionResponse {
            partitions,
            transaction: resolved.begun,
        }))
    }

    async fn partition_read(
        &self,
        req: Request<pb::PartitionReadRequest>,
    ) -> Result<Response<pb::PartitionResponse>, Status> {
        let req = req.into_inner();
        let session = self.emu.session(&req.session)?;
        let resolved = self.resolve(&session, req.transaction.as_ref())?;
        let Reader::ReadOnly(txn) = &resolved.reader else {
            return Err(Status::invalid_argument(
                "Partitioned reads require a read-only transaction.",
            ));
        };
        // Validates the table, index and key set; the whole read is one partition.
        let compat = self.emu.emulator_compat;
        txn.read(|cat| read_target(cat, &req.table, &req.index, req.key_set.as_ref(), compat))
            .map_err(to_status)?;
        Ok(Response::new(pb::PartitionResponse {
            partitions: vec![pb::Partition {
                partition_token: b"spanbolt:all".to_vec(),
            }],
            transaction: resolved.begun,
        }))
    }

    type BatchWriteStream = ResultStream<pb::BatchWriteResponse>;

    async fn batch_write(
        &self,
        req: Request<pb::BatchWriteRequest>,
    ) -> Result<Response<Self::BatchWriteStream>, Status> {
        let req = req.into_inner();
        let session = self.emu.session(&req.session)?;
        let mut out = Vec::with_capacity(req.mutation_groups.len());
        for (i, group) in req.mutation_groups.iter().enumerate() {
            let txn = ReadWriteTxn::new(session.db.clone());
            let (status, commit_timestamp) = match self.commit_mutations(&txn, &group.mutations) {
                Ok(c) => (
                    spanbolt_proto::google::rpc::Status::default(),
                    Some(c.ts.to_proto()),
                ),
                Err(s) => (
                    spanbolt_proto::google::rpc::Status {
                        code: s.code() as i32,
                        message: s.message().to_string(),
                        details: Vec::new(),
                    },
                    None,
                ),
            };
            out.push(Ok(pb::BatchWriteResponse {
                indexes: vec![i as i32],
                status: Some(status),
                commit_timestamp,
            }));
        }
        Ok(Response::new(Box::pin(tokio_stream::iter(out))))
    }

    type FetchCacheUpdateStream = ResultStream<pb::CacheUpdate>;

    async fn fetch_cache_update(
        &self,
        _: Request<pb::FetchCacheUpdateRequest>,
    ) -> Result<Response<Self::FetchCacheUpdateStream>, Status> {
        Err(unimplemented("FetchCacheUpdate"))
    }
}
