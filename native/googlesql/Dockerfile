# Builds libgooglesql_shim.so for Linux. Context: native/googlesql.
#   docker build --platform linux/amd64 -f Dockerfile -t spanbolt-googlesql .
#   docker create --name x spanbolt-googlesql && docker cp x:/out ./out-linux && docker rm x
# The BuildKit cache mount keeps Bazel's repository + disk cache between builds.
FROM ubuntu:24.04 AS build
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential clang-18 lld-18 ca-certificates curl git python3 unzip zip tzdata openjdk-21-jdk-headless \
    && rm -rf /var/lib/apt/lists/*
ARG BAZELISK_VERSION=v1.30.0
ARG TARGETARCH
RUN curl -fsSL -o /usr/local/bin/bazel \
      https://github.com/bazelbuild/bazelisk/releases/download/${BAZELISK_VERSION}/bazelisk-linux-${TARGETARCH} \
    && chmod +x /usr/local/bin/bazel
# clang: gcc 13 needs >2 GB per process on googlesql's generated parser and OOMs a 16 GB VM even
# at 6 jobs; clang OOMs at 4 jobs on 16 GB: resolved_ast_rewrite_visitor_*.cc need ~5+ GB each. Use ~RAM/6 GB.
ARG BAZEL_JOBS=2
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/root/.cache \
    bazel build --jobs=${BAZEL_JOBS} --repo_env=CC=clang-18 --repo_env=CXX=clang++-18 --disk_cache=/root/.cache/spanbolt-bazel-disk //:libgooglesql_shim.so \
    && mkdir -p /out \
    && cp bazel-bin/libgooglesql_shim.so /out/ \
    && cp -rL bazel-bin/external/googlesql+/googlesql/resolved_ast /out/gen-resolved_ast \
    && tar -C bazel-src/external/googlesql+ -cf /out/googlesql-protos.tar $(cd bazel-src/external/googlesql+ && find googlesql -name '*.proto')

FROM scratch
COPY --from=build /out /out
