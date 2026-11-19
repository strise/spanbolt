//! Comparison, equality and hashing of query values with GoogleSQL semantics.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use spanbolt_storage::{Code, Error, Value};

pub fn out_of_range(m: impl Into<String>) -> Error {
    Error::new(Code::OutOfRange, m)
}

fn float_order(a: f64, b: f64) -> Ordering {
    // ORDER BY: NaN sorts before every other float.
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// Total order used by ORDER BY, MIN/MAX and sorting: NULL first, NaN before other floats.
pub fn order_cmp(a: &Value, b: &Value) -> Ordering {
    use Value as V;
    match (a, b) {
        (V::Null, V::Null) => Ordering::Equal,
        (V::Null, _) => Ordering::Less,
        (_, V::Null) => Ordering::Greater,
        (V::Bool(x), V::Bool(y)) => x.cmp(y),
        (V::Int64(x), V::Int64(y)) => x.cmp(y),
        (V::Float64(x), V::Float64(y)) => float_order(*x, *y),
        (V::Float32(x), V::Float32(y)) => float_order(*x as f64, *y as f64),
        (V::Int64(x), V::Float64(y)) => float_order(*x as f64, *y),
        (V::Float64(x), V::Int64(y)) => float_order(*x, *y as f64),
        (V::Numeric(x), V::Numeric(y)) => x.cmp(y),
        (V::String(x), V::String(y)) => x.as_bytes().cmp(y.as_bytes()),
        (V::Bytes(x), V::Bytes(y)) => x.cmp(y),
        (V::Date(x), V::Date(y)) => x.cmp(y),
        (V::Timestamp(x), V::Timestamp(y)) => x.cmp(y),
        (V::Uuid(x), V::Uuid(y)) => x.cmp(y),
        (V::Interval(x), V::Interval(y)) => interval_key(x).cmp(&interval_key(y)),
        (V::Json(x), V::Json(y)) => x.cmp(y),
        (V::Array(x), V::Array(y)) | (V::Struct(x), V::Struct(y)) => {
            for (p, q) in x.iter().zip(y.iter()) {
                let o = order_cmp(p, q);
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        _ => Ordering::Equal,
    }
}

fn interval_key(i: &spanbolt_storage::Interval) -> i128 {
    // GoogleSQL compares intervals by normalised length: 1 month = 30 days.
    const DAY: i128 = 86_400_000_000_000;
    (i.months as i128 * 30 + i.days as i128) * DAY + i.nanos
}

/// SQL comparison: `None` when either side is NULL or the values are unordered (NaN).
pub fn sql_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    use Value as V;
    match (a, b) {
        (V::Null, _) | (_, V::Null) => None,
        (V::Float64(x), V::Float64(y)) => x.partial_cmp(y),
        (V::Float32(x), V::Float32(y)) => x.partial_cmp(y),
        (V::Int64(x), V::Float64(y)) => (*x as f64).partial_cmp(y),
        (V::Float64(x), V::Int64(y)) => x.partial_cmp(&(*y as f64)),
        _ => Some(order_cmp(a, b)),
    }
}

/// SQL `=`: NULL when either side is NULL. Arrays and structs compare element-wise.
pub fn sql_eq(a: &Value, b: &Value) -> Option<bool> {
    use Value as V;
    match (a, b) {
        (V::Null, _) | (_, V::Null) => None,
        (V::Array(x), V::Array(y)) | (V::Struct(x), V::Struct(y)) => {
            if x.len() != y.len() {
                return Some(false);
            }
            let mut unknown = false;
            for (p, q) in x.iter().zip(y.iter()) {
                match sql_eq(p, q) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown { None } else { Some(true) }
        }
        (V::Json(x), V::Json(y)) => Some(json_eq(x, y)),
        _ => sql_cmp(a, b).map(|o| o == Ordering::Equal),
    }
}

fn json_eq(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (
        serde_json::from_str::<serde_json::Value>(a),
        serde_json::from_str::<serde_json::Value>(b),
    ) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Grouping key: GROUP BY, DISTINCT, set operations and hash joins. NULLs group together, as do
/// NaNs, and `0.0` equals `-0.0`.
#[derive(Debug, Clone)]
pub struct Key(pub Vec<Value>);

fn hash_value<H: Hasher>(v: &Value, h: &mut H) {
    use Value as V;
    std::mem::discriminant(v).hash(h);
    match v {
        V::Null => {}
        V::Bool(b) => b.hash(h),
        V::Int64(i) => i.hash(h),
        V::Float64(f) => norm_f64(*f).hash(h),
        V::Float32(f) => norm_f64(*f as f64).hash(h),
        V::Numeric(n) => n.hash(h),
        V::String(s) | V::Json(s) => s.hash(h),
        V::Bytes(b) | V::TokenList(b) => b.hash(h),
        V::Date(d) => d.hash(h),
        V::Timestamp(t) => t.as_nanos().hash(h),
        V::Interval(i) => interval_key(i).hash(h),
        V::Uuid(u) => u.hash(h),
        V::Array(xs) | V::Struct(xs) => {
            xs.len().hash(h);
            for x in xs.iter() {
                hash_value(x, h);
            }
        }
    }
}

fn norm_f64(f: f64) -> u64 {
    if f.is_nan() {
        u64::MAX
    } else if f == 0.0 {
        0
    } else {
        f.to_bits()
    }
}

/// Equality for grouping (NULL = NULL, NaN = NaN).
pub fn group_eq(a: &Value, b: &Value) -> bool {
    use Value as V;
    match (a, b) {
        (V::Null, V::Null) => true,
        (V::Null, _) | (_, V::Null) => false,
        (V::Float64(x), V::Float64(y)) => norm_f64(*x) == norm_f64(*y),
        (V::Float32(x), V::Float32(y)) => norm_f64(*x as f64) == norm_f64(*y as f64),
        (V::Array(x), V::Array(y)) | (V::Struct(x), V::Struct(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| group_eq(p, q))
        }
        _ => std::mem::discriminant(a) == std::mem::discriminant(b) && order_cmp(a, b).is_eq(),
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Key) -> bool {
        self.0.len() == other.0.len() && self.0.iter().zip(&other.0).all(|(a, b)| group_eq(a, b))
    }
}

impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, h: &mut H) {
        for v in &self.0 {
            hash_value(v, h);
        }
    }
}

pub fn truthy(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}
