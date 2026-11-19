# Cloud Spanner vs emulator: grouped

Recorded on a production Cloud Spanner instance on 2026-11-19. 273 cases, 113 steps disagree. Details are in
`spanner-vs-emulator-tags.md` (tags search and oracle) and `spanner-vs-emulator-core.md` (sql/hints, sql/engine,
sql/aggregates, read/keysets). "Error messages only" means both sides return the same status code. "spanbolt fails"
counts steps where spanbolt doesn't match prod today (`check --oracle spanner`).

| Group | Steps | spanbolt fails | Follow-up |
|---|---:|---:|---|
| search | 71 | 71 | full-text search follows prod |
| window/QUALIFY/NULLS | 9 | 9 | window functions, QUALIFY and NULLS as prod supports them |
| filtered index | 4 | 1 | filtered-index WHERE columns not in the index |
| keyset reads | 1 | 1 | index read on a non-covered column returns UNIMPLEMENTED |
| hints | 1 | 0 | none needed |
| aggregates | 1 | 0 | none needed |
| error messages only | 26 | 26 | prod error text for key and index errors (and the rows above) |
| **total** | **113** | **108** | |

## search

**partitioned/no index**

- `sql/search_functions/and_operator`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/or_pipe`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/not_operator`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/double_negation`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/phrase_in_quotes`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/phrase_with_or_term`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/around_with_phrase`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/around_with_not_is_internal`: emulator INTERNAL, prod INVALID_ARGUMENT
- `sql/search_functions/around_needs_number`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/around_missing_space`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/around_unclosed`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/parentheses_rejected`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/trailing_separators_trimmed`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/lowercase_or_is_a_term`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/numbers_in_query`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/array_phrase_within_element`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/array_phrase_across_elements`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/array_around_across_elements`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/words_dialect`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/words_phrase_dialect`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/rquery_dialect_explicit`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/bad_dialect`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/score_counts_repeats`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/score_words_phrase`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/search_substring_default`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/search_substring_multiple`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/search_substring_word_prefix`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/search_substring_value_suffix`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/search_substring_phrase`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/search_substring_unsupported_type`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/search_substring_invalid_type`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/search_substring_all_types`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/search_ngrams_on_substring_tokens`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/tokenlist_concat_with_null`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_media/matching_mentions_accented_text_fails`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT

**WHERE only**

- `sql/search_functions/connected_phrase_order`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/around_operator`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/around_chain_mixed_distances`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/only_separators`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/empty_and_null_sources`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/search_substring_short_needs_whole_word`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/search_substring_prefix_multiword_null`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/search_ngrams_code`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_functions/tokenize_in_query`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_granite/search_null_array_column`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_persons/name_null_rows_and_null_query`: emulator OK, prod INVALID_ARGUMENT

**SCORE needs SEARCH**

- `sql/search_granite/score_raw_query_counts_terms`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_granite/score_null_source`: emulator OK, prod INVALID_ARGUMENT
- `sql/search_granite/score_on_null_array`: emulator OK, prod INVALID_ARGUMENT

**non-ASCII/punctuation**

- `sql/prod_fidelity/search_non_ascii_term`: emulator OK, prod OK
- `sql/prod_fidelity/search_non_ascii_uppercase`: emulator OK, prod OK
- `sql/prod_fidelity/score_non_ascii_order`: emulator OK, prod OK
- `sql/search_granite/quoted_norwegian_letters`: emulator INVALID_ARGUMENT, prod OK
- `sql/search_persons/name_hyphen_replaced_by_space`: emulator INVALID_ARGUMENT, prod OK
- `sql/search_persons/name_norwegian_letters_fail_to_parse`: emulator OK, prod OK
- `sql/search_persons/name_double_space_rejected`: emulator INVALID_ARGUMENT, prod OK

**SCORE values**

- `sql/search_functions/score_ngrams_on_substring`: emulator OK, prod OK
- `sql/search_granite/weighted_relevance_score`: emulator OK, prod OK
- `sql/search_granite/all_matches_group_by_max`: emulator OK, prod OK
- `sql/search_granite/ngram_score_order`: emulator OK, prod OK
- `sql/search_granite/ngram_score_values`: emulator OK, prod OK

**SNIPPET format**

- `sql/prod_fidelity/snippet_non_ascii`: emulator OK, prod OK
- `sql/prod_fidelity/snippet_escaped_quote`: emulator OK, prod OK
- `sql/search/snippet`: emulator OK, prod OK
- `sql/search_functions/snippet_default`: emulator OK, prod OK
- `sql/search_functions/snippet_multiple_terms`: emulator OK, prod OK
- `sql/search_functions/snippet_no_match_prefix`: emulator OK, prod OK
- `sql/search_functions/snippet_width_truncates`: emulator OK, prod OK
- `sql/search_functions/snippet_null_and_empty`: emulator OK, prod OK
- `sql/search_functions/snippet_html_content_type`: emulator OK, prod OK

**optimizer**

- `sql/search_media/paged_events_by_mentions`: emulator OK, prod INVALID_ARGUMENT

## window/QUALIFY/NULLS

- `sql/prod_fidelity/window_rank_dense_rank`: emulator INVALID_ARGUMENT, prod UNIMPLEMENTED
- `sql/prod_fidelity/window_partition_sum`: emulator INVALID_ARGUMENT, prod UNIMPLEMENTED
- `sql/prod_fidelity/window_running_sum_rows`: emulator INVALID_ARGUMENT, prod UNIMPLEMENTED
- `sql/prod_fidelity/window_lag_lead`: emulator INVALID_ARGUMENT, prod UNIMPLEMENTED
- `sql/prod_fidelity/window_first_last_value`: emulator INVALID_ARGUMENT, prod UNIMPLEMENTED
- `sql/prod_fidelity/window_row_number_partition`: emulator INVALID_ARGUMENT, prod UNIMPLEMENTED
- `sql/prod_fidelity/window_ntile_percent_rank`: emulator INVALID_ARGUMENT, prod UNIMPLEMENTED
- `sql/prod_fidelity/window_named_window`: emulator INVALID_ARGUMENT, prod UNIMPLEMENTED
- `sql/engine/window_function_rejected`: emulator INVALID_ARGUMENT, prod UNIMPLEMENTED

## filtered index

- `sql/hints/null_filtered_with_is_not_null`: emulator INVALID_ARGUMENT, prod OK (spanbolt matches prod)
- `sql/hints/null_filtered_equality`: emulator INVALID_ARGUMENT, prod OK (spanbolt matches prod)
- `sql/hints/null_filtered_check_disabled`: emulator OK, prod INVALID_ARGUMENT
- `sql/hints/null_filtered_in_unnest_is_not_null`: emulator INVALID_ARGUMENT, prod OK (spanbolt matches prod)

## keyset reads

- `read/keysets/index_non_covered_column`: emulator NOT_FOUND, prod UNIMPLEMENTED

## hints

- `sql/hints/statement_hint_optimizer`: emulator INVALID_ARGUMENT, prod OK (spanbolt matches prod)

## aggregates

- `sql/aggregates/array_agg_struct`: emulator OK, prod OK (spanbolt matches prod)

## error messages only

**filtered index**

- `sql/hints/null_filtered_without_filter`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/hints/null_filtered_in_unnest`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT

**hints**

- `sql/hints/force_index_unknown`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT

**keyset reads**

- `read/keysets/no_columns_in_key_too_long`: emulator FAILED_PRECONDITION, prod FAILED_PRECONDITION
- `read/keysets/key_wrong_type`: emulator FAILED_PRECONDITION, prod FAILED_PRECONDITION
- `read/keysets/index_full_key_point`: emulator FAILED_PRECONDITION, prod FAILED_PRECONDITION
- `read/keysets/index_composite_prefix`: emulator FAILED_PRECONDITION, prod FAILED_PRECONDITION
- `read/keysets/index_unknown`: emulator NOT_FOUND, prod NOT_FOUND
- `read/keysets/index_wrong_table`: emulator FAILED_PRECONDITION, prod FAILED_PRECONDITION

**search**

- `sql/search/search_non_tokenlist_error`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/score_bytes_query`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/score_ngrams_on_fulltext_error`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/tokenlist_concat_mixed_error`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/tokenize_ngrams_bad_sizes`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/tokenize_ngrams_too_large`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/tokenize_number_bad_algorithm`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/tokenize_number_min_max`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/tokenize_number_float_granularity`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/tokenize_number_fp_on_int`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/snippet_bad_width`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_functions/force_search_index_rw`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_persons/name_in_read_write_transaction`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/search_persons/dml_delete_with_search_in_read_write`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT

**window/QUALIFY/NULLS**

- `sql/prod_fidelity/order_nulls_last`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/prod_fidelity/order_desc_nulls_first`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT
- `sql/prod_fidelity/array_agg_order_by_nulls_last`: emulator INVALID_ARGUMENT, prod INVALID_ARGUMENT


## Follow-up recording: search, NULLS and filtered indexes

Recorded on production Cloud Spanner on 2026-11-19 (commit 12559f7): 73 `oracle2` cases (33 steps differ) and 19 cases from
`mutations/filtered_index`, `read/filtered_index` and `search_media/paged_events_by_mentions` (6 differ).
Details are in `spanner-vs-emulator-sb051-tags.md` and `-filters.md`; the follow-up for each group is in the table below.

| Group | Steps | spanbolt fails | Follow-up |
|---|---:|---:|---|
| search: rquery parsing and matching | 9 | 9 | full-text search |
| search: SCORE / SCORE_NGRAMS values or SEARCH-predicate rule | 10 | 10 | full-text search |
| search: partition / WHERE filter needed | 2 | 2 | full-text search |
| search: non-ASCII with filters | 1 | 1 | full-text search |
| search: optimizer version (hint-less paged query, hinted variants) | 5 | 2 | full-text search (don't follow) |
| search: error text only | 2 | 2 | full-text search |
| NULLS FIRST/LAST default orders accepted | 4 | 4 | window functions and NULLS |
| window function status code | 1 | 1 | window functions and NULLS |
| filtered index FORCE_INDEX message suffix | 2 | 2 | filtered indexes |
| filtered index / unique / FK error text only | 3 | 3 | filtered indexes |
| **total** | **39** | **36** | |

The open filtered-index question: prod rejects a filter column that is not in the index (`filter_column_not_in_index`:
`same`, UNIMPLEMENTED). spanbolt accepts it today.
