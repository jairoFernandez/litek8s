//! A Kubernetes-compatible API server, the first piece of making the
//! Kubernetes object model litek8s's core. It speaks the real API (discovery,
//! CRUD, patch, watch, CRDs) so kubectl and Helm work against it unchanged.
//! See compat/ for the corpus that measures how compatible it is.

mod admission;
mod aggregator;
pub mod agent;
pub mod cel;
pub mod controllers;
pub mod crd_schema;
pub mod defaults;
pub mod discovery;
pub mod dns;
pub mod fields;
pub mod join;
pub mod managed;
pub mod metrics;
pub mod patch;
mod pod_streams;
pub mod protobuf;
mod proto_pointers;
mod proxy;
pub mod quota;
pub mod quantity;
pub mod registry;
pub mod sa_token;
pub mod selector;
pub mod server;
pub(crate) mod spdy;
pub mod store;
pub mod table;
pub mod tls;
pub mod validation;
mod ws;

pub use server::{ApiServerConfig, ApiState, serve, serve_listener, write_kubeconfig};
