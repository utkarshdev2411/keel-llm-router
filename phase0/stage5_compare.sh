#!/bin/bash
# STAGE 5 - policy comparison at the rates that matter.
#
# METRIC NOTE: this simulator SHEDS rather than queues, so TTFT stays flat
# regardless of load and is useless as a discriminator here. err% IS the metric,
# with DISP kv spread as the mechanism check. A better router = fewer KV rejections.
#
# Mechanism under test, measured directly at rate 12 under least_conn:
#   inflight  [8001=14   8002=15   8003=14   8004=15  ]   <- counts balanced
#   occupancy [8001=0.99 8002=0.52 8003=0.99 8004=0.98]   <- load is not
# least_conn equalises COUNTS. KV is consumed by PROMPT TOKENS, which vary by
# orders of magnitude. pressure tracks kv_proj = sum(prompt tokens), so it should
# close that gap.
#
# REWRITTEN 2026-08-15:
#   - knee is 8 (measured), so default rates are 8 and 10, not 4/6/8
#   - versioned trace names, so pre-fix traces cannot be silently reused
#   - cold container restart before EVERY arm (kills the cache-warming confound)
#   - fixed --seed, so arms differ only in policy and runs are reproducible
#   - ./venv/bin/python, not bare python3
#   - REPEATS: each (rate, policy) runs $REPEATS times. One run per cell cannot
#     distinguish a real difference from noise, and the knee sweep already threw
#     a non-monotonic result (rate 14 scored better than rate 12).

set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

BACKENDS="http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004"
PY=./venv/bin/python
N=1500
RATES="${RATES:-8 10}"
POLICIES="${POLICIES:-least_conn kvts pressure}"
REPEATS="${REPEATS:-3}"
MAX_NUM_SEQS=64
THETA="${THETA:-0.55}"
SIGMA="${SIGMA:-0.95}"

mkdir -p results_compare

for R in $RATES; do
  TRACE="tr_v2_r${R}.json"
  [ -f "$TRACE" ] || $PY generate_trace.py --kind lognormal \
      --num-requests $N --rate "$R" --out "$TRACE"
done

for R in $RATES; do
  for P in $POLICIES; do
    for K in $(seq 1 "$REPEATS"); do
      echo ""
      echo "############################################"
      echo "### rate=$R  policy=$P  run=$K/$REPEATS"
      echo "############################################"
      ./restart_sims.sh "$MAX_NUM_SEQS"
      # Seed varies per REPEAT but is identical across policies at the same
      # (rate, run), so every policy faces the same tie-breaking draws.
      $PY loadgen.py --trace "tr_v2_r${R}.json" --backends "$BACKENDS" \
          --out "results_compare/r${R}_${P}__run${K}.csv" \
          --policy "$P" --max-num-seqs "$MAX_NUM_SEQS" \
          --kv-model prompt_only --seed "$K" \
          --theta "$THETA" --sigma "$SIGMA"
    done
  done
done

echo ""
echo "============================================"
echo "STAGE 5 COMPLETE"
echo "  Read err% first, then DISP kv spread."
echo "  Each cell ran $REPEATS times -- check the runs agree before believing a win."
echo "============================================"
$PY compare.py results_compare/*.csv
