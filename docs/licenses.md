# Licenses in spanbolt distributions

spanbolt is Apache-2.0 (`LICENSE`, `NOTICE`). `THIRD_PARTY_NOTICES.md` is the inventory of the
third-party code it builds on. Apache-2.0 §4, MIT, BSD, ISC and the ICU (Unicode-3.0) license
require their texts in binary distributions, so the Docker image and the prebuilt shim archive carry
them:

| Where | Contents |
|---|---|
| Image: `/opt/spanbolt/licenses/` | `LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.md`, `THIRD_PARTY_LICENSES.txt` (Rust crates), `native/` |
| Shim archive: `licenses/` | `LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.md`, `native/` |

`native/<component>/LICENSE` covers what is linked into `libgooglesql_shim`: GoogleSQL, Abseil,
protobuf, utf8_range, RE2, ICU and nlohmann/json, copied from the Bazel external repositories the
shim was built from by `scripts/native-licenses.sh`. `scripts/package-shim.sh` runs it on a Bazel
tree (`--licenses DIR` for a prefix without `licenses/native/`); the Dockerfile's `shim-licenses`
stage runs it after `bazel fetch` of those repositories.

## Rust crates

`about.toml` lists the licenses spanbolt accepts; `about.hbs` is the text template. The image's
build stage runs

```sh
cargo fetch --locked
cargo about generate --frozen --all-features --fail about.hbs -o THIRD_PARTY_LICENSES.txt
```

over every crate in `Cargo.lock` (build dependencies included, like the inventory). `--frozen` reads
the texts from the crate sources only, so the output is the same on every run.

`scripts/check-licenses.py` (in `scripts/gate.sh`) fails when a crate's license expression can't be
satisfied from `accepted`, without needing cargo-about. When it fails, decide whether the license is
acceptable; if so, add it to `accepted` and to `THIRD_PARTY_NOTICES.md`.

## Adding a native dependency

Check what the shim links (`nm` on the library: an archive in googlesql's `http_archive_deps.bzl`
is not necessarily linked), add a row to `scripts/native-licenses.sh` and to `THIRD_PARTY_NOTICES.md`.
