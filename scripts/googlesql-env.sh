# Source to build googlesql-sys against a local GoogleSQL shim prefix (bin/ + src/) instead of the
# pinned prebuilt download. Without a usable prefix nothing is exported and googlesql-sys/build.rs
# uses native/googlesql/bazel-bin or downloads the archive pinned in native/googlesql-shim.env.
#
# The prefix is GOOGLESQL_PREFIX, default <cache>/local (SPANBOLT_CACHE_DIR, else
# $XDG_CACHE_HOME/spanbolt or ~/.cache/spanbolt). Populate it by unpacking an archive from
# scripts/package-shim.sh (docs/building.md). It is used only when its MANIFEST names the shim
# sources of this tree: a prefix built from other sources links against missing or changed symbols,
# so it is skipped with a warning and the build falls back to the pinned shim.
GOOGLESQL_PREFIX="${GOOGLESQL_PREFIX:-${SPANBOLT_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/spanbolt}/local}"
unset GOOGLESQL_SHIM_DIR GOOGLESQL_SRC_DIR
if [ -f "$GOOGLESQL_PREFIX/bin/libgooglesql_shim.so" ]; then
  _gsql_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  _gsql_want="$("$_gsql_root/scripts/package-shim.sh" --source-hash)"
  _gsql_have="$(sed -n 's/^SHIM_SOURCE_SHA256=//p' "$GOOGLESQL_PREFIX/MANIFEST" 2>/dev/null | tail -1)"
  if [ -n "$_gsql_want" ] && [ "$_gsql_have" = "$_gsql_want" ]; then
    export GOOGLESQL_PREFIX
    export GOOGLESQL_SHIM_DIR="$GOOGLESQL_PREFIX/bin"
    export GOOGLESQL_SRC_DIR="$GOOGLESQL_PREFIX/src"
  else
    echo "googlesql-env: skipping $GOOGLESQL_PREFIX (MANIFEST SHIM_SOURCE_SHA256=${_gsql_have:-<none>}," \
      "shim sources hash to $_gsql_want); using the pinned shim. See docs/building.md." >&2
    unset GOOGLESQL_PREFIX
  fi
  unset _gsql_root _gsql_want _gsql_have
else
  unset GOOGLESQL_PREFIX
fi
