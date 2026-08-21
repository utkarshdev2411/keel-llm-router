//! Inbound HTTP request router and upstream proxy handler.
//!
//! Orchestrates feature extraction, routing strategy evaluation, cost lease reservation,
//! and streaming response proxying to selected backends.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use router_core::backend::Snapshot;
use router_core::cost::{occupancy, KvModel};
use router_core::lease::CostLease;
use router_core::strategy::RoutingStrategy;
use router_core::trace::DecisionTrace;

use crate::inbound;
use crate::length_estimator::{self, RouteHistograms};
use crate::observe;
use crate::upstream::{self, BodyParams, CountingSseBody};

pub type ResponseBody = BoxBody<Bytes, hyper::Error>;

/// Shared state holding backend configuration snapshots, active routing policy, client pool, and estimators.
pub struct RouterState {
    pub snapshot: arc_swap::ArcSwap<Snapshot>,
    pub strategy: Box<dyn RoutingStrategy>,
    pub client: upstream::PooledClient,
    pub max_request_body_bytes: usize,
    pub decision_trace_sample_rate: f64,
    /// Configured KV projection model controlling token cost estimation. Must match backend engine configuration.
    pub kv_model: KvModel,
    /// Shared per-route output length estimator providing completion token estimates `ô`.
    pub route_hists: Arc<RouteHistograms>,
}

/// Top-level hyper HTTP request entrypoint.
///
/// Wraps `handle_inner` and maps internal status codes into formatted HTTP error responses.
pub async fn handle(
    req: Request<Incoming>,
    state: Arc<RouterState>,
) -> Result<Response<ResponseBody>, std::convert::Infallible> {
    match handle_inner(req, state).await {
        Ok(resp) => Ok(resp),
        Err(status) => Ok(error_response(status)),
    }
}

async fn handle_inner(
    req: Request<Incoming>,
    state: Arc<RouterState>,
) -> Result<Response<ResponseBody>, StatusCode> {
    let now = Instant::now();

    // Read and buffer inbound request payload within configured size limit.
    let (parts, body_bytes) = inbound::read_body(req, state.max_request_body_bytes)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;

    // Extract request features (model, prompt tokens, max_tokens, etc.).
    let features = inbound::build_features(&body_bytes, now, &state.route_hists)
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let snap = state.snapshot.load();
    if snap.healthy.is_empty() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let decision_start = Instant::now();
    let mut rng = SmallRng::from_entropy();

    // Always collect a trace so we can read fell_through for the saturated_dispatches
    // counter. We only emit the full trace log at the sampled rate.
    let sampled = rng.gen::<f64>() < state.decision_trace_sample_rate;
    let mut trace = DecisionTrace::default();

    let backend_id = state
        .strategy
        .pick(&snap, &features, &mut rng, Some(&mut trace))
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    observe::record_decision_duration(
        state.strategy.name(),
        decision_start.elapsed().as_secs_f64(),
    );

    // Record saturation metric when admission gate falls through to least-bad backend.
    if trace.fell_through {
        metrics::counter!("router_saturated_dispatches_total").increment(1);
    }

    if sampled {
        tracing::debug!(
            strategy = trace.strategy,
            chosen = ?trace.chosen,
            prompt_tokens = trace.prompt_tokens,
            expected_output_tokens = trace.expected_output_tokens,
            candidates = trace.candidates.len(),
            fell_through = trace.fell_through,
            "decision trace"
        );
    }

    // NFR-3: Measure router processing overhead up to upstream dispatch (target: p99 < 1 ms).
    observe::record_router_overhead(now.elapsed().as_secs_f64());

    let backend = snap.backends[backend_id.0 as usize].clone();

    // Open cost lease BEFORE connecting to ensure concurrent decisions observe committed capacity.
    let lease = CostLease::open(
        backend.clone(),
        features.prompt_tokens,
        features.expected_output_tokens,
        state.kv_model,
        now,
    );

    // Update backend live state metrics after lease reservation.
    observe::record_inflight(&backend.key, backend.live.inflight.load(Relaxed));
    observe::record_occupancy(&backend.key, occupancy(&backend));
    observe::record_kv_projected(
        &backend.key,
        backend.live.kv_projected_tokens.load(Relaxed),
    );

    let upstream_req = match upstream::rebuild_request(&parts, body_bytes, &backend.uri) {
        Ok(r) => r,
        Err(_) => {
            drop(lease);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let resp = match upstream::dispatch(&state.client, upstream_req).await {
        Ok(r) => r,
        Err(_) => {
            observe::record_backend_error(&backend.key, "connect");
            drop(lease);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let (resp_parts, resp_body) = resp.into_parts();

    // Transfer lease ownership into `CountingSseBody` to handle streaming token observation,
    // metric recording, and lease release upon stream termination or client disconnect.
    let counted = CountingSseBody::new(
        resp_body,
        BodyParams {
            lease,
            backend_key: backend.key.clone(),
            estimated_output_tokens: features.expected_output_tokens,
            dispatched_at: now,
            upstream_status: resp_parts.status,
            route: length_estimator::route_key_for(&features),
            route_hists: state.route_hists.clone(),
        },
    );

    let body: ResponseBody = counted.map_err(hyper::Error::from).boxed();
    Ok(Response::from_parts(resp_parts, body))
}

/// Constructs a static HTTP error response and records request failure metrics.
fn error_response(status: StatusCode) -> Response<ResponseBody> {
    observe::record_request_result(false);
    let body: ResponseBody =
        Full::new(Bytes::new())
            .map_err(|never: std::convert::Infallible| match never {})
            .boxed();
    Response::builder()
        .status(status)
        .body(body)
        .expect("static response is valid")
}
