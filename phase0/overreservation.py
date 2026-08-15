"""How much KV memory did `pressure` reserve versus what requests actually used.

`pressure` admits and scores a backend using (prompt_tokens + max_tokens) as the
projected peak KV footprint of a request. But `max_tokens` is a client-supplied
ceiling, not a prediction, and most requests finish well short of it. So the
router is reserving more capacity than a request will actually consume.

This answers the reviewer question: "you're wrong most of the time, since
max_tokens overestimates -- so what does that conservatism cost?"

    over-reservation factor = (prompt + reserved_for) / (prompt + actually_used)

Uses ACTUAL prompt tokens from the trace file, joined by req_id, not the
requested_max_tokens/actual_tokens pair alone -- prompt length matters because
it is the larger term for the long-prompt/short-output requests this workload
is full of.
"""

import argparse
import csv
import json
import statistics
import sys


def pct(sorted_vals, q):
    if not sorted_vals:
        return None
    k = (len(sorted_vals) - 1) * q
    f = int(k)
    c = min(f + 1, len(sorted_vals) - 1)
    return sorted_vals[f] if f == c else sorted_vals[f] + (sorted_vals[c] - sorted_vals[f]) * (k - f)


def analyze(trace_path, csv_path):
    trace = json.load(open(trace_path))
    rows = list(csv.DictReader(open(csv_path)))

    factors = []
    reserved_total = 0
    used_total = 0
    zero_actual = 0

    for r in rows:
        if r["error"]:
            continue
        i = int(r["req_id"])
        if i >= len(trace):
            continue
        t = trace[i]
        p = t["_prompt_tokens"]
        reserved_for = int(r["requested_max_tokens"])
        actually_used = int(r["actual_tokens"])

        reserved = p + reserved_for
        used = p + actually_used

        reserved_total += reserved
        used_total += used

        if used <= 0:
            zero_actual += 1
            continue

        factors.append(reserved / used)

    if not factors:
        return None

    factors.sort()
    return {
        "n": len(factors),
        "zero_actual_excluded": zero_actual,
        "mean_factor": statistics.mean(factors),
        "median_factor": pct(factors, 0.50),
        "p90_factor": pct(factors, 0.90),
        "p99_factor": pct(factors, 0.99),
        "min_factor": factors[0],
        "max_factor": factors[-1],
        "fleet_factor": reserved_total / used_total,  # capacity actually spent vs needed, in aggregate
        "reserved_total": reserved_total,
        "used_total": used_total,
    }


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--trace", required=True, help="the .json trace file used for this run")
    ap.add_argument("csvs", nargs="+", help="result CSVs from runs against that trace")
    args = ap.parse_args()

    print(f"{'file':<40} {'n':>5} {'mean':>7} {'p50':>7} {'p90':>7} {'p99':>7} {'fleet':>7}")
    print("-" * 82)
    for c in args.csvs:
        s = analyze(args.trace, c)
        if s is None:
            print(f"{c:<40}  (no usable rows)")
            continue
        print(f"{c:<40} {s['n']:>5} {s['mean_factor']:>6.2f}x {s['median_factor']:>6.2f}x "
              f"{s['p90_factor']:>6.2f}x {s['p99_factor']:>6.2f}x {s['fleet_factor']:>6.2f}x")

    print()
    print("Reading this:")
    print("  mean/median/p90/p99: per-request over-reservation. 2.0x means the router held")
    print("    twice the KV tokens a request actually needed, for that request's lifetime.")
    print("  fleet: total tokens reserved across all requests / total tokens actually used.")
    print("    This is the aggregate capacity cost -- the number to quote.")
    print("  If fleet is much lower than mean, a few small requests with huge max_tokens")
    print("    are dragging the per-request mean up without costing much real capacity.")
