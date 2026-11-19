//! The per-RPC timing log (`SPANBOLT_TIMING`). Its own test binary: the log is opened once per
//! process, from the environment.

use std::sync::Arc;

use spanbolt_proto::database_admin as da;
use spanbolt_proto::database_admin::database_admin_client::DatabaseAdminClient;
use spanbolt_proto::instance_admin as ia;
use spanbolt_proto::instance_admin::instance_admin_client::InstanceAdminClient;
use spanbolt_proto::spanner as pb;
use spanbolt_proto::spanner::spanner_client::SpannerClient;
use spanbolt_server::Emulator;
use tonic::transport::Channel;

#[tokio::test]
async fn every_rpc_writes_a_line_with_its_statement() {
    let path = std::env::temp_dir().join(format!("spanbolt-timing-{}.jsonl", std::process::id()));
    // SAFETY: set before any thread reads the environment; the only test in this binary.
    unsafe { std::env::set_var("SPANBOLT_TIMING", &path) };
    let server = spanbolt_server::spawn("127.0.0.1:0", Arc::new(Emulator::default()))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let ch = Channel::from_shared(format!("http://{}", server.addr))
        .unwrap_or_else(|e| panic!("{e}"))
        .connect()
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    InstanceAdminClient::new(ch.clone())
        .create_instance(ia::CreateInstanceRequest {
            parent: "projects/p".into(),
            instance_id: "inst".into(),
            instance: Some(ia::Instance {
                config: "projects/p/instanceConfigs/emulator-config".into(),
                node_count: 1,
                ..Default::default()
            }),
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let db = "projects/p/instances/inst/databases/db";
    DatabaseAdminClient::new(ch.clone())
        .create_database(da::CreateDatabaseRequest {
            parent: "projects/p/instances/inst".into(),
            create_statement: "CREATE DATABASE `db`".into(),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    DatabaseAdminClient::new(ch.clone())
        .update_database_ddl(da::UpdateDatabaseDdlRequest {
            database: db.into(),
            statements: vec!["CREATE TABLE T (K INT64) PRIMARY KEY (K)".into()],
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let mut sp = SpannerClient::new(ch.clone());
    let session = sp
        .create_session(pb::CreateSessionRequest {
            database: db.into(),
            session: None,
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner()
        .name;
    // Without the SQL engine this is UNIMPLEMENTED; the line still names the statement.
    let _ = sp
        .execute_sql(pb::ExecuteSqlRequest {
            session: session.clone(),
            sql: "SELECT K FROM T".into(),
            ..Default::default()
        })
        .await;
    let _ = sp
        .read(pb::ReadRequest {
            session,
            table: "T".into(),
            columns: vec!["K".into()],
            key_set: Some(pb::KeySet {
                all: true,
                ..Default::default()
            }),
            ..Default::default()
        })
        .await;
    drop(server);
    spanbolt_server::timing::flush();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{e}"));
    let _ = std::fs::remove_file(&path);
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    let find = |m: &str| {
        lines
            .iter()
            .find(|l| l["m"] == m)
            .unwrap_or_else(|| panic!("no {m} in {text}"))
    };
    let ddl = find("UpdateDatabaseDdl");
    assert_eq!(ddl["sql"], "CREATE TABLE T (K INT64) PRIMARY KEY (K)");
    assert_eq!(ddl["n"], 1);
    assert_eq!(ddl["code"], 0);
    assert!(ddl["us"].as_u64().is_some() && ddl["t"].as_f64().is_some());
    assert_eq!(find("ExecuteSql")["sql"], "SELECT K FROM T");
    assert_eq!(find("Read")["sql"], "READ T(K)");
    assert_eq!(find("Read")["code"], 0);
    find("CreateSession");
}
