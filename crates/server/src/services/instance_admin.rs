use std::sync::Arc;

use spanbolt_proto::google::iam::v1 as iam;
use spanbolt_proto::instance_admin as ia;
use spanbolt_proto::instance_admin::instance_admin_server::InstanceAdmin;
use spanbolt_proto::longrunning::Operation;
use tonic::{Request, Response, Status};

use crate::emulator::{EMULATOR_CONFIG, Emulator, Instance, InstanceMeta};
use crate::names;
use crate::page::page;
use crate::services::operations::{any, done};
use crate::status::unimplemented;

pub struct InstanceAdminService {
    pub emu: Arc<Emulator>,
}

const TYPE_PREFIX: &str = "type.googleapis.com/google.spanner.admin.instance.v1.";

fn config(project: &str) -> ia::InstanceConfig {
    ia::InstanceConfig {
        name: format!("projects/{project}/instanceConfigs/{EMULATOR_CONFIG}"),
        display_name: "Emulator Instance Config".into(),
        config_type: ia::instance_config::Type::GoogleManaged as i32,
        state: ia::instance_config::State::Ready as i32,
        ..Default::default()
    }
}

fn to_proto(inst: &Instance) -> ia::Instance {
    let m = inst.meta.read();
    ia::Instance {
        name: inst.name.clone(),
        config: m.config.clone(),
        display_name: m.display_name.clone(),
        node_count: m.node_count,
        processing_units: m.processing_units,
        state: ia::instance::State::Ready as i32,
        labels: m.labels.clone().into_iter().collect(),
        create_time: Some(m.create_time.to_proto()),
        update_time: Some(m.update_time.to_proto()),
        ..Default::default()
    }
}

/// Spanner derives one of node_count / processing_units from the other.
fn capacity(node_count: i32, processing_units: i32) -> (i32, i32) {
    match (node_count, processing_units) {
        (0, 0) => (1, 1000),
        (n, 0) => (n, n * 1000),
        (0, p) => (p / 1000, p),
        (n, p) => (n, p),
    }
}

#[tonic::async_trait]
impl InstanceAdmin for InstanceAdminService {
    async fn list_instance_configs(
        &self,
        req: Request<ia::ListInstanceConfigsRequest>,
    ) -> Result<Response<ia::ListInstanceConfigsResponse>, Status> {
        let req = req.get_ref();
        let project = names::project_from_parent(&req.parent)?;
        let (instance_configs, next_page_token) = page(
            vec![config(project)],
            |c| &c.name,
            req.page_size,
            &req.page_token,
        );
        Ok(Response::new(ia::ListInstanceConfigsResponse {
            instance_configs,
            next_page_token,
        }))
    }

    async fn get_instance_config(
        &self,
        req: Request<ia::GetInstanceConfigRequest>,
    ) -> Result<Response<ia::InstanceConfig>, Status> {
        let name = &req.get_ref().name;
        match name.split('/').collect::<Vec<_>>().as_slice() {
            ["projects", p, "instanceConfigs", c] if *c == EMULATOR_CONFIG => {
                Ok(Response::new(config(p)))
            }
            _ => Err(Status::not_found(format!(
                "Instance config not found: {name}"
            ))),
        }
    }

    async fn create_instance_config(
        &self,
        _: Request<ia::CreateInstanceConfigRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(unimplemented("CreateInstanceConfig"))
    }

    async fn update_instance_config(
        &self,
        _: Request<ia::UpdateInstanceConfigRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(unimplemented("UpdateInstanceConfig"))
    }

    async fn delete_instance_config(
        &self,
        _: Request<ia::DeleteInstanceConfigRequest>,
    ) -> Result<Response<()>, Status> {
        Err(unimplemented("DeleteInstanceConfig"))
    }

    async fn list_instance_config_operations(
        &self,
        _: Request<ia::ListInstanceConfigOperationsRequest>,
    ) -> Result<Response<ia::ListInstanceConfigOperationsResponse>, Status> {
        Ok(Response::new(
            ia::ListInstanceConfigOperationsResponse::default(),
        ))
    }

    async fn list_instances(
        &self,
        req: Request<ia::ListInstancesRequest>,
    ) -> Result<Response<ia::ListInstancesResponse>, Status> {
        let req = req.get_ref();
        let project = names::project_from_parent(&req.parent)?;
        let all: Vec<_> = self
            .emu
            .instances_in(project)
            .iter()
            .map(|i| to_proto(i))
            .collect();
        let (instances, next_page_token) = page(
            all,
            |i: &ia::Instance| &i.name,
            req.page_size,
            &req.page_token,
        );
        Ok(Response::new(ia::ListInstancesResponse {
            instances,
            next_page_token,
            ..Default::default()
        }))
    }

    async fn list_instance_partitions(
        &self,
        _: Request<ia::ListInstancePartitionsRequest>,
    ) -> Result<Response<ia::ListInstancePartitionsResponse>, Status> {
        Ok(Response::new(ia::ListInstancePartitionsResponse::default()))
    }

    async fn get_instance(
        &self,
        req: Request<ia::GetInstanceRequest>,
    ) -> Result<Response<ia::Instance>, Status> {
        let inst = self.emu.instance(&req.get_ref().name)?;
        Ok(Response::new(to_proto(&inst)))
    }

    async fn create_instance(
        &self,
        req: Request<ia::CreateInstanceRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        names::project_from_parent(&req.parent)?;
        names::validate_instance_id(&req.instance_id)?;
        let name = format!("{}/instances/{}", req.parent, req.instance_id);
        let spec = req.instance.unwrap_or_default();
        let (node_count, processing_units) = capacity(spec.node_count, spec.processing_units);
        let now = self.emu.clock.now();
        let inst = self.emu.create_instance(
            name.clone(),
            InstanceMeta {
                config: spec.config,
                display_name: if spec.display_name.is_empty() {
                    req.instance_id.clone()
                } else {
                    spec.display_name
                },
                node_count,
                processing_units,
                labels: spec.labels.into_iter().collect(),
                create_time: now,
                update_time: now,
            },
        )?;
        let proto = to_proto(&inst);
        let meta = ia::CreateInstanceMetadata {
            instance: Some(proto.clone()),
            start_time: Some(now.to_proto()),
            end_time: Some(now.to_proto()),
            ..Default::default()
        };
        let op = done(
            format!("{name}/operations/{}", self.emu.opaque_id()),
            any(&format!("{TYPE_PREFIX}CreateInstanceMetadata"), &meta),
            any(&format!("{TYPE_PREFIX}Instance"), &proto),
        );
        Ok(Response::new(self.emu.put_operation(op)))
    }

    async fn update_instance(
        &self,
        req: Request<ia::UpdateInstanceRequest>,
    ) -> Result<Response<Operation>, Status> {
        let req = req.into_inner();
        let spec = req
            .instance
            .ok_or_else(|| Status::invalid_argument("Missing instance"))?;
        let inst = self.emu.instance(&spec.name)?;
        let paths = req.field_mask.map(|m| m.paths).unwrap_or_default();
        {
            let mut m = inst.meta.write();
            for p in &paths {
                match p.as_str() {
                    "display_name" => m.display_name = spec.display_name.clone(),
                    "node_count" => {
                        (m.node_count, m.processing_units) = capacity(spec.node_count, 0)
                    }
                    "processing_units" => {
                        (m.node_count, m.processing_units) = capacity(0, spec.processing_units)
                    }
                    "labels" => m.labels = spec.labels.clone().into_iter().collect(),
                    "config" => m.config = spec.config.clone(),
                    _ => {}
                }
            }
            m.update_time = self.emu.clock.now();
        }
        let proto = to_proto(&inst);
        let now = self.emu.clock.now().to_proto();
        let meta = ia::UpdateInstanceMetadata {
            instance: Some(proto.clone()),
            start_time: Some(now),
            end_time: Some(now),
            ..Default::default()
        };
        let op = done(
            format!("{}/operations/{}", inst.name, self.emu.opaque_id()),
            any(&format!("{TYPE_PREFIX}UpdateInstanceMetadata"), &meta),
            any(&format!("{TYPE_PREFIX}Instance"), &proto),
        );
        Ok(Response::new(self.emu.put_operation(op)))
    }

    async fn delete_instance(
        &self,
        req: Request<ia::DeleteInstanceRequest>,
    ) -> Result<Response<()>, Status> {
        self.emu.delete_instance(&req.get_ref().name)?;
        Ok(Response::new(()))
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

    async fn get_instance_partition(
        &self,
        _: Request<ia::GetInstancePartitionRequest>,
    ) -> Result<Response<ia::InstancePartition>, Status> {
        Err(unimplemented("GetInstancePartition"))
    }

    async fn create_instance_partition(
        &self,
        _: Request<ia::CreateInstancePartitionRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(unimplemented("CreateInstancePartition"))
    }

    async fn delete_instance_partition(
        &self,
        _: Request<ia::DeleteInstancePartitionRequest>,
    ) -> Result<Response<()>, Status> {
        Err(unimplemented("DeleteInstancePartition"))
    }

    async fn update_instance_partition(
        &self,
        _: Request<ia::UpdateInstancePartitionRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(unimplemented("UpdateInstancePartition"))
    }

    async fn list_instance_partition_operations(
        &self,
        _: Request<ia::ListInstancePartitionOperationsRequest>,
    ) -> Result<Response<ia::ListInstancePartitionOperationsResponse>, Status> {
        Ok(Response::new(
            ia::ListInstancePartitionOperationsResponse::default(),
        ))
    }

    async fn move_instance(
        &self,
        _: Request<ia::MoveInstanceRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(unimplemented("MoveInstance"))
    }
}
