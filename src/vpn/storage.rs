//! On-disk persistence for VPN state.
//!
//! Phase 1 stores hub and spoke state in local JSON files under
//! `~/.local/share/litek8s/vpn/`. Phase 2 will replicate the peer table via
//! raft so any control-plane node can serve `vpn status`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{KeyPair, PublicKey, VpnMembership, VpnNetwork};
use crate::config;

/// Hub-side view: the network it owns plus the peer registry.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HubState {
    pub network: Option<VpnNetwork>,
    #[serde(default)]
    pub peers: Vec<RegisteredPeer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredPeer {
    pub node_name: String,
    pub membership: VpnMembership,
}

/// Spoke-side view: the network it joined and its own assigned membership.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpokeState {
    pub network: VpnNetwork,
    pub membership: VpnMembership,
}

impl HubState {
    pub fn load() -> Result<Self> {
        let path = config::default_vpn_state_file()?;
        load_or_default(&path)
    }

    pub fn save(&self) -> Result<()> {
        let path = config::default_vpn_state_file()?;
        config::ensure_parent_dir(&path)?;
        let payload = serde_json::to_vec_pretty(self).context("failed to serialize hub state")?;
        atomic_write(&path, &payload)?;
        Ok(())
    }

    pub fn upsert_peer(&mut self, node_name: String, membership: VpnMembership) {
        if let Some(existing) = self.peers.iter_mut().find(|p| p.node_name == node_name) {
            existing.membership = membership;
        } else {
            self.peers.push(RegisteredPeer {
                node_name,
                membership,
            });
        }
    }

    pub fn remove_peer(&mut self, node_name: &str) -> bool {
        let before = self.peers.len();
        self.peers.retain(|p| p.node_name != node_name);
        self.peers.len() != before
    }

    pub fn taken_ips(&self) -> Vec<std::net::Ipv4Addr> {
        self.peers.iter().map(|p| p.membership.ip).collect()
    }

    pub fn find_peer_by_pubkey(&self, pubkey: &PublicKey) -> Option<&RegisteredPeer> {
        self.peers
            .iter()
            .find(|p| &p.membership.pubkey == pubkey)
    }
}

impl SpokeState {
    pub fn load() -> Result<Option<Self>> {
        let path = config::default_vpn_spoke_config_file()?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Ok(Some(serde_json::from_slice(&raw).with_context(|| {
            format!("failed to parse spoke config at {}", path.display())
        })?))
    }

    pub fn save(&self) -> Result<()> {
        let path = config::default_vpn_spoke_config_file()?;
        config::ensure_parent_dir(&path)?;
        let payload = serde_json::to_vec_pretty(self).context("failed to serialize spoke state")?;
        atomic_write(&path, &payload)?;
        Ok(())
    }
}

/// Load the hub key pair from disk, or generate and persist a fresh one.
pub fn load_or_generate_keypair() -> Result<KeyPair> {
    let path = config::default_vpn_private_key_file()?;
    if path.exists() {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        KeyPair::from_secret_base64(raw.trim())
    } else {
        let pair = KeyPair::generate();
        config::ensure_parent_dir(&path)?;
        atomic_write(&path, pair.secret_base64().as_bytes())?;
        set_secret_permissions(&path)?;
        Ok(pair)
    }
}

fn load_or_default<T>(path: &Path) -> Result<T>
where
    T: Default + serde::de::DeserializeOwned,
{
    if !path.exists() {
        return Ok(T::default());
    }
    let raw = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_secret_permissions(path: &Path) -> Result<()> {
    let mut perms = fs::metadata(path)
        .with_context(|| format!("failed to read permissions for {}", path.display()))?
        .permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)
        .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_secret_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
