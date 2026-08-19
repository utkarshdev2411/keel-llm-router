use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;

pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().json().with_env_filter(filter).init();
}

/// Installs the global Prometheus recorder, serving `/metrics` on `admin_bind`.
pub fn install_metrics_recorder(
    admin_bind: SocketAddr,
) -> Result<(), metrics_exporter_prometheus::BuildError> {
    PrometheusBuilder::new().with_http_listener(admin_bind).install()
}

pub fn describe_metrics() {
    // --- Request-level ---
    metrics::describe_counter!(
        "router_requests_total",
        "Requests by result. `result` is `ok` or `error`. \
         There is no `rejected` label: admission never refuses a request."
    );
    metrics::describe_counter!(
        "router_backend_errors_total",
        "Per-backend errors by kind. The primary result metric for routing-policy comparisons."
    );

    // --- Admission gate ---
    metrics::describe_counter!(
        "router_saturated_dispatches_total",
        "How often the admission gate found no eligible backend. \
         A pressure signal, NOT a drop count — every counted request was still dispatched."
    );

    // --- Per-backend live state (Phase 2 mechanism metrics) ---
    metrics::describe_gauge!(
        "router_backend_occupancy",
        "Fraction of capacity committed per backend: max(inflight/max_num_seqs, \
         kv_projected/kv_capacity). The mechanism metric. \
         Spread across backends and time at or above sigma explains the error-rate result."
    );
    metrics::describe_gauge!(
        "router_backend_kv_projected",
        "Projected KV tokens held per backend. \
         Monotonic rise is a leak — the most expensive bug in this project."
    );
    metrics::describe_gauge!("router_backend_inflight", "In-flight requests per backend.");

    // --- Decision path timing ---
    metrics::describe_histogram!(
        "router_decision_duration_seconds",
        "Time spent in the routing decision per strategy. NFR-1 evidence (p99 < 10 µs target)."
    );
    metrics::describe_histogram!(
        "router_overhead_seconds",
        "End-to-end router cost per request: body parse + feature extraction + routing \
         decision, measured to the moment the upstream request is dispatched. \
         This is NFR-3's p99 < 1 ms budget."
    );

    // --- Output-length estimation accuracy (Phase 2) ---
    metrics::describe_histogram!(
        "router_output_length_ratio",
        "Ratio of estimated output tokens to actual (usage.completion_tokens). \
         Values near 1.0 mean the estimate is accurate; persistent drift means \
         the histogram or the max_tokens heuristic is wrong."
    );

    // --- Projection drift (Phase 3 cross-check, registered now) ---
    metrics::describe_gauge!(
        "router_projection_drift",
        "Router's kv_projected/kv_capacity divided by the backend's reported kv_usage_perc. \
         Should sit near 1.0. Departure by 2x in either direction means the kv_model is wrong."
    );
}

// ---------------------------------------------------------------------------
// Recording helpers
// ---------------------------------------------------------------------------

pub fn record_request_result(ok: bool) {
    let result = if ok { "ok" } else { "error" };
    metrics::counter!("router_requests_total", "result" => result).increment(1);
}

pub fn record_backend_error(backend_key: &str, kind: &str) {
    metrics::counter!(
        "router_backend_errors_total",
        "backend" => backend_key.to_string(),
        "kind" => kind.to_string()
    )
    .increment(1);
}

pub fn record_inflight(backend_key: &str, n: u32) {
    metrics::gauge!("router_backend_inflight", "backend" => backend_key.to_string())
        .set(n as f64);
}

/// Phase 2: record occupancy after each dispatch.
pub fn record_occupancy(backend_key: &str, occupancy: f64) {
    metrics::gauge!("router_backend_occupancy", "backend" => backend_key.to_string())
        .set(occupancy);
}

/// Phase 2: record kv_projected after each dispatch.
pub fn record_kv_projected(backend_key: &str, tokens: i64) {
    metrics::gauge!("router_backend_kv_projected", "backend" => backend_key.to_string())
        .set(tokens as f64);
}

pub fn record_decision_duration(strategy: &str, seconds: f64) {
    metrics::histogram!(
        "router_decision_duration_seconds",
        "strategy" => strategy.to_string()
    )
    .record(seconds);
}

pub fn record_router_overhead(seconds: f64) {
    metrics::histogram!("router_overhead_seconds").record(seconds);
}

/// Phase 2: record ratio of estimated to actual output tokens.
/// Called once per completed streaming request when usage frame is available.
pub fn record_output_length_ratio(estimated: u32, actual: u32) {
    if actual > 0 {
        let ratio = estimated as f64 / actual as f64;
        metrics::histogram!("router_output_length_ratio").record(ratio);
    }
}
