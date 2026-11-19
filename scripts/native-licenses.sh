#!/usr/bin/env bash
# Copies the license texts of the native code linked into libgooglesql_shim out of a Bazel build.
#
#   scripts/native-licenses.sh EXTERNAL OUT
#       EXTERNAL  the Bazel external repositories: native/googlesql/bazel-googlesql/external,
#                 or $(bazel info output_base)/external
#       OUT       created if needed; gets <component>/LICENSE per row below
#
# The list follows THIRD_PARTY_NOTICES.md ("Native library"). Fails if a file is missing, so a
# renamed repository or license file stops the shim archive and the image instead of shipping
# without it. Used by scripts/package-shim.sh and the Dockerfile's shim stage.
set -euo pipefail
[ $# -eq 2 ] || { sed -n '2,10p' "$0" >&2; exit 2; }
ext="$1" out="$2"

# <component>  <file in EXTERNAL>
licenses=(
  googlesql      googlesql+/LICENSE
  abseil-cpp     abseil-cpp+/LICENSE
  protobuf       protobuf+/LICENSE
  utf8_range     protobuf+/third_party/utf8_range/LICENSE
  re2            re2+/LICENSE
  icu            googlesql++googlesql_http_archive_deps+icu/LICENSE
  nlohmann_json  nlohmann_json+/LICENSE.MIT
)

missing=0
for ((i = 0; i < ${#licenses[@]}; i += 2)); do
  name=${licenses[i]} file="$ext/${licenses[i + 1]}"
  if [ ! -f "$file" ]; then
    echo "native-licenses: missing $file ($name)" >&2
    missing=1
    continue
  fi
  mkdir -p "$out/$name"
  # cat, not cp: Bazel's repository files are read-only symlinks.
  cat "$file" > "$out/$name/LICENSE"
done
[ $missing -eq 0 ] || exit 1
cat > "$out/README" <<'EOF'
License texts of the native libraries linked into libgooglesql_shim (THIRD_PARTY_NOTICES.md),
copied from the Bazel repositories the shim was built from:
  googlesql      GoogleSQL                                Apache-2.0
  abseil-cpp     Abseil, including CCTZ                   Apache-2.0
  protobuf       Protocol Buffers                         BSD-3-Clause
  utf8_range     utf8_range (part of protobuf)            MIT
  re2            RE2                                      BSD-3-Clause
  icu            ICU, code and data                       Unicode-3.0 (ICU License)
  nlohmann_json  nlohmann/json                            MIT
EOF
