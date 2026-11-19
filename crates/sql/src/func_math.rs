//! math functions.

use std::cmp::Ordering;
use std::sync::Arc;

use spanbolt_storage::{Error, Numeric, Result, Value};

use crate::cast::numeric_string;
use crate::func::{NUMERIC_MAX, ScalarFn, Sig, numeric_checked, numeric_div, numeric_mul, strict};
use crate::types::Type;
use crate::value_ops::{order_cmp, out_of_range};

const SCALE: i128 = 1_000_000_000;

/// C++ `ostream << double` (`%g`, precision 6), used in floating point error messages.
pub(crate) fn g6(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    let e = format!("{x:.5e}");
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let trim = |s: &str| -> String {
        let mut m = s.to_string();
        if m.contains('.') {
            while m.ends_with('0') {
                m.pop();
            }
            if m.ends_with('.') {
                m.pop();
            }
        }
        m
    };
    if !(-4..6).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mant), exp.abs())
    } else {
        let decimals = (5 - exp).max(0) as usize;
        trim(&format!("{x:.decimals$}"))
    }
}

fn fp_arg(v: &Value) -> String {
    match v {
        Value::Float64(x) => g6(*x),
        Value::Float32(x) => g6(*x as f64),
        Value::Int64(i) => i.to_string(),
        Value::Numeric(n) => numeric_string(*n),
        _ => String::new(),
    }
}

fn call_text(name: &str, args: &[Value]) -> String {
    let a: Vec<String> = args.iter().map(fp_arg).collect();
    format!("{}({})", name.to_uppercase(), a.join(", "))
}

fn fp_error(name: &str, args: &[Value]) -> Error {
    out_of_range(format!(
        "Floating point error in function: {}",
        call_text(name, args)
    ))
}

fn fp_overflow(name: &str, args: &[Value]) -> Error {
    out_of_range(format!(
        "Floating point overflow in function: {}",
        call_text(name, args)
    ))
}

fn num(v: &Value) -> i128 {
    match v {
        Value::Numeric(n) => n.0,
        _ => 0,
    }
}

fn fl(v: &Value) -> f64 {
    match v {
        Value::Float64(x) => *x,
        Value::Float32(x) => *x as f64,
        Value::Int64(i) => *i as f64,
        _ => f64::NAN,
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int64(i) => *i,
        _ => 0,
    }
}

/// Rounds `n` (scaled units) to a multiple of `unit` with `mode`.
#[derive(Clone, Copy)]
enum RMode {
    HalfAway,
    Trunc,
    Ceil,
    Floor,
}

fn round_units(n: i128, unit: i128, mode: RMode) -> Option<i128> {
    let q = n / unit;
    let r = n % unit;
    let adj = match mode {
        RMode::Trunc => 0,
        RMode::HalfAway => {
            if r.unsigned_abs() * 2 >= unit as u128 {
                n.signum()
            } else {
                0
            }
        }
        RMode::Ceil => i128::from(r > 0),
        RMode::Floor => -i128::from(r < 0),
    };
    q.checked_add(adj)?.checked_mul(unit)
}

/// NUMERIC rounding at `digits` decimal places.
fn numeric_round(n: i128, digits: i64, mode: RMode) -> Option<i128> {
    if digits >= 9 {
        return Some(n);
    }
    let shift = 9 - digits;
    if shift > 38 {
        return Some(0);
    }
    let unit = 10i128.checked_pow(shift as u32)?;
    round_units(n, unit, mode)
}

fn fround(x: f64, mode: RMode) -> f64 {
    match mode {
        RMode::HalfAway => x.round(),
        RMode::Trunc => x.trunc(),
        RMode::Ceil => x.ceil(),
        RMode::Floor => x.floor(),
    }
}

/// Rounds `x` at `digits` decimal places on its exact decimal expansion (as GoogleSQL's
/// extended-precision arithmetic does: `ROUND(1.45, 1)` is 1.4).
fn decimal_round(x: f64, digits: i64, mode: RMode) -> f64 {
    let biased = ((x.to_bits() >> 52) & 0x7ff) as i64;
    let prec = (1075 - biased.max(1)).clamp(0, 1074) as usize;
    let text = format!("{:.prec$}", x.abs());
    let (int, frac) = text.split_once('.').unwrap_or((&text, ""));
    let all: Vec<u8> = int.bytes().chain(frac.bytes()).collect();
    let pos = int.len() as i64 + digits;
    if pos >= all.len() as i64 {
        return x;
    }
    let mut keep: Vec<u8> = if pos > 0 {
        all[..pos as usize].to_vec()
    } else {
        Vec::new()
    };
    let first_dropped = if pos >= 0 { all[pos as usize] } else { b'0' };
    let dropped_nonzero = all[pos.max(0) as usize..].iter().any(|&c| c != b'0');
    let up = match mode {
        RMode::HalfAway => first_dropped >= b'5',
        RMode::Trunc => false,
        RMode::Ceil => dropped_nonzero && x > 0.0,
        RMode::Floor => dropped_nonzero && x < 0.0,
    };
    let exp = all.len() as i64 - frac.len() as i64 - pos;
    if up {
        if keep.is_empty() {
            keep.push(b'1');
        } else {
            let mut k = keep.len();
            loop {
                if k == 0 {
                    keep.insert(0, b'1');
                    break;
                }
                k -= 1;
                if keep[k] == b'9' {
                    keep[k] = b'0';
                } else {
                    keep[k] += 1;
                    break;
                }
            }
        }
    }
    let digits_str = if keep.is_empty() {
        "0".to_string()
    } else {
        String::from_utf8_lossy(&keep).into_owned()
    };
    let v: f64 = format!("{digits_str}e{exp}")
        .parse()
        .unwrap_or(f64::INFINITY);
    if x.is_sign_negative() { -v } else { v }
}

fn float_round(name: &'static str, x: f64, digits: i64, mode: RMode) -> Result<f64> {
    if !x.is_finite() {
        return Ok(x);
    }
    if digits == 0 {
        return Ok(fround(x, mode));
    }
    let r = decimal_round(x, digits.clamp(-400, 400), mode);
    if !r.is_finite() {
        return Err(fp_overflow(
            name,
            &[Value::Float64(x), Value::Int64(-digits)],
        ));
    }
    Ok(r)
}

fn rounding(name: &'static str, sig: &Sig, mode: RMode) -> Option<ScalarFn> {
    match (sig.args.first()?, sig.args.len()) {
        (Type::Float64, 1 | 2) => Some(strict(move |a| {
            let d = a.get(1).map(int).unwrap_or(0);
            Ok(Value::Float64(float_round(name, fl(&a[0]), d, mode)?))
        })),
        (Type::Float32, 1) => Some(strict(move |a| match &a[0] {
            Value::Float32(x) => Ok(Value::Float32(fround(*x as f64, mode) as f32)),
            _ => Ok(Value::Null),
        })),
        (Type::Numeric, n @ (1 | 2)) => Some(strict(move |a| {
            let x = num(&a[0]);
            let d = a.get(1).map(int).unwrap_or(0);
            let r = numeric_round(x, d, mode).filter(|r| r.abs() <= NUMERIC_MAX);
            r.map(|r| Value::Numeric(Numeric(r))).ok_or_else(|| {
                let up = name.to_uppercase();
                let msg = if n == 2 || name == "round" {
                    format!("{up}({}, {d})", numeric_string(Numeric(x)))
                } else {
                    format!("{up}({})", numeric_string(Numeric(x)))
                };
                out_of_range(format!("numeric overflow: {msg}"))
            })
        })),
        _ => None,
    }
}

/// A FLOAT64 function of one or two FLOAT64 arguments with GoogleSQL's error reporting:
/// a non-finite result from finite inputs is an error.
fn float_fn(name: &'static str, overflow_msg: bool, g: fn(&[f64]) -> f64) -> ScalarFn {
    strict(move |a| {
        let xs: Vec<f64> = a.iter().map(fl).collect();
        let r = g(&xs);
        let finite_in = xs.iter().all(|x| x.is_finite());
        if finite_in && !r.is_finite() {
            return Err(if overflow_msg && !r.is_nan() {
                fp_overflow(name, a)
            } else {
                fp_error(name, a)
            });
        }
        Ok(Value::Float64(r))
    })
}

fn all_float(sig: &Sig) -> bool {
    matches!(sig.ret, Type::Float64) && sig.args.iter().all(|t| matches!(t, Type::Float64))
}

fn greatest(want: Ordering) -> ScalarFn {
    strict(move |a| {
        if a.iter()
            .any(|v| matches!(v, Value::Float64(x) if x.is_nan()))
            || a.iter()
                .any(|v| matches!(v, Value::Float32(x) if x.is_nan()))
        {
            return Ok(a
                .iter()
                .find(|v| match v {
                    Value::Float64(x) => x.is_nan(),
                    Value::Float32(x) => x.is_nan(),
                    _ => false,
                })
                .cloned()
                .unwrap_or(Value::Null));
        }
        let mut best = &a[0];
        for v in &a[1..] {
            if order_cmp(v, best) == want {
                best = v;
            }
        }
        Ok(best.clone())
    })
}

fn checked_arith(op: &str, a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::Int64(x), Value::Int64(y)) => match op {
            "+" => x.checked_add(*y),
            "-" => x.checked_sub(*y),
            _ => x.checked_mul(*y),
        }
        .map(Value::Int64),
        (Value::Float64(x), Value::Float64(y)) => {
            let r = match op {
                "+" => x + y,
                "-" => x - y,
                _ => x * y,
            };
            (r.is_finite() || !x.is_finite() || !y.is_finite()).then_some(Value::Float64(r))
        }
        (Value::Numeric(x), Value::Numeric(y)) => match op {
            "+" => x.0.checked_add(y.0),
            "-" => x.0.checked_sub(y.0),
            _ => numeric_mul(x.0, y.0),
        }
        .filter(|r| r.abs() <= NUMERIC_MAX)
        .map(|r| Value::Numeric(Numeric(r))),
        _ => None,
    }
}

fn safe_arith(op: &'static str, sig: &Sig) -> Option<ScalarFn> {
    if !matches!(sig.ret, Type::Int64 | Type::Float64 | Type::Numeric) {
        return None;
    }
    Some(strict(move |a| {
        Ok(checked_arith(op, &a[0], &a[1]).unwrap_or(Value::Null))
    }))
}

fn bytes_bitwise(op: &'static str) -> ScalarFn {
    strict(move |a| match (&a[0], &a[1]) {
        (Value::Int64(x), Value::Int64(y)) => Ok(Value::Int64(match op {
            "&" => x & y,
            "|" => x | y,
            _ => x ^ y,
        })),
        (Value::Bytes(x), Value::Bytes(y)) => {
            if x.len() != y.len() {
                return Err(out_of_range(format!(
                    "Bitwise binary operator for BYTES requires equal length of the inputs. Got {} bytes on the left hand side and {} bytes on the right hand side.",
                    x.len(),
                    y.len()
                )));
            }
            let out: Vec<u8> = x
                .iter()
                .zip(y.iter())
                .map(|(p, q)| match op {
                    "&" => p & q,
                    "|" => p | q,
                    _ => p ^ q,
                })
                .collect();
            Ok(Value::Bytes(Arc::from(out)))
        }
        _ => Err(Error::internal("bad bitwise operands")),
    })
}

fn shift_bytes(b: &[u8], n: u64, left: bool) -> Vec<u8> {
    let len = b.len();
    let mut out = vec![0u8; len];
    let total = (len as u64) * 8;
    if n >= total {
        return out;
    }
    let (byte_shift, bit_shift) = ((n / 8) as usize, (n % 8) as u32);
    for (i, slot) in out.iter_mut().enumerate() {
        if left {
            let src = i + byte_shift;
            let hi = b.get(src).copied().unwrap_or(0);
            let lo = b.get(src + 1).copied().unwrap_or(0);
            *slot = if bit_shift == 0 {
                hi
            } else {
                (hi << bit_shift) | (lo >> (8 - bit_shift))
            };
        } else {
            let Some(src) = i.checked_sub(byte_shift) else {
                continue;
            };
            let lo = b.get(src).copied().unwrap_or(0);
            let hi = src
                .checked_sub(1)
                .and_then(|k| b.get(k))
                .copied()
                .unwrap_or(0);
            *slot = if bit_shift == 0 {
                lo
            } else {
                (lo >> bit_shift) | (hi << (8 - bit_shift))
            };
        }
    }
    out
}

fn shift(left: bool) -> ScalarFn {
    strict(move |a| {
        let n = int(&a[1]);
        if n < 0 {
            return Err(out_of_range("Bitwise shift by negative offset."));
        }
        match &a[0] {
            Value::Int64(x) => {
                let u = *x as u64;
                let r = if n >= 64 {
                    0
                } else if left {
                    u << n
                } else {
                    u >> n
                };
                Ok(Value::Int64(r as i64))
            }
            Value::Bytes(b) => Ok(Value::Bytes(Arc::from(shift_bytes(b, n as u64, left)))),
            _ => Err(Error::internal("bad shift operands")),
        }
    })
}

fn numeric_sign(n: i128) -> Value {
    Value::Numeric(Numeric(n.signum() * SCALE))
}

pub fn lookup(sig: &Sig) -> Option<ScalarFn> {
    let first = sig.args.first();
    Some(match sig.name {
        "abs" => match first? {
            Type::Int64 => strict(|a| {
                let x = int(&a[0]);
                x.checked_abs()
                    .map(Value::Int64)
                    .ok_or_else(|| out_of_range(format!("int64 overflow: -{x}")))
            }),
            Type::Float64 => strict(|a| Ok(Value::Float64(fl(&a[0]).abs()))),
            Type::Float32 => strict(|a| match &a[0] {
                Value::Float32(x) => Ok(Value::Float32(x.abs())),
                _ => Ok(Value::Null),
            }),
            Type::Numeric => strict(|a| Ok(Value::Numeric(Numeric(num(&a[0]).abs())))),
            _ => return None,
        },
        "sign" => match first? {
            Type::Int64 => strict(|a| Ok(Value::Int64(int(&a[0]).signum()))),
            Type::Float64 => strict(|a| {
                let x = fl(&a[0]);
                Ok(Value::Float64(if x.is_nan() || x == 0.0 {
                    if x.is_nan() { x } else { 0.0 }
                } else {
                    x.signum()
                }))
            }),
            Type::Numeric => strict(|a| Ok(numeric_sign(num(&a[0])))),
            Type::Float32 => strict(|a| match &a[0] {
                Value::Float32(x) if x.is_nan() => Ok(Value::Float32(*x)),
                Value::Float32(x) if *x == 0.0 => Ok(Value::Float32(0.0)),
                Value::Float32(x) => Ok(Value::Float32(x.signum())),
                _ => Ok(Value::Null),
            }),
            _ => return None,
        },
        "round" => rounding("round", sig, RMode::HalfAway)?,
        "trunc" => rounding("trunc", sig, RMode::Trunc)?,
        "ceil" | "ceiling" if sig.args.len() == 1 => rounding("ceil", sig, RMode::Ceil)?,
        "floor" if sig.args.len() == 1 => rounding("floor", sig, RMode::Floor)?,
        "mod" => match first? {
            Type::Int64 => strict(|a| {
                let (x, y) = (int(&a[0]), int(&a[1]));
                if y == 0 {
                    return Err(out_of_range(format!("division by zero: MOD({x}, {y})")));
                }
                Ok(Value::Int64(x.wrapping_rem(y)))
            }),
            Type::Numeric => strict(|a| {
                let (x, y) = (num(&a[0]), num(&a[1]));
                if y == 0 {
                    return Err(out_of_range(format!(
                        "division by zero: MOD({}, {})",
                        numeric_string(Numeric(x)),
                        numeric_string(Numeric(y))
                    )));
                }
                Ok(Value::Numeric(Numeric(x % y)))
            }),
            _ => return None,
        },
        "div" => match first? {
            Type::Int64 => strict(|a| {
                let (x, y) = (int(&a[0]), int(&a[1]));
                if y == 0 {
                    return Err(out_of_range(format!("division by zero: {x} / {y}")));
                }
                x.checked_div(y)
                    .map(Value::Int64)
                    .ok_or_else(|| out_of_range(format!("int64 overflow: -{x}")))
            }),
            Type::Numeric => strict(|a| {
                let (x, y) = (num(&a[0]), num(&a[1]));
                if y == 0 {
                    return Err(out_of_range(format!(
                        "division by zero: DIV({}, {})",
                        numeric_string(Numeric(x)),
                        numeric_string(Numeric(y))
                    )));
                }
                numeric_checked((x / y).checked_mul(SCALE), || {
                    format!(
                        "DIV({}, {})",
                        numeric_string(Numeric(x)),
                        numeric_string(Numeric(y))
                    )
                })
            }),
            _ => return None,
        },
        "sqrt" if all_float(sig) => strict(|a| {
            let x = fl(&a[0]);
            if x < 0.0 {
                return Err(out_of_range(format!(
                    "Argument to SQRT cannot be negative: {}",
                    g6(x)
                )));
            }
            Ok(Value::Float64(x.sqrt()))
        }),
        "pow" | "power" if all_float(sig) => float_fn("pow", false, |x| x[0].powf(x[1])),
        "exp" if all_float(sig) => float_fn("exp", false, |x| x[0].exp()),
        "ln" if all_float(sig) => float_fn("ln", false, |x| ln(x[0])),
        "log10" if all_float(sig) => float_fn("log10", false, |x| {
            if x[0] <= 0.0 { f64::NAN } else { x[0].log10() }
        }),
        "log" if all_float(sig) && sig.args.len() == 1 => float_fn("log", false, |x| ln(x[0])),
        "log" if all_float(sig) && sig.args.len() == 2 => float_fn("log", false, |x| {
            if x[0] <= 0.0 || x[1] <= 0.0 || x[1] == 1.0 {
                f64::NAN
            } else {
                x[0].ln() / x[1].ln()
            }
        }),
        "cos" if all_float(sig) => float_fn("cos", false, |x| x[0].cos()),
        "sin" if all_float(sig) => float_fn("sin", false, |x| x[0].sin()),
        "tan" if all_float(sig) => float_fn("tan", false, |x| x[0].tan()),
        "acos" if all_float(sig) => float_fn("acos", false, |x| x[0].acos()),
        "asin" if all_float(sig) => float_fn("asin", false, |x| x[0].asin()),
        "atan" if all_float(sig) => float_fn("atan", false, |x| x[0].atan()),
        "atan2" if all_float(sig) => float_fn("atan2", false, |x| x[0].atan2(x[1])),
        "cosh" if all_float(sig) => float_fn("cosh", true, |x| x[0].cosh()),
        "sinh" if all_float(sig) => float_fn("sinh", true, |x| x[0].sinh()),
        "tanh" if all_float(sig) => float_fn("tanh", false, |x| x[0].tanh()),
        "acosh" if all_float(sig) => float_fn("acosh", false, |x| x[0].acosh()),
        "asinh" if all_float(sig) => float_fn("asinh", false, |x| x[0].asinh()),
        "atanh" if all_float(sig) => float_fn("atanh", false, |x| {
            if x[0].abs() >= 1.0 {
                f64::NAN
            } else {
                x[0].atanh()
            }
        }),
        "greatest" => greatest(Ordering::Greater),
        "least" => greatest(Ordering::Less),
        "ieee_divide" if all_float(sig) => strict(|a| Ok(Value::Float64(fl(&a[0]) / fl(&a[1])))),
        "safe_divide" => match first? {
            Type::Float64 => strict(|a| {
                let (x, y) = (fl(&a[0]), fl(&a[1]));
                let r = x / y;
                Ok(if y == 0.0 || (r.is_infinite() && x.is_finite()) {
                    Value::Null
                } else {
                    Value::Float64(r)
                })
            }),
            Type::Numeric => strict(|a| {
                let (x, y) = (num(&a[0]), num(&a[1]));
                if y == 0 {
                    return Ok(Value::Null);
                }
                Ok(match numeric_div(x, y)? {
                    Some(r) if r.abs() <= NUMERIC_MAX => Value::Numeric(Numeric(r)),
                    _ => Value::Null,
                })
            }),
            _ => return None,
        },
        "safe_add" => safe_arith("+", sig)?,
        "safe_subtract" => safe_arith("-", sig)?,
        "safe_multiply" => safe_arith("*", sig)?,
        "safe_negate" => match first? {
            Type::Int64 => {
                strict(|a| Ok(int(&a[0]).checked_neg().map_or(Value::Null, Value::Int64)))
            }
            Type::Float64 => strict(|a| Ok(Value::Float64(-fl(&a[0])))),
            Type::Numeric => strict(|a| Ok(Value::Numeric(Numeric(-num(&a[0]))))),
            _ => return None,
        },
        "is_nan" if matches!(first?, Type::Float64 | Type::Float32) => {
            strict(|a| Ok(Value::Bool(fl(&a[0]).is_nan())))
        }
        "is_inf" if matches!(first?, Type::Float64 | Type::Float32) => {
            strict(|a| Ok(Value::Bool(fl(&a[0]).is_infinite())))
        }
        "bit_count" => strict(|a| match &a[0] {
            Value::Int64(x) => Ok(Value::Int64(x.count_ones() as i64)),
            Value::Bytes(b) => Ok(Value::Int64(b.iter().map(|x| x.count_ones() as i64).sum())),
            _ => Err(Error::internal("bad BIT_COUNT operand")),
        }),
        "$bitwise_and" => bytes_bitwise("&"),
        "$bitwise_or" => bytes_bitwise("|"),
        "$bitwise_xor" => bytes_bitwise("^"),
        "$bitwise_not" => strict(|a| match &a[0] {
            Value::Int64(x) => Ok(Value::Int64(!x)),
            Value::Bytes(b) => Ok(Value::Bytes(b.iter().map(|x| !x).collect())),
            _ => Err(Error::internal("bad bitwise operand")),
        }),
        "$bitwise_left_shift" => shift(true),
        "$bitwise_right_shift" => shift(false),
        _ => return None,
    })
}

fn ln(x: f64) -> f64 {
    if x <= 0.0 { f64::NAN } else { x.ln() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g6_matches_ostream() {
        assert_eq!(g6(0.3333333333), "0.333333");
        assert_eq!(g6(1.5e308), "1.5e+308");
        assert_eq!(g6(-8.0), "-8");
        assert_eq!(g6(1000.0), "1000");
        assert_eq!(g6(1234567.0), "1.23457e+06");
        assert_eq!(g6(0.0001), "0.0001");
    }

    #[test]
    fn exact_float_rounding() {
        assert_eq!(decimal_round(1.45, 1, RMode::HalfAway), 1.4);
        assert_eq!(decimal_round(2.675, 2, RMode::HalfAway), 2.67);
        assert_eq!(decimal_round(0.25, 1, RMode::HalfAway), 0.3);
        assert_eq!(decimal_round(1234.5, -2, RMode::HalfAway), 1200.0);
        assert_eq!(decimal_round(99.5, -2, RMode::HalfAway), 100.0);
        assert_eq!(decimal_round(1.2789, 2, RMode::Trunc), 1.27);
        assert_eq!(decimal_round(1.0, -400, RMode::HalfAway), 0.0);
        assert!(decimal_round(1.5e308, -308, RMode::HalfAway).is_infinite());
    }

    #[test]
    fn numeric_rounding() {
        assert_eq!(
            numeric_round(2_500_000_000, 0, RMode::HalfAway),
            Some(3 * SCALE)
        );
        assert_eq!(
            numeric_round(-2_500_000_000, 0, RMode::HalfAway),
            Some(-3 * SCALE)
        );
        assert_eq!(
            numeric_round(-1_200_000_000, 0, RMode::Floor),
            Some(-2 * SCALE)
        );
        assert_eq!(numeric_round(-1_200_000_000, 0, RMode::Ceil), Some(-SCALE));
    }

    #[test]
    fn byte_shifts() {
        assert_eq!(shift_bytes(&[1, 2], 4, true), vec![0x10, 0x20]);
        assert_eq!(shift_bytes(&[1, 2], 4, false), vec![0x00, 0x10]);
        assert_eq!(shift_bytes(&[1, 2], 8, true), vec![2, 0]);
        assert_eq!(shift_bytes(&[1, 2], 16, true), vec![0, 0]);
    }
}
