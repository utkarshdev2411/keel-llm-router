"""Time-at-ceiling fraction per backend, scraped from the router's admin endpoint.

The Phase 2 criterion has a mechanism half: the winning policy should keep
backends out of the region above sigma, not merely fail less often. That cannot
come from the load generator, which in proxy mode only ever sees the router's
single URL and never learns which backend served a request.

The router samples every backend on a fixed tick instead, accumulating two
counters per backend. Their ratio is the fraction of the run that backend spent
at or above sigma. Counters are cumulative over the router process's lifetime,
so a router started fresh for each arm makes one scrape at the end of the run
cover exactly that run.

Usage:
    python3 bench/ceiling_stats.py http://127.0.0.1:9090/metrics
"""
import re
import sys
import urllib.request

TOTAL = "router_backend_occupancy_ticks_total"
CEIL = "router_backend_ticks_at_ceiling_total"


def scrape(url):
    return urllib.request.urlopen(url, timeout=5).read().decode()


def series(text, name):
    """Return {backend_label: value} for one metric family."""
    out = {}
    # Prometheus text format: `name{backend="..."} value`. The label block is
    # required here — these metrics are always emitted per backend — but a
    # missing one must not crash the whole report.
    pat = re.compile(rf'^{re.escape(name)}\{{([^}}]*)\}}\s+([0-9.eE+-]+)$', re.M)
    for labels, value in pat.findall(text):
        m = re.search(r'backend="([^"]*)"', labels)
        if m:
            out[m.group(1)] = float(value)
    return out


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    text = scrape(sys.argv[1])
    totals, ceils = series(text, TOTAL), series(text, CEIL)

    if not totals:
        print(f"No {TOTAL} samples found. Is the occupancy sampler running, and is "
              f"this the router's admin port?")
        return 1

    print(f"{'backend':<28} {'ticks':>8} {'at_ceiling':>11} {'fraction':>9}")
    frs = []
    for b in sorted(totals):
        t = totals[b]
        c = ceils.get(b, 0.0)
        fr = c / t if t else 0.0
        frs.append(fr)
        print(f"{b:<28} {t:>8.0f} {c:>11.0f} {fr:>8.1%}")

    # The headline number for the criterion: across the whole fleet, what share
    # of backend-time was spent at or above the ceiling.
    tot, cei = sum(totals.values()), sum(ceils.get(b, 0.0) for b in totals)
    print(f"{'-' * 58}")
    print(f"{'FLEET':<28} {tot:>8.0f} {cei:>11.0f} {(cei / tot if tot else 0):>8.1%}")
    print(f"{'worst backend':<28} {'':>8} {'':>11} {max(frs, default=0):>8.1%}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
