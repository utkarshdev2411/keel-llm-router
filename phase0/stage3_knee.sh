#!/bin/bash
# STAGE 3 - find the knee.
#
# least_conn only, sweep the arrival rate. The knee is where p99 first climbs
# sharply. Below it no policy can win; far above it none can help. Everything
# downstream (theta tuning, the final comparison) happens AT the knee, so this
# must run before them.
#
# REWRITTEN 2026-08-15 after the KV model was corrected. Four changes:
#
#   1. RATES. The old sweep was 8-24 req/s, chosen when capacity was mis-derived.
#      Measured KV per request is ~250 tokens (the PROMPT only -- the simulator
#      does not grow KV during decode, see kv_curve.py), giving ~32 concurrent
#      per backend, ~131 across four, and saturation near 10-11 req/s. Sweeping
#      4-14 straddles that instead of sitting entirely past it.
#
#   2. MAX_NUM_SEQS is 64, not 32. At 32 the slot limit (32) and the KV limit
#      (~32.8) bind at almost exactly the same point, so the simulator's own slot
#      cap partially protects against KV exhaustion and muddies the very thing
#      being measured. At 64, KV is unambiguously the binding constraint.
#
#   3. TRACE NAMES are versioned (tr_v2_*). The old script skipped regeneration
#      when a file already existed, so it would silently reuse traces built by the
#      pre-fix generator that drew prompt and output independently.
#
#   4. Containers restart COLD between arms (see restart_sims.sh).

set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

BACKENDS="http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004"
PY=./venv/bin/python
N=1500
RATES="4 6 8 10 12 14"
MAX_NUM_SEQS=64

mkdir -p results_knee

for R in $RATES; do
  TRACE="tr_v2_r${R}.json"
  if [ ! -f "$TRACE" ]; then
    echo ""
    echo "### generating trace rate=$R"
    $PY generate_trace.py --kind lognormal --num-requests $N --rate "$R" --out "$TRACE"
  fi
done

for R in $RATES; do
  echo ""
  echo "############################################"
  echo "### KNEE SWEEP  rate=$R  policy=least_conn"
  echo "############################################"
  ./restart_sims.sh "$MAX_NUM_SEQS"
  $PY loadgen.py --trace "tr_v2_r${R}.json" --backends "$BACKENDS" \
      --out "results_knee/r${R}_least_conn.csv" \
      --policy least_conn --max-num-seqs "$MAX_NUM_SEQS" --kv-model prompt_only
done

echo ""
echo "============================================"
echo "STAGE 3 COMPLETE - find where p99 jumps"
echo "  Record the knee rate before running the comparison stages."
echo "============================================"
$PY compare.py results_knee/*.csv
