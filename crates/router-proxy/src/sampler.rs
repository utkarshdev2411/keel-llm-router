//! Periodic occupancy sampling.
//!
//! The Phase 2 exit criterion has two halves. Error rate is measured by the load
//! generator, but the mechanism half — "a lower fraction of the run spent at or
//! above sigma occupancy" — cannot be. The load generator only ever sees one URL
//! in proxy mode, so `occupancy_stats.py` prints nothing for these runs.
//!
//! Sampling on the request path does not work either. `record_occupancy` in the
//! handler only fires for the backend that was *chosen*, so a backend the policy
//! correctly stops choosing because it is saturated freezes its gauge at a stale
//! value. That is precisely the backend whose occupancy the criterion is about.
//!
//! This task therefore samples every backend on a fixed wall-clock tick,
//! independent of traffic, and accumulates two counters per backend. Their ratio
//! is the fraction of the run that backend spent at or above the ceiling.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use router_core::cost::occupancy;

use crate::observe;
use crate::router::RouterState;

/// Sample every backend's occupancy on a fixed tick until the process exits.
///
/// `sigma` is the same ceiling the admission gate filters on, so
/// `ticks_at_ceiling / ticks_total` answers "how much of the run did this
/// backend spend in the region the gate was trying to keep it out of".
pub async fn sample_occupancy_loop(state: Arc<RouterState>, sigma: f64, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    // A missed tick must not cause a burst of catch-up samples: that would
    // over-weight whatever the occupancy happened to be during the stall.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        let snap = state.snapshot.load();
        for b in snap.backends.iter() {
            let u = occupancy(b);
            observe::record_occupancy(&b.key, u);
            observe::record_kv_projected(&b.key, b.live.kv_projected_tokens.load(Relaxed));
            observe::record_inflight(&b.key, b.live.inflight.load(Relaxed));
            observe::record_occupancy_tick(&b.key, u >= sigma);
        }
    }
}
