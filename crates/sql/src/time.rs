//! DATE and TIMESTAMP helpers. Spanner's default time zone is America/Los_Angeles.

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Offset, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use spanbolt_storage::{Date, Result, Timestamp};

use crate::value_ops::out_of_range;

pub const DEFAULT_TZ: Tz = chrono_tz::America::Los_Angeles;

/// 0001-01-01T00:00:00Z and 9999-12-31T23:59:59.999999999Z.
pub const MIN_SECS: i64 = -62_135_596_800;
pub const MAX_SECS: i64 = 253_402_300_799;
pub const MIN_DAYS: i32 = -719_162;
pub const MAX_DAYS: i32 = 2_932_896;

pub fn naive_date(d: Date) -> NaiveDate {
    NaiveDate::from_num_days_from_ce_opt(d.0 + 719_163).unwrap_or(NaiveDate::MIN)
}

pub fn from_naive_date(d: NaiveDate) -> Date {
    Date(d.num_days_from_ce() - 719_163)
}

pub fn utc(t: Timestamp) -> DateTime<Utc> {
    DateTime::from_timestamp(t.secs, t.nanos).unwrap_or_default()
}

pub fn from_utc(dt: DateTime<Utc>) -> Timestamp {
    Timestamp {
        secs: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos(),
    }
}

/// Civil time of `t` in `tz`.
pub fn local(t: Timestamp, tz: Tz) -> NaiveDateTime {
    utc(t).with_timezone(&tz).naive_local()
}

/// The timestamp of civil time `n` in `tz` (earliest when ambiguous; skipped times move forward).
pub fn from_local(n: NaiveDateTime, tz: Tz) -> Timestamp {
    match tz.from_local_datetime(&n) {
        chrono::LocalResult::Single(d) => from_utc(d.with_timezone(&Utc)),
        chrono::LocalResult::Ambiguous(a, _) => from_utc(a.with_timezone(&Utc)),
        chrono::LocalResult::None => {
            let shifted = n + chrono::Duration::hours(1);
            match tz.from_local_datetime(&shifted) {
                chrono::LocalResult::Single(d) | chrono::LocalResult::Ambiguous(d, _) => {
                    from_utc(d.with_timezone(&Utc))
                }
                chrono::LocalResult::None => from_utc(Utc.from_utc_datetime(&n)),
            }
        }
    }
}

pub fn offset_secs(t: Timestamp, tz: Tz) -> i32 {
    utc(t).with_timezone(&tz).offset().fix().local_minus_utc()
}

fn push_frac(out: &mut String, nanos: u32) {
    if nanos == 0 {
        return;
    }
    let s = format!("{nanos:09}");
    let digits = if nanos.is_multiple_of(1_000_000) {
        3
    } else if nanos.is_multiple_of(1_000) {
        6
    } else {
        9
    };
    out.push('.');
    out.push_str(&s[..digits]);
}

pub fn push_offset(out: &mut String, secs: i32) {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.abs();
    out.push(sign);
    out.push_str(&format!("{:02}", a / 3600));
    if a % 3600 != 0 {
        out.push_str(&format!(":{:02}", (a % 3600) / 60));
    }
}

/// `CAST(ts AS STRING)`: `2020-07-01 05:00:00-07`.
pub fn format_ts(t: Timestamp, tz: Tz) -> String {
    Zone::Named(tz).format_ts(t)
}

pub fn format_date(d: Date) -> String {
    let mut s = String::new();
    d.format(&mut s);
    s
}

/// Parses a GoogleSQL timestamp string; zone-less strings are civil time in `tz`.
pub fn parse_ts(s: &str, tz: Tz) -> Option<Timestamp> {
    parse_ts_zone(s, Zone::Named(tz)).map(|(t, _)| t)
}

fn take_digits(s: &str, max: usize) -> Option<(u32, &str)> {
    let n = s.bytes().take(max).take_while(u8::is_ascii_digit).count();
    if n == 0 {
        return None;
    }
    Some((s[..n].parse().ok()?, &s[n..]))
}

/// Parses a timestamp string; also reports whether it named its own zone.
pub fn parse_ts_zone(s: &str, default: Zone) -> Option<(Timestamp, bool)> {
    parse_ts_parts(s, default).filter(|(t, _)| t.secs >= MIN_SECS && t.secs <= MAX_SECS)
}

/// [`parse_ts_zone`] without the range check.
pub fn parse_ts_parts(s: &str, default: Zone) -> Option<(Timestamp, bool)> {
    let s = s.trim();
    let (date_part, rest) = match s.find([' ', 'T', 't']) {
        Some(i) => (&s[..i], s[i + 1..].trim_start()),
        None => (s, ""),
    };
    let d = Date::parse(date_part)?;
    let (mut h, mut mi, mut sec, mut nanos) = (0u32, 0u32, 0u32, 0u32);
    let mut zone_text = rest;
    if rest.starts_with(|c: char| c.is_ascii_digit()) {
        let (hh, r) = take_digits(rest, 2)?;
        let r = r.strip_prefix(':')?;
        let (mm, r) = take_digits(r, 2)?;
        let r = r.strip_prefix(':')?;
        let (ss, mut r) = take_digits(r, 2)?;
        if let Some(f) = r.strip_prefix('.') {
            let n = f.bytes().take_while(u8::is_ascii_digit).count();
            if n > 9 {
                return None;
            }
            if n > 0 {
                nanos = format!("{:0<9}", &f[..n]).parse().ok()?;
            }
            r = &f[n..];
        }
        if hh > 23 || mm > 59 || ss > 59 {
            return None;
        }
        (h, mi, sec) = (hh, mm, ss);
        zone_text = r;
    }
    let naive = naive_date(d).and_hms_nano_opt(h, mi, sec, nanos)?;
    let z = zone_text.trim();
    let (zone, named) = if z.is_empty() {
        (default, false)
    } else if z == "Z" || z == "z" {
        (Zone::Fixed(0), true)
    } else {
        (Zone::parse(z).ok()?, true)
    };
    Some((zone.to_utc(naive), named))
}

/// A time zone argument: an IANA name or a fixed UTC offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Zone {
    Named(Tz),
    Fixed(i32),
}

impl Default for Zone {
    fn default() -> Zone {
        Zone::Named(DEFAULT_TZ)
    }
}

fn parse_fixed(s: &str) -> Option<i32> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) if !h.is_empty() && h.len() <= 2 && m.len() == 2 => {
            (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?)
        }
        Some(_) => return None,
        None if !rest.is_empty() && rest.len() <= 2 => (rest.parse::<i32>().ok()?, 0),
        None if rest.len() == 4 => (rest[..2].parse().ok()?, rest[2..].parse().ok()?),
        None => return None,
    };
    if !rest.bytes().all(|b| b.is_ascii_digit() || b == b':') || m > 59 {
        return None;
    }
    let total = h * 3600 + m * 60;
    if total > 14 * 3600 {
        return None;
    }
    Some(sign * total)
}

impl Zone {
    pub fn parse(s: &str) -> Result<Zone> {
        if s.is_empty() {
            return Err(out_of_range("Invalid empty time zone"));
        }
        let bad = || out_of_range(format!("Invalid time zone: {s}"));
        if let Some(off) = parse_fixed(s) {
            return Ok(Zone::Fixed(off));
        }
        if let Some(rest) = s.strip_prefix("UTC")
            && !rest.is_empty()
        {
            return parse_fixed(rest).map(Zone::Fixed).ok_or_else(bad);
        }
        if s == "UTC" {
            return Ok(Zone::Named(Tz::UTC));
        }
        s.parse::<Tz>().map(Zone::Named).map_err(|_| bad())
    }

    /// Offset from UTC in seconds at `t`.
    pub fn offset(&self, t: Timestamp) -> i32 {
        match self {
            Zone::Named(tz) => offset_secs(t, *tz),
            Zone::Fixed(o) => *o,
        }
    }

    /// The offset formatting functions use: whole minutes, truncated toward zero.
    pub fn fmt_offset(&self, t: Timestamp) -> i32 {
        self.offset(t) / 60 * 60
    }

    pub fn local(&self, t: Timestamp) -> NaiveDateTime {
        match self {
            Zone::Named(tz) => local(t, *tz),
            Zone::Fixed(o) => utc(t).naive_utc() + chrono::Duration::seconds(*o as i64),
        }
    }

    /// Civil time as the formatting functions show it.
    pub fn local_fmt(&self, t: Timestamp) -> NaiveDateTime {
        utc(t).naive_utc() + chrono::Duration::seconds(self.fmt_offset(t) as i64)
    }

    pub fn to_utc(self, n: NaiveDateTime) -> Timestamp {
        match self {
            Zone::Named(tz) => from_local(n, tz),
            Zone::Fixed(o) => {
                let base = from_utc(Utc.from_utc_datetime(&n));
                Timestamp {
                    secs: base.secs - o as i64,
                    nanos: base.nanos,
                }
            }
        }
    }

    pub fn name(&self) -> String {
        match self {
            Zone::Named(tz) => tz.name().to_string(),
            Zone::Fixed(o) => {
                let mut s = "UTC".to_string();
                push_offset(&mut s, *o);
                s
            }
        }
    }

    /// `CAST(ts AS STRING)` in this zone: `2020-07-01 05:00:00-07`.
    pub fn format_ts(&self, t: Timestamp) -> String {
        let n = self.local_fmt(t);
        let mut out = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            n.year(),
            n.month(),
            n.day(),
            n.hour(),
            n.minute(),
            n.second()
        );
        push_frac(&mut out, t.nanos);
        push_offset(&mut out, self.fmt_offset(t));
        out
    }
}
