"""Instantaneous occupancy statistics, parsed from a run log.

WHY THIS EXISTS
---------------
`compare.py`'s DISP kv column measures CUMULATIVE prompt tokens dispatched per
backend across a whole run. That was introduced as the mechanism metric for
`pressure` and it turned out to be the wrong quantity: at rate 8, `pressure` beat
`least_conn` 0.3% to 2.0% on errors while showing HIGHER cumulative spread
(~25% vs ~15%).

That is not a contradiction once you notice what actually causes a KV rejection.
A backend rejects when its occupancy crosses the ceiling AT THAT MOMENT. Totals
over a run say nothing about peaks. A policy can dispatch exactly equal totals and
still overflow by timing them badly, or dispatch very unequal totals while never
letting any single backend peak.

So the mechanism claim has to be tested on instantaneous state. The ticker in
loadgen.py already prints per-backend occupancy every 5 seconds, and that estimate
was verified against the backends' real kv_cache_usage_perc (see kv_curve.py and
the memory-model correction in ../journey/phase-0.md section 7), so the log is
a usable sample of it.

What this reports, per (rate, policy):

  max_occ mean/p95   How full the FULLEST backend is at a typical moment. This is
                     what determines whether the next arrival is rejected.
  %ticks >= sigma    Fraction of samples where some backend sat at or above the
                     admission ceiling. Time spent in the danger zone.
  inst spread        Mean over ticks of (max-min)/mean occupancy. THIS is balance
                     in the sense that matters, as opposed to cumulative totals.

Usage:
    ./venv/bin/python occupancy_stats.py results_compare/compare_log.txt
"""

import argparse
import collections
import re
import statistics

# stage5_compare.sh: "### rate=8  policy=pressure  run=1/3"
HEADER = re.compile(r"###\s+rate=([\d.]+)\s+policy=(\S+)\s+run=(\d+)")
# stage6_competitors.sh: "--- ours:pressure  run=1/3 ---". No rate in the header,
# so it is taken from the trace/stage banner elsewhere in the log.
HEADER_ARM = re.compile(r"---\s+(\S+?)\s+run=(\d+)/")
ANY_RATE = re.compile(r"rate=([\d.]+)")
TICK = re.compile(r"occupancy\[([^\]]*)\]")


def parse(path):
    """Return {(rate, policy): [ [occ per backend] per tick ]}."""
    groups = collections.defaultdict(list)
    key = None
    fallback_rate = "?"
    for line in open(path, errors="replace"):
        h = HEADER.search(line)
        if h:
            key = (h.group(1), h.group(2))
            continue
        h = HEADER_ARM.search(line)
        if h:
            key = (fallback_rate, h.group(1))
            continue
        # Any "rate=N" outside a header (e.g. the stage banner) seeds the rate for
        # log formats that do not carry it per arm.
        r = ANY_RATE.search(line)
        if r and "occupancy[" not in line:
            fallback_rate = r.group(1)
        if key is None:
            continue
        t = TICK.search(line)
        if not t:
            continue
        vals = []
        for tok in t.group(1).split():
            try:
                vals.append(float(tok.split("=")[1]))
            except (IndexError, ValueError):
                pass
        # Ignore all-zero ticks: those are the drain tail after every request has
        # finished, and including them drags every average toward zero.
        if vals and any(v > 0 for v in vals):
            groups[key].append(vals)
    return groups


def pct(sorted_vals, q):
    if not sorted_vals:
        return 0.0
    return sorted_vals[min(len(sorted_vals) - 1, int(len(sorted_vals) * q))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--sigma", type=float, default=0.95,
                    help="admission ceiling, for the danger-zone column")
    a = ap.parse_args()

    groups = parse(a.log)
    if not groups:
        print("No occupancy ticks found. Note that --policy proxy runs do not print "
              "an occupancy line, by design.")
        return

    print()
    print(f"INSTANTANEOUS OCCUPANCY  --  {a.log}")
    print("=" * 92)
    print(f"{'rate':>5} {'policy':<14} {'ticks':>6} {'max_occ':>9} {'max p95':>9} "
          f"{'%>={:.2f}'.format(a.sigma):>9} {'inst spread':>12}")
    print("-" * 92)

    last_rate = None
    def sort_key(k):
        try:
            return (float(k[0]), k[1])
        except ValueError:
            return (float("inf"), k[1])   # unknown rate sorts last

    for (rate, policy) in sorted(groups, key=sort_key):
        ticks = groups[(rate, policy)]
        if last_rate is not None and rate != last_rate:
            print()
        maxes = sorted(max(t) for t in ticks)
        danger = sum(1 for t in ticks if max(t) >= a.sigma) / len(ticks) * 100
        spreads = []
        for t in ticks:
            m = sum(t) / len(t)
            if m > 0:
                spreads.append((max(t) - min(t)) / m * 100)
        print(f"{rate:>5} {policy:<14} {len(ticks):>6} "
              f"{statistics.mean(maxes):>9.3f} {pct(maxes, 0.95):>9.3f} "
              f"{danger:>8.1f}% {statistics.mean(spreads) if spreads else 0:>11.1f}%")
        last_rate = rate

    print()
    print("How to read this:")
    print("  max_occ    : how full the FULLEST backend is at a typical moment.")
    print("               A rejection happens when this crosses the ceiling, so")
    print("               this is the quantity that actually predicts err%.")
    print(f"  %>={a.sigma:.2f}   : share of time some backend sat at or above the")
    print("               admission ceiling. Time spent one arrival away from")
    print("               a rejection.")
    print("  inst spread: balance in the sense that matters. Contrast with")
    print("               compare.py's DISP kv, which is CUMULATIVE and can move")
    print("               in the opposite direction without contradiction.")
    print()


if __name__ == "__main__":
    main()
