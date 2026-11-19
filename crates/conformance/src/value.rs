//! Types and values: Spanner `Type` text form, JSON <-> `google.protobuf.Value` conversion.

use prost_types::value::Kind;
use prost_types::{ListValue, Value};
use serde_json::Value as Json;
use spanbolt_proto::spanner::{StructType, Type, TypeCode, struct_type};

/// `INT64`, `ARRAY<STRING>`, `STRUCT<a INT64, STRING>` (anonymous fields are type only).
pub fn type_text(t: &Type) -> String {
    let code = TypeCode::try_from(t.code).unwrap_or(TypeCode::Unspecified);
    match code {
        TypeCode::Array => match &t.array_element_type {
            Some(e) => format!("ARRAY<{}>", type_text(e)),
            None => "ARRAY<?>".into(),
        },
        TypeCode::Struct => {
            let fields = t.struct_type.as_ref().map(fields_text).unwrap_or_default();
            format!("STRUCT<{fields}>")
        }
        TypeCode::Proto | TypeCode::Enum => format!("{}<{}>", code.as_str_name(), t.proto_type_fqn),
        other => other.as_str_name().to_string(),
    }
}

/// Columns of a result: `K INT64, V STRING`.
pub fn fields_text(st: &StructType) -> String {
    st.fields
        .iter()
        .map(|f| {
            let t = f
                .r#type
                .as_ref()
                .map(type_text)
                .unwrap_or_else(|| "?".into());
            if f.name.is_empty() {
                t
            } else {
                format!("{} {t}", f.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn scalar(code: TypeCode) -> Type {
    Type {
        code: code as i32,
        ..Default::default()
    }
}

/// Parses the type syntax used in `params:` lines.
pub fn parse_type(s: &str) -> Result<Type, String> {
    let tokens = tokenize_type(s);
    let mut pos = 0;
    let t = parse_type_at(&tokens, &mut pos)?;
    if pos != tokens.len() {
        return Err(format!("trailing input in type `{s}`"));
    }
    Ok(t)
}

fn tokenize_type(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' || c == '.' {
            cur.push(c);
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            if !c.is_whitespace() {
                out.push(c.to_string());
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn parse_type_at(tokens: &[String], pos: &mut usize) -> Result<Type, String> {
    let word = tokens
        .get(*pos)
        .ok_or_else(|| "unexpected end of type".to_string())?
        .to_ascii_uppercase();
    *pos += 1;
    let expect = |pos: &mut usize, tok: &str| -> Result<(), String> {
        if tokens.get(*pos).map(String::as_str) == Some(tok) {
            *pos += 1;
            Ok(())
        } else {
            Err(format!("expected `{tok}` in type"))
        }
    };
    match word.as_str() {
        "ARRAY" => {
            expect(pos, "<")?;
            let elem = parse_type_at(tokens, pos)?;
            expect(pos, ">")?;
            Ok(Type {
                code: TypeCode::Array as i32,
                array_element_type: Some(Box::new(elem)),
                ..Default::default()
            })
        }
        "STRUCT" => {
            expect(pos, "<")?;
            let mut fields = Vec::new();
            while tokens.get(*pos).map(String::as_str) != Some(">") {
                // `name TYPE` or `TYPE`: a name is followed by another word.
                let named = matches!(tokens.get(*pos + 1).map(String::as_str), Some(t) if t != "<" && t != "," && t != ">");
                let name = if named {
                    *pos += 1;
                    tokens[*pos - 1].clone()
                } else {
                    String::new()
                };
                let t = parse_type_at(tokens, pos)?;
                fields.push(struct_type::Field {
                    name,
                    r#type: Some(t),
                });
                if tokens.get(*pos).map(String::as_str) == Some(",") {
                    *pos += 1;
                }
                if *pos >= tokens.len() {
                    return Err("unterminated STRUCT".into());
                }
            }
            *pos += 1;
            Ok(Type {
                code: TypeCode::Struct as i32,
                struct_type: Some(StructType { fields }),
                ..Default::default()
            })
        }
        other => {
            let code = TypeCode::from_str_name(other)
                .filter(|c| {
                    !matches!(
                        c,
                        TypeCode::Unspecified | TypeCode::Array | TypeCode::Struct
                    )
                })
                .ok_or_else(|| format!("unknown type `{other}`"))?;
            Ok(scalar(code))
        }
    }
}

pub fn null() -> Value {
    Value {
        kind: Some(Kind::NullValue(0)),
    }
}

pub fn string(s: impl Into<String>) -> Value {
    Value {
        kind: Some(Kind::StringValue(s.into())),
    }
}

fn list(values: Vec<Value>) -> Value {
    Value {
        kind: Some(Kind::ListValue(ListValue { values })),
    }
}

/// Untyped conversion for mutation values and keys: integers become INT64 strings, other numbers
/// FLOAT64 numbers, objects their JSON text.
pub fn json_to_proto(j: &Json) -> Value {
    match j {
        Json::Null => null(),
        Json::Bool(b) => Value {
            kind: Some(Kind::BoolValue(*b)),
        },
        Json::Number(n) if n.is_i64() || n.is_u64() => string(n.to_string()),
        Json::Number(n) => Value {
            kind: Some(Kind::NumberValue(n.as_f64().unwrap_or(f64::NAN))),
        },
        Json::String(s) => string(s.clone()),
        Json::Array(a) => list(a.iter().map(json_to_proto).collect()),
        Json::Object(_) => string(j.to_string()),
    }
}

pub fn json_row(j: &Json) -> Result<ListValue, String> {
    match j {
        Json::Array(a) => Ok(ListValue {
            values: a.iter().map(json_to_proto).collect(),
        }),
        other => Err(format!("expected a JSON array, got {other}")),
    }
}

/// Typed conversion for query parameters.
pub fn json_to_typed(j: &Json, t: &Type) -> Result<Value, String> {
    if j.is_null() {
        return Ok(null());
    }
    let code = TypeCode::try_from(t.code).unwrap_or(TypeCode::Unspecified);
    match code {
        TypeCode::Array => {
            let elem = t
                .array_element_type
                .as_deref()
                .ok_or("ARRAY without element type")?;
            let Json::Array(items) = j else {
                return Err(format!("expected an array for {}, got {j}", type_text(t)));
            };
            Ok(list(
                items
                    .iter()
                    .map(|i| json_to_typed(i, elem))
                    .collect::<Result<_, _>>()?,
            ))
        }
        TypeCode::Struct => {
            let fields = t
                .struct_type
                .as_ref()
                .map(|s| s.fields.as_slice())
                .unwrap_or_default();
            let Json::Array(items) = j else {
                return Err(format!("expected an array for {}, got {j}", type_text(t)));
            };
            if items.len() != fields.len() {
                return Err(format!(
                    "{} has {} fields, value has {}",
                    type_text(t),
                    fields.len(),
                    items.len()
                ));
            }
            let mut out = Vec::new();
            for (item, field) in items.iter().zip(fields) {
                let ft = field.r#type.as_ref().ok_or("struct field without type")?;
                out.push(json_to_typed(item, ft)?);
            }
            Ok(list(out))
        }
        TypeCode::Bool => match j {
            Json::Bool(b) => Ok(Value {
                kind: Some(Kind::BoolValue(*b)),
            }),
            other => Err(format!("expected a bool, got {other}")),
        },
        TypeCode::Float64 | TypeCode::Float32 => match j {
            Json::Number(n) => Ok(Value {
                kind: Some(Kind::NumberValue(n.as_f64().unwrap_or(f64::NAN))),
            }),
            Json::String(s) => Ok(string(s.clone())),
            other => Err(format!("expected a number, got {other}")),
        },
        TypeCode::Json => match j {
            Json::String(s) => Ok(string(s.clone())),
            other => Ok(string(other.to_string())),
        },
        _ => match j {
            Json::String(s) => Ok(string(s.clone())),
            Json::Number(n) => Ok(string(n.to_string())),
            other => Err(format!(
                "expected a string for {}, got {other}",
                type_text(t)
            )),
        },
    }
}

/// Spanner's JSON wire form of a value.
pub fn proto_to_json(v: &Value) -> Json {
    match &v.kind {
        None | Some(Kind::NullValue(_)) => Json::Null,
        Some(Kind::BoolValue(b)) => Json::Bool(*b),
        Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(*n)
            .map(Json::Number)
            .unwrap_or_else(|| Json::String(format!("<number {n}>"))),
        Some(Kind::StringValue(s)) => Json::String(s.clone()),
        Some(Kind::ListValue(l)) => Json::Array(l.values.iter().map(proto_to_json).collect()),
        Some(Kind::StructValue(s)) => Json::Object(
            s.fields
                .iter()
                .map(|(k, v)| (k.clone(), proto_to_json(v)))
                .collect(),
        ),
    }
}

pub fn row_json(row: &ListValue) -> Json {
    Json::Array(row.values.iter().map(proto_to_json).collect())
}

/// Seconds since the epoch of an RFC 3339 UTC timestamp (`2024-01-02T03:04:05.123Z`).
pub fn parse_timestamp_secs(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let days = parse_date_days(date)?;
    let mut parts = time.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let sec: i64 = parts.next()?.split('.').next()?.parse().ok()?;
    Some(days * 86_400 + h * 3600 + m * 60 + sec)
}

/// Days since the epoch of `YYYY-MM-DD`.
pub fn parse_date_days(s: &str) -> Option<i64> {
    let mut parts = s.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// RFC 3339 text of a protobuf timestamp.
pub fn timestamp_text(ts: &prost_types::Timestamp) -> String {
    let days = ts.seconds.div_euclid(86_400);
    let secs = ts.seconds.rem_euclid(86_400);
    // civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:09}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60,
        ts.nanos
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_round_trip() {
        for s in [
            "INT64",
            "ARRAY<STRING>",
            "STRUCT<a INT64, b ARRAY<STRUCT<x FLOAT64, BYTES>>>",
            "ARRAY<STRUCT<id INT64, label STRING>>",
        ] {
            let t = parse_type(s).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(type_text(&t), s);
        }
        assert!(parse_type("WAT").is_err());
    }

    #[test]
    fn typed_params() {
        let t = parse_type("STRUCT<a INT64, b FLOAT64, c JSON>").unwrap_or_else(|e| panic!("{e}"));
        let j: Json =
            serde_json::from_str(r#"[5, 1.5, {"k": 1}]"#).unwrap_or_else(|e| panic!("{e}"));
        let v = json_to_typed(&j, &t).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(proto_to_json(&v).to_string(), r#"["5",1.5,"{\"k\":1}"]"#);
    }

    #[test]
    fn untyped_values() {
        let j: Json = serde_json::from_str(r#"[1, 2.0, "x", null, [1, 2]]"#)
            .unwrap_or_else(|e| panic!("{e}"));
        let row = json_row(&j).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            row_json(&row).to_string(),
            r#"["1",2.0,"x",null,["1","2"]]"#
        );
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp_secs("1970-01-02T00:00:01Z"), Some(86_401));
        let ts = prost_types::Timestamp {
            seconds: 1_700_000_000,
            nanos: 5,
        };
        let text = timestamp_text(&ts);
        assert_eq!(text, "2023-11-14T22:13:20.000000005Z");
        assert_eq!(parse_timestamp_secs(&text), Some(1_700_000_000));
    }
}
