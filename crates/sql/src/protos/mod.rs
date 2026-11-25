//! PROTO and ENUM values of the database's proto bundle (`CREATE PROTO BUNDLE`). A PROTO value
//! is its serialized bytes (`Value::Bytes`), an ENUM value its number (`Value::Int64`); the
//! bundle's descriptors give them meaning: field access, `NEW`, `REPLACE_FIELDS`, casts.

mod text;
mod wire;

use std::borrow::Cow;
use std::sync::Arc;

use prost::Message as _;
use prost::bytes::Bytes;
use prost_reflect::{
    DescriptorPool, DynamicMessage, EnumDescriptor, FieldDescriptor, Kind, MessageDescriptor,
    Value as PValue,
};
use spanbolt_ddl::ProtoBundle;
use spanbolt_storage::{Code, Error, Result, Value};

use wire::{Corrupt, Fields, Wire};

/// Language features the analyser needs for SQL over a bundle's types.
pub const FEATURES: &[&str] = &["FEATURE_PROTO_BASE", "FEATURE_REPLACE_FIELDS"];

/// A bundle's types: the shim's, for analysis, and the descriptors, for evaluation.
pub struct Bundle {
    pub gsql: Arc<googlesql_sys::Protos>,
    pool: DescriptorPool,
    /// What it was built from: a schema version with the same bundle reuses it.
    pub descriptors: Arc<Vec<u8>>,
    pub types: Vec<String>,
}

impl Bundle {
    pub fn new(b: &ProtoBundle) -> Result<Bundle> {
        let types: Vec<String> = b.types.keys().cloned().collect();
        let names: Vec<&str> = types.iter().map(String::as_str).collect();
        let gsql = googlesql_sys::Protos::new(&b.descriptors, &names)
            .map_err(|e| Error::internal(format!("cannot build the proto bundle: {e}")))?;
        let set = prost_types::FileDescriptorSet::decode(b.descriptors.as_slice())
            .map_err(|e| Error::internal(format!("proto descriptors: {e}")))?;
        // The well-known types, for imports the descriptors leave out.
        let mut pool = DescriptorPool::global();
        pool.add_file_descriptor_set(set)
            .map_err(|e| Error::internal(format!("proto descriptors: {e}")))?;
        Ok(Bundle {
            gsql: Arc::new(gsql),
            pool,
            descriptors: b.descriptors.clone(),
            types,
        })
    }

    pub fn message(&self, name: &str) -> Result<MessageDescriptor> {
        self.pool
            .get_message_by_name(name)
            .ok_or_else(|| Error::internal(format!("proto {name} is not in the bundle")))
    }

    pub fn enumeration(&self, name: &str) -> Result<EnumDescriptor> {
        self.pool
            .get_enum_by_name(name)
            .ok_or_else(|| Error::internal(format!("enum {name} is not in the bundle")))
    }

    pub fn field(&self, message: &str, number: i32) -> Result<FieldDescriptor> {
        let m = self.message(message)?;
        u32::try_from(number)
            .ok()
            .and_then(|n| m.get_field(n))
            .ok_or_else(|| Error::internal(format!("proto {message} has no field {number}")))
    }
}

/// One wire value read as its field's type.
#[derive(Debug, Clone)]
pub(crate) enum Scalar<'a> {
    I64(i64),
    U64(u64),
    Bool(bool),
    F64(f64),
    F32(f32),
    Str(&'a [u8]),
    Bytes(&'a [u8]),
    Enum(i32),
    Msg(Cow<'a, [u8]>),
}

/// `w` as a value of `kind`; `None` when the wire type doesn't fit, which readers skip like an
/// unknown field.
pub(crate) fn scalar<'a>(kind: &Kind, w: Wire<'a>) -> Option<Scalar<'a>> {
    Some(match (kind, w) {
        (Kind::Double, Wire::Fixed64(v)) => Scalar::F64(f64::from_bits(v)),
        (Kind::Float, Wire::Fixed32(v)) => Scalar::F32(f32::from_bits(v)),
        (Kind::Int64, Wire::Varint(v)) => Scalar::I64(v as i64),
        (Kind::Uint64, Wire::Varint(v)) => Scalar::U64(v),
        (Kind::Int32, Wire::Varint(v)) => Scalar::I64(i64::from(v as i32)),
        (Kind::Fixed64, Wire::Fixed64(v)) => Scalar::U64(v),
        (Kind::Fixed32, Wire::Fixed32(v)) => Scalar::U64(u64::from(v)),
        (Kind::Bool, Wire::Varint(v)) => Scalar::Bool(v != 0),
        (Kind::String, Wire::Len(b)) => Scalar::Str(b),
        (Kind::Bytes, Wire::Len(b)) => Scalar::Bytes(b),
        (Kind::Message(_), Wire::Len(b) | Wire::Group(b)) => Scalar::Msg(Cow::Borrowed(b)),
        (Kind::Uint32, Wire::Varint(v)) => Scalar::U64(u64::from(v as u32)),
        (Kind::Enum(_), Wire::Varint(v)) => Scalar::Enum(v as i32),
        (Kind::Sfixed32, Wire::Fixed32(v)) => Scalar::I64(i64::from(v as i32)),
        (Kind::Sfixed64, Wire::Fixed64(v)) => Scalar::I64(v as i64),
        (Kind::Sint32, Wire::Varint(v)) => Scalar::I64(i64::from(wire::zigzag32(v as u32))),
        (Kind::Sint64, Wire::Varint(v)) => Scalar::I64(wire::zigzag64(v)),
        _ => return None,
    })
}

fn to_value(s: Scalar) -> Value {
    match s {
        Scalar::I64(v) => Value::Int64(v),
        Scalar::U64(v) => Value::Int64(v as i64),
        Scalar::Bool(v) => Value::Bool(v),
        Scalar::F64(v) => Value::Float64(v),
        Scalar::F32(v) => Value::Float32(v),
        Scalar::Str(b) => Value::String(Arc::from(String::from_utf8_lossy(b).as_ref())),
        Scalar::Bytes(b) => Value::Bytes(Arc::from(b)),
        Scalar::Enum(n) => Value::Int64(i64::from(n)),
        Scalar::Msg(b) => Value::Bytes(Arc::from(b.as_ref())),
    }
}

/// Errors of evaluating PROTO and ENUM expressions, per mode: the emulator's GoogleSQL errors,
/// or production's.
#[derive(Clone, Copy)]
pub struct Errors {
    pub emulator_compat: bool,
}

const PROD_INVALID: &str = "Request contains an invalid argument.";

impl Errors {
    fn corrupt(self, field: &FieldDescriptor) -> Error {
        if self.emulator_compat {
            Error::new(
                Code::OutOfRange,
                format!(
                    "Corrupted protocol buffer: Failed to read value for field {}",
                    field.full_name()
                ),
            )
        } else {
            Error::new(Code::Internal, "Internal error encountered.")
        }
    }

    fn eval(self, message: String) -> Error {
        if self.emulator_compat {
            Error::new(Code::OutOfRange, message)
        } else {
            Error::new(Code::InvalidArgument, PROD_INVALID)
        }
    }

    fn parse(self, e: text::ParseError) -> Error {
        let code = if self.emulator_compat {
            Code::OutOfRange
        } else {
            Code::InvalidArgument
        };
        Error::new(code, format!("Error parsing proto: {e}"))
    }
}

/// `proto.field` and `proto.has_field`.
pub struct FieldGetter {
    field: FieldDescriptor,
    has_bit: bool,
    default: Value,
    errors: Errors,
}

impl FieldGetter {
    pub fn new(field: FieldDescriptor, has_bit: bool, default: Value, errors: Errors) -> Self {
        FieldGetter {
            field,
            has_bit,
            default,
            errors,
        }
    }

    pub fn get(&self, v: &Value) -> Result<Value> {
        let bytes = match v {
            Value::Bytes(b) => b,
            _ => return Ok(Value::Null),
        };
        let number = self.field.number();
        let mut found = Vec::new();
        for f in Fields::new(bytes) {
            match f {
                Ok((n, w)) if n == number => found.push(w),
                Ok(_) => {}
                // The emulator's reader takes a tag it can't parse as the end of the message.
                Err(Corrupt::Tag) if self.errors.emulator_compat => break,
                Err(_) => return Err(self.errors.corrupt(&self.field)),
            }
        }
        let values = text::values(&self.field, &found);
        if self.has_bit {
            return Ok(Value::Bool(!values.is_empty()));
        }
        if self.field.is_list() {
            return Ok(Value::Array(values.into_iter().map(to_value).collect()));
        }
        Ok(values
            .into_iter()
            .last()
            .map_or_else(|| self.default.clone(), to_value))
    }
}

/// A SQL value as a value of field `fd`; `None` for NULL.
fn field_value(fd: &FieldDescriptor, kind: &Kind, v: &Value) -> Result<Option<PValue>> {
    Ok(Some(match (kind, v) {
        (_, Value::Null) => return Ok(None),
        (Kind::Double, Value::Float64(f)) => PValue::F64(*f),
        (Kind::Float, Value::Float32(f)) => PValue::F32(*f),
        (Kind::Float, Value::Float64(f)) => PValue::F32(*f as f32),
        (Kind::Int64 | Kind::Sint64 | Kind::Sfixed64, Value::Int64(i)) => PValue::I64(*i),
        (Kind::Int32 | Kind::Sint32 | Kind::Sfixed32, Value::Int64(i)) => PValue::I32(*i as i32),
        (Kind::Uint32 | Kind::Fixed32, Value::Int64(i)) => PValue::U32(*i as u32),
        (Kind::Uint64 | Kind::Fixed64, Value::Int64(i)) => PValue::U64(*i as u64),
        (Kind::Bool, Value::Bool(b)) => PValue::Bool(*b),
        (Kind::String, Value::String(s)) => PValue::String(s.to_string()),
        (Kind::Bytes, Value::Bytes(b)) => PValue::Bytes(Bytes::copy_from_slice(b)),
        (Kind::Enum(_), Value::Int64(n)) => PValue::EnumNumber(*n as i32),
        (Kind::Message(m), Value::Bytes(b)) => PValue::Message(
            DynamicMessage::decode(m.clone(), b.as_ref())
                .map_err(|_| Error::new(Code::OutOfRange, "Invalid protocol buffer"))?,
        ),
        (_, Value::Array(items)) if fd.is_list() => {
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                match field_value(fd, kind, item)? {
                    Some(x) => out.push(x),
                    None => {
                        return Err(Error::new(
                            Code::OutOfRange,
                            format!(
                                "Cannot encode a null value in repeated protocol message field {}",
                                fd.full_name()
                            ),
                        ));
                    }
                }
            }
            PValue::List(out)
        }
        (_, other) => {
            return Err(Error::internal(format!(
                "cannot write {other:?} to proto field {}",
                fd.full_name()
            )));
        }
    }))
}

/// `NEW m(v AS f, ...)`.
pub fn make(m: &MessageDescriptor, fields: &[(FieldDescriptor, Value)]) -> Result<Value> {
    let mut msg = DynamicMessage::new(m.clone());
    for (fd, v) in fields {
        if let Some(x) = field_value(fd, &fd.kind(), v)? {
            msg.set_field(fd, x);
        }
    }
    Ok(Value::Bytes(Arc::from(msg.encode_to_vec())))
}

/// `REPLACE_FIELDS(p, v AS path, ...)`: each path is a chain of fields from `m`.
pub fn replace(
    m: &MessageDescriptor,
    proto: &Value,
    items: &[(Vec<FieldDescriptor>, Value)],
) -> Result<Value> {
    let Value::Bytes(b) = proto else {
        return Ok(Value::Null);
    };
    let mut msg = DynamicMessage::decode(m.clone(), b.as_ref())
        .map_err(|_| Error::new(Code::OutOfRange, "Invalid protocol buffer"))?;
    for (path, v) in items {
        let Some((last, parents)) = path.split_last() else {
            continue;
        };
        let mut target = &mut msg;
        for p in parents {
            if !target.has_field(p) {
                return Err(Error::new(
                    Code::OutOfRange,
                    format!(
                        "REPLACE_FIELDS() cannot be used to modify a field of an unset proto: {}",
                        p.full_name()
                    ),
                ));
            }
            target = match target.get_field_mut(p) {
                PValue::Message(inner) => inner,
                _ => return Err(Error::internal("REPLACE_FIELDS path is not a message")),
            };
        }
        match field_value(last, &last.kind(), v)? {
            Some(x) => target.set_field(last, x),
            None => target.clear_field(last),
        }
    }
    Ok(Value::Bytes(Arc::from(msg.encode_to_vec())))
}

/// `CAST(proto AS STRING)`.
pub fn to_text(m: &MessageDescriptor, v: &Value, errors: Errors) -> Result<Value> {
    match v {
        Value::Bytes(b) => text::print(m, b)
            .map(|s| Value::String(Arc::from(s)))
            .ok_or_else(|| errors.eval("Invalid protocol buffer".into())),
        _ => Ok(Value::Null),
    }
}

/// `CAST(string AS proto)`.
pub fn from_text(m: &MessageDescriptor, v: &Value, errors: Errors) -> Result<Value> {
    match v {
        Value::String(s) => {
            let msg = text::parse(m, s).map_err(|e| errors.parse(e))?;
            Ok(Value::Bytes(Arc::from(msg.encode_to_vec())))
        }
        _ => Ok(Value::Null),
    }
}

/// `CAST(enum AS STRING)`.
pub fn enum_name(e: &EnumDescriptor, v: &Value, errors: Errors) -> Result<Value> {
    match v {
        Value::Int64(n) => i32::try_from(*n)
            .ok()
            .and_then(|n| e.get_value(n))
            .map(|v| Value::string(v.name()))
            .ok_or_else(|| {
                errors.eval(format!(
                    "Out of range cast of integer {n} to enum type {}",
                    e.full_name()
                ))
            }),
        _ => Ok(Value::Null),
    }
}

/// `CAST(string AS enum)`.
pub fn enum_from_name(e: &EnumDescriptor, v: &Value, errors: Errors) -> Result<Value> {
    match v {
        Value::String(s) => match e.get_value_by_name(s) {
            Some(v) => Ok(Value::Int64(i64::from(v.number()))),
            None => {
                let mut msg = format!(
                    "Out of range cast of string '{s}' to enum type {}",
                    e.full_name()
                );
                if let Some(best) = suggest(s, e.values().map(|v| v.name().to_string())) {
                    msg.push_str(&format!("; Did you mean '{best}'?"));
                }
                Err(errors.eval(msg))
            }
        },
        _ => Ok(Value::Null),
    }
}

/// `CAST(int AS enum)`.
pub fn enum_from_number(e: &EnumDescriptor, v: &Value, errors: Errors) -> Result<Value> {
    match v {
        Value::Int64(n) => i32::try_from(*n)
            .ok()
            .filter(|n| e.get_value(*n).is_some())
            .map(|n| Value::Int64(i64::from(n)))
            .ok_or_else(|| {
                errors.eval(format!(
                    "Out of range cast of integer {n} to enum type {}",
                    e.full_name()
                ))
            }),
        _ => Ok(Value::Null),
    }
}

/// GoogleSQL's `SuggestEnumValue`: the closest name within about 20% edit distance, ignoring
/// case; ties go to the first in order.
fn suggest(typed: &str, names: impl Iterator<Item = String>) -> Option<String> {
    const MIN_LENGTH: usize = 3;
    let len = typed.chars().count();
    if len < MIN_LENGTH {
        return None;
    }
    let threshold = if len < 5 { len / 2 } else { len / 5 + 2 };
    let typed: Vec<char> = typed.to_lowercase().chars().collect();
    let mut best: Option<(usize, String)> = None;
    for name in names {
        let lower: Vec<char> = name.to_lowercase().chars().collect();
        let d = levenshtein(&typed, &lower);
        if d > threshold {
            continue;
        }
        let better = match &best {
            None => true,
            Some((bd, bn)) => d < *bd || (d == *bd && name.to_lowercase() < bn.to_lowercase()),
        };
        if better {
            best = Some((d, name));
        }
    }
    best.map(|(_, n)| n)
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// `TO_JSON(proto)` as Cloud Spanner returns it: the proto3 JSON mapping.
pub fn to_json(m: &MessageDescriptor, v: &Value) -> Result<Option<serde_json::Value>> {
    let Value::Bytes(b) = v else {
        return Ok(None);
    };
    let msg = DynamicMessage::decode(m.clone(), b.as_ref())
        .map_err(|_| Error::new(Code::Internal, "Internal error encountered."))?;
    serde_json::to_value(&msg)
        .map(Some)
        .map_err(|e| Error::internal(format!("proto to JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_like_googlesql() {
        let names = || {
            ["POP", "JAZZ", "FOLK", "ROCK"]
                .iter()
                .map(|s| s.to_string())
        };
        assert_eq!(suggest("NOPE", names()).as_deref(), Some("POP"));
        assert_eq!(suggest("jaz", names()).as_deref(), Some("JAZZ"));
        assert_eq!(suggest("QWERTY", names()), None);
        assert_eq!(suggest("PO", names()), None);
    }
}
