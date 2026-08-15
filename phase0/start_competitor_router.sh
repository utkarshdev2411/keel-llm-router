#!/bin/bash
# Launch sgl-router in front of the four simulated backends, so a real
# competitor router can be benchmarked on the identical trace.
#   $1 = policy (cache_aware | power_of_two | round_robin | random)
#   $2 = port   (default 9000)
#
# sgl-router also starts an internal Prometheus exporter on a FIXED default
# port (29000) regardless of --port. Running two instances without giving
# each a distinct --prometheus-port makes the second one panic with
# "Address already in use", and the panic is silent to anything that only
# checks whether the main port came up. Derive a unique metrics port here.
POLICY="${1:-cache_aware}"
PORT="${2:-9000}"
METRICS_PORT=$((PORT + 20000))   # 9000 -> 29000, 9001 -> 29001, ...
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"
setsid ./venv/bin/python -m sglang_router.launch_router \
  --host 127.0.0.1 --port "$PORT" \
  --prometheus-port "$METRICS_PORT" \
  --worker-urls http://localhost:8001 http://localhost:8002 http://localhost:8003 http://localhost:8004 \
  --policy "$POLICY" --log-level warn \
  > "sglrouter_${POLICY}.log" 2>&1 < /dev/null &
PID=$!
echo "$PID" > "sglrouter_${POLICY}.pid"
echo "launched policy=$POLICY port=$PORT metrics_port=$METRICS_PORT pid=$PID"
