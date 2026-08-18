use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64};
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwapOption;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct BackendId(pub u16);

pub struct Snapshot {
    pub epoch: u64,
    pub backends: Vec<Arc<Backend>>,
    pub healthy: Box<[BackendId]>,
    pub ring: Box<[(u64, BackendId)]>,
}

pub struct Backend {
    pub id: BackendId,
    pub key: Arc<str>,
    pub uri: Arc<str>,
    pub model: Arc<str>,
    pub weight: f32,
    pub caps: CapsEstimate,
    pub live: LiveCounters,
    pub reported: ArcSwapOption<ReportedLoad>,
    pub health: HealthState,
}

#[derive(Copy, Clone, Debug)]
pub struct CapsEstimate {
    pub kv_capacity_tokens: u32,
    pub max_num_seqs: u32,
}

#[repr(align(64))]
pub struct LiveCounters {
    pub inflight: AtomicU32,
    pub kv_projected_tokens: AtomicI64,
    pub total_dispatched: AtomicU64,
    pub ewma_ttft_us: AtomicU64,
    pub ewma_itl_us: AtomicU64,
}

impl Default for LiveCounters {
    fn default() -> Self {
        Self {
            inflight: AtomicU32::new(0),
            kv_projected_tokens: AtomicI64::new(0),
            total_dispatched: AtomicU64::new(0),
            ewma_ttft_us: AtomicU64::new(0),
            ewma_itl_us: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReportedLoad {
    pub observed_at: Instant,
    pub kv_usage_perc: Option<f32>,
    pub num_running: Option<u32>,
    pub num_waiting: Option<u32>,
    pub preemptions: Option<u64>,
    pub prefix_hit_rate: Option<f32>,
}

impl ReportedLoad {
    pub fn is_stale(&self, now: Instant, max_age: std::time::Duration) -> bool {
        now.saturating_duration_since(self.observed_at) > max_age
    }
}

#[derive(Debug)]
pub struct HealthState {
    pub consecutive_failures: AtomicU32,
    pub ejected: std::sync::atomic::AtomicBool,
}

impl Default for HealthState {
    fn default() -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            ejected: std::sync::atomic::AtomicBool::new(false),
        }
    }
}
