//! Aggregation: GROUP BY with aggregate functions.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use spanbolt_storage::{Error, Numeric, Result, Value};

use crate::agg_variance::{self, Stat};
use crate::exec::{BoxOp, Cx, Env, Eval, Op, Row, RowIter, indent};
use crate::func::{NUMERIC_MAX, numeric_checked, numeric_div};
use crate::ops::{SortKey, compare_keys};
use crate::types::{Type, unimplemented};
use crate::value_ops::{Key, order_cmp, out_of_range};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AggKind {
    CountStar,
    Count,
    CountIf,
    Sum,
    Avg,
    Min,
    Max,
    ArrayAgg,
    ArrayConcatAgg,
    StringAgg,
    LogicalAnd,
    LogicalOr,
    AnyValue,
    BitAnd,
    BitOr,
    BitXor,
    StddevSamp,
    VarSamp,
}

impl AggKind {
    pub fn parse(name: &str) -> Result<AggKind> {
        Ok(match name {
            "$count_star" => AggKind::CountStar,
            "count" => AggKind::Count,
            "countif" => AggKind::CountIf,
            "sum" => AggKind::Sum,
            "avg" => AggKind::Avg,
            "min" => AggKind::Min,
            "max" => AggKind::Max,
            "array_agg" => AggKind::ArrayAgg,
            "array_concat_agg" => AggKind::ArrayConcatAgg,
            "string_agg" => AggKind::StringAgg,
            "logical_and" => AggKind::LogicalAnd,
            "logical_or" => AggKind::LogicalOr,
            "any_value" => AggKind::AnyValue,
            "bit_and" => AggKind::BitAnd,
            "bit_or" => AggKind::BitOr,
            "bit_xor" => AggKind::BitXor,
            "stddev" | "stddev_samp" => AggKind::StddevSamp,
            "variance" | "var_samp" => AggKind::VarSamp,
            other => {
                return Err(unimplemented(format!(
                    "Aggregate function {}",
                    other.to_uppercase()
                )));
            }
        })
    }
}

pub struct AggSpec {
    pub kind: AggKind,
    pub args: Vec<Eval>,
    pub distinct: bool,
    pub ignore_nulls: bool,
    pub order: Vec<SortKey>,
    pub limit: Option<Eval>,
    pub ret: Type,
}

pub struct Aggregate {
    pub input: BoxOp,
    pub group: Vec<Eval>,
    pub aggs: Vec<AggSpec>,
}

/// Running state of one aggregate in one group.
///
/// Aggregates that only need a few bytes of state (COUNT, SUM, AVG, MIN, MAX, ...) without
/// DISTINCT, ORDER BY or LIMIT fold each row as it arrives. Everything else buffers its argument
/// values and ORDER BY keys and computes the result in `finish`.
enum Acc {
    Buffer(Vec<(Vec<Value>, Vec<Value>)>),
    Fold(Fold),
}

enum Fold {
    /// COUNT(*), COUNT(x) and COUNTIF(x).
    Count(i64),
    /// SUM: the running sum, or the first overflow error (reported at the end, like before).
    Sum(Result<Option<Value>>),
    /// AVG over NUMERIC: exact scaled sum and count of non-NULL inputs.
    AvgNumeric(Result<i128>, i64),
    /// AVG over everything else: f64 sum and count of non-NULL inputs.
    AvgFloat(f64, i64),
    /// MIN / MAX: the best value so far; a NaN sticks once seen.
    MinMax(Option<Value>),
    Logical(Option<bool>),
    AnyValue(Option<Value>),
    Bit(Option<i64>),
}

impl Acc {
    fn new(spec: &AggSpec) -> Acc {
        if spec.kind == AggKind::CountStar {
            return Acc::Fold(Fold::Count(0));
        }
        if spec.distinct || !spec.order.is_empty() || spec.limit.is_some() {
            return Acc::Buffer(Vec::new());
        }
        Acc::Fold(match spec.kind {
            AggKind::Count | AggKind::CountIf => Fold::Count(0),
            AggKind::Sum => Fold::Sum(Ok(None)),
            AggKind::Avg if matches!(spec.ret, Type::Numeric) => Fold::AvgNumeric(Ok(0), 0),
            AggKind::Avg => Fold::AvgFloat(0.0, 0),
            AggKind::Min | AggKind::Max => Fold::MinMax(None),
            AggKind::LogicalAnd | AggKind::LogicalOr => Fold::Logical(None),
            AggKind::AnyValue => Fold::AnyValue(None),
            AggKind::BitAnd | AggKind::BitOr | AggKind::BitXor => Fold::Bit(None),
            _ => return Acc::Buffer(Vec::new()),
        })
    }
}

/// Folds one non-NULL argument value into a running state.
fn step(kind: AggKind, f: &mut Fold, v: Value) {
    match f {
        Fold::Count(n) => {
            if kind != AggKind::CountIf || matches!(v, Value::Bool(true)) {
                *n += 1;
            }
        }
        Fold::Sum(acc) => {
            if let Ok(cur) = acc {
                *acc = sum_step(cur.take(), v).map(Some);
            }
        }
        Fold::AvgNumeric(s, n) => {
            *n += 1;
            if let (Ok(cur), Value::Numeric(x)) = (&*s, &v) {
                *s = cur
                    .checked_add(x.0)
                    .ok_or_else(|| out_of_range("numeric overflow"));
            }
        }
        Fold::AvgFloat(s, n) => {
            *n += 1;
            *s += match v {
                Value::Int64(i) => i as f64,
                Value::Float64(f) => f,
                Value::Float32(f) => f as f64,
                _ => 0.0,
            };
        }
        Fold::MinMax(best) => {
            if best.as_ref().is_some_and(is_nan) {
                return;
            }
            let better = match best {
                None => true,
                Some(b) => {
                    let o = order_cmp(&v, b);
                    is_nan(&v)
                        || if kind == AggKind::Min {
                            o.is_lt()
                        } else {
                            o.is_gt()
                        }
                }
            };
            if better {
                *best = Some(v);
            }
        }
        Fold::Logical(r) => {
            let and = kind == AggKind::LogicalAnd;
            let mut cur = r.unwrap_or(and);
            if let Value::Bool(b) = v {
                cur = if and { cur && b } else { cur || b };
            }
            *r = Some(cur);
        }
        Fold::AnyValue(r) => {
            if r.is_none() {
                *r = Some(v);
            }
        }
        Fold::Bit(r) => {
            if let Value::Int64(i) = v {
                *r = Some(match (*r, kind) {
                    (None, _) => i,
                    (Some(x), AggKind::BitAnd) => x & i,
                    (Some(x), AggKind::BitOr) => x | i,
                    (Some(x), _) => x ^ i,
                });
            }
        }
    }
}

fn is_nan(v: &Value) -> bool {
    matches!(v, Value::Float64(f) if f.is_nan()) || matches!(v, Value::Float32(f) if f.is_nan())
}

fn finish_fold(f: Fold) -> Result<Value> {
    Ok(match f {
        Fold::Count(n) => Value::Int64(n),
        Fold::Sum(acc) => acc?.unwrap_or(Value::Null),
        Fold::AvgNumeric(s, n) => {
            if n == 0 {
                return Ok(Value::Null);
            }
            match numeric_div(s?, n as i128 * 1_000_000_000)? {
                Some(q) if q.abs() <= NUMERIC_MAX => Value::Numeric(Numeric(q)),
                _ => return Err(out_of_range("numeric overflow")),
            }
        }
        Fold::AvgFloat(s, n) => {
            if n == 0 {
                Value::Null
            } else {
                Value::Float64(s / n as f64)
            }
        }
        Fold::MinMax(v) | Fold::AnyValue(v) => v.unwrap_or(Value::Null),
        Fold::Logical(r) => r.map_or(Value::Null, Value::Bool),
        Fold::Bit(r) => r.map_or(Value::Null, Value::Int64),
    })
}

impl Op for Aggregate {
    fn open<'a>(&'a self, cx: &'a Cx<'a>, env: &'a Env<'a>) -> Result<RowIter<'a>> {
        let mut groups: HashMap<Key, usize> = HashMap::new();
        let mut order: Vec<(Vec<Value>, Vec<Acc>)> = Vec::new();
        for r in self.input.open(cx, env)? {
            let r = r?;
            let e = Env {
                row: &r,
                up: Some(env),
            };
            let mut gk = Vec::with_capacity(self.group.len());
            for g in &self.group {
                gk.push(g(cx, &e)?);
            }
            let key = Key(gk);
            let idx = match groups.get(&key) {
                Some(&i) => i,
                None => {
                    let i = order.len();
                    order.push((key.0.clone(), self.aggs.iter().map(Acc::new).collect()));
                    groups.insert(key, i);
                    i
                }
            };
            let accs = &mut order[idx].1;
            for (spec, acc) in self.aggs.iter().zip(accs.iter_mut()) {
                if spec.kind == AggKind::CountStar {
                    if let Acc::Fold(Fold::Count(n)) = acc {
                        *n += 1;
                    }
                    continue;
                }
                let mut args = Vec::with_capacity(spec.args.len());
                for a in &spec.args {
                    args.push(a(cx, &e)?);
                }
                match acc {
                    Acc::Fold(f) => {
                        // Every folded aggregate skips NULL inputs.
                        if let Some(v) = args.into_iter().next().filter(|v| !v.is_null()) {
                            step(spec.kind, f, v);
                        }
                    }
                    Acc::Buffer(rows) => {
                        let mut keys = Vec::with_capacity(spec.order.len());
                        for k in &spec.order {
                            keys.push((k.expr)(cx, &e)?);
                        }
                        rows.push((args, keys));
                    }
                }
            }
        }
        if order.is_empty() && self.group.is_empty() {
            order.push((Vec::new(), self.aggs.iter().map(Acc::new).collect()));
        }
        let mut out = Vec::with_capacity(order.len());
        for (gk, accs) in order {
            let mut row: Row = gk;
            for (spec, acc) in self.aggs.iter().zip(accs) {
                row.push(finish(cx, env, spec, acc)?);
            }
            out.push(row);
        }
        Ok(Box::new(out.into_iter().map(Ok)))
    }

    fn describe(&self, out: &mut String, d: usize) {
        indent(out, d);
        out.push_str(&format!(
            "Aggregate(groups={}, aggs={})\n",
            self.group.len(),
            self.aggs.len()
        ));
        self.input.describe(out, d + 1);
    }
}

fn finish(cx: &Cx, env: &Env, spec: &AggSpec, acc: Acc) -> Result<Value> {
    let mut rows = match acc {
        Acc::Fold(f) => return finish_fold(f),
        Acc::Buffer(rows) => rows,
    };
    let skips_nulls = !matches!(spec.kind, AggKind::ArrayAgg) || spec.ignore_nulls;
    if skips_nulls {
        rows.retain(|(a, _)| a.first().is_none_or(|v| !v.is_null()));
    }
    if spec.distinct {
        let mut seen = HashSet::new();
        rows.retain(|(a, _)| seen.insert(Key(a[..1.min(a.len())].to_vec())));
    }
    if !spec.order.is_empty() {
        rows.sort_by(|a, b| compare_keys(&spec.order, &a.1, &b.1));
    }
    if let Some(l) = &spec.limit {
        match l(cx, env)? {
            Value::Int64(n) if n >= 0 => rows.truncate(n as usize),
            _ => return Err(out_of_range("LIMIT in aggregate must be non-negative")),
        }
    }
    let vals = || rows.iter().map(|(a, _)| &a[0]);
    Ok(match spec.kind {
        AggKind::CountStar => unreachable_count(),
        AggKind::Count => Value::Int64(rows.len() as i64),
        AggKind::CountIf => {
            Value::Int64(vals().filter(|v| matches!(v, Value::Bool(true))).count() as i64)
        }
        AggKind::Sum => sum(vals())?,
        AggKind::Avg => avg(vals(), &spec.ret)?,
        AggKind::StddevSamp => agg_variance::finish(Stat::StddevSamp, vals()),
        AggKind::VarSamp => agg_variance::finish(Stat::VarSamp, vals()),
        AggKind::Min | AggKind::Max => {
            let mut best: Option<&Value> = None;
            for v in vals() {
                if matches!(v, Value::Float64(f) if f.is_nan())
                    || matches!(v, Value::Float32(f) if f.is_nan())
                {
                    return Ok(v.clone());
                }
                best = match best {
                    None => Some(v),
                    Some(b) => {
                        let o = order_cmp(v, b);
                        let better = if spec.kind == AggKind::Min {
                            o.is_lt()
                        } else {
                            o.is_gt()
                        };
                        Some(if better { v } else { b })
                    }
                };
            }
            best.cloned().unwrap_or(Value::Null)
        }
        AggKind::ArrayAgg => {
            if rows.is_empty() {
                Value::Null
            } else {
                Value::Array(Arc::from(vals().cloned().collect::<Vec<_>>()))
            }
        }
        AggKind::ArrayConcatAgg => {
            if rows.is_empty() {
                Value::Null
            } else {
                let mut out = Vec::new();
                for v in vals() {
                    if let Value::Array(a) = v {
                        out.extend(a.iter().cloned());
                    }
                }
                Value::Array(Arc::from(out))
            }
        }
        AggKind::StringAgg => {
            if rows.is_empty() {
                return Ok(Value::Null);
            }
            let delim = rows[0].0.get(1).cloned();
            match vals().next() {
                Some(Value::Bytes(_)) => {
                    let d: Vec<u8> = match delim {
                        Some(Value::Bytes(b)) => b.to_vec(),
                        _ => b",".to_vec(),
                    };
                    let mut out = Vec::new();
                    for (i, v) in vals().enumerate() {
                        if i > 0 {
                            out.extend_from_slice(&d);
                        }
                        if let Value::Bytes(b) = v {
                            out.extend_from_slice(b);
                        }
                    }
                    Value::Bytes(Arc::from(out))
                }
                _ => {
                    let d = match delim {
                        Some(Value::String(s)) => s.to_string(),
                        _ => ",".to_string(),
                    };
                    let mut out = String::new();
                    for (i, v) in vals().enumerate() {
                        if i > 0 {
                            out.push_str(&d);
                        }
                        if let Value::String(s) = v {
                            out.push_str(s);
                        }
                    }
                    Value::String(Arc::from(out))
                }
            }
        }
        AggKind::LogicalAnd | AggKind::LogicalOr => {
            if rows.is_empty() {
                return Ok(Value::Null);
            }
            let and = spec.kind == AggKind::LogicalAnd;
            let mut r = and;
            for v in vals() {
                if let Value::Bool(b) = v {
                    r = if and { r && *b } else { r || *b };
                }
            }
            Value::Bool(r)
        }
        AggKind::AnyValue => vals().next().cloned().unwrap_or(Value::Null),
        AggKind::BitAnd | AggKind::BitOr | AggKind::BitXor => {
            let mut r: Option<i64> = None;
            for v in vals() {
                if let Value::Int64(i) = v {
                    r = Some(match (r, spec.kind) {
                        (None, _) => *i,
                        (Some(x), AggKind::BitAnd) => x & i,
                        (Some(x), AggKind::BitOr) => x | i,
                        (Some(x), _) => x ^ i,
                    });
                }
            }
            r.map_or(Value::Null, Value::Int64)
        }
    })
}

fn unreachable_count() -> Value {
    Value::Null
}

fn sum<'v>(vals: impl Iterator<Item = &'v Value>) -> Result<Value> {
    let mut acc: Option<Value> = None;
    for v in vals {
        acc = Some(sum_step(acc, v.clone())?);
    }
    Ok(acc.unwrap_or(Value::Null))
}

fn sum_step(acc: Option<Value>, v: Value) -> Result<Value> {
    Ok(match (acc, v) {
        (None, v) => v,
        (Some(Value::Int64(a)), Value::Int64(b)) => Value::Int64(
            a.checked_add(b)
                .ok_or_else(|| out_of_range("int64 overflow"))?,
        ),
        (Some(Value::Float64(a)), Value::Float64(b)) => Value::Float64(a + b),
        (Some(Value::Float32(a)), Value::Float32(b)) => Value::Float32(a + b),
        (Some(Value::Numeric(a)), Value::Numeric(b)) => {
            numeric_checked(a.0.checked_add(b.0), || "SUM".into())?
        }
        (Some(Value::Interval(_)), _) => return Err(unimplemented("SUM of INTERVAL")),
        _ => return Err(Error::internal("bad SUM input")),
    })
}

fn avg<'v>(vals: impl Iterator<Item = &'v Value>, ret: &Type) -> Result<Value> {
    let vals: Vec<&Value> = vals.collect();
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    let n = vals.len();
    match ret {
        Type::Numeric => {
            let mut s: i128 = 0;
            for v in &vals {
                if let Value::Numeric(x) = v {
                    s = s
                        .checked_add(x.0)
                        .ok_or_else(|| out_of_range("numeric overflow"))?;
                }
            }
            let q = numeric_div(s, n as i128 * 1_000_000_000)?;
            match q {
                Some(q) if q.abs() <= NUMERIC_MAX => Ok(Value::Numeric(Numeric(q))),
                _ => Err(out_of_range("numeric overflow")),
            }
        }
        _ => {
            let mut s = 0f64;
            for v in &vals {
                s += match v {
                    Value::Int64(i) => *i as f64,
                    Value::Float64(f) => *f,
                    Value::Float32(f) => *f as f64,
                    _ => 0.0,
                };
            }
            Ok(Value::Float64(s / n as f64))
        }
    }
}
