//! Streaming reads over gRPC: chunked values, resume tokens and List* pagination.

use std::sync::Arc;

use prost_types::value::Kind;
use prost_types::{ListValue, Value};
use spanbolt_proto::database_admin as da;
use spanbolt_proto::database_admin::database_admin_client::DatabaseAdminClient;
use spanbolt_proto::instance_admin as ia;
use spanbolt_proto::instance_admin::instance_admin_client::InstanceAdminClient;
use spanbolt_proto::spanner as pb;
use spanbolt_proto::spanner::spanner_client::SpannerClient;
use spanbolt_server::{Emulator, Running};
use tokio_stream::StreamExt;
use tonic::transport::Channel;

const DB: &str = "projects/p/instances/inst/databases/db";

fn s(v: &str) -> Value {
    Value {
        kind: Some(Kind::StringValue(v.into())),
    }
}

fn text(v: &Value) -> String {
    match &v.kind {
        Some(Kind::StringValue(s)) => s.clone(),
        other => panic!("not a string: {other:?}"),
    }
}

struct Env {
    _server: Running,
    channel: Channel,
}

impl Env {
    fn spanner(&self) -> SpannerClient<Channel> {
        SpannerClient::new(self.channel.clone()).max_decoding_message_size(100 << 20)
    }
    async fn session(&self) -> String {
        self.spanner()
            .create_session(pb::CreateSessionRequest {
                database: DB.into(),
                session: None,
            })
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .into_inner()
            .name
    }
    async fn put(&self, session: &str, rows: Vec<(i64, String)>) {
        self.spanner()
            .commit(pb::CommitRequest {
                session: session.into(),
                transaction: Some(pb::commit_request::Transaction::SingleUseTransaction(
                    pb::TransactionOptions {
                        mode: Some(pb::transaction_options::Mode::ReadWrite(Default::default())),
                        ..Default::default()
                    },
                )),
                mutations: vec![pb::Mutation {
                    operation: Some(pb::mutation::Operation::InsertOrUpdate(
                        pb::mutation::Write {
                            table: "T".into(),
                            columns: vec!["Id".into(), "Body".into()],
                            values: rows
                                .into_iter()
                                .map(|(id, body)| ListValue {
                                    values: vec![s(&id.to_string()), s(&body)],
                                })
                                .collect(),
                        },
                    )),
                }],
                ..Default::default()
            })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
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
    InstanceAdminClient::new(channel.clone())
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
    DatabaseAdminClient::new(channel.clone())
        .create_database(da::CreateDatabaseRequest {
            parent: "projects/p/instances/inst".into(),
            create_statement: "CREATE DATABASE `db`".into(),
            extra_statements: vec![
                "CREATE TABLE T (Id INT64 NOT NULL, Body STRING(MAX)) PRIMARY KEY (Id)".into(),
            ],
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    Env {
        _server: server,
        channel,
    }
}

fn read_all(session: &str, token: Vec<u8>) -> pb::ReadRequest {
    pb::ReadRequest {
        session: session.into(),
        table: "T".into(),
        columns: vec!["Id".into(), "Body".into()],
        key_set: Some(pb::KeySet {
            all: true,
            ..Default::default()
        }),
        resume_token: token,
        ..Default::default()
    }
}

async fn partials(env: &Env, req: pb::ReadRequest) -> Vec<pb::PartialResultSet> {
    let mut stream = env
        .spanner()
        .streaming_read(req)
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    let mut out = Vec::new();
    while let Some(p) = stream.next().await {
        out.push(p.unwrap_or_else(|e| panic!("{e}")));
    }
    out
}

/// Merges chunked string values (all columns here are strings) and groups them into rows.
fn rows(parts: &[pb::PartialResultSet]) -> Vec<(String, String)> {
    let mut vals: Vec<String> = Vec::new();
    let mut pending = false;
    for p in parts {
        let mut it = p.values.iter().map(text);
        if pending && let (Some(last), Some(first)) = (vals.last_mut(), it.next()) {
            last.push_str(&first);
        }
        vals.extend(it);
        pending = p.chunked_value;
    }
    vals.chunks(2)
        .map(|c| (c[0].clone(), c[1].clone()))
        .collect()
}

#[tokio::test]
async fn value_larger_than_a_chunk_round_trips() {
    let env = setup().await;
    let session = env.session().await;
    let big: String = (0..3_000_000u32)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    env.put(
        &session,
        vec![(1, "small".into()), (2, big.clone()), (3, "tail".into())],
    )
    .await;
    let parts = partials(&env, read_all(&session, Vec::new())).await;
    assert!(parts.len() >= 3, "{} partials", parts.len());
    assert!(parts.iter().any(|p| p.chunked_value));
    assert!(
        parts
            .iter()
            .all(|p| p.values.iter().all(|v| text(v).len() <= 1 << 20))
    );
    assert_eq!(
        rows(&parts),
        vec![
            ("1".into(), "small".into()),
            ("2".into(), big),
            ("3".into(), "tail".into())
        ]
    );
}

#[tokio::test]
async fn resume_returns_the_remaining_rows_once_from_the_same_snapshot() {
    let env = setup().await;
    let session = env.session().await;
    let body = "x".repeat(2000);
    env.put(&session, (0..3000).map(|i| (i, body.clone())).collect())
        .await;
    let parts = partials(&env, read_all(&session, Vec::new())).await;
    let all = rows(&parts);
    assert_eq!(all.len(), 3000);
    let (cut, token) = parts
        .iter()
        .enumerate()
        .find(|(_, p)| !p.resume_token.is_empty() && !p.last)
        .map(|(i, p)| (i, p.resume_token.clone()))
        .unwrap_or_else(|| panic!("no mid-stream resume token in {} partials", parts.len()));
    let have = rows(&parts[..=cut]);
    // A commit after the token must not show up in the resumed single-use read.
    env.put(&session, vec![(-1, "new".into()), (5000, "new".into())])
        .await;
    let rest = rows(&partials(&env, read_all(&session, token)).await);
    let mut joined = have;
    joined.extend(rest);
    assert_eq!(joined, all);
}

#[tokio::test]
async fn bad_resume_token_is_invalid_argument() {
    let env = setup().await;
    let session = env.session().await;
    let err = env
        .spanner()
        .streaming_read(read_all(&session, b"nope".to_vec()))
        .await
        .err()
        .unwrap_or_else(|| panic!("accepted a bad token"));
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn read_errors_fail_the_rpc_before_streaming() {
    let env = setup().await;
    let session = env.session().await;
    let mut req = read_all(&session, Vec::new());
    req.columns.push("Nope".into());
    let err = env
        .spanner()
        .streaming_read(req)
        .await
        .err()
        .unwrap_or_else(|| panic!("accepted a bad column"));
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn list_sessions_and_databases_paginate() {
    let env = setup().await;
    let mut created = Vec::new();
    for _ in 0..5 {
        created.push(env.session().await);
    }
    created.sort();
    let mut seen = Vec::new();
    let mut token = String::new();
    loop {
        let resp = env
            .spanner()
            .list_sessions(pb::ListSessionsRequest {
                database: DB.into(),
                page_size: 2,
                page_token: token.clone(),
                filter: String::new(),
            })
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .into_inner();
        assert!(resp.sessions.len() <= 2);
        seen.extend(resp.sessions.into_iter().map(|s| s.name));
        if resp.next_page_token.is_empty() {
            break;
        }
        token = resp.next_page_token;
    }
    assert_eq!(seen, created);

    let mut admin = DatabaseAdminClient::new(env.channel.clone());
    for id in ["db2", "db3"] {
        admin
            .create_database(da::CreateDatabaseRequest {
                parent: "projects/p/instances/inst".into(),
                create_statement: format!("CREATE DATABASE `{id}`"),
                ..Default::default()
            })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
    }
    let page = |token: String| da::ListDatabasesRequest {
        parent: "projects/p/instances/inst".into(),
        page_size: 2,
        page_token: token,
    };
    let first = admin
        .list_databases(page(String::new()))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert_eq!(first.databases.len(), 2);
    assert_eq!(first.next_page_token, format!("{DB}3"));
    let second = admin
        .list_databases(page(first.next_page_token))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .into_inner();
    assert_eq!(second.databases.len(), 1);
    assert!(second.next_page_token.is_empty());
}

#[cfg(feature = "googlesql")]
mod sql {
    use super::*;

    fn query(session: &str, sql: &str, token: Vec<u8>) -> pb::ExecuteSqlRequest {
        pb::ExecuteSqlRequest {
            session: session.into(),
            sql: sql.into(),
            resume_token: token,
            ..Default::default()
        }
    }

    async fn run(env: &Env, req: pb::ExecuteSqlRequest) -> Vec<pb::PartialResultSet> {
        let mut stream = env
            .spanner()
            .execute_streaming_sql(req)
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .into_inner();
        let mut out = Vec::new();
        while let Some(p) = stream.next().await {
            out.push(p.unwrap_or_else(|e| panic!("{e}")));
        }
        out
    }

    const CROSS: &str = "SELECT CAST(a * 1000 + b AS STRING) AS n, REPEAT('v', 40) AS pad \
         FROM UNNEST(GENERATE_ARRAY(0, 299)) AS a, UNNEST(GENERATE_ARRAY(0, 999)) AS b";

    #[tokio::test]
    async fn query_streams_and_resumes_exactly_once() {
        let env = setup().await;
        let session = env.session().await;
        let parts = run(&env, query(&session, CROSS, Vec::new())).await;
        let all = rows(&parts);
        assert_eq!(all.len(), 300_000);
        assert!(parts.len() > 10, "{} partials", parts.len());
        assert!(parts[0].metadata.is_some());
        let tokens: Vec<usize> = parts
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.resume_token.is_empty() && !p.last)
            .map(|(i, _)| i)
            .collect();
        assert!(tokens.len() > 5);
        let cut = tokens[tokens.len() / 2];
        let mut joined = rows(&parts[..=cut]);
        let resumed = run(
            &env,
            query(&session, CROSS, parts[cut].resume_token.clone()),
        )
        .await;
        joined.extend(rows(&resumed));
        assert_eq!(joined, all);
    }

    /// Merges chunked values with the clients' rules: strings concatenate; lists concatenate,
    /// merging the boundary elements when both are strings or both lists.
    fn merge(a: Value, b: Value) -> Value {
        match (a.kind, b.kind) {
            (Some(Kind::StringValue(x)), Some(Kind::StringValue(y))) => s(&(x + &y)),
            (Some(Kind::ListValue(x)), Some(Kind::ListValue(y))) => {
                let mut xs = x.values;
                let mut ys = y.values.into_iter().peekable();
                let mergeable = |v: &Value| {
                    matches!(
                        v.kind,
                        Some(Kind::StringValue(_)) | Some(Kind::ListValue(_))
                    )
                };
                if let (Some(last), Some(first)) = (xs.last(), ys.peek())
                    && mergeable(last)
                    && std::mem::discriminant(&last.kind) == std::mem::discriminant(&first.kind)
                {
                    let (l, f) = (xs.pop(), ys.next());
                    xs.push(merge(l.unwrap_or_default(), f.unwrap_or_default()));
                }
                xs.extend(ys);
                Value {
                    kind: Some(Kind::ListValue(ListValue { values: xs })),
                }
            }
            (a, b) => panic!("unmergeable {a:?} / {b:?}"),
        }
    }

    #[tokio::test]
    async fn array_of_large_strings_is_chunked_and_merges_back() {
        let env = setup().await;
        let session = env.session().await;
        let parts = run(
            &env,
            query(
                &session,
                "SELECT ARRAY(SELECT CONCAT(CAST(i AS STRING), REPEAT('abc', 300000)) \
                 FROM UNNEST(GENERATE_ARRAY(1, 4)) AS i ORDER BY i) AS v",
                Vec::new(),
            ),
        )
        .await;
        assert!(parts.iter().filter(|p| p.chunked_value).count() >= 3);
        let mut vals: Vec<Value> = Vec::new();
        let mut pending = false;
        for p in &parts {
            let mut it = p.values.iter().cloned();
            if pending && let (Some(last), Some(first)) = (vals.pop(), it.next()) {
                vals.push(merge(last, first));
            }
            vals.extend(it);
            pending = p.chunked_value;
        }
        let expected: Vec<Value> = (1..=4)
            .map(|i| s(&format!("{i}{}", "abc".repeat(300_000))))
            .collect();
        assert_eq!(
            vals,
            vec![Value {
                kind: Some(Kind::ListValue(ListValue { values: expected })),
            }]
        );
    }

    #[tokio::test]
    async fn streaming_dml_reports_count_on_the_last_partial() {
        let env = setup().await;
        let session = env.session().await;
        let begin = env
            .spanner()
            .begin_transaction(pb::BeginTransactionRequest {
                session: session.clone(),
                options: Some(pb::TransactionOptions {
                    mode: Some(pb::transaction_options::Mode::ReadWrite(Default::default())),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .into_inner();
        let mut req = query(
            &session,
            "INSERT INTO T (Id, Body) VALUES (1, 'a'), (2, 'b')",
            Vec::new(),
        );
        req.transaction = Some(pb::TransactionSelector {
            selector: Some(pb::transaction_selector::Selector::Id(begin.id)),
        });
        req.seqno = 1;
        let parts = run(&env, req).await;
        let last = parts.last().unwrap_or_else(|| panic!("no partials"));
        assert!(last.last);
        assert_eq!(
            last.stats.as_ref().and_then(|s| s.row_count),
            Some(pb::result_set_stats::RowCount::RowCountExact(2))
        );
    }

    #[tokio::test]
    async fn analysis_errors_fail_the_rpc() {
        let env = setup().await;
        let session = env.session().await;
        let err = env
            .spanner()
            .execute_streaming_sql(query(&session, "SELECT * FROM Nope", Vec::new()))
            .await
            .err()
            .unwrap_or_else(|| panic!("accepted a bad query"));
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}

/// Every DATA frame of a streaming RPC is at most 16 KiB, like the emulator's Go server writes,
/// even when the client allows frames up to 16 MiB. A raw h2 client sees each frame on its own.
#[tokio::test]
async fn streamed_data_frames_are_at_most_16_kib() {
    use prost::Message;

    let env = setup().await;
    let session = env.session().await;
    let body = "y".repeat(1000);
    for batch in 0..4 {
        env.put(
            &session,
            (0..2500)
                .map(|i| (batch * 2500 + i, body.clone()))
                .collect(),
        )
        .await;
    }

    let tcp = tokio::net::TcpStream::connect(env._server.addr)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let (mut client, conn) = h2::client::Builder::new()
        .max_frame_size(16_777_215)
        .initial_window_size(1 << 30)
        .initial_connection_window_size(1 << 30)
        .handshake::<_, prost::bytes::Bytes>(tcp)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    tokio::spawn(conn);

    let msg = read_all(&session, Vec::new()).encode_to_vec();
    let mut grpc = vec![0u8];
    grpc.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    grpc.extend_from_slice(&msg);
    let req = http::Request::post(format!(
        "http://{}/google.spanner.v1.Spanner/StreamingRead",
        env._server.addr
    ))
    .header("content-type", "application/grpc")
    .header("te", "trailers")
    .body(())
    .unwrap_or_else(|e| panic!("{e}"));
    let (resp, mut send) = client
        .send_request(req, false)
        .unwrap_or_else(|e| panic!("{e}"));
    send.send_data(grpc.into(), true)
        .unwrap_or_else(|e| panic!("{e}"));
    let mut recv = resp.await.unwrap_or_else(|e| panic!("{e}")).into_body();

    let (mut frames, mut total, mut largest) = (0usize, 0usize, 0usize);
    while let Some(data) = recv.data().await {
        let data = data.unwrap_or_else(|e| panic!("{e}"));
        let _ = recv.flow_control().release_capacity(data.len());
        frames += 1;
        total += data.len();
        largest = largest.max(data.len());
    }
    let trailers = recv
        .trailers()
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("no trailers"));
    assert_eq!(
        trailers.get("grpc-status").map(|v| v.as_bytes()),
        Some(&b"0"[..])
    );
    assert!(total > 10_000_000, "{total} bytes");
    assert!(largest <= 16 << 10, "largest DATA frame {largest} bytes");
    assert!(
        frames >= total / (16 << 10),
        "{frames} frames for {total} bytes"
    );
}
