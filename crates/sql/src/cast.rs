//! CAST semantics and the text forms of values.

use std::sync::Arc;

use spanbolt_storage::{Date, Error, Numeric, Result, Value};

use crate::time::{self, DEFAULT_TZ};
use crate::types::{self, Type};
use crate::value_ops::out_of_range;

const SCALE: i128 = 1_000_000_000;

/// `%.{prec}g` as C prints it.
fn format_g(f: f64, prec: usize) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    let e = format!("{:.*e}", prec - 1, f);
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if exp < -4 || exp >= prec as i32 {
        let mut m = mant.to_string();
        if m.contains('.') {
            while m.ends_with('0') {
                m.pop();
            }
            if m.ends_with('.') {
                m.pop();
            }
        }
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{m}e{sign}{:02}", exp.abs())
    } else {
        let decimals = (prec as i32 - 1 - exp).max(0) as usize;
        let mut s = format!("{:.*}", decimals, f);
        if s.contains('.') {
            while s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
        }
        s
    }
}

/// GoogleSQL's round-trip double formatting (`%.15g`, else `%.17g`).
pub fn format_f64(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let s = format_g(f, 15);
    if s.parse::<f64>().ok() == Some(f) {
        s
    } else {
        format_g(f, 17)
    }
}

pub fn format_f32(f: f32) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let s = format_g(f as f64, 6);
    if s.parse::<f32>().ok() == Some(f) {
        s
    } else {
        format_g(f as f64, 9)
    }
}

pub fn numeric_string(n: Numeric) -> String {
    let mut s = String::new();
    n.format(&mut s);
    s
}

/// Text form used by CAST(... AS STRING), CONCAT on casts, FORMAT('%t').
pub fn to_string(v: &Value) -> Result<Option<String>> {
    Ok(Some(match v {
        Value::Null => return Ok(None),
        Value::Bool(b) => b.to_string(),
        Value::Int64(i) => i.to_string(),
        Value::Float64(f) => format_f64(*f),
        Value::Float32(f) => format_f32(*f),
        Value::Numeric(n) => numeric_string(*n),
        Value::String(s) | Value::Json(s) => s.to_string(),
        Value::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => return Err(out_of_range("Invalid UTF-8 in BYTES to STRING cast")),
        },
        Value::Date(d) => time::format_date(*d),
        Value::Timestamp(t) => time::format_ts(*t, DEFAULT_TZ),
        Value::Interval(i) => crate::func_string::interval_text(i),
        Value::Uuid(u) => {
            let mut s = String::new();
            spanbolt_storage::value::format_uuid(*u, &mut s);
            s
        }
        Value::TokenList(_) | Value::Array(_) | Value::Struct(_) => {
            return Err(Error::invalid_argument("Cannot cast this type to STRING"));
        }
    }))
}

pub fn f64_to_i64(f: f64) -> Result<i64> {
    let r = f.round();
    if !r.is_finite() || !(-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&r) {
        return Err(out_of_range(format!(
            "int64 out of range: {}",
            format_f64(f)
        )));
    }
    Ok(r as i64)
}

pub fn numeric_to_i64(n: Numeric) -> Result<i64> {
    let q = n.0 / SCALE;
    let r = (n.0 % SCALE).abs();
    let q = if r * 2 >= SCALE {
        if n.0 < 0 { q - 1 } else { q + 1 }
    } else {
        q
    };
    i64::try_from(q).map_err(|_| out_of_range(format!("int64 out of range: {}", numeric_string(n))))
}

pub fn numeric_to_f64(n: Numeric) -> f64 {
    numeric_string(n).parse().unwrap_or(0.0)
}

pub fn f64_to_numeric(f: f64) -> Result<Numeric> {
    if !f.is_finite() {
        return Err(out_of_range(format!(
            "Illegal conversion of non-finite floating point number to numeric: {}",
            format_f64(f)
        )));
    }
    Numeric::parse(&format!("{f:.9}"))
        .ok_or_else(|| out_of_range(format!("numeric out of range: {}", format_f64(f))))
}

pub fn i64_to_numeric(i: i64) -> Numeric {
    Numeric(i as i128 * SCALE)
}

fn parse_i64(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let v = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i128::from_str_radix(h, 16).ok()?
    } else {
        if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        body.parse::<i128>().ok()?
    };
    i64::try_from(if neg { -v } else { v }).ok()
}

fn parse_f64(s: &str) -> Option<f64> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => return Some(f64::INFINITY),
        "-inf" | "-infinity" => return Some(f64::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => return Some(f64::NAN),
        _ => {}
    }
    if t.is_empty() || t.chars().any(|c| c.is_alphabetic() && c != 'e' && c != 'E') {
        return None;
    }
    t.parse().ok()
}

fn bad(kind: &str, s: &str) -> Error {
    out_of_range(format!("Bad {kind} value: {s}"))
}

/// Casts `v` (of type `from`) to `to`.
pub fn cast(v: &Value, from: &Type, to: &Type) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    if types::same(from, to) && !matches!(to, Type::Struct(_) | Type::Array(_)) {
        return Ok(v.clone());
    }
    Ok(match (v, to) {
        (Value::Array(xs), Type::Array(te)) => {
            let fe = match from {
                Type::Array(f) => f.as_ref(),
                _ => te.as_ref(),
            };
            let mut out = Vec::with_capacity(xs.len());
            for x in xs.iter() {
                out.push(cast(x, fe, te)?);
            }
            Value::Array(Arc::from(out))
        }
        (Value::Struct(xs), Type::Struct(tf)) => {
            let ff: Vec<Type> = match from {
                Type::Struct(f) => f.iter().map(|(_, t)| t.clone()).collect(),
                _ => tf.iter().map(|(_, t)| t.clone()).collect(),
            };
            let mut out = Vec::with_capacity(xs.len());
            for ((x, f), (_, t)) in xs.iter().zip(&ff).zip(tf) {
                out.push(cast(x, f, t)?);
            }
            Value::Struct(Arc::from(out))
        }
        (_, Type::String(_)) => match to_string(v)? {
            Some(s) => Value::String(Arc::from(s)),
            None => Value::Null,
        },
        (Value::Int64(i), Type::Float64) => Value::Float64(*i as f64),
        (Value::Int64(i), Type::Float32) => Value::Float32(*i as f32),
        (Value::Int64(i), Type::Numeric) => Value::Numeric(i64_to_numeric(*i)),
        (Value::Int64(i), Type::Bool) => Value::Bool(*i != 0),
        (Value::Bool(b), Type::Int64) => Value::Int64(*b as i64),
        (Value::Float64(f), Type::Int64) => Value::Int64(f64_to_i64(*f)?),
        (Value::Float32(f), Type::Int64) => Value::Int64(f64_to_i64(*f as f64)?),
        (Value::Float64(f), Type::Float32) => Value::Float32(*f as f32),
        (Value::Float32(f), Type::Float64) => Value::Float64(*f as f64),
        (Value::Float64(f), Type::Numeric) => Value::Numeric(f64_to_numeric(*f)?),
        (Value::Float32(f), Type::Numeric) => Value::Numeric(f64_to_numeric(*f as f64)?),
        (Value::Numeric(n), Type::Int64) => Value::Int64(numeric_to_i64(*n)?),
        (Value::Numeric(n), Type::Float64) => Value::Float64(numeric_to_f64(*n)),
        (Value::Numeric(n), Type::Float32) => Value::Float32(numeric_to_f64(*n) as f32),
        (Value::String(s), Type::Int64) => {
            Value::Int64(parse_i64(s).ok_or_else(|| bad("int64", s))?)
        }
        (Value::String(s), Type::Float64) => {
            Value::Float64(parse_f64(s).ok_or_else(|| bad("double", s))?)
        }
        (Value::String(s), Type::Float32) => {
            Value::Float32(parse_f64(s).ok_or_else(|| bad("float", s))? as f32)
        }
        (Value::String(s), Type::Numeric) => {
            Value::Numeric(Numeric::parse(s).ok_or_else(|| bad("NUMERIC", s))?)
        }
        (Value::String(s), Type::Bool) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(bad("bool", s)),
        },
        (Value::String(s), Type::Bytes(_)) => Value::Bytes(Arc::from(s.as_bytes())),
        (Value::String(s), Type::Date) => Value::Date(
            Date::parse(s.trim()).ok_or_else(|| out_of_range(format!("Invalid date: '{s}'")))?,
        ),
        (Value::String(s), Type::Timestamp) => Value::Timestamp(
            time::parse_ts(s, DEFAULT_TZ)
                .ok_or_else(|| out_of_range(format!("Invalid timestamp: '{s}'")))?,
        ),
        (Value::String(s), Type::Interval) => Value::Interval(
            spanbolt_storage::Interval::parse(s)
                .ok_or_else(|| out_of_range(format!("Invalid INTERVAL value '{s}'")))?,
        ),
        (Value::String(s), Type::Uuid) => Value::Uuid(
            spanbolt_storage::value::parse_uuid(s.trim())
                .ok_or_else(|| out_of_range(format!("Invalid input for UUID: {s}")))?,
        ),
        (Value::Date(d), Type::Timestamp) => Value::Timestamp(time::from_local(
            time::naive_date(*d)
                .and_hms_opt(0, 0, 0)
                .unwrap_or_default(),
            DEFAULT_TZ,
        )),
        (Value::Timestamp(t), Type::Date) => {
            Value::Date(time::from_naive_date(time::local(*t, DEFAULT_TZ).date()))
        }
        (Value::Uuid(u), Type::Bytes(_)) => Value::Bytes(Arc::from(u.to_be_bytes().to_vec())),
        (Value::Bytes(b), Type::Uuid) => {
            let arr: [u8; 16] = b
                .as_ref()
                .try_into()
                .map_err(|_| out_of_range("Invalid input for UUID"))?;
            Value::Uuid(u128::from_be_bytes(arr))
        }
        _ => {
            return Err(crate::types::unimplemented(format!(
                "CAST from {} to {}",
                types::spanner_name(from),
                types::spanner_name(to)
            )));
        }
    })
}
