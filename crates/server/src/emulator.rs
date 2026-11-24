//! In-memory registry of instances, databases, sessions, transactions and operations.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use spanbolt_ddl::Schema;
use spanbolt_proto::longrunning::Operation;
use spanbolt_storage::snapshot::{DatabaseImage, InstanceImage};
use spanbolt_storage::{DbState, Timestamp};
use spanbolt_txn::{Clock, Database, ReadOnlyTxn, ReadWriteTxn, Version};
use tonic::Status;

use crate::names;
use crate::page::Pager;
use crate::sql::{SqlEngine, default_engine};
use crate::status::{resource_not_found, to_status};

pub const EMULATOR_CONFIG: &str = "emulator-config";

#[derive(Debug, Clone)]
pub struct InstanceMeta {
    pub config: String,
    pub display_name: String,
    pub node_count: i32,
    pub processing_units: i32,
    pub labels: BTreeMap<String, String>,
    pub create_time: Timestamp,
    pub update_time: Timestamp,
}

pub struct Instance {
    pub name: String,
    pub meta: RwLock<InstanceMeta>,
    pub databases: RwLock<BTreeMap<String, Arc<DatabaseEntry>>>,
}

pub struct DatabaseEntry {
    pub name: String,
    pub db: Arc<Database>,
    pub create_time: Timestamp,
}

pub struct Session {
    pub name: String,
    pub database: String,
    pub db: Arc<Database>,
    pub multiplexed: bool,
    pub create_time: Timestamp,
    pub labels: std::collections::HashMap<String, String>,
    pub creator_role: String,
    /// Regular sessions run one transaction at a time; beginning another retires the previous one.
    pub current_txn: Mutex<Option<u64>>,
    /// When an RPC last named this session, in `IdleClock` milliseconds.
    pub last_used: AtomicU64,
}

impl Session {
    /// How long this session may go unused before it expires.
    pub fn idle_limit(&self) -> Duration {
        if self.multiplexed {
            MULTIPLEXED_SESSION_IDLE
        } else {
            SESSION_IDLE
        }
    }

    /// Unused for longer than its idle limit at `now` (`IdleClock` milliseconds).
    pub fn expired(&self, now: u64) -> bool {
        let idle = now.saturating_sub(self.last_used.load(Ordering::Relaxed));
        u128::from(idle) > self.idle_limit().as_millis()
    }

    fn touch(&self, now: u64) {
        self.last_used.fetch_max(now, Ordering::Relaxed);
    }
}

/// Monotonic time for idle expiry of sessions and transactions, in milliseconds since start.
/// Separate from the Spanner clock (`Emulator::clock`), which tests fix or move: pinning commit
/// timestamps must not expire sessions.
pub struct IdleClock {
    start: Instant,
    skew_ms: AtomicU64,
    /// Elapsed milliseconds the clock stopped at (`freeze`), or `RUNNING`.
    frozen_ms: AtomicU64,
}

const RUNNING: u64 = u64::MAX;

impl Default for IdleClock {
    fn default() -> Self {
        IdleClock {
            start: Instant::now(),
            skew_ms: AtomicU64::new(0),
            frozen_ms: AtomicU64::new(RUNNING),
        }
    }
}

impl IdleClock {
    pub fn now(&self) -> u64 {
        let elapsed = match self.frozen_ms.load(Ordering::Relaxed) {
            RUNNING => u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX - 1),
            frozen => frozen,
        };
        elapsed.saturating_add(self.skew_ms.load(Ordering::Relaxed))
    }

    /// Stops real time for this clock, so only `advance` moves it: tests that compare exact
    /// idle ages need that.
    pub fn freeze(&self) {
        let elapsed = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX - 1);
        let _ =
            self.frozen_ms
                .compare_exchange(RUNNING, elapsed, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// Moves idle time forward, so tests can expire sessions without waiting.
    pub fn advance(&self, by: Duration) {
        let ms = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.skew_ms.fetch_add(ms, Ordering::Relaxed);
    }
}

pub enum TxnKind {
    ReadWrite(Arc<ReadWriteTxn>),
    ReadOnly(ReadOnlyTxn),
    PartitionedDml,
}

pub struct TxnEntry {
    pub session: String,
    pub kind: TxnKind,
    /// `IdleClock` milliseconds.
    pub created: u64,
}

pub struct Emulator {
    pub clock: Arc<Clock>,
    pub idle: IdleClock,
    instances: RwLock<BTreeMap<String, Arc<Instance>>>,
    sessions: DashMap<String, Arc<Session>>,
    txns: DashMap<u64, Arc<TxnEntry>>,
    operations: Mutex<Operations>,
    next_id: AtomicU64,
    id_salt: u64,
    pub sql: Arc<dyn SqlEngine>,
    /// Follow the emulator's errors instead of production Spanner's (`--emulator-compat`).
    pub emulator_compat: bool,
}

/// The most operations kept; past it the oldest go first. Every spanbolt operation is done when it
/// is created, so the oldest are finished ones (real Spanner keeps operations for about 7 days).
pub const MAX_OPERATIONS: usize = 10_000;

/// Long-running operations by name, with their insertion order for eviction.
pub struct Operations {
    by_name: BTreeMap<String, (u64, Operation)>,
    by_age: BTreeMap<u64, String>,
    next_seq: u64,
    cap: usize,
}

impl Default for Operations {
    fn default() -> Self {
        Operations {
            by_name: BTreeMap::new(),
            by_age: BTreeMap::new(),
            next_seq: 0,
            cap: MAX_OPERATIONS,
        }
    }
}

impl Operations {
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    fn insert(&mut self, op: Operation) {
        let seq = self.next_seq;
        self.next_seq += 1;
        if let Some((old, _)) = self.by_name.insert(op.name.clone(), (seq, op.clone())) {
            self.by_age.remove(&old);
        }
        self.by_age.insert(seq, op.name);
        self.evict();
    }

    /// Drops the oldest operations past the cap.
    fn evict(&mut self) {
        while self.by_name.len() > self.cap {
            let Some((_, name)) = self.by_age.pop_first() else {
                break;
            };
            self.by_name.remove(&name);
        }
    }

    fn get(&self, name: &str) -> Option<&Operation> {
        self.by_name.get(name).map(|(_, op)| op)
    }

    fn remove(&mut self, name: &str) -> Option<Operation> {
        let (seq, op) = self.by_name.remove(name)?;
        self.by_age.remove(&seq);
        Some(op)
    }

    /// Operations whose name starts with `prefix`, in name order.
    fn under<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a Operation> + 'a {
        self.by_name
            .range::<str, _>((
                std::ops::Bound::Included(prefix),
                std::ops::Bound::Unbounded,
            ))
            .take_while(move |(k, _)| k.starts_with(prefix))
            .map(|(_, (_, op))| op)
    }

    /// Drops every operation whose name starts with `prefix`.
    fn remove_under(&mut self, prefix: &str) {
        let names: Vec<String> = self.under(prefix).map(|o| o.name.clone()).collect();
        for n in names {
            self.remove(&n);
        }
    }
}

/// Transactions idle this long are dropped by the sweeper.
pub const TXN_IDLE: Duration = Duration::from_secs(3600);

/// Regular sessions unused this long expire, like the emulator's and Cloud Spanner's.
pub const SESSION_IDLE: Duration = Duration::from_secs(3600);

/// Multiplexed sessions unused this long expire: 28 days, like the emulator's and Cloud Spanner's.
pub const MULTIPLEXED_SESSION_IDLE: Duration = Duration::from_secs(28 * 24 * 3600);

impl Default for Emulator {
    fn default() -> Self {
        Emulator::new(default_engine(false))
    }
}

impl Emulator {
    pub fn new(sql: Arc<dyn SqlEngine>) -> Emulator {
        let salt = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Emulator {
            clock: Arc::new(Clock::default()),
            idle: IdleClock::default(),
            instances: RwLock::new(BTreeMap::new()),
            sessions: DashMap::new(),
            txns: DashMap::new(),
            operations: Mutex::new(Operations::default()),
            next_id: AtomicU64::new(1),
            id_salt: salt.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            sql,
            emulator_compat: false,
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Opaque, unique-per-process id text for sessions and operations.
    pub fn opaque_id(&self) -> String {
        format!("{:016x}{:08x}", self.id_salt, self.next_id())
    }

    // ---- instances ----

    pub fn instance(&self, name: &str) -> Result<Arc<Instance>, Status> {
        self.instances
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| instance_not_found(name))
    }

    pub fn instances_in(&self, project: &str) -> Vec<Arc<Instance>> {
        let prefix = format!("projects/{project}/instances/");
        self.instances
            .read()
            .values()
            .filter(|i| i.name.starts_with(&prefix))
            .cloned()
            .collect()
    }

    pub fn create_instance(
        &self,
        name: String,
        meta: InstanceMeta,
    ) -> Result<Arc<Instance>, Status> {
        let mut instances = self.instances.write();
        if instances.contains_key(&name) {
            return Err(Status::already_exists(format!(
                "Instance already exists: {name}"
            )));
        }
        let inst = Arc::new(Instance {
            name: name.clone(),
            meta: RwLock::new(meta),
            databases: RwLock::new(BTreeMap::new()),
        });
        instances.insert(name, inst.clone());
        Ok(inst)
    }

    pub fn delete_instance(&self, name: &str) -> Result<(), Status> {
        let inst = self
            .instances
            .write()
            .remove(name)
            .ok_or_else(|| instance_not_found(name))?;
        for db in inst.databases.read().values() {
            db.db.mark_dropped();
        }
        // Like the emulator: the instance's sessions go with it.
        self.remove_sessions_under(&format!("{name}/databases/"));
        self.operations
            .lock()
            .remove_under(&format!("{name}/databases/"));
        Ok(())
    }

    // ---- databases ----

    pub fn database(&self, name: &str) -> Result<Arc<DatabaseEntry>, Status> {
        let not_found = || database_not_found(name);
        let dn = names::database_name(name).map_err(|_| not_found())?;
        let inst = self
            .instances
            .read()
            .get(dn.instance)
            .cloned()
            .ok_or_else(not_found)?;
        let db = inst
            .databases
            .read()
            .get(dn.id)
            .cloned()
            .ok_or_else(not_found)?;
        Ok(db)
    }

    /// Registers a database. `make` runs only when the name is free.
    pub fn add_database(
        &self,
        name: &str,
        make: impl FnOnce() -> Result<Database, Status>,
    ) -> Result<Arc<DatabaseEntry>, Status> {
        let dn = names::database_name(name)?;
        let inst = self.instance(dn.instance)?;
        let mut dbs = inst.databases.write();
        if dbs.contains_key(dn.id) {
            return Err(Status::already_exists(format!(
                "Database already exists: {name}"
            )));
        }
        let entry = Arc::new(DatabaseEntry {
            name: name.to_string(),
            db: Arc::new(make()?),
            create_time: self.clock.now(),
        });
        dbs.insert(dn.id.to_string(), entry.clone());
        Ok(entry)
    }

    /// Creates the database `name` (`projects/P/instances/I/databases/D`) with an empty schema,
    /// and its instance first if needed, as `--database` does at startup. Returns whether the
    /// instance and the database were created; existing ones (from a snapshot) are kept.
    pub fn ensure_database(&self, name: &str) -> Result<(bool, bool), Status> {
        let dn = names::database_name(name)?;
        let (project, instance_id) = names::instance_from_name(dn.instance)?;
        names::validate_instance_id(instance_id)?;
        names::validate_database_id(dn.id)?;
        let created_instance = if self.instance(dn.instance).is_ok() {
            false
        } else {
            let now = self.clock.now();
            let meta = InstanceMeta {
                config: format!("projects/{project}/instanceConfigs/{EMULATOR_CONFIG}"),
                display_name: instance_id.to_string(),
                node_count: 1,
                processing_units: 1000,
                labels: BTreeMap::new(),
                create_time: now,
                update_time: now,
            };
            match self.create_instance(dn.instance.to_string(), meta) {
                Ok(_) => true,
                Err(e) if e.code() == tonic::Code::AlreadyExists => false,
                Err(e) => return Err(e),
            }
        };
        match self.add_database(name, || self.new_database(Schema::default())) {
            Ok(_) => Ok((created_instance, true)),
            Err(e) if e.code() == tonic::Code::AlreadyExists => Ok((created_instance, false)),
            Err(e) => Err(e),
        }
    }

    pub fn drop_database(&self, name: &str) -> Result<(), Status> {
        let dn = names::database_name(name)?;
        let inst = self.instance(dn.instance)?;
        let removed = inst.databases.write().remove(dn.id);
        // The emulator answers OK for a database the instance doesn't have.
        let Some(entry) = removed else {
            return Ok(());
        };
        entry.db.mark_dropped();
        self.remove_sessions_under(&format!("{name}/sessions/"));
        // Like the emulator: a dropped database's operations are gone (GetOperation: NOT_FOUND).
        self.operations
            .lock()
            .remove_under(&format!("{name}/operations/"));
        Ok(())
    }

    /// Test hook while DDL parsing is unavailable: replaces a database's schema directly.
    pub fn install_schema(&self, database: &str, schema: Schema) -> Result<Timestamp, Status> {
        let entry = self.database(database)?;
        entry.db.update_schema(|_| Ok(schema)).map_err(to_status)
    }

    pub fn new_database(&self, schema: Schema) -> Result<Database, Status> {
        let mut state = DbState::new(schema).map_err(to_status)?;
        state.prod_messages = !self.emulator_compat;
        Ok(Database::new(self.clock.clone(), state))
    }

    // ---- sessions ----

    pub fn add_session(&self, s: Session) -> Arc<Session> {
        let s = Arc::new(s);
        s.touch(self.idle.now());
        self.sessions.insert(s.name.clone(), s.clone());
        s
    }

    /// The session `name`, marked as used now. Every RPC that names a session looks it up here,
    /// so any of them keeps it alive (the emulator does the same in `SessionManager::GetSession`).
    pub fn session(&self, name: &str) -> Result<Arc<Session>, Status> {
        let not_found = || resource_not_found("Session", "google.spanner.v1.Session", name);
        let s = self
            .sessions
            .get(name)
            .map(|s| s.clone())
            .ok_or_else(not_found)?;
        let now = self.idle.now();
        if s.expired(now) {
            // Gone as soon as it expires, not only once the sweeper has run.
            if self
                .sessions
                .remove_if(name, |_, s| s.expired(now))
                .is_some()
            {
                self.txns.retain(|_, t| t.session != name);
            }
            return Err(not_found());
        }
        s.touch(now);
        if s.db.is_dropped() {
            return Err(database_not_found(&s.database));
        }
        Ok(s)
    }

    /// One page of a database's regular, unexpired sessions in name order, and the next page token
    /// ([`Pager`]). One pass over the registry, keeping only the page.
    pub fn sessions_page(
        &self,
        database: &str,
        page_size: i32,
        page_token: &str,
    ) -> (Vec<Arc<Session>>, String) {
        let now = self.idle.now();
        let prefix = format!("{database}/sessions/");
        let mut page = Pager::new(|s: &Arc<Session>| s.name.as_str(), page_size, page_token);
        for s in self.sessions.iter() {
            // The key is the name: check it before touching the session.
            if s.key().starts_with(&prefix)
                && page.wants(s.key())
                && !s.multiplexed
                && !s.expired(now)
            {
                page.push(s.value().clone());
            }
        }
        page.finish()
    }

    /// Like the emulator: deleting a session that does not exist (never did, already deleted or
    /// expired) succeeds, and a multiplexed session cannot be deleted (it goes with its database).
    pub fn delete_session(&self, name: &str) -> Result<(), Status> {
        crate::names::validate_session_name(name)?;
        if self
            .sessions
            .remove_if(name, |_, s| !s.multiplexed)
            .is_some()
        {
            self.txns.retain(|_, t| t.session != name);
        } else if self.sessions.contains_key(name) {
            return Err(Status::invalid_argument(
                "Multiplexed sessions may not be deleted.",
            ));
        }
        Ok(())
    }

    /// `Session.approximate_last_use_time`: the Spanner clock's now, less how long ago an RPC
    /// last named the session (the idle clock that expires it, so millisecond resolution), never
    /// before its creation. GetSession reports its own call, as the emulator does.
    pub fn last_use_time(&self, s: &Session) -> Timestamp {
        let age_ms = self
            .idle
            .now()
            .saturating_sub(s.last_used.load(Ordering::Relaxed));
        let age_us = i64::try_from(age_ms.saturating_mul(1000)).unwrap_or(i64::MAX);
        let used = self.clock.now().as_micros().saturating_sub(age_us);
        if used <= s.create_time.as_micros() {
            s.create_time
        } else {
            Timestamp::from_micros(used)
        }
    }

    /// Drops every session whose name starts with `prefix`, and their transactions.
    fn remove_sessions_under(&self, prefix: &str) {
        self.sessions.retain(|k, _| !k.starts_with(prefix));
        self.txns.retain(|_, t| !t.session.starts_with(prefix));
    }

    // ---- transactions ----

    pub fn add_txn(&self, session: &Session, kind: TxnKind) -> u64 {
        let id = self.next_id();
        self.txns.insert(
            id,
            Arc::new(TxnEntry {
                session: session.name.clone(),
                kind,
                created: self.idle.now(),
            }),
        );
        if !session.multiplexed {
            let prev = session.current_txn.lock().replace(id);
            if let Some(prev) = prev {
                self.txns.remove(&prev);
            }
        }
        id
    }

    pub fn txn(&self, session: &Session, id: &[u8]) -> Result<Arc<TxnEntry>, Status> {
        let not_found = || Status::not_found(format!("Transaction not found: {}", hex(id)));
        let raw: [u8; 8] = id
            .try_into()
            .map_err(|_| Status::invalid_argument("Invalid transaction ID"))?;
        let t = self
            .txns
            .get(&u64::from_be_bytes(raw))
            .map(|t| t.clone())
            .ok_or_else(not_found)?;
        if t.session != session.name {
            return Err(not_found());
        }
        Ok(t)
    }

    pub fn end_txn(&self, id: &[u8]) {
        if let Ok(raw) = <[u8; 8]>::try_from(id) {
            self.txns.remove(&u64::from_be_bytes(raw));
        }
    }

    /// Drops sessions unused past their idle limit (clients that never delete theirs would
    /// otherwise grow the registry without bound), and transactions that clients never finished
    /// (read-only ones are never closed explicitly) or whose session is gone.
    pub fn sweep(&self) {
        let now = self.idle.now();
        self.sessions.retain(|_, s| !s.expired(now));
        let txn_idle = TXN_IDLE.as_millis();
        self.txns.retain(|_, t| {
            u128::from(now.saturating_sub(t.created)) < txn_idle
                && self.sessions.contains_key(&t.session)
        });
    }

    /// Entry counts of the registries (soak tests).
    pub fn registry_sizes(&self) -> Vec<(&'static str, usize)> {
        let instances = self.instances.read();
        vec![
            ("instances", instances.len()),
            (
                "databases",
                instances.values().map(|i| i.databases.read().len()).sum(),
            ),
            ("sessions", self.sessions.len()),
            ("txns", self.txns.len()),
            ("operations", self.operations.lock().len()),
        ]
    }

    // ---- operations ----

    pub fn put_operation(&self, op: Operation) -> Operation {
        self.operations.lock().insert(op.clone());
        op
    }

    pub fn operation(&self, name: &str) -> Result<Operation, Status> {
        self.operations
            .lock()
            .get(name)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("Operation not found: {name}")))
    }

    pub fn operations_under(&self, prefix: &str) -> Vec<Operation> {
        self.operations.lock().under(prefix).cloned().collect()
    }

    pub fn delete_operation(&self, name: &str) -> Result<(), Status> {
        self.operations
            .lock()
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| Status::not_found(format!("Operation not found: {name}")))
    }

    // ---- snapshots ----

    pub fn images(&self) -> Vec<InstanceImage> {
        self.instances
            .read()
            .values()
            .map(|inst| {
                let meta = inst.meta.read().clone();
                InstanceImage {
                    name: inst.name.clone(),
                    config: meta.config,
                    display_name: meta.display_name,
                    node_count: meta.node_count,
                    processing_units: meta.processing_units,
                    labels: meta.labels.into_iter().collect(),
                    databases: inst
                        .databases
                        .read()
                        .values()
                        .map(|d| {
                            let v = d.db.latest();
                            DatabaseImage {
                                name: d.name.clone(),
                                commit_ts: v.ts,
                                state: v.state.clone(),
                            }
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// Replaces every instance and database with the snapshot's contents.
    pub fn restore(&self, images: Vec<InstanceImage>) -> (u32, u32) {
        let now = self.clock.now();
        let mut next = BTreeMap::new();
        let mut n_dbs = 0;
        for img in images {
            let mut dbs = BTreeMap::new();
            for d in img.databases {
                let id = names::database_name(&d.name)
                    .map(|n| n.id.to_string())
                    .unwrap_or_else(|_| d.name.clone());
                let mut state = Arc::unwrap_or_clone(d.state);
                state.prod_messages = !self.emulator_compat;
                let version = Arc::new(Version {
                    ts: d.commit_ts,
                    state: Arc::new(state),
                });
                let db = Database::at_version(self.clock.clone(), version);
                dbs.insert(
                    id,
                    Arc::new(DatabaseEntry {
                        name: d.name,
                        db: Arc::new(db),
                        create_time: now,
                    }),
                );
                n_dbs += 1;
            }
            let meta = InstanceMeta {
                config: img.config,
                display_name: img.display_name,
                node_count: img.node_count,
                processing_units: img.processing_units,
                labels: img.labels.into_iter().collect(),
                create_time: now,
                update_time: now,
            };
            next.insert(
                img.name.clone(),
                Arc::new(Instance {
                    name: img.name,
                    meta: RwLock::new(meta),
                    databases: RwLock::new(dbs),
                }),
            );
        }
        let n_inst = next.len() as u32;
        let old = std::mem::replace(&mut *self.instances.write(), next);
        for inst in old.values() {
            for d in inst.databases.read().values() {
                d.db.mark_dropped();
            }
        }
        self.sessions.clear();
        self.txns.clear();
        (n_inst, n_dbs)
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn instance_not_found(name: &str) -> Status {
    resource_not_found(
        "Instance",
        "google.spanner.admin.instance.v1.Instance",
        name,
    )
}

fn database_not_found(name: &str) -> Status {
    resource_not_found(
        "Database",
        "google.spanner.admin.database.v1.Database",
        name,
    )
}

#[cfg(test)]
mod operation_tests {
    use super::*;

    fn op(name: &str) -> Operation {
        Operation {
            name: name.into(),
            done: true,
            ..Default::default()
        }
    }

    #[test]
    fn evicts_oldest_past_the_cap() {
        let mut ops = Operations {
            cap: 3,
            ..Default::default()
        };
        for i in 0..5 {
            ops.insert(op(&format!("db/operations/{i}")));
        }
        assert_eq!(ops.len(), 3);
        assert!(ops.get("db/operations/0").is_none());
        assert!(ops.get("db/operations/1").is_none());
        assert!(ops.get("db/operations/4").is_some());
        // Re-inserting a name makes it the newest.
        ops.insert(op("db/operations/2"));
        ops.insert(op("db/operations/5"));
        assert!(ops.get("db/operations/2").is_some());
        assert!(ops.get("db/operations/3").is_none());
        assert_eq!(ops.by_age.len(), ops.by_name.len());
    }

    #[test]
    fn remove_under_drops_only_that_prefix() {
        let mut ops = Operations::default();
        for n in [
            "a/operations/1",
            "a/operations/2",
            "ab/operations/1",
            "b/operations/1",
        ] {
            ops.insert(op(n));
        }
        ops.remove_under("a/operations/");
        let left: Vec<_> = ops.under("").map(|o| o.name.clone()).collect();
        assert_eq!(left, ["ab/operations/1", "b/operations/1"]);
        assert_eq!(ops.by_age.len(), 2);
    }
}
