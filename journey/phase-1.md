---
tags: [journey, phase-1, transport, rust]
status: complete
created: 2026-08-16
updated: 2026-08-18
---

# Phase 1: Building the Instrument

Phase 0 established that the problem is real and that a capacity-aware policy fixes it, using
a Python harness against simulated backends. Phase 1 builds the actual router in Rust, and it
deliberately does **not** implement that policy.

The goal here is a correct, fast, observable proxy running only *baseline* strategies, the
same ones a conventional load balancer runs. If the proxy cannot match a conventional load
balancer while running the same algorithm, then every later measurement is reporting a
transport bug rather than a routing improvement. This phase builds the instrument; the next
one uses it.

---

## 1. Crate layout, and one rule enforced by tooling

Three crates, with a dependency direction that is checked mechanically rather than by
convention:

```
router-bin     main, config load, wiring
    |
router-proxy   all I/O: hyper, tokio, listener, upstream relay, health, metrics
    |
router-core    pure: types, scoring, admission gate, config schema
```

`router-core` may not contain an `async fn`, a `tokio` import, a socket, or a wall-clock read.
`Instant` is always passed in by the caller. This is enforced through `clippy.toml`:

```toml
disallowed-types = [
    { path = "std::time::SystemTime", reason = "non-monotonic; use Instant, passed in" },
    { path = "tokio::time::Instant", reason = "router-core must not depend on tokio" },
]
```

with `#![deny(clippy::disallowed_types)]` on the crate.

The reason is not tidiness. A routing decision that is a pure function of
`(Snapshot, RequestFeatures, seed)` can be replayed offline, which is what makes parameter
sweeps cheap and property testing possible at all. One `.await` inside `router-core` and that
property is gone, quietly. Enforcing it in the linter means it cannot erode by accident.

---

## 2. What was built

| Component | File | Responsibility |
|---|---|---|
| Proxy core | `router-proxy/src/inbound.rs` | Body buffering with a size cap, JSON parse, feature extraction |
| Streaming relay | `router-proxy/src/upstream.rs` | Request rebuild, unbuffered frame relay, `CountingSseBody` |
| SSE classification | `router-proxy/src/sse.rs` | Frame typing, including the in-band error frame |
| Backend registry | `router-core/src/backend.rs` | Immutable `Snapshot`, cache-line-aligned atomics |
| Health / ejection | `router-proxy/src/health.rs` | Consecutive-failure ejection, healthy-set rebuild |
| Baseline strategies | `router-core/src/strategy/` | `round_robin`, `least_requests`, `p2c` |
| Accounting | `router-core/src/lease.rs` | RAII `CostLease`, released on every exit path |
| Observability | `router-proxy/src/observe.rs` | Prometheus exporter, structured logs, decision traces |

The cost model and the `pressure` policy exist as types and functions in `router-core` but are
not wired into any shipping strategy yet. That is Phase 2.

### The load generator was not rewritten

`phase0/loadgen.py` already is an open-loop generator that measures from scheduled arrival
time, asserts its own schedule adherence, and parses the SSE surface correctly. It produced
every published number in Phase 0. Reimplementing it in Rust would mean the numbers being
defended and the tool defending them could drift apart silently. It is pointed at the Rust
router through `--policy proxy` instead, exactly as it was already pointed at `sgl-router`
during the Phase 0 competitor benchmark.

---

## 3. Two details that carry most of the correctness

### The lease must be owned by the response body

`CostLease` and `CountingSseBody` decrement a backend's in-flight count in `Drop`, and the body
is what owns them. Tokio drops the whole future when a client disconnects, so a disconnect
releases the accounting with no special-case code. Holding the lease *beside* the body instead
would leak on precisely the path that is hardest to test.

The invariant worth asserting, and the one asserted in tests: **when `inflight` is 0,
`kv_projected_tokens` must be 0.** A leak here is silent and cumulative, and it presents as a
healthy backend slowly receiving less traffic, which is indistinguishable from a real
algorithmic finding until somebody checks.

### Error frames arrive on HTTP 200

A streaming request can fail after the status line has already been sent. The status is `200
OK`, the headers are flushed, and the failure has nowhere to go but the body:

```
data: {"error":{"message":"the kv cache does not have sufficient capacity ..."}}
data: [DONE]
```

There is no `choices` key at all, so a parser that branches on `choices[0].delta` never
reaches it and records the request as a *successful* response that happened to produce zero
tokens. In Phase 0 this made a measured error rate read 0.0% against a true rate near 60%.

`sse::classify` therefore checks for the `error` key **before** looking at `choices`, and a
separate test asserts that counted content frames reconcile against the usage frame's token
count. Phase 1's run confirmed this works end to end: 204 in-band KV-exhaustion errors were
detected and attributed across the verification runs, with zero zero-token successes.

---

## 4. Bugs found while building

Recorded because each cost real time, and most are invisible once fixed.

| Bug | Symptom | Cause |
|---|---|---|
| Toolchain pin too old | `feature edition2024 is required` on a transitive dep | `rust-toolchain.toml` pinned 1.75.0; moved to `stable` |
| `SmallRng` not found | Unresolved import in all three strategies | `rand`'s `small_rng` feature not enabled |
| `pin_project!` drop conflict | `conflicting implementations of MustNotImplDrop` | A manual `Drop` on a pin-projected struct needs `PinnedDrop` inside the macro block, not an `impl Drop` outside it |
| Occupancy test failed | `A should score as emptier: 0.73 vs 0.5` | My own test data was wrong, not the algorithm. Rewritten to the worked example from the design docs (A=0.125, B=0.500) |
| Router started silent | No output at all, no way to tell if it bound | `EnvFilter::from_default_env()` enables nothing when `RUST_LOG` is unset. Now defaults to `info` |
| Every request failed to connect | `All connection attempts failed`, 1500 times | The verification runbook chained `cargo build --release && ./target/release/router` into one backgrounded command followed by `sleep 2`. A cold release build takes 30-70s, so the load generator started firing while cargo was still compiling |
| `verify.py` false failure | "some backends received no traffic" | Its backend-coverage check cannot work in proxy mode, where the generator only ever sees one URL. It skips filenames containing `proxy`; the output file had to be named accordingly |

The last two are worth dwelling on: both were defects in the *verification procedure*, not in
the router. A runbook that races itself produces a wall of connection errors that looks exactly
like a broken proxy, and a checker that reports a structural false positive trains you to
ignore it. Neither would have been caught by unit tests.

---

## 5. Does it match the reference?

The gate for this phase: running a baseline strategy, the Rust router must reproduce
`loadgen.py --policy least_conn` on the same trace, on error rate and per-backend dispatch,
with router overhead p99 under 1 ms and a clean integrity check.

1500 requests at arrival rate 8, three repeats per arm, backends restarted cold before every
run, identical trace and seed.

### Error rate

| Run | Reference (`least_conn`, direct) | Ours (`least_requests`, proxied) |
|---|---|---|
| 1 | 3.3% | 2.2% |
| 2 | 2.5% | 3.8% |
| 3 | 1.6% | 1.5% |
| **mean** | **2.5%** | **2.5%** |
| range | 1.6 - 3.3 | 1.5 - 3.8 |

Means are identical and the ranges overlap heavily.

### Latency

| Metric | Reference | Ours |
|---|---|---|
| TTFT p50 | 63-64 ms | 63-64 ms |
| TTFT p95 | 67 ms | 67 ms |
| TTFT p99 | 68 ms | 68 ms |

Indistinguishable, which is the point: the same algorithm should produce the same curve.

### Per-backend distribution

Proxy mode cannot show this in the results table, because the generator only ever sees the
router's single URL and every row shares a backend value. Recovered instead by diffing each
backend's Prometheus counters either side of a run:

```
per-backend served:  363  337  375  370
request spread:      10.5%
KV-token spread:     6.2%
```

The reference runs showed request spread between 6.1% and 13.2%. Ours sits inside that range.

### Router overhead

Body parse, feature extraction and the routing decision, measured up to the moment the
upstream request is dispatched, over 1500 requests:

| Quantile | Overhead |
|---|---|
| p50 | 72 µs |
| p90 | 168 µs |
| p95 | 208 µs |
| **p99** | **257 µs** |
| max | 258 µs |

Roughly 4x inside the 1 ms budget, and about 0.4% of the ~68 ms TTFT it sits in front of.

### Measurement integrity

`verify.py` across all six result files: **8 passed, 0 warnings, 0 failed, 2 skipped.** The two
skips are the KV-accounting-leak and coordinated-omission checks, which need a run log this
sequence did not capture. Worth wiring in before Phase 2's comparison, since the leak check is
the strongest structural assertion available.

### A false alarm worth recording

An earlier smoke run at 200 requests showed 8 errors for the reference against 1 for ours, and
looked like a real behavioural divergence. It was not. At that sample size a handful of
KV rejections is noise, and the full 1500-request runs with repeats show the two arms landing
on the same mean. The lesson is the one Phase 0 already taught and this nearly repeated:
a single small run cannot distinguish a difference from variance, and the instinct to explain
an interesting-looking gap should wait until the gap survives repetition.

---

## 6. What this does and does not establish

**Established.** The transport is correct. Streaming relays unmodified and unbuffered, frames
split across network reads are handled, in-band error frames are detected rather than silently
counted as empty successes, accounting does not leak, and the router's own cost is small enough
relative to TTFT to be irrelevant to any later comparison. When running the same algorithm as
the reference, it produces the same numbers.

**Not established, deliberately.** Nothing about routing quality. `least_requests` is a
baseline, not a contribution. The capacity-aware policy that Phase 0 validated in Python is not
wired into any shipping strategy yet, the admission gate is not on the request path, and no
backend metrics are being scraped. Those are Phase 2.

**Known gaps.**

- Prompt token counts are byte-length estimates, not exact tokenizer counts. Adequate for
  baseline strategies, which never read the value; not adequate for a policy that projects KV
  from it.
- The two `verify.py` checks that need a run log were skipped.
- Only `least_requests` was compared against the reference. `round_robin` and `p2c` are
  implemented and unit-tested but have not been run against the harness.
- All of this is still against `llm-d-inference-sim`, which rejects under KV pressure rather
  than preempting. That limitation is inherited from Phase 0 and is unchanged here.

---

## Files

| Path | Purpose |
|---|---|
| `crates/router-core/` | Pure routing core: types, scoring, gate, config |
| `crates/router-proxy/` | I/O layer: listener, inbound, upstream, SSE, health, metrics |
| `crates/router-bin/` | Binary: config load and wiring |
| `bench/configs/` | Baseline configs for nginx, HAProxy, Envoy, and this router |
| `bench/README.md` | The comparison matrix and the verification runbook used above |
