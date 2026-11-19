# Third-party notices

spanbolt is licensed under Apache-2.0 (`LICENSE`, `NOTICE`). This file lists the third-party code it
builds on and their licenses. It is an inventory, not the full license texts. Those ship with the
binaries (`docs/licenses.md`): the Docker image has them in `/opt/spanbolt/licenses/`
(`THIRD_PARTY_LICENSES.txt` for the Rust crates, rendered by `cargo about` from `about.toml`;
`native/` for the shim's libraries), and the prebuilt shim archive in `licenses/`.

None of these licenses requires spanbolt to use a license other than Apache-2.0. The only copyleft
terms are file-level MPL-2.0 (`imbl`, `imbl-sized-chunks`, the Public Suffix List). Those files
stay under MPL-2.0 and their source is public. spanbolt uses the imbl crates unmodified; its copy of
the Public Suffix List drops comments and converts IDN rules to punycode, and keeps the MPL header.

## Native library: `libgooglesql_shim`

Built by Bazel from `native/googlesql` and loaded by the `spanbolt` binary (shipped next to it).
Linked in statically (checked against the symbols of the macOS arm64 build):

| Component | Version | License |
|---|---|---|
| GoogleSQL | 2026.11.2 | Apache-2.0 |
| Abseil (including CCTZ) | 20260817.0 | Apache-2.0 |
| Protocol Buffers | 37.0-rc3 | BSD-3-Clause |
| utf8_range (part of the protobuf repository) | 37.0-rc3 | MIT |
| RE2 | 2025-11-05 | BSD-3-Clause |
| ICU (code and trimmed `icudt76l.dat`) | 76 | Unicode-3.0 (ICU License) |
| nlohmann/json | 3.12.0 | MIT |

gRPC, BoringSSL and c-ares appear in `MODULE.bazel.lock` as transitive build dependencies but are
not linked into the shim. Build-only tools (rules_cc, rules_python, platforms, flex, bison) are
not shipped.

The Docker image also contains the distroless base image (`gcr.io/distroless/cc-debian13`: glibc,
libstdc++, tzdata), under their Debian package licenses.

## Vendored files

| Path | Origin | License |
|---|---|---|
| `crates/proto/proto/google/**` | googleapis | Apache-2.0 (`crates/proto/proto/LICENSE-googleapis`) |
| `crates/sql/src/public_suffix_list.dat` | Public Suffix List, as embedded in Cloud Spanner Emulator 1.5.59 | MPL-2.0 |
| `crates/sql/src/supported_functions.txt`, `crates/sql/src/info_schema_columns.tsv` | Cloud Spanner Emulator | Apache-2.0 |
| Ported code (see `NOTICE`) | Cloud Spanner Emulator, GoogleSQL | Apache-2.0 |

`crates/conformance/beam` is a Maven project that depends on Apache Beam (Apache-2.0) and SLF4J
(MIT); nothing from it ships.

## Rust crates

From `Cargo.lock` with all features (`cargo metadata --all-features`), 201 crates. Crates offering a
choice are used under a permissive option (for `r-efi`, MIT or Apache-2.0; it is UEFI-only and
not in the binaries). `ring` (Apache-2.0 AND ISC, with BoringSSL-derived notices in its `LICENSE`)
comes in through rustls in tonic's TLS stack.

| License | Crates |
|---|---|
| MIT OR Apache-2.0 | anstream 1.0.0, anstyle 1.0.14, anstyle-parse 1.0.0, anstyle-query 1.1.5, anstyle-wincon 3.0.11, anyhow 1.0.104, arc-swap 1.9.5, async-trait 0.1.92, base64 0.22.1, base64 0.23.1, bitflags 2.13.2, block-buffer 0.10.4, cc 1.11.2, cfg-if 1.0.5, chrono 0.4.45, chrono-tz 0.10.4, clap 4.6.8, clap_builder 4.6.7, clap_derive 4.6.8, clap_lex 1.1.2, colorchoice 1.0.5, core-foundation 0.10.1, core-foundation-sys 0.8.7, cpufeatures 0.2.17, crossbeam-utils 0.8.23, crypto-common 0.1.7, digest 0.10.7, either 1.19.0, errno 0.3.15, find-msvc-tools 0.1.15, fixedbitset 0.5.9, futures-channel 0.3.34, futures-core 0.3.34, futures-sink 0.3.34, futures-task 0.3.34, futures-util 0.3.34, getrandom 0.2.17, getrandom 0.4.3, hashbrown 0.14.5, hashbrown 0.15.5, hashbrown 0.17.1, heck 0.5.0, http 1.5.0, httparse 1.10.1, httpdate 1.0.3, hyper-timeout 0.5.2, is_terminal_polyfill 1.70.2, itertools 0.15.0, itoa 1.0.19, jobserver 0.1.35, libc 0.2.191, lock_api 0.4.14, log 0.4.35, md-5 0.10.6, mime 0.3.18, multimap 0.10.1, num-traits 0.2.19, once_cell 1.21.4, once_cell_polyfill 1.70.2, openssl-probe 0.2.2, parking_lot 0.12.5, parking_lot_core 0.9.12, percent-encoding 2.3.2, petgraph 0.8.3, pkg-config 0.3.34, prettyplease 0.2.37, prettyplease 0.3.0, proc-macro2 1.0.107, prost-reflect 0.16.5, quote 1.0.47, rand_core 0.9.5, rand_xoshiro 0.7.0, regex 1.13.2, regex-automata 0.4.19, regex-syntax 0.8.12, rustls-pki-types 1.16.0, rustversion 1.0.23, scopeguard 1.2.0, security-framework 3.7.0, security-framework-sys 2.17.0, serde 1.0.229, serde_core 1.0.229, serde_derive 1.0.229, serde_json 1.0.154, sha1 0.10.7, sha2 0.10.9, shlex 2.0.1, signal-hook-registry 1.4.8, siphasher 1.0.4, smallvec 1.16.6, socket2 0.6.5, syn 2.0.119, syn 3.0.10, tempfile 3.27.0, thiserror 2.0.21, thiserror-impl 2.0.21, tokio-rustls 0.26.6, typenum 1.20.1, unicase 2.10.0, windows-link 0.2.1, windows-sys 0.52.0, windows-sys 0.61.2, windows-targets 0.52.6, windows_aarch64_gnullvm 0.52.6, windows_aarch64_msvc 0.52.6, windows_i686_gnu 0.52.6, windows_i686_gnullvm 0.52.6, windows_i686_msvc 0.52.6, windows_x86_64_gnu 0.52.6, windows_x86_64_gnullvm 0.52.6, windows_x86_64_msvc 0.52.6 |
| MIT | archery 1.2.3, axum 0.8.9, axum-core 0.5.6, bytes 1.12.2, dashmap 6.2.1, generic-array 0.14.7, h2 0.4.20, http-body 1.1.0, http-body-util 0.1.5, hyper 1.12.0, hyper-util 0.1.21, mio 1.2.5, ordered-float 2.10.1, phf 0.12.1, phf_shared 0.12.1, protoc-bin-vendored 3.3.0 (and its eight per-platform `protoc-bin-vendored-*` 3.3.0 crates: build-only, protoc is not shipped), pulldown-cmark 0.13.6, redox_syscall 0.5.18, schannel 0.1.30, serde-value 0.7.0, slab 0.4.12, strsim 0.11.1, tokio 1.54.1, tokio-macros 2.7.2, tokio-stream 0.1.19, tokio-util 0.7.20, tonic 0.14.9, tonic-build 0.14.9, tonic-prost 0.14.9, tonic-prost-build 0.14.9, tower 0.5.3, tower-layer 0.3.3, tower-service 0.3.3, tracing 0.1.44, tracing-attributes 0.1.31, tracing-core 0.1.36, try-lock 0.2.5, want 0.3.2, zmij 1.0.23 |
| Apache-2.0 OR MIT | atomic-waker 1.1.2, autocfg 1.5.1, equivalent 1.0.3, fastrand 2.5.0, indexmap 2.14.2, pin-project 1.1.13, pin-project-internal 1.1.13, pin-project-lite 0.2.17, utf8parse 0.2.2, zeroize 1.9.2 |
| Apache-2.0 | prost 0.14.6, prost-build 0.14.6, prost-derive 0.14.6, prost-types 0.14.6, pulldown-cmark-to-cmark 22.0.3, sync_wrapper 1.0.2 |
| Zlib OR Apache-2.0 OR MIT | bytemuck 1.25.2, safe_arch 0.7.4, tinyvec 1.13.3, wide 0.7.33 |
| Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | linux-raw-sys 0.12.1, rustix 1.1.6, wasi 0.11.1+wasi-snapshot-preview1 |
| BSD-3-Clause | subtle 2.6.1, zstd 0.14.4, zstd-safe 8.3.0 |
| (MIT OR Apache-2.0) AND Unicode-3.0 | unicode-ident 1.0.27, unicode-normalization 0.1.26 |
| Apache-2.0 OR ISC OR MIT | rustls 0.23.46, rustls-native-certs 0.8.5 |
| ISC | rustls-webpki 0.103.17, untrusted 0.9.0 |
| MPL-2.0+ | imbl 7.0.2, imbl-sized-chunks 0.2.0 |
| Unlicense OR MIT | aho-corasick 1.1.5, memchr 2.8.3 |
| Apache-2.0 / MIT | fnv 1.0.7 |
| Apache-2.0 AND ISC | ring 0.17.14 |
| BSD-3-Clause AND BSD-2-Clause | zstd-sys 2.1.3+zstd.1.5.7 |
| MIT AND BSD-3-Clause | matchit 0.8.4 |
| MIT OR Apache-2.0 OR LGPL-2.1-or-later | r-efi 6.0.0 |
| MIT/Apache-2.0 | version_check 0.9.5 |
| Zlib | foldhash 0.1.5 |
