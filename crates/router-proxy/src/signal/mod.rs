//! Signal plane subsystem for backend metrics scraping and drift detection.
//!
//! The signal plane scrapes backend metrics endpoints and validates router
//! projections against backend reality. It operates independently of routing
//! decisions — scraped data is used only for drift detection and observability,
//! NEVER as input to the gate or any routing strategy.

pub mod drift;
pub mod scrape;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use router_core::backend::Backend;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::observe;
use crate::router::RouterState;
use crate::upstream::{self, PooledClient};

/// Failure modes for a single `/metrics` scrape, kept distinct so the caller
/// can log and label metrics differently for each (matches the collector's
/// existing "ok" / "timeout" / "error" result taxonomy).
#[derive(thiserror::Error, Debug)]
pub enum ScrapeError {
    #[error("malformed metrics URL: {0}")]
    BadUri(String),
    #[error("could not connect: {0}")]
    Connect(String),
    #[error("failed to read response body: {0}")]
    Body(String),
    #[error("scrape timed out")]
    Timeout,
}

/// Fetch `/metrics` from a backend over the shared pooled client and return
/// the raw body as text.
///
/// `timeout` wraps the ENTIRE operation -- connect through body read -- not
/// just the initial response head. A slow body drip past the head would
/// otherwise be unbounded even though the send itself completed in time.
/// (This was a real gap in an earlier version that timed out only the
/// request send and read the body outside any bound at all.)
pub async fn fetch_metrics(
    client: &PooledClient,
    metrics_url: &str,
    timeout: Duration,
) -> Result<String, ScrapeError> {
    let uri: hyper::Uri = metrics_url
        .parse()
        .map_err(|e| ScrapeError::BadUri(format!("{e}")))?;

    let fetch = async {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Full::new(Bytes::new()))
            .map_err(|e| ScrapeError::BadUri(format!("{e}")))?;

        let resp = upstream::dispatch(client, req)
            .await
            .map_err(|e| ScrapeError::Connect(e.to_string()))?;

        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| ScrapeError::Body(e.to_string()))?;

        Ok(String::from_utf8_lossy(&body.to_bytes()).into_owned())
    };

    tokio::time::timeout(timeout, fetch)
        .await
        .map_err(|_| ScrapeError::Timeout)?
}

/// Configuration for the signal plane collector.
///
/// Mirrors `router_core::config::SignalConfig` but lives here so signal I/O
/// stays entirely inside `router-proxy`. (`router-core` must stay tokio-free
/// to keep offline replay possible.)
#[derive(Clone)]
pub struct SignalConfig {
    pub enabled: bool,
    pub scrape_interval: Duration,
    pub scrape_timeout: Duration,
    pub max_signal_age: Duration,
    pub drift_warn_ratio: f64,
    pub validate_capacity_at_startup: bool,
}

/// Spawn one collector task per backend.
///
/// Returns the join handles so `main` can hold them alive for the process
/// lifetime. Dropping the handles would silently cancel the tasks.
///
/// # One task per backend
/// A single loop iterating all backends means one slow or hanging backend
/// delays every other reading. The staleness is invisible — data looks fresh
/// but represents the wrong moment. Separate tasks isolate failures.
pub fn spawn_collectors(
    state: Arc<RouterState>,
    cfg: SignalConfig,
) -> Vec<JoinHandle<()>> {
    if !cfg.enabled {
        tracing::info!("signal plane disabled; no collectors spawned");
        return Vec::new();
    }

    let snap = state.snapshot.load();
    let mut handles = Vec::with_capacity(snap.backends.len());

    // One shared pooled client for all collectors, same connection-pooling
    // machinery the request path already uses -- no second HTTP stack, no
    // separate TLS dependency to scrape a plaintext /metrics endpoint.
    let client = upstream::build_client();

    for backend in snap.backends.iter() {
        let backend = backend.clone();
        let cfg = cfg.clone();
        let client = client.clone();

        let handle = tokio::spawn(collect_loop(backend, client, cfg));
        handles.push(handle);
    }

    tracing::info!(count = handles.len(), "signal collectors spawned");
    handles
}

/// Per-backend collection loop. Runs until the task is cancelled.
///
/// Loop body order (from execution plan §4.2):
/// 1. tick
/// 2. GET /metrics with timeout
/// 3. Instant::now() on receipt, before parsing
/// 4. On success: parse → store → record metrics + drift
/// 5. On failure: preserve last value, record error, continue
async fn collect_loop(backend: Arc<Backend>, client: PooledClient, cfg: SignalConfig) {
    let mut ticker = tokio::time::interval(cfg.scrape_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    // Previous counter sample for computing windowed prefix hit rate.
    let mut prev_sample: Option<scrape::CounterSample> = None;

    let metrics_url = format!("{}/metrics", backend.uri.trim_end_matches('/'));

    loop {
        ticker.tick().await;

        let fetch_start = Instant::now();
        let result = fetch_metrics(&client, &metrics_url, cfg.scrape_timeout).await;

        // Capture router-monotonic time on receipt, BEFORE parsing.
        // Never use any timestamp from the backend response.
        let now = Instant::now();
        let duration_secs = fetch_start.elapsed().as_secs_f64();

        match result {
            Ok(text) => {
                let (load, new_sample) =
                    scrape::parse_reported(&text, now, prev_sample.as_ref());
                prev_sample = Some(new_sample);

                // Store the new reading.
                backend.reported.store(Some(Arc::new(load.clone())));

                // Record scrape success and the scraped values.
                observe::record_scrape_result(&backend.key, "ok", duration_secs);
                observe::record_scraped_metrics(&backend.key, &load, now);

                // Drift check — cross-check only, never affects routing.
                if let Some(ratio) =
                    drift::projection_drift(&backend, &load, now, cfg.max_signal_age)
                {
                    observe::record_projection_drift(&backend.key, ratio, cfg.drift_warn_ratio);
                }
            }
            Err(ScrapeError::Timeout) => {
                tracing::warn!(
                    backend = %backend.key,
                    timeout_ms = cfg.scrape_timeout.as_millis(),
                    "signal: /metrics scrape timed out"
                );
                // Preserve last good value — do NOT touch backend.reported.
                observe::record_scrape_result(&backend.key, "timeout", duration_secs);
            }
            Err(e) => {
                tracing::warn!(
                    backend = %backend.key,
                    error = %e,
                    "signal: /metrics scrape failed"
                );
                // Preserve last good value — do NOT store None, do NOT clear.
                observe::record_scrape_result(&backend.key, "error", duration_secs);
            }
        }
    }
}
