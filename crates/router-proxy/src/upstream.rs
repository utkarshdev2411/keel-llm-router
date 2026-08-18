use std::pin::Pin;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body, Frame as BodyFrame, SizeHint};
use hyper::body::Incoming;
use hyper::{Request, Response};
use pin_project_lite::pin_project;
use router_core::backend::Backend;

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

/// Result of one completed request, for the caller to attribute to metrics.
pub struct StreamOutcome {
    pub content_tokens: u32,
    pub ttft: Option<std::time::Duration>,
    pub failed: bool,
}

pin_project! {
    /// Relays response frames unmodified while classifying SSE lines for
    /// token counting. Decrements `backend.live.inflight` on drop, covering
    /// normal completion, error, and client disconnect uniformly.
    pub struct CountingSseBody<B> {
        #[pin]
        inner: B,
        backend: Arc<Backend>,
        line_buf: String,
        content_tokens: u32,
        dispatched_at: Instant,
        first_token_at: Option<Instant>,
        released: bool,
        saw_done_or_error: bool,
    }

    impl<B> PinnedDrop for CountingSseBody<B> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if !*this.released {
                *this.released = true;
                this.backend.live.inflight.fetch_sub(1, Relaxed);
            }
        }
    }
}

impl<B> CountingSseBody<B> {
    pub fn new(inner: B, backend: Arc<Backend>, dispatched_at: Instant) -> Self {
        Self {
            inner,
            backend,
            line_buf: String::new(),
            content_tokens: 0,
            dispatched_at,
            first_token_at: None,
            released: false,
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
                        // Drain complete lines; keep any trailing partial
                        // line in the buffer for the next poll, since SSE
                        // frames can be split across network reads.
                        while let Some(pos) = this.line_buf.find('\n') {
                            let line: String = this.line_buf.drain(..=pos).collect();
                            let line = line.trim();
                            let line = line.strip_prefix("data:").unwrap_or(line).trim();
                            if let Some(parsed) = sse::parse_line(line) {
                                match sse::classify(&parsed) {
                                    Some(sse::Frame::Content { .. }) => {
                                        *this.content_tokens += 1;
                                        if this.first_token_at.is_none() {
                                            *this.first_token_at = Some(Instant::now());
                                        }
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
    use http_body_util::StreamBody;
    use router_core::backend::{BackendId, CapsEstimate, HealthState, LiveCounters};
    use arc_swap::ArcSwapOption;

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

    #[tokio::test]
    async fn counts_content_frames_split_across_chunks() {
        let backend = test_backend();
        backend.live.inflight.fetch_add(1, Relaxed);

        // Split a single SSE line across two chunks to exercise the
        // partial-line buffering path.
        let chunks: Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> = vec![
            Ok(BodyFrame::data(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel",
            ))),
            Ok(BodyFrame::data(Bytes::from_static(b"lo\"}}]}\n"))),
            Ok(BodyFrame::data(Bytes::from_static(b"data: [DONE]\n"))),
        ];
        let stream = futures_util::stream::iter(chunks);
        let inner = StreamBody::new(stream);
        let mut body = CountingSseBody::new(inner, backend.clone(), Instant::now());

        while let Some(f) = futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            f.unwrap();
        }

        assert_eq!(body.content_tokens, 1);
        assert!(body.saw_done_or_error);
    }

    #[tokio::test]
    async fn releases_inflight_on_drop() {
        let backend = test_backend();
        backend.live.inflight.fetch_add(1, Relaxed);
        let chunks: Vec<Result<BodyFrame<Bytes>, std::convert::Infallible>> = vec![];
        let stream = futures_util::stream::iter(chunks);
        let inner = StreamBody::new(stream);
        let body = CountingSseBody::new(inner, backend.clone(), Instant::now());
        drop(body);
        assert_eq!(backend.live.inflight.load(Relaxed), 0);
    }
}
