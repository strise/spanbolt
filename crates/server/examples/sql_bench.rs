//! SQL latency over gRPC (tonic client) against an in-process server with 100k rows:
//! `source scripts/googlesql-env.sh && cargo run --release -p spanbolt-server --features googlesql --example sql_bench`

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use prost_types::value::Kind;
use prost_types::{ListValue, Value};
use spanbolt_proto::database_admin as da;
use spanbolt_proto::database_admin::database_admin_client::DatabaseAdminClient;
use spanbolt_proto::instance_admin as ia;
use spanbolt_proto::instance_admin::instance_admin_client::InstanceAdminClient;
use spanbolt_proto::spanner as pb;
use spanbolt_proto::spanner::spanner_client::SpannerClient;
use spanbolt_server::Emulator;
use tonic::transport::Channel;

/// The allocator the `spanbolt` binary uses (src/main.rs), so the numbers match it.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

const DB: &str = "projects/bench/instances/bench/databases/bench";
const ROWS: i64 = 100_000;

fn s(v: &str) -> Value {
    Value {
        kind: Some(Kind::StringValue(v.into())),
    }
}

fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let pct = |p: f64| samples[((samples.len() as f64 * p) as usize).min(samples.len() - 1)];
    let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    println!(
        "{name:<34} n={:<5} mean={:>8.1}µs p50={:>8.1}µs p99={:>8.1}µs",
        samples.len(),
        mean.as_secs_f64() * 1e6,
        pct(0.5).as_secs_f64() * 1e6,
        pct(0.99).as_secs_f64() * 1e6
    );
}

fn params(
    p: &[(&str, pb::TypeCode, Value)],
) -> (Option<prost_types::Struct>, HashMap<String, pb::Type>) {
    let mut fields = std::collections::BTreeMap::new();
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

#[tokio::main]
async fn main() -> Result<(), BoxErr> {
    let engine = Arc::new(spanbolt_sql::Engine::default());
    let emu = Arc::new(Emulator::new(engine.clone()));
    let server = spanbolt_server::spawn("127.0.0.1:0", emu.clone()).await?;
    let channel = Channel::from_shared(format!("http://{}", server.addr))?
        .connect()
        .await?;
    InstanceAdminClient::new(channel.clone())
        .create_instance(ia::CreateInstanceRequest {
            parent: "projects/bench".into(),
            instance_id: "bench".into(),
            instance: Some(ia::Instance {
                config: "emulator-config".into(),
                ..Default::default()
            }),
        })
        .await?;
    DatabaseAdminClient::new(channel.clone())
        .create_database(da::CreateDatabaseRequest {
            parent: "projects/bench/instances/bench".into(),
            create_statement: "CREATE DATABASE bench".into(),
            extra_statements: vec![
                "CREATE TABLE Teams (TeamId INT64 NOT NULL, Name STRING(MAX)) PRIMARY KEY (TeamId)".into(),
                "CREATE TABLE Users (UserId INT64 NOT NULL, TeamId INT64, Email STRING(MAX), Score INT64) PRIMARY KEY (UserId)".into(),
                "CREATE INDEX UsersByEmail ON Users(Email)".into(),
                "CREATE TABLE Orders (UserId INT64 NOT NULL, OrderId INT64 NOT NULL, Amount INT64) PRIMARY KEY (UserId, OrderId), INTERLEAVE IN PARENT Users ON DELETE CASCADE".into(),
            ],
            ..Default::default()
        })
        .await?;
    let mut sp = SpannerClient::new(channel.clone());
    let session = sp
        .create_session(pb::CreateSessionRequest {
            database: DB.into(),
            session: None,
        })
        .await?
        .into_inner()
        .name;
    let single_rw = || {
        Some(pb::commit_request::Transaction::SingleUseTransaction(
            pb::TransactionOptions {
                mode: Some(pb::transaction_options::Mode::ReadWrite(Default::default())),
                ..Default::default()
            },
        ))
    };
    let write = |table: &str, cols: &[&str], rows: Vec<ListValue>| pb::Mutation {
        operation: Some(pb::mutation::Operation::Insert(pb::mutation::Write {
            table: table.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            values: rows,
        })),
    };
    let t0 = Instant::now();
    let teams: Vec<ListValue> = (0..100)
        .map(|t| ListValue {
            values: vec![s(&t.to_string()), s(&format!("team-{t}"))],
        })
        .collect();
    sp.commit(pb::CommitRequest {
        session: session.clone(),
        transaction: single_rw(),
        mutations: vec![write("Teams", &["TeamId", "Name"], teams)],
        ..Default::default()
    })
    .await?;
    for chunk in (0..ROWS).collect::<Vec<_>>().chunks(5000) {
        let users = chunk
            .iter()
            .map(|i| ListValue {
                values: vec![
                    s(&i.to_string()),
                    s(&(i % 100).to_string()),
                    s(&format!("user{i}@x.io")),
                    s(&(i % 1000).to_string()),
                ],
            })
            .collect();
        let orders = chunk
            .iter()
            .map(|i| ListValue {
                values: vec![s(&i.to_string()), s("1"), s(&(i % 77).to_string())],
            })
            .collect();
        sp.commit(pb::CommitRequest {
            session: session.clone(),
            transaction: single_rw(),
            mutations: vec![
                write("Users", &["UserId", "TeamId", "Email", "Score"], users),
                write("Orders", &["UserId", "OrderId", "Amount"], orders),
            ],
            ..Default::default()
        })
        .await?;
    }
    println!(
        "seeded {ROWS} users + {ROWS} orders in {:.0} ms",
        t0.elapsed().as_secs_f64() * 1e3
    );

    let queries: Vec<(&str, &str, &str, pb::TypeCode)> = vec![
        (
            "PK point query",
            "SELECT UserId, Email, Score FROM Users WHERE UserId = @k",
            "k",
            pb::TypeCode::Int64,
        ),
        (
            "index equality",
            "SELECT UserId, Email FROM Users WHERE Email = @k",
            "k",
            pb::TypeCode::String,
        ),
        (
            "3-table join",
            "SELECT u.Email, t.Name, o.Amount FROM Users u JOIN Teams t ON t.TeamId = u.TeamId JOIN Orders o ON o.UserId = u.UserId WHERE u.UserId = @k",
            "k",
            pb::TypeCode::Int64,
        ),
    ];
    for (name, sql, p, ty) in &queries {
        let mut samples = Vec::new();
        for i in 0..2000i64 {
            let k = (i * 7919) % ROWS;
            let v = if *ty == pb::TypeCode::String {
                s(&format!("user{k}@x.io"))
            } else {
                s(&k.to_string())
            };
            let (params, param_types) = params(&[(p, *ty, v)]);
            let t = Instant::now();
            let rs = sp
                .execute_sql(pb::ExecuteSqlRequest {
                    session: session.clone(),
                    sql: sql.to_string(),
                    params,
                    param_types,
                    ..Default::default()
                })
                .await?
                .into_inner();
            samples.push(t.elapsed());
            assert_eq!(rs.rows.len(), 1, "{name}");
        }
        report(name, samples);
    }

    let mut samples = Vec::new();
    for i in 0..1000i64 {
        let t = Instant::now();
        let id = ROWS + i;
        let (params, param_types) = params(&[("id", pb::TypeCode::Int64, s(&id.to_string()))]);
        let rs = sp
            .execute_sql(pb::ExecuteSqlRequest {
                session: session.clone(),
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
                sql: "INSERT INTO Users (UserId, TeamId, Email) VALUES (@id, 1, 'new@x.io')".into(),
                params,
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
            ..Default::default()
        })
        .await?;
        samples.push(t.elapsed());
    }
    report("INSERT DML + commit (RW txn)", samples);

    let state = emu.database(DB)?.db.latest().state.clone();
    let (hits, misses) = engine.cache_stats(&state);
    println!(
        "analysis cache: {hits} hits, {misses} misses ({:.2}% hit rate)",
        100.0 * hits as f64 / (hits + misses).max(1) as f64
    );
    Ok(())
}
