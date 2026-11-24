use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use parking_lot::{Mutex, RwLock};
use spanbolt_ddl::Schema;
use spanbolt_storage::{
    Catalog, DbState, Error, Key, Mutation, Result, Span, Target, Timestamp, Writer,
};

use crate::clock::Clock;

/// Versions kept for stale reads; older ones are dropped even inside the retention period.
const MAX_VERSIONS: usize = 4096;
/// Distinct schemas kept in the stale-read history. A data commit shares almost everything with
/// the version before it, but every schema change carries a whole new schema, storage catalog and,
/// once queried, GoogleSQL catalog (~2.4 MB for the common schema). A Liquibase migration makes
/// ~1,700 of them, which held ~4 GB for the retention period. Reads from before the
/// oldest kept schema fail as garbage collected.
const MAX_SCHEMAS: usize = 16;
/// Commit write sets kept for conflict checks. Transactions that started before the oldest
/// entry abort rather than risk a missed conflict.
const MAX_LOG: usize = 16384;
/// Longest DDL text an abort message quotes.
const MAX_DDL_IN_MESSAGE: usize = 300;

/// One committed database state.
#[derive(Debug)]
pub struct Version {
    pub ts: Timestamp,
    pub state: Arc<DbState>,
}

/// How a read picks its version.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimestampBound {
    Strong,
    ReadTimestamp(Timestamp),
    MinReadTimestamp(Timestamp),
    ExactStaleness(Duration),
    MaxStaleness(Duration),
}

/// Keys and ranges a read-write transaction read, per table or index.
#[derive(Debug, Default, Clone)]
pub struct ReadSet {
    points: HashSet<(Target, Key)>,
    ranges: Vec<(Target, Span)>,
}

impl ReadSet {
    pub fn record(&mut self, target: Target, spans: &[Span]) {
        for s in spans {
            match s {
                Span::Point(k) => {
                    self.points.insert((target, k.clone()));
                }
                Span::Range { .. } => self.ranges.push((target, s.clone())),
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty() && self.ranges.is_empty()
    }

    fn hit(&self, target: Target, key: &Key) -> bool {
        self.points.contains(&(target, key.clone()))
            || self
                .ranges
                .iter()
                .any(|(t, s)| *t == target && s.contains(key))
    }
}

/// What a commit changed, for conflict checks.
enum WriteLog {
    /// The keys a data commit wrote.
    Keys(Arc<[(Target, Key)]>),
    /// A schema change, which conflicts with every older transaction; `ddl` says which, when the
    /// caller described it.
    Schema { ddl: Option<Arc<str>> },
}

/// Versions for stale reads, oldest first, with the number of distinct schemas among them.
struct History {
    versions: VecDeque<Arc<Version>>,
    schemas: usize,
}

impl History {
    fn new(version: Arc<Version>) -> History {
        History {
            versions: VecDeque::from([version]),
            schemas: 1,
        }
    }

    fn push(&mut self, version: Arc<Version>) {
        if self
            .versions
            .back()
            .is_none_or(|v| !Arc::ptr_eq(&v.state.schema, &version.state.schema))
        {
            self.schemas += 1;
        }
        self.versions.push_back(version);
    }

    fn pop_front(&mut self) {
        let Some(old) = self.versions.pop_front() else {
            return;
        };
        if self
            .versions
            .front()
            .is_none_or(|v| !Arc::ptr_eq(&v.state.schema, &old.state.schema))
        {
            self.schemas -= 1;
        }
    }
}

#[derive(Default)]
struct CommitLog {
    entries: VecDeque<(Timestamp, WriteLog)>,
    /// Set once entries have been dropped: transactions older than the first entry must abort.
    truncated: bool,
}

impl CommitLog {
    /// The abort for a transaction whose catalog predates the latest schema: names the newest
    /// schema change still in the log.
    fn schema_abort(&self) -> Error {
        self.entries
            .iter()
            .rev()
            .find_map(|(ts, e)| match e {
                WriteLog::Schema { ddl } => Some(schema_change_aborted(*ts, ddl.as_deref())),
                WriteLog::Keys(_) => None,
            })
            .unwrap_or_else(aborted)
    }
}

/// The parts of a transaction the commit needs.
pub struct CommitInput<'a> {
    /// Snapshot timestamp of the transaction's reads; `None` for blind writes.
    pub read_ts: Option<Timestamp>,
    pub reads: &'a ReadSet,
    /// The catalog the mutations were resolved against.
    pub catalog: &'a Arc<Catalog>,
    pub mutations: &'a [Mutation],
    /// How many leading `mutations` re-apply the transaction's DML, which counted towards the
    /// mutation limit statement by statement.
    pub dml_ops: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct CommitOutcome {
    pub ts: Timestamp,
    pub mutation_count: usize,
}

/// A database: the latest version (lock-free to read), recent versions for stale reads, and a
/// short per-database commit lock.
pub struct Database {
    clock: Arc<Clock>,
    latest: ArcSwap<Version>,
    history: RwLock<History>,
    history_truncated: AtomicBool,
    commit: Mutex<CommitLog>,
    abort_next: AtomicU32,
    dropped: AtomicBool,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("latest_ts", &self.latest.load().ts)
            .finish()
    }
}

pub(crate) fn aborted() -> Error {
    Error::aborted(
        "Transaction was aborted due to a conflicting concurrent transaction. Retry the transaction.",
    )
}

/// The abort for a transaction that a schema change committed at `ts` overtook, quoting the
/// change's DDL when known, so suites that apply DDL in parallel can tell which one it was.
fn schema_change_aborted(ts: Timestamp, ddl: Option<&str>) -> Error {
    let mut msg = format!(
        "Transaction was aborted due to a concurrent schema change committed at {}. Retry the \
         transaction.",
        ts.to_rfc3339()
    );
    if let Some(ddl) = ddl {
        msg.push_str(" Schema change: ");
        match ddl.char_indices().nth(MAX_DDL_IN_MESSAGE) {
            Some((cut, _)) => {
                msg.push_str(&ddl[..cut]);
                msg.push_str("...");
            }
            None => msg.push_str(ddl),
        }
    }
    Error::aborted(msg)
}

impl Database {
    pub fn new(clock: Arc<Clock>, state: DbState) -> Database {
        let ts = clock.next_commit_ts();
        Database::at_version(
            clock,
            Arc::new(Version {
                ts,
                state: Arc::new(state),
            }),
        )
    }

    /// A database whose only version is `version` (snapshot load, clone).
    pub fn at_version(clock: Arc<Clock>, version: Arc<Version>) -> Database {
        clock.observe(version.ts);
        Database {
            clock,
            latest: ArcSwap::new(version.clone()),
            history: RwLock::new(History::new(version)),
            history_truncated: AtomicBool::new(false),
            commit: Mutex::new(CommitLog::default()),
            abort_next: AtomicU32::new(0),
            dropped: AtomicBool::new(false),
        }
    }

    pub fn clock(&self) -> &Arc<Clock> {
        &self.clock
    }

    pub fn latest(&self) -> Arc<Version> {
        self.latest.load_full()
    }

    /// Copy-on-write clone: shares the latest state, O(1).
    pub fn clone_database(&self) -> Database {
        Database::at_version(self.clock.clone(), self.latest())
    }

    pub fn mark_dropped(&self) {
        self.dropped.store(true, Ordering::Release);
    }

    pub fn is_dropped(&self) -> bool {
        self.dropped.load(Ordering::Acquire)
    }

    /// Makes the next `n` commits fail with ABORTED.
    pub fn inject_aborts(&self, n: u32) {
        self.abort_next.store(n, Ordering::Release);
    }

    /// The version a read with `bound` sees.
    pub fn snapshot(&self, bound: TimestampBound) -> Result<Arc<Version>> {
        match bound {
            TimestampBound::Strong | TimestampBound::MaxStaleness(_) => Ok(self.latest()),
            TimestampBound::MinReadTimestamp(_) => Ok(self.latest()),
            TimestampBound::ReadTimestamp(ts) => self.version_at(ts),
            TimestampBound::ExactStaleness(d) => {
                let now = self.clock.now();
                let ts = Timestamp::from_nanos(now.as_nanos() - d.as_nanos() as i128);
                self.version_at(ts)
            }
        }
    }

    /// How long old versions stay readable: the latest schema's `version_retention_period`.
    pub fn version_retention(&self) -> Duration {
        self.latest().state.schema.version_retention()
    }

    /// The oldest timestamp a stale read can still use: the retention period back from now,
    /// or the oldest version kept once the history has been pruned below that.
    pub fn earliest_version_time(&self) -> Timestamp {
        let by_retention = Timestamp::from_nanos(
            self.clock.now().as_nanos() - self.version_retention().as_nanos() as i128,
        );
        if !self.history_truncated.load(Ordering::Acquire) {
            return by_retention;
        }
        let history = self.history.read();
        history
            .versions
            .front()
            .map_or(by_retention, |v| v.ts.max(by_retention))
    }

    /// Fails once `ts` is `version_retention_period` or more in the past, as Spanner and the
    /// emulator do for stale reads and for every read in an older read-only transaction.
    pub fn check_retention(&self, ts: Timestamp) -> Result<()> {
        let retention = self.version_retention();
        let horizon = self.clock.now().as_nanos() - retention.as_nanos() as i128;
        if ts.as_nanos() <= horizon {
            return Err(Error::failed_precondition(format!(
                "Read-only transaction timestamp {} has exceeded the maximum timestamp staleness",
                ts.format_offset()
            )));
        }
        Ok(())
    }

    fn version_at(&self, ts: Timestamp) -> Result<Arc<Version>> {
        self.check_retention(ts)?;
        self.find_version(ts)
    }

    /// The version a read at `ts` sees, without the retention check (the caller made it).
    pub(crate) fn find_version(&self, ts: Timestamp) -> Result<Arc<Version>> {
        let latest = self.latest();
        if ts >= latest.ts {
            // Pin `ts` in the past for every later commit, under the commit lock so no commit with
            // a smaller timestamp is still being published.
            let _guard = self.commit.lock();
            self.clock.observe(ts);
            return Ok(self.latest());
        }
        let guard = self.history.read();
        let history = &guard.versions;
        let idx = history.partition_point(|v| v.ts <= ts);
        if idx == 0 {
            if self.history_truncated.load(Ordering::Acquire) {
                return Err(Error::failed_precondition(format!(
                    "Read timestamp {} is too old; the version has been garbage collected.",
                    ts.to_rfc3339()
                )));
            }
            return Ok(history.front().cloned().unwrap_or(latest));
        }
        Ok(history[idx - 1].clone())
    }

    /// Validates against commits since `read_ts`, applies the mutations to the latest state and
    /// publishes the new version.
    pub fn commit(&self, input: CommitInput<'_>) -> Result<CommitOutcome> {
        let mut log = self.commit.lock();
        if self.is_dropped() {
            return Err(Error::not_found("Database not found"));
        }
        if self
            .abort_next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Error::aborted(
                "Transaction was aborted (injected by spanbolt Control.InjectAbort).",
            ));
        }
        let latest = self.latest();
        if !Arc::ptr_eq(&latest.state.catalog, input.catalog) {
            return Err(log.schema_abort());
        }
        // The read set is checked before the mutations are applied: a transaction whose reads
        // went stale must abort (and retry) rather than fail a constraint against the winner's
        // rows, as it would in Spanner, where it waits on the winner's locks.
        let since = match input.read_ts {
            Some(read_ts) => Some(check_reads(&log, read_ts, input.reads)?),
            None => None,
        };
        let ts = self.clock.next_commit_ts();
        let mut w = Writer::new(&latest.state, ts).with_now(self.clock.now());
        let (dml, rest) = input
            .mutations
            .split_at(input.dml_ops.min(input.mutations.len()));
        for m in dml {
            w.apply(m)?;
        }
        let before_rest = w.mutation_count();
        for m in rest {
            w.apply(m)?;
        }
        w.check_mutation_limit(before_rest)?;
        let mutation_count = w.mutation_count();
        let (state, written) = w.finish()?;
        if let Some(start) = since {
            check_writes(&log, start, &written)?;
        }
        if written.is_empty() {
            // Nothing changed: no new version, but the timestamp is still consumed.
            return Ok(CommitOutcome { ts, mutation_count });
        }
        self.publish(&mut log, ts, state, WriteLog::Keys(Arc::from(written)));
        Ok(CommitOutcome { ts, mutation_count })
    }

    /// Replaces the schema, migrating data. Waits for in-flight commits; aborts open
    /// read-write transactions that started earlier when they commit.
    pub fn update_schema(&self, f: impl FnOnce(&DbState) -> Result<Schema>) -> Result<Timestamp> {
        let mut log = self.commit.lock();
        let latest = self.latest();
        let schema = f(&latest.state)?;
        let state = latest.state.with_schema_at(schema, self.clock.now())?;
        let ts = self.clock.next_commit_ts();
        self.publish(&mut log, ts, state, WriteLog::Schema { ddl: None });
        Ok(ts)
    }

    /// Replaces the whole state (schema and migrated data) with what `f` derives from the latest
    /// one, under the same rules as `update_schema`. `f` also returns the DDL it applied, which
    /// the transactions this change aborts are told about.
    pub fn update_state(
        &self,
        f: impl FnOnce(&DbState) -> Result<(DbState, String)>,
    ) -> Result<Timestamp> {
        let mut log = self.commit.lock();
        let latest = self.latest();
        let (state, ddl) = f(&latest.state)?;
        let ts = self.clock.next_commit_ts();
        let ddl = (!ddl.is_empty()).then(|| Arc::from(ddl));
        self.publish(&mut log, ts, state, WriteLog::Schema { ddl });
        Ok(ts)
    }

    fn publish(&self, log: &mut CommitLog, ts: Timestamp, state: DbState, written: WriteLog) {
        let version = Arc::new(Version {
            ts,
            state: Arc::new(state),
        });
        {
            let mut history = self.history.write();
            history.push(version.clone());
            let retention = version.state.schema.version_retention();
            let horizon = Timestamp::from_nanos(ts.as_nanos() - retention.as_nanos() as i128);
            while history.versions.len() > MAX_VERSIONS
                || history.schemas > MAX_SCHEMAS
                || (history.versions.len() > 1 && history.versions[1].ts < horizon)
            {
                history.pop_front();
                self.history_truncated.store(true, Ordering::Release);
            }
        }
        self.latest.store(version);
        log.entries.push_back((ts, written));
        while log.entries.len() > MAX_LOG {
            log.entries.pop_front();
            log.truncated = true;
        }
    }
}

/// Aborts when a commit since `read_ts` wrote a key in `reads` (or changed the schema, or fell
/// out of the log). Returns the index of the first log entry after `read_ts`.
fn check_reads(log: &CommitLog, read_ts: Timestamp, reads: &ReadSet) -> Result<usize> {
    let start = log.entries.partition_point(|(ts, _)| *ts <= read_ts);
    if start == 0 && log.truncated {
        return Err(aborted());
    }
    for (ts, entry) in log.entries.range(start..) {
        match entry {
            WriteLog::Schema { ddl } => return Err(schema_change_aborted(*ts, ddl.as_deref())),
            WriteLog::Keys(keys) if keys.iter().any(|wk| reads.hit(wk.0, &wk.1)) => {
                return Err(aborted());
            }
            WriteLog::Keys(_) => {}
        }
    }
    Ok(start)
}

/// Aborts when a commit in the log from `start` on wrote a key this commit also writes.
fn check_writes(log: &CommitLog, start: usize, written: &[(Target, Key)]) -> Result<()> {
    if log.entries.len() <= start || written.is_empty() {
        return Ok(());
    }
    let own: HashSet<&(Target, Key)> = written.iter().collect();
    for (_, entry) in log.entries.range(start..) {
        if let WriteLog::Keys(keys) = entry
            && keys.iter().any(|wk| own.contains(wk))
        {
            return Err(aborted());
        }
    }
    Ok(())
}
