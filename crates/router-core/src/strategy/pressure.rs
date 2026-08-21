use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use smallvec::SmallVec;
use std::sync::atomic::Ordering::Relaxed;

use crate::backend::{BackendId, Snapshot};
use crate::cost::{occupancy, pressure_score, KvModel};
use crate::features::RequestFeatures;
use crate::gate;
use crate::trace::{CandidateScore, DecisionTrace, GateReason};

use super::RoutingStrategy;

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

        let mut eligible: SmallVec<[BackendId; 16]> = SmallVec::new();
        gate::eligible(snap, need_kv, self.sigma, &mut eligible);

        let fell_through = eligible.is_empty();
        let pool: &[BackendId] = if fell_through {
            &snap.healthy
        } else {
            &eligible
        };

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

        let picked = ties.choose(rng).copied();

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

    #[test]
    fn dispatches_to_least_bad_when_no_backend_passes_the_gate() {
        let backends = vec![
            backend(0, 0, 8192, 8192, 32),
            backend(1, 0, 8192, 8192, 32),
            backend(2, 0, 8192, 8192, 32),
        ];
        let s = snap(backends);
        let p = pressure();
        let mut rng = SmallRng::seed_from_u64(0);
        let result = p.pick(&s, &req(100, 0), &mut rng, None);
        assert!(result.is_some(), "must dispatch even when all backends exceed the gate");
    }

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

    #[test]
    fn beats_least_requests_on_heavy_tailed_synthetic_snapshot() {
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

    #[test]
    fn empty_healthy_returns_none() {
        let s = Snapshot { epoch: 0, backends: vec![], healthy: Box::new([]), ring: Box::new([]) };
        let p = pressure();
        let mut rng = SmallRng::seed_from_u64(0);
        assert!(p.pick(&s, &req(10, 5), &mut rng, None).is_none());
    }

    #[test]
    fn trace_records_fell_through_flag() {
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
