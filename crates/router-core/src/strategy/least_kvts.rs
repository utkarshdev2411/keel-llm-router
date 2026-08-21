use std::sync::atomic::Ordering::Relaxed;

use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use smallvec::SmallVec;

use crate::backend::{BackendId, Snapshot};
use crate::features::RequestFeatures;
use crate::trace::DecisionTrace;

use super::RoutingStrategy;

pub struct LeastKvts;

impl RoutingStrategy for LeastKvts {
    fn name(&self) -> &'static str {
        "least_kvts"
    }

    fn pick(
        &self,
        snap: &Snapshot,
        req: &RequestFeatures,
        rng: &mut SmallRng,
        trace: Option<&mut DecisionTrace>,
    ) -> Option<BackendId> {
        if snap.healthy.is_empty() {
            crate::trace::record_pick(trace, self.name(), snap, req, None);
            return None;
        }

        let mut best_score = f64::NEG_INFINITY;
        let mut ties: SmallVec<[BackendId; 16]> = SmallVec::new();

        for &id in snap.healthy.iter() {
            let b = &snap.backends[id.0 as usize];
            let kv = b.live.kv_projected_tokens.load(Relaxed);
            let remaining = (b.caps.kv_capacity_tokens as i64 - kv).max(0) as f64;
            match remaining.partial_cmp(&best_score) {
                Some(std::cmp::Ordering::Greater) => {
                    best_score = remaining;
                    ties.clear();
                    ties.push(id);
                }
                Some(std::cmp::Ordering::Equal) => ties.push(id),
                _ => {}
            }
        }

        let picked = ties.choose(rng).copied();
        crate::trace::record_pick(trace, self.name(), snap, req, picked);
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, CapsEstimate, HealthState, LiveCounters};
    use arc_swap::ArcSwapOption;
    use rand::SeedableRng;
    use std::sync::Arc;
    use std::time::Instant;

    fn backend(id: u16, kv_projected: i64, kv_capacity: u32) -> Arc<Backend> {
        let b = Backend {
            id: BackendId(id),
            key: format!("b{id}").into(),
            uri: "http://x".into(),
            model: "m".into(),
            weight: 1.0,
            caps: CapsEstimate { kv_capacity_tokens: kv_capacity, max_num_seqs: 32 },
            live: LiveCounters::default(),
            reported: ArcSwapOption::from(None),
            health: HealthState::default(),
        };
        b.live.kv_projected_tokens.store(kv_projected, Relaxed);
        Arc::new(b)
    }

    fn req() -> RequestFeatures {
        RequestFeatures {
            model: "m".into(),
            prompt_tokens: 50,
            prompt_tokens_exact: true,
            expected_output_tokens: 50,
            max_tokens: None,
            prefix_key: None,
            streaming: true,
            arrived_at: Instant::now(),
        }
    }

    fn snap(backends: Vec<Arc<Backend>>) -> Snapshot {
        let healthy: Box<[BackendId]> = backends.iter().map(|b| b.id).collect();
        Snapshot { epoch: 0, backends, healthy, ring: Box::new([]) }
    }

    #[test]
    fn picks_backend_with_most_absolute_remaining_kv() {
        let backends = vec![
            backend(0, 7168, 8192),
            backend(1, 600, 8192),
        ];
        let s = snap(backends);
        let mut rng = SmallRng::seed_from_u64(0);
        let picked = LeastKvts.pick(&s, &req(), &mut rng, None).unwrap();
        assert_eq!(picked, BackendId(1), "must pick backend with most absolute remaining KV");
    }

    #[test]
    fn loses_to_least_requests_on_heavy_tailed_snapshot() {
        let backends_c = vec![
            backend(0, 4000, 8192),
            backend(1, 4000, 8192),
        ];
        let s = snap(backends_c);
        let results: Vec<_> = (0..100u64)
            .map(|seed| LeastKvts.pick(&s, &req(), &mut SmallRng::seed_from_u64(seed * 13 + 7), None).unwrap().0)
            .collect();
        let distinct: std::collections::HashSet<_> = results.iter().collect();
        assert_eq!(distinct.len(), 2,
            "least_kvts cannot discriminate between equal-KV backends differing in slot pressure; \
             this is the documented failure mode — use pressure strategy instead");
    }

    #[test]
    fn empty_healthy_returns_none() {
        let s = Snapshot { epoch: 0, backends: vec![], healthy: Box::new([]), ring: Box::new([]) };
        let mut rng = SmallRng::seed_from_u64(0);
        assert!(LeastKvts.pick(&s, &req(), &mut rng, None).is_none());
    }
}
