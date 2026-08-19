use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use smallvec::SmallVec;
use std::sync::atomic::Ordering::Relaxed;

use crate::backend::{BackendId, Snapshot};
use crate::cost::{KvModel, occupancy, pressure_score};
use crate::features::RequestFeatures;
use crate::gate;
use crate::trace::{CandidateScore, DecisionTrace, GateReason};

use super::RoutingStrategy;

/// The shipping routing strategy.
///
/// Decision procedure (§4.2 of the LLD):
/// 1. Compute `need_kv` from the configured `KvModel`.
/// 2. Filter to eligible backends via the admission gate.
/// 3. **If the eligible set is empty**, restore the full healthy set and increment
///    `router_saturated_dispatches_total`. This is a correctness requirement, not a
///    graceful-degradation nicety: a gate that refused would turn routing results into
///    load-shedding artifacts.
/// 4. Full argmin over `pressure_score`, **random** tie-break (never index-based).
/// 5. Return `None` only when `snap.healthy` is itself empty.
pub struct Pressure {
    pub theta: f64,
    pub penalty: f64,
    pub sigma: f64,
    pub kv_model: KvModel,
}

impl RoutingStrategy for Pressure {
    fn name(&self) -> &'static str {
        "pressure"
    }

    fn pick(
        &self,
        snap: &Snapshot,
        req: &RequestFeatures,
        rng: &mut SmallRng,
        trace: Option<&mut DecisionTrace>,
    ) -> Option<BackendId> {
        if snap.healthy.is_empty() {
            if let Some(t) = trace {
                t.strategy = self.name();
                t.chosen = None;
                t.prompt_tokens = req.prompt_tokens;
                t.expected_output_tokens = req.expected_output_tokens;
            }
            return None;
        }

        let need_kv = self.kv_model.project(req.prompt_tokens, req.expected_output_tokens);

        // --- Step 2: filter eligible backends ---
        let mut eligible: SmallVec<[BackendId; 16]> = SmallVec::new();
        gate::eligible(snap, need_kv, self.sigma, &mut eligible);

        // --- Step 3: fall-through when nothing passes the gate ---
        let fell_through = eligible.is_empty();
        let pool: &[BackendId] = if fell_through {
            // Gate found nothing clean. Dispatch to least-bad anyway.
            // This is NOT a rejection. The caller (router.rs) is responsible
            // for incrementing the saturated_dispatches counter — router-core
            // must stay pure (no metrics crate dependency).
            &snap.healthy
        } else {
            &eligible
        };

        // --- Step 4: full argmin over pressure_score, random tie-break ---
        let mut min_score = f64::INFINITY;
        let mut ties: SmallVec<[BackendId; 16]> = SmallVec::new();

        for &id in pool {
            let b = &snap.backends[id.0 as usize];
            let u = occupancy(b);
            let score = pressure_score(u, self.theta, self.penalty);
            match score.partial_cmp(&min_score) {
                Some(std::cmp::Ordering::Less) => {
                    min_score = score;
                    ties.clear();
                    ties.push(id);
                }
                Some(std::cmp::Ordering::Equal) => ties.push(id),
                _ => {}
            }
        }

        // Random tie-break — never index-based: at low load all scores are equal and
        // an index tie-break sends every request to backend zero.
        let picked = ties.choose(rng).copied();

        // --- Populate trace ---
        if let Some(t) = trace {
            t.strategy = self.name();
            t.chosen = picked;
            t.prompt_tokens = req.prompt_tokens;
            t.expected_output_tokens = req.expected_output_tokens;
            t.fell_through = fell_through;
            t.candidates = snap
                .healthy
                .iter()
                .map(|&id| {
                    let b = &snap.backends[id.0 as usize];
                    let u = occupancy(b);
                    let score = pressure_score(u, self.theta, self.penalty);
                    // Determine gate reason for backends that didn't pass
                    let gated_by = if !fell_through && !eligible.contains(&id) {
                        let kv_ok = b.live.kv_projected_tokens.load(Relaxed) + need_kv
                            <= (self.sigma * b.caps.kv_capacity_tokens as f64) as i64;
                        if !kv_ok {
                            Some(GateReason::KvHeadroom)
                        } else {
                            Some(GateReason::SlotLimit)
                        }
                    } else {
                        None
                    };
                    CandidateScore {
                        backend: id,
                        score,
                        occupancy: u,
                        inflight: b.live.inflight.load(Relaxed),
                        kv_projected: b.live.kv_projected_tokens.load(Relaxed),
                        gated_by,
                        reported_kv: b
                            .reported
                            .load()
                            .as_ref()
                            .and_then(|r| r.kv_usage_perc),
                        signal_age_ms: None,
                    }
                })
                .collect();
        }

        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, CapsEstimate, HealthState, LiveCounters, Snapshot};
    use crate::cost::KvModel;
    use arc_swap::ArcSwapOption;
    use rand::SeedableRng;
    use std::sync::Arc;
    use std::time::Instant;

    fn backend(id: u16, inflight: u32, kv_projected: i64, kv_capacity: u32, max_seqs: u32) -> Arc<Backend> {
        let b = Backend {
            id: BackendId(id),
            key: format!("b{id}").into(),
            uri: "http://x".into(),
            model: "m".into(),
            weight: 1.0,
            caps: CapsEstimate { kv_capacity_tokens: kv_capacity, max_num_seqs: max_seqs },
            live: LiveCounters::default(),
            reported: ArcSwapOption::from(None),
            health: HealthState::default(),
        };
        b.live.inflight.store(inflight, Relaxed);
        b.live.kv_projected_tokens.store(kv_projected, Relaxed);
        Arc::new(b)
    }

    fn req(prompt: u32, output: u32) -> RequestFeatures {
        RequestFeatures {
            model: "m".into(),
            prompt_tokens: prompt,
            prompt_tokens_exact: true,
            expected_output_tokens: output,
            max_tokens: None,
            prefix_key: None,
            streaming: true,
            arrived_at: Instant::now(),
        }
    }

    fn pressure() -> Pressure {
        Pressure {
            theta: crate::cost::DEFAULT_THETA,
            penalty: crate::cost::DEFAULT_PENALTY,
            sigma: crate::cost::DEFAULT_SIGMA,
            kv_model: KvModel::PromptOnly,
        }
    }

    fn snap(backends: Vec<Arc<Backend>>) -> Snapshot {
        let healthy: Box<[BackendId]> = backends.iter().map(|b| b.id).collect();
        Snapshot { epoch: 0, backends, healthy, ring: Box::new([]) }
    }

    /// The load-bearing structural test for Phase 2:
    /// When every backend is made ineligible by the gate, the router must still
    /// dispatch rather than returning None or erroring. A router that returns None
    /// here turns the whole result into a load-shedding artifact.
    #[test]
    fn dispatches_to_least_bad_when_no_backend_passes_the_gate() {
        // All backends saturated: kv_projected == kv_capacity (occupancy = 1.0)
        let backends = vec![
            backend(0, 0, 8192, 8192, 32),
            backend(1, 0, 8192, 8192, 32),
            backend(2, 0, 8192, 8192, 32),
        ];
        let s = snap(backends);
        let p = pressure();
        let mut rng = SmallRng::seed_from_u64(0);
        // Request needs 100 KV tokens — would exceed sigma * capacity on all backends
        let result = p.pick(&s, &req(100, 0), &mut rng, None);
        assert!(result.is_some(), "must dispatch even when all backends exceed the gate");
    }

    /// Random tie-break: at low load all backends score equally, so backend zero
    /// must NOT be picked every time.
    #[test]
    fn tie_break_is_random_never_index_based() {
        let backends = vec![
            backend(0, 0, 0, 8192, 32),
            backend(1, 0, 0, 8192, 32),
            backend(2, 0, 0, 8192, 32),
        ];
        let s = snap(backends);
        let p = pressure();
        let mut rng = SmallRng::seed_from_u64(42);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let picked = p.pick(&s, &req(10, 5), &mut rng, None).unwrap();
            seen.insert(picked.0);
        }
        assert!(seen.len() > 1, "tie-break must not always pick backend 0; got {:?}", seen);
    }

    /// Routing is deterministic given the same state and seed (NFR-9 / LLD §4.3).
    #[test]
    fn decision_is_deterministic_given_seed() {
        let backends = vec![
            backend(0, 2, 500, 8192, 32),
            backend(1, 1, 200, 8192, 32),
            backend(2, 3, 800, 8192, 32),
        ];
        let s = snap(backends);
        let p = pressure();
        let r = req(100, 50);
        let first = p.pick(&s, &r, &mut SmallRng::seed_from_u64(7), None);
        let second = p.pick(&s, &r, &mut SmallRng::seed_from_u64(7), None);
        assert_eq!(first, second);
    }

    /// pressure should pick the backend with the genuinely lower occupancy,
    /// not the one that looks cheaper in absolute work terms.
    #[test]
    fn beats_least_requests_on_heavy_tailed_synthetic_snapshot() {
        // Backend A: 1 long request, holds 4000/8192 KV tokens (occupancy ~0.49)
        // Backend B: 3 short requests, holds 600/8192 KV tokens (occupancy ~0.37, but 3 inflight)
        // Backend C: 0 requests, 0 KV (occupancy 0.0) — clear winner
        let backends = vec![
            backend(0, 1, 4000, 8192, 32),
            backend(1, 3, 600, 8192, 32),
            backend(2, 0, 0, 8192, 32),
        ];
        let s = snap(backends);
        let p = pressure();
        let mut rng = SmallRng::seed_from_u64(0);
        let picked = p.pick(&s, &req(50, 20), &mut rng, None).unwrap();
        assert_eq!(picked, BackendId(2), "must pick the empty backend");
    }

    /// Empty healthy → None
    #[test]
    fn empty_healthy_returns_none() {
        let s = Snapshot { epoch: 0, backends: vec![], healthy: Box::new([]), ring: Box::new([]) };
        let p = pressure();
        let mut rng = SmallRng::seed_from_u64(0);
        assert!(p.pick(&s, &req(10, 5), &mut rng, None).is_none());
    }

    /// Trace fell_through flag is set correctly
    #[test]
    fn trace_records_fell_through_flag() {
        // All backends fully saturated on KV
        let backends = vec![
            backend(0, 0, 8192, 8192, 32),
            backend(1, 0, 8192, 8192, 32),
        ];
        let s = snap(backends);
        let p = pressure();
        let mut rng = SmallRng::seed_from_u64(0);
        let mut trace = DecisionTrace::default();
        p.pick(&s, &req(100, 0), &mut rng, Some(&mut trace));
        assert!(trace.fell_through, "trace must record that gate found nothing eligible");
    }
}
