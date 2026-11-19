//! TOKENLIST values and the tokenizers (TOKENIZE_*, TOKEN, TOKENLIST_CONCAT).
//!
//! Ported from cloud-spanner-emulator `backend/query/search/*_tokenizer.cc` and
//! `tokenlist_concat.cc` (Apache-2.0). Like the emulator, a TOKENLIST is a list of byte strings
//! whose first entry is a signature naming the tokenizer and its options (`fulltext-0`,
//! `substring-4-1-0-0`, `ngrams-3-3-0`, …); `\x01` marks the gap between array elements.

use std::sync::Arc;

use spanbolt_storage::{Error, Result, Value};

pub const FULLTEXT: &str = "fulltext";
pub const SUBSTRING: &str = "substring";
pub const NGRAMS: &str = "ngrams";
pub const GAP: &[u8] = b"\x01";

/// Characters that split text into tokens (`kDelimiter`).
pub fn is_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'~' | b'!'
            | b'@'
            | b'#'
            | b'$'
            | b'%'
            | b'^'
            | b'&'
            | b'*'
            | b'('
            | b')'
            | b'_'
            | b'+'
            | b'-'
            | b'='
            | b'{'
            | b'}'
            | b'['
            | b']'
            | b'|'
            | b'\\'
            | b'/'
            | b'"'
            | b'`'
            | b':'
            | b';'
            | b'\''
            | b'<'
            | b'>'
            | b','
            | b'.'
            | b'?'
            | b'\r'
            | b'\n'
            | b'\t'
            | 0x08
            | 0x0c
            | b' '
    )
}

/// `absl::StrSplit(s, ByAnyChar(kDelimiter), SkipWhitespace())`.
pub fn split(s: &[u8]) -> impl Iterator<Item = &[u8]> {
    s.split(|&b| is_delimiter(b))
        .filter(|p| !p.iter().all(|b| b.is_ascii_whitespace() || *b == 0x0b))
}

pub fn lower(s: &str) -> String {
    s.to_lowercase()
}

/// Encodes tokens as a TOKENLIST value: each token is a LEB128 length followed by its bytes.
pub fn encode<T: AsRef<[u8]>>(tokens: &[T]) -> Value {
    let mut out = Vec::with_capacity(tokens.iter().map(|t| t.as_ref().len() + 1).sum());
    for t in tokens {
        let t = t.as_ref();
        let mut n = t.len();
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
        out.extend_from_slice(t);
    }
    Value::TokenList(Arc::from(out))
}

/// Iterates the tokens of an encoded TOKENLIST without allocating.
pub struct Tokens<'a> {
    bytes: &'a [u8],
    i: usize,
}

pub fn iter(bytes: &[u8]) -> Tokens<'_> {
    Tokens { bytes, i: 0 }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Result<&'a [u8]>;

    fn next(&mut self) -> Option<Result<&'a [u8]>> {
        if self.i >= self.bytes.len() {
            return None;
        }
        let bad = || Error::internal("malformed TOKENLIST");
        let mut n = 0usize;
        let mut shift = 0;
        loop {
            let Some(&b) = self.bytes.get(self.i) else {
                self.i = self.bytes.len();
                return Some(Err(bad()));
            };
            self.i += 1;
            n |= ((b & 0x7f) as usize) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        let t = self.bytes.get(self.i..self.i + n);
        self.i += n;
        Some(t.ok_or_else(bad))
    }
}

/// Decodes a TOKENLIST value into borrowed tokens.
pub fn decode(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    iter(bytes).collect()
}

/// `IsTokenizerSignature`.
pub fn is_signature(t: &[u8]) -> bool {
    [
        "exact_match-",
        "fulltext-",
        "substring-",
        "numeric-",
        "bool-",
        "ngrams-",
        "json-",
    ]
    .iter()
    .any(|p| t.starts_with(p.as_bytes()))
}

pub fn arg(args: &[Value], i: usize) -> Option<&Value> {
    args.get(i).filter(|v| !v.is_null())
}

pub fn int_arg(args: &[Value], i: usize, default: i64) -> i64 {
    match arg(args, i) {
        Some(Value::Int64(v)) => *v,
        _ => default,
    }
}

pub fn str_arg(args: &[Value], i: usize) -> Option<&str> {
    match arg(args, i) {
        Some(Value::String(s)) => Some(s),
        _ => None,
    }
}

fn bool_digit(b: bool) -> &'static str {
    if b { "1" } else { "0" }
}

/// Applies `f` to the value, or to each array element followed by a gap.
fn each_text(
    v: &Value,
    mut f: impl FnMut(&str) -> Result<()>,
    gap: &mut dyn FnMut(),
) -> Result<()> {
    match v {
        Value::String(s) => f(s),
        Value::Array(a) => {
            for e in a.iter() {
                match e {
                    Value::String(s) => f(s)?,
                    _ => f("")?,
                }
                gap();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub fn tokenize_fulltext(args: &[Value]) -> Result<Value> {
    let text = &args[0];
    let toks = std::cell::RefCell::new(vec![
        format!("{FULLTEXT}-{}", bool_digit(text.is_null())).into_bytes(),
    ]);
    each_text(
        text,
        |s| {
            let l = lower(s);
            toks.borrow_mut()
                .extend(split(l.as_bytes()).map(<[u8]>::to_vec));
            Ok(())
        },
        &mut || toks.borrow_mut().push(GAP.to_vec()),
    )?;
    Ok(encode(&toks.into_inner()))
}

const MAX_NGRAM: i64 = 12;

fn validate_ngram_size(min: i64, max: i64) -> Result<()> {
    let msg = if max <= 0 {
        "ngram_size_max must be greater than 0.".to_string()
    } else if min <= 0 {
        "ngram_size_min must be greater than 0.".to_string()
    } else if min > max {
        "ngram_size_min cannot be greater than ngram_size_max.".to_string()
    } else if max > MAX_NGRAM {
        format!("ngram_size_max cannot be greater than {MAX_NGRAM}.")
    } else {
        return Ok(());
    };
    Err(Error::invalid_argument(msg))
}

/// `TokenizeNgrams`: byte n-grams of every size in `min..=max`.
pub fn ngrams_of(s: &[u8], min: i64, max: i64, out: &mut Vec<Vec<u8>>) {
    for n in min..=max {
        let n = n as usize;
        if n > s.len() {
            break;
        }
        for i in 0..=s.len() - n {
            out.push(s[i..i + n].to_vec());
        }
    }
}

/// `int` truncation of an INT64 argument, as the emulator reads ngram sizes.
fn as_int(v: i64) -> i64 {
    v as i32 as i64
}

/// `emulator` lower-cases the text like the emulator; Cloud Spanner keeps its case.
pub fn tokenize_ngrams(args: &[Value], emulator: bool) -> Result<Value> {
    let max = as_int(int_arg(args, 1, 4));
    let min = as_int(int_arg(args, 2, 1));
    validate_ngram_size(min, max)?;
    let text = &args[0];
    let toks = std::cell::RefCell::new(vec![
        format!("{NGRAMS}-{max}-{min}-{}", bool_digit(text.is_null())).into_bytes(),
    ]);
    each_text(
        text,
        |s| {
            if emulator {
                ngrams_of(lower(s).as_bytes(), min, max, &mut toks.borrow_mut());
            } else {
                ngrams_of(s.as_bytes(), min, max, &mut toks.borrow_mut());
            }
            Ok(())
        },
        &mut || toks.borrow_mut().push(GAP.to_vec()),
    )?;
    Ok(encode(&toks.into_inner()))
}

/// Relative search types (`RelativeSearchType` bit flags).
pub const WORD_PREFIX: i64 = 1;
pub const WORD_SUFFIX: i64 = 2;
pub const VALUE_PREFIX: i64 = 4;
pub const VALUE_SUFFIX: i64 = 8;
pub const PHRASE: i64 = 16;
const ALL: i64 = 31;

pub fn parse_relative_search_type(s: &str) -> Result<i64> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "word_prefix" => WORD_PREFIX,
        "word_suffix" => WORD_SUFFIX,
        "value_prefix" => VALUE_PREFIX,
        "value_suffix" => VALUE_SUFFIX,
        "phrase" | "adjacent_and_in_order" => PHRASE,
        _ => {
            return Err(Error::invalid_argument(format!(
                "Invalid relative_search_type: '{s}'."
            )));
        }
    })
}

pub fn tokenize_substring(args: &[Value]) -> Result<Value> {
    let max = as_int(int_arg(args, 1, 4));
    let min = as_int(int_arg(args, 2, 1));
    validate_ngram_size(min, max)?;
    let support = arg(args, 3);
    let types = arg(args, 5);
    if support.is_some() && types.is_some() {
        return Err(Error::invalid_argument(
            "TOKENIZE_SUBSTRING: only one of 'support_relative_search' and 'relative_search_types' can be specified.",
        ));
    }
    let mut supported = 0;
    if let Some(Value::Array(a)) = types {
        for t in a.iter() {
            let s = match t {
                Value::String(s) => s.as_ref(),
                _ => "",
            };
            if s.eq_ignore_ascii_case("all") {
                supported = ALL;
                break;
            }
            supported |= parse_relative_search_type(s)?;
        }
    }
    if let Some(Value::Bool(true)) = support {
        supported = ALL;
    }
    let text = &args[0];
    let toks = std::cell::RefCell::new(vec![
        format!(
            "{SUBSTRING}-{max}-{min}-{}-{supported}",
            bool_digit(text.is_null())
        )
        .into_bytes(),
    ]);
    each_text(
        text,
        |s| {
            toks.borrow_mut().push(lower(s).into_bytes());
            Ok(())
        },
        &mut || toks.borrow_mut().push(GAP.to_vec()),
    )?;
    Ok(encode(&toks.into_inner()))
}

pub fn token(_args: &[Value]) -> Result<Value> {
    Ok(encode(&[b"exact_match"]))
}

pub fn tokenize_bool(_args: &[Value]) -> Result<Value> {
    Ok(encode(&[b"bool"]))
}

pub fn tokenize_json(_args: &[Value]) -> Result<Value> {
    Ok(encode(&[b"json"]))
}

/// `std::to_string(double)`.
fn fmt_double(v: f64) -> String {
    format!("{v:.6}")
}

pub fn tokenize_number(args: &[Value], float_bounds: bool, float_value: bool) -> Result<Value> {
    const FN: &str = "TOKENIZE_NUMBER";
    if let Some(ct) = str_arg(args, 1)
        && !["all", "range", "equality"]
            .iter()
            .any(|c| c.eq_ignore_ascii_case(ct))
    {
        return Err(Error::invalid_argument(format!(
            "{FN}: Unsupported comparison type: '{ct}', supported comparison types: 'all', 'range', 'equality' (equality is only allowed for integrals)"
        )));
    }
    let precision = int_arg(args, 7, 15);
    if !(1..=15).contains(&precision) {
        return Err(Error::invalid_argument(format!(
            "{FN}: precision must be in the range [1, 15], got: {precision}"
        )));
    }
    let algorithm = str_arg(args, 2).unwrap_or("auto");
    if !["auto", "logtree", "prefixtree", "floatingpoint"]
        .iter()
        .any(|a| a.eq_ignore_ascii_case(algorithm))
    {
        return Err(Error::invalid_argument(format!(
            "{FN}: Algorithm is not supported: '{algorithm}'; supported algorithms are: 'auto', 'logtree', 'prefixtree', 'floatingpoint' (floatingpoint is supported only for floating point types)"
        )));
    }
    if algorithm.eq_ignore_ascii_case("floatingpoint") {
        if !float_value {
            return Err(Error::invalid_argument(
                "TOKENIZE_NUMBER: algorithm 'floatingpoint' can only be used to tokenize floating point values",
            ));
        }
    } else {
        let tree_base = int_arg(args, 6, 2);
        if !(2..=10).contains(&tree_base) {
            return Err(Error::invalid_argument(format!(
                "{FN}: tree_base must be in the range [2, 10], got: {tree_base}"
            )));
        }
        if float_bounds {
            let get = |i: usize, d: f64| match arg(args, i) {
                Some(Value::Float64(v)) => *v,
                _ => d,
            };
            let (min, max, g) = (get(3, f64::MIN), get(4, f64::MAX), get(5, 1.0));
            validate_bounds(
                min.is_finite(),
                max.is_finite(),
                min >= max,
                g <= 0.0 || !g.is_finite(),
                min + g > max,
                (fmt_double(min), fmt_double(max), fmt_double(g)),
            )?;
        } else {
            let (min, max, g) = (
                int_arg(args, 3, i64::MIN),
                int_arg(args, 4, i64::MAX),
                int_arg(args, 5, 1),
            );
            validate_bounds(
                true,
                true,
                min >= max,
                g <= 0,
                (min as f64) + (g as f64) > max as f64,
                (min.to_string(), max.to_string(), g.to_string()),
            )?;
        }
    }
    Ok(encode(&[b"numeric"]))
}

fn validate_bounds(
    min_finite: bool,
    max_finite: bool,
    min_ge_max: bool,
    bad_granularity: bool,
    too_wide: bool,
    (min, max, g): (String, String, String),
) -> Result<()> {
    let msg = if !min_finite {
        format!("TOKENIZE_NUMBER: min must be finite, got: {min}")
    } else if !max_finite {
        format!("TOKENIZE_NUMBER: max must be finite, got: {max}")
    } else if min_ge_max {
        format!("TOKENIZE_NUMBER: min must be less than max, got: {min} and {max}")
    } else if bad_granularity {
        format!("TOKENIZE_NUMBER: granularity must be finite and positive, got: {g}")
    } else if too_wide {
        format!(
            "TOKENIZE_NUMBER: granularity ({g}) must be less than or equal to the difference between min and max ({min}, {max})"
        )
    } else {
        return Ok(());
    };
    Err(Error::invalid_argument(msg))
}

pub fn tokenlist_concat(args: &[Value]) -> Result<Value> {
    let Value::Array(a) = &args[0] else {
        return Ok(Value::Null);
    };
    let mut out: Vec<&[u8]> = Vec::new();
    let mut first: Option<Vec<&[u8]>> = None;
    for e in a.iter() {
        let Value::TokenList(b) = e else { continue };
        let toks = decode(b)?;
        let sig: Vec<&[u8]> = toks
            .first()
            .map(|t| t.split(|&c| c == b'-').collect())
            .unwrap_or_else(|| vec![&b""[..]]);
        match &first {
            None => first = Some(sig),
            Some(f) => {
                let conflict = sig[0] != f[0]
                    || ((sig[0] == NGRAMS.as_bytes() || sig[0] == SUBSTRING.as_bytes())
                        && (sig.get(1) != f.get(1) || sig.get(2) != f.get(2)));
                if conflict {
                    return Err(Error::invalid_argument(
                        "All elements to TOKENLIST_CONCAT must be produced by the same kind of tokenization function.",
                    ));
                }
            }
        }
        out.extend(toks);
        out.push(GAP);
    }
    Ok(encode(&out))
}
