#!/bin/bash
# STAGE 4 - tune theta at the knee.
#
# Usage:  ./stage4_theta.sh <KNEE_RATE>
#   e.g.  ./stage4_theta.sh 6
#
# theta is where the convex penalty engages. Too high and long generations stack
# up before anything resists them:
#
#   3 long requests : u = max(3/8, 4860/8192) = 0.593  -> no penalty at theta=0.70
#   5 short requests: u = max(5/8,  600/8192) = 0.625  -> scores worse
#   => pressure picks the 3-long backend. That is the rate-3 regression.
#
# Lowering theta makes the penalty engage before the pile-up forms.
# least_conn is included as the fixed reference line.

set -e
if [ -z "$1" ]; then
  echo "usage: ./stage4_theta.sh <KNEE_RATE>   (e.g. ./stage4_theta.sh 6)"
  exit 1
fi

R="$1"
BACKENDS="http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004"
N=1500
THETAS="0.35 0.45 0.55 0.70"
TRACE="tr_lognorm_r${R}.json"

if [ ! -f "$TRACE" ]; then
  python3 generate_trace.py --kind lognormal --num-requests $N --rate "$R" --out "$TRACE"
fi

mkdir -p results_theta

echo ""
echo "### reference: least_conn at rate=$R"
python3 loadgen.py --trace "$TRACE" --backends "$BACKENDS" \
    --out "results_theta/r${R}_least_conn.csv" --policy least_conn --max-num-seqs 32

for T in $THETAS; do
  TAG=$(echo "$T" | tr -d '.')
  echo ""
  echo "############################################"
  echo "### THETA SWEEP  rate=$R  theta=$T"
  echo "############################################"
  python3 loadgen.py --trace "$TRACE" --backends "$BACKENDS" \
      --out "results_theta/r${R}_pressure-t${TAG}.csv" --policy pressure --theta "$T" --max-num-seqs 32
done

echo ""
echo "============================================"
echo "STAGE 4 COMPLETE - pick the theta with the best p99"
echo "  (and confirm p95 moves the same direction)"
echo "============================================"
python3 compare.py results_theta/*.csv
