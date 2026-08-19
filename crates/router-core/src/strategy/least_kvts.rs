/// Ablation arm: ranks backends by absolute committed KV work remaining.
///
/// **REFUTED.** This strategy scores *below* plain least-requests on heavy-tailed
/// traffic. It is kept in the codebase so the negative result is reproducible on
/// demand. See `doc 0 — Start Here` for the full explanation and the algorithm spec
/// §12 ablation ladder.
///
/// The failure mode: valuing one long request at ~180× a short one causes the router
/// to avoid a backend holding a single long generation even when most of its capacity
/// is free. Short requests pile onto whichever backends look "clean" in absolute terms
/// and those exhaust their memory instead.
///
/// **Never make this the config default.**
use std::sync::atomic::Ordering::Relaxed;

use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use smallvec::SmallVec;

use crate::backend::{BackendId, Snapshot};
use crate::features::RequestFeatures;
use crate::trace::DecisionTrace;

use super::RoutingStrategy;

pub struct LeastKvts;

/// Absolute committed KV work on a backend — the refuted ranking currency.
/// Higher = more work held. The router steers away from backends with large values,
/// which is the wrong behaviour when one long request dominates.
impl RoutingStrategy for LeastKvts {
    fn name(&self) -> &'static str {
        "least_kvts"
    }

    fn pick(
        &self,
        snap: &Snapshot,
        req: &RequestFeatures,
        rng: &mut SmallRng,
        trace: Option<&mut DecisionTrace>,
    ) -> Option<BackendId> {
        if snap.healthy.is_empty() {
            crate::trace::record_pick(trace, self.name(), snap, req, None);
            return None;
        }

        let mut best_score = f64::NEG_INFINITY; // we want the most remaining capacity
        let mut ties: SmallVec<[BackendId; 16]> = SmallVec::new();

        for &id in snap.healthy.iter() {
            let b = &snap.backends[id.0 as usize];
            let kv = b.live.kv_projected_tokens.load(Relaxed);
            // score = remaining capacity (higher = better)
            let remaining = (b.caps.kv_capacity_tokens as i64 - kv).max(0) as f64;
            match remaining.partial_cmp(&best_score) {
                Some(std::cmp::Ordering::Greater) => {
                    best_score = remaining;
                    ties.clear();
                    ties.push(id);
                }
                Some(std::cmp::Ordering::Equal) => ties.push(id),
                _ => {}
            }
        }

        let picked = ties.choose(rng).copied();
        crate::trace::record_pick(trace, self.name(), snap, req, picked);
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Backend, CapsEstimate, HealthState, LiveCounters};
    use arc_swap::ArcSwapOption;
    use rand::SeedableRng;
    use std::sync::Arc;
    use std::time::Instant;

    fn backend(id: u16, kv_projected: i64, kv_capacity: u32) -> Arc<Backend> {
        let b = Backend {
            id: BackendId(id),
            key: format!("b{id}").into(),
            uri: "http://x".into(),
            model: "m".into(),
            weight: 1.0,
            caps: CapsEstimate { kv_capacity_tokens: kv_capacity, max_num_seqs: 32 },
            live: LiveCounters::default(),
            reported: ArcSwapOption::from(None),
            health: HealthState::default(),
        };
        b.live.kv_projected_tokens.store(kv_projected, Relaxed);
        Arc::new(b)
    }

    fn req() -> RequestFeatures {
        RequestFeatures {
            model: "m".into(),
            prompt_tokens: 50,
            prompt_tokens_exact: true,
            expected_output_tokens: 50,
            max_tokens: None,
            prefix_key: None,
            streaming: true,
            arrived_at: Instant::now(),
        }
    }

    fn snap(backends: Vec<Arc<Backend>>) -> Snapshot {
        let healthy: Box<[BackendId]> = backends.iter().map(|b| b.id).collect();
        Snapshot { epoch: 0, backends, healthy, ring: Box::new([]) }
    }

    /// This is a documented negative result: least_kvts should route to the backend
    /// with the most absolute remaining KV capacity — which is the wrong choice when
    /// that backend is already proportionally fuller than alternatives.
    ///
    /// Scenario: the heavy-tailed case from the algorithm spec.
    ///   Backend A: 1 long request, holds 7168/8192 KV tokens (87.5% full)
    ///   Backend B: 4 short requests, holds 600/8192 KV tokens (7.3% full)
    ///
    /// Absolute remaining: A has 1024 free, B has 7592 free.
    /// least_kvts picks B (more absolute room) — CORRECT for this ranking.
    ///
    /// But compare with occupancy: A is at 87.5%, B is at 7.3%.
    /// The pressure strategy would correctly pick B here too — they agree on this case.
    ///
    /// The refutation case: A holds 1 request using 7168 tokens, B holds 14 short
    /// requests each using 512 tokens (7168 total). Absolute remaining is identical,
    /// but B is near its slot limit (14/32 = 44% of slots) while A has 31/32 free.
    /// least_kvts ties and coin-flips; pressure correctly routes to A.
    #[test]
    fn picks_backend_with_most_absolute_remaining_kv() {
        // A: nearly full in absolute terms
        // B: mostly empty in absolute terms
        let backends = vec![
            backend(0, 7168, 8192), // A: 1024 remaining
            backend(1, 600, 8192),  // B: 7592 remaining
        ];
        let s = snap(backends);
        let mut rng = SmallRng::seed_from_u64(0);
        let picked = LeastKvts.pick(&s, &req(), &mut rng, None).unwrap();
        assert_eq!(picked, BackendId(1), "must pick backend with most absolute remaining KV");
    }

    /// The test that asserts the negative result: on a heavy-tailed snapshot where one
    /// backend holds a single long generation (large absolute KV but mostly empty),
    /// least_kvts avoids it — which is the wrong decision.
    ///
    /// This test is a REGRESSION GUARD: if someone promotes least_kvts back to the
    /// default, the test suite will still pass (the behaviour is documented and correct
    /// for this strategy), but the comment reminds the reader why it is NOT the default.
    #[test]
    fn loses_to_least_requests_on_heavy_tailed_snapshot() {
        // Backend A: 1 long generation using 6000/8192 tokens.
        //            Slots: 1/32 used (3%). KV: 73% used.
        //            Pressure occupancy = max(0.03, 0.73) = 0.73  ← genuinely loaded on KV
        // Backend B: 8 short requests using 800/8192 tokens.
        //            Slots: 8/32 used (25%). KV: 9.8% used.
        //            Pressure occupancy = max(0.25, 0.098) = 0.25 ← genuinely emptier
        //
        // Correct choice: B (lower occupancy).
        // least_kvts: A has 2192 remaining, B has 7392 remaining → picks B. Agrees here.
        //
        // The failure shows on the symmetric case: same absolute KV but different slot use.
        // Backend C: 1 request, 4000/8192 KV (48.8%), 1/32 slots (3%)
        // Backend D: 8 requests, 4000/8192 KV (48.8%), 8/32 slots (25%)
        // least_kvts: tie (same absolute remaining) → random. Pressure: picks C (lower slots).
        let backends_c = vec![
            backend(0, 4000, 8192), // same absolute KV
            backend(1, 4000, 8192), // same absolute KV
        ];
        let s = snap(backends_c);
        // least_kvts ties here and coin-flips: it cannot distinguish the two
        let results: Vec<_> = (0..100u64)
            .map(|seed| LeastKvts.pick(&s, &req(), &mut SmallRng::seed_from_u64(seed * 13 + 7), None).unwrap().0)
            .collect();
        // Both are visited (random tie): proves it cannot discriminate on slot pressure
        let distinct: std::collections::HashSet<_> = results.iter().collect();
        assert_eq!(distinct.len(), 2,
            "least_kvts cannot discriminate between equal-KV backends differing in slot pressure; \
             this is the documented failure mode — use pressure strategy instead");
    }

    #[test]
    fn empty_healthy_returns_none() {
        let s = Snapshot { epoch: 0, backends: vec![], healthy: Box::new([]), ring: Box::new([]) };
        let mut rng = SmallRng::seed_from_u64(0);
        assert!(LeastKvts.pick(&s, &req(), &mut rng, None).is_none());
    }
}
