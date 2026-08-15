"""Measurement-integrity checks for Phase 0 result sets.

Answers one question: can the numbers in a results directory be believed?

This exists because Phase 0 has repeatedly produced confident, wrong numbers.
Every check below corresponds to a bug that actually happened and silently
corrupted results for multiple runs before anyone noticed:

  - KV rejections arrived as HTTP 200 and were counted as zero-token SUCCESSES,
    so err% read 0.0% while 61% of requests were failing.
  - A stale CSV from an older, incompatible run sat in the results directory and
    was picked up by a `*.csv` glob, adding three fictional rows to a table.
  - The router's KV projection was wrong by 2x in one direction, then by 2.02x in
    the other, and both times the run "completed successfully".

None of those announce themselves. A run that has lost a third of its requests
prints the same reassuring "Done." as a clean one. So these checks assert the
invariants directly.

Usage:
    ./venv/bin/python verify.py results_compare
    ./venv/bin/python verify.py results_knee --log results_knee/sweep_log.txt
    ./venv/bin/python verify.py results_compare --echo-mode

Exit code is 1 if any check FAILS, so it can gate a pipeline.
"""

import argparse
import collections
import csv
import glob
import os
import re
import sys

from compare import summarize

PASS, FAIL, WARN, SKIP = "PASS", "FAIL", "WARN", "SKIP"

# Thresholds. Deliberately explicit rather than buried in the checks.
MAX_NON_KV_ERROR_FRAC = 0.05   # above this, err% is not "KV exhaustion rate"
MAX_ERROR_E2E_S = 5.0          # a KV reject returns instantly; slow = timeout
MAX_LAG_FRAC = 0.005           # coordinated omission
MAX_TRIM_SHIFT_PP = 2.0        # trim should not change the story
MAX_LEN_MISMATCH_FRAC = 0.01   # echo mode: output should equal prompt


def load_runs(paths):
    runs = []
    for p in sorted(paths):
        try:
            rows = list(csv.DictReader(open(p)))
        except Exception as e:
            runs.append({"path": p, "rows": None, "error": str(e)})
            continue
        runs.append({"path": p, "rows": rows, "error": None})
    return runs


# ---------------------------------------------------------------------------
# Checks. Each returns (status, headline, [detail lines]).
# ---------------------------------------------------------------------------

def check_readable(runs):
    bad = [r for r in runs if r["rows"] is None]
    if bad:
        return FAIL, f"{len(bad)} file(s) could not be parsed", \
            [f"{r['path']}: {r['error']}" for r in bad]
    if not runs:
        return FAIL, "no CSV files found", \
            ["Check the directory path. An empty glob silently produces an empty",
             "report that looks like a clean pass."]
    return PASS, f"{len(runs)} result file(s) parsed", []


def check_conservation(runs):
    """Every dispatched request must appear exactly once.

    req_ids are assigned 0..N-1 at dispatch. If the set is not contiguous with no
    duplicates, requests were lost between dispatch and the results file -- and a
    lost request is counted as neither a success nor an error, so it vanishes from
    err% entirely rather than showing up as a failure.
    """
    detail, bad = [], False
    for r in runs:
        if r["rows"] is None:
            continue
        ids = [int(x["req_id"]) for x in r["rows"]]
        n, uniq = len(ids), len(set(ids))
        expected = max(ids) + 1 if ids else 0
        if n != uniq or n != expected:
            bad = True
            detail.append(f"{os.path.basename(r['path'])}: rows={n} unique={uniq} "
                          f"expected={expected}  <- MISSING {expected - uniq} request(s)")
    if bad:
        return FAIL, "requests missing from results", detail + [
            "", "A dropped request is invisible in err%: it is neither ok nor error.",
            "Suspect an exception escaping send_request, or the run being killed."]
    return PASS, "all requests accounted for (contiguous ids, no duplicates)", []


def check_silent_failures(runs):
    """A row marked successful with zero tokens is a failure counted as a win.

    This is the exact bug that made err% read 0.0% when the true rate was 61%: the
    simulator returns KV-exhaustion as HTTP 200 with an error object inside the SSE
    stream, and the parser treated it as a successful empty completion.
    """
    detail, total = [], 0
    for r in runs:
        if r["rows"] is None:
            continue
        silent = [x for x in r["rows"]
                  if not x["error"] and int(x["actual_tokens"]) == 0]
        if silent:
            total += len(silent)
            detail.append(f"{os.path.basename(r['path'])}: {len(silent)} zero-token "
                          f"'successes'")
    if total:
        return FAIL, f"{total} silent failure(s): success with 0 tokens", detail + [
            "", "These inflate the success count and deflate err%.",
            "Check the SSE error-detection branch in loadgen.send_request."]
    return PASS, "no zero-token successes", []


def check_error_taxonomy(runs):
    """err% is reported as 'KV exhaustion rate'. Verify that is what it measures.

    send_request catches ANY exception into `err` -- connection refused, read
    timeout, malformed response. Those are real failures but they are not KV
    exhaustion, and folding them together turns the headline metric into something
    broader than the claim it supports.
    """
    kinds = collections.Counter()
    for r in runs:
        if r["rows"] is None:
            continue
        for x in r["rows"]:
            if x["error"]:
                kinds[x["error"][:80]] += 1
    total = sum(kinds.values())
    if total == 0:
        return PASS, "no errors recorded", []

    kv = sum(v for k, v in kinds.items() if "kv cache" in k.lower())
    other = total - kv
    frac = other / total
    detail = [f"{v:6d}  {k}" for k, v in kinds.most_common(8)]
    head = f"{total} errors: {kv} KV-exhaustion, {other} other ({frac:.1%})"
    if frac > MAX_NON_KV_ERROR_FRAC:
        return WARN, head, detail + [
            "", f"More than {MAX_NON_KV_ERROR_FRAC:.0%} of errors are NOT KV exhaustion.",
            "Do not describe err% as 'KV exhaustion rate' without splitting these out.",
            "Connection errors often mean a backend was still starting up."]
    return PASS, head, detail


def check_error_latency(runs):
    """KV rejections return immediately. Slow errors are a different failure mode."""
    detail, bad = [], False
    for r in runs:
        if r["rows"] is None:
            continue
        e = sorted(float(x["e2e_s"]) for x in r["rows"] if x["error"])
        if not e:
            continue
        if e[-1] > MAX_ERROR_E2E_S:
            bad = True
            detail.append(f"{os.path.basename(r['path'])}: max error e2e={e[-1]:.1f}s "
                          f"(p50={e[len(e)//2]:.3f}s)")
    if bad:
        return WARN, f"some errors took over {MAX_ERROR_E2E_S}s", detail + [
            "", "A KV reject returns in ~0.0s. Slow errors are timeouts or dropped",
            "connections being counted alongside capacity rejections."]
    return PASS, "all errors returned promptly (consistent with KV rejection)", []


def check_backend_coverage(runs):
    """Every backend should receive traffic. A missing one means a broken URL."""
    detail, bad = [], False
    for r in runs:
        if r["rows"] is None:
            continue
        name = os.path.basename(r["path"])
        counts = collections.Counter(x["backend"] for x in r["rows"])
        # proxy runs legitimately see a single URL: the external router.
        if "proxy" in name or "sglrouter" in name:
            continue
        if len(counts) < 4:
            bad = True
            detail.append(f"{name}: only {len(counts)} backend(s) used: "
                          f"{[b[-4:] for b in counts]}")
    if bad:
        return FAIL, "some backends received no traffic", detail + [
            "", "A backend absent from the results was never routed to at all.",
            "Check the --backends list and that all four containers were up."]
    return PASS, "all four backends received traffic in every non-proxy run", []


def check_length_invariant(runs, echo_mode):
    """In echo mode the backend replays the prompt, so output must equal prompt."""
    if not echo_mode:
        return SKIP, "not echo mode; prompt/output decoupling is expected", []
    detail, bad, checked = [], False, 0
    for r in runs:
        if r["rows"] is None:
            continue
        rows = [x for x in r["rows"] if not x["error"]]
        if not rows or "prompt_tokens" not in rows[0]:
            continue
        checked += 1
        mism = [x for x in rows
                if int(x["actual_tokens"]) != int(x["prompt_tokens"])]
        if rows and len(mism) / len(rows) > MAX_LEN_MISMATCH_FRAC:
            bad = True
            detail.append(f"{os.path.basename(r['path'])}: "
                          f"{len(mism)}/{len(rows)} rows where output != prompt")
    if checked == 0:
        return SKIP, "no prompt_tokens column (CSVs predate 2026-08-15)", [
            "Re-run to get this check; it verifies the backend behaves as assumed."]
    if bad:
        return WARN, "output length does not match prompt length", detail + [
            "", "Echo mode should replay the prompt exactly. A mismatch means the",
            "backend is truncating, or --output-model/--kv-model no longer match",
            "reality, which invalidates the KV projection."]
    return PASS, "output == prompt in every run (echo mode behaving as assumed)", []


def check_trim_sensitivity(runs):
    """The warmup/drain trim must not be what creates the result."""
    detail, bad = [], False
    for r in runs:
        if r["rows"] is None:
            continue
        try:
            a = summarize(r["path"], 0.0)
            b = summarize(r["path"], 0.20)
        except Exception as e:
            detail.append(f"{os.path.basename(r['path'])}: could not summarize: {e}")
            continue
        shift = abs(a["err_pct"] - b["err_pct"])
        if shift > MAX_TRIM_SHIFT_PP:
            bad = True
            detail.append(f"{os.path.basename(r['path'])}: err% "
                          f"{a['err_pct']:.1f}% untrimmed vs {b['err_pct']:.1f}% "
                          f"trimmed ({shift:.1f}pp shift)")
    if bad:
        return WARN, "trimming materially changes err%", detail + [
            "", "The 20% warmup/drain trim is defensible, but if it moves the",
            "headline number by more than a couple of points it is doing real work",
            "and must be disclosed rather than assumed."]
    return PASS, "err% stable with and without warmup/drain trim", []


def check_kv_leak(log_path):
    """Structural invariant: in_flight == 0 implies kv_proj == 0.

    Every request decrements both in its `finally` block. If occupancy is nonzero
    while nothing is in flight, the router permanently believes memory is held that
    is not, and will under-admit for the rest of the run.
    """
    if not log_path or not os.path.exists(log_path):
        return SKIP, "no run log supplied (--log)", [
            "This is the strongest structural check available. Capture a log with",
            "`./stage5_compare.sh 2>&1 | tee results_compare/compare_log.txt`."]
    txt = open(log_path, errors="replace").read()
    leaks = []
    for m in re.finditer(r"inflight\[([^\]]*)\] \| occupancy\[([^\]]*)\]", txt):
        try:
            inf = dict(x.split("=") for x in m.group(1).split())
            occ = dict(x.split("=") for x in m.group(2).split())
        except ValueError:
            continue
        for b in inf:
            if b in occ and int(inf[b]) == 0 and float(occ[b]) > 0.01:
                leaks.append(f"backend {b}: in_flight=0 but occupancy={occ[b]}")
    if leaks:
        uniq = sorted(set(leaks))
        return FAIL, f"{len(leaks)} KV accounting leak(s)", uniq[:10] + [
            "", "kv_proj was not fully released. Suspect an early return or an",
            "exception path in send_request that skips the finally block."]
    return PASS, "no KV accounting leaks (in_flight=0 always implies occupancy=0)", []


def check_coordinated_omission(log_path):
    """lag_events > 0 means arrivals were late, so every latency number is optimistic."""
    if not log_path or not os.path.exists(log_path):
        return SKIP, "no run log supplied (--log)", []
    txt = open(log_path, errors="replace").read()
    hits = re.findall(r"lag_events=(\d+), saturated_dispatches=(\d+)", txt)
    if not hits:
        return SKIP, "no lag_events lines found in log", []
    worst = max(int(a) for a, _ in hits)
    sat = max(int(b) for _, b in hits)
    # Each run prints this counter twice (at "all dispatched" and at "Done."), so
    # the number of matches is not the number of runs.
    detail = [f"{len(hits)} lag report(s); worst lag_events={worst}, "
              f"max saturated_dispatches={sat}"]
    # 1500 requests per run is the standing default here.
    if worst > 1500 * MAX_LAG_FRAC:
        return FAIL, f"coordinated omission: {worst} late dispatches", detail + [
            "", "The generator fell behind its own arrival schedule, so requests were",
            "sent later than the trace specifies. Latency is understated and the",
            "offered load never actually reached the target rate.",
            "Reduce the rate, or the generator itself is the bottleneck."]
    return PASS, f"open-loop schedule held (worst lag_events={worst})", detail


# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("results", help="directory or glob of result CSVs")
    ap.add_argument("--log", default=None,
                    help="run log (enables KV-leak and coordinated-omission checks)")
    ap.add_argument("--echo-mode", action="store_true", default=True,
                    help="assert output == prompt (llm-d-inference-sim --mode echo)")
    ap.add_argument("--no-echo-mode", dest="echo_mode", action="store_false")
    a = ap.parse_args()

    if os.path.isdir(a.results):
        paths = glob.glob(os.path.join(a.results, "*.csv"))
        if a.log is None:
            # Match any *log* file rather than exact names: these get created by
            # hand-typed `tee` commands and turn up as sweep_log.t, run.log, etc.
            cands = [p for p in glob.glob(os.path.join(a.results, "*log*"))
                     if not p.endswith(".csv")]
            if cands:
                a.log = max(cands, key=os.path.getmtime)
    else:
        paths = glob.glob(a.results)

    runs = load_runs(paths)

    checks = [
        ("files readable", lambda: check_readable(runs)),
        ("request conservation", lambda: check_conservation(runs)),
        ("silent failures", lambda: check_silent_failures(runs)),
        ("error taxonomy", lambda: check_error_taxonomy(runs)),
        ("error latency", lambda: check_error_latency(runs)),
        ("backend coverage", lambda: check_backend_coverage(runs)),
        ("length invariant", lambda: check_length_invariant(runs, a.echo_mode)),
        ("trim sensitivity", lambda: check_trim_sensitivity(runs)),
        ("KV accounting leak", lambda: check_kv_leak(a.log)),
        ("coordinated omission", lambda: check_coordinated_omission(a.log)),
    ]

    print()
    print(f"MEASUREMENT INTEGRITY  --  {a.results}")
    if a.log:
        print(f"log: {a.log}")
    print("=" * 78)

    tally = collections.Counter()
    for name, fn in checks:
        try:
            status, head, detail = fn()
        except Exception as e:
            status, head, detail = FAIL, f"check crashed: {e}", []
        tally[status] += 1
        print(f"\n[{status}] {name}")
        print(f"       {head}")
        for line in detail:
            print(f"       {line}" if line else "")

    print()
    print("=" * 78)
    print(f"{tally[PASS]} passed, {tally[WARN]} warnings, "
          f"{tally[FAIL]} failed, {tally[SKIP]} skipped")
    if tally[FAIL]:
        print()
        print("FAIL means a result set is not trustworthy. Fix the cause and re-run")
        print("the measurements; do not reason about numbers that failed these checks.")
    elif tally[WARN]:
        print()
        print("No hard failures. Warnings are things to disclose when reporting,")
        print("not necessarily things to fix.")
    print()
    return 1 if tally[FAIL] else 0


if __name__ == "__main__":
    sys.exit(main())
