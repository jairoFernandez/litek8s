//! VPN membership types persisted in the cluster store.

use std::net::Ipv4Addr;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use super::keys::PublicKey;

/// Role of a node in the hub-and-spoke topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VpnRole {
    Hub,
    Spoke,
}

/// Liveness signal derived from the latest handshake age.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VpnStatus {
    Up,
    Degraded,
    Down,
}

/// VPN membership facts about a node — replicated via raft, never includes
/// the private key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VpnMembership {
    pub role: VpnRole,
    pub ip: Ipv4Addr,
    pub pubkey: PublicKey,
    /// `host:port` reachable by spokes. Only set for the hub.
    pub endpoint: Option<String>,
    pub last_handshake_unix: Option<u64>,
    pub status: VpnStatus,
}

impl VpnMembership {
    pub fn new_hub(ip: Ipv4Addr, pubkey: PublicKey, endpoint: String) -> Self {
        Self {
            role: VpnRole::Hub,
            ip,
            pubkey,
            endpoint: Some(endpoint),
            last_handshake_unix: None,
            status: VpnStatus::Down,
        }
    }

    pub fn new_spoke(ip: Ipv4Addr, pubkey: PublicKey) -> Self {
        Self {
            role: VpnRole::Spoke,
            ip,
            pubkey,
            endpoint: None,
            last_handshake_unix: None,
            status: VpnStatus::Down,
        }
    }
}

/// Cluster-wide VPN configuration. Owned by the hub, replicated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VpnNetwork {
    /// IPv4 CIDR like `10.42.0.0/24`. Stored as base + prefix len.
    pub cidr_base: Ipv4Addr,
    pub cidr_prefix: u8,
    pub hub_endpoint: String,
    pub hub_pubkey: PublicKey,
}

impl VpnNetwork {
    pub fn new(
        cidr_base: Ipv4Addr,
        cidr_prefix: u8,
        hub_endpoint: String,
        hub_pubkey: PublicKey,
    ) -> Result<Self> {
        ensure!(
            cidr_prefix <= 30,
            "VPN CIDR prefix must be /30 or shorter to leave room for peers"
        );
        ensure!(
            !hub_endpoint.trim().is_empty(),
            "hub endpoint must not be empty"
        );
        validate_endpoint(&hub_endpoint)?;
        Ok(Self {
            cidr_base,
            cidr_prefix,
            hub_endpoint,
            hub_pubkey,
        })
    }

    pub fn hub_ip(&self) -> Ipv4Addr {
        ip_offset(self.cidr_base, 1)
    }

    /// Pick the next available IP `cidr_base + n` skipping `.0` (network),
    /// `.1` (hub), and any already-taken IPs. Returns `None` when the range
    /// is exhausted.
    pub fn next_spoke_ip(&self, taken: &[Ipv4Addr]) -> Option<Ipv4Addr> {
        let host_bits = 32u32 - u32::from(self.cidr_prefix);
        let max_hosts = if host_bits >= 32 {
            u32::MAX
        } else {
            (1u32 << host_bits) - 1
        };
        for offset in 2..max_hosts {
            let candidate = ip_offset(self.cidr_base, offset);
            if !taken.contains(&candidate) {
                return Some(candidate);
            }
        }
        None
    }
}

fn ip_offset(base: Ipv4Addr, offset: u32) -> Ipv4Addr {
    let base_u32 = u32::from(base);
    Ipv4Addr::from(base_u32.wrapping_add(offset))
}

fn validate_endpoint(endpoint: &str) -> Result<()> {
    let (host, port) = endpoint.rsplit_once(':').ok_or_else(|| {
        anyhow::anyhow!("hub endpoint `{endpoint}` must be `host:port`")
    })?;
    if host.trim().is_empty() {
        bail!("hub endpoint `{endpoint}` has empty host");
    }
    port.parse::<u16>()
        .map_err(|_| anyhow::anyhow!("hub endpoint `{endpoint}` has invalid port"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vpn::keys::KeyPair;

    fn sample_pubkey() -> PublicKey {
        KeyPair::generate().public()
    }

    #[test]
    fn hub_ip_is_first_usable_address() {
        let network = VpnNetwork::new(
            Ipv4Addr::new(10, 42, 0, 0),
            24,
            "54.1.2.3:51820".to_string(),
            sample_pubkey(),
        )
        .expect("network");
        assert_eq!(network.hub_ip(), Ipv4Addr::new(10, 42, 0, 1));
    }

    #[test]
    fn assigns_sequential_spoke_ips() {
        let network = VpnNetwork::new(
            Ipv4Addr::new(10, 42, 0, 0),
            24,
            "54.1.2.3:51820".to_string(),
            sample_pubkey(),
        )
        .expect("network");

        let first = network.next_spoke_ip(&[]).expect("first spoke");
        assert_eq!(first, Ipv4Addr::new(10, 42, 0, 2));

        let second = network.next_spoke_ip(&[first]).expect("second spoke");
        assert_eq!(second, Ipv4Addr::new(10, 42, 0, 3));

        let third = network
            .next_spoke_ip(&[first, second])
            .expect("third spoke");
        assert_eq!(third, Ipv4Addr::new(10, 42, 0, 4));
    }

    #[test]
    fn skips_taken_spoke_ips() {
        let network = VpnNetwork::new(
            Ipv4Addr::new(10, 42, 0, 0),
            24,
            "54.1.2.3:51820".to_string(),
            sample_pubkey(),
        )
        .expect("network");

        let taken = vec![
            Ipv4Addr::new(10, 42, 0, 2),
            Ipv4Addr::new(10, 42, 0, 3),
            Ipv4Addr::new(10, 42, 0, 5),
        ];
        let next = network.next_spoke_ip(&taken).expect("next");
        assert_eq!(next, Ipv4Addr::new(10, 42, 0, 4));
    }

    #[test]
    fn rejects_too_narrow_cidr() {
        assert!(
            VpnNetwork::new(
                Ipv4Addr::new(10, 42, 0, 0),
                31,
                "54.1.2.3:51820".to_string(),
                sample_pubkey(),
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_invalid_endpoint() {
        assert!(
            VpnNetwork::new(
                Ipv4Addr::new(10, 42, 0, 0),
                24,
                "missing-port".to_string(),
                sample_pubkey(),
            )
            .is_err()
        );
        assert!(
            VpnNetwork::new(
                Ipv4Addr::new(10, 42, 0, 0),
                24,
                ":51820".to_string(),
                sample_pubkey(),
            )
            .is_err()
        );
    }
}
