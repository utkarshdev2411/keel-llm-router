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
        observe::record_request_result(false);
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let decision_start = Instant::now();
    let mut rng = SmallRng::from_entropy();

    let sampled = rng.gen::<f64>() < state.decision_trace_sample_rate;
    let mut trace = DecisionTrace::default();
    let trace_arg = if sampled { Some(&mut trace) } else { None };

    let backend_id = state
        .strategy
        .pick(&snap, &features, &mut rng, trace_arg)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    observe::record_decision_duration(state.strategy.name(), decision_start.elapsed().as_secs_f64());

    if sampled {
        tracing::debug!(
            strategy = trace.strategy,
            chosen = ?trace.chosen,
            prompt_tokens = trace.prompt_tokens,
            expected_output_tokens = trace.expected_output_tokens,
            candidates = trace.candidates.len(),
            "decision trace"
        );
    }

    let backend = snap.backends[backend_id.0 as usize].clone();
    backend.live.inflight.fetch_add(1, Relaxed);
    observe::record_inflight(&backend.key, backend.live.inflight.load(Relaxed));

    let upstream_req = match upstream::rebuild_request(&parts, body_bytes, &backend.uri) {
        Ok(r) => r,
        Err(_) => {
            backend.live.inflight.fetch_sub(1, Relaxed);
            observe::record_request_result(false);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let resp = match upstream::dispatch(&state.client, upstream_req).await {
        Ok(r) => r,
        Err(_) => {
            backend.live.inflight.fetch_sub(1, Relaxed);
            observe::record_backend_error(&backend.key, "connect");
            observe::record_request_result(false);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let (resp_parts, resp_body) = resp.into_parts();
    // CountingSseBody takes over the inflight decrement from here, on drop,
    // so it must NOT be decremented again above on this path.
    let counted = CountingSseBody::new(resp_body, backend, now);
    observe::record_request_result(true);

    let body: ResponseBody = counted.map_err(hyper::Error::from).boxed();
    Ok(Response::from_parts(resp_parts, body))
}

fn error_response(status: StatusCode) -> Response<ResponseBody> {
    observe::record_request_result(false);
    let body: ResponseBody = Full::new(Bytes::new()).map_err(|never: std::convert::Infallible| match never {}).boxed();
    Response::builder().status(status).body(body).expect("static response is valid")
}
