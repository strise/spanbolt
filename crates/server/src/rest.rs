//! REST gateway on the emulator's port 9020: the `google.api.http` bindings of the Spanner, admin
//! and long-running operation APIs, transcoded to proto3 JSON and served by the same in-process
//! services as gRPC.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use prost::Message;
use prost_reflect::{DescriptorPool, DeserializeOptions, DynamicMessage, SerializeOptions};
use serde_json::{Map, Value};
use spanbolt_proto::database_admin::database_admin_server::DatabaseAdmin;
use spanbolt_proto::instance_admin::instance_admin_server::InstanceAdmin;
use spanbolt_proto::longrunning::operations_server::Operations;
use spanbolt_proto::spanner::spanner_server::Spanner;
use spanbolt_proto::{
    database_admin as da, instance_admin as ia, longrunning as lro, spanner as pb,
};
use tokio::net::TcpListener;
use tonic::{Request, Status};

use crate::Emulator;
use crate::services::database_admin::DatabaseAdminService;
use crate::services::instance_admin::InstanceAdminService;
use crate::services::operations::OperationsService;
use crate::services::spanner::SpannerService;

/// One RPC reachable over REST, with the request field its URL path fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rpc {
    ListInstanceConfigs,
    GetInstanceConfig,
    ListInstances,
    GetInstance,
    CreateInstance,
    UpdateInstance,
    DeleteInstance,
    ListDatabases,
    GetDatabase,
    CreateDatabase,
    UpdateDatabase,
    DropDatabase,
    GetDatabaseDdl,
    UpdateDatabaseDdl,
    ListOperations,
    GetOperation,
    DeleteOperation,
    CancelOperation,
    CreateSession,
    BatchCreateSessions,
    ListSessions,
    GetSession,
    DeleteSession,
    ExecuteSql,
    ExecuteBatchDml,
    Read,
    BeginTransaction,
    Commit,
    Rollback,
}

/// Matches `segments` (the path after `/v1/`, without a trailing `:verb`) to an RPC and the request
/// field (dotted for nested) that receives the resource name.
fn route(method: &Method, segments: &[&str], verb: Option<&str>) -> Option<(Rpc, &'static str)> {
    use Rpc::*;
    let get = method == Method::GET;
    let post = method == Method::POST;
    let patch = method == Method::PATCH;
    let delete = method == Method::DELETE;
    let n = segments.len();
    if n >= 2 && segments[n - 2] == "operations" {
        return match (verb, get, post, delete) {
            (None, true, _, _) => Some((GetOperation, "name")),
            (None, _, _, true) => Some((DeleteOperation, "name")),
            (Some("cancel"), _, true, _) => Some((CancelOperation, "name")),
            _ => None,
        };
    }
    if n >= 3 && segments[n - 1] == "operations" && get && verb.is_none() {
        return Some((ListOperations, "name"));
    }
    let rpc = match (segments, verb) {
        (["projects", _, "instanceConfigs"], None) if get => (ListInstanceConfigs, "parent"),
        (["projects", _, "instanceConfigs", _], None) if get => (GetInstanceConfig, "name"),
        (["projects", _, "instances"], None) if get => (ListInstances, "parent"),
        (["projects", _, "instances"], None) if post => (CreateInstance, "parent"),
        (["projects", _, "instances", _], None) if get => (GetInstance, "name"),
        (["projects", _, "instances", _], None) if patch => (UpdateInstance, "instance.name"),
        (["projects", _, "instances", _], None) if delete => (DeleteInstance, "name"),
        (["projects", _, "instances", _, "databases"], None) if get => (ListDatabases, "parent"),
        (["projects", _, "instances", _, "databases"], None) if post => (CreateDatabase, "parent"),
        (["projects", _, "instances", _, "databases", _], None) if get => (GetDatabase, "name"),
        (["projects", _, "instances", _, "databases", _], None) if patch => {
            (UpdateDatabase, "database.name")
        }
        (["projects", _, "instances", _, "databases", _], None) if delete => {
            (DropDatabase, "database")
        }
        (["projects", _, "instances", _, "databases", _, "ddl"], None) if get => {
            (GetDatabaseDdl, "database")
        }
        (["projects", _, "instances", _, "databases", _, "ddl"], None) if patch => {
            (UpdateDatabaseDdl, "database")
        }
        (["projects", _, "instances", _, "databases", _, "sessions"], None) if post => {
            (CreateSession, "database")
        }
        (["projects", _, "instances", _, "databases", _, "sessions"], None) if get => {
            (ListSessions, "database")
        }
        (["projects", _, "instances", _, "databases", _, "sessions"], Some("batchCreate"))
            if post =>
        {
            (BatchCreateSessions, "database")
        }
        (["projects", _, "instances", _, "databases", _, "sessions", _], None) if get => {
            (GetSession, "name")
        }
        (["projects", _, "instances", _, "databases", _, "sessions", _], None) if delete => {
            (DeleteSession, "name")
        }
        (["projects", _, "instances", _, "databases", _, "sessions", _], Some(v)) if post => {
            let rpc = match v {
                "executeSql" => ExecuteSql,
                "executeBatchDml" => ExecuteBatchDml,
                "read" => Read,
                "beginTransaction" => BeginTransaction,
                "commit" => Commit,
                "rollback" => Rollback,
                _ => return None,
            };
            (rpc, "session")
        }
        _ => return None,
    };
    Some(rpc)
}

/// The resource name a route's path field takes: collections (`.../databases`, `.../ddl`) name
/// their parent.
fn resource_name(rpc: Rpc, segments: &[&str]) -> String {
    use Rpc::*;
    let keep = match rpc {
        ListInstanceConfigs | ListInstances | CreateInstance | ListDatabases | CreateDatabase
        | GetDatabaseDdl | UpdateDatabaseDdl | CreateSession | ListSessions
        | BatchCreateSessions => segments.len() - 1,
        _ => segments.len(),
    };
    segments[..keep].join("/")
}

pub struct Gateway {
    pool: DescriptorPool,
    spanner: SpannerService,
    database_admin: DatabaseAdminService,
    instance_admin: InstanceAdminService,
    operations: OperationsService,
}

#[derive(Debug)]
enum Failure {
    Status(Status),
    BadRequest(String),
}

impl From<Status> for Failure {
    fn from(s: Status) -> Self {
        Failure::Status(s)
    }
}

/// grpc-gateway's mapping from gRPC codes to HTTP statuses, which the emulator's gateway uses.
fn http_status(code: tonic::Code) -> StatusCode {
    use tonic::Code as C;
    match code {
        C::Ok => StatusCode::OK,
        C::Cancelled => StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST),
        C::InvalidArgument | C::FailedPrecondition | C::OutOfRange => StatusCode::BAD_REQUEST,
        C::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
        C::NotFound => StatusCode::NOT_FOUND,
        C::AlreadyExists | C::Aborted => StatusCode::CONFLICT,
        C::PermissionDenied => StatusCode::FORBIDDEN,
        C::Unauthenticated => StatusCode::UNAUTHORIZED,
        C::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        C::Unimplemented => StatusCode::NOT_IMPLEMENTED,
        C::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        C::Unknown | C::Internal | C::DataLoss => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn json_response(status: StatusCode, body: String) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

/// The emulator's gateway error body: `details` only when there are some.
fn error_body(code: tonic::Code, message: &str, details: Vec<Value>) -> String {
    let mut body = serde_json::json!({ "code": code as i32, "message": message });
    if !details.is_empty() {
        body["details"] = Value::Array(details);
    }
    body.to_string()
}

fn error_response(code: tonic::Code, message: &str) -> Response {
    json_response(http_status(code), error_body(code, message, Vec::new()))
}

impl Failure {
    fn into_response(self, pool: &DescriptorPool) -> Response {
        match self {
            Failure::Status(s) => {
                // The `google.rpc.Status` details (ResourceInfo...), as the emulator's gateway
                // renders them; details whose type is unknown are dropped.
                let details = (!s.details().is_empty())
                    .then(|| status_details(pool, s.details()))
                    .flatten()
                    .unwrap_or_default();
                json_response(
                    http_status(s.code()),
                    error_body(s.code(), s.message(), details),
                )
            }
            Failure::BadRequest(m) => error_response(tonic::Code::InvalidArgument, &m),
        }
    }
}

fn status_details(pool: &DescriptorPool, bytes: &[u8]) -> Option<Vec<Value>> {
    let st = spanbolt_proto::google::rpc::Status::decode(bytes).ok()?;
    let any = pool.get_message_by_name("google.protobuf.Any")?;
    let rendered = st
        .details
        .iter()
        .filter_map(|d| {
            let msg = DynamicMessage::decode(any.clone(), d.encode_to_vec().as_slice()).ok()?;
            serde_json::to_value(&msg).ok()
        })
        .collect();
    Some(rendered)
}

/// Decodes `%XX` escapes; `+` stays literal in paths and becomes a space in query strings.
fn percent_decode(s: &str, plus_is_space: bool) -> String {
    let hex = |b: u8| (b as char).to_digit(16);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(if plus_is_space && b == b'+' { b' ' } else { b });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Sets `field` (dotted for nested messages) in a JSON object.
fn set_field(obj: &mut Map<String, Value>, field: &str, value: Value) {
    match field.split_once('.') {
        Some((head, rest)) => {
            let entry = obj
                .entry(head.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Value::Object(inner) = entry {
                set_field(inner, rest, value);
            }
        }
        None => {
            obj.insert(field.to_string(), value);
        }
    }
}

impl Gateway {
    pub fn new(emu: Arc<Emulator>) -> Result<Self, String> {
        let pool = DescriptorPool::decode(spanbolt_proto::FILE_DESCRIPTOR_SET)
            .map_err(|e| format!("descriptor set: {e}"))?;
        Ok(Gateway {
            pool,
            spanner: SpannerService { emu: emu.clone() },
            database_admin: DatabaseAdminService { emu: emu.clone() },
            instance_admin: InstanceAdminService { emu: emu.clone() },
            operations: OperationsService { emu },
        })
    }

    fn decode<T: Message + Default>(&self, type_name: &str, json: Value) -> Result<T, Failure> {
        let desc = self
            .pool
            .get_message_by_name(type_name)
            .ok_or_else(|| Failure::Status(Status::internal(format!("no type {type_name}"))))?;
        let options = DeserializeOptions::new().deny_unknown_fields(false);
        let msg = DynamicMessage::deserialize_with_options(desc, json, &options)
            .map_err(|e| Failure::BadRequest(format!("Invalid JSON for {type_name}: {e}")))?;
        T::decode(msg.encode_to_vec().as_slice())
            .map_err(|e| Failure::Status(Status::internal(e.to_string())))
    }

    fn encode<T: Message>(&self, type_name: &str, msg: &T) -> Result<String, Failure> {
        let desc = self
            .pool
            .get_message_by_name(type_name)
            .ok_or_else(|| Failure::Status(Status::internal(format!("no type {type_name}"))))?;
        let dynamic = DynamicMessage::decode(desc, msg.encode_to_vec().as_slice())
            .map_err(|e| Failure::Status(Status::internal(e.to_string())))?;
        let mut out = Vec::new();
        let mut ser = serde_json::Serializer::new(&mut out);
        dynamic
            .serialize_with_options(&mut ser, &SerializeOptions::new())
            .map_err(|e| Failure::Status(Status::internal(e.to_string())))?;
        String::from_utf8(out).map_err(|e| Failure::Status(Status::internal(e.to_string())))
    }

    async fn dispatch(&self, rpc: Rpc, json: Value) -> Result<String, Failure> {
        // `call!(service, method, RequestType, "request.Name", "response.Name")`; a response
        // name of "" means google.protobuf.Empty.
        macro_rules! call {
            ($svc:expr, $method:ident, $req:ty, $req_name:literal, $resp_name:literal) => {{
                let req: $req = self.decode($req_name, json)?;
                let resp = $svc.$method(Request::new(req)).await?.into_inner();
                self.encode($resp_name, &resp)
            }};
            ($svc:expr, $method:ident, $req:ty, $req_name:literal) => {{
                let req: $req = self.decode($req_name, json)?;
                $svc.$method(Request::new(req)).await?;
                Ok("{}".to_string())
            }};
        }
        match rpc {
            Rpc::ListInstanceConfigs => call!(
                self.instance_admin,
                list_instance_configs,
                ia::ListInstanceConfigsRequest,
                "google.spanner.admin.instance.v1.ListInstanceConfigsRequest",
                "google.spanner.admin.instance.v1.ListInstanceConfigsResponse"
            ),
            Rpc::GetInstanceConfig => call!(
                self.instance_admin,
                get_instance_config,
                ia::GetInstanceConfigRequest,
                "google.spanner.admin.instance.v1.GetInstanceConfigRequest",
                "google.spanner.admin.instance.v1.InstanceConfig"
            ),
            Rpc::ListInstances => call!(
                self.instance_admin,
                list_instances,
                ia::ListInstancesRequest,
                "google.spanner.admin.instance.v1.ListInstancesRequest",
                "google.spanner.admin.instance.v1.ListInstancesResponse"
            ),
            Rpc::GetInstance => call!(
                self.instance_admin,
                get_instance,
                ia::GetInstanceRequest,
                "google.spanner.admin.instance.v1.GetInstanceRequest",
                "google.spanner.admin.instance.v1.Instance"
            ),
            Rpc::CreateInstance => call!(
                self.instance_admin,
                create_instance,
                ia::CreateInstanceRequest,
                "google.spanner.admin.instance.v1.CreateInstanceRequest",
                "google.longrunning.Operation"
            ),
            Rpc::UpdateInstance => call!(
                self.instance_admin,
                update_instance,
                ia::UpdateInstanceRequest,
                "google.spanner.admin.instance.v1.UpdateInstanceRequest",
                "google.longrunning.Operation"
            ),
            Rpc::DeleteInstance => call!(
                self.instance_admin,
                delete_instance,
                ia::DeleteInstanceRequest,
                "google.spanner.admin.instance.v1.DeleteInstanceRequest"
            ),
            Rpc::ListDatabases => call!(
                self.database_admin,
                list_databases,
                da::ListDatabasesRequest,
                "google.spanner.admin.database.v1.ListDatabasesRequest",
                "google.spanner.admin.database.v1.ListDatabasesResponse"
            ),
            Rpc::GetDatabase => call!(
                self.database_admin,
                get_database,
                da::GetDatabaseRequest,
                "google.spanner.admin.database.v1.GetDatabaseRequest",
                "google.spanner.admin.database.v1.Database"
            ),
            Rpc::CreateDatabase => call!(
                self.database_admin,
                create_database,
                da::CreateDatabaseRequest,
                "google.spanner.admin.database.v1.CreateDatabaseRequest",
                "google.longrunning.Operation"
            ),
            Rpc::UpdateDatabase => call!(
                self.database_admin,
                update_database,
                da::UpdateDatabaseRequest,
                "google.spanner.admin.database.v1.UpdateDatabaseRequest",
                "google.longrunning.Operation"
            ),
            Rpc::DropDatabase => call!(
                self.database_admin,
                drop_database,
                da::DropDatabaseRequest,
                "google.spanner.admin.database.v1.DropDatabaseRequest"
            ),
            Rpc::GetDatabaseDdl => call!(
                self.database_admin,
                get_database_ddl,
                da::GetDatabaseDdlRequest,
                "google.spanner.admin.database.v1.GetDatabaseDdlRequest",
                "google.spanner.admin.database.v1.GetDatabaseDdlResponse"
            ),
            Rpc::UpdateDatabaseDdl => call!(
                self.database_admin,
                update_database_ddl,
                da::UpdateDatabaseDdlRequest,
                "google.spanner.admin.database.v1.UpdateDatabaseDdlRequest",
                "google.longrunning.Operation"
            ),
            Rpc::ListOperations => call!(
                self.operations,
                list_operations,
                lro::ListOperationsRequest,
                "google.longrunning.ListOperationsRequest",
                "google.longrunning.ListOperationsResponse"
            ),
            Rpc::GetOperation => call!(
                self.operations,
                get_operation,
                lro::GetOperationRequest,
                "google.longrunning.GetOperationRequest",
                "google.longrunning.Operation"
            ),
            Rpc::DeleteOperation => call!(
                self.operations,
                delete_operation,
                lro::DeleteOperationRequest,
                "google.longrunning.DeleteOperationRequest"
            ),
            Rpc::CancelOperation => call!(
                self.operations,
                cancel_operation,
                lro::CancelOperationRequest,
                "google.longrunning.CancelOperationRequest"
            ),
            Rpc::CreateSession => call!(
                self.spanner,
                create_session,
                pb::CreateSessionRequest,
                "google.spanner.v1.CreateSessionRequest",
                "google.spanner.v1.Session"
            ),
            Rpc::BatchCreateSessions => call!(
                self.spanner,
                batch_create_sessions,
                pb::BatchCreateSessionsRequest,
                "google.spanner.v1.BatchCreateSessionsRequest",
                "google.spanner.v1.BatchCreateSessionsResponse"
            ),
            Rpc::ListSessions => call!(
                self.spanner,
                list_sessions,
                pb::ListSessionsRequest,
                "google.spanner.v1.ListSessionsRequest",
                "google.spanner.v1.ListSessionsResponse"
            ),
            Rpc::GetSession => call!(
                self.spanner,
                get_session,
                pb::GetSessionRequest,
                "google.spanner.v1.GetSessionRequest",
                "google.spanner.v1.Session"
            ),
            Rpc::DeleteSession => call!(
                self.spanner,
                delete_session,
                pb::DeleteSessionRequest,
                "google.spanner.v1.DeleteSessionRequest"
            ),
            Rpc::ExecuteSql => call!(
                self.spanner,
                execute_sql,
                pb::ExecuteSqlRequest,
                "google.spanner.v1.ExecuteSqlRequest",
                "google.spanner.v1.ResultSet"
            ),
            Rpc::ExecuteBatchDml => call!(
                self.spanner,
                execute_batch_dml,
                pb::ExecuteBatchDmlRequest,
                "google.spanner.v1.ExecuteBatchDmlRequest",
                "google.spanner.v1.ExecuteBatchDmlResponse"
            ),
            Rpc::Read => call!(
                self.spanner,
                read,
                pb::ReadRequest,
                "google.spanner.v1.ReadRequest",
                "google.spanner.v1.ResultSet"
            ),
            Rpc::BeginTransaction => call!(
                self.spanner,
                begin_transaction,
                pb::BeginTransactionRequest,
                "google.spanner.v1.BeginTransactionRequest",
                "google.spanner.v1.Transaction"
            ),
            Rpc::Commit => call!(
                self.spanner,
                commit,
                pb::CommitRequest,
                "google.spanner.v1.CommitRequest",
                "google.spanner.v1.CommitResponse"
            ),
            Rpc::Rollback => call!(
                self.spanner,
                rollback,
                pb::RollbackRequest,
                "google.spanner.v1.RollbackRequest"
            ),
        }
    }

    /// Handles one HTTP request: route, build the request JSON from path, query and body, call.
    async fn handle(&self, method: Method, uri: &Uri, body: &[u8]) -> Result<String, Failure> {
        let not_found = || {
            // The emulator's gateway answers an unbound path exactly like this.
            Failure::Status(Status::not_found("Not Found"))
        };
        let path = uri.path().strip_prefix("/v1/").ok_or_else(not_found)?;
        let path = percent_decode(path, false);
        // A custom method is `<resource>:<verb>` on the last segment.
        let last_start = path.rfind('/').map_or(0, |i| i + 1);
        let (resource, verb) = match path[last_start..].find(':') {
            Some(at) => (&path[..last_start + at], Some(&path[last_start + at + 1..])),
            None => (path.as_str(), None),
        };
        let segments: Vec<&str> = resource
            .trim_end_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let (rpc, field) = route(&method, &segments, verb).ok_or_else(not_found)?;

        let mut json = if body.iter().all(u8::is_ascii_whitespace) {
            Map::new()
        } else {
            match serde_json::from_slice::<Value>(body) {
                Ok(Value::Object(m)) => m,
                Ok(_) => {
                    return Err(Failure::BadRequest(
                        "Request body must be a JSON object".into(),
                    ));
                }
                Err(e) => return Err(Failure::BadRequest(format!("Invalid JSON body: {e}"))),
            }
        };
        if let Some(query) = uri.query() {
            for pair in query.split('&').filter(|p| !p.is_empty()) {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                set_field(
                    &mut json,
                    &percent_decode(k, true),
                    Value::String(percent_decode(v, true)),
                );
            }
        }
        set_field(
            &mut json,
            field,
            Value::String(resource_name(rpc, &segments)),
        );
        self.dispatch(rpc, Value::Object(json)).await
    }
}

async fn handler(
    State(gw): State<Arc<Gateway>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    // On the blocking pool, like the gRPC calls (`offload`): handlers run engine work inline.
    let out = crate::offload::run({
        let gw = gw.clone();
        async move { gw.handle(method, &uri, &body).await }
    })
    .await;
    match out {
        Ok(json) => json_response(StatusCode::OK, json),
        Err(f) => f.into_response(&gw.pool),
    }
}

/// Request bodies up to 100 MiB, like the gRPC side.
const MAX_BODY: usize = 100 << 20;

/// Serves the REST gateway on `listener` until `shutdown` resolves.
pub async fn serve(
    listener: TcpListener,
    emu: Arc<Emulator>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let gw = Arc::new(Gateway::new(emu).map_err(std::io::Error::other)?);
    let app = axum::Router::new()
        .fallback(handler)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(gw);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(method: Method, path: &str) -> Option<(Rpc, &'static str, String)> {
        let (res, verb) = match path.rsplit_once(':') {
            Some((a, b)) => (a, Some(b)),
            None => (path, None),
        };
        let segs: Vec<&str> = res.split('/').collect();
        route(&method, &segs, verb).map(|(rpc, f)| (rpc, f, resource_name(rpc, &segs)))
    }

    #[test]
    fn routes_resolve_names() {
        let db = "projects/p/instances/i/databases/d";
        assert_eq!(
            r(Method::GET, "projects/p/instances"),
            Some((Rpc::ListInstances, "parent", "projects/p".into()))
        );
        assert_eq!(
            r(Method::PATCH, &format!("{db}/ddl")),
            Some((Rpc::UpdateDatabaseDdl, "database", db.into()))
        );
        assert_eq!(
            r(Method::POST, &format!("{db}/sessions")),
            Some((Rpc::CreateSession, "database", db.into()))
        );
        assert_eq!(
            r(Method::POST, &format!("{db}/sessions/s1:executeSql")),
            Some((Rpc::ExecuteSql, "session", format!("{db}/sessions/s1")))
        );
        assert_eq!(
            r(Method::GET, &format!("{db}/operations/o1")),
            Some((Rpc::GetOperation, "name", format!("{db}/operations/o1")))
        );
        assert_eq!(
            r(Method::DELETE, db),
            Some((Rpc::DropDatabase, "database", db.into()))
        );
        assert_eq!(r(Method::PUT, db), None);
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("a%2Fb+c", true), "a/b c");
        assert_eq!(percent_decode("a%2Fb+c", false), "a/b+c");
        assert_eq!(percent_decode("100%", false), "100%");
    }

    #[test]
    fn nested_fields() {
        let mut m = Map::new();
        set_field(&mut m, "instance.name", Value::String("x".into()));
        assert_eq!(
            Value::Object(m),
            serde_json::json!({"instance": {"name": "x"}})
        );
    }
}
