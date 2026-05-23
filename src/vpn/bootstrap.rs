//! High-level VPN operations that compose keys + state + tunnel.
//!
//! These helpers are the entry points the CLI calls. They are written so they
//! can later be re-hosted inside the daemon (talking to raft) without changing
//! the surface — for now they touch local files only.

use std::net::Ipv4Addr;

use anyhow::{Context, Result, bail};

use super::{
    HubState, JoinToken, KeyPair, PublicKey, SpokeState, VpnMembership, VpnNetwork, VpnRole,
    VpnStatus, load_or_generate_keypair,
};

/// Initialize (or reload) the hub side. Persists the hub key pair on first
/// call and writes a `HubState` with the requested network. Returns
/// `(network, keypair, token)` for the caller to display.
pub fn init_hub(
    cidr_base: Ipv4Addr,
    cidr_prefix: u8,
    public_endpoint: String,
) -> Result<(VpnNetwork, KeyPair, JoinToken)> {
    let keypair = load_or_generate_keypair()?;
    let network = VpnNetwork::new(cidr_base, cidr_prefix, public_endpoint, keypair.public())?;

    let mut state = HubState::load()?;
    state.network = Some(network.clone());

    let already_registered = state
        .peers
        .iter()
        .any(|peer| peer.node_name == HUB_NODE_NAME);
    if !already_registered {
        state.peers.push(super::RegisteredPeer {
            node_name: HUB_NODE_NAME.to_string(),
            membership: VpnMembership {
                role: VpnRole::Hub,
                ip: network.hub_ip(),
                pubkey: keypair.public(),
                endpoint: Some(network.hub_endpoint.clone()),
                last_handshake_unix: None,
                status: VpnStatus::Down,
            },
        });
    }

    state.save()?;

    let token = JoinToken {
        hub_pubkey: keypair.public(),
        hub_endpoint: network.hub_endpoint.clone(),
        cidr_base: network.cidr_base,
        cidr_prefix: network.cidr_prefix,
    };

    Ok((network, keypair, token))
}

pub const HUB_NODE_NAME: &str = "hub";

/// Register a spoke on the hub. Allocates the next free IP and persists the
/// peer entry. Returns the assigned membership so the caller can ship it back
/// to the spoke (via SSH bootstrap or out-of-band).
pub fn register_spoke(node_name: &str, spoke_pubkey: PublicKey) -> Result<VpnMembership> {
    let mut state = HubState::load()?;
    let network = state
        .network
        .clone()
        .context("hub has no network — run `litek8s vpn init` first")?;

    if let Some(existing) = state.find_peer_by_pubkey(&spoke_pubkey) {
        return Ok(existing.membership.clone());
    }

    let taken = state.taken_ips();
    let assigned_ip = network
        .next_spoke_ip(&taken)
        .context("VPN CIDR is exhausted: no free IPs left for a new spoke")?;

    let membership = VpnMembership::new_spoke(assigned_ip, spoke_pubkey);
    state.upsert_peer(node_name.to_string(), membership.clone());
    state.save()?;
    Ok(membership)
}

/// Remove a peer by node name. Returns whether a peer was actually removed.
pub fn remove_spoke(node_name: &str) -> Result<bool> {
    let mut state = HubState::load()?;
    let removed = state.remove_peer(node_name);
    if removed {
        state.save()?;
    }
    Ok(removed)
}

/// Apply a join token on a spoke. Generates a local key pair (or reuses the
/// persisted one), records the spoke state, and returns the new
/// `(SpokeState, KeyPair)` so the caller can hand-deliver the spoke's pubkey
/// to the hub.
pub fn prepare_spoke(token: &JoinToken) -> Result<(SpokeState, KeyPair)> {
    let keypair = load_or_generate_keypair()?;
    let network = VpnNetwork::new(
        token.cidr_base,
        token.cidr_prefix,
        token.hub_endpoint.clone(),
        token.hub_pubkey.clone(),
    )?;

    // IP is not known until the hub assigns one. We persist a placeholder
    // (0.0.0.0) so callers can see the partial state; `complete_spoke` rewrites
    // it once the hub responds.
    let membership = VpnMembership::new_spoke(Ipv4Addr::new(0, 0, 0, 0), keypair.public());
    let state = SpokeState {
        network,
        membership,
    };
    state.save()?;
    Ok((state, keypair))
}

/// Resolve the local node's VPN membership, if any.
///
/// Prefers the spoke side (we joined a remote hub) over the hub side (we ARE
/// the hub). Returns `None` when no VPN state is present on disk.
pub fn current_local_membership() -> Result<Option<VpnMembership>> {
    if let Some(spoke) = SpokeState::load()? {
        return Ok(Some(spoke.membership));
    }
    let hub = HubState::load()?;
    if let Some(network) = hub.network {
        let hub_peer = hub
            .peers
            .into_iter()
            .find(|peer| peer.node_name == HUB_NODE_NAME);
        if let Some(peer) = hub_peer {
            return Ok(Some(peer.membership));
        }
        return Ok(Some(VpnMembership {
            role: VpnRole::Hub,
            ip: network.hub_ip(),
            pubkey: network.hub_pubkey.clone(),
            endpoint: Some(network.hub_endpoint.clone()),
            last_handshake_unix: None,
            status: VpnStatus::Down,
        }));
    }
    Ok(None)
}

/// Finalize spoke state once the hub has assigned an IP.
pub fn complete_spoke(assigned: VpnMembership) -> Result<SpokeState> {
    let mut state = SpokeState::load()?
        .context("spoke has no pending join — run `litek8s join --vpn-token ...` first")?;
    if state.membership.pubkey != assigned.pubkey {
        bail!(
            "hub assigned a membership whose pubkey does not match this spoke (got {}, expected {})",
            assigned.pubkey.as_str(),
            state.membership.pubkey.as_str()
        );
    }
    state.membership = assigned;
    state.save()?;
    Ok(state)
}
