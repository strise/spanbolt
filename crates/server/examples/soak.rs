//! Soak test: an in-process server under a mixed workload for a long time, sampling RSS, latency
//! percentiles and cache sizes into a CSV.
//!
//! ```text
//! source scripts/googlesql-env.sh
//! cargo build --release -p spanbolt-conformance
//! cargo run --release -p spanbolt-server --features googlesql --example soak -- \
//!     --duration 3600 --out .mcp-output/soak.csv
//! ```
//!
//! Workload, all at once:
//! - the conformance suite in a loop (`target/release/spanbolt-conformance check --oracle spanner`,
//!   a subprocess against this strict server, as gate.sh's prod mode: thousands of database
//!   creates, DDL and drops per pass);
//! - `--workers` cycles of: create a database, seed it, run point/index/SEARCH queries, DML and
//!   mutations (including distinct SQL texts), clone a seeded template, work on the clone, drop
//!   both;
//! - one long-lived database taking single-row commits and reads (MVCC history);
//! - `--churn` clients that each open a new connection, create a session, run a query and go
//!   away, as a test suite that builds a client per test does (the emulator leaked or slowed down
//!   per connection: GoogleCloudPlatform/cloud-spanner-emulator#72). Every other client uses a
//!   multiplexed session, as the Java and Go clients do with
//!   `GOOGLE_CLOUD_SPANNER_MULTIPLEXED_SESSIONS(_FOR_RW,_PARTITIONED_OPS)=true`, and runs a
//!   read-write transaction with its precommit token, a partitioned DML and a PartitionQuery on it
//!   (the emulator crashed on these: #260, #273). Regular sessions are deleted; multiplexed ones
//!   can't be and stay until they idle out, so watch the `sessions` and `txns` columns.
//!   `--churn-pause-ms` (default 20) spaces the clients out: each leaves a socket in TIME_WAIT.
//!
//! Runs on the `spanbolt` binary's allocator (mimalloc, src/main.rs). Memory columns: `rss_mb`,
//! `footprint_mb` (macOS physical footprint, counts compressed pages), `rust_heap_mb` (live bytes
//! allocated through mimalloc, i.e. by Rust code) and `malloc_mb` (bytes in use in the system malloc
//! zones: the GoogleSQL shim's C++ heap, which mimalloc does not replace). mimalloc's own numbers
//! don't give the live heap: release builds keep no live-byte statistics, and its committed memory
//! on macOS is the eagerly committed arena reservation (a constant 2 GiB).
//!
//! Every `--sample` seconds a CSV row gets: elapsed, memory, operations and p50/p99 per operation
//! kind in that window, registry sizes (databases, sessions, transactions, operations) and SQL
//! cache sizes. The summary at the end fits memory growth after `--warmup` seconds.

use std::alloc::{GlobalAlloc, Layout};
use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use prost_types::value::Kind;
use prost_types::{ListValue, Value};
use spanbolt_proto::control as ctl;
use spanbolt_proto::control::control_client::ControlClient;
use spanbolt_proto::database_admin as da;
use spanbolt_proto::database_admin::database_admin_client::DatabaseAdminClient;
use spanbolt_proto::instance_admin as ia;
use spanbolt_proto::instance_admin::instance_admin_client::InstanceAdminClient;
use spanbolt_proto::spanner as pb;
use spanbolt_proto::spanner::spanner_client::SpannerClient;
use spanbolt_server::Emulator;
use tonic::transport::Channel;

/// The allocator the `spanbolt` binary uses (src/main.rs), so memory and latency match it, with a
/// count of the bytes live in it.
#[global_allocator]
static GLOBAL: Counting = Counting;

/// Live bytes, sharded by thread so the count doesn't serialise allocations: one shared counter
/// cost a third of the soak's throughput. A shard can go negative (freed on another thread); the
/// sum is the live total.
static LIVE: [Shard; 64] = [const { Shard(AtomicIsize::new(0)) }; 64];
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

#[repr(align(128))]
struct Shard(AtomicIsize);

thread_local! {
    static SHARD: usize = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % LIVE.len();
}

fn count(delta: isize) {
    // `try_with`: allocations while the thread's locals are torn down go to shard 0.
    let i = SHARD.try_with(|s| *s).unwrap_or(0);
    LIVE[i].0.fetch_add(delta, Ordering::Relaxed);
}

fn rust_heap() -> u64 {
    LIVE.iter()
        .map(|s| s.0.load(Ordering::Relaxed))
        .sum::<isize>()
        .max(0) as u64
}

struct Counting;

// SAFETY: every call goes to mimalloc unchanged; only the counter is added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { mimalloc::MiMalloc.alloc(l) };
        if !p.is_null() {
            count(l.size() as isize);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { mimalloc::MiMalloc.alloc_zeroed(l) };
        if !p.is_null() {
            count(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { mimalloc::MiMalloc.dealloc(p, l) };
        count(-(l.size() as isize));
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        let q = unsafe { mimalloc::MiMalloc.realloc(p, l, size) };
        if !q.is_null() {
            count(size as isize - l.size() as isize);
        }
        q
    }
}

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

const INSTANCE: &str = "projects/soak/instances/soak";
const SEED_ROWS: i64 = 200;
const TEMPLATE_ROWS: i64 = 2000;

const SCHEMA: &[&str] = &[
    "CREATE TABLE Items (Id INT64 NOT NULL, Grp INT64, Name STRING(MAX), Body STRING(MAX), Body_Tokens TOKENLIST AS (TOKENIZE_FULLTEXT(Body)) HIDDEN) PRIMARY KEY (Id)",
    "CREATE INDEX ItemsByGrp ON Items(Grp)",
    "CREATE SEARCH INDEX ItemsSearch ON Items(Body_Tokens)",
    "CREATE TABLE Notes (Id INT64 NOT NULL, NoteId INT64 NOT NULL, Text STRING(MAX)) PRIMARY KEY (Id, NoteId), INTERLEAVE IN PARENT Items ON DELETE CASCADE",
];

const WORDS: &[&str] = &[
    "fjord", "harbour", "invest", "shipping", "holding", "consult", "north", "south", "river",
    "mountain", "bridge", "market", "energy", "salmon", "timber", "copper",
];

struct Args {
    duration: Duration,
    sample: Duration,
    warmup: Duration,
    workers: usize,
    churn: usize,
    churn_pause: Duration,
    out: String,
    conformance: Option<String>,
}

fn args() -> Result<Args, BoxErr> {
    let mut a = Args {
        duration: Duration::from_secs(3600),
        sample: Duration::from_secs(10),
        warmup: Duration::from_secs(300),
        workers: 4,
        churn: 2,
        churn_pause: Duration::from_millis(20),
        out: ".mcp-output/soak.csv".into(),
        conformance: Some("target/release/spanbolt-conformance".into()),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--duration" => a.duration = Duration::from_secs(val()?.parse()?),
            "--sample" => a.sample = Duration::from_secs(val()?.parse()?),
            "--warmup" => a.warmup = Duration::from_secs(val()?.parse()?),
            "--workers" => a.workers = val()?.parse()?,
            "--churn" => a.churn = val()?.parse()?,
            "--churn-pause-ms" => a.churn_pause = Duration::from_millis(val()?.parse()?),
            "--out" => a.out = val()?,
            "--conformance" => a.conformance = Some(val()?),
            "--no-conformance" => a.conformance = None,
            _ => return Err(format!("unknown flag {flag}").into()),
        }
    }
    Ok(a)
}

/// Latency samples (µs) per operation kind for the current window.
#[derive(Default)]
struct Window {
    samples: Mutex<BTreeMap<&'static str, Vec<u32>>>,
    errors: AtomicU64,
}

impl Window {
    fn record(&self, kind: &'static str, t: Instant) {
        let us = t.elapsed().as_micros().min(u32::MAX as u128) as u32;
        self.samples.lock().entry(kind).or_default().push(us);
    }

    fn take(&self) -> BTreeMap<&'static str, Vec<u32>> {
        std::mem::take(&mut *self.samples.lock())
    }
}

const KINDS: &[&str] = &[
    "create_db",
    "seed",
    "point",
    "index",
    "search",
    "dml",
    "commit",
    "clone",
    "drop_db",
    "connect",
    "mux_rw",
    "mux_pdml",
    "mux_partition",
];

fn s(v: impl ToString) -> Value {
    Value {
        kind: Some(Kind::StringValue(v.to_string())),
    }
}

fn rss_kb() -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output();
    out.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

/// Physical footprint and bytes in use in the system malloc zones, in bytes. macOS compresses idle
/// pages under memory pressure, so RSS alone can under-report; the footprint counts compressed
/// pages. Rust allocations go to mimalloc, so the zones hold what C/C++ code allocates (the
/// GoogleSQL shim).
#[cfg(target_os = "macos")]
fn footprint_and_malloc() -> (u64, u64) {
    #[repr(C)]
    #[derive(Default)]
    struct MallocStats {
        blocks_in_use: u32,
        size_in_use: usize,
        max_size_in_use: usize,
        size_allocated: usize,
    }
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
        fn malloc_zone_statistics(zone: *mut std::ffi::c_void, stats: *mut MallocStats);
    }
    // rusage_info_v0: 16-byte uuid, then u64 fields; ri_phys_footprint is the 8th of them.
    let mut ru = [0u64; 32];
    let mut ms = MallocStats::default();
    // SAFETY: the buffer is larger than rusage_info_v0; a null zone means all zones.
    let ok = unsafe {
        malloc_zone_statistics(std::ptr::null_mut(), &mut ms);
        proc_pid_rusage(std::process::id() as i32, 0, ru.as_mut_ptr())
    };
    (if ok == 0 { ru[9] } else { 0 }, ms.size_in_use as u64)
}

#[cfg(not(target_os = "macos"))]
fn footprint_and_malloc() -> (u64, u64) {
    (0, 0)
}

fn body(i: i64) -> String {
    let u = i as usize;
    format!(
        "{} {} {} unik{i}",
        WORDS[u % 16],
        WORDS[(u / 16) % 16],
        WORDS[(u * 7) % 16]
    )
}

fn single_rw() -> Option<pb::commit_request::Transaction> {
    Some(pb::commit_request::Transaction::SingleUseTransaction(
        pb::TransactionOptions {
            mode: Some(pb::transaction_options::Mode::ReadWrite(Default::default())),
            ..Default::default()
        },
    ))
}

fn items(range: std::ops::Range<i64>) -> pb::Mutation {
    pb::Mutation {
        operation: Some(pb::mutation::Operation::InsertOrUpdate(
            pb::mutation::Write {
                table: "Items".into(),
                columns: ["Id", "Grp", "Name", "Body"].map(String::from).to_vec(),
                values: range
                    .map(|i| ListValue {
                        values: vec![s(i), s(i % 50), s(format!("item-{i}")), s(body(i))],
                    })
                    .collect(),
            },
        )),
    }
}

fn params(
    p: &[(&str, pb::TypeCode, Value)],
) -> (Option<prost_types::Struct>, HashMap<String, pb::Type>) {
    let mut fields = BTreeMap::new();
    let mut types = HashMap::new();
    for (n, t, v) in p {
        fields.insert(n.to_string(), v.clone());
        types.insert(
            n.to_string(),
            pb::Type {
                code: *t as i32,
                ..Default::default()
            },
        );
    }
    (Some(prost_types::Struct { fields }), types)
}

struct Client {
    sp: SpannerClient<Channel>,
    admin: DatabaseAdminClient<Channel>,
    control: ControlClient<Channel>,
    w: Arc<Window>,
}

impl Client {
    fn new(channel: &Channel, w: Arc<Window>) -> Client {
        Client {
            sp: SpannerClient::new(channel.clone()),
            admin: DatabaseAdminClient::new(channel.clone()),
            control: ControlClient::new(channel.clone()),
            w,
        }
    }

    async fn create_db(&mut self, id: &str) -> Result<String, BoxErr> {
        let t = Instant::now();
        self.admin
            .create_database(da::CreateDatabaseRequest {
                parent: INSTANCE.into(),
                create_statement: format!("CREATE DATABASE `{id}`"),
                extra_statements: SCHEMA.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            })
            .await?;
        self.w.record("create_db", t);
        Ok(format!("{INSTANCE}/databases/{id}"))
    }

    async fn drop_db(&mut self, db: &str) -> Result<(), BoxErr> {
        let t = Instant::now();
        self.admin
            .drop_database(da::DropDatabaseRequest {
                database: db.into(),
            })
            .await?;
        self.w.record("drop_db", t);
        Ok(())
    }

    async fn session(&mut self, db: &str) -> Result<String, BoxErr> {
        Ok(self
            .sp
            .create_session(pb::CreateSessionRequest {
                database: db.into(),
                session: None,
            })
            .await?
            .into_inner()
            .name)
    }

    async fn commit(
        &mut self,
        session: &str,
        m: Vec<pb::Mutation>,
        kind: &'static str,
    ) -> Result<(), BoxErr> {
        let t = Instant::now();
        self.sp
            .commit(pb::CommitRequest {
                session: session.into(),
                transaction: single_rw(),
                mutations: m,
                ..Default::default()
            })
            .await?;
        self.w.record(kind, t);
        Ok(())
    }

    async fn query(
        &mut self,
        session: &str,
        sql: String,
        p: &[(&str, pb::TypeCode, Value)],
        kind: &'static str,
    ) -> Result<usize, BoxErr> {
        let (params, param_types) = params(p);
        let t = Instant::now();
        let rs = self
            .sp
            .execute_sql(pb::ExecuteSqlRequest {
                session: session.into(),
                sql,
                params,
                param_types,
                ..Default::default()
            })
            .await?
            .into_inner();
        self.w.record(kind, t);
        Ok(rs.rows.len())
    }

    /// An UPDATE in a read-write transaction begun inline, then committed.
    async fn dml(&mut self, session: &str, id: i64, seqno: i64) -> Result<(), BoxErr> {
        let (params, param_types) = params(&[
            ("id", pb::TypeCode::Int64, s(id)),
            ("b", pb::TypeCode::String, s(body(id + seqno))),
        ]);
        let t = Instant::now();
        let rs = self
            .sp
            .execute_sql(pb::ExecuteSqlRequest {
                session: session.into(),
                transaction: Some(pb::TransactionSelector {
                    selector: Some(pb::transaction_selector::Selector::Begin(
                        pb::TransactionOptions {
                            mode: Some(
                                pb::transaction_options::Mode::ReadWrite(Default::default()),
                            ),
                            ..Default::default()
                        },
                    )),
                }),
                sql: "UPDATE Items SET Body = @b WHERE Id = @id".into(),
                params,
                param_types,
                seqno,
                ..Default::default()
            })
            .await?
            .into_inner();
        let txn = rs
            .metadata
            .and_then(|m| m.transaction)
            .map(|t| t.id)
            .unwrap_or_default();
        self.sp
            .commit(pb::CommitRequest {
                session: session.into(),
                transaction: Some(pb::commit_request::Transaction::TransactionId(txn)),
                ..Default::default()
            })
            .await?;
        self.w.record("dml", t);
        Ok(())
    }

    /// Queries, DML and mutations on a seeded database.
    async fn work(&mut self, session: &str, cycle: u64, rows: i64) -> Result<(), BoxErr> {
        for j in 0..40i64 {
            let k = (cycle as i64 * 7919 + j * 31) % rows;
            let n = self
                .query(
                    session,
                    "SELECT Id, Name, Body FROM Items WHERE Id = @k".into(),
                    &[("k", pb::TypeCode::Int64, s(k))],
                    "point",
                )
                .await?;
            if n != 1 {
                return Err(format!("point read of {k} returned {n} rows").into());
            }
            self.query(
                session,
                "SELECT Id, Name FROM Items@{FORCE_INDEX=ItemsByGrp} WHERE Grp = @g".into(),
                &[("g", pb::TypeCode::Int64, s(k % 50))],
                "index",
            )
            .await?;
            self.query(
                session,
                "SELECT Id FROM Items WHERE SEARCH(Body_Tokens, @q)".into(),
                &[("q", pb::TypeCode::String, s(WORDS[(k as usize) % 16]))],
                "search",
            )
            .await?;
            if j % 4 == 0 {
                self.dml(session, k, j + 1).await?;
                self.commit(
                    session,
                    vec![pb::Mutation {
                        operation: Some(pb::mutation::Operation::InsertOrUpdate(
                            pb::mutation::Write {
                                table: "Notes".into(),
                                columns: ["Id", "NoteId", "Text"].map(String::from).to_vec(),
                                values: vec![ListValue {
                                    values: vec![s(k), s(j), s(format!("note {cycle}"))],
                                }],
                            },
                        )),
                    }],
                    "commit",
                )
                .await?;
            }
        }
        // A statement text that is new every cycle: the plan cache sees a stream of distinct
        // SQL, like a test suite with many generated queries.
        self.query(
            session,
            format!(
                "SELECT COUNT(*) AS c{cycle} FROM Items WHERE Grp < {}",
                cycle % 50
            ),
            &[],
            "index",
        )
        .await?;
        Ok(())
    }

    async fn cycle(&mut self, worker: usize, cycle: u64) -> Result<(), BoxErr> {
        let db = self.create_db(&format!("w{worker}-{cycle}")).await?;
        let session = self.session(&db).await?;
        let t = Instant::now();
        self.commit(&session, vec![items(0..SEED_ROWS)], "commit")
            .await?;
        self.w.record("seed", t);
        self.work(&session, cycle, SEED_ROWS).await?;
        self.drop_db(&db).await?;

        let clone = format!("{INSTANCE}/databases/c{worker}-{cycle}");
        let t = Instant::now();
        self.control
            .clone_database(ctl::CloneDatabaseRequest {
                source: format!("{INSTANCE}/databases/template"),
                target: clone.clone(),
            })
            .await?;
        self.w.record("clone", t);
        let session = self.session(&clone).await?;
        self.work(&session, cycle, TEMPLATE_ROWS).await?;
        self.drop_db(&clone).await
    }
}

fn rw_options() -> pb::TransactionOptions {
    pb::TransactionOptions {
        mode: Some(pb::transaction_options::Mode::ReadWrite(Default::default())),
        ..Default::default()
    }
}

/// One short-lived client: a new connection, a session, a point read on `db`; on a multiplexed
/// session also a read-write transaction, a partitioned DML and a PartitionQuery. `n` picks the
/// rows and whether the session is multiplexed.
async fn churn_client(url: &str, db: &str, n: u64, w: &Window) -> Result<(), BoxErr> {
    let multiplexed = n % 2 == 1;
    // Rows the hot writer doesn't touch (it writes 0..5000), so the update doesn't abort.
    let k = 5000 + (n % 5000) as i64;
    let t = Instant::now();
    let channel = Channel::from_shared(url.to_string())?.connect().await?;
    let mut sp = SpannerClient::new(channel);
    let session = sp
        .create_session(pb::CreateSessionRequest {
            database: db.into(),
            session: Some(pb::Session {
                multiplexed,
                ..Default::default()
            }),
        })
        .await?
        .into_inner()
        .name;
    let (ps, param_types) = params(&[("k", pb::TypeCode::Int64, s(k))]);
    sp.execute_sql(pb::ExecuteSqlRequest {
        session: session.clone(),
        sql: "SELECT Id, Body FROM Items WHERE Id = @k".into(),
        params: ps,
        param_types,
        ..Default::default()
    })
    .await?;
    if !multiplexed {
        sp.delete_session(pb::DeleteSessionRequest {
            name: session.clone(),
        })
        .await?;
        w.record("connect", t);
        return Ok(());
    }
    w.record("connect", t);

    // A read-write transaction begun inline; the commit carries the precommit token.
    let t = Instant::now();
    let (ps, param_types) = params(&[
        ("k", pb::TypeCode::Int64, s(k)),
        ("b", pb::TypeCode::String, s(body(k + n as i64))),
    ]);
    let rs = sp
        .execute_sql(pb::ExecuteSqlRequest {
            session: session.clone(),
            transaction: Some(pb::TransactionSelector {
                selector: Some(pb::transaction_selector::Selector::Begin(rw_options())),
            }),
            sql: "UPDATE Items SET Body = @b WHERE Id = @k".into(),
            params: ps,
            param_types,
            seqno: 1,
            ..Default::default()
        })
        .await?
        .into_inner();
    let txn = rs
        .metadata
        .and_then(|m| m.transaction)
        .map(|t| t.id)
        .unwrap_or_default();
    sp.commit(pb::CommitRequest {
        session: session.clone(),
        transaction: Some(pb::commit_request::Transaction::TransactionId(txn)),
        precommit_token: rs.precommit_token,
        ..Default::default()
    })
    .await?;
    w.record("mux_rw", t);

    let t = Instant::now();
    let pdml = sp
        .begin_transaction(pb::BeginTransactionRequest {
            session: session.clone(),
            options: Some(pb::TransactionOptions {
                mode: Some(pb::transaction_options::Mode::PartitionedDml(
                    Default::default(),
                )),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await?
        .into_inner();
    let (ps, param_types) = params(&[("g", pb::TypeCode::Int64, s(k % 50))]);
    sp.execute_sql(pb::ExecuteSqlRequest {
        session: session.clone(),
        transaction: Some(pb::TransactionSelector {
            selector: Some(pb::transaction_selector::Selector::Id(pdml.id)),
        }),
        sql: "UPDATE Items SET Name = Name WHERE Grp = @g".into(),
        params: ps,
        param_types,
        seqno: 1,
        ..Default::default()
    })
    .await?;
    w.record("mux_pdml", t);

    let t = Instant::now();
    let parts = sp
        .partition_query(pb::PartitionQueryRequest {
            session: session.clone(),
            transaction: Some(pb::TransactionSelector {
                selector: Some(pb::transaction_selector::Selector::Begin(
                    pb::TransactionOptions {
                        mode: Some(pb::transaction_options::Mode::ReadOnly(Default::default())),
                        ..Default::default()
                    },
                )),
            }),
            sql: "SELECT Id, Grp FROM Items".into(),
            ..Default::default()
        })
        .await?
        .into_inner();
    let ro = parts.transaction.map(|t| t.id).unwrap_or_default();
    for p in parts.partitions {
        sp.execute_sql(pb::ExecuteSqlRequest {
            session: session.clone(),
            transaction: Some(pb::TransactionSelector {
                selector: Some(pb::transaction_selector::Selector::Id(ro.clone())),
            }),
            sql: "SELECT Id, Grp FROM Items".into(),
            partition_token: p.partition_token,
            ..Default::default()
        })
        .await?;
    }
    w.record("mux_partition", t);
    Ok(())
}

/// (elapsed s, RSS MB, footprint MB, Rust heap MB, system malloc MB).
type Mem = (f64, f64, f64, f64, f64);
type MemField = fn(&Mem) -> f64;

/// Least-squares slope of `(seconds, MB)` points, in MB per hour.
fn slope_per_hour(pts: &[(f64, f64)]) -> f64 {
    if pts.len() < 3 {
        return 0.0;
    }
    let n = pts.len() as f64;
    let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
    let cov: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let var: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
    cov / var * 3600.0
}

#[tokio::main]
async fn main() -> Result<(), BoxErr> {
    let a = args()?;
    let emu = Arc::new(Emulator::new(spanbolt_server::sql::default_engine(false)));
    let server = spanbolt_server::spawn("127.0.0.1:0", emu.clone()).await?;
    let channel = Channel::from_shared(format!("http://{}", server.addr))?
        .connect()
        .await?;
    InstanceAdminClient::new(channel.clone())
        .create_instance(ia::CreateInstanceRequest {
            parent: "projects/soak".into(),
            instance_id: "soak".into(),
            instance: Some(ia::Instance {
                config: "emulator-config".into(),
                ..Default::default()
            }),
        })
        .await?;
    let window = Arc::new(Window::default());
    let mut setup = Client::new(&channel, window.clone());
    let template = setup.create_db("template").await?;
    let ts = setup.session(&template).await?;
    for c in (0..TEMPLATE_ROWS).step_by(500) {
        setup.commit(&ts, vec![items(c..c + 500)], "seed").await?;
    }
    let hot = setup.create_db("hot").await?;
    let churn_db = hot.clone();
    window.take();

    let stop = Arc::new(AtomicBool::new(false));
    let passes = Arc::new(AtomicU64::new(0));
    let last_pass = Arc::new(Mutex::new(String::new()));
    let mut tasks = Vec::new();

    if let Some(bin) = a.conformance.clone() {
        let (stop, passes, last_pass, addr) =
            (stop.clone(), passes.clone(), last_pass.clone(), server.addr);
        tasks.push(tokio::task::spawn_blocking(move || {
            while !stop.load(Ordering::Relaxed) {
                let out = Command::new(&bin)
                    // Against Cloud Spanner's expectations, as gate.sh's prod mode does: the
                    // server is strict, so a failure is a regression, not an emulator difference.
                    .args([
                        "check",
                        "--oracle",
                        "spanner",
                        "--target",
                        &addr.to_string(),
                    ])
                    .output();
                match out {
                    Ok(o) => {
                        let text = String::from_utf8_lossy(&o.stdout);
                        if let Some(l) = text.lines().rev().find(|l| l.starts_with("TOTAL")) {
                            *last_pass.lock() = l.split_whitespace().collect::<Vec<_>>().join(" ");
                        }
                        passes.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("soak: cannot run {bin}: {e}");
                        return;
                    }
                }
            }
        }));
    }

    for worker in 0..a.workers {
        let (stop, mut c) = (stop.clone(), Client::new(&channel, window.clone()));
        tasks.push(tokio::spawn(async move {
            let mut cycle = 0u64;
            while !stop.load(Ordering::Relaxed) {
                if let Err(e) = c.cycle(worker, cycle).await {
                    c.w.errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("soak: worker {worker} cycle {cycle}: {e}");
                }
                cycle += 1;
            }
        }));
    }

    {
        let (stop, mut c) = (stop.clone(), Client::new(&channel, window.clone()));
        tasks.push(tokio::spawn(async move {
            let Ok(session) = c.session(&hot).await else {
                return;
            };
            let mut i = 0i64;
            while !stop.load(Ordering::Relaxed) {
                let r = async {
                    c.commit(&session, vec![items(i % 5000..i % 5000 + 1)], "commit")
                        .await?;
                    c.query(
                        &session,
                        "SELECT Id, Body FROM Items WHERE Id = @k".into(),
                        &[("k", pb::TypeCode::Int64, s(i % 5000))],
                        "point",
                    )
                    .await
                }
                .await;
                if let Err(e) = r {
                    c.w.errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("soak: hot writer: {e}");
                }
                i += 1;
            }
        }));
    }

    let url = format!("http://{}", server.addr);
    let churned = Arc::new(AtomicU64::new(0));
    for _ in 0..a.churn {
        let pause = a.churn_pause;
        let (stop, w, url, hot, churned) = (
            stop.clone(),
            window.clone(),
            url.clone(),
            churn_db.clone(),
            churned.clone(),
        );
        tasks.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let n = churned.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = churn_client(&url, &hot, n, &w).await {
                    w.errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("soak: churn client {n}: {e}");
                }
                // Each client leaves a socket in TIME_WAIT; without a pause the OS runs out of
                // ephemeral ports within a minute.
                tokio::time::sleep(pause).await;
            }
        }));
    }

    if let Some(dir) = std::path::Path::new(&a.out).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut csv = std::fs::File::create(&a.out)?;
    let regs: Vec<&str> = emu.registry_sizes().iter().map(|(k, _)| *k).collect();
    let caches: Vec<&str> = emu.sql.cache_sizes().iter().map(|(k, _)| *k).collect();
    let mut header = vec![
        "elapsed_s",
        "rss_mb",
        "footprint_mb",
        "rust_heap_mb",
        "malloc_mb",
        "ops",
        "errors",
        "conformance_passes",
    ];
    let lat: Vec<String> = KINDS
        .iter()
        .flat_map(|k| {
            [
                format!("{k}_n"),
                format!("{k}_p50_us"),
                format!("{k}_p99_us"),
            ]
        })
        .collect();
    header.extend(lat.iter().map(|s| s.as_str()));
    header.extend(&regs);
    header.extend(&caches);
    writeln!(csv, "{}", header.join(","))?;

    let start = Instant::now();
    let mut rss: Vec<Mem> = Vec::new();
    let mut first_pct: Option<BTreeMap<&str, (u32, u32)>> = None;
    let mut last_pct = BTreeMap::new();
    let mut tick = tokio::time::interval(a.sample);
    tick.tick().await;
    while start.elapsed() < a.duration {
        tick.tick().await;
        let el = start.elapsed().as_secs_f64();
        let mb = rss_kb() as f64 / 1024.0;
        let (fp, malloc) = footprint_and_malloc();
        let mib = |b: u64| b as f64 / 1048576.0;
        let (fp, mi, malloc) = (mib(fp), mib(rust_heap()), mib(malloc));
        rss.push((el, mb, fp, mi, malloc));
        let mut samples = window.take();
        let ops: usize = samples.values().map(|v| v.len()).sum();
        let mut row = vec![
            format!("{el:.0}"),
            format!("{mb:.1}"),
            format!("{fp:.1}"),
            format!("{mi:.1}"),
            format!("{malloc:.1}"),
            ops.to_string(),
            window.errors.load(Ordering::Relaxed).to_string(),
            passes.load(Ordering::Relaxed).to_string(),
        ];
        let mut pct = BTreeMap::new();
        for k in KINDS {
            let v = samples.entry(k).or_default();
            v.sort_unstable();
            let p = |q: f64| {
                v.get(((v.len() as f64 * q) as usize).min(v.len().saturating_sub(1)))
                    .copied()
                    .unwrap_or(0)
            };
            let (p50, p99) = (p(0.5), p(0.99));
            if !v.is_empty() {
                pct.insert(*k, (p50, p99));
            }
            row.extend([v.len().to_string(), p50.to_string(), p99.to_string()]);
        }
        if el >= a.warmup.as_secs_f64() && first_pct.is_none() {
            first_pct = Some(pct.clone());
        }
        last_pct = pct;
        row.extend(emu.registry_sizes().iter().map(|(_, n)| n.to_string()));
        row.extend(emu.sql.cache_sizes().iter().map(|(_, n)| n.to_string()));
        writeln!(csv, "{}", row.join(","))?;
        csv.flush()?;
        eprintln!(
            "soak: {el:>6.0}s rss={mb:>7.1}MB footprint={fp:>7.1}MB rust_heap={mi:>7.1}MB malloc={malloc:>7.1}MB ops={ops} passes={} {}",
            passes.load(Ordering::Relaxed),
            emu.registry_sizes()
                .iter()
                .chain(emu.sql.cache_sizes().iter())
                .map(|(k, n)| format!("{k}={n}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    stop.store(true, Ordering::Relaxed);
    for t in tasks {
        let _ = t.await;
    }

    let warm = a.warmup.as_secs_f64();
    println!(
        "== soak summary ({:.0} s, {} workers, {} churn tasks, {} short-lived clients)",
        start.elapsed().as_secs_f64(),
        a.workers,
        a.churn,
        churned.load(Ordering::Relaxed)
    );
    let series: [(&str, MemField); 4] = [
        ("rss", |p| p.1),
        ("footprint", |p| p.2),
        ("rust_heap", |p| p.3),
        ("malloc", |p| p.4),
    ];
    for (name, f) in series {
        let pts: Vec<(f64, f64)> = rss
            .iter()
            .filter(|p| p.0 >= warm)
            .map(|p| (p.0, f(p)))
            .collect();
        let at = |p: Option<&Mem>| p.map_or(0.0, f);
        println!(
            "{name}: start {:.1} MB, after warm-up {:.1} MB, end {:.1} MB, peak {:.1} MB, slope after warm-up {:+.1} MB/h",
            at(rss.first()),
            pts.first().map_or(0.0, |p| p.1),
            at(rss.last()),
            rss.iter().map(f).fold(0.0, f64::max),
            slope_per_hour(&pts)
        );
    }
    println!(
        "conformance passes: {} (last: {}); workload errors: {}",
        passes.load(Ordering::Relaxed),
        last_pass.lock(),
        window.errors.load(Ordering::Relaxed)
    );
    println!("latency p50/p99 µs, first window after warm-up -> last window:");
    for k in KINDS {
        let f = first_pct
            .as_ref()
            .and_then(|m| m.get(k))
            .copied()
            .unwrap_or_default();
        let l = last_pct.get(k).copied().unwrap_or_default();
        println!("  {k:<10} {:>7}/{:<8} -> {:>7}/{:<8}", f.0, f.1, l.0, l.1);
    }
    println!(
        "final sizes: {}",
        emu.registry_sizes()
            .iter()
            .chain(emu.sql.cache_sizes().iter())
            .map(|(k, n)| format!("{k}={n}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("csv: {}", a.out);
    Ok(())
}
