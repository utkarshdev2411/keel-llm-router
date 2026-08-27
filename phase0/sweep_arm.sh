#!/usr/bin/env bash
# Run one arm of the Phase 3B sweep across arrival rates.
# Cold-restarts both backends before EVERY cell, per the benchmark rules.
# usage: ./sweep_arm.sh <router_config> <label> <rate> [rate ...]
set -euo pipefail
ROOT=/home/utkarsh/Development/keel-llm-router
CONFIG="$1"; LABEL="$2"; shift 2

snap() { for p in 8000 8001; do
  curl -s "http://localhost:$p/metrics" | awk '/^vllm:num_preemptions_total/{printf "%d ", $2}'
done; }

mkdir -p "$ROOT/phase0/results_3b"
for rate in "$@"; do
  cd "$ROOT"
  PID=$(pgrep -x router || true); if [ -n "$PID" ]; then kill "$PID"; sleep 1; fi
  "$ROOT/phase0/restart_real.sh" 512 32 >/dev/null 2>&1
  setsid nohup "$ROOT/target/release/router" "$CONFIG" \
      > "/tmp/router_${LABEL}_r${rate}.log" 2>&1 < /dev/null &
  sleep 4
  if ! grep -q listening "/tmp/router_${LABEL}_r${rate}.log"; then
    echo "ROUTER FAILED for $LABEL r$rate"; cat "/tmp/router_${LABEL}_r${rate}.log"; exit 1
  fi

  cd "$ROOT/phase0"
  before=$(snap)
  python3 loadgen.py --trace "tr_sweep_r${rate}.json" --backends http://127.0.0.1:8080 \
    --out "results_3b/${LABEL}_r${rate}.csv" --policy proxy \
    --output-model max_tokens --kv-model prompt_plus_output \
    --max-num-seqs 32 --kv-capacity 8192 --sigma 0.95 --theta 0.55 --penalty 10 \
    --seed 7 > "/tmp/loadgen_${LABEL}_r${rate}.log" 2>&1
  after=$(snap)

  errs=$(python3 -c "
import csv;rows=list(csv.DictReader(open('results_3b/${LABEL}_r${rate}.csv')))
print(sum(1 for r in rows if r.get('error')), len(rows))")
  sat=$(curl -s http://127.0.0.1:9090/metrics | awk '/^router_saturated_dispatches_total/{print $2}')
  b=($before); a=($after)
  d0=$(python3 -c "print(int(${a[0]})-int(${b[0]}))")
  d1=$(python3 -c "print(int(${a[1]})-int(${b[1]}))")
  echo "RESULT ${LABEL} rate=${rate}  preempt_delta=$((d0+d1))  (b8000=+${d0} b8001=+${d1})  errors=${errs}  saturated=${sat:-0}"
done

PID=$(pgrep -x router || true); if [ -n "$PID" ]; then kill "$PID"; fi; true
