"""Server RSS growth while the Python client streams large query results with a slow reader.

Usage: python3 scripts/stream_rss.py HOST:PORT SERVER_PID [n ...]
For each n (default 1000 and 3000) runs n*n-row cross, hash and FULL joins of two UNNESTs and a
UNION ALL of two half-size cross joins, after a warm-up with tiny inputs, reading slowly (0.1 s pause every 100k rows), and prints the server's
RSS before, at peak and after each query.
"""
import os
import sys
import threading
import time

host, pid = sys.argv[1], int(sys.argv[2])
sizes = [int(a) for a in sys.argv[3:]] or [1000, 3000]
os.environ["SPANNER_EMULATOR_HOST"] = host

from google.auth.credentials import AnonymousCredentials  # noqa: E402
from google.cloud import spanner  # noqa: E402

client = spanner.Client(project="rss", credentials=AnonymousCredentials())
inst = client.instance("rss", configuration_name="projects/rss/instanceConfigs/emulator-config", node_count=1)
if not inst.exists():
    inst.create().result()
db = inst.database("rss")
if not db.exists():
    db.create().result()


import psutil  # noqa: E402

proc = psutil.Process(pid)


def rss_mb():
    return proc.memory_info().rss / (1 << 20)


def run(name, sql):
    peak = [rss_mb()]
    before = peak[0]
    done = threading.Event()

    def sample():
        while not done.is_set():
            peak[0] = max(peak[0], rss_mb())
            time.sleep(0.1)

    t = threading.Thread(target=sample)
    t.start()
    n = 0
    t0 = time.time()
    with db.snapshot() as s:
        for _ in s.execute_sql(sql):
            n += 1
            if n % 100_000 == 0:
                time.sleep(0.1)
    done.set()
    t.join()
    after = rss_mb()
    print(f"  {name:<10} rows={n:>9} {time.time() - t0:6.1f}s  before={before:7.1f}  peak={peak[0]:7.1f} (+{peak[0] - before:6.1f})  after={after:7.1f} (+{after - before:6.1f}) MB", flush=True)



def queries(n):
    a = f"UNNEST(GENERATE_ARRAY(1, {n}))"
    half = f"UNNEST(GENERATE_ARRAY(1, {n // 2}))"
    return [
        ("cross", f"SELECT x, y FROM {a} x, {a} y"),
        ("hash", f"SELECT x, y FROM {a} x JOIN {a} y ON MOD(x, 1) = MOD(y, 1)"),
        ("full", f"SELECT x, y FROM {a} x FULL JOIN {a} y ON MOD(x, 1) = MOD(y, 1)"),
        ("union_all", f"SELECT x, y FROM {a} x, {half} y UNION ALL SELECT x, y FROM {a} x, {half} y"),
    ]


# Warm up the analyser and plan cache with the same query shapes on small inputs.
print("warm-up (10x10):")
for name, sql in queries(10):
    run(name, sql)
for n in sizes:
    print(f"{n}x{n}:")
    for name, sql in queries(n):
        run(name, sql)
