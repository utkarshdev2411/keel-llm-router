---
tags: [journey, phase-3, signal-plane, observability]
status: 3A complete, 3B blocked on hardware
created: 2026-08-27
updated: 2026-08-27
---

# Phase 3: The Signal Plane, and Verifying Someone Else's Green Build

Phase 2 put `pressure` and the admission gate on the request path, running entirely on the
router's own local accounting. Phase 3 adds the piece that watches that accounting from the
outside: a signal plane that scrapes each backend's own reported state and cross-checks the
router's projection against it. The rule going in was absolute and stayed absolute throughout:
scraped data may inform an alarm, and nothing else. It is never a gate input, never a scoring
input, never anything a routing decision depends on.

This phase also has a different shape than the first two. The initial implementation was
written by a different development agent (Kiro), working from a detailed execution plan rather
than open-ended instructions. The code that came back passed 132 tests and a clean clippy run.
Verifying it anyway — actually starting the binary, not just trusting the suite — found a
startup panic the entire test suite was structurally incapable of catching, plus a dependency
choice that quietly reintroduced an unbounded-timeout bug of exactly the shape Phase 2 had just
finished fixing elsewhere. The lesson from Phase 2 was "a green build is not the same claim as
a correct one." This phase is the same lesson, applied to code someone — something — else wrote.

---

## 1. What was built

| Component | Responsibility |
|---|---|
| `router_proxy::signal::scrape` | Prometheus text parsing: absent-metric handling, label-vs-value extraction, windowed counter deltas |
| `router_proxy::signal::drift` | `projection_drift()` — router-projected fraction ÷ backend-reported fraction, with idle and staleness guards |
| `router_proxy::signal` (collector loop) | One task per backend, ticking independently of traffic, storing into `Backend.reported` |
| Startup capacity assertion | Scrapes each backend once before accepting traffic; hard error on a `kv_tokens` mismatch, warn-and-proceed if the backend is merely unreachable |
| `router_prompt_token_ratio` (Phase 2) → `router_projection_drift` (this phase) | The same audit pattern — running mean, minimum sample count, warn-once — applied to KV usage instead of prompt-token count |

The one-task-per-backend structure matters for the same reason the occupancy sampler in Phase 2
does: a single loop iterating every backend means one slow or hanging backend silently
delays every other backend's reading, and the staleness that introduces is invisible.

---

## 2. The panic every test passed around

The first live start of the router after Kiro's implementation:

```
thread 'main' panicked at crates/router-proxy/src/signal/mod.rs:68:22:
there is no reactor running, must be called from the context of a Tokio 1.x runtime
```

`spawn_collectors` called bare `tokio::spawn` from inside `main()`, which owns its Tokio runtime
explicitly (`Runtime::new()`, not `#[tokio::main]`) and had never entered that runtime's context
before making the call. Every one of the seven signal integration tests is a `#[tokio::test]`,
which means every single one of them already runs inside a runtime — none of them could ever
have exercised the code path that broke. 132 passing tests, a clean `cargo clippy`, and the
binary could not start.

Fixed with a `runtime.enter()` guard around the spawn call, matching how the occupancy sampler
two lines above it was already being spawned. Confirmed live afterward: capacity assertion
passes on all four backends, four collectors spawn, router listens, no panic.

---

## 3. A dependency choice that reintroduced Phase 2's timeout bug in a new place

Separately from the panic, the implementation added `reqwest` with `rustls-tls` to scrape
plaintext `localhost` — a second HTTP stack and its own TLS chain (16 additional crates in the
dependency tree) for something the router already had a pooled client for. Two consequences,
neither hypothetical:

- **The dependency was unnecessary.** The router already builds a `PooledClient` on top of
  `hyper_util` for the request path.
- **The timeout was actually broken.** `tokio::time::timeout` wrapped only the request *send*.
  `response.text()` — reading the response body — ran outside it, unbounded. A backend that
  accepted a connection and then dripped its `/metrics` body slowly would hang the collector
  past the configured timeout. `.build().unwrap_or_default()` on the reqwest client also meant
  a builder failure would silently fall back to a default client with no configured timeout at
  all, rather than surfacing the failure.

This is the same shape of gap as the Content-Length bug from Phase 2 — a timeout that looks
like it bounds an operation but only bounds part of it.

Replaced with a `fetch_metrics` helper built on the existing pooled hyper client, wrapping
connect-through-body-read in one `tokio::time::timeout`. `reqwest` removed from both crates
entirely. Re-verified live rather than assumed equivalent: projection drift under a full
1500-request run read 0.997–1.006 both before and after the rewrite — identical, confirming the
replacement changed nothing observable except removing the dependency and closing the timeout
gap. The three tests that had called `reqwest` directly (bypassing `router_proxy::signal`
entirely) were rewired to call `fetch_metrics`, so they now exercise the real production path
instead of a parallel one that happened to look similar.

---

## 4. Verifying the parts that could pass silently

Two properties don't show up as a red test if they're wrong — they show up as nothing at all,
which is worse.

**Absent-not-zero.** The simulator does not expose `vllm:num_preemptions` at all. Confirmed on
the wire: `curl .../metrics | grep preemption` returns nothing, both from the simulator directly
and from the router's own exported metrics. A parser that defaulted a missing field to zero
would have made this indistinguishable from "definitely zero preemptions," which is a
meaningfully different and much stronger claim than "this field was never reported."

**Traffic-independent collection.** The collectors tick on a clock, not on requests. Confirmed
by reading the metrics endpoint with zero traffic sent: `router_signal_scrape_total` was already
climbing, `router_backend_reported_kv_usage` and friends were already populated.

**Drift under real load.** Full 1500-request run: **0.997–1.006** across all four backends,
comfortably inside the "few percent" tolerance the design calls for. No spurious drift alarm.

**The routing boundary held.** Checked directly rather than assumed: `pressure.rs` is the only
file in `router-core` that calls `gate::eligible` or `gate::admits`, and it does so only to
populate the decision trace, never to compute a score. `git diff` across every commit in this
phase touches zero lines in `gate.rs`, `cost.rs`, or anything under `strategy/`. The rule that
scraped data is a cross-check, never an input, was not just stated — it was verifiable.

---

## 5. The degradation test

The actual requirement (from the design notes, restated plainly): a backend can serve traffic
perfectly with a broken metrics endpoint, and losing `/metrics` must lose only the drift check,
nothing else. The clean way to test this outright is to kill a backend entirely — a strictly
harder case than "just its metrics endpoint died," since `llm-d-inference-sim` serves both the
chat API and `/metrics` on the same port and process.

Killed one of four simulator containers outright mid-run, then sent 1500 requests through the
router with three of four backends alive:

| Check | Result |
|---|---|
| Requests dispatched | 1500/1500, 0 lag events |
| Error rate | 0.0%, p99 94ms |
| Killed backend's scrapes | `10 ok` → `180 error` after the kill; the other three stayed `190 ok`, zero errors |
| Panic or signal-driven ejection | None. Ejection is driven by the existing health/passive-ejection path on real dispatch failures, never by the signal collector |
| Surviving backends' projection drift | 0.998–1.018 — identical to the healthy baseline, completely unaffected by the dead backend |
| Dead backend in the drift metric | **Absent**, correctly. A stale reading returns `None` rather than fabricating a comparison |

One real, minor finding surfaced by this test rather than by code review: `router_signal_age_seconds`
is only updated on a successful scrape, so for a backend whose scrapes have started failing it
freezes at whatever small age it last recorded instead of growing to reflect genuine staleness.
The underlying stored reading is not corrupted — `router_signal_scrape_total{result="error"}`
is the correct signal and it behaves exactly right — but a dashboard showing only the age gauge
would display a falsely-fresh number for a backend that has been dead for minutes. Worth fixing
by recomputing age on every tick regardless of scrape outcome; not a blocker, since nothing
downstream currently reads the age gauge as a health signal on its own.

---

## 6. What this does and does not establish

**Established.** The signal plane collects, parses, and cross-checks correctly, on real
traffic, with the routing boundary held provably rather than just by convention. A capacity
misconfiguration — the same class of denominator error that cost the most time in Phase 2 — is
now caught at startup instead of discovered mid-benchmark. Losing a backend's metrics endpoint
degrades exactly one thing, the drift check, and degrades nothing else.

**Not established, and not attemptable here.** Whether the admission gate actually prevents
engine preemption. `llm-d-inference-sim` rejects requests under KV pressure rather than
preempting them, and does not expose a preemption metric at all — confirmed directly on the
wire, not assumed from documentation. That validation needs real vLLM on a GPU and is
deliberately scoped out as Phase 3B: a separate, hardware-gated runbook rather than a task
quietly skipped or faked against a backend structurally incapable of answering the question.
Until it runs, σ = 0.95 remains a simulator-derived starting point.

**The throughline from Phase 2 repeats itself, worth stating once more.** Every defect this
phase found — the panic, the unbounded timeout, the unnecessary dependency — shipped inside a
fully passing test suite. None of them were the code doing something other than what was
written. They were gaps between what the tests could see and what the binary actually did when
started for real. The fix was never a smarter test in isolation; it was running the thing.

---

## Files

| Path | Purpose |
|---|---|
| `crates/router-proxy/src/signal/scrape.rs` | Prometheus parsing, absent-vs-zero handling |
| `crates/router-proxy/src/signal/drift.rs` | Projection-vs-reported cross-check |
| `crates/router-proxy/src/signal/mod.rs` | Per-backend collector loop, `fetch_metrics` |
| `crates/router-bin/src/main.rs` | Startup capacity assertion, collector spawn |
| `crates/router-proxy/tests/signal_integration.rs` | M1–M7, now exercising the real production fetch path |
