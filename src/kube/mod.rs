//! A Kubernetes-compatible API server, the first piece of making the
//! Kubernetes object model litek8s's core. It speaks the real API (discovery,
//! CRUD, patch, watch, CRDs) so kubectl and Helm work against it unchanged.
//! See compat/ for the corpus that measures how compatible it is.

pub mod discovery;
pub mod patch;
pub mod protobuf;
pub mod registry;
pub mod selector;
pub mod server;
pub mod store;
pub mod table;

pub use server::{ApiServerConfig, ApiState, serve, serve_listener, write_kubeconfig};
