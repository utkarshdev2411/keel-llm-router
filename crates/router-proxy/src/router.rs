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
use crate::observe;
use crate::upstream::{self, CountingSseBody};

pub type ResponseBody = BoxBody<Bytes, hyper::Error>;

pub struct RouterState {
    pub snapshot: arc_swap::ArcSwap<Snapshot>,
    pub strategy: Box<dyn RoutingStrategy>,
    pub client: upstream::PooledClient,
    pub max_request_body_bytes: usize,
    pub decision_trace_sample_rate: f64,
    /// KV projection model, from config. Controls whether generated tokens
    /// are charged as additional KV or not. Must match the backend engine.
    pub kv_model: KvModel,
}

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

    let (parts, body_bytes) = inbound::read_body(req, state.max_request_body_bytes)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;

    let features = inbound::build_features(&body_bytes, now)
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

    // Emit saturated_dispatches counter when the gate fell through.
    // Done here (proxy layer) rather than in router-core to preserve core's
    // purity boundary (no metrics crate in router-core).
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

    // NFR-3: measure router overhead up to the point of upstream dispatch,
    // excluding network and generation time. This is the p99 < 1ms budget.
    observe::record_router_overhead(now.elapsed().as_secs_f64());

    let backend = snap.backends[backend_id.0 as usize].clone();

    // Open the cost lease BEFORE dispatching. The charge must exist during
    // the connect phase: a slow-to-accept backend must still look loaded to
    // concurrent routing decisions. (LLD §5.1)
    let lease = CostLease::open(
        backend.clone(),
        features.prompt_tokens,
        features.expected_output_tokens,
        state.kv_model,
        now,
    );

    // Record metrics after lease is open (so inflight and kv_projected are current).
    observe::record_inflight(&backend.key, backend.live.inflight.load(Relaxed));
    observe::record_occupancy(&backend.key, occupancy(&backend));
    observe::record_kv_projected(
        &backend.key,
        backend.live.kv_projected_tokens.load(Relaxed),
    );

    let upstream_req = match upstream::rebuild_request(&parts, body_bytes, &backend.uri) {
        Ok(r) => r,
        Err(_) => {
            // lease drops here → release() called → inflight--, kv_projected-=charged
            drop(lease);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let resp = match upstream::dispatch(&state.client, upstream_req).await {
        Ok(r) => r,
        Err(_) => {
            observe::record_backend_error(&backend.key, "connect");
            // lease drops here
            drop(lease);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let (resp_parts, resp_body) = resp.into_parts();

    // Move the lease into CountingSseBody. From this point, drop of the body
    // releases the charge AND records the terminal result — covering normal
    // completion, error, client disconnect, timeout, and task cancellation
    // (LLD §5.4 ownership rule).
    //
    // The result is deliberately NOT recorded here. The response head says
    // nothing about whether the request succeeded: a KV-exhaustion failure is
    // an error object inside the body of an HTTP 200.
    let counted = CountingSseBody::new(
        resp_body,
        lease,
        backend.key.clone(),
        features.expected_output_tokens,
        now,
        resp_parts.status,
    );

    let body: ResponseBody = counted.map_err(hyper::Error::from).boxed();
    Ok(Response::from_parts(resp_parts, body))
}

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
