//! Type conversions between GoogleSQL's `TypeProto`, Spanner's wire `Type` and the schema's
//! `ColumnType`, which is also the runtime type of query values.

use googlesql_sys::proto::{TypeKind, TypeProto};
use spanbolt_ddl::ColumnType;
use spanbolt_proto::spanner as pb;
use spanbolt_storage::{Code, Error, Result};

pub type Type = ColumnType;

pub fn unimplemented(what: impl std::fmt::Display) -> Error {
    Error::new(
        Code::Unimplemented,
        format!("{what} is not supported by spanbolt yet."),
    )
}

/// GoogleSQL's name of `INFORMATION_SCHEMA.SCHEMATA.PROTO_BUNDLE`'s type, the only PROTO type.
pub const FILE_DESCRIPTOR_SET: &str = "google.protobuf.FileDescriptorSet";

/// Production's name for a PROTO type: Spanner's descriptors live in `proto2`, not
/// `google.protobuf`. INFORMATION_SCHEMA spells it this way in both modes; result metadata only
/// by default.
pub fn production_proto_name(name: &str) -> String {
    match name.strip_prefix("google.protobuf.") {
        Some(rest) => format!("proto2.{rest}"),
        None => name.to_string(),
    }
}

/// The type of a column read from a table or returned by a query. Unlike [`from_proto`], it
/// accepts `PROTO_BUNDLE`'s PROTO type, which spanbolt can scan and return but nothing else.
pub fn column_from_proto(t: &TypeProto) -> Result<Type> {
    if t.type_kind == Some(TypeKind::TypeProto as i32)
        && let Some(name) = t.proto_type.as_ref().and_then(|p| p.proto_name.as_deref())
        && name == FILE_DESCRIPTOR_SET
    {
        return Ok(Type::Proto(name.to_string()));
    }
    from_proto(t)
}

/// A query result column's type as the mode reports it: production names PROTO types
/// `proto2.*`, the emulator `google.protobuf.*`.
pub fn result_type(t: Type, emulator_compat: bool) -> Type {
    match t {
        Type::Proto(name) if !emulator_compat => Type::Proto(production_proto_name(&name)),
        other => other,
    }
}

pub fn from_proto(t: &TypeProto) -> Result<Type> {
    let kind = t.type_kind.and_then(|k| TypeKind::try_from(k).ok());
    Ok(match kind {
        Some(TypeKind::TypeBool) => Type::Bool,
        // Enum-typed arguments (date parts) are carried as their numbers.
        Some(TypeKind::TypeInt64) | Some(TypeKind::TypeEnum) => Type::Int64,
        Some(TypeKind::TypeFloat) => Type::Float32,
        Some(TypeKind::TypeDouble) => Type::Float64,
        Some(TypeKind::TypeNumeric) => Type::Numeric,
        Some(TypeKind::TypeString) => Type::String(None),
        Some(TypeKind::TypeBytes) => Type::Bytes(None),
        Some(TypeKind::TypeDate) => Type::Date,
        Some(TypeKind::TypeTimestamp) => Type::Timestamp,
        Some(TypeKind::TypeJson) => Type::Json,
        Some(TypeKind::TypeInterval) => Type::Interval,
        Some(TypeKind::TypeUuid) => Type::Uuid,
        Some(TypeKind::TypeTokenlist) => Type::TokenList,
        Some(TypeKind::TypeArray) => {
            let elem = t
                .array_type
                .as_ref()
                .and_then(|a| a.element_type.as_deref())
                .ok_or_else(|| Error::internal("array type without element"))?;
            Type::Array(Box::new(from_proto(elem)?))
        }
        Some(TypeKind::TypeStruct) => {
            let mut fields = Vec::new();
            for f in t.struct_type.iter().flat_map(|s| &s.field) {
                let ft = f
                    .field_type
                    .as_ref()
                    .ok_or_else(|| Error::internal("struct field without type"))?;
                let name = f.field_name.clone().filter(|n| !n.is_empty());
                fields.push((name, from_proto(ft)?));
            }
            Type::Struct(fields)
        }
        // Only scanning and returning PROTO_BUNDLE is supported (`column_from_proto`).
        Some(TypeKind::TypeProto) => return Err(unimplemented("Using a PROTO value")),
        // Graph elements are carried as structs (see `graph_exec`).
        Some(TypeKind::TypeGraphElement) => {
            Type::Struct(vec![(Some("$graph_element".into()), Type::Int64)])
        }
        other => {
            return Err(unimplemented(format!(
                "Type {}",
                other.map_or("UNKNOWN", |k| k.as_str_name())
            )));
        }
    })
}

/// Whether a value of type `t` holds graph elements or paths.
pub fn has_graph(t: &TypeProto) -> bool {
    match t.type_kind.and_then(|k| TypeKind::try_from(k).ok()) {
        Some(TypeKind::TypeGraphElement) | Some(TypeKind::TypeGraphPath) => true,
        Some(TypeKind::TypeArray) => t
            .array_type
            .as_ref()
            .and_then(|a| a.element_type.as_deref())
            .is_some_and(has_graph),
        Some(TypeKind::TypeStruct) => t
            .struct_type
            .iter()
            .flat_map(|s| &s.field)
            .any(|f| f.field_type.as_ref().is_some_and(has_graph)),
        _ => false,
    }
}

/// Spanner wire type for result metadata.
pub fn spanner_type(t: &Type) -> pb::Type {
    use pb::TypeCode as C;
    let code = |c: C| pb::Type {
        code: c as i32,
        ..Default::default()
    };
    match t {
        Type::Bool => code(C::Bool),
        Type::Int64 => code(C::Int64),
        Type::Float32 => code(C::Float32),
        Type::Float64 => code(C::Float64),
        Type::Numeric => code(C::Numeric),
        Type::String(_) => code(C::String),
        Type::Bytes(_) | Type::TokenList => code(C::Bytes),
        Type::Date => code(C::Date),
        Type::Timestamp => code(C::Timestamp),
        Type::Json => code(C::Json),
        Type::Interval => code(C::Interval),
        Type::Uuid => code(C::Uuid),
        Type::Proto(name) => pb::Type {
            code: C::Proto as i32,
            proto_type_fqn: name.clone(),
            ..Default::default()
        },
        Type::Array(e) => pb::Type {
            code: C::Array as i32,
            array_element_type: Some(Box::new(spanner_type(e))),
            ..Default::default()
        },
        Type::Struct(fields) => pb::Type {
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

/// Wire type of a query parameter.
pub fn from_spanner(t: &pb::Type) -> Result<Type> {
    use pb::TypeCode as C;
    let code = C::try_from(t.code).unwrap_or(C::Unspecified);
    Ok(match code {
        C::Bool => Type::Bool,
        C::Int64 => Type::Int64,
        C::Float32 => Type::Float32,
        C::Float64 => Type::Float64,
        C::Numeric => Type::Numeric,
        C::String => Type::String(None),
        C::Bytes => Type::Bytes(None),
        C::Date => Type::Date,
        C::Timestamp => Type::Timestamp,
        C::Json => Type::Json,
        C::Interval => Type::Interval,
        C::Uuid => Type::Uuid,
        C::Array => {
            let e = t
                .array_element_type
                .as_deref()
                .ok_or_else(|| Error::invalid_argument("Array type is missing an element type."))?;
            Type::Array(Box::new(from_spanner(e)?))
        }
        C::Struct => {
            let mut fields = Vec::new();
            for f in t.struct_type.iter().flat_map(|s| &s.fields) {
                let ft = f
                    .r#type
                    .as_ref()
                    .ok_or_else(|| Error::invalid_argument("Struct field is missing a type."))?;
                fields.push((
                    (!f.name.is_empty()).then(|| f.name.clone()),
                    from_spanner(ft)?,
                ));
            }
            Type::Struct(fields)
        }
        _ => {
            return Err(Error::invalid_argument(format!(
                "Unsupported query parameter type code {}.",
                t.code
            )));
        }
    })
}

/// GoogleSQL type name, as the analyser's type parser accepts it.
pub fn gsql_name(t: &Type) -> String {
    match t {
        Type::Bool => "BOOL".into(),
        Type::Int64 => "INT64".into(),
        Type::Float32 => "FLOAT32".into(),
        Type::Float64 => "FLOAT64".into(),
        Type::Numeric => "NUMERIC".into(),
        Type::String(_) => "STRING".into(),
        Type::Bytes(_) => "BYTES".into(),
        Type::Date => "DATE".into(),
        Type::Timestamp => "TIMESTAMP".into(),
        Type::Json => "JSON".into(),
        Type::Interval => "INTERVAL".into(),
        Type::Uuid => "UUID".into(),
        Type::TokenList => "TOKENLIST".into(),
        // The shim builds this one PROTO type by this name.
        Type::Proto(name) => format!("PROTO<{name}>"),
        Type::Array(e) => format!("ARRAY<{}>", gsql_name(e)),
        Type::Struct(fields) => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(n, t)| match n {
                    Some(n) => format!("`{}` {}", n.replace('`', "\\`"), gsql_name(t)),
                    None => gsql_name(t),
                })
                .collect();
            format!("STRUCT<{}>", parts.join(", "))
        }
    }
}

/// Spanner's spelling of a type in error messages and INFORMATION_SCHEMA.
pub fn spanner_name(t: &Type) -> String {
    match t {
        Type::Float32 => "FLOAT32".into(),
        Type::Float64 => "FLOAT64".into(),
        Type::Proto(name) => format!("PROTO<{}>", production_proto_name(name)),
        Type::Array(e) => format!("ARRAY<{}>", spanner_name(e)),
        Type::Struct(fields) => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(n, t)| match n {
                    Some(n) => format!("{n} {}", spanner_name(t)),
                    None => spanner_name(t),
                })
                .collect();
            format!("STRUCT<{}>", parts.join(", "))
        }
        other => gsql_name(other),
    }
}

/// Same type ignoring STRING/BYTES lengths.
pub fn same(a: &Type, b: &Type) -> bool {
    match (a, b) {
        (Type::String(_), Type::String(_)) | (Type::Bytes(_), Type::Bytes(_)) => true,
        (Type::Array(x), Type::Array(y)) => same(x, y),
        (Type::Struct(x), Type::Struct(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(&p.1, &q.1))
        }
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googlesql_sys::proto::ProtoTypeProto;

    fn proto_type(name: &str) -> TypeProto {
        TypeProto {
            type_kind: Some(TypeKind::TypeProto as i32),
            proto_type: Some(ProtoTypeProto {
                proto_name: Some(name.into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn proto_bundle_is_only_a_column_type() {
        let t = proto_type(FILE_DESCRIPTOR_SET);
        assert_eq!(
            column_from_proto(&t).ok(),
            Some(Type::Proto(FILE_DESCRIPTOR_SET.into()))
        );
        assert_eq!(
            from_proto(&t).err().map(|e| e.code),
            Some(Code::Unimplemented)
        );
        let other = proto_type("google.protobuf.FileDescriptorProto");
        assert_eq!(
            column_from_proto(&other).err().map(|e| e.code),
            Some(Code::Unimplemented)
        );
    }

    #[test]
    fn proto_names_per_mode() {
        let t = Type::Proto(FILE_DESCRIPTOR_SET.into());
        assert_eq!(gsql_name(&t), "PROTO<google.protobuf.FileDescriptorSet>");
        assert_eq!(spanner_name(&t), "PROTO<proto2.FileDescriptorSet>");
        assert_eq!(result_type(t.clone(), true), t);
        assert_eq!(
            spanner_type(&result_type(t, false)).proto_type_fqn,
            "proto2.FileDescriptorSet"
        );
    }
}
