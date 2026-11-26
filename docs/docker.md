# spanbolt in Docker

`Dockerfile` builds a drop-in replacement for `gcr.io/cloud-spanner-emulator/emulator`: gRPC on
9010, the REST gateway on 9020, and the startup line Testcontainers waits for.

## Build

```sh
# linux/arm64 on an arm64 host (Apple silicon, Graviton)
DOCKER_BUILDKIT=1 docker build --platform linux/arm64 -t spanbolt:latest .
docker run --rm -p 9010:9010 -p 9020:9020 spanbolt:latest
```

Stages:

| Stage | What | Cache |
|---|---|---|
| `shim` | `libgooglesql_shim.so` and the googlesql protos, built with Bazel + clang 18 on Ubuntu 24.04 (`SHIM=bazel`), or the pinned archive (`SHIM=prebuilt`) | layer cache (rebuilt only when `native/googlesql/` changes); `--mount=type=cache,id=spanbolt-bazel-<arch>` for Bazel |
| `shim-licenses`, `shim-fds` | `SHIM=bazel` only: the native license texts (`bazel fetch` of the six repositories, about a minute) and `bin/resolved_ast.fds` (protoc 31.1), which a prebuilt archive already carries | layer cache |
| `cargo-about` | `cargo install cargo-about` (version `CARGO_ABOUT_VERSION`) | layer cache |
| `build` | `cargo build --release -p spanbolt` (rust:1.91-trixie, no protoc), then `strip`, then the license texts into `licenses/` (`cargo about generate --frozen`) | registry, git and `target/` cache mounts |
| `runtime` | `gcr.io/distroless/cc-debian13:nonroot` (glibc, libstdc++, tzdata; no shell) with `spanbolt` and the shim in `/opt/spanbolt`, user `nonroot` (65532), writable `/data` | |

Build args: `BAZEL_JOBS` (default 2), `PROTOC_VERSION` (`SHIM=bazel` only), `CARGO_ABOUT_VERSION`, `RUST_IMAGE`, `RUNTIME_IMAGE`.

`/opt/spanbolt/licenses/` holds the license texts the image must ship (`docs/licenses.md`).

`cc-debian12` cannot be the base. The shim is built on Ubuntu 24.04 and needs glibc 2.38 and
`GLIBCXX_3.4.31`; debian12 has glibc 2.36 and libstdc++ 6.0.30. The image has no shell. To debug,
use `docker run --entrypoint /opt/spanbolt/spanbolt ...`, or build with
`--build-arg RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:debug-nonroot`.

**Memory, not CPU, limits the shim build.** Each compile job on googlesql's generated sources needs
5 GB or more. Set `BAZEL_JOBS` to about RAM / 6 GB: 2 on a 16 GB Docker VM, 5 on a 32 GB runner.

### Reusing a prebuilt shim

The shim changes only with googlesql or `native/googlesql/shim`. Build it once and substitute the
stage:

```sh
docker build --platform linux/arm64 --target shim -t spanbolt-shim:arm64 .
docker build --platform linux/arm64 --build-context shim=docker-image://spanbolt-shim:arm64 -t spanbolt .
```

For CI this is the reliable way to keep the shim warm: build it once per googlesql version on a
large runner and pass it in. The Bazel cache mount was not reliable here (see below).

### linux/amd64

```sh
docker buildx build --platform linux/amd64 --build-arg BAZEL_JOBS=5 -t spanbolt:amd64 --load .
```

Build amd64 on a native amd64 runner. Under QEMU emulation on an arm64 Mac, the C++ build takes
hours. On a native 32 GB runner, expect 30–45 minutes cold and about a minute when only Rust
changed. A multi-arch image needs both builds (or `--build-context shim=...` per arch), plus a
registry or `--output type=oci,dest=spanbolt.tar`.

## Numbers (linux/arm64, Apple M-series, Docker VM with 12 vCPU and 16 GB)

| What | Result |
|---|---|
| Image `spanbolt:latest` (distroless cc-debian13) | 91.0 MB, 28.5 MB gzipped (`docker save \| gzip`); 118.4 MB / 39.5 MB before the ICU data trim |
| Same image on `debian:trixie-slim` + tzdata (first version) | 180.1 MB, 58.6 MB gzipped |
| Official `gcr.io/cloud-spanner-emulator/emulator:1.5.59` | 177.2 MB, 59.4 MB gzipped |
| Contents of `/opt/spanbolt` | shim 35.0 MB (`strip --strip-unneeded`; 62.4 MB before the ICU data trim), `spanbolt` 17.8 MB (`strip`) |
| Cold build, shim stage (`BAZEL_JOBS=2`, empty Bazel cache, 7,485 actions) | 2,374 s (39.6 min); no OOM in 16 GB. A second cold run: 2,283 s |
| Cold build, Rust stage (empty cargo caches) | 33 s compile; 55 s for the whole image with the shim layer cached |
| Warm rebuild after a Rust source edit | 11 s (8.4 s incremental `cargo build`) |
| Warm rebuild, nothing changed | under 1 s (all layers cached) |
| Rebuild after a shim source edit | a cold Bazel build again (~38–40 min); see below |
| Time from `docker run` to the ready line in the log | ~300–360 ms; the process itself is ready in ~7–9 ms |

**The Bazel cache mount did not survive between builds on this Docker Desktop.** Two later builds
(a `--no-cache` rebuild and a build after a shim edit) found `/root/.cache` empty and started
Bazel from scratch. Small cache mounts and the cargo mounts did persist, and the VM had 193 GB free.
The cause (BuildKit's GC policy for large cache mounts, or an outside `builder prune`) is
unconfirmed. In day-to-day use the shim stage stays warm through the layer cache. For CI, use a
prebuilt shim (above).

**The cargo `target/` cache mount is shared by every build on the host**, from any checkout or
branch. cargo trusts a crate whose sources are older than the cached artifact, and `COPY` keeps the
context's mtimes. One image (768056d07199, 2026-11-18) shipped a stale `spanbolt-storage` this way:
only proto, sql and server were recompiled, so TOKENLIST columns declared without `STORED` were
stored as NULL and `SEARCH` matched nothing. The build stage now touches `crates/` before
`cargo build`, so the workspace crates always rebuild (22 s on 12 vCPU); dependencies stay cached.
If an image behaves differently from the native binary of the same commit, check the build log
for which `Compiling spanbolt-*` lines appear.

## Drop-in notes

- **Ports.** gRPC on 9010 (`--host-port 0.0.0.0:9010`), REST on 9020 (`--rest-port 9020` or
  `SPANBOLT_REST_PORT`). Both are in the image's default command.
- **Testcontainers.** `SpannerEmulatorContainer` (testcontainers-java, module `gcloud`: both the
  deprecated `org.testcontainers.containers` class and `org.testcontainers.gcloud`) exposes 9010 and
  9020 and waits with `Wait.forLogMessage(".*Cloud Spanner emulator running\\..*", 1)`. spanbolt
  prints `spanbolt: Cloud Spanner emulator running. gRPC on 0.0.0.0:9010, REST on 0.0.0.0:9020
  (ready in N ms)` once both listeners are bound. Checked with Testcontainers 1.21.4: started in
  1.6 s, against 2.6 s for the official image (container overhead included).
- **Image name.** The constructor calls
  `assertCompatibleWith("gcr.io/cloud-spanner-emulator/emulator")`, so a plain `spanbolt:latest`
  throws `IllegalStateException`. Tag the local image under the emulator's repository (a local tag;
  nothing is pushed) and pass that tag:

  ```sh
  docker tag spanbolt:latest gcr.io/cloud-spanner-emulator/emulator:spanbolt
  ```

  For Gradle builds, see "Java test suites through Testcontainers" below: `./gradlew -D...` does
  not reach the test JVMs.

  Code that builds its own `DockerImageName` can use
  `.asCompatibleSubstituteFor("gcr.io/cloud-spanner-emulator/emulator")` instead.
- **REST gateway coverage.** The `google.api.http` bindings, transcoded to proto3 JSON
  (`prost-reflect` over the descriptor set) and served by the same in-process services as gRPC:
  - instance admin: list/get instance configs; list, get, create, update (PATCH), delete instances
  - database admin: list, get, create, update (PATCH), drop databases; get DDL, update DDL (PATCH)
  - operations: get, list, delete, cancel
  - Spanner: create, batchCreate, list, get and delete sessions; `executeSql`, `executeBatchDml`,
    `read`, `beginTransaction`, `commit`, `rollback`

  Errors are a non-2xx status (grpc-gateway's code mapping) with a `{code, message, details}` body,
  as in the emulator, including `ResourceInfo` details. One deliberate difference: the emulator's
  gateway answers SQL errors with HTTP 500 `failed to marshal error message`, while spanbolt returns
  the real status (for example 400, INVALID_ARGUMENT, `Table not found: Nope`). Not bound:
  `executeStreamingSql`, `streamingRead`, `partition*`, `batchWrite`, backups and IAM.
- **Instance config.** Any `config` name is accepted, as in the emulator; e2e setup scripts rely on it.
- **Shutdown.** SIGTERM and SIGINT stop the server, so `docker stop` returns at once.
- **Not supported.** The emulator's own flags (`--override_max_databases_per_instance` and
  friends).

## Java test suites through Testcontainers

Test suites that start the emulator themselves usually take the image name from a system property
or an environment variable. Pass the spanbolt tag (`gcr.io/cloud-spanner-emulator/emulator:spanbolt`,
above) there. With Gradle, a property set with `./gradlew -D...` stays in the Gradle JVM and does
not reach the forked test JVMs; set it through `JAVA_TOOL_OPTIONS`, which every JVM reads (a build
that overwrites `_JAVA_OPTIONS` for its test JVMs leaves `JAVA_TOOL_OPTIONS` alone):

```sh
JAVA_TOOL_OPTIONS=-D<your.emulator.image.property>=gcr.io/cloud-spanner-emulator/emulator:spanbolt \
  ./gradlew test
```

Measured 2026-11-19 on a Java API test suite (linux/arm64 images, Apple M-series host, one test
JVM, Gradle daemon warm), against official emulator `1.5.40`:

| What | official `1.5.40` | spanbolt image |
|---|---|---|
| Container start, `docker run` to the ready line (5 runs) | 1,125–1,251 ms | 130–183 ms |
| Schema replay: 607 DDL in one `UpdateDatabaseDdl` (5 runs) | 28.6–30.7 s | 105–115 ms |
| Seed rows: 15 INSERTs (5 runs) | 428–469 ms | 122–190 ms |
| Five API test classes, 97 tests (JUnit suite time, two rounds) | 329.7 s / 314.0 s | 42.6 s / 37.4 s |

The container start and the schema replay happen inside the first test that touches Spanner, so
they count towards that class's time. Schema replay is the main setup difference: on 1.5.40 the
schema step alone takes about 30 s per test JVM.

## Pre-seeded images

`--snapshot <path>` or `SPANBOLT_SNAPSHOT=<path>` loads a snapshot at start.
`spanbolt snapshot <path>` asks a running server to write one through
`spanbolt.v1.Control/SaveSnapshot`; `--target` defaults to `SPANNER_EMULATOR_HOST`, or
`127.0.0.1:9010`. The path is on the server's filesystem.

Workflow for CI:

```sh
docker run -d --name seedbox -p 9010:9010 -p 9020:9020 spanbolt:latest
# Run migrations and seeding against localhost:9010 (gRPC) / localhost:9020 (REST), for example:
SPANNER_EMULATOR_HOST=localhost:9010 python3 scripts/make_seed_snapshot.py
docker exec seedbox spanbolt snapshot /data/seed.snap
mkdir -p seed && docker cp seedbox:/data/seed.snap seed/seed.snap
docker rm -f seedbox
docker build -f Dockerfile.seeded -t spanbolt-seeded:latest seed
docker run --rm -p 9010:9010 -p 9020:9020 spanbolt-seeded:latest
```

`Dockerfile.seeded` is `FROM spanbolt` plus the snapshot and `SPANBOLT_SNAPSHOT`. Its build
context is the directory holding `seed.snap`, because the repository's `.dockerignore` excludes
`*.snap`.

Measured with `scripts/make_seed_snapshot.py` on a 530-statement production schema (262 tables;
`fixtures/schema.json` is a generated schema of the same shape and the script's default) plus 50,000 rows, 10,000 in each of five wide tables (17–134 columns, with search and
secondary indexes):

| What | Result |
|---|---|
| Apply 530 DDL statements / insert 50k rows (Python client, batches of 1,000) | 0.2–0.3 s / 6–11 s |
| `spanbolt snapshot` (SaveSnapshot) | 59–68 ms |
| Snapshot file | 1.7 MB (zstd; synthetic rows compress well, so real data will be larger) |
| Seeded image (distroless) | 119.3 MB, 40.6 MB gzipped |
| Snapshot load inside the container (logged by spanbolt) | 166–250 ms (10 runs) |
| Time from `docker run` to the ready line, seeded | 451–713 ms (10 runs; the first run of a fresh image is slowest). Unseeded: ~300 ms |

Rechecked 2026-11-19 with full-text search merged (image with the cargo-cache fix): 50k rows in
6.5 s, `SaveSnapshot` 74.5 ms, snapshot 1.93 MB, seeded image 120 MB, load 175–180 ms, `docker run`
to the ready line 540–600 ms (3 runs). The generated TOKENLIST columns survive the snapshot:
`SEARCH(name_tokens, 'name')` on `granite_no_search_entities` matches all 10,000 seeded rows and
`'name 0000042'` matches 1.

### Snapshots from spanbolt 0.2.1 and earlier

Up to 0.2.1, generated columns without `STORED` (`L INT64 AS (CHAR_LENGTH(S))`) were never
computed and held NULL. Newer versions compute them on write
([#45](https://github.com/strise/spanbolt/pull/45)), but a snapshot keeps the values it was
saved with: loading an old one gives NULL in those columns, and in any index on
them, until the row is written again. The same goes for a STORED generated column or a TOKENLIST
column whose expression reads a non-stored generated column, or a generated column declared after
it.

TOKENLIST columns are stored as well. Since 0.3.0 the full-text tokenizers match production
([#102](https://github.com/strise/spanbolt/issues/102),
[#138](https://github.com/strise/spanbolt/issues/138)), but a snapshot from 0.2.1 or earlier keeps
the tokens it was saved with. `SEARCH` over those rows can miss matches until the rows are
written again. This affects every table with a TOKENLIST column, not only those with non-stored
generated columns.

To fix an old snapshot, do one of these:

- Re-create it (the safest way): run the migrations and the seeding against 0.3.0 or later and
  save a new snapshot, as in the seeded image build above.
- Rewrite it: load it into the new version, update every row of each table with a non-stored
  generated column or a TOKENLIST column, and save it again. Any update recomputes all of a
  row's generated columns and their index entries, so setting a column to itself is enough (as
  Partitioned DML, or in a read-write transaction):

  ```sql
  UPDATE T SET S = S WHERE TRUE
  ```

  Then `spanbolt snapshot <path>`. `S` is any column that is neither in the primary key nor
  generated. A table whose other columns are all generated has to be re-created.

## Making the image smaller

**ICU data.** A third of the shim was ICU's prebuilt common data (`icudt76l.dat`,
31.9 MB): every locale's names, currencies and units, collation tailorings, break dictionaries,
transliterators and codepage converters. googlesql links it statically through `libicudata.a`.
The analyser needs only the locale-independent part, and spanbolt evaluates functions in Rust.
`//:icu_data` (`native/googlesql/tools/icu_data.py`) writes a trimmed copy (82 items, 4.4 MB:
normalisation, properties, root collation, supplemental tables) as a C array that defines
`icudt76_dat` inside `:shim`. The linker sees `:shim` before ICU's archives, so the full data is
never pulled in.

| What | Before | After |
|---|---|---|
| Image (linux/arm64) | 118.4 MB, 39.5 MB gzipped | 91.0 MB, 28.5 MB gzipped |
| Shim in the image (stripped) | 62.4 MB | 35.0 MB |
| Shim on macOS (unstripped) | 69.0 MB | 41.4 MB |

Conformance (native and against the image) and `scripts/docker-smoke.sh` pass. ICU's data is only
read by ICU calls the analyser makes. If one ever needs a dropped item, it fails with an ICU
"missing resource" error, not a crash: add the item to `keep()` in `icu_data.py`.

**Checked, not changed.** Bazel's auto-configured toolchain compiles `-c opt` with `-O2
-ffunction-sections -fdata-sections` and links with `--gc-sections` (Linux) or `-dead_strip` (macOS):
`bazel aquery` shows them, and `unix_cc_configure.bzl` uses the same template on both. ICU's own
code is built optimised, and only about 240 ICU functions are linked.

**Not tried.** `-Os` across googlesql: perhaps 2–5 MB of the ~20 MB of
code, at some cost in catalog-build latency, which runs after every DDL. Linking the shim into
`spanbolt` statically: little size gain, since the shim already exports only the six
`gsql_*` functions, but one file to ship. `upx` would shrink the files but slow every start.

## Smoke test

```sh
IMAGE=spanbolt:latest scripts/docker-smoke.sh
```

The script starts the image on ephemeral ports and waits for the Testcontainers ready line. It
then runs:

- the REST calls a typical e2e setup makes: create an instance (any config); create, update DDL,
  get DDL and drop a database; sessions, begin, executeSql, executeBatchDml, commit and rollback;
  and the error shapes
- the Python client (`google-cloud-spanner`): DDL, mutations, DML in a read-write transaction,
  a query and a read
- `spanbolt-conformance check --tag select`

`SMOKE_GRPC=host:port SMOKE_REST=host:port` points it at a running server instead of Docker. The
script also passes against `gcr.io/cloud-spanner-emulator/emulator:1.5.59`.
