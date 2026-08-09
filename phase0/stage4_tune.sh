#!/bin/bash
# STAGE 4 - tune the two knobs of the `pressure` policy.
#
#   theta : occupancy at which the convex penalty engages.
#           Too high and long generations stack on one backend before anything
#           resists. Too low and the router over-avoids backends that are fine.
#   sigma : admission ceiling. A backend is ineligible if projected KV would
#           exceed sigma * capacity.
#
# Runs theta first, auto-picks the best by error rate, then sweeps sigma at that
# theta. 8 runs total rather than a 16-cell grid.
#
# METRIC: err% only. This simulator sheds rather than queues, so TTFT is flat
# regardless of load and cannot discriminate between policies.
#
# Usage:  ./stage4_tune.sh [RATE]      default 8

set -e
R="${1:-8}"
N=500                      # smaller than the comparison runs: err% is a
                           # proportion, so it is stable well before 1500
BACKENDS="http://localhost:8001,http://localhost:8002,http://localhost:8003,http://localhost:8004"
TRACE="tr_lognorm_r${R}.json"
THETAS="0.35 0.45 0.55 0.70"
SIGMAS="0.75 0.85 0.90 0.95"

[ -f "$TRACE" ] || python3 generate_trace.py --kind lognormal --num-requests $N --rate "$R" --out "$TRACE"

mkdir -p results_tune
rm -f results_tune/*.csv

echo "############################################"
echo "### baseline reference: least_conn @ rate $R"
echo "############################################"
python3 loadgen.py --trace "$TRACE" --backends "$BACKENDS" \
    --out "results_tune/r${R}_least_conn.csv" --policy least_conn --max-num-seqs 32

echo ""
echo "############################################"
echo "### PHASE A: sweep theta (sigma fixed at 0.90)"
echo "############################################"
for T in $THETAS; do
  TAG=$(echo "$T" | tr -d '.')
  echo ""
  echo "--- theta=$T ---"
  python3 loadgen.py --trace "$TRACE" --backends "$BACKENDS" \
      --out "results_tune/r${R}_pressure-t${TAG}.csv" \
      --policy pressure --theta "$T" --sigma 0.90 --max-num-seqs 32
done

echo ""
python3 compare.py results_tune/*.csv

BEST_T=$(python3 - "$R" <<'PYEOF'
import csv, glob, re, sys
best, best_err = None, 1e9
for f in glob.glob("results_tune/*pressure-t*.csv"):
    rows = list(csv.DictReader(open(f)))
    err = sum(1 for r in rows if r["error"]) / max(1, len(rows))
    t = re.search(r"-t(\d+)\.csv", f).group(1)
    theta = float("0." + t[1:]) if t.startswith("0") else float(t) / 100
    if err < best_err:
        best_err, best = err, theta
print(f"{best:.2f}")
PYEOF
)

echo ""
echo "############################################"
echo "### PHASE B: sweep sigma at theta=$BEST_T"
echo "############################################"
for S in $SIGMAS; do
  TAG=$(echo "$S" | tr -d '.')
  echo ""
  echo "--- sigma=$S (theta=$BEST_T) ---"
  python3 loadgen.py --trace "$TRACE" --backends "$BACKENDS" \
      --out "results_tune/r${R}_pressureS-s${TAG}.csv" \
      --policy pressure --theta "$BEST_T" --sigma "$S" --max-num-seqs 32
done

echo ""
echo "============================================"
echo "STAGE 4 COMPLETE   best theta = $BEST_T"
echo "  Read err% only. Pick the sigma with the lowest err%."
echo "  If two are within ~2 points, prefer the HIGHER sigma:"
echo "  it reserves less headroom and wastes less capacity."
echo "============================================"
python3 compare.py results_tune/*.csv
