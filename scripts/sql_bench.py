"""End-to-end SQL latency with the Python client (google-cloud-spanner).

Usage: python3 scripts/sql_bench.py HOST:PORT [rows]
Seeds a Users table (100k rows by default, index on Email) and times a PK point query,
an index equality query, a 3-table join and an INSERT DML in a read-write transaction.
"""
import os
import statistics
import sys
import time

host = sys.argv[1]
rows = int(sys.argv[2]) if len(sys.argv) > 2 else 100_000
os.environ["SPANNER_EMULATOR_HOST"] = host

from google.auth.credentials import AnonymousCredentials  # noqa: E402
from google.cloud import spanner  # noqa: E402
from google.cloud.spanner_v1 import param_types  # noqa: E402

client = spanner.Client(project="bench", credentials=AnonymousCredentials())
inst = client.instance("bench", configuration_name="projects/bench/instanceConfigs/emulator-config", node_count=1)
if not inst.exists():
    inst.create().result()
db = inst.database("bench", ddl_statements=[
    "CREATE TABLE Teams (TeamId INT64 NOT NULL, Name STRING(MAX)) PRIMARY KEY (TeamId)",
    "CREATE TABLE Users (UserId INT64 NOT NULL, TeamId INT64, Email STRING(MAX), Score INT64) PRIMARY KEY (UserId)",
    "CREATE INDEX UsersByEmail ON Users(Email)",
    "CREATE TABLE Orders (UserId INT64 NOT NULL, OrderId INT64 NOT NULL, Amount INT64) PRIMARY KEY (UserId, OrderId), INTERLEAVE IN PARENT Users ON DELETE CASCADE",
])
if db.exists():
    db.drop()
db.create().result()

t0 = time.time()
with db.batch() as b:
    b.insert("Teams", ["TeamId", "Name"], [(t, f"team-{t}") for t in range(100)])
for start in range(0, rows, 2000):
    ids = range(start, min(rows, start + 2000))
    with db.batch() as b:
        b.insert("Users", ["UserId", "TeamId", "Email", "Score"], [(i, i % 100, f"user{i}@x.io", i % 1000) for i in ids])
        b.insert("Orders", ["UserId", "OrderId", "Amount"], [(i, 1, i % 77) for i in ids])
print(f"{host}: seeded {rows} users in {time.time() - t0:.1f}s")


def bench(name, fn, n=200):
    fn(-1)
    samples = []
    for i in range(n):
        t = time.perf_counter()
        fn(i)
        samples.append((time.perf_counter() - t) * 1000)
    samples.sort()
    print(f"  {name:<28} p50={statistics.median(samples):8.2f} ms  mean={statistics.mean(samples):8.2f} ms  p99={samples[int(n * 0.99) - 1]:8.2f} ms")


def query(sql, params, types):
    def run(i):
        k = (i * 7919) % rows
        p = {name: (f"user{k}@x.io" if t == param_types.STRING else k) for name, t in types.items()}
        with db.snapshot() as s:
            out = list(s.execute_sql(sql, params=p, param_types=types))
        assert len(out) == 1, (sql, out)
    return run


bench("PK point query", query("SELECT UserId, Email, Score FROM Users WHERE UserId = @k", None, {"k": param_types.INT64}))
bench("index equality", query("SELECT UserId, Email FROM Users WHERE Email = @k", None, {"k": param_types.STRING}))
bench("3-table join", query(
    "SELECT u.Email, t.Name, o.Amount FROM Users u JOIN Teams t ON t.TeamId = u.TeamId "
    "JOIN Orders o ON o.UserId = u.UserId WHERE u.UserId = @k", None, {"k": param_types.INT64}))


def insert(i):
    def txn(t):
        t.execute_update("INSERT INTO Users (UserId, TeamId, Email) VALUES (@id, 1, 'n@x.io')",
                         params={"id": rows + 1_000_000 + i}, param_types={"id": param_types.INT64})
    db.run_in_transaction(txn)


bench("INSERT DML (RW txn)", insert)
