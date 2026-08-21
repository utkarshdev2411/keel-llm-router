use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body, Frame as BodyFrame, SizeHint};
use hyper::body::Incoming;
use hyper::{Request, Response};
use pin_project_lite::pin_project;
use router_core::features::RouteKey;
use router_core::lease::CostLease;

use crate::length_estimator::RouteHistograms;
use crate::observe;
use crate::sse;

/// Error conditions encountered during upstream request building or network connection dispatch.
#[derive(thiserror::Error, Debug)]
pub enum UpstreamError {
    #[error("upstream connect failed: {0}")]
    Connect(String),
    #[error("upstream request build failed: {0}")]
    Build(String),
}

/// Reconstructs an HTTP request targeting a specific backend URI, forwarding headers except Host.
pub fn rebuild_request(
    parts: &hyper::http::request::Parts,
    body: Bytes,
    backend_uri: &str,
) -> Result<Request<http_body_util::Full<Bytes>>, UpstreamError> {
    let path_and_query = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let uri: hyper::Uri = format!("{}{}", backend_uri.trim_end_matches('/'), path_and_query)
        .parse()
        .map_err(|e| UpstreamError::Build(format!("{e}")))?;

    let mut builder = Request::builder().method(parts.method.clone()).uri(uri);
    for (name, value) in parts.headers.iter() {
        if name == hyper::header::HOST {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(http_body_util::Full::new(body))
        .map_err(|e| UpstreamError::Build(format!("{e}")))
}

pin_project! {
    /// Transparent SSE response body wrapper that performs stream parsing, token accounting, and terminal result recording.
    ///
    /// ### Key Architectural Responsibilities
    /// - **RAII Lease Management**: Owns the `CostLease`, ensuring capacity charges are released on all exit paths
    ///   (normal stream end, HTTP error, client disconnect, task cancellation).
    /// - **Dynamic Recharge**: Classifies SSE content frames and feeds token counts into `CostLease` to revise KV projections upward on output overrun.
    /// - **In-Band Error Classification**: Detects error payloads emitted within HTTP 200 streams (e.g. KV cache exhaustion)
    ///   and records them as request failures.
    /// - **Feedback Loop**: Extracts completion token usage from stream usage frames to update per-route output estimators.
    pub struct CountingSseBody<B> {
        #[pin]
        inner: B,
        lease: CostLease,
        backend_key: Arc<str>,
        line_buf: String,
        content_tokens: u32,
        estimated_output_tokens: u32,
        dispatched_at: Instant,
        first_token_at: Option<Instant>,
        saw_done_or_error: bool,
        upstream_status: hyper::StatusCode,
        saw_error_frame: bool,
        route: RouteKey,
        route_hists: Arc<RouteHistograms>,
    }

    impl<B> PinnedDrop for CountingSseBody<B> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let failed = *this.saw_error_frame || !this.upstream_status.is_success();
            if failed {
                let kind = if *this.saw_error_frame { "in_band_error" } else { "http_status" };
                observe::record_backend_error(this.backend_key, kind);
            }
            observe::record_request_result(!failed);
        }
    }
}

/// Constructor parameters for initializing a `CountingSseBody`.
pub struct BodyParams {
    pub lease: CostLease,
    pub backend_key: Arc<str>,
    pub estimated_output_tokens: u32,
    pub dispatched_at: Instant,
    pub upstream_status: hyper::StatusCode,
    pub route: RouteKey,
    pub route_hists: Arc<RouteHistograms>,
}

impl<B> CountingSseBody<B> {
    pub fn new(inner: B, p: BodyParams) -> Self {
        Self {
            inner,
            lease: p.lease,
            backend_key: p.backend_key,
            line_buf: String::new(),
            content_tokens: 0,
            estimated_output_tokens: p.estimated_output_tokens,
            dispatched_at: p.dispatched_at,
            first_token_at: None,
            saw_done_or_error: false,
            upstream_status: p.upstream_status,
            saw_error_frame: false,
            route: p.route,
            route_hists: p.route_hists,
        }
    }
}

impl<B> CountingSseBody<B> {
    pub fn content_tokens(&self) -> u32 {
        self.content_tokens
    }

    pub fn saw_error_frame(&self) -> bool {
        self.saw_error_frame
    }
}

impl<B> Body for CountingSseBody<B>
where
    B: Body<Data = Bytes>,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<BodyFrame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    if let Ok(text) = std::str::from_utf8(data) {
                        this.line_buf.push_str(text);
                        while let Some(pos) = this.line_buf.find('\n') {
                            let line: String = this.line_buf.drain(..=pos).collect();
                            let line = line.trim();
                            let line = line.strip_prefix("data:").unwrap_or(line).trim();
                            if let Some(parsed) = sse::parse_line(line) {
                                match sse::classify(&parsed) {
                                    Some(sse::Frame::Content { .. }) => {
                                        *this.content_tokens += 1;
                                        this.lease.observe_tokens(1);
                                        if this.first_token_at.is_none() {
                                            *this.first_token_at = Some(Instant::now());
                                        }
                                    }
                                    Some(sse::Frame::Usage { completion_tokens }) => {
                                        observe::record_output_length_ratio(
                                            *this.estimated_output_tokens,
                                            completion_tokens,
                                        );
                                        if completion_tokens > 0 {
                                            this.route_hists
                                                .observe(this.route, completion_tokens);
                                        }
                                    }
                                    Some(sse::Frame::Error { .. }) => {
                                        *this.saw_error_frame = true;
                                        *this.saw_done_or_error = true;
                                    }
                                    Some(sse::Frame::Done) => {
                                        *this.saw_done_or_error = true;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Type alias for pooled hyper HTTP client.
pub type PooledClient = hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    http_body_util::Full<Bytes>,
>;

/// Instantiates pooled HTTP client using Tokio runtime executor.
pub fn build_client() -> PooledClient {
    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http()
}

/// Dispatches HTTP request upstream using client pool.
pub async fn dispatch(
    client: &PooledClient,
    req: Request<http_body_util::Full<Bytes>>,
) -> Result<Response<Incoming>, UpstreamError> {
    client.request(req).await.map_err(|e| UpstreamError::Connect(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwapOption;
    use http_body_util::StreamBody;
    use router_core::backend::{Backend, BackendId, CapsEstimate, HealthState, LiveCounters};
    use router_core::cost::KvModel;
    use router_core::lease::CostLease;
    use std::sync::atomic::Ordering::Relaxed;

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

    fn test_params(lease: CostLease, backend: &Arc<Backend>, est: u32) -> BodyParams {
        BodyParams {
            lease,
            backend_key: backend.key.clone(),
            estimated_output_tokens: est,
            dispatched_at: Instant::now(),
            upstream_status: hyper::StatusCode::OK,
            route: RouteKey(backend.model.clone(), 64),
            route_hists: Arc::new(RouteHistograms::new(300.0)),
        }
    }

    fn open_lease(backend: Arc<Backend>) -> CostLease {
        CostLease::open(backend, 100, 50, KvModel::PromptOnly, Instant::now())
    }

    /// Verifies parsing and counting of content frames split across multiple stream chunks.
    #[tokio::test]
    async fn counts_content_frames_split_across_chunks() {
        let backend = test_backend();
        let lease = open_lease(backend.clone());

        let chunks: Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> = vec![
            Ok(BodyFrame::data(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel",
            ))),
            Ok(BodyFrame::data(Bytes::from_static(b"lo\"}}]}\n"))),
            Ok(BodyFrame::data(Bytes::from_static(b"data: [DONE]\n"))),
        ];
        let stream = futures_util::stream::iter(chunks);
        let inner = StreamBody::new(stream);
        let mut body = CountingSseBody::new(inner, test_params(lease, &backend, 50));

        while let Some(f) =
            futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
        {
            f.unwrap();
        }

        assert_eq!(body.content_tokens, 1);
        assert!(body.saw_done_or_error);
    }

    /// Verifies that dropping the SSE response body releases backend in-flight and KV projections.
    #[tokio::test]
    async fn releases_lease_on_drop() {
        let backend = test_backend();
        let lease = open_lease(backend.clone());

        assert_eq!(backend.live.inflight.load(Relaxed), 1);

        let chunks: Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> = vec![];
        let stream = futures_util::stream::iter(chunks);
        let inner = StreamBody::new(stream);
        let body = CountingSseBody::new(inner, test_params(lease, &backend, 50));

        drop(body);

        assert_eq!(backend.live.inflight.load(Relaxed), 0,
            "inflight must be 0 after body drop");
        assert_eq!(backend.live.kv_projected_tokens.load(Relaxed), 0,
            "kv_projected must be 0 after body drop");
    }

    /// Verifies that in-band error payloads on HTTP 200 streams are recorded as failures.
    #[tokio::test]
    async fn in_band_error_frame_on_http_200_is_a_failure() {
        let backend = test_backend();
        let lease = open_lease(backend.clone());

        let chunks: Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> = vec![
            Ok(BodyFrame::data(Bytes::from_static(
                b"data: {\"error\":{\"message\":\"the kv cache does not have sufficient capacity\"}}\n",
            ))),
            Ok(BodyFrame::data(Bytes::from_static(b"data: [DONE]\n"))),
        ];
        let stream = futures_util::stream::iter(chunks);
        let inner = StreamBody::new(stream);
        let mut body =
            CountingSseBody::new(inner, test_params(lease, &backend, 50));

        while let Some(f) =
            futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
        {
            f.unwrap();
        }

        assert!(body.saw_error_frame,
            "an error object inside a 200 body must be recorded as a failure");
        assert_eq!(body.content_tokens, 0,
            "an error stream carries no content tokens");
    }

    /// Verifies clean stream completion recording on HTTP 200 responses.
    #[tokio::test]
    async fn clean_stream_on_http_200_is_a_success() {
        let backend = test_backend();
        let lease = open_lease(backend.clone());

        let chunks: Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> = vec![
            Ok(BodyFrame::data(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n",
            ))),
            Ok(BodyFrame::data(Bytes::from_static(b"data: [DONE]\n"))),
        ];
        let stream = futures_util::stream::iter(chunks);
        let inner = StreamBody::new(stream);
        let mut body =
            CountingSseBody::new(inner, test_params(lease, &backend, 50));

        while let Some(f) =
            futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
        {
            f.unwrap();
        }

        assert!(!body.saw_error_frame);
        assert!(body.upstream_status.is_success());
        assert_eq!(body.content_tokens, 1);
    }

    /// Verifies that stream usage frames feed observed completion tokens into the route length estimator.
    #[tokio::test]
    async fn usage_frame_feeds_the_route_estimate_for_the_next_request() {
        let backend = test_backend();
        let lease = open_lease(backend.clone());
        let hists = Arc::new(RouteHistograms::new(300.0));
        let route = RouteKey(backend.model.clone(), 64);

        assert_eq!(
            hists.estimate(&route, None),
            crate::length_estimator::DEFAULT_OUTPUT_ESTIMATE,
            "precondition: route has no history yet"
        );

        let chunks: Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> = vec![
            Ok(BodyFrame::data(Bytes::from_static(
                b"data: {\"choices\":[],\"usage\":{\"completion_tokens\":37}}\n",
            ))),
            Ok(BodyFrame::data(Bytes::from_static(b"data: [DONE]\n"))),
        ];
        let inner = StreamBody::new(futures_util::stream::iter(chunks));
        let mut body = CountingSseBody::new(
            inner,
            BodyParams {
                lease,
                backend_key: backend.key.clone(),
                estimated_output_tokens: 128,
                dispatched_at: Instant::now(),
                upstream_status: hyper::StatusCode::OK,
                route: route.clone(),
                route_hists: hists.clone(),
            },
        );

        while let Some(f) =
            futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
        {
            f.unwrap();
        }

        assert_eq!(
            hists.estimate(&route, None),
            37,
            "the usage frame's completion_tokens must reach the route estimate"
        );
    }

    /// Verifies that output token overrun triggers KV projection recharge under PromptPlusOutput mode.
    #[tokio::test]
    async fn recharge_fires_when_output_overruns_estimate() {
        let backend = test_backend();
        let lease = CostLease::open(backend.clone(), 10, 3, KvModel::PromptPlusOutput, Instant::now());
        let kv_before = backend.live.kv_projected_tokens.load(Relaxed);

        let content_frames: Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> = (0..10)
            .map(|_| Ok(BodyFrame::data(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
            ))))
            .collect();
        let stream = futures_util::stream::iter(content_frames);
        let inner = StreamBody::new(stream);
        let mut body = CountingSseBody::new(inner, test_params(lease, &backend, 3));

        while let Some(f) =
            futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
        {
            f.unwrap();
        }

        let kv_after = backend.live.kv_projected_tokens.load(Relaxed);
        assert!(kv_after > kv_before,
            "kv_projected must increase when output overruns estimate under PromptPlusOutput");
    }
}
