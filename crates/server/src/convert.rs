//! Conversions between wire types and storage types.

use spanbolt_ddl::ColumnType;
use spanbolt_proto::spanner as pb;
use spanbolt_storage::{KeyRange, KeySet, Timestamp, decode_key, decode_point_key};
use tonic::Status;

use crate::status::to_status;

pub fn spanner_type(t: &ColumnType) -> pb::Type {
    use pb::TypeCode as C;
    let code = |c: C| pb::Type {
        code: c as i32,
        ..Default::default()
    };
    match t {
        ColumnType::Bool => code(C::Bool),
        ColumnType::Int64 => code(C::Int64),
        ColumnType::Float32 => code(C::Float32),
        ColumnType::Float64 => code(C::Float64),
        ColumnType::Numeric => code(C::Numeric),
        ColumnType::String(_) => code(C::String),
        ColumnType::Bytes(_) | ColumnType::TokenList => code(C::Bytes),
        ColumnType::Date => code(C::Date),
        ColumnType::Timestamp => code(C::Timestamp),
        ColumnType::Json => code(C::Json),
        ColumnType::Interval => code(C::Interval),
        ColumnType::Uuid => code(C::Uuid),
        ColumnType::Array(e) => pb::Type {
            code: C::Array as i32,
            array_element_type: Some(Box::new(spanner_type(e))),
            ..Default::default()
        },
        ColumnType::Struct(fields) => pb::Type {
            code: C::Struct as i32,
            struct_type: Some(pb::StructType {
                fields: fields
                    .iter()
                    .map(|(n, t)| pb::struct_type::Field {
                        name: n.clone().unwrap_or_default(),
                        r#type: Some(spanner_type(t)),
                    })
                    .collect(),
            }),
            ..Default::default()
        },
    }
}

/// `name` (table or index) is used in key-arity errors, worded as production Spanner does with
/// `prod_messages`.
pub fn keyset(
    ks: Option<&pb::KeySet>,
    types: &[ColumnType],
    name: &str,
    prod_messages: bool,
) -> spanbolt_storage::Result<KeySet> {
    let Some(ks) = ks else {
        return Ok(KeySet::default());
    };
    if ks.all {
        return Ok(KeySet::all());
    }
    let mut out = KeySet::default();
    for k in &ks.keys {
        out.keys
            .push(decode_point_key(k, types, name, prod_messages)?);
    }
    use pb::key_range::{EndKeyType, StartKeyType};
    for r in &ks.ranges {
        let (start, start_closed) = match &r.start_key_type {
            Some(StartKeyType::StartClosed(l)) => (decode_key(l, types)?, true),
            Some(StartKeyType::StartOpen(l)) => (decode_key(l, types)?, false),
            None => (Vec::new(), true),
        };
        let (end, end_closed) = match &r.end_key_type {
            Some(EndKeyType::EndClosed(l)) => (decode_key(l, types)?, true),
            Some(EndKeyType::EndOpen(l)) => (decode_key(l, types)?, false),
            None => (Vec::new(), true),
        };
        out.ranges.push(KeyRange {
            start,
            start_closed,
            end,
            end_closed,
        });
    }
    Ok(out)
}

pub fn timestamp(t: &prost_types::Timestamp) -> Result<Timestamp, Status> {
    Timestamp::from_proto(t).ok_or_else(|| Status::invalid_argument("Invalid timestamp"))
}

pub fn duration(d: &prost_types::Duration) -> Result<std::time::Duration, Status> {
    if d.seconds < 0 || d.nanos < 0 {
        return Err(Status::invalid_argument("Staleness must be non-negative."));
    }
    Ok(std::time::Duration::new(d.seconds as u64, d.nanos as u32))
}

pub fn storage_err(e: spanbolt_storage::Error) -> Status {
    to_status(e)
}
