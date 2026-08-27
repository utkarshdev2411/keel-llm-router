#!/usr/bin/env bash
# Run ONE cell with live sampling of router internals.
# usage: ./probe.sh <router_config> <label> <trace.json>
set -uo pipefail
ROOT=/home/utkarsh/Development/keel-llm-router
CONFIG="$1"; LABEL="$2"; TRACE="$3"; SEED="${4:-11}"

snap() { for p in 8000 8001; do
  curl -s "http://localhost:$p/metrics" | awk '/^vllm:num_preemptions_total/{printf "%d ", $2}'
done; }

cd "$ROOT"
PID=$(pgrep -x router || true); if [ -n "$PID" ]; then kill "$PID"; sleep 1; fi
"$ROOT/phase0/restart_real.sh" "${BLOCKS:-512}" 32 >/dev/null 2>&1
setsid nohup "$ROOT/target/release/router" "$CONFIG" > "/tmp/router_${LABEL}.log" 2>&1 < /dev/null &
sleep 4
grep -q listening "/tmp/router_${LABEL}.log" || { echo "ROUTER FAILED"; cat "/tmp/router_${LABEL}.log"; exit 1; }

setsid nohup python3 /tmp/diag_sample.py > "/tmp/diag_${LABEL}.csv" 2>&1 < /dev/null &
SAMPLER=$!
sleep 1

cd "$ROOT/phase0"
before=$(snap)
python3 loadgen.py --trace "$TRACE" --backends http://127.0.0.1:8080 \
  --out "results_3b/${LABEL}.csv" --policy proxy \
  --output-model max_tokens --kv-model prompt_plus_output \
  --max-num-seqs 32 --kv-capacity "${KVCAP:-8192}" --sigma 0.95 --theta 0.55 --penalty 10 \
  --seed "$SEED" > "/tmp/loadgen_${LABEL}.log" 2>&1
after=$(snap)
sleep 5
kill "$SAMPLER" 2>/dev/null || true

b=($before); a=($after)
sat=$(curl -s http://127.0.0.1:9090/metrics | awk '/^router_saturated_dispatches_total/{print $2}')
python3 - "$LABEL" "${b[0]}" "${b[1]}" "${a[0]}" "${a[1]}" "${sat:-0}" <<'PY'
import csv,sys
label,b0,b1,a0,a1,sat=sys.argv[1:7]
rows=[r for r in csv.DictReader(open(f"/tmp/diag_{label}.csv")) if r.get("kv_projected_8000")]
def fl(r,k):
    try: return float(r[k])
    except: return 0.0
cap,sig=int(__import__("os").environ.get("KVCAP",8192)),0.95
pk={b:max((fl(r,f"kv_projected_{b}") for r in rows),default=0) for b in("8000","8001")}
inf={b:max((fl(r,f"inflight_{b}") for r in rows),default=0) for b in("8000","8001")}
rep={b:max((fl(r,f"reported_kv_usage_{b}") for r in rows),default=0) for b in("8000","8001")}
tail=rows[-1] if rows else {}
errs=sum(1 for r in csv.DictReader(open(f"results_3b/{label}.csv")) if r.get("error"))
n=sum(1 for _ in csv.DictReader(open(f"results_3b/{label}.csv")))
d=(int(a0)-int(b0))+(int(a1)-int(b1))
print(f"PROBE {label}: preempt_delta={d} (b8000=+{int(a0)-int(b0)} b8001=+{int(a1)-int(b1)}) "
      f"errors={errs}/{n} saturated={sat}")
for b in ("8000","8001"):
    print(f"   b{b}: kv_peak={pk[b]:.0f} ({100*pk[b]/cap:.0f}% cap, sigma_cap={sig*cap:.0f}) "
          f"inflight_peak={inf[b]:.0f} reported_kv_peak={rep[b]:.3f}")
print(f"   drain: kv0={tail.get('kv_projected_8000')} inf0={tail.get('inflight_8000')} "
      f"kv1={tail.get('kv_projected_8001')} inf1={tail.get('inflight_8001')}")
PY
