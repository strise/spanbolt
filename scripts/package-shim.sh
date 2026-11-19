#!/usr/bin/env bash
# Packages a built GoogleSQL shim as the prebuilt archive googlesql-sys/build.rs downloads.
#
#   scripts/package-shim.sh [--from DIR] [--platform P] [--out DIR] [--licenses DIR] [--pin]
#       --from DIR    native/googlesql after `bazelisk build //:libgooglesql_shim.so` (default), or a
#                     prefix with bin/ + src/ + licenses/native/ (the Docker shim stage, an unpacked
#                     archive)
#       --licenses DIR  the native license texts (scripts/native-licenses.sh output), for a prefix
#                     without licenses/native/; default: collected from the Bazel tree's external repos
#       --platform P  darwin-arm64 | linux-arm64 | linux-x86_64 (default: this machine)
#       --out DIR     where the archive and its .sha256 go (default: dist/)
#       --pin         write SHA256_<platform> and SHIM_SOURCE_SHA256 into native/googlesql-shim.env
#   scripts/package-shim.sh --source-hash   print the hash of the shim's sources
#   scripts/package-shim.sh --check         fail if googlesql-shim.env's SHIM_SOURCE_SHA256 is stale
#   scripts/package-shim.sh --pin-sums DIR  pin every <archive>.sha256 in DIR (e.g. downloaded from the
#                                           release CI built), plus this tree's SHIM_SOURCE_SHA256
#
# Archive googlesql-shim-<GOOGLESQL_VERSION>-s<SHIM_VERSION>-<platform>.tar.gz, no top directory:
#   MANIFEST                                versions, platform, source hash, protoc version
#   bin/libgooglesql_shim.so                the shim (Mach-O dylib on darwin, ELF on linux)
#   bin/resolved_ast.fds                    FileDescriptorSet of resolved_ast.proto with its imports,
#                                           so building against the archive needs no protoc
#   bin/external/googlesql+/googlesql/**.proto   generated protos (resolved_ast.proto, ...)
#   src/googlesql/**.proto                  googlesql's hand-written protos
#   licenses/LICENSE, NOTICE, THIRD_PARTY_NOTICES.md   spanbolt's (the shim is spanbolt code)
#   licenses/native/<component>/LICENSE     GoogleSQL, Abseil, protobuf, utf8_range, RE2, ICU,
#                                           nlohmann/json (scripts/native-licenses.sh)
# Needs protoc 27+ (edition-2023 imports).
set -euo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)
native="$root/native/googlesql"
pins="$root/native/googlesql-shim.env"
# Keep in sync with SHIM_SOURCES in crates/googlesql-sys/build.rs.
sources=(.bazelrc .bazelversion BUILD.bazel MODULE.bazel shim tools)

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
pin() { sed -n "s/^$1=//p" "$pins" | tail -1; }
source_hash() {
  (cd "$native" && find "${sources[@]}" -type f | LC_ALL=C sort | while read -r f; do sha256 "$f"; done) \
    | sha256 | cut -d' ' -f1
}
set_pin() { # <key> <value>
  local tmp; tmp=$(mktemp)
  awk -v k="$1" -v v="$2" 'BEGIN{FS=OFS="="} $1==k {print k "=" v; next} {print}' "$pins" > "$tmp"
  cat "$tmp" > "$pins"; rm -f "$tmp"
}

from="$native" platform="" outdir="$root/dist" do_pin=false licenses=""
while [ $# -gt 0 ]; do
  case "$1" in
    --source-hash) source_hash; exit 0 ;;
    --check)
      want=$(pin SHIM_SOURCE_SHA256) have=$(source_hash)
      if [ "$want" = "$have" ]; then echo "googlesql-shim.env matches the shim sources"; exit 0; fi
      echo "googlesql-shim.env is stale: SHIM_SOURCE_SHA256=${want:-<empty>}, sources hash to $have." >&2
      echo "Bump SHIM_VERSION and package new archives (docs/building.md)." >&2
      exit 1 ;;
    --pin-sums)
      prefix="googlesql-shim-$(pin GOOGLESQL_VERSION)-s$(pin SHIM_VERSION)-"
      n=0
      for f in "$2"/"$prefix"*.tar.gz.sha256; do
        [ -f "$f" ] || continue
        read -r sum name < "$f"
        p=${name#"$prefix"}; p=${p%.tar.gz}
        set_pin "SHA256_${p//-/_}" "$sum"; echo "SHA256_${p//-/_}=$sum"; n=$((n + 1))
      done
      [ "$n" -gt 0 ] || { echo "no ${prefix}*.tar.gz.sha256 in $2" >&2; exit 1; }
      set_pin SHIM_SOURCE_SHA256 "$(source_hash)"
      exit 0 ;;
    --from) from="$2"; shift 2 ;;
    --platform) platform="$2"; shift 2 ;;
    --out) outdir="$2"; shift 2 ;;
    --licenses) licenses="$2"; shift 2 ;;
    --pin) do_pin=true; shift ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done

if [ -z "$platform" ]; then
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) platform=darwin-arm64 ;;
    Linux-aarch64|Linux-arm64) platform=linux-arm64 ;;
    Linux-x86_64) platform=linux-x86_64 ;;
    *) echo "unknown platform $(uname -s)-$(uname -m); pass --platform" >&2; exit 2 ;;
  esac
fi

gsql=$(pin GOOGLESQL_VERSION) shimv=$(pin SHIM_VERSION)
grep -q "version = \"$gsql\"" "$native/MODULE.bazel" \
  || { echo "GOOGLESQL_VERSION=$gsql is not the googlesql version in MODULE.bazel" >&2; exit 1; }

if [ -f "$from/bazel-bin/libgooglesql_shim.so" ]; then # a Bazel tree
  bin="$from/bazel-bin"
  src="$(cd "$from/bazel-googlesql/external/googlesql+" && pwd -P)"
  gen="$bin/external/googlesql+"
  external="$from/bazel-googlesql/external"
elif [ -f "$from/bin/libgooglesql_shim.so" ]; then # a prefix
  bin="$from/bin" src="$from/src" gen="$from/bin/external/googlesql+" external=""
  [ -n "$licenses" ] || licenses="$from/licenses/native"
else
  echo "$from: neither a Bazel tree with bazel-bin/libgooglesql_shim.so nor a prefix with bin/" >&2
  exit 1
fi

lib_kind=$(file -b "$bin/libgooglesql_shim.so")
case "$platform:$lib_kind" in
  darwin-arm64:*Mach-O*arm64*|linux-arm64:*ELF*aarch64*|linux-x86_64:*ELF*x86-64*) ;;
  *) echo "libgooglesql_shim.so is '$lib_kind', not $platform" >&2; exit 1 ;;
esac

stem="googlesql-shim-$gsql-s$shimv-$platform"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
stage="$work/stage"
mkdir -p "$stage/bin/external/googlesql+" "$stage/src"
cp "$bin/libgooglesql_shim.so" "$stage/bin/"
chmod 0755 "$stage/bin/libgooglesql_shim.so"
(cd "$gen" && find -L googlesql -name '*.proto' | LC_ALL=C sort | tar -cf - -T - -h) | tar -C "$stage/bin/external/googlesql+" -xf -
(cd "$src" && find -L googlesql -name '*.proto' | LC_ALL=C sort | tar -cf - -T - -h) | tar -C "$stage/src" -xf -
mkdir -p "$stage/licenses/native"
cp "$root/LICENSE" "$root/NOTICE" "$root/THIRD_PARTY_NOTICES.md" "$stage/licenses/"
if [ -n "$licenses" ]; then
  [ -f "$licenses/README" ] || { echo "$licenses: no native license texts (scripts/native-licenses.sh)" >&2; exit 1; }
  cp -R "$licenses/." "$stage/licenses/native/"
else
  "$root/scripts/native-licenses.sh" "$external" "$stage/licenses/native"
fi
chmod -R u+w "$stage"
protoc="${PROTOC:-protoc}"
"$protoc" --include_imports --descriptor_set_out="$stage/bin/resolved_ast.fds" \
  -I"$stage/bin/external/googlesql+" -I"$stage/src" \
  "$stage/bin/external/googlesql+/googlesql/resolved_ast/resolved_ast.proto"
src_hash=$(source_hash)
cat > "$stage/MANIFEST" <<EOF
GOOGLESQL_VERSION=$gsql
SHIM_VERSION=$shimv
PLATFORM=$platform
SHIM_SOURCE_SHA256=$src_hash
PROTOC=$("$protoc" --version)
EOF

mkdir -p "$outdir"
archive="$outdir/$stem.tar.gz"
# Sorted entries, fixed owner and mtime, no macOS metadata: repackaging the same files gives the
# same archive.
find "$stage" -exec touch -t 202001010000 {} +
if tar --version 2>/dev/null | grep -q bsdtar; then owner=(--uid 0 --gid 0); else owner=(--owner 0 --group 0 --numeric-owner); fi
(cd "$stage" && find . -type f | LC_ALL=C sort | sed 's|^\./||' > ../list \
  && COPYFILE_DISABLE=1 tar -cf - "${owner[@]}" -T ../list) | gzip -9n > "$archive"
sum=$(sha256 "$archive" | cut -d' ' -f1)
echo "$sum  $stem.tar.gz" > "$archive.sha256"
echo "$archive"
echo "sha256 $sum ($(du -h "$archive" | cut -f1))"

if $do_pin; then
  set_pin "SHA256_${platform//-/_}" "$sum"
  set_pin SHIM_SOURCE_SHA256 "$src_hash"
  echo "pinned SHA256_${platform//-/_} and SHIM_SOURCE_SHA256 in ${pins#"$root"/}"
fi
