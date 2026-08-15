# Phase 0 Bug Fix Tracker

**Opened:** 2026-08-11
**Status:** NOT STARTED
**Context:** A review found that the KV projection in `loadgen.py` undercounts by exactly
2.0x. This invalidates every `pressure` result produced so far. Several smaller bugs were
found alongside it. This file tracks the fix through to corrected results.

**Resume rule:** work top to bottom. Do not start a phase until every blocking item above
it is DONE. Update the status box and the task checkboxes as you go, so a fresh session can
pick up from here without re-reading the whole history.

---

## Status at a glance

| Phase | What | Status |
|---|---|---|
| A | Code fixes, no runs | ☐ NOT STARTED |
| B | Verify the fixes | ☐ NOT STARTED |
| C | Re-find the knee | ☐ NOT STARTED |
| D | Re-run comparisons | ☐ NOT STARTED |
| E | Correct the documentation | ☐ NOT STARTED |

**Last worked on:** nothing yet
**Next action:** A1

---

## The bugs

### B1 — KV projection undercounts by exactly 2.0x  [CRITICAL] [VERIFIED]

`loadgen.py:252` and `:118` project `kv_new = p` (prompt only) when `--kv-growth` is off,
which is the default that every run used.

Reality, measured directly: echo mode ignores `max_tokens` and always returns the full
prompt, so a request holds `prompt + generated = 2p`.

```
500 words, max_tokens=50   -> 500 completion tokens
500 words, max_tokens=200  -> 500 completion tokens
500 words, max_tokens=5000 -> 500 completion tokens
```

Confirmed live: router read occupancy 0.43-0.65 while backends reported 0.89-0.94.

**Consequence:** the admission gate never fires correctly and the whole KV term of
`pressure` runs on fiction. Every `pressure` result so far is invalid.

### B2 — The trace's output distribution is fiction  [HIGH] [VERIFIED]

Follows from B1. The backend ignores `max_tokens`, so:

- The documented "lognormal output, median 127" never happened. Actual output follows the
  *prompt* distribution (median 274, mean 415).
- The "64% long-prompt/short-output, the RAG pattern" claim in the journey doc **does not
  exist in the workload**. Every request is prompt = output.

### B3 — Capacity is roughly 4x smaller than assumed  [HIGH] [DERIVED]

```
KV per request      = 2 x 415  ~ 830 tokens   (not ~415)
concurrent/backend  = 8192/830 ~ 9.9          (not ~19)
mean service time   = 415 x 20ms = 8.3s       (not 4.5s)
saturation rate     ~ 40 slots / 8.3s / 2.5   ~ 1.9 req/s
```

Rate 8 is about 4x oversubscribed. Every comparison ran deep in collapse, not near the knee.

### B4 — `compare.py` measures spread over successes only  [MEDIUM] [VERIFIED by reading]

The `by_backend` loop iterates `ok`, not all rows. A backend that receives heavy traffic and
rejects most of it registers as lightly loaded, so imbalance looks better than it is. Error
rates have been 20-30%, so this matters.

### B5 — `random` is never seeded  [MEDIUM] [VERIFIED by reading]

Tie-breaking varies run to run. Adds unquantified noise and makes runs unreproducible.

### B6 — Cache-warming confound  [UNKNOWN] [NOT VERIFIED]

Arms run sequentially on the same trace and `pressure` always ran last, with the warmest
cache. Run 1 measured **zero** cache hits, and only ~10 requests fit in cache, so this is
probably small. Unmeasured, and it points against us, so it needs settling.

### B7 — Proxy-mode spread is unmeasurable  [LOW] [VERIFIED]

The loadgen only sees one URL in proxy mode, so all rows share a backend value and spread
computes as a trivial 0.0%. `scrape_backend_counts.py` exists but is not wired into
`compare.py`.

### B8 — Minor  [LOW]

- `--max-num-seqs` defaults to 8 while sims run 32 (scripts pass it, so latent)
- Proxy-mode ticker prints a meaningless occupancy figure
- `kv_proj < 1e-9` clamp should be `< 0`

---

## Phase A — Code fixes, no runs

**Blocking.** Nothing downstream is valid until these land.
**Estimate:** 45 min

- [ ] **A1. `loadgen.py`: replace `--kv-growth` with `--output-model`**
      `echo` sets `o_hat = prompt_tokens` (this simulator).
      `max_tokens` sets `o_hat = max_tokens` (real vLLM, stage 0b).
      KV projection becomes `p + o_hat` in both cases, giving the correct `2p` for echo.
      Removes the `kv_growth` flag, whose premise was wrong.
      *Fixes B1.*

- [ ] **A2. `loadgen.py`: add `--seed` (default 0)** and seed `random` at startup.
      *Fixes B5.*

- [ ] **A3. `loadgen.py`: default `--max-num-seqs` to 32.** Suppress the occupancy line in
      the ticker under proxy mode.
      *Fixes B8.*

- [ ] **A4. `compare.py`: report two spreads.**
      *dispatch spread* over all rows including errors (did the router balance where it sent
      traffic), and *completion spread* over successes (what actually got served).
      *Fixes B4.*

- [ ] **A5. `compare.py`: consume the proxy snapshots.** When `_before.json` / `_after.json`
      exist beside a CSV, diff them for real per-backend distribution.
      *Fixes B7.*

- [ ] **A6. `generate_trace.py`: make the trace honest.** Draw prompt from the desired
      lognormal, set `max_tokens` above it, stop reporting an output distribution the backend
      ignores, and print a warning that echo mode forces output = prompt.
      *Fixes B2.*

- [ ] **A7. Scripts: restart all four containers between every arm** so each starts cold.
      Removes B6 structurally regardless of its size.

---

## Phase B — Verify the fixes

**Blocking.** Gate on B1-check before going further.
**Estimate:** 20 min

- [ ] **B1-check.** Short run; compare router occupancy against live
      `kv_cache_usage_perc`. They should now track within a few percent.
      **If they do not match, STOP and re-diagnose. Do not proceed.**

- [ ] **B2-check.** Settle the cache confound: same trace twice on cold containers, compare
      error rates. Quantifies B6 instead of assuming it.

---

## Phase C — Re-establish the operating point

**Blocking.** Everything in D depends on knowing where the knee actually is.
**Estimate:** 30 min

- [ ] **C1. Re-run the knee sweep with corrected capacity.** Saturation is now estimated
      near **1.9 req/s**, so sweep roughly **0.5 to 3.0**, not 8 to 24.
      Record the chosen knee rate here: `__________`

---

## Phase D — Re-run comparisons

**Not blocking each other.** Either order is fine.
**Estimate:** 60 min

- [ ] **D1. Policy comparison** (`least_conn`, `kvts`, `pressure`) at the new knee, cold
      containers between arms, fixed seed.

- [ ] **D2. Competitor comparison** against `sgl-router` (`cache_aware`, `power_of_two`),
      with metrics snapshots so spread is real.

---

## Phase E — Correct the documentation

**Not blocking.** Do after D so the numbers are final.
**Estimate:** 30 min

- [ ] **E1. `journey/phase-0.md`** — real corrections, not additions:
      - The workload section describing lognormal *output* and 64% long-prompt/short-output
        is **wrong** and must be rewritten
      - Replace all result tables with post-fix numbers
      - Add the echo-mode limitation as a first-class finding

- [ ] **E2. `README.md`** — take the headline table (73%/27%/32%) **down** until re-measured.

- [ ] **E3. Add a "simulator limitations" section** documenting that echo mode forces
      output = prompt, which is why over-reservation cannot be measured here and why
      prompt/output decoupling is impossible.

---

## Open decisions

- [ ] **Are the 73%/27%/32% numbers dead?**
      Provisional read: yes. Measured with a router misjudging capacity by 2x, at ~4x past
      saturation, with spread computed over successes only. The *direction* may survive since
      all arms shared the same broken model, but the magnitude is not defensible. Should come
      out of the README until re-measured.
      **Decision:** ______________

- [ ] **Stay on echo mode?**
      It structurally cannot decouple prompt from output, which kills the RAG-pattern
      workload and the over-reservation measurement. Alternative is the simulator's
      `--dataset-path` sqlite mode, which gives real response lengths but needs setup.
      Recommendation: stay on echo, document the limitation, let stage 0b on real vLLM cover
      what echo cannot.
      **Decision:** ______________

---

## Notes / log

Append findings here as work proceeds, newest last.

- **2026-08-11** — Tracker created. Nothing fixed yet.
