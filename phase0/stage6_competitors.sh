#!/bin/bash
# STAGE 6 - benchmark against a REAL competitor router, not just least-connections.
#
# sgl-router (SGLang's production router, written in Rust) sits in front of the same
# four simulated backends. The load generator drives it through --policy proxy, so
# sgl-router makes the routing decisions and we only measure the outcome.
#
# Identical trace, identical backends, identical measurement code. The only thing
# that differs between arms is who chooses the backend.
#
# sgl-router policies tested:
#   cache_aware   - its flagship: radix-tree prefix matching with a load-balance guard
#   power_of_two  - sample two workers, take the less loaded
#
# NOTE ON FAIRNESS: our trace has zero prefix sharing (--shared-prefix-frac 0),
# which is the regime Phase 0 needed to make the KV limit bind. cache_aware has
# nothing to exploit there, so this is NOT a fair test of its cache routing.
# It IS a fair test of "which router keeps backends inside their KV budget".
# Say so when reporting. A prefix-heavy trace belongs in Phase 4.

set -e
R="${1:-8}"
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

TRACE="tr_lognorm_r${R}.json"
OURS="http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004"
PY=./venv/bin/python

[ -f "$TRACE" ] || $PY generate_trace.py --kind lognormal --num-requests 1500 --rate "$R" --out "$TRACE"
mkdir -p results_compet

echo "############################################"
echo "### our arms (router picks the backend)"
echo "############################################"
for P in least_conn pressure; do
  echo ""
  echo "--- $P ---"
  $PY loadgen.py --trace "$TRACE" --backends "$OURS" \
      --out "results_compet/r${R}_ours-${P}.csv" \
      --policy "$P" --theta 0.55 --sigma 0.95 --max-num-seqs 32
done

echo ""
echo "############################################"
echo "### sgl-router arms (IT picks the backend)"
echo "############################################"
for SP in cache_aware power_of_two; do
  # One router process per policy, on its own port. Kill the PREVIOUS
  # instance by tracked PID before starting the next -- sgl-router also binds
  # a Prometheus exporter on a port derived from --port (see
  # start_competitor_router.sh), so a stale prior instance left running
  # causes the next one to panic on bind and get silently skipped.
  if [ -f "sglrouter_${PREV_SP:-none}.pid" ]; then
    kill "$(cat "sglrouter_${PREV_SP}.pid")" 2>/dev/null || true
    sleep 2
  fi
  PORT=9000
  [ "$SP" = "power_of_two" ] && PORT=9001
  ./start_competitor_router.sh "$SP" "$PORT" >/dev/null
  PREV_SP="$SP"
  echo ""
  echo "--- sglrouter:$SP (warming up) ---"
  sleep 25
  if ! curl -s --max-time 10 -o /dev/null "http://127.0.0.1:${PORT}/v1/models"; then
    echo "    router on $PORT did not come up -- check sglrouter_${SP}.log -- skipping"
    continue
  fi
  $PY scrape_backend_counts.py "$OURS" --out "results_compet/r${R}_sglrouter-${SP}_before.json"
  $PY loadgen.py --trace "$TRACE" --backends "http://127.0.0.1:${PORT}" \
      --out "results_compet/r${R}_sglrouter-${SP}.csv" \
      --policy proxy --max-num-seqs 32
  $PY scrape_backend_counts.py "$OURS" --out "results_compet/r${R}_sglrouter-${SP}_after.json"
done

# final cleanup
if [ -f "sglrouter_${PREV_SP:-none}.pid" ]; then
  kill "$(cat "sglrouter_${PREV_SP}.pid")" 2>/dev/null || true
fi

echo ""
echo "============================================"
echo "STAGE 6 COMPLETE"
echo "  Read err% only. It is the KV-exhaustion rate."
echo "  Caveat to report: zero prefix sharing in this trace, so"
echo "  cache_aware has nothing to exploit. Phase 4 tests that."
echo "============================================"
$PY compare.py results_compet/*.csv
