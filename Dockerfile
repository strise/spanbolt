# syntax=docker/dockerfile:1.7
# spanbolt: a drop-in replacement for gcr.io/cloud-spanner-emulator/emulator.
#   docker build -t spanbolt .                          # native arch; see docs/docker.md
#   docker build --build-arg SHIM=prebuilt -t spanbolt .   # pinned shim archive, no C++ build
#   docker run --rm -p 9010:9010 -p 9020:9020 spanbolt
# Stages: shim (GoogleSQL C++: Bazel, or the archive pinned in native/googlesql-shim.env)
# -> build (Rust) -> runtime. Both linux/amd64 and linux/arm64 (docs/building.md).

ARG UBUNTU=ubuntu:24.04
ARG RUST_IMAGE=rust:1.91-trixie
# distroless cc (glibc, libstdc++, libgcc_s, tzdata). debian12 is too old for the shim, which needs
# glibc 2.38 and GLIBCXX_3.4.31 (built on Ubuntu 24.04).
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot
# Where the shim comes from: bazel (build it here, 40 min cold) or prebuilt (download the pinned
# archive, or take it from --build-context shim-archives=dist).
ARG SHIM=bazel

# ---- shim: libgooglesql_shim.so and the protos googlesql-sys generates Rust from ----
# Output: /out/bin/libgooglesql_shim.so, /out/bin/external/googlesql+/... (generated protos),
# /out/src/googlesql/... (source protos): the layout scripts/googlesql-env.sh expects.
# Replace the shim stage with a prebuilt one via `--build-context shim=docker-image://<image>`.
FROM ${UBUNTU} AS shim-tools
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential clang-18 lld-18 ca-certificates curl git python3 unzip zip openjdk-21-jdk-headless \
    && rm -rf /var/lib/apt/lists/*
ARG BAZELISK_VERSION=v1.30.0
ARG TARGETARCH
RUN curl -fsSL -o /usr/local/bin/bazel \
      https://github.com/bazelbuild/bazelisk/releases/download/${BAZELISK_VERSION}/bazelisk-linux-${TARGETARCH} \
    && chmod +x /usr/local/bin/bazel

FROM shim-tools AS shim-build
ARG TARGETARCH
# Memory, not CPU, bounds this build: googlesql's generated sources need 5+ GB per compile job.
# Use about RAM / 6 GB (2 on a 16 GB Docker VM, 5 on a 32 GB runner).
ARG BAZEL_JOBS=2
WORKDIR /src
COPY native/googlesql/ .
# The cache mount holds Bazel's output base and disk cache, per architecture.
RUN --mount=type=cache,id=spanbolt-bazel-${TARGETARCH},target=/root/.cache \
    bazel build --jobs=${BAZEL_JOBS} --repo_env=CC=clang-18 --repo_env=CXX=clang++-18 \
      --disk_cache=/root/.cache/spanbolt-bazel-disk //:libgooglesql_shim.so \
    && gen=bazel-bin/external/googlesql+ \
    && src=bazel-src/external/googlesql+ \
    && mkdir -p /out/bin/external/googlesql+ /out/src \
    && cp bazel-bin/libgooglesql_shim.so /out/bin/ \
    && (cd "$gen" && find googlesql -name '*.proto' | tar -cf - -T - --dereference) \
       | tar -C /out/bin/external/googlesql+ -xf - \
    && (cd "$src" && find googlesql -name '*.proto' | tar -cf - -T - --dereference) \
       | tar -C /out/src -xf -

# License texts of the native libraries in the shim, from the same Bazel repositories. A stage of
# its own that only fetches (a minute), so changing it never invalidates the 40-minute build above.
FROM shim-tools AS shim-licenses
WORKDIR /src
COPY native/googlesql/ .
COPY scripts/native-licenses.sh /usr/local/bin/
RUN bazel fetch --repo=@@googlesql+ --repo=@@abseil-cpp+ --repo=@@protobuf+ --repo=@@re2+ \
      --repo=@@nlohmann_json+ --repo=@@googlesql++googlesql_http_archive_deps+icu \
    && native-licenses.sh "$(bazel info output_base)/external" /out/licenses/native

# bin/resolved_ast.fds, as in a prebuilt archive (scripts/package-shim.sh), so the build stage never
# needs protoc. googlesql's protos use edition 2023: protoc 27+ (Debian ships 3.21).
FROM ${RUST_IMAGE} AS shim-fds
ARG TARGETARCH
ARG PROTOC_VERSION=31.1
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends unzip \
    && rm -rf /var/lib/apt/lists/* \
    && case "${TARGETARCH}" in arm64) a=aarch_64 ;; amd64) a=x86_64 ;; *) echo "unsupported ${TARGETARCH}"; exit 1 ;; esac \
    && curl -fsSL -o /tmp/protoc.zip \
       "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-${a}.zip" \
    && unzip -q /tmp/protoc.zip -d /usr/local bin/protoc 'include/*' && rm /tmp/protoc.zip
COPY --from=shim-build /out /shim
RUN mkdir -p /out/bin && protoc --include_imports --descriptor_set_out=/out/bin/resolved_ast.fds \
      -I/shim/bin/external/googlesql+ -I/shim/src \
      /shim/bin/external/googlesql+/googlesql/resolved_ast/resolved_ast.proto

FROM scratch AS shim-bazel
COPY --from=shim-build /out /out
COPY --from=shim-fds /out/bin/resolved_ast.fds /out/bin/
COPY --from=shim-licenses /out/licenses /out/licenses

# ---- prebuilt shim: the archive pinned in native/googlesql-shim.env, checksum-verified ----
# Archives in the shim-archives context (default empty) win over the download.
FROM scratch AS shim-archives
FROM ${RUST_IMAGE} AS shim-fetch
ARG TARGETARCH
# Base URL holding the archives; default: the GitHub release named in googlesql-shim.env.
ARG SPANBOLT_SHIM_URL=
COPY native/googlesql-shim.env /googlesql-shim.env
COPY --from=shim-archives / /archives/
RUN set -eu; . /googlesql-shim.env \
    && case "${TARGETARCH}" in arm64) p=linux-arm64 ;; amd64) p=linux-x86_64 ;; *) echo "unsupported ${TARGETARCH}"; exit 1 ;; esac \
    && name="googlesql-shim-${GOOGLESQL_VERSION}-s${SHIM_VERSION}-${p}.tar.gz" \
    && eval "sha=\${SHA256_$(echo "$p" | tr - _)}" \
    && if [ -z "$sha" ]; then echo "no prebuilt shim pinned for $p in googlesql-shim.env; use SHIM=bazel"; exit 1; fi \
    && if [ -f "/archives/$name" ]; then cp "/archives/$name" /tmp/shim.tar.gz; \
       else curl -fsSL --retry 3 -o /tmp/shim.tar.gz \
         "${SPANBOLT_SHIM_URL:-${SHIM_RELEASE_URL}/shim-${GOOGLESQL_VERSION}-s${SHIM_VERSION}}/$name"; fi \
    && echo "$sha  /tmp/shim.tar.gz" | sha256sum -c - \
    && mkdir /out && tar -xzf /tmp/shim.tar.gz -C /out && rm /tmp/shim.tar.gz

FROM scratch AS shim-prebuilt
COPY --from=shim-fetch /out /out

FROM shim-${SHIM} AS shim

# ---- cargo-about: renders the Rust crates' license texts (about.toml, about.hbs) ----
FROM ${RUST_IMAGE} AS cargo-about
ARG CARGO_ABOUT_VERSION=0.9.3
RUN cargo install --locked --features cli cargo-about --version ${CARGO_ABOUT_VERSION}

# ---- build: the spanbolt binary with the GoogleSQL engine ----
FROM ${RUST_IMAGE} AS build
ARG TARGETARCH
# No protoc: spanbolt-proto uses the vendored one (protoc-bin-vendored), and both shim stages carry
# bin/resolved_ast.fds for googlesql-sys.
COPY --from=shim /out /opt/googlesql
ENV GOOGLESQL_PREFIX=/opt/googlesql
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY conformance/ conformance/
COPY fixtures/ fixtures/
COPY --from=cargo-about /usr/local/cargo/bin/cargo-about /usr/local/cargo/bin/
COPY LICENSE NOTICE THIRD_PARTY_NOTICES.md about.toml about.hbs ./
# The target/ cache mount is shared by every build on the host, from any checkout or branch, and
# cargo trusts a crate when its sources are older than the cached artifact. COPY keeps the
# context's mtimes, so a file last changed before some other build ran looked fresh and the image
# shipped a stale crate (a spanbolt-storage without implicit TOKENLIST materialisation). Touching
# the workspace sources forces the workspace crates to rebuild; dependencies stay cached.
RUN --mount=type=cache,id=spanbolt-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=spanbolt-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=spanbolt-target-${TARGETARCH},target=/src/target,sharing=locked \
    find crates -type f -exec touch {} + \
    && cargo build --release --locked -p spanbolt-server --features googlesql \
    && mkdir -p /dist \
    && cp target/release/spanbolt /dist/ \
    && cp /opt/googlesql/bin/libgooglesql_shim.so /dist/ \
    && strip /dist/spanbolt \
    && strip --strip-unneeded /dist/libgooglesql_shim.so \
    && mkdir -p /dist/licenses \
    && cp LICENSE NOTICE THIRD_PARTY_NOTICES.md /dist/licenses/ \
    && cp -R /opt/googlesql/licenses/native /dist/licenses/native \
    && cargo fetch --locked \
    && cargo about generate --frozen --all-features --fail about.hbs -o /dist/licenses/THIRD_PARTY_LICENSES.txt \
    && mkdir -p /data

# ---- runtime ----
FROM ${RUNTIME_IMAGE} AS runtime
# The binary loads libgooglesql_shim.so from its own directory ($ORIGIN rpath). tzdata (Spanner's
# default time zone is America/Los_Angeles) comes with the distroless base. /opt/spanbolt/licenses:
# LICENSE, NOTICE, THIRD_PARTY_NOTICES.md, THIRD_PARTY_LICENSES.txt (Rust crates) and native/.
COPY --from=build /dist/ /opt/spanbolt/
COPY --from=build --chown=65532:65532 /data /data
ENV PATH=/opt/spanbolt:/usr/local/bin:/usr/bin:/bin
USER nonroot
WORKDIR /data
EXPOSE 9010 9020
# SPANBOLT_SNAPSHOT=/path/seed.snap loads a snapshot at start (see Dockerfile.seeded).
ENTRYPOINT ["/opt/spanbolt/spanbolt"]
CMD ["--host-port", "0.0.0.0:9010", "--rest-port", "9020"]
