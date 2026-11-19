use std::sync::Arc;

use prost::Message;
use spanbolt_proto::longrunning as lro;
use spanbolt_proto::longrunning::operations_server::Operations;
use tonic::{Request, Response, Status};

use crate::emulator::Emulator;
use crate::page::page;

pub fn any(type_url: &str, msg: &impl Message) -> prost_types::Any {
    prost_types::Any {
        type_url: type_url.to_string(),
        value: msg.encode_to_vec(),
    }
}

/// A completed operation (every admin operation in spanbolt completes immediately).
pub fn done(
    name: String,
    metadata: prost_types::Any,
    response: prost_types::Any,
) -> lro::Operation {
    lro::Operation {
        name,
        metadata: Some(metadata),
        done: true,
        result: Some(lro::operation::Result::Response(response)),
    }
}

pub fn failed(name: String, metadata: prost_types::Any, status: &Status) -> lro::Operation {
    lro::Operation {
        name,
        metadata: Some(metadata),
        done: true,
        result: Some(lro::operation::Result::Error(
            spanbolt_proto::google::rpc::Status {
                code: status.code() as i32,
                message: status.message().to_string(),
                details: Vec::new(),
            },
        )),
    }
}

pub struct OperationsService {
    pub emu: Arc<Emulator>,
}

#[tonic::async_trait]
impl Operations for OperationsService {
    async fn list_operations(
        &self,
        req: Request<lro::ListOperationsRequest>,
    ) -> Result<Response<lro::ListOperationsResponse>, Status> {
        let req = req.get_ref();
        let prefix = req.name.trim_end_matches('/');
        let (operations, next_page_token) = page(
            self.emu.operations_under(prefix),
            |o| &o.name,
            req.page_size,
            &req.page_token,
        );
        Ok(Response::new(lro::ListOperationsResponse {
            operations,
            next_page_token,
            unreachable: Vec::new(),
        }))
    }

    async fn get_operation(
        &self,
        req: Request<lro::GetOperationRequest>,
    ) -> Result<Response<lro::Operation>, Status> {
        Ok(Response::new(self.emu.operation(&req.get_ref().name)?))
    }

    async fn delete_operation(
        &self,
        req: Request<lro::DeleteOperationRequest>,
    ) -> Result<Response<()>, Status> {
        self.emu.delete_operation(&req.get_ref().name)?;
        Ok(Response::new(()))
    }

    async fn cancel_operation(
        &self,
        req: Request<lro::CancelOperationRequest>,
    ) -> Result<Response<()>, Status> {
        self.emu.operation(&req.get_ref().name)?;
        Ok(Response::new(()))
    }

    async fn wait_operation(
        &self,
        req: Request<lro::WaitOperationRequest>,
    ) -> Result<Response<lro::Operation>, Status> {
        Ok(Response::new(self.emu.operation(&req.get_ref().name)?))
    }
}
