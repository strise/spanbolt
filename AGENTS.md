# spanbolt

spanbolt is a Cloud Spanner emulator in Rust: gRPC and REST APIs, with SQL analysed by GoogleSQL.
Read DESIGN.md first. It is the contract for architecture, layout and rules. CONTRIBUTING.md and
docs/building.md cover setup.

- Checks: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
  Before handing in a change run `scripts/gate.sh`: fmt, clippy (with and without the SQL engine),
  tests, license checks and the conformance suite against the recorded baselines, in both server
  modes. `GATE_EXTRA='<command>'` adds a project-specific check at the end.
- Commits: conventional format `type(scope): description`.
- Never run destructive git (reset, checkout --, restore, stash, clean).
- Native dependency: `googlesql-sys` (the GoogleSQL analyser shim) and `spanbolt-sql` (the SQL
  engine) are outside `default-members`, so `cargo test`/`cargo build` work without the native
  library. The server takes the engine behind the cargo feature `googlesql` (off by default; without
  it every SQL RPC is UNIMPLEMENTED). No Bazel needed: `googlesql-sys/build.rs` downloads the
  prebuilt shim pinned in `native/googlesql-shim.env` (`SPANBOLT_SHIM_URL` points it at a local
  mirror). `source scripts/googlesql-env.sh` first to use a locally built shim instead. Then e.g.
  `cargo test --release -p spanbolt-sql`,
  `cargo build --release -p spanbolt-server --features googlesql` (the release binary),
  `cargo clippy --workspace --all-targets --features spanbolt-server/googlesql -- -D warnings`.
  The binary finds `libgooglesql_shim` through its rpath (the build directory, then the
  executable's directory: ship the library next to `spanbolt`). It needs zoneinfo for
  America/Los_Angeles, Spanner's default time zone (install tzdata in images).
- Changing the shim (`native/googlesql/shim`): `cd native/googlesql && bazelisk build //:libgooglesql_shim.so`
  (seconds with a warm disk cache). Other worktrees use it once it is packaged and unpacked into
  the prefix `scripts/googlesql-env.sh` reads (`docs/building.md`); a prefix from other shim sources is skipped. Full build notes: `docs/building.md`, `native/googlesql/SPIKE.md`.
- SQL engine layout (`crates/sql`): `catalog` (GoogleSQL catalog per schema + plan cache; GoogleSQL tables and analysed views are shared across schema versions, builtin functions live in one shim-wide parent catalog),
  `analyze`/`validate` (analyser call, hint names, supported-function allowlist), `plan` (resolved
  AST to operators, key/index range pushdown), `access`/`ops`/`agg` (operators), `func*` (scalar
  functions), `dml`, `info_schema`, `schema_expr` (DEFAULT/generated/CHECK for storage). Strict mode
  follows production Spanner: FORCE_INDEX on a NULL_FILTERED index needs `IS NOT NULL` filters on the
  indexed columns; `--emulator-compat` honours `spanner_emulator.disable_query_null_filtered_index_check`.
- Benchmarks: `cargo run --release -p spanbolt-server --features googlesql --example sql_bench`
  (tonic, in process), `--example search_bench` (SEARCH over 10k rows with and without a search index), `--example session_bench` (heap per session, BatchCreateSessions and ListSessions over 1M sessions; no SQL engine needed), `-p spanbolt-sql --example ddl_bench` (`DDL=<schema dump>`: DDL apply and first SQL after a DDL, replaying a large schema migration) and `python3 scripts/sql_bench.py HOST:PORT` (Python client, any endpoint).
  Soak: `--example soak -- --duration 3600` (conformance in a loop plus creates/clones/drops and a
  mixed workload; CSV of RSS, footprint, live heap, p50/p99 and cache sizes), summarised by
  `python3 scripts/soak_report.py CSV --svg out.svg`. Soak runs on mimalloc like the binary. On macOS
  judge memory by `rust_heap_mb` (live bytes in mimalloc), `malloc_mb` (the shim's C++ heap in the
  system malloc zones) and `footprint_mb`: RSS includes the shim library and freed pages the OS
  hasn't reclaimed. Runs before 2026-11-24 used the system allocator and have one `heap_mb` for both. Runs in `docs/soak/`. Server memory on large streams: `python3 scripts/stream_rss.py HOST:PORT SERVER_PID [n ...]`.
- Threads (`crates/server/src/offload.rs`): every gRPC and REST call runs its handler on tokio's blocking pool, because handlers do engine work synchronously; on the runtime workers a few long queries would stall every other call and the HTTP/2 handshake of new connections. Check with `--example connect_storm -- HOST:PORT` (40 new clients connecting while 24 others run long queries).
- Streaming (`crates/server/src/stream.rs`, `wire.rs`): `PartialWriter` encodes values into wire bytes as rows arrive; `wire::SpannerRouter` serves ExecuteStreamingSql/StreamingRead with a pass-through codec and hands every other Spanner method to the generated server.
- Full-text search (`crates/sql/src/search`): a port of the emulator's tokenizers, rquery parser and evaluators (TOKENLISTs are emulator-format token lists). A `SEARCH` conjunct over a TOKENIZE_FULLTEXT column in a search index scans candidate rows from an inverted index cached per table version and updated from the row map's diff.
- Conformance: start spanbolt (`cargo run --release -p spanbolt-server --features googlesql -- --host-port 127.0.0.1:19220`), then `cargo run --release -p spanbolt-conformance -- check --target 127.0.0.1:19220`; recording from the official emulator is in `crates/conformance/README.md`. Never edit recorded
  expectations in `conformance/cases` to make spanbolt pass: add cases, recorded from the official
  emulator or from Cloud Spanner.
