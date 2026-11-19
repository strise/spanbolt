use std::path::PathBuf;
use std::sync::Arc;

use spanbolt_proto::control as ctl;
use spanbolt_proto::control::control_server::Control;
use spanbolt_storage::snapshot;
use tonic::{Request, Response, Status};

use crate::convert;
use crate::emulator::Emulator;
use crate::names;
use crate::status::to_status;

pub struct ControlService {
    pub emu: Arc<Emulator>,
}

#[tonic::async_trait]
impl Control for ControlService {
    async fn clone_database(
        &self,
        req: Request<ctl::CloneDatabaseRequest>,
    ) -> Result<Response<ctl::CloneDatabaseResponse>, Status> {
        let req = req.into_inner();
        let source = self.emu.database(&req.source)?;
        let target = names::database_name(&req.target)?;
        names::validate_database_id(target.id)?;
        self.emu
            .add_database(&req.target, || Ok(source.db.clone_database()))?;
        Ok(Response::new(ctl::CloneDatabaseResponse {}))
    }

    async fn save_snapshot(
        &self,
        req: Request<ctl::SaveSnapshotRequest>,
    ) -> Result<Response<ctl::SaveSnapshotResponse>, Status> {
        let path = PathBuf::from(req.into_inner().path);
        let images = self.emu.images();
        let bytes = tokio::task::spawn_blocking(move || snapshot::save(&path, &images))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(to_status)?;
        Ok(Response::new(ctl::SaveSnapshotResponse { bytes }))
    }

    async fn load_snapshot(
        &self,
        req: Request<ctl::LoadSnapshotRequest>,
    ) -> Result<Response<ctl::LoadSnapshotResponse>, Status> {
        let path = PathBuf::from(req.into_inner().path);
        let images = tokio::task::spawn_blocking(move || snapshot::load(&path))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(to_status)?;
        let (instances, databases) = self.emu.restore(images);
        Ok(Response::new(ctl::LoadSnapshotResponse {
            instances,
            databases,
        }))
    }

    async fn set_clock(
        &self,
        req: Request<ctl::SetClockRequest>,
    ) -> Result<Response<ctl::SetClockResponse>, Status> {
        match req.into_inner().clock {
            Some(ctl::set_clock_request::Clock::Timestamp(t)) => {
                self.emu.clock.set(Some(convert::timestamp(&t)?))
            }
            Some(ctl::set_clock_request::Clock::Reset(_)) | None => self.emu.clock.set(None),
        }
        Ok(Response::new(ctl::SetClockResponse {}))
    }

    async fn inject_abort(
        &self,
        req: Request<ctl::InjectAbortRequest>,
    ) -> Result<Response<ctl::InjectAbortResponse>, Status> {
        let req = req.into_inner();
        self.emu
            .database(&req.database)?
            .db
            .inject_aborts(req.count);
        Ok(Response::new(ctl::InjectAbortResponse {}))
    }
}
