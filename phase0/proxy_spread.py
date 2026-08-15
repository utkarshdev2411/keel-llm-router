"""Real per-backend distribution for --policy proxy runs.

WHY THIS EXISTS
---------------
When an external router is under test, the load generator only ever sees that
router's single URL. Every result row therefore carries the same `backend` value,
and compare.py's spread columns collapse to a meaningless 0.0% -- which reads on
the table as "perfectly balanced" when it actually means "not measured".

`scrape_backend_counts.py` snapshots each real backend's Prometheus counters
before and after a run. Diffing a matched pair recovers what the external router
actually did, which is the only way to compare its balance against ours.

Usage:
    ./venv/bin/python proxy_spread.py results_compet
"""

import argparse
import collections
import glob
import json
import os
import re
import statistics


def spread(vals):
    if not vals or sum(vals) == 0:
        return 0.0
    return (max(vals) - min(vals)) / (sum(vals) / len(vals)) * 100


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("results", help="directory holding *_before.json / *_after.json")
    a = ap.parse_args()

    befores = sorted(glob.glob(os.path.join(a.results, "*_before.json")))
    if not befores:
        print("No _before.json snapshots found. Proxy runs need "
              "scrape_backend_counts.py before and after each arm.")
        return

    rows = []
    for b in befores:
        aft = b.replace("_before.json", "_after.json")
        if not os.path.exists(aft):
            print(f"skip {os.path.basename(b)}: no matching _after.json")
            continue
        try:
            snap_b = json.load(open(b))
            snap_a = json.load(open(aft))
        except Exception as e:
            print(f"skip {os.path.basename(b)}: {e}")
            continue

        reqs, toks = [], []
        for url in snap_b:
            vb, va = snap_b.get(url, {}), snap_a.get(url, {})
            if not isinstance(vb, dict) or not isinstance(va, dict):
                continue
            # A freshly restarted container has served nothing, so the Prometheus
            # client omits these counters entirely and the BEFORE snapshot reads
            # null. That is not a scrape failure: absent means zero. This is only
            # safe because the backends are restarted cold before every arm; if
            # they were not, a null before would hide prior traffic.
            if va.get("requests") is None:
                continue
            reqs.append(va["requests"] - (vb.get("requests") or 0.0))
            if va.get("prompt_tokens") is not None:
                toks.append(va["prompt_tokens"] - (vb.get("prompt_tokens") or 0.0))

        name = os.path.basename(b).replace("_before.json", "")
        rows.append({
            "name": name,
            "backends": len(reqs),
            "total_req": sum(reqs),
            "req_spread": spread(reqs),
            "kv_spread": spread(toks) if toks else None,
            "per_backend": reqs,
        })

    if not rows:
        print("No usable snapshot pairs.")
        return

    print()
    print("REAL PER-BACKEND DISTRIBUTION (from /metrics, proxy runs)")
    print("=" * 100)
    print(f"{'run':<38} {'bk':>3} {'served':>8} {'req sprd':>9} {'kv sprd':>9}  per-backend")
    print("-" * 100)
    for r in sorted(rows, key=lambda x: x["name"]):
        kv = "n/a" if r["kv_spread"] is None else f"{r['kv_spread']:.1f}%"
        pb = " ".join(f"{int(v)}" for v in r["per_backend"])
        print(f"{r['name']:<38} {r['backends']:>3} {int(r['total_req']):>8} "
              f"{r['req_spread']:>8.1f}% {kv:>9}  {pb}")

    # Group by arm (strip the __runN suffix) so repeats can be averaged.
    groups = collections.defaultdict(list)
    for r in rows:
        arm = re.sub(r"__run\d+$", "", r["name"])
        groups[arm].append(r)

    print()
    print("Averaged over repeats:")
    for arm in sorted(groups):
        g = groups[arm]
        rs = statistics.mean(x["req_spread"] for x in g)
        kvs = [x["kv_spread"] for x in g if x["kv_spread"] is not None]
        kv = f"{statistics.mean(kvs):.1f}%" if kvs else "n/a"
        print(f"  {arm:<40} req spread {rs:5.1f}%   kv spread {kv}")

    print()
    print("NOTE: these counters are cumulative per container. The numbers above are")
    print("valid only because the backends are restarted cold before every arm, so a")
    print("diff covers exactly one run. Without that, they would include prior traffic.")
    print()


if __name__ == "__main__":
    main()
