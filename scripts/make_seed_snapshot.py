#!/usr/bin/env python3
"""Seed a running spanbolt (or emulator) with fixtures/schema.json plus synthetic rows.

    SPANNER_EMULATOR_HOST=127.0.0.1:9010 python3 scripts/make_seed_snapshot.py [--rows-per-table 10000]

Creates projects/test-project/instances/test-instance/databases/common, applies the fixture's DDL
statements, then inserts rows into a few wide tables, with values generated from the column types.
Save the result with `spanbolt snapshot /data/seed.snap` (see docs/docker.md).
"""

import argparse
import datetime
import decimal
import json
import os
import re
import time

from google.cloud import spanner
from google.cloud.spanner_v1 import JsonObject

TABLES = [
    "listings_north",
    "listings_global",
    "sensor_versions",
    "inventory.export_log",
    "audit.note_queue",
]
COLUMN = re.compile(r"^\s{2}(`?[\w]+`?)\s+(.+?),?$")


def columns(create_table):
    """(name, type, not_null) for every column a client may write (no generated columns)."""
    out = []
    for line in create_table.splitlines()[1:]:
        m = COLUMN.match(line)
        if not m or line.startswith("  CONSTRAINT"):
            continue
        name, rest = m.group(1).strip("`"), m.group(2)
        if " AS (" in rest:
            continue
        ty = re.match(r"(ARRAY<[^>]+>|\w+(\(\w+\))?)", rest).group(1)
        out.append((name, ty, "NOT NULL" in rest))
    return out


def value(ty, i, col):
    if ty.startswith("ARRAY<"):
        inner = ty[6:-1]
        return [value(inner, i * 3 + k, col) for k in range(3)]
    base = ty.split("(")[0]
    if base == "STRING":
        size = re.search(r"\((\d+)\)", ty)
        limit = int(size.group(1)) if size else 64
        if limit <= 2:
            return ["NO", "SE", "DK", "FI", "GB"][i % 5][:limit]
        return f"{col}-{i:07d}"[: min(limit, 64)]
    if base == "INT64":
        return i
    if base in ("FLOAT64", "FLOAT32"):
        return i * 1.5
    if base == "NUMERIC":
        return decimal.Decimal(i) / 100
    if base == "BOOL":
        return i % 2 == 0
    if base == "TIMESTAMP":
        return datetime.datetime(2026, 1, 1, tzinfo=datetime.timezone.utc) + datetime.timedelta(seconds=i)
    if base == "DATE":
        return datetime.date(2000, 1, 1) + datetime.timedelta(days=i % 9000)
    if base == "JSON":
        return JsonObject({"i": i, "tag": f"t{i % 17}"})
    if base == "BYTES":
        return f"b{i}".encode()
    raise ValueError(f"no generator for {ty}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rows-per-table", type=int, default=10_000)
    ap.add_argument("--schema", default=os.path.join(os.path.dirname(__file__), "../fixtures/schema.json"))
    args = ap.parse_args()

    ddl = json.load(open(args.schema))["ddl"]
    client = spanner.Client(project="test-project")
    instance = client.instance("test-instance", configuration_name="projects/test-project/instanceConfigs/emulator-config")
    if not instance.exists():
        instance.create().result(60)
    db = instance.database("common")
    t = time.time()
    db.create().result(60)
    db.update_ddl(ddl).result(600)
    print(f"applied {len(ddl)} DDL statements in {time.time() - t:.2f} s")

    by_name = {re.match(r"CREATE TABLE ([\w.]+)", s).group(1): s for s in ddl if s.startswith("CREATE TABLE")}
    total = 0
    t = time.time()
    for table in TABLES:
        cols = columns(by_name[table])
        names = [c[0] for c in cols]
        for start in range(0, args.rows_per_table, 1000):
            rows = [
                [value(ty, i, name) for name, ty, _ in cols]
                for i in range(start, min(start + 1000, args.rows_per_table))
            ]
            with db.batch() as b:
                b.insert(table, names, rows)
            total += len(rows)
        print(f"  {table}: {args.rows_per_table} rows x {len(cols)} columns")
    print(f"inserted {total} rows in {time.time() - t:.2f} s")


if __name__ == "__main__":
    main()
