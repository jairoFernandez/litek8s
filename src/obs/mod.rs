//! Observability built in: metrics in an embedded time-series store with a
//! Prometheus-compatible query API, cluster-wide log search, and alerts.
//! See docs/observability.md.

pub mod tsdb;
pub mod promql;
