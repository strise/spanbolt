//! DATE and TIMESTAMP functions.

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike, Weekday};
use spanbolt_storage::{Date, Result, Timestamp, Value};

use crate::func::{ScalarFn, Sig, f, strict};
use crate::time::{
    self, MAX_DAYS, MAX_SECS, MIN_DAYS, MIN_SECS, Zone, from_naive_date, naive_date,
};
use crate::types::Type;
use crate::value_ops::out_of_range;

// googlesql functions.DateTimestampPart
const YEAR: i64 = 1;
const MONTH: i64 = 2;
const DAY: i64 = 3;
const DAYOFWEEK: i64 = 4;
const DAYOFYEAR: i64 = 5;
const QUARTER: i64 = 6;
const HOUR: i64 = 7;
const MINUTE: i64 = 8;
const SECOND: i64 = 9;
const MILLISECOND: i64 = 10;
const MICROSECOND: i64 = 11;
const NANOSECOND: i64 = 12;
const DATE: i64 = 13;
const WEEK: i64 = 14;
const ISOYEAR: i64 = 17;
const ISOWEEK: i64 = 18;

const NANOS: i128 = 1_000_000_000;
const MIN_NANOS: i128 = MIN_SECS as i128 * NANOS;
const MAX_NANOS: i128 = MAX_SECS as i128 * NANOS + 999_999_999;

fn part_name(p: i64) -> &'static str {
    match p {
        YEAR => "YEAR",
        MONTH => "MONTH",
        DAY => "DAY",
        DAYOFWEEK => "DAYOFWEEK",
        DAYOFYEAR => "DAYOFYEAR",
        QUARTER => "QUARTER",
        HOUR => "HOUR",
        MINUTE => "MINUTE",
        SECOND => "SECOND",
        MILLISECOND => "MILLISECOND",
        MICROSECOND => "MICROSECOND",
        NANOSECOND => "NANOSECOND",
        DATE => "DATE",
        WEEK => "WEEK",
        ISOYEAR => "ISOYEAR",
        ISOWEEK => "ISOWEEK",
        19 => "WEEK(MONDAY)",
        20 => "WEEK(TUESDAY)",
        21 => "WEEK(WEDNESDAY)",
        22 => "WEEK(THURSDAY)",
        23 => "WEEK(FRIDAY)",
        24 => "WEEK(SATURDAY)",
        _ => "UNKNOWN",
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int64(x) => *x,
        _ => 0,
    }
}

fn tsv(v: &Value) -> Timestamp {
    match v {
        Value::Timestamp(t) => *t,
        _ => Timestamp::default(),
    }
}

fn datev(v: &Value) -> Date {
    match v {
        Value::Date(d) => *d,
        _ => Date(0),
    }
}

fn text(v: &Value) -> &str {
    match v {
        Value::String(s) => s,
        _ => "",
    }
}

fn zone_arg(a: &[Value], k: usize) -> Result<Zone> {
    match a.get(k) {
        Some(v) => Zone::parse(text(v)),
        None => Ok(Zone::default()),
    }
}

fn nanos_of(t: Timestamp) -> i128 {
    t.as_nanos()
}

fn ts_from_nanos(n: i128) -> Result<Timestamp> {
    if !(MIN_NANOS..=MAX_NANOS).contains(&n) {
        return Err(out_of_range(
            "Unix picoseconds value is out of allowed range between -62135596800000000000000 to 253402300799999999999999",
        ));
    }
    Ok(Timestamp::from_nanos(n))
}

/// The first day of the week containing `d` (weeks start on `start`).
fn week_start(d: NaiveDate, start: Weekday) -> i64 {
    let back = (d.weekday().num_days_from_monday() as i64 - start.num_days_from_monday() as i64)
        .rem_euclid(7);
    days(d) - back
}

fn days(d: NaiveDate) -> i64 {
    from_naive_date(d).0 as i64
}

fn week_day(p: i64) -> Option<Weekday> {
    Some(match p {
        WEEK => Weekday::Sun,
        ISOWEEK => Weekday::Mon,
        19 => Weekday::Mon,
        20 => Weekday::Tue,
        21 => Weekday::Wed,
        22 => Weekday::Thu,
        23 => Weekday::Fri,
        24 => Weekday::Sat,
        _ => return None,
    })
}

fn iso_year_start(y: i32) -> Option<NaiveDate> {
    NaiveDate::from_isoywd_opt(y, 1, Weekday::Mon)
}

/// Truncates a civil date to `part`; `None` for parts finer than a day.
fn trunc_date(d: NaiveDate, part: i64) -> Option<i64> {
    Some(match part {
        DAY => days(d),
        YEAR => days(NaiveDate::from_ymd_opt(d.year(), 1, 1)?),
        MONTH => days(NaiveDate::from_ymd_opt(d.year(), d.month(), 1)?),
        QUARTER => days(NaiveDate::from_ymd_opt(
            d.year(),
            (d.month() - 1) / 3 * 3 + 1,
            1,
        )?),
        ISOYEAR => days(iso_year_start(d.iso_week().year())?),
        p => week_start(d, week_day(p)?),
    })
}

fn date_value(d: i64) -> Option<Value> {
    (MIN_DAYS as i64..=MAX_DAYS as i64)
        .contains(&d)
        .then_some(Value::Date(Date(d as i32)))
}

fn extract_civil(n: NaiveDateTime, nanos: u32, part: i64) -> Option<Value> {
    let d = n.date();
    Some(Value::Int64(match part {
        YEAR => d.year() as i64,
        MONTH => d.month() as i64,
        DAY => d.day() as i64,
        DAYOFWEEK => d.weekday().num_days_from_sunday() as i64 + 1,
        DAYOFYEAR => d.ordinal() as i64,
        QUARTER => (d.month() as i64 - 1) / 3 + 1,
        HOUR => n.hour() as i64,
        MINUTE => n.minute() as i64,
        SECOND => n.second() as i64,
        MILLISECOND => (nanos / 1_000_000) as i64,
        MICROSECOND => (nanos / 1000) as i64,
        NANOSECOND => nanos as i64,
        DATE => return date_value(days(d)),
        ISOYEAR => d.iso_week().year() as i64,
        ISOWEEK => d.iso_week().week() as i64,
        p => {
            let start = week_day(p)?;
            let jan1 = NaiveDate::from_ymd_opt(d.year(), 1, 1)?;
            let first = week_start(jan1, start);
            let first = if first < days(jan1) { first + 7 } else { first };
            (days(d) - first).div_euclid(7) + 1
        }
    }))
}

fn interval_field(name: &str, v: i128, max: i128) -> Result<i128> {
    if v.abs() > max {
        return Err(out_of_range(format!(
            "Interval field {name} '{v}' is out of range -{max} to {max}"
        )));
    }
    Ok(v)
}

const MAX_MONTHS: i128 = 120_000;
const MAX_IDAYS: i128 = 3_660_000;
const MAX_INANOS: i128 = 316_224_000_000_000_000_000;

fn make_interval(a: &[Value]) -> Result<Value> {
    let v = int(&a[0]) as i128;
    let mut iv = spanbolt_storage::Interval::default();
    match int(&a[1]) {
        YEAR => iv.months = interval_field("months", v * 12, MAX_MONTHS)? as i32,
        QUARTER => iv.months = interval_field("months", v * 3, MAX_MONTHS)? as i32,
        MONTH => iv.months = interval_field("months", v, MAX_MONTHS)? as i32,
        WEEK => iv.days = interval_field("days", v * 7, MAX_IDAYS)? as i32,
        DAY => iv.days = interval_field("days", v, MAX_IDAYS)? as i32,
        p => {
            let unit = unit_nanos(p)
                .ok_or_else(|| crate::types::unimplemented("INTERVAL of this date part"))?;
            iv.nanos = interval_field("nanoseconds", v * unit, MAX_INANOS)?;
        }
    }
    Ok(Value::Interval(iv))
}

/// GoogleSQL's `IntervalValue::FromMonthsDaysNanos`: validates each field.
fn interval_of(months: i64, days: i64, nanos: i128) -> Result<Value> {
    let months = interval_field("months", months as i128, MAX_MONTHS)?;
    let days = interval_field("days", days as i128, MAX_IDAYS)?;
    let nanos = interval_field("nanoseconds", nanos, MAX_INANOS)?;
    Ok(Value::Interval(spanbolt_storage::Interval {
        months: months as i32,
        days: days as i32,
        nanos,
    }))
}

/// MAKE_INTERVAL([year], [month], [day], [hour], [minute], [second]) (`FromYMDHMS`).
fn make_interval_ymdhms(a: &[Value]) -> Result<Value> {
    let arg = |i: usize| a.get(i).map_or(0, int);
    let (years, months) = (arg(0), arg(1));
    let year_months = years
        .checked_mul(12)
        .ok_or_else(|| out_of_range(format!("int64 overflow: {years} * 12")))?;
    let months = months
        .checked_add(year_months)
        .ok_or_else(|| out_of_range(format!("int64 overflow: {months} + {year_months}")))?;
    let nanos =
        3600 * NANOS * arg(3) as i128 + 60 * NANOS * arg(4) as i128 + NANOS * arg(5) as i128;
    interval_of(months, arg(2), nanos)
}

const NANOS_IN_DAY: i128 = 86_400 * NANOS;
const DAYS_IN_MONTH: i64 = 30;

fn justify_hours(iv: &spanbolt_storage::Interval) -> Result<Value> {
    let mut days = iv.days as i64 + (iv.nanos / NANOS_IN_DAY) as i64;
    let mut nanos = iv.nanos % NANOS_IN_DAY;
    if days > 0 && nanos < 0 {
        nanos += NANOS_IN_DAY;
        days -= 1;
    } else if days < 0 && nanos > 0 {
        nanos -= NANOS_IN_DAY;
        days += 1;
    }
    interval_of(iv.months as i64, days, nanos)
}

fn justify_days(iv: &spanbolt_storage::Interval) -> Result<Value> {
    let mut months = iv.months as i64 + iv.days as i64 / DAYS_IN_MONTH;
    let mut days = iv.days as i64 % DAYS_IN_MONTH;
    if months > 0 && days < 0 {
        days += DAYS_IN_MONTH;
        months -= 1;
    } else if months < 0 && days > 0 {
        days -= DAYS_IN_MONTH;
        months += 1;
    }
    interval_of(months, days, iv.nanos)
}

fn justify_interval(iv: &spanbolt_storage::Interval) -> Result<Value> {
    let mut nanos = iv.nanos;
    let mut days = iv.days as i64 + (nanos / NANOS_IN_DAY) as i64;
    nanos %= NANOS_IN_DAY;
    let mut months = iv.months as i64 + days / DAYS_IN_MONTH;
    days %= DAYS_IN_MONTH;
    // Postgres' rule: all parts end up with the same sign.
    if months > 0 && (days < 0 || (days == 0 && nanos < 0)) {
        days += DAYS_IN_MONTH;
        months -= 1;
    } else if months < 0 && (days > 0 || (days == 0 && nanos > 0)) {
        days -= DAYS_IN_MONTH;
        months += 1;
    }
    if days > 0 && nanos < 0 {
        nanos += NANOS_IN_DAY;
        days -= 1;
    } else if days < 0 && nanos > 0 {
        nanos -= NANOS_IN_DAY;
        days += 1;
    }
    interval_of(months, days, nanos)
}

fn interval_fn(g: fn(&spanbolt_storage::Interval) -> Result<Value>) -> ScalarFn {
    strict(move |a| match &a[0] {
        Value::Interval(iv) => g(iv),
        _ => Ok(Value::Null),
    })
}

fn extract_interval(iv: &spanbolt_storage::Interval, part: i64) -> Option<i64> {
    let n = iv.nanos;
    Some(match part {
        YEAR => (iv.months / 12) as i64,
        MONTH => (iv.months % 12) as i64,
        DAY => iv.days as i64,
        HOUR => (n / (3600 * NANOS)) as i64,
        MINUTE => ((n / (60 * NANOS)) % 60) as i64,
        SECOND => ((n / NANOS) % 60) as i64,
        MILLISECOND => ((n / 1_000_000) % 1000) as i64,
        MICROSECOND => ((n / 1000) % 1_000_000) as i64,
        NANOSECOND => (n % NANOS) as i64,
        _ => return None,
    })
}

fn extract(sig: &Sig) -> Option<ScalarFn> {
    match sig.args.first()? {
        Type::Interval => Some(strict(|a| match &a[0] {
            Value::Interval(iv) => extract_interval(iv, int(&a[1]))
                .map(Value::Int64)
                .ok_or_else(|| crate::types::unimplemented("EXTRACT of this part from INTERVAL")),
            _ => Ok(Value::Null),
        })),
        Type::Timestamp => Some(strict(|a| {
            let t = tsv(&a[0]);
            let zone = zone_arg(a, 2)?;
            if int(&a[1]) == DATE {
                return date_of(t, zone);
            }
            extract_civil(zone.local(t), t.nanos, int(&a[1]))
                .ok_or_else(|| crate::types::unimplemented("EXTRACT of this date part"))
        })),
        Type::Date => Some(strict(|a| {
            let n = naive_date(datev(&a[0]))
                .and_hms_opt(0, 0, 0)
                .unwrap_or_default();
            match int(&a[1]) {
                HOUR | MINUTE | SECOND | MILLISECOND | MICROSECOND | NANOSECOND => Err(
                    crate::types::unimplemented("EXTRACT of a time part from DATE"),
                ),
                p => extract_civil(n, 0, p)
                    .ok_or_else(|| crate::types::unimplemented("EXTRACT of this date part")),
            }
        })),
        _ => None,
    }
}

fn unit_nanos(part: i64) -> Option<i128> {
    Some(match part {
        NANOSECOND => 1,
        MICROSECOND => 1000,
        MILLISECOND => 1_000_000,
        SECOND => NANOS,
        MINUTE => 60 * NANOS,
        HOUR => 3600 * NANOS,
        DAY => 86_400 * NANOS,
        _ => return None,
    })
}

fn timestamp_add(neg: bool) -> ScalarFn {
    strict(move |a| {
        let t = tsv(&a[0]);
        let part = int(&a[2]);
        let unit = unit_nanos(part)
            .ok_or_else(|| crate::types::unimplemented("TIMESTAMP_ADD of this date part"))?;
        let v = int(&a[1]) as i128;
        let v = if neg { -v } else { v };
        if v.abs() > (MAX_NANOS - MIN_NANOS) / unit {
            return Err(out_of_range(format!(
                "TIMESTAMP_ADD interval value {v} at {} exceeds allowed range",
                part_name(part)
            )));
        }
        Ok(Value::Timestamp(ts_from_nanos(nanos_of(t) + v * unit)?))
    })
}

fn timestamp_diff() -> ScalarFn {
    strict(|a| {
        let part = int(&a[2]);
        let unit = unit_nanos(part)
            .ok_or_else(|| crate::types::unimplemented("TIMESTAMP_DIFF of this date part"))?;
        let d = (nanos_of(tsv(&a[0])) - nanos_of(tsv(&a[1]))) / unit;
        i64::try_from(d)
            .map(Value::Int64)
            .map_err(|_| out_of_range("TIMESTAMP_DIFF int64 overflow"))
    })
}

fn timestamp_trunc(a: &[Value]) -> Result<Value> {
    {
        let t = tsv(&a[0]);
        let part = int(&a[1]);
        let zone = zone_arg(a, 2)?;
        let n = nanos_of(t);
        let local = zone.local(t);
        let sub = |unit: i128| -> Result<Value> {
            let into = (local.time().num_seconds_from_midnight() as i128) * NANOS + t.nanos as i128;
            let r = n - into.rem_euclid(unit);
            Ok(Value::Timestamp(ts_from_nanos(r)?))
        };
        match part {
            NANOSECOND => Ok(Value::Timestamp(t)),
            MICROSECOND | MILLISECOND | SECOND | MINUTE | HOUR => {
                sub(unit_nanos(part).unwrap_or(1))
            }
            _ => {
                let d = trunc_date(local.date(), part)
                    .ok_or_else(|| crate::types::unimplemented("TIMESTAMP_TRUNC of this part"))?;
                let midnight = NaiveDate::from_num_days_from_ce_opt((d + 719_163) as i32)
                    .and_then(|x| x.and_hms_opt(0, 0, 0));
                let r = midnight.map(|m| zone.to_utc(m));
                match r {
                    Some(r) if r.secs >= MIN_SECS => Ok(Value::Timestamp(r)),
                    _ if week_day(part).is_some() => Err(out_of_range(format!(
                        "Truncating {} to the nearest week causes overflow",
                        zone.format_ts(t)
                    ))),
                    _ => Err(out_of_range(format!(
                        "Truncating to the nearest {} causes underflow at timezone {}",
                        part_name(part),
                        zone.name()
                    ))),
                }
            }
        }
    }
}

fn add_months(d: NaiveDate, months: i64) -> Option<NaiveDate> {
    let total = d.year() as i64 * 12 + d.month0() as i64 + months;
    let y = i32::try_from(total.div_euclid(12)).ok()?;
    let m = total.rem_euclid(12) as u32 + 1;
    let last = (28..=31)
        .rev()
        .find(|&dd| NaiveDate::from_ymd_opt(y, m, dd).is_some())?;
    NaiveDate::from_ymd_opt(y, m, d.day().min(last))
}

fn date_add(neg: bool) -> ScalarFn {
    strict(move |a| {
        let d = datev(&a[0]);
        let raw = int(&a[1]);
        let part = int(&a[2]);
        let v = if neg { raw.checked_neg() } else { Some(raw) };
        let overflow = || {
            let (verb, shown, prep) = match (neg, raw.checked_neg()) {
                (true, None) => ("Subtracting", raw, "from"),
                (true, Some(n)) => ("Adding", n, "to"),
                (false, _) => ("Adding", raw, "to"),
            };
            out_of_range(format!(
                "{verb} {shown} {} {prep} date {} causes overflow",
                part_name(part),
                time::format_date(d)
            ))
        };
        let v = v.ok_or_else(overflow)?;
        let nd = naive_date(d);
        let r = match part {
            DAY => v.checked_add(d.0 as i64),
            WEEK => v.checked_mul(7).and_then(|x| x.checked_add(d.0 as i64)),
            MONTH => add_months(nd, v).map(days),
            QUARTER => v.checked_mul(3).and_then(|m| add_months(nd, m)).map(days),
            YEAR => v.checked_mul(12).and_then(|m| add_months(nd, m)).map(days),
            _ => return Err(crate::types::unimplemented("DATE_ADD of this date part")),
        };
        r.and_then(date_value).ok_or_else(overflow)
    })
}

fn date_diff() -> ScalarFn {
    strict(|a| {
        let (x, y) = (naive_date(datev(&a[0])), naive_date(datev(&a[1])));
        let part = int(&a[2]);
        let ym = |d: NaiveDate| d.year() as i64 * 12 + d.month0() as i64;
        Ok(Value::Int64(match part {
            DAY => days(x) - days(y),
            MONTH => ym(x) - ym(y),
            QUARTER => ym(x).div_euclid(3) - ym(y).div_euclid(3),
            YEAR => x.year() as i64 - y.year() as i64,
            ISOYEAR => x.iso_week().year() as i64 - y.iso_week().year() as i64,
            p => match week_day(p) {
                Some(w) => (week_start(x, w) - week_start(y, w)) / 7,
                None => return Err(crate::types::unimplemented("DATE_DIFF of this date part")),
            },
        }))
    })
}

fn date_trunc() -> ScalarFn {
    strict(|a| {
        let d = datev(&a[0]);
        let part = int(&a[1]);
        let r = trunc_date(naive_date(d), part)
            .ok_or_else(|| crate::types::unimplemented("DATE_TRUNC of this date part"))?;
        date_value(r).ok_or_else(|| {
            out_of_range(format!(
                "Truncating date {} to {} resulted in an out of range date value: {r}",
                time::format_date(d),
                part_name(part)
            ))
        })
    })
}

/// The civil date of `t` in `zone`, which must be a valid DATE.
fn date_of(t: Timestamp, zone: Zone) -> Result<Value> {
    date_value(days(zone.local(t).date())).ok_or_else(|| {
        out_of_range(format!(
            "Invalid date extracted from timestamp {}",
            zone.format_ts(t)
        ))
    })
}

fn micros_to_ts(v: i64, scale: i64) -> Result<Value> {
    let err = || {
        out_of_range(format!(
            "Input value {v} cannot be converted into a TIMESTAMP, because it would be out of the allowed range between -62135596800000000 to 253402300799999999 (microseconds)"
        ))
    };
    let micros = v.checked_mul(scale).ok_or_else(err)?;
    if !(MIN_SECS * 1_000_000..=MAX_SECS * 1_000_000 + 999_999).contains(&micros) {
        return Err(err());
    }
    Ok(Value::Timestamp(Timestamp::from_micros(micros)))
}

fn constructors(sig: &Sig) -> Option<ScalarFn> {
    let args: Vec<&Type> = sig.args.iter().collect();
    Some(match (sig.name, args.as_slice()) {
        ("date", [Type::Timestamp] | [Type::Timestamp, Type::String(_)]) => strict(|a| {
            let zone = zone_arg(a, 1)?;
            date_of(tsv(&a[0]), zone)
        }),
        ("date", [Type::Int64, Type::Int64, Type::Int64]) => strict(|a| {
            let (y, m, d) = (int(&a[0]), int(&a[1]), int(&a[2]));
            let r = i32::try_from(y)
                .ok()
                .filter(|y| (1..=9999).contains(y))
                .and_then(|y| {
                    NaiveDate::from_ymd_opt(y, u32::try_from(m).ok()?, u32::try_from(d).ok()?)
                });
            r.map(|x| Value::Date(from_naive_date(x))).ok_or_else(|| {
                out_of_range(format!(
                    "Input calculates to invalid date: {y:04}-{m:02}-{d:02}"
                ))
            })
        }),
        ("date", [Type::String(_)]) => strict(|a| {
            let s = text(&a[0]);
            Date::parse(s.trim())
                .map(Value::Date)
                .ok_or_else(|| out_of_range(format!("Invalid date: '{s}'")))
        }),
        ("timestamp", [Type::String(_)] | [Type::String(_), Type::String(_)]) => strict(|a| {
            let s = text(&a[0]);
            let zone = zone_arg(a, 1)?;
            match time::parse_ts_parts(s, zone) {
                Some((_, true)) if a.len() > 1 => {
                    Err(out_of_range(format!("Timezone is not allowed in \"{s}\"")))
                }
                Some((t, _)) if t.secs < MIN_SECS || t.secs > MAX_SECS => Err(out_of_range(
                    format!("Invalid timestamp string \"{s}\" (scale TIMESTAMP_NANOSECOND)"),
                )),
                Some((t, _)) => Ok(Value::Timestamp(t)),
                None => Err(out_of_range(format!("Invalid timestamp: '{s}'"))),
            }
        }),
        ("timestamp", [Type::Date] | [Type::Date, Type::String(_)]) => strict(|a| {
            let zone = zone_arg(a, 1)?;
            let m = naive_date(datev(&a[0]))
                .and_hms_opt(0, 0, 0)
                .unwrap_or_default();
            let t = zone.to_utc(m);
            if t.secs < MIN_SECS || t.secs > MAX_SECS {
                return Err(out_of_range("Timestamp overflow"));
            }
            Ok(Value::Timestamp(t))
        }),
        _ => return None,
    })
}

pub fn lookup(sig: &Sig) -> Option<ScalarFn> {
    Some(match sig.name {
        "current_timestamp" => f(|cx, _| Ok(Value::Timestamp(cx.now))),
        "current_date" => f(|cx, a| {
            if a.first().is_some_and(Value::is_null) {
                return Ok(Value::Null);
            }
            let zone = zone_arg(a, 0)?;
            Ok(Value::Date(from_naive_date(zone.local(cx.now).date())))
        }),
        "$extract" => extract(sig)?,
        "$interval" => strict(make_interval),
        "make_interval" => strict(make_interval_ymdhms),
        "justify_hours" => interval_fn(justify_hours),
        "justify_days" => interval_fn(justify_days),
        "justify_interval" => interval_fn(justify_interval),
        "$extract_date" => strict(|a| {
            let zone = zone_arg(a, 1)?;
            date_of(tsv(&a[0]), zone)
        }),
        "timestamp_add" => timestamp_add(false),
        "timestamp_sub" => timestamp_add(true),
        "timestamp_diff" => timestamp_diff(),
        "timestamp_trunc" => strict(timestamp_trunc),
        "date_add" => date_add(false),
        "date_sub" => date_add(true),
        "date_diff" => date_diff(),
        "date_trunc" => date_trunc(),
        "date" | "timestamp" => constructors(sig)?,
        "unix_seconds" => strict(|a| Ok(Value::Int64(tsv(&a[0]).secs))),
        "unix_millis" => strict(|a| {
            let n = nanos_of(tsv(&a[0]));
            Ok(Value::Int64(n.div_euclid(1_000_000) as i64))
        }),
        "unix_micros" => strict(|a| {
            let n = nanos_of(tsv(&a[0]));
            Ok(Value::Int64(n.div_euclid(1000) as i64))
        }),
        "unix_date" => strict(|a| Ok(Value::Int64(datev(&a[0]).0 as i64))),
        "timestamp_seconds" => strict(|a| micros_to_ts(int(&a[0]), 1_000_000)),
        "timestamp_millis" => strict(|a| micros_to_ts(int(&a[0]), 1000)),
        "timestamp_micros" => strict(|a| micros_to_ts(int(&a[0]), 1)),
        "date_from_unix_date" => strict(|a| {
            let v = int(&a[0]);
            date_value(v).ok_or_else(|| {
                out_of_range(format!(
                    "DATE_FROM_UNIX_DATE range is {MIN_DAYS} to {MAX_DAYS} but saw {v}"
                ))
            })
        }),
        "format_timestamp" => strict(|a| {
            let t = tsv(&a[1]);
            let zone = zone_arg(a, 2)?;
            Ok(Value::String(
                format_time(text(&a[0]), &Civil::of_ts(t, zone), false).into(),
            ))
        }),
        "format_date" => strict(|a| {
            let c = Civil::of_date(datev(&a[1]));
            Ok(Value::String(format_time(text(&a[0]), &c, true).into()))
        }),
        "parse_timestamp" => strict(|a| {
            let zone = zone_arg(a, 2)?;
            parse_time(text(&a[0]), text(&a[1]), zone, false)
        }),
        "parse_date" => strict(|a| parse_time(text(&a[0]), text(&a[1]), Zone::Fixed(0), true)),
        _ => return None,
    })
}

// ---- FORMAT_TIMESTAMP / FORMAT_DATE

struct Civil {
    n: NaiveDateTime,
    nanos: u32,
    offset: i32,
    epoch: i64,
}

impl Civil {
    fn of_ts(t: Timestamp, zone: Zone) -> Civil {
        Civil {
            n: zone.local_fmt(t),
            nanos: t.nanos,
            offset: zone.fmt_offset(t),
            epoch: t.secs,
        }
    }

    fn of_date(d: Date) -> Civil {
        Civil {
            n: naive_date(d).and_hms_opt(0, 0, 0).unwrap_or_default(),
            nanos: 0,
            offset: 0,
            epoch: d.0 as i64 * 86_400,
        }
    }
}

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn offset_text(off: i32, colon: bool, secs: bool) -> String {
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.unsigned_abs();
    let (h, m, s) = (a / 3600, (a / 60) % 60, a % 60);
    match (colon, secs) {
        (_, true) => format!("{sign}{h:02}:{m:02}:{s:02}"),
        (true, false) => format!("{sign}{h:02}:{m:02}"),
        _ => format!("{sign}{h:02}{m:02}"),
    }
}

fn zone_abbrev(off: i32) -> String {
    if off == 0 {
        return "UTC".into();
    }
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.unsigned_abs();
    let (h, m) = (a / 3600, (a / 60) % 60);
    if m == 0 {
        format!("UTC{sign}{h}")
    } else {
        format!("UTC{sign}{h:02}{m:02}")
    }
}

fn frac(nanos: u32, digits: usize) -> String {
    let f = format!("{nanos:09}");
    f[..digits.min(9)].to_string()
}

/// Expands one element; `None` leaves it in the output as written.
fn element(c: &Civil, spec: &str, date_only: bool) -> Option<String> {
    let n = &c.n;
    let d = n.date();
    let wd = d.weekday().num_days_from_sunday() as usize;
    let hour12 = match n.hour() % 12 {
        0 => 12,
        h => h,
    };
    let time_part = matches!(
        spec,
        "H" | "I"
            | "k"
            | "l"
            | "M"
            | "S"
            | "p"
            | "P"
            | "s"
            | "Z"
            | "z"
            | "T"
            | "R"
            | "r"
            | "X"
            | "c"
            | "Ez"
            | "E*z"
            | ":z"
            | "E*S"
            | "OH"
            | "OI"
            | "OM"
            | "OS"
            | "EX"
            | "Ec"
    ) || (spec.starts_with('E') && spec.ends_with('S'));
    if date_only && time_part {
        return None;
    }
    let ydays = d.ordinal0() as i64;
    let week = |start_sunday: bool| {
        let w = if start_sunday {
            wd as i64
        } else {
            d.weekday().num_days_from_monday() as i64
        };
        (ydays + 7 - w) / 7
    };
    Some(match spec {
        "Y" | "EY" => d.year().to_string(),
        "E4Y" => format!("{:04}", d.year()),
        "C" | "EC" => d.year().div_euclid(100).to_string(),
        "y" | "Ey" | "Oy" => format!("{:02}", d.year().rem_euclid(100)),
        "G" => d.iso_week().year().to_string(),
        "g" => format!("{:02}", d.iso_week().year().rem_euclid(100)),
        "V" | "OV" => format!("{:02}", d.iso_week().week()),
        "U" | "OU" => format!("{:02}", week(true)),
        "W" | "OW" => format!("{:02}", week(false)),
        "m" | "Om" => format!("{:02}", d.month()),
        "d" | "Od" => format!("{:02}", d.day()),
        "e" | "Oe" => format!("{:2}", d.day()),
        "j" => format!("{:03}", d.ordinal()),
        "a" => DAYS[wd][..3].to_string(),
        "A" => DAYS[wd].to_string(),
        "b" | "h" => MONTHS[d.month0() as usize][..3].to_string(),
        "B" => MONTHS[d.month0() as usize].to_string(),
        "u" | "Ou" => (d.weekday().num_days_from_monday() + 1).to_string(),
        "w" | "Ow" => wd.to_string(),
        "Q" => ((d.month0() / 3) + 1).to_string(),
        "D" | "x" | "Ex" => format!(
            "{:02}/{:02}/{:02}",
            d.month(),
            d.day(),
            d.year().rem_euclid(100)
        ),
        "F" => format!("{}-{:02}-{:02}", d.year(), d.month(), d.day()),
        "H" | "OH" => format!("{:02}", n.hour()),
        "k" => format!("{:2}", n.hour()),
        "I" | "OI" => format!("{hour12:02}"),
        "l" => format!("{hour12:2}"),
        "M" | "OM" => format!("{:02}", n.minute()),
        "S" | "OS" => format!("{:02}", n.second()),
        "p" => (if n.hour() < 12 { "AM" } else { "PM" }).to_string(),
        "P" => (if n.hour() < 12 { "am" } else { "pm" }).to_string(),
        "s" => c.epoch.to_string(),
        "T" | "X" | "EX" => format!("{:02}:{:02}:{:02}", n.hour(), n.minute(), n.second()),
        "R" => format!("{:02}:{:02}", n.hour(), n.minute()),
        "r" => format!(
            "{hour12:02}:{:02}:{:02} {}",
            n.minute(),
            n.second(),
            if n.hour() < 12 { "AM" } else { "PM" }
        ),
        "c" | "Ec" => format!(
            "{} {} {:2} {:02}:{:02}:{:02} {}",
            &DAYS[wd][..3],
            &MONTHS[d.month0() as usize][..3],
            d.day(),
            n.hour(),
            n.minute(),
            n.second(),
            d.year()
        ),
        "z" => offset_text(c.offset, false, false),
        "Ez" | ":z" => offset_text(c.offset, true, false),
        "E*z" => offset_text(c.offset, true, true),
        "Z" => zone_abbrev(c.offset),
        "n" => "\n".into(),
        "t" => "\t".into(),
        "E*S" => {
            let mut s = format!("{:02}", n.second());
            if c.nanos != 0 {
                let f = format!("{:09}", c.nanos);
                s.push('.');
                s.push_str(f.trim_end_matches('0'));
            }
            s
        }
        _ => {
            let digits = spec.strip_prefix('E')?.strip_suffix('S')?;
            let k: usize = digits.parse().ok().filter(|k| *k <= 9)?;
            let mut s = format!("{:02}", n.second());
            if k > 0 {
                s.push('.');
                s.push_str(&frac(c.nanos, k));
            }
            s
        }
    })
}

/// Splits `%...` at `k` (just after the `%`) into the element spec and its length.
fn spec_at(f: &str) -> &str {
    let b = f.as_bytes();
    match b.first() {
        Some(b'E') => {
            if f.starts_with("E*S") || f.starts_with("E*z") {
                &f[..3]
            } else {
                let digits = b[1..].iter().take_while(|c| c.is_ascii_digit()).count();
                let end = (1 + digits + 1).min(f.len());
                if f.is_char_boundary(end) {
                    &f[..end]
                } else {
                    &f[..1]
                }
            }
        }
        Some(b'O') | Some(b':') => {
            if f.len() >= 2 && f.is_char_boundary(2) {
                &f[..2]
            } else {
                &f[..1]
            }
        }
        Some(_) => {
            let n = f.chars().next().map_or(1, char::len_utf8);
            &f[..n]
        }
        None => "",
    }
}

fn format_time(fmt: &str, c: &Civil, date_only: bool) -> String {
    let mut out = String::with_capacity(fmt.len() + 16);
    let mut rest = fmt;
    while let Some(k) = rest.find('%') {
        out.push_str(&rest[..k]);
        let after = &rest[k + 1..];
        if after.is_empty() {
            out.push('%');
            rest = after;
            break;
        }
        if let Some(r) = after.strip_prefix('%') {
            out.push('%');
            rest = r;
            continue;
        }
        let spec = spec_at(after);
        match element(c, spec, date_only) {
            Some(s) => out.push_str(&s),
            None => {
                out.push('%');
                out.push_str(spec);
            }
        }
        rest = &after[spec.len()..];
    }
    out.push_str(rest);
    out
}

// ---- PARSE_TIMESTAMP / PARSE_DATE

#[derive(Default)]
struct Fields {
    year: Option<i64>,
    century: Option<i64>,
    yy: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
    yday: Option<i64>,
    hour: Option<i64>,
    hour12: Option<i64>,
    pm: Option<bool>,
    minute: Option<i64>,
    second: Option<i64>,
    nanos: u32,
    epoch: Option<i64>,
    zone: Option<Zone>,
    weekday: Option<i64>,
    week_u: Option<i64>,
    week_w: Option<i64>,
    quarter: Option<i64>,
}

struct Input<'a> {
    s: &'a str,
    orig: &'a str,
    fmt: &'a str,
}

impl<'a> Input<'a> {
    fn skip_ws(&mut self) {
        self.s = self.s.trim_start();
    }

    fn num(&mut self, max_digits: usize, signed: bool) -> Option<i64> {
        let mut k = 0;
        let b = self.s.as_bytes();
        let neg = signed && b.first() == Some(&b'-');
        if signed && matches!(b.first(), Some(b'-') | Some(b'+')) {
            k = 1;
        }
        let start = k;
        while k < b.len() && k - start < max_digits && b[k].is_ascii_digit() {
            k += 1;
        }
        if k == start {
            return None;
        }
        let v: i64 = self.s[start..k].parse().ok()?;
        self.s = &self.s[k..];
        Some(if neg { -v } else { v })
    }

    fn range(&mut self, max_digits: usize, lo: i64, hi: i64) -> Option<i64> {
        let v = self.num(max_digits, false)?;
        (lo..=hi).contains(&v).then_some(v)
    }

    fn name(&mut self, names: &[&str]) -> Option<i64> {
        let lower = self.s.to_ascii_lowercase();
        for (k, full) in names.iter().enumerate() {
            let fl = full.to_ascii_lowercase();
            if lower.starts_with(&fl) {
                self.s = &self.s[fl.len()..];
                return Some(k as i64);
            }
        }
        for (k, full) in names.iter().enumerate() {
            let ab = full[..3].to_ascii_lowercase();
            if lower.starts_with(&ab) {
                self.s = &self.s[3..];
                return Some(k as i64);
            }
        }
        None
    }

    fn lit(&mut self, c: char) -> Option<()> {
        self.s = self.s.strip_prefix(c)?;
        Some(())
    }

    fn offset(&mut self, colon_ok: bool) -> Option<i32> {
        if let Some(r) = self.s.strip_prefix(['Z', 'z']) {
            self.s = r;
            return Some(0);
        }
        let sign = match self.s.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        self.s = &self.s[1..];
        let h = self.num(2, false)?;
        let mut m = 0;
        if colon_ok && self.s.starts_with(':') {
            self.s = &self.s[1..];
            m = self.num(2, false)?;
        } else if self.s.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            m = self.num(2, false)?;
        }
        if h > 24 || m > 59 {
            return None;
        }
        Some(sign * (h as i32 * 3600 + m as i32 * 60))
    }

    fn seconds_frac(&mut self, max_frac: Option<usize>) -> Option<(i64, u32)> {
        let s = self.range(2, 0, 60)?;
        let mut nanos = 0;
        if let Some(r) = self.s.strip_prefix('.') {
            let limit = max_frac.unwrap_or(usize::MAX);
            let n = r.bytes().take_while(u8::is_ascii_digit).count();
            if n == 0 || limit == 0 {
                return Some((s, 0));
            }
            let take = n.min(limit);
            let digits = &r[..take.min(9)];
            nanos = format!("{digits:0<9}").parse().ok()?;
            self.s = &r[take..];
        }
        Some((s, nanos))
    }
}

fn parse_elements(fmt: &str, inp: &mut Input, fl: &mut Fields, date_only: bool) -> Result<bool> {
    let mut rest = fmt;
    let fail = Ok(false);
    while !rest.is_empty() {
        let Some(ch) = rest.chars().next() else { break };
        if ch.is_whitespace() {
            inp.skip_ws();
            rest = &rest[ch.len_utf8()..];
            continue;
        }
        if ch != '%' {
            if inp.lit(ch).is_none() {
                if let Some(got) = inp.s.chars().next() {
                    return Err(out_of_range(format!(
                        "Mismatch between format character '{ch}' and string character '{got}' at string index: {} with format: '{}'",
                        inp.orig.len() - inp.s.len(),
                        inp.fmt
                    )));
                }
                return fail;
            }
            rest = &rest[ch.len_utf8()..];
            continue;
        }
        let after = &rest[1..];
        if after.is_empty() {
            if inp.lit('%').is_none() {
                return fail;
            }
            break;
        }
        let spec = spec_at(after);
        rest = &after[spec.len()..];
        let time_part = matches!(
            spec,
            "H" | "I"
                | "k"
                | "l"
                | "M"
                | "S"
                | "p"
                | "P"
                | "s"
                | "Z"
                | "z"
                | "T"
                | "R"
                | "r"
                | "X"
                | "c"
                | "Ez"
                | "E*z"
                | ":z"
                | "E*S"
                | "OH"
                | "OI"
                | "OM"
                | "OS"
        ) || (spec.starts_with('E') && spec.ends_with('S'));
        if date_only && time_part {
            return Err(out_of_range(format!(
                "Invalid format: %{spec} is not allowed for the DATE type."
            )));
        }
        let ok = match spec {
            "%" => inp.lit('%'),
            "n" | "t" => {
                inp.skip_ws();
                Some(())
            }
            "Y" | "EY" | "E4Y" => {
                let width = if rest.starts_with('%') { 4 } else { 5 };
                inp.num(width, false).map(|v| fl.year = Some(v))
            }
            "C" | "EC" => inp.num(2, true).map(|v| fl.century = Some(v)),
            "y" | "Ey" | "Oy" => inp.range(2, 0, 99).map(|v| fl.yy = Some(v)),
            "G" | "g" | "V" | "OV" => {
                return Err(out_of_range("ISO parse elements are not supported yet"));
            }
            "U" | "OU" => inp.range(2, 0, 53).map(|v| fl.week_u = Some(v)),
            "W" | "OW" => inp.range(2, 0, 53).map(|v| fl.week_w = Some(v)),
            "m" | "Om" => inp.range(2, 1, 12).map(|v| fl.month = Some(v)),
            "d" | "Od" => inp.range(2, 1, 31).map(|v| fl.day = Some(v)),
            "e" | "Oe" => {
                inp.skip_ws();
                inp.range(2, 1, 31).map(|v| fl.day = Some(v))
            }
            "j" => inp.range(3, 1, 366).map(|v| fl.yday = Some(v)),
            "a" | "A" => inp.name(&DAYS).map(|v| fl.weekday = Some(v)),
            "b" | "B" | "h" => inp.name(&MONTHS).map(|v| fl.month = Some(v + 1)),
            "u" | "Ou" | "w" | "Ow" => {
                return Err(out_of_range("Weekday parse elements are not supported yet"));
            }
            "Q" => inp.range(1, 1, 4).map(|v| fl.quarter = Some(v)),
            "H" | "OH" => inp.range(2, 0, 23).map(|v| fl.hour = Some(v)),
            "k" => {
                inp.skip_ws();
                inp.range(2, 0, 23).map(|v| fl.hour = Some(v))
            }
            "I" | "OI" => inp.range(2, 1, 12).map(|v| fl.hour12 = Some(v)),
            "l" => {
                inp.skip_ws();
                inp.range(2, 1, 12).map(|v| fl.hour12 = Some(v))
            }
            "M" | "OM" => inp.range(2, 0, 59).map(|v| fl.minute = Some(v)),
            "S" | "OS" => inp.range(2, 0, 60).map(|v| fl.second = Some(v)),
            "p" | "P" => {
                let l = inp.s.to_ascii_lowercase();
                if l.starts_with("am") {
                    fl.pm = Some(false);
                    inp.s = &inp.s[2..];
                    Some(())
                } else if l.starts_with("pm") {
                    fl.pm = Some(true);
                    inp.s = &inp.s[2..];
                    Some(())
                } else {
                    None
                }
            }
            "s" => inp.num(20, true).map(|v| fl.epoch = Some(v)),
            "z" | "Ez" | ":z" | "E*z" => inp
                .offset(spec != "z")
                .map(|o| fl.zone = Some(Zone::Fixed(o))),
            "Z" => {
                let n = inp
                    .s
                    .find(|c: char| !(c.is_ascii_alphanumeric() || "/_+-:".contains(c)))
                    .unwrap_or(inp.s.len());
                let name = &inp.s[..n];
                let z = Zone::parse(name)?;
                inp.s = &inp.s[n..];
                fl.zone = Some(z);
                Some(())
            }
            "E*S" => inp.seconds_frac(None).map(|(s, n)| {
                fl.second = Some(s);
                fl.nanos = n;
            }),
            "F" => {
                if !parse_elements("%Y-%m-%d", inp, fl, date_only)? {
                    return fail;
                }
                Some(())
            }
            "T" | "X" | "EX" => {
                if !parse_elements("%H:%M:%S", inp, fl, date_only)? {
                    return fail;
                }
                Some(())
            }
            "D" | "x" | "Ex" => {
                if !parse_elements("%m/%d/%y", inp, fl, date_only)? {
                    return fail;
                }
                Some(())
            }
            "R" => {
                if !parse_elements("%H:%M", inp, fl, date_only)? {
                    return fail;
                }
                Some(())
            }
            "r" => {
                if !parse_elements("%I:%M:%S %p", inp, fl, date_only)? {
                    return fail;
                }
                Some(())
            }
            "c" | "Ec" => {
                if !parse_elements("%a %b %e %H:%M:%S %Y", inp, fl, date_only)? {
                    return fail;
                }
                Some(())
            }
            other => {
                let digits = other.strip_prefix('E').and_then(|x| x.strip_suffix('S'));
                match digits.and_then(|d| d.parse::<usize>().ok()) {
                    Some(k) if k <= 9 => inp.seconds_frac(Some(k)).map(|(s, n)| {
                        fl.second = Some(s);
                        fl.nanos = n;
                    }),
                    _ => {
                        return Err(crate::types::unimplemented(format!(
                            "PARSE format element %{other}"
                        )));
                    }
                }
            }
        };
        if ok.is_none() {
            return fail;
        }
    }
    Ok(true)
}

fn parse_time(fmt: &str, s: &str, default: Zone, date_only: bool) -> Result<Value> {
    let mut fl = Fields::default();
    let mut inp = Input { s, orig: s, fmt };
    inp.skip_ws();
    let failed = || out_of_range(format!("Failed to parse input string \"{s}\""));
    if !parse_elements(fmt, &mut inp, &mut fl, date_only)? {
        return Err(failed());
    }
    inp.skip_ws();
    if !inp.s.is_empty() {
        return Err(failed());
    }
    let field_err = || out_of_range("Out-of-range datetime field in parsing function");
    let result_err = || out_of_range("Invalid result from parsing function");
    let zone = fl.zone.unwrap_or(default);
    if let Some(e) = fl.epoch {
        let t = Timestamp { secs: e, nanos: 0 };
        if !(MIN_SECS..=MAX_SECS).contains(&e) {
            return Err(result_err());
        }
        return Ok(if date_only {
            Value::Date(from_naive_date(zone.local(t).date()))
        } else {
            Value::Timestamp(t)
        });
    }
    let year = match (fl.year, fl.century, fl.yy) {
        (Some(y), _, _) => y,
        (None, Some(c), Some(y)) => c * 100 + y,
        (None, Some(c), None) => c * 100,
        (None, None, Some(y)) => {
            if y >= 69 {
                1900 + y
            } else {
                2000 + y
            }
        }
        _ => 1970,
    };
    let y32 = i32::try_from(year).map_err(|_| result_err())?;
    let month = fl.month.or(fl.quarter.map(|q| (q - 1) * 3 + 1));
    let week = fl
        .week_u
        .map(|w| (w, Weekday::Sun))
        .or(fl.week_w.map(|w| (w, Weekday::Mon)));
    let date = match (fl.yday, week) {
        (Some(j), _) if month.is_none() && fl.day.is_none() => {
            NaiveDate::from_yo_opt(y32, j as u32).ok_or_else(|| {
                let n = if NaiveDate::from_ymd_opt(y32, 2, 29).is_some() {
                    366
                } else {
                    365
                };
                out_of_range(format!(
                    "Year {year} has {n} days, but the specified day of year was {j}"
                ))
            })?
        }
        (_, Some((w, start))) if month.is_none() && fl.day.is_none() => {
            let jan1 = NaiveDate::from_ymd_opt(y32, 1, 1).ok_or_else(result_err)?;
            let ws = week_start(jan1, start);
            let first = if ws < days(jan1) { ws + 7 } else { ws };
            let d = first + (w - 1) * 7;
            naive_date(Date(i32::try_from(d).map_err(|_| result_err())?))
        }
        _ => NaiveDate::from_ymd_opt(y32, month.unwrap_or(1) as u32, fl.day.unwrap_or(1) as u32)
            .ok_or_else(field_err)?,
    };
    let hour = match (fl.hour, fl.hour12, fl.pm) {
        (Some(h), _, _) => h,
        (None, Some(h), pm) => h % 12 + if pm == Some(true) { 12 } else { 0 },
        _ => 0,
    };
    let (mut sec, mut extra) = (fl.second.unwrap_or(0), 0i64);
    if sec == 60 {
        sec = 59;
        extra = 1;
    }
    let n = date
        .and_hms_nano_opt(
            hour as u32,
            fl.minute.unwrap_or(0) as u32,
            sec as u32,
            fl.nanos,
        )
        .ok_or_else(field_err)?;
    let n = n + chrono::Duration::seconds(extra);
    if date_only {
        let d = from_naive_date(n.date());
        return date_value(d.0 as i64).ok_or_else(result_err);
    }
    if !(1..=9999).contains(&year) {
        return Err(result_err());
    }
    let t = zone.to_utc(n);
    if t.secs < MIN_SECS || t.secs > MAX_SECS {
        return Err(result_err());
    }
    Ok(Value::Timestamp(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        time::parse_ts_zone(s, Zone::Fixed(0))
            .map(|(t, _)| t)
            .unwrap_or_default()
    }

    fn trunc(t: &str, part: i64, zone: &str) -> String {
        let a = [
            Value::Timestamp(ts(t)),
            Value::Int64(part),
            Value::String(zone.into()),
        ];
        match timestamp_trunc(&a) {
            Ok(Value::Timestamp(t)) => t.to_rfc3339(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn trunc_across_dst() {
        assert_eq!(
            trunc("2020-11-01 09:30:00Z", HOUR, "America/Los_Angeles"),
            "2020-11-01T09:00:00Z"
        );
        assert_eq!(
            trunc("2020-11-01 08:30:00Z", HOUR, "America/Los_Angeles"),
            "2020-11-01T08:00:00Z"
        );
        assert_eq!(
            trunc("2020-11-01 12:00:00Z", DAY, "America/Los_Angeles"),
            "2020-11-01T07:00:00Z"
        );
        assert_eq!(
            trunc("2018-11-04 12:00:00Z", DAY, "America/Sao_Paulo"),
            "2018-11-04T03:00:00Z"
        );
        assert_eq!(
            trunc("2020-07-15 12:34:56Z", HOUR, "Asia/Kolkata"),
            "2020-07-15T12:30:00Z"
        );
    }

    #[test]
    fn format_elements() {
        let c = Civil::of_ts(ts("2020-07-01 00:04:05Z"), Zone::Fixed(0));
        assert_eq!(
            format_time("%p %I %l %k %e %y %C %u %w %s", &c, false),
            "AM 12 12  0  1 20 20 3 3 1593561845"
        );
        let c = Civil::of_ts(ts("2021-01-03 12:00:00Z"), Zone::Fixed(0));
        assert_eq!(format_time("%U %W %V %G %g", &c, false), "01 00 53 2020 20");
    }
}
