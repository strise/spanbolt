//! JSON functions.

use std::sync::Arc;

use base64::Engine as _;
use spanbolt_storage::{Error, Result, Value};

use crate::cast::format_f64;
use crate::func::{ScalarFn, Sig, f, strict};
use crate::types::Type;
use crate::value_ops::out_of_range;

/// `json.field`: the member as JSON, or NULL when absent or not an object.
pub fn member(json: &str, name: &str) -> Value {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Object(m)) => match m.get(name) {
            Some(v) => Value::Json(Arc::from(v.to_string())),
            None => Value::Null,
        },
        _ => Value::Null,
    }
}

// ---- document model

/// A parsed JSON document. Numbers keep their literal text; objects keep member order and
/// duplicates (canonicalisation sorts them and keeps the first).
#[derive(Clone, Debug, PartialEq)]
enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

/// A JSON number as GoogleSQL's JSON type holds it.
#[derive(Clone, Copy, Debug)]
enum Num {
    Int(i64),
    UInt(u64),
    Float(f64),
}

fn is_int_literal(t: &str) -> bool {
    !t.contains(['.', 'e', 'E'])
}

/// Classifies a number literal; `None` when it is out of the double range.
fn classify(t: &str) -> Option<Num> {
    if is_int_literal(t) {
        if let Ok(i) = t.parse::<i64>() {
            return Some(Num::Int(i));
        }
        if let Ok(u) = t.parse::<u64>() {
            return Some(Num::UInt(u));
        }
    }
    let f: f64 = t.parse().ok()?;
    f.is_finite().then_some(Num::Float(f))
}

/// Shortest round-trip digits of `x` in `%g` layout (exponent from 1e15 or below 1e-4).
fn shortest_g(x: f64) -> String {
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    let e = format!("{x:e}");
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let sign = if neg { "-" } else { "" };
    if !(-4..15).contains(&exp) {
        let (h, t) = digits.split_at(1);
        let frac = if t.is_empty() {
            String::new()
        } else {
            format!(".{t}")
        };
        let es = if exp < 0 { '-' } else { '+' };
        format!("{sign}{h}{frac}e{es}{:02}", exp.abs())
    } else if exp < 0 {
        format!("{sign}0.{}{digits}", "0".repeat((-exp - 1) as usize))
    } else {
        let point = exp as usize + 1;
        if digits.len() <= point {
            format!("{sign}{digits}{}", "0".repeat(point - digits.len()))
        } else {
            format!("{sign}{}.{}", &digits[..point], &digits[point..])
        }
    }
}

/// The text GoogleSQL's JSON type prints for a double.
fn double_text(x: f64) -> String {
    let t = shortest_g(x);
    if t.bytes().all(|c| c.is_ascii_digit() || c == b'-') {
        format!("{t}.0")
    } else {
        t
    }
}

fn num_text(n: Num) -> String {
    match n {
        Num::Int(i) => i.to_string(),
        Num::UInt(u) => u.to_string(),
        Num::Float(f) => double_text(f),
    }
}

/// Decimal digits and exponent of a number literal, normalised (no leading/trailing zeros).
fn decimal_key(t: &str) -> Option<(bool, String, i64)> {
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t),
    };
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(k) => (&body[..k], body[k + 1..].parse::<i64>().ok()?),
        None => (body, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digits: String = format!("{int}{frac}");
    let exp = exp - frac.len() as i64;
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return Some((false, String::new(), 0));
    }
    let core = trimmed.trim_end_matches('0');
    let exp = exp + (trimmed.len() - core.len()) as i64;
    Some((neg, core.to_string(), exp))
}

fn round_trips(literal: &str, f: f64) -> bool {
    let shortest = format!("{f:e}");
    decimal_key(literal).is_some() && decimal_key(literal) == decimal_key(&shortest)
}

// ---- parser (nlohmann-style error messages)

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

type PResult<T> = std::result::Result<T, String>;

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Parser<'a> {
        Parser {
            s: s.as_bytes(),
            pos: 0,
        }
    }

    fn ws(&mut self) {
        while self.pos < self.s.len() && matches!(self.s[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn token_name(&self) -> String {
        match self.peek() {
            None => "end of input".into(),
            Some(b'{') => "'{'".into(),
            Some(b'}') => "'}'".into(),
            Some(b'[') => "'['".into(),
            Some(b']') => "']'".into(),
            Some(b',') => "','".into(),
            Some(b':') => "':'".into(),
            Some(b'"') => "string literal".into(),
            Some(b'-' | b'0'..=b'9') => "number literal".into(),
            Some(b't' | b'f' | b'n') => "literal".into(),
            Some(_) => "invalid literal".into(),
        }
    }

    fn unexpected(&self, ctx: &str, expected: &str) -> String {
        format!(
            "syntax error while parsing {ctx} - unexpected {}; expected {expected}",
            self.token_name()
        )
    }

    fn invalid_literal(&self, ctx: &str, start: usize) -> String {
        let end = (self.pos + 1).min(self.s.len());
        format!(
            "syntax error while parsing {ctx} - invalid literal; last read: '{}'",
            String::from_utf8_lossy(&self.s[start..end])
        )
    }

    fn value(&mut self, ctx: &str) -> PResult<J> {
        self.ws();
        match self.peek() {
            None => Err(if self.s.iter().all(u8::is_ascii_whitespace) {
                "attempting to parse an empty input; check that your input string or stream contains the expected JSON".into()
            } else {
                self.unexpected("value", "'[', '{', or a literal")
            }),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string(ctx).map(J::Str),
            Some(b'-' | b'0'..=b'9') => self.number(ctx),
            Some(b't') => self.word("true", J::Bool(true), ctx),
            Some(b'f') => self.word("false", J::Bool(false), ctx),
            Some(b'n') => self.word("null", J::Null, ctx),
            Some(b'}' | b']' | b',' | b':') => {
                Err(self.unexpected("value", "'[', '{', or a literal"))
            }
            Some(_) => Err(self.invalid_literal(ctx, self.pos)),
        }
    }

    fn word(&mut self, w: &str, v: J, ctx: &str) -> PResult<J> {
        let start = self.pos;
        for &c in w.as_bytes() {
            if self.peek() != Some(c) {
                return Err(self.invalid_literal(ctx, start));
            }
            self.pos += 1;
        }
        Ok(v)
    }

    fn number(&mut self, ctx: &str) -> PResult<J> {
        let start = self.pos;
        let s = self.s;
        let mut k = self.pos;
        if s.get(k) == Some(&b'-') {
            k += 1;
        }
        match s.get(k) {
            Some(b'0') => k += 1,
            Some(b'1'..=b'9') => {
                while s.get(k).is_some_and(u8::is_ascii_digit) {
                    k += 1;
                }
            }
            _ => {
                self.pos = k;
                return Err(self.invalid_literal(ctx, start));
            }
        }
        if s.get(k) == Some(&b'.') {
            k += 1;
            if !s.get(k).is_some_and(u8::is_ascii_digit) {
                self.pos = k;
                return Err(self.invalid_literal(ctx, start));
            }
            while s.get(k).is_some_and(u8::is_ascii_digit) {
                k += 1;
            }
        }
        if matches!(s.get(k), Some(b'e' | b'E')) {
            k += 1;
            if matches!(s.get(k), Some(b'+' | b'-')) {
                k += 1;
            }
            if !s.get(k).is_some_and(u8::is_ascii_digit) {
                self.pos = k;
                return Err(self.invalid_literal(ctx, start));
            }
            while s.get(k).is_some_and(u8::is_ascii_digit) {
                k += 1;
            }
        }
        self.pos = k;
        Ok(J::Num(String::from_utf8_lossy(&s[start..k]).into_owned()))
    }

    fn string(&mut self, ctx: &str) -> PResult<String> {
        let start = self.pos;
        self.pos += 1;
        let mut out: Vec<u8> = Vec::new();
        let last_read = |p: &Parser, end: usize| -> String {
            let mut t = String::new();
            for &c in &p.s[start..end.min(p.s.len())] {
                if c < 0x20 {
                    t.push_str(&format!("<U+{c:04X}>"));
                } else {
                    t.push(c as char);
                }
            }
            t
        };
        loop {
            let Some(c) = self.peek() else {
                return Err(format!(
                    "syntax error while parsing {ctx} - invalid string: missing closing quote; last read: '{}'",
                    last_read(self, self.pos)
                ));
            };
            match c {
                b'"' => {
                    self.pos += 1;
                    return String::from_utf8(out).map_err(|_| {
                        format!("syntax error while parsing {ctx} - invalid string: ill-formed UTF-8 byte")
                    });
                }
                b'\\' => {
                    let e = self.s.get(self.pos + 1).copied();
                    let simple = match e {
                        Some(b'"') => Some(b'"'),
                        Some(b'\\') => Some(b'\\'),
                        Some(b'/') => Some(b'/'),
                        Some(b'b') => Some(8),
                        Some(b'f') => Some(12),
                        Some(b'n') => Some(b'\n'),
                        Some(b'r') => Some(b'\r'),
                        Some(b't') => Some(b'\t'),
                        _ => None,
                    };
                    if let Some(ch) = simple {
                        out.push(ch);
                        self.pos += 2;
                        continue;
                    }
                    if e != Some(b'u') {
                        return Err(format!(
                            "syntax error while parsing {ctx} - invalid string: forbidden character after backslash; last read: '{}'",
                            last_read(self, self.pos + 2)
                        ));
                    }
                    let hex = |p: &Parser, at: usize| -> Option<u32> {
                        let h = p.s.get(at..at + 4)?;
                        u32::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()
                    };
                    let bad = |p: &Parser| {
                        format!(
                            "syntax error while parsing {ctx} - invalid string: '\\u' must be followed by 4 hex digits; last read: '{}'",
                            last_read(p, p.pos + 6)
                        )
                    };
                    let Some(hi) = hex(self, self.pos + 2) else {
                        return Err(bad(self));
                    };
                    self.pos += 6;
                    let cp = if (0xD800..0xDC00).contains(&hi) {
                        let lo = if self.s.get(self.pos..self.pos + 2) == Some(b"\\u") {
                            hex(self, self.pos + 2)
                        } else {
                            None
                        };
                        match lo {
                            Some(lo) if (0xDC00..0xE000).contains(&lo) => {
                                self.pos += 6;
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            }
                            _ => {
                                return Err(format!(
                                    "syntax error while parsing {ctx} - invalid string: surrogate U+D800..U+DBFF must be followed by U+DC00..U+DFFF; last read: '{}'",
                                    last_read(self, self.pos)
                                ));
                            }
                        }
                    } else {
                        hi
                    };
                    let ch = char::from_u32(cp).ok_or_else(|| {
                        format!(
                            "syntax error while parsing {ctx} - invalid string: surrogate U+DC00..U+DFFF must follow U+D800..U+DBFF; last read: '{}'",
                            last_read(self, self.pos)
                        )
                    })?;
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                c if c < 0x20 => {
                    let name = match c {
                        0 => "NUL",
                        8 => "BS",
                        9 => "HT",
                        10 => "LF",
                        12 => "FF",
                        13 => "CR",
                        _ => "",
                    };
                    let esc = match c {
                        8 => " or \\b",
                        9 => " or \\t",
                        10 => " or \\n",
                        12 => " or \\f",
                        13 => " or \\r",
                        _ => "",
                    };
                    let label = if name.is_empty() {
                        format!("U+{c:04X}")
                    } else {
                        format!("U+{c:04X} ({name})")
                    };
                    return Err(format!(
                        "syntax error while parsing {ctx} - invalid string: control character {label} must be escaped to \\u{c:04X}{esc}; last read: '{}'",
                        last_read(self, self.pos + 1)
                    ));
                }
                c => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    fn array(&mut self) -> PResult<J> {
        self.pos += 1;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(J::Arr(items));
        }
        loop {
            items.push(self.value("value")?);
            self.ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(J::Arr(items));
                }
                _ => return Err(self.unexpected("array", "']'")),
            }
        }
    }

    fn key(&mut self) -> PResult<String> {
        self.ws();
        match self.peek() {
            Some(b'"') => self.string("object key"),
            Some(b'-' | b'0'..=b'9' | b'{' | b'[' | b'}' | b']' | b',' | b':') | None => {
                Err(self.unexpected("object key", "string literal"))
            }
            Some(_) => {
                let start = self.pos.saturating_sub(1);
                Err(format!(
                    "{}; expected string literal",
                    self.invalid_literal("object key", start)
                ))
            }
        }
    }

    fn object(&mut self) -> PResult<J> {
        self.pos += 1;
        let mut members = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(J::Obj(members));
        }
        loop {
            let k = self.key()?;
            self.ws();
            if self.peek() != Some(b':') {
                return Err(self.unexpected("object separator", "':'"));
            }
            self.pos += 1;
            let v = self.value("value")?;
            members.push((k, v));
            self.ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(J::Obj(members));
                }
                _ => return Err(self.unexpected("object", "'}'")),
            }
        }
    }

    fn document(&mut self) -> PResult<J> {
        let v = self.value("value")?;
        self.ws();
        if self.pos < self.s.len() {
            return Err(self.unexpected("value", "end of input"));
        }
        Ok(v)
    }
}

fn parse(s: &str) -> PResult<J> {
    Parser::new(s).document()
}

/// Parses a stored JSON value (always valid).
fn load(v: &Value) -> Result<J> {
    match v {
        Value::Json(s) => parse(s).map_err(|e| Error::internal(format!("stored JSON: {e}"))),
        _ => Err(Error::internal("expected JSON")),
    }
}

// ---- canonical form and serialisation

#[derive(Clone, Copy, PartialEq)]
enum Wide {
    Exact,
    Round,
}

/// GoogleSQL's JSON value of a parsed document: typed numbers, sorted keys, first duplicate wins.
fn canonical(j: J, wide: Wide) -> PResult<J> {
    Ok(match j {
        J::Num(t) => {
            let n = classify(&t).ok_or_else(|| format!("number overflow parsing '{t}'"))?;
            if let Num::Float(f) = n
                && wide == Wide::Exact
                && !round_trips(&t, f)
            {
                return Err(format!(
                    "Input number: {t} cannot round-trip through string representation"
                ));
            }
            J::Num(num_text(n))
        }
        J::Arr(xs) => J::Arr(
            xs.into_iter()
                .map(|x| canonical(x, wide))
                .collect::<PResult<Vec<J>>>()?,
        ),
        J::Obj(ms) => {
            let mut out: Vec<(String, J)> = Vec::with_capacity(ms.len());
            for (k, v) in ms {
                if !out.iter().any(|(x, _)| *x == k) {
                    out.push((k, canonical(v, wide)?));
                }
            }
            out.sort_by(|a, b| a.0.cmp(&b.0));
            J::Obj(out)
        }
        other => other,
    })
}

fn escape_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write(out: &mut String, j: &J, indent: Option<usize>) {
    let nl = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    };
    match j {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Num(t) => out.push_str(t),
        J::Str(s) => escape_into(out, s),
        J::Arr(xs) => {
            out.push('[');
            if !xs.is_empty() {
                for (k, x) in xs.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    if let Some(d) = indent {
                        nl(out, d + 1);
                    }
                    write(out, x, indent.map(|d| d + 1));
                }
                if let Some(d) = indent {
                    nl(out, d);
                }
            }
            out.push(']');
        }
        J::Obj(ms) => {
            out.push('{');
            if !ms.is_empty() {
                for (k, (key, v)) in ms.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    if let Some(d) = indent {
                        nl(out, d + 1);
                    }
                    escape_into(out, key);
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }
                    write(out, v, indent.map(|d| d + 1));
                }
                if let Some(d) = indent {
                    nl(out, d);
                }
            }
            out.push('}');
        }
    }
}

fn text(j: &J) -> String {
    let mut s = String::new();
    write(&mut s, j, None);
    s
}

fn json_value(j: &J) -> Value {
    Value::Json(Arc::from(text(j)))
}

/// A JSON value from JSON text that serde wrote (`TO_JSON` of a proto).
pub(crate) fn from_serde(j: &serde_json::Value) -> Result<Value> {
    let s = j.to_string();
    parse(&s)
        .and_then(|j| canonical(j, Wide::Round))
        .map(|j| json_value(&j))
        .map_err(|e| Error::internal(format!("proto JSON: {e}")))
}

// ---- JSONPath

#[derive(Debug, Clone)]
enum Step {
    Key(String),
    Index(Option<usize>),
}

fn parse_path(p: &str) -> Result<Vec<Step>> {
    let rest = p
        .strip_prefix('$')
        .ok_or_else(|| out_of_range("JSONPath must start with '$'"))?;
    let invalid = |at: &str| out_of_range(format!("Invalid token in JSONPath at: {at}"));
    let mut steps = Vec::new();
    let mut r = rest;
    while !r.is_empty() {
        if let Some(after) = r.strip_prefix('.') {
            if after.starts_with('.') {
                return Err(out_of_range("Unsupported operator in JSONPath: .."));
            }
            if after.starts_with('*') {
                return Err(out_of_range("Unsupported operator in JSONPath: *"));
            }
            if let Some(q) = after.strip_prefix('"') {
                let end = q.find('"').ok_or_else(|| invalid(r))?;
                steps.push(Step::Key(q[..end].to_string()));
                r = &q[end + 1..];
                continue;
            }
            let end = after.find(['.', '[']).unwrap_or(after.len());
            if end == 0 {
                return Err(invalid(r));
            }
            steps.push(Step::Key(after[..end].to_string()));
            r = &after[end..];
        } else if let Some(after) = r.strip_prefix('[') {
            let inner = after.trim_start();
            if inner.starts_with('*') {
                return Err(out_of_range("Unsupported operator in JSONPath: *"));
            }
            let close = after.find(']').ok_or_else(|| invalid(r))?;
            let body = after[..close].trim();
            if body.is_empty() || !body.bytes().all(|c| c.is_ascii_digit() || c == b'-') {
                return Err(invalid(r));
            }
            let idx = if body.len() > 1 && body.starts_with('0') {
                None
            } else {
                body.parse::<usize>().ok()
            };
            steps.push(Step::Index(idx));
            r = &after[close + 1..];
        } else {
            return Err(invalid(r));
        }
    }
    Ok(steps)
}

fn walk<'j>(mut j: &'j J, path: &[Step]) -> Option<&'j J> {
    for s in path {
        j = match (s, j) {
            (Step::Key(k), J::Obj(ms)) => ms.iter().find(|(x, _)| x == k).map(|(_, v)| v)?,
            (Step::Key(k), J::Arr(xs)) => {
                let i: usize = k
                    .parse()
                    .ok()
                    .filter(|_| !(k.len() > 1 && k.starts_with('0')))?;
                xs.get(i)?
            }
            (Step::Index(Some(i)), J::Arr(xs)) => xs.get(*i)?,
            _ => return None,
        };
    }
    Some(j)
}

/// Streaming extraction from JSON text: stops as soon as the path's value is read, so input
/// after it may be malformed. `None` for no match or a parse error first.
fn extract_text(s: &str, path: &[Step]) -> Option<J> {
    fn go(p: &mut Parser, path: &[Step]) -> Option<J> {
        let Some(step) = path.first() else {
            return p.value("value").ok();
        };
        p.ws();
        match p.peek()? {
            b'{' => {
                p.pos += 1;
                p.ws();
                if p.peek() == Some(b'}') {
                    return None;
                }
                loop {
                    let k = p.key().ok()?;
                    p.ws();
                    if p.peek() != Some(b':') {
                        return None;
                    }
                    p.pos += 1;
                    if matches!(step, Step::Key(x) if *x == k) {
                        return go(p, &path[1..]);
                    }
                    p.value("value").ok()?;
                    p.ws();
                    match p.peek()? {
                        b',' => p.pos += 1,
                        _ => return None,
                    }
                }
            }
            b'[' => {
                let want = match step {
                    Step::Index(i) => (*i)?,
                    Step::Key(k) => {
                        if k.len() > 1 && k.starts_with('0') {
                            return None;
                        }
                        k.parse().ok()?
                    }
                };
                p.pos += 1;
                p.ws();
                if p.peek() == Some(b']') {
                    return None;
                }
                let mut i = 0usize;
                loop {
                    if i == want {
                        return go(p, &path[1..]);
                    }
                    p.value("value").ok()?;
                    p.ws();
                    match p.peek()? {
                        b',' => p.pos += 1,
                        _ => return None,
                    }
                    i += 1;
                }
            }
            _ => None,
        }
    }
    go(&mut Parser::new(s), path)
}

/// The value at `path` in a JSON or JSON-text argument, with whether it came from text.
fn locate(doc: &Value, path: &str) -> Result<Option<(J, bool)>> {
    let steps = parse_path(path)?;
    Ok(match doc {
        Value::Json(_) => walk(&load(doc)?, &steps).cloned().map(|j| (j, false)),
        Value::String(s) => extract_text(s, &steps).map(|j| (j, true)),
        _ => None,
    })
}

fn path_arg(a: &[Value], k: usize) -> Option<&str> {
    match a.get(k) {
        None => Some("$"),
        Some(Value::String(p)) => Some(p),
        Some(_) => None,
    }
}

/// JSON_VALUE's text of a scalar; `None` for null, arrays and objects.
fn scalar_text(j: &J, raw: bool) -> Option<String> {
    Some(match j {
        J::Str(s) => s.clone(),
        J::Bool(b) => b.to_string(),
        J::Num(t) if raw => t.clone(),
        J::Num(t) => num_text(classify(t)?),
        _ => return None,
    })
}

fn canonical_text(j: &J, raw: bool) -> String {
    if raw {
        text(j)
    } else {
        canonical(j.clone(), Wide::Round)
            .map(|c| text(&c))
            .unwrap_or_else(|_| text(j))
    }
}

fn query(a: &[Value], is_json: bool) -> Result<Value> {
    let Some(path) = path_arg(a, 1) else {
        return Ok(Value::Null);
    };
    Ok(match locate(&a[0], path)? {
        None => Value::Null,
        Some((j, raw)) => {
            if is_json {
                json_value(&j)
            } else {
                Value::String(Arc::from(canonical_text(&j, raw)))
            }
        }
    })
}

fn query_array(a: &[Value], is_json: bool, values: bool) -> Result<Value> {
    let Some(path) = path_arg(a, 1) else {
        return Ok(Value::Null);
    };
    let Some((J::Arr(xs), raw)) = locate(&a[0], path)? else {
        return Ok(Value::Null);
    };
    let mut out = Vec::with_capacity(xs.len());
    for x in &xs {
        out.push(if values {
            match x {
                J::Null => Value::Null,
                J::Arr(_) | J::Obj(_) => return Ok(Value::Null),
                s => match scalar_text(s, raw) {
                    Some(t) => Value::String(Arc::from(t)),
                    None => Value::Null,
                },
            }
        } else if is_json {
            json_value(x)
        } else {
            Value::String(Arc::from(canonical_text(x, raw)))
        });
    }
    Ok(Value::Array(Arc::from(out)))
}

// ---- conversions to JSON

fn timestamp_text(t: spanbolt_storage::Timestamp) -> String {
    let full = crate::time::format_ts(t, chrono_tz::Tz::UTC);
    // `2020-01-01 12:00:00.500+00` -> `2020-01-01T12:00:00.500Z`
    let body = full.strip_suffix("+00").unwrap_or(&full);
    format!("{}Z", body.replacen(' ', "T", 1))
}

fn numeric_json(n: spanbolt_storage::Numeric, stringify: bool) -> Option<J> {
    let t = crate::cast::numeric_string(n);
    if n.0 % 1_000_000_000 == 0
        && let Ok(i) = i64::try_from(n.0 / 1_000_000_000)
    {
        if stringify && !(-(1i64 << 53)..=(1i64 << 53)).contains(&i) {
            return Some(J::Str(t));
        }
        return Some(J::Num(i.to_string()));
    }
    let f = crate::cast::numeric_to_f64(n);
    if round_trips(&t, f) {
        return Some(J::Num(double_text(f)));
    }
    stringify.then_some(J::Str(t))
}

fn to_j(v: &Value, t: &Type, stringify: bool, ctx: &str) -> Result<J> {
    Ok(match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int64(i) => {
            if stringify && !(-(1i64 << 53)..=(1i64 << 53)).contains(i) {
                J::Str(i.to_string())
            } else {
                J::Num(i.to_string())
            }
        }
        Value::Float64(_) | Value::Float32(_) => {
            let x = match v {
                Value::Float64(x) => *x,
                Value::Float32(x) => *x as f64,
                _ => 0.0,
            };
            if x.is_nan() {
                J::Str("NaN".into())
            } else if x.is_infinite() {
                J::Str(if x > 0.0 { "Infinity" } else { "-Infinity" }.into())
            } else if x == 0.0 {
                J::Num("0.0".into())
            } else {
                J::Num(double_text(x))
            }
        }
        Value::Numeric(n) => numeric_json(*n, stringify)
            .ok_or_else(|| out_of_range(format!("{ctx}Failed to convert type NUMERIC to JSON")))?,
        Value::String(s) => J::Str(s.to_string()),
        Value::Bytes(b) => J::Str(base64::engine::general_purpose::STANDARD.encode(b)),
        Value::Date(d) => J::Str(crate::time::format_date(*d)),
        Value::Timestamp(ts) => J::Str(timestamp_text(*ts)),
        Value::Interval(iv) => {
            let mut s = String::new();
            iv.format(&mut s);
            J::Str(s)
        }
        Value::Uuid(u) => {
            let mut s = String::new();
            spanbolt_storage::value::format_uuid(*u, &mut s);
            J::Str(s)
        }
        Value::Json(_) => load(v)?,
        Value::Array(xs) => {
            let et = match t {
                Type::Array(e) => e.as_ref(),
                _ => t,
            };
            J::Arr(
                xs.iter()
                    .map(|x| to_j(x, et, stringify, ctx))
                    .collect::<Result<Vec<J>>>()?,
            )
        }
        Value::Struct(xs) => {
            let fields: &[(Option<String>, Type)] = match t {
                Type::Struct(fs) => fs,
                _ => &[],
            };
            let mut ms: Vec<(String, J)> = Vec::with_capacity(xs.len());
            for (k, x) in xs.iter().enumerate() {
                let (name, ft) = fields
                    .get(k)
                    .map(|(n, t)| (n.clone().unwrap_or_default(), t))
                    .unwrap_or((String::new(), &Type::Bool));
                let j = to_j(x, ft, stringify, ctx)?;
                if !ms.iter().any(|(n, _)| *n == name) {
                    ms.push((name, j));
                }
            }
            ms.sort_by(|a, b| a.0.cmp(&b.0));
            J::Obj(ms)
        }
        Value::TokenList(_) => {
            return Err(crate::types::unimplemented("TO_JSON of TOKENLIST"));
        }
    })
}

// ---- JSON to SQL values

fn not_a(what: &str) -> Error {
    out_of_range(format!("The provided JSON input is not {what}"))
}

fn wide_mode(a: &[Value], k: usize, msg: &str) -> Result<Option<Wide>> {
    match a.get(k) {
        None => Ok(Some(Wide::Round)),
        Some(Value::Null) => Ok(None),
        Some(Value::String(m)) => match m.as_ref() {
            "exact" => Ok(Some(Wide::Exact)),
            "round" => Ok(Some(Wide::Round)),
            other => Err(out_of_range(format!("{msg}{other}"))),
        },
        Some(_) => Ok(None),
    }
}

fn to_int64(j: &J) -> Result<Value> {
    let J::Num(t) = j else {
        return Err(not_a("an integer"));
    };
    match classify(t) {
        Some(Num::Int(i)) => Ok(Value::Int64(i)),
        Some(Num::Float(f)) => {
            if f.fract() == 0.0
                && (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&f)
            {
                Ok(Value::Int64(f as i64))
            } else {
                Err(out_of_range(format!(
                    "The provided JSON number: {} cannot be converted to an int64",
                    format_f64(f)
                )))
            }
        }
        _ => Err(not_a("an integer")),
    }
}

fn to_float64(j: &J, wide: Wide) -> Result<Value> {
    let J::Num(t) = j else {
        return Err(not_a("a number"));
    };
    let n = classify(t).ok_or_else(|| not_a("a number"))?;
    let (f, exact) = match n {
        Num::Int(i) => (i as f64, (i as f64) as i128 == i as i128),
        Num::UInt(u) => (
            u as f64,
            (u as f64) as u128 == u as u128 && u as f64 != 1.8446744073709552e19,
        ),
        Num::Float(f) => (f, true),
    };
    if wide == Wide::Exact && !exact {
        return Err(out_of_range(format!(
            "JSON number: {t} cannot be converted to FLOAT64 without loss of precision"
        )));
    }
    Ok(Value::Float64(f))
}

fn lax_int64(j: &J) -> Value {
    let from_f64 = |f: f64| -> Value {
        let r = f.round();
        if r.is_finite() && (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&r) {
            Value::Int64(r as i64)
        } else {
            Value::Null
        }
    };
    match j {
        J::Bool(b) => Value::Int64(*b as i64),
        J::Num(t) => match classify(t) {
            Some(Num::Int(i)) => Value::Int64(i),
            Some(Num::Float(f)) => from_f64(f),
            _ => Value::Null,
        },
        J::Str(s) => {
            let s = s.trim();
            if let Ok(i) = s.parse::<i64>() {
                return Value::Int64(i);
            }
            let numeric_like = !s.is_empty()
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'));
            match s.parse::<f64>() {
                Ok(f) if numeric_like => from_f64(f),
                _ => Value::Null,
            }
        }
        _ => Value::Null,
    }
}

fn lax_float64(j: &J) -> Value {
    match j {
        J::Num(t) => match classify(t) {
            Some(Num::Int(i)) => Value::Float64(i as f64),
            Some(Num::UInt(u)) => Value::Float64(u as f64),
            Some(Num::Float(f)) => Value::Float64(f),
            None => Value::Null,
        },
        J::Str(s) => {
            let s = s.trim();
            let lower = s.to_ascii_lowercase();
            let special = match lower.as_str() {
                "nan" | "+nan" | "-nan" => Some(f64::NAN),
                "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
                "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
                _ => None,
            };
            if let Some(x) = special {
                return Value::Float64(x);
            }
            let numeric_like = !s.is_empty()
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'));
            match s.parse::<f64>() {
                Ok(f) if numeric_like => Value::Float64(f),
                _ => Value::Null,
            }
        }
        _ => Value::Null,
    }
}

fn lax_string(j: &J) -> Value {
    match j {
        J::Str(s) => Value::String(Arc::from(s.as_str())),
        J::Bool(b) => Value::String(Arc::from(b.to_string())),
        J::Num(t) => match classify(t) {
            Some(Num::Float(f)) => {
                let s = if f == 0.0 {
                    "0".to_string()
                } else {
                    format_f64(f)
                };
                Value::String(Arc::from(s))
            }
            Some(n) => Value::String(Arc::from(num_text(n))),
            None => Value::Null,
        },
        _ => Value::Null,
    }
}

fn lax_bool(j: &J) -> Value {
    match j {
        J::Bool(b) => Value::Bool(*b),
        J::Str(s) => match s.to_ascii_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::Null,
        },
        J::Num(t) => match classify(t) {
            Some(Num::Int(i)) => Value::Bool(i != 0),
            Some(Num::UInt(u)) => Value::Bool(u != 0),
            Some(Num::Float(f)) => Value::Bool(f != 0.0),
            None => Value::Null,
        },
        _ => Value::Null,
    }
}

fn json_type(j: &J) -> &'static str {
    match j {
        J::Null => "null",
        J::Bool(_) => "boolean",
        J::Num(_) => "number",
        J::Str(_) => "string",
        J::Arr(_) => "array",
        J::Obj(_) => "object",
    }
}

fn key_part(k: &str) -> String {
    if k.contains(['.', '"', '[', ']']) {
        let mut s = String::new();
        escape_into(&mut s, k);
        s
    } else {
        k.to_string()
    }
}

fn json_keys(j: &J, prefix: &str, depth: i64, lax: bool, out: &mut Vec<String>) {
    if depth == 0 {
        return;
    }
    match j {
        J::Obj(ms) => {
            for (k, v) in ms {
                let p = if prefix.is_empty() {
                    key_part(k)
                } else {
                    format!("{prefix}.{}", key_part(k))
                };
                out.push(p.clone());
                json_keys(v, &p, depth - 1, lax, out);
            }
        }
        J::Arr(xs) if lax => {
            for x in xs {
                json_keys(x, prefix, depth, lax, out);
            }
        }
        _ => {}
    }
}

fn subscript(a: &[Value]) -> Result<Value> {
    let j = load(&a[0])?;
    Ok(match (&j, &a[1]) {
        (J::Arr(xs), Value::Int64(i)) => usize::try_from(*i)
            .ok()
            .and_then(|k| xs.get(k))
            .map_or(Value::Null, json_value),
        (J::Obj(ms), Value::String(k)) => ms
            .iter()
            .find(|(x, _)| x.as_str() == k.as_ref())
            .map_or(Value::Null, |(_, v)| json_value(v)),
        _ => Value::Null,
    })
}

fn json_object(a: &[Value], types: &[Type]) -> Result<Value> {
    let ctx = "Invalid input to JSON_OBJECT: ";
    let err = |m: &str| out_of_range(format!("{ctx}{m}"));
    let mut ms: Vec<(String, J)> = Vec::new();
    let mut add = |k: &Value, v: &Value, t: &Type| -> Result<()> {
        let Value::String(k) = k else {
            return Err(err("A key cannot be NULL"));
        };
        let j = to_j(v, t, false, ctx)?;
        if !ms.iter().any(|(x, _)| x.as_str() == k.as_ref()) {
            ms.push((k.to_string(), j));
        }
        Ok(())
    };
    if let [Type::Array(_), vt @ Type::Array(_)] = types {
        let Value::Array(keys) = &a[0] else {
            return Err(err("The keys array cannot be NULL"));
        };
        let Value::Array(vals) = &a[1] else {
            return Err(err("The values array cannot be NULL"));
        };
        if keys.len() != vals.len() {
            return Err(err("The number of keys and values must match"));
        }
        let et = match vt {
            Type::Array(e) => e.as_ref(),
            t => t,
        };
        for (k, v) in keys.iter().zip(vals.iter()) {
            add(k, v, et)?;
        }
    } else {
        for (k, pair) in a.chunks(2).enumerate() {
            let t = types.get(2 * k + 1).unwrap_or(&Type::Bool);
            match pair {
                [key, v] => add(key, v, t)?,
                _ => return Err(err("The number of keys and values must match")),
            }
        }
    }
    ms.sort_by(|x, y| x.0.cmp(&y.0));
    Ok(json_value(&J::Obj(ms)))
}

fn remove_at(j: &mut J, path: &[Step]) {
    let Some((last, init)) = path.split_last() else {
        return;
    };
    match (last, node_mut(j, init)) {
        (Step::Key(k), Some(J::Obj(ms))) => ms.retain(|(x, _)| x != k),
        (Step::Index(Some(i)), Some(J::Arr(xs))) if *i < xs.len() => {
            xs.remove(*i);
        }
        _ => {}
    }
}

/// JSON_STRIP_NULLS of one value; `None` removes it from its parent.
fn strip(j: J, arrays: bool, remove_empty: bool) -> Option<J> {
    match j {
        J::Null => None,
        J::Obj(ms) => {
            let ms: Vec<(String, J)> = ms
                .into_iter()
                .filter_map(|(k, v)| strip(v, arrays, remove_empty).map(|v| (k, v)))
                .collect();
            (!(remove_empty && ms.is_empty())).then_some(J::Obj(ms))
        }
        J::Arr(xs) if arrays => {
            let xs: Vec<J> = xs
                .into_iter()
                .filter_map(|x| strip(x, arrays, remove_empty))
                .collect();
            (!(remove_empty && xs.is_empty())).then_some(J::Arr(xs))
        }
        other => Some(other),
    }
}

fn strip_nulls(a: &[Value]) -> Result<Value> {
    let path = match a.get(1) {
        Some(Value::String(p)) => p.to_string(),
        _ => "$".into(),
    };
    let arrays = !matches!(a.get(2), Some(Value::Bool(false)));
    let remove_empty = matches!(a.get(3), Some(Value::Bool(true)));
    let steps = parse_path(&path)
        .map_err(|e| out_of_range(format!("Invalid input to JSON_STRIP_NULLS: {}", e.message)))?;
    let mut j = load(&a[0])?;
    let target = {
        let mut cur = Some(&mut j);
        for s in &steps {
            cur = match (s, cur) {
                (Step::Key(k), Some(J::Obj(ms))) => {
                    ms.iter_mut().find(|(x, _)| x == k).map(|(_, v)| v)
                }
                (Step::Index(Some(i)), Some(J::Arr(xs))) => xs.get_mut(*i),
                _ => None,
            };
        }
        cur
    };
    if let Some(t) = target {
        let v = std::mem::replace(t, J::Null);
        *t = strip(v, arrays, remove_empty).unwrap_or(J::Null);
    }
    Ok(json_value(&j))
}

// ---- JSON_SET, JSON_ARRAY_APPEND, JSON_ARRAY_INSERT, JSON_CONTAINS

/// The JSONPath of the JSON mutators: `$`, `.name`, `."quoted"` and `[n]` only. An object key on an
/// array, or an index on an object, matches nothing.
fn parse_mut_path(p: &str) -> Result<Vec<Step>> {
    let rest = p
        .strip_prefix('$')
        .ok_or_else(|| out_of_range("JSONPath must start with '$'"))?;
    let invalid = |at: &str| out_of_range(format!("Invalid token in JSONPath at: {at}"));
    let mut steps = Vec::new();
    let mut r = rest;
    while !r.is_empty() {
        if let Some(after) = r.strip_prefix('.') {
            if let Some(q) = after.strip_prefix('"') {
                let end = q.find('"').ok_or_else(|| invalid(r))?;
                steps.push(Step::Key(q[..end].to_string()));
                r = &q[end + 1..];
                continue;
            }
            let end = after
                .find(['.', '[', ']', '$', '*', '"', '\''])
                .unwrap_or(after.len());
            if end == 0 {
                return Err(invalid(r));
            }
            steps.push(Step::Key(after[..end].to_string()));
            r = &after[end..];
        } else if let Some(after) = r.strip_prefix('[') {
            let close = after.find(']').ok_or_else(|| invalid(r))?;
            let body = after[..close].trim();
            let idx = (!body.is_empty() && body.bytes().all(|c| c.is_ascii_digit()))
                .then(|| body.parse::<usize>().ok())
                .flatten()
                .ok_or_else(|| invalid(r))?;
            steps.push(Step::Index(Some(idx)));
            r = &after[close + 1..];
        } else {
            return Err(invalid(r));
        }
    }
    Ok(steps)
}

/// The existing node at `path`, without creating anything.
fn node_mut<'j>(mut j: &'j mut J, path: &[Step]) -> Option<&'j mut J> {
    for s in path {
        j = match (s, j) {
            (Step::Key(k), J::Obj(ms)) => ms.iter_mut().find(|(x, _)| x == k).map(|(_, v)| v)?,
            (Step::Index(Some(i)), J::Arr(xs)) => xs.get_mut(*i)?,
            _ => return None,
        };
    }
    Some(j)
}

/// Inserts a member keeping the canonical (sorted) key order.
fn insert_member(ms: &mut Vec<(String, J)>, k: &str, v: J) {
    let at = ms.partition_point(|(x, _)| x.as_str() < k);
    ms.insert(at, (k.to_string(), v));
}

/// JSON_SET of one path. A JSON null on the way becomes an object or array when creating; arrays
/// are padded with nulls; anything else that doesn't fit the path is left alone.
fn set_at(j: &mut J, path: &[Step], v: &J, create: bool) {
    let Some((s, rest)) = path.split_first() else {
        *j = v.clone();
        return;
    };
    if create && *j == J::Null {
        *j = match s {
            Step::Key(_) => J::Obj(Vec::new()),
            Step::Index(_) => J::Arr(Vec::new()),
        };
    }
    match (s, j) {
        (Step::Key(k), J::Obj(ms)) => match ms.iter().position(|(x, _)| x == k) {
            Some(i) => set_at(&mut ms[i].1, rest, v, create),
            None if create => {
                let mut n = J::Null;
                set_at(&mut n, rest, v, create);
                insert_member(ms, k, n);
            }
            None => {}
        },
        (Step::Index(Some(i)), J::Arr(xs)) => {
            if *i >= xs.len() {
                if !create {
                    return;
                }
                xs.resize(*i + 1, J::Null);
            }
            set_at(&mut xs[*i], rest, v, create);
        }
        _ => {}
    }
}

/// The JSON values a mutator adds for one SQL value: each element of an array when `each` is set.
fn mutator_values(v: &Value, t: &Type, each: bool, ctx: &str) -> Result<Vec<J>> {
    match (v, t) {
        (Value::Array(xs), Type::Array(et)) if each => xs
            .iter()
            .map(|x| to_j(x, et, false, ctx))
            .collect::<Result<Vec<J>>>(),
        (Value::Null, Type::Array(_)) if each => Ok(Vec::new()),
        _ => Ok(vec![to_j(v, t, false, ctx)?]),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mutator {
    Set,
    Append,
    Insert,
}

/// JSON_SET / JSON_ARRAY_APPEND / JSON_ARRAY_INSERT: `(doc, path, value, [path, value]..., [flag])`.
/// A NULL flag returns the document unchanged; a NULL path skips its pair.
fn mutate(a: &[Value], types: &[Type], m: Mutator) -> Result<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let (pairs, flag) = if a.len().is_multiple_of(2) {
        (&a[1..a.len() - 1], a.last())
    } else {
        (&a[1..], None)
    };
    let flag = match flag {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Ok(a[0].clone()),
    };
    let (name, prefixed) = match m {
        Mutator::Set => ("JSON_SET", true),
        Mutator::Append => ("JSON_ARRAY_APPEND", false),
        Mutator::Insert => ("JSON_ARRAY_INSERT", false),
    };
    let ctx = format!("Invalid input to {name}: ");
    let mut j = load(&a[0])?;
    for (k, pair) in pairs.chunks(2).enumerate() {
        let [p, v] = pair else {
            return Err(Error::internal(format!("{name}: odd argument count")));
        };
        let Value::String(p) = p else { continue };
        let steps = parse_mut_path(p).map_err(|e| {
            if prefixed {
                out_of_range(format!("{ctx}{}", e.message))
            } else {
                e
            }
        })?;
        let t = types.get(2 + 2 * k).unwrap_or(&Type::Bool);
        match m {
            Mutator::Set => {
                let v = to_j(v, t, false, &ctx)?;
                set_at(&mut j, &steps, &v, flag);
            }
            Mutator::Append => {
                let Some(target) = node_mut(&mut j, &steps) else {
                    continue;
                };
                let vs = mutator_values(v, t, flag, &ctx)?;
                match target {
                    J::Arr(xs) => xs.extend(vs),
                    J::Null => *target = J::Arr(vs),
                    _ => {}
                }
            }
            Mutator::Insert => {
                let Some((Step::Index(Some(i)), init)) = steps.split_last() else {
                    continue;
                };
                let Some(target) = node_mut(&mut j, init) else {
                    continue;
                };
                let vs = mutator_values(v, t, flag, &ctx)?;
                if *target == J::Null {
                    *target = J::Arr(Vec::new());
                }
                if let J::Arr(xs) = target {
                    if vs.is_empty() {
                        // The emulator pads through the index even with nothing to insert.
                        if *i >= xs.len() {
                            xs.resize(*i + 1, J::Null);
                        }
                    } else {
                        if *i > xs.len() {
                            xs.resize(*i, J::Null);
                        }
                        xs.splice(*i..*i, vs);
                    }
                }
            }
        }
    }
    Ok(json_value(&j))
}

fn num_eq(x: &str, y: &str) -> bool {
    match (classify(x), classify(y)) {
        (Some(Num::Int(a)), Some(Num::Int(b))) => a == b,
        (Some(Num::UInt(a)), Some(Num::UInt(b))) => a == b,
        (Some(a), Some(b)) => {
            let f = |n: Num| match n {
                Num::Int(i) => i as f64,
                Num::UInt(u) => u as f64,
                Num::Float(f) => f,
            };
            f(a) == f(b)
        }
        _ => x == y,
    }
}

/// JSON_CONTAINS below the top level: objects by key, arrays by "every candidate element is
/// contained in some element", scalars by equality.
fn contains(t: &J, c: &J) -> bool {
    match (t, c) {
        (J::Obj(tm), J::Obj(cm)) => cm.iter().all(|(k, cv)| {
            tm.iter()
                .find(|(x, _)| x == k)
                .is_some_and(|(_, tv)| contains(tv, cv))
        }),
        (J::Arr(tx), J::Arr(cx)) => cx.iter().all(|cv| tx.iter().any(|tv| contains(tv, cv))),
        (J::Num(x), J::Num(y)) => num_eq(x, y),
        (J::Null, J::Null) => true,
        (J::Bool(x), J::Bool(y)) => x == y,
        (J::Str(x), J::Str(y)) => x == y,
        _ => false,
    }
}

/// JSON_CONTAINS: at the top level an array also contains each of its scalar elements.
fn json_contains(t: &J, c: &J) -> bool {
    match (t, c) {
        (J::Arr(tx), c) if !matches!(c, J::Arr(_) | J::Obj(_)) => {
            tx.iter().any(|tv| contains(tv, c))
        }
        _ => contains(t, c),
    }
}

/// BOOL_ARRAY, INT64_ARRAY, ...: every element converted with `conv`.
fn typed_array(j: &J, conv: impl Fn(&J) -> Result<Value>) -> Result<Value> {
    let J::Arr(xs) = j else {
        return Err(not_a("an array"));
    };
    Ok(Value::Array(Arc::from(
        xs.iter().map(conv).collect::<Result<Vec<Value>>>()?,
    )))
}

fn to_bool(j: &J) -> Result<Value> {
    match j {
        J::Bool(b) => Ok(Value::Bool(*b)),
        _ => Err(not_a("a boolean")),
    }
}

fn to_string_value(j: &J) -> Result<Value> {
    match j {
        J::Str(s) => Ok(Value::String(Arc::from(s.as_str()))),
        _ => Err(not_a("a string")),
    }
}

fn to_float32(j: &J, wide: Wide) -> Result<Value> {
    let J::Num(t) = j else {
        return Err(not_a("a number"));
    };
    let n = classify(t).ok_or_else(|| not_a("a number"))?;
    let (d, exact) = match n {
        Num::Int(i) => (i as f64, (i as f64) as i128 == i as i128),
        Num::UInt(u) => (
            u as f64,
            (u as f64) as u128 == u as u128 && u as f64 != 1.8446744073709552e19,
        ),
        Num::Float(f) => (f, true),
    };
    let x = d as f32;
    if x.is_infinite() {
        return Err(out_of_range(format!(
            "JSON number: {t} cannot be converted to FLOAT32"
        )));
    }
    if wide == Wide::Exact && (!exact || x as f64 != d) {
        return Err(out_of_range(format!(
            "JSON number: {t} cannot be converted to FLOAT32 without loss of precision"
        )));
    }
    Ok(Value::Float32(x))
}

fn is_text(t: Option<&Type>) -> bool {
    matches!(t, Some(Type::String(_)))
}

pub fn lookup(sig: &Sig) -> Option<ScalarFn> {
    let first = sig.args.first();
    let doc_ok = matches!(first, Some(Type::Json | Type::String(_)));
    let is_json = matches!(first, Some(Type::Json));
    let on_json = matches!(first, Some(Type::Json));
    Some(match sig.name {
        "json_value" if doc_ok => f(|_, a| {
            if a[0].is_null() {
                return Ok(Value::Null);
            }
            let Some(path) = path_arg(a, 1) else {
                return Ok(Value::Null);
            };
            Ok(match locate(&a[0], path)? {
                Some((j, raw)) => match scalar_text(&j, raw) {
                    Some(t) => Value::String(Arc::from(t)),
                    None => Value::Null,
                },
                None => Value::Null,
            })
        }),
        "json_query" if doc_ok => f(move |_, a| {
            if a[0].is_null() {
                return Ok(Value::Null);
            }
            query(a, is_json)
        }),
        "json_value_array" if doc_ok => f(move |_, a| {
            if a[0].is_null() {
                return Ok(Value::Null);
            }
            query_array(a, is_json, true)
        }),
        "json_query_array" if doc_ok => f(move |_, a| {
            if a[0].is_null() {
                return Ok(Value::Null);
            }
            query_array(a, is_json, false)
        }),
        "parse_json" if is_text(first) => strict(|a| {
            let Value::String(s) = &a[0] else {
                return Ok(Value::Null);
            };
            let Some(wide) = wide_mode(
                a,
                1,
                "Invalid `wide_number_mode` specified for PARSE_JSON: ",
            )?
            else {
                return Ok(Value::Null);
            };
            let j = parse(s)
                .and_then(|j| canonical(j, wide))
                .map_err(|e| out_of_range(format!("Invalid input to PARSE_JSON: {e}")))?;
            Ok(json_value(&j))
        }),
        "to_json" | "safe_to_json" => {
            let t = first?.clone();
            let safe = sig.name == "safe_to_json";
            f(move |_, a| {
                let stringify = matches!(a.get(1), Some(Value::Bool(true)));
                match to_j(&a[0], &t, stringify, "") {
                    Ok(j) => Ok(json_value(&j)),
                    Err(_) if safe => Ok(Value::Null),
                    Err(e) => Err(e),
                }
            })
        }
        "to_json_string" if on_json => f(|_, a| {
            let pretty = matches!(a.get(1), Some(Value::Bool(true)));
            if a.get(1).is_some_and(Value::is_null) {
                return Ok(Value::Null);
            }
            let j = match &a[0] {
                Value::Null => J::Null,
                v => load(v)?,
            };
            let mut s = String::new();
            write(&mut s, &j, pretty.then_some(0));
            Ok(Value::String(Arc::from(s)))
        }),
        "json_type" if on_json => {
            strict(|a| Ok(Value::String(Arc::from(json_type(&load(&a[0])?)))))
        }
        "bool" if on_json => strict(|a| match load(&a[0])? {
            J::Bool(b) => Ok(Value::Bool(b)),
            _ => Err(not_a("a boolean")),
        }),
        "int64" if on_json => strict(|a| to_int64(&load(&a[0])?)),
        "float64" if on_json => f(|_, a| {
            if a[0].is_null() {
                return Ok(Value::Null);
            }
            let Some(wide) = wide_mode(a, 1, "Invalid `wide_number_mode` specified: ")? else {
                return Ok(Value::Null);
            };
            to_float64(&load(&a[0])?, wide)
        }),
        "string" if on_json => strict(|a| match load(&a[0])? {
            J::Str(s) => Ok(Value::String(Arc::from(s))),
            _ => Err(not_a("a string")),
        }),
        "lax_bool" if on_json => strict(|a| Ok(lax_bool(&load(&a[0])?))),
        "lax_int64" if on_json => strict(|a| Ok(lax_int64(&load(&a[0])?))),
        "lax_float64" if on_json => strict(|a| Ok(lax_float64(&load(&a[0])?))),
        "lax_string" if on_json => strict(|a| Ok(lax_string(&load(&a[0])?))),
        "json_keys" if on_json => f(|_, a| {
            if a.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            let mut depth = i64::MAX;
            let mut lax = false;
            for v in &a[1..] {
                match v {
                    Value::Int64(d) => {
                        if *d <= 0 {
                            return Err(out_of_range("max_depth must be positive."));
                        }
                        depth = *d;
                    }
                    Value::String(m) => match m.to_ascii_lowercase().as_str() {
                        "strict" => lax = false,
                        "lax" | "lax recursive" => lax = true,
                        _ => return Err(crate::types::unimplemented("JSON_KEYS mode")),
                    },
                    _ => {}
                }
            }
            let mut keys = Vec::new();
            json_keys(&load(&a[0])?, "", depth, lax, &mut keys);
            keys.sort();
            keys.dedup();
            Ok(Value::Array(Arc::from(
                keys.into_iter()
                    .map(|k| Value::String(Arc::from(k)))
                    .collect::<Vec<Value>>(),
            )))
        }),
        "$subscript" if on_json => strict(subscript),
        "json_remove" if on_json => f(|_, a| {
            if a[0].is_null() {
                return Ok(Value::Null);
            }
            let mut j = load(&a[0])?;
            for p in &a[1..] {
                let Value::String(p) = p else { continue };
                let steps = parse_mut_path(p).map_err(|e| {
                    out_of_range(format!("Invalid input to JSON_REMOVE: {}", e.message))
                })?;
                if steps.is_empty() {
                    return Err(out_of_range("The JSONPath cannot be '$'"));
                }
                remove_at(&mut j, &steps);
            }
            Ok(json_value(&j))
        }),
        "json_strip_nulls" if on_json => f(|_, a| {
            if a[0].is_null() || a[1..].iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            strip_nulls(a)
        }),
        "json_set" | "json_array_append" | "json_array_insert" if on_json => {
            let types = sig.args.to_vec();
            let m = match sig.name {
                "json_set" => Mutator::Set,
                "json_array_append" => Mutator::Append,
                _ => Mutator::Insert,
            };
            f(move |_, a| mutate(a, &types, m))
        }
        "json_contains" if on_json => {
            strict(|a| Ok(Value::Bool(json_contains(&load(&a[0])?, &load(&a[1])?))))
        }
        "bool_array" if on_json => strict(|a| typed_array(&load(&a[0])?, to_bool)),
        "int64_array" if on_json => strict(|a| typed_array(&load(&a[0])?, to_int64)),
        "string_array" if on_json => strict(|a| typed_array(&load(&a[0])?, to_string_value)),
        "float64_array" | "float32_array" | "float32" if on_json => {
            let name: &'static str = match sig.name {
                "float64_array" => "float64_array",
                "float32_array" => "float32_array",
                _ => "float32",
            };
            f(move |_, a| {
                if a[0].is_null() {
                    return Ok(Value::Null);
                }
                let Some(wide) = wide_mode(a, 1, "Invalid `wide_number_mode` specified: ")? else {
                    return Ok(Value::Null);
                };
                let j = load(&a[0])?;
                match name {
                    "float64_array" => typed_array(&j, |x| to_float64(x, wide)),
                    "float32_array" => typed_array(&j, |x| to_float32(x, wide)),
                    _ => to_float32(&j, wide),
                }
            })
        }
        "json_array" => {
            let types = sig.args.to_vec();
            f(move |_, a| {
                let mut xs = Vec::with_capacity(a.len());
                for (v, t) in a.iter().zip(&types) {
                    xs.push(to_j(v, t, false, "Invalid input to JSON_ARRAY: ")?);
                }
                Ok(json_value(&J::Arr(xs)))
            })
        }
        "json_object" => {
            let types = sig.args.to_vec();
            f(move |_, a| json_object(a, &types))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(s: &str) -> String {
        match parse(s).and_then(|j| canonical(j, Wide::Exact)) {
            Ok(j) => text(&j),
            Err(e) => format!("ERR {e}"),
        }
    }

    fn doc(s: &str) -> J {
        parse(s).and_then(|j| canonical(j, Wide::Exact)).unwrap()
    }

    #[test]
    fn set_creates_and_pads() {
        let set = |d: &str, p: &str, create: bool| {
            let mut j = doc(d);
            set_at(
                &mut j,
                &parse_mut_path(p).unwrap(),
                &J::Num("2".into()),
                create,
            );
            text(&j)
        };
        assert_eq!(set(r#"{"a":null}"#, "$.a[1]", true), r#"{"a":[null,2]}"#);
        assert_eq!(set(r#"{"b":1}"#, "$.a.c", true), r#"{"a":{"c":2},"b":1}"#);
        assert_eq!(set(r#"{"a":1}"#, "$.a.b", true), r#"{"a":1}"#);
        assert_eq!(set(r#"{"a":1}"#, "$.b", false), r#"{"a":1}"#);
        assert_eq!(set("[1]", "$.0", true), "[1]");
    }

    #[test]
    fn mutator_path_tokens() {
        let err = |p: &str| parse_mut_path(p).err().map(|e| e.message);
        assert_eq!(
            err("$[-1]").as_deref(),
            Some("Invalid token in JSONPath at: [-1]")
        );
        assert_eq!(
            err("$..a").as_deref(),
            Some("Invalid token in JSONPath at: ..a")
        );
        assert_eq!(
            err("$.a*").as_deref(),
            Some("Invalid token in JSONPath at: *")
        );
        assert!(parse_mut_path("$.a b.\"x.y\"[01]").is_ok());
    }

    #[test]
    fn contains_rules() {
        let c = |t: &str, x: &str| json_contains(&doc(t), &doc(x));
        assert!(c("[1,2,3]", "1"));
        assert!(!c(r#"{"a":[1,2]}"#, r#"{"a":2}"#));
        assert!(!c("[1,[2,3]]", "[2]"));
        assert!(c("[1,[2,3]]", "[[2]]"));
        assert!(c("1e2", "100"));
        assert!(!c(r#"[{"a":1,"b":2}]"#, r#"{"a":1}"#));
    }

    #[test]
    fn canonical_numbers_and_keys() {
        assert_eq!(
            canon(r#"{"b": 1, "a": [1, 2.5, "x"]}"#),
            r#"{"a":[1,2.5,"x"],"b":1}"#
        );
        assert_eq!(canon("1e2"), "100.0");
        assert_eq!(canon("-0"), "0");
        assert_eq!(canon("1e-7"), "1e-07");
        assert_eq!(canon("1e16"), "1e+16");
        assert_eq!(canon(r#"{"a": 1, "a": 2}"#), r#"{"a":1}"#);
        assert!(canon("1.2345678901234567890123").starts_with("ERR Input number"));
        assert_eq!(canon("0.1"), "0.1");
        assert_eq!(double_text(1.100000023841858), "1.100000023841858");
        assert_eq!(double_text(1234567890123456.0), "1.234567890123456e+15");
        assert_eq!(double_text(123456789012345.0), "123456789012345.0");
        assert_eq!(double_text(0.0001), "0.0001");
        assert_eq!(double_text(0.00001), "1e-05");
    }

    #[test]
    fn parse_errors() {
        assert_eq!(
            canon("[1 2]"),
            "ERR syntax error while parsing array - unexpected number literal; expected ']'"
        );
        assert_eq!(
            canon("tru"),
            "ERR syntax error while parsing value - invalid literal; last read: 'tru'"
        );
        assert_eq!(
            canon("1 2"),
            "ERR syntax error while parsing value - unexpected number literal; expected end of input"
        );
    }

    #[test]
    fn lenient_text_extraction() {
        let p = parse_path("$[1]").unwrap_or_default();
        assert_eq!(extract_text("[1, 2", &p), Some(J::Num("2".into())));
        let p = parse_path("$.a").unwrap_or_default();
        assert_eq!(
            extract_text(r#"{"a": 1, "a": 2}"#, &p),
            Some(J::Num("1".into()))
        );
        assert_eq!(extract_text(r#"{"a": 1}x"#, &p), Some(J::Num("1".into())));
    }

    #[test]
    fn path_errors() {
        let msg = |p: &str| parse_path(p).err().map(|e| e.message.to_string());
        assert_eq!(msg("a").as_deref(), Some("JSONPath must start with '$'"));
        assert_eq!(
            msg("$..a").as_deref(),
            Some("Unsupported operator in JSONPath: ..")
        );
        assert_eq!(
            msg("$['a']").as_deref(),
            Some("Invalid token in JSONPath at: ['a']")
        );
        assert_eq!(
            msg("$ .a").as_deref(),
            Some("Invalid token in JSONPath at:  .a")
        );
    }
}
