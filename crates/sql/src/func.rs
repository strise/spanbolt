//! Scalar function registry and the core operators. Function families live in `func_*` modules;
//! each exposes `lookup(&Sig) -> Option<ScalarFn>`.

use std::cmp::Ordering;
use std::sync::Arc;

use spanbolt_storage::{Error, Numeric, Result, Value};

use crate::cast;
use crate::exec::Cx;
use crate::types::Type;
use crate::value_ops::{out_of_range, sql_cmp, sql_eq};

pub type ScalarFn = Arc<dyn Fn(&Cx, &[Value]) -> Result<Value> + Send + Sync>;

/// A resolved call: the function name without the `GoogleSQL:` group, argument and result types.
pub struct Sig<'a> {
    pub name: &'a str,
    pub args: &'a [Type],
    pub ret: &'a Type,
    /// `--emulator-compat`: functions that differ between the emulator and Cloud Spanner behave
    /// like the emulator.
    pub emulator_compat: bool,
}

pub fn f(g: impl Fn(&Cx, &[Value]) -> Result<Value> + Send + Sync + 'static) -> ScalarFn {
    Arc::new(g)
}

/// NULL if any argument is NULL, else `g(args)`.
pub fn strict(g: impl Fn(&[Value]) -> Result<Value> + Send + Sync + 'static) -> ScalarFn {
    Arc::new(move |_, a| {
        if a.iter().any(Value::is_null) {
            Ok(Value::Null)
        } else {
            g(a)
        }
    })
}

pub fn lookup(sig: &Sig) -> Option<ScalarFn> {
    // `+`/`-` on dates, timestamps and intervals belong to func_time.
    let numeric = |t: &Type| {
        matches!(
            t,
            Type::Int64 | Type::Float64 | Type::Float32 | Type::Numeric
        )
    };
    let core_ok =
        !matches!(sig.name, "$add" | "$subtract" | "$unary_minus") || sig.args.iter().all(numeric);
    core_ok
        .then(|| core(sig))
        .flatten()
        .or_else(|| crate::func_math::lookup(sig))
        .or_else(|| crate::func_string::lookup(sig))
        .or_else(|| crate::func_array::lookup(sig))
        .or_else(|| crate::func_time::lookup(sig))
        .or_else(|| crate::func_json::lookup(sig))
        .or_else(|| crate::func_hash::lookup(sig))
        .or_else(|| crate::func_net::lookup(sig))
        .or_else(|| crate::search::lookup(sig))
}

pub const NUMERIC_MAX: i128 = 99_999_999_999_999_999_999_999_999_999_999_999_999;
const SCALE: i128 = 1_000_000_000;

fn num_str(n: i128) -> String {
    cast::numeric_string(Numeric(n))
}

pub fn numeric_checked(v: Option<i128>, what: impl FnOnce() -> String) -> Result<Value> {
    match v {
        Some(x) if x.abs() <= NUMERIC_MAX => Ok(Value::Numeric(Numeric(x))),
        _ => Err(out_of_range(format!("numeric overflow: {}", what()))),
    }
}

fn round_div(n: u128, d: u128) -> u128 {
    let q = n / d;
    if (n % d) * 2 >= d { q + 1 } else { q }
}

pub fn numeric_mul(a: i128, b: i128) -> Option<i128> {
    let neg = (a < 0) != (b < 0);
    let (x, y) = (a.unsigned_abs(), b.unsigned_abs());
    let s = SCALE as u128;
    let (x1, x0, y1, y0) = (x / s, x % s, y / s, y % s);
    let r = x1
        .checked_mul(y1)?
        .checked_mul(s)?
        .checked_add(x1.checked_mul(y0)?)?
        .checked_add(x0.checked_mul(y1)?)?
        .checked_add(round_div(x0 * y0, s))?;
    let r = i128::try_from(r).ok()?;
    Some(if neg { -r } else { r })
}

pub fn numeric_div(a: i128, b: i128) -> Result<Option<i128>> {
    if b == 0 {
        return Err(out_of_range(format!(
            "division by zero: {} / {}",
            num_str(a),
            num_str(b)
        )));
    }
    let neg = (a < 0) != (b < 0);
    let (x, y) = (a.unsigned_abs(), b.unsigned_abs());
    let mut q = x / y;
    let mut r = x % y;
    // Nine decimal digits of fraction plus one for rounding.
    for _ in 0..10 {
        let Some(r10) = r.checked_mul(10) else {
            return Err(crate::types::unimplemented(
                "NUMERIC division with a divisor this large",
            ));
        };
        let Some(q10) = q.checked_mul(10) else {
            return Ok(None);
        };
        q = q10 + r10 / y;
        r = r10 % y;
    }
    let q = q / 10 + if q % 10 >= 5 { 1 } else { 0 };
    let q = i128::try_from(q).ok();
    Ok(q.map(|v| if neg { -v } else { v }))
}

fn arith(op: &'static str) -> ScalarFn {
    strict(move |a| {
        Ok(match (&a[0], &a[1]) {
            (Value::Int64(x), Value::Int64(y)) => {
                let r = match op {
                    "+" => x.checked_add(*y),
                    "-" => x.checked_sub(*y),
                    _ => x.checked_mul(*y),
                };
                Value::Int64(
                    r.ok_or_else(|| out_of_range(format!("int64 overflow: {x} {op} {y}")))?,
                )
            }
            (Value::Float64(x), Value::Float64(y)) => {
                let r = match op {
                    "+" => x + y,
                    "-" => x - y,
                    _ => x * y,
                };
                if r.is_infinite() && x.is_finite() && y.is_finite() {
                    return Err(out_of_range(format!(
                        "double overflow: {} {op} {}",
                        cast::format_f64(*x),
                        cast::format_f64(*y)
                    )));
                }
                Value::Float64(r)
            }
            (Value::Float32(x), Value::Float32(y)) => {
                let r = match op {
                    "+" => x + y,
                    "-" => x - y,
                    _ => x * y,
                };
                if r.is_infinite() && x.is_finite() && y.is_finite() {
                    return Err(out_of_range("float overflow"));
                }
                Value::Float32(r)
            }
            (Value::Numeric(x), Value::Numeric(y)) => {
                let r = match op {
                    "+" => x.0.checked_add(y.0),
                    "-" => x.0.checked_sub(y.0),
                    _ => numeric_mul(x.0, y.0),
                };
                numeric_checked(r, || format!("{} {op} {}", num_str(x.0), num_str(y.0)))?
            }
            _ => return Err(Error::internal(format!("bad operands for {op}"))),
        })
    })
}

fn cmp_fn(pred: fn(Ordering) -> bool) -> ScalarFn {
    strict(move |a| {
        Ok(match sql_cmp(&a[0], &a[1]) {
            Some(o) => Value::Bool(pred(o)),
            None => Value::Bool(false),
        })
    })
}

/// LIKE with `%`, `_` and backslash escapes, on characters (STRING) or bytes (BYTES).
pub fn like<T: PartialEq + Copy>(s: &[T], p: &[T], pct: T, us: T, esc: T) -> Result<bool> {
    // Iterative matcher with backtracking on the last `%`.
    let (mut i, mut j) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if j < p.len() {
            let c = p[j];
            if c == pct {
                star = Some((j, i));
                j += 1;
                continue;
            }
            let (lit, adv) = if c == esc {
                match p.get(j + 1) {
                    Some(&n) => (Some(n), 2),
                    None => {
                        return Err(Error::invalid_argument(
                            "LIKE pattern ends with a backslash",
                        ));
                    }
                }
            } else if c == us {
                (None, 1)
            } else {
                (Some(c), 1)
            };
            if i < s.len() && lit.is_none_or(|l| l == s[i]) {
                i += 1;
                j += adv;
                continue;
            }
        } else if i == s.len() {
            return Ok(true);
        }
        match star {
            Some((sj, si)) if si < s.len() => {
                star = Some((sj, si + 1));
                i = si + 1;
                j = sj + 1;
            }
            _ => return Ok(false),
        }
    }
}

fn like_fn() -> ScalarFn {
    strict(|a| {
        Ok(Value::Bool(match (&a[0], &a[1]) {
            (Value::String(s), Value::String(p)) => {
                let s: Vec<char> = s.chars().collect();
                let p: Vec<char> = p.chars().collect();
                like(&s, &p, '%', '_', '\\')?
            }
            (Value::Bytes(s), Value::Bytes(p)) => like(s, p, b'%', b'_', b'\\')?,
            _ => return Err(Error::internal("bad LIKE operands")),
        }))
    })
}

/// `x IN (list)` with three-valued logic.
pub fn in_list(x: &Value, items: &[Value]) -> Value {
    if x.is_null() {
        return Value::Null;
    }
    let mut unknown = false;
    for it in items {
        match sql_eq(x, it) {
            Some(true) => return Value::Bool(true),
            None => unknown = true,
            Some(false) => {}
        }
    }
    if unknown {
        Value::Null
    } else {
        Value::Bool(false)
    }
}

fn distinct(a: &Value, b: &Value) -> bool {
    match (a.is_null(), b.is_null()) {
        (true, true) => false,
        (true, false) | (false, true) => true,
        _ => !crate::value_ops::group_eq(a, b),
    }
}

fn array_index(name: &'static str) -> ScalarFn {
    let ordinal = name.contains("ordinal");
    let safe = name.starts_with("$safe");
    f(move |_, a| {
        let (Value::Array(arr), Value::Int64(i)) = (&a[0], &a[1]) else {
            return Ok(Value::Null);
        };
        let idx = if ordinal { *i - 1 } else { *i };
        match usize::try_from(idx).ok().and_then(|k| arr.get(k)) {
            Some(v) => Ok(v.clone()),
            None if safe => Ok(Value::Null),
            None => Err(out_of_range(format!("Array index {i} is out of bounds"))),
        }
    })
}

fn core(sig: &Sig) -> Option<ScalarFn> {
    Some(match sig.name {
        "$add" => arith("+"),
        "$subtract" => arith("-"),
        "$multiply" => arith("*"),
        "$divide" => strict(|a| match (&a[0], &a[1]) {
            (Value::Float64(x), Value::Float64(y)) => {
                if *y == 0.0 {
                    return Err(out_of_range(format!(
                        "division by zero: {} / {}",
                        cast::format_f64(*x),
                        cast::format_f64(*y)
                    )));
                }
                Ok(Value::Float64(x / y))
            }
            (Value::Float32(x), Value::Float32(y)) => {
                if *y == 0.0 {
                    return Err(out_of_range("division by zero"));
                }
                Ok(Value::Float32(x / y))
            }
            (Value::Numeric(x), Value::Numeric(y)) => {
                let r = numeric_div(x.0, y.0)?;
                numeric_checked(r, || format!("{} / {}", num_str(x.0), num_str(y.0)))
            }
            _ => Err(Error::internal("bad operands for /")),
        }),
        "$unary_minus" => strict(|a| match &a[0] {
            Value::Int64(x) => x
                .checked_neg()
                .map(Value::Int64)
                .ok_or_else(|| out_of_range(format!("int64 overflow: -({x})"))),
            Value::Float64(x) => Ok(Value::Float64(-x)),
            Value::Float32(x) => Ok(Value::Float32(-x)),
            Value::Numeric(x) => Ok(Value::Numeric(Numeric(-x.0))),
            _ => Err(Error::internal("bad operand for unary minus")),
        }),
        "$equal" => strict(|a| Ok(sql_eq(&a[0], &a[1]).map_or(Value::Null, Value::Bool))),
        "$not_equal" => {
            strict(|a| Ok(sql_eq(&a[0], &a[1]).map_or(Value::Null, |b| Value::Bool(!b))))
        }
        "$less" => cmp_fn(|o| o == Ordering::Less),
        "$less_or_equal" => cmp_fn(|o| o != Ordering::Greater),
        "$greater" => cmp_fn(|o| o == Ordering::Greater),
        "$greater_or_equal" => cmp_fn(|o| o != Ordering::Less),
        "$not" => strict(|a| match &a[0] {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            _ => Err(Error::internal("bad operand for NOT")),
        }),
        "$is_null" => f(|_, a| Ok(Value::Bool(a[0].is_null()))),
        "$is_true" => f(|_, a| Ok(Value::Bool(matches!(a[0], Value::Bool(true))))),
        "$is_false" => f(|_, a| Ok(Value::Bool(matches!(a[0], Value::Bool(false))))),
        "$is_distinct_from" => f(|_, a| Ok(Value::Bool(distinct(&a[0], &a[1])))),
        "$is_not_distinct_from" => f(|_, a| Ok(Value::Bool(!distinct(&a[0], &a[1])))),
        "$like" => like_fn(),
        "$between" => f(|_, a| {
            let lo = sql_cmp(&a[0], &a[1]).map(|o| o != Ordering::Less);
            let hi = sql_cmp(&a[0], &a[2]).map(|o| o != Ordering::Greater);
            Ok(match (lo, hi) {
                (Some(false), _) | (_, Some(false)) => Value::Bool(false),
                (Some(true), Some(true)) => Value::Bool(true),
                _ => Value::Null,
            })
        }),
        "$in" => f(|_, a| Ok(in_list(&a[0], &a[1..]))),
        "$in_array" => f(|_, a| match &a[1] {
            Value::Array(items) => Ok(in_list(&a[0], items)),
            _ => Ok(if a[0].is_null() {
                Value::Null
            } else {
                Value::Bool(false)
            }),
        }),
        "$make_array" => f(|_, a| Ok(Value::Array(Arc::from(a.to_vec())))),
        "$array_at_offset" => array_index("$array_at_offset"),
        "$array_at_ordinal" => array_index("$array_at_ordinal"),
        "$safe_array_at_offset" => array_index("$safe_array_at_offset"),
        "$safe_array_at_ordinal" => array_index("$safe_array_at_ordinal"),
        _ => return None,
    })
}
