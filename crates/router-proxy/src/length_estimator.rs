/// Per-route output-length estimation for Phase 2.
///
/// The algorithm uses `max_tokens` when the client supplies it (an exact upper bound,
/// free). Otherwise it draws from a decaying per-route histogram of recently observed
/// output lengths. **One estimate** is returned — the gate and the score use the same
/// projection. An earlier design used a separate high-percentile estimate for admission
/// only; the validated gate does not do that.
///
/// No learned model, ever. Published work found a coarse classifier at 61% accuracy
/// still delivered most of the available benefit. Self-correction (the recharge loop
/// in `CostLease`) is what makes this work, not prediction precision.
use std::sync::Arc;

use router_core::features::{RequestFeatures, RouteKey};

/// A single decaying quantile sketch. Tracks a running weighted estimate using
/// exponential decay so recent observations carry more weight than old ones.
///
/// Implementation: a simple exponential moving average with a configurable half-life.
/// This is deliberately unsophisticated — precision is not what drives the algorithm's
/// correctness; self-correction via `observe_tokens` recharge is.
pub struct DecayingQuantile {
    halflife_s: f64,
    /// Current estimate (tokens). None until at least one observation.
    estimate: Option<f64>,
    /// Timestamp of last observation (seconds since an arbitrary epoch).
    last_obs_s: f64,
}

impl DecayingQuantile {
    pub fn new(halflife_s: f64) -> Self {
        Self { halflife_s, estimate: None, last_obs_s: 0.0 }
    }

    /// Record a new observed output length and update the decaying estimate.
    pub fn observe(&mut self, value: u32, now_s: f64) {
        let v = value as f64;
        match self.estimate {
            None => {
                self.estimate = Some(v);
                self.last_obs_s = now_s;
            }
            Some(prev) => {
                // Exponential decay weight: how much the old estimate has faded
                let elapsed = (now_s - self.last_obs_s).max(0.0);
                let decay = (-elapsed * std::f64::consts::LN_2 / self.halflife_s).exp();
                // Blend: new = old * decay + new_value * (1 - decay)
                self.estimate = Some(prev * decay + v * (1.0 - decay));
                self.last_obs_s = now_s;
            }
        }
    }

    /// Current estimate in tokens. Returns `None` if no observations yet.
    pub fn estimate(&self) -> Option<u32> {
        self.estimate.map(|v| v.round() as u32)
    }
}

/// Per-route histogram holding a p50 and p95 decaying quantile.
/// Only p50 is used for the output estimate (one estimate, same for scoring and gate).
pub struct RouteHistogram {
    pub p50: DecayingQuantile,
    pub p95: DecayingQuantile,
}

impl RouteHistogram {
    pub fn new(halflife_s: f64) -> Self {
        Self {
            p50: DecayingQuantile::new(halflife_s),
            p95: DecayingQuantile::new(halflife_s * 3.0), // p95 decays slower
        }
    }

    pub fn observe(&mut self, completion_tokens: u32, now_s: f64) {
        self.p50.observe(completion_tokens, now_s);
        self.p95.observe(completion_tokens, now_s);
    }
}

/// Default fallback when no histogram entry exists yet. 128 tokens — deliberately
/// conservative; under-estimates are corrected upward by the recharge loop.
pub const DEFAULT_OUTPUT_ESTIMATE: u32 = 128;

/// Derive the output-length estimate `ô` for one request.
///
/// Priority (from the algorithm spec §5):
/// 1. `max_tokens` from the request — an exact upper bound, always wins.
/// 2. The route histogram p50.
/// 3. The hardcoded default.
///
/// Returns a value >= 1. A zero estimate makes `prompt_plus_output` projection equal
/// to `prompt_only`, silently changing the effective KV model.
pub fn estimate_output_tokens(
    max_tokens: Option<u32>,
    route_hist: Option<&RouteHistogram>,
) -> u32 {
    if let Some(mt) = max_tokens {
        return mt.max(1);
    }
    if let Some(hist) = route_hist {
        if let Some(est) = hist.p50.estimate() {
            return est.max(1);
        }
    }
    DEFAULT_OUTPUT_ESTIMATE
}

/// Derive the route key for histogram lookup.
/// Buckets by model name and coarse prompt-length bucket (powers of 2, capped at 4096).
pub fn route_key_for(features: &RequestFeatures) -> RouteKey {
    let bucket = prompt_bucket(features.prompt_tokens);
    RouteKey(Arc::clone(&features.model), bucket)
}

fn prompt_bucket(tokens: u32) -> u32 {
    // Round down to the nearest power of two, capped at 4096.
    if tokens == 0 {
        return 0;
    }
    let floored = 1u32 << (31 - tokens.leading_zeros());
    floored.min(4096)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_tokens_always_wins_over_histogram_estimate() {
        let mut hist = RouteHistogram::new(300.0);
        // Seed the histogram with a high value
        hist.observe(1000, 0.0);
        hist.observe(1000, 1.0);
        // But max_tokens=50 must win
        assert_eq!(estimate_output_tokens(Some(50), Some(&hist)), 50);
    }

    #[test]
    fn histogram_decays_toward_recent_observations() {
        let mut hist = RouteHistogram::new(1.0); // 1-second half-life for fast decay
        // Seed with high value at t=0
        hist.p50.observe(1000, 0.0);
        // Many recent low observations at t=10 (10 half-lives later)
        for i in 0..20 {
            hist.p50.observe(100, 10.0 + i as f64 * 0.1);
        }
        let est = hist.p50.estimate().unwrap();
        // After many observations of 100 with decay, estimate should be much closer to 100
        assert!(
            est < 500,
            "estimate ({est}) should have decayed toward recent observations (100)"
        );
    }

    #[test]
    fn estimate_falls_back_to_default_with_no_history() {
        assert_eq!(estimate_output_tokens(None, None), DEFAULT_OUTPUT_ESTIMATE);
        let empty_hist = RouteHistogram::new(300.0);
        assert_eq!(estimate_output_tokens(None, Some(&empty_hist)), DEFAULT_OUTPUT_ESTIMATE);
    }

    #[test]
    fn estimate_uses_histogram_when_no_max_tokens() {
        let mut hist = RouteHistogram::new(300.0);
        hist.observe(256, 0.0);
        let est = estimate_output_tokens(None, Some(&hist));
        assert_eq!(est, 256);
    }

    #[test]
    fn zero_max_tokens_clamped_to_one() {
        // A zero estimate is forbidden: it silently changes the effective KV model.
        assert_eq!(estimate_output_tokens(Some(0), None), 1);
    }

    #[test]
    fn prompt_bucket_rounds_to_power_of_two() {
        assert_eq!(prompt_bucket(1), 1);
        assert_eq!(prompt_bucket(100), 64);
        assert_eq!(prompt_bucket(512), 512);
        assert_eq!(prompt_bucket(1000), 512);
        assert_eq!(prompt_bucket(8192), 4096); // capped
    }
}
