---
tags: [journey, phase-2, routing, admission-control, rust]
status: complete (one open item, see §6)
created: 2026-08-18
updated: 2026-08-23
---

# Phase 2: Occupancy Accounting, and Learning Not to Trust a Green Build

Phase 1 proved the transport was faithful: running a baseline strategy, the Rust router
reproduced the Python reference exactly. Phase 2 puts the actual capacity-aware policy on the
request path — live occupancy accounting, the `pressure` scoring rule, and the admission gate
that never refuses a request, only filters candidates.

The algorithm itself was correct from the first pass. Every bug this phase found was in the
*measurement*: a token count that quietly meant something other than what it said, a metric
that never fired because nothing fed it, and a benchmark script that measured the same router
process twice and called it two arms. All were caught by actually running traffic and reading
the numbers with suspicion, not by code review or unit tests — the unit tests passed the whole
time. That is the throughline of this phase, and it is worth stating plainly: a green test
suite proves the code does what the tests describe. It does not prove the tests describe the
right thing.

---

## 1. What was built

| Component | Responsibility |
|---|---|
| `router_core::cost` | `occupancy(backend) = max(inflight/max_num_seqs, kv_projected/kv_capacity)`, and `pressure_score`: linear below θ, `u + P·((u−θ)/(1−θ))²` above |
| `router_core::gate` | Admission filter: eligible iff `kv_projected + need_kv <= σ·kv_capacity` and `inflight < max_num_seqs`. Empty result is a valid outcome |
| `router_core::strategy::pressure` | Full argmin over eligible backends, random tie-break. Falls through to the full healthy set — never the eligible-but-empty set — when nothing passes the gate, and dispatches to the least-bad backend anyway |
| `router_core::lease::CostLease` | RAII charge, released on every exit path via `Drop`; recharges upward, never down, when observed output exceeds the estimate |
| `router_core::tokens::TokenCounter` | Prompt-token counting as configuration, not a hardcoded heuristic (§3) |
| `router_proxy::length_estimator` | Per-route decaying output-length estimate, consulted only when the client sends no `max_tokens` |
| `router_proxy::sampler` | Periodic all-backend occupancy sampling, independent of traffic (§4) |
| `router_proxy::observe` | `router_prompt_token_ratio`, a metric that exists specifically to catch the next version of this phase's core bug (§3) |

The gate is part of this phase, not a later one: every result Phase 0 validated ran with the
gate switched on, so shipping `pressure` without it would mean shipping an untested
configuration.

---

## 2. The bug that cost the most time: a token count that was wrong by 1.75x

The router estimated prompt tokens as `chars / 4`. The simulator backend it talks to counts
tokens as whitespace-separated words. Measured against the full benchmark trace, those two
disagreed by a consistent **1.75x** (min 1.74, max 1.77 across 1500 requests — not a
distribution, a near-constant).

Because the admission gate compares `kv_projected` — built from this count — against
`σ · kv_capacity`, that inflation silently rescaled the ceiling:

| | Configured | Actually running |
|---|---|---|
| σ | 0.95 | **0.54** |
| Concurrency allowed per backend | ~37 | **20** |
| KV capacity left unused | — | **46%** |

Nothing crashed. Nothing errored. Every unit test passed, because every unit test constructed
its own `RequestFeatures` by hand and never exercised the estimator against a real prompt
string. The only visible symptom was `saturated_dispatches` sitting at 93% of all requests —
the gate finding nothing clean on almost every decision — which is a counter nobody was
required to look at, in a metrics endpoint nobody was polling mid-run.

This was caught by a direct instruction to verify rather than trust a passing build, followed
by comparing the router's token count against the trace generator's own ground-truth field for
all 1500 requests and finding the ratio dead constant at 1.75, not scattered — which is what
made it diagnosable as a systematic unit mismatch rather than noise.

**Fix:** `TokenCounter` in `router-core`, with two modes — `Whitespace` (exact for this
backend, and now the default) and `CharsPerToken` (the old heuristic, kept for a backend where
it has actually been calibrated). A test pins the exact 1.75x figure so this specific
regression cannot silently return.

**The more important fix** is a live audit, not a static default: `router_prompt_token_ratio`
compares the router's count against the backend's own reported `usage.prompt_tokens` on every
completed stream, and after 50 samples, if the running mean drifts more than 10% from 1.0, the
router logs a hard `tracing::error!` naming the actual effective σ. A static default can go
stale the moment the backend or its tokenizer changes; a live comparison against ground truth
cannot.

---

## 3. The backend doesn't volunteer ground truth — you have to ask

Wiring in the audit above required a source of truth for `prompt_tokens`. The obvious one is
the SSE stream's usage frame. Curling the simulator directly showed it never sends one:

```
data: {...,"usage":null,"choices":[{"delta":{"content":"words"}}]}
data: {...,"usage":null,"choices":[{"finish_reason":"length","delta":{}}]}
data: [DONE]
```

`usage` stays `null` end to end, and no frame ever has empty `choices` — which is exactly the
shape `sse::classify` requires to recognize a usage frame. Adding `stream_options:
{"include_usage": true}` to the request unlocks it:

```
data: {...,"usage":{"prompt_tokens":10,"completion_tokens":5,...},"choices":[]}
```

Neither the load generator nor the router set this flag. Which means `router_output_length_ratio`
— a metric that predates this phase — and the per-route length estimator's feedback loop had
never received a single real sample against the actual simulator, in any run to date, in the
whole project. Every test exercising them constructed a synthetic usage frame by hand. The gap
between "passes in a unit test" and "does anything against the real backend" was total, and
invisible until someone asked the metrics endpoint a direct question and got silence back.

**Fix:** `inbound::ensure_usage_requested` sets the flag when the client's own request is
silent on the question. An explicit client choice — including an explicit `false` — is left
alone. The one visible side effect for a client that asked for neither is one extra trailing
frame with empty `choices`, which is standard behavior under this flag across the ecosystem and
already handled by mainstream clients.

**This fix immediately broke every request.** Rewriting the body to add the flag grows it, and
`rebuild_request` was forwarding the client's original `Content-Length` verbatim. The backend
read exactly the old, shorter byte count and truncated the JSON mid-parse — surfacing as an
opaque `"unexpected end of JSON input"`, on every single one of 1500 requests. Traced by
capturing the raw bytes the router actually sent upstream with a throwaway TCP listener in
place of a real backend, confirming the header and the body length disagreed. Fixed by dropping
`Content-Length` from the forwarded headers and letting hyper compute it from the real body.

Neither of these two bugs would have been caught by the unit test suite, because the unit tests
never sent a request through the real relay to a real backend. They were caught only by
insisting on a live curl against the simulator before believing anything the metrics endpoint
reported.

---

## 4. Measuring the mechanism required sampling on a clock, not on traffic

The exit criterion has a mechanism half: `pressure` should spend less of the run at or above σ
occupancy than the baseline. `record_occupancy` originally fired only on dispatch — which means
a backend the policy correctly *stopped* choosing, because it was saturated, froze its gauge at
a stale value. That is exactly the backend the criterion is about, and it was invisible.

**Fix:** `sampler::sample_occupancy_loop` ticks every 100ms over every backend regardless of
traffic, accumulating `occupancy_ticks_total` and `ticks_at_ceiling_total` per backend. Their
ratio is the fraction the criterion needs. Verified live before trusting it: 31 ticks recorded
in 3.1 seconds against a 100ms interval, correct per-backend labels.

---

## 5. A benchmark script that measured one router twice

Running the comparison at a low arrival rate (8 req/s) produced two arms that read as an exact
tie. The instinct was to explain it as a floor effect — `p2c` outperforming the rate this trace
was calibrated for. The actual cause was structural: `router_requests_total` across the six
supposedly-independent runs read **1501, 3002, 4503, 6004, 7505, 9006** — strictly increasing,
never reset. A router process left over from unrelated debugging held the ports; every fresh
router this script started died on the (correctly) fatal admin-port bind added earlier in this
phase; and the readiness poll, which only checked that *something* answered on the traffic
port, was satisfied by the stale process and let the run proceed. All six "arms" dispatched
through one process running one policy.

**Fix, in two parts, the second one found only by running the hardened script and watching it
fail correctly:**

1. Kill any leftover router before building, abort if the ports won't clear, and — the
   check that actually matters — scrape `router_requests_total` immediately after the readiness
   poll and abort if it is already nonzero. A freshly started router has served exactly zero
   requests; any other value means the wrong process is being measured.

2. That freshness check immediately tripped on itself: the readiness poll's own probe —
   `curl http://.../v1/models` — is a real HTTP request that reaches the router's handler,
   fails JSON parsing on the empty body, and increments `router_requests_total{result="error"}`
   by exactly one. The fix that was supposed to catch a stale router was, on every run, one
   request away from falsely declaring a fresh one stale. Split the readiness check in two: the
   admin port (a real `curl`, but not a counted request) confirms the process is alive, and a
   bare TCP connect probe against the traffic port — a handshake with no bytes sent, so hyper
   never parses a request out of it — confirms it is bound. Verified live before trusting a
   multi-minute run on it: the probe connects, and the counter still reads zero afterward.

---

## 6. The result, and what's still open

With every bug above fixed, three repeats per arm, cold backend restarts, fixed seeds, at rate
10:

| | `p2c` | `pressure` | Criterion |
|---|---|---|---|
| Error rate | 2.0–5.9% (mean 4.30%) | 1.0–1.1% (mean 1.07%) | ≥3x lower — **4.0x, met** |
| Time at/above σ | 3.8–9.8% (mean 7.6%) | 4.2–4.7% (mean 4.5%) | lower — **met** |
| Requests served | mean 835 | mean 893 | ≥ — **met** |
| TTFT p99 | 71–74ms | 80–82ms (2 of 3 runs) | slightly worse, expected | **as predicted** |

This is the first run of the phase where the error-rate result and the mechanism result agree
with each other and with the design, instead of one of them being an artifact of a
mismeasurement. The full six-file `verify.py` sweep passed 8/8 checks with 0 failures.

**Open item, not yet root-caused.** The third `pressure` repeat showed a p99 of 817ms against
77-82ms on its own other two runs — traced to 23 requests clustered in a single ~1-second
window with a queueing-decay signature (1.84s down to 0.82s across consecutive request IDs), no
router-side error or warning logged around it, and no effect on the error-rate count since
every one of those requests eventually succeeded. It looks like transient burst contention,
either in the client's connection handling or in one of the four simulator processes, but it
was not chased to a root cause before closing this phase out. It should not be read as
disqualifying the result above — the primary criterion is error rate, and it was unaffected —
but a p99 spike an order of magnitude past what the design predicts deserves an explanation
before it's treated as settled, and that explanation is deferred rather than found.

---

## 7. What this does and does not establish

**Established.** The occupancy score, the admission gate, and the RAII lease all behave exactly
as specified, verified against live traffic through the real simulator rather than only against
hand-built test fixtures. The Phase 0 result reproduces in Rust: `pressure` beats a stronger
baseline than the one it was originally validated against, by a comparable margin, at higher
throughput.

**Not established.** Why one run out of three showed anomalous tail latency. Whether the result
holds at other arrival rates — only rate 10 was run to completion on the corrected pipeline.
Anything about the per-route output-length estimator's real-world accuracy: every request in
this trace carries an explicit `max_tokens`, which by design always wins over the learned
estimate, so the estimator was wired and functional but never actually exercised by this
benchmark.

**The methodological lesson, stated once more because it is the one worth carrying forward.**
Every bug in this phase shipped with a fully passing test suite. None of them were code bugs in
the sense of doing something other than what was written — the token counter counted `chars/4`
exactly as written, the readiness poll checked what it was told to check. They were bugs in
what the code was asked to measure, and the only thing that caught them was refusing to accept
a plausible-looking number without tracing it back to a live process and a raw byte capture.

---

## Files

| Path | Purpose |
|---|---|
| `crates/router-core/src/cost.rs` | Occupancy, `pressure_score`, constants |
| `crates/router-core/src/gate.rs` | Admission filter |
| `crates/router-core/src/strategy/pressure.rs` | The shipping strategy |
| `crates/router-core/src/lease.rs` | RAII KV accounting |
| `crates/router-core/src/tokens.rs` | Configurable, self-auditing token counting |
| `crates/router-proxy/src/sampler.rs` | Traffic-independent occupancy sampling |
| `crates/router-proxy/src/inbound.rs` | Feature extraction, usage-frame request rewriting |
| `bench/run_pressure_comparison.sh` | The comparison harness, hardened against a stale router |
| `bench/ceiling_stats.py` | Reads the mechanism number from the router's own counters |
