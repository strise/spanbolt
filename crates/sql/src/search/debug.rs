//! DEBUG_TOKENLIST: prints a TOKENLIST the way Cloud Spanner does (the emulator doesn't have it).
//! The format comes from recordings (`conformance/cases/sql/debug_tokenlist*.case`):
//!
//! - Tokens at one position print as `[a, b]`, sorted by their printed form; positions are
//!   separated by `, `. A token is C-escaped (`\t`, `\"`, `\x01`, a hex digit after a `\x` escape
//!   is escaped too), bytes that aren't UTF-8 print as `?`, and it is quoted when it holds a
//!   space, a comma, a parenthesis or a bracket.
//! - TOKENIZE_FULLTEXT marks the first and last position of every value `(boundary)` and
//!   `(end_boundary)`; alternates (`foo@bar` next to `foo`) share a position.
//! - TOKENIZE_SUBSTRING prints the n-grams shorter than `ngram_size_max` as one sorted set first,
//!   then each word's longest n-grams, with `<gap(1)>` between words.
//! - TOKENIZE_NGRAMS puts the shorter n-grams in the position of the first longest one.
//! - Array elements and TOKENLIST_CONCAT parts are `<gap(25)>` apart. A concatenated list that adds
//!   nothing still leaves its gap behind; an array element that adds nothing doesn't.
//! - TOKEN prints its values sorted, TOKENIZE_BOOL `y`/`n`, TOKENIZE_NUMBER `==v` and, for range
//!   search, the log tree's ranges, TOKENIZE_JSON a binary encoding of every path.
//!
//! UNIMPLEMENTED where the format isn't known: TOKENIZE_SUBSTRING with relative search types, and
//! TOKENIZE_NUMBER over DOUBLE values, with `algorithm => 'prefixtree'` or over arrays without
//! equality.

use spanbolt_storage::{Error, Result, Value};

use super::tokens::{
    self, ALT, BOOL, FULLTEXT, GAP, JSON, NGRAMS, NUMBER, SUBSTRING, TOKEN, VALUE,
};

/// Gap between array elements and between TOKENLIST_CONCAT parts.
const VALUE_GAP: u32 = 25;

fn unimplemented(what: &str) -> Error {
    crate::types::unimplemented(format!("DEBUG_TOKENLIST over {what}"))
}

pub fn debug_tokenlist(args: &[Value]) -> Result<Value> {
    let Value::TokenList(b) = &args[0] else {
        return Ok(Value::Null);
    };
    let toks = tokens::decode(b)?;
    // Only TOKENLIST_CONCAT of no lists has no signature.
    let Some(first) = toks.first() else {
        return Ok(Value::Null);
    };
    let one_value = |kind: &str| -> Result<Option<&[u8]>> {
        match &toks[1..] {
            [] => Ok(None),
            [v] if v.first() == Some(&VALUE) => Ok(Some(&v[1..])),
            _ => Err(unimplemented(&format!("TOKENLIST_CONCAT of {kind} lists"))),
        }
    };
    let printed = match *first {
        t if t == TOKEN.as_bytes() => one_value("TOKEN")?.map(token).transpose()?,
        t if t == BOOL.as_bytes() => one_value("TOKENIZE_BOOL")?.map(fmt_token),
        t if t == NUMBER.as_bytes() => one_value("TOKENIZE_NUMBER")?.map(number).transpose()?,
        t if t == JSON.as_bytes() => one_value("TOKENIZE_JSON")?.map(json).transpose()?,
        _ => text(&toks)?.map(|items| print(&items)),
    };
    Ok(printed.map_or(Value::Null, |s| Value::string(&s)))
}

enum Item {
    Pos(Vec<Vec<u8>>, &'static [&'static str]),
    Gap(u32),
}

fn pos(t: Vec<u8>) -> Item {
    Item::Pos(vec![t], &[])
}

fn print(items: &[Item]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    let mut gap = 0;
    for it in items {
        match it {
            Item::Gap(n) => {
                gap += n;
                continue;
            }
            Item::Pos(toks, attrs) => {
                if gap > 0 {
                    out.push(format!("<gap({gap})>"));
                    gap = 0;
                }
                let mut ps: Vec<String> = toks.iter().map(|t| fmt_token(t)).collect();
                ps.sort();
                ps.dedup();
                let mut s = if ps.len() == 1 {
                    ps.remove(0)
                } else {
                    format!("[{}]", ps.join(", "))
                };
                if !attrs.is_empty() {
                    s = format!("{s}({})", attrs.join(", "));
                }
                out.push(s);
            }
        }
    }
    if gap > 0 {
        out.push(format!("<gap({gap})>"));
    }
    out.join(", ")
}

/// A token as Cloud Spanner prints it: `absl::CHexEscape` that keeps UTF-8, `?` for bytes that
/// aren't UTF-8, quoted when it holds a space, a comma, a parenthesis or a bracket.
fn fmt_token(t: &[u8]) -> String {
    let mut out = String::with_capacity(t.len());
    let mut hex = false;
    let mut rest = t;
    while !rest.is_empty() {
        let (c, n) = match std::str::from_utf8(&rest[..utf8_len(rest[0]).min(rest.len())]) {
            Ok(s) => match s.chars().next() {
                Some(c) => (Some(c), s.len()),
                None => (None, 1),
            },
            Err(_) => (None, 1),
        };
        rest = &rest[n..];
        let Some(c) = c else {
            out.push('?');
            hex = false;
            continue;
        };
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c == '\x7f' || (hex && c.is_ascii_hexdigit()) => {
                out.push_str(&format!("\\x{:02x}", c as u32));
                hex = true;
                continue;
            }
            c => out.push(c),
        }
        hex = false;
    }
    if t.iter().any(|b| b" ,()[]".contains(b)) {
        format!("\"{out}\"")
    } else {
        out
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0xf0..=0xf7 => 4,
        0xe0..=0xef => 3,
        0xc0..=0xdf => 2,
        _ => 1,
    }
}

/// TOKEN: the values, sorted and without duplicates.
fn token(v: &[u8]) -> Result<String> {
    let mut vals = Vec::new();
    let mut rest = v;
    while !rest.is_empty() {
        let mut n = 0usize;
        let mut shift = 0;
        loop {
            let (&b, r) = rest
                .split_first()
                .ok_or_else(|| Error::internal("malformed TOKEN list"))?;
            rest = r;
            n |= ((b & 0x7f) as usize) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        let (t, r) = rest
            .split_at_checked(n)
            .ok_or_else(|| Error::internal("malformed TOKEN list"))?;
        vals.push(t);
        rest = r;
    }
    vals.sort();
    vals.dedup();
    Ok(vals
        .iter()
        .map(|t| fmt_token(t))
        .collect::<Vec<_>>()
        .join(", "))
}

/// TOKENIZE_NUMBER: `==v`, then (for range search) every range of the log tree that holds `v`.
fn number(v: &[u8]) -> Result<String> {
    let ints: Vec<i64> = v
        .get(1..)
        .unwrap_or_default()
        .chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap_or_default()))
        .collect();
    match v.first() {
        Some(b'E') => {
            let mut vs = ints;
            vs.sort();
            vs.dedup();
            Ok(vs
                .iter()
                .map(|v| format!("=={v}"))
                .collect::<Vec<_>>()
                .join(", "))
        }
        Some(b'L') if ints.len() == 5 => {
            let [base, min, max, g, v] =
                [ints[0], ints[1], ints[2], ints[3], ints[4]].map(i128::from);
            let mut out = vec![format!("=={v}")];
            for (lo, hi) in log_tree(base as u128, min, max, g, v) {
                out.push(format!("[{lo}, {hi}]"));
            }
            Ok(out.join(", "))
        }
        _ => Err(unimplemented(
            "TOKENIZE_NUMBER lists of DOUBLE values, prefix trees or arrays",
        )),
    }
}

/// The ranges of the log tree that hold `v` (clamped to `[min, max]`), sorted. Buckets are
/// `granularity` wide around `min + i * granularity`; the tree has a level for every power of
/// `base` that is at most the number of buckets and fits in 64 bits.
fn log_tree(base: u128, min: i128, max: i128, g: i128, v: i128) -> Vec<(i128, i128)> {
    let bucket = |x: i128| (x - min + g / 2) / g;
    let i = bucket(v.clamp(min, max)) as u128;
    let buckets = bucket(max) as u128 + 1;
    let mut out = Vec::new();
    let mut size: u128 = 1;
    while size <= buckets && size <= u64::MAX as u128 {
        let lo_i = (i - i % size) as i128;
        let hi_i = lo_i + size as i128 - 1;
        let lo = (min + lo_i * g - g / 2).max(min);
        let hi = min + (hi_i + 1) * g - g / 2 - 1;
        out.push((lo, hi));
        size *= base;
    }
    out.sort();
    out.dedup();
    out
}

/// TOKENIZE_JSON: a token per node, its path from the root encoded as type tags (`\x0c` object,
/// `\x10` array), key lengths and keys, and for a scalar its encoded value.
fn json(v: &[u8]) -> Result<String> {
    let doc: serde_json::Value =
        serde_json::from_slice(v).map_err(|_| Error::internal("malformed JSON list"))?;
    let mut out = Vec::new();
    json_tokens(Vec::new(), &doc, &mut out)?;
    Ok(out
        .iter()
        .map(|t| fmt_token(t))
        .collect::<Vec<_>>()
        .join(", "))
}

fn json_tokens(mut path: Vec<u8>, v: &serde_json::Value, out: &mut Vec<Vec<u8>>) -> Result<()> {
    use serde_json::Value as J;
    match v {
        J::Null => path.push(0x00),
        J::Bool(true) => path.push(0x04),
        J::Bool(false) => path.push(0x08),
        J::Number(n) => {
            let int = n.as_i64().or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && *f >= -(2f64.powi(63)) && *f < 2f64.powi(63))
                    .map(|f| f as i64)
            });
            match int {
                Some(i @ 0..=15) => path.push(((i as u8) << 4) | 0x02),
                Some(i) => {
                    let m = i.unsigned_abs();
                    let bytes = m.to_le_bytes();
                    let n = 8 - (m.leading_zeros() as usize / 8);
                    path.push(0x0a | (((n - 1) as u8) << 5) | if i < 0 { 0x10 } else { 0 });
                    path.extend_from_slice(&bytes[..n]);
                }
                None if n.is_u64() => {
                    return Err(unimplemented("TOKENIZE_JSON numbers above INT64"));
                }
                None => {
                    path.push(0x14);
                    path.extend_from_slice(&n.as_f64().unwrap_or(0.0).to_le_bytes());
                }
            }
        }
        J::String(s) => {
            tokens::put_varint(&mut path, ((s.len() as u64) << 2) | 1);
            path.extend_from_slice(s.as_bytes());
        }
        J::Array(a) => {
            path.push(0x10);
            out.push(path.clone());
            for e in a {
                json_tokens(path.clone(), e, out)?;
            }
            return Ok(());
        }
        J::Object(o) => {
            path.push(0x0c);
            out.push(path.clone());
            for (k, e) in o {
                let mut p = path.clone();
                tokens::put_len_prefixed(&mut p, k.as_bytes());
                out.push(p.clone());
                json_tokens(p, e, out)?;
            }
            return Ok(());
        }
    }
    out.push(path);
    Ok(())
}

/// The items of a TOKENIZE_FULLTEXT, TOKENIZE_SUBSTRING or TOKENIZE_NGRAMS list, or of a
/// TOKENLIST_CONCAT of them: one part per signature. None when every part is NULL.
fn text(toks: &[&[u8]]) -> Result<Option<Vec<Item>>> {
    let mut items: Vec<Item> = Vec::new();
    let mut any = false;
    let mut i = 0;
    while i < toks.len() {
        let sig: Vec<&[u8]> = toks[i].split(|&c| c == b'-').collect();
        let end = toks[i + 1..]
            .iter()
            .position(|t| tokens::is_signature(t))
            .map_or(toks.len(), |p| i + 1 + p);
        let body = &toks[i + 1..end];
        i = end;
        // `fulltext-<null>`, `ngrams-<max>-<min>-<null>`, `substring-<max>-<min>-<null>-<types>`.
        let null_at = if sig[0] == FULLTEXT.as_bytes() { 1 } else { 3 };
        if sig.get(null_at) == Some(&&b"1"[..]) {
            continue; // a NULL value
        }
        let num = |k: usize| -> i64 {
            sig.get(k)
                .and_then(|p| std::str::from_utf8(p).ok())
                .and_then(|p| p.parse().ok())
                .unwrap_or(0)
        };
        let part = match sig[0] {
            k if k == FULLTEXT.as_bytes() => fulltext(body),
            k if k == SUBSTRING.as_bytes() => {
                if num(4) != 0 {
                    return Err(unimplemented(
                        "TOKENIZE_SUBSTRING with relative_search_types",
                    ));
                }
                substring(body, num(1), num(2))
            }
            k if k == NGRAMS.as_bytes() => ngrams(body, num(1)),
            _ => return Err(Error::internal("malformed TOKENLIST")),
        };
        if any && !items.is_empty() {
            items.push(Item::Gap(VALUE_GAP));
        }
        any = true;
        items.extend(part);
    }
    Ok(any.then_some(items))
}

/// The values of a list: array elements (and the trailing TOKENLIST_CONCAT gap) split at GAP.
fn values<'a>(body: &'a [&'a [u8]]) -> impl Iterator<Item = &'a [&'a [u8]]> {
    body.split(|t| *t == GAP)
}

fn fulltext(body: &[&[u8]]) -> Vec<Item> {
    let mut out = Vec::new();
    for v in values(body) {
        let mut ps: Vec<Vec<Vec<u8>>> = Vec::new();
        for t in v {
            match (t.strip_prefix(&[ALT][..]), ps.last_mut()) {
                (Some(a), Some(last)) => last.push(a.to_vec()),
                _ => ps.push(vec![t.to_vec()]),
            }
        }
        if ps.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Item::Gap(VALUE_GAP));
        }
        let n = ps.len();
        for (j, p) in ps.into_iter().enumerate() {
            let attrs: &'static [&'static str] = match (j == 0, j == n - 1) {
                (true, true) => &["boundary", "end_boundary"],
                (true, false) => &["boundary"],
                (false, true) => &["end_boundary"],
                _ => &[],
            };
            out.push(Item::Pos(p, attrs));
        }
    }
    out
}

/// Character n-grams of `s` of `n` characters.
fn grams(chars: &[(usize, char)], s: &str, n: usize) -> Vec<Vec<u8>> {
    if n == 0 || n > chars.len() {
        return Vec::new();
    }
    (0..=chars.len() - n)
        .map(|i| {
            let end = chars.get(i + n).map_or(s.len(), |c| c.0);
            s.as_bytes()[chars[i].0..end].to_vec()
        })
        .collect()
}

fn substring(body: &[&[u8]], max: i64, min: i64) -> Vec<Item> {
    let mut short: Vec<Vec<u8>> = Vec::new();
    let mut values_out: Vec<Vec<Item>> = Vec::new();
    for v in values(body) {
        let mut words_out: Vec<Vec<Vec<u8>>> = Vec::new();
        for t in v {
            for w in tokens::substring_words(tokens::unescape(t), false) {
                let Ok(w) = std::str::from_utf8(w) else {
                    continue;
                };
                let chars: Vec<(usize, char)> = w.char_indices().collect();
                let k = (max.max(1) as usize).min(chars.len());
                for n in (min.max(1) as usize)..k {
                    short.extend(grams(&chars, w, n));
                }
                words_out.push(grams(&chars, w, k));
            }
        }
        if words_out.is_empty() {
            continue;
        }
        let mut items = Vec::new();
        for (j, w) in words_out.into_iter().enumerate() {
            if j > 0 {
                items.push(Item::Gap(1));
            }
            items.extend(w.into_iter().map(pos));
        }
        values_out.push(items);
    }
    // Prod leaves out the short n-grams that are also tokens at a position (`foo` of `foo_bar`
    // when the text has the word `foo`).
    let long: std::collections::HashSet<&[u8]> = values_out
        .iter()
        .flatten()
        .filter_map(|i| match i {
            Item::Pos(ts, _) => Some(ts.iter().map(Vec::as_slice)),
            _ => None,
        })
        .flatten()
        .collect();
    short.retain(|g| !long.contains(g.as_slice()));
    let mut out = Vec::new();
    if !short.is_empty() {
        out.push(Item::Pos(short, &[]));
    }
    for (j, v) in values_out.into_iter().enumerate() {
        if j > 0 {
            out.push(Item::Gap(VALUE_GAP));
        }
        out.extend(v);
    }
    out
}

fn ngrams(body: &[&[u8]], max: i64) -> Vec<Item> {
    let mut short: Vec<Vec<u8>> = Vec::new();
    let mut out: Vec<Item> = Vec::new();
    for v in values(body) {
        if v.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Item::Gap(VALUE_GAP));
        }
        for t in v {
            let t = tokens::unescape(t);
            let n = std::str::from_utf8(t).map_or(t.len(), |s| s.chars().count());
            if n as i64 >= max {
                out.push(pos(t.to_vec()));
            } else {
                short.push(t.to_vec());
            }
        }
    }
    if !short.is_empty() {
        match out.first_mut() {
            Some(Item::Pos(first, _)) => first.extend(short),
            _ => out.insert(0, Item::Pos(short, &[])),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn s(v: &str) -> Value {
        Value::string(v)
    }

    fn arr(vs: &[&str]) -> Value {
        Value::Array(vs.iter().map(|v| s(v)).collect::<Vec<_>>().into())
    }

    fn debug(v: Result<Value>) -> Option<String> {
        match debug_tokenlist(&[v.expect("tokenize")]).expect("debug") {
            Value::String(s) => Some(s.to_string()),
            Value::Null => None,
            v => panic!("{v:?}"),
        }
    }

    fn named(args: &[(usize, Value)], n: usize) -> Vec<Value> {
        let mut out = vec![Value::Null; n];
        for (i, v) in args {
            out[*i] = v.clone();
        }
        out
    }

    /// From `conformance/cases/sql/debug_tokenlist*.case` (Cloud Spanner).
    #[test]
    fn text_lists_match_recordings() {
        let ft = |v: Value| debug(tokens::tokenize_fulltext(&[v], false));
        assert_eq!(
            ft(s("alpha beta gamma")).unwrap(),
            "alpha(boundary), beta, gamma(end_boundary)"
        );
        assert_eq!(
            ft(s("foo@bar")).unwrap(),
            "[foo, foo@bar](boundary), bar(end_boundary)"
        );
        assert_eq!(
            ft(arr(&["a", "!!", "b"])).unwrap(),
            "a(boundary, end_boundary), <gap(25)>, b(boundary, end_boundary)"
        );
        assert_eq!(ft(s("")).unwrap(), "");
        assert_eq!(ft(Value::Null), None);

        let sub = |v: Value, max: i64, min: i64| {
            debug(tokens::tokenize_substring(
                &named(&[(0, v), (1, Value::Int64(max)), (2, Value::Int64(min))], 6),
                false,
            ))
            .unwrap()
        };
        assert_eq!(
            sub(s("alpha beta"), 4, 1),
            "[a, al, alp, b, be, bet, e, et, eta, h, ha, l, lp, lph, p, ph, pha, t, ta], alph, lpha, <gap(1)>, beta"
        );
        assert_eq!(
            sub(s("a bc def"), 3, 2),
            "[de, ef], a, <gap(1)>, bc, <gap(1)>, def"
        );
        assert_eq!(sub(s("aaaaaa"), 2, 1), "a, aa, aa, aa, aa, aa");
        assert_eq!(
            sub(arr(&["ab", "cdefg"]), 4, 1),
            "[a, b, c, cd, cde, d, de, def, e, ef, efg, f, fg, g], ab, <gap(25)>, cdef, defg"
        );

        let ng = |v: Value| debug(tokens::tokenize_ngrams(&[v], false)).unwrap();
        assert_eq!(
            ng(s("alpha")),
            "[a, al, alp, alph, h, ha, l, lp, lph, p, ph, pha], lpha"
        );
        assert_eq!(ng(arr(&["ab", "cd"])), "[a, ab, b, c, cd, d]");
        assert_eq!(
            ng(arr(&["abcde", "fg", "hijkl"])),
            "[a, ab, abc, abcd, b, bc, bcd, c, cd, cde, d, de, e, f, fg, g, h, hi, hij, i, ij, ijk, j, jk, jkl, k, kl, l], bcde, <gap(50)>, hijk, ijkl"
        );
        assert_eq!(
            ng(s("abc  def")),
            r#"["  ", "  d", " ", " d", " de", "abc ", "bc ", "c  ", "c ", a, ab, abc, b, bc, c, d, de, def, e, ef, f], "bc  ", "c  d", "  de", " def""#
        );
    }

    #[test]
    fn concat_leaves_gaps_like_spanner() {
        let ft = |v: Value| tokens::tokenize_fulltext(&[v], false).unwrap();
        let cat =
            |vs: Vec<Value>| debug(tokens::tokenlist_concat(&[Value::Array(vs.into())], false));
        assert_eq!(
            cat(vec![ft(s("ab")), ft(arr(&[]))]).unwrap(),
            "ab(boundary, end_boundary), <gap(25)>"
        );
        assert_eq!(
            cat(vec![ft(s("")), ft(s("a"))]).unwrap(),
            "a(boundary, end_boundary)"
        );
        assert_eq!(
            cat(vec![ft(Value::Null), ft(s("a"))]).unwrap(),
            "a(boundary, end_boundary)"
        );
        assert_eq!(cat(vec![ft(Value::Null)]), None);
        assert_eq!(cat(vec![]), None);
    }

    #[test]
    fn value_lists_match_recordings() {
        let tok = |v: Value| debug(tokens::token(&[v]));
        assert_eq!(tok(arr(&["b", "a", "b"])).unwrap(), "a, b");
        assert_eq!(tok(arr(&["a", "b c"])).unwrap(), "a, \"b c\"");
        assert_eq!(
            tok(Value::Bytes(Arc::from(&b"\x00\x01ab\xff"[..]))).unwrap(),
            "\\x00\\x01\\x61\\x62?"
        );
        assert_eq!(tok(s("")).unwrap(), "");
        assert_eq!(tok(Value::Null), None);

        assert_eq!(
            debug(tokens::tokenize_bool(&[Value::Bool(true)])).unwrap(),
            "y"
        );
        assert_eq!(debug(tokens::tokenize_bool(&[Value::Null])), None);

        let num =
            |args: &[(usize, Value)]| debug(tokens::tokenize_number(&named(args, 8), false, false));
        assert_eq!(
            num(&[
                (0, Value::Int64(-3)),
                (3, Value::Int64(-10)),
                (4, Value::Int64(10))
            ])
            .unwrap(),
            "==-3, [-10, -3], [-10, 5], [-6, -3], [-4, -3], [-3, -3]"
        );
        assert_eq!(
            num(&[
                (0, Value::Int64(42)),
                (3, Value::Int64(0)),
                (4, Value::Int64(1000)),
                (5, Value::Int64(10))
            ])
            .unwrap(),
            "==42, [0, 74], [0, 154], [0, 314], [0, 634], [35, 44], [35, 54], [35, 74]"
        );
        let eq = |v: Value| num(&[(0, v), (1, s("equality"))]).unwrap();
        assert_eq!(
            eq(Value::Array(
                vec![
                    Value::Int64(3),
                    Value::Int64(1),
                    Value::Null,
                    Value::Int64(3)
                ]
                .into()
            )),
            "==1, ==3"
        );
        let unimpl = debug_tokenlist(&[tokens::tokenize_number(
            &named(&[(0, Value::Int64(1)), (2, s("prefixtree"))], 8),
            false,
            false,
        )
        .unwrap()]);
        assert!(unimpl.is_err());

        let js = |j: &str| debug(tokens::tokenize_json(&[Value::Json(Arc::from(j))])).unwrap();
        assert_eq!(
            js(r#"{"a":{"b":"x y"},"c":[1,2]}"#),
            "\\x0c, \\x0c\\x01\\x61, \\x0c\\x01\\x61\\x0c, \\x0c\\x01\\x61\\x0c\\x01\\x62, \"\\x0c\\x01\\x61\\x0c\\x01\\x62\\rx y\", \\x0c\\x01\\x63, \\x0c\\x01\\x63\\x10, \\x0c\\x01\\x63\\x10\\x12, \\x0c\\x01\\x63\\x10\\\""
        );
        assert_eq!(
            js("[0,16,1000,-1,1.5]"),
            "\\x10, \\x10\\x02, \\x10\\n\\x10, \\x10*?\\x03, \\x10\\x1a\\x01, \\x10\\x14\\x00\\x00\\x00\\x00\\x00\\x00??"
        );
    }

    #[test]
    fn escapes_and_quotes_like_spanner() {
        assert_eq!(fmt_token(b"a\"b"), "a\\\"b");
        assert_eq!(fmt_token(b"(x)"), "\"(x)\"");
        assert_eq!(fmt_token(b"\x00\x01ab\xff"), "\\x00\\x01\\x61\\x62?");
        assert_eq!(fmt_token(b"\x0c\x01k\ra,b"), "\"\\x0c\\x01k\\ra,b\"");
        assert_eq!(fmt_token("Ærø".as_bytes()), "Ærø");
        assert_eq!(fmt_token(b"\x10J\xa0\x86\x01"), "\\x10J??\\x01");
    }

    #[test]
    fn log_tree_matches_recordings() {
        let r = log_tree(2, 0, 100, 1, 42);
        assert_eq!(
            r,
            vec![
                (0, 63),
                (32, 47),
                (32, 63),
                (40, 43),
                (40, 47),
                (42, 42),
                (42, 43)
            ]
        );
        let r = log_tree(2, -100, 100, 5, -7);
        assert_eq!(
            r,
            vec![
                (-100, 57),
                (-22, -3),
                (-22, 17),
                (-22, 57),
                (-12, -3),
                (-7, -3)
            ]
        );
        assert_eq!(
            log_tree(2, i64::MIN as i128, i64::MAX as i128, 1, 0).len(),
            64
        );
        assert_eq!(
            log_tree(10, i64::MIN as i128, i64::MAX as i128, 1, 42).len(),
            20
        );
    }
}
