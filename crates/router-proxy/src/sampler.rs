//! Periodic background occupancy sampling.
//!
//! ### Technical Motivation
//! Sampling backend metrics solely on the active request path is insufficient:
//! when a backend becomes saturated and the routing policy correctly avoids dispatching to it,
//! request-path metrics for that backend cease updating and freeze at stale values.
//!
//! ### Periodic Wall-Clock Sampling
//! To maintain accurate telemetry for all backends regardless of traffic allocation:
//! - This task runs an asynchronous loop sampling every backend at a fixed wall-clock interval.
//! - Records live capacity occupancy, in-flight requests, and projected KV tokens.
//! - Tracks accumulated sample ticks (`ticks_total`) and ticks where occupancy meets or exceeds the admission ceiling `sigma` (`ticks_at_ceiling`).
//! - The ratio `ticks_at_ceiling / ticks_total` evaluates the fraction of time backends spend at or above safety thresholds.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use router_core::cost::occupancy;

use crate::observe;
use crate::router::RouterState;

/// Background loop that periodically samples capacity occupancy and live counters for all backends.
///
/// Evaluates backend occupancy against the safety ceiling `sigma` to track time spent in saturated states.
/// Configures `MissedTickBehavior::Delay` to prevent catch-up sample bursts from distorting metric ratios after stalls.
pub async fn sample_occupancy_loop(state: Arc<RouterState>, sigma: f64, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
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
