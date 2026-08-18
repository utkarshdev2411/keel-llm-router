use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use rand::rngs::SmallRng;

use crate::backend::{BackendId, Snapshot};
use crate::features::RequestFeatures;
use crate::trace::DecisionTrace;

use super::RoutingStrategy;

pub struct RoundRobin {
    next: AtomicUsize,
}

impl RoundRobin {
    pub fn new() -> Self {
        Self { next: AtomicUsize::new(0) }
    }
}

impl Default for RoundRobin {
    fn default() -> Self {
        Self::new()
    }
}

impl RoutingStrategy for RoundRobin {
    fn name(&self) -> &'static str {
        "round_robin"
    }

    fn pick(
        &self,
        snap: &Snapshot,
        _req: &RequestFeatures,
        _rng: &mut SmallRng,
        _trace: Option<&mut DecisionTrace>,
    ) -> Option<BackendId> {
        if snap.healthy.is_empty() {
            return None;
        }
        let i = self.next.fetch_add(1, Relaxed) % snap.healthy.len();
        Some(snap.healthy[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Snapshot;

    fn snap(n: u16) -> Snapshot {
        Snapshot {
            epoch: 0,
            backends: vec![],
            healthy: (0..n).map(BackendId).collect(),
            ring: Box::new([]),
        }
    }

    #[test]
    fn cycles_through_all_backends() {
        let rr = RoundRobin::new();
        let s = snap(3);
        let mut rng = rand::SeedableRng::seed_from_u64(0);
        let req = crate::features::RequestFeatures {
            model: "m".into(),
            prompt_tokens: 1,
            prompt_tokens_exact: true,
            expected_output_tokens: 1,
            max_tokens: None,
            prefix_key: None,
            streaming: true,
            arrived_at: std::time::Instant::now(),
        };
        let picks: Vec<_> = (0..6).map(|_| rr.pick(&s, &req, &mut rng, None).unwrap().0).collect();
        assert_eq!(picks, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn empty_healthy_returns_none() {
        let rr = RoundRobin::new();
        let s = snap(0);
        let mut rng = rand::SeedableRng::seed_from_u64(0);
        let req = crate::features::RequestFeatures {
            model: "m".into(),
            prompt_tokens: 1,
            prompt_tokens_exact: true,
            expected_output_tokens: 1,
            max_tokens: None,
            prefix_key: None,
            streaming: true,
            arrived_at: std::time::Instant::now(),
        };
        assert!(rr.pick(&s, &req, &mut rng, None).is_none());
    }
}
