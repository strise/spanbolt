# Beam SpannerIO smoke test

A small Maven project that runs Apache Beam pipelines (DirectRunner, Beam 2.73.0)
through `SpannerIO` against one endpoint and writes one JSON line per step. `scripts/beam-smoke.sh`
runs it against this worktree's spanbolt and against the official emulator (1.5.59, in Docker)
and compares the two runs with `scripts/beam_smoke_compare.py`: every field but the timing must
match.

```bash
scripts/lock.sh heavy -- scripts/beam-smoke.sh
SPANBOLT_ONLY=1 scripts/beam-smoke.sh      # spanbolt only, no Docker, no comparison
```

Needs JDK 21 (`/usr/libexec/java_home -v 21`, or set `JAVA_HOME_21`) and Maven. Output and logs go
to `.mcp-output/beam/`.

What it covers (the patterns typical batch jobs use):

| Step | SpannerIO usage |
|---|---|
| `write.default.customers` | `write()` with the defaults (schema lookup through INFORMATION_SCHEMA) |
| `write.changefacts.*` | `write()` with `ComputeChangeFacts.writeChangeLog`'s settings (`withLowPriority`, `withMaxCommitDelay`, `withBatchSizeBytes`, `withMaxNumRows`, `withMaxNumMutations`, `withGroupingFactor`, `REPORT_FAILURES`) into an interleaved table with three secondary indexes, then a mix of updates, replaces, deletes and a range delete |
| `read.query.*.{unbatched,batched}` | `read().withQuery(...)` with batching off (`BatchReadOnlyTransaction` not used) and on (`PartitionQuery` + partition tokens), including `TO_JSON(table)` and query parameters |
| `read.table.*.{unbatched,batched}` | `read().withTable().withColumns()`, with `withIndex` over all keys, key prefixes and a range, and a primary-key range (`PartitionRead` when batched) |
| `write.grouped.report_failures` | `WriteGrouped` with `BqToSpannerSync`'s settings and four failing groups (missing parent, duplicate insert, missing NOT NULL column, unique index violation): the batch fails, SpannerIO retries group by group and reports only the failing groups |
| `error.*` | the same failing groups through the client, for the error code and message |
| `state.*` | the tables and every index read back with the client, after the writes and at the end |
