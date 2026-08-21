//! Observability, metrics registration, and telemetry recording helper routines.
//!
//! Provides JSON structured tracing initialization, Prometheus metrics setup,
//! and metric helper functions for latency budgets, backend occupancy, and routing decision telemetry.

use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;

/// Initializes structured JSON tracing with `tracing_subscriber`, falling back to `info` log level.
pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().json().with_env_filter(filter).init();
}

/// Installs the global Prometheus metrics exporter listener serving `/metrics` on `admin_bind`.
pub fn install_metrics_recorder(
    admin_bind: SocketAddr,
) -> Result<(), metrics_exporter_prometheus::BuildError> {
    PrometheusBuilder::new().with_http_listener(admin_bind).install()
}

/// Registers metadata and descriptions for all router Prometheus metrics.
pub fn describe_metrics() {
    // --- Request-level metrics ---
    metrics::describe_counter!(
        "router_requests_total",
        "Total requests processed by outcome (`ok` or `error`). \
         Reflects total traffic without a `rejected` label, as admission control never sheds requests."
    );
    metrics::describe_counter!(
        "router_backend_errors_total",
        "Per-backend request error counter partitioned by error kind. \
         Primary metric for routing policy efficacy comparisons."
    );

    // --- Admission gate metrics ---
    metrics::describe_counter!(
        "router_saturated_dispatches_total",
        "Count of dispatches where the admission gate found zero clean backends. \
         Measures system load pressure; all counted requests are still dispatched to the least-bad backend."
    );

    // --- Per-backend live state metrics ---
    metrics::describe_gauge!(
        "router_backend_occupancy",
        "Committed capacity fraction per backend: `max(inflight / max_num_seqs, kv_projected / kv_capacity)`. \
         Primary mechanism metric explaining error rates and load distribution."
    );
    metrics::describe_gauge!(
        "router_backend_kv_projected",
        "Current projected KV tokens reserved per backend. \
         Unbounded monotonic increase signals an unreleased lease memory leak."
    );
    metrics::describe_gauge!("router_backend_inflight", "Current in-flight request count per backend.");
    metrics::describe_counter!(
        "router_backend_occupancy_ticks_total",
        "Total periodic occupancy samples taken for a backend. Denominator for time-at-ceiling calculation."
    );
    metrics::describe_counter!(
        "router_backend_ticks_at_ceiling_total",
        "Count of periodic occupancy samples where backend occupancy met or exceeded `sigma`. \
         Ratio over total ticks measures fraction of run time spent in the saturated region."
    );

    // --- Decision path latency metrics ---
    metrics::describe_histogram!(
        "router_decision_duration_seconds",
        "Routing strategy algorithm execution time (target: p99 < 10 µs)."
    );
    metrics::describe_histogram!(
        "router_overhead_seconds",
        "End-to-end router overhead prior to upstream dispatch (target: p99 < 1 ms)."
    );

    // --- Estimation accuracy & drift metrics ---
    metrics::describe_histogram!(
        "router_output_length_ratio",
        "Ratio of estimated to actual completion tokens (`estimated / completion_tokens`). \
         Ratios near 1.0 indicate accurate output length estimation."
    );
    metrics::describe_gauge!(
        "router_projection_drift",
        "Ratio of router-projected KV fraction to backend-reported KV usage percentage. \
         Significant divergence indicates KV cost model mis-calibration."
    );
}

// ---------------------------------------------------------------------------
// Recording helper routines
// ---------------------------------------------------------------------------

/// Records final request status (`ok` vs `error`) to `router_requests_total`.
pub fn record_request_result(ok: bool) {
    let result = if ok { "ok" } else { "error" };
    metrics::counter!("router_requests_total", "result" => result).increment(1);
}

/// Records a backend error labeled by backend identifier and error classification.
pub fn record_backend_error(backend_key: &str, kind: &str) {
    metrics::counter!(
        "router_backend_errors_total",
        "backend" => backend_key.to_string(),
        "kind" => kind.to_string()
    )
    .increment(1);
}

/// Updates the current active in-flight request gauge for a backend.
pub fn record_inflight(backend_key: &str, n: u32) {
    metrics::gauge!("router_backend_inflight", "backend" => backend_key.to_string())
        .set(n as f64);
}

/// Updates the active committed capacity occupancy gauge after dispatch.
pub fn record_occupancy(backend_key: &str, occupancy: f64) {
    metrics::gauge!("router_backend_occupancy", "backend" => backend_key.to_string())
        .set(occupancy);
}

/// Records a periodic occupancy sample, incrementing total ticks and conditionally ceiling ticks when >= `sigma`.
pub fn record_occupancy_tick(backend_key: &str, at_ceiling: bool) {
    metrics::counter!(
        "router_backend_occupancy_ticks_total",
        "backend" => backend_key.to_string()
    )
    .increment(1);
    if at_ceiling {
        metrics::counter!(
            "router_backend_ticks_at_ceiling_total",
            "backend" => backend_key.to_string()
        )
        .increment(1);
    }
}

/// Updates the active projected KV token gauge for a backend.
pub fn record_kv_projected(backend_key: &str, tokens: i64) {
    metrics::gauge!("router_backend_kv_projected", "backend" => backend_key.to_string())
        .set(tokens as f64);
}

/// Records routing strategy decision duration to `router_decision_duration_seconds`.
pub fn record_decision_duration(strategy: &str, seconds: f64) {
    metrics::histogram!(
        "router_decision_duration_seconds",
        "strategy" => strategy.to_string()
    )
    .record(seconds);
}

/// Records end-to-end pre-dispatch processing overhead to `router_overhead_seconds`.
pub fn record_router_overhead(seconds: f64) {
    metrics::histogram!("router_overhead_seconds").record(seconds);
}

/// Records the ratio of estimated to actual completion tokens upon request stream completion.
pub fn record_output_length_ratio(estimated: u32, actual: u32) {
    if actual > 0 {
        let ratio = estimated as f64 / actual as f64;
        metrics::histogram!("router_output_length_ratio").record(ratio);
    }
}
