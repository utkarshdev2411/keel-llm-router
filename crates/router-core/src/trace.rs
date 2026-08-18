use crate::backend::BackendId;

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
