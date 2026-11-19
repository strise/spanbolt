#!/usr/bin/env python3
"""Summarises a spanbolt timing log (SPANBOLT_TIMING=<path>, see crates/server/src/timing.rs).

  python3 scripts/timing-report.py LOG.jsonl [--top N] [--junit DIR]

Prints server-side totals per RPC, per category (DDL, commit, query, DML, read, session, other)
and the top statements by total time, fingerprinted (literals replaced by ?). With --junit, also
splits server time by test case: each JUnit XML testcase gets a window from the suite's start
timestamp and the cumulative times, and the RPCs that started in it count toward it.

Times are wall time until each RPC's response body ended. Concurrent RPCs overlap, so 'busy' (the
union of RPC intervals) is the bound on how much of the test wall time spanbolt can explain.
"""
import argparse
import collections
import datetime
import glob
import json
import re
import sys
import xml.etree.ElementTree as ET

STR = re.compile(r"'(?:[^'\\]|\\.)*'|\"(?:[^\"\\]|\\.)*\"")
NUM = re.compile(r"\b\d+(?:\.\d+)?\b")
INLIST = re.compile(r"\(\s*\?(?:\s*,\s*\?)+\s*\)")
WS = re.compile(r"\s+")


def fingerprint(sql):
    s = STR.sub("?", sql)
    s = NUM.sub("?", s)
    s = INLIST.sub("(?, ...)", s)
    return WS.sub(" ", s).strip()


def category(e):
    m = e["m"]
    sql = e.get("sql", "")
    if m == "UpdateDatabaseDdl" or m == "CreateDatabase":
        return "ddl"
    if m == "Commit":
        return "commit"
    if m in ("Read", "StreamingRead"):
        return "read"
    if m == "ExecuteBatchDml":
        return "dml"
    if m in ("ExecuteSql", "ExecuteStreamingSql"):
        head = sql.lstrip().split(None, 1)[0].upper() if sql.strip() else ""
        return "dml" if head in ("INSERT", "UPDATE", "DELETE") else "query"
    if "Session" in m:
        return "session"
    if m in ("BeginTransaction", "Rollback"):
        return "txn"
    return "other"


def p99(xs):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * 0.99))] if xs else 0


def busy(events):
    """Union of [t, t+us) intervals, in seconds."""
    iv = sorted((e["t"], e["t"] + e["us"] / 1000.0) for e in events)
    total = 0.0
    cur_s = cur_e = None
    for s, e in iv:
        if cur_e is None or s > cur_e:
            if cur_e is not None:
                total += cur_e - cur_s
            cur_s, cur_e = s, e
        else:
            cur_e = max(cur_e, e)
    if cur_e is not None:
        total += cur_e - cur_s
    return total / 1000.0


def table(rows, head):
    print("| " + " | ".join(head) + " |")
    print("|" + "|".join("---" for _ in head) + "|")
    for r in rows:
        print("| " + " | ".join(str(c) for c in r) + " |")
    print()


def summarise(events, top, label=""):
    tot = sum(e["us"] for e in events) / 1e6
    print(f"### {label}{len(events)} RPCs, sum {tot:.2f} s, busy {busy(events):.2f} s"
          + (f", span {(max(e['t'] + e['us'] / 1000 for e in events) - min(e['t'] for e in events)) / 1000:.1f} s" if events else ""))
    print()
    by = collections.defaultdict(list)
    for e in events:
        by[category(e)].append(e["us"])
    table(sorted(([k, len(v), f"{sum(v) / 1e6:.2f}", f"{100 * sum(v) / 1e6 / tot:.0f}%" if tot else "-",
                   f"{p99(v) / 1000:.1f}"] for k, v in by.items()), key=lambda r: -float(r[2])),
          ["category", "count", "total s", "share", "p99 ms"])
    by = collections.defaultdict(list)
    for e in events:
        by[e["m"]].append(e["us"])
    table(sorted(([k, len(v), f"{sum(v) / 1e6:.2f}", f"{sum(v) / len(v) / 1000:.2f}",
                   f"{p99(v) / 1000:.1f}", f"{max(v) / 1000:.1f}"] for k, v in by.items()),
                 key=lambda r: -float(r[2])),
          ["rpc", "count", "total s", "mean ms", "p99 ms", "max ms"])
    by = collections.defaultdict(list)
    for e in events:
        if "sql" in e:
            by[(e["m"], fingerprint(e["sql"]))].append(e["us"])
    rows = sorted(by.items(), key=lambda kv: -sum(kv[1]))[:top]
    table([[i + 1, m, len(v), f"{sum(v) / 1e6:.2f}", f"{p99(v) / 1000:.1f}",
            "`" + (fp[:140] + ("…" if len(fp) > 140 else "")).replace("|", "\\|") + "`"]
           for i, ((m, fp), v) in enumerate(rows)],
          ["#", "rpc", "count", "total s", "p99 ms", "statement"])
    codes = collections.Counter(e["code"] for e in events if e["code"] != 0)
    if codes:
        print("non-OK codes: " + ", ".join(f"{c}×{n}" for c, n in codes.most_common()) + "\n")


def junit_cases(d):
    cases = []
    for x in glob.glob(d + "/**/*.xml", recursive=True):
        root = ET.parse(x).getroot()
        for suite in root.iter("testsuite"):
            ts = suite.get("timestamp")
            if not ts:
                continue
            start = datetime.datetime.fromisoformat(ts).replace(tzinfo=datetime.timezone.utc).timestamp() * 1000
            t = start
            for tc in suite.iter("testcase"):
                dur = float(tc.get("time") or 0) * 1000
                cases.append((t, t + dur, suite.get("name"), tc.get("name")))
                t += dur
    return sorted(cases)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--top", type=int, default=20)
    ap.add_argument("--junit")
    ap.add_argument("--slow-cases", type=int, default=15)
    a = ap.parse_args()
    events = [json.loads(l) for l in open(a.log) if l.strip()]
    if not events:
        sys.exit("empty log")
    summarise(events, a.top, "all: ")
    if a.junit:
        cases = junit_cases(a.junit)
        if not cases:
            return
        test_time = sum(c[1] - c[0] for c in cases) / 1000
        print(f"JUnit: {len(cases)} test cases, test time {test_time:.1f} s, "
              f"from {datetime.datetime.fromtimestamp(cases[0][0] / 1000)} to {datetime.datetime.fromtimestamp(cases[-1][1] / 1000)}\n")
        per = collections.defaultdict(list)
        outside = []
        for e in events:
            hit = None
            for c in cases:
                if c[0] <= e["t"] < c[1]:
                    hit = c
                    break
            (per[hit].append(e) if hit else outside.append(e))
        rows = []
        for c in sorted(cases, key=lambda c: -(c[1] - c[0]))[: a.slow_cases]:
            ev = per.get(c, [])
            ddl = sum(e["us"] for e in ev if category(e) == "ddl") / 1e6
            rows.append([c[2].rsplit(".", 1)[-1], c[3][:60], f"{(c[1] - c[0]) / 1000:.2f}", len(ev),
                         f"{sum(e['us'] for e in ev) / 1e6:.2f}", f"{busy(ev):.2f}" if ev else "0", f"{ddl:.2f}"])
        table(rows, ["class", "test", "test s", "RPCs", "server sum s", "server busy s", "DDL s"])
        print(f"RPCs outside any test case window (class setup/teardown, Gradle): {len(outside)}, "
              f"sum {sum(e['us'] for e in outside) / 1e6:.2f} s, busy {busy(outside) if outside else 0:.2f} s\n")


if __name__ == "__main__":
    main()
