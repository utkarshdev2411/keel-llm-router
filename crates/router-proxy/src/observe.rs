use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;

pub fn init_tracing() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}

/// Installs the global Prometheus recorder, serving `/metrics` on
/// `admin_bind`. Call once at startup, before any
/// `metrics::counter!`/`gauge!`/`histogram!` call.
pub fn install_metrics_recorder(admin_bind: SocketAddr) -> Result<(), metrics_exporter_prometheus::BuildError> {
    PrometheusBuilder::new().with_http_listener(admin_bind).install()
}

pub fn describe_metrics() {
    metrics::describe_counter!(
        "router_requests_total",
        "Requests by result. `result` is `ok` or `error`; admission never rejects, so there is no `rejected` value."
    );
    metrics::describe_counter!(
        "router_backend_errors_total",
        "Per-backend errors by kind. The primary result metric for routing-policy comparisons."
    );
    metrics::describe_counter!(
        "router_saturated_dispatches_total",
        "How often the admission gate found no eligible backend. A pressure signal, not a drop count."
    );
    metrics::describe_gauge!("router_backend_occupancy", "Fraction of capacity committed, per backend.");
    metrics::describe_gauge!("router_backend_kv_projected", "Projected KV tokens held, per backend.");
    metrics::describe_gauge!("router_backend_inflight", "In-flight requests, per backend.");
    metrics::describe_histogram!("router_decision_duration_seconds", "Time spent in the routing decision, per strategy.");
    metrics::describe_histogram!("router_inbound_duration_seconds", "Time spent parsing and building request features.");
}

pub fn record_request_result(ok: bool) {
    let result = if ok { "ok" } else { "error" };
    metrics::counter!("router_requests_total", "result" => result).increment(1);
}

pub fn record_backend_error(backend_key: &str, kind: &str) {
    metrics::counter!("router_backend_errors_total", "backend" => backend_key.to_string(), "kind" => kind.to_string())
        .increment(1);
}

pub fn record_inflight(backend_key: &str, n: u32) {
    metrics::gauge!("router_backend_inflight", "backend" => backend_key.to_string()).set(n as f64);
}

pub fn record_decision_duration(strategy: &str, seconds: f64) {
    metrics::histogram!("router_decision_duration_seconds", "strategy" => strategy.to_string()).record(seconds);
}
