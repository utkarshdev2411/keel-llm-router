use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;

use crate::cost::{DEFAULT_PENALTY, DEFAULT_SIGMA, DEFAULT_THETA, KvModel};
use crate::tokens::{TokenCounter, TokenCounterKind};

#[derive(Deserialize)]
pub struct RawConfig {
    pub listener: RawListener,
    #[serde(default)]
    pub routing: RawRouting,
    #[serde(default)]
    pub admission: RawAdmission,
    #[serde(default)]
    pub observability: RawObservability,
    #[serde(default)]
    pub signal: RawSignal,
    pub backends: Vec<RawBackend>,
}

#[derive(Deserialize)]
pub struct RawListener {
    pub bind: String,
    pub admin_bind: String,
    #[serde(default = "default_max_body")]
    pub max_request_body_bytes: usize,
}

fn default_max_body() -> usize {
    4 * 1024 * 1024
}

#[derive(Deserialize)]
pub struct RawRouting {
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default = "default_theta")]
    pub theta: f64,
    #[serde(default = "default_penalty")]
    pub penalty: f64,
    #[serde(default)]
    pub kv_model: Option<KvModel>,
    #[serde(default = "default_trace_sample_rate")]
    pub decision_trace_sample_rate: f64,
    /// Half-life in seconds for the per-route output-length histogram (Phase 2).
    #[serde(default = "default_route_p50_halflife_s")]
    pub route_p50_halflife_s: f64,
    /// How to count prompt tokens. MUST match what the backend counts: this is the
    /// numerator of every KV projection, so a mismatch rescales sigma silently.
    #[serde(default = "default_token_counter")]
    pub token_counter: TokenCounterKind,
    /// Divisor for `token_counter = "chars_per_token"`. Ignored otherwise.
    #[serde(default = "default_chars_per_token")]
    pub chars_per_token: f64,
}

fn default_strategy() -> String {
    // Phase 2: pressure is now the default shipping strategy.
    "pressure".to_string()
}
fn default_theta() -> f64 {
    DEFAULT_THETA
}
fn default_penalty() -> f64 {
    DEFAULT_PENALTY
}
fn default_trace_sample_rate() -> f64 {
    0.01
}
fn default_route_p50_halflife_s() -> f64 {
    300.0 // 5 minutes, per algorithm spec §11
}
fn default_token_counter() -> TokenCounterKind {
    // Exact for llm-d-inference-sim, and a defensible floor elsewhere. The old
    // chars/4 default was silently wrong by 1.75x against that backend.
    TokenCounterKind::Whitespace
}
fn default_chars_per_token() -> f64 {
    4.0
}

impl Default for RawRouting {
    fn default() -> Self {
        Self {
            strategy: default_strategy(),
            theta: default_theta(),
            penalty: default_penalty(),
            kv_model: None,
            decision_trace_sample_rate: default_trace_sample_rate(),
            route_p50_halflife_s: default_route_p50_halflife_s(),
            token_counter: default_token_counter(),
            chars_per_token: default_chars_per_token(),
        }
    }
}

#[derive(Deserialize)]
pub struct RawAdmission {
    #[serde(default = "default_sigma")]
    pub sigma: f64,
}

fn default_sigma() -> f64 {
    DEFAULT_SIGMA
}

impl Default for RawAdmission {
    fn default() -> Self {
        Self { sigma: default_sigma() }
    }
}

#[derive(Deserialize)]
pub struct RawObservability {
    /// Tick for the periodic all-backend occupancy sampler. The mechanism half
    /// of the Phase 2 criterion is a fraction of wall-clock time, so it needs a
    /// traffic-independent sample: a backend the policy is correctly avoiding
    /// generates no requests and would otherwise never be observed.
    #[serde(default = "default_occupancy_sample_interval_ms")]
    pub occupancy_sample_interval_ms: u64,
}

fn default_occupancy_sample_interval_ms() -> u64 {
    100
}

impl Default for RawObservability {
    fn default() -> Self {
        Self { occupancy_sample_interval_ms: default_occupancy_sample_interval_ms() }
    }
}

#[derive(Deserialize)]
pub struct RawSignal {
    #[serde(default = "default_signal_enabled")]
    pub enabled: bool,
    #[serde(default = "default_scrape_interval_ms")]
    pub scrape_interval_ms: u64,
    #[serde(default = "default_scrape_timeout_ms")]
    pub scrape_timeout_ms: u64,
    #[serde(default = "default_max_signal_age_ms")]
    pub max_signal_age_ms: u64,
    #[serde(default = "default_drift_warn_ratio")]
    pub drift_warn_ratio: f64,
    #[serde(default = "default_validate_capacity")]
    pub validate_capacity_at_startup: bool,
}

fn default_signal_enabled() -> bool {
    true
}
fn default_scrape_interval_ms() -> u64 {
    1000
}
fn default_scrape_timeout_ms() -> u64 {
    500
}
fn default_max_signal_age_ms() -> u64 {
    5000
}
fn default_drift_warn_ratio() -> f64 {
    2.0
}
fn default_validate_capacity() -> bool {
    true
}

impl Default for RawSignal {
    fn default() -> Self {
        Self {
            enabled: default_signal_enabled(),
            scrape_interval_ms: default_scrape_interval_ms(),
            scrape_timeout_ms: default_scrape_timeout_ms(),
            max_signal_age_ms: default_max_signal_age_ms(),
            drift_warn_ratio: default_drift_warn_ratio(),
            validate_capacity_at_startup: default_validate_capacity(),
        }
    }
}

#[derive(Deserialize)]
pub struct RawBackend {
    pub url: String,
    pub model: String,
    pub kv_tokens: u32,
    pub max_num_seqs: u32,
    #[serde(default = "default_weight")]
    pub weight: f32,
}

fn default_weight() -> f32 {
    1.0
}

pub struct Config {
    pub listener_bind: String,
    pub admin_bind: String,
    pub max_request_body_bytes: usize,
    pub strategy: String,
    pub theta: f64,
    pub penalty: f64,
    pub sigma: f64,
    pub kv_model: KvModel,
    pub decision_trace_sample_rate: f64,
    pub route_p50_halflife_s: f64,
    pub token_counter: TokenCounter,
    pub occupancy_sample_interval_ms: u64,
    pub signal: SignalConfig,
    pub backends: Vec<RawBackend>,
}

pub struct SignalConfig {
    pub enabled: bool,
    pub scrape_interval: Duration,
    pub scrape_timeout: Duration,
    pub max_signal_age: Duration,
    pub drift_warn_ratio: f64,
    pub validate_capacity_at_startup: bool,
}

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("backend {0}: kv_tokens must be > 0")]
    ZeroKvCapacity(String),
    #[error("backend {0}: max_num_seqs must be > 0")]
    ZeroMaxNumSeqs(String),
    #[error("theta must be in (0,1), got {0}")]
    ThetaOutOfRange(f64),
    #[error("sigma must be in (0,1], got {0}")]
    SigmaOutOfRange(f64),
    #[error("no backends configured")]
    NoBackends,
    #[error("occupancy_sample_interval_ms must be > 0")]
    ZeroSampleInterval,
    #[error("chars_per_token must be finite and > 0, got {0}")]
    BadCharsPerToken(f64),
    #[error("unknown strategy {0:?} (expected one of: pressure, p2c, least_requests, round_robin, least_kvts)")]
    UnknownStrategy(String),
    #[error("duplicate backend url: {0}")]
    DuplicateBackend(String),
    #[error("signal: scrape_timeout_ms must be strictly less than scrape_interval_ms, got timeout={0}ms >= interval={1}ms")]
    SignalTimeoutNotLessThanInterval(u64, u64),
    #[error("signal: scrape_timeout_ms and scrape_interval_ms must be > 0, got timeout={0}ms, interval={1}ms")]
    SignalTimeoutsNotPositive(u64, u64),
    #[error("signal: max_signal_age_ms must be >= scrape_interval_ms, got age={0}ms < interval={1}ms")]
    SignalAgeBeforeInterval(u64, u64),
    #[error("signal: drift_warn_ratio must be > 1.0, got {0}")]
    SignalDriftRatioTooLow(f64),
}

impl RawConfig {
    pub fn validate(self) -> Result<Config, ConfigError> {
        if self.backends.is_empty() {
            return Err(ConfigError::NoBackends);
        }
        if !(0.0 < self.routing.theta && self.routing.theta < 1.0) {
            return Err(ConfigError::ThetaOutOfRange(self.routing.theta));
        }
        if !(0.0 < self.admission.sigma && self.admission.sigma <= 1.0) {
            return Err(ConfigError::SigmaOutOfRange(self.admission.sigma));
        }
        if self.observability.occupancy_sample_interval_ms == 0 {
            return Err(ConfigError::ZeroSampleInterval);
        }
        // Validated even under `whitespace`, so switching modes later cannot
        // activate a divisor that was never checked.
        if !(self.routing.chars_per_token.is_finite() && self.routing.chars_per_token > 0.0) {
            return Err(ConfigError::BadCharsPerToken(self.routing.chars_per_token));
        }
        // Reject an unknown strategy rather than silently falling back. A typo in a
        // comparison config would otherwise run the default policy under the other
        // arm's name, and the benchmark would compare a policy against itself.
        if !matches!(
            self.routing.strategy.as_str(),
            "pressure" | "p2c" | "least_requests" | "least_conn" | "round_robin" | "least_kvts"
        ) {
            return Err(ConfigError::UnknownStrategy(self.routing.strategy.clone()));
        }

        // Signal plane validation
        if self.signal.scrape_timeout_ms == 0 || self.signal.scrape_interval_ms == 0 {
            return Err(ConfigError::SignalTimeoutsNotPositive(
                self.signal.scrape_timeout_ms,
                self.signal.scrape_interval_ms,
            ));
        }
        if self.signal.scrape_timeout_ms >= self.signal.scrape_interval_ms {
            return Err(ConfigError::SignalTimeoutNotLessThanInterval(
                self.signal.scrape_timeout_ms,
                self.signal.scrape_interval_ms,
            ));
        }
        if self.signal.max_signal_age_ms < self.signal.scrape_interval_ms {
            return Err(ConfigError::SignalAgeBeforeInterval(
                self.signal.max_signal_age_ms,
                self.signal.scrape_interval_ms,
            ));
        }
        if self.signal.drift_warn_ratio <= 1.0 {
            return Err(ConfigError::SignalDriftRatioTooLow(self.signal.drift_warn_ratio));
        }

        let mut seen = HashSet::new();
        for b in &self.backends {
            if b.kv_tokens == 0 {
                return Err(ConfigError::ZeroKvCapacity(b.url.clone()));
            }
            if b.max_num_seqs == 0 {
                return Err(ConfigError::ZeroMaxNumSeqs(b.url.clone()));
            }
            if !seen.insert(b.url.clone()) {
                return Err(ConfigError::DuplicateBackend(b.url.clone()));
            }
        }

        Ok(Config {
            listener_bind: self.listener.bind,
            admin_bind: self.listener.admin_bind,
            max_request_body_bytes: self.listener.max_request_body_bytes,
            strategy: self.routing.strategy,
            theta: self.routing.theta,
            penalty: self.routing.penalty,
            sigma: self.admission.sigma,
            kv_model: self.routing.kv_model.unwrap_or(KvModel::PromptOnly),
            decision_trace_sample_rate: self.routing.decision_trace_sample_rate,
            route_p50_halflife_s: self.routing.route_p50_halflife_s,
            token_counter: TokenCounter::new(
                self.routing.token_counter,
                self.routing.chars_per_token,
            ),
            occupancy_sample_interval_ms: self.observability.occupancy_sample_interval_ms,
            signal: SignalConfig {
                enabled: self.signal.enabled,
                scrape_interval: Duration::from_millis(self.signal.scrape_interval_ms),
                scrape_timeout: Duration::from_millis(self.signal.scrape_timeout_ms),
                max_signal_age: Duration::from_millis(self.signal.max_signal_age_ms),
                drift_warn_ratio: self.signal.drift_warn_ratio,
                validate_capacity_at_startup: self.signal.validate_capacity_at_startup,
            },
            backends: self.backends,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(kv: u32, seqs: u32) -> RawConfig {
        RawConfig {
            listener: RawListener {
                bind: "0.0.0.0:8080".into(),
                admin_bind: "127.0.0.1:9090".into(),
                max_request_body_bytes: default_max_body(),
            },
            routing: RawRouting::default(),
            admission: RawAdmission::default(),
            observability: RawObservability::default(),
            signal: RawSignal::default(),
            backends: vec![RawBackend {
                url: "http://a:8000".into(),
                model: "m".into(),
                kv_tokens: kv,
                max_num_seqs: seqs,
                weight: 1.0,
            }],
        }
    }

    #[test]
    fn zero_denominator_rejected_at_load() {
        assert!(matches!(raw(0, 32).validate(), Err(ConfigError::ZeroKvCapacity(_))));
        assert!(matches!(raw(8192, 0).validate(), Err(ConfigError::ZeroMaxNumSeqs(_))));
    }

    /// A typo must not silently start the default policy. If it did, a
    /// comparison run would measure the default against itself under the
    /// other arm's name and report a null result as a real one.
    #[test]
    fn unknown_strategy_is_rejected_not_defaulted() {
        let mut c = raw(8192, 32);
        c.routing.strategy = "presure".into();
        assert!(matches!(c.validate(), Err(ConfigError::UnknownStrategy(_))));
    }

    #[test]
    fn every_shipping_strategy_name_is_accepted() {
        for name in ["pressure", "p2c", "least_requests", "least_conn", "round_robin", "least_kvts"] {
            let mut c = raw(8192, 32);
            c.routing.strategy = name.into();
            assert!(c.validate().is_ok(), "{name} must be a valid strategy name");
        }
    }

    #[test]
    fn bad_chars_per_token_rejected_even_under_whitespace_mode() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut c = raw(8192, 32);
            c.routing.chars_per_token = bad;
            assert!(matches!(c.validate(), Err(ConfigError::BadCharsPerToken(_))),
                "chars_per_token={bad} must be rejected");
        }
    }

    /// The default must be the mode that is exact for the backend in use. A
    /// chars/4 default silently rescaled sigma by 1.75x.
    #[test]
    fn default_token_counter_is_exact() {
        assert!(raw(8192, 32).validate().unwrap().token_counter.is_exact());
    }

    #[test]
    fn zero_sample_interval_rejected() {
        let mut c = raw(8192, 32);
        c.observability.occupancy_sample_interval_ms = 0;
        assert!(matches!(c.validate(), Err(ConfigError::ZeroSampleInterval)));
    }

    #[test]
    fn valid_config_passes() {
        assert!(raw(8192, 32).validate().is_ok());
    }

    #[test]
    fn pressure_is_default_strategy() {
        let cfg = raw(8192, 32).validate().unwrap();
        assert_eq!(cfg.strategy, "pressure");
    }

    #[test]
    fn sigma_out_of_range_rejected() {
        let mut r = raw(8192, 32);
        r.admission.sigma = 1.1;
        assert!(matches!(r.validate(), Err(ConfigError::SigmaOutOfRange(_))));

        let mut r2 = raw(8192, 32);
        r2.admission.sigma = 0.0;
        assert!(matches!(r2.validate(), Err(ConfigError::SigmaOutOfRange(_))));
    }

    #[test]
    fn signal_timeout_not_less_than_interval_rejected() {
        let mut r = raw(8192, 32);
        r.signal.scrape_timeout_ms = 1000;
        r.signal.scrape_interval_ms = 1000;
        assert!(matches!(
            r.validate(),
            Err(ConfigError::SignalTimeoutNotLessThanInterval(1000, 1000))
        ));

        let mut r2 = raw(8192, 32);
        r2.signal.scrape_timeout_ms = 1500;
        r2.signal.scrape_interval_ms = 1000;
        assert!(matches!(
            r2.validate(),
            Err(ConfigError::SignalTimeoutNotLessThanInterval(1500, 1000))
        ));
    }

    #[test]
    fn signal_timeouts_not_positive_rejected() {
        let mut r = raw(8192, 32);
        r.signal.scrape_timeout_ms = 0;
        r.signal.scrape_interval_ms = 1000;
        assert!(matches!(
            r.validate(),
            Err(ConfigError::SignalTimeoutsNotPositive(0, 1000))
        ));

        let mut r2 = raw(8192, 32);
        r2.signal.scrape_timeout_ms = 500;
        r2.signal.scrape_interval_ms = 0;
        assert!(matches!(
            r2.validate(),
            Err(ConfigError::SignalTimeoutsNotPositive(500, 0))
        ));
    }

    #[test]
    fn signal_age_before_interval_rejected() {
        let mut r = raw(8192, 32);
        r.signal.max_signal_age_ms = 500;
        r.signal.scrape_interval_ms = 1000;
        assert!(matches!(
            r.validate(),
            Err(ConfigError::SignalAgeBeforeInterval(500, 1000))
        ));
    }

    #[test]
    fn signal_drift_ratio_too_low_rejected() {
        let mut r = raw(8192, 32);
        r.signal.drift_warn_ratio = 1.0;
        assert!(matches!(
            r.validate(),
            Err(ConfigError::SignalDriftRatioTooLow(1.0))
        ));

        let mut r2 = raw(8192, 32);
        r2.signal.drift_warn_ratio = 0.5;
        assert!(matches!(
            r2.validate(),
            Err(ConfigError::SignalDriftRatioTooLow(0.5))
        ));
    }

    #[test]
    fn signal_valid_config_passes() {
        let mut r = raw(8192, 32);
        r.signal.scrape_timeout_ms = 500;
        r.signal.scrape_interval_ms = 1000;
        r.signal.max_signal_age_ms = 5000;
        r.signal.drift_warn_ratio = 2.0;
        let cfg = r.validate().unwrap();
        assert_eq!(cfg.signal.scrape_timeout.as_millis(), 500);
        assert_eq!(cfg.signal.scrape_interval.as_millis(), 1000);
        assert_eq!(cfg.signal.max_signal_age.as_millis(), 5000);
        assert_eq!(cfg.signal.drift_warn_ratio, 2.0);
        assert!(cfg.signal.enabled);
        assert!(cfg.signal.validate_capacity_at_startup);
    }

    #[test]
    fn signal_default_values() {
        let cfg = raw(8192, 32).validate().unwrap();
        assert_eq!(cfg.signal.scrape_timeout.as_millis(), 500);
        assert_eq!(cfg.signal.scrape_interval.as_millis(), 1000);
        assert_eq!(cfg.signal.max_signal_age.as_millis(), 5000);
        assert_eq!(cfg.signal.drift_warn_ratio, 2.0);
        assert!(cfg.signal.enabled);
        assert!(cfg.signal.validate_capacity_at_startup);
    }
}
