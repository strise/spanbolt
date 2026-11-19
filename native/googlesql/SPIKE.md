# Spike: GoogleSQL analyser via FFI

Verdict: **go.** GoogleSQL 2026.11.2 builds unpatched on macOS arm64. A 500-line shim gives Rust a
resolved AST for every probe statement in about 0.1–0.2 ms, and a catalog of 524 tables builds in
36 ms. The costs are a 25-minute cold C++ build (memory-hungry on Linux) and a 58 MB library.

## Build recipe

Prerequisites: `bazelisk` (it picks Bazel 9.3.2 from `.bazelversion`), Xcode CLT clang, and
`protoc` on PATH (used by `googlesql-sys/build.rs`).

```sh
cd native/googlesql
bazelisk build //:libgooglesql_shim.so      # -c opt and the shared disk cache come from .bazelrc
cd ../..
cargo test --release -p googlesql-sys -- --nocapture
cargo test --release -p googlesql-sys --test analyze -- --ignored --nocapture bench
cargo run --release -p googlesql-sys --example dump -- 'SELECT 1'
```

- `MODULE.bazel` depends on `googlesql` through `archive_override` (release tarball plus integrity
  hash), which is how cloud-spanner-emulator consumes it. No googlesql sources are vendored.
- Two settings have to be repeated at the root because Bazel ignores them in non-root modules:
  googlesql's `grpc` `single_version_override` (a sed patch), and a default Python 3.11 toolchain.
  googlesql's codegen pip wheels are pinned to cp311.
- On macOS, `--macos_minimum_os=11.0 --host_macos_minimum_os=11.0` is required. Without it,
  `rules_cc_autoconf` fails with "'path' is unavailable: introduced in macOS 10.15". googlesql's own
  `.bazelrc` sets this under `build:darwin`, a config that never fires (the platform config is
  `macos`).
- On Linux, gcc 13 (Ubuntu 24.04) needs `-Wno-changes-meaning` because `property_graph.h` trips
  the diagnostic. The Docker build uses clang 18, which needs less memory.
- No source patches were needed.
- `cargo test --workspace` now needs the Bazel output. For real use, gate `googlesql-sys` behind a
  feature, or ship a prebuilt library per target and point `GOOGLESQL_SHIM_DIR` at it.

## Numbers (M-series, 12 cores, release)

| What | Result |
|---|---|
| Clean build of `//:libgooglesql_shim.so` (fetch + ~4,000 actions) | 1,492 s (~25 min); critical path 540 s |
| Incremental rebuild after editing one shim file | 3.7 s |
| Library size | 69 MB unstripped, 58 MB `strip -x`, 21 MB gzipped. `-dead_strip` does not help because googlesql uses `alwayslink` throughout |
| Catalog of 524 tables (the fixture's 262 tables ×2), built-ins included | 36 ms |
| First analysis on a fresh catalog | 2.5 ms |
| Warm analysis, p50 / p99 over 10 statements | 115 µs / 234 µs (range 69–213 µs p50) |
| Throughput with 1 / 2 / 4 / 8 threads | 7.2k / 11.8k / 20.2k / 33.4k analyses/s |
| Linux arm64 Docker build (clang 18, 16 GB VM) | ~35 min of compile split over three runs that shared a cache (8, 4, then 2 jobs; the first two died of OOM); 75 MB `.so` depending only on libc, libm, libstdc++ and libgcc_s |

Latency covers analysis, proto serialization and the copy to Rust. Analysis results are cached per
(schema version, SQL text), so analysis is not on the hot path.

## Probe statements (`crates/googlesql-sys/tests/analyze.rs`)

All thirteen analyse with the emulator's `LanguageOptions` plus `FEATURE_ANALYTIC_FUNCTIONS`,
`FEATURE_QUALIFY` and `FEATURE_NULLS_FIRST_LAST_IN_ORDER_BY`. With the emulator's options exactly,
three are rejected, the same way the official emulator (GoogleSQL dialect) rejects them:

- `ROW_NUMBER() OVER (...)` → "Analytic functions not supported". The emulator never enables
  `FEATURE_ANALYTIC_FUNCTIONS`.
- `QUALIFY` → "QUALIFY is not supported"
- `ORDER BY ... NULLS LAST` → "NULLS FIRST and NULLS LAST are not supported". The emulator enables
  it only for PostgreSQL.

Before choosing between these feature sets, check real Spanner. The fidelity target is prod, not
the emulator, so if real Spanner accepts these, spanbolt should enable the features.
`extra_features` in the catalog JSON switches them on.

The other cases: `@{FORCE_INDEX=...}` arrives as a `hint_list` `ResolvedOption` on the
`ResolvedTableScan`, and Spanner's index validation stays our job. Also covered: `IN UNNEST(@ids)`,
`ARRAY(SELECT AS STRUCT ...)`, `JSON_VALUE`/`JSON_QUERY`, `WITH`, `INSERT/UPDATE ... THEN RETURN`,
`DELETE`, `SEARCH`/`SCORE`/`SNIPPET` on a `TOKENLIST` column, `IS DISTINCT FROM`, and
`PENDING_COMMIT_TIMESTAMP()`. Named schemas (`monitoring.alerts`) are handled as nested catalogs.

## Spanner-specific functions

`TOKENLIST` is a GoogleSQL built-in type (`FEATURE_TOKENIZED_SEARCH`). The search functions are not
built in. The emulator registers them as ordinary `googlesql::Function`s with evaluators:
`backend/query/search/search_function_catalog.cc` (903 lines) and `PENDING_COMMIT_TIMESTAMP` /
`BIT_REVERSE` in `backend/query/function_catalog.cc`. For analysis only the signatures matter, so
`shim/spanner_functions.cc` copies them (~150 lines, without evaluators). Rust evaluates them.

Not yet ported:

- The emulator's post-analysis checks: `query_validator.cc` (929 lines; hint values, where PCT may
  appear, unsupported functions), `index_hint_validator.cc` (207), and
  `feature_filter/gsql_supported_functions.cc`. The last one is an allowlist of the GoogleSQL
  built-ins Spanner supports. Without it, BigQuery-only functions analyse fine.
- `GET_NEXT_SEQUENCE_VALUE`, ML, graph and PostgreSQL functions.

All of these can be ported as data plus a Rust visitor over the proto.

## Reuse of the emulator's C++

Linking the emulator's catalog code directly is not worth it. `function_catalog.cc` includes
`backend/schema/*`, `third_party/spanner_pg/*` (the PostgreSQL translator), ML, feature flags and
`common/errors`. The emulator's `googlesql.patch` (534 lines) widens googlesql's visibility to make
that work. Instead, copy the signatures and the allowlists (Apache-2.0, with attribution) and port
the validators to Rust over the resolved AST.

## `reference_impl` as a fallback executor

The official emulator already executes everything this way: `query_engine.cc` uses
`googlesql::PreparedQuery` / `PreparedModify`. API surface (`public/evaluator_base.h`):

- `PreparedQuery(const ResolvedQueryStmt*, EvaluatorOptions)`, then `Prepare` and
  `ExecuteAfterPrepare(params)`, which returns an `EvaluatorTableIterator`.
- `PreparedModify` covers DML and returns a modify iterator plus a `THEN RETURN` iterator.
- Data comes in through `Table::CreateEvaluatorTableIterator(column_idxs)`. Every table becomes a
  full scan that the evaluator filters, which is why the emulator is slow.

A fallback would need several pieces:

- The C++ `AnalyzerOutput` kept alive behind a handle, rather than bytes only.
- A C++ `Table` subclass that pulls rows from Rust over FFI.
- A two-way `Value` conversion.
- Whole-statement granularity. Rust operators cannot be mixed inside the reference evaluator's
  operator tree.

That is roughly 80k lines of reference code on the request path, with emulator-like performance.
Recommendation: don't build it. Goal 7 already says "UNIMPLEMENTED, never a silently different
answer". If a fallback is ever needed, add it per statement behind a flag.

## Linux

`Dockerfile` (context `native/googlesql`) builds on Ubuntu 24.04 with clang 18 and the system
toolchain. googlesql's hermetic LLVM toolchain is a dev-dependency, so it is not inherited. The
build writes `/out/libgooglesql_shim.so` and the generated `.proto` files to a scratch image, so it
can be a build stage of the spanbolt image (`COPY --from=googlesql /out ...`).

```sh
cd native/googlesql
DOCKER_BUILDKIT=1 docker build --platform linux/amd64 -t spanbolt-googlesql .   # or linux/arm64
docker create --name x spanbolt-googlesql true && docker cp x:/out ./out-linux && docker rm x
```

Measured on linux/arm64 under Docker Desktop (12 vCPU, 16 GB, no swap):

- **Memory is the constraint, not CPU.** The generated `resolved_ast_rewrite_visitor_*.cc` and
  `parser/tm_parser.cc` each need 5 GB or more at `-O2`. gcc 13 OOMed at 6 jobs, and clang 18 OOMed
  at 8 and at 4. Only `--jobs=2` completed, so the Dockerfile defaults to `BAZEL_JOBS=2`. Rule of
  thumb: jobs ≈ RAM / 6 GB.
- gcc 13 also needs `-Wno-changes-meaning`, now in `.bazelrc` under `build:linux`. Clang is the
  Docker default because it uses less memory.
- About 35 minutes of compile in total, spread over the three runs (349 s, 608 s and 1,110 s). The
  last run alone, at 2 jobs, was 18.5 minutes for the remaining ~25% of actions.
- I have not loaded the Linux `.so` from Rust yet. The code is identical, and `ldd` shows only libc,
  libm, libstdc++ and libgcc_s.
- linux/amd64 was not built: emulated on this Mac it would take hours. Estimate on a native amd64
  CI runner with 32 GB (about 5 jobs): 30–45 minutes cold, about 1 minute when only the shim
  changed and the cache is warm.

CI plan: never build this in the per-PR path. Build it once per googlesql version, either as a
release job that pushes `ghcr.io/.../spanbolt-googlesql:<googlesql-version>-<arch>` or as a GitHub
release asset, using a 32 GB runner and a BuildKit or remote Bazel cache. The spanbolt image and
`cargo test` then pull the prebuilt library through `GOOGLESQL_SHIM_DIR`.

## Recommendation for spanbolt's SQL layer

1. `googlesql-sys`: this shim, kept small. It builds the catalog from `spanbolt-ddl`'s `Schema`
   (passed as JSON or FlatBuffers; not a hot path) and returns the proto. Add a handle API later
   only if the reference evaluator is ever wanted.
2. `spanbolt-sql::analyze`: an LRU keyed by (schema version, SQL), holding the decoded resolved AST.
   Spanner validators run here as Rust visitors: the function allowlist, hint and index validation,
   PCT rules, and NULL_FILTERED strict mode.
3. `spanbolt-sql::plan`: lowers the resolved AST into Rust's own plan IR. It picks index or key
   ranges from `TableScan` hints and filters, and returns UNIMPLEMENTED for any node kind not lowered
   yet.
4. `spanbolt-sql::exec`: the Rust executor over `imbl` maps. Spanner functions (SEARCH, SCORE,
   SNIPPET, TOKENIZE_*) are implemented in Rust.

Distribution: build the library in a dedicated Docker stage per googlesql version, and pin both the
googlesql version and `MODULE.bazel.lock`. Bump deliberately, because GoogleSQL makes no API
stability guarantees.
