# Cloud Spanner vs emulator

96 steps disagree.

### `sql/prod_fidelity/window_rank_dense_rank` (line 23)

```sql
SELECT Id, RANK() OVER (ORDER BY Val DESC) AS r, DENSE_RANK() OVER (ORDER BY Val DESC) AS dr FROM Items WHERE Val IS NOT NULL ORDER BY Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Analytic functions not supported [at 1:12]
```

Spanner:

```text
error: UNIMPLEMENTED
message: Unsupported analytic scan: multiple analytic functions aren't supported
```

### `sql/prod_fidelity/window_partition_sum` (line 33)

```sql
SELECT Id, SUM(Val) OVER (PARTITION BY Grp) AS s, COUNT(*) OVER (PARTITION BY Grp) AS n FROM Items ORDER BY Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Analytic functions not supported [at 1:12]
```

Spanner:

```text
error: UNIMPLEMENTED
message: Unsupported analytic scan: multiple analytic functions aren't supported
```

### `sql/prod_fidelity/window_running_sum_rows` (line 40)

```sql
SELECT Id, SUM(Val) OVER (ORDER BY Id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS s FROM Items ORDER BY Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Analytic functions not supported [at 1:12]
```

Spanner:

```text
error: UNIMPLEMENTED
message: Unsupported built-in function: SUM.
```

### `sql/prod_fidelity/window_lag_lead` (line 47)

```sql
SELECT Id, LAG(Val) OVER (ORDER BY Id) AS prev, LEAD(Val, 2, -1) OVER (ORDER BY Id) AS next2 FROM Items ORDER BY Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Analytic functions not supported [at 1:12]
```

Spanner:

```text
error: UNIMPLEMENTED
message: Unsupported analytic scan: multiple analytic functions aren't supported
```

### `sql/prod_fidelity/window_first_last_value` (line 54)

```sql
SELECT Id, FIRST_VALUE(Id) OVER (PARTITION BY Grp ORDER BY Id) AS f, LAST_VALUE(Id) OVER (PARTITION BY Grp ORDER BY Id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) AS l FROM Items ORDER BY Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Analytic functions not supported [at 1:12]
```

Spanner:

```text
error: UNIMPLEMENTED
message: Unsupported analytic scan: multiple analytic functions aren't supported
```

### `sql/prod_fidelity/window_row_number_partition` (line 61)

```sql
SELECT Id, ROW_NUMBER() OVER (PARTITION BY Grp ORDER BY Val DESC, Id) AS rn FROM Items ORDER BY Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Analytic functions not supported [at 1:12]
```

Spanner:

```text
error: UNIMPLEMENTED
message: Unsupported built-in function: ROW_NUMBER.
```

### `sql/prod_fidelity/window_ntile_percent_rank` (line 68)

```sql
SELECT Id, NTILE(2) OVER (ORDER BY Id) AS t, PERCENT_RANK() OVER (ORDER BY Id) AS pr FROM Items ORDER BY Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Analytic functions not supported [at 1:12]
```

Spanner:

```text
error: UNIMPLEMENTED
message: Unsupported analytic scan: multiple analytic functions aren't supported
```

### `sql/prod_fidelity/window_named_window` (line 75)

```sql
SELECT Id, ROW_NUMBER() OVER w AS rn FROM Items WINDOW w AS (ORDER BY Id DESC) ORDER BY Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Analytic functions not supported [at 1:12]
```

Spanner:

```text
error: UNIMPLEMENTED
message: Unsupported built-in function: ROW_NUMBER.
```

### `sql/prod_fidelity/order_nulls_last` (line 89)

```sql
SELECT Id, Val FROM Items ORDER BY Val NULLS LAST, Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: NULLS FIRST and NULLS LAST are not supported [at 1:40]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: Non default sort order of NULL values is not supported. Sort expressions in `ORDER BY` clause should use `NULLS FIRST` option with ascending order and `NULLS LAST` option with descending order.
```

### `sql/prod_fidelity/order_desc_nulls_first` (line 96)

```sql
SELECT Id, Val FROM Items ORDER BY Val DESC NULLS FIRST, Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: NULLS FIRST and NULLS LAST are not supported [at 1:45]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: Non default sort order of NULL values is not supported. Sort expressions in `ORDER BY` clause should use `NULLS FIRST` option with ascending order and `NULLS LAST` option with descending order.
```

### `sql/prod_fidelity/search_non_ascii_term` (line 115)

```sql
SELECT Id FROM Items WHERE SEARCH(Body_Tokens, 'tromsø') ORDER BY Id
```

Emulator:

```text
columns: Id INT64
```

Spanner:

```text
columns: Id INT64
["1"]
["2"]
```

### `sql/prod_fidelity/search_non_ascii_uppercase` (line 121)

```sql
SELECT Id FROM Items WHERE SEARCH(Body_Tokens, 'ÆRLIG ålesund') ORDER BY Id
```

Emulator:

```text
columns: Id INT64
```

Spanner:

```text
columns: Id INT64
["2"]
```

### `sql/prod_fidelity/score_non_ascii_order` (line 169)

```sql
SELECT Id, SCORE(Body_Tokens, 'tromsø') > 0 AS scored FROM Items WHERE SEARCH(Body_Tokens, 'tromsø') ORDER BY SCORE(Body_Tokens, 'tromsø') DESC, Id
```

Emulator:

```text
columns: Id INT64, scored BOOL
```

Spanner:

```text
columns: Id INT64, scored BOOL
["1",true]
["2",true]
```

### `sql/prod_fidelity/snippet_non_ascii` (line 175)

```sql
SELECT Id, SNIPPET(Body, 'tromsø') AS s FROM Items WHERE Id IN (1, 2) ORDER BY Id
```

Emulator:

```text
columns: Id INT64, s JSON
["1","{\"snippets\":[{\"highlights\":[],\"snippet\":\"Tromsø kommune signerte avtalen\"}]}"]
["2","{\"snippets\":[{\"highlights\":[],\"snippet\":\"Ærlig talt: Ålesund og Tromsø\"}]}"]
```

Spanner:

```text
columns: Id INT64, s JSON
["1","{\"snippets\":[{\"highlights\":[{\"begin\":1,\"end\":7}],\"snippet\":\"Tromsø kommune signerte avtalen\",\"source_begin\":1,\"source_end\":32}]}"]
["2","{\"snippets\":[{\"highlights\":[{\"begin\":24,\"end\":30}],\"snippet\":\"Ærlig talt: Ålesund og Tromsø\",\"source_begin\":1,\"source_end\":30}]}"]
```

### `sql/prod_fidelity/snippet_escaped_quote` (line 183)

```sql
SELECT Id, SNIPPET(Body, 'hello') AS s FROM Items WHERE Id = 4
```

Emulator:

```text
columns: Id INT64, s JSON
["4","{\"snippets\":[{\"highlights\":[{\"end_position\":15,\"start_position\":10}],\"snippet\":\"He said \\\"hello\\\" to O'Brien\"}]}"]
```

Spanner:

```text
columns: Id INT64, s JSON
["4","{\"snippets\":[{\"highlights\":[{\"begin\":10,\"end\":15}],\"snippet\":\"He said \\\"hello\\\" to O'Brien\",\"source_begin\":1,\"source_end\":27}]}"]
```

### `sql/prod_fidelity/array_agg_order_by_nulls_last` (line 190)

```sql
SELECT ARRAY_AGG(Val IGNORE NULLS ORDER BY Val DESC) AS v, ARRAY_AGG(Id ORDER BY Val NULLS LAST, Id) AS ids FROM Items
```

Emulator:

```text
error: INVALID_ARGUMENT
message: NULLS FIRST and NULLS LAST are not supported [at 1:86]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: Non default sort order of NULL values is not supported. Sort expressions in `ORDER BY` clause should use `NULLS FIRST` option with ascending order and `NULLS LAST` option with descending order.
```

### `sql/search/snippet` (line 101)

```sql
SELECT DocId, SNIPPET(Body, 'sanctions') AS snip FROM Docs WHERE SEARCH(Body_Tokens, 'sanctions')
```

Emulator:

```text
columns: DocId INT64, snip JSON
["4","{\"snippets\":[{\"highlights\":[{\"end_position\":14,\"start_position\":5}],\"snippet\":\"New sanctions target shipping firms in the region.\"}]}"]
["3","{\"snippets\":[{\"highlights\":[{\"end_position\":48,\"start_position\":39}],\"snippet\":\"Delta ASA filed its annual report. No sanctions noted.\"}]}"]
```

Spanner:

```text
columns: DocId INT64, snip JSON
["3","{\"snippets\":[{\"highlights\":[{\"begin\":39,\"end\":48}],\"snippet\":\"Delta ASA filed its annual report. No sanctions noted\",\"source_begin\":1,\"source_end\":54}]}"]
["4","{\"snippets\":[{\"highlights\":[{\"begin\":5,\"end\":14}],\"snippet\":\"New sanctions target shipping firms in the region\",\"source_begin\":1,\"source_end\":50}]}"]
```

### `sql/search/search_non_tokenlist_error` (line 125)

```sql
SELECT DocId FROM Docs WHERE SEARCH(Body, 'beta')
```

Emulator:

```text
error: INVALID_ARGUMENT
message: No matching signature for function SPANNER:SEARCH Argument types: STRING, STRING Signature: SEARCH(TOKENLIST, STRING, [enhance_query => BOOL], [language_tag => STRING], [dialect => STRING], [dictionary => STRING]) Argument 1: Unable to coerce type STRING to expected type TOKENLIST [at 1:30]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: No matching signature for function SPANNER:SEARCH\n Argument types: STRING, STRING\n Signature: SEARCH(TOKENLIST, STRING, [enhance_query => BOOL], [language_tag => STRING], [dialect => STRING], [enhance_query_options => JSON], [dictionary => STRING])\n Argument 1: Unable to coerce type STRING to expected type TOKENLIST [at 1:30]
```

### `sql/search_functions/and_operator` (line 35)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'spanner emulator')
```

Emulator:

```text
columns: Id INT64
["3"]
["1"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/or_pipe` (line 43)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'fox|google')
```

Emulator:

```text
columns: Id INT64
["1"]
["2"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/not_operator` (line 51)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'spanner -cloud')
```

Emulator:

```text
columns: Id INT64
["3"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/double_negation` (line 58)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, '--cloud')
```

Emulator:

```text
columns: Id INT64
["1"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/phrase_in_quotes` (line 65)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, '"lazy dog"')
```

Emulator:

```text
columns: Id INT64
["2"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/phrase_with_or_term` (line 72)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, '"quick brown|red fox"')
```

Emulator:

```text
columns: Id INT64
["2"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/connected_phrase_order` (line 79)

```sql
SELECT Id, SEARCH(Body_Tokens, 'spanner-emulator') AS a, SEARCH(Body_Tokens, 'emulator-spanner') AS b FROM Docs WHERE Id IN (1, 3)
```

Emulator:

```text
columns: Id INT64, a BOOL, b BOOL
["1",true,false]
["3",false,false]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The function SEARCH can only be used in a WHERE clause.
```

### `sql/search_functions/around_operator` (line 87)

```sql
SELECT Id, SEARCH(Body_Tokens, 'quick AROUND(1) fox') AS a1, SEARCH(Body_Tokens, 'quick AROUND(2) fox') AS a2, SEARCH(Body_Tokens, 'fox AROUND(3) quick') AS a3 FROM Docs WHERE Id = 2
```

Emulator:

```text
columns: Id INT64, a1 BOOL, a2 BOOL, a3 BOOL
["2",true,true,true]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The function SEARCH can only be used in a WHERE clause.
```

### `sql/search_functions/around_chain_mixed_distances` (line 94)

```sql
SELECT Id, SEARCH(Body_Tokens, 'the AROUND(1) fox AROUND(9) dog') AS r FROM Docs WHERE Id = 2
```

Emulator:

```text
columns: Id INT64, r BOOL
["2",true]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The function SEARCH can only be used in a WHERE clause.
```

### `sql/search_functions/around_with_phrase` (line 101)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, '"lazy dog" AROUND(4) jumps')
```

Emulator:

```text
columns: Id INT64
["2"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/around_with_not_is_internal` (line 108)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, '-fox AROUND(2) dog')
```

Emulator:

```text
error: INTERNAL
message: GOOGLESQL_RET_CHECK failure (backend/query/search/search_evaluator.cc:152) !in_phrase NOT nodes cannot appear in phrases
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/around_needs_number` (line 115)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'quick AROUND(two) fox')
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Error(s) parsing search query `quick AROUND(two) fox`, has error(s): -Syntax error on column 14: Expecting '<NUMBER>
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/around_missing_space` (line 122)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'quick AROUND(2)fox')
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Error(s) parsing search query `quick AROUND(2)fox`, has error(s): -Syntax error on column 16: Expecting '<SEPARATOR>
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/around_unclosed` (line 129)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'quick AROUND(2 fox')
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Error(s) parsing search query `quick AROUND(2 fox`, has error(s): -Syntax error on column 15: Expecting ')
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/parentheses_rejected` (line 136)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'spanner (cloud | fox)')
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Error(s) parsing search query `spanner (cloud | fox)`, has error(s): -Encountered error on column 9 while parsing: sub_expression. Using parentheses to group query terms is not supported in rquery parser.
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/trailing_separators_trimmed` (line 143)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, '!!emulator?? ')
```

Emulator:

```text
columns: Id INT64
["3"]
["1"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/only_separators` (line 151)

```sql
SELECT Id, SEARCH(Body_Tokens, '?!*') AS r FROM Docs WHERE Id = 1
```

Emulator:

```text
columns: Id INT64, r BOOL
["1",false]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The function SEARCH can only be used in a WHERE clause.
```

### `sql/search_functions/lowercase_or_is_a_term` (line 158)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'fox or dog')
```

Emulator:

```text
columns: Id INT64
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/numbers_in_query` (line 164)

```sql
SELECT Id FROM Docs WHERE SEARCH(Parts_Tokens, 'solo') OR SEARCH(Body_Tokens, '42')
```

Emulator:

```text
columns: Id INT64
["5"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/array_phrase_within_element` (line 171)

```sql
SELECT Id FROM Docs WHERE SEARCH(Parts_Tokens, '"cloud spanner"')
```

Emulator:

```text
columns: Id INT64
["1"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/array_phrase_across_elements` (line 178)

```sql
SELECT Id FROM Docs WHERE SEARCH(Parts_Tokens, '"spanner emulator"')
```

Emulator:

```text
columns: Id INT64
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/array_around_across_elements` (line 184)

```sql
SELECT Id FROM Docs WHERE SEARCH(Parts_Tokens, 'spanner AROUND(5) emulator')
```

Emulator:

```text
columns: Id INT64
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/empty_and_null_sources` (line 190)

```sql
SELECT Id, SEARCH(Body_Tokens, 'x') AS b, SEARCH(Parts_Tokens, 'x') AS p FROM Docs WHERE Id IN (4, 5)
```

Emulator:

```text
columns: Id INT64, b BOOL, p BOOL
["4",false,null]
["5",null,false]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The function SEARCH can only be used in a WHERE clause.
```

### `sql/search_functions/words_dialect` (line 198)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'Spanner -emulator', dialect => 'words')
```

Emulator:

```text
columns: Id INT64
["3"]
["1"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/words_phrase_dialect` (line 206)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'the lazy dog', dialect => 'words_phrase')
```

Emulator:

```text
columns: Id INT64
["2"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/rquery_dialect_explicit` (line 213)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'lazy OR cloud', dialect => 'rquery', enhance_query => true)
```

Emulator:

```text
columns: Id INT64
["1"]
["2"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/bad_dialect` (line 221)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'lazy', dialect => 'klingon')
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Invalid dialect: klingon
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/score_counts_repeats` (line 228)

```sql
SELECT Id, SCORE(Body_Tokens, 'spanner cloud') AS s FROM Docs ORDER BY Id
```

Emulator:

```text
columns: Id INT64, s FLOAT64
["1",3.0]
["2",0.0]
["3",2.0]
["4",0.0]
["5",0.0]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SCORE is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/score_words_phrase` (line 239)

```sql
SELECT Id, SCORE(Body_Tokens, 'spanner emulator', dialect => 'words_phrase') AS s FROM Docs WHERE Id IN (1, 3) ORDER BY Id
```

Emulator:

```text
columns: Id INT64, s FLOAT64
["1",1.0]
["3",0.0]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SCORE is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/score_bytes_query` (line 247)

```sql
SELECT Id, SCORE(Body_Tokens, b'spanner') AS s FROM Docs WHERE Id = 1
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Invalid search query type: BYTES.
```

Spanner:

```text
error: INVALID_ARGUMENT
message: No matching signature for function SPANNER:SCORE\n Argument types: TOKENLIST, BYTES\n Signature: SCORE(TOKENLIST, STRING, [enhance_query => BOOL], [language_tag => STRING], [dialect => STRING], [enhance_query_options => JSON], [options => JSON], [dictionary => STRING])\n Argument 2: Unable to coerce type BYTES to expected type STRING [at 1:12]
```

### `sql/search_functions/search_substring_default` (line 254)

```sql
SELECT Id FROM Docs WHERE SEARCH_SUBSTRING(Body_Substr, 'pann')
```

Emulator:

```text
columns: Id INT64
["3"]
["1"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH_SUBSTRING is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/search_substring_short_needs_whole_word` (line 262)

```sql
SELECT Id, SEARCH_SUBSTRING(Body_Substr, 'o') AS o, SEARCH_SUBSTRING(Body_Substr, 'on') AS on2 FROM Docs WHERE Id IN (1, 2) ORDER BY Id
```

Emulator:

```text
columns: Id INT64, o BOOL, on2 BOOL
["1",false,true]
["2",false,false]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The function SEARCH_SUBSTRING can only be used in a WHERE clause.
```

### `sql/search_functions/search_substring_multiple` (line 270)

```sql
SELECT Id FROM Docs WHERE SEARCH_SUBSTRING(Body_Substr, 'emul span')
```

Emulator:

```text
columns: Id INT64
["3"]
["1"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH_SUBSTRING is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/search_substring_word_prefix` (line 278)

```sql
SELECT Id FROM Docs WHERE SEARCH_SUBSTRING(Body_Substr, 'jum', relative_search_type => 'word_prefix')
```

Emulator:

```text
columns: Id INT64
["2"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH_SUBSTRING is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/search_substring_value_suffix` (line 285)

```sql
SELECT Id FROM Docs WHERE SEARCH_SUBSTRING(Body_Substr, 'cloud', relative_search_type => 'value_suffix')
```

Emulator:

```text
columns: Id INT64
["1"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH_SUBSTRING is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/search_substring_phrase` (line 292)

```sql
SELECT Id FROM Docs WHERE SEARCH_SUBSTRING(Body_Substr, 'lazy do', relative_search_type => 'phrase')
```

Emulator:

```text
columns: Id INT64
["2"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH_SUBSTRING is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/search_substring_prefix_multiword_null` (line 299)

```sql
SELECT Id, SEARCH_SUBSTRING(Body_Substr, 'lazy dog', relative_search_type => 'word_prefix') AS r FROM Docs WHERE Id = 2
```

Emulator:

```text
columns: Id INT64, r BOOL
["2",null]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The function SEARCH_SUBSTRING can only be used in a WHERE clause.
```

### `sql/search_functions/search_substring_unsupported_type` (line 306)

```sql
SELECT Id FROM Docs WHERE SEARCH_SUBSTRING(Body_Substr, 'fox', relative_search_type => 'word_suffix')
```

Emulator:

```text
error: INVALID_ARGUMENT
message: The referenced substring TOKENLIST is not tokenized with 'word_suffix' support.
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH_SUBSTRING is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/search_substring_invalid_type` (line 313)

```sql
SELECT Id FROM Docs WHERE SEARCH_SUBSTRING(Body_Substr, 'fox', relative_search_type => 'middle')
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Invalid relative_search_type: 'middle'.
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH_SUBSTRING is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/search_substring_all_types` (line 320)

```sql
SELECT Id FROM Docs WHERE SEARCH_SUBSTRING(Code_Substr, '123', relative_search_type => 'word_suffix')
```

Emulator:

```text
columns: Id INT64
["1"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH_SUBSTRING cannot be used because there is no search index indexing the column Code_Substr, or the query is executed with base table only hint.
```

### `sql/search_functions/search_ngrams_on_substring_tokens` (line 355)

```sql
SELECT Id FROM Docs WHERE SEARCH_NGRAMS(Body_Substr, 'emulatr')
```

Emulator:

```text
columns: Id INT64
["3"]
["1"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH_NGRAMS is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/search_ngrams_code` (line 363)

```sql
SELECT Id, SEARCH_NGRAMS(Code_Ngrams, 'abc') AS a, SEARCH_NGRAMS(Code_Ngrams, 'xyz987') AS b FROM Docs WHERE Id IN (1, 2, 3, 5) ORDER BY Id
```

Emulator:

```text
columns: Id INT64, a BOOL, b BOOL
["1",true,false]
["2",false,false]
["3",true,false]
["5",false,false]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The function SEARCH_NGRAMS can only be used in a WHERE clause.
```

### `sql/search_functions/score_ngrams_on_substring` (line 373)

```sql
SELECT Id, SCORE_NGRAMS(Body_Substr, 'lazy dogs') AS s FROM Docs WHERE Id = 2
```

Emulator:

```text
columns: Id INT64, s FLOAT64
["2",0.15]
```

Spanner:

```text
columns: Id INT64, s FLOAT64
["2",0.1590909090909091]
```

### `sql/search_functions/score_ngrams_on_fulltext_error` (line 380)

```sql
SELECT Id, SCORE_NGRAMS(Body_Tokens, 'lazy') AS s FROM Docs WHERE Id = 2
```

Emulator:

```text
error: INVALID_ARGUMENT
message: SCORE_NGRAMS function's first argument must be a TOKENLIST column generated by TOKENIZE_SUBSTRING or TOKENIZE_NGRAMS
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SCORE_NGRAMS function can only be used with TOKENLIST values generated by TOKENIZE_SUBSTRING or TOKENIZE_NGRAMS
```

### `sql/search_functions/tokenlist_concat_mixed_error` (line 394)

```sql
SELECT Id FROM Docs WHERE SEARCH(TOKENLIST_CONCAT([Body_Tokens, Body_Substr]), 'fox')
```

Emulator:

```text
error: INVALID_ARGUMENT
message: All elements to TOKENLIST_CONCAT must be produced by the same kind of tokenization function.
```

Spanner:

```text
error: INVALID_ARGUMENT
message: All elements to TOKENLIST_CONCAT must be produced by the same kind of tokenization function. [at 1:51]
```

### `sql/search_functions/tokenlist_concat_with_null` (line 401)

```sql
SELECT Id FROM Docs WHERE SEARCH(TOKENLIST_CONCAT([Body_Tokens, Parts_Tokens]), 'solo OR fox')
```

Emulator:

```text
columns: Id INT64
["5"]
["2"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_functions/tokenize_in_query` (line 409)

```sql
SELECT SEARCH(TOKENIZE_FULLTEXT('Hello, World! Ærlig'), 'world') AS a, SEARCH(TOKENIZE_FULLTEXT(['a b', 'c']), '"b c"') AS b, SCORE(TOKENIZE_FULLTEXT('a a a'), 'a') AS c
```

Emulator:

```text
columns: a BOOL, b BOOL, c FLOAT64
[true,false,3.0]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The function SEARCH can only be used in a WHERE clause.
```

### `sql/search_functions/tokenize_ngrams_bad_sizes` (line 416)

```sql
SELECT SEARCH_NGRAMS(TOKENIZE_NGRAMS('abc', ngram_size_min => 3, ngram_size_max => 2), 'abc') AS a
```

Emulator:

```text
error: INVALID_ARGUMENT
message: ngram_size_min cannot be greater than ngram_size_max.
```

Spanner:

```text
error: INVALID_ARGUMENT
message: Invalid use of TOKENIZE_NGRAMS: ngram_size_min cannot be greater than ngram_size_max. [at 1:22]
```

### `sql/search_functions/tokenize_ngrams_too_large` (line 423)

```sql
SELECT SEARCH_NGRAMS(TOKENIZE_NGRAMS('abc', ngram_size_max => 13), 'abc') AS a
```

Emulator:

```text
error: INVALID_ARGUMENT
message: ngram_size_max cannot be greater than 12.
```

Spanner:

```text
error: INVALID_ARGUMENT
message: Invalid use of TOKENIZE_NGRAMS: ngram_size_max cannot be greater than 12.
```

### `sql/search_functions/tokenize_number_bad_algorithm` (line 437)

```sql
SELECT TOKENIZE_NUMBER(5, algorithm => 'magic') IS NULL AS a
```

Emulator:

```text
error: INVALID_ARGUMENT
message: TOKENIZE_NUMBER: Algorithm is not supported: 'magic'; supported algorithms are: 'auto', 'logtree', 'prefixtree', 'floatingpoint' (floatingpoint is supported only for floating point types)
```

Spanner:

```text
error: INVALID_ARGUMENT
message: unsupported algorithm 'magic'; supported algorithms are 'auto', 'logtree', 'prefixtree', or 'floatingpoint' (floatingpoint is supported only for floating point types)
```

### `sql/search_functions/tokenize_number_min_max` (line 444)

```sql
SELECT TOKENIZE_NUMBER(5, min => 10, max => 1) IS NULL AS a
```

Emulator:

```text
error: INVALID_ARGUMENT
message: TOKENIZE_NUMBER: min must be less than max, got: 10 and 1
```

Spanner:

```text
error: INVALID_ARGUMENT
message: TOKENIZE_NUMBER: min must be less than max, got: 10 and 1 [at 1:8]
```

### `sql/search_functions/tokenize_number_float_granularity` (line 451)

```sql
SELECT TOKENIZE_NUMBER(1.5, min => 0.0, max => 1.0, granularity => 2.0) IS NULL AS a
```

Emulator:

```text
error: INVALID_ARGUMENT
message: TOKENIZE_NUMBER: granularity (2.000000) must be less than or equal to the difference between min and max (0.000000, 1.000000)
```

Spanner:

```text
error: INVALID_ARGUMENT
message: min is not allowed for algorithm IEEE_FP
```

### `sql/search_functions/tokenize_number_fp_on_int` (line 458)

```sql
SELECT TOKENIZE_NUMBER(5, algorithm => 'floatingpoint') IS NULL AS a
```

Emulator:

```text
error: INVALID_ARGUMENT
message: TOKENIZE_NUMBER: algorithm 'floatingpoint' can only be used to tokenize floating point values
```

Spanner:

```text
error: INVALID_ARGUMENT
message: algorithm 'floatingpoint' can only be used to tokenize floating point values
```

### `sql/search_functions/snippet_default` (line 472)

```sql
SELECT Id, SNIPPET(Body, 'spanner') AS s FROM Docs WHERE Id IN (1, 3)
```

Emulator:

```text
columns: Id INT64, s JSON
["1","{\"snippets\":[{\"highlights\":[{\"end_position\":14,\"start_position\":7}],\"snippet\":\"Cloud Spanner emulator runs on Google Cloud\"}]}"]
["3","{\"snippets\":[{\"highlights\":[{\"end_position\":8,\"start_position\":1},{\"end_position\":37,\"start_position\":30}],\"snippet\":\"Spanner: the emulator is not Spanner itself.\"}]}"]
```

Spanner:

```text
columns: Id INT64, s JSON
["1","{\"snippets\":[{\"highlights\":[{\"begin\":7,\"end\":14}],\"snippet\":\"Cloud Spanner emulator runs on Google Cloud\",\"source_begin\":1,\"source_end\":44}]}"]
["3","{\"snippets\":[{\"highlights\":[{\"begin\":1,\"end\":8},{\"begin\":30,\"end\":37}],\"snippet\":\"Spanner: the emulator is not Spanner itself\",\"source_begin\":1,\"source_end\":44}]}"]
```

### `sql/search_functions/snippet_multiple_terms` (line 480)

```sql
SELECT Id, SNIPPET(Body, 'quick dog') AS s FROM Docs WHERE Id = 2
```

Emulator:

```text
columns: Id INT64, s JSON
["2","{\"snippets\":[{\"highlights\":[{\"end_position\":10,\"start_position\":5},{\"end_position\":44,\"start_position\":41}],\"snippet\":\"The quick brown fox jumps over the lazy dog\"}]}"]
```

Spanner:

```text
columns: Id INT64, s JSON
["2","{\"snippets\":[{\"highlights\":[{\"begin\":5,\"end\":10},{\"begin\":41,\"end\":44}],\"snippet\":\"The quick brown fox jumps over the lazy dog\",\"source_begin\":1,\"source_end\":44}]}"]
```

### `sql/search_functions/snippet_no_match_prefix` (line 487)

```sql
SELECT Id, SNIPPET(Body, 'zebra', max_snippet_width => 20) AS s FROM Docs WHERE Id = 2
```

Emulator:

```text
columns: Id INT64, s JSON
["2","{\"snippets\":[{\"highlights\":[],\"snippet\":\"The quick brown fox\"}]}"]
```

Spanner:

```text
columns: Id INT64, s JSON
["2","{\"snippets\":[{\"highlights\":[],\"snippet\":\"The quick brown\",\"source_begin\":1,\"source_end\":16}]}"]
```

### `sql/search_functions/snippet_width_truncates` (line 494)

```sql
SELECT Id, SNIPPET(Body, 'lazy', max_snippet_width => 15) AS s FROM Docs WHERE Id = 2
```

Emulator:

```text
columns: Id INT64, s JSON
["2","{\"snippets\":[{\"highlights\":[{\"end_position\":9,\"start_position\":5}],\"snippet\":\"the lazy dog\"}]}"]
```

Spanner:

```text
columns: Id INT64, s JSON
["2","{\"snippets\":[{\"highlights\":[{\"begin\":5,\"end\":9}],\"snippet\":\"the lazy dog\",\"source_begin\":32,\"source_end\":44}]}"]
```

### `sql/search_functions/snippet_null_and_empty` (line 501)

```sql
SELECT Id, SNIPPET(Body, 'x') AS s FROM Docs WHERE Id IN (4, 5)
```

Emulator:

```text
columns: Id INT64, s JSON
["4","{\"snippets\":[{\"highlights\":[],\"snippet\":\"\"}]}"]
["5",null]
```

Spanner:

```text
columns: Id INT64, s JSON
["4","{\"snippets\":[]}"]
["5",null]
```

### `sql/search_functions/snippet_bad_width` (line 509)

```sql
SELECT SNIPPET('abc', 'abc', max_snippet_width => 0) AS s
```

Emulator:

```text
error: INVALID_ARGUMENT
message: SNIPPET has out of range value for max_snippet_length argument.
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SNIPPET has out of range value for max_snippet_width argument.
```

### `sql/search_functions/snippet_html_content_type` (line 523)

```sql
SELECT SNIPPET('<b>Spanner</b> rocks', 'spanner', content_type => 'text/html') AS s
```

Emulator:

```text
columns: s JSON
["{\"snippets\":[{\"highlights\":[{\"end_position\":11,\"start_position\":4}],\"snippet\":\"<b>Spanner</b> rocks\"}]}"]
```

Spanner:

```text
columns: s JSON
["{\"snippets\":[{\"highlights\":[{\"begin\":1,\"end\":8}],\"snippet\":\"Spanner rocks\",\"source_begin\":4,\"source_end\":21}]}"]
```

### `sql/search_functions/force_search_index_rw` (line 531)

```sql
SELECT Id FROM Docs@{FORCE_INDEX=DocsSearch} WHERE Flag = true
```

Emulator:

```text
error: INVALID_ARGUMENT
message: The index DocsSearch cannot be used because it is a SEARCH index type which is not supported for transactional queries by default
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The search index DocsSearch cannot be used in transactional queries by default.
```

### `sql/search_granite/quoted_norwegian_letters` (line 108)

```sql
SELECT id FROM granite_no WHERE SEARCH(name_tokens, @q)
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Error(s) parsing search query `"Bjørn Åse Ærlig AS"`, has error(s): -Syntax error on column 6: Expecting '"
```

Spanner:

```text
columns: id STRING
["no-6"]
```

### `sql/search_granite/weighted_relevance_score` (line 161)

```sql
SELECT id,
  (
    COALESCE(SCORE(identifiers_tokens, @trimmedQuery ) * 10.0, 0) +
    COALESCE(SCORE(name_tokens,        @query ) * 5.0, 0) +
    COALESCE(SCORE(aliases_tokens,     @query ) * 2.0, 0) +
    case when lower(entity_kind) = 'company' then 1.0 else 0.0 end +
    LN(coalesce(num_connections, 0) + 1) * 1.5
    + 1
  ) AS relevance_score
FROM granite_no AS t
WHERE (SEARCH(name_tokens, @quotedQuery) OR SEARCH(aliases_tokens, @quotedQuery) OR SEARCH(identifiers_tokens, @trimmedQuery))
ORDER BY relevance_score DESC, id
LIMIT 100
```

Emulator:

```text
columns: id STRING, relevance_score FLOAT64
["no-3",19.079441541679834]
```

Spanner:

```text
columns: id STRING, relevance_score FLOAT64
["no-3",20.410344118126368]
```

### `sql/search_granite/score_raw_query_counts_terms` (line 184)

```sql
SELECT id, SCORE(name_tokens, @query) AS s, SCORE(aliases_tokens, @query) AS a FROM granite_no ORDER BY id
```

Emulator:

```text
columns: id STRING, s FLOAT64, a FLOAT64
["no-1",1.0,0.0]
["no-2",1.0,0.0]
["no-3",3.0,0.0]
["no-4",0.0,0.0]
["no-5",1.0,0.0]
["no-6",1.0,0.0]
["no-7",0.0,0.0]
["no-8",0.0,0.0]
["no-9",0.0,0.0]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: ESCORER_SCORE: This function may only be used with a compatible SEARCH predicate
```

### `sql/search_granite/score_null_source` (line 201)

```sql
SELECT id, SCORE(name_tokens, 'ghost') AS s, SCORE(aliases_tokens, 'ghost') AS a FROM granite_no WHERE id = 'no-9'
```

Emulator:

```text
columns: id STRING, s FLOAT64, a FLOAT64
["no-9",0.0,1.0]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: ESCORER_SCORE: This function may only be used with a compatible SEARCH predicate
```

### `sql/search_granite/score_on_null_array` (line 208)

```sql
SELECT id, SCORE(aliases_tokens, 'nordlys') AS a FROM granite_no WHERE id IN ('no-5', 'no-7')
```

Emulator:

```text
columns: id STRING, a FLOAT64
["no-5",0.0]
["no-7",0.0]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: ESCORER_SCORE: This function may only be used with a compatible SEARCH predicate
```

### `sql/search_granite/search_null_array_column` (line 216)

```sql
SELECT id, SEARCH(aliases_tokens, 'nordlys') AS hit FROM granite_no WHERE id IN ('no-5', 'no-7', 'no-3')
```

Emulator:

```text
columns: id STRING, hit BOOL
["no-3",false]
["no-5",null]
["no-7",null]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The function SEARCH can only be used in a WHERE clause.
```

### `sql/search_granite/all_matches_group_by_max` (line 249)

```sql
WITH id_match AS (
  SELECT id, 1000000.0 AS relevance_score FROM granite_no AS t WHERE id = @query
),
full_text_match AS (
  SELECT id, (COALESCE(SCORE(name_tokens, @query) * 5.0, 0) + 1) AS relevance_score
  FROM granite_no AS t
  WHERE (SEARCH(name_tokens, @quotedQuery) OR SEARCH(aliases_tokens, @quotedQuery))
  ORDER BY relevance_score DESC
  LIMIT 100
),
all_matches AS (
  SELECT id, MAX(relevance_score) AS relevance_score FROM (
    SELECT id, relevance_score FROM id_match
    UNION ALL SELECT id, relevance_score FROM full_text_match
  )
  GROUP BY id
)
SELECT am.id, am.relevance_score, t.name
FROM all_matches am
INNER JOIN granite_no t USING (id)
ORDER BY relevance_score DESC, id
LIMIT @limit
```

Emulator:

```text
columns: id STRING, relevance_score FLOAT64, name STRING
["no-1",6.0,"Fjordkraft Testselskap AS"]
```

Spanner:

```text
columns: id STRING, relevance_score FLOAT64, name STRING
["no-1",7.294458508491516,"Fjordkraft Testselskap AS"]
```

### `sql/search_granite/ngram_score_order` (line 336)

```sql
SELECT id,
  (COALESCE(SCORE_NGRAMS(name_ngram_tokens, @ngramQuery) * 8.0, 0) +
   case when lower(entity_kind) = 'company' then 1.0 else 0.0 end +
   coalesce(num_connections, 0) * 0.01) AS relevance_score
FROM granite_no AS t
WHERE SEARCH_NGRAMS(name_ngram_tokens, @ngramQuery, min_ngrams_percent => 40.0)
ORDER BY relevance_score DESC, id
LIMIT 100
```

Emulator:

```text
columns: id STRING, relevance_score FLOAT64
["no-3",3.53]
["no-5",3.5]
```

Spanner:

```text
columns: id STRING, relevance_score FLOAT64
["no-3",3.8299999999999996]
["no-5",3.8]
```

### `sql/search_granite/ngram_score_values` (line 353)

```sql
SELECT id, SCORE_NGRAMS(name_ngram_tokens, @q) AS s FROM granite_no WHERE id IN ('no-1', 'no-3', 'no-6', 'no-9')
```

Emulator:

```text
columns: id STRING, s FLOAT64
["no-9",0.0]
["no-3",0.7647058823529411]
["no-1",0.027777777777777776]
["no-6",0.0]
```

Spanner:

```text
columns: id STRING, s FLOAT64
["no-1",0.023809523809523808]
["no-3",0.7142857142857143]
["no-6",0.0]
["no-9",0.0]
```

### `sql/search_media/matching_mentions_accented_text_fails` (line 102)

```sql
SELECT id FROM news_events_entities nee WHERE SEARCH(text__tokens, @entityText)
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Error(s) parsing search query `"BJØRN ÅSE ÆRLIG" OR "BJØRN ASE ÆRLIG"`, has error(s): -Syntax error on column 6: Expecting '"
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_media/paged_events_by_mentions` (line 111)

```sql
WITH matched AS (
  SELECT DISTINCT id
  FROM news_events_entities nee
  WHERE SEARCH(text__tokens, @entityText)
    AND negative_ams_weight IS NOT NULL
    AND entity_type IN UNNEST(@entityTypes)
),
page AS (
  SELECT ne.id, ne.published_at
  FROM matched INNER JOIN @{JOIN_METHOD=APPLY_JOIN} news_events ne USING (id)
  WHERE NOT ne.archived IS TRUE
    AND ne.language IN UNNEST(@languages)
  ORDER BY ne.published_at DESC, ne.id
  LIMIT @limit
  OFFSET @offset
),
page_entities AS (
  SELECT id, ARRAY_AGG(nee.entity_id ORDER BY nee.entity_id) AS entities
  FROM news_events_entities nee
  WHERE id IN (SELECT id FROM page)
    AND SEARCH(text__tokens, @entityText)
    AND negative_ams_weight IS NOT NULL
    AND entity_type IN UNNEST(@entityTypes)
  GROUP BY id
),
final AS (
  SELECT ne.id, ne.published_at, ne.publisher, page_entities.entities AS entities
  FROM page_entities INNER JOIN @{JOIN_METHOD=APPLY_JOIN} news_events ne USING (id)
)
SELECT final.id, final.publisher, final.entities FROM final
ORDER BY final.published_at DESC, final.id
```

Emulator:

```text
columns: id STRING, publisher STRING, entities ARRAY<STRING>
["e4","Testbladet",["x1"]]
["e2","Test Times",["x1"]]
["e1","Testavisa",["x1","x2"]]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is only supported in queries with query optimizer version 6 or above.
```

### `sql/search_persons/name_hyphen_replaced_by_space` (line 57)

```sql
SELECT p.person_id FROM persons p WHERE SEARCH(p.name_tokens, @nameQuery)
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Error(s) parsing search query `Nordmann Hansen, Kari`, has error(s): -Syntax error on column 18: Expecting 'AROUND(
```

Spanner:

```text
columns: person_id STRING
["p2"]
```

### `sql/search_persons/name_norwegian_letters_fail_to_parse` (line 93)

```sql
SELECT p.person_id FROM persons p WHERE SEARCH(p.name_tokens, @nameQuery)
```

Emulator:

```text
columns: person_id STRING
```

Spanner:

```text
columns: person_id STRING
["p3"]
```

### `sql/search_persons/name_double_space_rejected` (line 101)

```sql
SELECT p.person_id FROM persons p WHERE SEARCH(p.name_tokens, @nameQuery)
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Error(s) parsing search query `Ola Testmann`, has error(s): -Syntax error on column 6: Expecting 'AROUND(
```

Spanner:

```text
columns: person_id STRING
["p1"]
["p7"]
```

### `sql/search_persons/name_null_rows_and_null_query` (line 189)

```sql
SELECT p.person_id, SEARCH(p.name_tokens, @nameQuery) AS hit FROM persons p WHERE p.person_id IN ('p1', 'p6')
```

Emulator:

```text
columns: person_id STRING, hit BOOL
["p6",null]
["p1",null]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The function SEARCH can only be used in a WHERE clause.
```

### `sql/search_persons/name_in_read_write_transaction` (line 200)

```sql
SELECT p.person_id FROM persons p WHERE SEARCH(p.name_tokens, 'ola')
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Invalid use of search related function: SQL Search functions are not supported for transactional queries by default
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH cannot be used in transactional queries by default.
```

### `sql/search_persons/dml_delete_with_search_in_read_write` (line 243)

```sql
DELETE FROM persons p WHERE SEARCH(p.name_tokens, 'nordmann')
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Invalid use of search related function: SQL Search functions are not supported for transactional queries by default
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH cannot be used in transactional queries by default.
```

