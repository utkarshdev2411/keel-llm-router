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

**4. Restart the backends cold** — this removes any cache-warming confound left over from
step 3:

```bash
cd "$(git rev-parse --show-toplevel)/phase0" && ./restart_sims.sh 64
```

**Then start our router in front of them, in release mode** (a debug build's overhead
numbers are not representative). This one is deliberately from the repository root, not
`phase0/`, since it builds and runs the Rust workspace:

```bash
cd "$(git rev-parse --show-toplevel)" && cargo build --workspace --release && ./target/release/router bench/configs/router/least_requests.toml &
sleep 2
```

**5. Drive the identical trace through our router via `--policy proxy`:**

```bash
cd "$(git rev-parse --show-toplevel)/phase0" && ./venv/bin/python loadgen.py \
    --trace tr_v2_r8.json \
    --backends "http://127.0.0.1:8080" \
    --out results_router_check/r8_our_router_least_requests.csv \
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
| Error rate matches | step 8 table, `err%` column | `r8_reference_least_conn` and `r8_our_router_least_requests` agree within run-to-run variance |
| Dispatch counts match | step 8 table, `DISP req`/`DISP kv` columns | Same, within variance |
| TTFT p99 consistent | step 8 table, `p99` column | Our router's p99 is in the same range as the reference's |
| Router overhead < 1ms p99 | step 6 output | `router_overhead_seconds_bucket{le="0.001"}` divided by `router_overhead_seconds_count` is ≥ 0.99 |
| No measurement corruption | step 9 output | `verify.py` reports 0 failures |

If any of these disagree, do not trust the router's numbers yet — it is measuring a
transport bug rather than being ready to carry a routing improvement on top.
