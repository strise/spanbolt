#!/usr/bin/env bash
# Beam SpannerIO smoke test: runs crates/conformance/beam against this worktree's
# spanbolt and against the official emulator, then compares the two runs step by step.
# Heavy (Maven + a Docker container): run it as
#   scripts/lock.sh heavy -- scripts/beam-smoke.sh
# Env: SPANBOLT_ONLY=1 skips the emulator; EMULATOR_IMAGE overrides the emulator image.
set -uo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)
out="$root/.mcp-output/beam"; mkdir -p "$out"
export JAVA_HOME="${JAVA_HOME_21:-$(/usr/libexec/java_home -v 21)}"
image="${EMULATOR_IMAGE:-gcr.io/cloud-spanner-emulator/emulator:1.5.59}"

(cd crates/conformance/beam && mvn -q -B compile dependency:build-classpath \
  -Dmdep.outputFile=target/classpath.txt) || exit 1
cp="crates/conformance/beam/target/classes:$(cat crates/conformance/beam/target/classpath.txt)"
run() {  # run <label> <host:port>
  "$JAVA_HOME/bin/java" -Xmx2g -Dorg.slf4j.simpleLogger.defaultLogLevel=warn \
    --add-opens=java.base/java.util=ALL-UNNAMED --add-opens=java.base/java.lang=ALL-UNNAMED \
    -cp "$cp" spanbolt.beam.SpannerIOSmoke "$2" "$out/$1.jsonl" > "$out/$1.log" 2>&1
  local rc=$?
  echo "$1: exit $rc ($(wc -l < "$out/$1.jsonl" 2>/dev/null || echo 0) steps), log $out/$1.log"
}

source scripts/googlesql-env.sh
cargo build -q --release -p spanbolt || exit 1
port=$((20000 + RANDOM % 20000))
./target/release/spanbolt --host-port "127.0.0.1:$port" > "$out/spanbolt-server.log" 2>&1 &
srv=$!
cname="sb-beam-smoke-$$"
trap 'kill $srv 2>/dev/null; docker rm -f "$cname" >/dev/null 2>&1' EXIT
for _ in $(seq 1 200); do nc -z 127.0.0.1 "$port" 2>/dev/null && break; sleep 0.05; done
run spanbolt "127.0.0.1:$port"
kill $srv 2>/dev/null

if [ -z "${SPANBOLT_ONLY:-}" ]; then
  eport=$((20000 + RANDOM % 20000))
  docker run -d --name "$cname" -p "$eport:9010" "$image" > /dev/null || exit 1
  for _ in $(seq 1 600); do nc -z 127.0.0.1 "$eport" 2>/dev/null && break; sleep 0.1; done
  sleep 1
  run emulator "127.0.0.1:$eport"
  python3 scripts/beam_smoke_compare.py "$out/emulator.jsonl" "$out/spanbolt.jsonl"
fi
