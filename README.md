# spanbolt

A fast drop-in replacement for the [Cloud Spanner emulator](https://github.com/GoogleCloudPlatform/cloud-spanner-emulator),
for tests and CI.

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
  have recorded production's answer (full-text search, `NULL_FILTERED` indexes, window functions,
  filtered indexes), spanbolt follows production. `--emulator-compat` restores the emulator's
  behaviour.

The extra features live in a small `spanbolt.v1.Control` gRPC service
(`crates/proto/proto/spanbolt/v1/control.proto`).

## Status

Early, and used in earnest by one team: a large Spanner application's full test suite (Liquibase
migrations, JDBC, the Java client, Beam pipelines) passes on it. Expect gaps outside what that
suite and the conformance cases exercise.

- Anything spanbolt doesn't support fails with `UNIMPLEMENTED`, not a silently different answer.
- Not supported: graph queries (`GRAPH_TABLE`), `APPROX_*` distance functions with `options`,
  backups, IAM policies, the emulator's own command-line flags.
- Real Spanner takes locks where spanbolt aborts and lets the client retry. Both are outcomes the
  client libraries already handle, but the timing differs.
- Tested on macOS (arm64) and in the linux/arm64 image. linux/amd64 is untested.

## Quick start

### Docker

```sh
docker build -t spanbolt .                 # the first build compiles GoogleSQL: 30–45 min
docker run --rm -p 9010:9010 -p 9020:9020 spanbolt
export SPANNER_EMULATOR_HOST=localhost:9010
```

With Testcontainers, tag the image under the emulator's name (`SpannerEmulatorContainer` checks
it), then use the tag as the image:

```sh
docker tag spanbolt gcr.io/cloud-spanner-emulator/emulator:spanbolt
```

`docs/docker.md` covers multi-arch builds, a prebuilt GoogleSQL layer and pre-seeded images.

### Cargo

spanbolt is a Rust workspace. The SQL engine links a C++ shim around GoogleSQL
(`native/googlesql`), built with Bazel. See `docs/building.md` for getting the shim.

```sh
source scripts/googlesql-env.sh            # points the build at the shim
cargo build --release -p spanbolt-server --features googlesql
./target/release/spanbolt --host-port 127.0.0.1:9010 --rest-port 9020
```

Without the `googlesql` feature, `cargo build` and `cargo test` work with no native library; every
SQL call then returns `UNIMPLEMENTED`.

Useful flags: `--snapshot <file>` loads a snapshot at startup, `spanbolt snapshot <path>` asks a
running server to write one, and `--emulator-compat` follows the emulator instead of production.

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
crates/server         gRPC and REST server, the `spanbolt` binary
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
