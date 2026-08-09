"""Trace generator for Phase 0.

Produces a fixed file of (arrival_offset_ms, prompt, max_tokens) so every run
replays byte-identical traffic.

Output-length model
-------------------
Real LLM output lengths are heavy-tailed, not bimodal. Default is lognormal:

    median 120 tokens, sigma 1.1, clipped to [20, 3000]
      p50 ~ 120   p90 ~ 490   p99 ~ 1550   mean ~ 220

Prompt/output decoupling (echo mode)
------------------------------------
The simulator's echo mode returns the prompt, truncated by max_tokens. So:

    prompt_words = max(p, o)   -> enough text to echo
    max_tokens   = o           -> truncates the echo to exactly o

giving actual_prompt = max(p, o) and actual_output = o. When p > o this yields
LONG PROMPT / SHORT OUTPUT (the RAG pattern), which is where cost-aware and
count-based routing diverge most.

Known limit: cannot produce SHORT prompt / LONG output. Documented stage-0a
limitation; real vLLM in stage 0b covers that case.
"""

import json
import math
import random
import string
import argparse

# Distinct vocabulary. Prompts built from this produce distinct KV blocks, so
# the backend's KV cache actually fills.
#
# WHY THIS EXISTS: the previous generator emitted "word word word ..." for every
# request, making every prompt a prefix of every longer one. Block-level hashing
# deduped them, 20 concurrent requests cost what 1 cost, and the KV limit never
# bound. Measured: 20/20 identical prompts succeeded where 13/20 unique prompts
# hit HTTP 500 (KV capacity) at identical demand.
_VOCAB = None

def vocab(rng, size=20000):
    global _VOCAB
    if _VOCAB is None:
        _VOCAB = ["".join(rng.choices(string.ascii_lowercase, k=6)) for _ in range(size)]
    return _VOCAB


def lognormal_clipped(rng, median, sigma, lo, hi):
    mu = math.log(median)
    v = rng.lognormvariate(mu, sigma)
    return int(round(max(lo, min(hi, v))))


def make_prompt(rng, n_words, shared_prefix=None):
    """Unique body, with an optional shared prefix.

    shared_prefix models real structure (system prompts, few-shot templates)
    where genuine cache reuse exists. Keep it LOW for Phase 0: the point here is
    to make the KV limit bind. Raise it in Phase 4 to test prefix affinity.
    """
    n_words = max(1, n_words)
    v = vocab(rng)
    if shared_prefix:
        pre_len = len(shared_prefix.split())
        n_unique = max(1, n_words - pre_len)
        return shared_prefix + " " + " ".join(rng.choice(v) for _ in range(n_unique))
    return " ".join(rng.choice(v) for _ in range(n_words))


def sample_lengths(kind, rng, args):
    """Return (prompt_tokens_wanted, output_tokens_wanted)."""
    if kind == "uniform":
        # Negative control: every request costs the same. All policies MUST tie
        # here. If a cost-aware policy wins on this trace, there is a bug.
        return args.uniform_prompt, args.uniform_output

    if kind == "bimodal":
        # Kept for comparison against earlier runs.
        if rng.random() < 0.8:
            return 50, 50
        return 800, 800

    if kind == "lognormal":
        o = lognormal_clipped(rng, args.out_median, args.out_sigma,
                              args.out_min, args.out_max)
        p = lognormal_clipped(rng, args.prompt_median, args.prompt_sigma,
                              args.prompt_min, args.prompt_max)
        return p, o

    raise ValueError(kind)


def generate_trace(kind, num_requests, rate_per_sec, seed, args):
    rng = random.Random(seed)
    v = vocab(rng)

    # Pool of shared system prompts, used only when --shared-prefix-frac > 0.
    prefixes = [" ".join(rng.choice(v) for _ in range(args.shared_prefix_len))
                for _ in range(args.shared_prefix_pool)] if args.shared_prefix_frac > 0 else []

    trace = []
    t = 0.0
    for _ in range(num_requests):
        t += rng.expovariate(rate_per_sec)
        p_want, o_want = sample_lengths(kind, rng, args)
        prompt_words = max(p_want, o_want)     # must be long enough to echo o
        pre = rng.choice(prefixes) if prefixes and rng.random() < args.shared_prefix_frac else None
        trace.append({
            "offset_ms": round(t * 1000),
            "prompt": make_prompt(rng, prompt_words, pre),
            "max_tokens": o_want,
            "_prompt_tokens": prompt_words,     # for verification only
            "_expected_output": o_want,
            "_shared": bool(pre),
        })
    return trace


def summarize(trace):
    outs = sorted(r["_expected_output"] for r in trace)
    prompts = sorted(r["_prompt_tokens"] for r in trace)

    def pct(s, q):
        return s[min(len(s) - 1, int(len(s) * q))]

    dur = trace[-1]["offset_ms"] / 1000.0
    total_out = sum(outs)
    total_p = sum(prompts)
    decoupled = sum(1 for r in trace if r["_prompt_tokens"] > r["_expected_output"])

    print(f"  requests        : {len(trace)}")
    print(f"  duration        : {dur:.1f}s  (arrival window)")
    print(f"  output tokens   : p50={pct(outs,0.50)}  p90={pct(outs,0.90)}  "
          f"p99={pct(outs,0.99)}  max={outs[-1]}  mean={total_out/len(outs):.0f}")
    print(f"  prompt tokens   : p50={pct(prompts,0.50)}  p90={pct(prompts,0.90)}  "
          f"p99={pct(prompts,0.99)}  max={prompts[-1]}  mean={total_p/len(prompts):.0f}")
    print(f"  total out tokens: {total_out:,}")
    print(f"  long-prompt/short-output requests: {decoupled} "
          f"({decoupled/len(trace)*100:.0f}%)")

    # Rough capacity note: helps pick rates for the knee sweep.
    mean_out = total_out / len(outs)
    svc = mean_out * 0.020            # inter-token-latency 20ms, unloaded
    # Backends slow down as they fill (--time-factor-under-load), so service
    # time inflates with utilisation and effective capacity DROPS. Solving
    # u = lam*svc*(1+(F-1)*u)/S for u=1 gives the true saturation rate.
    mean_kv = (total_p + total_out) / len(outs)   # prompt + generated, held per request
    KV_PER_BACKEND, BACKENDS, F = 8192.0, 4, 2.5
    kv_slots = KV_PER_BACKEND / mean_kv           # concurrent requests before KV exhaustion
    S = kv_slots * BACKENDS
    lam_sat = S / (svc * F)
    shared = sum(1 for r in trace if r.get("_shared"))
    print(f"  shared-prefix requests: {shared} ({shared/len(trace)*100:.0f}%)")
    print(f"  mean KV held/request  : {mean_kv:.0f} tokens "
          f"=> ~{kv_slots:.1f} concurrent per backend before KV exhaustion")
    print(f"  mean service time     : ~{svc:.1f}s unloaded, ~{svc*F:.1f}s saturated")
    print(f"  => KV-bound capacity ~{S:.0f} concurrent; saturation near {lam_sat:.1f} req/s")
    print(f"  => sweep rates around {lam_sat*0.5:.1f} - {lam_sat*1.3:.1f}")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--kind", choices=["uniform", "bimodal", "lognormal"],
                    default="lognormal")
    ap.add_argument("--num-requests", type=int, default=500)
    ap.add_argument("--rate", type=float, default=5.0)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--out", required=True)

    # lognormal output distribution
    ap.add_argument("--out-median", type=float, default=120.0)
    ap.add_argument("--out-sigma", type=float, default=1.1)
    ap.add_argument("--out-min", type=int, default=20)
    ap.add_argument("--out-max", type=int, default=3000)

    # lognormal prompt distribution
    ap.add_argument("--prompt-median", type=float, default=200.0)
    ap.add_argument("--prompt-sigma", type=float, default=1.0)
    ap.add_argument("--prompt-min", type=int, default=20)
    ap.add_argument("--prompt-max", type=int, default=4000)

    # shared prefixes: keep at 0 for Phase 0 (make KV bind); raise for Phase 4
    ap.add_argument("--shared-prefix-frac", type=float, default=0.0)
    ap.add_argument("--shared-prefix-pool", type=int, default=5)
    ap.add_argument("--shared-prefix-len", type=int, default=200)

    # uniform (negative control)
    ap.add_argument("--uniform-prompt", type=int, default=100)
    ap.add_argument("--uniform-output", type=int, default=100)

    a = ap.parse_args()

    trace = generate_trace(a.kind, a.num_requests, a.rate, a.seed, a)
    with open(a.out, "w") as f:
        json.dump(trace, f)

    print(f"Wrote {a.out}  (kind={a.kind}, rate={a.rate}/s, seed={a.seed})")
    summarize(trace)
