//! The rquery parser SEARCH uses: a port of cloud-spanner-emulator
//! `backend/query/search/query_parser.jjt` and `query_parser.cc` (Apache-2.0), including the
//! JavaCC token manager's longest-match rules and its error messages.
//!
//! In emulator mode the parser reads bytes like the emulator: non-ASCII characters are skipped by
//! the lexer, so a query term with letters such as `ø` splits into two terms and fails to parse,
//! and two separators in a row (`a  b`, `a, b`) are a syntax error. Prod mode (the default) follows
//! Cloud Spanner: Unicode letters and digits are term characters (lower-cased with Unicode rules),
//! other non-ASCII characters separate terms, and a run of separators is one separator.

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
            self.expect(K::Around)?;
            if self.emulator {
                let n = self.expect(K::Number)?;
                c.push(Node::Number(number(&self.s[n.start..n.end])));
                self.expect(K::Close)?;
                self.expect(K::Sep)?;
            } else {
                // Prod: `AROUND(two)` parses and matches nothing; `)` and the separator after it
                // may be missing.
                let n = self.bump();
                c.push(Node::Number(match n.k {
                    K::Number => number(&self.s[n.start..n.end]),
                    K::Term => {
                        never = true;
                        DEFAULT_GAP
                    }
                    _ => return Err(self.parse_error("around_expression")),
                }));
                if self.peek(0) == K::Close {
                    self.bump();
                }
            }
            while self.peek(0) == K::Sep {
                self.bump();
            }
            c.push(self.or_expression()?);
        }
        if never {
            // An empty term: no token matches it.
            return Ok(Node::Term(Vec::new()));
        }
        Ok(wrap(c, Node::Around))
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
        self.peek(n) == K::Around
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
                Ok(Node::Not(Box::new(self.sub_expression()?)))
            }
            K::Quote => self.quoted_phrase(),
            K::Term | K::Number => self.connected_phrase(),
            // Prod groups with parentheses.
            K::Open if !self.emulator => {
                self.bump();
                let n = self.and_expression()?;
                if self.peek(0) == K::Close {
                    self.bump();
                }
                Ok(n)
            }
            _ => Err(self.parse_error("sub_expression")),
        }
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
        let mut c = vec![self.or_term()?];
        let inner = |k: K| matches!(k, K::Sep | K::Connector | K::Hyphen);
        while inner(self.peek(0)) {
            while inner(self.peek(0)) {
                self.bump();
            }
            c.push(self.or_term()?);
        }
        self.expect(K::Quote)?;
        Ok(wrap(c, Node::Phrase))
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

/// Any valid distance; `AROUND(two)` matches nothing whatever the distance.
const DEFAULT_GAP: i64 = 5;

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
        Node::Around(c) => c.iter().any(|c| not_in_around(c, true)),
        Node::And(c) | Node::Or(c) | Node::Phrase(c) => c.iter().any(|c| not_in_around(c, around)),
        Node::Term(_) | Node::Number(_) => false,
    }
}

/// A query with only negated parts, which matches nothing on prod (`--cloud`, `-a -b`).
pub fn only_negations(n: &Node) -> bool {
    match n {
        Node::Not(_) => true,
        Node::And(c) => c.iter().all(only_negations),
        _ => false,
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
pub fn parse(query: &str, emulator: bool) -> Result<Option<Node>> {
    let b = query.as_bytes();
    let starts = query.char_indices().map(|(i, _)| i);
    let Some(start) = starts.clone().find(|&i| !trimmed_at(b, i, emulator)) else {
        return Ok(None);
    };
    let last = starts
        .rev()
        .find(|&i| !trimmed_at(b, i, emulator))
        .unwrap_or(start);
    let s = &b[start..last + utf8_len(b, last)];
    let mut p = Parser {
        s,
        emulator,
        toks: lex(s, emulator),
        pos: 0,
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
    parsed.map(Some).map_err(|e| {
        Error::invalid_argument(format!(
            "Error(s) parsing search query `{}`, has error(s):\n-{e}",
            String::from_utf8_lossy(s)
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
        // The emulator's grammar is unchanged.
        assert!(parse("Ola  Testmann", true).is_err());
        assert_eq!(show(&parse("Åse", true).unwrap().unwrap()), "se");
    }
}
