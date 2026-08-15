# Keel

Keel is a load balancer built specifically for LLM-serving workloads.

Most load balancers were designed for traditional web traffic, where requests are roughly
uniform in cost and a simple request-count or connection-count heuristic is enough to keep
backends balanced. LLM inference does not fit that assumption: a single request can produce
a one-line answer or several pages of generated text, and the difference in actual work done
by the backend can span two to three orders of magnitude. A balancer that only counts
requests is blind to this, and it shows up as one backend quietly getting overloaded with
expensive work while others sit idle.

Keel routes on actual backend pressure rather than request count, so traffic distributes
according to real load instead of request volume.

## Status

Phase 0 stage 0a is complete. The problem has been reproduced and measured against
simulated backends, the mechanism behind it identified, and the proposed routing policy
validated against that mechanism.

The router itself is not yet written. Phase 0 exists to establish that the problem is real
and the approach works before committing to an implementation.

### What Phase 0 measured

Variable-length LLM traffic through four backends, byte-identical traces, three repeats per
cell, cold backends before every run. Errors are requests rejected because a backend ran out
of KV cache memory.

| arrival rate | least-connections | Keel `pressure` | reduction |
|---|---|---|---|
| 8 req/s | 2.0% | **0.3%** | 6.7x |
| 10 req/s | 3.5% | **0.9%** | 3.9x |
| 12 req/s | 6.1% | **1.9%** | 3.2x |
| 14 req/s | 10.8% | **3.3%** | 3.3x |

Against a real LLM-aware competitor, SGLang's production Rust router, at 10 req/s:

| router | policy | error rate |
|---|---|---|
| **Keel** | **pressure** | **1.03%** |
| Keel | least_conn | 4.60% |
| sgl-router | cache_aware | 4.97% |
| sgl-router | power_of_two | 5.10% |

The run-to-run ranges do not overlap: Keel spans 0.9-1.2% while the best single run any
competitor produced was 2.3%.

The mechanism was confirmed rather than assumed, and it is not the one originally expected.
Keel does **not** balance total load more evenly. It balances load *at each moment*.
sgl-router's `cache_aware` distributes cumulative KV load about eight times more evenly than
Keel does, and still fails nearly five times as often. What predicts failure is time spent
at the capacity ceiling: under least-connections some backend sits at or above it for 35-42%
of the run at higher rates, which Keel cuts to 15-24%.

> Spreading total work evenly across backends is not the same as preventing any backend from
> overflowing, and optimising the former can leave the latter completely untouched.

Full write-up, including the three measurement corrections that changed these numbers, in
[`journey/phase-0.md`](journey/phase-0.md).

### What is not yet established

These results come from `llm-d-inference-sim`, not real hardware. The simulator rejects
requests when KV memory runs out, whereas real vLLM preempts and recomputes. Whether the
improvement transfers is untested. Stage 0b repeats the same traces against real vLLM on a
GPU to close that gap.

The simulator also does not grow KV memory during generation, which real vLLM does. That
means the stronger form of the thesis, that *unpredictable output length* is the hidden cost,
is not testable on this harness. What is validated is that per-request cost varies by orders
of magnitude and is invisible to a request-counting router.

The trace contains no shared prefixes, which is the regime needed to make the memory limit
bind. `cache_aware`'s actual mechanism therefore has nothing to exploit, so the comparison
above is a fair test of KV discipline and not of cache-aware routing.

The comparison does not yet include llm-d, AIBrix, or NVIDIA Dynamo.

## Repository layout

```
keel-llm-router/
├── journey/          engineering log, one file per phase
├── phase0/           validation harness: trace generator, load generator, analysis
└── crates/           Rust workspace (Phase 1 onward, not yet present)
```

## Goals

Route LLM inference traffic based on the real cost of serving each request, not just how
many requests a backend is holding.

Work as a drop-in reverse proxy in front of standard OpenAI-compatible inference servers,
with no changes required on the backend or the client.

Stay lightweight and fast enough that the router itself is never the bottleneck.

Be benchmarked openly against common load-balancing approaches, with reproducible results.

## Tech stack

The router is written in Rust, chosen for predictable tail latency under load. A garbage
collected runtime introduces pauses that land directly in the percentile the project exists
to improve, and Rust's ownership model also makes the request accounting leak-free by
construction rather than by discipline.

The Phase 0 validation harness is Python, because that phase is about iterating on a
decision rule rather than serving traffic.

## License

TBD.
