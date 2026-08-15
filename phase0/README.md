# LLM Routing Policy Testbed

A harness for measuring whether a routing policy actually helps when you put several
LLM inference backends behind a load balancer.

It runs entirely on CPU. No GPU is required, and no model weights are downloaded.

## Why this exists

Conventional load balancers distribute traffic by counting: fewest active connections,
round robin, or a hash. That works when requests cost roughly the same. LLM traffic
breaks the assumption, because a request that generates twenty tokens and a request
that generates three thousand tokens look identical at the moment you have to route
them, and they differ in cost by two orders of magnitude.

The natural response is to route by predicted work instead of request count. The
problem is that it is very easy to build such a policy, watch a benchmark number move,
and conclude it worked when the benchmark was actually measuring something else. This
repository exists to make that mistake harder. It gives you fixed reproducible traces,
an open loop generator that does not deform under load, several routing policies behind
one interface, and an analysis step that trims warmup and drain and reports error rate
alongside latency.

You point it at any set of OpenAI compatible endpoints. There is nothing here specific
to a particular router, model, or serving stack.

## What is in the box

`generate_trace.py` produces a fixed workload file. Arrival times come from a Poisson
process, output lengths from a lognormal distribution, so the trace has the heavy tail
that real chat traffic has rather than two convenient buckets. The file is written once
and replayed byte for byte across every policy you compare, which removes workload
variation as an explanation for any difference you see.

`loadgen.py` replays a trace against a list of backends, applying a chosen routing
policy, and records per request timing to CSV. It is open loop by construction: arrival
times are computed up front and fired on schedule regardless of whether earlier
responses have returned.

`compare.py` reads one or more result CSVs and prints a side by side table with warmup
and drain trimmed off.

`verify.py` asserts ten measurement invariants on a results directory and exits non zero if
any fail. Run it before believing a number. Every check in it corresponds to a bug that
actually happened here and silently corrupted results for several runs before being caught.

`occupancy_stats.py` reports instantaneous per backend occupancy from a run log. This is the
metric that explains *why* a policy wins, and `compare.py` structurally cannot show it.

`proxy_spread.py` recovers the real per backend distribution for runs driven through an
external router, by diffing Prometheus counters from before and after the run.

`kv_curve.py` sends one request to an idle backend and plots its memory against time. Use it
to check what a backend actually charges per request before trusting any KV projection.

`scrape_backend_counts.py` snapshots per backend Prometheus counters to JSON.

`analyze.py` is a simpler single file summary, kept for quick checks.

`restart_sims.sh` brings all four backends up cold and waits until each answers. The
`stage*.sh` scripts drive the sweeps and are described further down.

## Requirements

Python 3.9 or newer, Docker, and roughly 2 GB of free RAM for four simulated backends. No
GPU, no model weights, no API keys.

```bash
cd phase0
python3 -m venv venv
./venv/bin/pip install httpx
```

`httpx` is all you need for everything except the competitor benchmark. That one additionally
needs SGLang's router, which is a large install and entirely optional:

```bash
./venv/bin/pip install sglang-router
```

Every command in this document uses `./venv/bin/python` explicitly rather than expecting an
activated virtualenv. If you have conda or another Python on your PATH, a bare `python3` will
usually be the wrong one and will fail with `ModuleNotFoundError: httpx`.

## Starting the backends

The backends are provided by `llm-d-inference-sim`, which speaks the OpenAI API,
streams Server Sent Events, exposes vLLM style Prometheus metrics, and enforces a KV
cache limit, all without a GPU.

```bash
docker pull ghcr.io/llm-d/llm-d-inference-sim:v0.10.2
```

Start four instances on ports 8001 through 8004. Use the helper script, which brings them up
cold and then polls each one until it actually answers:

```bash
./restart_sims.sh 64
```

It is safe to re-run at any time and is the correct way to reset between measurements. The
argument is `--max-num-seqs`, discussed in the flag table below.

If you prefer to do it by hand, this is the equivalent for one backend. Run it as a single
line: multi-line pastes with backslash continuations get broken by terminal wrapping, and the
container then starts with only some of its flags applied.

```bash
docker run -d --name sim1 -p 8001:8000 -e POD_IP=127.0.0.1 ghcr.io/llm-d/llm-d-inference-sim:v0.10.2 --model test-model --mode echo --max-model-len 8192 --enable-kvcache --kv-cache-size 512 --block-size 16 --max-num-seqs 64 --time-to-first-token 50ms --inter-token-latency 20ms --time-factor-under-load 2.5
```

Confirm they came up, and check the logs rather than trusting the process list, because
this image can print a fatal configuration error and exit after appearing to accept its
flags.

```bash
docker ps
docker logs sim1
curl http://localhost:8001/v1/models
```

What the flags do, and which ones are not optional:

| Flag | Effect |
|---|---|
| `-e POD_IP=127.0.0.1` | Required whenever `--enable-kvcache` is set. The image is built for Kubernetes and refuses to start without it |
| `--model test-model` | Required. Without it the process exits immediately with `model parameter is empty` |
| `--mode echo` | Returns the prompt back as the response, which is what lets the trace control output length |
| `--enable-kvcache` | Turns on KV accounting. Without it the cache limit is never enforced |
| `--kv-cache-size 512` and `--block-size 16` | 512 blocks of 16 tokens gives 8192 tokens of KV per backend |
| `--max-num-seqs 64` | Concurrency cap. Must sit well ABOVE the KV bound, or the slot limit binds first and hides the thing being measured. At 32 the two bind at almost the same point, which partly masks KV exhaustion |
| `--max-model-len 8192` | Per request context limit. Must exceed your longest prompt or those requests fail |
| `--inter-token-latency 20ms` | Simulated decode speed |
| `--time-factor-under-load 2.5` | How much the backend slows as it fills. At the default of 1.0 load has no effect on latency at all, which makes most experiments meaningless |

To tear down:

```bash
docker stop sim1 sim2 sim3 sim4 && docker rm sim1 sim2 sim3 sim4
```

## A first run

Ten minutes, start to finish. Generate a trace, replay it under two policies, compare, and
check that the measurement is trustworthy.

```bash
./restart_sims.sh 64

./venv/bin/python generate_trace.py --kind lognormal --num-requests 600 --rate 12 --out /tmp/r12_demo.json

BK=http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004

./venv/bin/python loadgen.py --trace /tmp/r12_demo.json --backends "$BK" \
  --out /tmp/r12_least_conn.csv --policy least_conn --max-num-seqs 64 --seed 1

./restart_sims.sh 64

./venv/bin/python loadgen.py --trace /tmp/r12_demo.json --backends "$BK" \
  --out /tmp/r12_pressure.csv --policy pressure --max-num-seqs 64 --seed 1

./venv/bin/python compare.py /tmp/r12_least_conn.csv /tmp/r12_pressure.csv
```

Three things about this that are not incidental:

The `restart_sims.sh` between the two runs is not optional if you intend to believe the
result. The second policy would otherwise inherit whatever cache the first one warmed.

Rate 12 is chosen because it is past the knee, where backends are actually failing. At rate 8
with a short run both policies come back near zero errors and the comparison shows nothing,
or worse, shows noise pointing the wrong way.

Result files are named `r<rate>_<policy>.csv` because `compare.py` parses the rate out of the
filename. Name them something else and the rate column reads `?`.

**This is a smoke test, not a measurement.** Six hundred requests through a single run of each
policy tells you the pipeline works. It does not tell you which policy is better: at that
sample size the difference between two runs of the *same* policy can exceed the difference
between policies. The real numbers come from `stage5_compare.sh`, which runs 1500 requests per
cell, three times per cell, with cold backends between every arm.

---

## The full Phase 0 workflow

This is the complete sequence that produced the published results, in order. Each stage
depends on the one before it.

### Step 0. Sanity checks before measuring anything

Two checks worth running once on any new simulator version or machine, because both have
silently invalidated results here before.

**Does the backend charge KV the way the router assumes?** This sends one request to an idle
backend and samples its memory across the whole request lifetime.

```bash
docker run -d --name simsolo -p 8005:8000 -e POD_IP=127.0.0.1 ghcr.io/llm-d/llm-d-inference-sim:v0.10.2 --model test-model --mode echo --max-model-len 8192 --enable-kvcache --kv-cache-size 512 --block-size 16 --max-num-seqs 64 --time-to-first-token 50ms --inter-token-latency 20ms --time-factor-under-load 2.5

sleep 5
./venv/bin/python kv_curve.py --backend http://localhost:8005 --words 1000
docker rm -f simsolo
```

It prints a verdict. Against `llm-d-inference-sim` the memory usage should be **flat** for the
whole request and equal to the prompt alone, which is why `--kv-model prompt_only` is the
default. If it climbs instead, the backend grows memory during generation like real vLLM does,
and every run needs `--kv-model prompt_plus_output`.

**Does the KV limit actually bind?** Generate a trace and read the capacity estimate:

```bash
./venv/bin/python generate_trace.py --kind lognormal --num-requests 500 --rate 8 --out /tmp/check.json
```

The output reports a length-biased mean and a saturation rate. If the concurrent-requests
figure is close to `--max-num-seqs`, the slot limit will bind before the memory limit and the
experiment measures the wrong constraint. Raise `--max-num-seqs`.

**Treat the saturation figure as a starting point for the sweep, not as ground truth.** It is
a closed-form estimate and it has been wrong in both directions. The earlier version divided
capacity by the *mean* request size and overestimated capacity by about three times. The
current version uses `E[L²]/E[L]`, which is the correct statistic, but `E[L²]` is dominated by
the few longest requests in the sample and is therefore noisy and pessimistic: on the
reference setup it suggests roughly 3 req/s while the measured knee is 8. Use it to choose a
sweep range, then let `stage3_knee.sh` tell you where the knee actually is.

### Step 1. Find the knee

No policy can show an advantage below the point where backends start failing, and none can
help far above it. This sweeps arrival rate to locate that point.

```bash
./stage3_knee.sh 2>&1 | tee results_knee/sweep_log.txt
```

Roughly 20-30 minutes. Read the `err%` column and pick the first rate where errors are
material but the system is not collapsed. On the reference setup that is **8 req/s**.

### Step 2. Compare the policies

```bash
RATES="8 10 12 14" ./stage5_compare.sh 2>&1 | tee results_compare/compare_log.txt
```

Roughly two hours for four rates at three repeats each. Override the defaults with
environment variables:

```bash
RATES="8 10" REPEATS=3 POLICIES="least_conn pressure" ./stage5_compare.sh
```

### Step 3. Compare against a real competitor

Needs `sglang-router` installed. Puts SGLang's production Rust router in front of the same
backends and drives it as an external proxy.

```bash
./stage6_competitors.sh 10 2>&1 | tee results_compet/compet_log.txt
```

Roughly 90 minutes. The first argument is the arrival rate.

### Step 4. Verify the measurements before believing them

```bash
./venv/bin/python verify.py results_compare
./venv/bin/python verify.py results_compet
```

Ten assertions, described in the next section. Exits non-zero if any fail. Run this before
quoting any number from a results directory.

### Step 5. Analyse

```bash
# headline comparison table
./venv/bin/python compare.py results_compare/*.csv

# the mechanism: instantaneous occupancy, parsed from the run log
./venv/bin/python occupancy_stats.py results_compare/compare_log.txt

# real per-backend distribution for external-router runs
./venv/bin/python proxy_spread.py results_compet
```

### Tearing down

```bash
docker rm -f sim1 sim2 sim3 sim4
```

## Routing policies

All policies live in `choose_backend()` in `loadgen.py` and are selected with
`--policy`.

`least_conn` sends each request to the backend with the fewest requests in flight,
breaking ties at random. This is the baseline that nginx, HAProxy and Envoy implement,
and it is what you are trying to beat.

`kvts` scores each backend by outstanding KV token seconds, which is a measure of
committed work integrated over its expected lifetime. A long generation holds more
memory and holds it for longer, so its cost grows quadratically with output length.

`pressure` scores by how close a backend is to its binding capacity limit rather than
by how much work it holds. Occupancy is the larger of the slot fraction and the KV
fraction, so whichever limit is nearer dominates. The score is linear below a threshold
and quadratic above it, which reflects the fact that adding a request to an idle backend
is nearly free while adding one to a full backend is not. A hard admission gate refuses
any backend whose projected KV would exceed a safety ceiling.

`kvts_p2c` and `pressure_p2c` are the same scores applied to two randomly sampled
backends instead of all of them. Sampling two and taking the better one avoids the
herding behaviour that pure least loaded selection exhibits when several requests arrive
at once.

Relevant tuning flags:

| Flag | Default | Meaning |
|---|---|---|
| `--max-num-seqs` | 32 | Must match the backend setting or the slot term is wrong |
| `--kv-capacity` | 8192 | Must match `kv-cache-size` multiplied by `block-size` |
| `--theta` | 0.70 | Occupancy at which the convex penalty begins |
| `--penalty` | 10.0 | Weight of the penalty above theta |
| `--sigma` | 0.90 | Admission ceiling as a fraction of KV capacity |
| `--kv-model` | `prompt_only` | Whether a request's KV grows as it generates. `prompt_only` matches this simulator, measured. `prompt_plus_output` matches real vLLM |
| `--output-model` | `echo` | How to predict output length. `echo` sets it to the prompt length, matching this simulator. `max_tokens` is for real vLLM |
| `--seed` | 0 | Seeds tie breaking. Without it two runs of the same trace are not comparable |
| `--verbose` | off | Log every completed request. Leave off during measurement |

## The scripts, and what each one takes

| script | argument | what it does | time |
|---|---|---|---|
| `restart_sims.sh` | max-num-seqs (default 64) | brings all four backends up cold, polls until ready | ~15 s |
| `stage3_knee.sh` | none | sweeps arrival rate 4-14 under `least_conn` to find the knee | ~25 min |
| `stage4_theta.sh` | rate | sweeps the `theta` parameter at one rate, with `least_conn` as reference | ~30 min |
| `stage5_compare.sh` | env: `RATES`, `POLICIES`, `REPEATS` | every policy at every rate, with repeats | ~30 min per rate |
| `stage6_competitors.sh` | rate (default 10) | benchmarks against sgl-router | ~90 min |
| `start_competitor_router.sh` | policy, port | launches one sgl-router instance | ~25 s |

All of them restart the backends cold before every arm, so runs are independent.

Redirect output to a file rather than watching it in a terminal, because terminal rendering
is slow enough to interfere with the event loop. `tee` gets you both, and the log is required
input for `occupancy_stats.py`:

```bash
./stage3_knee.sh 2>&1 | tee results_knee/sweep_log.txt
```

To run something long in the background and watch it:

```bash
./stage5_compare.sh > results_compare/compare_log.txt 2>&1 &
tail -f results_compare/compare_log.txt
```

### Running one measurement by hand

Everything the scripts do reduces to this:

```bash
./restart_sims.sh 64

./venv/bin/python loadgen.py \
  --trace tr_v2_r10.json \
  --backends http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004 \
  --out results/r10_pressure.csv \
  --policy pressure \
  --max-num-seqs 64 \
  --kv-model prompt_only \
  --theta 0.55 --sigma 0.95 \
  --seed 1
```

Watch a run live from a second terminal:

```bash
for i in 1 2 3 4; do echo -n "sim$i: "; curl -s http://localhost:800$i/metrics | grep '^vllm:kv_cache_usage_perc'; done
```

Comparing that against the `occupancy[...]` figures in the run's own status line is how the
router's memory model was validated against reality. The two should track within a few
percent. If they do not, the router is routing on a fiction and nothing downstream is valid.

## Reading the results

`compare.py` trims twenty percent of the arrival window from each end by default, so
that requests which arrived while the backends were still filling, and requests which
ran alone at the end with no competition, do not contaminate the percentiles. Trimming
is done by scheduled arrival time, so a request that arrived inside the steady window
counts regardless of when it finished. Use `--trim 0` to disable.

Read the error column first. When a backend runs out of KV it rejects the request, and a
rejected request has no time to first token, so a policy that sheds load can appear
faster than one that serves it. Error rate and latency have to be read together.

Treat a difference that appears only at p99 with suspicion. With fifteen hundred
requests, p99 rests on roughly fifteen samples. If p95 and p99 move in the same
direction by similar amounts, the result is more likely to be real.

The three spread columns describe how evenly load landed, and none of them is the
mechanism. That distinction cost a wrong conclusion once, so it is worth stating plainly.

`DISP kv` is the spread of prompt tokens dispatched per backend across **every** request,
rejections included. Read it as whether each backend did a fair share of total work over
the run. `DISP req` is the same for request counts, and `least_conn` drives it toward zero
by construction. `ok tok` covers successes only; do not read it as balance at all, because
a backend that receives heavy traffic and rejects most of it registers as lightly loaded.

**A cost aware policy can score worse on `DISP kv` while winning decisively on error
rate, and that is not a contradiction.** Measured here at rate 8: `pressure` cut errors
from 2.0% to 0.3% while showing *higher* cumulative KV spread than `least_conn`, roughly
25% against 15%. Rejections are caused by instantaneous occupancy crossing the ceiling,
not by unequal totals accumulated over a run. A policy that routes to whichever backend
has headroom at this instant will produce uneven totals deliberately.

For the mechanism, use `occupancy_stats.py`, which parses per backend occupancy out of a
run log and reports how full the fullest backend is at a typical moment, how much of the
run some backend spent at or above the admission ceiling, and the instantaneous spread.
On the same runs that produced the cumulative numbers above:

| rate | policy | instantaneous spread | time at or above 0.95 | err% |
|---|---|---|---|---|
| 8 | `least_conn` | 91.1% | 15.9% | 2.0% |
| 8 | `pressure` | 56.8% | 8.8% | 0.3% |
| 10 | `least_conn` | 96.9% | 30.7% | 3.5% |
| 10 | `pressure` | 53.0% | 14.4% | 0.9% |

Instantaneous spread halves, time in the danger zone halves, and error rate follows.
Cumulative spread moves the other way. Both are true, and only one of them explains the
result.

## Verifying that a result set can be believed

A run that has lost a third of its requests prints the same reassuring `Done.` as a clean
one. Nothing in the normal output tells you whether the numbers mean what you think they
mean, and this harness has produced confident wrong numbers more than once.

`verify.py` asserts the invariants directly. Point it at a results directory.

```bash
./venv/bin/python verify.py results_compare
./venv/bin/python verify.py results_knee --log results_knee/sweep_log.txt
```

It picks up `compare_log.txt` or `sweep_log.txt` automatically if either sits in the
directory. Each check prints `PASS`, `FAIL`, `WARN` or `SKIP` with the reason, and the
exit code is 1 if anything failed, so it can gate a pipeline.

| Check | Asserts | Why it exists |
|---|---|---|
| request conservation | `req_id`s are contiguous with no duplicates | A dropped request is counted as neither success nor error, so it disappears from the error rate instead of showing up as a failure |
| silent failures | No row is a success with zero tokens | KV rejections arrive as HTTP 200 with the error inside the SSE stream. Treating those as empty successes once made the error rate read 0.0% when the true rate was 61% |
| error taxonomy | Errors really are KV exhaustion | The request path catches every exception into the same field, so connection refusals and timeouts get reported as "KV exhaustion rate" unless you look |
| error latency | Errors returned instantly | A capacity rejection comes back in about 0.0s. A slow error is a timeout wearing the same label |
| backend coverage | All four backends received traffic | A backend absent from the results was never routed to, usually a typo in the backend list |
| length invariant | Output equals prompt in echo mode | If this breaks, the backend is not behaving as the KV projection assumes, and the projection is wrong |
| trim sensitivity | Error rate is stable with and without trimming | The warmup and drain trim is defensible, but if it moves the headline number it is doing real work and must be disclosed |
| KV accounting leak | `in_flight == 0` implies `occupancy == 0` | The strongest structural check available. Nonzero occupancy with nothing in flight means the router permanently believes memory is held that is not, and will under admit for the rest of the run |
| coordinated omission | `lag_events` is near zero | Late dispatches mean offered load never reached the target rate and every latency number is optimistic |

Run it before believing any comparison, and again before publishing anything.

## Things that will waste your time if you do not know them

**Streaming errors arrive as HTTP 200.** When the KV cache is exhausted during a
streaming request, this simulator returns status 200 and puts the error inside an SSE
frame as `{"error": {...}}`. Only non streaming requests get an HTTP 500. A parser that
looks only for `choices` will treat these as successful responses that happened to
produce zero tokens, and your error rate will read zero while most requests are failing.
`loadgen.py` checks for the error key explicitly.

**Repeated text in prompts defeats the KV cache.** If every prompt is built from the
same repeated token, every prompt becomes a prefix of every longer prompt, block level
hashing deduplicates them, and twenty concurrent requests cost about what one costs. The
KV limit then never binds no matter how much load you apply. `generate_trace.py` builds
prompts from a twenty thousand word random vocabulary for this reason. If you want
genuine prefix sharing, add it deliberately with `--shared-prefix-frac`.

**This simulator allocates KV by prompt length at admission.** It does not grow the
allocation as tokens are generated, which real vLLM does. That is why `--kv-model`
exists and why it defaults to `prompt_only` here.

This was measured directly rather than assumed, and it is worth knowing how, because
assuming the opposite cost this project several days. `kv_curve.py` sends one request to
an otherwise idle backend and samples `/metrics` across its lifetime. A thousand word
prompt generating a thousand tokens over twenty seconds held KV **flat at 0.1211 the
entire time**, which is 992 tokens, which is 62 blocks of 16, which is the prompt alone.
Generated tokens cost this backend nothing. Run it yourself if you change simulator
versions.

**Echo mode forces output length to equal prompt length, and ignores `max_tokens`
entirely.** Measured: a 500 word prompt returns exactly 500 completion tokens whether
`max_tokens` is 50, 200, or 5000. There is therefore exactly one length per request, not
two, and the trace generator draws one.

An earlier version of this file claimed the harness could produce long prompt with short
output, the retrieval augmented pattern, by setting `max_tokens` below the prompt length.
**That pattern never existed in any trace this repository generated.** The backend
ignored the cap and echoed the full prompt every time. Prompt and output cannot be
decoupled here at all; testing that shape needs `--backend-mode real` against real vLLM.

**Capacity per backend is not KV divided by mean request size.** It is roughly three
times smaller, because of length biased sampling. A request holds its KV for a time
proportional to its length, so long requests linger, and the set of requests occupying a
backend at any instant is therefore biased toward the long ones. A two thousand token
request is present about fifteen times longer than a one hundred and thirty token one,
so it is about fifteen times more likely to be there when you look. The figure that
matters is `E[L²]/E[L]`, not `E[L]`, and for a heavy tailed distribution those differ by
a factor of three or more. Predicting thirty two concurrent requests per backend from the
mean, this harness measured KV exhaustion at thirteen to fifteen. `generate_trace.py`
now computes both moments and prints the length biased mean.

**Logging inside the request loop inflates the metric you are measuring.** A `print`
call is a blocking syscall on a single threaded event loop, and while it runs no stream
is read while every in flight timer keeps running. Per request logging is off by default.

**Closed loop generators cannot measure overload.** If the generator waits for responses
before sending more requests, offered load falls as the system slows and the queue never
builds. Arrival times here are computed up front and latency is measured from scheduled
arrival rather than from send time. The generator reports a lag counter at the end of
each run, and a run with substantial lag should be discarded.

## Limitations

The simulator models concurrency limits, a load dependent slowdown, and a KV cache
ceiling. It does not model preemption, prefix cache hit effects on latency, chunked
prefill, or the batch size quantisation that real GPU kernels exhibit. When KV runs out
it rejects the request rather than evicting and recomputing one, which is what vLLM
does. Results here are useful for checking that a policy behaves the way you think it
does and for catching bugs cheaply. They are not a substitute for measuring against real
inference backends.

## Layout

```
generate_trace.py         build a reproducible trace
loadgen.py                replay a trace under a routing policy
compare.py                side by side comparison with trimming
occupancy_stats.py        instantaneous occupancy from a run log, which is the
                          mechanism metric compare.py cannot show
verify.py                 assert measurement integrity on a results directory
analyze.py                single file summary

kv_curve.py               single request KV against time, for checking what a
                          backend actually charges per request
scrape_backend_counts.py  snapshot per backend counters from /metrics, needed
                          for proxy runs where the generator cannot see which
                          backend served a request
restart_sims.sh           bring all four backends up cold

stage3_knee.sh            find the rate where backends start failing
stage4_theta.sh           tune the pressure threshold at a given rate
stage5_compare.sh         compare all policies across several rates
stage6_competitors.sh     compare against sgl-router

```

Traces, result CSVs and logs are generated artifacts and are not tracked.
