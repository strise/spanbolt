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
//! - TOKENIZE_NGRAMS and SEARCH_NGRAMS over it keep case (the emulator lower-cases both), and
//!   n-grams are cut from characters, not UTF-8 bytes (`tokens::char_ngrams_of`).
//!
//! DEBUG_TOKENLIST, which only prod has, prints token lists in prod's format (`debug`). It is
//! UNIMPLEMENTED until the analyser's catalog declares it (`engine.rs`).
//!
//! Not followed yet: SCORE with `enhance_query => true`; SNIPPET widths of scripts Arial doesn't
//! cover (Georgian, Armenian, Indic, Thai) and of texts with no letters or digits.

mod debug;
mod eval;
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
    m.to_string()
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
        "tokenize_substring" => f(|_, a| tokens::tokenize_substring(a)),
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
                tokens::tokenize_number(a, float_bounds, float_value)
                    .map_err(|e| prod_message(emu, e, prod_tokenize_number))
            })
        }
        "tokenlist_concat" => f(|_, a| tokens::tokenlist_concat(a)),
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
