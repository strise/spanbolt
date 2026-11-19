# Cloud Spanner vs emulator

17 steps disagree.

### `read/keysets/no_columns_in_key_too_long` (line 220)

```sql
[1, "a", "extra"]
```

Emulator:

```text
error: FAILED_PRECONDITION
message: Wrong number of key parts for T. Expected 2 key parts, found 3 key parts. Supplied key: values { string_value: "1" } values { string_value: "a" } values { string_value: "extra" }
```

Spanner:

```text
error: FAILED_PRECONDITION
message: Wrong number of key parts for T. Expected: 2. Got: ["1","a","extra"]
```

### `read/keysets/key_wrong_type` (line 227)

```sql
["one", "a"]
```

Emulator:

```text
error: FAILED_PRECONDITION
message: Could not parse one as an integer
```

Spanner:

```text
error: FAILED_PRECONDITION
message: Invalid value for column A in table T: Expected INT64.
```

### `read/keysets/index_full_key_point` (line 270)

```sql
[10, 2, "a"]
```

Emulator:

```text
error: FAILED_PRECONDITION
message: Wrong number of key parts for TByV. Expected 1 key parts, found 3 key parts. Supplied key: values { string_value: "10" } values { string_value: "2" } values { string_value: "a" }
```

Spanner:

```text
error: FAILED_PRECONDITION
message: Wrong number of key parts for TByV. Expected: 1. Got: ["10","2","a"]
```

### `read/keysets/index_non_covered_column` (line 308)

```sql
all
```

Emulator:

```text
error: NOT_FOUND
message: Index 'TByV' which indexes 'T' does not have a column named W. Reads with an index can only query columns present in the index (see https://cloud.google.com/spanner/docs/secondary-indexes#read-with-index).
```

Spanner:

```text
error: UNIMPLEMENTED
message: Reading non-indexed columns using an index is not supported. Consider adding W to the index using a STORING clause.
```

### `read/keysets/index_composite_prefix` (line 332)

```sql
[null]
```

Emulator:

```text
error: FAILED_PRECONDITION
message: Wrong number of key parts for TByWV. Expected 2 key parts, found 1 key parts. Supplied key: values { null_value: NULL_VALUE }
```

Spanner:

```text
error: FAILED_PRECONDITION
message: Wrong number of key parts for TByWV. Expected: 2. Got: [null]
```

### `read/keysets/index_unknown` (line 347)

```sql
all
```

Emulator:

```text
error: NOT_FOUND
message: No index 'Nope' was found for table 'T'.
```

Spanner:

```text
error: NOT_FOUND
message: Index not found on table T: Nope
```

### `read/keysets/index_wrong_table` (line 354)

```sql
[1]
["x"]
```

Emulator:

```text
error: FAILED_PRECONDITION
message: Could not parse x as an integer
```

Spanner:

```text
error: FAILED_PRECONDITION
message: Invalid value for column V in table _Index_TByV: Expected INT64.
```

### `sql/aggregates/array_agg_struct` (line 106)

```sql
SELECT EntityId, ARRAY_AGG(STRUCT(SignalType, Confidence)) AS signals FROM Signals GROUP BY EntityId
```

Emulator:

```text
columns: EntityId STRING, signals ARRAY<STRUCT<SignalType STRING, Confidence FLOAT64>>
["e3",[["news",0.6]]]
["e1",[["news",0.4],["news",0.8],["sanction",1.0]]]
["e5",[["pep",0.9]]]
```

Spanner:

```text
columns: EntityId STRING, signals ARRAY<STRUCT<SignalType STRING, Confidence FLOAT64>>
["e1",[["news",0.8],["news",0.4],["sanction",1.0]]]
["e3",[["news",0.6]]]
["e5",[["pep",0.9]]]
```

### `sql/engine/window_function_rejected` (line 212)

```sql
SELECT ROW_NUMBER() OVER (ORDER BY SingerId) FROM Singers
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Analytic functions not supported [at 1:8]
```

Spanner:

```text
error: UNIMPLEMENTED
message: Unsupported built-in function: ROW_NUMBER.
```

### `sql/hints/force_index_unknown` (line 90)

```sql
SELECT EntityId FROM Entities@{FORCE_INDEX=Nope}
```

Emulator:

```text
error: INVALID_ARGUMENT
message: The table does not have a secondary index called Nope
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The table Entities does not have an index called Nope
```

### `sql/hints/null_filtered_without_filter` (line 106)

```sql
SELECT EntityId, Status FROM Entities@{FORCE_INDEX=EntitiesByStatus}
```

Emulator:

```text
error: INVALID_ARGUMENT
message: The emulator is not able to determine whether the null filtered index EntitiesByStatus can be used to answer this query as it may filter out nulls that may be required to answer the query. Please test this query against Cloud Spanner. If you confirm against Cloud Spanner that the null filtered index can be used to answer the query, set the hint @{spanner_emulator.disable_query_null_filtered_index_check=true} on the table, or the query statement to bypass this check in the emulator. This hint will be ignored by the production Cloud Spanner service and the emulator will accept the query and return a valid result when it is run with the check disabled.
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The index EntitiesByStatus cannot be used because it filters nulls that are required to answer the query for column Status.
```

### `sql/hints/null_filtered_with_is_not_null` (line 115)

```sql
SELECT Status, Name FROM Entities@{FORCE_INDEX=EntitiesByStatus} WHERE Status IS NOT NULL
```

Emulator:

```text
error: INVALID_ARGUMENT
message: The emulator is not able to determine whether the null filtered index EntitiesByStatus can be used to answer this query as it may filter out nulls that may be required to answer the query. Please test this query against Cloud Spanner. If you confirm against Cloud Spanner that the null filtered index can be used to answer the query, set the hint @{spanner_emulator.disable_query_null_filtered_index_check=true} on the table, or the query statement to bypass this check in the emulator. This hint will be ignored by the production Cloud Spanner service and the emulator will accept the query and return a valid result when it is run with the check disabled.
```

Spanner:

```text
columns: Status STRING, Name STRING
["active","Acme AS"]
["active","Kari Nordmann"]
["active","Acme AS"]
["archived","Beta Holding"]
["pending","Ola Nordmann"]
```

### `sql/hints/null_filtered_equality` (line 124)

```sql
SELECT Status, Name FROM Entities@{FORCE_INDEX=EntitiesByStatus} WHERE Status = 'active'
```

Emulator:

```text
error: INVALID_ARGUMENT
message: The emulator is not able to determine whether the null filtered index EntitiesByStatus can be used to answer this query as it may filter out nulls that may be required to answer the query. Please test this query against Cloud Spanner. If you confirm against Cloud Spanner that the null filtered index can be used to answer the query, set the hint @{spanner_emulator.disable_query_null_filtered_index_check=true} on the table, or the query statement to bypass this check in the emulator. This hint will be ignored by the production Cloud Spanner service and the emulator will accept the query and return a valid result when it is run with the check disabled.
```

Spanner:

```text
columns: Status STRING, Name STRING
["active","Acme AS"]
["active","Kari Nordmann"]
["active","Acme AS"]
```

### `sql/hints/null_filtered_check_disabled` (line 133)

```sql
SELECT Status, Name FROM Entities@{FORCE_INDEX=EntitiesByStatus, spanner_emulator.disable_query_null_filtered_index_check=true}
```

Emulator:

```text
columns: Status STRING, Name STRING
["active","Kari Nordmann"]
[null,"Delta ASA"]
["pending","Ola Nordmann"]
["active","Acme AS"]
["archived","Beta Holding"]
["active","Acme AS"]
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The index EntitiesByStatus cannot be used because it filters nulls that are required to answer the query for column Status.
```

### `sql/hints/null_filtered_in_unnest` (line 147)

```sql
SELECT Name FROM Entities@{FORCE_INDEX=EntitiesByStatus} WHERE Status IN UNNEST(@s)
```

Emulator:

```text
error: INVALID_ARGUMENT
message: The emulator is not able to determine whether the null filtered index EntitiesByStatus can be used to answer this query as it may filter out nulls that may be required to answer the query. Please test this query against Cloud Spanner. If you confirm against Cloud Spanner that the null filtered index can be used to answer the query, set the hint @{spanner_emulator.disable_query_null_filtered_index_check=true} on the table, or the query statement to bypass this check in the emulator. This hint will be ignored by the production Cloud Spanner service and the emulator will accept the query and return a valid result when it is run with the check disabled.
```

Spanner:

```text
error: INVALID_ARGUMENT
message: The index EntitiesByStatus cannot be used because it filters nulls that are required to answer the query for column Status.
```

### `sql/hints/null_filtered_in_unnest_is_not_null` (line 158)

```sql
SELECT Name FROM Entities@{FORCE_INDEX=EntitiesByStatus} WHERE Status IS NOT NULL AND Status IN UNNEST(@s)
```

Emulator:

```text
error: INVALID_ARGUMENT
message: The emulator is not able to determine whether the null filtered index EntitiesByStatus can be used to answer this query as it may filter out nulls that may be required to answer the query. Please test this query against Cloud Spanner. If you confirm against Cloud Spanner that the null filtered index can be used to answer the query, set the hint @{spanner_emulator.disable_query_null_filtered_index_check=true} on the table, or the query statement to bypass this check in the emulator. This hint will be ignored by the production Cloud Spanner service and the emulator will accept the query and return a valid result when it is run with the check disabled.
```

Spanner:

```text
columns: Name STRING
["Acme AS"]
["Kari Nordmann"]
["Acme AS"]
["Ola Nordmann"]
```

### `sql/hints/statement_hint_optimizer` (line 169)

```sql
@{OPTIMIZER_VERSION=6} SELECT COUNT(*) AS n FROM Entities
```

Emulator:

```text
error: INVALID_ARGUMENT
message: Unsupported hint: OPTIMIZER_VERSION.
```

Spanner:

```text
columns: n INT64
["6"]
```

