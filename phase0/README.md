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

`analyze.py` is a simpler single file summary, kept for quick checks.

The three `stage*.sh` scripts drive common sweeps and are described further down.

## Requirements

Python 3.9 or newer, Docker, and roughly 2 GB of free RAM for four simulated backends.

```bash
python3 -m venv venv
source venv/bin/activate
pip install httpx
```

## Starting the backends

The backends are provided by `llm-d-inference-sim`, which speaks the OpenAI API,
streams Server Sent Events, exposes vLLM style Prometheus metrics, and enforces a KV
cache limit, all without a GPU.

```bash
docker pull ghcr.io/llm-d/llm-d-inference-sim:v0.10.2
```

Start four instances on ports 8001 through 8004. Run each command as a single line.

```bash
docker run -d --name sim1 -p 8001:8000 -e POD_IP=127.0.0.1 ghcr.io/llm-d/llm-d-inference-sim:v0.10.2 --port 8000 --model test-model --mode echo --max-model-len 8192 --enable-kvcache --kv-cache-size 512 --block-size 16 --max-num-seqs 32 --time-to-first-token 50ms --inter-token-latency 20ms --time-factor-under-load 2.5
```

```bash
docker run -d --name sim2 -p 8002:8000 -e POD_IP=127.0.0.1 ghcr.io/llm-d/llm-d-inference-sim:v0.10.2 --port 8000 --model test-model --mode echo --max-model-len 8192 --enable-kvcache --kv-cache-size 512 --block-size 16 --max-num-seqs 32 --time-to-first-token 50ms --inter-token-latency 20ms --time-factor-under-load 2.5
```

```bash
docker run -d --name sim3 -p 8003:8000 -e POD_IP=127.0.0.1 ghcr.io/llm-d/llm-d-inference-sim:v0.10.2 --port 8000 --model test-model --mode echo --max-model-len 8192 --enable-kvcache --kv-cache-size 512 --block-size 16 --max-num-seqs 32 --time-to-first-token 50ms --inter-token-latency 20ms --time-factor-under-load 2.5
```

```bash
docker run -d --name sim4 -p 8004:8000 -e POD_IP=127.0.0.1 ghcr.io/llm-d/llm-d-inference-sim:v0.10.2 --port 8000 --model test-model --mode echo --max-model-len 8192 --enable-kvcache --kv-cache-size 512 --block-size 16 --max-num-seqs 32 --time-to-first-token 50ms --inter-token-latency 20ms --time-factor-under-load 2.5
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
| `--max-num-seqs 32` | Concurrency cap. Set above the KV bound so that KV is the binding constraint |
| `--max-model-len 8192` | Per request context limit. Must exceed your longest prompt or those requests fail |
| `--inter-token-latency 20ms` | Simulated decode speed |
| `--time-factor-under-load 2.5` | How much the backend slows as it fills. At the default of 1.0 load has no effect on latency at all, which makes most experiments meaningless |

To tear down:

```bash
docker stop sim1 sim2 sim3 sim4 && docker rm sim1 sim2 sim3 sim4
```

## A first run

Generate a trace, replay it under two policies, and compare.

```bash
python3 generate_trace.py --kind lognormal --num-requests 1500 --rate 8 --out trace_r8.json

python3 loadgen.py --trace trace_r8.json \
  --backends http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004 \
  --out r8_least_conn.csv --policy least_conn --max-num-seqs 32

python3 loadgen.py --trace trace_r8.json \
  --backends http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004 \
  --out r8_pressure.csv --policy pressure --max-num-seqs 32

python3 compare.py r8_least_conn.csv r8_pressure.csv
```

The trace generator prints a summary of what it built, including a capacity estimate
that tells you roughly which arrival rates will saturate the backends.

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

## The sweep scripts

`stage3_knee.sh` sweeps arrival rate with a single policy to find where the backends
begin to fail. Below that point no policy can demonstrate an advantage because nothing
is under stress, and far above it none can help because the system is simply overloaded.
Everything else should be run at or just past that rate.

`stage4_theta.sh` takes a rate as its argument and sweeps the `theta` parameter at that
rate, with `least_conn` included as a fixed reference line.

`stage5_compare.sh` runs every policy at several rates and prints the comparison table.

Redirect output to a file rather than watching it in a terminal, because terminal
rendering is slow enough to interfere with the event loop.

```bash
./stage3_knee.sh > knee.log 2>&1 &
tail -f knee.log
```

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

BUGFIX_TRACKER.md         open defects and the state of the current fix cycle
```

Traces, result CSVs and logs are generated artifacts and are not tracked.
