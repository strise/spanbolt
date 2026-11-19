#!/usr/bin/env python3
"""Fails when a Rust crate's license is not covered by about.toml.

    python3 scripts/check-licenses.py [--metadata FILE]

Reads `cargo metadata --all-features` (the same crate set as THIRD_PARTY_NOTICES.md and
`cargo about generate`) and evaluates each registry crate's SPDX expression against `accepted`
in about.toml: an OR needs one accepted side, an AND needs both. `X WITH exception` and `X+`
count as X (an exception or a later version only adds permissions). Crates without a `license`
field need a `[<crate>.clarify]` entry in about.toml. Exit 1 lists every crate that fails.

No cargo-about needed, so it runs in the gate; cargo-about applies the same rule at release time.
"""
import argparse
import json
import pathlib
import re
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent


def tokens(expr):
    # `/` is the pre-SPDX spelling of OR (`MIT/Apache-2.0`).
    return re.findall(r"\(|\)|/|[A-Za-z0-9.+-]+", expr)


def satisfied(expr, accepted):
    toks = tokens(expr)
    pos = 0

    def peek():
        return toks[pos] if pos < len(toks) else None

    def take():
        nonlocal pos
        pos += 1
        return toks[pos - 1]

    def atom():
        t = take()
        if t == "(":
            v = disj()
            if take() != ")":
                raise ValueError(f"unbalanced parentheses in {expr!r}")
            return v
        lic = t[:-1] if t.endswith("+") else t
        if peek() == "WITH":
            take()
            take()
        return lic in accepted

    def conj():
        v = atom()
        while peek() == "AND":
            take()
            v = atom() and v
        return v

    def disj():
        v = conj()
        while peek() in ("OR", "/"):
            take()
            v = conj() or v
        return v

    v = disj()
    if pos != len(toks):
        raise ValueError(f"cannot parse license expression {expr!r}")
    return v


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--metadata", help="cargo metadata JSON (default: run cargo metadata)")
    args = ap.parse_args()

    about = tomllib.loads((ROOT / "about.toml").read_text())
    accepted = set(about.get("accepted", []))
    if args.metadata:
        meta = json.loads(pathlib.Path(args.metadata).read_text())
    else:
        out = subprocess.run(
            ["cargo", "metadata", "--all-features", "--format-version", "1", "--locked"],
            cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout
        meta = json.loads(out)

    bad = []
    crates = [p for p in meta["packages"] if p.get("source")]  # registry/git, not the workspace
    for p in crates:
        name, lic = f"{p['name']} {p['version']}", p.get("license")
        if not lic:
            if "clarify" not in about.get(p["name"], {}):
                bad.append(f"{name}: no license field and no [{p['name']}.clarify] in about.toml")
            continue
        try:
            if not satisfied(lic, accepted):
                bad.append(f"{name}: {lic}")
        except (ValueError, IndexError) as e:
            bad.append(f"{name}: {e}")

    if bad:
        print("Crates whose license about.toml does not accept:", file=sys.stderr)
        for b in sorted(bad):
            print(f"  {b}", file=sys.stderr)
        print(
            "Add the license to `accepted` in about.toml (and THIRD_PARTY_NOTICES.md) if it is "
            "acceptable, or drop the dependency.",
            file=sys.stderr,
        )
        return 1
    print(f"{len(crates)} crates, every license accepted by about.toml")
    return 0


if __name__ == "__main__":
    sys.exit(main())
