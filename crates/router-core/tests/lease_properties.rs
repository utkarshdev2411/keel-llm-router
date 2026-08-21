//! Property-based testing for `CostLease` RAII lifecycle and counter invariants.
//!
//! Verifies that under arbitrary request interleaving, token streaming, and abrupt aborts (e.g. disconnects),
//! live counters (`inflight` and `kv_projected_tokens`) strictly return to zero when all leases are dropped.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use arc_swap::ArcSwapOption;
use proptest::prelude::*;
use router_core::backend::{Backend, BackendId, CapsEstimate, HealthState, LiveCounters};
use router_core::cost::KvModel;
use router_core::lease::CostLease;

fn backend() -> Arc<Backend> {
    Arc::new(Backend {
        id: BackendId(0),
        key: "b0".into(),
        uri: "http://x".into(),
        model: "m".into(),
        weight: 1.0,
        caps: CapsEstimate { kv_capacity_tokens: 1 << 20, max_num_seqs: 4096 },
        live: LiveCounters::default(),
        reported: ArcSwapOption::from(None),
        health: HealthState::default(),
    })
}

/// Simulated request lifecycle defining initial tokens, output streaming batches, and optional early cancellation point.
#[derive(Debug, Clone)]
struct Lifecycle {
    prompt: u32,
    o_hat: u32,
    prompt_plus_output: bool,
    batches: Vec<u32>,
    /// Step index after which the request is dropped to simulate client disconnects or tasks cancellation.
    abort_after: Option<usize>,
}

fn lifecycle() -> impl Strategy<Value = Lifecycle> {
    (
        1u32..4000,
        1u32..600,
        any::<bool>(),
        prop::collection::vec(1u32..40, 0..25),
        prop::option::of(0usize..25),
    )
        .prop_map(|(prompt, o_hat, ppo, batches, abort)| Lifecycle {
            prompt,
            o_hat,
            prompt_plus_output: ppo,
            batches,
            abort_after: abort,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// Asserts that inflight and projected KV counters return to zero regardless of lease execution order or early aborts.
    #[test]
    fn all_counters_return_to_zero_after_any_interleaving(
        lives in prop::collection::vec(lifecycle(), 1..30),
        release_rotation in 0usize..30,
    ) {
        let b = backend();
        let mut open: Vec<Option<CostLease>> = lives
            .iter()
            .map(|l| {
                let model = if l.prompt_plus_output {
                    KvModel::PromptPlusOutput
                } else {
                    KvModel::PromptOnly
                };
                Some(CostLease::open(b.clone(), l.prompt, l.o_hat, model, Instant::now()))
            })
            .collect();

        prop_assert_eq!(
            b.live.inflight.load(Relaxed), lives.len() as u32,
            "every opened lease must be counted in flight"
        );

        let deepest = lives.iter().map(|l| l.batches.len()).max().unwrap_or(0);
        for step in 0..deepest {
            for (i, l) in lives.iter().enumerate() {
                if l.abort_after == Some(step) {
                    open[i] = None;
                }
                if let (Some(lease), Some(&n)) = (open[i].as_mut(), l.batches.get(step)) {
                    lease.observe_tokens(n);
                }
            }
            prop_assert!(
                b.live.kv_projected_tokens.load(Relaxed) >= 0,
                "kv_projected went negative at step {}: {}",
                step, b.live.kv_projected_tokens.load(Relaxed)
            );
        }

        let n = open.len();
        for k in 0..n {
            open[(k + release_rotation) % n] = None;
        }

        prop_assert_eq!(b.live.inflight.load(Relaxed), 0, "inflight must drain to zero");
        prop_assert_eq!(
            b.live.kv_projected_tokens.load(Relaxed), 0,
            "kv_projected must drain to zero"
        );
    }

    /// Verifies single lease charge-release symmetry upon completion or drop.
    #[test]
    fn charge_and_release_are_symmetric_for_one_lease(l in lifecycle()) {
        let b = backend();
        let model = if l.prompt_plus_output {
            KvModel::PromptPlusOutput
        } else {
            KvModel::PromptOnly
        };
        {
            let mut lease = CostLease::open(b.clone(), l.prompt, l.o_hat, model, Instant::now());
            for (step, &n) in l.batches.iter().enumerate() {
                if l.abort_after == Some(step) {
                    break;
                }
                lease.observe_tokens(n);
            }
        }
        prop_assert_eq!(b.live.inflight.load(Relaxed), 0);
        prop_assert_eq!(b.live.kv_projected_tokens.load(Relaxed), 0);
    }

    /// Verifies that under PromptOnly mode, KV token projections remain constant as streaming tokens arrive.
    #[test]
    fn prompt_only_projection_never_moves_with_output(
        prompt in 1u32..4000,
        o_hat in 1u32..600,
        batches in prop::collection::vec(1u32..200, 0..30),
    ) {
        let b = backend();
        let mut lease =
            CostLease::open(b.clone(), prompt, o_hat, KvModel::PromptOnly, Instant::now());
        let charged = b.live.kv_projected_tokens.load(Relaxed);
        prop_assert_eq!(charged, prompt as i64);

        for n in batches {
            lease.observe_tokens(n);
            prop_assert_eq!(
                b.live.kv_projected_tokens.load(Relaxed), charged,
                "prompt_only projection must not move as tokens arrive"
            );
        }
    }

    /// Verifies that under PromptPlusOutput mode, KV projections only increase monotonically as output overruns initial estimates.
    #[test]
    fn prompt_plus_output_projection_is_monotonically_non_decreasing(
        prompt in 1u32..4000,
        o_hat in 1u32..600,
        batches in prop::collection::vec(1u32..200, 0..40),
    ) {
        let b = backend();
        let mut lease =
            CostLease::open(b.clone(), prompt, o_hat, KvModel::PromptPlusOutput, Instant::now());
        let mut prev = b.live.kv_projected_tokens.load(Relaxed);
        prop_assert_eq!(prev, prompt as i64 + o_hat as i64);

        for n in batches {
            lease.observe_tokens(n);
            let now = b.live.kv_projected_tokens.load(Relaxed);
            prop_assert!(now >= prev, "projection was drawn down: {} -> {}", prev, now);
            prev = now;
        }
    }
}
