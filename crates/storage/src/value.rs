use std::fmt::Write as _;
use std::sync::Arc;

use base64::Engine as _;
use prost_types::value::Kind;
use spanbolt_ddl::ColumnType;

/// The value `spanner.commit_timestamp()` that mutations use to request the commit timestamp.
pub const COMMIT_TIMESTAMP_SENTINEL: &str = "spanner.commit_timestamp()";

/// A Spanner value. Untyped: the type comes from the column or the query.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    Float32(f32),
    Float64(f64),
    Numeric(Numeric),
    String(Arc<str>),
    Bytes(Arc<[u8]>),
    Date(Date),
    Timestamp(Timestamp),
    Json(Arc<str>),
    Interval(Interval),
    Uuid(u128),
    TokenList(Arc<[u8]>),
    Array(Arc<[Value]>),
    Struct(Arc<[Value]>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn string(s: &str) -> Value {
        Value::String(Arc::from(s))
    }
}

/// NUMERIC: precision 38, scale 9, stored as an integer count of 1e-9 units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Numeric(pub i128);

const NUMERIC_SCALE: i128 = 1_000_000_000;
const NUMERIC_MAX: i128 = 99_999_999_999_999_999_999_999_999_999_999_999_999;

impl Numeric {
    /// Like `parse`, but rejects values that would need rounding (wire values).
    pub fn parse_exact(s: &str) -> Option<Numeric> {
        Numeric::parse_impl(s, true)
    }

    pub fn parse(s: &str) -> Option<Numeric> {
        Numeric::parse_impl(s, false)
    }

    fn parse_impl(s: &str, exact: bool) -> Option<Numeric> {
        let s = s.trim();
        let (neg, body) = match s.as_bytes().first()? {
            b'-' => (true, &s[1..]),
            b'+' => (false, &s[1..]),
            _ => (false, s),
        };
        let (mantissa, exp) = match body.find(['e', 'E']) {
            Some(i) => (&body[..i], body[i + 1..].parse::<i32>().ok()?),
            None => (body, 0),
        };
        let (int_part, frac_part) = match mantissa.find('.') {
            Some(i) => (&mantissa[..i], &mantissa[i + 1..]),
            None => (mantissa, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        if !int_part
            .bytes()
            .chain(frac_part.bytes())
            .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let all: Vec<u8> = int_part
            .bytes()
            .chain(frac_part.bytes())
            .map(|b| b - b'0')
            .collect();
        // value = digits * 10^(point - len); units = value * 1e9.
        let lead = all.iter().position(|&d| d != 0).unwrap_or(all.len());
        let digits = &all[lead..];
        let shift = int_part.len() as i64 - all.len() as i64 + exp as i64 + 9;
        let (keep, round) = if shift >= 0 {
            (digits, false)
        } else {
            let drop = (-shift) as usize;
            if drop > digits.len() {
                if exact && digits.iter().any(|&d| d != 0) {
                    return None;
                }
                (&digits[..0], false)
            } else {
                let cut = digits.len() - drop;
                if exact && digits[cut..].iter().any(|&d| d != 0) {
                    return None;
                }
                (&digits[..cut], digits[cut] >= 5)
            }
        };
        if keep.len() as i64 + shift.max(0) > 38 {
            return None;
        }
        let mut units: i128 = 0;
        for &d in keep {
            units = units * 10 + d as i128;
        }
        for _ in 0..shift.max(0) {
            units = units.checked_mul(10)?;
        }
        if round {
            units += 1;
        }
        if units > NUMERIC_MAX {
            return None;
        }
        Some(Numeric(if neg { -units } else { units }))
    }

    pub fn format(&self, out: &mut String) {
        let v = self.0;
        if v < 0 {
            out.push('-');
        }
        let a = v.unsigned_abs();
        let int = a / NUMERIC_SCALE as u128;
        let frac = a % NUMERIC_SCALE as u128;
        let _ = write!(out, "{int}");
        if frac != 0 {
            let mut f = format!("{frac:09}");
            while f.ends_with('0') {
                f.pop();
            }
            out.push('.');
            out.push_str(&f);
        }
    }
}

/// Days since 1970-01-01.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(pub i32);

pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn parse_uint(s: &str, min_len: usize, max_len: usize) -> Option<u32> {
    if s.len() < min_len || s.len() > max_len || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn parse_ymd(s: &str, strict_width: bool) -> Option<i64> {
    let mut it = s.splitn(3, '-');
    let (ys, ms, ds) = (it.next()?, it.next()?, it.next()?);
    let (mn, dn) = if strict_width { (2, 2) } else { (1, 1) };
    let y = parse_uint(ys, 4, 4)? as i64;
    let m = parse_uint(ms, mn, 2)?;
    let d = parse_uint(ds, dn, 2)?;
    if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

impl Date {
    pub fn parse(s: &str) -> Option<Date> {
        parse_ymd(s, false).map(|d| Date(d as i32))
    }

    pub fn from_ymd(y: i64, m: u32, d: u32) -> Date {
        Date(days_from_civil(y, m, d) as i32)
    }

    pub fn format(&self, out: &mut String) {
        let (y, m, d) = civil_from_days(self.0 as i64);
        let _ = write!(out, "{y:04}-{m:02}-{d:02}");
    }
}

/// Seconds and nanoseconds since the Unix epoch, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp {
    pub secs: i64,
    pub nanos: u32,
}

const MIN_TS_SECS: i64 = -62135596800; // 0001-01-01T00:00:00Z
const MAX_TS_SECS: i64 = 253402300799; // 9999-12-31T23:59:59Z

impl Timestamp {
    pub const MIN: Timestamp = Timestamp {
        secs: MIN_TS_SECS,
        nanos: 0,
    };

    pub fn from_micros(micros: i64) -> Timestamp {
        Timestamp {
            secs: micros.div_euclid(1_000_000),
            nanos: (micros.rem_euclid(1_000_000) * 1000) as u32,
        }
    }

    /// The system's wall-clock time.
    pub fn system_now() -> Timestamp {
        Timestamp::from_nanos(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as i128),
        )
    }

    pub fn from_nanos(nanos: i128) -> Timestamp {
        Timestamp {
            secs: nanos.div_euclid(1_000_000_000) as i64,
            nanos: nanos.rem_euclid(1_000_000_000) as u32,
        }
    }

    pub fn as_nanos(&self) -> i128 {
        self.secs as i128 * 1_000_000_000 + self.nanos as i128
    }

    pub fn as_micros(&self) -> i64 {
        self.secs * 1_000_000 + (self.nanos / 1000) as i64
    }

    pub fn to_proto(self) -> prost_types::Timestamp {
        prost_types::Timestamp {
            seconds: self.secs,
            nanos: self.nanos as i32,
        }
    }

    pub fn from_proto(t: &prost_types::Timestamp) -> Option<Timestamp> {
        if !(0..1_000_000_000).contains(&t.nanos)
            || !(MIN_TS_SECS..=MAX_TS_SECS).contains(&t.seconds)
        {
            return None;
        }
        Some(Timestamp {
            secs: t.seconds,
            nanos: t.nanos as u32,
        })
    }

    /// RFC 3339 in UTC with a trailing `Z`, as the Spanner wire format requires.
    pub fn parse(s: &str) -> Option<Timestamp> {
        let s = s.strip_suffix('Z')?;
        let (date, time) = s.split_once(['T', 't'])?;
        let days = parse_ymd(date, true)?;
        let (hms, frac) = match time.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (time, None),
        };
        let mut it = hms.splitn(3, ':');
        let h = parse_uint(it.next()?, 2, 2)?;
        let mi = parse_uint(it.next()?, 2, 2)?;
        let se = parse_uint(it.next()?, 2, 2)?;
        if h > 23 || mi > 59 || se > 59 {
            return None;
        }
        let nanos = match frac {
            None => 0,
            Some(f) => {
                if f.is_empty() || f.len() > 9 || !f.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                f.parse::<u32>().ok()? * 10u32.pow(9 - f.len() as u32)
            }
        };
        let secs = days * 86400 + (h * 3600 + mi * 60 + se) as i64;
        Some(Timestamp { secs, nanos })
    }

    pub fn format(&self, out: &mut String) {
        self.format_with(out, "Z");
    }

    /// The emulator's message format (`+00:00` suffix).
    pub fn format_offset(&self) -> String {
        let mut s = String::new();
        self.format_with(&mut s, "+00:00");
        s
    }

    fn format_with(&self, out: &mut String, suffix: &str) {
        let days = self.secs.div_euclid(86400);
        let sod = self.secs.rem_euclid(86400);
        let (y, m, d) = civil_from_days(days);
        let _ = write!(
            out,
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
            sod / 3600,
            (sod / 60) % 60,
            sod % 60
        );
        if self.nanos != 0 {
            let mut f = format!("{:09}", self.nanos);
            while f.ends_with('0') {
                f.pop();
            }
            out.push('.');
            out.push_str(&f);
        }
        out.push_str(suffix);
    }

    pub fn to_rfc3339(&self) -> String {
        let mut s = String::new();
        self.format(&mut s);
        s
    }
}

/// INTERVAL: months, days and nanoseconds, each with its own sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub nanos: i128,
}

impl Interval {
    /// ISO 8601 duration, e.g. `P1Y2M3DT4H5M6.5S`.
    pub fn parse(s: &str) -> Option<Interval> {
        let rest = s.strip_prefix('P')?;
        let (date, time) = match rest.split_once('T') {
            Some((d, t)) => (d, Some(t)),
            None => (rest, None),
        };
        let mut iv = Interval::default();
        let mut any = false;
        let mut take = |part: &str, units: &[(char, u8)], iv: &mut Interval| -> Option<()> {
            let mut cur = part;
            while !cur.is_empty() {
                let end = cur.find(|c: char| c.is_ascii_alphabetic())?;
                let (num, unit) = (&cur[..end], cur[end..].chars().next()?);
                let code = units.iter().find(|(c, _)| *c == unit)?.1;
                any = true;
                match code {
                    0 => {
                        iv.months = iv
                            .months
                            .checked_add(num.parse::<i32>().ok()?.checked_mul(12)?)?
                    }
                    1 => iv.months = iv.months.checked_add(num.parse::<i32>().ok()?)?,
                    2 => iv.days = iv.days.checked_add(num.parse::<i32>().ok()?)?,
                    3 => iv.nanos += num.parse::<i128>().ok()? * 3_600_000_000_000,
                    4 => iv.nanos += num.parse::<i128>().ok()? * 60_000_000_000,
                    _ => {
                        let neg = num.starts_with('-');
                        let digits = num.trim_start_matches(['-', '+']);
                        let (w, f) = digits.split_once(['.', ',']).unwrap_or((digits, ""));
                        if f.len() > 9 || (w.is_empty() && f.is_empty()) {
                            return None;
                        }
                        let whole: i128 = if w.is_empty() { 0 } else { w.parse().ok()? };
                        let frac: i128 = if f.is_empty() {
                            0
                        } else {
                            f.parse::<i128>().ok()? * 10i128.pow(9 - f.len() as u32)
                        };
                        let n = whole * 1_000_000_000 + frac;
                        iv.nanos += if neg { -n } else { n };
                    }
                }
                cur = &cur[end + 1..];
            }
            Some(())
        };
        take(date, &[('Y', 0), ('M', 1), ('D', 2)], &mut iv)?;
        if let Some(t) = time {
            if t.is_empty() {
                return None;
            }
            take(t, &[('H', 3), ('M', 4), ('S', 5)], &mut iv)?;
        }
        any.then_some(iv)
    }

    pub fn format(&self, out: &mut String) {
        out.push('P');
        let (y, mo) = (self.months / 12, self.months % 12);
        let mut wrote = false;
        if y != 0 {
            let _ = write!(out, "{y}Y");
            wrote = true;
        }
        if mo != 0 {
            let _ = write!(out, "{mo}M");
            wrote = true;
        }
        if self.days != 0 {
            let _ = write!(out, "{}D", self.days);
            wrote = true;
        }
        if self.nanos != 0 {
            out.push('T');
            let h = self.nanos / 3_600_000_000_000;
            let m = (self.nanos / 60_000_000_000) % 60;
            let rem = self.nanos % 60_000_000_000;
            if h != 0 {
                let _ = write!(out, "{h}H");
            }
            if m != 0 {
                let _ = write!(out, "{m}M");
            }
            if rem != 0 {
                if rem < 0 {
                    out.push('-');
                }
                let a = rem.unsigned_abs();
                let _ = write!(out, "{}", a / 1_000_000_000);
                let f = a % 1_000_000_000;
                if f != 0 {
                    let mut fs = format!("{f:09}");
                    while fs.ends_with('0') {
                        fs.pop();
                    }
                    out.push('.');
                    out.push_str(&fs);
                }
                out.push('S');
            }
        } else if !wrote {
            out.push_str("0Y");
        }
    }
}

pub fn parse_uuid(s: &str) -> Option<u128> {
    let s = s
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .unwrap_or(s);
    let hex: String = if s.len() == 36 {
        let b = s.as_bytes();
        if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
            return None;
        }
        s.chars().filter(|&c| c != '-').collect()
    } else if s.len() == 32 {
        s.to_string()
    } else {
        return None;
    };
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok()
}

pub fn format_uuid(u: u128, out: &mut String) {
    let h = format!("{u:032x}");
    out.push_str(&h[0..8]);
    out.push('-');
    out.push_str(&h[8..12]);
    out.push('-');
    out.push_str(&h[12..16]);
    out.push('-');
    out.push_str(&h[16..20]);
    out.push('-');
    out.push_str(&h[20..32]);
}

pub static BASE64: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    base64::engine::GeneralPurposeConfig::new()
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
);

/// Normalizes JSON the way Spanner stores it: keys sorted, no insignificant whitespace.
pub fn normalize_json(s: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    serde_json::to_string(&v).ok()
}

pub fn type_name(ty: &ColumnType) -> String {
    match ty {
        ColumnType::Bool => "BOOL".into(),
        ColumnType::Int64 => "INT64".into(),
        ColumnType::Float32 => "FLOAT32".into(),
        ColumnType::Float64 => "FLOAT64".into(),
        ColumnType::Numeric => "NUMERIC".into(),
        ColumnType::String(_) => "STRING".into(),
        ColumnType::Bytes(_) => "BYTES".into(),
        ColumnType::Date => "DATE".into(),
        ColumnType::Timestamp => "TIMESTAMP".into(),
        ColumnType::Json => "JSON".into(),
        ColumnType::Interval => "INTERVAL".into(),
        ColumnType::Uuid => "UUID".into(),
        ColumnType::TokenList => "TOKENLIST".into(),
        ColumnType::Proto(name) | ColumnType::Enum(name) => name.clone(),
        ColumnType::Array(t) => format!("ARRAY<{}>", type_name(t)),
        ColumnType::Struct(fields) => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(n, t)| match n {
                    Some(n) => format!("{n} {}", type_name(t)),
                    None => type_name(t),
                })
                .collect();
            format!("STRUCT<{}>", inner.join(", "))
        }
    }
}

impl Value {
    /// Decodes the Spanner wire encoding of a value of type `ty`. The error is the message only;
    /// callers pick the status code (mutations use FAILED_PRECONDITION like the emulator).
    pub fn from_proto(v: &prost_types::Value, ty: &ColumnType) -> Result<Value, String> {
        crate::wire::decode(v, ty).map_err(|e| e.message)
    }

    pub fn to_proto(&self) -> prost_types::Value {
        let kind = match self {
            Value::Null => Kind::NullValue(0),
            Value::Bool(b) => Kind::BoolValue(*b),
            Value::Int64(i) => Kind::StringValue(i.to_string()),
            Value::Float32(f) => float_kind(*f as f64),
            Value::Float64(f) => float_kind(*f),
            Value::String(s) | Value::Json(s) => Kind::StringValue(s.to_string()),
            Value::Bytes(b) | Value::TokenList(b) => Kind::StringValue(BASE64.encode(b)),
            Value::Array(items) | Value::Struct(items) => Kind::ListValue(prost_types::ListValue {
                values: items.iter().map(Value::to_proto).collect(),
            }),
            other => {
                let mut s = String::new();
                other.format_scalar(&mut s);
                Kind::StringValue(s)
            }
        };
        prost_types::Value { kind: Some(kind) }
    }

    /// Text form of string-encoded scalars (NUMERIC, DATE, TIMESTAMP, INTERVAL, UUID, INT64).
    fn format_scalar(&self, out: &mut String) {
        match self {
            Value::Numeric(n) => n.format(out),
            Value::Date(d) => d.format(out),
            Value::Timestamp(t) => t.format(out),
            Value::Interval(i) => i.format(out),
            Value::Uuid(u) => format_uuid(*u, out),
            Value::Int64(i) => {
                let _ = write!(out, "{i}");
            }
            _ => {}
        }
    }

    /// GoogleSQL `DebugString`-style rendering used in the emulator's error messages.
    pub fn debug_typed(&self, ty: &ColumnType, out: &mut String) {
        let tn = match ty {
            ColumnType::Bool => "Bool",
            ColumnType::Int64 => "Int64",
            ColumnType::Float32 => "Float",
            ColumnType::Float64 => "Double",
            ColumnType::Numeric => "Numeric",
            ColumnType::String(_) => "String",
            ColumnType::Bytes(_) => "Bytes",
            ColumnType::Date => "Date",
            ColumnType::Timestamp => "Timestamp",
            ColumnType::Json => "Json",
            ColumnType::Interval => "Interval",
            ColumnType::Uuid => "Uuid",
            ColumnType::TokenList => "Tokenlist",
            ColumnType::Array(_) => "Array",
            ColumnType::Struct(_) => "Struct",
            ColumnType::Proto(_) => "Proto",
            ColumnType::Enum(_) => "Enum",
        };
        out.push_str(tn);
        out.push('(');
        match self {
            Value::Null => out.push_str("NULL"),
            Value::Bool(b) => {
                let _ = write!(out, "{b}");
            }
            Value::Float32(f) => {
                let _ = write!(out, "{f}");
            }
            Value::Float64(f) => {
                let _ = write!(out, "{f}");
            }
            Value::String(s) | Value::Json(s) => {
                let _ = write!(out, "{:?}", &**s);
            }
            Value::Bytes(b) | Value::TokenList(b) => {
                out.push_str("b\"");
                for &c in b.iter() {
                    if c.is_ascii_graphic() && c != b'"' && c != b'\\' {
                        out.push(c as char);
                    } else {
                        let _ = write!(out, "\\x{c:02x}");
                    }
                }
                out.push('"');
            }
            Value::Timestamp(t) => out.push_str(&t.format_offset()),
            Value::Array(_) | Value::Struct(_) => out.push_str("..."),
            other => other.format_scalar(out),
        }
        out.push(')');
    }
}

fn float_kind(f: f64) -> Kind {
    if f.is_nan() {
        Kind::StringValue("NaN".into())
    } else if f == f64::INFINITY {
        Kind::StringValue("Infinity".into())
    } else if f == f64::NEG_INFINITY {
        Kind::StringValue("-Infinity".into())
    } else {
        Kind::NumberValue(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(s: &str) -> prost_types::Value {
        prost_types::Value {
            kind: Some(Kind::StringValue(s.into())),
        }
    }

    #[test]
    fn numeric_parse_and_format() {
        let cases = [
            ("1.50", "1.5"),
            ("-0.000000001", "-0.000000001"),
            ("0", "0"),
            (
                "12345678901234567890123456789.123456789",
                "12345678901234567890123456789.123456789",
            ),
            ("1e3", "1000"),
            ("1.5e-3", "0.0015"),
            ("0.0000000015", "0.000000002"),
            (".5", "0.5"),
        ];
        for (input, want) in cases {
            let mut s = String::new();
            Numeric::parse(input)
                .unwrap_or_else(|| panic!("{input}"))
                .format(&mut s);
            assert_eq!(s, want, "{input}");
        }
        assert!(Numeric::parse("1e50").is_none());
        assert!(Numeric::parse("123456789012345678901234567890").is_none());
        assert!(Numeric::parse("abc").is_none());
    }

    #[test]
    fn timestamp_round_trip() {
        for s in [
            "2020-01-02T03:04:05.6Z",
            "0001-01-01T00:00:00Z",
            "9999-12-31T23:59:59.999999999Z",
            "1969-12-31T23:59:59.5Z",
        ] {
            assert_eq!(
                Timestamp::parse(s).map(|t| t.to_rfc3339()).as_deref(),
                Some(s)
            );
        }
        assert!(Timestamp::parse("2020-01-01").is_none());
        assert!(Timestamp::parse("2020-01-01T00:00:00+01:00").is_none());
        assert_eq!(
            Timestamp::parse("1970-01-01T00:00:01Z"),
            Some(Timestamp { secs: 1, nanos: 0 })
        );
    }

    #[test]
    fn date_rules() {
        assert_eq!(Date::parse("1970-01-01"), Some(Date(0)));
        assert_eq!(Date::parse("2020-1-2"), Some(Date::from_ymd(2020, 1, 2)));
        assert!(Date::parse("2020-13-01").is_none());
        assert!(Date::parse("2021-02-29").is_none());
        let mut s = String::new();
        Date::parse("0001-01-01").unwrap_or(Date(0)).format(&mut s);
        assert_eq!(s, "0001-01-01");
    }

    #[test]
    fn interval_round_trip() {
        for s in ["P1Y2M3DT4H5M6.5S", "P0Y", "P-1Y", "PT-0.5S", "P10D"] {
            let mut o = String::new();
            Interval::parse(s)
                .unwrap_or_else(|| panic!("{s}"))
                .format(&mut o);
            assert_eq!(o, s);
        }
    }

    #[test]
    fn wire_encoding() {
        assert_eq!(
            Value::from_proto(&sv("42"), &ColumnType::Int64),
            Ok(Value::Int64(42))
        );
        assert_eq!(Value::Int64(-7).to_proto(), sv("-7"));
        assert_eq!(Value::Float64(f64::NAN).to_proto(), sv("NaN"));
        assert_eq!(
            Value::Float64(f64::NEG_INFINITY).to_proto(),
            sv("-Infinity")
        );
        assert!(
            matches!(Value::from_proto(&sv("NaN"), &ColumnType::Float64), Ok(Value::Float64(f)) if f.is_nan())
        );
        assert!(Value::from_proto(&sv("abc"), &ColumnType::Float64).is_err());
        assert_eq!(
            Value::from_proto(&sv("AP8="), &ColumnType::Bytes(None)),
            Ok(Value::Bytes(Arc::from(&[0u8, 255][..])))
        );
        assert_eq!(
            Value::Bytes(Arc::from(&[0u8, 255][..])).to_proto(),
            sv("AP8=")
        );
        assert_eq!(
            Value::from_proto(&sv(r#"{"b": 1, "a": [1, 2]}"#), &ColumnType::Json),
            Ok(Value::Json(Arc::from(r#"{"a":[1,2],"b":1}"#)))
        );
        let uuid = "0123abcd-4567-89ef-0123-456789abcdef";
        let v = Value::from_proto(&sv(uuid), &ColumnType::Uuid).unwrap_or(Value::Null);
        assert_eq!(v.to_proto(), sv(uuid));
        let arr = prost_types::Value {
            kind: Some(Kind::ListValue(prost_types::ListValue {
                values: vec![
                    sv("1"),
                    prost_types::Value {
                        kind: Some(Kind::NullValue(0)),
                    },
                ],
            })),
        };
        let parsed = Value::from_proto(&arr, &ColumnType::Array(Box::new(ColumnType::Int64)));
        assert_eq!(
            parsed,
            Ok(Value::Array(Arc::from(vec![Value::Int64(1), Value::Null])))
        );
        assert_eq!(parsed.map(|v| v.to_proto()), Ok(arr));
    }
}
