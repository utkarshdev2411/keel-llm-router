use std::sync::atomic::Ordering::Relaxed;

use smallvec::SmallVec;

use crate::backend::{BackendId, Snapshot};

/// Would this backend remain within its safety ceiling after taking `need_kv`?
///
/// Both terms are router-local and current. Scraped `kv_usage_perc` is deliberately
/// NOT an input: it is sampled up to a scrape-interval ago and already includes
/// requests the router still holds a lease for, so mixing the two is simultaneously
/// stale and double-counting. (Algorithm spec §8, LLD §4.1)
#[inline]
pub fn admits(b: &crate::backend::Backend, need_kv: i64, sigma: f64) -> bool {
    let projected = b.live.kv_projected_tokens.load(Relaxed) + need_kv;
    let kv_ok = projected <= (sigma * b.caps.kv_capacity_tokens as f64) as i64;
    let slot_ok = b.live.inflight.load(Relaxed) < b.caps.max_num_seqs;
    kv_ok && slot_ok
}

/// Collect backends in `snap.healthy` that pass the gate into `out`.
///
/// An EMPTY result is a valid, expected outcome — not an error. The caller
/// (pressure::pick) is responsible for the fall-through: dispatch to the
/// least-bad backend anyway. The gate never refuses a request.
pub fn eligible(
    snap: &Snapshot,
    need_kv: i64,
    sigma: f64,
    out: &mut SmallVec<[BackendId; 16]>,
) {
    out.clear();
    for &id in snap.healthy.iter() {
        let b = &snap.backends[id.0 as usize];
        if admits(b, need_kv, sigma) {
            out.push(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, CapsEstimate, HealthState, LiveCounters};
    use arc_swap::ArcSwapOption;
    use std::sync::Arc;

    fn backend(id: u16, inflight: u32, kv_projected: i64, kv_capacity: u32, max_seqs: u32) -> Arc<Backend> {
        let b = Backend {
            id: BackendId(id),
            key: format!("b{id}").into(),
            uri: "http://x".into(),
            model: "m".into(),
            weight: 1.0,
            caps: CapsEstimate { kv_capacity_tokens: kv_capacity, max_num_seqs: max_seqs },
            live: LiveCounters::default(),
            reported: ArcSwapOption::from(None),
            health: HealthState::default(),
        };
        b.live.inflight.store(inflight, Relaxed);
        b.live.kv_projected_tokens.store(kv_projected, Relaxed);
        Arc::new(b)
    }

    fn snap(backends: Vec<Arc<Backend>>) -> Snapshot {
        let healthy: Box<[BackendId]> = backends.iter().map(|b| b.id).collect();
        Snapshot { epoch: 0, backends, healthy, ring: Box::new([]) }
    }

    /// The gate makes a backend **ineligible** — it does not reject a request.
    /// Naming matters: a "gate_rejects_" test asserts the wrong behaviour.
    #[test]
    fn gate_marks_backend_ineligible_when_projection_exceeds_sigma() {
        // Backend has 8192 KV capacity, sigma=0.95 → ceiling = 7782 tokens.
        // Already holding 7800 tokens → adding 100 more would exceed the ceiling.
        let b = backend(0, 0, 7800, 8192, 32);
        assert!(!admits(&b, 100, 0.95),
            "backend exceeding sigma*kv_capacity must be ineligible");
        // But a small request (50 tokens) that keeps total under ceiling is fine.
        // 7800 + 50 = 7850 > 7782 — still over
        assert!(!admits(&b, 50, 0.95));
        // Zero-cost request (need_kv=0) also blocked because current > ceiling
        // 7800 > 7782
        assert!(!admits(&b, 0, 0.95));
    }

    /// The gate checks BOTH conditions. A backend with free KV but no slot room
    /// must also be marked ineligible.
    #[test]
    fn gate_also_refuses_on_slot_limit_not_just_kv() {
        // Backend: plenty of KV (only 100/8192 used), but all slots taken (32/32).
        let b = backend(0, 32, 100, 8192, 32);
        assert!(!admits(&b, 50, 0.95),
            "backend at max_num_seqs must be ineligible even with free KV");
    }

    /// An empty eligible set is a valid outcome, not an error.
    /// The pressure strategy's fall-through handles it.
    #[test]
    fn empty_eligible_set_is_a_valid_outcome_not_an_error() {
        // All backends saturated
        let backends = vec![
            backend(0, 32, 8192, 8192, 32), // slots full AND kv full
            backend(1, 32, 8192, 8192, 32),
        ];
        let s = snap(backends);
        let mut out: SmallVec<[BackendId; 16]> = SmallVec::new();
        eligible(&s, 100, 0.95, &mut out);
        assert!(out.is_empty(), "empty eligible set must be accepted as a valid outcome");
    }

    /// A backend within both limits passes the gate.
    #[test]
    fn healthy_backend_is_eligible() {
        let b = backend(0, 5, 1000, 8192, 32);
        // 1000 + 200 = 1200 <= 0.95 * 8192 = 7782 → ok
        // 5 < 32 → ok
        assert!(admits(&b, 200, 0.95));
    }

    /// eligible() only includes backends that pass both conditions.
    #[test]
    fn eligible_filters_correctly() {
        let backends = vec![
            backend(0, 5, 1000, 8192, 32),  // passes
            backend(1, 32, 100, 8192, 32),  // slots full → fails
            backend(2, 0, 7900, 8192, 32),  // kv too high → fails
        ];
        let s = snap(backends);
        let mut out: SmallVec<[BackendId; 16]> = SmallVec::new();
        eligible(&s, 50, 0.95, &mut out);
        assert_eq!(out.as_slice(), &[BackendId(0)]);
    }
}
