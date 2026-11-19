//! Order-preserving key encoding.
//!
//! Each key part is a tag byte (`0x01` NULL, `0x02` value) followed by a prefix-free payload.
//! DESC parts invert every byte of the part, so NULLs sort first ascending and last descending,
//! as in Spanner. No part ever starts with `0xFF`, which makes `prefix ++ [0xFF]` an upper bound for
//! every key that extends `prefix`.

use std::ops::Bound;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::value::Value;

/// Encoded key bytes: table primary keys and index entry keys.
pub type Key = Arc<[u8]>;

const TAG_NULL: u8 = 0x01;
const TAG_VALUE: u8 = 0x02;
const PREFIX_END: u8 = 0xFF;

/// Appends one encoded key part to `buf`. Fails for types that cannot be key columns.
pub fn encode_part(buf: &mut Vec<u8>, v: &Value, desc: bool) -> Result<()> {
    let start = buf.len();
    match v {
        Value::Null => buf.push(TAG_NULL),
        Value::Bool(b) => buf.extend_from_slice(&[TAG_VALUE, *b as u8]),
        Value::Int64(i) => {
            buf.push(TAG_VALUE);
            buf.extend_from_slice(&((*i as u64) ^ (1 << 63)).to_be_bytes());
        }
        Value::Float64(f) => {
            buf.push(TAG_VALUE);
            buf.extend_from_slice(&f64_key(*f).to_be_bytes());
        }
        Value::Float32(f) => {
            buf.push(TAG_VALUE);
            buf.extend_from_slice(&f32_key(*f).to_be_bytes());
        }
        Value::Numeric(n) => {
            buf.push(TAG_VALUE);
            buf.extend_from_slice(&((n.0 as u128) ^ (1 << 127)).to_be_bytes());
        }
        Value::Date(d) => {
            buf.push(TAG_VALUE);
            buf.extend_from_slice(&((d.0 as u32) ^ (1 << 31)).to_be_bytes());
        }
        Value::Timestamp(t) => {
            buf.push(TAG_VALUE);
            buf.extend_from_slice(&((t.secs as u64) ^ (1 << 63)).to_be_bytes());
            buf.extend_from_slice(&t.nanos.to_be_bytes());
        }
        Value::Uuid(u) => {
            buf.push(TAG_VALUE);
            buf.extend_from_slice(&u.to_be_bytes());
        }
        Value::String(s) => {
            buf.push(TAG_VALUE);
            escape_bytes(buf, s.as_bytes());
        }
        Value::Bytes(b) => {
            buf.push(TAG_VALUE);
            escape_bytes(buf, b);
        }
        Value::Json(_)
        | Value::Interval(_)
        | Value::TokenList(_)
        | Value::Array(_)
        | Value::Struct(_) => {
            return Err(Error::invalid_argument(
                "Value of this type cannot be used in a key",
            ));
        }
    }
    if desc {
        for b in &mut buf[start..] {
            *b = !*b;
        }
    }
    Ok(())
}

fn escape_bytes(buf: &mut Vec<u8>, data: &[u8]) {
    buf.reserve(data.len() + 2);
    for &b in data {
        if b == 0 {
            buf.extend_from_slice(&[0x00, 0xFF]);
        } else {
            buf.push(b);
        }
    }
    buf.extend_from_slice(&[0x00, 0x01]);
}

fn f64_key(f: f64) -> u64 {
    // NaN first, then -inf .. +inf; -0 == +0.
    if f.is_nan() {
        return 0;
    }
    let f = if f == 0.0 { 0.0 } else { f };
    let bits = f.to_bits();
    if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    }
}

fn f32_key(f: f32) -> u32 {
    if f.is_nan() {
        return 0;
    }
    let f = if f == 0.0 { 0.0 } else { f };
    let bits = f.to_bits();
    if bits >> 31 == 1 {
        !bits
    } else {
        bits | (1 << 31)
    }
}

/// Encodes a full or partial key. `desc[i]` is the order of part `i`.
pub fn encode_key(values: &[Value], desc: &[bool]) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(values.len() * 10);
    for (v, d) in values.iter().zip(desc) {
        encode_part(&mut buf, v, *d)?;
    }
    Ok(buf)
}

/// Smallest byte string greater than every key that starts with `prefix`.
pub fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(prefix.len() + 1);
    v.extend_from_slice(prefix);
    v.push(PREFIX_END);
    v
}

/// A set of keys as Spanner's `KeySet`: point keys, ranges over (possibly partial) keys, or all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeySet {
    pub all: bool,
    pub keys: Vec<Vec<Value>>,
    pub ranges: Vec<KeyRange>,
}

impl KeySet {
    pub fn all() -> KeySet {
        KeySet {
            all: true,
            ..KeySet::default()
        }
    }

    pub fn keys(keys: Vec<Vec<Value>>) -> KeySet {
        KeySet {
            keys,
            ..KeySet::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyRange {
    pub start: Vec<Value>,
    pub start_closed: bool,
    pub end: Vec<Value>,
    pub end_closed: bool,
}

impl KeyRange {
    pub fn closed_open(start: Vec<Value>, end: Vec<Value>) -> KeyRange {
        KeyRange {
            start,
            start_closed: true,
            end,
            end_closed: false,
        }
    }
}

/// An encoded byte interval. `lo` is inclusive; `hi` is exclusive (`None` = unbounded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    /// A full key.
    Point(Key),
    Range {
        lo: Key,
        hi: Option<Key>,
    },
}

impl Span {
    pub fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        match self {
            Span::Point(k) => (Bound::Included(k), Bound::Included(k)),
            Span::Range { lo, hi } => (
                Bound::Included(lo),
                hi.as_deref().map_or(Bound::Unbounded, Bound::Excluded),
            ),
        }
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        match self {
            Span::Point(k) => **k == *key,
            Span::Range { lo, hi } => **lo <= *key && hi.as_deref().is_none_or(|h| key < h),
        }
    }

    fn lo(&self) -> &[u8] {
        match self {
            Span::Point(k) => k,
            Span::Range { lo, .. } => lo,
        }
    }
}

/// How point keys are interpreted when encoding a `KeySet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointMode {
    /// Table primary key: point keys must be complete.
    Exact,
    /// Index key: a (complete) index key matches every entry with that key, whatever the
    /// trailing primary-key columns.
    Prefix,
}

/// Encodes a `KeySet` into sorted, non-overlapping spans. `name` is used in error messages.
pub fn encode_keyset(ks: &KeySet, desc: &[bool], mode: PointMode, name: &str) -> Result<Vec<Span>> {
    if ks.all {
        return Ok(vec![Span::Range {
            lo: Arc::from(&[][..]),
            hi: None,
        }]);
    }
    let mut spans = Vec::with_capacity(ks.keys.len() + ks.ranges.len());
    for k in &ks.keys {
        if k.len() != desc.len() {
            return Err(wrong_parts(name, desc.len(), k.len()));
        }
        let enc = encode_key(k, desc)?;
        match mode {
            PointMode::Exact => spans.push(Span::Point(Arc::from(enc))),
            PointMode::Prefix => {
                let hi = prefix_end(&enc);
                spans.push(Span::Range {
                    lo: Arc::from(enc),
                    hi: Some(Arc::from(hi)),
                });
            }
        }
    }
    for r in &ks.ranges {
        if r.start.len() > desc.len() {
            return Err(wrong_parts(name, desc.len(), r.start.len()));
        }
        if r.end.len() > desc.len() {
            return Err(wrong_parts(name, desc.len(), r.end.len()));
        }
        let s = encode_key(&r.start, desc)?;
        let e = encode_key(&r.end, desc)?;
        let lo = if r.start_closed { s } else { prefix_end(&s) };
        let hi = if r.end_closed { prefix_end(&e) } else { e };
        if lo < hi {
            spans.push(Span::Range {
                lo: Arc::from(lo),
                hi: Some(Arc::from(hi)),
            });
        }
    }
    Ok(normalize(spans))
}

pub fn wrong_parts(name: &str, expected: usize, found: usize) -> Error {
    Error::failed_precondition(format!(
        "Wrong number of key parts for {name}. Expected {expected} key parts, found {found} key parts."
    ))
}

/// Sorts spans and merges overlapping ones, so iteration yields each row once, in key order.
fn normalize(mut spans: Vec<Span>) -> Vec<Span> {
    if spans.len() <= 1 {
        return spans;
    }
    spans.sort_by(|a, b| a.lo().cmp(b.lo()));
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for s in spans {
        if let Some(last) = out.last_mut() {
            match (&*last, &s) {
                (Span::Point(a), Span::Point(b)) if a == b => continue,
                (Span::Range { hi, .. }, _) if hi.as_deref().is_none_or(|h| s.lo() < h) => {
                    // `s` starts inside `last`: extend `last` to cover it.
                    let s_hi: Option<Key> = match &s {
                        Span::Point(k) => Some(Arc::from(prefix_after_point(k))),
                        Span::Range { hi, .. } => hi.clone(),
                    };
                    if let Span::Range { hi, .. } = last {
                        *hi = match (hi.take(), s_hi) {
                            (Some(a), Some(b)) => Some(if a >= b { a } else { b }),
                            _ => None,
                        };
                    }
                    continue;
                }
                (Span::Point(p), Span::Range { lo, hi }) if **p == **lo => {
                    *last = Span::Range {
                        lo: lo.clone(),
                        hi: hi.clone(),
                    };
                    continue;
                }
                _ => {}
            }
        }
        out.push(s);
    }
    out
}

/// Exclusive upper bound that covers exactly the point `k` (its immediate successor).
fn prefix_after_point(k: &[u8]) -> Vec<u8> {
    let mut v = k.to_vec();
    v.push(0x00);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Date, Numeric, Timestamp};

    fn enc(v: &Value, desc: bool) -> Vec<u8> {
        let mut b = Vec::new();
        encode_part(&mut b, v, desc).unwrap_or_default();
        b
    }

    fn assert_sorted(values: &[Value]) {
        for desc in [false, true] {
            for w in values.windows(2) {
                let (a, b) = (enc(&w[0], desc), enc(&w[1], desc));
                if desc {
                    assert!(a > b, "desc: {:?} should sort after {:?}", w[0], w[1]);
                } else {
                    assert!(a < b, "asc: {:?} should sort before {:?}", w[0], w[1]);
                }
            }
        }
    }

    #[test]
    fn int64_order_nulls_first() {
        assert_sorted(&[
            Value::Null,
            Value::Int64(i64::MIN),
            Value::Int64(-1),
            Value::Int64(0),
            Value::Int64(1),
            Value::Int64(i64::MAX),
        ]);
    }

    #[test]
    fn float_order_nan_first() {
        assert_sorted(&[
            Value::Null,
            Value::Float64(f64::NAN),
            Value::Float64(f64::NEG_INFINITY),
            Value::Float64(-1.5),
            Value::Float64(0.0),
            Value::Float64(f64::MIN_POSITIVE),
            Value::Float64(2.0),
            Value::Float64(f64::INFINITY),
        ]);
        assert_eq!(
            enc(&Value::Float64(-0.0), false),
            enc(&Value::Float64(0.0), false)
        );
        assert_sorted(&[
            Value::Float32(f32::NAN),
            Value::Float32(-1.0),
            Value::Float32(1.0),
        ]);
    }

    #[test]
    fn string_and_bytes_binary_order() {
        assert_sorted(&[
            Value::Null,
            Value::string(""),
            Value::string("\0"),
            Value::string("\0\0"),
            Value::string("\x01"),
            Value::string("a"),
            Value::string("a\0"),
            Value::string("ab"),
            Value::string("b"),
            Value::string("é"),
        ]);
        assert_sorted(&[
            Value::Bytes(Arc::from(&[0u8][..])),
            Value::Bytes(Arc::from(&[0u8, 0][..])),
            Value::Bytes(Arc::from(&[255u8][..])),
        ]);
    }

    #[test]
    fn other_scalars() {
        assert_sorted(&[Value::Bool(false), Value::Bool(true)]);
        assert_sorted(&[
            Value::Numeric(Numeric(-5)),
            Value::Numeric(Numeric(0)),
            Value::Numeric(Numeric(7)),
        ]);
        assert_sorted(&[
            Value::Date(Date(-1)),
            Value::Date(Date(0)),
            Value::Date(Date(1)),
        ]);
        assert_sorted(&[
            Value::Timestamp(Timestamp {
                secs: -1,
                nanos: 999,
            }),
            Value::Timestamp(Timestamp { secs: 0, nanos: 0 }),
            Value::Timestamp(Timestamp { secs: 0, nanos: 1 }),
        ]);
        assert_sorted(&[Value::Uuid(1), Value::Uuid(u128::MAX)]);
    }

    #[test]
    fn composite_keys_sort_part_by_part() {
        let desc = [false, true];
        let keys = [
            vec![Value::Int64(1), Value::string("b")],
            vec![Value::Int64(1), Value::string("a")],
            vec![Value::Int64(1), Value::Null],
            vec![Value::Int64(2), Value::string("z")],
        ];
        let encoded: Vec<Vec<u8>> = keys
            .iter()
            .map(|k| encode_key(k, &desc).unwrap_or_default())
            .collect();
        for w in encoded.windows(2) {
            assert!(w[0] < w[1]);
        }
        // A string part never lets a longer first part interleave with the next part.
        let a =
            encode_key(&[Value::string("a"), Value::Int64(9)], &[false, false]).unwrap_or_default();
        let b = encode_key(&[Value::string("ab"), Value::Int64(0)], &[false, false])
            .unwrap_or_default();
        assert!(a < b);
    }

    #[test]
    fn prefix_ranges() {
        let desc = [false, false];
        let ks = KeySet {
            ranges: vec![KeyRange {
                start: vec![Value::Int64(1)],
                start_closed: true,
                end: vec![Value::Int64(1)],
                end_closed: true,
            }],
            ..KeySet::default()
        };
        let spans = encode_keyset(&ks, &desc, PointMode::Exact, "T").unwrap_or_default();
        assert_eq!(spans.len(), 1);
        let inside =
            encode_key(&[Value::Int64(1), Value::Int64(i64::MAX)], &desc).unwrap_or_default();
        let inside_null = encode_key(&[Value::Int64(1), Value::Null], &desc).unwrap_or_default();
        let outside = encode_key(&[Value::Int64(2), Value::Null], &desc).unwrap_or_default();
        assert!(spans[0].contains(&inside));
        assert!(spans[0].contains(&inside_null));
        assert!(!spans[0].contains(&outside));

        let open = KeySet {
            ranges: vec![KeyRange {
                start: vec![Value::Int64(1)],
                start_closed: false,
                end: vec![],
                end_closed: true,
            }],
            ..KeySet::default()
        };
        let spans = encode_keyset(&open, &desc, PointMode::Exact, "T").unwrap_or_default();
        assert!(!spans[0].contains(&inside));
        assert!(spans[0].contains(&outside));
    }

    #[test]
    fn keyset_merges_and_dedups() {
        let desc = [false];
        let ks = KeySet {
            keys: vec![
                vec![Value::Int64(5)],
                vec![Value::Int64(1)],
                vec![Value::Int64(5)],
            ],
            ranges: vec![KeyRange::closed_open(
                vec![Value::Int64(3)],
                vec![Value::Int64(7)],
            )],
            all: false,
        };
        let spans = encode_keyset(&ks, &desc, PointMode::Exact, "T").unwrap_or_default();
        assert_eq!(spans.len(), 2);
        assert!(matches!(spans[0], Span::Point(_)));
        assert!(spans[1].contains(&encode_key(&[Value::Int64(5)], &desc).unwrap_or_default()));
        let err = encode_keyset(
            &KeySet::keys(vec![vec![Value::Int64(1), Value::Int64(2)]]),
            &desc,
            PointMode::Exact,
            "T",
        );
        assert!(err.is_err());
    }
}
