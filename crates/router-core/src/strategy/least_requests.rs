use std::sync::atomic::Ordering::Relaxed;

use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use smallvec::SmallVec;

use crate::backend::{BackendId, Snapshot};
use crate::features::RequestFeatures;
use crate::trace::DecisionTrace;

use super::RoutingStrategy;

pub struct LeastRequests;

impl RoutingStrategy for LeastRequests {
    fn name(&self) -> &'static str {
        "least_requests"
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
        let mut min = u32::MAX;
        let mut ties: SmallVec<[BackendId; 16]> = SmallVec::new();
        for &id in snap.healthy.iter() {
            let n = snap.backends[id.0 as usize].live.inflight.load(Relaxed);
            match n.cmp(&min) {
                std::cmp::Ordering::Less => {
                    min = n;
                    ties.clear();
                    ties.push(id);
                }
                std::cmp::Ordering::Equal => ties.push(id),
                std::cmp::Ordering::Greater => {}
            }
        }
        // Random tie-break, never index-based: index-based tie-breaking
        // piles every request onto backend zero at low load.
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

    fn backend(id: u16, inflight: u32) -> Arc<Backend> {
        let b = Backend {
            id: BackendId(id),
            key: format!("b{id}").into(),
            uri: "http://x".into(),
            model: "m".into(),
            weight: 1.0,
            caps: CapsEstimate { kv_capacity_tokens: 8192, max_num_seqs: 32 },
            live: LiveCounters::default(),
            reported: ArcSwapOption::from(None),
            health: HealthState::default(),
        };
        b.live.inflight.store(inflight, Relaxed);
        Arc::new(b)
    }

    fn req() -> RequestFeatures {
        RequestFeatures {
            model: "m".into(),
            prompt_tokens: 1,
            prompt_tokens_exact: true,
            expected_output_tokens: 1,
            max_tokens: None,
            prefix_key: None,
            streaming: true,
            arrived_at: std::time::Instant::now(),
        }
    }

    #[test]
    fn picks_the_backend_with_fewest_inflight() {
        let backends = vec![backend(0, 5), backend(1, 1), backend(2, 3)];
        let snap = Snapshot {
            epoch: 0,
            healthy: backends.iter().map(|b| b.id).collect(),
            backends,
            ring: Box::new([]),
        };
        let mut rng = SmallRng::seed_from_u64(0);
        let picked = LeastRequests.pick(&snap, &req(), &mut rng, None).unwrap();
        assert_eq!(picked, BackendId(1));
    }

    #[test]
    fn tie_break_uses_both_tied_backends_over_many_draws() {
        let backends = vec![backend(0, 2), backend(1, 2), backend(2, 9)];
        let snap = Snapshot {
            epoch: 0,
            healthy: backends.iter().map(|b| b.id).collect(),
            backends,
            ring: Box::new([]),
        };
        let mut rng = SmallRng::seed_from_u64(42);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let picked = LeastRequests.pick(&snap, &req(), &mut rng, None).unwrap();
            assert_ne!(picked, BackendId(2), "must never pick the non-tied, more-loaded backend");
            seen.insert(picked);
        }
        assert_eq!(seen.len(), 2, "both tied backends should appear across enough draws");
    }

    #[test]
    fn empty_healthy_returns_none() {
        let snap = Snapshot { epoch: 0, backends: vec![], healthy: Box::new([]), ring: Box::new([]) };
        let mut rng = SmallRng::seed_from_u64(0);
        assert!(LeastRequests.pick(&snap, &req(), &mut rng, None).is_none());
    }
}
