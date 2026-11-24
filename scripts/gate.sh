#!/usr/bin/env bash
# Merge gate. Run in a worktree before handing over, and by the coordinator before merging.
#   scripts/gate.sh            fmt, clippy, tests, conformance twice: default server vs baseline-prod.json (check --oracle spanner), --emulator-compat vs baseline.json
#   GATE_EXTRA='<command>'     also run this command last (through the shell); it fails the gate if it fails
#   GATE_BASE=origin/main      the ref recorded expectations are compared against (default main)
set -uo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)
if [ $# -gt 0 ]; then
  echo "gate.sh takes no arguments; run extra checks with GATE_EXTRA='<command>'" >&2
  exit 2
fi
out="$root/.mcp-output/gate"; mkdir -p "$out"
fail=0
step() { printf '\n== %s\n' "$1"; }
check() { if "$@"; then echo "ok"; else echo "FAILED: $*"; fail=1; fi; }

# shellcheck source=/dev/null
source scripts/googlesql-env.sh

step "fmt";    check cargo fmt --all --check
step "clippy"; check cargo clippy -q --all-targets -- -D warnings
step "clippy (googlesql)"
check cargo clippy -q --all-targets -p spanbolt-sql -p spanbolt-server -p spanbolt --features spanbolt-server/googlesql -- -D warnings
step "tests";  check cargo test -q
step "sql tests (release)"; check cargo test -q --release -p spanbolt-sql

step "crate licenses (about.toml)"; check python3 scripts/check-licenses.py

step "prebuilt shim pin"
# A warning, not a failure: after a shim change the archives need a Bazel build per platform.
scripts/package-shim.sh --check || echo "WARNING: clean clones get a stale or no prebuilt shim until it is repackaged"

step "recorded expectations untouched"
base=$(git merge-base HEAD "${GATE_BASE:-main}" 2>/dev/null || echo "${GATE_BASE:-main}")
# Only additions are allowed in case files; deleted or changed lines need an explicit sign-off.
removed=$(git diff "$base" -- conformance/cases | grep -E '^-[^-]' | grep -v -E '^-\s*$' || true)
if [ -n "$removed" ] && ! git log "$base"..HEAD --format=%B | grep -q '\[expectation-edit\]'; then
  echo "FAILED: existing conformance lines changed (needs a commit tagged [expectation-edit] and a reason):"
  echo "$removed" | head -20
  fail=1
else
  echo "ok"
fi

step "conformance"
cargo build -q --release -p spanbolt && cargo build -q --release -p spanbolt-conformance || fail=1
# Two runs: the default server against prod expectations (`expect@spanner:` where recorded),
# and the server in --emulator-compat mode against the emulator's expectations.
run_conformance() { # <label> <baseline> <server flags> -- <check flags>
  local label="$1" baseline="$2" sflags="$3" cflags="$4"
  local port=$((20000 + RANDOM % 20000))
  ./target/release/spanbolt --host-port "127.0.0.1:$port" $sflags > "$out/server-$label.log" 2>&1 &
  local srv=$!
  for _ in $(seq 1 200); do nc -z 127.0.0.1 "$port" 2>/dev/null && break; sleep 0.05; done
  ./target/release/spanbolt-conformance check --target "127.0.0.1:$port" $cflags --json "$out/conformance-$label.json" > "$out/conformance-$label.txt" 2>&1
  kill "$srv" 2>/dev/null
  echo "[$label] $(tail -1 "$out/conformance-$label.txt")"
  python3 scripts/conformance_compare.py "$baseline" "$out/conformance-$label.json" || fail=1
}
run_conformance prod conformance/baseline-prod.json "" "--oracle spanner"
run_conformance emulator conformance/baseline.json "--emulator-compat" ""

if [ -n "${GATE_EXTRA:-}" ]; then
  step "extra: $GATE_EXTRA"
  check bash -c "$GATE_EXTRA"
fi

echo
if [ $fail -eq 0 ]; then echo "GATE PASSED"; else echo "GATE FAILED"; fi
exit $fail
