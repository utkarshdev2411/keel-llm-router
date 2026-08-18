use std::sync::atomic::Ordering::Relaxed;

use crate::backend::{BackendId, Snapshot};
use crate::features::RequestFeatures;

#[derive(Default)]
pub struct DecisionTrace {
    pub strategy: &'static str,
    pub chosen: Option<BackendId>,
    pub candidates: Vec<CandidateScore>,
    pub prompt_tokens: u32,
    pub expected_output_tokens: u32,
    pub fell_through: bool,
}

pub struct CandidateScore {
    pub backend: BackendId,
    pub score: f64,
    pub occupancy: f64,
    pub inflight: u32,
    pub kv_projected: i64,
    pub gated_by: Option<GateReason>,
    pub reported_kv: Option<f32>,
    pub signal_age_ms: Option<u64>,
}

pub enum GateReason {
    KvHeadroom,
    SlotLimit,
}

/// Shared by every strategy's `pick`, so trace population isn't duplicated
/// per implementation. Baseline strategies have no occupancy/score concept
/// yet (Phase 1, no cost model), so `occupancy`/`score` read 0.0 and
/// `gated_by` is always `None` until Phase 2 introduces `pressure`.
pub fn record_pick(
    trace: Option<&mut DecisionTrace>,
    strategy: &'static str,
    snap: &Snapshot,
    req: &RequestFeatures,
    chosen: Option<BackendId>,
) {
    let Some(trace) = trace else { return };
    trace.strategy = strategy;
    trace.chosen = chosen;
    trace.prompt_tokens = req.prompt_tokens;
    trace.expected_output_tokens = req.expected_output_tokens;
    trace.fell_through = false;
    trace.candidates = snap
        .healthy
        .iter()
        .map(|&id| {
            let b = &snap.backends[id.0 as usize];
            CandidateScore {
                backend: id,
                score: 0.0,
                occupancy: 0.0,
                inflight: b.live.inflight.load(Relaxed),
                kv_projected: b.live.kv_projected_tokens.load(Relaxed),
                gated_by: None,
                reported_kv: None,
                signal_age_ms: None,
            }
        })
        .collect();
}
