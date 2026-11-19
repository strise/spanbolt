#!/usr/bin/env python3
"""Fail if any case that passes in the baseline no longer passes. New cases are reported, not judged."""
import json, sys

base = {c["name"]: c["status"] for c in json.load(open(sys.argv[1]))["cases"]}
now = {c["name"]: c["status"] for c in json.load(open(sys.argv[2]))["cases"]}
regressed = sorted(n for n, s in base.items() if s == "pass" and now.get(n) != "pass")
fixed = sorted(n for n, s in now.items() if s == "pass" and base.get(n) not in (None, "pass"))
new = sorted(n for n in now if n not in base)
new_pass = sum(1 for n in new if now[n] == "pass")
print(f"conformance: {sum(s == 'pass' for s in now.values())}/{len(now)} pass; "
      f"{len(fixed)} newly passing, {len(new)} new cases ({new_pass} pass), {len(regressed)} regressions")
for n in regressed[:30]:
    print(f"  REGRESSION {n}: {base[n]} -> {now.get(n, 'missing')}")
sys.exit(1 if regressed else 0)
