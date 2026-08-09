---
tags: [journey, phase-0, validation]
status: in-progress
created: 2026-08-09
---

# Phase 0: Proving the Problem Exists

Before writing a single line of the router, Phase 0 answers one question: does the problem
we are trying to solve actually occur, and can we measure it. If a standard load balancer
already spreads LLM traffic evenly, there is nothing worth building, and it is far cheaper
to discover that in two days than in two months.

This document records how Phase 0 was set up, what was measured, and where things currently
stand.

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
  --port 8000 --model test-model --mode echo --max-model-len 8192 \
  --enable-kvcache --kv-cache-size 512 --block-size 16 --max-num-seqs 32 \
  --time-to-first-token 50ms --inter-token-latency 20ms --time-factor-under-load 2.5
```

The same command repeats for `sim2` through `sim4` on ports 8002, 8003 and 8004.

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
the prompt sent. In the alternative `random` mode the simulator returns short canned
sentences and `max_tokens` cannot force a long response, which makes it impossible to
generate the long generations the test depends on.

`--max-model-len 8192` sets the context window. The default of 1024 is too small: a prompt
of 2000 tokens exceeds it on its own and the request fails before anything interesting
happens.

`--enable-kvcache --kv-cache-size 512 --block-size 16` turns on KV cache accounting and
sets its size. 512 blocks of 16 tokens gives 8192 tokens of KV capacity per backend. This
is the constraint the entire project is about, so it has to be real and it has to be
reachable.

`--max-num-seqs 32` caps concurrent requests per backend. This started at 8, which turned
out to be a mistake: with only 8 slots, the slot limit was always reached before the KV
limit, so the KV constraint never actually bound and the part of the algorithm that reasons
about memory was never exercised. Raising it to 32 makes KV the binding constraint, which
is the regime the router is designed for.

`--time-to-first-token 50ms --inter-token-latency 20ms` set baseline timing. A response of
N tokens takes roughly 50ms plus N times 20ms.

`--time-factor-under-load 2.5` makes the backend slow down as it fills up, by up to 2.5
times. At the default of 1.0 load has no effect on latency at all, which would make the
whole test meaningless.

### Verifying the backends came up

A container that prints an ID has not necessarily started successfully. It can exit
immediately afterwards, and `docker ps` only lists running containers.

```bash
docker ps
docker logs sim1
curl http://localhost:8001/v1/models
```

The logs are the important one. The simulator prints its full resolved configuration on
startup, so this is where you confirm the flags actually took effect, and where startup
errors appear.

---

## 3. Designing the workload

The workload is the heart of the test. It has to reproduce the situation the router is
meant to handle: many requests arriving continuously, with wildly different response sizes.

### Response lengths follow a lognormal distribution

Real chat traffic is mostly short replies with an occasional very long one. A lognormal
distribution produces exactly that shape. The generator draws each response length
independently, so the sequence is unpredictable, but small values are more likely than
large ones.

A typical sample of 500 requests looks like this:

| statistic | output tokens |
|---|---|
| median | 127 |
| 90th percentile | 497 |
| 99th percentile | 1720 |
| maximum | 3000 |
| mean | 223 |

Half the requests generate around 127 tokens, one in ten generates over 490, one in a
hundred generates over 1700. That spread is what a request counting load balancer is blind
to.

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

### Prompt length and response length vary independently

Because echo mode ties the response to the prompt, the generator draws both lengths
separately and sets the prompt to whichever is larger. When the prompt draw is larger, the
result is a long prompt with a short response, which is the pattern seen when someone
pastes a large document and asks a one line question. Roughly two thirds of generated
requests have this shape.

The one pattern this cannot produce is a short prompt with a long response. That is a known
limitation of echo mode and is left for stage 0b on real vLLM.

### Generating traces

```bash
python3 generate_trace.py --kind lognormal --num-requests 1500 --rate 8 --out tr_lognorm_r8.json
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
its own schedule and that run is invalid.

```bash
python3 loadgen.py --trace tr_lognorm_r8.json \
  --backends http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004 \
  --out results/r8_least_conn.csv --policy least_conn --max-num-seqs 32
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

Output is redirected to a file rather than a terminal, because terminal rendering is
considerably slower than a file write:

```bash
./stage5_compare.sh > compare.log 2>&1 &
tail -f compare.log
```

---

## 5. The routing policies under test

Three policies are compared, all against identical traffic.

**least_conn** sends each request to whichever backend currently has the fewest requests in
flight. This is what nginx and HAProxy do and it is the baseline being challenged.

**kvts** ranks backends by total committed work, measured in KV token seconds, which
accounts for both how much memory a request holds and how long it holds it. This policy was
implemented, tested, and found to be flawed. The formula values one long request at roughly
256 times one short request, so a backend holding a single long generation is avoided
entirely even when it has most of its capacity free. It manufactures scarcity by over
avoiding. It remains in the code as an ablation arm.

**pressure** is the corrected policy. Rather than comparing absolute work, it measures how
close each backend is to its limits as a fraction:

```
u = max( in_flight / max_num_seqs , kv_projected / kv_capacity )
```

Below a threshold the score is simply that fraction. Above it, a quadratic penalty engages,
because memory exhaustion fails suddenly rather than gradually. A hard admission gate
refuses any backend whose projected memory would exceed a safety ceiling.

This design comes directly from the research: per sequence cost is close to zero while a
backend has spare capacity, so the score should be nearly flat there, and the failure at the
top is super linear, so the penalty should be quadratic.

### Matching the router's memory model to the backend

The simulator allocates KV memory based on prompt length at admission and does not grow the
allocation as tokens are generated. This was measured by ramping concurrency on a single
backend until it started rejecting:

| concurrency | result |
|---|---|
| 16 | 16 of 16 succeeded |
| 20 | 19 of 20 succeeded |
| 32 | 19 of 32 succeeded |

The ceiling sits at 19 concurrent requests. With an average prompt of 415 tokens, 19 times
415 is 7885, which is just under the 8192 capacity. Had memory grown with generated tokens,
the ceiling would have been around 12. So the simulator allocates by prompt only.

Real vLLM does grow KV memory during generation. The load generator therefore has a
`--kv-growth` flag, off by default to match the simulator, which must be turned on for
stage 0b against real backends.

---

## 6. Measurement discipline

Three rules make the numbers trustworthy.

**Warmup and drain are discarded.** The first requests of a run hit empty backends and the
last ones run with no competition. Neither is steady state. The comparison tool keeps the
middle 60 percent of the arrival window, filtered by scheduled arrival time.

**Sample count has to support the statistic.** With 200 requests, the 99th percentile is
effectively the second worst request and moves wildly between runs. Runs use 1500 requests.

**Error rate is read before latency.** Rejected requests never receive a first token, so
they contribute nothing to latency percentiles. A policy that rejects half its traffic
would look extremely fast. In this simulator the backend sheds rather than queues, so
latency stays flat around 85 to 106 milliseconds no matter the load, and error rate is the
only meaningful discriminator.

---

## 7. What was found

Running `least_conn` across arrival rates from 8 to 24 produced this:

| rate | error rate | TTFT p99 |
|---|---|---|
| 8 | 27.6% | 94 ms |
| 12 | 38.6% | 104 ms |
| 16 | 51.0% | 100 ms |
| 20 | 61.0% | 98 ms |
| 24 | 60.9% | 106 ms |

Latency barely moves while the failure rate climbs steadily. The system does not get slower
under pressure, it starts refusing work.

Breaking down a single run at rate 8 by backend reveals the mechanism:

| backend | requests | prompt tokens | output tokens | errors |
|---|---|---|---|---|
| 8004 | 426 | 178,775 | 84,034 | 136 |
| 8001 | 382 | 171,464 | 83,515 | 105 |
| 8003 | 363 | 148,660 | 82,899 | 80 |
| 8002 | 329 | 138,147 | 84,520 | 69 |

Output tokens are spread across backends to within 1.9 percent, which looks like excellent
balance. Prompt tokens are spread by 25.5 percent, and prompt tokens are what the KV cache
actually holds. Errors line up with prompt tokens in exact order: the backend carrying the
most prompt tokens takes the most rejections, the one carrying the fewest takes the fewest,
with no exceptions.

This is the problem statement confirmed by direct measurement. A request counting balancer
believes it is doing well, because by its own metric it is. The quantity that actually
fills memory is 25 percent lopsided, and that lopsidedness is paid for in rejected
requests.

---

## 8. Current status

The problem is confirmed and its mechanism is measured. What has not yet been established
is whether the proposed algorithm fixes it.

The `pressure` policy tracks projected KV usage per backend, which is precisely the
quantity found to be imbalanced. It therefore makes a falsifiable prediction: it should
reduce the error rate at the same arrival rate on the same traffic. That comparison is the
next run.

```bash
./stage5_compare.sh > compare.log 2>&1 &
tail -f compare.log
```

Three policies across three arrival rates, roughly twenty minutes. Interpretation rules for
the result:

Error rate at the same rate is the whole test. If `pressure` does not reduce it, the
mechanism does not work as theorised, and that is a real result worth having rather than
something to explain away.

Latency should be ignored, since the simulator sheds rather than queues.

If `pressure` improves the error rate mainly by refusing to dispatch rather than by
distributing better, that is a different and weaker claim than the one being made. The
generator reports a `saturated_dispatches` counter which distinguishes the two.

Once this lands, stage 0b repeats the same traces against real vLLM on a rented GPU, with
`--kv-growth` enabled, to confirm the finding survives contact with real hardware. Only
then does Phase 1 begin and the Rust implementation starts.

---

## Files

| file | purpose |
|---|---|
| `generate_trace.py` | builds fixed traffic traces from a lognormal distribution |
| `loadgen.py` | open loop generator, routing policies, KV accounting |
| `compare.py` | side by side comparison with warmup and drain trimming |
| `stage3_knee.sh` | sweeps arrival rate to locate the failure threshold |
| `stage4_theta.sh` | tunes the penalty threshold of the pressure policy |
| `stage5_compare.sh` | runs all policies at all rates and prints the comparison |
