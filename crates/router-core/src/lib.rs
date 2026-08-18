#![forbid(unsafe_code)]

pub mod backend;
pub mod config;
pub mod cost;
pub mod features;
pub mod gate;
pub mod lease;
pub mod strategy;
pub mod trace;

pub use backend::{Backend, BackendId, CapsEstimate, HealthState, LiveCounters, ReportedLoad, Snapshot};
pub use config::{Config, ConfigError, RawConfig};
pub use cost::KvModel;
pub use features::RequestFeatures;
pub use lease::CostLease;
pub use strategy::RoutingStrategy;
pub use trace::{CandidateScore, DecisionTrace, GateReason};
