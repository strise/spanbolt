//! Protobuf text format as GoogleSQL's `CAST` between PROTO and STRING spells it: the C++
//! `TextFormat` printer in single-line mode with UTF-8 safe escaping, and its parser's errors.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

use prost::bytes::Bytes;
use prost_reflect::{
    DynamicMessage, FieldDescriptor, Kind, MessageDescriptor, ReflectMessage, Value as PValue,
};

use super::wire::{self, Fields, Wire};
use super::{Scalar, scalar};

/// `bytes` of message `desc` in single-line text format; `None` if the bytes don't parse.
pub fn print(desc: &MessageDescriptor, bytes: &[u8]) -> Option<String> {
    let mut out = String::new();
    print_message(desc, bytes, &mut out, 0)?;
    if out.ends_with(' ') {
        out.pop();
    }
    Some(out)
}

fn packable(kind: &Kind) -> Option<u64> {
    match kind {
        Kind::Double | Kind::Fixed64 | Kind::Sfixed64 => Some(1),
        Kind::Float | Kind::Fixed32 | Kind::Sfixed32 => Some(5),
        Kind::String | Kind::Bytes | Kind::Message(_) => None,
        _ => Some(0),
    }
}

/// The values of field `fd` found in the wire values `ws`, as the C++ reader merges them:
/// every element of a repeated field, the merged bytes of a message, else the last value.
pub(super) fn values<'a>(fd: &FieldDescriptor, ws: &[Wire<'a>]) -> Vec<Scalar<'a>> {
    let kind = fd.kind();
    if fd.is_list() {
        let mut out = Vec::new();
        for w in ws {
            match (w, packable(&kind)) {
                (Wire::Len(b), Some(t)) => {
                    out.extend(
                        wire::packed(b, t)
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(|w| scalar(&kind, w)),
                    );
                }
                _ => out.extend(scalar(&kind, *w)),
            }
        }
        return out;
    }
    if matches!(kind, Kind::Message(_)) {
        let parts: Vec<&[u8]> = ws
            .iter()
            .filter_map(|w| match w {
                Wire::Len(b) | Wire::Group(b) => Some(*b),
                _ => None,
            })
            .collect();
        return match parts.as_slice() {
            [] => Vec::new(),
            [one] => vec![Scalar::Msg(std::borrow::Cow::Borrowed(one))],
            many => vec![Scalar::Msg(std::borrow::Cow::Owned(many.concat()))],
        };
    }
    ws.iter()
        .rev()
        .find_map(|w| scalar(&kind, *w))
        .into_iter()
        .collect()
}

fn accepts(fd: &FieldDescriptor, w: &Wire) -> bool {
    let kind = fd.kind();
    scalar(&kind, *w).is_some()
        || (fd.is_list() && matches!(w, Wire::Len(_)) && packable(&kind).is_some())
}

fn print_message(
    desc: &MessageDescriptor,
    bytes: &[u8],
    out: &mut String,
    depth: u32,
) -> Option<()> {
    if depth > 100 {
        return None;
    }
    let mut known: BTreeMap<u32, (FieldDescriptor, Vec<Wire>)> = BTreeMap::new();
    let mut unknown = Vec::new();
    for f in Fields::new(bytes) {
        let (n, w) = f.ok()?;
        match desc.get_field(n) {
            Some(fd) if accepts(&fd, &w) => known.entry(n).or_insert((fd, Vec::new())).1.push(w),
            _ => unknown.push((n, w)),
        }
    }
    for (fd, ws) in known.values() {
        for v in values(fd, ws) {
            match v {
                Scalar::Msg(b) => {
                    let Kind::Message(m) = fd.kind() else {
                        return None;
                    };
                    out.push_str(fd.name());
                    out.push_str(" { ");
                    print_message(&m, &b, out, depth + 1)?;
                    out.push_str("} ");
                }
                v => {
                    out.push_str(fd.name());
                    out.push_str(": ");
                    print_scalar(fd, &v, out);
                    out.push(' ');
                }
            }
        }
    }
    print_unknown(&unknown, out, depth)
}

fn print_unknown(fields: &[(u32, Wire)], out: &mut String, depth: u32) -> Option<()> {
    for (n, w) in fields {
        match w {
            Wire::Varint(v) => {
                let _ = write!(out, "{n}: {v} ");
            }
            Wire::Fixed32(v) => {
                let _ = write!(out, "{n}: 0x{v:08x} ");
            }
            Wire::Fixed64(v) => {
                let _ = write!(out, "{n}: 0x{v:016x} ");
            }
            Wire::Len(b) | Wire::Group(b) => {
                let nested: Option<Vec<(u32, Wire)>> =
                    Fields::new(b).map(Result::ok).collect::<Option<Vec<_>>>();
                match nested {
                    Some(fs) if (!fs.is_empty() || matches!(w, Wire::Group(_))) && depth < 100 => {
                        let _ = write!(out, "{n} {{ ");
                        print_unknown(&fs, out, depth + 1)?;
                        out.push_str("} ");
                    }
                    _ => {
                        let _ = write!(out, "{n}: \"");
                        escape(b, out);
                        out.push_str("\" ");
                    }
                }
            }
        }
    }
    Some(())
}

fn print_scalar(fd: &FieldDescriptor, v: &Scalar, out: &mut String) {
    match v {
        Scalar::I64(i) => {
            let _ = write!(out, "{i}");
        }
        Scalar::U64(u) => {
            let _ = write!(out, "{u}");
        }
        Scalar::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Scalar::F64(f) => out.push_str(&simple_dtoa(*f)),
        Scalar::F32(f) => out.push_str(&simple_ftoa(*f)),
        Scalar::Str(b) | Scalar::Bytes(b) => {
            out.push('"');
            escape(b, out);
            out.push('"');
        }
        Scalar::Enum(n) => match fd.kind() {
            Kind::Enum(e) => match e.get_value(*n) {
                Some(v) => out.push_str(v.name()),
                None => {
                    let _ = write!(out, "{n}");
                }
            },
            _ => {
                let _ = write!(out, "{n}");
            }
        },
        Scalar::Msg(_) => {}
    }
}

/// absl's `Utf8SafeCEscape`: C escapes and octal for control bytes and invalid UTF-8; valid
/// multi-byte characters stay as they are.
pub fn escape(bytes: &[u8], out: &mut String) {
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '"' => out.push_str("\\\""),
                '\'' => out.push_str("\\'"),
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                    let _ = write!(out, "\\{:03o}", c as u32);
                }
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            let _ = write!(out, "\\{b:03o}");
        }
    }
}

/// C's `%.{precision}g`.
fn format_g(v: f64, precision: usize) -> String {
    let p = precision.max(1);
    let sci = format!("{:.*e}", p - 1, v);
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if exp < -4 || exp >= p as i32 {
        let mantissa = trim_zeros(mantissa);
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    } else {
        let decimals = (p as i32 - 1 - exp).max(0) as usize;
        trim_zeros(&format!("{v:.decimals$}")).to_string()
    }
}

fn trim_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

fn non_finite(v: f64) -> Option<&'static str> {
    if v.is_nan() {
        Some("nan")
    } else if v == f64::INFINITY {
        Some("inf")
    } else if v == f64::NEG_INFINITY {
        Some("-inf")
    } else {
        None
    }
}

/// protobuf's `SimpleDtoa`: 15 significant digits, 17 when 15 don't round-trip.
pub fn simple_dtoa(v: f64) -> String {
    if let Some(s) = non_finite(v) {
        return s.into();
    }
    let short = format_g(v, 15);
    if short.parse::<f64>().ok() == Some(v) {
        short
    } else {
        format_g(v, 17)
    }
}

/// protobuf's `SimpleFtoa`: 6 significant digits, 9 when 6 don't round-trip.
pub fn simple_ftoa(v: f32) -> String {
    if let Some(s) = non_finite(f64::from(v)) {
        return s.into();
    }
    let short = format_g(f64::from(v), 6);
    if short.parse::<f32>().ok() == Some(v) {
        short
    } else {
        format_g(f64::from(v), 9)
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing.

/// A parse error at a 1-based line and column, as GoogleSQL reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} [{}:{}]", self.message, self.line, self.column)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Ident,
    Int,
    Float,
    Str,
    Symbol,
    End,
}

#[derive(Debug, Clone)]
struct Token {
    kind: Tok,
    text: String,
    line: usize,
    column: usize,
}

struct Tokenizer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
    column: usize,
    current: Token,
}

impl<'a> Tokenizer<'a> {
    fn new(src: &'a str) -> Result<Self, ParseError> {
        let mut t = Tokenizer {
            src: src.as_bytes(),
            pos: 0,
            line: 0,
            column: 0,
            current: Token {
                kind: Tok::End,
                text: String::new(),
                line: 0,
                column: 0,
            },
        };
        t.next()?;
        Ok(t)
    }

    fn bump(&mut self) {
        match self.src[self.pos] {
            b'\n' => {
                self.line += 1;
                self.column = 0;
            }
            b'\t' => self.column += 8 - self.column % 8,
            _ => self.column += 1,
        }
        self.pos += 1;
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            line: self.line + 1,
            column: self.column + 1,
        }
    }

    fn next(&mut self) -> Result<(), ParseError> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c') => self.bump(),
                Some(b'#') => {
                    while self.peek().is_some_and(|c| c != b'\n') {
                        self.bump();
                    }
                }
                _ => break,
            }
        }
        let (line, column, start) = (self.line, self.column, self.pos);
        let kind = match self.peek() {
            None => Tok::End,
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                while self
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
                {
                    self.bump();
                }
                Tok::Ident
            }
            Some(c)
                if c.is_ascii_digit()
                    || (c == b'.'
                        && self.src.get(self.pos + 1).is_some_and(u8::is_ascii_digit)) =>
            {
                self.number()?
            }
            Some(q @ (b'"' | b'\'')) => {
                self.bump();
                loop {
                    match self.peek() {
                        None | Some(b'\n') => {
                            return Err(self.error("Unexpected end of string."));
                        }
                        Some(b'\\') => {
                            self.bump();
                            if self.peek().is_some() {
                                self.bump();
                            }
                        }
                        Some(c) if c == q => {
                            self.bump();
                            break;
                        }
                        Some(_) => self.bump(),
                    }
                }
                Tok::Str
            }
            Some(_) => {
                // One character, UTF-8 aware.
                let len = std::str::from_utf8(&self.src[self.pos..])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .map_or(1, char::len_utf8);
                for _ in 0..len {
                    self.bump();
                }
                Tok::Symbol
            }
        };
        self.current = Token {
            kind,
            text: String::from_utf8_lossy(&self.src[start..self.pos]).into_owned(),
            line,
            column,
        };
        Ok(())
    }

    fn number(&mut self) -> Result<Tok, ParseError> {
        let hex =
            self.peek() == Some(b'0') && matches!(self.src.get(self.pos + 1), Some(b'x' | b'X'));
        if hex {
            self.bump();
            self.bump();
            if !self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                return Err(self.error("\"0x\" must be followed by hex digits."));
            }
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.bump();
            }
            return Ok(Tok::Int);
        }
        let mut float = false;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some(b'.') {
            float = true;
            self.bump();
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            float = true;
            self.bump();
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.bump();
            }
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(self.error("\"e\" must be followed by exponent."));
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some(b'f' | b'F')) {
            float = true;
            self.bump();
        }
        Ok(if float { Tok::Float } else { Tok::Int })
    }
}

struct Parser<'a> {
    tok: Tokenizer<'a>,
}

impl Parser<'_> {
    fn cur(&self) -> &Token {
        &self.tok.current
    }

    fn error_here(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            line: self.cur().line + 1,
            column: self.cur().column + 1,
        }
    }

    fn looking_at(&self, s: &str) -> bool {
        self.cur().kind == Tok::Symbol && self.cur().text == s
    }

    fn try_consume(&mut self, s: &str) -> Result<bool, ParseError> {
        if self.looking_at(s) || (self.cur().kind == Tok::Ident && self.cur().text == s) {
            self.tok.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn consume(&mut self, s: &str) -> Result<(), ParseError> {
        if self.try_consume(s)? {
            return Ok(());
        }
        Err(self.error_here(format!("Expected \"{s}\", found \"{}\".", self.cur().text)))
    }

    fn message(&mut self, msg: &mut DynamicMessage, close: Option<&str>) -> Result<(), ParseError> {
        let mut seen = HashSet::new();
        loop {
            match close {
                Some(c) if self.looking_at(c) => return Ok(()),
                Some(c) if self.cur().kind == Tok::End => {
                    return Err(self.error_here(format!("Expected \"{c}\".")));
                }
                None if self.cur().kind == Tok::End => return Ok(()),
                _ => {}
            }
            self.field(msg, &mut seen)?;
        }
    }

    fn field(
        &mut self,
        msg: &mut DynamicMessage,
        seen: &mut HashSet<u32>,
    ) -> Result<(), ParseError> {
        let desc = msg.descriptor();
        if self.cur().kind != Tok::Ident {
            return Err(self.error_here(format!("Expected identifier, got: {}", self.cur().text)));
        }
        let name = self.cur().text.clone();
        self.tok.next()?;
        let fd = desc.get_field_by_name(&name).or_else(|| {
            // A group's field is named after its type, lowercased.
            desc.get_field_by_name(&name.to_ascii_lowercase())
                .filter(|f| f.is_group())
        });
        let Some(fd) = fd else {
            return Err(self.error_here(format!(
                "Message type \"{}\" has no field named \"{name}\".",
                desc.full_name()
            )));
        };
        if !fd.is_list() && !seen.insert(fd.number()) {
            return Err(self.error_here(format!(
                "Non-repeated field \"{name}\" is specified multiple times."
            )));
        }
        if let Kind::Message(m) = fd.kind() {
            self.try_consume(":")?;
            if fd.is_list() && self.try_consume("[")? {
                if !self.try_consume("]")? {
                    loop {
                        let v = self.message_value(&m)?;
                        push(msg, &fd, v);
                        if self.try_consume("]")? {
                            break;
                        }
                        self.consume(",")?;
                    }
                }
            } else {
                let v = self.message_value(&m)?;
                set(msg, &fd, v);
            }
        } else {
            self.consume(":")?;
            if fd.is_list() && self.try_consume("[")? {
                if !self.try_consume("]")? {
                    loop {
                        let v = self.value(&fd)?;
                        push(msg, &fd, v);
                        if self.try_consume("]")? {
                            break;
                        }
                        self.consume(",")?;
                    }
                }
            } else {
                let v = self.value(&fd)?;
                set(msg, &fd, v);
            }
        }
        if !self.try_consume(";")? {
            self.try_consume(",")?;
        }
        Ok(())
    }

    fn message_value(&mut self, m: &MessageDescriptor) -> Result<PValue, ParseError> {
        let close = if self.try_consume("<")? {
            ">"
        } else if self.try_consume("{")? {
            "}"
        } else {
            return Err(self.error_here(format!("Expected \"{{\", found \"{}\".", self.cur().text)));
        };
        let mut nested = DynamicMessage::new(m.clone());
        self.message(&mut nested, Some(close))?;
        self.consume(close)?;
        Ok(PValue::Message(nested))
    }

    fn integer(&mut self, kind: &Kind) -> Result<PValue, ParseError> {
        let neg = self.try_consume("-")?;
        if self.cur().kind != Tok::Int {
            return Err(self.error_here(format!("Expected integer, got: {}", self.cur().text)));
        }
        let text = self.cur().text.clone();
        let magnitude = parse_uint(&text)
            .ok_or_else(|| self.error_here(format!("Integer out of range ({text})")))?;
        let signed = |max: u64| -> Option<i64> {
            if neg {
                (magnitude <= max + 1).then(|| (magnitude as i128).wrapping_neg() as i64)
            } else {
                (magnitude <= max).then_some(magnitude as i64)
            }
        };
        let v = match kind {
            Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => {
                signed(i32::MAX as u64).map(|v| PValue::I32(v as i32))
            }
            Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => signed(i64::MAX as u64).map(PValue::I64),
            Kind::Uint32 | Kind::Fixed32 => {
                (!neg && magnitude <= u64::from(u32::MAX)).then(|| PValue::U32(magnitude as u32))
            }
            _ => (!neg).then_some(PValue::U64(magnitude)),
        };
        let v = v.ok_or_else(|| self.error_here(format!("Integer out of range ({text})")))?;
        self.tok.next()?;
        Ok(v)
    }

    fn float(&mut self) -> Result<f64, ParseError> {
        let neg = self.try_consume("-")?;
        let text = self.cur().text.clone();
        let v = match self.cur().kind {
            Tok::Int => parse_uint(&text).map(|u| u as f64),
            Tok::Float => text.trim_end_matches(['f', 'F']).parse::<f64>().ok(),
            Tok::Ident => match text.to_ascii_lowercase().as_str() {
                "inf" | "infinity" => Some(f64::INFINITY),
                "nan" => Some(f64::NAN),
                _ => None,
            },
            _ => None,
        };
        let v = v.ok_or_else(|| self.error_here(format!("Expected double, got: {text}")))?;
        self.tok.next()?;
        Ok(if neg { -v } else { v })
    }

    fn value(&mut self, fd: &FieldDescriptor) -> Result<PValue, ParseError> {
        let kind = fd.kind();
        Ok(match &kind {
            Kind::Double => PValue::F64(self.float()?),
            Kind::Float => PValue::F32(self.float()? as f32),
            Kind::Bool => {
                let text = self.cur().text.clone();
                let v = match (self.cur().kind, text.as_str()) {
                    (Tok::Ident, "true" | "True" | "t") | (Tok::Int, "1") => true,
                    (Tok::Ident, "false" | "False" | "f") | (Tok::Int, "0") => false,
                    _ => {
                        return Err(self.error_here(format!(
                            "Invalid value for boolean field \"{}\". Value: \"{text}\".",
                            fd.name()
                        )));
                    }
                };
                self.tok.next()?;
                PValue::Bool(v)
            }
            Kind::String | Kind::Bytes => {
                if self.cur().kind != Tok::Str {
                    return Err(
                        self.error_here(format!("Expected string, got: {}", self.cur().text))
                    );
                }
                let mut bytes = Vec::new();
                while self.cur().kind == Tok::Str {
                    let t = self.cur().text.clone();
                    unescape(&t[1..t.len() - 1], &mut bytes).ok_or_else(|| {
                        self.error_here("Invalid escape sequence in string literal.")
                    })?;
                    self.tok.next()?;
                }
                if matches!(kind, Kind::String) {
                    PValue::String(String::from_utf8_lossy(&bytes).into_owned())
                } else {
                    PValue::Bytes(Bytes::from(bytes))
                }
            }
            Kind::Enum(e) => {
                let text = self.cur().text.clone();
                let number = match self.cur().kind {
                    Tok::Ident => e.get_value_by_name(&text).map(|v| v.number()),
                    Tok::Int => text
                        .parse::<i32>()
                        .ok()
                        .filter(|n| e.get_value(*n).is_some()),
                    Tok::Symbol if text == "-" => {
                        self.tok.next()?;
                        let t = format!("-{}", self.cur().text);
                        t.parse::<i32>().ok().filter(|n| e.get_value(*n).is_some())
                    }
                    _ => {
                        return Err(
                            self.error_here(format!("Expected integer or identifier, got: {text}"))
                        );
                    }
                };
                let Some(number) = number else {
                    return Err(self.error_here(format!(
                        "Unknown enumeration value of \"{text}\" for field \"{}\".",
                        fd.name()
                    )));
                };
                self.tok.next()?;
                PValue::EnumNumber(number)
            }
            Kind::Message(_) => unreachable!("messages go through message_value"),
            k => self.integer(k)?,
        })
    }
}

fn set(msg: &mut DynamicMessage, fd: &FieldDescriptor, v: PValue) {
    msg.set_field(fd, v);
}

fn push(msg: &mut DynamicMessage, fd: &FieldDescriptor, v: PValue) {
    if let Some(list) = msg.get_field_mut(fd).as_list_mut() {
        list.push(v);
    }
}

fn parse_uint(text: &str) -> Option<u64> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else if text.len() > 1 && text.starts_with('0') {
        u64::from_str_radix(&text[1..], 8).ok()
    } else {
        text.parse().ok()
    }
}

/// C escapes of a text-format string literal's body.
fn unescape(s: &str, out: &mut Vec<u8>) -> Option<()> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        let c = *b.get(i)?;
        i += 1;
        match c {
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            b'\\' | b'?' | b'\'' | b'"' => out.push(c),
            b'0'..=b'7' => {
                let mut v = u32::from(c - b'0');
                for _ in 0..2 {
                    match b.get(i) {
                        Some(d @ b'0'..=b'7') => {
                            v = v * 8 + u32::from(d - b'0');
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(v as u8);
            }
            b'x' | b'X' => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 2 && b.get(i).is_some_and(u8::is_ascii_hexdigit) {
                    v = v * 16 + (b[i] as char).to_digit(16)?;
                    i += 1;
                    n += 1;
                }
                if n == 0 {
                    return None;
                }
                out.push(v as u8);
            }
            b'u' | b'U' => {
                let n = if c == b'u' { 4 } else { 8 };
                let hex = s.get(i..i + n)?;
                let ch = char::from_u32(u32::from_str_radix(hex, 16).ok()?)?;
                i += n;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            _ => return None,
        }
    }
    Some(())
}

/// Parses `text` into a message of type `desc`.
pub fn parse(desc: &MessageDescriptor, text: &str) -> Result<DynamicMessage, ParseError> {
    let mut p = Parser {
        tok: Tokenizer::new(text)?,
    };
    let mut msg = DynamicMessage::new(desc.clone());
    p.message(&mut msg, None)?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_like_protobuf() {
        assert_eq!(simple_dtoa(0.0), "0");
        assert_eq!(simple_dtoa(1.5), "1.5");
        assert_eq!(simple_dtoa(0.1), "0.1");
        assert_eq!(simple_dtoa(1e20), "1e+20");
        assert_eq!(simple_dtoa(123456.0), "123456");
        assert_eq!(simple_dtoa(1e-5), "1e-05");
        assert_eq!(simple_dtoa(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(simple_dtoa(f64::NEG_INFINITY), "-inf");
        assert_eq!(simple_ftoa(0.1), "0.1");
        assert_eq!(simple_ftoa(16777216.0), "16777216");
    }

    #[test]
    fn escapes_like_utf8_safe_cescape() {
        let mut s = String::new();
        escape("a\"b\n\x01'é".as_bytes(), &mut s);
        escape(b"\xff", &mut s);
        assert_eq!(s, "a\\\"b\\n\\001\\'é\\377");
    }
}
