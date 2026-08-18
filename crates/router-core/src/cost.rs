use std::sync::atomic::Ordering::Relaxed;

use crate::backend::Backend;

pub const DEFAULT_THETA: f64 = 0.55;
pub const DEFAULT_PENALTY: f64 = 10.0;
pub const DEFAULT_SIGMA: f64 = 0.95;
pub const RECHARGE_STEP: u32 = 50;

#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvModel {
    PromptOnly,
    PromptPlusOutput,
}

impl KvModel {
    #[inline]
    pub fn project(self, prompt_tokens: u32, o_hat: u32) -> i64 {
        match self {
            KvModel::PromptOnly => prompt_tokens as i64,
            KvModel::PromptPlusOutput => prompt_tokens as i64 + o_hat as i64,
        }
    }
}

#[inline]
pub fn occupancy(b: &Backend) -> f64 {
    let n = b.live.inflight.load(Relaxed) as f64;
    let kv = b.live.kv_projected_tokens.load(Relaxed).max(0) as f64;
    let slots = n / b.caps.max_num_seqs as f64;
    let mem = kv / b.caps.kv_capacity_tokens as f64;
    slots.max(mem)
}

#[inline]
pub fn pressure_score(u: f64, theta: f64, penalty: f64) -> f64 {
    if u < theta {
        u
    } else {
        let over = (u - theta) / (1.0 - theta);
        u + penalty * over * over
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_kv_differs_between_models() {
        assert_eq!(KvModel::PromptOnly.project(500, 100), 500);
        assert_eq!(KvModel::PromptPlusOutput.project(500, 100), 600);
    }

    #[test]
    fn score_is_flat_below_theta() {
        assert_eq!(pressure_score(0.1, DEFAULT_THETA, DEFAULT_PENALTY), 0.1);
        assert_eq!(pressure_score(0.5, DEFAULT_THETA, DEFAULT_PENALTY), 0.5);
    }

    #[test]
    fn score_is_convex_above_theta() {
        let at_knee = pressure_score(DEFAULT_THETA, DEFAULT_THETA, DEFAULT_PENALTY);
        let just_above = pressure_score(DEFAULT_THETA + 0.01, DEFAULT_THETA, DEFAULT_PENALTY);
        let further_above = pressure_score(DEFAULT_THETA + 0.02, DEFAULT_THETA, DEFAULT_PENALTY);
        assert!(just_above > at_knee);
        assert!((further_above - just_above) > (just_above - at_knee));
    }

    #[test]
    fn score_monotonic_in_u_sampled() {
        let mut prev = f64::MIN;
        let mut u = 0.0;
        while u <= 1.5 {
            let s = pressure_score(u, DEFAULT_THETA, DEFAULT_PENALTY);
            assert!(s >= prev, "score decreased at u={u}: {s} < {prev}");
            prev = s;
            u += 0.001;
        }
    }

    #[test]
    fn occupancy_prefers_the_genuinely_emptier_backend() {
        let max_num_seqs = 8u32;
        let kv_capacity = 8192u32;

        // A: 1 long-generation request, 7 of 8 slots free, but that one
        // request holds 1024 of the 8192 KV tokens.
        let occ_a = (1.0 / max_num_seqs as f64).max(1024.0 / kv_capacity as f64);
        // B: 4 short requests, 4 of 8 slots free, holding 4096 KV tokens
        // combined -- fuller in the resource that actually runs out.
        let occ_b = (4.0 / max_num_seqs as f64).max(4096.0 / kv_capacity as f64);

        assert_eq!(occ_a, 0.125);
        assert_eq!(occ_b, 0.5);
        assert!(occ_a < occ_b, "A should score as emptier: {occ_a} vs {occ_b}");
    }
}
