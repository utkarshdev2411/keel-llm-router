use std::sync::Arc;

use arc_swap::{ArcSwap, ArcSwapOption};
use router_core::backend::{Backend, BackendId, CapsEstimate, HealthState, LiveCounters, Snapshot};
use router_core::config::RawConfig;
use router_core::strategy::{LeastKvts, LeastRequests, P2c, Pressure, RoundRobin, RoutingStrategy};
use router_proxy::observe;
use router_proxy::router::RouterState;
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
    tracing::info!(strategy = strategy.name(), kv_model = ?kv_model, "starting router");

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
    });

    let runtime = tokio::runtime::Runtime::new()?;

    let sample_interval = std::time::Duration::from_millis(config.occupancy_sample_interval_ms);
    runtime.spawn(router_proxy::sampler::sample_occupancy_loop(
        state.clone(),
        config.sigma,
        sample_interval,
    ));

    runtime.block_on(router_proxy::listener::serve(&config.listener_bind, state))?;

    Ok(())
}
