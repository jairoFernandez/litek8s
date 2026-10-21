//! WireGuard for the pod overlay between nodes (`kube/agent/overlay.rs`):
//! X25519 keys and the kernel WireGuard device over generic netlink.

pub mod keys;
pub mod wireguard;

pub use keys::{KeyPair, PublicKey, load_or_generate_keypair_at};
