//! Decoding of Spanner wire values (`google.protobuf.Value`) with the official emulator's errors.

use std::fmt::Write as _;
use std::sync::Arc;

use base64::Engine as _;
use prost_types::value::Kind;
use spanbolt_ddl::ColumnType;

use crate::error::{Error, Result};
use crate::value::{BASE64, Date, Interval, Numeric, Timestamp, Value, parse_uuid, type_name};

/// Decodes the wire encoding of a value of type `ty`. Parse failures are FAILED_PRECONDITION,
/// except out-of-range dates (INVALID_ARGUMENT), as in the emulator.
pub fn decode(v: &prost_types::Value, ty: &ColumnType) -> Result<Value> {
    let k = &v.kind;
    if matches!(k, Some(Kind::NullValue(_))) {
        return Ok(Value::Null);
    }
    let mismatch = || {
        Error::failed_precondition(format!(
            "Could not parse {} as {}",
            short_debug(v),
            zeta_type(ty)
        ))
    };
    let s = match k {
        Some(Kind::StringValue(s)) => Some(s.as_str()),
        _ => None,
    };
    let need_str = || s.ok_or_else(mismatch);
    Ok(match ty {
        ColumnType::Bool => match k {
            Some(Kind::BoolValue(b)) => Value::Bool(*b),
            _ => return Err(mismatch()),
        },
        ColumnType::Int64 | ColumnType::Enum(_) => {
            if matches!(ty, ColumnType::Enum(_)) && s.is_none() {
                return Err(mismatch());
            }
            let s = need_str()?;
            let parse_err =
                || Error::failed_precondition(format!("Could not parse {s} as an integer"));
            // An ENUM's numbers are int32.
            Value::Int64(match ty {
                ColumnType::Enum(_) => s.parse::<i32>().map_err(|_| parse_err())?.into(),
                _ => s.parse().map_err(|_| parse_err())?,
            })
        }
        ColumnType::Float64 => Value::Float64(float(k, ty)?.ok_or_else(mismatch)?),
        ColumnType::Float32 => {
            let f = float(k, ty)?.ok_or_else(mismatch)?;
            if f.is_finite() && f.abs() > f32::MAX as f64 {
                return Err(mismatch());
            }
            Value::Float32(f as f32)
        }
        ColumnType::Numeric => {
            let s = need_str()?;
            Value::Numeric(Numeric::parse_exact(s).ok_or_else(|| {
                Error::failed_precondition(format!(
                    "Could not parse {s} as a NUMERIC. The NUMERIC type supports 38 digits of precision and 9 digits of scale. See https://cloud.google.com/spanner/docs/data-types for more details"
                ))
            })?)
        }
        ColumnType::String(_) => Value::String(Arc::from(need_str()?)),
        // A PROTO value is its message's serialized bytes.
        ColumnType::Bytes(_) | ColumnType::TokenList | ColumnType::Proto(_) => {
            let s = need_str()?;
            let b: Arc<[u8]> = Arc::from(BASE64.decode(s).map_err(|_| {
                Error::failed_precondition(format!(
                    "Could not parse {s} as BYTES. Byte strings must be base64-encoded. For more details, see https://cloud.google.com/spanner/docs/reference/rpc/google.spanner.v1#typecode"
                ))
            })?);
            if *ty == ColumnType::TokenList {
                Value::TokenList(b)
            } else {
                Value::Bytes(b)
            }
        }
        ColumnType::Date => Value::Date(date(need_str()?)?),
        ColumnType::Timestamp => Value::Timestamp(timestamp(need_str()?)?),
        ColumnType::Json => {
            let s = need_str()?;
            Value::Json(Arc::from(normalize_json_first_key(s).ok_or_else(|| {
                Error::failed_precondition(format!(
                    "Could not parse {s} as a JSON. See https://cloud.google.com/spanner/docs/data-types for more details"
                ))
            })?))
        }
        ColumnType::Interval => {
            let s = need_str()?;
            Value::Interval(Interval::parse(s).ok_or_else(|| generic(s, ty))?)
        }
        ColumnType::Uuid => {
            let s = need_str()?;
            Value::Uuid(parse_uuid(s).ok_or_else(|| generic(s, ty))?)
        }
        ColumnType::Array(elem) => match k {
            Some(Kind::ListValue(l)) => {
                let mut out = Vec::with_capacity(l.values.len());
                for (i, e) in l.values.iter().enumerate() {
                    out.push(decode(e, elem).map_err(|mut err| {
                        let _ = write!(
                            err.message,
                            "; When parsing array element #{i}: {{ {} }} in {}",
                            short_debug(e),
                            short_debug(v)
                        );
                        err
                    })?);
                }
                Value::Array(Arc::from(out))
            }
            _ => return Err(mismatch()),
        },
        ColumnType::Struct(fields) => match k {
            Some(Kind::ListValue(l)) if l.values.len() == fields.len() => {
                let mut out = Vec::with_capacity(fields.len());
                for (e, (_, t)) in l.values.iter().zip(fields) {
                    out.push(decode(e, t)?);
                }
                Value::Struct(Arc::from(out))
            }
            _ => return Err(mismatch()),
        },
    })
}

fn generic(s: &str, ty: &ColumnType) -> Error {
    Error::failed_precondition(format!(
        "Could not parse {s} as a {}. See https://cloud.google.com/spanner/docs/data-types for more details",
        type_name(ty)
    ))
}

/// `Ok(None)` = wrong wire kind.
fn float(k: &Option<Kind>, ty: &ColumnType) -> Result<Option<f64>> {
    Ok(match k {
        Some(Kind::NumberValue(n)) => Some(*n),
        Some(Kind::StringValue(s)) => Some(match s.as_str() {
            "NaN" => f64::NAN,
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            _ => {
                let t = type_name(ty);
                return Err(Error::failed_precondition(format!(
                    "Could not parse {s} as a {t}. Only the following string values are supported for {t}: 'Infinity', 'NaN', and '-Infinity'. See https://cloud.google.com/spanner/docs/data-types#floating-point-type for more details"
                )));
            }
        }),
        _ => None,
    })
}

const DATE_HELP: &str = "Dates must be in the format YYYY-[M]M-[D]D and the year must be in the range [1, 9999]. For more details on the DATE type encoding, see https://cloud.google.com/spanner/docs/data-types#date-type";

fn date(s: &str) -> Result<Date> {
    if let Some(d) = Date::parse(s) {
        return Ok(d);
    }
    // A well-formed date whose year is outside [1, 9999].
    let parts: Vec<&str> = s.splitn(3, '-').collect();
    let digits = |p: &str, lo: usize, hi: usize| {
        (lo..=hi).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit())
    };
    if parts.len() == 3
        && digits(parts[0], 4, 18)
        && digits(parts[1], 1, 2)
        && digits(parts[2], 1, 2)
        && parts[0].parse::<u64>().is_ok_and(|y| y == 0 || y > 9999)
    {
        return Err(Error::invalid_argument(format!(
            "Invalid date value: {s}. {DATE_HELP}"
        )));
    }
    Err(Error::failed_precondition(format!(
        "Could not parse {s} as a DATE. {DATE_HELP}"
    )))
}

fn timestamp(s: &str) -> Result<Timestamp> {
    if !s.ends_with('Z') {
        return Err(Error::failed_precondition(format!(
            "Could not parse {s} as a TIMESTAMP. The timestamp value must end with an uppercase literal 'Z' to specify Zulu time (UTC-0). For details on the TIMESTAMP type encoding, see https://cloud.google.com/spanner/docs/data-types#timestamp-type"
        )));
    }
    // Sub-nanosecond digits are truncated.
    let parsed = match s.find('.') {
        Some(dot) if s.len() - dot - 2 > 9 => {
            let frac = &s[dot + 1..s.len() - 1];
            if frac.bytes().all(|b| b.is_ascii_digit()) {
                Timestamp::parse(&format!("{}{}Z", &s[..dot + 1], &frac[..9]))
            } else {
                None
            }
        }
        _ => Timestamp::parse(s),
    };
    parsed.ok_or_else(|| {
        Error::failed_precondition(format!(
            "Could not parse {s} as a TIMESTAMP: Failed to parse input. For details on the TIMESTAMP type encoding, see https://cloud.google.com/spanner/docs/data-types#timestamp_type"
        ))
    })
}

/// GoogleSQL `Type::DebugString` names, as the emulator prints them in type-mismatch errors.
fn zeta_type(ty: &ColumnType) -> String {
    match ty {
        ColumnType::Float32 => "FLOAT".into(),
        ColumnType::Float64 => "DOUBLE".into(),
        ColumnType::Array(e) => format!("ARRAY<{}>", zeta_type(e)),
        ColumnType::Enum(name) => format!("ENUM<{name}>"),
        other => type_name(other),
    }
}

/// Protobuf `ShortDebugString` of a `google.protobuf.Value`.
pub fn short_debug(v: &prost_types::Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v);
    out
}

/// Protobuf `ShortDebugString` of a `google.protobuf.ListValue`.
/// A wire list as compact JSON, as production Spanner echoes a key: `["1","a",null,true]`.
pub fn list_json(l: &prost_types::ListValue) -> String {
    fn json(v: &prost_types::Value) -> serde_json::Value {
        match &v.kind {
            None | Some(Kind::NullValue(_)) => serde_json::Value::Null,
            Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
            Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(*n)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
            Some(Kind::ListValue(l)) => {
                serde_json::Value::Array(l.values.iter().map(json).collect())
            }
            Some(Kind::StructValue(st)) => serde_json::Value::Object(
                st.fields
                    .iter()
                    .map(|(k, v)| (k.clone(), json(v)))
                    .collect(),
            ),
        }
    }
    serde_json::Value::Array(l.values.iter().map(json).collect()).to_string()
}

pub fn list_short_debug(l: &prost_types::ListValue) -> String {
    let mut out = String::new();
    write_list_body(&mut out, l);
    out
}

fn write_list_body(out: &mut String, l: &prost_types::ListValue) {
    for (i, e) in l.values.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str("values { ");
        write_value(out, e);
        out.push_str(" }");
    }
}

fn write_value(out: &mut String, v: &prost_types::Value) {
    match &v.kind {
        None => {}
        Some(Kind::NullValue(_)) => out.push_str("null_value: NULL_VALUE"),
        Some(Kind::NumberValue(n)) => {
            out.push_str("number_value: ");
            out.push_str(&simple_dtoa(*n));
        }
        Some(Kind::StringValue(s)) => {
            out.push_str("string_value: \"");
            c_escape(out, s);
            out.push('"');
        }
        Some(Kind::BoolValue(b)) => {
            let _ = write!(out, "bool_value: {b}");
        }
        Some(Kind::ListValue(l)) => {
            out.push_str("list_value { ");
            write_list_body(out, l);
            if !l.values.is_empty() {
                out.push(' ');
            }
            out.push('}');
        }
        Some(Kind::StructValue(st)) => {
            out.push_str("struct_value { ");
            for (i, (key, val)) in st.fields.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str("fields { key: \"");
                c_escape(out, key);
                out.push_str("\" value { ");
                write_value(out, val);
                out.push_str(" } }");
            }
            if !st.fields.is_empty() {
                out.push(' ');
            }
            out.push('}');
        }
    }
}

/// absl's `CEscape` of raw bytes: the usual escapes, printable ASCII as is, every other byte as
/// three octal digits.
pub fn c_escape_bytes(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for &c in b {
        match c {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b'"' => out.push_str("\\\""),
            b'\'' => out.push_str("\\'"),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(c as char),
            c => {
                let _ = write!(out, "\\{c:03o}");
            }
        }
    }
    out
}

pub(crate) fn c_escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\{:03o}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// Protobuf's `SimpleDtoa`: `%.15g`, or `%.17g` when that does not round-trip.
pub fn simple_dtoa(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf" } else { "-inf" }.into();
    }
    let s = format_g(v, 15);
    if s.parse::<f64>().ok() == Some(v) {
        s
    } else {
        format_g(v, 17)
    }
}

/// C `printf("%.{p}g")`.
fn format_g(v: f64, p: usize) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let e_form = format!("{:.*e}", p - 1, v);
    let (mantissa, exp) = e_form.split_once('e').unwrap_or((&e_form, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if exp < -4 || exp >= p as i32 {
        let m = strip_zeros(mantissa);
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{m}e{sign}{:02}", exp.abs())
    } else {
        let decimals = (p as i32 - 1 - exp).max(0) as usize;
        strip_zeros(&format!("{v:.decimals$}")).to_string()
    }
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// Normalizes JSON text (keys sorted, no insignificant whitespace). Duplicate keys keep the
/// first value, as Spanner does.
pub fn normalize_json_first_key(s: &str) -> Option<String> {
    use serde::de::DeserializeSeed;
    let mut de = serde_json::Deserializer::from_str(s);
    let v = FirstKey.deserialize(&mut de).ok()?;
    de.end().ok()?;
    serde_json::to_string(&v).ok()
}

#[derive(Clone, Copy)]
struct FirstKey;

impl<'de> serde::de::DeserializeSeed<'de> for FirstKey {
    type Value = serde_json::Value;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        d: D,
    ) -> std::result::Result<serde_json::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for FirstKey {
    type Value = serde_json::Value;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> std::result::Result<Self::Value, E> {
        Ok(v.into())
    }

    fn visit_i64<E>(self, v: i64) -> std::result::Result<Self::Value, E> {
        Ok(v.into())
    }

    fn visit_u64<E>(self, v: u64) -> std::result::Result<Self::Value, E> {
        Ok(v.into())
    }

    fn visit_f64<E>(self, v: f64) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Number::from_f64(v).map_or(serde_json::Value::Null, Into::into))
    }

    fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E> {
        Ok(v.into())
    }

    fn visit_string<E>(self, v: String) -> std::result::Result<Self::Value, E> {
        Ok(v.into())
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(self)? {
            out.push(v);
        }
        Ok(serde_json::Value::Array(out))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(k) = map.next_key::<String>()? {
            let v = map.next_value_seed(self)?;
            out.entry(k).or_insert(v);
        }
        Ok(serde_json::Value::Object(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;

    fn sv(s: &str) -> prost_types::Value {
        prost_types::Value {
            kind: Some(Kind::StringValue(s.into())),
        }
    }

    fn nv(n: f64) -> prost_types::Value {
        prost_types::Value {
            kind: Some(Kind::NumberValue(n)),
        }
    }

    #[test]
    fn dtoa_matches_protobuf() {
        assert_eq!(simple_dtoa(1e39), "1e+39");
        assert_eq!(simple_dtoa(1.5), "1.5");
        assert_eq!(simple_dtoa(100000.0), "100000");
        assert_eq!(simple_dtoa(0.1), "0.1");
        assert_eq!(simple_dtoa(1e-5), "1e-05");
        assert_eq!(simple_dtoa(123456789012345680.0), "1.2345678901234568e+17");
    }

    #[test]
    fn decode_errors() {
        let e = decode(&nv(1e39), &ColumnType::Float32).err();
        assert_eq!(
            e.map(|e| e.message),
            Some("Could not parse number_value: 1e+39 as FLOAT".into())
        );
        let e = decode(&sv("10000-01-01"), &ColumnType::Date).err();
        assert_eq!(e.map(|e| e.code), Some(Code::InvalidArgument));
        assert!(decode(&sv("1.0000000001"), &ColumnType::Numeric).is_err());
        assert!(decode(&sv("1.0000000000"), &ColumnType::Numeric).is_ok());
    }

    #[test]
    fn timestamp_truncates_and_json_keeps_first_key() {
        let v = decode(
            &sv("2024-01-02T03:04:05.1234567891Z"),
            &ColumnType::Timestamp,
        );
        assert_eq!(
            v.map(|v| v.to_proto()),
            Ok(sv("2024-01-02T03:04:05.123456789Z"))
        );
        assert_eq!(
            normalize_json_first_key(r#"{"a":1,"a":2}"#).as_deref(),
            Some(r#"{"a":1}"#)
        );
        assert_eq!(normalize_json_first_key("{} x"), None);
    }
}
