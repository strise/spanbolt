//! GoogleSQL `ValueProto` literals to runtime values.

use std::sync::Arc;

use googlesql_sys::proto::ValueProto;
use googlesql_sys::proto::value_proto::Value as V;
use spanbolt_storage::{Date, Error, Numeric, Result, Timestamp, Value};

use crate::types::{Type, unimplemented};

/// GoogleSQL serialises NUMERIC as minimal little-endian two's complement of the scaled value.
fn numeric(bytes: &[u8]) -> Result<Numeric> {
    if bytes.len() > 16 {
        return Err(Error::internal("NUMERIC literal too wide"));
    }
    let mut buf = if bytes.last().is_some_and(|b| b & 0x80 != 0) {
        [0xFFu8; 16]
    } else {
        [0u8; 16]
    };
    buf[..bytes.len()].copy_from_slice(bytes);
    Ok(Numeric(i128::from_le_bytes(buf)))
}

/// GoogleSQL interval bytes: micros (i64 LE), days (i32 LE), then a u32 holding the month count
/// (sign in bit 31, magnitude from bit 13) and the sub-microsecond nanos (low bits).
fn interval(b: &[u8]) -> Result<spanbolt_storage::Interval> {
    let bad = || Error::internal("bad INTERVAL literal");
    let micros = i64::from_le_bytes(b.get(0..8).ok_or_else(bad)?.try_into().map_err(|_| bad())?);
    let days = i32::from_le_bytes(
        b.get(8..12)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?,
    );
    let mn = u32::from_le_bytes(
        b.get(12..16)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?,
    );
    let months = ((mn & 0x7FFF_FFFF) >> 13) as i32;
    let months = if mn & 0x8000_0000 != 0 {
        -months
    } else {
        months
    };
    let frac = (mn & 0x1FFF) as i128;
    Ok(spanbolt_storage::Interval {
        months,
        days,
        nanos: micros as i128 * 1000 + if micros < 0 { -frac } else { frac },
    })
}

pub fn value(v: Option<&ValueProto>, ty: &Type) -> Result<Value> {
    let Some(inner) = v.and_then(|v| v.value.as_ref()) else {
        return Ok(Value::Null);
    };
    Ok(match (inner, ty) {
        (V::BoolValue(b), _) => Value::Bool(*b),
        (V::Int64Value(i), _) => Value::Int64(*i),
        (V::Int32Value(i), _) => Value::Int64(*i as i64),
        // Enum arguments (date parts, etc.) are passed as their numbers.
        (V::EnumValue(i), _) => Value::Int64(*i as i64),
        (V::DoubleValue(f), _) => Value::Float64(*f),
        (V::FloatValue(f), _) => Value::Float32(*f),
        (V::StringValue(s), _) => Value::String(Arc::from(s.as_str())),
        (V::BytesValue(b), _) => Value::Bytes(Arc::from(b.as_slice())),
        (V::DateValue(d), _) => Value::Date(Date(*d)),
        (V::TimestampValue(t), _) => Value::Timestamp(Timestamp {
            secs: t.seconds,
            nanos: t.nanos.max(0) as u32,
        }),
        (V::TimestampPicosValue(t), _) => Value::Timestamp(Timestamp {
            secs: t.seconds.unwrap_or(0),
            nanos: (t.picos.unwrap_or(0) / 1000) as u32,
        }),
        (V::NumericValue(b), _) => Value::Numeric(numeric(b)?),
        (V::JsonValue(s), _) => Value::Json(Arc::from(
            spanbolt_storage::value::normalize_json(s).unwrap_or_else(|| s.clone()),
        )),
        (V::UuidValue(b), _) => {
            let arr: [u8; 16] = b
                .as_slice()
                .try_into()
                .map_err(|_| Error::internal("bad UUID literal"))?;
            Value::Uuid(u128::from_be_bytes(arr))
        }
        (V::IntervalValue(b), _) => Value::Interval(interval(b)?),
        (V::TokenlistValue(b), _) => Value::TokenList(Arc::from(b.as_slice())),
        (V::ArrayValue(a), Type::Array(et)) => {
            let mut out = Vec::with_capacity(a.element.len());
            for e in &a.element {
                out.push(value(Some(e), et)?);
            }
            Value::Array(Arc::from(out))
        }
        (V::StructValue(s), Type::Struct(fields)) => {
            let mut out = Vec::with_capacity(s.field.len());
            for (e, (_, t)) in s.field.iter().zip(fields) {
                out.push(value(Some(e), t)?);
            }
            Value::Struct(Arc::from(out))
        }
        _ => return Err(unimplemented("This literal type")),
    })
}
