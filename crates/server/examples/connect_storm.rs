//! Many clients connecting at once to a server that is busy with DDL and queries.
//!
//! Start spanbolt, then:
//! `cargo run --release -p spanbolt-server --example connect_storm -- HOST:PORT [clients] [load]`
//!
//! `load` background clients (default 24) loop on creating a database with a large schema,
//! querying it and dropping it. After two seconds `clients` (default 40) new clients each open
//! their own connection at the same moment and create an instance. Prints the connect time and the
//! time to a created instance per client, and exits non-zero if any client took 1 s or more.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use spanbolt_proto::database_admin as da;
use spanbolt_proto::database_admin::database_admin_client::DatabaseAdminClient;
use spanbolt_proto::instance_admin as ia;
use spanbolt_proto::instance_admin::instance_admin_client::InstanceAdminClient;
use spanbolt_proto::spanner as pb;
use spanbolt_proto::spanner::spanner_client::SpannerClient;
use tonic::transport::{Channel, Endpoint};

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

const TABLES: usize = 300;
const HEAVY: &str = "SELECT COUNT(*) FROM UNNEST(GENERATE_ARRAY(1, 10000)) AS a, \
                     UNNEST(GENERATE_ARRAY(1, 10000)) AS b WHERE MOD(a + b, 7) = 0";

fn schema() -> Vec<String> {
    let mut out = Vec::new();
    for t in 0..TABLES {
        let cols: String = (0..12)
            .map(|c| format!(", C{c} STRING(MAX)"))
            .collect::<String>();
        out.push(format!(
            "CREATE TABLE T{t} (Id INT64 NOT NULL{cols}, N INT64) PRIMARY KEY (Id)"
        ));
        out.push(format!("CREATE INDEX T{t}ByC0 ON T{t} (C0, N)"));
    }
    out
}

/// One project per run, so runs against the same server don't collide.
fn project() -> String {
    format!("projects/storm{}", std::process::id())
}

fn endpoint(target: &str) -> Result<Endpoint, BoxErr> {
    Ok(Channel::from_shared(format!("http://{target}"))?.connect_timeout(Duration::from_secs(30)))
}

async fn create_instance(ch: Channel, id: &str) -> Result<(), BoxErr> {
    InstanceAdminClient::new(ch)
        .create_instance(ia::CreateInstanceRequest {
            parent: project(),
            instance_id: id.into(),
            instance: Some(ia::Instance {
                config: "emulator-config".into(),
                ..Default::default()
            }),
        })
        .await?;
    Ok(())
}

async fn load(target: String, n: usize, stop: Arc<AtomicBool>, rounds: Arc<AtomicU64>) {
    let run = async {
        let ch = endpoint(&target)?.connect().await?;
        let inst = format!("load{n}");
        create_instance(ch.clone(), &inst).await?;
        let ddl = schema();
        let mut round = 0;
        while !stop.load(Ordering::Relaxed) {
            let db = format!("{}/instances/{inst}/databases/d{round}", project());
            DatabaseAdminClient::new(ch.clone())
                .create_database(da::CreateDatabaseRequest {
                    parent: format!("{}/instances/{inst}", project()),
                    create_statement: format!("CREATE DATABASE d{round}"),
                    extra_statements: ddl.clone(),
                    ..Default::default()
                })
                .await?;
            let mut spanner = SpannerClient::new(ch.clone());
            let session = spanner
                .create_session(pb::CreateSessionRequest {
                    database: db.clone(),
                    session: None,
                })
                .await?
                .into_inner();
            for t in 0..20 {
                spanner
                    .execute_sql(pb::ExecuteSqlRequest {
                        session: session.name.clone(),
                        sql: format!("SELECT Id, C0 FROM T{t} WHERE C0 = 'x' ORDER BY N"),
                        ..Default::default()
                    })
                    .await?;
            }
            // A CPU-bound query that holds the server for a while.
            spanner
                .execute_sql(pb::ExecuteSqlRequest {
                    session: session.name.clone(),
                    sql: HEAVY.into(),
                    ..Default::default()
                })
                .await?;
            DatabaseAdminClient::new(ch.clone())
                .drop_database(da::DropDatabaseRequest { database: db })
                .await?;
            rounds.fetch_add(1, Ordering::Relaxed);
            round += 1;
        }
        Ok::<_, BoxErr>(())
    };
    if let Err(e) = run.await {
        eprintln!("load client {n}: {e}");
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxErr> {
    let mut args = std::env::args().skip(1);
    let target = args.next().unwrap_or_else(|| "127.0.0.1:9010".into());
    let clients: usize = args.next().map_or(Ok(40), |a| a.parse())?;
    let loaders: usize = args.next().map_or(Ok(24), |a| a.parse())?;

    let stop = Arc::new(AtomicBool::new(false));
    let rounds = Arc::new(AtomicU64::new(0));
    let load_tasks: Vec<_> = (0..loaders)
        .map(|n| tokio::spawn(load(target.clone(), n, stop.clone(), rounds.clone())))
        .collect();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let gate = Arc::new(tokio::sync::Barrier::new(clients));
    let started = Instant::now();
    let storm: Vec<_> = (0..clients)
        .map(|n| {
            let target = target.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                gate.wait().await;
                let t0 = Instant::now();
                let ch = endpoint(&target)?.connect().await?;
                let connected = t0.elapsed();
                create_instance(ch, &format!("storm{n}")).await?;
                Ok::<_, BoxErr>((connected, t0.elapsed()))
            })
        })
        .collect();
    let mut results = Vec::new();
    for t in storm {
        results.push(t.await??);
    }
    let wall = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    for t in load_tasks {
        let _ = t.await;
    }

    results.sort_by_key(|r| r.1);
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let pct = |p: f64| results[((results.len() as f64 * p) as usize).min(results.len() - 1)];
    let max_connect = results.iter().map(|r| r.0).max().unwrap_or_default();
    println!(
        "{clients} clients under {loaders} load clients ({} load rounds): connect max {:.1} ms; \
         instance created p50 {:.1} ms, p90 {:.1} ms, max {:.1} ms; wall {:.1} ms",
        rounds.load(Ordering::Relaxed),
        ms(max_connect),
        ms(pct(0.5).1),
        ms(pct(0.9).1),
        ms(results.last().map_or(Duration::ZERO, |r| r.1)),
        ms(wall),
    );
    if results.iter().any(|r| r.1 >= Duration::from_secs(1)) {
        std::process::exit(1);
    }
    Ok(())
}
