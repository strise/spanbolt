# googlesql-sys

Rust bindings to a small C++ shim around Google's [GoogleSQL](https://github.com/google/googlesql)
analyser: build a catalog, analyse one statement against it, get the resolved AST back as prost
types. Part of [spanbolt](https://github.com/strise/spanbolt), a fast drop-in replacement for the
Google Cloud Spanner emulator; the API follows spanbolt's needs and may change in any release.

## The native library

The build script downloads a prebuilt `libgooglesql_shim` (pinned by URL and SHA-256 in
`googlesql-shim.env`) into `~/.cache/spanbolt` (`SPANBOLT_CACHE_DIR`, `XDG_CACHE_HOME`) and links
it dynamically. Prebuilt shims exist for `aarch64-apple-darwin`, `aarch64-unknown-linux-gnu` and
`x86_64-unknown-linux-gnu`; the Linux ones need glibc 2.38+. It needs `curl` and `tar`.

Binaries find the library through their rpath: the build directory, the executable's directory,
then the download cache. Ship the library next to your binary, or keep the cache.

On other targets, or to use your own build, set `GOOGLESQL_PREFIX` to an unpacked shim archive
(`bin/` + `src/`); `SPANBOLT_SHIM_URL` points the download at a mirror. Building the shim from
source needs Bazel and a git checkout of spanbolt:
[docs/building.md](https://github.com/strise/spanbolt/blob/main/docs/building.md).
