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

Running variable-length LLM traffic through four backends with standard least-connections
routing, then comparing against Keel's pressure-based policy on byte-identical traffic:

| arrival rate | least-connections error rate | Keel error rate | reduction |
|---|---|---|---|
| 4 req/s | 15.8% | 4.3% | 73% |
| 6 req/s | 17.1% | 12.5% | 27% |
| 8 req/s | 29.1% | 19.7% | 32% |

Errors here are requests rejected because a backend ran out of KV cache memory.

The mechanism behind the improvement was confirmed rather than assumed. KV memory is
allocated from prompt length, and under least-connections the distribution of prompt tokens
across backends was 25.8% imbalanced while output tokens looked almost perfectly balanced at
1.9%. Rejections tracked prompt-token load in exact order. Keel's policy cut that imbalance
to 6.9%, and the error rate fell in step.

Full write-up in [`journey/phase-0.md`](journey/phase-0.md).

### What is not yet established

These results come from `llm-d-inference-sim`, not real hardware. The simulator rejects
requests when KV memory runs out, whereas real vLLM preempts and recomputes. Whether the
improvement transfers is untested. Stage 0b repeats the same traces against real vLLM on a
GPU to close that gap.

The comparison is also against least-connections, which is what nginx and HAProxy do. It is
not yet a comparison against LLM-aware routers such as llm-d, AIBrix, or NVIDIA Dynamo.

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
