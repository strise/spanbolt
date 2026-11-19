#!/usr/bin/env python3
"""Top functions by CPU time in a samply profile saved with --save-only --unstable-presymbolicate.

  python3 scripts/samply-top.py PROFILE.json.gz [--top N] [--match REGEX]

Weights each sample by its thread CPU delta (so idle threads count nothing), symbolicates from the
.syms.json sidecar, and prints self and inclusive CPU per function. --match keeps only stacks
that contain a function matching REGEX (inclusive shares are then of that subset).
"""
import argparse
import bisect
import collections
import gzip
import json
import re


def load(path):
    p = json.load(gzip.open(path) if path.endswith(".gz") else open(path))
    syms = json.load(open(re.sub(r"\.gz$", "", path).replace(".json", ".json.syms.json")))
    strings = syms["string_table"]
    tables = {}
    for d in syms["data"]:
        st = sorted(d["symbol_table"], key=lambda s: s["rva"])
        tables[d["code_id"].upper()] = ([s["rva"] for s in st], st)
    return p, strings, tables


def resolve(addr, table, strings):
    rvas, st = table
    i = bisect.bisect_right(rvas, addr) - 1
    if i >= 0 and addr < st[i]["rva"] + st[i]["size"]:
        return strings[st[i]["symbol"]]
    return None


def clean(name):
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    return name


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("profile")
    ap.add_argument("--top", type=int, default=40)
    ap.add_argument("--match")
    a = ap.parse_args()
    p, strings, tables = load(a.profile)
    libs = p["libs"]
    selfw = collections.Counter()
    incl = collections.Counter()
    total = 0
    by_thread = collections.Counter()
    match = re.compile(a.match) if a.match else None
    for t in p["threads"]:
        ft, fn, stt, sm, sa = t["frameTable"], t["funcTable"], t["stackTable"], t["samples"], t["stringArray"]
        rt = t["resourceTable"]
        cache = {}

        def fname(frame):
            if frame in cache:
                return cache[frame]
            func = ft["func"][frame]
            res = fn["resource"][func]
            name = None
            if res is not None and res >= 0:
                lib = libs[rt["lib"][res]]
                table = tables.get(lib["codeId"].upper()) if lib.get("codeId") else None
                if table:
                    name = resolve(ft["address"][frame], table, strings)
                name = clean(name) if name else f"{lib['name']}+{ft['address'][frame]:#x}"
            else:
                name = sa[fn["name"][func]]
            cache[frame] = name
            return name

        stack_cache = {}

        def frames(stack):
            if stack in stack_cache:
                return stack_cache[stack]
            out = []
            s = stack
            while s is not None:
                out.append(fname(stt["frame"][s]))
                s = stt["prefix"][s]
            stack_cache[stack] = out
            return out

        cpu = sm.get("threadCPUDelta") or [None] * sm["length"]
        for i in range(sm["length"]):
            st = sm["stack"][i]
            w = cpu[i] if cpu[i] is not None else 0
            if st is None or not w:
                continue
            fs = frames(st)
            if match and not any(match.search(f) for f in fs):
                continue
            total += w
            by_thread[t["name"]] += w
            selfw[fs[0]] += w
            for f in set(fs):
                incl[f] += w
    print(f"total CPU {total / 1e6:.2f} s; by thread: " + ", ".join(
        f"{k} {v / 1e6:.2f}s" for k, v in by_thread.most_common(6)))
    print("\nself:")
    for f, w in selfw.most_common(a.top):
        print(f"  {100 * w / total:5.1f}%  {w / 1e6:7.2f}s  {f[:160]}")
    print("\ninclusive:")
    for f, w in incl.most_common(a.top):
        print(f"  {100 * w / total:5.1f}%  {w / 1e6:7.2f}s  {f[:160]}")


if __name__ == "__main__":
    main()
