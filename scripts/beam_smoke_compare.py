#!/usr/bin/env python3
"""Compare two SpannerIOSmoke runs (scripts/beam-smoke.sh): every field but the timing must match.
Usage: beam_smoke_compare.py EMULATOR.jsonl SPANBOLT.jsonl. Exits 1 on any difference."""
import json
import sys


def load(path):
    try:
        with open(path) as f:
            return {r["step"]: r for r in map(json.loads, filter(str.strip, f))}
    except FileNotFoundError:
        return {}


emu, sb = load(sys.argv[1]), load(sys.argv[2])
steps = list(emu) + [s for s in sb if s not in emu]
diffs = 0
tot_e = tot_s = 0
print(f"{'step':<48} {'rows':>6} {'emulator':>9} {'spanbolt':>9}  result")
for step in steps:
    e, s = emu.get(step), sb.get(step)
    if e is None or s is None:
        diffs += 1
        print(f"{step:<48} {'':>6} {'-' if e is None else e['ms']:>9} {'-' if s is None else s['ms']:>9}  MISSING")
        continue
    tot_e += e["ms"]
    tot_s += s["ms"]
    ke = {k: v for k, v in e.items() if k != "ms"}
    ks = {k: v for k, v in s.items() if k != "ms"}
    ok = ke == ks
    n = e.get("rows", e.get("mutations", e.get("groups", "")))
    print(f"{step:<48} {n!s:>6} {e['ms']:>7}ms {s['ms']:>7}ms  {'same' if ok else 'DIFF'}")
    if not ok:
        diffs += 1
        for k in sorted(set(ke) | set(ks)):
            if ke.get(k) != ks.get(k):
                print(f"    {k}: emulator={ke.get(k)!r}\n    {'':{len(k)}}  spanbolt={ks.get(k)!r}")
print(f"{'total':<48} {'':>6} {tot_e:>7}ms {tot_s:>7}ms  {diffs} difference(s)")
sys.exit(1 if diffs else 0)
