//! string functions.

use std::sync::Arc;

use base64::Engine as _;
use parking_lot::Mutex;
use regex::bytes::{Regex, RegexBuilder};
use spanbolt_storage::{Error, Result, Value};

use crate::func::{ScalarFn, Sig, f, strict};
use crate::time::Zone;
use crate::types::Type;
use crate::value_ops::out_of_range;

const MAX_OUT: usize = 1 << 20;

fn s(v: &Value) -> &str {
    match v {
        Value::String(x) => x,
        _ => "",
    }
}

fn b(v: &Value) -> &[u8] {
    match v {
        Value::String(x) => x.as_bytes(),
        Value::Bytes(x) => x,
        _ => &[],
    }
}

fn i(v: &Value) -> i64 {
    match v {
        Value::Int64(x) => *x,
        _ => 0,
    }
}

fn st(x: impl Into<Arc<str>>) -> Value {
    Value::String(x.into())
}

fn by(x: impl Into<Arc<[u8]>>) -> Value {
    Value::Bytes(x.into())
}

fn is_bytes(sig: &Sig) -> bool {
    matches!(sig.args.first(), Some(Type::Bytes(_)))
}

/// Builds a STRING or BYTES result from bytes the function produced from inputs of that kind.
fn out(bytes_kind: bool, v: Vec<u8>) -> Value {
    if bytes_kind {
        by(v)
    } else {
        st(String::from_utf8_lossy(&v).into_owned())
    }
}

/// Characters (STRING) or bytes (BYTES) as units, for position-based functions.
enum Units<'a> {
    Chars(Vec<&'a str>),
    Bytes(&'a [u8]),
}

impl<'a> Units<'a> {
    fn of(v: &'a Value) -> Units<'a> {
        match v {
            Value::String(x) => Units::Chars(
                x.char_indices()
                    .map(|(k, c)| &x[k..k + c.len_utf8()])
                    .collect(),
            ),
            _ => Units::Bytes(b(v)),
        }
    }

    fn len(&self) -> usize {
        match self {
            Units::Chars(c) => c.len(),
            Units::Bytes(b) => b.len(),
        }
    }

    fn slice(&self, from: usize, to: usize) -> Value {
        match self {
            Units::Chars(c) => st(c.get(from..to).unwrap_or(&[]).concat()),
            Units::Bytes(b) => by(b.get(from..to).unwrap_or(&[]).to_vec()),
        }
    }
}

fn substr(a: &[Value]) -> Result<Value> {
    let u = Units::of(&a[0]);
    let len = u.len() as i64;
    let pos = i(&a[1]);
    let start = if pos > 0 {
        (pos - 1).min(len)
    } else if pos == 0 {
        0
    } else {
        len.saturating_add(pos).max(0)
    };
    let end = match a.get(2) {
        Some(l) => {
            let l = i(l);
            if l < 0 {
                return Err(out_of_range(
                    "Third argument in SUBSTR() cannot be negative",
                ));
            }
            start.saturating_add(l).min(len)
        }
        None => len,
    };
    Ok(u.slice(start as usize, end.max(start) as usize))
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() {
        return Some(0);
    }
    h.windows(n.len()).position(|w| w == n)
}

fn strpos(a: &[Value]) -> Result<Value> {
    let (h, n) = (b(&a[0]), b(&a[1]));
    Ok(Value::Int64(match find(h, n) {
        None => 0,
        Some(k) => match &a[0] {
            Value::String(x) => x.get(..k).map_or(0, |p| p.chars().count()) as i64 + 1,
            _ => k as i64 + 1,
        },
    }))
}

fn replace(a: &[Value]) -> Result<Value> {
    let (h, from, to) = (b(&a[0]), b(&a[1]), b(&a[2]));
    if from.is_empty() {
        return Ok(a[0].clone());
    }
    let mut r = Vec::with_capacity(h.len());
    let mut k = 0;
    while k < h.len() {
        if h[k..].starts_with(from) {
            r.extend_from_slice(to);
            k += from.len();
        } else {
            r.push(h[k]);
            k += 1;
        }
    }
    Ok(out(matches!(a[0], Value::Bytes(_)), r))
}

#[derive(Clone, Copy)]
enum Side {
    Both,
    Left,
    Right,
}

fn trim(side: Side) -> ScalarFn {
    strict(move |a| {
        let (l, r) = (!matches!(side, Side::Right), !matches!(side, Side::Left));
        match &a[0] {
            Value::String(x) => {
                let set: Option<Vec<char>> = a.get(1).map(|v| s(v).chars().collect());
                let pred = |c: char| match &set {
                    Some(cs) => cs.contains(&c),
                    None => c.is_whitespace(),
                };
                let mut t: &str = x;
                if l {
                    t = t.trim_start_matches(pred);
                }
                if r {
                    t = t.trim_end_matches(pred);
                }
                Ok(st(t))
            }
            Value::Bytes(x) => {
                let set = a.get(1).map(b).unwrap_or(&[]);
                let mut t: &[u8] = x;
                while l && t.first().is_some_and(|c| set.contains(c)) {
                    t = &t[1..];
                }
                while r && t.last().is_some_and(|c| set.contains(c)) {
                    t = &t[..t.len() - 1];
                }
                Ok(by(t.to_vec()))
            }
            _ => Err(Error::internal("bad TRIM operand")),
        }
    })
}

fn pad(left: bool) -> ScalarFn {
    strict(move |a| {
        let size = i(&a[1]);
        if size < 0 {
            return Err(out_of_range(
                "Second argument (output size) for LPAD/RPAD cannot be negative",
            ));
        }
        let u = Units::of(&a[0]);
        let default_pad = Value::String(Arc::from(" "));
        let pad_v = a.get(2).unwrap_or(&default_pad);
        let p = Units::of(pad_v);
        if p.len() == 0 {
            return Err(out_of_range(
                "Third argument (pad pattern) for LPAD/RPAD cannot be empty",
            ));
        }
        let max_unit = match (&u, &p) {
            (Units::Chars(_), _) => 4,
            _ => 1,
        };
        if (size as u64).saturating_mul(1) > MAX_OUT as u64
            && (size as u64).saturating_mul(max_unit) > MAX_OUT as u64
        {
            return Err(out_of_range(
                "Output of LPAD/RPAD exceeds max allowed output size of 1MB",
            ));
        }
        let size = size as usize;
        let n = u.len();
        if size <= n {
            return Ok(u.slice(0, size));
        }
        let fill = size - n;
        let parts = |count: usize| -> Vec<u8> {
            let mut v = Vec::new();
            for k in 0..count {
                match &p {
                    Units::Chars(c) => v.extend_from_slice(c[k % c.len()].as_bytes()),
                    Units::Bytes(bs) => v.push(bs[k % bs.len()]),
                }
            }
            v
        };
        let body = b(&a[0]).to_vec();
        let mut r = Vec::new();
        if left {
            r.extend(parts(fill));
            r.extend(body);
        } else {
            r.extend(body);
            r.extend(parts(fill));
        }
        if r.len() > MAX_OUT {
            return Err(out_of_range(
                "Output of LPAD/RPAD exceeds max allowed output size of 1MB",
            ));
        }
        Ok(out(matches!(a[0], Value::Bytes(_)), r))
    })
}

fn repeat(a: &[Value]) -> Result<Value> {
    let n = i(&a[1]);
    if n < 0 {
        return Err(out_of_range(
            "Second argument (repeat count) for REPEAT cannot be negative",
        ));
    }
    let x = b(&a[0]);
    if (x.len() as u64).saturating_mul(n as u64) > MAX_OUT as u64 {
        return Err(out_of_range(
            "Output of REPEAT exceeds max allowed output size of 1MB",
        ));
    }
    Ok(out(matches!(a[0], Value::Bytes(_)), x.repeat(n as usize)))
}

fn split(a: &[Value]) -> Result<Value> {
    let bytes_kind = matches!(a[0], Value::Bytes(_));
    let h = b(&a[0]);
    let d: &[u8] = match a.get(1) {
        Some(v) => b(v),
        None => b",",
    };
    let mut parts: Vec<Value> = Vec::new();
    if d.is_empty() {
        if h.is_empty() {
            parts.push(out(bytes_kind, Vec::new()));
        } else if bytes_kind {
            parts.extend(h.iter().map(|c| by(vec![*c])));
        } else {
            parts.extend(s(&a[0]).chars().map(|c| st(c.to_string())));
        }
    } else {
        let mut k = 0;
        let mut start = 0;
        while k + d.len() <= h.len() {
            if &h[k..k + d.len()] == d {
                parts.push(out(bytes_kind, h[start..k].to_vec()));
                k += d.len();
                start = k;
            } else {
                k += 1;
            }
        }
        parts.push(out(bytes_kind, h[start..].to_vec()));
    }
    Ok(Value::Array(Arc::from(parts)))
}

fn soundex(x: &str) -> String {
    fn code(c: u8) -> u8 {
        match c.to_ascii_lowercase() {
            b'b' | b'f' | b'p' | b'v' => b'1',
            b'c' | b'g' | b'j' | b'k' | b'q' | b's' | b'x' | b'z' => b'2',
            b'd' | b't' => b'3',
            b'l' => b'4',
            b'm' | b'n' => b'5',
            b'r' => b'6',
            _ => b'0',
        }
    }
    let mut r = Vec::with_capacity(4);
    let mut last = b'0';
    for &c in x.as_bytes().iter().filter(|c| c.is_ascii_alphabetic()) {
        let k = code(c);
        if r.is_empty() {
            r.push(c);
        } else if k != b'0' && k != last {
            r.push(k);
            if r.len() == 4 {
                break;
            }
        }
        if k != b'0' {
            last = k;
        }
    }
    if r.is_empty() {
        return String::new();
    }
    while r.len() < 4 {
        r.push(b'0');
    }
    String::from_utf8_lossy(&r).into_owned()
}

/// Lossy UTF-8 decoding with one U+FFFD per invalid byte.
fn lossy_per_byte(mut x: &[u8]) -> String {
    let mut r = String::with_capacity(x.len());
    loop {
        match std::str::from_utf8(x) {
            Ok(v) => {
                r.push_str(v);
                return r;
            }
            Err(e) => {
                let ok = e.valid_up_to();
                r.push_str(std::str::from_utf8(&x[..ok]).unwrap_or_default());
                let bad = e.error_len().unwrap_or(x.len() - ok);
                for _ in 0..bad {
                    r.push('\u{FFFD}');
                }
                x = &x[ok + bad..];
            }
        }
    }
}

fn from_hex(x: &str) -> Result<Value> {
    let bad = |k: usize| {
        out_of_range(format!(
            "Failed to decode invalid hexadecimal string due to character at offset {k}: {x}"
        ))
    };
    let raw = x.as_bytes();
    if let Some(k) = raw.iter().position(|c| !c.is_ascii_hexdigit()) {
        return Err(bad(k));
    }
    let digits: Vec<u8> = if raw.len() % 2 == 1 {
        std::iter::once(b'0').chain(raw.iter().copied()).collect()
    } else {
        raw.to_vec()
    };
    let val = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
    Ok(by(digits
        .chunks(2)
        .map(|p| val(p[0]) * 16 + val(p.get(1).copied().unwrap_or(b'0')))
        .collect::<Vec<u8>>()))
}

fn to_hex(x: &[u8]) -> String {
    let mut r = String::with_capacity(x.len() * 2);
    for c in x {
        r.push_str(&format!("{c:02x}"));
    }
    r
}

fn from_base64(x: &str) -> Result<Value> {
    let cleaned: String = x.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let err = || out_of_range("Failed to decode invalid base64 string");
    let eng = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    eng.decode(cleaned.as_bytes()).map(by).map_err(|_| err())
}

fn random_uuid() -> Result<u128> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| Error::internal(format!("random: {e}")))?;
    buf[6] = (buf[6] & 0x0f) | 0x40;
    buf[8] = (buf[8] & 0x3f) | 0x80;
    Ok(u128::from_be_bytes(buf))
}

fn uuid_string(u: u128) -> String {
    let mut r = String::new();
    spanbolt_storage::value::format_uuid(u, &mut r);
    r
}

fn code_points_to_string(a: &[Value]) -> Result<Value> {
    let Value::Array(xs) = &a[0] else {
        return Ok(Value::Null);
    };
    let mut r = String::new();
    for x in xs.iter() {
        let Value::Int64(c) = x else {
            return Ok(Value::Null);
        };
        let ch = u32::try_from(*c)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(|| out_of_range(format!("Invalid codepoint {c}")))?;
        r.push(ch);
    }
    Ok(st(r))
}

fn code_points_to_bytes(a: &[Value]) -> Result<Value> {
    let Value::Array(xs) = &a[0] else {
        return Ok(Value::Null);
    };
    let mut r = Vec::new();
    for x in xs.iter() {
        let Value::Int64(c) = x else {
            return Ok(Value::Null);
        };
        r.push(u8::try_from(*c).map_err(|_| out_of_range(format!("Invalid ASCII value {c}")))?);
    }
    Ok(by(r))
}

// ---- NORMALIZE

fn normalize(x: &str, mode: i64) -> Option<String> {
    use unicode_normalization::UnicodeNormalization;
    Some(match mode {
        0 => x.nfc().collect(),
        1 => x.nfkc().collect(),
        2 => x.nfd().collect(),
        3 => x.nfkd().collect(),
        _ => return None,
    })
}

/// Full case folding for the characters where it is known; `None` when a character needs
/// mappings this table lacks.
fn casefold(x: &str) -> Option<String> {
    let mut r = String::with_capacity(x.len());
    for c in x.chars() {
        match c {
            'ß' | 'ẞ' => r.push_str("ss"),
            'ſ' => r.push('s'),
            'µ' => r.push('μ'),
            'ς' => r.push('σ'),
            '\u{0130}' => r.push_str("i\u{0307}"),
            '\u{00AD}'
            | '\u{0149}'
            | '\u{01F0}'
            | '\u{0345}'
            | '\u{0390}'
            | '\u{03B0}'
            | '\u{03D0}'..='\u{03D6}'
            | '\u{03F0}'..='\u{03F5}'
            | '\u{0587}'
            | '\u{13F8}'..='\u{13FD}'
            | '\u{1E96}'..='\u{1E9B}'
            | '\u{1F50}'..='\u{1FFF}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{AB70}'..='\u{ABBF}'
            | '\u{FB00}'..='\u{FB17}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}' => return None,
            c => r.extend(c.to_lowercase()),
        }
    }
    Some(r)
}

// ---- regular expressions

fn re_err(m: impl std::fmt::Display) -> Error {
    out_of_range(format!("Cannot parse regular expression: {m}"))
}

/// RE2's error text for a pattern the parser rejects.
fn re2_message(orig: &str, pat: &str, e: &regex_syntax::Error) -> String {
    use regex_syntax::ast::ErrorKind as A;
    use regex_syntax::hir::ErrorKind as H;
    let snippet =
        |sp: &regex_syntax::ast::Span| pat.get(sp.start.offset..sp.end.offset).unwrap_or("");
    match e {
        regex_syntax::Error::Parse(p) => {
            let sp = p.span();
            match p.kind() {
                A::GroupUnclosed => format!("missing ): {orig}"),
                A::GroupUnopened => format!("unexpected ): {orig}"),
                A::ClassUnclosed => {
                    format!("missing ]: {}", pat.get(sp.start.offset..).unwrap_or(""))
                }
                A::RepetitionMissing => {
                    let op: String = pat
                        .get(sp.start.offset..)
                        .and_then(|r| r.chars().next())
                        .map(String::from)
                        .unwrap_or_default();
                    let before = pat.get(..sp.start.offset).unwrap_or("");
                    match before.chars().last() {
                        Some(c @ ('*' | '+' | '?')) => format!("bad repetition operator: {c}{op}"),
                        _ => format!("no argument for repetition operator: {op}"),
                    }
                }
                A::ClassRangeInvalid => {
                    format!("invalid character class range: {}", snippet(sp))
                }
                A::EscapeUnexpectedEof => "trailing \\".to_string(),
                A::UnicodeClassInvalid => {
                    format!("invalid character class range: {}", snippet(sp))
                }
                A::EscapeUnrecognized | A::UnsupportedBackreference | A::ClassEscapeInvalid => {
                    format!("invalid escape sequence: {}", snippet(sp))
                }
                A::RepetitionCountInvalid
                | A::RepetitionCountUnclosed
                | A::RepetitionCountDecimalEmpty => {
                    format!("invalid repetition size: {}", snippet(sp))
                }
                A::UnsupportedLookAround => format!(
                    "invalid perl operator: {}",
                    pat.get(sp.start.offset..)
                        .unwrap_or("")
                        .chars()
                        .take(3)
                        .collect::<String>()
                ),
                A::FlagUnrecognized | A::FlagUnexpectedEof | A::FlagDanglingNegation => {
                    format!(
                        "invalid perl operator: {}",
                        pat.get(..sp.end.offset)
                            .and_then(|h| h.rfind("(?").map(|k| &h[k..]))
                            .unwrap_or("")
                    )
                }
                _ => p.to_string(),
            }
        }
        regex_syntax::Error::Translate(t) => {
            let sp = t.span();
            match t.kind() {
                H::UnicodePropertyNotFound | H::UnicodePropertyValueNotFound => format!(
                    "invalid character class range: {}",
                    pat.get(sp.start.offset..sp.end.offset).unwrap_or("")
                ),
                _ => t.to_string(),
            }
        }
        _ => e.to_string(),
    }
}

/// Constructs RE2 rejects that the `regex` crate accepts: nested repetition operators and
/// repetition counts above 1000.
fn ast_check(ast: &regex_syntax::ast::Ast, pat: &str) -> Option<String> {
    use regex_syntax::ast::{Ast, RepetitionKind, RepetitionRange};
    match ast {
        Ast::Repetition(r) => {
            if let Ast::Repetition(inner) = r.ast.as_ref() {
                return Some(format!(
                    "bad repetition operator: {}",
                    pat.get(inner.op.span.start.offset..r.op.span.end.offset)
                        .unwrap_or("")
                ));
            }
            if let RepetitionKind::Range(
                RepetitionRange::Exactly(n)
                | RepetitionRange::AtLeast(n)
                | RepetitionRange::Bounded(_, n),
            ) = &r.op.kind
                && *n > 1000
            {
                return Some(format!(
                    "invalid repetition size: {}",
                    pat.get(r.op.span.start.offset..r.op.span.end.offset)
                        .unwrap_or("")
                ));
            }
            ast_check(&r.ast, pat)
        }
        Ast::Group(g) => ast_check(&g.ast, pat),
        Ast::Alternation(a) => a.asts.iter().find_map(|x| ast_check(x, pat)),
        Ast::Concat(c) => c.asts.iter().find_map(|x| ast_check(x, pat)),
        _ => None,
    }
}

/// Rewrites RE2 syntax that the `regex` crate reads differently: ASCII-only Perl classes and
/// word boundaries, `\Q...\E` quoting and `\C`.
fn translate(p: &str) -> String {
    let mut r = String::with_capacity(p.len() + 8);
    let mut it = p.chars().peekable();
    while let Some(c) = it.next() {
        if c == '(' && it.peek() == Some(&'?') {
            // Group names are never used; RE2 allows duplicates, `regex` does not.
            let rest: String = it.clone().take(3).collect();
            let named = rest.starts_with("?P<")
                || (rest.starts_with("?<") && !rest.starts_with("?<=") && !rest.starts_with("?<!"));
            if named && it.clone().any(|x| x == '>') {
                for x in it.by_ref() {
                    if x == '>' {
                        break;
                    }
                }
                r.push('(');
                continue;
            }
        }
        if c != '\\' {
            r.push(c);
            continue;
        }
        let Some(n) = it.next() else {
            r.push('\\');
            break;
        };
        match n {
            'd' => r.push_str("[0-9]"),
            'D' => r.push_str("[^0-9]"),
            'w' => r.push_str("[0-9A-Za-z_]"),
            'W' => r.push_str("[^0-9A-Za-z_]"),
            's' => r.push_str("[\\t\\n\\f\\r ]"),
            'S' => r.push_str("[^\\t\\n\\f\\r ]"),
            'b' => r.push_str("(?-u:\\b)"),
            'B' => r.push_str("(?-u:\\B)"),
            'C' => r.push_str("(?s:.)"),
            'Q' => {
                let mut lit = String::new();
                while let Some(q) = it.next() {
                    if q == '\\' && it.peek() == Some(&'E') {
                        it.next();
                        break;
                    }
                    lit.push(q);
                }
                r.push_str(&regex::escape(&lit));
            }
            other => {
                r.push('\\');
                r.push(other);
            }
        }
    }
    r
}

/// Pattern bytes read as Latin-1, with non-ASCII bytes as `\xHH` escapes.
fn latin1_pattern(p: &[u8]) -> String {
    let mut r = String::with_capacity(p.len());
    for &c in p {
        if c.is_ascii() {
            r.push(c as char);
        } else {
            r.push_str(&format!("\\x{c:02X}"));
        }
    }
    r
}

fn compile(pat: &Value) -> Result<Regex> {
    let (orig, translated, unicode) = match pat {
        Value::String(p) => (p.to_string(), translate(p), true),
        _ => {
            let l = latin1_pattern(b(pat));
            (String::from_utf8_lossy(b(pat)).into_owned(), l, false)
        }
    };
    let ast = regex_syntax::ast::parse::Parser::new()
        .parse(&translated)
        .map_err(|e| {
            re_err(re2_message(
                &orig,
                &translated,
                &regex_syntax::Error::Parse(e),
            ))
        })?;
    if let Some(m) = ast_check(&ast, &translated) {
        return Err(re_err(m));
    }
    regex_syntax::ParserBuilder::new()
        .unicode(unicode)
        .utf8(unicode)
        .build()
        .parse(&translated)
        .map_err(|e| re_err(re2_message(&orig, &translated, &e)))?;
    RegexBuilder::new(&translated)
        .unicode(unicode)
        .size_limit(8 << 20)
        .build()
        .map_err(|_| re_err(format!("pattern too large - compile failed: {orig}")))
}

/// Compiles each distinct pattern once per call site.
struct ReCache(Mutex<Option<(Value, Arc<Regex>)>>);

impl ReCache {
    fn new() -> ReCache {
        ReCache(Mutex::new(None))
    }

    fn get(&self, pat: &Value) -> Result<Arc<Regex>> {
        let mut g = self.0.lock();
        if let Some((p, r)) = g.as_ref()
            && p == pat
        {
            return Ok(r.clone());
        }
        let r = Arc::new(compile(pat)?);
        *g = Some((pat.clone(), r.clone()));
        Ok(r)
    }
}

fn step(h: &[u8], pos: usize, bytes_kind: bool) -> usize {
    if bytes_kind {
        return pos + 1;
    }
    let mut k = pos + 1;
    while k < h.len() && (h[k] & 0xC0) == 0x80 {
        k += 1;
    }
    k
}

fn extraction_group(re: &Regex) -> Result<usize> {
    match re.captures_len() {
        1 => Ok(0),
        2 => Ok(1),
        _ => Err(out_of_range(
            "Regular expressions passed into extraction functions must not have more than 1 capturing group",
        )),
    }
}

fn regexp_fn(name: &'static str) -> ScalarFn {
    let cache = ReCache::new();
    strict(move |a| {
        let re = cache.get(&a[1])?;
        let bytes_kind = matches!(a[0], Value::Bytes(_));
        let h = b(&a[0]);
        match name {
            "regexp_contains" => Ok(Value::Bool(re.is_match(h))),
            "regexp_extract" => {
                let g = extraction_group(&re)?;
                Ok(match re.captures(h) {
                    Some(c) => match c.get(g) {
                        Some(m) => out(bytes_kind, m.as_bytes().to_vec()),
                        None => Value::Null,
                    },
                    None => Value::Null,
                })
            }
            "regexp_extract_all" => {
                let g = extraction_group(&re)?;
                let mut r = Vec::new();
                let mut pos = 0;
                while pos < h.len() || (pos == 0 && h.is_empty()) {
                    let Some(c) = re.captures_at(h, pos) else {
                        break;
                    };
                    let Some(m0) = c.get(0) else { break };
                    r.push(match c.get(g) {
                        Some(m) => out(bytes_kind, m.as_bytes().to_vec()),
                        None => out(bytes_kind, Vec::new()),
                    });
                    pos = if m0.is_empty() {
                        step(h, m0.end(), bytes_kind)
                    } else {
                        m0.end()
                    };
                }
                Ok(Value::Array(Arc::from(r)))
            }
            _ => regexp_replace(&re, h, b(&a[2]), bytes_kind),
        }
    })
}

enum Piece {
    Lit(Vec<u8>),
    Group(usize),
}

fn parse_rewrite(rw: &[u8], groups: usize) -> Result<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut lit = Vec::new();
    let mut k = 0;
    while k < rw.len() {
        let c = rw[k];
        if c != b'\\' {
            lit.push(c);
            k += 1;
            continue;
        }
        match rw.get(k + 1) {
            Some(d) if d.is_ascii_digit() => {
                let n = (d - b'0') as usize;
                if n >= groups {
                    return Err(out_of_range(format!(
                        "Invalid REGEXP_REPLACE pattern: Rewrite schema requests {n} matches, but the regexp only has {} parenthesized subexpressions.",
                        groups - 1
                    )));
                }
                if !lit.is_empty() {
                    pieces.push(Piece::Lit(std::mem::take(&mut lit)));
                }
                pieces.push(Piece::Group(n));
            }
            Some(b'\\') => lit.push(b'\\'),
            _ => {
                return Err(out_of_range(
                    "Invalid REGEXP_REPLACE pattern: Rewrite schema error: '\\' must be followed by a digit or '\\'.",
                ));
            }
        }
        k += 2;
    }
    if !lit.is_empty() {
        pieces.push(Piece::Lit(lit));
    }
    Ok(pieces)
}

fn regexp_replace(re: &Regex, h: &[u8], rw: &[u8], bytes_kind: bool) -> Result<Value> {
    let pieces = parse_rewrite(rw, re.captures_len())?;
    let mut r = Vec::with_capacity(h.len());
    let mut p = 0;
    let mut lastend: Option<usize> = None;
    while p <= h.len() {
        let Some(c) = re.captures_at(h, p) else {
            break;
        };
        let Some(m0) = c.get(0) else { break };
        if p < m0.start() {
            r.extend_from_slice(&h[p..m0.start()]);
        }
        if Some(m0.start()) == lastend && m0.is_empty() {
            if p < h.len() {
                let n = step(h, p, bytes_kind);
                r.extend_from_slice(&h[p..n.min(h.len())]);
                p = n;
            } else {
                p += 1;
            }
            continue;
        }
        for piece in &pieces {
            match piece {
                Piece::Lit(l) => r.extend_from_slice(l),
                Piece::Group(g) => {
                    if let Some(m) = c.get(*g) {
                        r.extend_from_slice(m.as_bytes());
                    }
                }
            }
        }
        p = m0.end();
        lastend = Some(p);
        if r.len() > MAX_OUT {
            return Err(out_of_range(
                "REGEXP_REPLACE: exceeded maximum output length",
            ));
        }
    }
    if p < h.len() {
        r.extend_from_slice(&h[p..]);
    }
    Ok(out(bytes_kind, r))
}

// ---- FORMAT

fn c_escape(x: &[u8], quote: Option<u8>, utf8: bool) -> String {
    let mut r = String::with_capacity(x.len() + 2);
    let mut k = 0;
    while k < x.len() {
        let c = x[k];
        match c {
            b'\n' => r.push_str("\\n"),
            b'\r' => r.push_str("\\r"),
            b'\t' => r.push_str("\\t"),
            b'\\' => r.push_str("\\\\"),
            b'"' | b'\'' if quote.is_none_or(|q| q == c) => {
                r.push('\\');
                r.push(c as char);
            }
            0x20..=0x7e => r.push(c as char),
            0x80..=0xff if utf8 => {
                // Copy the whole UTF-8 sequence.
                let mut e = k + 1;
                while e < x.len() && (x[e] & 0xC0) == 0x80 {
                    e += 1;
                }
                r.push_str(&String::from_utf8_lossy(&x[k..e]));
                k = e;
                continue;
            }
            _ => r.push_str(&format!("\\x{c:02x}")),
        }
        k += 1;
    }
    r
}

fn string_literal(x: &str) -> String {
    let q = if x.contains('"') && !x.contains('\'') {
        b'\''
    } else {
        b'"'
    };
    let qc = q as char;
    format!("{qc}{}{qc}", c_escape(x.as_bytes(), Some(q), true))
}

fn float_literal(t: String) -> String {
    if t.contains(['.', 'e', 'n', 'i']) {
        t
    } else {
        format!("{t}.0")
    }
}

/// GoogleSQL's interval text: `[-]Y-M [-]D [-]H:M:S[.fff]` (CAST AS STRING, FORMAT).
pub(crate) fn interval_text(iv: &spanbolt_storage::Interval) -> String {
    let m = iv.months;
    let sign = if m < 0 { "-" } else { "" };
    let ma = m.unsigned_abs();
    let n = iv.nanos;
    let nsign = if n < 0 { "-" } else { "" };
    let na = n.unsigned_abs();
    let secs = na / 1_000_000_000;
    let frac = na % 1_000_000_000;
    let mut r = format!(
        "{sign}{}-{} {} {nsign}{}:{}:{}",
        ma / 12,
        ma % 12,
        iv.days,
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    );
    if frac != 0 {
        let f = format!("{frac:09}");
        let digits = if frac.is_multiple_of(1_000_000) {
            3
        } else if frac.is_multiple_of(1000) {
            6
        } else {
            9
        };
        r.push('.');
        r.push_str(&f[..digits]);
    }
    r
}

/// FORMAT's `%t` (`literal == false`) and `%T` text of a value.
fn value_text(v: &Value, literal: bool) -> Result<String> {
    Ok(match v {
        Value::Null => "NULL".into(),
        Value::Bool(x) => x.to_string(),
        Value::Int64(x) => x.to_string(),
        Value::Float64(x) => {
            if literal && !x.is_finite() {
                format!("CAST(\"{}\" AS FLOAT64)", crate::cast::format_f64(*x))
            } else if literal {
                float_literal(crate::cast::format_f64(*x))
            } else {
                crate::cast::format_f64(*x)
            }
        }
        Value::Float32(x) => {
            if literal && !x.is_finite() {
                format!("CAST(\"{}\" AS FLOAT32)", crate::cast::format_f32(*x))
            } else if literal {
                float_literal(crate::cast::format_f32(*x))
            } else {
                crate::cast::format_f32(*x)
            }
        }
        Value::Numeric(n) => {
            let t = crate::cast::numeric_string(*n);
            if literal {
                format!("NUMERIC \"{t}\"")
            } else {
                t
            }
        }
        Value::String(x) => {
            if literal {
                string_literal(x)
            } else {
                x.to_string()
            }
        }
        Value::Bytes(x) => {
            if literal {
                format!("b\"{}\"", c_escape(x, Some(b'"'), false))
            } else {
                c_escape(x, None, false)
            }
        }
        Value::Date(d) => {
            let t = crate::time::format_date(*d);
            if literal { format!("DATE \"{t}\"") } else { t }
        }
        Value::Timestamp(t) => {
            let t = crate::time::format_ts(*t, chrono_tz::Tz::UTC);
            if literal {
                format!("TIMESTAMP \"{t}\"")
            } else {
                t
            }
        }
        Value::Json(j) => {
            if literal {
                format!("JSON {}", string_literal(j))
            } else {
                j.to_string()
            }
        }
        Value::Interval(iv) => {
            let t = interval_text(iv);
            if literal {
                format!("INTERVAL \"{t}\" YEAR TO SECOND")
            } else {
                t
            }
        }
        Value::Array(xs) => {
            let mut parts = Vec::with_capacity(xs.len());
            for x in xs.iter() {
                parts.push(value_text(x, literal)?);
            }
            format!("[{}]", parts.join(", "))
        }
        Value::Struct(xs) => {
            let mut parts = Vec::with_capacity(xs.len());
            for x in xs.iter() {
                parts.push(value_text(x, literal)?);
            }
            let body = parts.join(", ");
            if literal && xs.len() < 2 {
                format!("STRUCT({body})")
            } else {
                format!("({body})")
            }
        }
        Value::Uuid(_) | Value::TokenList(_) => {
            return Err(crate::types::unimplemented("FORMAT of this type"));
        }
    })
}

#[derive(Default, Clone, Copy)]
struct Spec {
    minus: bool,
    plus: bool,
    space: bool,
    hash: bool,
    zero: bool,
    group: bool,
    width: Option<usize>,
    prec: Option<usize>,
}

fn group_digits(int: &str) -> String {
    let n = int.len();
    let mut r = String::with_capacity(n + n / 3);
    for (k, c) in int.chars().enumerate() {
        if k > 0 && (n - k).is_multiple_of(3) {
            r.push(',');
        }
        r.push(c);
    }
    r
}

/// Applies sign, grouping, `#`, zero padding and width to a formatted number's magnitude.
fn finish_number(sp: &Spec, neg: bool, body: String, prefix: &str, numeric: bool) -> String {
    let mut body = body;
    if sp.group && numeric {
        let (int, rest) = match body.find(|c: char| !c.is_ascii_digit()) {
            Some(k) => (body[..k].to_string(), body[k..].to_string()),
            None => (body.clone(), String::new()),
        };
        body = format!("{}{rest}", group_digits(&int));
    }
    let sign = if neg {
        "-"
    } else if sp.plus {
        "+"
    } else if sp.space {
        " "
    } else {
        ""
    };
    let lead = format!("{sign}{prefix}");
    let len = lead.chars().count() + body.chars().count();
    match sp.width {
        Some(w) if w > len => {
            let fill = w - len;
            if sp.minus {
                format!("{lead}{body}{}", " ".repeat(fill))
            } else if sp.zero && numeric {
                format!("{lead}{}{body}", "0".repeat(fill))
            } else {
                format!("{}{lead}{body}", " ".repeat(fill))
            }
        }
        _ => format!("{lead}{body}"),
    }
}

fn pad_text(sp: &Spec, t: String) -> String {
    let t = match sp.prec {
        Some(p) => t.chars().take(p).collect(),
        None => t,
    };
    let len = t.chars().count();
    match sp.width {
        Some(w) if w > len => {
            if sp.minus {
                format!("{t}{}", " ".repeat(w - len))
            } else {
                format!("{}{t}", " ".repeat(w - len))
            }
        }
        _ => t,
    }
}

/// C's `%e` body for a finite non-negative value.
fn c_exp(x: f64, prec: usize, upper: bool) -> String {
    let e = format!("{x:.prec$e}");
    let (m, ex) = e.split_once('e').unwrap_or((&e, "0"));
    let ex: i32 = ex.parse().unwrap_or(0);
    let sign = if ex < 0 { '-' } else { '+' };
    let r = format!("{m}e{sign}{:02}", ex.abs());
    if upper { r.to_uppercase() } else { r }
}

fn strip_zeros(t: &str) -> String {
    if !t.contains('.') {
        return t.to_string();
    }
    let t = t.trim_end_matches('0');
    t.trim_end_matches('.').to_string()
}

/// C's `%g` body for a finite non-negative value.
fn c_g(x: f64, prec: usize, upper: bool, hash: bool) -> String {
    let p = if prec == 0 { 1 } else { prec };
    if x == 0.0 {
        return if hash {
            format!("{:.*}", p - 1, 0.0)
        } else {
            "0".into()
        };
    }
    let e = format!("{x:.*e}", p - 1);
    let ex: i32 = e
        .split_once('e')
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if ex < -4 || ex >= p as i32 {
        let r = c_exp(x, p - 1, upper);
        if hash {
            return r;
        }
        let (m, rest) = r.split_at(r.find(['e', 'E']).unwrap_or(r.len()));
        format!("{}{rest}", strip_zeros(m))
    } else {
        let decimals = (p as i32 - 1 - ex).max(0) as usize;
        let r = format!("{x:.decimals$}");
        if hash { r } else { strip_zeros(&r) }
    }
}

/// Exact `%.Nf` of a NUMERIC (half away from zero).
fn numeric_fixed(n: i128, prec: usize) -> String {
    let a = n.unsigned_abs();
    let scale: u128 = 1_000_000_000;
    let (mut int, mut frac) = (a / scale, a % scale);
    let frac_str = if prec >= 9 {
        format!("{frac:09}{}", "0".repeat(prec - 9))
    } else {
        let unit = 10u128.pow(9 - prec as u32);
        let mut q = frac / unit;
        if (frac % unit) * 2 >= unit {
            q += 1;
        }
        if q == 10u128.pow(prec as u32) {
            q = 0;
            int += 1;
        }
        frac = q;
        if prec == 0 {
            String::new()
        } else {
            format!("{frac:0prec$}")
        }
    };
    if frac_str.is_empty() {
        int.to_string()
    } else {
        format!("{int}.{frac_str}")
    }
}

fn format_float(sp: &Spec, conv: char, v: &Value) -> Result<String> {
    let prec = sp.prec.unwrap_or(6);
    let (x, numeric) = match v {
        Value::Float64(x) => (*x, None),
        Value::Float32(x) => (*x as f64, None),
        Value::Numeric(n) => (crate::cast::numeric_to_f64(*n), Some(n.0)),
        _ => {
            return Err(out_of_range(
                "Invalid type for FORMAT floating point specifier",
            ));
        }
    };
    let upper = conv.is_ascii_uppercase();
    if x.is_nan() || x.is_infinite() {
        let t = if x.is_nan() { "nan" } else { "inf" };
        let t = if upper {
            t.to_uppercase()
        } else {
            t.to_string()
        };
        let sp2 = Spec { zero: false, ..*sp };
        return Ok(finish_number(&sp2, x < 0.0, t, "", true));
    }
    let neg = numeric.map_or(x < 0.0, |n| n < 0);
    let a = x.abs();
    let body = match conv {
        'f' | 'F' => match numeric {
            Some(n) => numeric_fixed(n, prec),
            None => {
                let r = format!("{a:.prec$}");
                if sp.hash && prec == 0 {
                    format!("{r}.")
                } else {
                    r
                }
            }
        },
        'e' | 'E' => c_exp(a, prec, upper),
        _ => c_g(a, prec, upper, sp.hash),
    };
    Ok(finish_number(sp, neg, body, "", true))
}

fn format_int(sp: &Spec, conv: char, x: i64) -> String {
    let a = x.unsigned_abs();
    let (body, prefix) = match conv {
        'x' => (format!("{a:x}"), if sp.hash && a != 0 { "0x" } else { "" }),
        'X' => (format!("{a:X}"), if sp.hash && a != 0 { "0X" } else { "" }),
        'o' => (format!("{a:o}"), if sp.hash { "0" } else { "" }),
        _ => (a.to_string(), ""),
    };
    let body = match sp.prec {
        Some(p) if body.len() < p => format!("{}{body}", "0".repeat(p - body.len())),
        _ => body,
    };
    let sp2 = Spec {
        group: sp.group && matches!(conv, 'd' | 'i'),
        zero: sp.zero && sp.prec.is_none(),
        ..*sp
    };
    finish_number(&sp2, x < 0, body, prefix, true)
}

/// GoogleSQL's short type name in FORMAT's type errors (`ShortTypeName`).
fn format_type_name(t: &Type) -> String {
    match t {
        Type::Array(_) => "ARRAY".into(),
        Type::Struct(_) => "STRUCT".into(),
        Type::Proto(name) => name.clone(),
        t => crate::types::gsql_name(t),
    }
}

/// Checks a FORMAT pattern against its arguments' types as GoogleSQL's
/// `StringFormatEvaluator::SetPattern` does, which the emulator runs for a pattern that isn't a
/// literal (the analyser runs it for a literal): the first error in pattern order, then the
/// argument count. `types` are the declared types of the arguments after the pattern.
fn format_check(pat: &str, types: &[Type]) -> Result<()> {
    let b = pat.as_bytes();
    let invalid = || out_of_range(format!("Invalid FORMAT string: \"{pat}\""));
    let type_error = |i: usize, want: &str| {
        out_of_range(format!(
            "Invalid type for argument {} to FORMAT; Expected {want}; Got {}",
            i + 2,
            format_type_name(&types[i])
        ))
    };
    let int = |i: usize| -> Result<()> {
        match types.get(i) {
            Some(Type::Int64) | None => Ok(()),
            Some(_) => Err(type_error(i, "integer")),
        }
    };
    let (mut k, mut next) = (0usize, 0usize);
    while let Some(off) = b[k..].iter().position(|&c| c == b'%') {
        k += off + 1;
        let flags = k;
        while k < b.len() && b"-+ #'0".contains(&b[k]) {
            k += 1;
        }
        // The grouping flag `'` is dropped before `%%` is checked.
        let flagged = b[flags..k].iter().any(|&c| c != b'\'');
        let star_width = b.get(k) == Some(&b'*');
        let width = k;
        if star_width {
            k += 1;
        } else {
            while k < b.len() && b[k].is_ascii_digit() {
                k += 1;
            }
        }
        let has_width = k > width;
        let mut star_prec = false;
        let has_prec = b.get(k) == Some(&b'.');
        if has_prec {
            k += 1;
            if b.get(k) == Some(&b'*') {
                star_prec = true;
                k += 1;
            } else {
                while k < b.len() && b[k].is_ascii_digit() {
                    k += 1;
                }
            }
        }
        // The specifier is one character (UTF-8 aware, for the error message).
        let Some(spec) = pat[k..].chars().next() else {
            return Err(invalid());
        };
        k += spec.len_utf8();
        if spec == '%' {
            if flagged || has_width || has_prec {
                return Err(invalid());
            }
            continue;
        }
        if star_width {
            if next < types.len() {
                int(next)?;
            }
            next += 1;
        }
        if star_prec {
            if next < types.len() {
                int(next)?;
            }
            next += 1;
        }
        if next < types.len() {
            let t = &types[next];
            match spec {
                'd' | 'i' | 'o' | 'x' | 'X' => int(next)?,
                'u' => return Err(type_error(next, "UINT")),
                'f' | 'F' | 'e' | 'E' | 'g' | 'G' => {
                    if !matches!(t, Type::Float32 | Type::Float64 | Type::Numeric) {
                        return Err(type_error(next, "FLOAT64"));
                    }
                }
                's' => {
                    if !matches!(t, Type::String(_)) {
                        return Err(type_error(next, "STRING"));
                    }
                }
                'p' | 'P' => {
                    if !matches!(t, Type::Json | Type::Proto(_)) {
                        return Err(type_error(next, "PROTO or JSON"));
                    }
                    return Err(crate::types::unimplemented("FORMAT %p and %P"));
                }
                't' | 'T' => {}
                _ => {
                    return Err(out_of_range(format!(
                        "Invalid format specifier character \"{spec}\" in FORMAT string: \"{pat}\""
                    )));
                }
            }
        }
        next += 1;
    }
    if next != types.len() {
        let which = if next > types.len() { "few" } else { "many" };
        return Err(out_of_range(format!(
            "Too {which} arguments to FORMAT for pattern \"{pat}\"; Expected {}; Got {}",
            next + 1,
            types.len() + 1
        )));
    }
    Ok(())
}

/// Cloud Spanner's runtime check of a FORMAT pattern that wasn't a literal: the pattern is parsed
/// first, then the argument count, then each argument's specifier and type, in Spanner's words
/// (arguments counted from 1 after the pattern, messages ending in a period).
fn format_check_spanner(pat: &str, types: &[Type]) -> Result<()> {
    let b = pat.as_bytes();
    let invalid = || {
        out_of_range(format!(
            "Invalid format string provided to FORMAT; '{pat}'."
        ))
    };
    // What each argument must be: `None` for `*`, else the specifier it fills.
    let mut want: Vec<Option<char>> = Vec::new();
    let mut k = 0usize;
    while let Some(off) = b[k..].iter().position(|&c| c == b'%') {
        k += off + 1;
        let flags = k;
        while k < b.len() && b"-+ #'0".contains(&b[k]) {
            k += 1;
        }
        let flagged = b[flags..k].iter().any(|&c| c != b'\'');
        let width = k;
        if b.get(k) == Some(&b'*') {
            want.push(None);
            k += 1;
        } else {
            while k < b.len() && b[k].is_ascii_digit() {
                k += 1;
            }
        }
        let has_width = k > width;
        let has_prec = b.get(k) == Some(&b'.');
        if has_prec {
            k += 1;
            if b.get(k) == Some(&b'*') {
                want.push(None);
                k += 1;
            } else {
                while k < b.len() && b[k].is_ascii_digit() {
                    k += 1;
                }
            }
        }
        let Some(spec) = pat[k..].chars().next() else {
            return Err(invalid());
        };
        k += spec.len_utf8();
        if spec == '%' {
            if flagged || has_width || has_prec {
                return Err(invalid());
            }
            continue;
        }
        want.push(Some(spec));
    }
    if want.len() != types.len() {
        return Err(out_of_range(format!(
            "Wrong number of arguments provided to FORMAT; {} arguments provided, but pattern requires {}.",
            types.len(),
            want.len()
        )));
    }
    for (i, (w, t)) in want.iter().zip(types).enumerate() {
        let expected = match w {
            None => (!matches!(t, Type::Int64)).then_some("int64"),
            Some('d' | 'i' | 'o' | 'x' | 'X') => (!matches!(t, Type::Int64)).then_some("integer"),
            Some('u') => Some("unsigned integer"),
            Some('f' | 'F' | 'e' | 'E' | 'g' | 'G') => {
                (!matches!(t, Type::Float32 | Type::Float64 | Type::Numeric))
                    .then_some("floating point or numeric")
            }
            Some('s') => (!matches!(t, Type::String(_))).then_some("string"),
            Some('p' | 'P') => return Err(crate::types::unimplemented("FORMAT %p and %P")),
            Some('t' | 'T') => None,
            Some(c) => {
                return Err(out_of_range(format!(
                    "Invalid format string provided to FORMAT; unsupported format specifier %{c}."
                )));
            }
        };
        if let Some(want) = expected {
            return Err(out_of_range(format!(
                "Invalid type for argument {} to FORMAT; Expected {want}; Got {}.",
                i + 1,
                format_type_name(t)
            )));
        }
    }
    Ok(())
}

fn format_fn(a: &[Value], types: &[Type], emulator_compat: bool) -> Result<Value> {
    let Value::String(pat) = &a[0] else {
        return Ok(Value::Null);
    };
    let args = &a[1..];
    if emulator_compat {
        format_check(pat, types)?;
    } else {
        format_check_spanner(pat, types)?;
    }
    let mut next = 0usize;
    let mut take = || -> Result<&Value> {
        let v = args.get(next).ok_or_else(|| {
            out_of_range(format!(
                "Too few arguments to FORMAT for pattern \"{pat}\"; Expected {}; Got {}",
                next + 2,
                a.len()
            ))
        })?;
        next += 1;
        Ok(v)
    };
    let invalid = || out_of_range(format!("Invalid FORMAT string: \"{pat}\""));
    let mut r = String::new();
    let mut it = pat.chars().peekable();
    let mut null = false;
    while let Some(c) = it.next() {
        if c != '%' {
            r.push(c);
            continue;
        }
        if it.peek() == Some(&'%') {
            it.next();
            r.push('%');
            continue;
        }
        let mut sp = Spec::default();
        while let Some(&fl) = it.peek() {
            match fl {
                '-' => sp.minus = true,
                '+' => sp.plus = true,
                ' ' => sp.space = true,
                '#' => sp.hash = true,
                '0' => sp.zero = true,
                '\'' => sp.group = true,
                _ => break,
            }
            it.next();
        }
        let num = |it: &mut std::iter::Peekable<std::str::Chars>| -> Option<usize> {
            let mut d = String::new();
            while let Some(&c) = it.peek() {
                if c.is_ascii_digit() {
                    d.push(c);
                    it.next();
                } else {
                    break;
                }
            }
            d.parse().ok()
        };
        if it.peek() == Some(&'*') {
            it.next();
            match take()? {
                Value::Int64(w) => {
                    if *w < 0 {
                        sp.minus = true;
                    }
                    sp.width = Some(w.unsigned_abs() as usize);
                }
                _ => null = true,
            }
        } else {
            sp.width = num(&mut it);
        }
        if it.peek() == Some(&'.') {
            it.next();
            if it.peek() == Some(&'*') {
                it.next();
                match take()? {
                    Value::Int64(p) => sp.prec = (*p >= 0).then_some(*p as usize),
                    _ => null = true,
                }
            } else {
                sp.prec = Some(num(&mut it).unwrap_or(0));
            }
        }
        let conv = it.next().ok_or_else(invalid)?;
        if !matches!(
            conv,
            'd' | 'i' | 'x' | 'X' | 'o' | 'f' | 'F' | 'e' | 'E' | 'g' | 'G' | 's' | 't' | 'T'
        ) {
            if conv == '%' {
                // `%'%`: the grouping flag on an escaped `%` (format_check rejects other flags).
                r.push('%');
                continue;
            }
            return Err(out_of_range(format!(
                "Invalid format specifier character \"{conv}\" in FORMAT string: \"{pat}\""
            )));
        }
        let v = take()?;
        if v.is_null() && !matches!(conv, 't' | 'T') {
            null = true;
            continue;
        }
        let piece = match conv {
            'd' | 'i' | 'x' | 'X' | 'o' => match v {
                Value::Int64(x) => format_int(&sp, conv, *x),
                _ => return Err(out_of_range("Invalid type for FORMAT integer specifier")),
            },
            'f' | 'F' | 'e' | 'E' | 'g' | 'G' => format_float(&sp, conv, v)?,
            's' => match v {
                Value::String(x) => pad_text(&sp, x.to_string()),
                _ => return Err(out_of_range("Invalid type for FORMAT %s specifier")),
            },
            _ => pad_text(&sp, value_text(v, conv == 'T')?),
        };
        r.push_str(&piece);
    }
    if next < args.len() {
        return Err(out_of_range(format!(
            "Too many arguments to FORMAT for pattern \"{pat}\"; Expected {}; Got {}",
            next + 1,
            a.len()
        )));
    }
    Ok(if null { Value::Null } else { st(r) })
}

pub fn lookup(sig: &Sig) -> Option<ScalarFn> {
    let bytes_kind = is_bytes(sig);
    let n = sig.args.len();
    Some(match sig.name {
        "concat" => strict(move |a| {
            let mut r = Vec::new();
            for v in a {
                r.extend_from_slice(b(v));
            }
            Ok(out(bytes_kind, r))
        }),
        "lower" => strict(|a| match &a[0] {
            Value::String(x) => Ok(st(x.to_lowercase())),
            Value::Bytes(x) => Ok(by(x.to_ascii_lowercase())),
            _ => Ok(Value::Null),
        }),
        "upper" => strict(|a| match &a[0] {
            Value::String(x) => Ok(st(x.to_uppercase())),
            Value::Bytes(x) => Ok(by(x.to_ascii_uppercase())),
            _ => Ok(Value::Null),
        }),
        "length" | "char_length" | "character_length" => strict(|a| {
            Ok(Value::Int64(match &a[0] {
                Value::String(x) => x.chars().count() as i64,
                v => b(v).len() as i64,
            }))
        }),
        "byte_length" | "octet_length" => strict(|a| Ok(Value::Int64(b(&a[0]).len() as i64))),
        "substr" | "substring" => strict(substr),
        "starts_with" => strict(|a| Ok(Value::Bool(b(&a[0]).starts_with(b(&a[1]))))),
        "ends_with" => strict(|a| Ok(Value::Bool(b(&a[0]).ends_with(b(&a[1]))))),
        "strpos" => strict(strpos),
        "replace" => strict(replace),
        "trim" => trim(Side::Both),
        "ltrim" => trim(Side::Left),
        "rtrim" => trim(Side::Right),
        "lpad" => pad(true),
        "rpad" => pad(false),
        "repeat" => strict(repeat),
        "reverse" => strict(|a| match &a[0] {
            Value::String(x) => Ok(st(x.chars().rev().collect::<String>())),
            Value::Bytes(x) => Ok(by(x.iter().rev().copied().collect::<Vec<u8>>())),
            _ => Ok(Value::Null),
        }),
        "split" => strict(split),
        "soundex" => strict(|a| Ok(st(soundex(s(&a[0]))))),
        "to_code_points" => strict(|a| {
            let v: Vec<Value> = match &a[0] {
                Value::String(x) => x.chars().map(|c| Value::Int64(c as i64)).collect(),
                v => b(v).iter().map(|c| Value::Int64(*c as i64)).collect(),
            };
            Ok(Value::Array(Arc::from(v)))
        }),
        "code_points_to_string" => strict(code_points_to_string),
        "code_points_to_bytes" => strict(code_points_to_bytes),
        "safe_convert_bytes_to_string" => strict(|a| Ok(st(lossy_per_byte(b(&a[0]))))),
        "to_base64" => strict(|a| {
            Ok(st(
                base64::engine::general_purpose::STANDARD.encode(b(&a[0]))
            ))
        }),
        "from_base64" => strict(|a| from_base64(s(&a[0]))),
        "to_hex" => strict(|a| Ok(st(to_hex(b(&a[0]))))),
        "from_hex" => strict(|a| from_hex(s(&a[0]))),
        "normalize" => strict(|a| {
            let mode = a.get(1).map(i).unwrap_or(0);
            normalize(s(&a[0]), mode)
                .map(st)
                .ok_or_else(|| crate::types::unimplemented("NORMALIZE mode"))
        }),
        "normalize_and_casefold" => strict(|a| {
            let mode = a.get(1).map(i).unwrap_or(0);
            let folded = casefold(s(&a[0])).ok_or_else(|| {
                crate::types::unimplemented("NORMALIZE_AND_CASEFOLD of these characters")
            })?;
            normalize(&folded, mode)
                .map(st)
                .ok_or_else(|| crate::types::unimplemented("NORMALIZE mode"))
        }),
        "string" if matches!(sig.args.first(), Some(Type::Timestamp)) => strict(|a| {
            let Value::Timestamp(t) = &a[0] else {
                return Ok(Value::Null);
            };
            let zone = match a.get(1) {
                Some(z) => Zone::parse(s(z))?,
                None => Zone::default(),
            };
            Ok(st(zone.format_ts(*t)))
        }),
        "error" => f(|_, a| match a.first() {
            Some(Value::String(m)) => Err(out_of_range(m.to_string())),
            _ => Err(out_of_range("ERROR function called with NULL value")),
        }),
        "generate_uuid" => f(|_, _| Ok(st(uuid_string(random_uuid()?)))),
        "new_uuid" => f(|_, _| Ok(Value::Uuid(random_uuid()?))),
        "format" => {
            let types = sig.args.get(1..).unwrap_or_default().to_vec();
            let compat = sig.emulator_compat;
            f(move |_, a| {
                if a.first().is_none_or(Value::is_null) {
                    return Ok(Value::Null);
                }
                format_fn(a, &types, compat)
            })
        }
        "regexp_contains" if n == 2 => regexp_fn("regexp_contains"),
        "regexp_extract" if n == 2 => regexp_fn("regexp_extract"),
        "regexp_extract_all" if n == 2 => regexp_fn("regexp_extract_all"),
        "regexp_replace" if n == 3 => regexp_fn("regexp_replace"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(x: &str) -> Value {
        st(x)
    }

    fn fmt(p: &str, args: Vec<Value>) -> String {
        fmt_mode(p, args, true)
    }

    fn fmt_mode(p: &str, args: Vec<Value>, emulator_compat: bool) -> String {
        let mut a = vec![sv(p)];
        a.extend(args);
        let types: Vec<Type> = a[1..]
            .iter()
            .map(|v| match v {
                Value::Int64(_) => Type::Int64,
                Value::Float64(_) => Type::Float64,
                _ => Type::String(None),
            })
            .collect();
        match format_fn(&a, &types, emulator_compat) {
            Ok(Value::String(x)) => x.to_string(),
            Ok(other) => format!("{other:?}"),
            Err(e) => e.message.to_string(),
        }
    }

    #[test]
    fn format_checks_like_cloud_spanner() {
        let spanner = |p: &str, args| fmt_mode(p, args, false);
        assert_eq!(spanner("%'%", vec![]), "%");
        assert_eq!(
            spanner("%5%%d", vec![Value::Int64(1)]),
            "Invalid format string provided to FORMAT; '%5%%d'."
        );
        assert_eq!(
            spanner("%a", vec![]),
            "Wrong number of arguments provided to FORMAT; 0 arguments provided, but pattern requires 1."
        );
        assert_eq!(
            spanner("%a", vec![Value::Int64(1)]),
            "Invalid format string provided to FORMAT; unsupported format specifier %a."
        );
        assert_eq!(
            spanner("%*d", vec![sv("a"), Value::Int64(1)]),
            "Invalid type for argument 1 to FORMAT; Expected int64; Got STRING."
        );
        assert_eq!(
            spanner("%d %d", vec![Value::Int64(1), sv("x")]),
            "Invalid type for argument 2 to FORMAT; Expected integer; Got STRING."
        );
        assert_eq!(spanner("%d-%s", vec![Value::Int64(1), sv("x")]), "1-x");
    }

    #[test]
    fn format_checks_like_googlesql() {
        assert_eq!(fmt("%'%", vec![]), "%");
        assert_eq!(fmt("%5%", vec![]), "Invalid FORMAT string: \"%5%\"");
        assert_eq!(
            fmt("%", vec![Value::Int64(1)]),
            "Invalid FORMAT string: \"%\""
        );
        assert_eq!(
            fmt("%a", vec![Value::Int64(1)]),
            "Invalid format specifier character \"a\" in FORMAT string: \"%a\""
        );
        // Without an argument left, the specifier isn't checked: the count is wrong first.
        assert_eq!(
            fmt("%a", vec![]),
            "Too few arguments to FORMAT for pattern \"%a\"; Expected 2; Got 1"
        );
        assert_eq!(
            fmt("%d", vec![sv("a")]),
            "Invalid type for argument 2 to FORMAT; Expected integer; Got STRING"
        );
        assert_eq!(
            fmt("%*d", vec![sv("a"), Value::Int64(1)]),
            "Invalid type for argument 2 to FORMAT; Expected integer; Got STRING"
        );
        assert_eq!(
            fmt("%f", vec![sv("a")]),
            "Invalid type for argument 2 to FORMAT; Expected FLOAT64; Got STRING"
        );
        assert_eq!(
            fmt("%s %s", vec![sv("a")]),
            "Too few arguments to FORMAT for pattern \"%s %s\"; Expected 3; Got 2"
        );
    }

    #[test]
    fn format_basics() {
        assert_eq!(fmt("%s-%d", vec![sv("a"), Value::Int64(5)]), "a-5");
        assert_eq!(
            fmt("%5d|%-5d|%05d", vec![Value::Int64(42); 3]),
            "   42|42   |00042"
        );
        assert_eq!(fmt("%'d", vec![Value::Int64(-1234567)]), "-1,234,567");
        assert_eq!(fmt("%.2f", vec![Value::Float64(1.23456)]), "1.23");
        assert_eq!(fmt("%10.3f|", vec![Value::Float64(1.23456)]), "     1.235|");
        assert_eq!(fmt("%e", vec![Value::Float64(31415.9)]), "3.141590e+04");
        assert_eq!(fmt("%g", vec![Value::Float64(0.0000123)]), "1.23e-05");
        assert_eq!(fmt("%G", vec![Value::Float64(1e20)]), "1E+20");
        assert_eq!(fmt("%#x", vec![Value::Int64(255)]), "0xff");
        assert_eq!(fmt("%x", vec![Value::Int64(-1)]), "-1");
        assert_eq!(fmt("%08.3f", vec![Value::Float64(-1.23456)]), "-001.235");
        assert_eq!(fmt("%T", vec![sv("a'b\"c")]), "\"a'b\\\"c\"");
        assert_eq!(fmt("%T", vec![Value::Float64(1.0)]), "1.0");
        assert_eq!(fmt("%T", vec![Value::Float64(1.5e20)]), "1.5e+20");
        assert_eq!(fmt("%10.4T|", vec![sv("abcdef")]), "      \"abc|");
        assert_eq!(fmt("%d%%", vec![Value::Int64(50)]), "50%");
    }

    fn re(p: &str) -> Result<Regex> {
        compile(&sv(p))
    }

    fn errmsg(p: &str) -> String {
        match re(p) {
            Err(e) => e.message.to_string(),
            Ok(_) => "ok".into(),
        }
    }

    #[test]
    fn regex_errors_match_re2() {
        assert_eq!(errmsg("("), "Cannot parse regular expression: missing ): (");
        assert_eq!(
            errmsg("a)"),
            "Cannot parse regular expression: unexpected ): a)"
        );
        assert_eq!(
            errmsg("[a"),
            "Cannot parse regular expression: missing ]: [a"
        );
        assert_eq!(
            errmsg("*a"),
            "Cannot parse regular expression: no argument for repetition operator: *"
        );
        assert_eq!(
            errmsg("a{2,1}"),
            "Cannot parse regular expression: invalid repetition size: {2,1}"
        );
        assert_eq!(
            errmsg("a{1001}"),
            "Cannot parse regular expression: invalid repetition size: {1001}"
        );
        assert_eq!(
            errmsg("(?=a)"),
            "Cannot parse regular expression: invalid perl operator: (?="
        );
        assert_eq!(
            errmsg("\\1"),
            "Cannot parse regular expression: invalid escape sequence: \\1"
        );
        assert_eq!(
            errmsg("a\\"),
            "Cannot parse regular expression: trailing \\"
        );
    }

    #[test]
    fn regex_ascii_classes() {
        let r = re("\\d").unwrap_or_else(|_| Regex::new("x").unwrap_or_else(|_| unreachable!()));
        assert!(!r.is_match("٣".as_bytes()));
        assert!(r.is_match(b"3"));
    }

    fn replace_all(h: &str, p: &str, rw: &str) -> String {
        match re(p).and_then(|r| regexp_replace(&r, h.as_bytes(), rw.as_bytes(), false)) {
            Ok(Value::String(x)) => x.to_string(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn regex_replace_empty_matches() {
        assert_eq!(replace_all("abc", "", "-"), "-a-b-c-");
        assert_eq!(replace_all("aaa", "a*", "x"), "x");
        assert_eq!(replace_all("abc", "b*", "-"), "-a-c-");
        assert_eq!(replace_all("héllo", "", "-"), "-h-é-l-l-o-");
        assert_eq!(
            replace_all("foo bar", "(o+) (b)", "\\2-\\1-\\0"),
            "fb-oo-oo bar"
        );
    }

    #[test]
    fn soundex_matches_googlesql() {
        assert_eq!(soundex("Robert"), "R163");
        assert_eq!(soundex("Tymczak"), "T520");
        assert_eq!(soundex("Pfister"), "P236");
        assert_eq!(soundex("aeiou"), "a000");
        assert_eq!(soundex("Ébert"), "b630");
    }
}
