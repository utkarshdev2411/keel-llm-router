use std::sync::Arc;

use arc_swap::{ArcSwap, ArcSwapOption};
use router_core::backend::{Backend, BackendId, CapsEstimate, HealthState, LiveCounters, Snapshot};
use router_core::config::RawConfig;
use router_core::strategy::{LeastKvts, LeastRequests, P2c, Pressure, RoundRobin, RoutingStrategy};
use router_proxy::observe;
use router_proxy::router::RouterState;
use router_proxy::signal::{SignalConfig, spawn_collectors};
use router_proxy::upstream;

fn main() -> anyhow::Result<()> {
    observe::init_tracing();

    let config_path = std::env::args().nth(1).unwrap_or_else(|| "router.toml".to_string());
    let raw = std::fs::read_to_string(&config_path)?;
    let raw: RawConfig = toml::from_str(&raw)?;
    let config = raw.validate()?;

    let admin_bind: std::net::SocketAddr = config.admin_bind.parse()?;
    observe::install_metrics_recorder(admin_bind).map_err(|e| {
        anyhow::anyhow!(
            "could not bind the admin/metrics listener on {admin_bind}: {e}. \
             Another router is probably still running -- check with `ss -ltn` and \
             kill it. Refusing to start without metrics."
        )
    })?;
    observe::describe_metrics();

    let backends: Vec<Arc<Backend>> = config
        .backends
        .iter()
        .enumerate()
        .map(|(i, b)| {
            Arc::new(Backend {
                id: BackendId(i as u16),
                key: b.url.as_str().into(),
                uri: b.url.as_str().into(),
                model: b.model.as_str().into(),
                weight: b.weight,
                caps: CapsEstimate {
                    kv_capacity_tokens: b.kv_tokens,
                    max_num_seqs: b.max_num_seqs,
                },
                live: LiveCounters::default(),
                reported: ArcSwapOption::from(None),
                health: HealthState::default(),
            })
        })
        .collect();

    let healthy: Box<[BackendId]> = backends.iter().map(|b| b.id).collect();
    let snapshot = Snapshot { epoch: 0, backends, healthy, ring: Box::new([]) };

    let kv_model = config.kv_model;

    let strategy: Box<dyn RoutingStrategy> = match config.strategy.as_str() {
        "round_robin" => Box::new(RoundRobin::new()),
        "p2c" => Box::new(P2c),
        "least_requests" | "least_conn" => Box::new(LeastRequests),
        "least_kvts" => Box::new(LeastKvts),
        "pressure" => Box::new(Pressure {
            theta: config.theta,
            penalty: config.penalty,
            sigma: config.sigma,
            kv_model,
        }),
        other => anyhow::bail!("unknown strategy {other:?}"),
    };
    tracing::info!(
        strategy = strategy.name(),
        kv_model = ?kv_model,
        token_counter = ?config.token_counter.kind(),
        "starting router"
    );

    let state = Arc::new(RouterState {
        snapshot: ArcSwap::from_pointee(snapshot),
        strategy,
        client: upstream::build_client(),
        max_request_body_bytes: config.max_request_body_bytes,
        decision_trace_sample_rate: config.decision_trace_sample_rate,
        kv_model,
        route_hists: Arc::new(router_proxy::length_estimator::RouteHistograms::new(
            config.route_p50_halflife_s,
        )),
        token_counter: config.token_counter,
    });

    let signal_cfg = SignalConfig {
        enabled: config.signal.enabled,
        scrape_interval: config.signal.scrape_interval,
        scrape_timeout: config.signal.scrape_timeout,
        max_signal_age: config.signal.max_signal_age,
        drift_warn_ratio: config.signal.drift_warn_ratio,
        validate_capacity_at_startup: config.signal.validate_capacity_at_startup,
    };

    let runtime = tokio::runtime::Runtime::new()?;

    // Startup capacity assertion — runs synchronously before accepting traffic.
    if signal_cfg.validate_capacity_at_startup {
        let backends_ref = state.snapshot.load();
        let assertion_timeout = signal_cfg.scrape_timeout + std::time::Duration::from_millis(500);
        runtime.block_on(validate_backend_capacities(&backends_ref.backends, assertion_timeout))?;
    }

    let sample_interval = std::time::Duration::from_millis(config.occupancy_sample_interval_ms);
    runtime.spawn(router_proxy::sampler::sample_occupancy_loop(
        state.clone(),
        config.sigma,
        sample_interval,
    ));

    // Spawn one signal collector task per backend. Hold handles alive for process lifetime.

    let _signal_handles = {
        let _enter = runtime.enter();
        spawn_collectors(state.clone(), signal_cfg)
    };

    runtime.block_on(router_proxy::listener::serve(&config.listener_bind, state))?;

    Ok(())
}

/// Scrape each backend once at startup and assert that its self-reported KV capacity
/// matches the router config.
///
/// `kv_capacity_tokens = block_size * num_gpu_blocks` is the denominator of every
/// occupancy calculation. A mismatch rescales sigma silently, exactly like the
/// 1.75x token-count bug did. This catches it at startup rather than in a benchmark.
///
/// * Match     → info!, proceed
/// * Mismatch  → hard error, refuse to start
/// * Unreachable / metric absent → warn!, proceed (observability outage ≠ misconfiguration)
async fn validate_backend_capacities(
    backends: &[Arc<Backend>],
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    use router_proxy::signal::scrape::parse_capacity;
    use router_proxy::signal::ScrapeError;
    use router_proxy::upstream;

    // Same pooled client the request path and the signal collectors use --
    // no separate HTTP stack, no separate TLS dependency, for one plaintext
    // GET at startup.
    let client = upstream::build_client();

    let mut any_mismatch = false;

    for backend in backends {
        let metrics_url = format!("{}/metrics", backend.uri.trim_end_matches('/'));

        match router_proxy::signal::fetch_metrics(&client, &metrics_url, timeout).await {
            Ok(text) => match parse_capacity(&text) {
                Some(cap) => {
                    let expected = cap.block_size * cap.num_gpu_blocks;
                    let configured = backend.caps.kv_capacity_tokens;
                    if expected == configured {
                        tracing::info!(
                            backend = %backend.key,
                            capacity = expected,
                            block_size = cap.block_size,
                            num_gpu_blocks = cap.num_gpu_blocks,
                            "capacity assertion passed"
                        );
                    } else {
                        tracing::error!(
                            backend = %backend.key,
                            backend_reports = expected,
                            config_has = configured,
                            block_size = cap.block_size,
                            num_gpu_blocks = cap.num_gpu_blocks,
                            "CAPACITY MISMATCH: backend reports {expected} tokens \
                             (block_size={} * num_gpu_blocks={}), but config has \
                             kv_tokens={configured}. Every occupancy fraction is wrong \
                             by this factor. Fix kv_tokens in the router config.",
                            cap.block_size, cap.num_gpu_blocks
                        );
                        any_mismatch = true;
                    }
                }
                None => {
                    tracing::warn!(
                        backend = %backend.key,
                        "capacity assertion skipped: cache_config_info metric absent from /metrics"
                    );
                }
            },
            Err(ScrapeError::Timeout) => {
                tracing::warn!(
                    backend = %backend.key,
                    "capacity assertion skipped: /metrics timed out"
                );
            }
            Err(e) => {
                tracing::warn!(
                    backend = %backend.key,
                    error = %e,
                    "capacity assertion skipped: /metrics unreachable"
                );
            }
        }
    }

    if any_mismatch {
        anyhow::bail!(
            "one or more backends reported a KV capacity that does not match \
             the router config. See the CAPACITY MISMATCH log lines above. \
             Fix kv_tokens in the router config before starting."
        );
    }

    Ok(())
}
