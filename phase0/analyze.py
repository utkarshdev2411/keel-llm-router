import csv
import sys
import statistics

def load(fname):
    with open(fname) as f:
        return list(csv.DictReader(f))

def percentile(values, p):
    if not values:
        return None
    s = sorted(values)
    k = (len(s) - 1) * p
    f = int(k)
    c = min(f + 1, len(s) - 1)
    if f == c:
        return s[f]
    return s[f] + (s[c] - s[f]) * (k - f)

def analyze(fname):
    rows = load(fname)
    rows = [r for r in rows if not r['error']]

    ttfts = [float(r['ttft_s']) for r in rows if r['ttft_s']]

    print(f"\n=== {fname} ===")
    print(f"total requests: {len(rows)}")
    print(f"TTFT p50: {percentile(ttfts, 0.50)*1000:.1f} ms")
    print(f"TTFT p95: {percentile(ttfts, 0.95)*1000:.1f} ms")
    print(f"TTFT p99: {percentile(ttfts, 0.99)*1000:.1f} ms")

    by_backend = {}
    for r in rows:
        b = r['backend']
        by_backend.setdefault(b, {'count': 0, 'tokens': 0})
        by_backend[b]['count'] += 1
        by_backend[b]['tokens'] += int(r['actual_tokens'])

    print(f"\n{'backend':<28} {'requests':>10} {'actual tokens':>15}")
    for b, stats in sorted(by_backend.items()):
        print(f"{b:<28} {stats['count']:>10} {stats['tokens']:>15}")

    counts = [s['count'] for s in by_backend.values()]
    tokens = [s['tokens'] for s in by_backend.values()]
    count_spread = (max(counts) - min(counts)) / (sum(counts) / len(counts)) * 100
    token_spread = (max(tokens) - min(tokens)) / (sum(tokens) / len(tokens)) * 100
    print(f"\nrequest-count spread across backends: {count_spread:.1f}%")
    print(f"actual-token spread across backends:  {token_spread:.1f}%")

if __name__ == "__main__":
    for fname in sys.argv[1:]:
        analyze(fname)
