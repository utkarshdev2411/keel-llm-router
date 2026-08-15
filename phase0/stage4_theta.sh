#!/bin/bash
# STAGE 4 - tune the pressure policy's two knobs at a fixed arrival rate.
#
#   theta  the occupancy at which the convex penalty starts biting
#   sigma  the admission ceiling, as a fraction of KV capacity
#
# least_conn runs first as a fixed reference line, so every pressure variant can be
# read against the same baseline on the same traffic.
#
# WHAT THIS FOUND, so you know what to expect: theta has no measurable effect across
# 0.35 to 0.70. The scoring function is flat-ish below the knee and the admission gate
# does the real work, so moving the knee mostly does not change which backend gets
# picked. sigma matters more, and 0.95 was the best of those tried, though the margin
# was inside the noise of single runs. The defaults are theta 0.55, sigma 0.95.
#
# Do not expect a dramatic result here. The purpose is to confirm the policy is not
# sitting on a cliff edge in parameter space, which would make the headline comparison
# a lucky draw rather than a property of the design.
#
# REWRITTEN 2026-08-15: versioned trace names so pre-correction traces cannot be
# reused, ./venv/bin/python, --max-num-seqs 64, --kv-model, --seed, cold container
# restarts between arms, and repeats. Also absorbed the sigma sweep from the old
# stage4_tune.sh, which was a near-duplicate of this file and has been removed.

set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

R="${1:-10}"
BACKENDS="http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004"
PY=./venv/bin/python
N=1500
TRACE="tr_v2_r${R}.json"
MAX_NUM_SEQS=64
REPEATS="${REPEATS:-2}"
THETAS="${THETAS:-0.40 0.55 0.70}"
SIGMAS="${SIGMAS:-0.85 0.90 0.95}"

mkdir -p results_theta

[ -f "$TRACE" ] || $PY generate_trace.py --kind lognormal --num-requests $N --rate "$R" --out "$TRACE"

echo "############################################"
echo "### reference: least_conn at rate=$R"
echo "############################################"
for K in $(seq 1 "$REPEATS"); do
  echo ""
  echo "--- least_conn  run=$K/$REPEATS ---"
  ./restart_sims.sh "$MAX_NUM_SEQS"
  $PY loadgen.py --trace "$TRACE" --backends "$BACKENDS" \
      --out "results_theta/r${R}_least_conn__run${K}.csv" \
      --policy least_conn --max-num-seqs "$MAX_NUM_SEQS" \
      --kv-model prompt_only --seed "$K"
done

echo ""
echo "############################################"
echo "### theta sweep (sigma fixed at 0.95)"
echo "############################################"
for T in $THETAS; do
  TAG=$(echo "$T" | tr -d '.')
  for K in $(seq 1 "$REPEATS"); do
    echo ""
    echo "--- pressure theta=$T  run=$K/$REPEATS ---"
    ./restart_sims.sh "$MAX_NUM_SEQS"
    $PY loadgen.py --trace "$TRACE" --backends "$BACKENDS" \
        --out "results_theta/r${R}_pressure-t${TAG}__run${K}.csv" \
        --policy pressure --theta "$T" --sigma 0.95 \
        --max-num-seqs "$MAX_NUM_SEQS" --kv-model prompt_only --seed "$K"
  done
done

echo ""
echo "############################################"
echo "### sigma sweep (theta fixed at 0.55)"
echo "############################################"
for S in $SIGMAS; do
  TAG=$(echo "$S" | tr -d '.')
  for K in $(seq 1 "$REPEATS"); do
    echo ""
    echo "--- pressure sigma=$S  run=$K/$REPEATS ---"
    ./restart_sims.sh "$MAX_NUM_SEQS"
    $PY loadgen.py --trace "$TRACE" --backends "$BACKENDS" \
        --out "results_theta/r${R}_pressure-s${TAG}__run${K}.csv" \
        --policy pressure --theta 0.55 --sigma "$S" \
        --max-num-seqs "$MAX_NUM_SEQS" --kv-model prompt_only --seed "$K"
  done
done

echo ""
echo "============================================"
echo "STAGE 4 COMPLETE (rate=$R, $REPEATS repeats per cell)"
echo "  Compare each variant against the least_conn reference rows."
echo "  Differences smaller than the spread BETWEEN repeats of the same"
echo "  setting are noise, not tuning signal."
echo "============================================"
$PY compare.py results_theta/*.csv
echo ""
$PY verify.py results_theta || true
