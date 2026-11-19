//! Generated googleapis types and tonic services, plus spanbolt's `Control` service.
#![allow(clippy::all, clippy::pedantic, rustdoc::all, missing_docs)]

pub mod google {
    pub mod api {
        tonic::include_proto!("google.api");
    }
    pub mod rpc {
        tonic::include_proto!("google.rpc");
    }
    pub mod r#type {
        tonic::include_proto!("google.r#type");
    }
    pub mod iam {
        pub mod v1 {
            tonic::include_proto!("google.iam.v1");
        }
    }
    pub mod longrunning {
        tonic::include_proto!("google.longrunning");
    }
    pub mod spanner {
        pub mod v1 {
            tonic::include_proto!("google.spanner.v1");
        }
        pub mod admin {
            pub mod database {
                pub mod v1 {
                    tonic::include_proto!("google.spanner.admin.database.v1");
                }
            }
            pub mod instance {
                pub mod v1 {
                    tonic::include_proto!("google.spanner.admin.instance.v1");
                }
            }
        }
    }
}

pub mod spanbolt {
    pub mod v1 {
        tonic::include_proto!("spanbolt.v1");
    }
}

pub use google::longrunning;
pub use google::spanner::admin::database::v1 as database_admin;
pub use google::spanner::admin::instance::v1 as instance_admin;
pub use google::spanner::v1 as spanner;
pub use spanbolt::v1 as control;

/// Every message and service above as a `FileDescriptorSet` (imports included), for reflection
/// such as the REST gateway's proto3 JSON mapping.
pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/spanbolt_descriptor.bin"));
