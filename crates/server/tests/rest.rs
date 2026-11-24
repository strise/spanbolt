//! The REST gateway against answers recorded from the official emulator
//! (`gcr.io/cloud-spanner-emulator/emulator:1.5.59`, port 9020): status codes and JSON bodies for
//! the admin and session calls a typical e2e setup makes (create an instance and a database, update
//! and get DDL, sessions, transactions, SQL and batch DML). Timestamps, generated ids and the fields spanbolt adds
//! (`createTime` and friends, which prod returns too) are not compared.

use std::sync::Arc;

use serde_json::{Value, json};
use spanbolt_server::Emulator;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const INSTANCE: &str = "projects/p/instances/inst";
const DB: &str = "projects/p/instances/inst/databases/db";

struct Rest {
    addr: std::net::SocketAddr,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl Rest {
    async fn start(emu: Emulator) -> Rest {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let addr = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(spanbolt_server::rest::serve(
            listener,
            Arc::new(emu),
            async {
                let _ = rx.await;
            },
        ));
        let rest = Rest {
            addr,
            _shutdown: tx,
        };
        let (status, _) = rest
            .call(
                "POST",
                "projects/p/instances",
                Some(json!({
                    "instanceId": "inst",
                    "instance": {"config": "projects/p/instanceConfigs/emulator-config", "nodeCount": 1}
                })),
            )
            .await;
        assert_eq!(status, 200);
        let (status, _) = rest
            .call(
                "POST",
                &format!("{INSTANCE}/databases"),
                Some(json!({
                    "createStatement": "CREATE DATABASE `db`",
                    "extraStatements": ["CREATE TABLE T (K INT64 NOT NULL, V INT64) PRIMARY KEY (K)"]
                })),
            )
            .await;
        assert_eq!(status, 200);
        rest
    }

    /// One HTTP/1.1 request on its own connection; the status and the body as JSON.
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let request = format!(
            "{method} /v1/{path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut stream = TcpStream::connect(self.addr)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        stream
            .write_all(request.as_bytes())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let raw = String::from_utf8(raw).unwrap_or_else(|e| panic!("{e}"));
        let (head, body) = raw
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("no body in {raw}"));
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("no status in {head}"));
        let json = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
        };
        (status, json)
    }

    async fn ddl(&self) -> Vec<String> {
        let (status, body) = self.call("GET", &format!("{DB}/ddl"), None).await;
        assert_eq!(status, 200, "{body}");
        body["statements"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn update_ddl(&self, body: Value) -> (u16, Value) {
        self.call("PATCH", &format!("{DB}/ddl"), Some(body)).await
    }
}

async fn rest() -> Rest {
    Rest::start(Emulator::default()).await
}

const TABLE_T: &str = "CREATE TABLE T (\n  K INT64 NOT NULL,\n  V INT64,\n) PRIMARY KEY(K)";

#[tokio::test]
async fn errors_carry_details_only_when_there_are_some() {
    let r = rest().await;
    let (status, body) = r
        .call(
            "POST",
            "projects/p/instances",
            Some(json!({"instanceId": "inst", "instance": {"config": "c", "nodeCount": 1}})),
        )
        .await;
    assert_eq!(status, 409);
    assert_eq!(
        body,
        json!({"code": 6, "message": "Instance already exists: projects/p/instances/inst"})
    );
    let (status, body) = r.call("GET", "projects/p/nope", None).await;
    assert_eq!(
        (status, body),
        (404, json!({"code": 5, "message": "Not Found"}))
    );
    // A ResourceInfo detail is rendered.
    let (status, body) = r
        .call("GET", &format!("{INSTANCE}/databases/x"), None)
        .await;
    assert_eq!(status, 404);
    assert_eq!(
        body["details"][0]["@type"],
        "type.googleapis.com/google.rpc.ResourceInfo"
    );
}

#[tokio::test]
async fn invalid_ddl_fails_the_operation_and_keeps_the_statements_before_it() {
    let r = rest().await;
    let dup = "CREATE TABLE T (K INT64 NOT NULL) PRIMARY KEY (K)";
    let new = "CREATE TABLE W (K INT64 NOT NULL) PRIMARY KEY (K)";
    for (statements, kept) in [(vec![dup, new], 0), (vec![new, dup], 1)] {
        let (status, body) = r.update_ddl(json!({ "statements": statements })).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["done"], json!(true));
        assert_eq!(
            body["error"],
            json!({"code": 9, "message": "Duplicate name in schema: T."})
        );
        let stamps = body["metadata"]["commitTimestamps"]
            .as_array()
            .map_or(0, Vec::len);
        assert_eq!(stamps, kept, "{body}");
    }
    // A statement that doesn't parse still fails the request.
    let (status, _) = r
        .update_ddl(
            json!({"statements": ["CREATE TABLE X (K INT64) PRIMARY KEY (K)", "CREATE TABEL X"]}),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(
        r.ddl().await,
        vec![
            TABLE_T.to_string(),
            "CREATE TABLE W (\n  K INT64 NOT NULL,\n) PRIMARY KEY(K)".to_string()
        ]
    );
}

#[tokio::test]
async fn invalid_ddl_fails_the_request_and_applies_nothing_in_emulator_compat() {
    let mut emu = Emulator::default();
    emu.emulator_compat = true;
    let r = Rest::start(emu).await;
    let dup = "CREATE TABLE T (K INT64 NOT NULL) PRIMARY KEY (K)";
    let new = "CREATE TABLE W (K INT64 NOT NULL) PRIMARY KEY (K)";
    for statements in [vec![dup, new], vec![new, dup]] {
        let (status, body) = r.update_ddl(json!({ "statements": statements })).await;
        assert_eq!(status, 400);
        assert_eq!(
            body,
            json!({"code": 9, "message": "Duplicate name in schema: T."})
        );
    }
    let (status, body) = r
        .update_ddl(json!({"statements": ["CREATE TABEL X"]}))
        .await;
    assert_eq!(status, 400);
    assert_eq!(
        body,
        json!({"code": 3, "message": "Error parsing Spanner DDL statement: CREATE TABEL X : Syntax error on line 1, column 8: Encountered 'TABEL' while parsing: create_statement"})
    );
    assert_eq!(r.ddl().await, vec![TABLE_T]);
}

#[tokio::test]
async fn ddl_request_validation() {
    let r = rest().await;
    let (status, body) = r.update_ddl(json!({"statements": []})).await;
    assert_eq!(
        (status, body),
        (
            400,
            json!({"code": 3, "message": "Missing statements in the request."})
        )
    );
    let create = "CREATE TABLE Q (K INT64) PRIMARY KEY (K)";
    for bad in [
        "a",
        "aB",
        "A1",
        "_a",
        "1a",
        "a-b",
        &format!("a{}", "b".repeat(128)),
    ] {
        let (status, body) = r
            .update_ddl(json!({"statements": [create], "operationId": bad}))
            .await;
        assert_eq!(status, 400, "{bad}");
        assert_eq!(
            body,
            json!({"code": 3, "message": format!("Invalid operation id: {bad}.")})
        );
    }
    assert_eq!(r.ddl().await, vec![TABLE_T]);
    let good = format!("a{}", "b".repeat(127));
    let (status, body) = r
        .update_ddl(json!({"statements": [create], "operationId": good}))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["name"], format!("{DB}/operations/{good}"));
    let (status, body) = r
        .update_ddl(json!({"statements": ["DROP TABLE Q"], "operationId": good}))
        .await;
    assert_eq!(
        (status, body),
        (
            409,
            json!({"code": 6, "message": format!("Operation already exists: {DB}/operations/{good}")})
        )
    );
}

#[tokio::test]
async fn rows_violating_ddl_fail_the_operation_and_keep_earlier_statements() {
    let r = rest().await;
    let (status, session) = r
        .call("POST", &format!("{DB}/sessions"), Some(json!({})))
        .await;
    assert_eq!(status, 200);
    let session = session["name"].as_str().unwrap_or_default().to_string();
    let (status, body) = r
        .call(
            "POST",
            &format!("{session}:commit"),
            Some(json!({
                "singleUseTransaction": {"readWrite": {}},
                "mutations": [{"insert": {"table": "T", "columns": ["K", "V"], "values": [["1", "5"], ["2", "5"]]}}]
            })),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    // The emulator's answer, with the commit timestamp left out:
    // {"name": ".../operations/op_x", "metadata": {"@type": "...UpdateDatabaseDdlMetadata",
    //   "database": "...", "statements": [...], "commitTimestamps": ["<ts>"]},
    //  "done": true, "error": {"code": 9, "message": "Found uniqueness violation on index UV, ..."}}
    // The default server uses production Spanner's text instead: "Found uniqueness violation: [5,1] [5,2]."
    let statements = [
        "CREATE TABLE X1 (K INT64) PRIMARY KEY (K)",
        "CREATE UNIQUE INDEX UV ON T(V)",
    ];
    let (status, op) = r
        .update_ddl(json!({"statements": statements, "operationId": "op_x"}))
        .await;
    assert_eq!(status, 200, "{op}");
    assert_eq!(op["name"], format!("{DB}/operations/op_x"));
    assert_eq!(op["done"], true);
    assert_eq!(op["error"]["code"], 9);
    assert!(
        op["error"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("Found uniqueness violation: ")),
        "{op}"
    );
    assert_eq!(op["metadata"]["statements"], json!(statements));
    assert_eq!(
        op["metadata"]["commitTimestamps"].as_array().map(Vec::len),
        Some(1)
    );
    assert!(op.get("response").is_none());
    assert_eq!(
        r.ddl().await,
        vec![TABLE_T, "CREATE TABLE X1 (\n  K INT64,\n) PRIMARY KEY(K)"]
    );
    // The failed operation stays readable.
    let (status, again) = r.call("GET", &format!("{DB}/operations/op_x"), None).await;
    assert_eq!((status, &again["error"]), (200, &op["error"]));

    // Failing at the first statement: no timestamps, nothing applied.
    let (status, op) = r
        .update_ddl(json!({"statements": ["CREATE UNIQUE INDEX UV ON T(V)"]}))
        .await;
    assert_eq!(status, 200, "{op}");
    assert_eq!(op["error"]["code"], 9);
    assert!(op["metadata"].get("commitTimestamps").is_none(), "{op}");
    assert_eq!(r.ddl().await.len(), 2);
}

#[tokio::test]
async fn dropping_a_missing_database_succeeds() {
    let r = rest().await;
    let (status, body) = r
        .call("DELETE", &format!("{INSTANCE}/databases/nope"), None)
        .await;
    assert_eq!((status, body), (200, json!({})));
    let (status, body) = r
        .call("DELETE", "projects/p/instances/nope/databases/nope", None)
        .await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], 5);
    assert_eq!(
        body["message"],
        "Instance not found: projects/p/instances/nope"
    );
    let (status, body) = r.call("DELETE", DB, None).await;
    assert_eq!((status, body), (200, json!({})));
    let (status, _) = r.call("GET", DB, None).await;
    assert_eq!(status, 404);
}

#[cfg(feature = "googlesql")]
#[tokio::test]
async fn dml_without_a_transaction_is_a_read_only_request() {
    let r = Rest::start(Emulator::new(spanbolt_server::sql::default_engine(false))).await;
    let (_, session) = r
        .call("POST", &format!("{DB}/sessions"), Some(json!({})))
        .await;
    let session = session["name"].as_str().unwrap_or_default().to_string();
    let (status, body) = r
        .call(
            "POST",
            &format!("{session}:executeSql"),
            Some(json!({"sql": "INSERT INTO T (K) VALUES (1)"})),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(
        body,
        json!({"code": 3, "message": "DML statements can only be performed in a read-write or partitioned-dml transaction. Current transaction type is ReadOnly."})
    );
}
