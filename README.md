# Keel

**A load balancer that understands what LLM requests actually cost.**

Keel is a reverse proxy for OpenAI-compatible inference servers. It routes on *KV-cache
pressure* rather than request count, because for LLM traffic the scarce resource is GPU memory,
not connections.

```
┌────────┐      ┌──────────────┐      ┌─────────────────┐
│ client │─────▶│     Keel     │─────▶│  vLLM / SGLang  │
└────────┘      │              │      │  vLLM / SGLang  │
                │  pressure    │      │  vLLM / SGLang  │
                │  + KV gate   │─────▶│  vLLM / SGLang  │
                └──────────────┘      └─────────────────┘
                       │
                  scrapes /metrics
                  (audit only, never
                   a routing input)
```

---

## The problem

Every general-purpose load balancer — nginx, HAProxy, Envoy — routes by request count or
round-robin. That works when requests cost roughly the same. LLM requests do not: one returns a
single line, the next generates several pages, and the memory each holds can differ by two to
three orders of magnitude.

A request-counting balancer is blind to this. It will happily hand a backend its "fair share" of
requests while that backend is one token away from exhausting its KV cache, and hand another
backend the same share while it sits half idle. The failure is not slow responses. It is
**requests dropped or preempted because a backend ran out of memory**, while capacity sat unused
next door.

---

## The insight

The obvious fix — track how much total work each backend holds — was measured and **it made
things worse than plain least-connections at every arrival rate tested.** Valuing one long
request at ~180x a short one makes the router avoid a backend holding a single long generation
even when most of its memory is free.

What actually works is ranking by **fraction of capacity consumed**:

```
u = max( in_flight / max_num_seqs , kv_projected / kv_capacity )
```

linear below a knee, convex above it. Two consequences fall out, and both were measured rather
than assumed:

**Balance load at each moment, not in total.** These are different properties and they move in
opposite directions. A competing production router distributed cumulative KV load about *eight
times more evenly* than Keel — and still failed nearly five times as often. Only the
instantaneous view predicts failure.

**Do not predict output length, observe it.** Charge a rough estimate at dispatch, correct on
every streamed chunk. The error decays to zero as the request runs.

---

## Results

### Against request-counting routers (simulated backends)

Four backends, byte-identical traces, three repeats per cell, cold backends before every run.
Errors are requests rejected because a backend ran out of KV cache.

| arrival rate | least-connections | Keel `pressure` | reduction |
|---|---|---|---|
| 8 req/s | 2.0% | **0.3%** | **6.7x** |
| 10 req/s | 3.5% | **0.9%** | **3.9x** |
| 12 req/s | 6.1% | **1.9%** | **3.2x** |
| 14 req/s | 10.8% | **3.3%** | **3.3x** |

Least-connections is the algorithm nginx, HAProxy and Envoy implement, configured to its
strongest setting rather than its default round-robin.

### Against a production LLM-aware router (simulated backends)

SGLang's production Rust router, at 10 req/s:

| router | policy | error rate |
|---|---|---|
| **Keel** | **pressure** | **1.03%** |
| Keel | least_conn | 4.60% |
| sgl-router | cache_aware | 4.97% |
| sgl-router | power_of_two | 5.10% |

Run-to-run ranges do not overlap: Keel spans 0.9–1.2%, the best single run any competitor
produced was 2.3%.

### On real vLLM

Two vLLM v0.26.0 backends, pinned identical KV capacity, cold container restart before every
cell, seeded repeats per arm. Two observations, at two different loads.

**The mechanism**, at moderate load, three seeds. Count-balancing splits requests perfectly and
lets memory drift. Keel does the opposite:

| | request-count split | memory spread between backends |
|---|---|---|
| Count-balanced routing | 16 / 16 — perfectly even | 5, 11, **45** points apart |
| Keel `pressure` | 13 / 19 — deliberately *uneven* | **2** points, on every seed |

**The consequence**, at higher load, four seeds — enough pressure for the engine to start
evicting requests:

| | request-count split | engine preemptions |
|---|---|---|
| Count-balanced routing | 28 / 28 | **10 events**, in 3 of 4 runs |
| Keel `pressure` | 24 / 32 | **0**, in every run |

Keel sends deliberately uneven request counts in order to keep memory even. Note also that the
count-balanced arm's imbalance is luck-of-the-draw — 5 points on one seed, 45 on another — while
Keel lands on 2 points every time.

---

## Design

**Rank by fraction of capacity, never by absolute work.** Backends may be heterogeneous;
occupancy is dimensionless, so they compare directly.

**The admission gate filters, it never refuses.** When no backend passes the KV safety ceiling,
Keel dispatches to the least-bad one anyway. A gate that rejects turns a routing result into a
load-shedding artifact, because rejected requests carry no latency sample.

**Work accounting cannot leak.** Every request holds an RAII lease bound to the response
stream's lifetime, so dropping the stream releases the charge. `in_flight == 0` implies
`kv_projected == 0`, asserted continuously — verified holding across every run on real hardware.

**Backend metrics are an audit, never an input.** Keel scrapes each backend's `/metrics` on a
clock and cross-checks its own projection against the backend's reported usage. That signal
raises alarms. It never reaches a routing decision. The boundary is enforced by the type system
and verified by grep, not by convention.

**Capacity is asserted at startup.** KV capacity is the denominator of every occupancy fraction.
Keel scrapes each backend before accepting traffic and refuses to start on a mismatch, rather
than silently rescaling every decision it will ever make.

### Stack

Rust, on `hyper`. Chosen for predictable tail latency — a garbage-collected runtime introduces
pauses that land directly in the percentile this project exists to improve. `router-core` is
pure: no I/O, no tokio, enforced at compile time via `clippy.toml` disallowed types. All
transport lives in `router-proxy`.

---

## Quick start

```bash
cargo build --release
./target/release/router router.toml
```

Minimal configuration:

```toml
[listener]
bind = "0.0.0.0:8080"
admin_bind = "127.0.0.1:9090"

[routing]
strategy = "pressure"
kv_model = "prompt_plus_output"   # real vLLM grows KV during decode
theta    = 0.55
penalty  = 10.0

[admission]
sigma = 0.95

[[backends]]
url          = "http://127.0.0.1:8000"
model        = "test-model"
kv_tokens    = 8192   # block_size * num_gpu_blocks, read from the engine
max_num_seqs = 32
```

Metrics are exported in Prometheus format on `admin_bind`.

---

## What is proven, and what is not

Stated precisely, because the numbers above are only worth what their caveats allow.

**Established.** The mechanism is real and reproduces on real vLLM: Keel trades request-count
balance for memory balance, deterministically, and the memory balance is roughly ten times
tighter than count-balancing achieves. There is a load window in which that eliminates engine
preemption outright. Work accounting does not leak. Capacity misconfiguration is caught at
startup.

**Not established.**

- The **error-rate tables above are simulator-measured**, not real hardware. The real-vLLM run
  confirms the *mechanism*, over one load window with four repeats — a single well-controlled
  test, not a swept statistical claim.
- **No named competitor has been benchmarked on real hardware yet.** The sgl-router comparison
  is simulator-only. llm-d, AIBrix and NVIDIA Dynamo are not yet compared at all.
- **Scale is untested.** Real-hardware validation ran on one consumer GPU with a 0.5B model and
  two backends. Nothing here speaks to cluster scale.
- **σ is uncalibrated, not optimal.** Swept across 0.85–0.98 on real hardware over 24 runs and
  found to have *no measurable effect* on preemption in that range — informative, but it means
  0.95 is interchangeable rather than tuned.
- Traces contain **no shared prefixes**, so cache-aware routing has nothing to exploit. The
  comparison is a fair test of KV discipline, not of prefix caching.

The full engineering log — including every measurement correction that changed a headline
number, and the bugs found by running the thing rather than trusting its test suite — is in
[`journey/`](journey/).

---

## Roadmap

- **Prefix affinity** — route shared prompt prefixes to the same backend, bounded so one popular
  prefix cannot overload it. Conditional on real traffic actually having shared prefixes.
- **Full statistical validation on real hardware** — ablation ladder, negative control,
  confidence intervals, and competitor comparison, repeating on GPU what has been done on the
  simulator.
- **Broader competitor set** — llm-d, AIBrix, NVIDIA Dynamo.

---

## License

Apache License 2.0. See [LICENSE](LICENSE).
