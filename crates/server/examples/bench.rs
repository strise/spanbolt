//! Latency and throughput over gRPC. Run with `cargo run --release -p spanbolt-server --example bench`
//! (builds the `spanbolt` binary first for the startup measurement: `cargo build --release`).

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

fn s(v: &str) -> Value {
    Value {
        kind: Some(Kind::StringValue(v.into())),
    }
}

fn rw() -> Option<pb::commit_request::Transaction> {
    Some(pb::commit_request::Transaction::SingleUseTransaction(
        pb::TransactionOptions {
            mode: Some(pb::transaction_options::Mode::ReadWrite(Default::default())),
            ..Default::default()
        },
    ))
}

fn put(id: i64) -> pb::Mutation {
    pb::Mutation {
        operation: Some(pb::mutation::Operation::InsertOrUpdate(
            pb::mutation::Write {
                table: "T".into(),
                columns: vec!["Id".into(), "V".into()],
                values: vec![ListValue {
                    values: vec![s(&id.to_string()), s("payload-0123456789")],
                }],
            },
        )),
    }
}

fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let pct = |p: f64| samples[((samples.len() as f64 * p) as usize).min(samples.len() - 1)];
    let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    println!(
        "{name:<28} n={:<6} mean={:>7.1}µs p50={:>7.1}µs p99={:>7.1}µs",
        samples.len(),
        mean.as_secs_f64() * 1e6,
        pct(0.5).as_secs_f64() * 1e6,
        pct(0.99).as_secs_f64() * 1e6
    );
}

async fn startup() -> Result<(), BoxErr> {
    let exe = std::env::current_exe()?;
    let Some(bin) = exe
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("spanbolt"))
    else {
        return Ok(());
    };
    if !bin.exists() {
        println!("startup: skipped (build the binary with `cargo build --release`)");
        return Ok(());
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let mut runs = Vec::new();
    for _ in 0..10 {
        let t0 = Instant::now();
        let mut child = std::process::Command::new(&bin)
            .args(["--host-port", &format!("127.0.0.1:{port}")])
            .stderr(std::process::Stdio::null())
            .spawn()?;
        loop {
            if let Ok(ch) = Channel::from_shared(format!("http://127.0.0.1:{port}"))?
                .connect()
                .await
            {
                let mut c = InstanceAdminClient::new(ch);
                let req = ia::ListInstanceConfigsRequest {
                    parent: "projects/p".into(),
                    ..Default::default()
                };
                if c.list_instance_configs(req).await.is_ok() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_micros(200)).await;
        }
        runs.push(t0.elapsed());
        child.kill()?;
        child.wait()?;
    }
    report("startup to first RPC", runs);
    Ok(())
}

/// Engine cost without gRPC: commits and point reads straight on a `Database`.
fn in_process() -> Result<(), BoxErr> {
    use spanbolt_storage::{DbState, KeySet, Mutation, Target, WriteKind, scan};
    use spanbolt_txn::{Clock, Database, ReadOnlyTxn, ReadWriteTxn};
    let schema = spanbolt_storage::testing::ddl(&[
        "CREATE TABLE T (Id INT64 NOT NULL, V STRING(MAX)) PRIMARY KEY (Id)",
    ]);
    let db = Arc::new(Database::new(
        Arc::new(Clock::default()),
        DbState::new(schema)?,
    ));
    let cols = ["Id".to_string(), "V".to_string()];
    let mut samples = Vec::new();
    for i in 0..20000i64 {
        let t0 = Instant::now();
        let txn = ReadWriteTxn::new(db.clone());
        let cat = txn.catalog();
        let m = Mutation::write(
            &cat,
            WriteKind::InsertOrUpdate,
            "T",
            &cols,
            &[ListValue {
                values: vec![s(&i.to_string()), s("payload")],
            }],
        )?;
        txn.commit(&cat, &[m])?;
        samples.push(t0.elapsed());
    }
    report("in-process commit", samples);
    let mut samples = Vec::new();
    for i in 0..100000i64 {
        let t0 = Instant::now();
        let ro = ReadOnlyTxn {
            version: db.latest(),
        };
        let view = ro.read(|c| {
            Ok((
                Target::Table(c.lookup_table("T")?),
                KeySet::keys(vec![vec![spanbolt_storage::Value::Int64(i % 20000)]]),
            ))
        })?;
        let n = scan(view.state.map(view.target), &view.spans).count();
        samples.push(t0.elapsed());
        assert_eq!(n, 1);
    }
    report("in-process point read", samples);
    let t0 = Instant::now();
    let clone = db.clone_database();
    let elapsed = t0.elapsed();
    println!(
        "in-process clone (20k rows)   {:>7.2}µs ({} rows shared)",
        elapsed.as_secs_f64() * 1e6,
        clone.latest().state.row_count()
    );
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), BoxErr> {
    in_process()?;
    startup().await?;

    let server = spanbolt_server::spawn("127.0.0.1:0", Arc::new(Emulator::default())).await?;
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
                "CREATE TABLE T (Id INT64 NOT NULL, V STRING(MAX)) PRIMARY KEY (Id)".into(),
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

    for i in 0..200 {
        sp.commit(pb::CommitRequest {
            session: session.clone(),
            transaction: rw(),
            mutations: vec![put(i)],
            ..Default::default()
        })
        .await?;
    }
    let mut samples = Vec::new();
    for i in 0..5000 {
        let t0 = Instant::now();
        sp.commit(pb::CommitRequest {
            session: session.clone(),
            transaction: rw(),
            mutations: vec![put(i % 1000)],
            ..Default::default()
        })
        .await?;
        samples.push(t0.elapsed());
    }
    report("single-row commit", samples);

    let mut samples = Vec::new();
    for i in 0..10000 {
        let req = pb::ReadRequest {
            session: session.clone(),
            table: "T".into(),
            columns: vec!["Id".into(), "V".into()],
            key_set: Some(pb::KeySet {
                keys: vec![ListValue {
                    values: vec![s(&(i % 1000).to_string())],
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let t0 = Instant::now();
        let rs = sp.read(req).await?.into_inner();
        samples.push(t0.elapsed());
        assert_eq!(rs.rows.len(), 1);
    }
    report("point read", samples);

    for (label, rw_txn) in [
        ("8-way single-use commits", false),
        ("8-way read+commit txns", true),
    ] {
        let per_task = 3000;
        let t0 = Instant::now();
        let mut tasks = Vec::new();
        for t in 0..8i64 {
            let mut sp = SpannerClient::new(channel.clone());
            let session = sp
                .create_session(pb::CreateSessionRequest {
                    database: DB.into(),
                    session: None,
                })
                .await?
                .into_inner()
                .name;
            tasks.push(tokio::spawn(async move {
                let mut aborts = 0u32;
                for i in 0..per_task {
                    let id = 1_000_000 + t * 1_000_000 + i;
                    let transaction = if rw_txn {
                        let read = pb::ReadRequest {
                            session: session.clone(),
                            table: "T".into(),
                            columns: vec!["V".into()],
                            key_set: Some(pb::KeySet {
                                keys: vec![ListValue {
                                    values: vec![s(&id.to_string())],
                                }],
                                ..Default::default()
                            }),
                            transaction: Some(pb::TransactionSelector {
                                selector: Some(pb::transaction_selector::Selector::Begin(
                                    pb::TransactionOptions {
                                        mode: Some(pb::transaction_options::Mode::ReadWrite(
                                            Default::default(),
                                        )),
                                        ..Default::default()
                                    },
                                )),
                            }),
                            ..Default::default()
                        };
                        let rs = sp.read(read).await?.into_inner();
                        let id = rs
                            .metadata
                            .and_then(|m| m.transaction)
                            .map(|t| t.id)
                            .unwrap_or_default();
                        Some(pb::commit_request::Transaction::TransactionId(id))
                    } else {
                        rw()
                    };
                    let req = pb::CommitRequest {
                        session: session.clone(),
                        transaction,
                        mutations: vec![put(id)],
                        ..Default::default()
                    };
                    match sp.commit(req).await {
                        Ok(_) => {}
                        Err(e) if e.code() == tonic::Code::Aborted => aborts += 1,
                        Err(e) => return Err(e),
                    }
                }
                Ok::<u32, tonic::Status>(aborts)
            }));
        }
        let mut aborts = 0;
        for t in tasks {
            aborts += t.await??;
        }
        let elapsed = t0.elapsed();
        println!(
            "{label:<28} {:>9.0} commits/s  ({} commits in {:.2}s, {aborts} aborts)",
            (8 * per_task) as f64 / elapsed.as_secs_f64(),
            8 * per_task,
            elapsed.as_secs_f64()
        );
    }
    Ok(())
}
