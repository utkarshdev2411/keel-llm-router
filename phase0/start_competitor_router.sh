#!/bin/bash
# Launch sgl-router in front of the four simulated backends, so a real
# competitor router can be benchmarked on the identical trace.
#   $1 = policy (cache_aware | power_of_two | round_robin | random)
#   $2 = port   (default 9000)
POLICY="${1:-cache_aware}"
PORT="${2:-9000}"
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"
setsid ./venv/bin/python -m sglang_router.launch_router \
  --host 127.0.0.1 --port "$PORT" \
  --worker-urls http://localhost:8001 http://localhost:8002 http://localhost:8003 http://localhost:8004 \
  --policy "$POLICY" --log-level warn \
  > "sglrouter_${POLICY}.log" 2>&1 < /dev/null &
echo "launched policy=$POLICY port=$PORT pid=$!"
