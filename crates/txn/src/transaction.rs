use std::sync::Arc;

use parking_lot::Mutex;
use spanbolt_storage::{
    Catalog, DbState, Error, KeySet, Mutation, Result, Span, TableId, Target, Timestamp, Writer,
};

use crate::database::{CommitInput, CommitOutcome, Database, ReadSet, Version};

/// Checks the `(table, column)`s pending a commit timestamp after a DML statement.
pub type PendingCheck<'a> = dyn Fn(&[(TableId, usize)]) -> Result<()> + 'a;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Active,
    Committed,
    RolledBack,
    Aborted,
    /// A DML statement failed with a constraint error; every later use returns that error.
    Invalid,
}

struct Inner {
    status: Status,
    /// Pinned at the first read, so blind-write transactions never conflict on reads.
    snapshot: Option<Arc<Version>>,
    /// Snapshot plus this transaction's DML, for read-your-writes.
    local: Option<Arc<DbState>>,
    reads: ReadSet,
    dml: Vec<Mutation>,
    /// `(table, column)`s DML wrote with the commit timestamp: unreadable until commit.
    pending: Vec<(TableId, usize)>,
    commit: Option<CommitOutcome>,
    /// The constraint error that invalidated the transaction (`Status::Invalid`).
    invalid: Option<Error>,
}

/// A read-write transaction: optimistic, first committer wins.
pub struct ReadWriteTxn {
    db: Arc<Database>,
    inner: Mutex<Inner>,
}

/// The state a read sees, with the spans it covers already recorded in the read set.
pub struct ReadView {
    pub state: Arc<DbState>,
    pub target: Target,
    pub spans: Vec<Span>,
    /// Snapshot timestamp (for read-only transactions and stale reads).
    pub ts: Timestamp,
}

impl ReadWriteTxn {
    pub fn new(db: Arc<Database>) -> ReadWriteTxn {
        ReadWriteTxn {
            db,
            inner: Mutex::new(Inner {
                status: Status::Active,
                snapshot: None,
                local: None,
                reads: ReadSet::default(),
                dml: Vec::new(),
                pending: Vec::new(),
                commit: None,
                invalid: None,
            }),
        }
    }

    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    fn active(inner: &Inner) -> Result<()> {
        match inner.status {
            Status::Active => Ok(()),
            Status::Committed => Err(Error::failed_precondition(
                "Cannot use a transaction after it has been committed.",
            )),
            Status::RolledBack => Err(Error::failed_precondition(
                "Cannot use a transaction after it has been rolled back.",
            )),
            Status::Aborted => Err(crate::database::aborted()),
            Status::Invalid => Err(inner
                .invalid
                .clone()
                .unwrap_or_else(|| Error::internal("transaction invalidated"))),
        }
    }

    /// Records a failed DML statement. Constraint errors invalidate the transaction, as in
    /// production Spanner and the emulator: later reads, queries and the commit return that
    /// error, so nothing the transaction wrote is committed. Other errors (analysis, runtime)
    /// leave the transaction usable.
    pub fn dml_failed(&self, e: &Error) {
        if !e.constraint {
            return;
        }
        let mut inner = self.inner.lock();
        if inner.status == Status::Active {
            inner.status = Status::Invalid;
            // The emulator keeps code and message only, without the ConstraintError detail.
            inner.invalid = Some(Error::new(e.code, e.message.clone()));
        }
    }

    fn view(&self, inner: &mut Inner) -> (Arc<DbState>, Timestamp) {
        let snap = inner
            .snapshot
            .get_or_insert_with(|| self.db.latest())
            .clone();
        match &inner.local {
            Some(l) => (l.clone(), snap.ts),
            None => (snap.state.clone(), snap.ts),
        }
    }

    /// Resolves `keys` on `target` against the transaction's view and records the read.
    pub fn read(
        &self,
        resolve: impl FnOnce(&Catalog) -> Result<(Target, KeySet)>,
    ) -> Result<ReadView> {
        let mut inner = self.inner.lock();
        Self::active(&inner)?;
        let (state, ts) = self.view(&mut inner);
        let (target, keys) = resolve(&state.catalog)?;
        let spans = state.spans(target, &keys)?;
        inner.reads.record(target, &spans);
        Ok(ReadView {
            state,
            target,
            spans,
            ts,
        })
    }

    /// The current view without recording a read (SQL engines record what they scan).
    pub fn current_state(&self) -> Result<Arc<DbState>> {
        let mut inner = self.inner.lock();
        Self::active(&inner)?;
        Ok(self.view(&mut inner).0)
    }

    pub fn record_read(&self, target: Target, spans: &[Span]) {
        self.inner.lock().reads.record(target, spans);
    }

    /// Applies DML effects to the transaction's private view; they are re-applied at commit.
    pub fn apply_dml(&self, ops: Vec<Mutation>) -> Result<()> {
        self.apply_dml_checked(ops, &|_| Ok(()))
    }

    /// Like [`Self::apply_dml`], but first hands `check` the `(table, column)`s that would be
    /// pending a commit timestamp afterwards; an error leaves the transaction as it was.
    pub fn apply_dml_checked(&self, ops: Vec<Mutation>, check: &PendingCheck) -> Result<()> {
        let mut inner = self.inner.lock();
        Self::active(&inner)?;
        let (state, ts) = self.view(&mut inner);
        // The commit timestamp isn't chosen yet; it will exceed everything the clock handed out.
        let clock = self.db.clock();
        let mut w = Writer::new(&state, ts)
            .with_max_ts(clock.write_horizon())
            .with_now(clock.now());
        for m in &ops {
            w.apply(m)?;
        }
        let mut pending = inner.pending.clone();
        for &p in w.pending_commit_ts() {
            if !pending.contains(&p) {
                pending.push(p);
            }
        }
        let (next, written) = w.finish()?;
        check(&pending)?;
        inner.pending = pending;
        inner.local = Some(Arc::new(next));
        // DML reads the rows it changes.
        for (t, k) in written {
            inner.reads.record(t, &[Span::Point(k)]);
        }
        inner.dml.extend(ops);
        Ok(())
    }

    /// The `(table, column)`s this transaction's DML wrote with `PENDING_COMMIT_TIMESTAMP()`.
    /// Spanner rejects reading them before commit.
    pub fn pending_commit_ts(&self) -> Vec<(TableId, usize)> {
        self.inner.lock().pending.clone()
    }

    /// Commits DML effects plus `mutations`, which are resolved against `catalog`.
    pub fn commit(&self, catalog: &Arc<Catalog>, mutations: &[Mutation]) -> Result<CommitOutcome> {
        let mut inner = self.inner.lock();
        if inner.status == Status::Committed
            && let Some(c) = inner.commit
        {
            return Ok(c);
        }
        Self::active(&inner)?;
        let all: Vec<Mutation>;
        let muts = if inner.dml.is_empty() {
            mutations
        } else {
            all = inner.dml.iter().chain(mutations).cloned().collect();
            &all
        };
        let result = self.db.commit(CommitInput {
            read_ts: inner.snapshot.as_ref().map(|s| s.ts),
            reads: &inner.reads,
            catalog,
            mutations: muts,
        });
        match &result {
            Ok(c) => {
                inner.status = Status::Committed;
                inner.commit = Some(*c);
            }
            Err(e) if e.code == spanbolt_storage::Code::Aborted => inner.status = Status::Aborted,
            Err(_) => {}
        }
        result
    }

    /// The catalog mutations should be resolved against: the snapshot's if pinned, else the latest.
    pub fn catalog(&self) -> Arc<Catalog> {
        let inner = self.inner.lock();
        match &inner.snapshot {
            Some(s) => s.state.catalog.clone(),
            None => self.db.latest().state.catalog.clone(),
        }
    }

    pub fn rollback(&self) {
        let mut inner = self.inner.lock();
        if inner.status == Status::Active {
            inner.status = Status::RolledBack;
        }
    }

    pub fn is_active(&self) -> bool {
        self.inner.lock().status == Status::Active
    }
}

/// A read-only transaction or single read: a pinned version, no locks.
#[derive(Clone)]
pub struct ReadOnlyTxn {
    pub version: Arc<Version>,
}

impl ReadOnlyTxn {
    pub fn read(
        &self,
        resolve: impl FnOnce(&Catalog) -> Result<(Target, KeySet)>,
    ) -> Result<ReadView> {
        let state = self.version.state.clone();
        let (target, keys) = resolve(&state.catalog)?;
        let spans = state.spans(target, &keys)?;
        Ok(ReadView {
            state,
            target,
            spans,
            ts: self.version.ts,
        })
    }
}
