//! Per-route output-length estimation.
//!
//! The estimate `ô` feeds both the KV projection and the admission gate. Priority:
//! the client's `max_tokens` when present (an exact upper bound, free), otherwise a
//! decaying per-route estimate of recently observed output lengths, otherwise a
//! conservative default.
//!
//! **One estimate.** The gate and the score use the same number. An earlier design
//! used a separate high-percentile estimate for admission only; the validated gate
//! does not do that, so there is deliberately no p95 here.
//!
//! **No learned model.** Published work found a coarse classifier at 61% accuracy
//! still delivered most of the available benefit. What makes this work is
//! self-correction — the recharge loop in `CostLease` revises the projection upward
//! when output overruns — not prediction precision.
//!
//! Note that under `kv_model = "prompt_only"` the estimate does not enter the
//! projection at all, so nothing here affects routing. It becomes load-bearing under
//! `prompt_plus_output`, which is what real vLLM needs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use router_core::features::{RequestFeatures, RouteKey};

/// A decaying estimate of output length for one route.
///
/// An exponential moving average in the *time* domain, not the observation domain:
/// weight decays by elapsed seconds, so a route that goes quiet for an hour and then
/// returns is not still anchored to what it looked like before. Deliberately
/// unsophisticated.
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

/// Conservative fallback before a route has any history. Under-estimates are
/// corrected upward by the recharge loop, so erring low is cheap.
pub const DEFAULT_OUTPUT_ESTIMATE: u32 = 128;

/// The per-route store, shared across all connections.
///
/// A plain `Mutex<HashMap>` rather than a sharded or lock-free map: the map holds one
/// entry per (model, prompt-size bucket) pair, so it is tens of entries, and the
/// critical section is a hash lookup plus a handful of float operations. At the
/// request rates this router targets that is far below the point where lock
/// contention is measurable against a ~68 ms TTFT. Revisit if
/// `router_overhead_seconds` p99 starts tracking concurrency.
pub struct RouteHistograms {
    started: Instant,
    halflife_s: f64,
    inner: Mutex<HashMap<RouteKey, DecayingQuantile>>,
}

impl RouteHistograms {
    pub fn new(halflife_s: f64) -> Self {
        Self { started: Instant::now(), halflife_s, inner: Mutex::new(HashMap::new()) }
    }

    /// Monotonic seconds since the store was created. Never a wall-clock read: a
    /// clock step backwards would make `elapsed` negative and corrupt every decay.
    fn now_s(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// Derive `ô` for one request. Always returns at least 1: a zero estimate makes
    /// the `prompt_plus_output` projection equal `prompt_only`, silently changing the
    /// effective KV model.
    pub fn estimate(&self, key: &RouteKey, max_tokens: Option<u32>) -> u32 {
        // The exact bound wins and costs no lock.
        if let Some(mt) = max_tokens {
            return mt.max(1);
        }
        let guard = match self.inner.lock() {
            Ok(g) => g,
            // A poisoned lock means another thread panicked mid-update. That is a bug
            // worth fixing, but refusing to route over it would turn an estimation
            // detail into an outage. Fall back to the default.
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .get(key)
            .and_then(|q| q.estimate())
            .map(|e| e.max(1))
            .unwrap_or(DEFAULT_OUTPUT_ESTIMATE)
    }

    /// Feed back an observed output length, from the stream's usage frame.
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

/// Route key for a request: model name plus a coarse prompt-length bucket.
///
/// Bucketing by prompt size matters because output length correlates with it, and a
/// single per-model estimate would average a 50-token prompt together with a
/// 4000-token one.
pub fn route_key_for(features: &RequestFeatures) -> RouteKey {
    route_key(&features.model, features.prompt_tokens)
}

/// Same, from the parts — used during feature construction, before a
/// `RequestFeatures` exists to borrow from.
pub fn route_key(model: &Arc<str>, prompt_tokens: u32) -> RouteKey {
    RouteKey(Arc::clone(model), prompt_bucket(prompt_tokens))
}

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

    #[test]
    fn max_tokens_always_wins_over_learned_estimate() {
        let h = RouteHistograms::new(300.0);
        let k = key("m", 100);
        h.observe(&k, 1000);
        assert_eq!(h.estimate(&k, Some(50)), 50);
    }

    #[test]
    fn estimate_falls_back_to_default_with_no_history() {
        let h = RouteHistograms::new(300.0);
        assert_eq!(h.estimate(&key("m", 100), None), DEFAULT_OUTPUT_ESTIMATE);
    }

    #[test]
    fn estimate_uses_observed_history_when_no_max_tokens() {
        let h = RouteHistograms::new(300.0);
        let k = key("m", 100);
        h.observe(&k, 256);
        assert_eq!(h.estimate(&k, None), 256);
    }

    #[test]
    fn zero_max_tokens_clamped_to_one() {
        // A zero estimate silently changes the effective KV model.
        let h = RouteHistograms::new(300.0);
        assert_eq!(h.estimate(&key("m", 100), Some(0)), 1);
    }

    /// Routes must not share an estimate: a short-prompt route and a long-prompt
    /// route on the same model have different output distributions.
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

    #[test]
    fn prompt_bucket_rounds_to_power_of_two() {
        assert_eq!(prompt_bucket(1), 1);
        assert_eq!(prompt_bucket(100), 64);
        assert_eq!(prompt_bucket(512), 512);
        assert_eq!(prompt_bucket(1000), 512);
        assert_eq!(prompt_bucket(8192), 4096);
    }

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
