#!/bin/bash
# STAGE 5 - policy comparison at the rates that matter.
#
# METRIC NOTE: this simulator SHEDS rather than queues, so TTFT stays flat
# (~85-106ms) regardless of load and is useless as a discriminator here.
# err% IS the metric. A better router = fewer KV rejections.
#
# Mechanism under test (measured at rate 8, least_conn):
#   OUTPUT-token spread : 1.9%   <- least_conn balances this fine
#   PROMPT-token spread : 25.5%  <- but KV is allocated by PROMPT, and errors
#                                   correlate monotonically with it
# pressure tracks kv_proj = sum(prompt tokens), so it should close that gap.

set -e
BACKENDS="http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004"
N=1500
RATES="${RATES:-4 6 8}"
POLICIES="least_conn kvts pressure"

mkdir -p results_compare

for R in $RATES; do
  TRACE="tr_lognorm_r${R}.json"
  [ -f "$TRACE" ] || python3 generate_trace.py --kind lognormal --num-requests $N --rate "$R" --out "$TRACE"
done

for R in $RATES; do
  for P in $POLICIES; do
    echo ""
    echo "############################################"
    echo "### rate=$R  policy=$P"
    echo "############################################"
    python3 loadgen.py --trace "tr_lognorm_r${R}.json" --backends "$BACKENDS" \
        --out "results_compare/r${R}_${P}.csv" --policy "$P" --max-num-seqs 32
  done
done

echo ""
echo "============================================"
echo "STAGE 5 COMPLETE - compare err% at each rate"
echo "============================================"
python3 compare.py results_compare/*.csv
