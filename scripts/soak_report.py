#!/usr/bin/env python3
"""Summarise a soak CSV (crates/server/examples/soak.rs): a markdown table every N minutes, a
text chart of RSS and p99 latency, and optionally an SVG with both.

    python3 scripts/soak_report.py .mcp-output/soak.csv [--every 300] [--svg out.svg]
"""

import argparse
import csv

KINDS = ["point", "index", "search", "dml", "commit", "create_db", "clone", "drop_db"]
BARS = " ▁▂▃▄▅▆▇█"
# Memory columns: rust_heap_mb (live bytes in mimalloc) and malloc_mb (system malloc zones, the
# GoogleSQL shim) since soak runs on mimalloc; heap_mb (all malloc zones, the whole live heap) in
# older runs on the system allocator.
MEM = ("rss_mb", "footprint_mb", "rust_heap_mb", "malloc_mb", "heap_mb")


def spark(vals):
    lo, hi = min(vals), max(vals)
    span = (hi - lo) or 1
    return "".join(BARS[int((v - lo) / span * (len(BARS) - 1))] for v in vals)


def svg(rows, path):
    w, h, pad = 900, 420, 50
    t = [float(r["elapsed_s"]) for r in rows]
    series = [
        (c, c.replace("_mb", " MB"), color, 0)
        for c, color in [
            ("rss_mb", "#94a3b8"),
            ("footprint_mb", "#2563eb"),
            ("rust_heap_mb", "#7c3aed"),
            ("malloc_mb", "#0891b2"),
            ("heap_mb", "#7c3aed"),
        ]
        if c in rows[0]
    ] + [
        (f"{k}_p99_us", f"{k} p99 µs", c, 1)
        for k, c in [("point", "#16a34a"), ("search", "#d97706"), ("dml", "#dc2626")]
    ]
    half = (h - 3 * pad) / 2
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" font-family="sans-serif" font-size="11">',
        '<rect width="100%" height="100%" fill="white"/>',
    ]
    for panel, title in [(0, "memory (MB)"), (1, "p99 latency (µs)")]:
        top = pad + panel * (half + pad)
        cols = [s for s in series if s[3] == panel]
        hi = max(float(r[c[0]]) for r in rows for c in cols) * 1.1 or 1
        out.append(f'<text x="{pad}" y="{top - 8}" font-weight="bold">{title}, max {hi / 1.1:.0f}</text>')
        out.append(f'<rect x="{pad}" y="{top}" width="{w - 2 * pad}" height="{half}" fill="none" stroke="#ccc"/>')
        for i, (col, label, color, _) in enumerate(cols):
            pts = " ".join(
                f"{pad + (x / (t[-1] or 1)) * (w - 2 * pad):.1f},{top + half - float(r[col]) / hi * half:.1f}"
                for x, r in zip(t, rows)
            )
            out.append(f'<polyline points="{pts}" fill="none" stroke="{color}" stroke-width="1.5"/>')
            out.append(f'<text x="{w - pad - 110}" y="{top + 14 + 13 * i}" fill="{color}">{label}</text>')
    out.append(f'<text x="{w / 2}" y="{h - 10}" text-anchor="middle">elapsed, 0 to {t[-1] / 60:.0f} min</text>')
    out.append("</svg>")
    with open(path, "w") as f:
        f.write("\n".join(out))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("csv")
    ap.add_argument("--every", type=int, default=300, help="table row interval in seconds")
    ap.add_argument("--svg")
    a = ap.parse_args()
    rows = list(csv.DictReader(open(a.csv)))
    if not rows:
        raise SystemExit("empty csv")

    mem = [c for c in MEM if c in rows[0]]
    cols = ["min"] + [c.replace("_mb", " MB") for c in mem] + ["ops/s"] + [f"{k} p50/p99 µs" for k in ("point", "search", "dml", "commit")] + [
        "dbs",
        "operations",
        "catalogs",
        "plans",
    ]
    print("| " + " | ".join(cols) + " |")
    print("|" + "---|" * len(cols))
    next_t, prev_t = 0.0, 0.0
    for i, r in enumerate(rows):
        el = float(r["elapsed_s"])
        if el + 1e-9 < next_t and i != len(rows) - 1:
            prev_t = el
            continue
        dt = el - prev_t or 1
        prev_t, next_t = el, next_t + a.every
        cells = [f"{el / 60:.0f}"] + [r[c] for c in mem] + [f"{int(r['ops']) / dt:.0f}"]
        cells += [f"{r[k + '_p50_us']}/{r[k + '_p99_us']}" for k in ("point", "search", "dml", "commit")]
        cells += [r["databases"], r["operations"], r["catalogs"], r["plans"]]
        print("| " + " | ".join(cells) + " |")

    print()
    step = max(1, len(rows) // 60)
    sample = rows[::step]
    for c in mem:
        v = [float(r[c]) for r in sample]
        print(f"{c:<13} {spark(v)}  {min(v):.0f}..{max(v):.0f}")
    for k in KINDS:
        v = [int(r[f"{k}_p99_us"]) for r in sample]
        print(f"{k + ' p99':<13} {spark(v)}  {min(v)}..{max(v)} µs")
    if a.svg:
        svg(rows, a.svg)
        print(f"\nsvg: {a.svg}")


if __name__ == "__main__":
    main()
