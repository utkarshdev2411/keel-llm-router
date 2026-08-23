#!/bin/bash
# Phase 2 exit criterion: pressure vs power-of-two-choices least-requests.
#
# The criterion: at the knee arrival rate, pressure beats p2c on ERROR RATE by at
# least 3x, spends a lower fraction of the run at or above sigma occupancy, and
# serves at least as many requests. Slightly worse tail TTFT is expected and
# acceptable — keeping more requests alive means more concurrent work.
#
# Read err% and the ceiling fraction. Do NOT read TTFT p99 as a win condition.
#
# Rigor rules, carried over from the Phase 0 policy comparison:
#   - both arms go through the SAME router binary, so the extra network hop is
#     identical and cancels out of the comparison
#   - cold backend restart before EVERY run, including repeats
#   - the router is restarted per run too, so its cumulative ceiling counters
#     cover exactly that run
#   - the release build happens ONCE, in the foreground, before any run: a cold
#     build takes 30-70s and would otherwise race the load generator
#   - >=3 repeats per arm; a single run cannot separate a difference from noise
#   - identical trace and seed across arms

set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

R="${1:-8}"
REPEATS="${REPEATS:-3}"
N=1500
MAX_NUM_SEQS=64
PY="$ROOT/phase0/venv/bin/python"
TRACE="tr_v2_r${R}.json"
OUT="results_phase2"
ADMIN="http://127.0.0.1:9090/metrics"

ROUTER_PID=""
kill_router() {
  if [ -n "$ROUTER_PID" ]; then
    kill "$ROUTER_PID" 2>/dev/null || true
    wait "$ROUTER_PID" 2>/dev/null || true
    ROUTER_PID=""
    sleep 1
  fi
}
trap kill_router EXIT

# A router left over from an earlier session is the single most dangerous failure
# here. It keeps answering on 8080, so the readiness poll below is satisfied, while
# every router this script starts dies on the fatal admin-port bind. The whole
# comparison then runs against one stale process with one strategy, and both arms
# report identical numbers that look like a legitimate tie.
echo "=== clearing any router left over from a previous run ==="
pkill -f "$ROOT/target/release/router" 2>/dev/null || true
for _ in $(seq 1 20); do
  if ! ss -ltn 2>/dev/null | grep -qE ':(8080|9090)\s'; then break; fi
  sleep 0.5
done
if ss -ltn 2>/dev/null | grep -qE ':(8080|9090)\s'; then
  echo "ABORT: ports 8080/9090 are still held by something this script did not start."
  ss -ltnp 2>/dev/null | grep -E ':(8080|9090)\s'
  exit 1
fi

echo "=== building router (foreground; must finish before any traffic) ==="
cargo build --release

cd "$ROOT/phase0"
mkdir -p "$OUT"
[ -f "$TRACE" ] || "$PY" generate_trace.py --kind lognormal \
    --num-requests $N --rate "$R" --out "$TRACE"

for ARM in pressure p2c; do
  for K in $(seq 1 "$REPEATS"); do
    echo ""
    echo "--- ${ARM}  run ${K}/${REPEATS}  (rate=${R}) ---"

    # Order matters: stop the old router first, then rebuild the backends, then
    # start a fresh router against backends that are already up. Restarting the
    # sims under a live router makes it eject every backend as unhealthy.
    kill_router
    ./restart_sims.sh "$MAX_NUM_SEQS"

    "$ROOT/target/release/router" "$ROOT/bench/configs/router/${ARM}.toml" \
        > "/tmp/router_${ARM}_${K}.log" 2>&1 &
    ROUTER_PID=$!

    ready=0
    for _ in $(seq 1 30); do
      # A dead router must fail fast rather than let the poll time out against
      # whatever else might be answering on 8080.
      if ! kill -0 "$ROUTER_PID" 2>/dev/null; then
        echo "    router process exited during startup -- see /tmp/router_${ARM}_${K}.log"
        tail -20 "/tmp/router_${ARM}_${K}.log"
        exit 1
      fi
      if curl -s --max-time 1 -o /dev/null "http://127.0.0.1:8080/v1/models"; then
        ready=1; break
      fi
      sleep 1
    done
    if [ "$ready" -ne 1 ]; then
      echo "    router did not come up -- see /tmp/router_${ARM}_${K}.log"
      tail -20 "/tmp/router_${ARM}_${K}.log"
      exit 1
    fi

    # Freshness assertion. If the counters are non-zero before a single request has
    # been sent, we are scraping a router that already served traffic -- i.e. not
    # the one just started -- and every number from this run would be a blend of
    # two arms. This is a hard abort, not a warning.
    PRIOR=$(curl -s --max-time 2 "$ADMIN" \
            | awk '/^router_requests_total\{/ {s+=$2} END {printf "%d", s+0}')
    if [ "${PRIOR:-0}" -ne 0 ]; then
      echo "ABORT: admin endpoint reports $PRIOR requests already served before this run."
      echo "       The router being scraped is not the one just started. Kill all"
      echo "       stray routers and rerun."
      exit 1
    fi

    "$PY" scrape_backend_counts.py \
        "http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004" \
        --out "${OUT}/r${R}_ourproxy-${ARM}__run${K}_before.json"

    "$PY" loadgen.py --trace "$TRACE" --backends "http://127.0.0.1:8080" \
        --out "${OUT}/r${R}_ourproxy-${ARM}__run${K}.csv" \
        --policy proxy --max-num-seqs "$MAX_NUM_SEQS" --seed "$K"

    "$PY" scrape_backend_counts.py \
        "http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004" \
        --out "${OUT}/r${R}_ourproxy-${ARM}__run${K}_after.json"

    # Scrape the router's own counters BEFORE killing it. These are cumulative
    # over the process lifetime, which is exactly this one run.
    echo "  time at or above sigma:"
    python3 "$ROOT/bench/ceiling_stats.py" "$ADMIN" | sed 's/^/    /'
    curl -s "$ADMIN" | grep -E '^router_saturated_dispatches_total ' | sed 's/^/    /' || true
    curl -s "$ADMIN" > "${OUT}/r${R}_ourproxy-${ARM}__run${K}_metrics.txt"
  done
done

kill_router

echo ""
echo "============================================"
echo "PHASE 2 COMPARISON COMPLETE (rate=$R, $REPEATS repeats per arm)"
echo "  Judge on err% and the ceiling fraction, NOT on TTFT p99."
echo "  Spread columns are meaningless in proxy mode; use proxy_spread.py"
echo "  on the _before/_after JSON pairs."
echo "============================================"
"$PY" compare.py "${OUT}"/*.csv
echo ""
"$PY" verify.py "$OUT" || true
