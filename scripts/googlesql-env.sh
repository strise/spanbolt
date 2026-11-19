# Source to build googlesql-sys against a local GoogleSQL shim prefix (bin/ + src/) instead of the
# pinned prebuilt download. Populate the prefix from a Bazel build (docs/building.md); without one,
# nothing is exported and googlesql-sys/build.rs uses native/googlesql/bazel-bin or downloads the
# archive pinned in native/googlesql-shim.env.
export GOOGLESQL_PREFIX="${GOOGLESQL_PREFIX:-$HOME/.cache/spanbolt/googlesql-2026.11.2-shim6-sharedcat}"
if [ -f "$GOOGLESQL_PREFIX/bin/libgooglesql_shim.so" ]; then
  export GOOGLESQL_SHIM_DIR="$GOOGLESQL_PREFIX/bin"
  export GOOGLESQL_SRC_DIR="$GOOGLESQL_PREFIX/src"
else
  unset GOOGLESQL_PREFIX GOOGLESQL_SHIM_DIR GOOGLESQL_SRC_DIR
fi
