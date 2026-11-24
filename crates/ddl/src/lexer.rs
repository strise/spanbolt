//! Tokenizer for GoogleSQL DDL.

use crate::DdlError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tok {
    /// Unquoted identifier or keyword.
    Ident(String),
    /// Backquoted identifier (unescaped).
    QuotedIdent(String),
    Int(String),
    Float(String),
    /// Decoded string literal.
    Str(String),
    /// Bytes literal, kept as source text.
    Bytes(String),
    /// Punctuation or operator.
    Sym(&'static str),
    Eof,
}

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

const SYMS: &[&str] = &[
    "=>", "<=", ">=", "!=", "<>", "||", "<<", ">>", "->", "(", ")", ",", ".", ";", "<", ">", "=",
    "+", "-", "*", "/", "%", "&", "|", "^", "~", "[", "]", "{", "}", ":", "?", "@", "!", "$",
];

pub(crate) fn tokenize(src: &str) -> Result<Vec<Token>, DdlError> {
    let b = src.as_bytes();
    let mut out = Vec::with_capacity(src.len() / 4);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'#' || (c == b'-' && b.get(i + 1) == Some(&b'-')) {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let Some(end) = src[i + 2..].find("*/") else {
                return Err(syntax(src, i, "Unterminated comment"));
            };
            i += end + 4;
            continue;
        }
        let start = i;
        // String / bytes literals with optional r/b prefixes.
        if c == b'\'' || c == b'"' || is_literal_prefix(b, i) {
            let mut j = i;
            let mut raw = false;
            let mut bytes = false;
            while j < b.len() && b[j] != b'\'' && b[j] != b'"' {
                match b[j] {
                    b'r' | b'R' => raw = true,
                    b'b' | b'B' => bytes = true,
                    _ => {}
                }
                j += 1;
            }
            let (value, end) = lex_string(src, j, raw)?;
            let tok = if bytes {
                Tok::Bytes(src[start..end].to_string())
            } else {
                Tok::Str(value)
            };
            out.push(Token { tok, start, end });
            i = end;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            out.push(Token {
                tok: Tok::Ident(src[i..j].to_string()),
                start,
                end: j,
            });
            i = j;
            continue;
        }
        if c == b'`' {
            let mut j = i + 1;
            let mut name = String::new();
            loop {
                match b.get(j) {
                    None => return Err(syntax(src, i, "Unterminated quoted identifier")),
                    Some(b'`') => break,
                    Some(b'\\') => {
                        if let Some(&n) = b.get(j + 1) {
                            name.push(n as char);
                        }
                        j += 2;
                    }
                    Some(_) => {
                        let ch = src[j..].chars().next().unwrap_or('?');
                        name.push(ch);
                        j += ch.len_utf8();
                    }
                }
            }
            out.push(Token {
                tok: Tok::QuotedIdent(name),
                start,
                end: j + 1,
            });
            i = j + 1;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let (tok, end) = lex_number(src, i);
            out.push(Token { tok, start, end });
            i = end;
            continue;
        }
        let rest = &src[i..];
        match SYMS.iter().find(|s| rest.starts_with(**s)) {
            Some(s) => {
                out.push(Token {
                    tok: Tok::Sym(s),
                    start,
                    end: i + s.len(),
                });
                i += s.len();
            }
            None => {
                let ch = rest.chars().next().unwrap_or('?');
                return Err(syntax(src, i, &format!("Encountered '{ch}'")));
            }
        }
    }
    out.push(Token {
        tok: Tok::Eof,
        start: src.len(),
        end: src.len(),
    });
    Ok(out)
}

fn is_literal_prefix(b: &[u8], i: usize) -> bool {
    let p = |k: usize| b.get(i + k).map(|c| c.to_ascii_lowercase());
    let quote = |k: usize| matches!(b.get(i + k), Some(b'\'') | Some(b'"'));
    match (p(0), p(1)) {
        (Some(b'r'), Some(b'b')) | (Some(b'b'), Some(b'r')) => quote(2),
        (Some(b'r'), _) | (Some(b'b'), _) => quote(1),
        _ => false,
    }
}

/// Lexes a quoted literal starting at the opening quote; returns (decoded, end offset).
fn lex_string(src: &str, start: usize, raw: bool) -> Result<(String, usize), DdlError> {
    let b = src.as_bytes();
    let q = b[start];
    let triple = b.get(start + 1) == Some(&q) && b.get(start + 2) == Some(&q);
    let mut i = start + if triple { 3 } else { 1 };
    let mut out = String::new();
    loop {
        let Some(&c) = b.get(i) else {
            return Err(syntax(src, start, "Unterminated string literal"));
        };
        if c == q {
            if !triple {
                return Ok((out, i + 1));
            }
            if b.get(i + 1) == Some(&q) && b.get(i + 2) == Some(&q) {
                return Ok((out, i + 3));
            }
        }
        if c == b'\n' && !triple {
            return Err(syntax(src, start, "Unterminated string literal"));
        }
        if c == b'\\' {
            let Some(&n) = b.get(i + 1) else {
                return Err(syntax(src, start, "Unterminated string literal"));
            };
            if raw {
                out.push('\\');
                out.push(n as char);
                i += 2;
                continue;
            }
            i += 2;
            match n {
                b'n' => out.push('\n'),
                b't' => out.push('\t'),
                b'r' => out.push('\r'),
                b'a' => out.push('\x07'),
                b'b' => out.push('\x08'),
                b'f' => out.push('\x0c'),
                b'v' => out.push('\x0b'),
                b'\\' | b'\'' | b'"' | b'`' | b'?' => out.push(n as char),
                b'x' | b'X' => {
                    let v = hex(src, i, 2).ok_or_else(|| syntax(src, i, "Illegal escape"))?;
                    out.push(char::from_u32(v).unwrap_or('?'));
                    i += 2;
                }
                b'u' | b'U' => {
                    let len = if n == b'u' { 4 } else { 8 };
                    let v = hex(src, i, len).ok_or_else(|| syntax(src, i, "Illegal escape"))?;
                    out.push(char::from_u32(v).ok_or_else(|| syntax(src, i, "Illegal escape"))?);
                    i += len;
                }
                b'0'..=b'7' => {
                    let digits = src.get(i - 1..i + 2).unwrap_or("");
                    let v = u32::from_str_radix(digits, 8)
                        .map_err(|_| syntax(src, i, "Illegal escape"))?;
                    out.push(char::from_u32(v).unwrap_or('?'));
                    i += 2;
                }
                _ => return Err(syntax(src, i, "Illegal escape sequence")),
            }
            continue;
        }
        let ch = src[i..].chars().next().unwrap_or('?');
        out.push(ch);
        i += ch.len_utf8();
    }
}

fn hex(src: &str, i: usize, len: usize) -> Option<u32> {
    u32::from_str_radix(src.get(i..i + len)?, 16).ok()
}

fn lex_number(src: &str, start: usize) -> (Tok, usize) {
    let b = src.as_bytes();
    let mut i = start;
    if b[i] == b'0' && matches!(b.get(i + 1), Some(b'x') | Some(b'X')) {
        i += 2;
        while i < b.len() && b[i].is_ascii_hexdigit() {
            i += 1;
        }
        return (Tok::Int(src[start..i].to_string()), i);
    }
    let mut float = false;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        float = true;
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            float = true;
            i = j;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    let text = src[start..i].to_string();
    (
        if float {
            Tok::Float(text)
        } else {
            Tok::Int(text)
        },
        i,
    )
}

/// 1-based line and column of a byte offset.
pub(crate) fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let before = &src[..offset.min(src.len())];
    let line = before.matches('\n').count() + 1;
    let col = before
        .rfind('\n')
        .map_or(before.len(), |p| before.len() - p - 1)
        + 1;
    (line, col)
}

pub(crate) fn syntax(src: &str, offset: usize, msg: &str) -> DdlError {
    let (line, col) = line_col(src, offset);
    DdlError::Syntax(format!(
        "Error parsing Spanner DDL statement: {src} : Syntax error on line {line}, column {col}: {msg}"
    ))
}

/// GoogleSQL reserved keywords; these must be backquoted to be used as identifiers.
pub const RESERVED: &[&str] = &[
    "ALL",
    "AND",
    "ANY",
    "ARRAY",
    "AS",
    "ASC",
    "ASSERT_ROWS_MODIFIED",
    "AT",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "COLLATE",
    "CONTAINS",
    "CREATE",
    "CROSS",
    "CUBE",
    "CURRENT",
    "DEFAULT",
    "DEFINE",
    "DESC",
    "DISTINCT",
    "ELSE",
    "END",
    "ENUM",
    "ESCAPE",
    "EXCEPT",
    "EXCLUDE",
    "EXISTS",
    "EXTRACT",
    "FALSE",
    "FETCH",
    "FOLLOWING",
    "FOR",
    "FROM",
    "FULL",
    "GROUP",
    "GROUPING",
    "GROUPS",
    "HASH",
    "HAVING",
    "IF",
    "IGNORE",
    "IN",
    "INNER",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "JOIN",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "LOOKUP",
    "MERGE",
    "NATURAL",
    "NEW",
    "NO",
    "NOT",
    "NULL",
    "NULLS",
    "OF",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRECEDING",
    "PROTO",
    "QUALIFY",
    "RANGE",
    "RECURSIVE",
    "RESPECT",
    "RIGHT",
    "ROLLUP",
    "ROWS",
    "SELECT",
    "SET",
    "SOME",
    "STRUCT",
    "TABLESAMPLE",
    "THEN",
    "TO",
    "TREAT",
    "TRUE",
    "UNBOUNDED",
    "UNION",
    "UNNEST",
    "USING",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "WITHIN",
];

/// The identifiers in `sql`, quoted or not, in source order; string and bytes literals and comments
/// are skipped. `None` when `sql` does not tokenize.
pub fn identifiers(sql: &str) -> Option<Vec<String>> {
    let toks = tokenize(sql).ok()?;
    Some(
        toks.into_iter()
            .filter_map(|t| match t.tok {
                Tok::Ident(w) | Tok::QuotedIdent(w) => Some(w),
                _ => None,
            })
            .collect(),
    )
}

pub fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|k| k.eq_ignore_ascii_case(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_skip_literals_and_comments() {
        assert_eq!(
            identifiers("CONCAT(Src, 'Note', b'Tag', r\"K\") -- Other\n || `Mid`").unwrap(),
            ["CONCAT", "Src", "Mid"]
        );
        assert_eq!(identifiers("'unterminated"), None);
    }

    fn toks(s: &str) -> Vec<Tok> {
        tokenize(s).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn literals() {
        assert_eq!(
            toks(r#"'a\'b' "c" '''x'y''' r'\n' b'\x00' rb"z" 1.5 0x1F 3"#),
            vec![
                Tok::Str("a'b".into()),
                Tok::Str("c".into()),
                Tok::Str("x'y".into()),
                Tok::Str("\\n".into()),
                Tok::Bytes(r"b'\x00'".into()),
                Tok::Bytes(r#"rb"z""#.into()),
                Tok::Float("1.5".into()),
                Tok::Int("0x1F".into()),
                Tok::Int("3".into()),
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn comments_and_idents() {
        assert_eq!(
            toks("a -- x\n# y\n/* z */ `b c` => r"),
            vec![
                Tok::Ident("a".into()),
                Tok::QuotedIdent("b c".into()),
                Tok::Sym("=>"),
                Tok::Ident("r".into()),
                Tok::Eof,
            ]
        );
    }
}
