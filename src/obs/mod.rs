//! Observability built in: metrics in an embedded time-series store with a
//! Prometheus-compatible query API, cluster-wide log search, and alerts.
//! See docs/observability.md.

pub mod alerts;
pub mod api;
pub mod expo;
pub mod kube_state;
pub mod notify;
pub mod server;
pub mod tsdb;
pub mod promql;
pub mod logs;
pub mod node;

use serde::{Deserialize, Serialize};

/// What the API server tells node agents (in `Welcome`) when observability
/// is on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObsSettings {
    /// How often to sample, in milliseconds.
    pub interval_ms: u64,
    /// How long archived logs (of removed containers) are kept, seconds.
    pub log_retention_secs: u64,
    /// Most bytes of archived logs per node.
    pub log_max_bytes: u64,
}

/// Features node agents announce in `Hello`.
pub const AGENT_FEATURES: [&str; 2] = ["metrics", "logs"];
