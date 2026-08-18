use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use router_core::backend::{Backend, BackendId, Snapshot};

const CONSECUTIVE_FAILURES_TO_EJECT: u32 = 5;

pub fn record_failure(backend: &Backend) {
    let n = backend.health.consecutive_failures.fetch_add(1, Relaxed) + 1;
    if n >= CONSECUTIVE_FAILURES_TO_EJECT {
        backend.health.ejected.store(true, Relaxed);
    }
}

pub fn record_success(backend: &Backend) {
    backend.health.consecutive_failures.store(0, Relaxed);
    backend.health.ejected.store(false, Relaxed);
}

/// Rebuild `healthy` from current ejection state. Called after any health
/// change; the result feeds a fresh `Snapshot` swap, never a mutation of a
/// live one.
pub fn rebuild_healthy(backends: &[Arc<Backend>]) -> Box<[BackendId]> {
    backends
        .iter()
        .filter(|b| !b.health.ejected.load(Relaxed))
        .map(|b| b.id)
        .collect()
}

pub fn next_snapshot(prev: &Snapshot) -> Snapshot {
    Snapshot {
        epoch: prev.epoch + 1,
        healthy: rebuild_healthy(&prev.backends),
        backends: prev.backends.clone(),
        ring: prev.ring.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use router_core::backend::{CapsEstimate, HealthState, LiveCounters};
    use arc_swap::ArcSwapOption;

    fn backend(id: u16) -> Arc<Backend> {
        Arc::new(Backend {
            id: BackendId(id),
            key: format!("b{id}").into(),
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
    fn ejects_after_consecutive_failures() {
        let b = backend(0);
        for _ in 0..CONSECUTIVE_FAILURES_TO_EJECT - 1 {
            record_failure(&b);
        }
        assert!(!b.health.ejected.load(Relaxed));
        record_failure(&b);
        assert!(b.health.ejected.load(Relaxed));
    }

    #[test]
    fn success_resets_failure_count_and_ejection() {
        let b = backend(0);
        for _ in 0..CONSECUTIVE_FAILURES_TO_EJECT {
            record_failure(&b);
        }
        assert!(b.health.ejected.load(Relaxed));
        record_success(&b);
        assert!(!b.health.ejected.load(Relaxed));
        assert_eq!(b.health.consecutive_failures.load(Relaxed), 0);
    }

    #[test]
    fn rebuild_excludes_ejected_backends() {
        let b0 = backend(0);
        let b1 = backend(1);
        for _ in 0..CONSECUTIVE_FAILURES_TO_EJECT {
            record_failure(&b1);
        }
        let healthy = rebuild_healthy(&[b0.clone(), b1.clone()]);
        assert_eq!(healthy.as_ref(), &[BackendId(0)]);
    }
}
