"""Side-by-side policy comparison with warmup/drain trimming.

Trimming matters: the first requests hit empty backends and the last ones run
with no competition. Neither is steady state, and including them pollutes every
percentile. Default keeps the middle 60% of the arrival window.
"""

import csv
import sys
import os
import re
import argparse


def pct(vals, p):
    if not vals:
        return None
    s = sorted(vals)
    k = (len(s) - 1) * p
    f = int(k)
    c = min(f + 1, len(s) - 1)
    return s[f] if f == c else s[f] + (s[c] - s[f]) * (k - f)


def spread(vals):
    if not vals or sum(vals) == 0:
        return 0.0
    return (max(vals) - min(vals)) / (sum(vals) / len(vals)) * 100


def summarize(path, trim):
    rows = list(csv.DictReader(open(path)))
    total_raw = len(rows)

    # Trim by SCHEDULED arrival time, not completion time. A request scheduled
    # inside the steady window belongs in the sample regardless of when it
    # finished.
    offsets = [float(r["scheduled_offset_s"]) for r in rows]
    lo_t, hi_t = min(offsets), max(offsets)
    span = hi_t - lo_t
    keep_lo = lo_t + span * trim
    keep_hi = hi_t - span * trim

    rows = [r for r in rows if keep_lo <= float(r["scheduled_offset_s"]) <= keep_hi]
    ok = [r for r in rows if not r["error"]]
    ttfts = [float(r["ttft_s"]) for r in ok if r["ttft_s"]]

    # TWO spreads, deliberately (tracker B4).
    #
    # DISPATCH spread covers every row, rejections included, and uses prompt
    # tokens -- the KV each request asked the backend to hold. This answers "did
    # the router balance the load it handed out?"
    #
    # COMPLETION spread covers successes only. This answers "what actually got
    # served?"
    #
    # Reporting only the second is a trap: a backend that receives heavy traffic
    # and rejects most of it registers as lightly loaded, so a policy that sheds
    # aggressively scores as well balanced. Error rates at the knee are 3-11%, and
    # `pressure`'s whole mechanism is refusing admission, so this bias points
    # straight at our own conclusion.
    disp = {}
    for r in rows:
        b = r["backend"]
        disp.setdefault(b, {"n": 0, "kv": 0})
        disp[b]["n"] += 1
        # prompt_tokens is absent in CSVs written before 2026-08-15.
        if r.get("prompt_tokens"):
            disp[b]["kv"] += int(r["prompt_tokens"])

    has_prompt_tokens = any(r.get("prompt_tokens") for r in rows)

    by_backend = {}
    for r in ok:
        b = r["backend"]
        by_backend.setdefault(b, {"n": 0, "tok": 0})
        by_backend[b]["n"] += 1
        by_backend[b]["tok"] += int(r["actual_tokens"])

    name = os.path.basename(path).replace(".csv", "")
    m = re.match(r"r([\d.]+)_(.+)", name)
    rate, policy = (m.group(1), m.group(2)) if m else ("?", name)

    return {
        "rate": rate,
        "policy": policy,
        "n": len(ok),
        "n_raw": total_raw,
        "errors": len(rows) - len(ok),
        "err_pct": (len(rows) - len(ok)) / max(1, len(rows)) * 100,
        "p50": pct(ttfts, 0.50),
        "p95": pct(ttfts, 0.95),
        "p99": pct(ttfts, 0.99),
        "disp_req_spread": spread([v["n"] for v in disp.values()]),
        "disp_kv_spread": (spread([v["kv"] for v in disp.values()])
                           if has_prompt_tokens else None),
        "req_spread": spread([v["n"] for v in by_backend.values()]),
        "tok_spread": spread([v["tok"] for v in by_backend.values()]),
    }


def fmt_ms(v):
    return "n/a" if v is None else f"{v*1000:,.0f}"


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+")
    ap.add_argument("--trim", type=float, default=0.20,
                    help="fraction of the arrival window to discard at EACH end "
                         "(0.20 keeps the middle 60%%). Use 0 to disable.")
    a = ap.parse_args()

    rows = [summarize(p, a.trim) for p in a.files]
    rows.sort(key=lambda r: (float(r["rate"]) if r["rate"] != "?" else 0, r["policy"]))

    kept = rows[0]["n"] if rows else 0
    raw = rows[0]["n_raw"] if rows else 0
    print()
    print(f"warmup/drain trim: {a.trim:.0%} at each end "
          f"(~{kept}/{raw} requests kept per run)")
    print()
    print(f"{'rate':>6} {'policy':<22} {'n':>5} {'err':>5} {'err%':>6} "
          f"{'TTFT p50':>10} {'p95':>10} {'p99':>10} "
          f"{'DISP req':>9} {'DISP kv':>9} {'ok tok':>9}")
    print("-" * 127)

    last = None
    for r in rows:
        if last is not None and r["rate"] != last:
            print()
        kv = "n/a" if r["disp_kv_spread"] is None else f"{r['disp_kv_spread']:.1f}%"
        print(f"{r['rate']:>6} {r['policy']:<22} {r['n']:>5} {r['errors']:>5} "
              f"{r['err_pct']:>5.1f}% "
              f"{fmt_ms(r['p50']):>10} {fmt_ms(r['p95']):>10} {fmt_ms(r['p99']):>10} "
              f"{r['disp_req_spread']:>8.1f}% {kv:>9} {r['tok_spread']:>8.1f}%")
        last = r["rate"]

    print()
    print("How to read this:")
    print("  1. Knee    : the rate where least_conn's p99 first climbs sharply.")
    print("               Below it no policy can win; far above it none can help.")
    print("  2. Compare : only at and just past the knee, same rate.")
    print("  3. Trust   : p95 and p99 should move the SAME direction. A win that")
    print("               shows only at p99 rests on very few samples.")
    print("  4. err%    : KV exhaustion -> HTTP 500 in this simulator. This is the")
    print("               PRIMARY signal. Rejected requests have no TTFT, so a policy")
    print("               that sheds load can look artificially fast. Read err% FIRST.")
    print("  5. DISP kv : THE headline metric. Spread of prompt tokens dispatched per")
    print("               backend, over ALL requests including rejections. This is the")
    print("               imbalance the project exists to fix. Lower is better.")
    print("  6. DISP req: spread of request COUNTS dispatched. least_conn drives this")
    print("               toward 0 by construction. High DISP req + low DISP kv is the")
    print("               signature of working as intended: uneven counts, even load.")
    print("  7. ok tok  : served tokens, successes only. Kept for continuity, but do")
    print("               NOT read it as balance -- a backend that rejects most of its")
    print("               traffic looks lightly loaded here. That is why DISP kv exists.")
    print()
