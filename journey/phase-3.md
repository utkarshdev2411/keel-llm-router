---
tags: [journey, phase-3, signal-plane, observability, real-hardware]
status: 3A complete, 3B run on real vLLM, sigma calibration still open
created: 2026-08-27
updated: 2026-08-30
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

Part B, added three days later, is the same lesson a third time, applied to me. The claim that
blocked it was mine, it was stated confidently, and it was wrong.

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

## 6. What Part A established

The signal plane collects, parses, and cross-checks correctly, on real traffic, with the
routing boundary held provably rather than just by convention. A capacity misconfiguration —
the same class of denominator error that cost the most time in Phase 2 — is now caught at
startup instead of discovered mid-benchmark. Losing a backend's metrics endpoint degrades
exactly one thing, the drift check, and degrades nothing else.

What it could not establish is whether the admission gate actually prevents engine preemption.
`llm-d-inference-sim` rejects requests under KV pressure rather than preempting them, and does
not expose a preemption metric at all — confirmed directly on the wire, not assumed from
documentation. That validation was scoped out as Phase 3B: a separate, hardware-gated runbook
rather than a task quietly skipped or faked against a backend structurally incapable of
answering the question.

**The throughline from Phase 2 repeats itself, worth stating once.** Every defect Part A found —
the panic, the unbounded timeout, the unnecessary dependency — shipped inside a fully passing
test suite. None of them were the code doing something other than what was written. They were
gaps between what the tests could see and what the binary actually did when started for real.
The fix was never a smarter test in isolation; it was running the thing.

---

# Part B — real vLLM on real hardware

Part B was written as a runbook for a GPU that did not exist yet. Doc 9 named an engine pin,
launch flags, the two-arm structure, and a pass condition, on the assumption that someone would
eventually rent a cloud instance and execute it unchanged. What actually happened is that the
GPU turned out to be sitting under the desk the whole time.

---

## 7. The hardware limitation that was not one

The plan was a cloud rental. It got as far as a marketplace listing, an instance
configuration, and a payment dialog before the obvious question landed: why not the laptop?

The laptop has a **GTX 1650, 4 GB, compute capability 7.5**. That clears vLLM's floor, barely.
The card is a TU117 die, so it has no tensor cores — vLLM logs
`Cannot use FA version 2 ... only supported on devices with compute capability >= 8` and falls
back to `TRITON_ATTN`. It also cannot do bf16 and casts to fp16. Generation is slow. For a test
about *memory pressure* rather than throughput, none of that matters, and the small VRAM is
actively useful: KV exhaustion is reachable with a handful of requests instead of needing to
hammer a 24 GB card.

One instance came up fine. Then, asked to run the two-arm comparison, I said the GPU could not
host a second vLLM instance and that Phase 3B was therefore blocked on hardware. I had the
numbers to justify it — 1666 MiB used, 2050 MiB free, and each instance needs its own CUDA
context plus ~930 MiB of weights.

I had not tried it.

Asked to verify that claim rather than assert it, I launched the second instance. It came up
and served. **3074 MiB used, 642 MiB free, both backends healthy.** The arithmetic had been
close enough to sound right and was simply wrong, and the only reason it got caught is that
somebody asked for the test instead of the reasoning.

That is the Phase 2 lesson again, pointed the other way. A green test suite is not a correct
binary; a confident estimate is not a measurement.

---

## 8. The single-backend null result was structural, and provable

Before the second instance existed, the one-backend run produced a result that looked like a
finding: `pressure` and `least_requests` returned **identical** preemption counts (3 and 3),
identical output-token totals (7789 and 7789), and identical error counts.

That is not a measurement artifact. It is forced by the code. From
`crates/router-core/src/strategy/pressure.rs`:

```rust
let fell_through = eligible.is_empty();
let pool: &[BackendId] = if fell_through { &snap.healthy } else { &eligible };
```

With exactly one healthy backend `b`, `gate::eligible` returns either `[b]` or `[]`. If it
admits, `pool = eligible = [b]`. If it does not, `fell_through` is true and
`pool = snap.healthy = [b]`. **Both branches produce the same single-element pool**, the argmin
over it is trivial, and `pick()` returns `Some(b)` no matter what the gate decided. The gate's
verdict reaches only `t.fell_through` and `t.gated_by` — trace fields, pure observability.

So with N=1 the gate cannot change a dispatch, by construction. The router's own metric agreed
loudly: `router_saturated_dispatches_total = 63` of 80 dispatches found zero eligible backends,
fell through, and dispatched anyway. The gate was firing constantly and was structurally
powerless, because there was nowhere else to send anything.

Worth recording because the null result was real, reproducible, and would have been very easy
to write up as "the gate does not work."

---

## 9. Three things that had to be made deterministic before measuring anything

**Capacity is not reproducible across restarts.** vLLM sizes `num_gpu_blocks` by profiling free
VRAM at startup. With byte-identical flags it produced **232 blocks on one boot and 1245 on the
next**, and a second instance launched alongside the first profiled 232 against the first's
1245 — a 5.4x asymmetry between two backends that were supposed to be identical. Capacity is
the *denominator* of every occupancy fraction, so drift between arms silently rescales σ and
makes the arms incomparable. This is the same class of error as the Phase 2 token-count bug,
arriving through a different door.

Fixed by pinning with `--num-gpu-blocks-override` in a new `phase0/restart_real.sh`, so both
backends report an identical 8192 tokens (512 blocks × 16) on every cold restart. The startup
capacity assertion built in Part A caught the mismatch the first time it happened, before any
traffic was sent — which is exactly what it was written for.

**The token counter had to be recalibrated, and doc 9's instructions were backwards.** The
benchmark trace is random six-character nonsense words. `llm-d-inference-sim` tokenizes on
whitespace, so `token_counter = "whitespace"` is exact against it. A real subword tokenizer
fragments `tmhuug` into several BPE tokens, so the router under-counts badly.

Doc 9 said to start from `chars_per_token = 4.0` and *divide* by the observed mean ratio.
Measured, the mean was **0.463**, and dividing would have moved the setting to 8.6 — the wrong
direction, widening the under-count. `record_prompt_token_ratio` computes
`estimated / reported`, and `chars_per_token` is the *divisor* in the estimate, so the
correction is a multiplication: `4.0 × 0.463 = 1.85`. One restart later the median ratio read
**1.008** and the drift warning stopped. Doc 9 has been corrected, with the measurement inline
so nobody has to re-derive it.

**Prefix caching was disabled.** Cached blocks persisting between runs are a carry-over
confound, the traces share no prefixes by construction, and prefix reuse is Phase 4's subject,
not this one.

---

## 10. Two more bugs, both found by running against real hardware

**The preemption metric was never being read.** `scrape.rs` searched for
`vllm:num_preemptions`. Real vLLM v0.26.0 exports `vllm:num_preemptions_total`. `metric_value`
anchors matches on `{` or whitespace to prevent substring collisions, so after stripping the
search string the next character is `_` and the match correctly fails — returning `None`.

`None` is indistinguishable from "the metric is absent," which is precisely what the simulator
does and precisely what Part A went out of its way to represent honestly. The parser would have
reported "we never looked" in exactly the same way it reports "the gate held." The same bug hit
`prefix_cache_hits` and `prefix_cache_queries`, both of which are also `_total`-suffixed on
real vLLM and bare on the simulator.

Fixed with a `counter_value` helper that tries the bare name and then `<base>_total`, both
through the existing anchored matcher, so the sibling `<base>_created` — whose value is a unix
timestamp, not a count — can never be picked up by accident. Both backends stay supported.

**`loadgen.py` scored failures as successes.** Fourteen of eighty requests returned
**HTTP 400** from vLLM. The router counted all fourteen as errors. The CSV recorded all
fourteen as *successes with zero tokens*, `error` column empty.

The cause is a single missing check. `loadgen.py` detects `{"error": ...}` objects inside SSE
data frames — the simulator's format, and a check whose own comment explains it was added after
in-stream errors made error rate read 0.0% at a true 60%. But it never inspects
`r.status_code`. Real vLLM signals context-length overflow with an HTTP status, and that body
contains no `data:` lines at all, so the loop falls straight through with `c = 0` and
`err = None`. The exact failure the existing check was written to prevent, arriving through the
other door. Fixed with a status check before the stream loop.

The 400s themselves were legitimate: with the newly calibrated ~3.8 real tokens per nonsense
word, the largest trace prompts exceeded `max_model_len = 2048` on their own. The trace was
resized and the errors went to zero — but only after the harness became capable of reporting
them at all.

---

## 11. Designing a workload that can actually preempt

The runbook says to find the arrival rate empirically rather than pick one and hope. Sweeping
rate 2, 3, 4 and 6 produced **`saturated = 40` of 80 at every single rate** — a constant across
a 3x range of arrival rate, which is not what load-dependent behaviour looks like.

Sampling the router's internals during a run explained it. On this GPU a request takes 60–100 s,
and 80 requests arrive within 40 s even at the slowest rate tested, so *every request is in
flight simultaneously regardless of rate*. Arrival rate was not the control variable; total
request count was. At that load both backends sat at **99.8% reported KV** with `kv_projected`
at 188% of capacity and `inflight` at 41 against a `max_num_seqs` of 32. Aggregate demand
exceeded aggregate capacity by roughly 60%, and since the gate never refuses (ADR-021), it
correctly marked both backends ineligible and dispatched anyway. No routing policy can prevent
preemption when there is nowhere with headroom to route to. The rate sweep had been measuring
the wrong axis.

The same sampling run confirmed something more important, and negatively: **`kv_projected`
returns to exactly 0 with `inflight` at 0 after every run.** The leak invariant the design
calls the single most expensive bug available in this project holds on real hardware.

Two further facts shaped the final design, both learned by measurement rather than assumption:

- **vLLM queues before it preempts.** Tightening capacity from 8192 to 7168 tokens *lowered*
  the reported KV fraction (0.908 → 0.868) instead of raising it, because the engine admits
  fewer requests and queues the rest. Reported KV plateaus around 0.9 and only crosses 1.0
  under genuine oversubscription. Preemption is not the first-line failure mode on real vLLM;
  queueing is.
- **Count-based imbalance shrinks as request count grows.** At n=32 the ungated arm's KV spread
  was up to 0.45; at n=40 it had washed out to 0.04. The effect `pressure` exploits is largest
  with fewer, highly-variable requests — so the workload needs high length variance
  (`--prompt-sigma 1.0`), not more of it.

---

## 12. The result

Two real vLLM backends with pinned identical capacity, `max_num_seqs = 32`, cold container
restart before every single cell, both arms on the same trace and the same seed, interleaved.
The two configs differ by exactly one line, verified with `diff`.

### The mechanism

At n=32 with capacity pinned to 8192 tokens (512 blocks), the clearest single contrast:

| Arm | inflight split | `kv_projected` split | backend-reported KV |
|---|---|---|---|
| Ungated `least_requests` | **16 / 16** — perfectly count-balanced | 5733 / **9072** (58% apart) | 0.493 / **0.908** |
| Gated `pressure` | **12 / 20** — deliberately count-*im*balanced | 7494 / 7311 (**2.5% apart**) | 0.751 / 0.654 |

`least_requests` splits request *count* perfectly evenly and gets a 58% KV imbalance for it.
`pressure` sends deliberately uneven counts in order to keep KV even. That is the entire thesis
of the project — rank by fraction of capacity consumed, not by request count — visible directly
in the numbers on real hardware.

Repeating that at n=32 with capacity tightened to 7168 tokens (448 blocks), across three seeds —
the hottest backend's reported KV, and the spread between the two backends:

| seed | ungated hot / spread | gated hot / spread |
|---|---|---|
| 11 | 0.868 / 0.110 | 0.823 / **0.020** |
| 12 | 0.841 / 0.051 | 0.823 / **0.020** |
| 13 | **0.971** / **0.450** | 0.823 / **0.020** |

`pressure` is not only flatter, it is *deterministic*: identical peaks and an identical 0.020
spread on every seed, while the ungated arm's imbalance is luck-of-the-draw and ranges over an
order of magnitude.

### Preemption

Back at 8192 tokens (512 blocks), sweeping total request count — the axis that turned out to
matter, rather than arrival rate:

| Load | Ungated `least_requests` | Gated `pressure` |
|---|---|---|
| n = 48 (1 seed) | 1 | **0** |
| **n = 56 (4 seeds)** | 2, 0, 3, 5 — **10 events, 3 of 4 runs** | **0, 0, 0, 0 — zero, every run** |
| n = 64 (3 seeds) | 4, 1, 3 — 8 events, 3 of 3 runs | 1, 1, 1 — 3 events |

At n=56 the gate eliminates preemption outright: ten events become zero across four independent
seeded runs. The ungated arm gets there by holding both backends at 32/32 requests and letting
one hit exactly **1.000** reported KV; the gated arm splits 24/32 and holds both at 0.973–0.984,
just under the ceiling.

At n=64 aggregate demand exceeds what any routing decision can fix, and the gate degrades
rather than breaking: still 8 events down to 3, a 62% reduction. That is the designed behaviour
for a policy that never refuses, and it brackets the working window from above.

---

## 13. What Part B establishes, and what it does not

**Established.** The routing mechanism does on real vLLM exactly what the design says it should,
reproducibly and deterministically: it trades request-count balance for KV balance, and the KV
balance is roughly ten times tighter than the count-balancing baseline achieves. There is a real
load window in which that difference eliminates engine preemption entirely, and above that
window the policy degrades gracefully instead of collapsing. The KV lease invariant holds on
real hardware. The signal plane built in Part A reads real vLLM correctly, and its startup
capacity assertion caught a genuine misconfiguration the first time one occurred.

**Not established, and stated plainly.**

- **The runbook's pass condition was not met at a single load point.** It asks for ungated > 0
  across all three repeats *and* gated exactly 0. At n=56 the gated arm is 0 across four runs
  but one ungated seed came in at 0; at n=64 the ungated arm preempts 3 of 3 but the gated arm
  is 1 each. The honest claim is a load *window*, not a point.
- **σ is still uncalibrated.** Every run above used σ = 0.95, the simulator-derived default.
  Doc 9 asks for a sweep of 0.85 / 0.90 / 0.95 / 0.98 precisely because 0.95 was inherited
  rather than measured, and that sweep has not been run. σ = 0.95 remains a starting point, not
  a result.
- **Round robin was never measured.** The ungated baseline was `least_requests`, chosen because
  the benchmark rules require configuring baselines to their strongest setting and round robin
  ignores backend load entirely. Beating `least_requests` ought to imply beating round robin,
  but that is an inference, not a number.
- **One backend pair, one GPU, one model, one trace shape.** `Qwen2.5-0.5B-Instruct` on a
  4 GB Turing card with no tensor cores. Nothing here speaks to scale.

**A refinement to the design's premise, worth carrying forward.** The docs treat preemption as
the natural failure mode under KV pressure. On real vLLM it is not the first one — the engine
queues rather than over-admitting, and preemption only appears once genuinely oversubscribed.
"Prevents preemption" is therefore a narrower claim than "prevents KV exhaustion symptoms," and
the claim wording should say so before anything is published.

---

## Files

| Path | Purpose |
|---|---|
| `crates/router-proxy/src/signal/scrape.rs` | Prometheus parsing, absent-vs-zero handling, `counter_value` for `_total`-suffixed counters |
| `crates/router-proxy/src/signal/drift.rs` | Projection-vs-reported cross-check |
| `crates/router-proxy/src/signal/mod.rs` | Per-backend collector loop, `fetch_metrics` |
| `crates/router-bin/src/main.rs` | Startup capacity assertion, collector spawn |
| `crates/router-proxy/tests/signal_integration.rs` | M1–M7, exercising the real production fetch path |
| `phase0/restart_real.sh` | Cold-restart two real vLLM backends with pinned, identical KV capacity |
| `phase0/probe.sh` | Run one cell with live sampling of router internals; reports preemption delta, peak KV, drain state |
| `phase0/sweep_arm.sh` | Run one arm across arrival rates, cold restart before every cell |
| `phase0/loadgen.py` | HTTP status check added — real vLLM signals failure by status, not only by SSE error frame |
| `router.toml` / `router_ungated.toml` | The two arms; differ by exactly one line |
