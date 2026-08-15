#!/bin/bash
# Restart all four simulator backends COLD.
#
# WHY THIS EXISTS (tracker B6 / task A7): arms run sequentially against the same
# backends, so a later arm inherits whatever prefix cache the previous arm warmed.
# `pressure` always ran last and therefore always had the warmest cache. That
# confound points in our favour, so it has to be removed structurally rather than
# argued away. Restarting between every arm does that.
#
# Usage: ./restart_sims.sh [MAX_NUM_SEQS]
set -e

MAX_NUM_SEQS="${1:-64}"

docker rm -f sim1 sim2 sim3 sim4 >/dev/null 2>&1 || true

for i in 1 2 3 4; do
  docker run -d --name "sim$i" -p "800$i:8000" -e POD_IP=127.0.0.1 \
    ghcr.io/llm-d/llm-d-inference-sim:v0.10.2 \
    --model test-model --mode echo --max-model-len 8192 --enable-kvcache \
    --kv-cache-size 512 --block-size 16 --max-num-seqs "$MAX_NUM_SEQS" \
    --time-to-first-token 50ms --inter-token-latency 20ms \
    --time-factor-under-load 2.5 >/dev/null
done

# Poll for readiness instead of sleeping a fixed amount: a fixed sleep either
# wastes time or, worse, lets a run start against a backend that is not up yet
# and record its connection errors as routing failures.
for i in 1 2 3 4; do
  ready=0
  for _ in $(seq 1 40); do
    if curl -s --max-time 2 -o /dev/null "http://localhost:800$i/v1/models"; then
      ready=1
      break
    fi
    sleep 1
  done
  if [ "$ready" -ne 1 ]; then
    echo "sim$i did not come up" >&2
    exit 1
  fi
done

echo "sims 1-4 up cold (max-num-seqs=$MAX_NUM_SEQS)"
