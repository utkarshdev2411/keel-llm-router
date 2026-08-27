#!/usr/bin/env bash
# Cold-restart two real vLLM backends with PINNED, IDENTICAL KV capacity.
#
# Why pin: vLLM sizes num_gpu_blocks by profiling free VRAM at startup, which is
# not deterministic. Measured on a GTX 1650: the same flags produced 232 blocks
# on one boot and 1245 on the next, and a second instance launched alongside the
# first profiled 232 against the first's 1245. Capacity is the DENOMINATOR of
# every occupancy fraction, so drift between arms makes them incomparable and
# silently rescales sigma -- the same class of bug as the Phase 2 token-count
# error. --num-gpu-blocks-override removes the profiling step entirely.
#
# Usage: ./restart_real.sh [BLOCKS] [MAX_NUM_SEQS]
set -euo pipefail

BLOCKS="${1:-512}"          # 512 blocks * 16 tokens = 8192 tokens per backend
MAX_SEQS="${2:-32}"
IMAGE="vllm/vllm-openai:v0.26.0-x86_64-cu129-ubuntu2404"
MODEL="Qwen/Qwen2.5-0.5B-Instruct"

for n in 1 2; do
  port=$((7999 + n))
  docker rm -f "keel-vllm-$n" >/dev/null 2>&1 || true
done
sleep 2

for n in 1 2; do
  port=$((7999 + n))
  docker run -d --name "keel-vllm-$n" --gpus all \
    -p "${port}:${port}" --ipc=host \
    -v hf-cache:/root/.cache/huggingface \
    -v vllm-compile-cache:/root/.cache/vllm \
    "$IMAGE" \
    --model "$MODEL" --served-model-name test-model \
    --host 0.0.0.0 --port "$port" \
    --gpu-memory-utilization 0.35 \
    --no-enable-prefix-caching \
    --num-gpu-blocks-override "$BLOCKS" \
    --max-model-len 2048 \
    --max-num-seqs "$MAX_SEQS" >/dev/null
done

for n in 1 2; do
  port=$((7999 + n))
  printf 'waiting for backend %d (:%d) ' "$n" "$port"
  for _ in $(seq 1 60); do
    if curl -s -m 2 "http://localhost:${port}/health" >/dev/null 2>&1; then
      echo " up"; break
    fi
    printf '.'; sleep 5
  done
done

echo
echo "=== capacity (must be identical across backends) ==="
for n in 1 2; do
  port=$((7999 + n))
  tok=$(curl -s "http://localhost:${port}/metrics" | grep -oE 'kv_cache_size_tokens="[0-9]+"' | cut -d'"' -f2)
  blk=$(curl -s "http://localhost:${port}/metrics" | grep -oE 'num_gpu_blocks="[0-9]+"' | head -1 | cut -d'"' -f2)
  pre=$(curl -s "http://localhost:${port}/metrics" | grep "^vllm:num_preemptions_total" | awk '{print $2}')
  echo "  backend $n (:$port)  kv_tokens=$tok  blocks=$blk  preemptions=$pre"
done
