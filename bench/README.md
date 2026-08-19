# Baseline configurations

Every system this router is compared against, configured to its strongest realistic
setting. Comparing against a hobbled baseline is worse than no comparison at all — it
invites the first informed reader to point out the flag you forgot and discredits every
other number in the write-up.

| System | Config | Policy | Non-default settings |
|---|---|---|---|
| nginx | `configs/nginx.conf` | `least_conn` | `proxy_buffering off` — default buffering destroys streaming latency and would fake a TTFT win |
| HAProxy | `configs/haproxy.cfg` | `random(2)` | Deliberately not `leastconn` — `random(2)` is HAProxy's power-of-two-choices and the real strong default |
| Envoy | `configs/envoy.yaml` | `LEAST_REQUEST`, `choice_count: 2` | Stated explicitly rather than left implicit, so the setting survives a config diff |
| sgl-router | see `../phase0/start_competitor_router.sh` | `cache_aware`, `power_of_two` | Already built and used for the existing competitor benchmark — not duplicated here |
| This router | `configs/router/{round_robin,least_requests,p2c}.toml` | one file per baseline strategy | — |

All backend addresses point at the same four simulated backends
(`http://127.0.0.1:8001-8004`) used by the rest of `phase0/`, so every system in this matrix
is compared on identical infrastructure.

## Running a baseline

Run these from anywhere inside the repository; each re-anchors to the repository root first.

```bash
# our router
cd "$(git rev-parse --show-toplevel)" && cargo run -p router-bin -- bench/configs/router/least_requests.toml

# nginx
cd "$(git rev-parse --show-toplevel)" && nginx -c "$(pwd)/bench/configs/nginx.conf"

# HAProxy
cd "$(git rev-parse --show-toplevel)" && haproxy -f bench/configs/haproxy.cfg

# Envoy
cd "$(git rev-parse --show-toplevel)" && envoy -c bench/configs/envoy.yaml

# sgl-router
cd "$(git rev-parse --show-toplevel)" && ./phase0/start_competitor_router.sh cache_aware 9000
```

**Never run two systems at once.** They compete for the same cores and corrupt each
other's timing.

## Reusing the existing load generator

`phase0/loadgen.py` is the reference implementation for every published number in this
project (open-loop, measures from scheduled arrival time, asserts its own schedule
adherence, parses the SSE surface including the in-band error frame). Point it at whichever
system is running on port 8080 the same way `phase0/stage6_competitors.sh` already points it
at `sgl-router`:

```bash
./venv/bin/python loadgen.py --trace tr_r8.json --backends http://127.0.0.1:8080 \
    --out results/r8_nginx.csv --policy proxy --max-num-seqs 64
```

## Verifying the router against the reference baseline

Before any routing logic is trusted, the router has to prove it can carry traffic
correctly: running a baseline strategy, it should reproduce
`phase0/loadgen.py --policy least_conn` on the same trace — matching error rate and
per-backend dispatch counts within run-to-run variance, TTFT p99 consistent with the
reference, router overhead p99 under 1ms, and a clean `verify.py` pass. Run every step
below in order.

**Each command below re-anchors to the repository root itself**, via
`cd "$(git rev-parse --show-toplevel)"`, rather than assuming you are still wherever the
previous command's `cd` left you. That means you can run these one at a time, in a single
terminal, in order, without tracking your own working directory — and it's also why each
block is safe to copy-paste on its own into a fresh terminal.

**1. Bring up the four simulated backends fresh:**

```bash
cd "$(git rev-parse --show-toplevel)/phase0" && ./restart_sims.sh 64
```

**2. Generate the trace (skip if it already exists):**

```bash
cd "$(git rev-parse --show-toplevel)/phase0" && [ -f tr_v2_r8.json ] || ./venv/bin/python generate_trace.py --kind lognormal --num-requests 1500 --rate 8 --out tr_v2_r8.json
```

**3. Run the reference implementation directly against the backends:**

```bash
cd "$(git rev-parse --show-toplevel)/phase0" && mkdir -p results_router_check && ./venv/bin/python loadgen.py \
    --trace tr_v2_r8.json \
    --backends "http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004" \
    --out results_router_check/r8_reference_least_conn.csv \
    --policy least_conn --max-num-seqs 64 --seed 1
```

**4a. Restart the backends cold** — this removes any cache-warming confound left over from
step 3:

```bash
cd "$(git rev-parse --show-toplevel)/phase0" && ./restart_sims.sh 64
```

**4b. Build the router in release mode.** Run this in the foreground and let it finish. A
debug build's overhead numbers are not representative, and a cold release build takes on the
order of a minute — which is exactly long enough to lose a race against the next step if you
background it.

```bash
cd "$(git rev-parse --show-toplevel)" && cargo build --workspace --release
```

**4c. Start the router, then wait until it actually answers** before proceeding. Do not
substitute a fixed `sleep` here: if the router is not listening yet, every request in step 5
fails with `All connection attempts failed` and the run is wasted.

```bash
cd "$(git rev-parse --show-toplevel)" && ./target/release/router bench/configs/router/least_requests.toml > /tmp/router.log 2>&1 &
for i in $(seq 1 30); do
  curl -s --max-time 1 -o /dev/null "http://127.0.0.1:8080/v1/models" && break
  sleep 1
done
ss -ltn | grep -q ':8080' && echo "router is listening" || { echo "ROUTER DID NOT START — see /tmp/router.log"; tail -20 /tmp/router.log; }
```

The router logs as JSON to stdout, captured above in `/tmp/router.log`. You should see a
`"listening"` line with `"bind":"0.0.0.0:8080"`. If you want request-level detail, prefix the
command with `RUST_LOG=debug`.

**5. Drive the identical trace through our router via `--policy proxy`.** Note the output
filename contains `proxy`: `verify.py` skips its per-backend coverage check for such runs,
because in proxy mode the load generator only ever sees the single router URL and would
otherwise report a false failure.

```bash
cd "$(git rev-parse --show-toplevel)/phase0" && ./venv/bin/python loadgen.py \
    --trace tr_v2_r8.json \
    --backends "http://127.0.0.1:8080" \
    --out results_router_check/r8_ourproxy_least_requests.csv \
    --policy proxy --max-num-seqs 64 --seed 1
```

**6. Capture router overhead from its own metrics before shutting it down** — parse +
tokenize + decide, excluding upstream network and generation time.

```bash
curl -s http://127.0.0.1:9090/metrics | grep router_overhead_seconds
```

**7. Stop the router:**

```bash
kill %1 2>/dev/null; wait 2>/dev/null
```

**8. Compare the two runs:**

```bash
cd "$(git rev-parse --show-toplevel)/phase0" && ./venv/bin/python compare.py results_router_check/*.csv
```

**9. Verify measurement integrity on both runs — must show zero failures:**

```bash
cd "$(git rev-parse --show-toplevel)/phase0" && ./venv/bin/python verify.py results_router_check
```

### Reading the result

| Check | Where | Pass condition |
|---|---|---|
| Error rate matches | step 8 table, `err%` column | `r8_reference_least_conn` and `r8_ourproxy_least_requests` agree within run-to-run variance |
| Dispatch counts match | step 8 table, `DISP req`/`DISP kv` columns | Same, within variance |
| TTFT p99 consistent | step 8 table, `p99` column | Our router's p99 is in the same range as the reference's |
| Router overhead < 1ms p99 | step 6 output | `router_overhead_seconds_bucket{le="0.001"}` divided by `router_overhead_seconds_count` is ≥ 0.99 |
| No measurement corruption | step 9 output | `verify.py` reports 0 failures |

If any of these disagree, do not trust the router's numbers yet — it is measuring a
transport bug rather than being ready to carry a routing improvement on top.

---

## Comparing the capacity-aware policy against the baseline

Once the router is trusted to carry traffic (the section above), the question becomes
whether routing on occupancy actually beats routing on request count. Both arms run
through the **same router binary**, so the extra network hop is identical and cancels
out — the only thing that differs is the policy.

What has to be true:

| | Condition |
|---|---|
| **Error rate** | `pressure` at least 3x lower than `p2c` at the knee arrival rate |
| **Mechanism** | `pressure` spends a lower fraction of the run at or above sigma occupancy |
| **Throughput** | `pressure` serves at least as many requests |
| **Tail latency** | Expected to be *slightly worse*. This is not a failure |

That last row matters. Keeping requests alive instead of letting them fail means more
concurrent work, and more concurrent work costs tail latency. A run where `pressure`
also wins on TTFT p99 is more likely to indicate it is quietly shedding load than that
it is doing something right — check the served-request count before celebrating.

**Run it:**

```bash
cd "$(git rev-parse --show-toplevel)" && ./bench/run_pressure_comparison.sh 8
```

The script builds in the foreground first, then for each arm and each repeat restarts
the four backends cold, starts a fresh router, polls until it is actually listening,
runs the trace, and scrapes the router's counters before shutting it down. Default is
3 repeats per arm; override with `REPEATS=5`.

### Measuring time at the ceiling

Error rate comes from the load generator. The mechanism half cannot: in proxy mode the
generator only ever sees the router's single URL and never learns which backend served
a request, which is why `occupancy_stats.py` prints nothing for these runs.

The router samples every backend on a fixed 100 ms tick instead, independent of traffic.
This is deliberate — sampling on the request path would only ever observe the backend
that was *chosen*, so a backend the policy correctly stops choosing because it is
saturated would freeze at a stale value. That is exactly the backend the criterion is
about.

The comparison script prints this per run. To read it from a router that is already
running:

```bash
cd "$(git rev-parse --show-toplevel)" && python3 bench/ceiling_stats.py http://127.0.0.1:9090/metrics
```

`FLEET` is the headline: the share of all backend-time spent at or above sigma. Compare
that number between the two arms.

### Reading the result

| Check | Where | Pass condition |
|---|---|---|
| Error rate | `compare.py` table, `err%` column | `pressure` mean at least 3x below `p2c` mean, with non-overlapping run-to-run ranges |
| Time at ceiling | `ceiling_stats.py`, `FLEET` row | Lower for `pressure` |
| Requests served | `compare.py` table, `n` column | `pressure` >= `p2c` |
| Gate is active | `router_saturated_dispatches_total` | Non-zero under `pressure`. Stuck at zero usually means sigma or the KV model is wrong, not that the gate is unnecessary |
| No leak | `router_backend_kv_projected` in the saved `_metrics.txt` | Returns toward zero as the run drains. A monotonic rise is a lease leak — fix that before reading anything else |
| No measurement corruption | `verify.py` output | 0 failures |

If error rate improves but the ceiling fraction does not move, the improvement is not
coming from the mechanism the design claims, and the result should not be published as
though it were.
