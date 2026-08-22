//! Frame-boundary correctness, driven through the real relay.
//!
//! This test lives in `router-proxy` on purpose. An earlier version sat in
//! `router-core/tests/` and, because `sse` is not reachable from there, carried a
//! private reimplementation of the frame classifier. It therefore verified its own
//! copy and could not have caught a bug in the shipping one — the most expensive
//! kind of passing test.
//!
//! What is exercised here is the actual `CountingSseBody`: its partial-line buffer,
//! `sse::parse_line`, `sse::classify`, and the lease recharge, with the stream
//! delivered one byte per frame so every line boundary falls mid-chunk.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use arc_swap::ArcSwapOption;
use bytes::Bytes;
use http_body::{Body, Frame as BodyFrame};
use http_body_util::StreamBody;
use router_core::backend::{Backend, BackendId, CapsEstimate, HealthState, LiveCounters};
use router_core::cost::KvModel;
use router_core::features::RouteKey;
use router_core::lease::CostLease;
use router_proxy::length_estimator::RouteHistograms;
use router_proxy::upstream::{BodyParams, CountingSseBody};

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

/// Split `s` into one single-byte body frame per byte.
fn byte_frames(s: &str) -> Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> {
    s.bytes()
        .map(|b| Ok(BodyFrame::data(Bytes::copy_from_slice(&[b]))))
        .collect()
}

async fn drain<B>(body: &mut CountingSseBody<B>)
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Debug,
{
    while let Some(f) =
        futures_util::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await
    {
        f.unwrap();
    }
}

fn params(lease: CostLease, backend: &Arc<Backend>, est: u32) -> BodyParams {
    BodyParams {
        lease,
        backend_key: backend.key.clone(),
        estimated_output_tokens: est,
        dispatched_at: Instant::now(),
        upstream_status: hyper::StatusCode::OK,
        route: RouteKey(backend.model.clone(), 64),
        route_hists: Arc::new(RouteHistograms::new(300.0)),
            estimated_prompt_tokens: 100,
    }
}

/// A canonical response fed one byte at a time must produce a content-frame count
/// that reconciles exactly with the engine's own `usage.completion_tokens`.
///
/// Reconciliation is the point. Counting frames is not the same as counting tokens,
/// so the count is only trustworthy where the engine's own number agrees with it.
#[tokio::test]
async fn byte_at_a_time_token_count_reconciles_with_usage() {
    let stream = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"!\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n",
    );

    let backend = test_backend();
    let hists = Arc::new(RouteHistograms::new(300.0));
    let route = RouteKey(backend.model.clone(), 64);
    let lease = CostLease::open(backend.clone(), 5, 10, KvModel::PromptOnly, Instant::now());

    let inner = StreamBody::new(futures_util::stream::iter(byte_frames(stream)));
    let mut body = CountingSseBody::new(
        inner,
        BodyParams { route: route.clone(), route_hists: hists.clone(),
                estimated_prompt_tokens: 100, ..params(lease, &backend, 10) },
    );
    drain(&mut body).await;

    assert_eq!(
        body.content_tokens(),
        3,
        "three content frames must survive being split across byte boundaries"
    );
    assert_eq!(
        hists.estimate(&route, None),
        3,
        "the usage frame must reconcile with the counted content frames"
    );
    assert!(!body.saw_error_frame(), "a clean stream must not be flagged as an error");

    drop(body);
    assert_eq!(backend.live.inflight.load(Relaxed), 0);
    assert_eq!(backend.live.kv_projected_tokens.load(Relaxed), 0);
    #[cfg(debug_assertions)]
    router_core::lease::assert_invariant(&backend);
}

/// The same split treatment applied to an in-band error. A KV-exhaustion frame that
/// straddles byte boundaries must still be recognised as a failure, not relayed as
/// an empty success.
#[tokio::test]
async fn byte_at_a_time_in_band_error_is_still_detected() {
    let stream = concat!(
        "data: {\"error\":{\"message\":\"the kv cache does not have sufficient capacity\",",
        "\"type\":\"ServerError\"}}\n\n",
        "data: [DONE]\n\n",
    );

    let backend = test_backend();
    let lease = CostLease::open(backend.clone(), 5, 10, KvModel::PromptOnly, Instant::now());
    let inner = StreamBody::new(futures_util::stream::iter(byte_frames(stream)));
    let mut body = CountingSseBody::new(inner, params(lease, &backend, 10));
    drain(&mut body).await;

    assert!(body.saw_error_frame(), "an error split across byte boundaries must still be seen");
    assert_eq!(body.content_tokens(), 0);

    drop(body);
    assert_eq!(backend.live.inflight.load(Relaxed), 0);
    assert_eq!(backend.live.kv_projected_tokens.load(Relaxed), 0);
}

/// A stream cut off mid-frame must not invent a token, and must still release.
#[tokio::test]
async fn truncated_final_frame_is_not_counted_and_still_releases() {
    let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"con";

    let backend = test_backend();
    let lease = CostLease::open(backend.clone(), 5, 10, KvModel::PromptOnly, Instant::now());
    let inner = StreamBody::new(futures_util::stream::iter(byte_frames(stream)));
    let mut body = CountingSseBody::new(inner, params(lease, &backend, 10));
    drain(&mut body).await;

    assert_eq!(body.content_tokens(), 1, "the incomplete trailing frame must not be counted");

    drop(body);
    assert_eq!(backend.live.inflight.load(Relaxed), 0);
    assert_eq!(backend.live.kv_projected_tokens.load(Relaxed), 0);
}
