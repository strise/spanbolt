# Building spanbolt

spanbolt's SQL engine calls GoogleSQL's analyser, a C++ library, through a small shim
(`native/googlesql`, `libgooglesql_shim.so`). The Rust build links a prebuilt shim. The shim takes
about 25 minutes and a lot of memory to build with Bazel, so normally you download it instead.

```sh
cargo build --release -p spanbolt   # the binary; downloads the shim the first time
cargo test                          # default members: no shim needed
scripts/gate.sh                     # what CI runs
```

`spanbolt` (`crates/spanbolt`) is the binary with the SQL engine on (its default feature
`googlesql`); `spanbolt-server` is the library behind it, with the engine off unless you enable its
`googlesql` feature. `cargo install spanbolt` builds the published crates the same way.

Requirements: Rust 1.91+, `curl` and `tar`. The Spanner API protos compile with the protoc bundled
in `protoc-bin-vendored`. A system `protoc` 27+ is needed only when you build against your own
Bazel output (for `resolved_ast.proto`). Prebuilt shims exist for `darwin-arm64`, `linux-arm64` and `linux-x86_64`. The Linux shim
needs glibc 2.38+ (Ubuntu 24.04, Debian 13). The server needs zoneinfo for `America/Los_Angeles`.

## Where the shim comes from

`crates/googlesql-sys/build.rs` takes the first that applies:

1. **`GOOGLESQL_PREFIX`** (a `bin/` + `src/` layout, such as an unpacked archive) or
   **`GOOGLESQL_SHIM_DIR`** (+ `GOOGLESQL_SRC_DIR`). `source scripts/googlesql-env.sh` sets these
   when its local prefix exists.
2. **A local Bazel build**: `native/googlesql/bazel-bin/libgooglesql_shim.so`.
3. **The pinned prebuilt archive**: unpacked in the cache (`SPANBOLT_CACHE_DIR`, default
   `$XDG_CACHE_HOME/spanbolt` or `~/.cache/spanbolt`, under `prebuilt/`), else downloaded from
   `SPANBOLT_SHIM_URL` (a base URL or `file:///dir`; default: the GitHub release named in
   `native/googlesql-shim.env`) and checked against its pinned SHA-256. The cache lives outside
   `target/`, so `cargo clean` doesn't refetch.

If none applies, the build stops with the options above. A published crate has no
`native/googlesql`, so only 1 and 3 apply: `googlesql-sys` ships `googlesql-shim.env` (a symlink to
`native/googlesql-shim.env` in the repository, a plain file in the `.crate`) and skips the source
check. On docs.rs (`DOCS_RS`) the build script links nothing and stubs the resolved-AST types. Two more switches:

- The download path refuses an archive whose shim sources (`native/googlesql`: `shim/`, `tools/`,
  `BUILD.bazel`, `MODULE.bazel`, `.bazelrc`, `.bazelversion`) differ from the ones it was built
  from, because it would be stale. `SPANBOLT_SHIM_ALLOW_STALE=1` uses it anyway.
- `PROTOC` picks the protoc binary for the Bazel and prefix paths without `bin/resolved_ast.fds`
  (a system protoc, 27+), and for `spanbolt-proto` (default: the vendored one).

## Where the binary finds the shim

The shim is linked dynamically. `spanbolt`'s rpath lists, in order: the build directory
(`target/.../googlesql-sys-*/out`), the executable's directory (`@executable_path` / `$ORIGIN`:
the image and any tarball ship `libgooglesql_shim.so` next to `spanbolt`) and, for a downloaded
shim, `<cache>/prebuilt/<archive>/lib`, which holds the library under its install name. The last
one is what a `cargo install`ed binary uses: cargo deletes the build directory afterwards, and
`~/.cargo/bin` holds only the executable. Deleting the cache breaks such a binary
(`Library not loaded: @rpath/libgooglesql_shim.dylib`); `cargo install spanbolt --force`
downloads it again.

Static linking would avoid this, but the shim is a 40 MB C++ shared library built by Bazel (with
libc++ on Linux); a static archive would need a second Bazel output per platform and a link step
that knows its C++ runtime. Not worth it while the cache works.

## Building the shim with Bazel

Needs `bazelisk` and clang (Xcode on macOS; clang 18 on Linux, where gcc 13 needs more memory).

```sh
cd native/googlesql
bazelisk build //:libgooglesql_shim.so      # cold ~25 min on a 12-core Mac; seconds when cached
```

Memory bounds the build: googlesql's generated sources need 5+ GB per compile job. On Linux use
`--jobs` of about RAM / 6 GB and `--repo_env=CC=clang-18 --repo_env=CXX=clang++-18`. More notes:
`native/googlesql/SPIKE.md`.

cargo picks up `native/googlesql/bazel-bin` in the checkout that built it. To use the build from
other checkouts and worktrees, unpack it into the local prefix that `scripts/googlesql-env.sh` reads:

```sh
scripts/package-shim.sh --out /tmp/shim
rm -rf ~/.cache/spanbolt/local && mkdir -p ~/.cache/spanbolt/local
tar -xzf /tmp/shim/googlesql-shim-*.tar.gz -C ~/.cache/spanbolt/local
```

`googlesql-env.sh` uses the prefix only while its `MANIFEST` matches the shim sources of the tree
it runs in. After a shim change, or in a checkout on other shim sources, it warns and skips the
prefix, so the build falls back to the pinned archive instead of linking an outdated shim.

## The archive

`scripts/package-shim.sh` turns a Bazel build (or a prefix) into
`dist/googlesql-shim-<GOOGLESQL_VERSION>-s<SHIM_VERSION>-<platform>.tar.gz` plus a `.sha256`.
Repackaging the same files gives the same bytes. Contents, no top directory:

| Path | What |
|---|---|
| `MANIFEST` | versions, platform, source hash, protoc version |
| `bin/libgooglesql_shim.so` | the shim (a Mach-O dylib on macOS) |
| `bin/resolved_ast.fds` | descriptor set of `resolved_ast.proto` and its imports, so no protoc is needed |
| `bin/external/googlesql+/googlesql/**.proto` | generated protos (`resolved_ast.proto`, `resolved_node_kind.proto`) |
| `src/googlesql/**.proto` | googlesql's hand-written protos |
| `licenses/` | spanbolt's `LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.md`, and `native/<component>/LICENSE` for the libraries linked into the shim (`docs/licenses.md`) |

## Versions and bumping

`native/googlesql-shim.env` is the one place that pins the shim: `GOOGLESQL_VERSION` (must match
`MODULE.bazel`), `SHIM_VERSION`, `SHIM_SOURCE_SHA256` and one `SHA256_<platform>` per archive.
`scripts/gate.sh` warns when the pin no longer matches the shim sources
(`scripts/package-shim.sh --check`).

After changing the shim or upgrading googlesql:

1. Bump `SHIM_VERSION` (and `GOOGLESQL_VERSION` with `MODULE.bazel` for a googlesql upgrade).
2. Build the archives. Either tag `shim-<GOOGLESQL_VERSION>-s<SHIM_VERSION>`, which runs
   `.github/workflows/shim.yml` (Bazel per platform, release assets on the tag), then download the
   `*.sha256` assets and run `scripts/package-shim.sh --pin-sums <dir>`; or build locally and run
   `scripts/package-shim.sh --pin` per platform (Linux from the Docker `shim` stage:
   `docker build --platform linux/arm64 --target shim -o type=local,dest=out . && scripts/package-shim.sh --from out/out --platform linux-arm64 --pin`).
3. Commit `native/googlesql-shim.env`. Until the archives are published, point
   `SPANBOLT_SHIM_URL` at them (`file:///path/to/dist`).

## Docker

The `Dockerfile` builds for `linux/amd64` and `linux/arm64`. `--build-arg SHIM=bazel` (the default)
builds the shim in the image; `SHIM=prebuilt` downloads the pinned archive for the target
architecture and checks its SHA-256, and `--build-context shim-archives=dist` supplies local
archives instead:

```sh
docker build --platform linux/arm64 --build-arg SHIM=prebuilt --build-context shim-archives=dist -t spanbolt .
```

`--build-arg SPANBOLT_SHIM_URL=...` overrides the download base. More: `docs/docker.md`.

## CI

Drafts in `.github/workflows` (not enabled yet):

- `ci.yml`: `scripts/gate.sh` (fmt, clippy, tests, conformance in prod and emulator mode) on
  Linux x86_64, Linux arm64 and macOS arm64, with the prebuilt shim.
- `shim.yml`: builds the three archives with Bazel and publishes them on a `shim-*` tag.
- `crates.yml`: on a `v*` tag, publishes the crates to crates.io (`docs/releasing.md`).
- `image.yml`: on a `v*` tag, builds each architecture natively with `SHIM=prebuilt`, smoke-tests
  it (`scripts/docker-smoke.sh`), pushes by digest and merges a multi-arch manifest at
  `ghcr.io/strise/spanbolt`. Pull requests that touch the conformance cases, the harness, the
  smoke script, the Dockerfile or the workflow build and smoke-test linux/amd64 without pushing.

Lint them with `actionlint` (`.github/actionlint.yaml` declares the `ubuntu-24.04-arm` runner).
