//! Full-text search: tokenizers for TOKENLIST columns and the search functions, ported from the
//! official emulator (see `tokens`, `rquery` and `eval`).
//!
//! By default search follows Cloud Spanner (recorded in `conformance/spanner-vs-emulator-summary.md`);
//! `--emulator-compat` keeps the emulator's behaviour. Prod differs in:
//! - the rquery parser (`rquery`): Unicode letters in terms, runs of separators, parentheses,
//!   lenient `AROUND` (a bare `AROUND` is `AROUND(5)`, `"a AROUND b"` keeps the order), lower-case
//!   `or` ignored, connectors that join nothing ignored, NOT inside AROUND rejected, and double
//!   negations and negated groups matching nothing;
//! - stopwords in a query with other terms match every document (`score::drop_stopwords`), and
//!   a top-level AND of negations that had a stopword dropped also matches NULL documents;
//! - placement (`plan.rs`, `SearchRules`): SEARCH, SEARCH_SUBSTRING and SEARCH_NGRAMS only as
//!   predicates, SCORE only with a SEARCH over the same column, and every search function only
//!   through a search index whose PARTITION BY columns the WHERE clause fixes and whose
//!   `WHERE ... IS NOT NULL` filter it implies;
//! - SNIPPET's JSON (`eval::snippet`), SEARCH_SUBSTRING with a relative search type over several
//!   words, and some error messages.
//!
//! - SCORE values, fitted to prod recordings (`score`), and stopwords dropped from SEARCH and SCORE
//!   queries;
//! - SCORE_NGRAMS values (padded trigram Jaccard, `eval::score_ngrams_prod`);
//! - TOKENIZE_FULLTEXT joins runs of single letters (`x y z` and `U.S.A.` are one token each),
//!   and SEARCH, SCORE and SNIPPET read queries the same way (`tokens::joined_letters`);
//! - SNIPPET counts `max_snippet_width` in proportional character widths, cuts and splits
//!   snippets as prod does, and ignores stopwords in a query with other words (`eval::Snippets`);
//! - TOKENIZE_FULLTEXT and TOKENIZE_SUBSTRING, and SEARCH, SCORE and SEARCH_SUBSTRING queries,
//!   normalise text (compatibility forms of letters, full case folding, NFC: `fold`) where the
//!   emulator only lower-cases it;
//! - the tokenizers take `remove_diacritics`, and SEARCH and SCORE over a TOKENIZE_FULLTEXT list
//!   made with it strip diacritics from the query too (`tokens::removes_diacritics`);
//! - TOKENIZE_NGRAMS and SEARCH_NGRAMS over it keep case (the emulator lower-cases both), and
//!   n-grams are cut from characters, not UTF-8 bytes (`tokens::char_ngrams_of`).
//!
//! DEBUG_TOKENLIST, which only prod has, prints token lists in prod's format (`debug`); the
//! analyser declares it in default mode only.
//!
//! Not followed: CJK word segmentation (`日本語` is `日本, 語` in prod, recorded in
//! `search_tokenizer_cjk.case`; it is also the one row `search_tokenizer_more.case`'s `tokens`
//! fails on in default mode). Prod's segmenter is Google's own; on that corpus the ICU4X
//! dictionary segmenter matches 86 of 137 texts and lindera (IPADIC, CC-CEDICT, ko-dic) 96 for
//! seven times the binary size, so neither is worth a dependency. Not followed yet: SCORE with
//! `enhance_query => true`; SNIPPET widths of scripts Arial doesn't cover (Georgian, Armenian,
//! Indic, Thai) and of texts with no letters or digits.

mod debug;
mod eval;
mod fold;
mod index;
mod rquery;
mod score;
mod snippet_widths;
mod tokens;

pub use index::search_keys;

/// Entries in the inverted-index cache and the prepared search-query cache.
pub(crate) fn cache_sizes() -> (usize, usize) {
    (index::cache_len(), eval::cache_len())
}

use crate::func::{ScalarFn, Sig, f};
use crate::types::Type;

/// Functions that need `allow_search_indexes_in_transaction` in a read-write transaction and are
/// rejected in partitioned queries (the emulator's `CheckSearchFunctionsAreAllowed`).
pub fn is_search_function(name: &str) -> bool {
    matches!(name, "search" | "search_substring" | "score" | "snippet")
}

/// Rewords an error's message as Cloud Spanner does, unless in `--emulator-compat`.
fn prod_message(
    emulator: bool,
    mut e: spanbolt_storage::Error,
    f: impl Fn(&str) -> String,
) -> spanbolt_storage::Error {
    if !emulator {
        e.message = f(&e.message);
    }
    e
}

fn prod_tokenize_number(m: &str) -> String {
    if let Some(rest) = m.strip_prefix("TOKENIZE_NUMBER: Algorithm is not supported: ")
        && let Some((algorithm, _)) = rest.split_once(';')
    {
        return format!(
            "unsupported algorithm {algorithm}; supported algorithms are 'auto', 'logtree', 'prefixtree', or 'floatingpoint' (floatingpoint is supported only for floating point types)"
        );
    }
    if let Some(rest) = m.strip_prefix("TOKENIZE_NUMBER: algorithm 'floatingpoint'") {
        return format!("algorithm 'floatingpoint'{rest}");
    }
    // The bounds errors (`min must be less than max, got: 10 and 1`).
    if let Some(rest) = m.strip_prefix("TOKENIZE_NUMBER: ")
        && ["min ", "max ", "granularity "]
            .iter()
            .any(|p| rest.starts_with(p))
    {
        return rest.to_string();
    }
    m.to_string()
}

/// The error Cloud Spanner raises about a tokenizer call whose options are literals, before any
/// check of where SEARCH is used (no position since the 2026-W48 recording): TOKENIZE_NUMBER's INT64
/// bounds and TOKENIZE_NGRAMS's `ngram_size_min` above `ngram_size_max`
/// (`search_functions.case`). Spanner checks the other options when the function runs, without a
/// position, so those stay in the functions. `args` holds each argument's value if it is a
/// literal, `types` their types.
pub fn analysis_error(
    name: &str,
    args: &[Option<spanbolt_storage::Value>],
    types: &[Type],
) -> Option<spanbolt_storage::Error> {
    use spanbolt_storage::Value;
    // Literal arguments, NULL for the value and for absent options; None if an option is not
    // a literal.
    let options = |n: usize| -> Option<Vec<Value>> {
        let mut out = vec![Value::Null];
        for i in 1..=n {
            out.push(match args.get(i) {
                None => Value::Null,
                Some(v) => v.clone()?,
            });
        }
        Some(out)
    };
    match name {
        "tokenize_number" => {
            let float = |t: &Type| match t {
                Type::Float64 => true,
                Type::Array(e) => matches!(**e, Type::Float64),
                _ => false,
            };
            if types.iter().any(float) {
                return None;
            }
            let a = options(5)?;
            // Spanner rejects these comparison types and algorithms before the bounds.
            let all = tokens::str_arg(&a, 1).is_none_or(|c| c.eq_ignore_ascii_case("all"));
            let tree = tokens::str_arg(&a, 2).is_none_or(|al| {
                ["auto", "logtree", "prefixtree"]
                    .iter()
                    .any(|x| x.eq_ignore_ascii_case(al))
            });
            if !all || !tree {
                return None;
            }
            tokens::int_bounds(&a)
                .err()
                .map(|e| prod_message(false, e, prod_tokenize_number))
        }
        "tokenize_ngrams" => {
            let a = options(2)?;
            let (max, min) = (tokens::int_arg(&a, 1, 4), tokens::int_arg(&a, 2, 1));
            (min > 0 && max > 0 && min > max).then(|| {
                spanbolt_storage::Error::invalid_argument(
                    "Invalid use of TOKENIZE_NGRAMS: ngram_size_min cannot be greater than ngram_size_max.",
                )
            })
        }
        _ => None,
    }
}

pub fn lookup(sig: &Sig) -> Option<ScalarFn> {
    let float = |t: Option<&Type>| match t {
        Some(Type::Float64) => true,
        Some(Type::Array(e)) => matches!(**e, Type::Float64),
        _ => false,
    };
    let emu = sig.emulator_compat;
    Some(match sig.name {
        "debug_tokenlist" => f(|_, a| debug::debug_tokenlist(a)),
        "token" => f(|_, a| tokens::token(a)),
        "tokenize_fulltext" => f(move |_, a| tokens::tokenize_fulltext(a, emu)),
        "tokenize_substring" => f(move |_, a| tokens::tokenize_substring(a, emu)),
        "tokenize_ngrams" => f(move |_, a| {
            tokens::tokenize_ngrams(a, emu).map_err(|e| {
                prod_message(emu, e, |m| format!("Invalid use of TOKENIZE_NGRAMS: {m}"))
            })
        }),
        "tokenize_bool" => f(|_, a| tokens::tokenize_bool(a)),
        "tokenize_json" => f(|_, a| tokens::tokenize_json(a)),
        "tokenize_number" => {
            let float_value = float(sig.args.first());
            let float_bounds = sig.args.iter().skip(3).take(3).any(|t| float(Some(t)));
            f(move |_, a| {
                if !emu {
                    tokens::prod_number_options(a)?;
                }
                tokens::tokenize_number(a, float_bounds, float_value)
                    .map_err(|e| prod_message(emu, e, prod_tokenize_number))
            })
        }
        "tokenlist_concat" => f(move |_, a| tokens::tokenlist_concat(a, emu)),
        "search" => f(move |_, a| eval::search(a, emu)),
        "score" => f(move |_, a| eval::score(a, emu)),
        "search_substring" => f(move |_, a| eval::search_substring(a, emu)),
        "search_ngrams" => f(move |_, a| eval::search_ngrams(a, emu)),
        "score_ngrams" => f(move |_, a| {
            eval::score_ngrams(a, emu).map_err(|e| {
                prod_message(emu, e, |m| {
                    m.replace(
                        "function's first argument must be a TOKENLIST column generated by",
                        "function can only be used with TOKENLIST values generated by",
                    )
                })
            })
        }),
        "snippet" => {
            let cache = eval::SnippetCache::default();
            f(move |_, a| eval::snippet(a, emu, &cache))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spanbolt_storage::Value;

    #[test]
    fn analysis_errors_need_literal_options() {
        let int = |v| Some(Value::Int64(v));
        let s = |v: &str| Some(Value::String(v.into()));
        let ints = vec![Type::Int64; 6];
        let number = |args: &[Option<Value>]| {
            analysis_error("tokenize_number", args, &ints[..args.len()]).map(|e| e.message)
        };
        let (none, null) = (None, Some(Value::Null));
        let bounds = [none.clone(), null.clone(), null.clone(), int(10), int(1)];
        assert_eq!(
            number(&bounds).as_deref(),
            Some("min must be less than max, got: 10 and 1")
        );
        // A bound that isn't a literal, or options Spanner rejects first, wait for the call.
        assert_eq!(
            number(&[
                none.clone(),
                null.clone(),
                null.clone(),
                none.clone(),
                int(1)
            ]),
            None
        );
        assert_eq!(
            number(&[none.clone(), s("range"), null.clone(), int(10), int(1)]),
            None
        );
        assert_eq!(
            number(&[none.clone(), null.clone(), s("magic"), int(10), int(1)]),
            None
        );
        let floats = [
            Type::Float64,
            Type::Int64,
            Type::Int64,
            Type::Int64,
            Type::Int64,
        ];
        assert!(analysis_error("tokenize_number", &bounds, &floats).is_none());

        let ngrams = |max, min| {
            analysis_error("tokenize_ngrams", &[None, int(max), int(min)], &ints[..3])
                .map(|e| e.message)
        };
        assert!(ngrams(2, 3).is_some_and(|m| m.starts_with("Invalid use of TOKENIZE_NGRAMS: ")));
        assert_eq!(ngrams(13, 1), None);
        assert_eq!(ngrams(3, 2), None);
    }
}
