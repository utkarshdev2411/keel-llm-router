import json
import time
import asyncio
import argparse
import csv
import random
import httpx

# ---------------------------------------------------------------------------
# Cost / score functions
# ---------------------------------------------------------------------------

def kvts_remaining(p, h, c, o_hat):
    """Committed KV-token-seconds still outstanding for one request.

    Kept for the `kvts` ablation arm. NOTE: this is a correct measure of
    committed WORK, but work is the wrong ranking signal (see `pressure`).
    """
    rem = max(0, o_hat - c)
    return rem * (p - h + c + rem / 2.0)


def occupancy(state, b):
    """Fraction of this backend's binding capacity that is committed.

    Two independent limits cause queueing; whichever is closer dominates:
      - slot limit      : in_flight / max_num_seqs
      - KV memory limit : projected peak KV tokens / kv capacity

    Dimensionless, so heterogeneous backends compare correctly, and so the
    256x absolute-work distortion that broke `kvts` cannot arise.
    """
    u_slots = state["in_flight"][b] / state["max_num_seqs"]
    u_kv = state["kv_proj"][b] / state["kv_capacity"]
    return max(u_slots, u_kv)


def pressure_score(state, b):
    """Linear below the knee, convex above it.

    Research basis:
      - A-stream: `c` (per-sequence cost) ~ 0 below the compute-bound
        threshold, so adding a sequence to an uncongested backend is nearly
        free -> the score must be ~flat there.
      - Vault 05 section 5.6: the KV-pressure failure is super-linear, so the
        penalty is quadratic, not linear.

    The linear term gives a gentle preference for emptier backends; the
    quadratic term dominates near capacity and makes the cliff visible to the
    router *before* it falls off it.
    """
    u = occupancy(state, b)
    theta = state["theta"]
    if u < theta:
        return u
    over = (u - theta) / (1.0 - theta)
    return u + state["penalty"] * (over ** 2)


def admits(state, b, kv_new):
    """Hard constraint. Preemption discards all completed work, so this is a
    gate, never a score term."""
    if state["in_flight"][b] >= state["max_num_seqs"]:
        return False
    projected = state["kv_proj"][b] + kv_new
    return projected <= state["sigma"] * state["kv_capacity"]


def choose_backend(backends, state, policy, kv_new):
    if policy == "proxy":
        # External router under test. It is the only endpoint; it picks the backend.
        return backends[0]

    if policy == "least_conn":
        m = min(state["in_flight"].values())
        return random.choice([b for b, v in state["in_flight"].items() if v == m])

    if policy == "kvts":
        m = min(state["W"].values())
        return random.choice([b for b, v in state["W"].items() if v == m])

    if policy == "kvts_p2c":
        b1, b2 = random.sample(backends, 2)
        return b1 if state["W"][b1] <= state["W"][b2] else b2

    if policy == "pressure":
        eligible = [b for b in backends if admits(state, b, kv_new)]
        pool = eligible if eligible else backends   # all full -> least-bad
        scores = {b: pressure_score(state, b) for b in pool}
        m = min(scores.values())
        return random.choice([b for b, v in scores.items() if v == m])

    if policy == "pressure_p2c":
        eligible = [b for b in backends if admits(state, b, kv_new)]
        pool = eligible if eligible else backends
        if len(pool) == 1:
            return pool[0]
        b1, b2 = random.sample(pool, 2)
        return b1 if pressure_score(state, b1) <= pressure_score(state, b2) else b2

    raise ValueError(policy)


# ---------------------------------------------------------------------------
# Request lifecycle
# ---------------------------------------------------------------------------

async def send_request(client, backend, req, req_id, sched_s, results, state,
                       prog, policy, charged0, p, o_hat0):
    t0 = time.monotonic()
    payload = {
        "model": "test-model",
        "messages": [{"role": "user", "content": req["prompt"]}],
        "max_tokens": req["max_tokens"],
        "stream": True,
    }
    charged = charged0        # kvts debt (kvts arms only)
    kv_held = (p + o_hat0) if state["kv_growth"] else p   # projected peak KV
    o_hat = o_hat0
    c = 0
    first_tok = None
    err = None

    try:
        async with client.stream("POST", f"{backend}/v1/chat/completions",
                                 json=payload, timeout=300.0) as r:
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
                # This simulator reports KV-exhaustion as HTTP 200 with an error
                # object embedded in the SSE stream, not as an HTTP error status
                # (that only happens non-streaming). Missing this made every
                # KV rejection look like a silent 0-token success: err% read 0.0%
                # while 61% of requests at rate 24 were actually being rejected.
                if "error" in o:
                    err = o["error"].get("message", str(o["error"]))
                    break
                ch = o.get("choices") or []
                if not ch:
                    continue
                txt = ch[0].get("delta", {}).get("content", "")
                if not txt:
                    continue
                if first_tok is None:
                    first_tok = time.monotonic()
                c += 1

                # Output ran past the estimate: extend it and re-charge both
                # accounting channels. Under-estimating output length is what
                # causes preemption, so this must never lag reality.
                if c >= o_hat:
                    o_hat += 50
                    if state["kv_growth"]:
                        new_kv = p + o_hat
                        state["kv_proj"][backend] += (new_kv - kv_held)
                        kv_held = new_kv

                if policy.startswith("kvts"):
                    new = kvts_remaining(p, 0, c, o_hat)
                    state["W"][backend] += (new - charged)
                    charged = new
    except Exception as e:
        err = str(e)
    finally:
        state["in_flight"][backend] -= 1
        state["kv_proj"][backend] -= kv_held
        if state["kv_proj"][backend] < 1e-9:
            state["kv_proj"][backend] = 0.0
        if policy.startswith("kvts"):
            state["W"][backend] -= charged
            if abs(state["W"][backend]) < 1e-6:
                state["W"][backend] = 0.0

    t1 = time.monotonic()
    ttft = (first_tok - t0) if first_tok else None
    prog["done"] += 1

    # print() is a BLOCKING syscall on the single-threaded event loop. While it
    # runs, no stream is read and every in-flight TTFT timer keeps ticking, so
    # per-request logging inflates the very metric being measured. Off by
    # default; errors always surface.
    if state["verbose"] or err:
        print(f"[{prog['done']}/{prog['total']}] {backend[-4:]} req#{req_id} tok={c} "
              f"ttft={f'{ttft*1000:.0f}ms' if ttft else 'n/a'} "
              f"e2e={t1-t0:.1f}s {'ERR: ' + err if err else 'ok'}", flush=True)

    results.append({
        "req_id": req_id, "backend": backend, "scheduled_offset_s": sched_s,
        "ttft_s": ttft, "e2e_s": t1 - t0,
        "requested_max_tokens": req["max_tokens"], "actual_tokens": c, "error": err,
    })


async def ticker(state, prog, stop):
    while not stop.is_set():
        await asyncio.sleep(5)
        if stop.is_set():
            break
        inf = " ".join(f"{b[-4:]}={v}" for b, v in state["in_flight"].items())
        occ = " ".join(f"{b[-4:]}={occupancy(state, b):.2f}" for b in state["in_flight"])
        print(f"  -- disp={prog['disp']}/{prog['total']} done={prog['done']}/{prog['total']}"
              f" | inflight[{inf}] | occupancy[{occ}]", flush=True)


async def run(trace_path, backends, out_path, policy, cfg):
    with open(trace_path) as f:
        trace = json.load(f)

    results = []
    state = {
        "in_flight": {b: 0 for b in backends},
        "W": {b: 0.0 for b in backends},
        "kv_proj": {b: 0.0 for b in backends},
        "max_num_seqs": cfg["max_num_seqs"],
        "kv_capacity": cfg["kv_capacity"],
        "theta": cfg["theta"],
        "penalty": cfg["penalty"],
        "sigma": cfg["sigma"],
        "verbose": cfg["verbose"],
        "kv_growth": cfg["kv_growth"],
    }
    prog = {"total": len(trace), "disp": 0, "done": 0}
    stop = asyncio.Event()
    start = time.monotonic()
    tick = asyncio.create_task(ticker(state, prog, stop))
    lagged = 0
    gated = 0

    async with httpx.AsyncClient(limits=httpx.Limits(max_connections=400)) as client:
        tasks = []
        for i, req in enumerate(trace):
            target = start + req["offset_ms"] / 1000.0
            wait = target - time.monotonic()
            if wait > 0:
                await asyncio.sleep(wait)
            else:
                if -wait > 0.1:
                    lagged += 1
                await asyncio.sleep(0)   # always yield: state must never be stale

            p = len(req["prompt"].split())
            o_hat = req["max_tokens"]
            # kv_growth=False matches this simulator (allocates by prompt only).
            # Real vLLM grows KV per generated token -> use True in stage 0b.
            kv_new = (p + o_hat) if cfg["kv_growth"] else p
            charge = kvts_remaining(p, 0, 0, o_hat) if policy.startswith("kvts") else 0.0

            if policy.startswith("pressure"):
                if not any(admits(state, b, kv_new) for b in backends):
                    gated += 1

            backend = choose_backend(backends, state, policy, kv_new)

            # All state mutated synchronously at dispatch, before the next
            # routing decision can observe it.
            state["in_flight"][backend] += 1
            state["kv_proj"][backend] += kv_new
            if policy.startswith("kvts"):
                state["W"][backend] += charge
            prog["disp"] += 1

            tasks.append(asyncio.create_task(
                send_request(client, backend, req, i, req["offset_ms"] / 1000.0,
                             results, state, prog, policy, charge, p, o_hat)))

        print(f"all dispatched (policy={policy}, lag_events={lagged}, "
              f"saturated_dispatches={gated}), draining...", flush=True)
        await asyncio.gather(*tasks)

    stop.set()
    tick.cancel()
    try:
        await tick
    except asyncio.CancelledError:
        pass

    with open(out_path, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=["req_id", "backend", "scheduled_offset_s",
                                          "ttft_s", "e2e_s", "requested_max_tokens",
                                          "actual_tokens", "error"])
        w.writeheader()
        for r in sorted(results, key=lambda x: x["req_id"]):
            w.writerow(r)
    print(f"Done. {len(results)} requests -> {out_path} "
          f"(lag_events={lagged}, saturated_dispatches={gated})", flush=True)


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--trace", required=True)
    ap.add_argument("--backends", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--policy", default="least_conn",
                    choices=["least_conn", "kvts", "kvts_p2c", "pressure", "pressure_p2c",
                             "proxy"])
    # must match the simulator's flags
    ap.add_argument("--max-num-seqs", type=int, default=8)
    ap.add_argument("--kv-capacity", type=int, default=8192)  # kv-cache-size 512 x block-size 16
    # scoring knobs
    ap.add_argument("--theta", type=float, default=0.70)   # knee: penalty starts here
    ap.add_argument("--penalty", type=float, default=10.0) # convex weight above the knee
    ap.add_argument("--sigma", type=float, default=0.90)   # admission ceiling
    ap.add_argument("--kv-growth", action="store_true",
                    help="model KV as growing per generated token (real vLLM). "
                         "OFF matches llm-d-inference-sim, which allocates by "
                         "prompt length at admission only.")
    ap.add_argument("--verbose", action="store_true",
                    help="log every completed request. OFF by default: print() "
                         "blocks the event loop and inflates measured TTFT.")
    a = ap.parse_args()

    cfg = {
        "max_num_seqs": a.max_num_seqs,
        "kv_capacity": a.kv_capacity,
        "theta": a.theta,
        "penalty": a.penalty,
        "sigma": a.sigma,
        "verbose": a.verbose,
        "kv_growth": a.kv_growth,
    }
    asyncio.run(run(a.trace, a.backends.split(","), a.out, a.policy, cfg))
