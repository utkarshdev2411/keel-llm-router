"""Snapshot per-backend request count and prompt-token count from /metrics.

Needed for any --policy proxy run: the loadgen only sees the single external
router URL, never which of the real backends it dispatched to, so
compare.py's request/token spread is meaningless there (all rows land in one
bucket, spread computes as a trivial 0.0%). Diffing this snapshot before and
after a run gives the real per-backend distribution instead.
"""
import argparse
import json
import re
import sys
import urllib.request


def scrape(url):
    text = urllib.request.urlopen(url, timeout=5).read().decode()
    # Label block is optional: an exporter may emit the metric bare, and
    # requiring `{...}` would silently return None for every backend, making a
    # run look like it produced no traffic at all.
    reqs = re.search(r'^vllm:e2e_request_latency_seconds_count(?:\{[^}]*\})?\s+([\d.eE+]+)', text, re.M)
    toks = re.search(r'^vllm:prompt_tokens_total(?:\{[^}]*\})?\s+([\d.eE+]+)', text, re.M)
    return {
        "requests": float(reqs.group(1)) if reqs else None,
        "prompt_tokens": float(toks.group(1)) if toks else None,
    }


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("backends", help="comma-separated base URLs")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    snap = {}
    for b in a.backends.split(","):
        try:
            snap[b] = scrape(f"{b}/metrics")
        except Exception as e:
            snap[b] = {"error": str(e)}

    json.dump(snap, open(a.out, "w"), indent=2)
    print(f"snapshot written to {a.out}")
