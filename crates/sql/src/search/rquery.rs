//! The rquery parser SEARCH uses: a port of cloud-spanner-emulator
//! `backend/query/search/query_parser.jjt` and `query_parser.cc` (Apache-2.0), including the
//! JavaCC token manager's longest-match rules and its error messages.
//!
//! In emulator mode the parser reads bytes like the emulator: non-ASCII characters are skipped by
//! the lexer, so a query term with letters such as `ø` splits into two terms and fails to parse,
//! and two separators in a row (`a  b`, `a, b`) are a syntax error. Prod mode (the default) follows
//! Cloud Spanner: Unicode letters and digits are term characters (lower-cased with Unicode rules),
//! other non-ASCII characters separate terms, and a run of separators is one separator. It reads
//! runs of single letters as one term, as its tokenizer stores them (`tokens::joined_letters`).
//! Prod also ignores a connector (`.`, `/`, `-`, ...) that doesn't join two terms (`apple. pie`),
//! reads a bare upper-case `AROUND` as `AROUND(5)` and `"a AROUND b"` as AROUND in query order,
//! and matches nothing for a double negation (`--a`) or a negation inside or of a parenthesised
//! group. All fitted to recordings (`conformance/cases/sql/search_query_dots.case`,
//! `search_around.case`, `search_negation_only.case`).

use spanbolt_storage::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Term(Vec<u8>),
    Number(i64),
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Phrase(Vec<Node>),
    Around(Vec<Node>),
    /// Prod's `"a AROUND b"`: AROUND with the operands in query order.
    InOrder(Vec<Node>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum K {
    Eof,
    Hyphen,
    Quote,
    Or,
    Connector,
    Open,
    Close,
    Sep,
    OrOp,
    Number,
    Term,
    Around,
}

#[derive(Debug, Clone, Copy)]
struct Tok {
    k: K,
    start: usize,
    end: usize,
    col: usize,
}

fn is_sep(b: u8) -> bool {
    matches!(
        b,
        b'~' | b'`'
            | b'!'
            | b'@'
            | b'#'
            | b'$'
            | b'%'
            | b'^'
            | b'&'
            | b'_'
            | b'+'
            | b'{'
            | b'}'
            | b'['
            | b']'
            | b';'
            | b'<'
            | b'>'
            | b','
            | b'?'
            | b'*'
            | b' '
            | b'\r'
            | b'\n'
            | b'\t'
            | 0x08
            | 0x0c
    )
}

/// `kSeparators`, trimmed from both ends before parsing.
fn is_trimmed(b: u8) -> bool {
    matches!(
        b,
        b'~' | b'`'
            | b'!'
            | b'@'
            | b'#'
            | b'$'
            | b'%'
            | b'^'
            | b'&'
            | b'_'
            | b'+'
            | b'{'
            | b'}'
            | b'['
            | b']'
            | b'<'
            | b'>'
            | b','
            | b'?'
            | b'*'
            | b' '
            | b'\r'
            | b'\n'
            | b'\t'
            | 0x08
            | 0x0c
            | b';'
    )
}

/// JavaCC's column of each byte (tab size 1; a new line restarts at column 1).
fn columns(s: &[u8]) -> Vec<usize> {
    let mut out = Vec::with_capacity(s.len());
    let (mut col, mut cr, mut lf) = (0usize, false, false);
    for &c in s {
        col += 1;
        if lf {
            lf = false;
            col = 1;
        } else if cr {
            cr = false;
            if c == b'\n' {
                lf = true;
            } else {
                col = 1;
            }
        }
        match c {
            b'\r' => cr = true,
            b'\n' => lf = true,
            _ => {}
        }
        out.push(col);
    }
    out
}

fn lex(s: &[u8], emulator: bool) -> Vec<Tok> {
    let cols = columns(s);
    let mut toks = Vec::new();
    let mut i = 0;
    // JavaCC reports EOF at the column where the last lexeme (token or skipped text) began.
    let mut last_col = 0;
    while i < s.len() {
        last_col = cols[i];
        let c = s[i];
        let (k, len) = if is_sep(c) {
            let mut j = i;
            while j < s.len() && is_sep(s[j]) {
                j += 1;
            }
            let or = if s[j..].starts_with(b"OR") {
                2
            } else if s.get(j) == Some(&b'|') {
                1
            } else {
                0
            };
            let mut k = j + or;
            while or > 0 && k < s.len() && is_sep(s[k]) {
                k += 1;
            }
            if or > 0 && k > j + or {
                (Some(K::OrOp), k - i)
            } else {
                (Some(K::Sep), 1)
            }
        } else if c.is_ascii_alphanumeric() || (!emulator && unicode_alnum(s, i).is_some()) {
            let mut j = i;
            while j < s.len() {
                if s[j].is_ascii_alphanumeric() {
                    j += 1;
                } else if let Some(n) = unicode_alnum(s, j).filter(|_| !emulator) {
                    j += n;
                } else {
                    break;
                }
            }
            let word = &s[i..j];
            if word == b"AROUND" && s.get(j) == Some(&b'(') {
                (Some(K::Around), 7)
            } else if word == b"OR" {
                (Some(K::Or), 2)
            } else if word.iter().all(u8::is_ascii_digit) {
                (Some(K::Number), j - i)
            } else {
                (Some(K::Term), j - i)
            }
        } else {
            match c {
                b'-' => (Some(K::Hyphen), 1),
                b'"' => (Some(K::Quote), 1),
                b'|' => (Some(K::Or), 1),
                b'(' => (Some(K::Open), 1),
                b')' => (Some(K::Close), 1),
                b'=' | b'.' | b'/' | b'\'' | b':' => (Some(K::Connector), 1),
                b'\\' => match s.get(i + 1) {
                    Some(b'\\') => (Some(K::Connector), 2),
                    Some(_) => (None, 2),
                    None => (None, 1),
                },
                _ if !emulator && c >= 0x80 => (Some(K::Sep), utf8_len(s, i)),
                _ => (None, 1),
            }
        };
        if let Some(k) = k {
            toks.push(Tok {
                k,
                start: i,
                end: i + len,
                col: cols[i],
            });
        }
        i += len;
    }
    toks.push(Tok {
        k: K::Eof,
        start: s.len(),
        end: s.len(),
        col: last_col,
    });
    toks
}

/// Prod drops connectors and hyphens that don't join two terms or negate one (`apple. pie`,
/// `apple .pie`, `apple -`), then the separators left at either end.
fn drop_dangling(toks: Vec<Tok>) -> Vec<Tok> {
    let joins = |k: K| matches!(k, K::Connector | K::Hyphen);
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    let mut i = 0;
    let mut quotes = 0;
    while i < toks.len() {
        if !joins(toks[i].k) {
            quotes += usize::from(toks[i].k == K::Quote);
            out.push(toks[i]);
            i += 1;
            continue;
        }
        let mut j = i;
        while j < toks.len() && joins(toks[j].k) {
            j += 1;
        }
        let prev = out.last().map(|t| t.k);
        let next = toks.get(j).map_or(K::Eof, |t| t.k);
        // A quote after an odd number of them closes a phrase.
        let opens = next == K::Quote && quotes % 2 == 0;
        let starts = opens || matches!(next, K::Term | K::Number | K::Open | K::Around);
        let between = matches!(prev, Some(K::Term | K::Number | K::Close));
        let negation = toks[i..j].iter().all(|t| t.k == K::Hyphen);
        if starts && (between || negation) {
            out.extend_from_slice(&toks[i..j]);
        }
        i = j;
    }
    // Separators at the ends, or next to `|` once a connector between them is gone.
    let mut kept: Vec<Tok> = Vec::with_capacity(out.len());
    for (n, t) in out.iter().enumerate() {
        let next = out.get(n + 1).map_or(K::Eof, |t| t.k);
        let prev = kept.last().map(|t| t.k);
        if t.k == K::Sep
            && (matches!(prev, None | Some(K::OrOp)) || matches!(next, K::Eof | K::OrOp))
        {
            continue;
        }
        kept.push(*t);
    }
    kept
}

/// Length of the UTF-8 character at `i` (1 for a stray byte).
fn utf8_len(s: &[u8], i: usize) -> usize {
    let n = match s[i] {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    };
    if i + n <= s.len() && std::str::from_utf8(&s[i..i + n]).is_ok() {
        n
    } else {
        1
    }
}

/// The byte length of a non-ASCII Unicode letter or digit at `i`.
fn unicode_alnum(s: &[u8], i: usize) -> Option<usize> {
    if s[i] < 0x80 {
        return None;
    }
    let n = utf8_len(s, i);
    let c = std::str::from_utf8(&s[i..i + n]).ok()?.chars().next()?;
    c.is_alphanumeric().then_some(n)
}

/// Whether the character at `i` is trimmed from the ends of a query.
fn trimmed_at(s: &[u8], i: usize, emulator: bool) -> bool {
    is_trimmed(s[i]) || (!emulator && s[i] >= 0x80 && unicode_alnum(s, i).is_none())
}

struct Parser<'a> {
    s: &'a [u8],
    emulator: bool,
    toks: Vec<Tok>,
    pos: usize,
    /// Prod dropped a lower-case `or` from the top-level AND.
    dropped_or: bool,
    /// Depth of `and_expression` calls (1 is the top level).
    depth: usize,
}

type P<T> = std::result::Result<T, String>;

impl Parser<'_> {
    fn peek(&self, n: usize) -> K {
        self.toks.get(self.pos + n).map_or(K::Eof, |t| t.k)
    }

    fn tok(&self) -> Tok {
        self.toks[self.pos.min(self.toks.len() - 1)]
    }

    fn bump(&mut self) -> Tok {
        let t = self.tok();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, k: K) -> P<Tok> {
        if self.peek(0) == k {
            return Ok(self.bump());
        }
        let label = match k {
            K::Eof => "EOF",
            K::Quote => "\"",
            K::Around => "AROUND(",
            K::Number => "<NUMBER>",
            K::Close => ")",
            K::Sep => "<SEPARATOR>",
            _ => "?",
        };
        Err(format!(
            "Syntax error on column {}: Expecting '{label}",
            self.tok().col
        ))
    }

    fn parse_error(&self, production: &str) -> String {
        let t = self.tok();
        let extra = if matches!(t.k, K::Open | K::Close) {
            "Using parentheses to group query terms is not supported in rquery parser."
        } else {
            ""
        };
        format!(
            "Encountered error on column {} while parsing: {production}. {extra}",
            t.col
        )
    }

    fn rquery(&mut self) -> P<Node> {
        let n = self.and_expression()?;
        self.expect(K::Eof)?;
        Ok(n)
    }

    fn and_expression(&mut self) -> P<Node> {
        self.depth += 1;
        let n = self.and_expression_inner();
        self.depth -= 1;
        n
    }

    fn and_expression_inner(&mut self) -> P<Node> {
        let mut c = vec![self.around_expression()?];
        while self.peek(0) == K::Sep {
            while self.peek(0) == K::Sep {
                self.bump();
            }
            c.push(self.around_expression()?);
        }
        if !self.emulator && c.len() > 1 {
            // Prod ignores a lower-case `or` between terms.
            let kept: Vec<Node> = c
                .iter()
                .filter(|n| **n != Node::Term(b"or".to_vec()))
                .cloned()
                .collect();
            if !kept.is_empty() {
                self.dropped_or |= self.depth == 1 && kept.len() < c.len();
                c = kept;
            }
        }
        Ok(wrap(c, Node::And))
    }

    fn around_expression(&mut self) -> P<Node> {
        let mut c = vec![self.or_expression()?];
        let mut never = false;
        while self.peek(0) == K::Sep && self.around_follows() {
            while self.peek(0) == K::Sep {
                self.bump();
            }
            if self.emulator {
                self.expect(K::Around)?;
                let n = self.expect(K::Number)?;
                let d = number(&self.s[n.start..n.end]);
                // The emulator rejects a distance past i32 (-1 is its "invalid distance").
                c.push(Node::Number(if d > i64::from(i32::MAX) { -1 } else { d }));
                self.expect(K::Close)?;
                self.expect(K::Sep)?;
            } else {
                let d = self.prod_distance();
                never |= d.is_none();
                let mut gap = d.unwrap_or(DEFAULT_GAP);
                while self.peek(0) == K::Sep {
                    self.bump();
                }
                match self.peek(0) {
                    // `a AROUND(1)` matches nothing.
                    K::Eof => return Ok(never_node()),
                    // `a AROUND(1) | b` is `a AROUND b`.
                    K::OrOp => {
                        self.bump();
                        gap = DEFAULT_GAP;
                    }
                    _ => {}
                }
                c.push(Node::Number(gap));
            }
            while self.peek(0) == K::Sep {
                self.bump();
            }
            c.push(self.or_expression()?);
        }
        if never {
            return Ok(never_node());
        }
        Ok(wrap(c, Node::Around))
    }

    /// Whether the token `n` ahead is prod's bare `AROUND`: an upper-case word followed by a
    /// separator and an operand (`a AROUND b`; `a AROUND | b` and `a AROUND-b` are terms).
    fn bare_around(&self, n: usize) -> bool {
        let Some(t) = self.toks.get(self.pos + n) else {
            return false;
        };
        if t.k != K::Term || &self.s[t.start..t.end] != b"AROUND" || self.peek(n + 1) != K::Sep {
            return false;
        }
        let mut m = n + 1;
        while self.peek(m) == K::Sep {
            m += 1;
        }
        matches!(
            self.peek(m),
            K::Term | K::Number | K::Quote | K::Hyphen | K::Open | K::Around
        )
    }

    /// Prod: reads an AROUND operator (`AROUND(n)`, `AROUND (n)` or a bare `AROUND`) and its
    /// distance. `None` when the operator matches nothing (`AROUND(two)`, `AROUND()`,
    /// `AROUND(-1)`, `AROUND(1 2)`). A missing `)` is fine (`AROUND(2 b`); a distance that isn't
    /// a whole number of at most `MAX_GAP` (`AROUND(1.5)`) is the default.
    fn prod_distance(&mut self) -> Option<i64> {
        let t = self.bump();
        if t.k == K::Term {
            // Bare `AROUND`: `AROUND (n)` still gives the distance.
            let mut n = 0;
            while self.peek(n) == K::Sep {
                n += 1;
            }
            let mut m = n + 1;
            while self.peek(m) == K::Sep {
                m += 1;
            }
            if !(self.peek(n) == K::Open && self.peek(m) == K::Number) {
                return Some(DEFAULT_GAP);
            }
            for _ in 0..=n {
                self.bump();
            }
        }
        let skip_seps = |p: &mut Self| {
            while p.peek(0) == K::Sep {
                p.bump();
            }
        };
        skip_seps(self);
        match self.peek(0) {
            K::Number => {
                let n = self.bump();
                let mut d = number(&self.s[n.start..n.end]);
                if self.peek(0) == K::Connector && self.peek(1) == K::Number {
                    self.bump();
                    self.bump();
                    d = DEFAULT_GAP;
                }
                skip_seps(self);
                match self.peek(0) {
                    K::Close => {
                        self.bump();
                    }
                    K::Number | K::Hyphen | K::Connector => {
                        self.skip_past_close();
                        return None;
                    }
                    _ => {}
                }
                Some(if d > MAX_GAP { DEFAULT_GAP } else { d })
            }
            _ => {
                self.skip_past_close();
                None
            }
        }
    }

    /// Skips to just past the next `)`, or to the end.
    fn skip_past_close(&mut self) {
        while !matches!(self.peek(0), K::Close | K::Eof) {
            self.bump();
        }
        if self.peek(0) == K::Close {
            self.bump();
        }
    }

    /// Whether a separator starts an `AROUND(n)` operator. The emulator's grammar reads two
    /// separators as the start of one (so `a  b` is a syntax error); prod skips the whole run.
    fn around_follows(&self) -> bool {
        if self.emulator {
            return matches!(self.peek(1), K::Sep | K::Around);
        }
        let mut n = 1;
        while self.peek(n) == K::Sep {
            n += 1;
        }
        self.peek(n) == K::Around || self.bare_around(n)
    }

    fn or_expression(&mut self) -> P<Node> {
        let mut c = vec![self.sub_expression()?];
        while self.peek(0) == K::OrOp
            && (matches!(self.peek(1), K::Hyphen | K::Quote | K::Term | K::Number)
                || (!self.emulator && self.peek(1) == K::Open))
        {
            self.bump();
            c.push(self.sub_expression()?);
        }
        Ok(wrap(c, Node::Or))
    }

    fn sub_expression(&mut self) -> P<Node> {
        match self.peek(0) {
            K::Hyphen => {
                self.bump();
                let inner = self.sub_expression()?;
                // Prod: `--a` and `-(a)` match nothing.
                let negated = matches!(inner, Node::Not(_)) || inner == never_node();
                if !self.emulator && (negated || self.after_group()) {
                    return Ok(never_node());
                }
                Ok(Node::Not(Box::new(inner)))
            }
            K::Quote => self.quoted_phrase(),
            K::Term | K::Number => self.connected_phrase(),
            // Prod groups with parentheses; a group with a negation in it matches nothing.
            K::Open if !self.emulator => {
                self.bump();
                let n = self.and_expression()?;
                if self.peek(0) == K::Close {
                    self.bump();
                }
                Ok(if has_not(&n) { never_node() } else { n })
            }
            _ => Err(self.parse_error("sub_expression")),
        }
    }

    /// Whether the last sub-expression read was a parenthesised group.
    fn after_group(&self) -> bool {
        self.pos > 0 && self.toks[self.pos - 1].k == K::Close
    }

    fn connected_phrase(&mut self) -> P<Node> {
        let mut c = vec![self.or_term()?];
        while matches!(self.peek(0), K::Connector | K::Hyphen) {
            while matches!(self.peek(0), K::Connector | K::Hyphen) {
                self.bump();
            }
            c.push(self.or_term()?);
        }
        Ok(wrap(c, Node::Phrase))
    }

    fn quoted_phrase(&mut self) -> P<Node> {
        self.expect(K::Quote)?;
        // Prod: `"a AROUND b"` splits the phrase into AROUND operands.
        let mut parts = vec![(vec![self.or_term()?], DEFAULT_GAP)];
        let inner = |k: K| matches!(k, K::Sep | K::Connector | K::Hyphen);
        let mut never = false;
        loop {
            if !self.emulator && self.peek(0) == K::Sep {
                let mut n = 0;
                while self.peek(n) == K::Sep {
                    n += 1;
                }
                if self.peek(n) == K::Around || self.bare_around(n) {
                    for _ in 0..n {
                        self.bump();
                    }
                    let d = self.prod_distance();
                    never |= d.is_none();
                    while self.peek(0) == K::Sep {
                        self.bump();
                    }
                    parts.push((vec![self.or_term()?], d.unwrap_or(DEFAULT_GAP)));
                    continue;
                }
            }
            if !inner(self.peek(0)) {
                break;
            }
            while inner(self.peek(0)) {
                self.bump();
            }
            if let Some(last) = parts.last_mut() {
                last.0.push(self.or_term()?);
            }
        }
        self.expect(K::Quote)?;
        if parts.len() == 1 {
            let (c, _) = parts.pop().unwrap_or_default();
            // Prod keeps a quoted single term a phrase, so a quoted stopword stays (`"the"`).
            if !self.emulator && c.len() == 1 {
                return Ok(Node::Phrase(c));
            }
            return Ok(wrap(c, Node::Phrase));
        }
        // Only single terms around AROUND have been seen to match (`"a b AROUND c"` matches
        // nothing).
        if never || parts.iter().any(|(c, _)| c.len() != 1) {
            return Ok(never_node());
        }
        let mut c = Vec::new();
        for (i, (mut t, d)) in parts.into_iter().enumerate() {
            if i > 0 {
                c.push(Node::Number(d));
            }
            c.append(&mut t);
        }
        Ok(Node::InOrder(c))
    }

    fn or_term(&mut self) -> P<Node> {
        let mut c = vec![self.term()?];
        while self.peek(0) == K::Or {
            self.bump();
            c.push(self.term()?);
        }
        Ok(wrap(c, Node::Or))
    }

    fn term(&mut self) -> P<Node> {
        match self.peek(0) {
            K::Term | K::Number => {
                let t = self.bump();
                let raw = &self.s[t.start..t.end];
                Ok(Node::Term(if self.emulator || raw.is_ascii() {
                    raw.to_ascii_lowercase()
                } else {
                    String::from_utf8_lossy(raw).to_lowercase().into_bytes()
                }))
            }
            _ => Err(self.parse_error("term")),
        }
    }
}

/// Prod's distance for a bare `AROUND` and for one it can't read (`AROUND(1.5)`).
const DEFAULT_GAP: i64 = 5;

/// The largest distance prod honours; `AROUND(256)` is `AROUND(5)`.
const MAX_GAP: i64 = 255;

/// A node no document matches (an empty term).
fn never_node() -> Node {
    Node::Term(Vec::new())
}

fn has_not(n: &Node) -> bool {
    match n {
        Node::Not(_) => true,
        Node::And(c) | Node::Or(c) | Node::Phrase(c) | Node::Around(c) | Node::InOrder(c) => {
            c.iter().any(has_not)
        }
        Node::Term(_) | Node::Number(_) => false,
    }
}

fn number(digits: &[u8]) -> i64 {
    std::str::from_utf8(digits)
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(i64::MAX)
}

/// NOT inside AROUND, which prod rejects.
fn not_in_around(n: &Node, around: bool) -> bool {
    match n {
        Node::Not(c) => around || not_in_around(c, around),
        Node::Around(c) | Node::InOrder(c) => c.iter().any(|c| not_in_around(c, true)),
        Node::And(c) | Node::Or(c) | Node::Phrase(c) => c.iter().any(|c| not_in_around(c, around)),
        Node::Term(_) | Node::Number(_) => false,
    }
}

fn wrap(mut c: Vec<Node>, f: fn(Vec<Node>) -> Node) -> Node {
    if c.len() > 1 {
        f(c)
    } else {
        c.pop().unwrap_or(Node::And(Vec::new()))
    }
}

/// Parses a SEARCH query. `None` means the query is empty after trimming separators.
/// `emulator` selects the emulator's grammar quirks (see the module docs).
#[cfg(test)]
pub fn parse(query: &str, emulator: bool) -> Result<Option<Node>> {
    parse_query(query, emulator).map(|q| q.map(|q| q.node))
}

/// A parsed SEARCH query.
pub struct Query {
    pub node: Node,
    /// Prod dropped a lower-case `or` from the query's top-level AND (`-a or`).
    pub dropped_or: bool,
}

/// `parse`, keeping what prod's grammar dropped.
pub fn parse_query(query: &str, emulator: bool) -> Result<Option<Query>> {
    let b = query.as_bytes();
    let starts = query.char_indices().map(|(i, _)| i);
    let Some(start) = starts.clone().find(|&i| !trimmed_at(b, i, emulator)) else {
        return Ok(None);
    };
    let last = starts
        .rev()
        .find(|&i| !trimmed_at(b, i, emulator))
        .unwrap_or(start);
    let orig = &b[start..last + utf8_len(b, last)];
    // Prod reads runs of single letters as one term, like its tokenizer (`x y` is `xy`).
    let joined =
        (!emulator).then(|| super::tokens::join_letters(&query[start..last + utf8_len(b, last)]));
    let s = joined.as_deref().map_or(orig, str::as_bytes);
    let mut toks = lex(s, emulator);
    if !emulator {
        toks = drop_dangling(toks);
        if toks.iter().all(|t| t.k == K::Eof) {
            return Ok(None);
        }
    }
    let mut p = Parser {
        s,
        emulator,
        toks,
        pos: 0,
        dropped_or: false,
        depth: 0,
    };
    let parsed = p.rquery();
    if !emulator
        && let Ok(n) = &parsed
        && not_in_around(n, false)
    {
        return Err(Error::invalid_argument(
            "Invalid use of negation found in search query. Negation may not be used within AROUND.",
        ));
    }
    parsed
        .map(|node| {
            Some(Query {
                node,
                dropped_or: p.dropped_or,
            })
        })
        .map_err(|e| {
            Error::invalid_argument(format!(
                "Error(s) parsing search query `{}`, has error(s):\n-{e}",
                String::from_utf8_lossy(orig)
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(n: &Node) -> String {
        let list = |tag: &str, c: &[Node]| {
            format!(
                "({tag} {})",
                c.iter().map(show).collect::<Vec<_>>().join(" ")
            )
        };
        match n {
            Node::Term(t) => String::from_utf8_lossy(t).into_owned(),
            Node::Number(v) => v.to_string(),
            Node::And(c) => list("a", c),
            Node::Or(c) => list("o", c),
            Node::Not(c) => format!("(n {})", show(c)),
            Node::Phrase(c) => list("p", c),
            Node::Around(c) => list("ar", c),
            Node::InOrder(c) => list("io", c),
        }
    }

    fn ok(q: &str, want: &str) {
        let got = parse(q, true)
            .unwrap()
            .map(|n| show(&n))
            .unwrap_or_default();
        assert_eq!(got, want, "query {q:?}");
    }

    fn err(q: &str, want: &str) {
        let e = parse(q, true).unwrap_err();
        assert!(e.message.contains(want), "query {q:?}: {}", e.message);
    }

    // Cases from the emulator's query_parser_test.cc.
    #[test]
    fn emulator_parser_cases() {
        ok("cloud spanner emulator", "(a cloud spanner emulator)");
        ok("cloud | spanner | emulator", "(o cloud spanner emulator)");
        ok("cloud|spanner|emulator", "(o cloud spanner emulator)");
        ok("cloud OR spanner OR emulator", "(o cloud spanner emulator)");
        ok(
            "cloudORspannerOR emulator ORany",
            "(a cloudorspanneror emulator orany)",
        );
        ok("-cloud", "(n cloud)");
        ok("cloud-spanner-emulator", "(p cloud spanner emulator)");
        ok("\"cloud spanner emulator\"", "(p cloud spanner emulator)");
        ok(
            "cloud AROUND(3) spanner AROUND(5) emulator",
            "(ar cloud 3 spanner 5 emulator)",
        );
        ok(
            "cloud AROUND(3) spanner emulator",
            "(a (ar cloud 3 spanner) emulator)",
        );
        ok(
            "cloud | spanner AROUND(3) emulator",
            "(ar (o cloud spanner) 3 emulator)",
        );
        ok(
            "-cloud AROUND(3) spanner | -emulator",
            "(ar (n cloud) 3 (o spanner (n emulator)))",
        );
        ok(
            "-cloud | spanner emulator",
            "(a (o (n cloud) spanner) emulator)",
        );
        ok("cloud | spanner-emulator", "(o cloud (p spanner emulator))");
        ok(
            "---cloud-spanner-emulator",
            "(n (n (n (p cloud spanner emulator))))",
        );
        ok("cloud\\\\spanner'emulator", "(p cloud spanner emulator)");
        ok("cloud:=spanner//=emulator", "(p cloud spanner emulator)");
        ok("cloud|spanner-emulator", "(p (o cloud spanner) emulator)");
        ok(
            "\"cloud|spanner -emulator\"",
            "(p (o cloud spanner) emulator)",
        );
        ok(
            "google|alphabet-cloud-spanner|sql-emulator|prod",
            "(p (o google alphabet) cloud (o spanner sql) (o emulator prod))",
        );
        ok("2panner-c10ud-4mulator", "(p 2panner c10ud 4mulator)");
        ok("ClOUd AROUND(3) EmuLAtor", "(ar cloud 3 emulator)");
        ok("CLOUD oR EMULATOR", "(a cloud or emulator)");
        ok("", "");
        ok(" @!cloud spanner!@ ", "(a cloud spanner)");
        ok("\\!term", "term");
        ok("term\\'!", "term");
        ok("\\!\\@term", "term");
        err("cloud || spanner", "Encountered error");
        err("cloud | AROUND(3) spanner", "Syntax error");
        err("cloud| spanner emulator", "Encountered error");
        err("|cloud spanner emulator", "Encountered error");
        err("OR cloud spanner emulator", "Encountered error");
        err("cloud spanner emulator|", "Encountered error");
        err("cloud spanner emulator OR", "Encountered error");
        err("\"cloud AROUND(3) spanner", "Encountered error");
        err("-(cloud spanner) | emulator", "Using parentheses");
        err("谷歌", "Encountered error");
        err("ClOUd ARouND(3) EmuLAtor", "Syntax error");
    }

    #[test]
    fn emulator_error_messages() {
        let m = |q: &str| parse(q, true).unwrap_err().message;
        assert!(m("acme  beta").ends_with("Syntax error on column 7: Expecting 'AROUND("));
        assert!(m("nårstad").ends_with("Syntax error on column 4: Expecting 'EOF"));
        assert!(m("øø").ends_with("Encountered error on column 4 while parsing: sub_expression. "));
        assert!(m("acme.").ends_with("Encountered error on column 5 while parsing: term. "));
        assert!(m("acme\tbeta\t\tgamma").ends_with("column 12: Expecting 'AROUND("));
        assert!(m("acme\\.com").ends_with("column 7: Expecting 'EOF"));
        assert!(m("acme AROUND(x) beta").ends_with("column 13: Expecting '<NUMBER>"));
        assert!(m("\"acme beta").ends_with("column 7: Expecting '\""));
    }

    fn prod(q: &str) -> String {
        match parse(q, false) {
            Ok(n) => n.map(|n| show(&n)).unwrap_or_default(),
            Err(e) => format!("error: {}", e.message),
        }
    }

    #[test]
    fn prod_grammar() {
        assert_eq!(prod("tromsø"), "tromsø");
        assert_eq!(prod("ÆRLIG ålesund"), "(a ærlig ålesund)");
        assert_eq!(prod("Åse Øvrebø"), "(a åse øvrebø)");
        assert_eq!(prod("Ola  Testmann"), "(a ola testmann)");
        assert_eq!(prod("Nordmann Hansen, Kari"), "(a nordmann hansen kari)");
        assert_eq!(prod("\"Bjørn Åse Ærlig AS\""), "(p bjørn åse ærlig as)");
        assert_eq!(
            prod("\"BJØRN ÅSE\" OR \"BJØRN ASE\""),
            "(o (p bjørn åse) (p bjørn ase))"
        );
        assert_eq!(prod("a  AROUND(2)  b"), "(ar a 2 b)");
        assert_eq!(prod("quick AROUND(two) fox"), "");
        assert_eq!(prod("quick AROUND(2)fox"), "(ar quick 2 fox)");
        assert_eq!(prod("quick AROUND(2 fox"), "(ar quick 2 fox)");
        assert_eq!(prod("spanner (cloud | fox)"), "(a spanner (o cloud fox))");
        assert_eq!(prod("fox or dog"), "(a fox dog)");
        assert!(prod("-fox AROUND(2) dog").contains("Negation may not be used within AROUND"));
        assert_eq!(prod("«tromsø»"), "tromsø");
        assert_eq!(prod("a – b"), "(a a b)");
        assert_eq!(prod("谷歌"), "谷歌");
        // Connectors that join nothing are ignored (#121).
        assert_eq!(prod("apple. banana"), "(a apple banana)");
        assert_eq!(prod("apple .banana"), "(a apple banana)");
        assert_eq!(prod("apple banana."), "(a apple banana)");
        assert_eq!(prod("apple. | banana"), "(o apple banana)");
        assert_eq!(prod("\"apple banana.\""), "(p apple banana)");
        assert_eq!(prod("apple -"), "apple");
        assert_eq!(prod("- -apple"), "(n apple)");
        assert_eq!(prod("apple.banana"), "(p apple banana)");
        assert_eq!(prod(". ."), "");
        // AROUND (#122).
        assert_eq!(prod("alpha AROUND omega"), "(ar alpha 5 omega)");
        assert_eq!(prod("alpha AROUND (2) omega"), "(ar alpha 2 omega)");
        assert_eq!(prod("alpha AROUND( 2 ) omega"), "(ar alpha 2 omega)");
        assert_eq!(prod("alpha AROUND(1.5) omega"), "(ar alpha 5 omega)");
        assert_eq!(prod("alpha AROUND(2147483647) omega"), "(ar alpha 5 omega)");
        assert_eq!(prod("alpha AROUND(255) omega"), "(ar alpha 255 omega)");
        assert_eq!(prod("alpha AROUND(256) omega"), "(ar alpha 5 omega)");
        assert_eq!(prod("alpha AROUND(1) | omega"), "(ar alpha 5 omega)");
        assert_eq!(prod("alpha AROUND(-1) omega"), "");
        assert_eq!(prod("alpha AROUND() omega"), "");
        assert_eq!(prod("alpha AROUND(1 2) omega"), "");
        assert_eq!(prod("alpha AROUND(1)"), "");
        assert_eq!(prod("quick AROUND | fox"), "(a quick (o around fox))");
        assert_eq!(prod("quick AROUND-fox"), "(a quick (p around fox))");
        assert_eq!(prod("\"quick AROUND fox\""), "(io quick 5 fox)");
        assert_eq!(prod("\"quick AROUND(1) fox\""), "(io quick 1 fox)");
        assert_eq!(prod("\"quick brown AROUND fox\""), "");
        assert_eq!(prod("\"quick AROUND\""), "(p quick around)");
        assert!(prod("quick AROUND -fox").contains("Negation may not be used within AROUND"));
        // Double negation and negated groups match nothing (#120).
        assert_eq!(prod("--apple"), "");
        assert_eq!(prod("---apple"), "");
        assert_eq!(prod("-(apple)"), "");
        assert_eq!(prod("cherry (-banana)"), "(a cherry )");
        assert_eq!(prod("-apple -banana"), "(a (n apple) (n banana))");
        assert_eq!(prod("\"the\""), "(p the)");
        // The emulator's grammar is unchanged.
        assert!(parse("Ola  Testmann", true).is_err());
        assert!(
            parse("alpha AROUND omega", true)
                .is_ok_and(|n| n.is_some_and(|n| show(&n) == "(a alpha around omega)"))
        );
        assert!(parse("apple. banana", true).is_err());
        assert_eq!(show(&parse("Åse", true).unwrap().unwrap()), "se");
    }
}
