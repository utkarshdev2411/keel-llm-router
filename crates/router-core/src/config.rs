use std::collections::HashSet;

use serde::Deserialize;

use crate::cost::{DEFAULT_PENALTY, DEFAULT_SIGMA, DEFAULT_THETA, KvModel};

#[derive(Deserialize)]
pub struct RawConfig {
    pub listener: RawListener,
    #[serde(default)]
    pub routing: RawRouting,
    #[serde(default)]
    pub admission: RawAdmission,
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
}

fn default_strategy() -> String {
    "least_requests".to_string()
}
fn default_theta() -> f64 {
    DEFAULT_THETA
}
fn default_penalty() -> f64 {
    DEFAULT_PENALTY
}

impl Default for RawRouting {
    fn default() -> Self {
        Self {
            strategy: default_strategy(),
            theta: default_theta(),
            penalty: default_penalty(),
            kv_model: None,
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
    pub backends: Vec<RawBackend>,
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
    #[error("duplicate backend url: {0}")]
    DuplicateBackend(String),
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

    #[test]
    fn valid_config_passes() {
        assert!(raw(8192, 32).validate().is_ok());
    }
}
