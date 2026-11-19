# Cloud Spanner vs emulator

6 steps disagree.

### `mutations/filtered_index/unique_duplicate_indexed_rows` (line 24)

```sql
[1, 10, "x"]
[2, 10, "y"]
```

Emulator:

```text
error: ALREADY_EXISTS
message: UNIQUE violation on index UByA, duplicate key: {Int64(10)} in this transaction.
```

Spanner:

```text
error: ALREADY_EXISTS
message: UNIQUE violation on index UByA. Row [2] in table U duplicates key [10] created in this transaction.
```

### `mutations/filtered_index/unique_backfill_duplicate` (line 54)

```sql
CREATE UNIQUE INDEX UByA ON U(A) STORING (B) WHERE B IS NOT NULL
```

Emulator:

```text
error: FAILED_PRECONDITION
message: Found uniqueness violation on index UByA, duplicate key: {Int64(10)}
```

Spanner:

```text
error: FAILED_PRECONDITION
message: Found uniqueness violation: [10,1] [10,2].
```

### `mutations/filtered_index/foreign_key_ignores_filtered_index` (line 69)

```sql
[1]
```

Emulator:

```text
error: FAILED_PRECONDITION
message: Foreign key `FkP` constraint violation when deleting or updating referenced key `{Int64(1)}` in table `P`. A row with that key exists in the referencing table `C`.
```

Spanner:

```text
error: FAILED_PRECONDITION
message: Foreign key constraint violation when deleting or updating referenced row(s): referencing row(s) found in table `C`.
```

### `read/filtered_index/query_force_index_no_filter` (line 52)

```sql
SELECT K, A, B FROM T@{FORCE_INDEX=TByAB}
```

Emulator:

```text
columns: K INT64, A INT64, B STRING
["2","20",null]
["4",null,null]
["1","10","x"]
["3",null,"z"]
["5","10",null]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The index TByAB cannot be used because it filters nulls that are required to answer the query for column B.
```

### `read/filtered_index/query_force_index_check_disabled` (line 67)

```sql
SELECT K, A, B FROM T@{FORCE_INDEX=TByAB, spanner_emulator.disable_query_null_filtered_index_check=true}
```

Emulator:

```text
columns: K INT64, A INT64, B STRING
["2","20",null]
["4",null,null]
["1","10","x"]
["3",null,"z"]
["5","10",null]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The index TByAB cannot be used because it filters nulls that are required to answer the query for column B.
```

### `sql/search_media/paged_events_by_mentions` (line 118)

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

