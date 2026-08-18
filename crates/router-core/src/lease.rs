use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;

use crate::backend::Backend;
use crate::cost::KvModel;

/// Not Clone, not Copy. Exactly one release per open, on every exit path.
pub struct CostLease {
    backend: Arc<Backend>,
    charged: i64,
    kv_model: KvModel,
    prompt_tokens: u32,
    o_hat: u32,
    tokens_seen: u32,
    dispatched_at: Instant,
    released: bool,
}

impl CostLease {
    pub fn open(backend: Arc<Backend>, prompt_tokens: u32, o_hat: u32,
                kv_model: KvModel, now: Instant) -> Self {
        let charged = kv_model.project(prompt_tokens, o_hat);
        backend.live.inflight.fetch_add(1, Relaxed);
        backend.live.kv_projected_tokens.fetch_add(charged, Relaxed);

        Self {
            backend,
            charged,
            kv_model,
            prompt_tokens,
            o_hat,
            tokens_seen: 0,
            dispatched_at: now,
            released: false,
        }
    }

    pub fn dispatched_at(&self) -> Instant {
        self.dispatched_at
    }

    /// Called once per content-bearing SSE frame. The charge is a peak
    /// projection, not a running debt: it is not drawn down as tokens
    /// arrive, only revised upward if the output overruns the estimate.
    /// Strictly greater than, not >=, or an estimate that exactly matches
    /// the eventual output fires a spurious recharge on every request.
    pub fn observe_tokens(&mut self, n: u32) {
        self.tokens_seen += n;
        if self.tokens_seen > self.o_hat {
            const STEP: u32 = crate::cost::RECHARGE_STEP;
            self.o_hat += STEP;
            let new_charge = self.kv_model.project(self.prompt_tokens, self.o_hat);
            let delta = new_charge - self.charged;
            self.charged = new_charge;
            if delta != 0 {
                self.backend.live.kv_projected_tokens.fetch_add(delta, Relaxed);
            }
        }
    }

    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.backend.live.inflight.fetch_sub(1, Relaxed);
        self.backend.live.kv_projected_tokens.fetch_sub(self.charged, Relaxed);
        self.charged = 0;
    }
}

impl Drop for CostLease {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{CapsEstimate, HealthState, LiveCounters};
    use arc_swap::ArcSwapOption;

    fn test_backend() -> Arc<Backend> {
        Arc::new(Backend {
            id: crate::backend::BackendId(0),
            key: "b0".into(),
            uri: "http://x".into(),
            model: "m".into(),
            weight: 1.0,
            caps: CapsEstimate { kv_capacity_tokens: 8192, max_num_seqs: 32 },
            live: LiveCounters::default(),
            reported: ArcSwapOption::from(None),
            health: HealthState::default(),
        })
    }

    #[test]
    fn charge_release_symmetric_on_normal_completion() {
        let b = test_backend();
        {
            let mut lease = CostLease::open(b.clone(), 100, 50, KvModel::PromptOnly, Instant::now());
            lease.observe_tokens(10);
        }
        assert_eq!(b.live.inflight.load(Relaxed), 0);
        assert_eq!(b.live.kv_projected_tokens.load(Relaxed), 0);
    }

    #[test]
    fn charge_release_symmetric_on_early_drop() {
        let b = test_backend();
        let lease = CostLease::open(b.clone(), 100, 50, KvModel::PromptOnly, Instant::now());
        drop(lease);
        assert_eq!(b.live.inflight.load(Relaxed), 0);
        assert_eq!(b.live.kv_projected_tokens.load(Relaxed), 0);
    }

    #[test]
    fn recharge_is_noop_under_prompt_only() {
        let b = test_backend();
        let mut lease = CostLease::open(b.clone(), 100, 10, KvModel::PromptOnly, Instant::now());
        let before = b.live.kv_projected_tokens.load(Relaxed);
        lease.observe_tokens(20);
        assert_eq!(b.live.kv_projected_tokens.load(Relaxed), before);
    }

    #[test]
    fn recharge_fires_under_prompt_plus_output() {
        let b = test_backend();
        let mut lease = CostLease::open(b.clone(), 100, 10, KvModel::PromptPlusOutput, Instant::now());
        let before = b.live.kv_projected_tokens.load(Relaxed);
        lease.observe_tokens(11);
        assert!(b.live.kv_projected_tokens.load(Relaxed) > before);
    }

    #[test]
    fn no_leak_under_many_random_lifecycles() {
        let b = test_backend();
        for i in 0..500u32 {
            let mut lease = CostLease::open(b.clone(), 50 + i % 20, 30, KvModel::PromptPlusOutput, Instant::now());
            for _ in 0..(i % 7) {
                lease.observe_tokens(5);
            }
            if i % 3 == 0 {
                drop(lease);
            }
        }
        assert_eq!(b.live.inflight.load(Relaxed), 0);
        assert_eq!(b.live.kv_projected_tokens.load(Relaxed), 0);
    }
}
