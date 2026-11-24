//! Runs cases against a Spanner gRPC endpoint and collects one observation per step.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use prost_types::{ListValue, Timestamp};
use serde_json::Value as Json;
use spanbolt_proto::control::{self as ctl, control_client::ControlClient};
use spanbolt_proto::database_admin::{self as da, database_admin_client::DatabaseAdminClient};
use spanbolt_proto::instance_admin::{self as ia, instance_admin_client::InstanceAdminClient};
use spanbolt_proto::longrunning::{self as lro, operations_client::OperationsClient};
use spanbolt_proto::spanner::{
    self as pb, spanner_client::SpannerClient, transaction_options as topts,
    transaction_selector::Selector,
};
use tokio::sync::{Mutex, OnceCell};
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, ClientTlsConfig};
use tonic::{Code, Request, Status};

use crate::case::{Case, Step, Verb, WriteKind, split_statements};
use crate::observe::Observation;
use crate::value;

const PROJECT: &str = "projects/conformance";
const INSTANCE: &str = "projects/conformance/instances/conformance";
const SPANNER_ENDPOINT: &str = "https://spanner.googleapis.com";
/// Spanner database ids are at most 30 characters; `spanbolt-oracle-YYYYMMDD-` takes 25.
const ORACLE_PREFIX: &str = "spanbolt-oracle-";
/// Stands for the case database's id in `ddl:` bodies (e.g. `ALTER DATABASE <dbid> SET OPTIONS`)
/// and in their error messages.
const DB_ID: &str = "<dbid>";

type Chan = InterceptedService<Channel, Auth>;

/// Adds `authorization: Bearer <token>` when talking to real Spanner.
#[derive(Clone, Default)]
pub struct Auth(Option<Arc<std::sync::RwLock<String>>>);

impl tonic::service::Interceptor for Auth {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        if let Some(token) = &self.0 {
            let token = token.read().map(|t| t.clone()).unwrap_or_default();
            let value = MetadataValue::try_from(format!("Bearer {token}"))
                .map_err(|_| Status::unauthenticated("bad access token"))?;
            req.metadata_mut().insert("authorization", value);
        }
        Ok(req)
    }
}

/// Real Cloud Spanner: an existing instance where the harness may only create, use and drop
/// databases named `spanbolt-oracle-<mmdd><run>-<n>`.
#[derive(Clone, Debug)]
pub struct SpannerOracle {
    pub project: String,
    pub instance: String,
    /// Append-only log of every database created and dropped.
    pub db_log: std::path::PathBuf,
}

fn access_token() -> Result<String, String> {
    let out = std::process::Command::new("gcloud")
        .args(["auth", "application-default", "print-access-token"])
        .output()
        .map_err(|e| format!("gcloud: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "gcloud auth application-default print-access-token: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `YYYYMMDD` (UTC) for a unix time.
fn yyyymmdd(secs: i64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}")
}
const MAX_MSG: usize = 256 << 20;

type TemplateCell = Arc<OnceCell<Result<String, SetupError>>>;

pub struct Target {
    channel: Channel,
    auth: Auth,
    /// `projects/<p>/instances/<i>` the case databases live in.
    instance: String,
    spanner: Option<SpannerOracle>,
    /// `spanbolt-oracle-<mmdd><run>-` for real Spanner (see `oracle_db_prefix`).
    db_prefix: String,
    log: std::sync::Mutex<()>,
    run_id: String,
    counter: AtomicUsize,
    /// Whether the endpoint has spanbolt's `Control.CloneDatabase`.
    pub can_clone: bool,
    templates: Mutex<HashMap<String, TemplateCell>>,
}

/// Why a case could not reach its first step.
#[derive(Debug, Clone)]
pub struct SetupError {
    pub code: Option<Code>,
    pub message: String,
}

impl SetupError {
    fn harness(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }
    fn status(context: &str, s: &Status) -> Self {
        Self {
            code: Some(s.code()),
            message: format!(
                "{context}: {}: {}",
                crate::observe::code_name(s.code()),
                s.message()
            ),
        }
    }
}

pub struct CaseRun {
    pub setup: Result<(), SetupError>,
    /// One entry per case step; `Err` is a harness problem (bad case), not a target error.
    pub steps: Vec<Result<Observation, String>>,
}

impl Target {
    pub async fn connect(addr: &str) -> Result<Self, String> {
        let url = if addr.contains("://") {
            addr.to_string()
        } else {
            format!("http://{addr}")
        };
        let channel = Channel::from_shared(url)
            .map_err(|e| e.to_string())?
            .connect_timeout(Duration::from_secs(10))
            .connect()
            .await
            .map_err(|e| format!("cannot connect to {addr}: {e}"))?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let run_id = to_base36((nanos / 1000) as u64 % 60_466_176, 5);
        let mut t = Self {
            channel,
            auth: Auth::default(),
            instance: INSTANCE.into(),
            spanner: None,
            db_prefix: String::new(),
            log: std::sync::Mutex::new(()),
            run_id,
            counter: AtomicUsize::new(0),
            can_clone: false,
            templates: Mutex::new(HashMap::new()),
        };
        t.ensure_instance().await?;
        t.can_clone = t.probe_clone().await;
        Ok(t)
    }

    /// Connects to real Cloud Spanner with the local application-default credentials. Never
    /// creates instances and never touches databases outside `spanbolt-oracle-<mmdd><run>-<n>`.
    pub async fn connect_spanner(oracle: SpannerOracle) -> Result<Self, String> {
        let token = Arc::new(std::sync::RwLock::new(access_token()?));
        let refresh = token.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(20 * 60)).await;
                match tokio::task::spawn_blocking(access_token).await {
                    Ok(Ok(t)) => {
                        if let Ok(mut w) = refresh.write() {
                            *w = t;
                        }
                    }
                    Ok(Err(e)) => eprintln!("token refresh failed: {e}"),
                    Err(e) => eprintln!("token refresh failed: {e}"),
                }
            }
        });
        let channel = Channel::from_static(SPANNER_ENDPOINT)
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|e| e.to_string())?
            .connect_timeout(Duration::from_secs(20))
            .connect()
            .await
            .map_err(|e| format!("cannot connect to {SPANNER_ENDPOINT}: {e}"))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let db_prefix = oracle_db_prefix(now, std::process::id());
        let instance = format!("projects/{}/instances/{}", oracle.project, oracle.instance);
        Ok(Self {
            channel,
            auth: Auth(Some(token)),
            instance,
            spanner: Some(oracle),
            db_prefix,
            log: std::sync::Mutex::new(()),
            run_id: String::new(),
            counter: AtomicUsize::new(1),
            can_clone: false,
            templates: Mutex::new(HashMap::new()),
        })
    }

    pub fn is_spanner(&self) -> bool {
        self.spanner.is_some()
    }

    fn chan(&self) -> Chan {
        InterceptedService::new(self.channel.clone(), self.auth.clone())
    }

    fn spanner(&self) -> SpannerClient<Chan> {
        SpannerClient::new(self.chan())
            .max_decoding_message_size(MAX_MSG)
            .max_encoding_message_size(MAX_MSG)
    }
    fn db_admin(&self) -> DatabaseAdminClient<Chan> {
        DatabaseAdminClient::new(self.chan()).max_decoding_message_size(MAX_MSG)
    }

    /// Appends `<event> <database id>` to the Spanner database log.
    fn log_db(&self, event: &str, id: &str) {
        let Some(o) = &self.spanner else { return };
        let _guard = self.log.lock();
        let line = format!("{event} {id}\n");
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&o.db_log)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        if let Err(e) = res {
            eprintln!("{}: {e}", o.db_log.display());
        }
    }

    async fn ensure_instance(&self) -> Result<(), String> {
        let mut client = InstanceAdminClient::new(self.channel.clone());
        let req = ia::CreateInstanceRequest {
            parent: PROJECT.into(),
            instance_id: "conformance".into(),
            instance: Some(ia::Instance {
                name: INSTANCE.into(),
                config: format!("{PROJECT}/instanceConfigs/emulator-config"),
                display_name: "conformance".into(),
                node_count: 1,
                ..Default::default()
            }),
        };
        match client.create_instance(req).await {
            Ok(op) => self
                .wait(op.into_inner())
                .await
                .map(|_| ())
                .map_err(|s| format!("CreateInstance: {s}")),
            Err(s) if s.code() == Code::AlreadyExists => Ok(()),
            Err(s) => Err(format!("CreateInstance: {s}")),
        }
    }

    async fn probe_clone(&self) -> bool {
        let req = ctl::CloneDatabaseRequest {
            source: format!("{INSTANCE}/databases/probe-missing"),
            target: format!("{INSTANCE}/databases/probe-target"),
        };
        match ControlClient::new(self.channel.clone())
            .clone_database(req)
            .await
        {
            Err(s) => s.code() != Code::Unimplemented,
            Ok(_) => true,
        }
    }

    async fn wait(&self, mut op: lro::Operation) -> Result<lro::Operation, Status> {
        let (mut delay, max) = if self.is_spanner() {
            (Duration::from_millis(500), Duration::from_secs(3))
        } else {
            (Duration::from_millis(5), Duration::from_millis(200))
        };
        loop {
            if op.done {
                if let Some(lro::operation::Result::Error(e)) = &op.result {
                    return Err(Status::new(Code::from(e.code), e.message.clone()));
                }
                return Ok(op);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(max);
            op = OperationsClient::new(self.chan())
                .get_operation(lro::GetOperationRequest {
                    name: op.name.clone(),
                })
                .await?
                .into_inner();
        }
    }

    fn next_db_id(&self, prefix: char) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        if self.is_spanner() {
            return format!("{}{n}", self.db_prefix);
        }
        format!("{prefix}{}x{n}", self.run_id)
    }

    async fn create_db(&self, schema: &[String]) -> Result<String, Status> {
        let id = self.next_db_id('c');
        let name = format!("{}/databases/{id}", self.instance);
        // Logged before the request: a create that times out on our side may still finish.
        self.log_db("create", &id);
        let op = self
            .db_admin()
            .create_database(da::CreateDatabaseRequest {
                parent: self.instance.clone(),
                create_statement: format!("CREATE DATABASE `{id}`"),
                extra_statements: schema.to_vec(),
                ..Default::default()
            })
            .await;
        let op = match op {
            Ok(op) => op.into_inner(),
            Err(s) => {
                self.log_db("failed", &id);
                return Err(s);
            }
        };
        if let Err(s) = self.wait(op).await {
            // The database may exist (e.g. the operation failed on a schema statement).
            self.drop_db(&name).await;
            return Err(s);
        }
        Ok(name)
    }

    async fn drop_db(&self, db: &str) {
        let res = self
            .db_admin()
            .drop_database(da::DropDatabaseRequest {
                database: db.to_string(),
            })
            .await;
        let id = db.rsplit('/').next().unwrap_or(db);
        match res {
            Ok(_) => self.log_db("dropped", id),
            Err(s) if s.code() == Code::NotFound => self.log_db("absent", id),
            Err(s) => {
                self.log_db("drop-failed", id);
                if self.is_spanner() {
                    eprintln!("DropDatabase {db}: {s}");
                }
            }
        }
    }

    /// A fresh database holding the case's schema and prelude.
    async fn fresh_db(&self, case: &Case) -> Result<String, SetupError> {
        if !self.can_clone {
            return self.build_db(case).await;
        }
        let cell = {
            let mut map = self.templates.lock().await;
            map.entry(case.template_key()).or_default().clone()
        };
        let template = cell
            .get_or_init(|| async { self.build_db(case).await })
            .await
            .clone()?;
        let target = format!("{}/databases/{}", self.instance, self.next_db_id('k'));
        ControlClient::new(self.chan())
            .clone_database(ctl::CloneDatabaseRequest {
                source: template,
                target: target.clone(),
            })
            .await
            .map_err(|s| SetupError::status("CloneDatabase", &s))?;
        Ok(target)
    }

    /// Drops every template database this run built and forgets them. Templates still being
    /// built are left alone (only reachable on Ctrl-C).
    pub async fn drop_templates(&self) {
        let cells: Vec<TemplateCell> = self
            .templates
            .lock()
            .await
            .drain()
            .map(|(_, c)| c)
            .collect();
        for cell in cells {
            if let Some(Ok(db)) = cell.get() {
                self.drop_db(db).await;
            }
        }
    }

    async fn build_db(&self, case: &Case) -> Result<String, SetupError> {
        let db = self
            .create_db(&case.schema)
            .await
            .map_err(|s| SetupError::status("CreateDatabase", &s))?;
        let res = self.run_prelude(case, &db).await;
        if res.is_err() {
            self.drop_db(&db).await;
        }
        res.map(|()| db)
    }

    async fn run_prelude(&self, case: &Case, db: &str) -> Result<(), SetupError> {
        let mut ctx = Ctx::new(self, db)
            .await
            .map_err(|s| SetupError::status("CreateSession", &s))?;
        for step in case.prelude.iter() {
            let obs = match ctx.run(step).await {
                Ok(obs) => obs,
                Err(e) => {
                    ctx.close().await;
                    return Err(SetupError::harness(format!(
                        "prelude line {}: {e}",
                        step.line + 1
                    )));
                }
            };
            if let Some(code) = &obs.error {
                ctx.close().await;
                let msg = match &obs.message {
                    Some(crate::observe::MessageRule::Exact(m)) => m.clone(),
                    _ => String::new(),
                };
                return Err(SetupError {
                    code: Some(code_from_name(code)),
                    message: format!("prelude line {}: {code}: {msg}", step.line + 1),
                });
            }
        }
        ctx.close().await;
        Ok(())
    }

    pub async fn run_case(&self, case: &Case) -> CaseRun {
        let db = match self.fresh_db(case).await {
            Ok(db) => db,
            Err(e) => {
                return CaseRun {
                    setup: Err(e),
                    steps: Vec::new(),
                };
            }
        };
        let mut ctx = match Ctx::new(self, &db).await {
            Ok(c) => c,
            Err(s) => {
                self.drop_db(&db).await;
                return CaseRun {
                    setup: Err(SetupError::status("CreateSession", &s)),
                    steps: Vec::new(),
                };
            }
        };
        let mut steps = Vec::with_capacity(case.steps.len());
        for step in &case.steps {
            steps.push(ctx.run(step).await);
        }
        ctx.close().await;
        self.drop_db(&db).await;
        CaseRun {
            setup: Ok(()),
            steps,
        }
    }
}

/// One more than the highest `<n>` the log has for `prefix` (1 for an empty log).
/// `spanbolt-oracle-<mmdd><run>-`: the UTC date, then four base-36 characters from the clock and
/// the process id, so recorders running at the same time (in other worktrees, with other logs)
/// don't pick the same names. Leaves room for a five-digit case number.
fn oracle_db_prefix(now: Duration, pid: u32) -> String {
    let date = yyyymmdd(now.as_secs() as i64);
    let mix = (now.as_nanos() as u64 / 1000) ^ u64::from(pid).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let run = to_base36(mix % 36u64.pow(4), 4);
    format!("{ORACLE_PREFIX}{}{run}-", &date[4..])
}

fn to_base36(mut n: u64, width: usize) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    for _ in 0..width {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn code_from_name(name: &str) -> Code {
    (0..17)
        .map(Code::from)
        .find(|c| crate::observe::code_name(*c) == name)
        .unwrap_or(Code::Unknown)
}

struct Txn {
    id: Vec<u8>,
    session: String,
    seqno: i64,
    mutations: Vec<pb::Mutation>,
}

struct Ctx<'a> {
    t: &'a Target,
    db: String,
    session: String,
    sessions: Vec<String>,
    txns: HashMap<String, Txn>,
    timestamps: HashMap<String, Timestamp>,
    /// Operation names saved by `ddl op=NAME`.
    ops: HashMap<String, String>,
    /// Session names saved by `create_session session=NAME`.
    named_sessions: HashMap<String, String>,
}

impl Ctx<'_> {
    /// The case database's id, the last segment of its name.
    fn db_id(&self) -> &str {
        self.db.rsplit('/').next().unwrap_or(&self.db)
    }
}

fn rw_options() -> pb::TransactionOptions {
    pb::TransactionOptions {
        mode: Some(topts::Mode::ReadWrite(Default::default())),
        ..Default::default()
    }
}

fn ro_options(bound: topts::read_only::TimestampBound) -> pb::TransactionOptions {
    pb::TransactionOptions {
        mode: Some(topts::Mode::ReadOnly(topts::ReadOnly {
            timestamp_bound: Some(bound),
            return_read_timestamp: false,
        })),
        ..Default::default()
    }
}

fn parse_duration(s: &str) -> Result<prost_types::Duration, String> {
    let (num, unit) = s
        .find(|c: char| c.is_ascii_alphabetic())
        .map(|i| s.split_at(i))
        .ok_or_else(|| format!("duration `{s}` needs a unit"))?;
    let n: f64 = num.parse().map_err(|_| format!("bad duration `{s}`"))?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return Err(format!("bad duration unit in `{s}`")),
    };
    Ok(prost_types::Duration {
        seconds: secs.trunc() as i64,
        nanos: ((secs.fract()) * 1e9).round() as i32,
    })
}

fn obs_of(rs: &pb::ResultSet, is_query: bool) -> Observation {
    let mut obs = Observation::default();
    let fields = rs
        .metadata
        .as_ref()
        .and_then(|m| m.row_type.as_ref())
        .cloned()
        .unwrap_or_default();
    if is_query || !fields.fields.is_empty() {
        obs.columns = Some(value::fields_text(&fields));
        obs.row_types = fields
            .fields
            .iter()
            .map(|f| f.r#type.clone().unwrap_or_default())
            .collect();
        obs.rows = rs.rows.iter().map(value::row_json).collect();
    }
    if !is_query {
        obs.count = rs.stats.as_ref().and_then(|s| match s.row_count {
            Some(pb::result_set_stats::RowCount::RowCountExact(n)) => Some(n),
            Some(pb::result_set_stats::RowCount::RowCountLowerBound(n)) => Some(n),
            None => None,
        });
    }
    obs
}

impl<'a> Ctx<'a> {
    async fn new(t: &'a Target, db: &str) -> Result<Self, Status> {
        let session = Self::create_session(t, db).await?;
        Ok(Self {
            t,
            db: db.to_string(),
            session: session.clone(),
            sessions: vec![session],
            txns: HashMap::new(),
            timestamps: HashMap::new(),
            ops: HashMap::new(),
            named_sessions: HashMap::new(),
        })
    }

    async fn create_session(t: &Target, db: &str) -> Result<String, Status> {
        Ok(t.spanner()
            .create_session(pb::CreateSessionRequest {
                database: db.to_string(),
                session: None,
            })
            .await?
            .into_inner()
            .name)
    }

    async fn close(&mut self) {
        for (_, txn) in self.txns.drain() {
            let _ = self
                .t
                .spanner()
                .rollback(pb::RollbackRequest {
                    session: txn.session,
                    transaction_id: txn.id,
                })
                .await;
        }
        for s in self.sessions.drain(..) {
            let _ = self
                .t
                .spanner()
                .delete_session(pb::DeleteSessionRequest { name: s })
                .await;
        }
    }

    fn bound(&self, step: &Step) -> Result<topts::read_only::TimestampBound, String> {
        use topts::read_only::TimestampBound as B;
        let Some(spec) = step.opts.get("bound") else {
            return Ok(B::Strong(true));
        };
        let (kind, arg) = spec.split_once(':').unwrap_or((spec.as_str(), ""));
        let ts = |name: &str| {
            self.timestamps
                .get(name)
                .copied()
                .ok_or_else(|| format!("unknown saved timestamp `{name}`"))
        };
        Ok(match kind {
            "strong" => B::Strong(true),
            "read_timestamp" => B::ReadTimestamp(ts(arg)?),
            "min_read_timestamp" => B::MinReadTimestamp(ts(arg)?),
            "exact_staleness" => B::ExactStaleness(parse_duration(arg)?),
            "max_staleness" => B::MaxStaleness(parse_duration(arg)?),
            other => return Err(format!("unknown bound `{other}`")),
        })
    }

    /// Selector and session for a read/query step.
    fn read_selector(&mut self, step: &Step) -> Result<(String, Selector, i64), String> {
        match step.txn() {
            Some(name) => {
                let txn = self
                    .txns
                    .get_mut(name)
                    .ok_or_else(|| format!("no open transaction `{name}`"))?;
                txn.seqno += 1;
                Ok((txn.session.clone(), Selector::Id(txn.id.clone()), txn.seqno))
            }
            None => Ok((
                match step.opts.get("session") {
                    Some(label) => self.named_session(label)?,
                    None => self.session.clone(),
                },
                Selector::SingleUse(ro_options(self.bound(step)?)),
                0,
            )),
        }
    }

    fn params(step: &Step) -> Result<(prost_types::Struct, HashMap<String, pb::Type>), String> {
        let mut fields = std::collections::BTreeMap::new();
        let mut types = HashMap::new();
        for p in &step.params {
            let t = value::parse_type(&p.type_text)?;
            fields.insert(p.name.clone(), value::json_to_typed(&p.value, &t)?);
            types.insert(p.name.clone(), t);
        }
        Ok((prost_types::Struct { fields }, types))
    }

    fn sql_request(
        &self,
        step: &Step,
        session: String,
        selector: Selector,
        seqno: i64,
    ) -> Result<pb::ExecuteSqlRequest, String> {
        let (params, param_types) = Self::params(step)?;
        Ok(pb::ExecuteSqlRequest {
            session,
            transaction: Some(pb::TransactionSelector {
                selector: Some(selector),
            }),
            sql: step.sql(),
            params: (!params.fields.is_empty()).then_some(params),
            param_types,
            seqno,
            ..Default::default()
        })
    }

    async fn begin(
        &self,
        session: &str,
        options: pb::TransactionOptions,
    ) -> Result<Vec<u8>, Status> {
        Ok(self
            .t
            .spanner()
            .begin_transaction(pb::BeginTransactionRequest {
                session: session.to_string(),
                options: Some(options),
                ..Default::default()
            })
            .await?
            .into_inner()
            .id)
    }

    async fn commit_id(
        &self,
        session: &str,
        id: Vec<u8>,
        mutations: Vec<pb::Mutation>,
    ) -> Result<pb::CommitResponse, Status> {
        Ok(self
            .t
            .spanner()
            .commit(pb::CommitRequest {
                session: session.to_string(),
                transaction: Some(pb::commit_request::Transaction::TransactionId(id)),
                mutations,
                ..Default::default()
            })
            .await?
            .into_inner())
    }

    async fn rollback_id(&self, session: &str, id: Vec<u8>) {
        let _ = self
            .t
            .spanner()
            .rollback(pb::RollbackRequest {
                session: session.to_string(),
                transaction_id: id,
            })
            .await;
    }

    fn save_ts(&mut self, step: &Step, resp: &pb::CommitResponse) {
        if let (Some(name), Some(ts)) = (step.opts.get("save"), resp.commit_timestamp) {
            self.timestamps.insert(name.clone(), ts);
        }
    }

    async fn run(&mut self, step: &Step) -> Result<Observation, String> {
        Ok(match self.run_inner(step).await? {
            Ok(o) => o,
            Err(s) => Observation::from_status(&s),
        })
    }

    async fn run_inner(&mut self, step: &Step) -> Result<Result<Observation, Status>, String> {
        match &step.verb {
            Verb::Query => {
                let (session, sel, seqno) = self.read_selector(step)?;
                let req = self.sql_request(step, session, sel, seqno)?;
                Ok(self
                    .t
                    .spanner()
                    .execute_sql(req)
                    .await
                    .map(|r| obs_of(&r.into_inner(), true)))
            }
            Verb::Dml => self.dml(step).await,
            Verb::Pdml => {
                let session = self.session.clone();
                let opts = pb::TransactionOptions {
                    mode: Some(topts::Mode::PartitionedDml(Default::default())),
                    ..Default::default()
                };
                let id = match self.begin(&session, opts).await {
                    Ok(id) => id,
                    Err(s) => return Ok(Err(s)),
                };
                let req = self.sql_request(step, session, Selector::Id(id), 1)?;
                Ok(self
                    .t
                    .spanner()
                    .execute_sql(req)
                    .await
                    .map(|r| obs_of(&r.into_inner(), false)))
            }
            Verb::Batch => self.batch(step).await,
            Verb::Write(_) | Verb::Delete => {
                let m = mutation(step)?;
                if let Some(name) = step.txn() {
                    let txn = self
                        .txns
                        .get_mut(name)
                        .ok_or_else(|| format!("no open transaction `{name}`"))?;
                    txn.mutations.push(m);
                    return Ok(Ok(Observation::default()));
                }
                let resp = self
                    .t
                    .spanner()
                    .commit(pb::CommitRequest {
                        session: self.session.clone(),
                        transaction: Some(pb::commit_request::Transaction::SingleUseTransaction(
                            rw_options(),
                        )),
                        mutations: vec![m],
                        ..Default::default()
                    })
                    .await;
                Ok(match resp {
                    Ok(r) => {
                        self.save_ts(step, r.get_ref());
                        Ok(Observation::default())
                    }
                    Err(s) => Err(s),
                })
            }
            Verb::Read => {
                let (session, sel, _) = self.read_selector(step)?;
                let req = pb::ReadRequest {
                    session,
                    transaction: Some(pb::TransactionSelector {
                        selector: Some(sel),
                    }),
                    table: step.table.clone().unwrap_or_default(),
                    index: step.opts.get("index").cloned().unwrap_or_default(),
                    columns: cols(step),
                    key_set: Some(key_set(&step.body)?),
                    limit: match step.opts.get("limit") {
                        Some(l) => l.parse().map_err(|_| format!("bad limit `{l}`"))?,
                        None => 0,
                    },
                    ..Default::default()
                };
                Ok(self
                    .t
                    .spanner()
                    .read(req)
                    .await
                    .map(|r| obs_of(&r.into_inner(), true)))
            }
            Verb::Begin => {
                let name = step.txn().unwrap_or("default").to_string();
                if self.txns.contains_key(&name) {
                    return Err(format!("transaction `{name}` is already open"));
                }
                let options = if step.has_flag("ro") {
                    ro_options(self.bound(step)?)
                } else {
                    rw_options()
                };
                // Each transaction gets its own session: a session runs one transaction at a time.
                let session = match Self::create_session(self.t, &self.db).await {
                    Ok(s) => s,
                    Err(s) => return Ok(Err(s)),
                };
                self.sessions.push(session.clone());
                match self.begin(&session, options).await {
                    Ok(id) => {
                        self.txns.insert(
                            name,
                            Txn {
                                id,
                                session,
                                seqno: 0,
                                mutations: Vec::new(),
                            },
                        );
                        Ok(Ok(Observation::default()))
                    }
                    Err(s) => Ok(Err(s)),
                }
            }
            Verb::Commit => {
                let name = step.txn().unwrap_or("default");
                let txn = self
                    .txns
                    .remove(name)
                    .ok_or_else(|| format!("no open transaction `{name}`"))?;
                Ok(
                    match self.commit_id(&txn.session, txn.id, txn.mutations).await {
                        Ok(r) => {
                            self.save_ts(step, &r);
                            Ok(Observation::default())
                        }
                        Err(s) => Err(s),
                    },
                )
            }
            Verb::Rollback => {
                let name = step.txn().unwrap_or("default");
                let txn = self
                    .txns
                    .remove(name)
                    .ok_or_else(|| format!("no open transaction `{name}`"))?;
                Ok(self
                    .t
                    .spanner()
                    .rollback(pb::RollbackRequest {
                        session: txn.session,
                        transaction_id: txn.id,
                    })
                    .await
                    .map(|_| Observation::default()))
            }
            Verb::Ddl => {
                let id = self.db_id().to_string();
                let req = da::UpdateDatabaseDdlRequest {
                    database: self.db.clone(),
                    statements: split_statements(&step.body)
                        .into_iter()
                        .map(|s| s.replace(DB_ID, &id))
                        .collect(),
                    ..Default::default()
                };
                let res = match self.t.db_admin().update_database_ddl(req).await {
                    Ok(op) => {
                        let op = op.into_inner();
                        if let Some(label) = step.opts.get("op") {
                            self.ops.insert(label.clone(), op.name.clone());
                        }
                        self.t.wait(op).await.map(|_| ())
                    }
                    Err(s) => Err(s),
                };
                Ok(res
                    .map(|()| Observation::default())
                    .map_err(|s| Status::new(s.code(), s.message().replace(&id, DB_ID))))
            }
            Verb::GetDdl => Ok(self
                .t
                .db_admin()
                .get_database_ddl(da::GetDatabaseDdlRequest {
                    database: self.db.clone(),
                })
                .await
                .map(|r| Observation {
                    statements: Some(r.into_inner().statements),
                    ..Default::default()
                })),
            Verb::GetDatabase => Ok(self
                .t
                .db_admin()
                .get_database(da::GetDatabaseRequest {
                    name: self.db.clone(),
                })
                .await
                .map(|r| Observation {
                    columns: Some("version_retention_period STRING".into()),
                    rows: vec![Json::Array(vec![Json::String(
                        r.into_inner().version_retention_period,
                    )])],
                    ..Default::default()
                })),
            Verb::ListSessions => self.list_sessions(step).await,
            Verb::CreateSession => self.create_named_session(step).await,
            Verb::GetSession => self.get_session(step).await,
            Verb::SessionLastUse => self.session_last_use(step).await,
            Verb::DeleteSession => self.delete_session(step).await,
            Verb::GetOperation => self.get_operation(step).await,
            Verb::ListDatabaseOperations => self.list_saved_operations(false).await,
            Verb::ListOperations => self.list_saved_operations(true).await,
            Verb::DropDatabase => Ok(self
                .t
                .db_admin()
                .drop_database(da::DropDatabaseRequest {
                    database: self.db.clone(),
                })
                .await
                .map(|_| Observation::default())),
        }
    }

    /// `GetOperation` on an operation saved by `ddl op=NAME`. Observes `counts: [1]` when it is
    /// found and done (`[0]` when not done); the operation and database names in an error message
    /// become `<op>` and `<db>`.
    async fn get_operation(&mut self, step: &Step) -> Result<Result<Observation, Status>, String> {
        let label = &step.opts["op"];
        let name = self
            .ops
            .get(label)
            .ok_or_else(|| format!("unknown saved operation `{label}`"))?
            .clone();
        let res = OperationsClient::new(self.t.chan())
            .get_operation(lro::GetOperationRequest { name: name.clone() })
            .await;
        Ok(match res {
            Ok(op) => Ok(Observation {
                counts: Some(vec![i64::from(op.get_ref().done)]),
                ..Default::default()
            }),
            Err(s) => {
                let msg = s.message().replace(&name, "<op>").replace(&self.db, "<db>");
                Err(Status::new(s.code(), msg))
            }
        })
    }

    /// Pages through `ListDatabaseOperations` on the instance (`longrunning`: `ListOperations` on
    /// `<db>/operations`) and observes, as `counts: [n]`, how many of the operations saved by
    /// `ddl op=NAME` it lists. Only saved operations count: on spanbolt a case database is a clone,
    /// so it has no CreateDatabase operation of its own.
    async fn list_saved_operations(
        &mut self,
        longrunning: bool,
    ) -> Result<Result<Observation, Status>, String> {
        let instance = self
            .db
            .split("/databases/")
            .next()
            .unwrap_or_default()
            .to_string();
        let mut found = 0i64;
        let mut page_token = String::new();
        for _ in 0..10_000 {
            let resp = if longrunning {
                let req = lro::ListOperationsRequest {
                    name: format!("{}/operations", self.db),
                    page_token: page_token.clone(),
                    ..Default::default()
                };
                OperationsClient::new(self.t.chan())
                    .list_operations(req)
                    .await
                    .map(|r| {
                        let r = r.into_inner();
                        (r.operations, r.next_page_token)
                    })
            } else {
                let req = da::ListDatabaseOperationsRequest {
                    parent: instance.clone(),
                    page_token: page_token.clone(),
                    ..Default::default()
                };
                self.t
                    .db_admin()
                    .list_database_operations(req)
                    .await
                    .map(|r| {
                        let r = r.into_inner();
                        (r.operations, r.next_page_token)
                    })
            };
            let (operations, next) = match resp {
                Ok(r) => r,
                Err(s) => return Ok(Err(s)),
            };
            found += operations
                .iter()
                .filter(|o| self.ops.values().any(|n| *n == o.name))
                .count() as i64;
            if next.is_empty() {
                break;
            }
            page_token = next;
        }
        Ok(Ok(Observation {
            counts: Some(vec![found]),
            ..Default::default()
        }))
    }

    fn named_session(&self, label: &str) -> Result<String, String> {
        self.named_sessions
            .get(label)
            .cloned()
            .ok_or_else(|| format!("unknown session `{label}`"))
    }

    /// Sleeps for the step's `wait=DURATION`, if any.
    async fn wait(step: &Step) -> Result<(), String> {
        if let Some(w) = step.opts.get("wait") {
            let d = parse_duration(w)?;
            let d = Duration::new(d.seconds.max(0) as u64, d.nanos.max(0) as u32);
            tokio::time::sleep(d).await;
        }
        Ok(())
    }

    /// A saved session's name in an error message becomes `<session>`, the database's `<db>`.
    fn session_status(&self, s: Status, saved: Option<&str>) -> Status {
        let mut msg = s.message().to_string();
        if let Some(name) = saved {
            msg = msg.replace(name, "<session>");
        }
        Status::new(s.code(), msg.replace(&self.db, "<db>"))
    }

    /// `create_session session=NAME [multiplexed]`: CreateSession, saving the name under NAME.
    async fn create_named_session(
        &mut self,
        step: &Step,
    ) -> Result<Result<Observation, Status>, String> {
        let label = step.opts["session"].clone();
        let req = pb::CreateSessionRequest {
            database: self.db.clone(),
            session: Some(pb::Session {
                multiplexed: step.has_flag("multiplexed"),
                ..Default::default()
            }),
        };
        Ok(match self.t.spanner().create_session(req).await {
            Ok(r) => {
                let name = r.into_inner().name;
                self.sessions.push(name.clone());
                self.named_sessions.insert(label, name);
                Ok(Observation::default())
            }
            Err(s) => Err(s),
        })
    }

    /// `counts: [1]` when the session's `approximate_last_use_time` is at least half a second
    /// after its `create_time` (it has moved with use), else `[0]`. A coarse comparison: the
    /// targets report these times at different resolutions.
    fn last_use_observation(s: &pb::Session) -> Observation {
        let micros = |t: &Option<prost_types::Timestamp>| {
            t.as_ref()
                .map_or(0, |t| t.seconds * 1_000_000 + i64::from(t.nanos) / 1000)
        };
        let advanced = micros(&s.approximate_last_use_time) - micros(&s.create_time) >= 500_000;
        Observation {
            counts: Some(vec![i64::from(advanced)]),
            ..Default::default()
        }
    }

    /// `get_session session=NAME [wait=DURATION]`: GetSession (itself a use of the session),
    /// observed as in `last_use_observation`.
    async fn get_session(&mut self, step: &Step) -> Result<Result<Observation, Status>, String> {
        let name = self.named_session(&step.opts["session"])?;
        Self::wait(step).await?;
        let req = pb::GetSessionRequest { name: name.clone() };
        Ok(match self.t.spanner().get_session(req).await {
            Ok(r) => Ok(Self::last_use_observation(&r.into_inner())),
            Err(s) => Err(self.session_status(s, Some(&name))),
        })
    }

    /// `session_last_use session=NAME [wait=DURATION]`: finds the session in ListSessions (which
    /// does not count as a use) and observes it as in `last_use_observation`; `counts: [-1]` when
    /// it is not listed.
    async fn session_last_use(
        &mut self,
        step: &Step,
    ) -> Result<Result<Observation, Status>, String> {
        let name = self.named_session(&step.opts["session"])?;
        Self::wait(step).await?;
        let mut page_token = String::new();
        for _ in 0..1000 {
            let req = pb::ListSessionsRequest {
                database: self.db.clone(),
                page_size: 0,
                page_token: page_token.clone(),
                filter: String::new(),
            };
            let resp = match self.t.spanner().list_sessions(req).await {
                Ok(r) => r.into_inner(),
                Err(s) => return Ok(Err(s)),
            };
            if let Some(s) = resp.sessions.iter().find(|s| s.name == name) {
                return Ok(Ok(Self::last_use_observation(s)));
            }
            if resp.next_page_token.is_empty() {
                break;
            }
            page_token = resp.next_page_token;
        }
        Ok(Ok(Observation {
            counts: Some(vec![-1]),
            ..Default::default()
        }))
    }

    /// `delete_session session=NAME` (a saved session) or `delete_session name=TEXT` (a literal
    /// name in which `<db>` stands for the case database).
    async fn delete_session(&mut self, step: &Step) -> Result<Result<Observation, Status>, String> {
        let (name, saved) = match (step.opts.get("session"), step.opts.get("name")) {
            (Some(label), _) => (self.named_session(label)?, true),
            (None, Some(text)) => (text.replace("<db>", &self.db), false),
            (None, None) => return Err("`delete_session` needs session= or name=".into()),
        };
        let req = pb::DeleteSessionRequest { name: name.clone() };
        Ok(match self.t.spanner().delete_session(req).await {
            Ok(_) => Ok(Observation::default()),
            Err(s) => Err(self.session_status(s, saved.then_some(name.as_str()))),
        })
    }

    /// Creates `create=K` sessions, then pages through ListSessions with `page_size=N`; observes
    /// the size of each page as `counts:`.
    async fn list_sessions(&mut self, step: &Step) -> Result<Result<Observation, Status>, String> {
        let opt = |k: &str| -> Result<i32, String> {
            step.opts
                .get(k)
                .map_or(Ok(0), |v| v.parse().map_err(|_| format!("bad {k}={v}")))
        };
        let (create, page_size) = (opt("create")?, opt("page_size")?);
        for _ in 0..create {
            match Self::create_session(self.t, &self.db).await {
                Ok(s) => self.sessions.push(s),
                Err(s) => return Ok(Err(s)),
            }
        }
        let mut counts = Vec::new();
        let mut page_token = String::new();
        loop {
            let req = pb::ListSessionsRequest {
                database: self.db.clone(),
                page_size,
                page_token: page_token.clone(),
                filter: String::new(),
            };
            let resp = match self.t.spanner().list_sessions(req).await {
                Ok(r) => r.into_inner(),
                Err(s) => return Ok(Err(s)),
            };
            counts.push(resp.sessions.len() as i64);
            if resp.next_page_token.is_empty() || counts.len() > 1000 {
                break;
            }
            page_token = resp.next_page_token;
        }
        Ok(Ok(Observation {
            counts: Some(counts),
            ..Default::default()
        }))
    }

    async fn dml(&mut self, step: &Step) -> Result<Result<Observation, Status>, String> {
        if let Some(name) = step.txn() {
            let txn = self
                .txns
                .get_mut(name)
                .ok_or_else(|| format!("no open transaction `{name}`"))?;
            txn.seqno += 1;
            let (session, id, seqno) = (txn.session.clone(), txn.id.clone(), txn.seqno);
            let req = self.sql_request(step, session, Selector::Id(id), seqno)?;
            return Ok(self
                .t
                .spanner()
                .execute_sql(req)
                .await
                .map(|r| obs_of(&r.into_inner(), false)));
        }
        let session = self.session.clone();
        let id = match self.begin(&session, rw_options()).await {
            Ok(id) => id,
            Err(s) => return Ok(Err(s)),
        };
        let req = self.sql_request(step, session.clone(), Selector::Id(id.clone()), 1)?;
        let obs = match self.t.spanner().execute_sql(req).await {
            Ok(r) => obs_of(&r.into_inner(), false),
            Err(s) => {
                self.rollback_id(&session, id).await;
                return Ok(Err(s));
            }
        };
        Ok(match self.commit_id(&session, id, Vec::new()).await {
            Ok(r) => {
                self.save_ts(step, &r);
                Ok(obs)
            }
            Err(s) => Err(s),
        })
    }

    async fn batch(&mut self, step: &Step) -> Result<Result<Observation, Status>, String> {
        let statements = split_statements(&step.body)
            .into_iter()
            .map(|sql| pb::execute_batch_dml_request::Statement {
                sql,
                ..Default::default()
            })
            .collect();
        let (session, id, seqno, auto) = match step.txn() {
            Some(name) => {
                let txn = self
                    .txns
                    .get_mut(name)
                    .ok_or_else(|| format!("no open transaction `{name}`"))?;
                txn.seqno += 1;
                (txn.session.clone(), txn.id.clone(), txn.seqno, false)
            }
            None => {
                let session = self.session.clone();
                match self.begin(&session, rw_options()).await {
                    Ok(id) => (session, id, 1, true),
                    Err(s) => return Ok(Err(s)),
                }
            }
        };
        let req = pb::ExecuteBatchDmlRequest {
            session: session.clone(),
            transaction: Some(pb::TransactionSelector {
                selector: Some(Selector::Id(id.clone())),
            }),
            statements,
            seqno,
            ..Default::default()
        };
        let resp = match self.t.spanner().execute_batch_dml(req).await {
            Ok(r) => r.into_inner(),
            Err(s) => {
                if auto {
                    self.rollback_id(&session, id).await;
                }
                return Ok(Err(s));
            }
        };
        let counts = resp
            .result_sets
            .iter()
            .map(|rs| obs_of(rs, false).count.unwrap_or(-1))
            .collect();
        let mut obs = Observation {
            counts: Some(counts),
            ..Default::default()
        };
        let failed = resp.status.as_ref().is_some_and(|s| s.code != 0);
        if let Some(st) = resp.status.as_ref().filter(|s| s.code != 0) {
            let e = Observation::error(crate::observe::code_name(Code::from(st.code)), &st.message);
            obs.error = e.error;
            obs.message = e.message;
        }
        if auto {
            if failed {
                self.rollback_id(&session, id).await;
            } else if let Err(s) = self.commit_id(&session, id, Vec::new()).await {
                return Ok(Err(s));
            }
        }
        Ok(Ok(obs))
    }
}

fn cols(step: &Step) -> Vec<String> {
    step.opts
        .get("cols")
        .map(|c| c.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default()
}

fn mutation(step: &Step) -> Result<pb::Mutation, String> {
    use pb::mutation::Operation as Op;
    let table = step.table.clone().unwrap_or_default();
    let op = match &step.verb {
        Verb::Delete => Op::Delete(pb::mutation::Delete {
            table,
            key_set: Some(key_set(&step.body)?),
        }),
        Verb::Write(kind) => {
            let mut values = Vec::new();
            for line in step.body.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
                let j: serde_json::Value =
                    serde_json::from_str(line).map_err(|e| format!("row `{line}`: {e}"))?;
                values.push(value::json_row(&j)?);
            }
            let w = pb::mutation::Write {
                table,
                columns: cols(step),
                values,
            };
            match kind {
                WriteKind::Insert => Op::Insert(w),
                WriteKind::Update => Op::Update(w),
                WriteKind::InsertOrUpdate => Op::InsertOrUpdate(w),
                WriteKind::Replace => Op::Replace(w),
            }
        }
        _ => return Err("not a mutation".into()),
    };
    Ok(pb::Mutation {
        operation: Some(op),
    })
}

/// Key set lines: `all`, `[k1, k2]`, or `range [a] .. [b]` with `..`, `..<`, `<..`, `<..<`.
pub fn key_set(body: &[String]) -> Result<pb::KeySet, String> {
    use pb::key_range::{EndKeyType, StartKeyType};
    let mut ks = pb::KeySet::default();
    let parse_key = |s: &str| -> Result<ListValue, String> {
        let j: serde_json::Value =
            serde_json::from_str(s.trim()).map_err(|e| format!("key `{s}`: {e}"))?;
        value::json_row(&j)
    };
    for line in body.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        if line == "all" {
            ks.all = true;
        } else if let Some(r) = line.strip_prefix("range ") {
            let (op, (start, end)) = [" <..< ", " <.. ", " ..< ", " .. "]
                .iter()
                .find_map(|op| r.split_once(op).map(|p| (op.trim(), p)))
                .ok_or_else(|| format!("bad range `{line}`"))?;
            let (start, end) = (parse_key(start)?, parse_key(end)?);
            let start_open = op.starts_with('<');
            let end_open = op.ends_with('<');
            ks.ranges.push(pb::KeyRange {
                start_key_type: Some(if start_open {
                    StartKeyType::StartOpen(start)
                } else {
                    StartKeyType::StartClosed(start)
                }),
                end_key_type: Some(if end_open {
                    EndKeyType::EndOpen(end)
                } else {
                    EndKeyType::EndClosed(end)
                }),
            });
        } else {
            ks.keys.push(parse_key(line)?);
        }
    }
    Ok(ks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_sets() {
        let body: Vec<String> = ["[1, \"a\"]", "range [1] ..< [5]", "range [] <.. [\"x\"]"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let ks = key_set(&body).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(ks.keys.len(), 1);
        assert!(matches!(
            ks.ranges[0].end_key_type,
            Some(pb::key_range::EndKeyType::EndOpen(_))
        ));
        assert!(matches!(
            ks.ranges[1].start_key_type,
            Some(pb::key_range::StartKeyType::StartOpen(_))
        ));
        assert!(!ks.all);
    }

    #[test]
    fn oracle_db_names_fit_and_differ_per_run() {
        let now = Duration::from_secs(1_795_553_832);
        let a = oracle_db_prefix(now, 100);
        let b = oracle_db_prefix(now, 101);
        assert!(a.starts_with("spanbolt-oracle-1124"), "{a}");
        assert_ne!(a, b);
        // Spanner allows database ids of at most 30 characters.
        assert!(format!("{a}99999").len() <= 30, "{a}");
    }

    #[test]
    fn durations() {
        let d = parse_duration("1500ms").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((d.seconds, d.nanos), (1, 500_000_000));
        assert!(parse_duration("10").is_err());
    }
}
