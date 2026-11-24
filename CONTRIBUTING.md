# Contributing

Issues and pull requests are welcome. For anything larger than a fix, open an issue first so we
can agree on the approach.

## Use the templates

Issues and pull requests must follow the repository's templates. We may close ones that don't, or
ask you to fill them in first.

- **Issues**: pick the **Bug report**, **Unsupported feature** or **Question** form under "New
  issue". A bug report needs the client library and language, the DDL, the request, what
  spanbolt returns and what the official emulator returns. A small schema and request that answer
  differently on spanbolt than on the emulator is the most useful report there is.
- **Pull requests**: fill in the template's Why, What and How it was tested sections, and link
  the issue (`Fixes #123`). The title must be a conventional commit (see below).
- **Security problems** never go in an issue: report them privately (`SECURITY.md`).

## Setup

You need Rust 1.91+, `curl` and `tar`. Read `DESIGN.md` first: it sets the architecture and the
rules (no `unwrap()` on request paths, errors map to Spanner's status codes and messages).

The workspace's default members build without any native code:

```sh
cargo build
cargo test
```

The SQL engine (`crates/sql`) and `googlesql-sys` call GoogleSQL's analyser through a C++ shim. You
don't build it yourself: the first build with the engine downloads the prebuilt shim pinned in
`native/googlesql-shim.env` and checks its SHA-256. Prebuilt shims exist for macOS arm64 and Linux
x86_64/arm64 (glibc 2.38+).

```sh
cargo build --release -p spanbolt   # the real server
cargo test --release -p spanbolt-sql
```

Only if you change the shim itself (`native/googlesql`) do you need Bazel; see `docs/building.md`.

## Checks

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/gate.sh   # what CI runs: the above with and without the engine, license checks, conformance in both modes
```

## Behaviour changes need conformance cases

spanbolt's reference is Cloud Spanner, then the official emulator. A change in behaviour comes with
cases in `conformance/cases` whose expectations are recorded from the official emulator (or Cloud
Spanner), never written by hand or edited to make spanbolt pass. If spanbolt can't support
something, it must return `UNIMPLEMENTED`, not a different answer.

Record and check (details and the case format: `crates/conformance/README.md`):

```sh
docker run -d --name sb-conf-oracle -p 19210:9010 gcr.io/cloud-spanner-emulator/emulator:1.5.59
cargo run --release -p spanbolt-conformance -- record --target 127.0.0.1:19210 --filter <case>
docker rm -f sb-conf-oracle

cargo run --release -p spanbolt -- --host-port 127.0.0.1:19220 &
cargo run --release -p spanbolt-conformance -- check --target 127.0.0.1:19220
```

Review the recorded diff before committing.

## Pull requests

- PRs are squash-merged, and the **PR title becomes the commit message**. It must be a conventional
  commit, `type(scope): description`, for example `fix(sql): keep NULLs last in DESC order`
  (types: feat, fix, perf, refactor, docs, test, ci, build, chore, revert; `!` marks a breaking
  change). CI checks the title.
- The PR description becomes the commit body: say why, what changed and how you tested it.
- CI runs `scripts/gate.sh` on Linux x86_64, Linux arm64 and macOS. It has to pass before merge.
- A first-time contributor's workflows wait for a maintainer to approve them.

## License

By contributing you agree that your contribution is licensed under Apache-2.0, as the rest of the
project (`LICENSE`). Code ported from other projects must keep its license and be named in
`NOTICE`.
