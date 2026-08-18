use std::sync::atomic::Ordering::Relaxed;

use rand::rngs::SmallRng;
use rand::seq::SliceRandom;

use crate::backend::{BackendId, Snapshot};
use crate::features::RequestFeatures;
use crate::trace::DecisionTrace;

use super::RoutingStrategy;

pub struct P2c;

impl RoutingStrategy for P2c {
    fn name(&self) -> &'static str {
        "p2c"
    }

    fn pick(
        &self,
        snap: &Snapshot,
        req: &RequestFeatures,
        rng: &mut SmallRng,
        trace: Option<&mut DecisionTrace>,
    ) -> Option<BackendId> {
        let picked = match snap.healthy.len() {
            0 => None,
            1 => Some(snap.healthy[0]),
            _ => {
                let sample: Vec<_> = snap.healthy.choose_multiple(rng, 2).copied().collect();
                let (a, b) = (sample[0], sample[1]);
                let load_a = snap.backends[a.0 as usize].live.inflight.load(Relaxed);
                let load_b = snap.backends[b.0 as usize].live.inflight.load(Relaxed);
                Some(if load_a <= load_b { a } else { b })
            }
        };
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
    fn single_healthy_backend_returned_directly() {
        let backends = vec![backend(0, 3)];
        let snap = Snapshot {
            epoch: 0,
            healthy: backends.iter().map(|b| b.id).collect(),
            backends,
            ring: Box::new([]),
        };
        let mut rng = SmallRng::seed_from_u64(0);
        assert_eq!(P2c.pick(&snap, &req(), &mut rng, None), Some(BackendId(0)));
    }

    #[test]
    fn never_picks_the_more_loaded_of_a_sampled_pair() {
        // With only two backends, every sample includes both, so the choice
        // is deterministic: always the lower-loaded one.
        let backends = vec![backend(0, 10), backend(1, 1)];
        let snap = Snapshot {
            epoch: 0,
            healthy: backends.iter().map(|b| b.id).collect(),
            backends,
            ring: Box::new([]),
        };
        let mut rng = SmallRng::seed_from_u64(7);
        for _ in 0..50 {
            assert_eq!(P2c.pick(&snap, &req(), &mut rng, None), Some(BackendId(1)));
        }
    }

    #[test]
    fn empty_healthy_returns_none() {
        let snap = Snapshot { epoch: 0, backends: vec![], healthy: Box::new([]), ring: Box::new([]) };
        let mut rng = SmallRng::seed_from_u64(0);
        assert!(P2c.pick(&snap, &req(), &mut rng, None).is_none());
    }
}
