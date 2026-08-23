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

use router_core::backend::Backend;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::observe;
use crate::router::RouterState;

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

    for backend in snap.backends.iter() {
        let backend = backend.clone();
        let cfg = cfg.clone();

        // Build a dedicated reqwest client per backend with the scrape timeout
        // as the overall request timeout.
        let client = reqwest::Client::builder()
            .timeout(cfg.scrape_timeout + Duration::from_millis(50)) // slight buffer; real cutoff is the tokio::time::timeout below
            .build()
            .unwrap_or_default();

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
async fn collect_loop(backend: Arc<Backend>, client: reqwest::Client, cfg: SignalConfig) {
    let mut ticker = tokio::time::interval(cfg.scrape_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    // Previous counter sample for computing windowed prefix hit rate.
    let mut prev_sample: Option<scrape::CounterSample> = None;

    let metrics_url = format!("{}/metrics", backend.uri.trim_end_matches('/'));

    loop {
        ticker.tick().await;

        let fetch_start = Instant::now();

        let result = tokio::time::timeout(
            cfg.scrape_timeout,
            client.get(&metrics_url).send(),
        )
        .await;

        // Capture router-monotonic time on receipt, BEFORE parsing.
        // Never use any timestamp from the backend response.
        let now = Instant::now();
        let duration_secs = fetch_start.elapsed().as_secs_f64();

        match result {
            Ok(Ok(response)) => {
                match response.text().await {
                    Ok(text) => {
                        let (load, new_sample) =
                            scrape::parse_reported(&text, now, prev_sample.as_ref());
                        prev_sample = Some(new_sample);

                        // Store the new reading.
                        backend
                            .reported
                            .store(Some(Arc::new(load.clone())));

                        // Record scrape success and the scraped values.
                        observe::record_scrape_result(&backend.key, "ok", duration_secs);
                        observe::record_scraped_metrics(&backend.key, &load, now);

                        // Drift check — cross-check only, never affects routing.
                        if let Some(ratio) = drift::projection_drift(
                            &backend,
                            &load,
                            now,
                            cfg.max_signal_age,
                        ) {
                            observe::record_projection_drift(
                                &backend.key,
                                ratio,
                                cfg.drift_warn_ratio,
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            backend = %backend.key,
                            error = %e,
                            "signal: failed to read /metrics response body"
                        );
                        // Preserve last good value — do NOT store None.
                        observe::record_scrape_result(&backend.key, "error", duration_secs);
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    backend = %backend.key,
                    error = %e,
                    "signal: /metrics request failed"
                );
                // Preserve last good value — do NOT touch backend.reported.
                observe::record_scrape_result(&backend.key, "error", duration_secs);
            }
            Err(_elapsed) => {
                tracing::warn!(
                    backend = %backend.key,
                    timeout_ms = cfg.scrape_timeout.as_millis(),
                    "signal: /metrics scrape timed out"
                );
                // Preserve last good value — do NOT touch backend.reported.
                observe::record_scrape_result(&backend.key, "timeout", duration_secs);
            }
        }
    }
}
