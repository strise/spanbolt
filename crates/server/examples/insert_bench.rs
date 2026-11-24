//! Cost of a 10,000-row insert commit as the table grows (cloud-spanner-emulator#187): random
//! UUID keys, one `Insert` mutation per row, columns with DEFAULT values.
//! `source scripts/googlesql-env.sh && cargo run --release -p spanbolt-server --features googlesql --example insert_bench [-- ROWS]`
//!
//! Part 1 times the commit phases directly on the storage layer (decode, DEFAULT evaluation,
//! writer apply, constraint checks, the whole commit, dropping a discarded state) and the bare
//! persistent-map inserts, in mutation order and sorted. Part 2 times the whole Commit RPC over
//! gRPC against an in-process server. Remove the `#[global_allocator]` below to see the system
//! allocator: on Linux (glibc) decoding and DEFAULT evaluation slow down 3-5x as the heap grows.

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
use spanbolt_storage::{DbState, Mutation, WriteKind, Writer};
use spanbolt_txn::{Clock, CommitInput, Database, ReadSet};
use tonic::transport::Channel;

/// The allocator the `spanbolt` binary uses (src/main.rs), so the numbers match it.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

const BATCH: usize = 10_000;
const DDL: &str = "CREATE TABLE Singers (SingerId STRING(MAX), Name STRING(MAX) DEFAULT (\"foo\"), Age INT64 DEFAULT (20)) PRIMARY KEY (SingerId)";
/// Table sizes (rows before the batch) to report.
const MARKS: &[usize] = &[10_000, 50_000, 100_000, 200_000, 500_000, 1_000_000];

fn s(v: &str) -> Value {
    Value {
        kind: Some(Kind::StringValue(v.into())),
    }
}

/// Random version-4-shaped UUID strings from a seeded xorshift generator.
struct Uuids(u64);

impl Uuids {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn next(&mut self) -> String {
        let (a, b) = (self.next_u64(), self.next_u64());
        let h = format!("{a:016x}{b:016x}");
        format!(
            "{}-{}-4{}-a{}-{}",
            &h[0..8],
            &h[8..12],
            &h[13..16],
            &h[17..20],
            &h[20..32]
        )
    }
}

/// One wire mutation per row, as the Go client's `client.Apply` sends them.
fn wire_batch(ids: &mut Uuids, explicit: bool) -> Vec<pb::Mutation> {
    (0..BATCH)
        .map(|_| {
            let id = ids.next();
            let (columns, values) = if explicit {
                (
                    vec!["SingerId".into(), "Name".into(), "Age".into()],
                    vec![s(&id), s("foo"), s("20")],
                )
            } else {
                (vec!["SingerId".into()], vec![s(&id)])
            };
            pb::Mutation {
                operation: Some(pb::mutation::Operation::Insert(pb::mutation::Write {
                    table: "Singers".into(),
                    columns,
                    values: vec![ListValue { values }],
                })),
            }
        })
        .collect()
}

fn decode(state: &DbState, batch: &[pb::Mutation]) -> Result<Vec<Mutation>, BoxErr> {
    let mut out = Vec::with_capacity(batch.len());
    for m in batch {
        let Some(pb::mutation::Operation::Insert(w)) = &m.operation else {
            return Err("not an insert".into());
        };
        out.push(Mutation::write(
            &state.catalog,
            WriteKind::Insert,
            &w.table,
            &w.columns,
            &w.values,
        )?);
    }
    Ok(out)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Phases of one batch, timed on the storage layer.
#[derive(Default, Clone, Copy)]
struct Phases {
    decode: Duration,
    defaults: Duration,
    apply: Duration,
    finish: Duration,
    drop: Duration,
    commit: Duration,
}

fn phases(db: &Database, ids: &mut Uuids, explicit: bool) -> Result<Phases, BoxErr> {
    let latest = db.latest();
    let state = &latest.state;
    let wire = wire_batch(ids, explicit);
    let mut p = Phases::default();

    let t = Instant::now();
    let muts = decode(state, &wire)?;
    p.decode = t.elapsed();

    // DEFAULT evaluation alone, as the writer runs it for each row.
    let tm = state.catalog.table(0);
    let t = Instant::now();
    for m in &muts {
        if let Mutation::Write { columns, rows, .. } = m {
            let mut row = vec![spanbolt_storage::Value::Null; tm.columns.len()];
            for (v, &c) in rows[0].iter().zip(columns) {
                row[c] = v.clone();
            }
            spanbolt_storage::expr::compute_row(
                tm,
                &mut row,
                spanbolt_storage::Timestamp::system_now(),
                &|c| !columns.contains(&c),
                &|_| true,
            )?;
        }
    }
    p.defaults = t.elapsed();

    // The writer on a scratch copy of the latest state (what `Database::commit` does under the
    // commit lock), then the state is dropped.
    let t = Instant::now();
    let mut w = Writer::new(state, db.clock().next_commit_ts());
    for m in &muts {
        w.apply(m)?;
    }
    p.apply = t.elapsed();
    let t = Instant::now();
    let (next, written) = w.finish()?;
    p.finish = t.elapsed();
    let t = Instant::now();
    drop((next, written));
    p.drop = t.elapsed();

    // The real commit of the same mutations: lock, apply, checks, publish.
    let reads = ReadSet::default();
    let t = Instant::now();
    db.commit(CommitInput {
        read_ts: None,
        reads: &reads,
        catalog: &state.catalog,
        mutations: &muts,
        dml_ops: 0,
    })?;
    p.commit = t.elapsed();
    Ok(p)
}

/// Raw persistent-map work of the batch: the decoded keys inserted into a copy of the table's
/// map, in mutation order and sorted.
fn map_only(state: &DbState, muts: &[Mutation]) -> Result<(Duration, Duration), BoxErr> {
    let mut keys: Vec<spanbolt_storage::Key> = Vec::with_capacity(muts.len());
    for m in muts {
        if let Mutation::Write { rows, .. } = m {
            let mut buf = Vec::new();
            spanbolt_storage::key::encode_part(&mut buf, &rows[0][0], false)?;
            keys.push(Arc::from(&buf[..]));
        }
    }
    let row: spanbolt_storage::Row = Arc::from(vec![spanbolt_storage::Value::Null; 3]);
    let time = |keys: &[spanbolt_storage::Key]| {
        let mut map = state.tables[0].clone();
        let t = Instant::now();
        for k in keys {
            map.insert(k.clone(), row.clone());
        }
        let d = t.elapsed();
        drop(map);
        d
    };
    let unsorted = time(&keys);
    keys.sort();
    Ok((unsorted, time(&keys)))
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn storage_part(max_rows: usize, explicit: bool) -> Result<(), BoxErr> {
    let pattern = if explicit {
        "explicit values"
    } else {
        "DEFAULT values"
    };
    println!("== storage layer, {BATCH} inserts per batch, {pattern} (ms, median of 5) ==");
    println!(
        "{:>9} {:>7} {:>9} {:>7} {:>7} {:>7} {:>7} {:>9} {:>9}",
        "rows", "decode", "defaults", "apply", "finish", "drop", "commit", "map", "map_sort"
    );
    let mut schema = spanbolt_ddl::Schema::default();
    schema.apply(&spanbolt_ddl::parse_statement(DDL)?)?;
    let db = Database::new(Arc::new(Clock::default()), DbState::new(schema)?);
    let mut ids = Uuids(0x9e37_79b9_7f4a_7c15);
    let mut rows = 0;
    let mut runs: Vec<(Phases, Duration, Duration)> = Vec::new();
    while rows < max_rows {
        let latest = db.latest();
        let wire = wire_batch(&mut ids, explicit);
        let muts = decode(&latest.state, &wire)?;
        let (map, map_sorted) = map_only(&latest.state, &muts)?;
        drop((latest, wire, muts));
        let p = phases(&db, &mut ids, explicit)?;
        runs.push((p, map, map_sorted));
        if runs.len() > 5 {
            runs.remove(0);
        }
        let next = rows + BATCH;
        // Report the five batches that end at a mark (or at the last batch).
        if MARKS.contains(&next) || next > max_rows {
            let col = |f: &dyn Fn(&(Phases, Duration, Duration)) -> Duration| {
                median(runs.iter().map(|r| ms(f(r))).collect())
            };
            println!(
                "{next:>9} {:>7.1} {:>9.1} {:>7.1} {:>7.1} {:>7.1} {:>7.1} {:>9.1} {:>9.1}",
                col(&|r| r.0.decode),
                col(&|r| r.0.defaults),
                col(&|r| r.0.apply),
                col(&|r| r.0.finish),
                col(&|r| r.0.drop),
                col(&|r| r.0.commit),
                col(&|r| r.1),
                col(&|r| r.2),
            );
        }
        rows = next;
    }
    Ok(())
}

async fn grpc_part(max_rows: usize, explicit: bool) -> Result<(), BoxErr> {
    let pattern = if explicit {
        "explicit values"
    } else {
        "DEFAULT values"
    };
    println!(
        "== Commit RPC over gRPC, in-process server, {BATCH} insert mutations, {pattern} (ms, median of 5) =="
    );
    let db = if explicit { "explicit" } else { "defaults" };
    let emu = Arc::new(Emulator::new(Arc::new(spanbolt_sql::Engine::default())));
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
            create_statement: format!("CREATE DATABASE {db}"),
            extra_statements: vec![DDL.into()],
            ..Default::default()
        })
        .await?;
    let mut sp = SpannerClient::new(channel.clone())
        .max_decoding_message_size(usize::MAX)
        .max_encoding_message_size(usize::MAX);
    let session = sp
        .create_session(pb::CreateSessionRequest {
            database: format!("projects/bench/instances/bench/databases/{db}"),
            session: None,
        })
        .await?
        .into_inner()
        .name;
    let mut ids = Uuids(0x2545_f491_4f6c_dd1d);
    let mut rows = 0;
    let mut window: Vec<f64> = Vec::new();
    println!("{:>9} {:>9}", "rows", "commit");
    while rows < max_rows {
        let mutations = wire_batch(&mut ids, explicit);
        let t = Instant::now();
        sp.commit(pb::CommitRequest {
            session: session.clone(),
            mutations,
            transaction: Some(pb::commit_request::Transaction::SingleUseTransaction(
                pb::TransactionOptions {
                    mode: Some(pb::transaction_options::Mode::ReadWrite(Default::default())),
                    ..Default::default()
                },
            )),
            ..Default::default()
        })
        .await?;
        let d = ms(t.elapsed());
        window.push(d);
        if window.len() > 5 {
            window.remove(0);
        }
        rows += BATCH;
        if MARKS.contains(&rows) || rows >= max_rows {
            println!("{rows:>9} {:>9.1}", median(window.clone()));
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), BoxErr> {
    let max_rows: usize = std::env::args()
        .nth(1)
        .map(|a| a.parse())
        .transpose()?
        .unwrap_or(1_000_000);
    spanbolt_storage::expr::set_expr_evaluator(Some(Arc::new(spanbolt_sql::SchemaExprs::new()?)));
    for explicit in [false, true] {
        storage_part(max_rows, explicit)?;
    }
    for explicit in [false, true] {
        grpc_part(max_rows, explicit).await?;
    }
    Ok(())
}
