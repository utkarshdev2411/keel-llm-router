//! Per-route output-length estimation (`ô`).
//!
//! Provides estimated completion token counts used by KV projection and admission gating.
//!
//! ### Estimation Priority
//! 1. Client-supplied `max_tokens` (exact upper bound).
//! 2. Time-decayed per-route historical estimate (`DecayingQuantile`).
//! 3. Conservative default (`DEFAULT_OUTPUT_ESTIMATE = 128`).
//!
//! ### Key Architectural Guarantees
//! - **Unified Estimate**: A single estimate is shared by both admission control and pressure scoring.
//! - **Self-Correction over Precision**: Uses exponential time-decay rather than complex predictive models;
//!   under-estimates are dynamically revised upward by `CostLease` token recharges.
//! - **Mode Dependency**: Only impacts routing when `KvModel::PromptPlusOutput` is active.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use router_core::features::{RequestFeatures, RouteKey};

/// Time-decaying output length estimator for a specific route.
///
/// Implements an exponential moving average in the time domain (seconds elapsed) rather than observation domain,
/// ensuring routes that remain idle decay their historical weighting rather than remaining anchored to old state.
pub struct DecayingQuantile {
    halflife_s: f64,
    estimate: Option<f64>,
    last_obs_s: f64,
}

impl DecayingQuantile {
    pub fn new(halflife_s: f64) -> Self {
        Self { halflife_s, estimate: None, last_obs_s: 0.0 }
    }

    pub fn observe(&mut self, value: u32, now_s: f64) {
        let v = value as f64;
        match self.estimate {
            None => {
                self.estimate = Some(v);
                self.last_obs_s = now_s;
            }
            Some(prev) => {
                let elapsed = (now_s - self.last_obs_s).max(0.0);
                let decay = (-elapsed * std::f64::consts::LN_2 / self.halflife_s).exp();
                self.estimate = Some(prev * decay + v * (1.0 - decay));
                self.last_obs_s = now_s;
            }
        }
    }

    pub fn estimate(&self) -> Option<u32> {
        self.estimate.map(|v| v.round() as u32)
    }
}

/// Fallback completion token estimate when no route history or `max_tokens` is available.
///
/// Initial under-estimates are safely corrected by `CostLease` runtime recharges.
pub const DEFAULT_OUTPUT_ESTIMATE: u32 = 128;

/// Shared thread-safe store for per-route output length estimates.
///
/// Uses a `Mutex<HashMap>` keyed by `RouteKey` (model name + prompt length bucket).
/// Lock hold time is microsecond-scale over tens of entries, introducing negligible contention relative to model TTFT.
pub struct RouteHistograms {
    started: Instant,
    halflife_s: f64,
    inner: Mutex<HashMap<RouteKey, DecayingQuantile>>,
}

impl RouteHistograms {
    pub fn new(halflife_s: f64) -> Self {
        Self { started: Instant::now(), halflife_s, inner: Mutex::new(HashMap::new()) }
    }

    /// Calculates monotonic seconds since creation to prevent clock skew from corrupting decay calculations.
    fn now_s(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// Resolves the estimated completion tokens `ô` for a request.
    ///
    /// Priority: explicit `max_tokens` > route history > `DEFAULT_OUTPUT_ESTIMATE`.
    /// Always returns at least 1 to ensure `PromptPlusOutput` projections do not collapse into `PromptOnly`.
    pub fn estimate(&self, key: &RouteKey, max_tokens: Option<u32>) -> u32 {
        if let Some(mt) = max_tokens {
            return mt.max(1);
        }
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .get(key)
            .and_then(|q| q.estimate())
            .map(|e| e.max(1))
            .unwrap_or(DEFAULT_OUTPUT_ESTIMATE)
    }

    /// Records an observed completion token length for a route key upon stream completion.
    pub fn observe(&self, key: &RouteKey, completion_tokens: u32) {
        let now_s = self.now_s();
        let halflife = self.halflife_s;
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .entry(key.clone())
            .or_insert_with(|| DecayingQuantile::new(halflife))
            .observe(completion_tokens, now_s);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

/// Derives a `RouteKey` combining the model identifier and prompt length bucket.
///
/// Bucketing by prompt length separates short and long prompt distributions for the same model.
pub fn route_key_for(features: &RequestFeatures) -> RouteKey {
    route_key(&features.model, features.prompt_tokens)
}

/// Constructs a `RouteKey` from model name and prompt token count.
pub fn route_key(model: &Arc<str>, prompt_tokens: u32) -> RouteKey {
    RouteKey(Arc::clone(model), prompt_bucket(prompt_tokens))
}

/// Buckets prompt token counts into logarithmic power-of-two intervals capped at 4096.
fn prompt_bucket(tokens: u32) -> u32 {
    if tokens == 0 {
        return 0;
    }
    let floored = 1u32 << (31 - tokens.leading_zeros());
    floored.min(4096)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(model: &str, prompt: u32) -> RouteKey {
        route_key(&Arc::from(model), prompt)
    }

    /// Verifies that explicit max_tokens takes precedence over learned estimates.
    #[test]
    fn max_tokens_always_wins_over_learned_estimate() {
        let h = RouteHistograms::new(300.0);
        let k = key("m", 100);
        h.observe(&k, 1000);
        assert_eq!(h.estimate(&k, Some(50)), 50);
    }

    /// Verifies default fallback when no history exists for a route.
    #[test]
    fn estimate_falls_back_to_default_with_no_history() {
        let h = RouteHistograms::new(300.0);
        assert_eq!(h.estimate(&key("m", 100), None), DEFAULT_OUTPUT_ESTIMATE);
    }

    /// Verifies estimation uses observed history when max_tokens is absent.
    #[test]
    fn estimate_uses_observed_history_when_no_max_tokens() {
        let h = RouteHistograms::new(300.0);
        let k = key("m", 100);
        h.observe(&k, 256);
        assert_eq!(h.estimate(&k, None), 256);
    }

    /// Ensures zero max_tokens is clamped to 1 to preserve PromptPlusOutput model semantics.
    #[test]
    fn zero_max_tokens_clamped_to_one() {
        let h = RouteHistograms::new(300.0);
        assert_eq!(h.estimate(&key("m", 100), Some(0)), 1);
    }

    /// Verifies separate keying by model name and prompt bucket.
    #[test]
    fn routes_are_keyed_separately_by_model_and_prompt_bucket() {
        let h = RouteHistograms::new(300.0);
        h.observe(&key("a", 100), 50);
        h.observe(&key("b", 100), 900);
        h.observe(&key("a", 4000), 400);
        assert_eq!(h.estimate(&key("a", 100), None), 50);
        assert_eq!(h.estimate(&key("b", 100), None), 900);
        assert_eq!(h.estimate(&key("a", 4000), None), 400);
        assert_eq!(h.len(), 3);
    }

    /// Verifies time-decay weighting toward recent observations.
    #[test]
    fn estimate_decays_toward_recent_observations() {
        let mut q = DecayingQuantile::new(1.0);
        q.observe(1000, 0.0);
        for i in 0..20 {
            q.observe(100, 10.0 + i as f64 * 0.1);
        }
        let est = q.estimate().unwrap();
        assert!(est < 500, "estimate ({est}) should have decayed toward 100");
    }

    /// Verifies prompt length bucketing to power-of-two bounds.
    #[test]
    fn prompt_bucket_rounds_to_power_of_two() {
        assert_eq!(prompt_bucket(1), 1);
        assert_eq!(prompt_bucket(100), 64);
        assert_eq!(prompt_bucket(512), 512);
        assert_eq!(prompt_bucket(1000), 512);
        assert_eq!(prompt_bucket(8192), 4096);
    }

    /// Verifies thread safety and concurrent updates across worker threads.
    #[test]
    fn shared_store_is_usable_from_many_threads() {
        let h = Arc::new(RouteHistograms::new(300.0));
        let mut handles = vec![];
        for t in 0..8 {
            let h = Arc::clone(&h);
            handles.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    let k = key("m", 100);
                    h.observe(&k, 100 + t);
                    let _ = h.estimate(&k, None);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let est = h.estimate(&key("m", 100), None);
        assert!((100..=108).contains(&est), "estimate {est} outside observed range");
    }
}
