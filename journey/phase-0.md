---
tags: [journey, phase-0, validation]
status: stage-0a-complete
created: 2026-08-09
updated: 2026-08-15
---

# Phase 0: Proving the Problem Exists

Before writing a single line of the router, Phase 0 answers one question: does the problem
we are trying to solve actually occur, and can we measure it. If a standard load balancer
already spreads LLM traffic evenly, there is nothing worth building, and it is far cheaper
to discover that in two weeks than in two months.

This document records how Phase 0 was set up, what was measured, what went wrong, and what
came out of it. Stage 0a, the simulator half, is complete. Stage 0b against real hardware
is not.

The short version of the result: a cost aware routing policy called `pressure` reduces KV
exhaustion errors by three to seven times against least connections, and by roughly five
times against SGLang's production Rust router, on identical traffic. Getting to a number
worth reporting took three significant corrections along the way, and those corrections are
documented here rather than quietly fixed, because two of them changed what the project
believes about its own thesis.

---

## 1. Why a simulator and not a real GPU

The obvious way to test this is to run several copies of vLLM behind a load balancer and
watch what happens. That requires a GPU with enough memory to hold multiple model replicas,
which the development machine (a 4 GB GTX 1650) cannot do. Renting one costs money and
time.

There is a cheaper option. The llm-d project ships a simulator called
`llm-d-inference-sim`, a program that speaks the same HTTP API as vLLM, streams responses
the same way, exposes the same Prometheus metrics, and enforces a configurable KV cache
limit. It runs on CPU. From the router's point of view it is indistinguishable from a real
backend.

That makes it ideal for Phase 0. The router code written against the simulator is the same
code that will later run against real vLLM, so nothing is thrown away. What the simulator
cannot do is reproduce real GPU physics, so any result from it has to be confirmed against
real hardware later. Phase 0 is therefore split into two stages: stage 0a on the simulator,
stage 0b on a rented GPU.

Everything below is stage 0a.

---

## 2. Setting up the backends

Four simulator instances stand in for four LLM servers, each on its own port.

```bash
docker run -d --name sim1 -p 8001:8000 -e POD_IP=127.0.0.1 \
  ghcr.io/llm-d/llm-d-inference-sim:v0.10.2 \
  --model test-model --mode echo --max-model-len 8192 \
  --enable-kvcache --kv-cache-size 512 --block-size 16 --max-num-seqs 64 \
  --time-to-first-token 50ms --inter-token-latency 20ms --time-factor-under-load 2.5
```

The same command repeats for `sim2` through `sim4` on ports 8002, 8003 and 8004. A helper
script, `restart_sims.sh`, brings all four up cold and polls each one until it answers,
rather than sleeping a fixed interval and hoping.

Paste each command as a single line. Multi line pastes with backslash continuations get
broken by terminal wrapping, and the container silently starts with only some of the flags
applied.

### What each flag does and why it was chosen

`-e POD_IP=127.0.0.1` is required whenever `--enable-kvcache` is set. The simulator was
built for Kubernetes, where every pod has its own IP, and it refuses to start without one.
In plain Docker you supply it manually.

`--model test-model` is mandatory. Without it the simulator exits immediately with
`model parameter is empty`. Since `test-model` is not a real HuggingFace model, the
simulator falls back to a built in dummy tokenizer, which is fine because we care about
timing rather than text.

`--mode echo` makes the simulator return the prompt back as the response. This is what
gives control over response length: the length of the reply is determined by the length of
the prompt sent.

`--max-model-len 8192` sets the context window. The default of 1024 is too small: a prompt
of 2000 tokens exceeds it on its own and the request fails before anything interesting
happens.

`--enable-kvcache` with `--kv-cache-size 512` and `--block-size 16` gives each backend 8192
tokens of KV memory. Without `--enable-kvcache` the limit is never enforced and the entire
experiment measures nothing.

`--max-num-seqs 64` caps concurrency. This value matters more than it looks and is
discussed in section 7.

`--time-factor-under-load 2.5` makes the backend slow down as it fills. At the default of
1.0 load has no effect on latency at all, which makes most experiments meaningless.

---

## 3. Designing the workload

The workload is the heart of the test. It has to reproduce the situation the router is
meant to handle: many requests arriving continuously, with wildly different sizes.

### Lengths follow a lognormal distribution

Real chat traffic is mostly short exchanges with an occasional very long one. A lognormal
distribution produces exactly that shape. The generator draws each length independently, so
the sequence is unpredictable, but small values are far more likely than large ones.

A typical sample of 1500 requests looks like this:

| statistic | tokens |
|---|---|
| median | 161 |
| 90th percentile | 607 |
| 99th percentile | 1461 |
| maximum | 2930 |
| mean | 267 |

Half the requests are around 160 tokens, one in ten is over 600, one in a hundred is over
1400, and the largest is eighteen times the median. That spread is what a request counting
load balancer is blind to.

### Prompts are built from a unique vocabulary

This detail matters more than it looks. The first version of the generator built every
prompt by repeating the word "word". Every prompt was therefore a prefix of every longer
prompt, the backend's block level cache deduplicated them almost entirely, and twenty
concurrent requests consumed roughly the memory of one. The KV limit never filled.

The effect was measured directly. Twenty concurrent requests of 600 tokens each, against a
capacity of 8192 tokens:

| prompt style | result |
|---|---|
| every prompt identical | 20 of 20 succeeded |
| unique prompts | 13 of 20 succeeded, 7 rejected for KV capacity |

Same demand, same capacity, opposite outcome. The generator now builds prompts by sampling
from a vocabulary of 20,000 distinct random words, so each request occupies its own cache
blocks.

A `--shared-prefix-frac` option exists to reintroduce controlled sharing later, when Phase
4 tests prefix affinity. For Phase 0 it stays at zero, because the goal here is to make the
memory limit bind.

### Echo mode forces output length to equal prompt length

This was originally misunderstood, and the correction is documented in section 7. The
measured behaviour is that `--mode echo` replays the whole prompt and **ignores `max_tokens`
entirely**. A 500 word prompt returns exactly 500 completion tokens whether `max_tokens` is
50, 200 or 5000.

There is therefore exactly one length per request, not two, and the generator draws one. The
consequence worth stating plainly is that prompt and output cannot be decoupled on this
harness at all. Testing a long prompt with a short response, which is the retrieval
augmented pattern, requires real vLLM in stage 0b.

### Generating traces

```bash
python generate_trace.py --kind lognormal --num-requests 1500 --rate 8 --out tr_v2_r8.json
```

`--rate` is the average number of new requests arriving per second. Arrival times are drawn
from a Poisson process, so gaps are irregular in the way real traffic is irregular. The
whole trace is written to a file before any run starts, so every routing policy is tested
against byte identical traffic.

A `uniform` mode also exists, where every request is the same size. It serves as a negative
control: on that workload every policy should tie, because there is no cost variation to
exploit. If a cost aware policy wins there, something is wrong.

---

## 4. The load generator

The generator fires requests on a fixed schedule regardless of whether earlier responses
have come back. This is called open loop generation and it is not optional.

A closed loop generator, which waits for responses before sending more, slows down when the
system slows down. Offered load drops exactly when the system is struggling, the queue
never builds, and overload is never actually measured. This is the single most common
reason LLM benchmark numbers fail to reproduce. Latency is measured from the scheduled
arrival time rather than from when the request was actually sent, so any delay caused by
the generator itself counts against the measurement rather than being hidden.

The generator reports a `lag_events` counter. If it climbs, the generator could not keep
its own schedule and that run is invalid. Across every run reported in this document,
`lag_events` was zero.

```bash
python loadgen.py --trace tr_v2_r8.json \
  --backends http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004 \
  --out results/r8_least_conn.csv --policy least_conn --max-num-seqs 64 --seed 1
```

### Detecting failures correctly

The simulator reports KV exhaustion differently depending on the mode. A non streaming
request gets HTTP 500. A streaming request gets HTTP 200 with an error object embedded
inside the response stream:

```
data: {"error":{"message":"the kv cache does not have sufficient capacity to store this request", ...}}
data: [DONE]
```

Since the router always streams, and the original parser only looked for a `choices` field,
these frames were skipped silently. Failed requests were recorded as successful responses
that happened to deliver zero tokens. Error rate read 0.0 percent while 60 percent of
requests at rate 24 were actually being rejected.

The parser now checks for an `error` key on every frame. This single fix changed the
measured error rate at rate 24 from 0.0 percent to 60.0 percent, and every comparison run
made before it was invalid.

### Keeping measurement overhead out of the results

Printing a line for every completed request seems harmless but is not. `print` is a
blocking system call on a single threaded event loop. While it runs, no response stream is
being read, and the first token timer of every request still in flight keeps ticking. Per
request logging therefore inflates the exact metric being measured.

Logging is off by default and enabled with `--verbose`. Errors always print. A status line
every five seconds shows progress and per backend occupancy without meaningful cost.

---

## 5. The routing policies under test

Three policies are compared, all against identical traffic.

**least_conn** sends each request to whichever backend currently has the fewest requests in
flight, breaking ties at random. This is what nginx, HAProxy and Envoy do and it is the
baseline being challenged.

**kvts** ranks backends by total committed work, measured in KV token seconds, which
accounts for both how much memory a request holds and how long it holds it. This was the
project's original algorithm. It was implemented, tested, and refuted. Section 7 explains
why, and it remains in the code as an ablation arm.

**pressure** is the corrected policy. Rather than comparing absolute work, it measures how
close each backend is to its limits as a fraction:

```
u = max( in_flight / max_num_seqs , kv_projected / kv_capacity )
```

Whichever limit is nearer dominates. Below a threshold the score is simply that fraction.
Above it, a quadratic penalty engages, because memory exhaustion fails suddenly rather than
gradually. A hard admission gate refuses any backend whose projected memory would exceed a
safety ceiling, and when no backend passes the gate the policy falls through to the least
bad option rather than dropping the request.

This design comes directly from the research: per sequence cost is close to zero while a
backend has spare capacity, so the score should be nearly flat there, and the failure at the
top is super linear, so the penalty should be quadratic. Taking the maximum over normalised
resources is the same pattern as Dominant Resource Fairness, which is worth knowing about
before claiming novelty.

---

## 6. Measurement discipline

Four rules make the numbers trustworthy.

**Warmup and drain are discarded.** The first requests of a run hit empty backends and the
last ones run with no competition. Neither is steady state. The comparison tool keeps the
middle 60 percent of the arrival window, filtered by scheduled arrival time.

**Sample count has to support the statistic.** With 200 requests, the 99th percentile is
effectively the second worst request and moves wildly between runs. Runs use 1500 requests.

**Every cell is run three times.** A single run cannot distinguish a real difference from
noise. This was learned the hard way: an early rate sweep produced a result where a higher
arrival rate showed fewer errors than a lower one, which is not physically sensible and was
simply variance. Tie breaking is seeded so runs are reproducible, and containers are
restarted cold before every single arm so no run inherits the previous one's warm cache.

**Error rate is read before latency.** Rejected requests never receive a first token, so
they contribute nothing to latency percentiles. A policy that rejects half its traffic
would look extremely fast. In this simulator the backend sheds rather than queues, so
latency stays nearly flat regardless of load, and error rate is the only meaningful
discriminator.

### Asserting the measurements rather than trusting them

A run that has lost a third of its requests prints the same reassuring `Done.` as a clean
one. After being caught out twice by results that were confidently wrong, the harness gained
a checker, `verify.py`, that asserts ten invariants directly and exits non zero if any fail.

| check | asserts |
|---|---|
| request conservation | request ids are contiguous with no duplicates, so nothing was silently dropped |
| silent failures | no row is a success with zero tokens |
| error taxonomy | the errors really are KV exhaustion and not connection failures wearing the same label |
| error latency | errors returned instantly, as a capacity rejection does, rather than being timeouts |
| backend coverage | all four backends actually received traffic |
| length invariant | output equals prompt, so the backend still behaves as the memory model assumes |
| trim sensitivity | the warmup trim is not what creates the result |
| KV accounting leak | zero requests in flight implies zero projected memory held |
| coordinated omission | the generator kept its own arrival schedule |

The last one deserves emphasis. If the router thinks memory is held that is not, it will
under admit for the remainder of the run and every number after that point is wrong. The
check is cheap and it has never failed since being added, which is exactly the point.

The final competitor benchmark passed all ten with zero warnings.

---

## 7. Three corrections that changed the results

Most of Phase 0 was not writing the algorithm. It was discovering that the measurements
were wrong, three separate times. Each is recorded here because each changed a conclusion,
and because a reader deciding whether to trust the final numbers should know what was
caught and how.

### Correction 1: the original algorithm was wrong, not just untuned

`kvts` ranked backends by committed KV token seconds. The formula is quadratic in output
length, which means it values one long request at roughly 256 times one short request. A
backend holding a single long generation is therefore avoided almost entirely even when
most of its capacity is free. Short requests pile onto whichever backends happen to look
clean, and those backends exhaust their memory instead.

It manufactures scarcity by over avoiding. Measured, it performed **worse than plain least
connections at every arrival rate tested**, in both the original run and the corrected rerun
months apart.

The pivot was from ranking by absolute work to ranking by fractional occupancy. Work is
unbounded and incomparable across heterogeneous backends. Occupancy is dimensionless,
bounded at 1.0, and directly expresses the thing that actually causes failure. That change
is the difference between `kvts` and `pressure`, and it is the single most important design
decision in the project.

This is kept as a negative result rather than deleted. It explains why the final policy is
shaped the way it is.

### Correction 2: the memory model did not match the backend

Partway through, a review found that the router's projection of KV usage disagreed with the
backend's own reported usage by a factor of two. The proposed explanation was that echo mode
ignores `max_tokens` and echoes the whole prompt, so a request must hold prompt plus
generated output, roughly twice the prompt.

That explanation was half right and the fix made things worse. The first half is true and
verified. The inference was not. A dedicated diagnostic, `kv_curve.py`, sent one request to
an otherwise idle backend and sampled its metrics across the whole request lifetime:

| elapsed | kv_cache_usage_perc | tokens generated |
|---|---|---|
| 0.0 s | 0.0000 | 0 |
| 1.0 s | 0.1211 | 47 |
| 10.0 s | 0.1211 | 490 |
| 20.1 s | 0.1211 | 983 |
| 21.1 s | 0.0000 | 1000 |

Memory usage is **flat for the entire twenty seconds**. And `0.1211 x 8192 = 992` tokens,
which is 62 blocks of 16, which is the prompt alone. The simulator allocates the prompt's
blocks at admission, holds them constant, and frees them at completion. Generated tokens
cost it nothing.

So the original projection had been right, and the fix had introduced a two times error in
the opposite direction. The resolution was an explicit flag, `--kv-model`, with
`prompt_only` matching this simulator and `prompt_plus_output` matching real vLLM, where
every generated token genuinely does append to the cache.

Two lessons worth carrying forward. First, measure the backend rather than reasoning about
it, because a plausible mechanism can be confidently wrong in either direction. Second, the
harness README had already stated the correct behaviour before the incorrect fix was
applied. Documentation that nobody re-reads is not a safeguard, which is why `verify.py`
now asserts these properties mechanically.

### Correction 3: capacity planning from the mean is wrong by three times

With the memory model settled, the expected capacity was 8192 tokens divided by the mean
request size of about 250 tokens, giving roughly 32 concurrent requests per backend.
Measured, backends began rejecting at **13 to 15**.

The mean is the wrong statistic. A request holds its memory for a time proportional to its
length, so long requests linger, and the population of requests occupying a backend at any
instant is biased toward the long ones. A 2000 token request is present roughly fifteen
times longer than a 130 token one, so it is roughly fifteen times more likely to be there
when you look. This is the inspection paradox.

The quantity that matters is `E[L²]/E[L]`, not `E[L]`. For this distribution those are about
736 tokens and 220 tokens, a factor of 3.3. Saturation is therefore

```
lambda_sat = B * KV_capacity / (ITL * F * E[L²])
```

The practical consequence generalises well beyond this project: **capacity planning from
mean request size overestimates by three times or more on heavy tailed LLM traffic.** Every
arrival rate sweep before this correction was run at the wrong operating point.

---

## 8. What was found

With the corrections applied, sweeping `least_conn` across arrival rates locates the knee.

| rate | error rate | TTFT p99 |
|---|---|---|
| 4 | 0.0% | 61 ms |
| 6 | 0.4% | 65 ms |
| 8 | 3.2% | 69 ms |
| 10 | 6.7% | 72 ms |
| 12 | 10.6% | 74 ms |
| 14 | 6.2% | 78 ms |

Latency barely moves while the failure rate climbs. The system does not get slower under
pressure, it starts refusing work. Rate 8 is the knee: the first rate where errors are
material without the system being collapsed.

The rate 14 figure being lower than rate 12 is exactly the kind of single run artifact that
motivated running three repeats of everything afterwards.

### The mechanism, visible in one line of log output

A status line from `least_conn` at rate 12 states the problem better than any table:

```
inflight [8001=14   8002=15   8003=14   8004=15  ]
occupancy[8001=0.99 8002=0.52 8003=0.99 8004=0.98]
```

Request counts are balanced to within one. Actual memory occupancy ranges from 52 percent to
99 percent. Three backends are at the edge of rejecting work while the fourth sits half
empty, and the load balancer believes it is doing an excellent job, because by its own
metric it is.

That is the entire argument for this project in two lines.

---

## 9. Does the algorithm fix it

Three policies, four arrival rates, three repeats each, byte identical traces, cold
containers before every run, seeded tie breaking.

### Error rate

| rate | least_conn | kvts | pressure | improvement |
|---|---|---|---|---|
| 8 | 2.0% | 9.7% | **0.3%** | 6.7x fewer errors |
| 10 | 3.5% | 14.8% | **0.9%** | 3.9x |
| 12 | 6.1% | 22.1% | **1.9%** | 3.2x |
| 14 | 10.8% | 26.3% | **3.3%** | 3.3x |

The advantage does not decay under stress. It settles at three to four times while the
absolute gap widens, from 1.7 percentage points at rate 8 to 7.5 at rate 14. `pressure` also
serves more requests: 909 of 912 against 893 at rate 8.

`kvts` is worse than the baseline at every rate, which independently confirms the refutation
described in section 7 on fully corrected code.

The variance is worth noting. Across three runs at rate 8, `pressure` returned 0.3, 0.3 and
0.3 percent. `least_conn` returned 1.0, 2.7 and 2.4. The cost aware policy is not merely
better on average, it is substantially more predictable, which for a production system is
worth as much as the mean.

### The mechanism was confirmed, not just the outcome

A win with no explanation is a win that cannot be trusted. The router's occupancy estimate
was independently validated against each backend's own reported memory usage before being
used as evidence, and the two track within sampling noise.

| rate | policy | instantaneous spread | time at or above 0.95 |
|---|---|---|---|
| 8 | least_conn | 91.1% | 15.9% |
| 8 | **pressure** | **56.8%** | **8.8%** |
| 10 | least_conn | 96.9% | 30.7% |
| 10 | **pressure** | **53.0%** | **14.4%** |
| 12 | least_conn | 71.5% | 35.4% |
| 12 | **pressure** | **47.9%** | **14.9%** |
| 14 | least_conn | 85.9% | 42.0% |
| 14 | **pressure** | **54.7%** | **23.8%** |

At rates 12 and 14, `least_conn` leaves some backend sitting at or above the admission
ceiling for 35 to 42 percent of the run, which is to say roughly two seconds in every five
it is one arrival away from rejecting work. `pressure` cuts that to 15 to 24 percent.

Worth being precise about how it wins: the fullest backend reaches almost the same peak
under both policies, 0.78 against 0.75 at rate 12. `pressure` does not lower the ceiling. It
spends much less time against it.

### It is not winning by shedding load

This was the most important thing to rule out. A policy that refuses requests rather than
distributing them better would show a lower error rate for the wrong reason.

Two checks rule it out. The arithmetic first: successes plus errors equals the request count
in every run, and `verify.py` asserts it. `pressure` did not serve fewer requests more
successfully, it served the same requests with fewer failures, and in fact completed more of
them.

Second, the code path. The admission gate never actually refuses. When no backend passes it,
the policy dispatches to the least bad option anyway. The `saturated_dispatches` counter
records how often the gate found nothing clean, not how often a request was dropped.

### One tradeoff to record honestly

`pressure` has slightly worse tail latency, 79 ms against 68 ms at p99 at rate 8. This is
expected and is the correct direction. Keeping more requests alive means more work in flight
at any moment. A request that completes in 79 ms is better than one rejected at 68 ms.

---

## 10. Against a real competitor

Beating nginx style least connections is necessary but not sufficient. The interesting
question is whether it beats a router built specifically for LLM serving.

`sgl-router` is SGLang's production router, written in Rust, and it sits in front of the
same four backends while the load generator drives it as an external proxy. It makes the
routing decisions; the harness only measures the outcome. Rate 10, three repeats per arm,
cold backends and a fresh router process before every single run.

| router | policy | mean error rate |
|---|---|---|
| **Keel** | **pressure** | **1.03%** |
| Keel | least_conn | 4.60% |
| sgl-router | cache_aware | 4.97% |
| sgl-router | power_of_two | 5.10% |

`pressure` is roughly 4.8 times better than `cache_aware`, sgl-router's flagship policy.

The ranges do not overlap. `pressure` spans 0.9 to 1.2 percent across its runs, while the
best single run any competitor produced was 2.3 percent. Combined with the rate 10 repeats
from the previous section, that is six runs against six with no overlap at all, which is a
clean result that does not depend on assuming anything about the shape of the distribution.

Also notable: sgl-router's two policies are not better than plain least connections here,
4.97 and 5.10 against 4.60. None of the three models KV memory, so none of them can see the
constraint that is actually binding.

Three caveats belong with these numbers and should travel with them anywhere they are
quoted. The trace has zero prefix sharing, which is the regime needed to make the memory
limit bind, so `cache_aware`'s actual mechanism has nothing to exploit. This is a fair test
of KV discipline and not a fair test of cache aware routing; Phase 4 tests that properly.
The sgl-router arms carry an extra network hop that has nothing to do with routing quality,
so only error rate is comparable and not latency. And this is a simulator, not real vLLM.

---

## 11. The finding that was not expected

The competitor run produced the most interesting number in the project, and it contradicts
what this document originally set out to prove.

Because the load generator only sees the external router's URL in proxy mode, per backend
distribution had to be recovered separately, by diffing each backend's Prometheus counters
before and after each run. Doing that gives the real answer to how evenly each router spread
its load:

| router / policy | cumulative KV spread | error rate |
|---|---|---|
| sgl-router cache_aware | **5.6%**, nearly perfect | 4.97% |
| sgl-router power_of_two | 12.0% | 5.10% |
| Keel pressure | **~47%**, very uneven | **1.03%** |

`cache_aware` distributes total memory load about eight times more evenly than `pressure`
does, and still fails nearly five times as often. Across these arms, cumulative balance is
not a weak predictor of failure. It is inversely related to it.

The resolution is that these measure different things. Cumulative spread asks whether each
backend did a fair share of the total work over the run. Rejections are caused by
instantaneous occupancy crossing a ceiling at a particular moment. A policy can distribute
totals perfectly while still letting one backend spike, and a policy can distribute totals
very unevenly while never letting any backend spike, which is precisely what `pressure` does
on purpose: it routes to whichever backend has headroom right now, and accepts lopsided
totals as the price.

Stated generally:

> Spreading total work evenly across backends is not the same as preventing any backend from
> overflowing, and optimising the former can leave the latter completely untouched.

This is a sharper claim than the project began with, and a more useful one. It also explains
a confusing result from earlier in the same day, where a newly added cumulative balance
metric appeared to show the algorithm making things worse. The metric was not broken. It was
faithfully measuring the thing that does not matter.

---

## 12. Where stage 0a leaves things

The problem is confirmed, its mechanism is measured, the proposed fix is validated against
that mechanism, and it beats both a conventional baseline and a real LLM aware competitor by
a margin whose run to run ranges do not overlap.

What the algorithm does is mitigate rather than eliminate. At rate 14 roughly one request in
thirty still fails. The router makes the cliff arrive later and less steeply, it does not
remove it. Past a certain load the answer is more capacity, and no routing policy changes
that.

Four gaps remain between this and a claim that would survive full public scrutiny.

**The simulator does not model preemption.** When memory runs out it rejects the request.
Real vLLM evicts a running sequence and recomputes it. Same trigger, different consequence,
and the admission gate exists specifically to prevent an event this harness cannot produce.
This is the largest gap and the first thing an informed reader will ask about.

**The simulator does not grow memory during generation.** Real vLLM appends to the cache for
every token produced. `--kv-model prompt_plus_output` exists for exactly this and has never
been exercised against a real backend. It also means the original thesis, that unpredictable
output length is the hidden cost, is not testable here. What was validated is the weaker and
still useful claim that cost varies enormously and is invisible to a request counting router.

**The workload is a single shape.** Lognormal lengths, no prefix sharing, four homogeneous
backends. Real traffic contains shared system prompts, which changes memory behaviour
substantially because blocks get deduplicated.

**Tail percentiles rest on few samples.** With about 900 requests kept per run, the 99th
percentile is a handful of requests. The error rate result is solid because it is a
proportion over the whole run; the latency figures should be read as directional.

Stage 0b closes the largest of these. It repeats these same traces against real vLLM on a
rented GPU with `--kv-model prompt_plus_output` enabled, which costs an afternoon and a few
dollars. Phase 1 and the Rust implementation begin after that.

---

## Files

| file | purpose |
|---|---|
| `generate_trace.py` | builds fixed traffic traces from a lognormal distribution |
| `loadgen.py` | open loop generator, routing policies, KV accounting |
| `compare.py` | side by side comparison with warmup and drain trimming |
| `occupancy_stats.py` | instantaneous occupancy from a run log, the mechanism metric |
| `proxy_spread.py` | real per backend distribution for external router runs |
| `verify.py` | asserts ten measurement invariants on a results directory |
| `kv_curve.py` | single request memory against time, for checking what a backend charges |
| `scrape_backend_counts.py` | snapshots per backend counters from Prometheus |
| `restart_sims.sh` | brings all four backends up cold and waits for readiness |
| `stage3_knee.sh` | sweeps arrival rate to locate the failure threshold |
| `stage5_compare.sh` | runs all policies at all rates with repeats |
| `stage6_competitors.sh` | benchmarks against sgl-router |
