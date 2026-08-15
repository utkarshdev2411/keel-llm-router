"""Isolated single-request KV-vs-time diagnostic.

Sends ONE request to a dedicated, otherwise-idle backend and samples /metrics
across its lifetime, to see how the backend's real KV usage evolves.

WHY THIS EXISTS
---------------
After the B1 fix, a live check showed router-reported occupancy (0.50-0.55)
running HIGHER than the backend's real kv_cache_usage_perc (0.20-0.24) --
the opposite direction from the original bug, which had the router reading
LOW. Three candidate explanations, which this script separates:

  1. RAMP (expected, not a bug). loadgen.py reserves the full peak
     (prompt + full estimated output) in kv_proj at dispatch, t=0. The
     backend's real KV starts at just the prompt and grows as tokens are
     generated. A mid-request snapshot then compares router-peak against
     backend-partial and looks like 2x over-estimation even though both
     numbers are correct for what they measure. Signature: the curve climbs
     steadily and its PEAK matches the router's projection.

  2. UNIT MISMATCH (real bug). loadgen.py computes p as
     len(prompt.split()) -- a WORD count -- but KV is measured in TOKENS. If
     the simulator does not tokenize one-token-per-word, every projection is
     off by that ratio. Signature: peak KV disagrees with the projection by a
     constant factor, and tokens_generated != prompt_words below.

  3. CAPACITY MISMATCH (real bug). The router assumes kv_capacity=8192
     (kv-cache-size 512 x block-size 16). If the backend's denominator for
     kv_cache_usage_perc is something else, everything is scaled wrong.
     Signature: peak KV is a clean fraction/multiple of the projection.

The script prints the observed peak next to the router's projection so these
can be told apart directly.

Usage:
    ./venv/bin/python kv_curve.py --backend http://localhost:8005 --words 1000
"""
import argparse
import asyncio
import json
import re
import time
import httpx

# Labels are optional: a metric may be emitted as either
#   vllm:kv_cache_usage_perc{model_name="x"} 0.25
#   vllm:kv_cache_usage_perc 0.25
# The earlier version of this file required the label block and would have
# silently reported "n/a" for every sample against an unlabelled exporter.
_KV_RE = re.compile(
    r'^vllm:kv_cache_usage_perc(?:\{[^}]*\})?\s+([\d.eE+-]+)', re.M)
_RUNNING_RE = re.compile(
    r'^vllm:num_requests_running(?:\{[^}]*\})?\s+([\d.eE+-]+)', re.M)


async def scrape(client, backend):
    """One /metrics sample. Returns (kv_usage_perc, num_running)."""
    try:
        text = (await client.get(f"{backend}/metrics", timeout=5.0)).text
    except Exception:
        return None, None
    kv = _KV_RE.search(text)
    run = _RUNNING_RE.search(text)
    return (float(kv.group(1)) if kv else None,
            float(run.group(1)) if run else None)


async def poll_metrics(client, backend, samples, t0, interval):
    """Sample /metrics until cancelled. Records (elapsed, kv, running)."""
    while True:
        kv, run = await scrape(client, backend)
        samples.append((time.monotonic() - t0, kv, run))
        await asyncio.sleep(interval)


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--backend", required=True)
    ap.add_argument("--words", type=int, default=1000,
                    help="prompt length in whitespace words (what loadgen.py counts)")
    ap.add_argument("--max-tokens", type=int, default=4000,
                    help="recorded only; echo mode ignores it")
    ap.add_argument("--kv-capacity", type=int, default=8192,
                    help="must match loadgen.py --kv-capacity")
    ap.add_argument("--interval", type=float, default=1.0,
                    help="metrics sampling period in seconds")
    a = ap.parse_args()

    # Distinct tokens, so no block-level prefix-cache dedup skews the result.
    prompt = " ".join(f"tok{i}" for i in range(a.words))
    prompt_words = len(prompt.split())

    payload = {
        "model": "test-model",
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": a.max_tokens,
        "stream": True,
    }

    samples = []
    tok_events = []   # (elapsed, cumulative_tokens)

    async with httpx.AsyncClient() as client:
        # Baseline BEFORE the request, so idle KV is known. The earlier version
        # of this file omitted this and could not distinguish "backend starts at
        # zero" from "backend has retained blocks from a previous run".
        idle_kv, idle_run = await scrape(client, a.backend)
        print(f"idle baseline: kv_cache_usage_perc={idle_kv}  num_requests_running={idle_run}")
        if idle_kv is None:
            print("WARNING: could not read kv_cache_usage_perc from /metrics. "
                  "Check the backend is up and exposes it.")

        t0 = time.monotonic()
        poller = asyncio.create_task(poll_metrics(client, a.backend, samples, t0, a.interval))

        c = 0
        err = None
        try:
            async with client.stream("POST", f"{a.backend}/v1/chat/completions",
                                     json=payload, timeout=600.0) as r:
                async for line in r.aiter_lines():
                    if not line.startswith("data:"):
                        continue
                    d = line[5:].strip()
                    if d == "[DONE]":
                        break
                    try:
                        o = json.loads(d)
                    except json.JSONDecodeError:
                        continue
                    if "error" in o:
                        err = o["error"].get("message", str(o["error"]))
                        break
                    ch = o.get("choices") or []
                    if not ch:
                        continue
                    txt = ch[0].get("delta", {}).get("content", "")
                    if not txt:
                        continue
                    c += 1
                    tok_events.append((time.monotonic() - t0, c))
        except Exception as e:
            err = str(e)
        t1 = time.monotonic() - t0

        # Keep sampling briefly past completion to confirm KV is released.
        await asyncio.sleep(a.interval * 3)
        poller.cancel()
        try:
            await poller
        except asyncio.CancelledError:
            pass

    if err:
        print(f"\nREQUEST ERROR: {err}")

    # What loadgen.py would have reserved for this exact request, under
    # --output-model echo: o_hat = p, so kv_new = p + o_hat = 2p.
    projected_tokens = 2 * prompt_words
    projected_occ = projected_tokens / a.kv_capacity

    print(f"\n{'='*64}")
    print(f"prompt words (what loadgen counts as p) : {prompt_words}")
    print(f"completion tokens actually generated    : {c}")
    print(f"request duration                        : {t1:.1f}s")
    print(f"{'='*64}")
    print(f"router WOULD reserve : {projected_tokens} tokens "
          f"= {projected_occ:.4f} occupancy (held from t=0)")
    print(f"{'='*64}\n")

    print(f"{'elapsed_s':>10}  {'kv_usage':>10}  {'running':>8}  {'tokens_so_far':>14}")
    peak = None
    for elapsed, kv, run in samples:
        tok_so_far = 0
        for te, tc in tok_events:
            if te <= elapsed:
                tok_so_far = tc
            else:
                break
        if kv is not None and (peak is None or kv > peak):
            peak = kv
        kv_s = f"{kv:.4f}" if kv is not None else "n/a"
        run_s = f"{run:.0f}" if run is not None else "n/a"
        print(f"{elapsed:>10.1f}  {kv_s:>10}  {run_s:>8}  {tok_so_far:>14}")

    print(f"\n{'='*64}")
    print("VERDICT")
    print(f"{'='*64}")
    if peak is None:
        print("No KV samples read. Cannot conclude. Check /metrics on the backend.")
        return

    print(f"observed PEAK kv_usage : {peak:.4f}")
    print(f"router projection      : {projected_occ:.4f}")
    ratio = projected_occ / peak if peak > 0 else float("inf")
    print(f"projection / peak      : {ratio:.2f}x")
    print()
    if c != prompt_words:
        print(f"NOTE: generated {c} tokens for a {prompt_words}-word prompt "
              f"(ratio {c/prompt_words:.2f}).")
        print("      loadgen.py counts WORDS but KV is in TOKENS, so this ratio is a")
        print("      systematic projection error. That is explanation 2 (a real bug).")
    if 0.85 <= ratio <= 1.15:
        print("Projection MATCHES the peak. The mid-run gap is explanation 1 (RAMP):")
        print("the router holds peak from t=0 while the backend climbs to it. Not a bug --")
        print("but it means router occupancy is an upper bound, and comparing it against an")
        print("instantaneous backend reading will always look high. Compare at peak instead.")
    else:
        print(f"Projection does NOT match the peak ({ratio:.2f}x off). This is not just the")
        print("ramp. Suspect the word-vs-token unit mismatch (explanation 2) or a wrong")
        print("kv_capacity denominator (explanation 3). A clean 2x/0.5x points at capacity;")
        print("a messy ratio matching the token/word ratio above points at units.")


if __name__ == "__main__":
    asyncio.run(main())
