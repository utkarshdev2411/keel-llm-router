# Baseline configurations

Every system in the Phase 1 comparison matrix, configured to its strongest realistic
setting. Comparing against a hobbled baseline is worse than no comparison at all — it
invites the first informed reader to point out the flag you forgot and discredits every
other number in the write-up.

| System | Config | Policy | Non-default settings |
|---|---|---|---|
| nginx | `configs/nginx.conf` | `least_conn` | `proxy_buffering off` — default buffering destroys streaming latency and would fake a TTFT win |
| HAProxy | `configs/haproxy.cfg` | `random(2)` | Deliberately not `leastconn` — `random(2)` is HAProxy's power-of-two-choices and the real strong default |
| Envoy | `configs/envoy.yaml` | `LEAST_REQUEST`, `choice_count: 2` | Stated explicitly rather than left implicit, so the setting survives a config diff |
| sgl-router | see `../phase0/start_competitor_router.sh` | `cache_aware`, `power_of_two` | Already built and used for the Phase 0 competitor benchmark — not duplicated here |
| This router | `configs/router/{round_robin,least_requests,p2c}.toml` | one file per baseline strategy | — |

All backend addresses point at the same four simulated backends
(`http://127.0.0.1:8001-8004`) used throughout Phase 0, so every system in this matrix is
compared on identical infrastructure.

## Running a baseline

```bash
# our router
cargo run -p router-bin -- bench/configs/router/least_requests.toml

# nginx
nginx -c "$(pwd)/bench/configs/nginx.conf"

# HAProxy
haproxy -f bench/configs/haproxy.cfg

# Envoy
envoy -c bench/configs/envoy.yaml

# sgl-router
./phase0/start_competitor_router.sh cache_aware 9000
```

**Never run two systems at once.** They compete for the same cores and corrupt each
other's timing.

## Reusing the existing load generator

`phase0/loadgen.py` is the reference implementation for every published Phase 0 number
(open-loop, measures from scheduled arrival time, asserts its own schedule adherence,
parses the SSE surface including the in-band error frame). Point it at whichever system
is running on port 8080 the same way `phase0/stage6_competitors.sh` already points it at
`sgl-router`:

```bash
./venv/bin/python loadgen.py --trace tr_r8.json --backends http://127.0.0.1:8080 \
    --out results/r8_nginx.csv --policy proxy --max-num-seqs 64
```

See `Load-Balancer-Docs/implementation-algorithm/4. Benchmark & Test Guide.md` for the
full methodology this matrix feeds into.
