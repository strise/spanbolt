# spanbolt

A fast drop-in replacement for the [Cloud Spanner emulator](https://github.com/GoogleCloudPlatform/cloud-spanner-emulator),
for tests and CI. Site: [strise.github.io/spanbolt](https://strise.github.io/spanbolt/).

spanbolt speaks the same gRPC API on port 9010 and the same REST gateway on 9020. Point
`SPANNER_EMULATOR_HOST` at it and the client libraries work unchanged. Tested with the Java and
Python clients, the JDBC driver, Liquibase, Beam's `SpannerIO` and Testcontainers'
`SpannerEmulatorContainer`.

## Why

The official emulator is correct but slow for large test suites. It rebuilds its catalog for every
statement, scans whole tables where Spanner would use a key or an index, and runs one read-write
transaction per database at a time. spanbolt keeps Google's own SQL analyser (GoogleSQL), so names,
types and errors resolve the way Spanner resolves them, and replaces the rest:

| | Official emulator 1.5.59 | spanbolt |
|---|---|---|
| Startup | ~1 s | ~5 ms |
| Point query on a 100k-row table (gRPC) | 88–122 ms (full scan) | ~70 µs (key lookup) |
| Single-row commit (gRPC) | ~4 ms | ~60 µs |
| 8 concurrent writers on different rows | ~14 transactions/s (serialised) | ~27,000 commits/s |
| Create a database with a 530-statement schema | 8.5 s | 13 ms |

Measured on an Apple M-series laptop, server side, release build. On a real application's test
suite of about 3,700 tests the end-to-end gain was 5–7x, because the tests' own work stays.

It also adds what the emulator lacks:

- **O(1) database clones** (copy-on-write), so each test or test class can start from a fresh copy
  of a seeded template.
- **Snapshots:** save every instance and database to a file and load it at startup. A pre-seeded
  Docker image is the binary plus that file.
- **Real concurrency:** optimistic transactions; non-conflicting transactions never abort.
- **Test controls:** a settable clock and deterministic `ABORTED` injection.
- **Production behaviour by default.** Where the emulator and production Spanner disagree and we
  have recorded production's answer (full-text search, pipe syntax, `NULL_FILTERED` indexes, window
  functions, filtered indexes, reading a field of a returned `ARRAY<STRUCT>` subquery, returning an
  `ARRAY<STRUCT>` that can be NULL or is built with `[STRUCT(...)]`, INFORMATION_SCHEMA's views and
  columns, a DDL batch that fails midway keeping the statements before the failing one), spanbolt
  follows production. `--emulator-compat` restores the emulator's behaviour.

The extra features live in a small `spanbolt.v1.Control` gRPC service
(`crates/proto/proto/spanbolt/v1/control.proto`).

## Status

Early, and used in earnest by one team: a large Spanner application's full test suite (Liquibase
migrations, JDBC, the Java client, Beam pipelines) passes on it. Expect gaps outside what that
suite and the conformance cases exercise.

- Anything spanbolt doesn't support fails with `UNIMPLEMENTED`, not a silently different answer.
- Spanner Graph: `CREATE [OR REPLACE] PROPERTY GRAPH`, `DROP PROPERTY GRAPH`,
  `INFORMATION_SCHEMA.PROPERTY_GRAPHS`, and GQL (`GRAPH g MATCH ...`, `GRAPH_TABLE`) with
  `WHERE`/`FILTER`, `RETURN`, `NEXT`, `FOR`, `OPTIONAL MATCH`, bounded quantified paths and
  `LABELS`. Path variables, `ANY`/`SHORTEST` search, `SIMPLE` paths and graph elements in results
  or `TO_JSON` are not supported.
- PostgreSQL-dialect databases are not supported: `CreateDatabase` with
  `database_dialect = POSTGRESQL` returns `UNIMPLEMENTED`, so PGAdapter doesn't work. The
  emulator has had them since 1.5.12.
- Not supported: `APPROX_*` distance functions with `options`, backups, IAM policies, the
  emulator's own command-line flags (see below for what to use instead), change stream reads (`READ_<stream>`), sequence values
  (`GET_NEXT_SEQUENCE_VALUE`; sequences themselves are stored), `ListDatabaseRoles`, and these
  DDL statements: queues, placements, user-defined functions, vector indexes and expression
  index keys.
- Proto bundles (`CREATE/ALTER/DROP PROTO BUNDLE`) and PROTO and ENUM columns: DDL, mutations,
  the Read API, `INFORMATION_SCHEMA`, and SQL (field access, `NEW`, `REPLACE_FIELDS`, casts to and
  from STRING, BYTES and INT64, `TO_JSON`, DML and `UPDATE ... SET col.field`). Not supported:
  `UPDATE` of a field deeper than one level or of a NULL proto, and STRUCT fields in
  `REPLACE_FIELDS`.
- Locality groups are validated and stored (DDL, `INFORMATION_SCHEMA.LOCALITY_GROUP_OPTIONS`) but
  don't change where or how data is stored.
- Unnamed CHECK and FOREIGN KEY constraints get the emulator's generated names in both modes.
  Cloud Spanner's names end in a different hash that we couldn't reproduce, so errors and
  `INFORMATION_SCHEMA` that show those names differ from production (#146).
- Deeply nested queries fail with `RESOURCE_EXHAUSTED` ("Out of stack space ..."), as Cloud
  Spanner's do, but at fixed depths where Spanner's and the emulator's depend on their stack:
  types nested more than 1200 STRUCT and ARRAY levels (2000 with `--emulator-compat`; Spanner stops
  between 1100 and 1300, the emulator answers about 5000), and expressions more than 500 levels
  (Spanner stops a chain of STRUCT field accesses between 400 and 500) (#160, #162).
  `--emulator-compat` keeps the 500 too, where the emulator answers 5000 STRUCT constructors under
  5000 field accesses and stops before 6000. GoogleSQL's serialized AST, which spanbolt decodes,
  repeats each node's whole type, so N nested constructors make N²/2 levels of type: 1000 of them
  took 1.5 GB to analyse, 5000 over 4 GB (#172).
- `SCORE` with `enhance_query => true` scores like plain `SCORE`. Cloud Spanner's enhanced score
  comes from Google's query expansion model: two made-up words in the same number of rows score
  differently, and a word scores the same whatever its document frequency, so it can't be computed
  from the data (#160).
- Real Spanner takes locks where spanbolt aborts and lets the client retry. Both are outcomes the
  client libraries already handle, but the timing differs.
- Tested on macOS (arm64) and in the linux/arm64 image. CI smoke-tests the linux/amd64 image; the
  full suite has not run on it.

## Quick start

### Docker

```sh
docker run --rm -p 9010:9010 -p 9020:9020 ghcr.io/strise/spanbolt:0.2
export SPANNER_EMULATOR_HOST=localhost:9010
```

The image is published for linux/amd64 and linux/arm64: `:X.Y.Z` and `:X.Y` for releases, `:main`
for the latest commit on main. `docker build -t spanbolt .` builds it locally (the first build
compiles GoogleSQL unless you pass `--build-arg SHIM=prebuilt`).

With Testcontainers, mark the image as a substitute for the emulator (`SpannerEmulatorContainer`
checks the name):

```java
var image = DockerImageName.parse("ghcr.io/strise/spanbolt:0.2")
    .asCompatibleSubstituteFor("gcr.io/cloud-spanner-emulator/emulator");
var spanner = new SpannerEmulatorContainer(image);
```

Suites that take the image name from a property instead can tag it under the emulator's name:
`docker tag ghcr.io/strise/spanbolt:0.2 gcr.io/cloud-spanner-emulator/emulator:spanbolt`.

`docs/docker.md` covers multi-arch builds, a prebuilt GoogleSQL layer and pre-seeded images.

### Cargo

```sh
cargo install spanbolt
spanbolt --host-port 127.0.0.1:9010 --rest-port 9020
```

The SQL engine links GoogleSQL's analyser through a native library, which the build downloads
(pinned by SHA-256, needs `curl` and `tar`) for macOS arm64 and Linux arm64/x86_64 (glibc 2.38+,
e.g. Ubuntu 24.04, Debian 13). The binary loads it from that download cache
(`~/.cache/spanbolt/prebuilt`): run `cargo install spanbolt --force` again if you clear the cache.
On other platforms use the image, or build the library from source (`docs/building.md`).

From a checkout:

```sh
cargo build --release -p spanbolt          # downloads the GoogleSQL shim the first time
./target/release/spanbolt --host-port 127.0.0.1:9010 --rest-port 9020
```

`cargo build` and `cargo test` in the workspace leave the SQL engine out and need no native
library. `docs/building.md` covers the shim and building it with Bazel.

Useful flags: `--snapshot <file>` loads a snapshot at startup, `spanbolt snapshot <path>` asks a
running server to write one (snapshots from 0.2.1 and earlier hold NULL in non-stored generated
columns: see `docs/docker.md`), and `--emulator-compat` follows the emulator instead of production.
`--database projects/P/instances/I/databases/D` (repeatable, or comma-separated in
`SPANBOLT_DATABASES`) creates an empty database, and its instance, at startup, so a docker-compose
service or CI job can connect without creating them first:

```sh
docker run --rm -p 9010:9010 -e SPANBOLT_DATABASES=projects/test/instances/test/databases/app \
  ghcr.io/strise/spanbolt:0.2
```

## Coming from the emulator

spanbolt doesn't take the emulator's flags. What each one maps to:

| Emulator (`gateway_main` / `emulator_main`) | spanbolt |
|---|---|
| `--hostname`, `--grpc_port`, `--host_port` | `--host-port HOST:PORT` (default `0.0.0.0:9010`) |
| `--http_port` | `--rest-port PORT` (off unless set; the image sets 9020) |
| `--disable_query_null_filtered_index_check` | `--emulator-compat`, which also honours the `spanner_emulator.disable_query_null_filtered_index_check` hint. Without it a `NULL_FILTERED` index needs `IS NOT NULL` filters on its columns, as in production. |
| `--enable_fault_injection`, `--abort_current_transaction_probability` | `Control.InjectAbort`: the next `n` commits fail with `ABORTED`, deterministically instead of at random. |
| `--log_requests` | `SPANBOLT_TIMING=<path>`: one JSON line per call with its timing (not the messages). |
| `--override_max_databases_per_instance`, `MAX_DATABASES_PER_INSTANCE` | Not needed: spanbolt has no limit. |
| `--override_change_stream_partition_token_alive_seconds`, `--enable_change_stream_churning` | Not needed: change streams are stored but can't be read. |
| `--copy_emulator_stdout`, `--copy_emulator_stderr` | Not needed: one process, logs go to stderr. |

`--emulator-compat` is more than the null-filtered check: it follows the emulator wherever it and
production disagree and we have recorded both (error messages, full-text search details,
INFORMATION_SCHEMA views). Use it if your tests assert on the emulator's exact answers.

### Row order

A table or index scan without `ORDER BY` returns rows in key order, because tables and indexes are
ordered maps and nothing shuffles them (joins and aggregates make no such promise). That is what production does in practice for
simple scans, so tests that wrongly depend on it pass on spanbolt and on Spanner alike. The
emulator shuffles unordered results on purpose to flush those tests out
(GoogleCloudPlatform/cloud-spanner-emulator#14). We decided to keep key order: following
production matters more here, and the order is not a promise either way. Use `ORDER BY` when the
order matters.

## How we check correctness

`conformance/cases` holds about 3,650 cases: schema, mutations, reads, queries and API calls, with
expected results recorded from the official emulator 1.5.59 and, where they differ, from
production Spanner. `crates/conformance` replays them against any endpoint and compares results
and error codes:

```sh
cargo run --release -p spanbolt-conformance -- check --target 127.0.0.1:9010
```

`conformance/baseline.json` and `baseline-prod.json` record each case's status in each mode;
`scripts/gate.sh` fails when a passing case stops passing. Recording new cases: `crates/conformance/README.md`.

## Layout

```
crates/proto          Google API protos (vendored) and the Control service
crates/storage        values, key encoding, persistent tables and indexes, snapshots
crates/txn            transactions and MVCC versions
crates/ddl            DDL parser, schema model, GetDatabaseDdl printer
crates/sql            planner, executor, functions, full-text search, INFORMATION_SCHEMA
crates/googlesql-sys  Rust bindings to the GoogleSQL shim
crates/server         gRPC and REST server (library)
crates/spanbolt       the `spanbolt` binary (SQL engine on)
crates/conformance    the conformance harness
native/googlesql      the C++ shim around GoogleSQL
```

`DESIGN.md` has the architecture.

## Contributing

Issues and pull requests are welcome. Both must use the repository's templates: open issues from
the bug report, unsupported feature or question form, and fill in the pull request template. See
`CONTRIBUTING.md`.

## License

Apache-2.0. See `LICENSE`, `NOTICE` and `THIRD_PARTY_NOTICES.md`. Parts of the full-text search and
query validation are ports of the Cloud Spanner emulator's code (Apache-2.0).

spanbolt is not affiliated with or endorsed by Google.
