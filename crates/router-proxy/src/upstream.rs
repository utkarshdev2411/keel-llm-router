use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body, Frame as BodyFrame, SizeHint};
use hyper::body::Incoming;
use hyper::{Request, Response};
use pin_project_lite::pin_project;
use router_core::lease::CostLease;

use crate::observe;
use crate::sse;

#[derive(thiserror::Error, Debug)]
pub enum UpstreamError {
    #[error("upstream connect failed: {0}")]
    Connect(String),
    #[error("upstream request build failed: {0}")]
    Build(String),
}

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
    /// Relays response frames byte-identically while:
    ///   - classifying SSE frames to count content tokens
    ///   - feeding observed token counts into the `CostLease` for recharge
    ///   - recording per-backend occupancy and inflight metrics
    ///   - releasing the `CostLease` on drop (covers normal completion,
    ///     error, client disconnect, timeout, and task cancellation)
    ///
    /// The lease is owned here, not in the handler, so that Tokio's
    /// drop-on-cancel releases the charge on client disconnect with no
    /// special-case code. (LLD §5.4 ownership rule)
    pub struct CountingSseBody<B> {
        #[pin]
        inner: B,
        // RAII charge. Dropped when this body is dropped on any exit path.
        lease: CostLease,
        backend_key: Arc<str>,
        line_buf: String,
        content_tokens: u32,
        // Estimated output tokens at dispatch time, for ratio recording.
        estimated_output_tokens: u32,
        dispatched_at: Instant,
        first_token_at: Option<Instant>,
        saw_done_or_error: bool,
    }
}

impl<B> CountingSseBody<B> {
    /// `lease` must already be opened (inflight incremented, KV charged).
    /// Ownership is moved here so the lease is released on drop of this body.
    pub fn new(
        inner: B,
        lease: CostLease,
        backend_key: Arc<str>,
        estimated_output_tokens: u32,
        dispatched_at: Instant,
    ) -> Self {
        Self {
            inner,
            lease,
            backend_key,
            line_buf: String::new(),
            content_tokens: 0,
            estimated_output_tokens,
            dispatched_at,
            first_token_at: None,
            saw_done_or_error: false,
        }
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
                        // Drain complete lines; keep any trailing partial line
                        // in the buffer — SSE frames can be split across reads.
                        while let Some(pos) = this.line_buf.find('\n') {
                            let line: String = this.line_buf.drain(..=pos).collect();
                            let line = line.trim();
                            let line = line.strip_prefix("data:").unwrap_or(line).trim();
                            if let Some(parsed) = sse::parse_line(line) {
                                match sse::classify(&parsed) {
                                    Some(sse::Frame::Content { .. }) => {
                                        *this.content_tokens += 1;
                                        // Feed the lease so it can recharge if output
                                        // overruns the estimate (strictly >, not >=).
                                        this.lease.observe_tokens(1);
                                        if this.first_token_at.is_none() {
                                            *this.first_token_at = Some(Instant::now());
                                        }
                                    }
                                    Some(sse::Frame::Usage { completion_tokens }) => {
                                        // Record output-length estimation accuracy.
                                        observe::record_output_length_ratio(
                                            *this.estimated_output_tokens,
                                            completion_tokens,
                                        );
                                    }
                                    Some(sse::Frame::Done) | Some(sse::Frame::Error { .. }) => {
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

pub type PooledClient = hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    http_body_util::Full<Bytes>,
>;

pub fn build_client() -> PooledClient {
    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http()
}

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

    fn open_lease(backend: Arc<Backend>) -> CostLease {
        CostLease::open(backend, 100, 50, KvModel::PromptOnly, Instant::now())
    }

    #[tokio::test]
    async fn counts_content_frames_split_across_chunks() {
        let backend = test_backend();
        let lease = open_lease(backend.clone());

        // Split a single SSE line across two chunks to exercise partial-line buffering.
        let chunks: Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> = vec![
            Ok(BodyFrame::data(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel",
            ))),
            Ok(BodyFrame::data(Bytes::from_static(b"lo\"}}]}\n"))),
            Ok(BodyFrame::data(Bytes::from_static(b"data: [DONE]\n"))),
        ];
        let stream = futures_util::stream::iter(chunks);
        let inner = StreamBody::new(stream);
        let mut body = CountingSseBody::new(inner, lease, backend.key.clone(), 50, Instant::now());

        while let Some(f) =
            futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
        {
            f.unwrap();
        }

        assert_eq!(body.content_tokens, 1);
        assert!(body.saw_done_or_error);
    }

    /// The lease is released (inflight=0, kv_projected=0) when the body is dropped —
    /// this covers normal completion, error, and client disconnect uniformly.
    #[tokio::test]
    async fn releases_lease_on_drop() {
        let backend = test_backend();
        let lease = open_lease(backend.clone());

        // inflight should be 1 while the lease is open
        assert_eq!(backend.live.inflight.load(Relaxed), 1);

        let chunks: Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> = vec![];
        let stream = futures_util::stream::iter(chunks);
        let inner = StreamBody::new(stream);
        let body = CountingSseBody::new(inner, lease, backend.key.clone(), 50, Instant::now());

        drop(body); // lease dropped here

        assert_eq!(backend.live.inflight.load(Relaxed), 0,
            "inflight must be 0 after body drop");
        assert_eq!(backend.live.kv_projected_tokens.load(Relaxed), 0,
            "kv_projected must be 0 after body drop");
    }

    /// Recharge fires when observed tokens exceed the estimate under PromptPlusOutput.
    #[tokio::test]
    async fn recharge_fires_when_output_overruns_estimate() {
        let backend = test_backend();
        // Open with a small estimate of 3 tokens under PromptPlusOutput
        let lease = CostLease::open(backend.clone(), 10, 3, KvModel::PromptPlusOutput, Instant::now());
        let kv_before = backend.live.kv_projected_tokens.load(Relaxed);

        // Stream 10 content frames — well past the estimate of 3
        let content_frames: Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> = (0..10)
            .map(|_| Ok(BodyFrame::data(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
            ))))
            .collect();
        let stream = futures_util::stream::iter(content_frames);
        let inner = StreamBody::new(stream);
        let mut body = CountingSseBody::new(inner, lease, backend.key.clone(), 3, Instant::now());

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
