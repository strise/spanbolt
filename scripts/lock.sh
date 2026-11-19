#!/usr/bin/env bash
# Machine-wide FIFO mutex for heavy jobs (Bazel, docker build, large test suites, long benchmarks).
# Usage: scripts/lock.sh <name> -- <command...>
# Waiters take a ticket; only the oldest live ticket may take the lock, so a job that releases
# and immediately locks again goes to the back of the queue.
set -euo pipefail
name="$1"; shift
[ "${1:-}" = "--" ] && shift
dir="${TMPDIR:-/tmp}/spanbolt-lock-$name"
queue="$dir.queue"
mkdir -p "$queue"
me="$(perl -MTime::HiRes=time -e 'printf "%d", time*1e6')-$$"
echo "$*" > "$queue/$me"
trap 'rm -f "$queue/$me"' EXIT

oldest_live() {
  local t
  for t in $(ls "$queue" 2>/dev/null | sort -n); do
    if kill -0 "${t##*-}" 2>/dev/null; then echo "$t"; return; fi
    rm -f "$queue/$t"   # stale: waiter died
  done
}

waited=0
until [ "$(oldest_live)" = "$me" ] && mkdir "$dir" 2>/dev/null; do
  holder=$(cat "$dir/pid" 2>/dev/null || true)
  if [ -n "$holder" ] && ! kill -0 "$holder" 2>/dev/null; then
    rm -rf "$dir"   # stale: holder died
    continue
  fi
  if [ $((waited % 60)) -eq 0 ]; then
    pos=$(ls "$queue" | sort -n | grep -n -- "$me" | cut -d: -f1 || true)
    echo "lock.sh: waiting for '$name' (queue position ${pos:-?}; held by pid ${holder:-?}: $(cat "$dir/cmd" 2>/dev/null || true))" >&2
  fi
  sleep 5; waited=$((waited + 5))
done
rm -f "$queue/$me"
echo $$ > "$dir/pid"
echo "$*" > "$dir/cmd"
trap 'rm -rf "$dir"' EXIT
"$@"
