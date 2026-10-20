//! A practical PromQL subset: enough for Grafana's Kubernetes dashboards
//! and for alert rules. See docs/observability.md for what is supported.

pub mod eval;
pub mod parser;

pub use eval::{Sample, Series, Value, format_value, instant, range};
pub use parser::{Expr, parse};
