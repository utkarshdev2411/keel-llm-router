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
        _req: &RequestFeatures,
        rng: &mut SmallRng,
        _trace: Option<&mut DecisionTrace>,
    ) -> Option<BackendId> {
        match snap.healthy.len() {
            0 => None,
            1 => Some(snap.healthy[0]),
            _ => {
                let sample: Vec<_> = snap.healthy.choose_multiple(rng, 2).copied().collect();
                let (a, b) = (sample[0], sample[1]);
                let load_a = snap.backends[a.0 as usize].live.inflight.load(Relaxed);
                let load_b = snap.backends[b.0 as usize].live.inflight.load(Relaxed);
                Some(if load_a <= load_b { a } else { b })
            }
        }
    }
}
