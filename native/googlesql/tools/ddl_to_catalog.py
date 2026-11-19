#!/usr/bin/env python3
"""Spike helper: turn fixtures/schema.json DDL into the shim's catalog JSON.

Usage: ddl_to_catalog.py <schema.json> > catalog.json
Only CREATE TABLE columns are extracted; spanbolt-ddl will own this for real.
"""
import json
import re
import sys

STOP = re.compile(r"\s+(NOT\s+NULL|AS\s*\(|DEFAULT\s*\(|OPTIONS\s*\(|HIDDEN\b)", re.I)


def split_top(body):
    parts, depth, cur = [], 0, []
    for ch in body:
        if ch in "(<":
            depth += 1
        elif ch in ")>":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    parts.append("".join(cur))
    return [p.strip() for p in parts if p.strip()]


def table(stmt):
    m = re.match(r"CREATE TABLE\s+([\w.]+)\s*\(", stmt)
    if not m:
        return None
    start = m.end()
    end = stmt.rfind(") PRIMARY KEY")
    cols = []
    for part in split_top(stmt[start:end]):
        if re.match(r"(CONSTRAINT|FOREIGN\s+KEY|CHECK)\b", part, re.I):
            continue
        name, _, rest = part.partition(" ")
        rest = STOP.split(" " + rest.strip(), maxsplit=1)[0].strip()
        cols.append({"name": name.strip("`"), "type": rest})
    return {"name": m.group(1), "columns": cols}


def main():
    ddl = json.load(open(sys.argv[1]))["ddl"]
    tables = [t for t in (table(s) for s in ddl) if t]
    json.dump({"tables": tables}, sys.stdout)


if __name__ == "__main__":
    main()
