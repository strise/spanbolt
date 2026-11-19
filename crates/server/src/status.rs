use prost::Message;
use spanbolt_storage::{Code, Error};
use tonic::Status;

pub fn to_status(e: Error) -> Status {
    let code = match e.code {
        Code::InvalidArgument => tonic::Code::InvalidArgument,
        Code::NotFound => tonic::Code::NotFound,
        Code::AlreadyExists => tonic::Code::AlreadyExists,
        Code::FailedPrecondition => tonic::Code::FailedPrecondition,
        Code::OutOfRange => tonic::Code::OutOfRange,
        Code::Aborted => tonic::Code::Aborted,
        Code::Unimplemented => tonic::Code::Unimplemented,
        Code::Internal => tonic::Code::Internal,
        Code::DeadlineExceeded => tonic::Code::DeadlineExceeded,
        Code::ResourceExhausted => tonic::Code::ResourceExhausted,
    };
    if e.constraint {
        let detail = prost_types::Any {
            type_url: "google.spanner.ConstraintError".into(),
            value: Vec::new(),
        };
        return with_details(code, e.message, vec![detail]);
    }
    Status::new(code, e.message)
}

fn with_details(code: tonic::Code, message: String, details: Vec<prost_types::Any>) -> Status {
    let st = spanbolt_proto::google::rpc::Status {
        code: code as i32,
        message: message.clone(),
        details,
    };
    Status::with_details(code, message, st.encode_to_vec().into())
}

/// NOT_FOUND with the `ResourceInfo` detail the emulator attaches.
pub fn resource_not_found(kind: &str, type_name: &str, name: &str) -> Status {
    let info = spanbolt_proto::google::rpc::ResourceInfo {
        resource_type: format!("type.googleapis.com/{type_name}"),
        resource_name: name.to_string(),
        owner: String::new(),
        description: format!("{kind} does not exist."),
    };
    let detail = prost_types::Any {
        type_url: "type.googleapis.com/google.rpc.ResourceInfo".into(),
        value: info.encode_to_vec(),
    };
    with_details(
        tonic::Code::NotFound,
        format!("{kind} not found: {name}"),
        vec![detail],
    )
}

pub fn unimplemented(what: &str) -> Status {
    Status::unimplemented(format!("{what} is not supported by spanbolt yet."))
}
