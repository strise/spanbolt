//! SEARCH and SNIPPET latency over gRPC (tonic client) against an in-process server, 10k rows
//! with a search index (Spanner rejects SEARCH without one, and so does spanbolt by default):
//! `source scripts/googlesql-env.sh && cargo run --release -p spanbolt-server --features googlesql --example search_bench`

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

const ROWS: i64 = 10_000;

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

const FIRST: &[&str] = &[
    "Ola", "Kari", "Per", "Anne", "Nils", "Ingrid", "Lars", "Sigrid", "Erik", "Marit",
];
const LAST: &[&str] = &[
    "Testmann", "Nordmann", "Fjordli", "Bakken", "Dalen", "Haugen", "Lien", "Moen", "Strand", "Vik",
];
const KIND: &[&str] = &["Holding", "Invest", "Shipping", "Consulting", "Eiendom"];

fn name(i: i64) -> String {
    let u = i as usize;
    format!(
        "{} {} {} {} AS",
        FIRST[u % 10],
        LAST[(u / 10) % 10],
        KIND[(u / 100) % 5],
        format_args!("Unik{i}")
    )
}

#[tokio::main]
async fn main() -> Result<(), BoxErr> {
    let engine = Arc::new(spanbolt_sql::Engine::default());
    spanbolt_storage::expr::set_expr_evaluator(Some(Arc::new(spanbolt_sql::SchemaExprs::new()?)));
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
    let table = "CREATE TABLE Entities (Id INT64 NOT NULL, Name STRING(MAX), Country STRING(2), Name_Tokens TOKENLIST AS (TOKENIZE_FULLTEXT(Name)) HIDDEN, Name_Ngrams TOKENLIST AS (TOKENIZE_NGRAMS(Name, ngram_size_max => 3, ngram_size_min => 3)) HIDDEN) PRIMARY KEY (Id)";
    for (db, indexed) in [("indexed", true)] {
        let mut extra = vec![table.to_string()];
        if indexed {
            extra.push(
                "CREATE SEARCH INDEX EntitiesSearch ON Entities(Name_Tokens) STORING (Country)"
                    .into(),
            );
            extra.push("CREATE SEARCH INDEX EntitiesNgrams ON Entities(Name_Ngrams)".into());
        }
        DatabaseAdminClient::new(channel.clone())
            .create_database(da::CreateDatabaseRequest {
                parent: "projects/bench/instances/bench".into(),
                create_statement: format!("CREATE DATABASE {db}"),
                extra_statements: extra,
                ..Default::default()
            })
            .await?;
        let path = format!("projects/bench/instances/bench/databases/{db}");
        let mut sp = SpannerClient::new(channel.clone());
        let session = sp
            .create_session(pb::CreateSessionRequest {
                database: path.clone(),
                session: None,
            })
            .await?
            .into_inner()
            .name;
        let t0 = Instant::now();
        for chunk in (0..ROWS).collect::<Vec<_>>().chunks(2000) {
            let rows = chunk
                .iter()
                .map(|i| ListValue {
                    values: vec![
                        s(&i.to_string()),
                        s(&name(*i)),
                        s(if i % 3 == 0 { "SE" } else { "NO" }),
                    ],
                })
                .collect();
            sp.commit(pb::CommitRequest {
                session: session.clone(),
                transaction: Some(pb::commit_request::Transaction::SingleUseTransaction(
                    pb::TransactionOptions {
                        mode: Some(pb::transaction_options::Mode::ReadWrite(Default::default())),
                        ..Default::default()
                    },
                )),
                mutations: vec![pb::Mutation {
                    operation: Some(pb::mutation::Operation::Insert(pb::mutation::Write {
                        table: "Entities".into(),
                        columns: vec!["Id".into(), "Name".into(), "Country".into()],
                        values: rows,
                    })),
                }],
                ..Default::default()
            })
            .await?;
        }
        println!(
            "[{db}] seeded {ROWS} rows in {:.0} ms",
            t0.elapsed().as_secs_f64() * 1e3
        );
        let queries: &[(&str, &str, &str, usize)] = &[
            (
                "rare term (1 row)",
                "SELECT Id FROM Entities WHERE SEARCH(Name_Tokens, @q)",
                "unik4242",
                1,
            ),
            (
                "two terms (100 rows)",
                "SELECT Id FROM Entities WHERE SEARCH(Name_Tokens, @q)",
                "kari fjordli",
                100,
            ),
            (
                "phrase OR phrase",
                "SELECT Id FROM Entities WHERE SEARCH(Name_Tokens, @q)",
                "\"ola testmann holding\" OR \"per vik invest\"",
                40,
            ),
            (
                "common term + SCORE top 10",
                "SELECT Id, SCORE(Name_Tokens, @q) AS s FROM Entities WHERE SEARCH(Name_Tokens, @q) ORDER BY s DESC, Id LIMIT 10",
                "holding",
                10,
            ),
            (
                "SEARCH + filter COUNT",
                "SELECT COUNT(*) AS c FROM Entities WHERE SEARCH(Name_Tokens, @q) AND Country = 'SE'",
                "shipping",
                1,
            ),
            (
                "SNIPPET of 100 rows",
                "SELECT SNIPPET(Name, @q) AS s FROM Entities WHERE SEARCH(Name_Tokens, @q)",
                "kari fjordli",
                100,
            ),
            (
                "SEARCH_NGRAMS 40%",
                "SELECT Id FROM Entities WHERE SEARCH_NGRAMS(Name_Ngrams, @q, min_ngrams_percent => 40.0) LIMIT 50",
                "Unik424",
                50,
            ),
        ];
        for (label, sql, q, want) in queries {
            let mut samples = Vec::new();
            for _ in 0..50 {
                let (params, param_types) = params(&[("q", pb::TypeCode::String, s(q))]);
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
                assert_eq!(rs.rows.len(), *want, "{label}");
            }
            report(&format!("[{db}] {label}"), samples);
        }
    }
    Ok(())
}
