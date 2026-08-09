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
| `--max-num-seqs` | 8 | Must match the backend setting or the slot term is wrong |
| `--kv-capacity` | 8192 | Must match `kv-cache-size` multiplied by `block-size` |
| `--theta` | 0.70 | Occupancy at which the convex penalty begins |
| `--penalty` | 10.0 | Weight of the penalty above theta |
| `--sigma` | 0.90 | Admission ceiling as a fraction of KV capacity |
| `--kv-growth` | off | Model KV as growing per generated token. Off matches this simulator. Turn it on against real vLLM |
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

The two spread columns describe how evenly load landed. Request spread and token spread
measure different things, and a policy that deliberately sends unequal request counts in
order to equalise actual work will show high request spread and low token spread. That
combination is the intended behaviour of a cost aware policy, not a fault.

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
allocation as tokens are generated, which real vLLM does. That is why `--kv-growth`
exists and why it defaults to off here.

**Echo mode ties output length to prompt length.** The trace generator sets prompt
length to the larger of the desired prompt and desired output so there is enough text to
echo. That produces long prompt with short output, which is the retrieval augmented
pattern, but it cannot produce short prompt with long output.

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
generate_trace.py    build a reproducible trace
loadgen.py           replay a trace under a routing policy
compare.py           side by side comparison with trimming
analyze.py           single file summary
stage3_knee.sh       find the rate where backends start failing
stage4_theta.sh      tune the pressure threshold at a given rate
stage5_compare.sh    compare all policies across several rates
```

Traces, result CSVs and logs are generated artifacts and are not tracked.
