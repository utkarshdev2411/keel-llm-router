//! Observability, metrics registration, and telemetry recording helper routines.
//!
//! Provides JSON structured tracing initialization, Prometheus metrics setup,
//! and metric helper functions for latency budgets, backend occupancy, and routing decision telemetry.

use std::net::SocketAddr;
use std::time::Instant;

use metrics_exporter_prometheus::PrometheusBuilder;
use router_core::backend::ReportedLoad;

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
        "router_prompt_token_ratio",
        "Router's prompt-token count divided by the backend's reported \
         usage.prompt_tokens. MUST sit at 1.0. Any other value means the configured \
         token_counter does not match what the backend counts, which rescales the \
         admission ceiling by exactly that factor: a ratio of 1.75 turns sigma=0.95 \
         into an effective 0.54."
    );
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

    // --- Signal plane metrics ---
    metrics::describe_counter!(
        "router_signal_scrape_total",
        "Total signal scrape attempts per backend, labeled by result (ok/timeout/error)."
    );
    metrics::describe_histogram!(
        "router_signal_scrape_duration_seconds",
        "Time spent scraping one backend /metrics endpoint, in seconds."
    );
    metrics::describe_gauge!(
        "router_signal_age_seconds",
        "Age in seconds of the currently held signal reading for a backend."
    );
    metrics::describe_gauge!(
        "router_backend_reported_kv_usage",
        "Backend-reported KV cache usage as a fraction 0.0-1.0. Absent when metric unavailable."
    );
    metrics::describe_gauge!(
        "router_backend_num_running",
        "Backend-reported count of currently running requests."
    );
    metrics::describe_gauge!(
        "router_backend_num_waiting",
        "Backend-reported count of requests waiting for a sequence slot."
    );
    metrics::describe_gauge!(
        "router_backend_prefix_hit_rate",
        "Windowed prefix cache hit rate from the backend (delta hits / delta queries)."
    );
    metrics::describe_gauge!(
        "router_backend_preemptions_total",
        "Backend-reported cumulative preemption count. Not emitted on simulator (metric absent)."
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

/// Compare the router's prompt-token count against the backend's own, and shout
/// once if they systematically disagree.
///
/// This guards the failure that invalidated a whole benchmark run: a `chars / 4`
/// count against a word-counting backend read 1.75x high, silently turning
/// `sigma = 0.95` into an effective 0.54 and leaving 46% of KV capacity unused.
/// Nothing errored. The only symptom was a saturated_dispatches counter sitting at
/// 93%, in a file nobody was obliged to read.
///
/// Warns on the running mean, not any single request, so one odd prompt cannot cry
/// wolf; and warns exactly once, because this is a static misconfiguration rather
/// than an event.
pub fn record_prompt_token_ratio(estimated: u32, reported: u32) {
    if reported == 0 {
        return; // Backend did not report a count; nothing to compare against.
    }
    let ratio = estimated as f64 / reported as f64;
    metrics::histogram!("router_prompt_token_ratio").record(ratio);

    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    static SAMPLES: AtomicU64 = AtomicU64::new(0);
    static SUM_MILLI: AtomicU64 = AtomicU64::new(0);
    static WARNED: AtomicBool = AtomicBool::new(false);

    const MIN_SAMPLES: u64 = 50;
    const TOLERANCE: f64 = 0.10;

    let milli = (ratio * 1000.0).round() as u64;
    let n = SAMPLES.fetch_add(1, Ordering::Relaxed) + 1;
    let sum = SUM_MILLI.fetch_add(milli, Ordering::Relaxed) + milli;

    if n < MIN_SAMPLES || WARNED.load(Ordering::Relaxed) {
        return;
    }
    let mean = sum as f64 / 1000.0 / n as f64;
    if (mean - 1.0).abs() > TOLERANCE
        && WARNED
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::error!(
            mean_ratio = mean,
            samples = n,
            "TOKEN COUNT MISMATCH: the router counts {mean:.2}x the prompt tokens the \
             backend reports. Every KV projection is wrong by this factor, so the \
             effective admission ceiling is sigma/{mean:.2}, not sigma. Fix \
             `token_counter` in the router config before trusting any result from \
             this process."
        );
    }
}

/// Records the ratio of estimated to actual completion tokens upon request stream completion.
pub fn record_output_length_ratio(estimated: u32, actual: u32) {
    if actual > 0 {
        let ratio = estimated as f64 / actual as f64;
        metrics::histogram!("router_output_length_ratio").record(ratio);
    }
}

/// Records a signal scrape attempt result and its duration.
///
/// `result` should be `"ok"`, `"timeout"`, or `"error"`.
pub fn record_scrape_result(backend_key: &str, result: &str, duration_secs: f64) {
    metrics::counter!(
        "router_signal_scrape_total",
        "backend" => backend_key.to_string(),
        "result" => result.to_string()
    )
    .increment(1);
    metrics::histogram!(
        "router_signal_scrape_duration_seconds",
        "backend" => backend_key.to_string()
    )
    .record(duration_secs);
}

/// Exports backend-scraped signal values to Prometheus.
///
/// **Critical:** Only emits a gauge when the field is `Some`. Emitting zero for
/// a `None` field recreates exactly the bug the `Option` types exist to prevent —
/// zero reads as "idle backend" and would pin traffic to a backend whose metrics broke.
pub fn record_scraped_metrics(backend_key: &str, reported: &ReportedLoad, now: Instant) {
    let age = now.saturating_duration_since(reported.observed_at).as_secs_f64();
    metrics::gauge!("router_signal_age_seconds", "backend" => backend_key.to_string())
        .set(age);

    if let Some(kv) = reported.kv_usage_perc {
        metrics::gauge!(
            "router_backend_reported_kv_usage",
            "backend" => backend_key.to_string()
        )
        .set(kv as f64);
    }
    if let Some(running) = reported.num_running {
        metrics::gauge!(
            "router_backend_num_running",
            "backend" => backend_key.to_string()
        )
        .set(running as f64);
    }
    if let Some(waiting) = reported.num_waiting {
        metrics::gauge!(
            "router_backend_num_waiting",
            "backend" => backend_key.to_string()
        )
        .set(waiting as f64);
    }
    if let Some(hit_rate) = reported.prefix_hit_rate {
        metrics::gauge!(
            "router_backend_prefix_hit_rate",
            "backend" => backend_key.to_string()
        )
        .set(hit_rate as f64);
    }
    if let Some(preemptions) = reported.preemptions {
        metrics::gauge!(
            "router_backend_preemptions_total",
            "backend" => backend_key.to_string()
        )
        .set(preemptions as f64);
    }
}

/// Record projection drift ratio and warn once if sustained mean sits outside
/// the expected range.
///
/// Copies the warn-once pattern from `record_prompt_token_ratio`: running mean
/// over MIN_SAMPLES, warn exactly once via `AtomicBool::compare_exchange`.
///
/// The warning text names the three benign causes alongside the real one so
/// the reader does not hunt for a bug that may not exist.
pub fn record_projection_drift(backend_key: &str, ratio: f64, warn_ratio: f64) {
    metrics::gauge!("router_projection_drift", "backend" => backend_key.to_string())
        .set(ratio);

    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    static SAMPLES: AtomicU64 = AtomicU64::new(0);
    static SUM_MILLI: AtomicU64 = AtomicU64::new(0);
    static WARNED: AtomicBool = AtomicBool::new(false);

    const MIN_SAMPLES: u64 = 50;

    let milli = (ratio * 1000.0).round() as u64;
    let n = SAMPLES.fetch_add(1, Ordering::Relaxed) + 1;
    let sum = SUM_MILLI.fetch_add(milli, Ordering::Relaxed) + milli;

    if n < MIN_SAMPLES || WARNED.load(Ordering::Relaxed) {
        return;
    }
    let mean = sum as f64 / 1000.0 / n as f64;
    let lo = 1.0 / warn_ratio;
    let hi = warn_ratio;
    if (mean < lo || mean > hi)
        && WARNED
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(
            mean_ratio = mean,
            samples = n,
            warn_ratio = warn_ratio,
            backend = backend_key,
            "PROJECTION DRIFT: router projection is {mean:.2}x the backend reported KV \
             usage. Three benign causes to check before assuming a bug: \
             (1) Wrong kv_model — prompt_only vs prompt_plus_output changes the \
             projection by the output/prompt ratio; \
             (2) Block rounding — the backend allocates ceil(tokens/block_size)*block_size \
             but the router projects raw tokens, causing a few-percent under-projection \
             on short prompts; \
             (3) Prefix deduplication — identical prompts share blocks so the backend \
             reports far less than the router projects under prefix-heavy traffic, \
             and that is the prefix cache working correctly, not drift. \
             If none of these apply, check kv_capacity_tokens in the router config."
        );
    }
}
