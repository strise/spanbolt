# Cloud Spanner vs emulator

33 steps disagree.

### `sql/prod_fidelity/order_asc_nulls_first_default` (line 264)

```sql
SELECT Id, Val FROM Items ORDER BY Val ASC NULLS FIRST, Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: NULLS FIRST and NULLS LAST are not supported [at 1:44]
```

Spanner:

```text
columns: Id INT64, Val INT64
["4",null]
["5","5"]
["6","7"]
["1","10"]
["2","20"]
["3","20"]
```

### `sql/prod_fidelity/order_desc_nulls_last_default` (line 272)

```sql
SELECT Id, Val FROM Items ORDER BY Val DESC NULLS LAST, Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: NULLS FIRST and NULLS LAST are not supported [at 1:45]
```

Spanner:

```text
columns: Id INT64, Val INT64
["2","20"]
["3","20"]
["1","10"]
["6","7"]
["5","5"]
["4",null]
```

### `sql/prod_fidelity/order_nulls_first_no_direction` (line 280)

```sql
SELECT Id, Val FROM Items ORDER BY Val NULLS FIRST, Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: NULLS FIRST and NULLS LAST are not supported [at 1:40]
```

Spanner:

```text
columns: Id INT64, Val INT64
["4",null]
["5","5"]
["6","7"]
["1","10"]
["2","20"]
["3","20"]
```

### `sql/prod_fidelity/array_agg_order_desc_nulls_last` (line 288)

```sql
SELECT ARRAY_AGG(Id ORDER BY Val DESC NULLS LAST, Id) AS ids FROM Items
```

Emulator:

```text
error: INVALID_ARGUMENT
message: NULLS FIRST and NULLS LAST are not supported [at 1:39]
```

Spanner:

```text
columns: ids ARRAY<INT64>
[["2","3","1","6","5","4"]]
```

### `sql/prod_fidelity/window_single_rank` (line 296)

```sql
SELECT Id, RANK() OVER (ORDER BY Val DESC) AS r FROM Items WHERE Val IS NOT NULL ORDER BY Id
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Analytic functions not supported [at 1:12]
```

Spanner:

```text
error: UNIMPLEMENTED
message: Unsupported built-in function: RANK.
```

### `sql/search_functions/partition_filter_score` (line 780)

```sql
SELECT Id, SCORE(Body_Tokens, 'spanner cloud') AS s FROM Docs WHERE Flag = true AND SEARCH(Body_Tokens, 'spanner cloud') ORDER BY Id
```

Emulator:

```text
columns: Id INT64, s FLOAT64
["1",3.0]
```

Spanner:

```text
columns: Id INT64, s FLOAT64
["1",2.9443299770355225]
```

### `sql/search_granite/score_with_search_predicate` (line 472)

```sql
SELECT id, SCORE(name_tokens, @query) AS s FROM granite_no WHERE SEARCH(name_tokens, @query) ORDER BY id
```

Emulator:

```text
columns: id STRING, s FLOAT64
["no-3",3.0]
```

Spanner:

```text
columns: id STRING, s FLOAT64
["no-3",3.2661805152893066]
```

### `sql/search_granite/score_single_term_with_search_predicate` (line 482)

```sql
SELECT id, SCORE(name_tokens, 'nordlys') AS s FROM granite_no WHERE SEARCH(name_tokens, 'nordlys') ORDER BY id
```

Emulator:

```text
columns: id STRING, s FLOAT64
["no-3",1.0]
["no-5",1.0]
```

Spanner:

```text
columns: id STRING, s FLOAT64
["no-3",1.2588917016983032]
["no-5",1.2588917016983032]
```

### `sql/search_granite/score_or_query_two_columns` (line 491)

```sql
SELECT id, SCORE(name_tokens, @query) AS s, SCORE(aliases_tokens, @query) AS a FROM granite_no WHERE SEARCH(name_tokens, @query) OR SEARCH(aliases_tokens, @query) ORDER BY id
```

Emulator:

```text
columns: id STRING, s FLOAT64, a FLOAT64
["no-1",1.0,1.0]
["no-3",1.0,0.0]
["no-5",1.0,0.0]
```

Spanner:

```text
columns: id STRING, s FLOAT64, a FLOAT64
["no-1",1.2704493999481201,1.2704493999481201]
["no-3",1.2704493999481201,0.0]
["no-5",1.2704493999481201,0.0]
```

### `sql/search_media/paged_events_optimizer_latest` (line 355)

```sql
@{OPTIMIZER_VERSION=latest_version}
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
error: INVALID_ARGUMENT
message: Unsupported hint: OPTIMIZER_VERSION.
```

Spanner:

```text
columns: id STRING, publisher STRING, entities ARRAY<STRING>
["e4","Testbladet",["x1"]]
["e2","Test Times",["x1"]]
["e1","Testavisa",["x1","x2"]]
```

### `sql/search_media/paged_events_optimizer_default` (line 401)

```sql
@{OPTIMIZER_VERSION=default_version}
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
error: INVALID_ARGUMENT
message: Unsupported hint: OPTIMIZER_VERSION.
```

Spanner:

```text
columns: id STRING, publisher STRING, entities ARRAY<STRING>
["e4","Testbladet",["x1"]]
["e2","Test Times",["x1"]]
["e1","Testavisa",["x1","x2"]]
```

### `sql/search_media/paged_events_optimizer_7` (line 447)

```sql
@{OPTIMIZER_VERSION=7}
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
error: INVALID_ARGUMENT
message: Unsupported hint: OPTIMIZER_VERSION.
```

Spanner:

```text
columns: id STRING, publisher STRING, entities ARRAY<STRING>
["e4","Testbladet",["x1"]]
["e2","Test Times",["x1"]]
["e1","Testavisa",["x1","x2"]]
```

### `sql/search_media/paged_events_without_join_hints` (line 492)

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
  FROM matched INNER JOIN news_events ne USING (id)
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
  FROM page_entities INNER JOIN news_events ne USING (id)
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

### `sql/search_media/matching_mentions_accented_with_partition_filter` (line 580)

```sql
SELECT DISTINCT id FROM news_events_entities nee
WHERE SEARCH(text__tokens, @entityText)
  AND negative_ams_weight IS NOT NULL
  AND entity_type IN UNNEST(@entityTypes)
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Error(s) parsing search query `"BJØRN ÅSE ÆRLIG" OR "BJØRN ASE ÆRLIG" OR "BJORN ASE AERLIG"`, has error(s): -Syntax error on column 6: Expecting '"
```

Spanner:

```text
columns: id STRING
["e4"]
```

### `sql/search_media/matching_mentions_without_partition_filter` (line 595)

```sql
SELECT DISTINCT id FROM news_events_entities nee
WHERE SEARCH(text__tokens, @entityText)
  AND negative_ams_weight IS NOT NULL
```

Emulator:

```text
columns: id STRING
["e2"]
["e1"]
["e4"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_media/matching_mentions_without_where_filter` (line 610)

```sql
SELECT DISTINCT id FROM news_events_entities nee
WHERE SEARCH(text__tokens, @entityText)
  AND entity_type IN UNNEST(@entityTypes)
```

Emulator:

```text
columns: id STRING
["e2"]
["e1"]
["e4"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: SEARCH is not supported in this query. Possible reasons are: There is no appropriate search index to use, or the function is used in unsupported query shapes. You can get more details on why a specific search index cannot be used by adding @{force_index} hint to the query.
```

### `sql/search_unpartitioned/double_negation` (line 60)

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
columns: Id INT64
```

### `sql/search_unpartitioned/around_with_not_is_internal` (line 88)

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
message: Invalid use of negation found in search query. Negation may not be used within AROUND.
```

### `sql/search_unpartitioned/around_needs_number` (line 95)

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
columns: Id INT64
```

### `sql/search_unpartitioned/around_missing_space` (line 102)

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
columns: Id INT64
["2"]
```

### `sql/search_unpartitioned/around_unclosed` (line 109)

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
columns: Id INT64
["2"]
```

### `sql/search_unpartitioned/parentheses_rejected` (line 116)

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
columns: Id INT64
["1"]
```

### `sql/search_unpartitioned/lowercase_or_is_a_term` (line 131)

```sql
SELECT Id FROM Docs WHERE SEARCH(Body_Tokens, 'fox or dog')
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

### `sql/search_unpartitioned/score_counts_repeats` (line 193)

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
message: ESCORER_SCORE: This function may only be used with a compatible SEARCH predicate
```

### `sql/search_unpartitioned/score_words_phrase` (line 204)

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
message: ESCORER_SCORE: This function may only be used with a compatible SEARCH predicate
```

### `sql/search_unpartitioned/search_substring_unsupported_type` (line 249)

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
message: The referenced substring TOKENLIST is not tokenized with `word_suffix` support.
```

### `sql/search_unpartitioned/search_substring_invalid_type` (line 256)

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
message: Invalid relative_search_type: 'middle'. Valid relative_search_type values: ['value_prefix', 'value_suffix', 'word_prefix', 'word_suffix', 'phrase']
```

### `sql/search_unpartitioned/where_search_substring_prefix_multiword` (line 349)

```sql
SELECT Id FROM Docs WHERE SEARCH_SUBSTRING(Body_Substr, 'lazy dog', relative_search_type => 'word_prefix') ORDER BY Id
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

### `sql/search_unpartitioned/where_search_ngrams_code` (line 355)

```sql
SELECT Id FROM Docs WHERE SEARCH_NGRAMS(Code_Ngrams, 'abc') ORDER BY Id
```

Emulator:

```text
columns: Id INT64
["1"]
["3"]
```

Spanner:

```text
columns: Id INT64
["3"]
```

### `sql/search_unpartitioned/score_with_search_predicate` (line 379)

```sql
SELECT Id, SCORE(Body_Tokens, 'spanner cloud') AS s FROM Docs WHERE SEARCH(Body_Tokens, 'spanner cloud') ORDER BY Id
```

Emulator:

```text
columns: Id INT64, s FLOAT64
["1",3.0]
```

Spanner:

```text
columns: Id INT64, s FLOAT64
["1",2.9443299770355225]
```

### `sql/search_unpartitioned/score_or_query_with_search_predicate` (line 386)

```sql
SELECT Id, SCORE(Body_Tokens, 'spanner OR cloud') AS s FROM Docs WHERE SEARCH(Body_Tokens, 'spanner OR cloud') ORDER BY Id
```

Emulator:

```text
columns: Id INT64, s FLOAT64
["1",3.0]
["3",2.0]
```

Spanner:

```text
columns: Id INT64, s FLOAT64
["1",2.9443299770355225]
["3",1.673880696296692]
```

### `sql/search_unpartitioned/score_repeats_single_term` (line 394)

```sql
SELECT Id, SCORE(Body_Tokens, 'spanner') AS s FROM Docs WHERE SEARCH(Body_Tokens, 'spanner') ORDER BY Id
```

Emulator:

```text
columns: Id INT64, s FLOAT64
["1",1.0]
["3",2.0]
```

Spanner:

```text
columns: Id INT64, s FLOAT64
["1",1.2588917016983032]
["3",1.6450823545455933]
```

### `sql/search_unpartitioned/score_ngrams_with_search_ngrams` (line 402)

```sql
SELECT Id, SCORE_NGRAMS(Body_Substr, 'emulatr') AS s FROM Docs WHERE SEARCH_NGRAMS(Body_Substr, 'emulatr') ORDER BY Id
```

Emulator:

```text
columns: Id INT64, s FLOAT64
["1",0.10256410256410256]
["3",0.10810810810810811]
```

Spanner:

```text
columns: Id INT64, s FLOAT64
["1",0.11904761904761904]
["3",0.13157894736842105]
```

