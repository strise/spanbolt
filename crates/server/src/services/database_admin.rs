use std::sync::Arc;

use spanbolt_ddl::{AlterTable, DataCheck, Schema, Statement};
use spanbolt_proto::database_admin as da;
use spanbolt_proto::database_admin::database_admin_server::DatabaseAdmin;
use spanbolt_proto::google::iam::v1 as iam;
use spanbolt_proto::longrunning::Operation;
use spanbolt_storage::expr::{SchemaChange, expr_evaluator};
use spanbolt_storage::{DbState, Timestamp};
use tonic::{Request, Response, Status};

use crate::emulator::{DatabaseEntry, Emulator};
use crate::names;
use crate::page::page;
use crate::services::operations::{any, done, failed};
use crate::status::{to_status, unimplemented};

pub struct DatabaseAdminService {
    pub emu: Arc<Emulator>,
}

const TYPE_PREFIX: &str = "type.googleapis.com/google.spanner.admin.database.v1.";

/// `emulator_compat`: the emulator leaves `version_retention_period` empty; production reports
/// the option, or its default of one hour.
fn to_proto(entry: &DatabaseEntry, emulator_compat: bool) -> da::Database {
    let schema = &entry.db.latest().state.schema;
    let version_retention_period = match schema.version_retention_option() {
        _ if emulator_compat => String::new(),
        Some(period) => period.to_string(),
        None => "1h".into(),
    };
    let earliest = entry.db.earliest_version_time().max(entry.create_time);
    da::Database {
        name: entry.name.clone(),
        state: da::database::State::Ready as i32,
        create_time: Some(entry.create_time.to_proto()),
        version_retention_period,
        earliest_version_time: Some(earliest.to_proto()),
        database_dialect: da::DatabaseDialect::GoogleStandardSql as i32,
        ..Default::default()
    }
}

/// Why a DDL batch failed, with the index of the failing statement.
#[derive(Debug)]
pub enum DdlFailure {
    /// The statement is invalid against the schema. The emulator fails the whole request
    /// synchronously and applies nothing; Cloud Spanner commits the statements before it and
    /// reports this one in the operation's error.
    Schema(usize, Status),
    /// The schema change is valid but the existing rows violate it (a unique index over duplicates,
    /// a CHECK the rows fail). The emulator commits the statements before it and reports this one
    /// in the operation's error.
    Data(usize, Status),
}

impl DdlFailure {
    pub fn into_status(self) -> Status {
        match self {
            DdlFailure::Schema(_, s) | DdlFailure::Data(_, s) => s,
        }
    }
}

/// Applies DDL statements in order: first every statement against the schema, then the
/// row-dependent checks and data migration against the rows moved to each intermediate schema.
/// Nothing is applied on failure.
/// `now` is `CURRENT_TIMESTAMP()` for the DEFAULT values backfilled into existing rows.
pub fn apply_ddl(
    state: &DbState,
    statements: &[String],
    now: Timestamp,
) -> Result<DbState, DdlFailure> {
    let has_rows = state.row_count() > 0;
    let mut schema = (*state.schema).clone();
    // With rows, each statement's schema and checks, for the data phase.
    let mut steps: Vec<(Schema, Vec<DataCheck>)> = Vec::new();
    for (i, sql) in statements.iter().enumerate() {
        let checks = apply_schema(&mut schema, sql, !state.prod_messages)
            .map_err(|e| DdlFailure::Schema(i, e))?;
        if has_rows {
            steps.push((schema.clone(), checks));
        }
    }
    if !has_rows {
        return state
            .with_schema_at(schema, now)
            .map_err(|e| DdlFailure::Schema(0, to_status(e)));
    }
    let mut migrated = state.clone();
    for (i, (schema, checks)) in steps.into_iter().enumerate() {
        let next = migrated
            .with_schema_at(schema, now)
            .map_err(|e| DdlFailure::Data(i, to_status(e)))?;
        for check in &checks {
            next.run_data_check(check)
                .map_err(|e| DdlFailure::Data(i, to_status(e)))?;
        }
        migrated = next;
    }
    Ok(migrated)
}

/// Validates the DEFAULT, generated-column and CHECK expressions a statement declares through
/// the registered SQL evaluator (none without the `googlesql` feature).
fn validate_exprs(schema: &Schema, stmt: &Statement) -> Result<(), Status> {
    use spanbolt_storage::expr::ExprKind;
    let (table, only): (&str, Option<&str>) = match stmt {
        Statement::Opaque {
            object,
            drop: false,
        } if object.kind == "PROPERTY GRAPH" => {
            let Some(ev) = expr_evaluator() else {
                return Ok(());
            };
            return ev
                .validate_property_graph(&std::sync::Arc::new(schema.clone()), &object.name)
                .map_err(to_status);
        }
        Statement::CreateTable { table, .. } => (&table.name, None),
        Statement::AlterTable { table, action, .. } => match action {
            AlterTable::AddColumn { column, .. } | AlterTable::AlterColumn { column } => {
                (table, Some(column.name.as_str()))
            }
            AlterTable::SetColumnDefault { name, .. } => (table, Some(name.as_str())),
            AlterTable::AddCheck(c) => (table, Some(c.name.as_str())),
            _ => return Ok(()),
        },
        _ => return Ok(()),
    };
    let Some(t) = schema.table(table) else {
        return Ok(());
    };
    let mut work: Vec<(String, ExprKind)> = Vec::new();
    for c in &t.columns {
        if only.is_some_and(|o| !o.eq_ignore_ascii_case(&c.name)) {
            continue;
        }
        if let Some(d) = &c.default {
            work.push((
                d.clone(),
                ExprKind::Default {
                    column: c.name.clone(),
                    ty: c.ty.clone(),
                },
            ));
        }
        if let Some((e, _)) = c.generated.as_ref().and_then(|g| g.expr()) {
            work.push((
                e.to_string(),
                ExprKind::Generated {
                    column: c.name.clone(),
                },
            ));
        }
    }
    for ck in &t.checks {
        if only.is_none_or(|o| o.eq_ignore_ascii_case(&ck.name)) {
            work.push((
                ck.expr.clone(),
                ExprKind::Check {
                    name: ck.name.clone(),
                },
            ));
        }
    }
    if work.is_empty() {
        return Ok(());
    }
    let Some(ev) = expr_evaluator() else {
        return Ok(());
    };
    let catalog = spanbolt_storage::Catalog::build(schema).map_err(to_status)?;
    let Some(tm) = catalog.table_id(&t.name).map(|id| catalog.table(id)) else {
        return Ok(());
    };
    for (expr, kind) in &work {
        ev.validate(expr, tm, kind).map_err(to_status)?;
    }
    Ok(())
}

/// Applies one statement to `schema`, returning the data checks the existing rows must pass.
fn apply_schema(
    schema: &mut Schema,
    sql: &str,
    emulator_compat: bool,
) -> Result<Vec<DataCheck>, Status> {
    let ddl = |e: spanbolt_ddl::DdlError| to_status(e.into());
    let stmt = if emulator_compat {
        spanbolt_ddl::parse_statement_emulator_compat(sql)
    } else {
        spanbolt_ddl::parse_statement(sql)
    }
    .map_err(ddl)?;
    let checks = schema.data_checks(&stmt);
    for check in &checks {
        if let DataCheck::TableEmpty { message, .. } = check
            && matches!(
                &stmt,
                Statement::AlterTable {
                    action: AlterTable::AddColumn { .. },
                    ..
                }
            )
        {
            // The official emulator rejects ADD COLUMN NOT NULL without a default outright.
            return Err(Status::unimplemented(message.clone()));
        }
    }
    // `CREATE OR REPLACE VIEW` of an existing view words a bad query differently.
    let replacing = match &stmt {
        Statement::CreateView {
            view,
            or_replace: true,
        } => schema.views.contains_key(&view.name.to_ascii_lowercase()),
        _ => false,
    };
    let graph_change = expr_evaluator()
        .and_then(|ev| Some((ev, graph_change(schema, &stmt)?, Arc::new(schema.clone()))));
    if emulator_compat {
        schema.apply_emulator_compat(&stmt).map_err(ddl)?;
    } else {
        schema.apply(&stmt).map_err(ddl)?;
    }
    validate_exprs(schema, &stmt)?;
    if let Statement::CreateView { view, .. } = &stmt
        && let Some(ev) = expr_evaluator()
    {
        ev.validate_view(&Arc::new(schema.clone()), &view.name, replacing)
            .map_err(to_status)?;
    }
    if let Some((ev, change, before)) = graph_change {
        ev.check_graph_dependents(&before, &Arc::new(schema.clone()), &change)
            .map_err(to_status)?;
    }
    Ok(checks)
}

/// What `stmt` drops or changes that a property graph of `schema` may use: only when a graph's
/// definition names the table or view, so other statements skip the check.
fn graph_change(schema: &Schema, stmt: &Statement) -> Option<SchemaChange> {
    let graphs: Vec<&str> = schema
        .opaque
        .iter()
        .filter(|o| o.kind == "PROPERTY GRAPH")
        .map(|o| o.ddl.as_str())
        .collect();
    if graphs.is_empty() {
        return None;
    }
    let table = |name: &str| schema.table(name).map(|t| t.name.clone());
    let view = |name: &str| {
        schema
            .views
            .values()
            .find(|v| v.name.eq_ignore_ascii_case(name))
            .map(|v| v.name.clone())
    };
    let column = |t: &str, c: &str| {
        let t = schema.table(t)?;
        let c = t.columns.iter().find(|x| x.name.eq_ignore_ascii_case(c))?;
        Some((t.name.clone(), c.name.clone()))
    };
    let change = match stmt {
        Statement::DropTable { name, .. } => SchemaChange::DropTable(table(name)?),
        Statement::DropView { name, .. } => SchemaChange::DropView(view(name)?),
        Statement::CreateView {
            view: v,
            or_replace: true,
        } => SchemaChange::ReplaceView(view(&v.name)?),
        Statement::AlterTable {
            table: t, action, ..
        } => match action {
            AlterTable::DropColumn { name } => {
                let (table, column) = column(t, name)?;
                SchemaChange::DropColumn { table, column }
            }
            AlterTable::AlterColumn { column: c } => {
                let (table, column) = column(t, &c.name)?;
                SchemaChange::AlterColumn { table, column }
            }
            _ => return None,
        },
        _ => return None,
    };
    let object = match &change {
        SchemaChange::DropTable(n) | SchemaChange::DropView(n) | SchemaChange::ReplaceView(n) => n,
        SchemaChange::DropColumn { table, .. } | SchemaChange::AlterColumn { table, .. } => table,
    };
    graphs
        .iter()
        .any(|g| spanbolt_storage::expr::mentions(g, object))
        .then_some(change)
}

#[tonic::async_trait]
impl DatabaseAdmin for DatabaseAdminService {
    async fn list_databases(
        &self,
        req: Request<da::ListDatabasesRequest>,
    ) -> Result<Response<da::ListDatabasesResponse>, Status> {
        let req = req.get_ref();
        let inst = self.emu.instance(&req.parent)?;
        let all: Vec<_> = inst
            .databases
            .read()
            .values()
            .map(|d| to_proto(d, self.emu.emulator_compat))
            .collect();
        let (databases, next_page_token) = page(
            all,
            |d: &da::Database| &d.name,
            req.page_size,
            &req.page_token,
        );
        Ok(Response::new(da::ListDatabasesResponse {
            databases,
            next_page_token,
        }))
    }

    async fn create_database(
        &self,
        req: Request<da::CreateDatabaseRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        if req.database_dialect == da::DatabaseDialect::Postgresql as i32 {
            return Err(unimplemented("The PostgreSQL dialect"));
        }
        let id = names::parse_create_database(&req.create_statement)?;
        names::validate_database_id(&id)?;
        self.emu.instance(&req.parent)?;
        let name = format!("{}/databases/{id}", req.parent);
        let entry = self.emu.add_database(&name, || {
            let state = apply_ddl(
                &DbState::default(),
                &req.extra_statements,
                self.emu.clock.now(),
            )
            .map_err(DdlFailure::into_status)?;
            self.emu.new_database((*state.schema).clone())
        })?;
        let proto = to_proto(&entry, self.emu.emulator_compat);
        let meta = da::CreateDatabaseMetadata {
            database: name.clone(),
        };
        let op = done(
            format!("{name}/operations/_auto_op_{}", self.emu.opaque_id()),
            any(&format!("{TYPE_PREFIX}CreateDatabaseMetadata"), &meta),
            any(&format!("{TYPE_PREFIX}Database"), &proto),
        );
        Ok(Response::new(self.emu.put_operation(op)))
    }

    async fn get_database(
        &self,
        req: Request<da::GetDatabaseRequest>,
    ) -> Result<Response<da::Database>, Status> {
        let entry = self.emu.database(&req.get_ref().name)?;
        Ok(Response::new(to_proto(&entry, self.emu.emulator_compat)))
    }

    async fn update_database(
        &self,
        req: Request<da::UpdateDatabaseRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        let spec = req
            .database
            .ok_or_else(|| Status::invalid_argument("Missing database"))?;
        let entry = self.emu.database(&spec.name)?;
        let mut proto = to_proto(&entry, self.emu.emulator_compat);
        proto.enable_drop_protection = spec.enable_drop_protection;
        let meta = da::UpdateDatabaseMetadata {
            request: None,
            progress: None,
            cancel_time: None,
        };
        let op = done(
            format!(
                "{}/operations/_auto_op_{}",
                entry.name,
                self.emu.opaque_id()
            ),
            any(&format!("{TYPE_PREFIX}UpdateDatabaseMetadata"), &meta),
            any(&format!("{TYPE_PREFIX}Database"), &proto),
        );
        Ok(Response::new(self.emu.put_operation(op)))
    }

    async fn update_database_ddl(
        &self,
        req: Request<da::UpdateDatabaseDdlRequest>,
    ) -> Result<Response<Operation>, Status> {
        crate::timing::note(
            req.extensions(),
            || req.get_ref().statements.join(";\n"),
            Some(req.get_ref().statements.len() as u64),
        );
        let req = req.into_inner();
        if !req.operation_id.is_empty() && !valid_operation_id(&req.operation_id) {
            return Err(Status::invalid_argument(format!(
                "Invalid operation id: {}.",
                req.operation_id
            )));
        }
        let entry = self.emu.database(&req.database)?;
        if req.statements.is_empty() {
            return Err(Status::invalid_argument(
                "Missing statements in the request.",
            ));
        }
        let op_name = format!(
            "{}/operations/{}",
            entry.name,
            if req.operation_id.is_empty() {
                format!("_auto_op_{}", self.emu.opaque_id())
            } else {
                req.operation_id.clone()
            }
        );
        if !req.operation_id.is_empty() && self.emu.operation(&op_name).is_ok() {
            return Err(Status::already_exists(format!(
                "Operation already exists: {op_name}"
            )));
        }
        // Cloud Spanner rejects the request when a statement doesn't parse. Past that, a statement
        // that is invalid against the schema, or whose rows violate it, fails the operation and
        // keeps the statements before it. The emulator (`--emulator-compat`) fails the request
        // and applies nothing for an invalid statement, and keeps the statements before one the
        // rows violate. UNIMPLEMENTED always fails the request.
        let compat = self.emu.emulator_compat;
        if !compat {
            for sql in &req.statements {
                spanbolt_ddl::parse_statement(sql).map_err(|e| to_status(e.into()))?;
            }
        }
        let mut schema_error: Option<Status> = None;
        let mut op_error: Option<(usize, Status)> = None;
        let now = self.emu.clock.now();
        let result = entry.db.update_state(|s| {
            let (mut keep, mut status) = match apply_ddl(s, &req.statements, now) {
                Ok(next) => return Ok((next, req.statements.join("; "))),
                Err(DdlFailure::Schema(_, e))
                    if compat || e.code() == tonic::Code::Unimplemented =>
                {
                    let err = spanbolt_storage::Error::new(code_of(&e), e.message());
                    schema_error = Some(e);
                    return Err(err);
                }
                Err(DdlFailure::Schema(i, e) | DdlFailure::Data(i, e)) => (i, e),
            };
            // The statements before the failing one. A schema failure is found before the rows
            // are checked, so they can still fail on the rows of an earlier statement: then that
            // one is the error, and fewer statements are kept.
            let next = loop {
                if keep == 0 {
                    break None;
                }
                match apply_ddl(s, &req.statements[..keep], now) {
                    Ok(next) => break Some(next),
                    Err(DdlFailure::Schema(i, e) | DdlFailure::Data(i, e)) => {
                        keep = i;
                        status = e;
                    }
                }
            };
            let err = spanbolt_storage::Error::new(code_of(&status), status.message());
            op_error = Some((keep, status));
            next.map(|next| (next, req.statements[..keep].join("; ")))
                .ok_or(err)
        });
        if let Some(e) = schema_error {
            return Err(e);
        }
        let committed = match (&result, &op_error) {
            (Ok(_), Some((keep, _))) => *keep,
            (Ok(_), None) => req.statements.len(),
            (Err(_), Some(_)) => 0,
            (Err(e), None) => return Err(to_status(e.clone())),
        };
        let meta = da::UpdateDatabaseDdlMetadata {
            database: entry.name.clone(),
            statements: req.statements.clone(),
            commit_timestamps: match &result {
                Ok(ts) => vec![ts.to_proto(); committed],
                Err(_) => Vec::new(),
            },
            ..Default::default()
        };
        let meta = any(&format!("{TYPE_PREFIX}UpdateDatabaseDdlMetadata"), &meta);
        let op = match op_error {
            Some((_, e)) => failed(op_name, meta, &e),
            None => done(
                op_name,
                meta,
                any("type.googleapis.com/google.protobuf.Empty", &()),
            ),
        };
        Ok(Response::new(self.emu.put_operation(op)))
    }

    async fn drop_database(
        &self,
        req: Request<da::DropDatabaseRequest>,
    ) -> Result<Response<()>, Status> {
        self.emu.drop_database(&req.get_ref().database)?;
        Ok(Response::new(()))
    }

    async fn get_database_ddl(
        &self,
        req: Request<da::GetDatabaseDdlRequest>,
    ) -> Result<Response<da::GetDatabaseDdlResponse>, Status> {
        let entry = self.emu.database(&req.get_ref().database)?;
        let schema = &entry.db.latest().state.schema;
        // The emulator leaves roles and grants out of GetDatabaseDdl.
        let statements = if self.emu.emulator_compat {
            schema.to_ddl_emulator_compat()
        } else {
            schema.to_ddl()
        };
        Ok(Response::new(da::GetDatabaseDdlResponse {
            statements,
            proto_descriptors: Vec::new(),
        }))
    }

    async fn set_iam_policy(
        &self,
        _: Request<iam::SetIamPolicyRequest>,
    ) -> Result<Response<iam::Policy>, Status> {
        Err(unimplemented("SetIamPolicy"))
    }

    async fn get_iam_policy(
        &self,
        _: Request<iam::GetIamPolicyRequest>,
    ) -> Result<Response<iam::Policy>, Status> {
        Err(unimplemented("GetIamPolicy"))
    }

    async fn test_iam_permissions(
        &self,
        _: Request<iam::TestIamPermissionsRequest>,
    ) -> Result<Response<iam::TestIamPermissionsResponse>, Status> {
        Err(unimplemented("TestIamPermissions"))
    }

    async fn create_backup(
        &self,
        _: Request<da::CreateBackupRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(unimplemented("CreateBackup"))
    }

    async fn copy_backup(
        &self,
        _: Request<da::CopyBackupRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(unimplemented("CopyBackup"))
    }

    async fn get_backup(
        &self,
        _: Request<da::GetBackupRequest>,
    ) -> Result<Response<da::Backup>, Status> {
        Err(unimplemented("GetBackup"))
    }

    async fn update_backup(
        &self,
        _: Request<da::UpdateBackupRequest>,
    ) -> Result<Response<da::Backup>, Status> {
        Err(unimplemented("UpdateBackup"))
    }

    async fn delete_backup(
        &self,
        _: Request<da::DeleteBackupRequest>,
    ) -> Result<Response<()>, Status> {
        Err(unimplemented("DeleteBackup"))
    }

    async fn list_backups(
        &self,
        _: Request<da::ListBackupsRequest>,
    ) -> Result<Response<da::ListBackupsResponse>, Status> {
        Ok(Response::new(da::ListBackupsResponse::default()))
    }

    async fn restore_database(
        &self,
        _: Request<da::RestoreDatabaseRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(unimplemented("RestoreDatabase"))
    }

    async fn list_database_operations(
        &self,
        _: Request<da::ListDatabaseOperationsRequest>,
    ) -> Result<Response<da::ListDatabaseOperationsResponse>, Status> {
        // Like the emulator, which leaves the method unimplemented (an empty message); the
        // longrunning ListOperations on `<database>/operations` lists a database's operations.
        Err(Status::unimplemented(""))
    }

    async fn list_backup_operations(
        &self,
        _: Request<da::ListBackupOperationsRequest>,
    ) -> Result<Response<da::ListBackupOperationsResponse>, Status> {
        Ok(Response::new(da::ListBackupOperationsResponse::default()))
    }

    async fn list_database_roles(
        &self,
        req: Request<da::ListDatabaseRolesRequest>,
    ) -> Result<Response<da::ListDatabaseRolesResponse>, Status> {
        // The emulator leaves the method unimplemented (an empty message).
        if self.emu.emulator_compat {
            return Err(Status::unimplemented(""));
        }
        let req = req.get_ref();
        let entry = self.emu.database(&req.parent)?;
        let schema = &entry.db.latest().state.schema;
        // Production lists the database's roles and the system roles every database has.
        let roles = schema
            .opaque
            .iter()
            .filter(|o| o.kind == "ROLE")
            .map(|o| o.name.as_str())
            .chain(spanbolt_ddl::SYSTEM_ROLES)
            .map(|r| da::DatabaseRole {
                name: format!("{}/databaseRoles/{r}", entry.name),
            });
        let (database_roles, next_page_token) = page(
            roles,
            |r: &da::DatabaseRole| &r.name,
            req.page_size,
            &req.page_token,
        );
        Ok(Response::new(da::ListDatabaseRolesResponse {
            database_roles,
            next_page_token,
        }))
    }

    async fn add_split_points(
        &self,
        req: Request<da::AddSplitPointsRequest>,
    ) -> Result<Response<da::AddSplitPointsResponse>, Status> {
        self.emu.database(&req.get_ref().database)?;
        Ok(Response::new(da::AddSplitPointsResponse {}))
    }

    async fn create_backup_schedule(
        &self,
        _: Request<da::CreateBackupScheduleRequest>,
    ) -> Result<Response<da::BackupSchedule>, Status> {
        Err(unimplemented("CreateBackupSchedule"))
    }

    async fn get_backup_schedule(
        &self,
        _: Request<da::GetBackupScheduleRequest>,
    ) -> Result<Response<da::BackupSchedule>, Status> {
        Err(unimplemented("GetBackupSchedule"))
    }

    async fn update_backup_schedule(
        &self,
        _: Request<da::UpdateBackupScheduleRequest>,
    ) -> Result<Response<da::BackupSchedule>, Status> {
        Err(unimplemented("UpdateBackupSchedule"))
    }

    async fn delete_backup_schedule(
        &self,
        _: Request<da::DeleteBackupScheduleRequest>,
    ) -> Result<Response<()>, Status> {
        Err(unimplemented("DeleteBackupSchedule"))
    }

    async fn list_backup_schedules(
        &self,
        _: Request<da::ListBackupSchedulesRequest>,
    ) -> Result<Response<da::ListBackupSchedulesResponse>, Status> {
        Ok(Response::new(da::ListBackupSchedulesResponse::default()))
    }

    async fn internal_update_graph_operation(
        &self,
        _: Request<da::InternalUpdateGraphOperationRequest>,
    ) -> Result<Response<da::InternalUpdateGraphOperationResponse>, Status> {
        Err(unimplemented("InternalUpdateGraphOperation"))
    }
}

/// The emulator's operation ids: a lowercase letter, then 1 to 127 lowercase letters, digits or `_`.
fn valid_operation_id(id: &str) -> bool {
    let b = id.as_bytes();
    (2..=128).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

fn code_of(s: &Status) -> spanbolt_storage::Code {
    use spanbolt_storage::Code;
    match s.code() {
        tonic::Code::NotFound => Code::NotFound,
        tonic::Code::AlreadyExists => Code::AlreadyExists,
        tonic::Code::FailedPrecondition => Code::FailedPrecondition,
        _ => Code::InvalidArgument,
    }
}
