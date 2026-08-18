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
        _req: &RequestFeatures,
        rng: &mut SmallRng,
        _trace: Option<&mut DecisionTrace>,
    ) -> Option<BackendId> {
        if snap.healthy.is_empty() {
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
        ties.choose(rng).copied()
    }
}
