//! VPN pack: WireGuard-managed mesh for cross-cloud clusters.
//!
//! Hub-and-spoke topology, boringtun userspace backend (added later),
//! peer state replicated via raft.
//!
//! See `docs/specs/03-vpn-pack.md`.

pub mod bootstrap;
pub mod config;
pub mod keys;
pub mod storage;
pub mod token;
pub mod tunnel;
pub mod wireguard;

pub use bootstrap::{
    HUB_NODE_NAME, complete_spoke, current_local_membership, init_hub, prepare_spoke,
    register_spoke, remove_spoke,
};
pub use config::{VpnMembership, VpnNetwork, VpnRole, VpnStatus};
pub use keys::{KeyPair, PublicKey};
pub use storage::{HubState, RegisteredPeer, SpokeState, load_or_generate_keypair, load_or_generate_keypair_at};
pub use token::JoinToken;
pub use tunnel::{TunnelHandle, write_wg_quick_config, write_wg_quick_hub_config};
