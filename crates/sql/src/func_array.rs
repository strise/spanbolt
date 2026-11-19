//! array functions.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;

use chrono::Datelike;
use spanbolt_storage::{Date, Numeric, Result, Value};

use crate::exec::{Cx, Env, Eval};
use crate::func::{NUMERIC_MAX, ScalarFn, Sig, strict};
use crate::time::{MAX_DAYS, MIN_DAYS, from_naive_date, naive_date};
use crate::types::Type;
use crate::value_ops::{Key, order_cmp, out_of_range, sql_eq, truthy};

const MAX_ELEMENTS: i128 = 16_000;

fn arr(v: &Value) -> &[Value] {
    match v {
        Value::Array(xs) => xs,
        _ => &[],
    }
}

fn array(v: Vec<Value>) -> Value {
    Value::Array(Arc::from(v))
}

fn too_many() -> spanbolt_storage::Error {
    out_of_range("Cannot generate arrays with more than 16000 elements.")
}

fn step_zero() -> spanbolt_storage::Error {
    out_of_range("Sequence step cannot be 0.")
}

/// Element count of `start..=end` by `step` (all scaled to integers).
fn count(start: i128, end: i128, step: i128) -> Result<i128> {
    if step == 0 {
        return Err(step_zero());
    }
    let n = if (step > 0 && start > end) || (step < 0 && start < end) {
        0
    } else {
        (end - start) / step + 1
    };
    if n > MAX_ELEMENTS {
        return Err(too_many());
    }
    Ok(n)
}

fn generate_array(a: &[Value]) -> Result<Value> {
    Ok(match (&a[0], &a[1], a.get(2)) {
        (Value::Int64(s), Value::Int64(e), st) => {
            let step = st.map_or(1, |v| match v {
                Value::Int64(x) => *x,
                _ => 1,
            });
            let n = count(*s as i128, *e as i128, step as i128)?;
            array(
                (0..n)
                    .map(|k| Value::Int64((*s as i128 + k * step as i128) as i64))
                    .collect(),
            )
        }
        (Value::Numeric(s), Value::Numeric(e), st) => {
            let step = st.map_or(1_000_000_000, |v| match v {
                Value::Numeric(x) => x.0,
                _ => 1_000_000_000,
            });
            let n = count(s.0, e.0, step)?;
            let mut out = Vec::with_capacity(n as usize);
            for k in 0..n {
                let v = s.0 + k * step;
                if v.abs() > NUMERIC_MAX {
                    break;
                }
                out.push(Value::Numeric(Numeric(v)));
            }
            array(out)
        }
        (Value::Float64(s), Value::Float64(e), st) => {
            let step = st.map_or(1.0, |v| match v {
                Value::Float64(x) => *x,
                _ => 1.0,
            });
            if s.is_nan() || e.is_nan() || step.is_nan() {
                return Err(out_of_range("Sequence start, end, and step cannot be NaN."));
            }
            if step == 0.0 {
                return Err(step_zero());
            }
            if (step > 0.0 && s > e) || (step < 0.0 && s < e) {
                return Ok(array(Vec::new()));
            }
            let span = ((e - s) / step).floor();
            if !span.is_finite() || span + 1.0 > MAX_ELEMENTS as f64 {
                return Err(too_many());
            }
            let mut out = Vec::new();
            let mut k = 0.0f64;
            loop {
                let v = s + k * step;
                if (step > 0.0 && v > *e) || (step < 0.0 && v < *e) {
                    break;
                }
                out.push(Value::Float64(v));
                if out.len() as i128 > MAX_ELEMENTS {
                    return Err(too_many());
                }
                k += 1.0;
            }
            array(out)
        }
        _ => return Err(crate::types::unimplemented("GENERATE_ARRAY of this type")),
    })
}

fn add_months(d: Date, months: i64) -> Option<i64> {
    let nd = naive_date(d);
    let total = nd.year() as i64 * 12 + nd.month0() as i64 + months;
    let y = i32::try_from(total.div_euclid(12)).ok()?;
    let m = total.rem_euclid(12) as u32 + 1;
    let last = (28..=31)
        .rev()
        .find(|&dd| chrono::NaiveDate::from_ymd_opt(y, m, dd).is_some())?;
    let r = chrono::NaiveDate::from_ymd_opt(y, m, nd.day().min(last))?;
    Some(from_naive_date(r).0 as i64)
}

fn generate_date_array(a: &[Value]) -> Result<Value> {
    let (Value::Date(s), Value::Date(e)) = (&a[0], &a[1]) else {
        return Ok(Value::Null);
    };
    let (step, part) = match (a.get(2), a.get(3)) {
        (Some(Value::Int64(n)), Some(Value::Int64(p))) => (*n, *p),
        _ => (1, 3),
    };
    if step == 0 {
        return Err(step_zero());
    }
    let (days, months) = match part {
        3 => (Some(step as i128), None),
        14 => (Some(step as i128 * 7), None),
        2 => (None, Some(step as i128)),
        6 => (None, Some(step as i128 * 3)),
        1 => (None, Some(step as i128 * 12)),
        _ => {
            return Err(crate::types::unimplemented(
                "GENERATE_DATE_ARRAY of this date part",
            ));
        }
    };
    if let Some(d) = days {
        let n = count(s.0 as i128, e.0 as i128, d)?;
        let mut out = Vec::with_capacity(n as usize);
        for k in 0..n {
            let v = s.0 as i128 + k * d;
            if v < MIN_DAYS as i128 || v > MAX_DAYS as i128 {
                break;
            }
            out.push(Value::Date(Date(v as i32)));
        }
        return Ok(array(out));
    }
    let m = i64::try_from(months.unwrap_or(1)).map_err(|_| too_many())?;
    let ascending = m > 0;
    let mut out = Vec::new();
    let mut cur = s.0 as i64;
    loop {
        if (ascending && cur > e.0 as i64) || (!ascending && cur < e.0 as i64) {
            break;
        }
        out.push(Value::Date(Date(cur as i32)));
        if out.len() as i128 > MAX_ELEMENTS {
            return Err(too_many());
        }
        match add_months(Date(cur as i32), m) {
            Some(next) if (MIN_DAYS as i64..=MAX_DAYS as i64).contains(&next) => cur = next,
            _ => break,
        }
    }
    Ok(array(out))
}

/// ARRAY_MIN / ARRAY_MAX: NULL elements are skipped; NULL when nothing is left.
/// A NaN element makes the result NaN for both.
fn array_extreme(xs: &[Value], want: Ordering) -> Value {
    if let Some(nan) = xs.iter().find(|x| match x {
        Value::Float64(f) => f.is_nan(),
        Value::Float32(f) => f.is_nan(),
        _ => false,
    }) {
        return nan.clone();
    }
    let mut best: Option<&Value> = None;
    for x in xs.iter().filter(|x| !x.is_null()) {
        if best.is_none_or(|b| order_cmp(x, b) == want) {
            best = Some(x);
        }
    }
    best.cloned().unwrap_or(Value::Null)
}

/// ARRAY_FIRST / ARRAY_LAST.
fn end(xs: &[Value], fname: &str, last: bool) -> Result<Value> {
    let v = if last { xs.last() } else { xs.first() };
    v.cloned().ok_or_else(|| {
        out_of_range(format!(
            "{fname} cannot get the {} element of an empty array",
            if last { "last" } else { "first" }
        ))
    })
}

/// ARRAY_SLICE: inclusive offsets, negative ones count from the end, clamped to the array.
fn slice(xs: &[Value], start: i64, end: i64) -> &[Value] {
    let n = xs.len() as i128;
    let norm = |i: i64| {
        let i = i as i128;
        if i < 0 { n + i } else { i }
    };
    let s = norm(start).max(0);
    let e = norm(end).min(n - 1);
    if n == 0 || s > e {
        return &[];
    }
    &xs[s as usize..=e as usize]
}

fn includes(xs: &[Value], target: &Value) -> Value {
    Value::Bool(xs.iter().any(|x| sql_eq(x, target) == Some(true)))
}

fn includes_any(xs: &[Value], targets: &[Value]) -> Value {
    Value::Bool(
        targets
            .iter()
            .any(|t| matches!(includes(xs, t), Value::Bool(true))),
    )
}

fn includes_all(xs: &[Value], targets: &[Value]) -> Value {
    Value::Bool(
        targets
            .iter()
            .all(|t| matches!(includes(xs, t), Value::Bool(true))),
    )
}

/// Checked double arithmetic for the distance functions: an infinite result from finite operands
/// is an overflow, as in GoogleSQL's `Multiply`/`Add`/`Subtract`.
fn checked(x: f64, y: f64, op: char) -> Result<f64> {
    let r = match op {
        '*' => x * y,
        '+' => x + y,
        _ => x - y,
    };
    if r.is_infinite() && x.is_finite() && y.is_finite() {
        return Err(out_of_range(format!(
            "double overflow: {} {op} {}",
            format_g(x),
            format_g(y)
        )));
    }
    Ok(r)
}

/// C's `%g` (six significant digits, trailing zeros dropped), as the distance functions print
/// overflowing operands.
fn format_g(f: f64) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    let sci = format!("{f:.5e}");
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let trim = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    if (-4..6).contains(&exp) {
        trim(&format!("{f:.*}", (5 - exp) as usize))
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mant), exp.abs())
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float64(f) => Some(*f),
        Value::Float32(f) => Some(*f as f64),
        Value::Int64(i) => Some(*i as f64),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum Distance {
    Cosine,
    Euclidean,
    Dot,
}

/// COSINE_DISTANCE, EUCLIDEAN_DISTANCE and DOT_PRODUCT over two non-NULL arrays, in double
/// precision (FLOAT32 and INT64 elements are widened first).
fn distance(kind: Distance, a: &[Value], b: &[Value]) -> Result<Value> {
    if a.len() != b.len() {
        return Err(out_of_range(match kind {
            Distance::Dot => format!(
                "Array arguments to DOT_PRODUCT must have equal length. The given arrays have lengths of {} and {}",
                a.len(),
                b.len()
            ),
            _ => format!("Array length mismatch: {} and {}", a.len(), b.len()),
        }));
    }
    // DOT_PRODUCT checks the whole first array for NULLs before the second; the distances check
    // both arrays at each offset.
    if let Distance::Dot = kind {
        for (which, xs) in [("first", a), ("second", b)] {
            if let Some(i) = xs.iter().position(Value::is_null) {
                return Err(out_of_range(format!(
                    "Cannot compute DOT_PRODUCT with a NULL element, since it is unclear if NULLs should be ignored, counted as a zero value, or another interpretation. The NULL element was found in the {which} array argument at OFFSET {i}"
                )));
            }
        }
    }
    let (mut acc, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (p, q) in a.iter().zip(b) {
        let (x, y) = match (as_f64(p), as_f64(q)) {
            (Some(x), Some(y)) => (x, y),
            (x, _) => {
                let which = if x.is_none() { "first" } else { "second" };
                return Err(out_of_range(format!(
                    "NULL array element in {which} argument"
                )));
            }
        };
        match kind {
            // DOT_PRODUCT's sum overflow carries no operands.
            Distance::Dot => {
                acc = checked(acc, checked(x, y, '*')?, '+')
                    .map_err(|_| out_of_range("double overflow"))?
            }
            Distance::Euclidean => {
                let d = checked(x, y, '-')?;
                acc = checked(acc, checked(d, d, '*')?, '+')?;
            }
            Distance::Cosine => {
                acc = checked(acc, checked(x, y, '*')?, '+')?;
                na = checked(na, checked(x, x, '*')?, '+')?;
                nb = checked(nb, checked(y, y, '*')?, '+')?;
            }
        }
    }
    Ok(Value::Float64(match kind {
        Distance::Dot => acc,
        Distance::Euclidean => acc.sqrt(),
        Distance::Cosine => {
            if na == 0.0 || nb == 0.0 {
                return Err(out_of_range(
                    "Cannot compute cosine distance against zero vector",
                ));
            }
            1.0 - acc / (na.sqrt() * nb.sqrt())
        }
    }))
}

fn vector(sig: &Sig) -> Option<ScalarFn> {
    let kind = match sig.name {
        "cosine_distance" => Distance::Cosine,
        "euclidean_distance" => Distance::Euclidean,
        _ => Distance::Dot,
    };
    if sig.args.len() != 2 {
        return None;
    }
    Some(strict(move |a| distance(kind, arr(&a[0]), arr(&a[1]))))
}

/// A compiled inline lambda, called with an element and its offset.
pub type Lambda = Arc<dyn Fn(&Cx, &Env, &Value, usize) -> Result<Value> + Send + Sync>;

/// ARRAY_FILTER, ARRAY_TRANSFORM and ARRAY_INCLUDES with a lambda argument.
pub fn with_lambda(name: &str, arr_ev: Eval, lambda: Lambda) -> Option<Eval> {
    let kind = match name {
        "array_filter" => 0,
        "array_transform" => 1,
        "array_includes" => 2,
        _ => return None,
    };
    Some(Arc::new(move |cx: &Cx, env: &Env| {
        let Value::Array(xs) = arr_ev(cx, env)? else {
            return Ok(Value::Null);
        };
        match kind {
            0 => {
                let mut out = Vec::new();
                for (i, x) in xs.iter().enumerate() {
                    if truthy(&lambda(cx, env, x, i)?) {
                        out.push(x.clone());
                    }
                }
                Ok(array(out))
            }
            1 => {
                let mut out = Vec::with_capacity(xs.len());
                for (i, x) in xs.iter().enumerate() {
                    out.push(lambda(cx, env, x, i)?);
                }
                Ok(array(out))
            }
            _ => {
                for (i, x) in xs.iter().enumerate() {
                    if truthy(&lambda(cx, env, x, i)?) {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
        }
    }))
}

pub fn lookup(sig: &Sig) -> Option<ScalarFn> {
    Some(match sig.name {
        "array_min" => strict(|a| Ok(array_extreme(arr(&a[0]), Ordering::Less))),
        "array_max" => strict(|a| Ok(array_extreme(arr(&a[0]), Ordering::Greater))),
        "array_first" => strict(|a| end(arr(&a[0]), "ARRAY_FIRST", false)),
        "array_last" => strict(|a| end(arr(&a[0]), "ARRAY_LAST", true)),
        "array_slice" => strict(|a| match (&a[1], &a[2]) {
            (Value::Int64(s), Value::Int64(e)) => Ok(array(slice(arr(&a[0]), *s, *e).to_vec())),
            _ => Ok(Value::Null),
        }),
        "array_includes" => strict(|a| Ok(includes(arr(&a[0]), &a[1]))),
        "array_includes_any" => strict(|a| Ok(includes_any(arr(&a[0]), arr(&a[1])))),
        "array_includes_all" => strict(|a| Ok(includes_all(arr(&a[0]), arr(&a[1])))),
        "cosine_distance" | "euclidean_distance" | "dot_product" => vector(sig)?,
        "array_length" => strict(|a| Ok(Value::Int64(arr(&a[0]).len() as i64))),
        "array_to_string" => strict(|a| {
            let bytes_kind = matches!(a[1], Value::Bytes(_));
            let raw = |v: &Value| -> Vec<u8> {
                match v {
                    Value::String(x) => x.as_bytes().to_vec(),
                    Value::Bytes(x) => x.to_vec(),
                    _ => Vec::new(),
                }
            };
            let delim = raw(&a[1]);
            let null_text = a.get(2).map(raw);
            let mut out: Vec<u8> = Vec::new();
            let mut first = true;
            for x in arr(&a[0]) {
                let piece = match (x, &null_text) {
                    (Value::Null, None) => continue,
                    (Value::Null, Some(t)) => t.clone(),
                    (v, _) => raw(v),
                };
                if !first {
                    out.extend_from_slice(&delim);
                }
                first = false;
                out.extend(piece);
            }
            Ok(if bytes_kind {
                Value::Bytes(Arc::from(out))
            } else {
                Value::String(Arc::from(String::from_utf8_lossy(&out).into_owned()))
            })
        }),
        "array_concat" => strict(|a| {
            let mut out = Vec::new();
            for x in a {
                out.extend_from_slice(arr(x));
            }
            Ok(array(out))
        }),
        "array_reverse" => strict(|a| Ok(array(arr(&a[0]).iter().rev().cloned().collect()))),
        "array_is_distinct" => strict(|a| {
            let mut seen = HashSet::new();
            for x in arr(&a[0]) {
                if !seen.insert(Key(vec![x.clone()])) {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }),
        "generate_array" => match sig.args.first()? {
            Type::Int64 | Type::Float64 | Type::Numeric => strict(generate_array),
            _ => return None,
        },
        "generate_date_array" => strict(generate_date_array),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_steps_multiply() {
        let v = generate_array(&[
            Value::Float64(0.0),
            Value::Float64(1.0),
            Value::Float64(0.1),
        ]);
        let Ok(Value::Array(xs)) = v else {
            panic!("expected an array");
        };
        assert_eq!(xs.len(), 11);
        assert_eq!(xs[3], Value::Float64(0.30000000000000004));
        assert_eq!(xs[6], Value::Float64(0.6000000000000001));
    }

    #[test]
    fn int_edges() {
        let v = generate_array(&[
            Value::Int64(i64::MIN),
            Value::Int64(i64::MAX),
            Value::Int64(i64::MAX),
        ]);
        let Ok(Value::Array(xs)) = v else {
            panic!("expected an array");
        };
        assert_eq!(xs.len(), 3);
        assert!(generate_array(&[Value::Int64(1), Value::Int64(16001)]).is_err());
    }

    fn f64s(xs: &[Option<f64>]) -> Vec<Value> {
        xs.iter()
            .map(|x| x.map_or(Value::Null, Value::Float64))
            .collect()
    }

    fn err(r: Result<Value>) -> String {
        match r {
            Err(e) => e.message,
            Ok(v) => panic!("expected an error, got {v:?}"),
        }
    }

    #[test]
    fn dot_product_checks_first_array_for_nulls_first() {
        let a = f64s(&[Some(1.0), None]);
        let b = f64s(&[None, Some(4.0)]);
        assert!(err(distance(Distance::Dot, &a, &b)).ends_with("first array argument at OFFSET 1"));
        assert_eq!(
            err(distance(Distance::Cosine, &a, &b)),
            "NULL array element in second argument"
        );
    }

    #[test]
    fn overflow_operands_print_like_printf_g() {
        assert_eq!(format_g(1e308), "1e+308");
        assert_eq!(format_g(1.6899999999999998e308), "1.69e+308");
        assert_eq!(format_g(-1e308), "-1e+308");
        assert_eq!(format_g(123456.7), "123457");
        assert_eq!(format_g(1234567.0), "1.23457e+06");
        assert_eq!(format_g(0.0001), "0.0001");
        assert_eq!(format_g(0.00001234), "1.234e-05");
        assert_eq!(format_g(2.5), "2.5");
    }

    #[test]
    fn dot_product_sum_overflow_has_no_operands() {
        let a = f64s(&[Some(1e308), Some(1e308)]);
        let b = f64s(&[Some(1.0), Some(1.0)]);
        assert_eq!(err(distance(Distance::Dot, &a, &b)), "double overflow");
    }
}
