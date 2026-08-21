use std::sync::atomic::Ordering::Relaxed;

use smallvec::SmallVec;

use crate::backend::{BackendId, Snapshot};

/// Checks if a backend can admit `need_kv` tokens without exceeding its KV safety threshold or slot limit.
///
/// Uses router-local live counters (`kv_projected_tokens` and `inflight`) rather than scraped metrics
/// to prevent double-counting active leases and using stale telemetry.
#[inline]
pub fn admits(b: &crate::backend::Backend, need_kv: i64, sigma: f64) -> bool {
    let projected = b.live.kv_projected_tokens.load(Relaxed) + need_kv;
    let kv_ok = projected <= (sigma * b.caps.kv_capacity_tokens as f64) as i64;
    let slot_ok = b.live.inflight.load(Relaxed) < b.caps.max_num_seqs;
    kv_ok && slot_ok
}

/// Populates `out` with all healthy backends that pass admission control.
///
/// Returning an empty set is a valid outcome; the routing strategy handles fall-through
/// to dispatch to the best available backend when all backends exceed limits.
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

    #[test]
    fn gate_marks_backend_ineligible_when_projection_exceeds_sigma() {
        let b = backend(0, 0, 7800, 8192, 32);
        assert!(!admits(&b, 100, 0.95),
            "backend exceeding sigma*kv_capacity must be ineligible");
        assert!(!admits(&b, 50, 0.95));
        assert!(!admits(&b, 0, 0.95));
    }

    #[test]
    fn gate_also_refuses_on_slot_limit_not_just_kv() {
        let b = backend(0, 32, 100, 8192, 32);
        assert!(!admits(&b, 50, 0.95),
            "backend at max_num_seqs must be ineligible even with free KV");
    }

    #[test]
    fn empty_eligible_set_is_a_valid_outcome_not_an_error() {
        let backends = vec![
            backend(0, 32, 8192, 8192, 32),
            backend(1, 32, 8192, 8192, 32),
        ];
        let s = snap(backends);
        let mut out: SmallVec<[BackendId; 16]> = SmallVec::new();
        eligible(&s, 100, 0.95, &mut out);
        assert!(out.is_empty(), "empty eligible set must be accepted as a valid outcome");
    }

    #[test]
    fn healthy_backend_is_eligible() {
        let b = backend(0, 5, 1000, 8192, 32);
        assert!(admits(&b, 200, 0.95));
    }

    #[test]
    fn eligible_filters_correctly() {
        let backends = vec![
            backend(0, 5, 1000, 8192, 32),
            backend(1, 32, 100, 8192, 32),
            backend(2, 0, 7900, 8192, 32),
        ];
        let s = snap(backends);
        let mut out: SmallVec<[BackendId; 16]> = SmallVec::new();
        eligible(&s, 50, 0.95, &mut out);
        assert_eq!(out.as_slice(), &[BackendId(0)]);
    }
}
