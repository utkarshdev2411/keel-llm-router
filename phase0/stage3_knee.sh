#!/bin/bash
# STAGE 3 - find the knee.
#
# least_conn only, small runs, sweep the arrival rate. The knee is where p99
# first climbs sharply. Below it no policy can win; far above it none can help.
# Everything downstream (theta tuning, the final comparison) happens AT the knee,
# so this must run before them.

set -e
BACKENDS="http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004"
N=1500
RATES="8 12 16 20 24"

mkdir -p results_knee

for R in $RATES; do
  TRACE="tr_lognorm_r${R}.json"
  if [ ! -f "$TRACE" ]; then
    echo ""
    echo "### generating trace rate=$R"
    python3 generate_trace.py --kind lognormal --num-requests $N --rate "$R" --out "$TRACE"
  fi
done

for R in $RATES; do
  echo ""
  echo "############################################"
  echo "### KNEE SWEEP  rate=$R  policy=least_conn"
  echo "############################################"
  python3 loadgen.py --trace "tr_lognorm_r${R}.json" --backends "$BACKENDS" \
      --out "results_knee/r${R}_least_conn.csv" --policy least_conn --max-num-seqs 32
done

echo ""
echo "============================================"
echo "STAGE 3 COMPLETE - find where p99 jumps"
echo "============================================"
python3 compare.py results_knee/*.csv
