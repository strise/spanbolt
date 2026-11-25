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

/// Longest run of single letters Cloud Spanner joins into one token; a longer run is cut into
/// tokens of this many letters.
const MAX_JOINED_LETTERS: usize = 21;

/// Which characters of `chars` Cloud Spanner's fulltext tokenizer drops to join runs of single
/// ASCII letters into one token. Fitted to DEBUG_TOKENLIST recordings
/// (`conformance/cases/sql/search_letters.case`); the emulator splits them.
/// - Letters separated by one `.` or one `-`, the same separator throughout, join whatever their
///   case: `U.S.A.` is `usa` (a `.` run also drops the period after its last letter), `x-y-zz` is
///   `xy, zz`.
/// - Then letters separated only by spaces (not tabs or newlines) and of the same case join: `x y z`
///   is `xyz`, `I J` is `ij`, but `x I` stays `x, i`. A letter right before a `.` is an initial and
///   joins nothing (`x y.` is `x, y`), nor does a letter already in a `.`/`-` run (`x y-z` is
///   `x, yz`).
///
/// A token is a run of characters between delimiters (`is_delimiter`), so `é` or `x1` is no single
/// letter.
pub fn joined_letters(chars: &[char]) -> Vec<bool> {
    joined_letters_across(chars, |gap| gap.iter().all(|&c| c == ' '))
}

/// The letters SNIPPET shows joined: as `joined_letters`, but same-case letters also join across
/// any run of ASCII whitespace that starts with a space (`x \n y` is `xy`, `x\n y` stays
/// `x y`). Recorded from Cloud Spanner (`snippet_display.case`).
pub fn snippet_joined_letters(chars: &[char]) -> Vec<bool> {
    joined_letters_across(chars, |gap| {
        gap.first() == Some(&' ') && gap.iter().all(char::is_ascii_whitespace)
    })
}

/// `joined_letters`, with `spaces` deciding which gaps between single letters are spaces.
fn joined_letters_across(chars: &[char], spaces: impl Fn(&[char]) -> bool) -> Vec<bool> {
    let delim = |c: char| c.is_ascii() && is_delimiter(c as u8);
    let mut toks: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    for (i, &c) in chars.iter().enumerate() {
        match (delim(c), start) {
            (true, Some(s)) => {
                toks.push((s, i));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        toks.push((s, chars.len()));
    }
    let n = toks.len();
    let mut drop = vec![false; chars.len()];
    let mut used = vec![false; n];
    let single = |i: usize| toks[i].1 - toks[i].0 == 1 && chars[toks[i].0].is_ascii_alphabetic();
    let gap = |i: usize| &chars[toks[i].1..toks[i + 1].0];
    let after = |i: usize| chars.get(toks[i].1).copied();
    // Joins tokens `a..=b`, in chunks of MAX_JOINED_LETTERS.
    let join = |a: usize, b: usize, used: &mut [bool], drop: &mut [bool]| {
        for k in a..=b {
            used[k] = true;
            if k < b && (k - a) % MAX_JOINED_LETTERS != MAX_JOINED_LETTERS - 1 {
                drop[toks[k].1..toks[k + 1].0].fill(true);
            }
        }
    };
    let mut i = 0;
    while i < n {
        let sep = match (i + 1 < n).then(|| gap(i)) {
            Some(&[c @ ('.' | '-')]) if single(i) && single(i + 1) => c,
            _ => {
                i += 1;
                continue;
            }
        };
        let mut j = i + 1;
        while j + 1 < n && single(j + 1) && gap(j) == [sep] {
            j += 1;
        }
        join(i, j, &mut used, &mut drop);
        if sep == '.' && after(j) == Some('.') {
            drop[toks[j].1] = true;
        }
        i = j + 1;
    }
    let eligible = |i: usize, used: &[bool]| single(i) && !used[i] && after(i) != Some('.');
    let upper = |i: usize| chars[toks[i].0].is_ascii_uppercase();
    let mut i = 0;
    while i < n {
        if !eligible(i, &used) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j + 1 < n && eligible(j + 1, &used) && upper(j) == upper(j + 1) && spaces(gap(j)) {
            j += 1;
        }
        if j > i {
            join(i, j, &mut used, &mut drop);
        }
        i = j + 1;
    }
    drop
}

/// `s` with runs of single letters joined as Cloud Spanner's fulltext tokenizer joins them
/// (`joined_letters`).
pub fn join_letters(s: &str) -> std::borrow::Cow<'_, str> {
    let chars: Vec<char> = s.chars().collect();
    let drop = joined_letters(&chars);
    if !drop.contains(&true) {
        return s.into();
    }
    chars
        .iter()
        .zip(drop)
        .filter(|(_, d)| !d)
        .map(|(c, _)| c)
        .collect::<String>()
        .into()
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

/// Prefix of a token Cloud Spanner stores at the same position as the token before it
/// (`foo@bar` is `[foo, foo@bar], bar`). No token starts with it: control characters split tokens.
pub const ALT: u8 = 0x02;

/// Runs of ASCII symbols Cloud Spanner's fulltext tokenizer keeps as tokens (recorded in
/// `conformance/cases/sql/search_tokenizer_*.case`); it drops every other run.
pub const SYMBOL_TOKENS: &[&str] = &[
    "--", "---", "-->", "<--", "<-", "->", "=>", "<=", "</", "/>", "<>", "</>", "<</", "&&", "&&&",
    "&-", "-&", "&>", "&<", "@@", "||", "::", "++", "##",
];

/// Non-ASCII symbols Cloud Spanner keeps as tokens of their own (`foo€bar` is `foo, €, bar`):
/// currency, letterlike, arrows, math and other symbols, and emoji.
fn symbol_token(c: char) -> bool {
    !c.is_alphanumeric()
        && matches!(c,
            '\u{20a0}'..='\u{20cf}' | '\u{2100}'..='\u{214f}' | '\u{2190}'..='\u{2bff}'
            | '\u{1f000}'..='\u{1faff}')
}

/// Non-ASCII characters that split tokens in Cloud Spanner: spaces, format characters, Latin-1
/// symbols and punctuation (`©`, `±`, `·`), general and CJK punctuation. Letters, digits and
/// marks stay in the token (`½`, `x́`).
fn unicode_break(c: char) -> bool {
    c.is_whitespace()
        || (matches!(c, '\u{80}'..='\u{bf}' | '\u{d7}' | '\u{f7}') && !c.is_alphanumeric())
        || matches!(c,
            '\u{2000}'..='\u{206f}' | '\u{3000}'..='\u{303f}' | '\u{fe10}'..='\u{fe1f}'
            | '\u{fe30}'..='\u{fe4f}' | '\u{feff}' | '\u{ff01}'..='\u{ff0f}'
            | '\u{ff1a}'..='\u{ff20}' | '\u{ff3b}'..='\u{ff40}' | '\u{ff5b}'..='\u{ff65}')
}

/// Whether `fulltext_tokens` keeps `c` in the run of words and ASCII symbols it is reading, so
/// that a run of these characters is tokenized on its own (anything else ends the run).
pub fn fulltext_chunk_char(c: char) -> bool {
    if matches!(c, '\u{ad}' | '\u{2019}') {
        return true;
    }
    let word = (c == '_' || c.is_ascii_alphanumeric() || (!c.is_ascii() && !unicode_break(c)))
        && !symbol_token(c);
    word || (c.is_ascii_punctuation() && c != '_')
}

enum Part {
    /// A word and the token prod stores at its position too.
    Word(String, Option<String>),
    Sym(String),
}

/// Cloud Spanner's fulltext tokens of lower-cased `s`; `true` marks a token at the same position
/// as the one before it.
/// - `_` and a lone `&` are part of a token (`foo_bar`, `at&t`); a token with `_` at an end is
///   also stored without it (`_foo` is `[foo, _foo]`);
/// - other ASCII symbols split tokens, and a run of them is a token only if it is in
///   `SYMBOL_TOKENS` (`foo -- bar` is `foo, --, bar`);
/// - `3.14`, `foo+`, `c++` and `c#` stay whole, `1,000` is `1000`;
/// - `foo@bar.com` is `[foo, foo@bar.com], bar, com`; `#tag`, `@foo` and `$5` are also stored with
///   the symbol, `foo's` (and `foo’s`) as `[foo, foo's]`;
/// - Unicode spaces, format characters and punctuation split tokens (`unicode_break`), a soft
///   hyphen is dropped, symbols such as `€`, `→` and emoji are tokens (`symbol_token`).
///
/// Not followed: CJK word segmentation (`日本語` is `日本, 語` in prod) and Unicode
/// normalisation (`ﬁle`, `İstanbul`).
pub fn fulltext_tokens(s: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut chunk: Vec<Part> = Vec::new();
    for c in s.chars() {
        let c = if c == '\u{2019}' { '\'' } else { c };
        if c == '\u{ad}' {
            continue;
        }
        let word = c == '_' || c.is_ascii_alphanumeric() || (!c.is_ascii() && !unicode_break(c));
        let word = word && !symbol_token(c);
        let sym = c.is_ascii_punctuation() && c != '_';
        if word || sym {
            match chunk.last_mut() {
                Some(Part::Word(w, _)) if word => w.push(c),
                Some(Part::Sym(w)) if sym => w.push(c),
                _ if word => chunk.push(Part::Word(c.to_string(), None)),
                _ => chunk.push(Part::Sym(c.to_string())),
            }
            continue;
        }
        chunk_tokens(std::mem::take(&mut chunk), &mut out);
        if symbol_token(c) {
            out.push((c.to_string(), false));
        }
    }
    chunk_tokens(chunk, &mut out);
    out
}

/// The tokens of a run of words and ASCII symbols with no space in it (`fulltext_tokens`).
fn chunk_tokens(mut parts: Vec<Part>, out: &mut Vec<(String, bool)>) {
    use Part::{Sym, Word};
    let digits = |w: &str| !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit());
    // `3.14`, `1,000`, then a lone `&` joining the words around it.
    let mut i = 0;
    while i + 2 < parts.len() {
        let joined = match (&parts[i], &parts[i + 1], &parts[i + 2]) {
            (Word(a, _), Sym(s), Word(b, _))
                if digits(a) && digits(b) && (s == "." || (s == "," && b.len() == 3)) =>
            {
                Some(format!("{a}{}{b}", if s == "." { "." } else { "" }))
            }
            (Word(a, _), Sym(s), Word(b, _)) if s == "&" => Some(format!("{a}&{b}")),
            _ => None,
        };
        match joined {
            Some(w) => drop(parts.splice(i..i + 3, [Word(w, None)])),
            None => i += 1,
        }
    }
    let n = parts.len();
    for k in 0..n {
        let (prev, rest) = parts.split_at_mut(k);
        let Sym(s) = &rest[0] else { continue };
        if s != "&" {
            continue;
        }
        let before = matches!(prev.last(), Some(Word(..)));
        let after = matches!(rest.get(1), Some(Word(..)));
        match (before, after) {
            (true, _) => {
                if let Some(Word(w, _)) = prev.last_mut() {
                    w.push('&');
                }
                rest[0] = Sym(String::new());
            }
            (false, true) => {
                if let Word(w, _) = &mut rest[1] {
                    w.insert(0, '&');
                }
                rest[0] = Sym(String::new());
            }
            (false, false) => rest[0] = Word("&".into(), None),
        }
    }
    parts.retain(|p| !matches!(p, Sym(s) if s.is_empty()));
    // `foo+`, `c++`, `c#` at the end.
    if let [.., Word(w, _), Sym(s)] = parts.as_mut_slice() {
        let letter = w.len() == 1 && w.as_bytes()[0].is_ascii_alphabetic();
        if s == "+" || (s == "++" && w == "c") || (s == "#" && letter) {
            w.push_str(s);
            parts.pop();
        }
    }
    // `foo's`.
    let mut k = 0;
    while k + 2 < parts.len() {
        if let (Word(..), Sym(s), Word(t, None)) = (&parts[k], &parts[k + 1], &parts[k + 2])
            && s == "'"
            && t == "s"
            && !matches!(parts.get(k + 3), Some(Word(..)))
        {
            parts.drain(k + 1..k + 3);
            if let Word(w, alt) = &mut parts[k] {
                *alt = Some(format!("{w}'s"));
            }
        }
        k += 1;
    }
    // `#tag`, `@foo`, `$5` at the start.
    if let [Sym(s), Word(w, alt @ None), ..] = parts.as_mut_slice()
        && matches!(s.as_str(), "#" | "@" | "$")
    {
        *alt = Some(format!("{s}{w}"));
        parts.remove(0);
    }
    // An address: one `@` between two words; the first word also stores the whole address.
    let ats: Vec<usize> = (0..parts.len())
        .filter(|&k| matches!(&parts[k], Sym(s) if s.contains('@')))
        .collect();
    if let [at] = ats[..]
        && matches!(&parts[at], Sym(s) if s == "@")
        && at > 0
        && matches!(parts[at - 1], Word(..))
        && matches!(parts.get(at + 1), Some(Word(..)))
    {
        let first = parts
            .iter()
            .position(|p| matches!(p, Word(..)))
            .unwrap_or(0);
        let last = parts
            .iter()
            .rposition(|p| matches!(p, Word(..)))
            .unwrap_or(first);
        let addr: String = parts[first..=last]
            .iter()
            .map(|p| match p {
                Word(w, _) | Sym(w) => w.as_str(),
            })
            .collect();
        if let Word(_, alt) = &mut parts[first] {
            *alt = Some(addr);
        }
    }
    for p in parts {
        match p {
            Word(w, alt) => {
                let core = w.trim_matches('_');
                let underscored = !core.is_empty() && core.len() < w.len();
                if underscored {
                    out.push((core.to_string(), false));
                    out.push((w, true));
                } else {
                    out.push((w, false));
                }
                if let Some(a) = alt {
                    out.push((a, true));
                }
            }
            Sym(s) if SYMBOL_TOKENS.contains(&s.as_str()) => out.push((s, false)),
            Sym(_) => {}
        }
    }
}

/// `emulator` splits runs of single letters like the emulator; Cloud Spanner joins them
/// (`joined_letters`).
pub fn tokenize_fulltext(args: &[Value], emulator: bool) -> Result<Value> {
    let text = &args[0];
    let toks = std::cell::RefCell::new(vec![
        format!("{FULLTEXT}-{}", bool_digit(text.is_null())).into_bytes(),
    ]);
    each_text(
        text,
        |s| {
            let mut t = toks.borrow_mut();
            if emulator {
                t.extend(split(lower(s).as_bytes()).map(<[u8]>::to_vec));
            } else {
                t.extend(
                    fulltext_tokens(&lower(&join_letters(s)))
                        .into_iter()
                        .map(|(t, alt)| {
                            let mut b = Vec::with_capacity(t.len() + 1);
                            if alt {
                                b.push(ALT);
                            }
                            b.extend_from_slice(t.as_bytes());
                            b
                        }),
                );
            }
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

/// Cloud Spanner's n-grams: character n-grams of every size in `min..=max`.
pub fn char_ngrams_of(s: &str, min: i64, max: i64, out: &mut Vec<Vec<u8>>) {
    let bounds: Vec<usize> = s.char_indices().map(|(i, _)| i).chain([s.len()]).collect();
    let chars = bounds.len() - 1;
    for n in min..=max {
        let n = n as usize;
        if n > chars {
            break;
        }
        for i in 0..=chars - n {
            out.push(s.as_bytes()[bounds[i]..bounds[i + n]].to_vec());
        }
    }
}

/// `int` truncation of an INT64 argument, as the emulator reads ngram sizes.
fn as_int(v: i64) -> i64 {
    v as i32 as i64
}

/// `emulator` lower-cases the text and cuts byte n-grams like the emulator; Cloud Spanner keeps
/// its case and cuts character n-grams.
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
                char_ngrams_of(s, min, max, &mut toks.borrow_mut());
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

pub const TOKEN: &str = "exact_match";
pub const BOOL: &str = "bool";
pub const NUMBER: &str = "numeric";
pub const JSON: &str = "json";

/// Prefix of the token after the signature that keeps the value behind a TOKEN, TOKENIZE_BOOL,
/// TOKENIZE_NUMBER or TOKENIZE_JSON list, for DEBUG_TOKENLIST (`super::debug`). Absent for NULL.
/// Nothing else reads it: search functions reject these lists.
pub const VALUE: u8 = 0x03;

/// `value` starts with VALUE.
fn with_value(sig: &str, value: Option<&[u8]>) -> Value {
    match value {
        Some(v) => encode(&[sig.as_bytes(), v]),
        None => encode(&[sig.as_bytes()]),
    }
}

/// Appends a LEB128 length and the bytes.
pub fn put_len_prefixed(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

pub fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// The value is its elements' bytes, each length-prefixed (NULL elements left out).
pub fn token(args: &[Value]) -> Result<Value> {
    fn bytes(v: &Value) -> Option<&[u8]> {
        match v {
            Value::String(s) => Some(s.as_bytes()),
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
    let value = match &args[0] {
        Value::Array(a) => {
            let mut out = vec![VALUE];
            for b in a.iter().filter_map(bytes) {
                put_len_prefixed(&mut out, b);
            }
            Some(out)
        }
        v => bytes(v).map(|b| {
            let mut out = Vec::with_capacity(b.len() + 4);
            out.push(VALUE);
            put_len_prefixed(&mut out, b);
            out
        }),
    };
    Ok(with_value(TOKEN, value.as_deref()))
}

pub fn tokenize_bool(args: &[Value]) -> Result<Value> {
    let value: Option<&[u8]> = match &args[0] {
        Value::Bool(true) => Some(&[VALUE, b'y']),
        Value::Bool(false) => Some(&[VALUE, b'n']),
        _ => None,
    };
    Ok(with_value(BOOL, value))
}

/// The value is the JSON text.
pub fn tokenize_json(args: &[Value]) -> Result<Value> {
    let value = match &args[0] {
        Value::Json(s) => {
            let mut out = Vec::with_capacity(s.len() + 1);
            out.push(VALUE);
            out.extend_from_slice(s.as_bytes());
            Some(out)
        }
        _ => None,
    };
    Ok(with_value(JSON, value.as_deref()))
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
    Ok(with_value(
        NUMBER,
        number_value(args, float_value || float_bounds).as_deref(),
    ))
}

/// What DEBUG_TOKENLIST needs to print a TOKENIZE_NUMBER list, after VALUE: `E` and the values
/// (equality), `L` and base, min, max, granularity and the value (logtree over INT64), each
/// little-endian i64, or `U` for lists it can't print yet (DOUBLE values, prefixtree, arrays
/// without equality). None for NULL.
fn number_value(args: &[Value], float: bool) -> Option<Vec<u8>> {
    let v = arg(args, 0)?;
    let equality = str_arg(args, 1).is_some_and(|c| c.eq_ignore_ascii_case("equality"));
    let logtree = str_arg(args, 2)
        .is_none_or(|a| a.eq_ignore_ascii_case("auto") || a.eq_ignore_ascii_case("logtree"));
    let mut out = Vec::with_capacity(42);
    out.push(VALUE);
    match (float, equality, v) {
        (false, true, Value::Int64(i)) => {
            out.push(b'E');
            out.extend_from_slice(&i.to_le_bytes());
        }
        (false, true, Value::Array(a)) => {
            out.push(b'E');
            for e in a.iter() {
                if let Value::Int64(i) = e {
                    out.extend_from_slice(&i.to_le_bytes());
                }
            }
        }
        (false, false, Value::Int64(i)) if logtree => {
            out.push(b'L');
            for n in [
                int_arg(args, 6, 2),
                int_arg(args, 3, i64::MIN),
                int_arg(args, 4, i64::MAX),
                int_arg(args, 5, 1),
                *i,
            ] {
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
        _ => out.push(b'U'),
    }
    Some(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Prod's fulltext tokens, from DEBUG_TOKENLIST recordings.
    fn prod_tokens(s: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (t, alt) in fulltext_tokens(&lower(&join_letters(s))) {
            match out.last_mut() {
                Some(last) if alt => {
                    let inner = last
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .to_string();
                    *last = format!("[{inner}, {t}]");
                }
                _ => out.push(t),
            }
        }
        out
    }

    /// DEBUG_TOKENLIST recordings from `search_tokenizer_symbols.case`. Tokens at one position
    /// are in brackets (`[foo, foo@bar]`), the primary token first.
    #[test]
    fn tokenizes_symbols_like_prod() {
        for (text, want) in [
            ("foo_bar", "foo_bar"),
            ("foo__bar", "foo__bar"),
            ("a_b_c", "a_b_c"),
            ("1_2", "1_2"),
            ("foo _ bar", "foo _ bar"),
            ("é_f", "é_f"),
            ("FOO_BAR", "foo_bar"),
            ("_foo", "[foo, _foo]"),
            ("__init__", "[init, __init__]"),
            ("AT&T", "at&t"),
            ("foo & bar", "foo & bar"),
            ("&foo", "&foo"),
            ("foo&", "foo&"),
            ("a&&b", "a && b"),
            ("x&y&z", "x&y&z"),
            ("foo&amp;bar", "foo&amp bar"),
            ("foo&bar_baz", "foo&bar_baz"),
            ("foo&-bar", "foo &- bar"),
            ("foo-&bar", "foo -& bar"),
            ("foo@", "foo"),
            ("a@b@c", "a b c"),
            ("foo @ bar", "foo bar"),
            ("foo@@bar", "foo @@ bar"),
            ("--", "--"),
            ("foo--bar", "foo -- bar"),
            ("foo---bar", "foo --- bar"),
            ("----", ""),
            ("-", ""),
            ("--foo", "-- foo"),
            ("x--y--z", "x -- y -- z"),
            ("</", "</"),
            ("<", ""),
            ("<foo", "foo"),
            ("foo>", "foo"),
            ("<!--", ""),
            ("-->", "-->"),
            ("<br/>", "br />"),
            ("x<y", "x y"),
            ("<>", "<>"),
            ("</>", "</>"),
            ("<//", ""),
            ("<p>hello</p>", "p hello </ p"),
            ("x<</y", "x <</ y"),
            ("foo\u{a0}bar", "foo bar"),
            ("foo\u{3000}\u{3000}bar", "foo bar"),
            ("foo\u{200b}bar", "foo bar"),
            ("foo\u{85}bar", "foo bar"),
            ("foo\u{b}bar", "foo bar"),
            ("foo\u{2060}bar", "foo bar"),
            ("foo\u{feff}bar", "foo bar"),
            ("x\u{a0}y\u{a0}z", "x y z"),
            ("foo—bar", "foo bar"),
            ("foo\u{ad}bar", "foobar"),
            ("foo、bar", "foo bar"),
            ("foo，bar", "foo bar"),
            ("foo·bar baz", "foo bar baz"),
            ("foo“bar”", "foo bar"),
            ("foo…bar", "foo bar"),
            ("foo€bar", "foo € bar"),
            ("foo©bar", "foo bar"),
            ("foo™bar", "foo ™ bar"),
            ("foo±bar", "foo bar"),
            ("foo½bar", "foo½bar"),
            ("c++", "c++"),
            ("c#", "c#"),
            ("node.js", "node js"),
            ("3.14", "3.14"),
            ("1,000", "1000"),
            // Prod stores the address as `a@b.c`; single letters are joined first here.
            ("a@b.c", "[a, a@bc] bc"),
        ] {
            assert_eq!(prod_tokens(text).join(" "), want, "{text:?}");
        }
    }

    #[test]
    fn joins_single_letters_like_prod() {
        for (text, want) in [
            ("x y z w", "xyzw"),
            ("hello b c world", "hello bc world"),
            ("x yz w", "x yz w"),
            ("x, y z", "x yz"),
            ("x-y z", "xy z"),
            ("x y-z", "x yz"),
            ("1 2 3", "1 2 3"),
            ("X Y", "xy"),
            ("x Y z", "x y z"),
            ("x I y", "x i y"),
            ("Z x y", "z xy"),
            ("é f g", "é fg"),
            ("ß x y", "ß xy"),
            ("x  y\nz", "xy z"),
            ("x\ty", "x y"),
            ("b. c", "b c"),
            ("x y.", "x y"),
            ("x y, z", "xy z"),
            ("(x y)", "xy"),
            ("U.S.A. today", "usa today"),
            ("U.S. A", "us a"),
            ("x.Y", "xy"),
            ("x-y-zz", "xy zz"),
            ("xx-y", "xx y"),
            ("x.y-z", "xy z"),
            ("x.-y", "x y"),
            ("x . y z", "x yz"),
            ("1x y z", "1x yz"),
            (
                "a b c d e f g h i j k l m n o p q r s t u v w x y z",
                "abcdefghijklmnopqrstu vwxyz",
            ),
            (
                "a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r.s.t.u.v.w.x.y.z",
                "abcdefghijklmnopqrstu vwxyz",
            ),
        ] {
            assert_eq!(prod_tokens(text).join(" "), want, "{text:?}");
        }
    }

    #[test]
    fn joins_query_text() {
        assert_eq!(join_letters("-x y z"), "-xyz");
        assert_eq!(join_letters("x -y z"), "x -yz");
        assert_eq!(join_letters("x* y"), "x* y");
        assert_eq!(join_letters("x y*"), "xy*");
        assert_eq!(join_letters("\"x y\" OR z"), "\"xy\" OR z");
        assert_eq!(join_letters("no letters here"), "no letters here");
    }
}
