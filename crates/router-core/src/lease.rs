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
    pub fn open(
        backend: Arc<Backend>,
        prompt_tokens: u32,
        o_hat: u32,
        kv_model: KvModel,
        now: Instant,
    ) -> Self {
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
    ///
    /// Strictly greater than, not `>=`: under `output_model = echo` the
    /// estimate equals the prompt length exactly, so `>=` fires a spurious
    /// recharge on every single request. (LLD §5.2)
    ///
    /// Under `KvModel::PromptOnly` the delta is always zero — the projection
    /// does not depend on `o_hat` at all. That is correct and intentional;
    /// do not "fix" it. (LLD §5.2)
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

    /// Idempotent. Called by Drop.
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

/// Debug-build quiescent invariant: when no requests are in-flight, no KV
/// tokens should be projected.
///
/// A violation is silent and cumulative — it presents as a backend gradually
/// receiving less traffic, which is indistinguishable from a genuine
/// algorithmic result until someone checks the counter. This is the single
/// most expensive bug available in this project. (LLD §2.2, algorithm spec §13)
///
/// Call at the end of integration tests and after draining a run.
/// Cheaper than a per-lease sum and catches the same class of leak.
#[cfg(debug_assertions)]
pub fn assert_invariant(backend: &Backend) {
    let inflight = backend.live.inflight.load(Relaxed);
    let kv_projected = backend.live.kv_projected_tokens.load(Relaxed);
    assert!(
        inflight != 0 || kv_projected == 0,
        "lease invariant violated on backend {:?}: \
         inflight={inflight} but kv_projected={kv_projected} (must be 0 when inflight=0). \
         This is a lease leak — charge was not released on some exit path.",
        backend.key,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendId, CapsEstimate, HealthState, LiveCounters};
    use arc_swap::ArcSwapOption;

    fn test_backend() -> Arc<Backend> {
        Arc::new(Backend {
            id: BackendId(0),
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
            let mut lease =
                CostLease::open(b.clone(), 100, 50, KvModel::PromptOnly, Instant::now());
            lease.observe_tokens(10);
        }
        assert_eq!(b.live.inflight.load(Relaxed), 0);
        assert_eq!(b.live.kv_projected_tokens.load(Relaxed), 0);
        #[cfg(debug_assertions)]
        assert_invariant(&b);
    }

    #[test]
    fn charge_release_symmetric_on_early_drop() {
        let b = test_backend();
        let lease = CostLease::open(b.clone(), 100, 50, KvModel::PromptOnly, Instant::now());
        drop(lease);
        assert_eq!(b.live.inflight.load(Relaxed), 0);
        assert_eq!(b.live.kv_projected_tokens.load(Relaxed), 0);
        #[cfg(debug_assertions)]
        assert_invariant(&b);
    }

    #[test]
    fn recharge_is_noop_under_prompt_only() {
        let b = test_backend();
        let mut lease =
            CostLease::open(b.clone(), 100, 10, KvModel::PromptOnly, Instant::now());
        let before = b.live.kv_projected_tokens.load(Relaxed);
        lease.observe_tokens(20);
        assert_eq!(
            b.live.kv_projected_tokens.load(Relaxed),
            before,
            "recharge must be a no-op under PromptOnly"
        );
    }

    /// Strictly greater than, not >=: observing exactly o_hat tokens must not recharge.
    #[test]
    fn recharge_fires_strictly_greater_not_gte() {
        let b = test_backend();
        let mut lease =
            CostLease::open(b.clone(), 100, 10, KvModel::PromptPlusOutput, Instant::now());
        let at_estimate = b.live.kv_projected_tokens.load(Relaxed);
        // tokens_seen == o_hat (10), not strictly greater — must NOT recharge
        lease.observe_tokens(10);
        assert_eq!(
            b.live.kv_projected_tokens.load(Relaxed),
            at_estimate,
            "observe_tokens at exactly o_hat must not recharge (strictly > rule)"
        );
        // tokens_seen == 11 > 10 — must recharge now
        lease.observe_tokens(1);
        assert!(
            b.live.kv_projected_tokens.load(Relaxed) > at_estimate,
            "observe_tokens past o_hat must trigger recharge"
        );
    }

    #[test]
    fn recharge_fires_under_prompt_plus_output() {
        let b = test_backend();
        let mut lease =
            CostLease::open(b.clone(), 100, 10, KvModel::PromptPlusOutput, Instant::now());
        let before = b.live.kv_projected_tokens.load(Relaxed);
        lease.observe_tokens(11);
        assert!(b.live.kv_projected_tokens.load(Relaxed) > before);
    }

    #[test]
    fn no_leak_under_many_random_lifecycles() {
        let b = test_backend();
        for i in 0..500u32 {
            let mut lease = CostLease::open(
                b.clone(),
                50 + i % 20,
                30,
                KvModel::PromptPlusOutput,
                Instant::now(),
            );
            for _ in 0..(i % 7) {
                lease.observe_tokens(5);
            }
            if i % 3 == 0 {
                drop(lease);
            }
        }
        assert_eq!(b.live.inflight.load(Relaxed), 0);
        assert_eq!(b.live.kv_projected_tokens.load(Relaxed), 0);
        #[cfg(debug_assertions)]
        assert_invariant(&b);
    }

    /// assert_invariant must panic when kv_projected is non-zero with inflight==0.
    #[test]
    #[cfg(debug_assertions)]
    fn assert_invariant_fires_on_leaked_state() {
        let b = test_backend();
        b.live.kv_projected_tokens.store(999, Relaxed);
        // inflight=0, kv_projected=999 → must panic
        let result = std::panic::catch_unwind(|| assert_invariant(&b));
        assert!(
            result.is_err(),
            "assert_invariant must panic when kv_projected != 0 and inflight == 0"
        );
    }
}
