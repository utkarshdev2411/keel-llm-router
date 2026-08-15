#!/bin/bash
# STAGE 6 - benchmark against a REAL competitor router, not just least-connections.
#
# sgl-router (SGLang's production router, written in Rust) sits in front of the same
# four simulated backends. The load generator drives it through --policy proxy, so
# sgl-router makes the routing decisions and we only measure the outcome.
#
# sgl-router policies tested:
#   cache_aware   - its flagship: radix-tree prefix matching with a load-balance guard
#   power_of_two  - sample two workers, take the less loaded
#
# ---------------------------------------------------------------------------
# THREE CAVEATS THAT MUST BE REPORTED ALONGSIDE ANY NUMBER FROM THIS SCRIPT
# ---------------------------------------------------------------------------
# 1. ZERO PREFIX SHARING. The trace uses --shared-prefix-frac 0, which is what
#    Phase 0 needed to make the KV limit bind. cache_aware's whole mechanism is
#    prefix reuse, so it has nothing to exploit here. This is a fair test of
#    "which router keeps backends inside their KV budget" and NOT a fair test of
#    cache-aware routing. Phase 4 is where that gets tested properly.
#
# 2. EXTRA NETWORK HOP. Our arms are loadgen -> backend. The sgl-router arms are
#    loadgen -> sgl-router -> backend. That extra hop adds latency that has
#    nothing to do with routing quality, so TTFT is structurally biased against
#    sgl-router. Compare err%, not latency.
#
# 3. SPREAD IS UNMEASURABLE IN PROXY MODE. The generator only ever sees the one
#    router URL, so every row shares a backend value and compare.py's spread
#    columns collapse to a meaningless 0.0%. The _before/_after JSON snapshots
#    hold the real per-backend distribution; diff them by hand until A5 wires
#    them into compare.py.
#
# REWRITTEN 2026-08-15 to match the rigor of the policy comparison (see
# ../journey/phase-0.md section 9):
#   - REPEATS (default 3). The previous version ran each arm ONCE and invited a
#     comparison against D1's 3-run averages. pressure at rate 10 spanned
#     0.8-1.1% across its repeats, so a single run cannot distinguish a real
#     difference from noise. This was the most serious defect here.
#   - kills any stale router from a previous session before starting, so a
#     leftover process holding port 9000/29000 cannot silently kill the new one
#   - kills the previous router BEFORE restarting the sims, so a live router is
#     never left health-checking containers being destroyed under it
#   - polls for router readiness instead of a fixed sleep 25
#   - cold container restart before EVERY arm and every repeat, ours included
#   - versioned trace names, --kv-model prompt_only, --seed, --max-num-seqs 64,
#     all matching D1 exactly
#   - default rate 10, which D1 already has clean 3-run numbers for

set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

R="${1:-10}"
TRACE="tr_v2_r${R}.json"
OURS="http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004"
PY=./venv/bin/python
MAX_NUM_SEQS=64
REPEATS="${REPEATS:-3}"
N=1500

[ -f "$TRACE" ] || $PY generate_trace.py --kind lognormal --num-requests $N --rate "$R" --out "$TRACE"
mkdir -p results_compet

CUR_PID=""

kill_router() {
  # Kill whatever router this script started, if any.
  if [ -n "$CUR_PID" ]; then
    kill "$CUR_PID" 2>/dev/null || true
    CUR_PID=""
    sleep 2
  fi
}

cleanup() {
  kill_router
}
trap cleanup EXIT

# Stale routers from an earlier session hold ports 9000/9001 and their Prometheus
# ports 29000/29001. A new instance then panics on bind, and the panic is invisible
# to anything that only checks whether the main port answers. This is exactly how
# the 2026-08-11 run lost its power_of_two arm.
for f in sglrouter_*.pid; do
  [ -f "$f" ] || continue
  OLD=$(cat "$f" 2>/dev/null || true)
  if [ -n "$OLD" ] && kill -0 "$OLD" 2>/dev/null; then
    echo "killing stale router pid=$OLD from $f"
    kill "$OLD" 2>/dev/null || true
  fi
  rm -f "$f"
done
sleep 2

echo "############################################"
echo "### our arms (router picks the backend)"
echo "############################################"
for P in least_conn pressure; do
  for K in $(seq 1 "$REPEATS"); do
    echo ""
    echo "--- ours:$P  run=$K/$REPEATS ---"
    ./restart_sims.sh "$MAX_NUM_SEQS"
    $PY loadgen.py --trace "$TRACE" --backends "$OURS" \
        --out "results_compet/r${R}_ours-${P}__run${K}.csv" \
        --policy "$P" --theta 0.55 --sigma 0.95 --max-num-seqs "$MAX_NUM_SEQS" \
        --kv-model prompt_only --seed "$K"
  done
done

echo ""
echo "############################################"
echo "### sgl-router arms (IT picks the backend)"
echo "############################################"
for SP in cache_aware power_of_two; do
  PORT=9000
  [ "$SP" = "power_of_two" ] && PORT=9001

  for K in $(seq 1 "$REPEATS"); do
    echo ""
    echo "--- sglrouter:$SP  run=$K/$REPEATS ---"

    # Order matters: stop the old router FIRST, then rebuild the backends, then
    # start a fresh router against backends that are already up. Restarting the
    # sims underneath a live router makes it mark every worker unhealthy.
    kill_router
    ./restart_sims.sh "$MAX_NUM_SEQS"

    ./start_competitor_router.sh "$SP" "$PORT" >/dev/null
    CUR_PID=$(cat "sglrouter_${SP}.pid")

    ready=0
    for _ in $(seq 1 60); do
      if curl -s --max-time 2 -o /dev/null "http://127.0.0.1:${PORT}/v1/models"; then
        ready=1
        break
      fi
      sleep 1
    done
    if [ "$ready" -ne 1 ]; then
      echo "    router on $PORT did not come up -- see sglrouter_${SP}.log -- SKIPPING"
      tail -5 "sglrouter_${SP}.log" 2>/dev/null || true
      continue
    fi
    sleep 3   # accept connections != ready to route

    $PY scrape_backend_counts.py "$OURS" \
        --out "results_compet/r${R}_sglrouter-${SP}__run${K}_before.json"
    $PY loadgen.py --trace "$TRACE" --backends "http://127.0.0.1:${PORT}" \
        --out "results_compet/r${R}_sglrouter-${SP}__run${K}.csv" \
        --policy proxy --max-num-seqs "$MAX_NUM_SEQS" --seed "$K"
    $PY scrape_backend_counts.py "$OURS" \
        --out "results_compet/r${R}_sglrouter-${SP}__run${K}_after.json"
  done
done

kill_router

echo ""
echo "============================================"
echo "STAGE 6 COMPLETE  (rate=$R, $REPEATS repeats per arm)"
echo "  Read err% ONLY. Not latency: the sgl-router arms carry an extra"
echo "  network hop that has nothing to do with routing quality."
echo "  Spread columns are meaningless for sglrouter rows by construction;"
echo "  diff the _before/_after JSON snapshots for the real distribution."
echo "  Caveat to report: zero prefix sharing, so cache_aware has nothing"
echo "  to exploit. Phase 4 tests that."
echo "============================================"
$PY compare.py results_compet/*.csv
echo ""
echo "Verifying measurement integrity..."
$PY verify.py results_compet || true
