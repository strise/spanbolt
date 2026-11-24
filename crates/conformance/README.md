# spanbolt-conformance

Differential conformance harness. Cases live as plain text under `conformance/cases/**/*.case`.
Expectations are **recorded from Google's official emulator** (`record`), then any endpoint speaking
the Spanner gRPC API is checked against them (`check`).

## Commands

```bash
# 1. Start the oracle (official emulator) and record expectations into the case files.
docker rm -f sb-conf-oracle; docker run -d --name sb-conf-oracle -p 19210:9010 gcr.io/cloud-spanner-emulator/emulator:1.5.59
cargo run --release -p spanbolt-conformance -- record --target 127.0.0.1:19210
docker rm -f sb-conf-oracle

# 2. Start spanbolt and check it against the recorded expectations.
cargo run --release -p spanbolt-server -- --host-port 127.0.0.1:19220 &
cargo run --release -p spanbolt-conformance -- check --target 127.0.0.1:19220 \
  --junit target/conformance/junit.xml --json target/conformance/summary.json

# Useful flags (both modes)
#   --cases <dir>           case root (default: conformance/cases)
#   --filter <substr>       only cases whose name contains <substr> (repeatable)
#   --tag <tag>             only cases with this tag (repeatable)
#   --jobs <n>              parallel cases (default 8)
# check only
#   --unimplemented fail    count UNIMPLEMENTED as a failure (default: skip; always reported separately)
#   -v                      print every diff and every UNIMPLEMENTED case, not just the first 40 failures
```

### Real Cloud Spanner (`--oracle spanner`)

```bash
cargo run --release -p spanbolt-conformance -- record --oracle spanner \
  --spanner-project <project> --spanner-instance <instance> \
  --tag search --jobs 6 --report target/conformance/spanner-vs-emulator.md
cargo run --release -p spanbolt-conformance -- check --oracle spanner --target 127.0.0.1:19220
cargo run --release -p spanbolt-conformance -- diff-oracles     # offline: where Spanner and the emulator disagree
```

`record --oracle spanner` connects to `spanner.googleapis.com` with the application-default
credentials (`gcloud auth application-default print-access-token`, refreshed every 20 minutes). It
never creates instances: each case gets a database `spanbolt-oracle-<yyyymmdd>-<n>` in the given
instance, dropped when the case ends (also when its schema or prelude fails). Every create and drop
is appended to `target/conformance/spanner-oracle-dbs.log` (`--db-log`), which also picks the next
`<n>`; check it for `drop-failed` lines after a run. The databases need
`spanner.databases.create`, `drop`, `updateDdl`, `read`, `write` and `sessions.create` on the
instance.

Results go into `expect@spanner:` blocks right after the step's `expect:` block. The body is `same`
when Spanner matched the emulator expectation, otherwise Spanner's own expectation in the usual
format. `record` (emulator) and `record --oracle spanner` each rewrite only their own blocks.
`check --oracle spanner` uses `expect@spanner:` where present and `expect:` elsewhere; the default
`check` ignores the Spanner blocks.

Run the commands from the workspace root. `record` rewrites only the `expect:` blocks of the cases it
ran, so `record --filter <name>` re-records one case. Review the diff before committing: an expectation
is the oracle's answer, not necessarily Spanner's.

`check` exits non-zero when any case fails (and, with `--unimplemented fail`, when any step returned
`UNIMPLEMENTED`). The summary prints pass / fail / unimplemented / error per tag.

Each case runs in a fresh database (`CreateDatabase` with the case schema, then the file prelude).
On spanbolt the harness builds one template database per distinct (schema, prelude) and gives each
case an O(1) copy via `Control.CloneDatabase`; on targets without `Control` it falls back to creating
a database per case.

## Case file format

Line based. A **directive** starts at column 0; its **body** is the following lines indented by two
spaces (blank lines inside a body are kept). `#` at column 0 is a comment.

```text
# File-level defaults apply to every case in the file.
tags: sql join
schema:
  CREATE TABLE Singers (
    SingerId INT64 NOT NULL,
    Name STRING(MAX),
  ) PRIMARY KEY (SingerId);
  CREATE INDEX SingersByName ON Singers(Name)
# Steps before the first `case` are the prelude: run in every case, must succeed.
insert Singers cols=SingerId,Name:
  [1, "Marc"]
  [2, "Catalina"]

case select_by_name
query:
  SELECT SingerId FROM Singers WHERE Name = @name
params:
  @name STRING = "Marc"
expect:
  columns: SingerId INT64
  ["1"]
```

### Case directives

| Directive | Meaning |
|---|---|
| `case NAME` | Starts a case. Full name is `<file path without .case>/NAME`. |
| `tags: a b` | Extra tags (file-level tags are inherited; the top directory under `cases/` is always a tag). |
| `schema:` | DDL statements (separated by `;` at line end) passed to `CreateDatabase`. A case's own `schema:` replaces the file's. |
| `skip: reason` | Do not run (reported as skipped). |

### Steps

Steps take `key=value` options in the header. `txn=NAME` runs the step in a transaction opened by
`begin txn=NAME`; without it, `query`/`read` use a single-use strong read-only transaction, and
`dml`/`batch`/mutations run in their own read-write transaction that is committed immediately.

| Step | Body | Notes |
|---|---|---|
| `query [ordered\|unordered] [txn=] [bound=] [session=S]:` | SQL | Rows compared in order only if the SQL has a top-level `ORDER BY` (override with `ordered`/`unordered`). `session=S` runs it in a session saved by `create_session`. |
| `dml [txn=]:` | SQL | Expects `count:` and, with `THEN RETURN`, rows. |
| `pdml:` | SQL | Partitioned DML; expects `count:` (lower bound). |
| `batch [txn=]:` | statements, `;` at line end | `ExecuteBatchDml`; expects `counts:` and optionally `error:`. |
| `insert\|update\|insert_or_update\|replace TABLE cols=A,B [txn=] [save=TS]:` | one JSON array per row | Mutations. |
| `delete TABLE [txn=] [save=TS]:` | key set lines | Delete mutation. |
| `read TABLE cols=A,B [index=I] [limit=N] [txn=] [bound=]:` | key set lines | Read API. Rows always compared in order. |
| `begin [txn=NAME] [ro] [bound=]` | – | `BeginTransaction` (read-write unless `ro`). |
| `commit [txn=NAME] [save=TS]` | – | Commits buffered mutations. |
| `rollback [txn=NAME]` | – | |
| `ddl:` | DDL statements | `UpdateDatabaseDdl`, waits for the operation. |
| `get_ddl` | – | `GetDatabaseDdl`; expects `statements:`. |
| `list_sessions [create=K] [page_size=N]` | – | Creates K sessions, then pages through `ListSessions`; expects `counts:` (sessions per page). |
| `create_session session=S [multiplexed]` | – | `CreateSession`, saving the name under `S`. |
| `get_session session=S [wait=D]` | – | Sleeps `D`, then `GetSession`; expects `counts: [1]` when `approximate_last_use_time` is at least 0.5 s after `create_time`, else `[0]`. |
| `session_last_use session=S [wait=D]` | – | Same, read from `ListSessions` (not a use); `[-1]` when `S` is not listed. |
| `delete_session session=S` / `name=TEXT` | – | `DeleteSession` on a saved session or a literal name (`<db>` is the case database). |
| `params:` | `@name TYPE = <json>` per line | Attaches to the preceding `query`/`dml`/`pdml`. |
| `expect:` | see below | Attaches to the preceding step. Written by `record`. |

`save=TS` stores the commit timestamp under `TS`; `bound=` accepts `strong`,
`read_timestamp:TS`, `min_read_timestamp:TS`, `exact_staleness:10s`, `max_staleness:10s`
(durations: `ms`, `s`, `m`, `h`).

**Key set lines:** `all`; a key as a JSON array `[1, "a"]`; or a range
`range [1] .. [5]` (closed..closed), `[1] ..< [5]` (closed..open), `[1] <.. [5]` (open..closed),
`[1] <..< [5]` (open..open).

**Values** (rows, keys, params) are JSON: integers become INT64 (sent as strings), numbers with a
`.`/exponent become FLOAT64, `"NaN"`/`"Infinity"`/`"-Infinity"` are strings (as on the wire); NUMERIC,
DATE, TIMESTAMP, BYTES (base64) and JSON are strings; a JSON object becomes its JSON text; arrays are
arrays; STRUCT params are arrays of field values. Param types: `INT64`, `FLOAT32`, `FLOAT64`, `BOOL`,
`STRING`, `BYTES`, `NUMERIC`, `DATE`, `TIMESTAMP`, `JSON`, `ARRAY<T>`, `STRUCT<a INT64, b STRING>`.

### Expectations

```text
expect:
  ok                                  # step succeeded, no output
  error: INVALID_ARGUMENT             # gRPC code name
  message: Table not found: Foo       # normalised message; or `message: ignore`, or `message~: <regex>`
  columns: K INT64, V ARRAY<STRING>   # result type (column name + type; anonymous columns are type only)
  ["1", ["a", null]]                  # one row per line, values in Spanner's JSON wire form
  count: 3                            # DML row count (PDML: lower bound)
  counts: [1, 1]                      # batch DML per-statement counts
  statements:                         # get_ddl; one JSON string per following line
  "CREATE TABLE ..."
```

Comparison rules:

- Rows are a multiset unless the statement has a top-level `ORDER BY` (or the step says `ordered`).
  Read API results are ordered.
- Floats compare by exact bits; NaN equals NaN.
- `"<now>"` matches any TIMESTAMP/DATE (recorded automatically for values within 10 minutes of the
  recording clock, e.g. `CURRENT_TIMESTAMP()` and commit timestamps); `"<any>"` matches any value.
  `record` keeps hand-written placeholders and `message: ignore` / `message~:` rules.
- Messages are compared after normalisation: the emulator's `goo.gle/debugonly` markers and the
  statement echo after `[at L:C]` are removed, and whitespace is collapsed. A step can set
  `message: ignore`, or `message~: <regex>` for messages that carry volatile values.
- A step without `expect:` must succeed (setup).

## Corpus

`conformance/cases/<area>/*.case`; the top directory is always a tag.

| Area | What it covers |
|---|---|
| `values` | Mutation + Read round trips for every type: INT64/FLOAT32/FLOAT64 edges, NaN/±Inf, -0.0, NUMERIC precision and scale, timestamps with nanoseconds, BYTES base64, JSON normalisation, arrays with NULLs, and the parse errors. |
| `ddl` | `UpdateDatabaseDdl` validation errors, `GetDatabaseDdl` canonical printing, schema changes against existing data. |
| `mutations` | NOT NULL, duplicate keys, missing interleave parents, cascade / NO ACTION deletes, unique indexes, foreign keys, CHECK, generated columns, defaults, mutation order within a commit, key ordering. |
| `read` | Read API key sets, ranges with every open/closed combination, index reads (NULL_FILTERED, DESC, STORING), limits. |
| `txn` | Visibility of buffered mutations, read-your-writes DML, rollback, read-only transactions, stale reads with every timestamp bound. |
| `sql` | Query shapes taken from real application code, against small synthetic schemas: joins, aggregates, UNNEST/arrays, subqueries, CTEs, set operations, expressions, strings, dates, JSON, hashing, hints and NULL_FILTERED errors, DML, batch and partitioned DML, INFORMATION_SCHEMA, SEARCH. |

Cases marked `skip:` say why. The concurrent read-write transaction cases are skipped because the
emulator only runs one read-write transaction at a time and aborts the other.
