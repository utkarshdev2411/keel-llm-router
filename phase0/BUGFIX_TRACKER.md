# Phase 0 Bug Fix Tracker

**Opened:** 2026-08-11
**Status:** IN PROGRESS — Phase A partially done (B1/B2/B3 only), Phase B blocked
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
| A | Code fixes, no runs | ◐ A1-A4, A6, A7 done + B1 re-fixed via --kv-model; only A5 (proxy snapshots) left |
| B | Verify the fixes | ◐ B1-check PASSED; B2-check (cache confound) now moot, A7 removes it structurally |
| C | Re-find the knee | ◐ KNEE = 8 req/s, provisional — rate 12/14 non-monotonic, needs repeats |
| D | Re-run comparisons | ☐ NOT STARTED |
| E | Correct the documentation | ☐ NOT STARTED |

**Last worked on:** B1-check PASSED with `--kv-model prompt_only`. Router and backend KV now
track within sampling noise across the whole run.
**Next action:** D1 — run `./stage5_compare.sh` (rates 8 and 10, 3 repeats per cell).
A2/A3/A4 are done, so runs are now reproducible and imbalance is measured over all rows.

---

## The bugs

### B1 — KV projection wrong  [CRITICAL] [RE-DIAGNOSED 2026-08-15, NOW FIXED]

> **The original diagnosis below was half wrong. Read this box first.**
>
> "Echo mode ignores `max_tokens` and echoes the full prompt" is TRUE and verified.
> The inference drawn from it -- "therefore a request holds `prompt + generated = 2p`
> of KV" -- is FALSE for this simulator.
>
> `kv_curve.py` measured one 1000-word request on an idle backend. KV usage was
> **flat at 0.1211 for the entire 20.4s**, never growing, and `0.1211 x 8192 = 992
> tokens = 62 blocks x 16 = the prompt alone`. The simulator allocates prompt blocks
> at admission, holds them constant, frees them at completion. **Generated tokens
> cost it no KV.** Also: 1000 prompt words produced exactly 1000 completion tokens,
> so words == tokens and there is no unit-mismatch bug.
>
> So the *first* fix (projecting `2p`) made the router read **2.02x HIGH**, which is
> exactly the 0.50-0.55 vs 0.20-0.24 gap that blocked Phase B.
>
> **Resolution:** added `--kv-model {prompt_only, prompt_plus_output}`.
> `prompt_only` is the default and matches this simulator. `prompt_plus_output` is
> physically correct for real vLLM and is what stage 0b must use.
>
> **Net effect on the original numbers:** the pre-fix runs used `kv_new = p`, which
> is what the corrected default now computes. So the original KV projection was
> **right for this simulator after all**, and B1 does not by itself invalidate the
> old results. See the notes/log for what this does and does not rescue.

<details>
<summary>Original (partly incorrect) diagnosis, kept for the record</summary>


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

</details>

### B2 — The trace's output distribution is fiction  [HIGH] [CODE FIXED]

Follows from B1. The backend ignores `max_tokens`, so:

- The documented "lognormal output, median 127" never happened. Actual output follows the
  *prompt* distribution (median 274, mean 415).
- The "64% long-prompt/short-output, the RAG pattern" claim in the journey doc **does not
  exist in the workload**. Every request is prompt = output.

### B3 — Capacity is roughly 4x smaller than assumed  [WITHDRAWN — the premise was wrong]

> **This bug does not exist as stated.** It was derived from B1's incorrect
> `2 x prompt` KV assumption. Since the simulator holds only the prompt
> (measured), KV per request is ~1x prompt, and the capacity estimate is back to
> roughly the original figure, not 4x smaller. `generate_trace.py` now computes
> `mean_kv = total_p / n`.
>
> The separate question of whether the OLD sweep range (8-24 req/s) was too high
> is still open, but it must be re-derived from the corrected capacity math rather
> than from this withdrawn 1.9 req/s number. That is Phase C.

<details>
<summary>Original (incorrect) derivation, kept for the record</summary>


```
KV per request      = 2 x 415  ~ 830 tokens   (not ~415)
concurrent/backend  = 8192/830 ~ 9.9          (not ~19)
mean service time   = 415 x 20ms = 8.3s       (not 4.5s)
saturation rate     ~ 40 slots / 8.3s / 2.5   ~ 1.9 req/s
```

Rate 8 is about 4x oversubscribed. Every comparison ran deep in collapse, not near the knee.

</details>

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

- [x] **A1. `loadgen.py`: replace `--kv-growth` with `--output-model`**
      `echo` sets `o_hat = prompt_tokens` (this simulator).
      `max_tokens` sets `o_hat = max_tokens` (real vLLM, stage 0b).
      KV projection becomes `p + o_hat` in both cases, giving the correct `2p` for echo.
      Removes the `kv_growth` flag, whose premise was wrong.
      *Fixes B1.* **DONE — verified by direct read of loadgen.py: `kv_new = p + o_hat`
      throughout (lines 122, 167, 263), recharge trigger is `c > o_hat`, `--output-model`
      flag present, `--kv-growth` gone.**

- [x] **A2. `loadgen.py`: add `--seed` (default 0)** and seed `random` at startup.
      *Fixes B5.* **DONE.**

- [x] **A3. `loadgen.py`: default `--max-num-seqs` to 32.** Suppress the occupancy line in
      the ticker under proxy mode.
      *Fixes B8.* **DONE — also fixed the `kv_proj < 1e-9` clamp to `< 0`; the old test
      zeroed small POSITIVE residuals, silently discarding real held KV.**

- [x] **A4. `compare.py`: report two spreads.**
      *dispatch spread* over all rows including errors (did the router balance where it sent
      traffic), and *completion spread* over successes (what actually got served).
      *Fixes B4.* **DONE. Required a `loadgen.py` change too: the CSV had no per-row KV cost,
      so `prompt_tokens` is now recorded for EVERY row including rejections. Table columns are
      now `DISP req` / `DISP kv` (all rows) and `ok tok` (successes only), with `DISP kv` as
      the headline metric.**

- [ ] **A5. `compare.py`: consume the proxy snapshots.** When `_before.json` / `_after.json`
      exist beside a CSV, diff them for real per-backend distribution.
      *Fixes B7.*

- [x] **A6. `generate_trace.py`: make the trace honest.** Draw prompt from the desired
      lognormal, set `max_tokens` above it, stop reporting an output distribution the backend
      ignores, and print a warning that echo mode forces output = prompt.
      *Fixes B2.* **DONE — verified by direct read: `sample_lengths()` draws one length in
      echo mode, `summarize()` prints the honest NOTE, capacity math uses `total_p + total_out`
      (fixes B3 as a consequence).**

- [x] **A7. Scripts: restart all four containers between every arm** so each starts cold.
      Removes B6 structurally regardless of its size.
      **DONE — `restart_sims.sh` added (polls for readiness rather than a fixed sleep) and
      wired into `stage3_knee.sh`. Still needs wiring into stage4/stage5/stage6.**

---

## Phase B — Verify the fixes

**Blocking.** Gate on B1-check before going further.
**Estimate:** 20 min

- [x] **B1-check. PASSED 2026-08-15.** With `--kv-model prompt_only`, a 250-req run at
      rate 4 gave router occupancy vs backend `kv_cache_usage_perc`:
      early 0.21/0.19/0.20/0.21 vs 0.256/0.225/0.264/0.240;
      mid 0.46/0.37/0.35/0.40 vs 0.463/0.424/0.416/0.408;
      peak 0.74/0.73/0.67/0.73 vs 0.738/0.725/0.650/0.725.
      The 2.02x factor is gone; residual gap is sampling-offset while occupancy climbed.
      `in_flight` peaked at 10-14 against max_num_seqs=32, so `u_slots` maxed at ~0.44
      while reported occupancy hit 0.74 — the KV term dominated, so this is a real
      KV-to-KV comparison and not slots in disguise.
      Original text of this task follows.

- [x] ~~**B1-check.** Short run; compare router occupancy against live
      `kv_cache_usage_perc`. They should now track within a few percent.
      **If they do not match, STOP and re-diagnose. Do not proceed.**
      **RAN, DID NOT PASS.** Restarted all 4 sims fresh, ran a 250-req trace at rate 4 with
      `pressure`. Router self-reported occupancy 0.50-0.55; backend's real
      `kv_cache_usage_perc` simultaneously read 0.20-0.24 — router now reads ~2.3x HIGH,
      opposite direction from the original bug. Not yet explained. Leading theory: router
      reserves peak KV (`prompt + full estimated output`) at dispatch time, while the
      backend's real KV grows gradually from `prompt` up to `prompt + tokens-so-far` over the
      request's life — an apples-to-oranges comparison (peak-reserved vs. instantaneous-actual)
      rather than a real bug, but not confirmed. A follow-up isolated single-request test
      (dedicated container, one fixed prompt, sampling `/metrics` every 2.5s across the
      request's lifetime to build a real KV-vs-time curve) was designed to settle this but was
      not run.
      **RESOLVED 2026-08-15.** Ran `kv_curve.py`. None of the three theories was
      right: the simulator does not grow KV during decode at all, so the *fix* was
      what made the router read high. Corrected via `--kv-model` (see B1 box).
      Re-run the occupancy comparison with the corrected default to actually
      close this item.~~ **(done — see the PASSED entry above)**

- [ ] **B2-check.** Settle the cache confound: same trace twice on cold containers, compare
      error rates. Quantifies B6 instead of assuming it.

---

## Phase C — Re-establish the operating point

**Blocking.** Everything in D depends on knowing where the knee actually is.
**Estimate:** 30 min

- [ ] **C1. Re-run the knee sweep with corrected capacity.** ~~Saturation is now estimated
      near 1.9 req/s, so sweep roughly 0.5 to 3.0~~ — that figure came from the withdrawn B3.
      Corrected: KV per request is ~250 tokens (prompt only), so ~32 concurrent per backend,
      ~131 across four, saturation near **10-11 req/s**. `stage3_knee.sh` now sweeps
      **4, 6, 8, 10, 12, 14** with `--max-num-seqs 64`.
      Run: `./stage3_knee.sh`
      Record the chosen knee rate here: **8 req/s** (provisional — see caveats)

      Measured 2026-08-15 (rates 4-14, max-num-seqs 64, cold containers per arm):

      | rate | err% | TTFT p99 |
      |---|---|---|
      | 4  | 0.0%  | 61 |
      | 6  | 0.4%  | 65 |
      | 8  | 3.2%  | 69 |
      | 10 | 6.7%  | 72 |
      | 12 | 10.6% | 74 |
      | 14 | 6.2%  | 78 |

      Knee is 8: first rate where errors are material without the system being collapsed.
      Run the comparison at **8 and 10**, and optionally 12.

      TTFT is NOT the signal here — p99 moves only 61→78ms across the whole sweep, because
      rejected requests never record a TTFT and the survivors are served fast. Read err%.

      **Caveat 1:** rate 14 (6.2%) is LOWER than rate 12 (10.6%). Non-monotonic and
      unexplained. One run per rate with unseeded tie-breaking (B5), so run-to-run variance
      is unquantified. Repeat 12 and 14 a few times before trusting either.

      **Caveat 2:** the first table produced also contained rows for rates 16/20/24. Those
      were STALE CSVs left in `results_knee/` from pre-fix runs (max-num-seqs 32, old traces)
      that `compare.py results_knee/*.csv` picked up. Moved to `results_knee_old/`.
      Glob-everything comparison is a standing hazard — check `ls` before believing a table.

      Note on max-num-seqs: raised 32 -> 64 deliberately. At 32 the slot limit and the KV
      limit (~32.8 concurrent) bind at nearly the same point, so the simulator's own slot cap
      partially hides KV exhaustion. At 64 the KV limit binds alone, which is what this
      project is actually about.

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
- **2026-08-11** — Fixed B1 (`loadgen.py`: `--kv-growth` -> `--output-model`, KV projection
  always `p + o_hat`, recharge trigger `c > o_hat`) and B2/B3 (`generate_trace.py`: single
  length draw in echo mode, honest `summarize()`, corrected capacity math). Ran B1-check:
  router occupancy (0.50-0.55) did not match backend `kv_cache_usage_perc` (0.20-0.24) — a
  new mismatch, opposite direction from the pre-fix bug. Not explained yet. B4-B8, Phase C/D/E
  untouched (fix was scoped to B1/B2/B3 only). Verified against the actual files on disk
  (not from memory) before writing this entry.
- **2026-08-15** — **B1 re-diagnosed. The first fix was wrong.** Ran `kv_curve.py` against a
  dedicated idle backend: one 1000-word prompt, 1000 completion tokens, 20.4s. KV usage held
  **flat at 0.1211 the entire time** (= 992 tokens = 62 blocks x 16 = the prompt alone), then
  dropped to 0 at completion. The simulator allocates prompt blocks at admission and never
  grows them during decode. Words == tokens exactly (1000 -> 1000), so no unit bug.
  Consequences:
    * B1's `2p` projection made the router read **2.02x HIGH** — that WAS the unexplained
      Phase-B gap. Fixed by adding `--kv-model {prompt_only, prompt_plus_output}`, default
      `prompt_only`.
    * **B3 is withdrawn.** Its 4x-capacity-shrink followed from the wrong `2p` premise.
      `generate_trace.py` reverted to `mean_kv = total_p / n`.
    * **B2 still stands.** Output really does equal prompt and `max_tokens` really is
      ignored; that fix is independent and correct.
    * The pre-fix runs projected `kv_new = p`, which is what the corrected default computes.
      **So the original KV projection was right for this simulator, and B1 alone does not
      invalidate the old results.** What still threatens them is B4 (spread over successes
      only), B5 (unseeded ties) and B6 (cache warming) — all unfixed.
  * **Bigger implication, needs a decision.** If the simulator charges no KV for generated
    tokens, then on this harness a request's KV cost is fully known at arrival (it is just
    the prompt). The project's core thesis — that *unpredictable output length* is the hidden
    cost that count-based balancing misses — is therefore **not testable here**. What IS
    testable is the weaker claim that balancing by real per-request cost beats balancing by
    request count. Echo mode ties duration to prompt length, so cost still varies by orders
    of magnitude; it is just knowable up front. This has to be stated plainly in the journey
    doc, and it strengthens the case for stage 0b on real vLLM.
- **2026-08-15** — **B1-check PASSED.** 250 reqs at rate 4, `--kv-model prompt_only`, four
  cold sims. Router occupancy tracked backend `kv_cache_usage_perc` across the whole run
  (peak 0.74/0.73/0.67/0.73 vs 0.738/0.725/0.650/0.725). KV term dominated `max()` throughout,
  so the comparison is genuinely KV-to-KV. Zero errors and `saturated_dispatches=0` at rate 4,
  confirming rate 4 sits well below the knee.
  Also landed: `restart_sims.sh` (task A7, kills the B6 cache confound structurally) and a
  rewritten `stage3_knee.sh` — versioned trace names `tr_v2_*` so the old pre-fix traces
  cannot be silently reused, `./venv/bin/python` instead of bare `python3`, rates 4-14, and
  `--max-num-seqs 64` to stop the slot limit from masking KV exhaustion.
- **2026-08-15** — **Knee sweep done. Knee = 8 req/s.** Two findings worth keeping:

  **(a) The capacity estimate was wrong again — length-biased sampling.** I predicted ~32
  concurrent per backend from `8192 / mean_length(250)`. Observed KV exhaustion at **13-15**
  in flight. The mean is the wrong statistic: a request holds KV for a time proportional to
  its length, so long requests linger and the in-flight population is biased toward them. The
  cost of a randomly-observed in-flight request is `E[L^2]/E[L]`, not `E[L]` — for this
  lognormal roughly 736 vs 220 tokens, a 3.3x difference. Saturation is
  `lambda = B*KV / (ITL*F*E[L^2])`. `generate_trace.py` now computes both moments empirically
  and prints the length-biased mean. This is worth writing up: it is a genuine, non-obvious
  property of heavy-tailed LLM traffic and it means **capacity planning from mean request
  size systematically overestimates by 3x+**.

  **(b) The thesis reproduced cleanly in the logs.** Under `least_conn` at rate 12:
  `inflight[8001=14 8002=15 8003=14 8004=15]` with
  `occupancy[8001=0.99 8002=0.52 8003=0.99 8004=0.98]`. Identical request counts, three
  backends at capacity and one half idle. That is the entire argument for the project in one
  line of log output — keep it for the journey doc and the LinkedIn post.
