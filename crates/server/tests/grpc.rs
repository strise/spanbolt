//! End to end over gRPC with the generated tonic clients.

use std::sync::Arc;

use prost_types::value::Kind;
use prost_types::{ListValue, Value};
use spanbolt_proto::control as ctl;
use spanbolt_proto::control::control_client::ControlClient;
use spanbolt_proto::database_admin as da;
use spanbolt_proto::database_admin::database_admin_client::DatabaseAdminClient;
use spanbolt_proto::instance_admin as ia;
use spanbolt_proto::instance_admin::instance_admin_client::InstanceAdminClient;
use spanbolt_proto::longrunning as lro;
use spanbolt_proto::longrunning::operation;
use spanbolt_proto::longrunning::operations_client::OperationsClient;
use spanbolt_proto::spanner as pb;
use spanbolt_proto::spanner::spanner_client::SpannerClient;
use spanbolt_server::{Emulator, Running};
use tonic::Code;
use tonic::transport::Channel;

const DB: &str = "projects/p/instances/inst/databases/db";

fn s(v: &str) -> Value {
    Value {
        kind: Some(Kind::StringValue(v.into())),
    }
}

fn row(vals: &[&str]) -> ListValue {
    ListValue {
        values: vals.iter().map(|v| s(v)).collect(),
    }
}

fn text(v: &Value) -> String {
    match &v.kind {
        Some(Kind::StringValue(s)) => s.clone(),
        Some(Kind::NullValue(_)) => "NULL".into(),
        other => format!("{other:?}"),
    }
}

fn rows(rs: &pb::ResultSet) -> Vec<Vec<String>> {
    rs.rows
        .iter()
        .map(|r| r.values.iter().map(text).collect())
        .collect()
}

struct Env {
    server: Running,
    channel: Channel,
}

impl Env {
    fn spanner(&self) -> SpannerClient<Channel> {
        SpannerClient::new(self.channel.clone())
    }
    fn admin(&self) -> DatabaseAdminClient<Channel> {
        DatabaseAdminClient::new(self.channel.clone())
    }
    fn control(&self) -> ControlClient<Channel> {
        ControlClient::new(self.channel.clone())
    }
    async fn session(&self, db: &str) -> String {
        let req = pb::CreateSessionRequest {
            database: db.into(),
            session: None,
        };
        self.spanner()
            .create_session(req)
            .await
            .map(|r| r.into_inner().name)
            .unwrap_or_else(|e| panic!("{e}"))
    }
}

async fn setup() -> Env {
    let server = spanbolt_server::spawn("127.0.0.1:0", Arc::new(Emulator::default()))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let channel = Channel::from_shared(format!("http://{}", server.addr))
        .unwrap_or_else(|e| panic!("{e}"))
        .connect()
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let op = InstanceAdminClient::new(channel.clone())
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
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert!(op.done);
    let op = DatabaseAdminClient::new(channel.clone())
        .create_database(da::CreateDatabaseRequest {
            parent: "projects/p/instances/inst".into(),
            create_statement: "CREATE DATABASE `db`".into(),
            extra_statements: vec![
                "CREATE TABLE Accounts (Id INT64 NOT NULL, Owner STRING(MAX), Balance INT64) PRIMARY KEY (Id)".into(),
                "CREATE INDEX AccountsByOwner ON Accounts(Owner) STORING (Balance)".into(),
            ],
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert!(matches!(op.result, Some(operation::Result::Response(_))));
    Env { server, channel }
}

fn single_use_rw() -> Option<pb::commit_request::Transaction> {
    Some(pb::commit_request::Transaction::SingleUseTransaction(
        pb::TransactionOptions {
            mode: Some(pb::transaction_options::Mode::ReadWrite(Default::default())),
            ..Default::default()
        },
    ))
}

fn upsert(table: &str, cols: &[&str], values: Vec<ListValue>) -> pb::Mutation {
    pb::Mutation {
        operation: Some(pb::mutation::Operation::InsertOrUpdate(
            pb::mutation::Write {
                table: table.into(),
                columns: cols.iter().map(|c| c.to_string()).collect(),
                values,
            },
        )),
    }
}

fn keys(k: &[&str]) -> Option<pb::KeySet> {
    Some(pb::KeySet {
        keys: k.iter().map(|k| row(&[k])).collect(),
        ..Default::default()
    })
}

fn read_req(session: &str, keyset: Option<pb::KeySet>) -> pb::ReadRequest {
    pb::ReadRequest {
        session: session.into(),
        table: "Accounts".into(),
        columns: vec!["Id".into(), "Owner".into(), "Balance".into()],
        key_set: keyset,
        ..Default::default()
    }
}

fn by_id(id: Vec<u8>) -> Option<pb::TransactionSelector> {
    Some(pb::TransactionSelector {
        selector: Some(pb::transaction_selector::Selector::Id(id)),
    })
}

async fn seed(env: &Env, session: &str) {
    let resp = env
        .spanner()
        .commit(pb::CommitRequest {
            session: session.into(),
            transaction: single_use_rw(),
            mutations: vec![upsert(
                "Accounts",
                &["Id", "Owner", "Balance"],
                (1..=20)
                    .map(|i| {
                        row(&[
                            &i.to_string(),
                            if i % 2 == 0 { "even" } else { "odd" },
                            "100",
                        ])
                    })
                    .collect(),
            )],
            return_commit_stats: true,
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert!(resp.commit_timestamp.is_some());
    assert_eq!(resp.commit_stats.map(|s| s.mutation_count), Some(60));
}

#[tokio::test]
async fn ddl_commit_and_reads() {
    let env = setup().await;
    let session = env.session(DB).await;
    seed(&env, &session).await;
    let mut sp = env.spanner();

    let ddl = env
        .admin()
        .get_database_ddl(da::GetDatabaseDdlRequest {
            database: DB.into(),
        })
        .await;
    assert_eq!(ddl.map(|r| r.into_inner().statements.len()).ok(), Some(2));

    let rs = sp
        .read(read_req(&session, keys(&["3"])))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert_eq!(rows(&rs), vec![vec!["3", "odd", "100"]]);
    assert_eq!(
        rs.metadata.and_then(|m| m.row_type).map(|t| t.fields.len()),
        Some(3)
    );

    let range = pb::KeySet {
        ranges: vec![pb::KeyRange {
            start_key_type: Some(pb::key_range::StartKeyType::StartClosed(row(&["5"]))),
            end_key_type: Some(pb::key_range::EndKeyType::EndOpen(row(&["9"]))),
        }],
        ..Default::default()
    };
    let rs = sp
        .read(read_req(&session, Some(range.clone())))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    let ids: Vec<String> = rows(&rs).into_iter().map(|r| r[0].clone()).collect();
    assert_eq!(ids, vec!["5", "6", "7", "8"]);
    let mut limited = read_req(&session, Some(range));
    limited.limit = 2;
    assert_eq!(
        sp.read(limited)
            .await
            .map(|r| r.into_inner().rows.len())
            .ok(),
        Some(2)
    );

    // Index read: entries in (Owner, Id) order; the STORING column is readable.
    let mut by_owner = read_req(&session, keys(&["even"]));
    by_owner.index = "AccountsByOwner".into();
    let rs = sp
        .read(by_owner)
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert_eq!(rs.rows.len(), 10);
    assert_eq!(rows(&rs)[0], vec!["2", "even", "100"]);

    let mut stream = sp
        .streaming_read(read_req(
            &session,
            Some(pb::KeySet {
                all: true,
                ..Default::default()
            }),
        ))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    let mut values = 0;
    while let Some(part) = stream.message().await.unwrap_or_else(|e| panic!("{e}")) {
        values += part.values.len();
    }
    assert_eq!(values, 60);

    let err = sp
        .read(pb::ReadRequest {
            table: "Nope".into(),
            ..read_req(&session, keys(&["1"]))
        })
        .await
        .err();
    assert_eq!(
        err.map(|e| (e.code(), e.message().to_string())),
        Some((Code::NotFound, "Table not found: Nope".into()))
    );
    let dup = sp
        .commit(pb::CommitRequest {
            session: session.clone(),
            transaction: single_use_rw(),
            mutations: vec![pb::Mutation {
                operation: Some(pb::mutation::Operation::Insert(pb::mutation::Write {
                    table: "Accounts".into(),
                    columns: vec!["Id".into()],
                    values: vec![row(&["1"])],
                })),
            }],
            ..Default::default()
        })
        .await
        .err();
    assert_eq!(
        dup.map(|e| (e.code(), e.message().to_string())),
        Some((
            Code::AlreadyExists,
            "Table Accounts: Row {Int64(1)} already exists.".into()
        ))
    );

    let sql = sp
        .execute_sql(pb::ExecuteSqlRequest {
            session: session.clone(),
            sql: "SELECT 1".into(),
            ..Default::default()
        })
        .await;
    if cfg!(feature = "googlesql") {
        let rows = sql.map(|r| r.into_inner().rows.len()).map_err(|e| e.code());
        assert_eq!(rows, Ok(1));
    } else {
        assert_eq!(sql.err().map(|e| e.code()), Some(Code::Unimplemented));
    }
}

#[tokio::test]
async fn inline_begin_and_conflicts() {
    let env = setup().await;
    let session = env.session(DB).await;
    seed(&env, &session).await;
    let mut sp = env.spanner();
    let begin = || pb::TransactionSelector {
        selector: Some(pb::transaction_selector::Selector::Begin(
            pb::TransactionOptions {
                mode: Some(pb::transaction_options::Mode::ReadWrite(Default::default())),
                ..Default::default()
            },
        )),
    };
    // Two read-write transactions read the same row (inline begin), then both write it.
    let s2 = env.session(DB).await;
    let mut txns = Vec::new();
    for s in [&session, &s2] {
        let mut req = read_req(s, keys(&["1"]));
        req.transaction = Some(begin());
        let rs = sp
            .read(req)
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .into_inner();
        let id = rs
            .metadata
            .and_then(|m| m.transaction)
            .map(|t| t.id)
            .unwrap_or_default();
        assert!(!id.is_empty(), "inline begin returns the transaction");
        txns.push((s.clone(), id));
    }
    let mut results = Vec::new();
    for (s, id) in txns {
        let res = sp
            .commit(pb::CommitRequest {
                session: s,
                transaction: Some(pb::commit_request::Transaction::TransactionId(id)),
                mutations: vec![upsert(
                    "Accounts",
                    &["Id", "Balance"],
                    vec![row(&["1", "0"])],
                )],
                ..Default::default()
            })
            .await;
        results.push(res.map(|_| ()).map_err(|e| e.code()));
    }
    assert_eq!(results, vec![Ok(()), Err(Code::Aborted)]);

    env.control()
        .inject_abort(ctl::InjectAbortRequest {
            database: DB.into(),
            count: 1,
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let commit = || pb::CommitRequest {
        session: session.clone(),
        transaction: single_use_rw(),
        mutations: vec![upsert("Accounts", &["Id"], vec![row(&["99"])])],
        ..Default::default()
    };
    assert_eq!(
        sp.commit(commit()).await.err().map(|e| e.code()),
        Some(Code::Aborted)
    );
    assert!(sp.commit(commit()).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_concurrent_writers_never_abort() {
    let env = setup().await;
    let seed_session = env.session(DB).await;
    seed(&env, &seed_session).await;
    let mut tasks = Vec::new();
    for t in 0..8 {
        let mut sp = env.spanner();
        let session = env.session(DB).await;
        tasks.push(tokio::spawn(async move {
            let mut aborts = 0;
            for i in 0..25 {
                let tx = sp
                    .begin_transaction(pb::BeginTransactionRequest {
                        session: session.clone(),
                        options: Some(pb::TransactionOptions {
                            mode: Some(
                                pb::transaction_options::Mode::ReadWrite(Default::default()),
                            ),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                    .await
                    .unwrap_or_else(|e| panic!("{e}"))
                    .into_inner();
                // Each task reads and writes only its own key.
                let key = (t + 1).to_string();
                let mut req = read_req(&session, keys(&[&key]));
                req.transaction = by_id(tx.id.clone());
                let rs = sp
                    .read(req)
                    .await
                    .unwrap_or_else(|e| panic!("{e}"))
                    .into_inner();
                let balance: i64 = rows(&rs)
                    .first()
                    .and_then(|r| r[2].parse().ok())
                    .unwrap_or(0);
                let res = sp
                    .commit(pb::CommitRequest {
                        session: session.clone(),
                        transaction: Some(pb::commit_request::Transaction::TransactionId(tx.id)),
                        mutations: vec![upsert(
                            "Accounts",
                            &["Id", "Balance"],
                            vec![row(&[&key, &(balance + 1).to_string()])],
                        )],
                        ..Default::default()
                    })
                    .await;
                match res {
                    Ok(_) => {}
                    Err(e) if e.code() == Code::Aborted => aborts += 1,
                    Err(e) => panic!("commit {i} failed: {e}"),
                }
            }
            aborts
        }));
    }
    let mut total_aborts = 0;
    for t in tasks {
        total_aborts += t.await.unwrap_or_else(|e| panic!("{e}"));
    }
    assert_eq!(total_aborts, 0);
    let rs = env
        .spanner()
        .read(read_req(
            &seed_session,
            keys(&["1", "2", "3", "4", "5", "6", "7", "8"]),
        ))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert_eq!(rs.rows.len(), 8);
    assert!(rows(&rs).iter().all(|r| r[2] == "125"));
}

#[tokio::test]
async fn clone_snapshot_and_ddl_operations() {
    let env = setup().await;
    let session = env.session(DB).await;
    seed(&env, &session).await;
    let mut ctl = env.control();
    let clone = "projects/p/instances/inst/databases/clone";
    ctl.clone_database(ctl::CloneDatabaseRequest {
        source: DB.into(),
        target: clone.into(),
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let cs = env.session(clone).await;
    let mut sp = env.spanner();
    sp.commit(pb::CommitRequest {
        session: cs.clone(),
        transaction: single_use_rw(),
        mutations: vec![upsert(
            "Accounts",
            &["Id", "Owner"],
            vec![row(&["1", "clone"])],
        )],
        ..Default::default()
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let owner = |rs: pb::ResultSet| rows(&rs)[0][1].clone();
    let src = sp
        .read(read_req(&session, keys(&["1"])))
        .await
        .map(|r| owner(r.into_inner()));
    let cl = sp
        .read(read_req(&cs, keys(&["1"])))
        .await
        .map(|r| owner(r.into_inner()));
    assert_eq!(
        (src.ok().as_deref(), cl.ok().as_deref()),
        (Some("odd"), Some("clone"))
    );

    let file = std::env::temp_dir().join(format!("spanbolt-test-{}.snap", std::process::id()));
    let path = file.to_string_lossy().to_string();
    ctl.save_snapshot(ctl::SaveSnapshotRequest { path: path.clone() })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let loaded = ctl
        .load_snapshot(ctl::LoadSnapshotRequest { path })
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    let _ = std::fs::remove_file(&file);
    assert_eq!((loaded.instances, loaded.databases), (1, 2));
    let s = env.session(clone).await;
    let cl = sp
        .read(read_req(&s, keys(&["1"])))
        .await
        .map(|r| owner(r.into_inner()));
    assert_eq!(cl.ok().as_deref(), Some("clone"));

    let mut admin = env.admin();
    let ddl = |stmt: &str| da::UpdateDatabaseDdlRequest {
        database: DB.into(),
        statements: vec![stmt.into()],
        ..Default::default()
    };
    let op = admin
        .update_database_ddl(ddl("ALTER TABLE Accounts ADD COLUMN Note STRING(MAX)"))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert!(matches!(op.result, Some(operation::Result::Response(_))));
    // ADD COLUMN NOT NULL without a default is UNIMPLEMENTED, as in the official emulator, which
    // fails the request itself rather than the operation.
    let code = admin
        .update_database_ddl(ddl("ALTER TABLE Accounts ADD COLUMN Req INT64 NOT NULL"))
        .await
        .map(|_| Code::Ok)
        .unwrap_or_else(|e| e.code());
    assert_eq!(code, Code::Unimplemented);
    let s = env.session(DB).await;
    let mut req = read_req(&s, keys(&["2"]));
    req.columns.push("Note".into());
    let note = sp
        .read(req)
        .await
        .map(|r| rows(&r.into_inner())[0][3].clone());
    assert_eq!(note.ok().as_deref(), Some("NULL"));
}

#[tokio::test]
async fn test_hook_installs_schema() {
    let env = setup().await;
    let schema = spanbolt_storage::testing::ddl(&[
        "CREATE TABLE Hooked (K STRING(MAX) NOT NULL) PRIMARY KEY (K)",
    ]);
    env.server
        .emulator
        .install_schema(DB, schema)
        .unwrap_or_else(|e| panic!("{e}"));
    let session = env.session(DB).await;
    env.spanner()
        .commit(pb::CommitRequest {
            session,
            transaction: single_use_rw(),
            mutations: vec![upsert("Hooked", &["K"], vec![row(&["a"])])],
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
}

#[tokio::test]
async fn drop_database_drops_its_operations() {
    let env = setup().await;
    let db = "projects/p/instances/inst/databases/opsdb";
    let mut admin = env.admin();
    let created = admin
        .create_database(da::CreateDatabaseRequest {
            parent: "projects/p/instances/inst".into(),
            create_statement: "CREATE DATABASE opsdb".into(),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    let ddl = admin
        .update_database_ddl(da::UpdateDatabaseDdlRequest {
            database: db.into(),
            statements: vec!["CREATE TABLE U (Id INT64 NOT NULL) PRIMARY KEY (Id)".into()],
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    let mut ops = OperationsClient::new(env.channel.clone());
    for name in [&created.name, &ddl.name] {
        ops.get_operation(lro::GetOperationRequest { name: name.clone() })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
    }
    admin
        .drop_database(da::DropDatabaseRequest {
            database: db.into(),
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    for name in [&created.name, &ddl.name] {
        let err = ops
            .get_operation(lro::GetOperationRequest { name: name.clone() })
            .await
            .expect_err("operation of a dropped database");
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert_eq!(err.message(), format!("Operation not found: {name}"));
    }
    let listed = ops
        .list_operations(lro::ListOperationsRequest {
            name: format!("{db}/operations"),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert!(listed.operations.is_empty());
}

/// Two transactions run `INSERT ... SELECT MAX(...) + 1` over the same empty range and insert the
/// same key. The loser's DML read is stale, so its commit aborts (and its retry takes the next
/// key) rather than failing with ALREADY_EXISTS.
#[cfg(feature = "googlesql")]
#[tokio::test]
async fn dml_read_then_insert_race_aborts_loser() {
    let env = setup().await;
    let mut sp = env.spanner();
    let sql = "INSERT INTO Accounts (Id, Owner, Balance) \
               SELECT COALESCE(MAX(Id), 999) + 1, 'race', 0 FROM Accounts WHERE Id >= 1000";
    let begin = || pb::TransactionSelector {
        selector: Some(pb::transaction_selector::Selector::Begin(
            pb::TransactionOptions {
                mode: Some(pb::transaction_options::Mode::ReadWrite(Default::default())),
                ..Default::default()
            },
        )),
    };
    let mut txns = Vec::new();
    for _ in 0..2 {
        let s = env.session(DB).await;
        let rs = sp
            .execute_sql(pb::ExecuteSqlRequest {
                session: s.clone(),
                transaction: Some(begin()),
                sql: sql.into(),
                seqno: 1,
                ..Default::default()
            })
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .into_inner();
        let id = rs
            .metadata
            .and_then(|m| m.transaction)
            .map(|t| t.id)
            .unwrap_or_default();
        assert!(!id.is_empty(), "inline begin returns the transaction");
        txns.push((s, id));
    }
    let mut results = Vec::new();
    for (s, id) in txns {
        let res = sp
            .commit(pb::CommitRequest {
                session: s,
                transaction: Some(pb::commit_request::Transaction::TransactionId(id)),
                ..Default::default()
            })
            .await;
        results.push(res.map(|_| ()).map_err(|e| e.code()));
    }
    assert_eq!(results, vec![Ok(()), Err(Code::Aborted)]);

    // The retry sees the winner's row and takes the next key.
    let s = env.session(DB).await;
    let rs = sp
        .execute_sql(pb::ExecuteSqlRequest {
            session: s.clone(),
            transaction: Some(begin()),
            sql: sql.into(),
            seqno: 1,
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    let id = rs
        .metadata
        .and_then(|m| m.transaction)
        .map(|t| t.id)
        .unwrap_or_default();
    sp.commit(pb::CommitRequest {
        session: s.clone(),
        transaction: Some(pb::commit_request::Transaction::TransactionId(id)),
        ..Default::default()
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let ids = sp
        .execute_sql(pb::ExecuteSqlRequest {
            session: s,
            sql: "SELECT Id FROM Accounts WHERE Id >= 1000 ORDER BY Id".into(),
            ..Default::default()
        })
        .await
        .map(|r| rows(&r.into_inner()))
        .map_err(|e| e.code());
    assert_eq!(
        ids,
        Ok(vec![vec!["1000".to_string()], vec!["1001".to_string()]])
    );
}

/// Property graphs over gRPC: CREATE PROPERTY GRAPH is checked against the tables, then GQL
/// queries and INFORMATION_SCHEMA.PROPERTY_GRAPHS read it.
#[cfg(feature = "googlesql")]
#[tokio::test]
async fn property_graph_ddl_and_gql() {
    let env = setup().await;
    let mut admin = env.admin();
    let ddl = |stmts: &[&str]| da::UpdateDatabaseDdlRequest {
        database: DB.into(),
        statements: stmts.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let failed = |r: Result<tonic::Response<lro::Operation>, tonic::Status>| match r {
        Err(e) => Some((e.code(), e.message().to_string())),
        Ok(op) => match op.into_inner().result {
            Some(operation::Result::Error(s)) => Some((Code::from(s.code), s.message)),
            _ => None,
        },
    };
    let bad = admin
        .update_database_ddl(ddl(&[
            "CREATE PROPERTY GRAPH g NODE TABLES (Missing KEY (Id) LABEL X PROPERTIES ALL COLUMNS)",
        ]))
        .await;
    assert_eq!(failed(bad).map(|f| f.0), Some(Code::InvalidArgument));
    let ok = admin
        .update_database_ddl(ddl(&[
            "CREATE TABLE Transfers (Id INT64 NOT NULL, Src INT64, Dst INT64, Amount INT64) PRIMARY KEY (Id)",
            "CREATE PROPERTY GRAPH g NODE TABLES (Accounts LABEL Account PROPERTIES ALL COLUMNS)
               EDGE TABLES (Transfers KEY (Id) SOURCE KEY (Src) REFERENCES Accounts (Id)
                 DESTINATION KEY (Dst) REFERENCES Accounts (Id) LABEL Transfer PROPERTIES ALL COLUMNS)",
        ]))
        .await;
    assert_eq!(failed(ok), None);

    let session = env.session(DB).await;
    seed(&env, &session).await;
    let mut sp = env.spanner();
    sp.commit(pb::CommitRequest {
        session: session.clone(),
        transaction: single_use_rw(),
        mutations: vec![upsert(
            "Transfers",
            &["Id", "Src", "Dst", "Amount"],
            vec![row(&["1", "1", "2", "10"]), row(&["2", "2", "3", "20"])],
        )],
        ..Default::default()
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let query = |sql: &str| pb::ExecuteSqlRequest {
        session: session.clone(),
        sql: sql.into(),
        ..Default::default()
    };
    let rs = sp
        .execute_sql(query(
            "GRAPH g MATCH (a:Account WHERE a.Id = 1)-[t:Transfer]->{1,2}(b) RETURN b.Id AS id ORDER BY id",
        ))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert_eq!(
        rows(&rs),
        vec![vec!["2".to_string()], vec!["3".to_string()]]
    );
    let node = sp
        .execute_sql(query("GRAPH g MATCH (a:Account) RETURN a"))
        .await
        .err()
        .map(|e| e.code());
    assert_eq!(node, Some(Code::InvalidArgument));
    let names = sp
        .execute_sql(query(
            "SELECT PROPERTY_GRAPH_NAME FROM INFORMATION_SCHEMA.PROPERTY_GRAPHS",
        ))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert_eq!(rows(&names), vec![vec!["g".to_string()]]);
}

fn registry_size(env: &Env, what: &str) -> usize {
    env.server
        .emulator
        .registry_sizes()
        .into_iter()
        .find(|(k, _)| *k == what)
        .map_or(0, |(_, n)| n)
}

async fn get_session(env: &Env, name: &str) -> Result<pb::Session, tonic::Status> {
    env.spanner()
        .get_session(pb::GetSessionRequest { name: name.into() })
        .await
        .map(|r| r.into_inner())
}

fn assert_session_not_found(res: Result<impl std::fmt::Debug, tonic::Status>, name: &str) {
    let err = res.expect_err("session should be gone");
    assert_eq!(err.code(), Code::NotFound);
    assert_eq!(err.message(), format!("Session not found: {name}"));
}

/// Sessions unused for an hour expire (multiplexed ones after 28 days), with the emulator's
/// NOT_FOUND, and the sweeper drops them and their transactions without anyone naming them.
#[tokio::test]
async fn idle_sessions_expire() {
    const MIN: std::time::Duration = std::time::Duration::from_secs(60);
    let env = setup().await;
    let idle = &env.server.emulator.idle;
    let mut sp = env.spanner();
    let stale = env.session(DB).await;
    let used = env.session(DB).await;
    let mux = sp
        .create_session(pb::CreateSessionRequest {
            database: DB.into(),
            session: Some(pb::Session {
                multiplexed: true,
                ..Default::default()
            }),
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner()
        .name;

    // Any RPC naming a session keeps it alive; an hour without one expires it.
    idle.advance(59 * MIN);
    sp.begin_transaction(pb::BeginTransactionRequest {
        session: used.clone(),
        options: Some(pb::TransactionOptions {
            mode: Some(pb::transaction_options::Mode::ReadOnly(Default::default())),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    idle.advance(2 * MIN);
    assert_session_not_found(get_session(&env, &stale).await, &stale);
    let begin = sp
        .begin_transaction(pb::BeginTransactionRequest {
            session: stale.clone(),
            options: Some(Default::default()),
            ..Default::default()
        })
        .await;
    assert_session_not_found(begin, &stale);
    get_session(&env, &used)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    get_session(&env, &mux)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let listed: Vec<String> = sp
        .list_sessions(pb::ListSessionsRequest {
            database: DB.into(),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner()
        .sessions
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(listed, vec![used.clone()]);

    // The sweeper drops what nobody names again, with its transaction.
    assert_eq!(registry_size(&env, "sessions"), 2);
    assert_eq!(registry_size(&env, "txns"), 1);
    idle.advance(61 * MIN);
    assert_eq!(registry_size(&env, "sessions"), 2);
    env.server.emulator.sweep();
    assert_eq!(registry_size(&env, "sessions"), 1);
    assert_eq!(registry_size(&env, "txns"), 0);
    assert_session_not_found(get_session(&env, &used).await, &used);

    // Multiplexed sessions last 28 days unused.
    idle.advance(28 * 24 * 60 * MIN - 62 * MIN);
    get_session(&env, &mux)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    idle.advance(28 * 24 * 60 * MIN + MIN);
    env.server.emulator.sweep();
    assert_eq!(registry_size(&env, "sessions"), 0);
    assert_session_not_found(get_session(&env, &mux).await, &mux);
}

/// DeleteSession, DropDatabase and DeleteInstance remove sessions from the registry, and the
/// sessions answer "Session not found" afterwards, as on the emulator.
#[tokio::test]
async fn sessions_go_with_their_database_and_instance() {
    let env = setup().await;
    let mut sp = env.spanner();
    let deleted = env.session(DB).await;
    sp.delete_session(pb::DeleteSessionRequest {
        name: deleted.clone(),
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    assert_session_not_found(get_session(&env, &deleted).await, &deleted);
    assert_eq!(registry_size(&env, "sessions"), 0);

    let dropped = env.session(DB).await;
    env.admin()
        .drop_database(da::DropDatabaseRequest {
            database: DB.into(),
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(registry_size(&env, "sessions"), 0);
    assert_session_not_found(get_session(&env, &dropped).await, &dropped);

    let other = "projects/p/instances/inst/databases/other";
    env.admin()
        .create_database(da::CreateDatabaseRequest {
            parent: "projects/p/instances/inst".into(),
            create_statement: "CREATE DATABASE other".into(),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let in_instance = env.session(other).await;
    InstanceAdminClient::new(env.channel.clone())
        .delete_instance(ia::DeleteInstanceRequest {
            name: "projects/p/instances/inst".into(),
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(registry_size(&env, "sessions"), 0);
    assert_session_not_found(get_session(&env, &in_instance).await, &in_instance);
}
