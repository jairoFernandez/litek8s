//! Tunnel lifecycle.
//!
//! Phase 1 is a stub: the tunnel is not actually brought up. We only emit a
//! `wg-quick`-compatible config so the operator can validate the parameters
//! (and bring it up manually with the standard `wg` tooling) until the
//! boringtun integration lands.
//!
//! Phase 2 will replace `TunnelHandle::start` with a real boringtun device:
//!   - create `utun` (macOS) / `/dev/net/tun` (Linux) interface `lk-wg0`
//!   - open UDP socket
//!   - pump packets between TUN and `boringtun::noise::Tunn`
//!   - update `VpnStatus` based on handshake events

use std::path::PathBuf;

use anyhow::{Context, Result};

use super::{HubState, RegisteredPeer, SpokeState};

/// Opaque handle to a (stubbed) running tunnel. Dropping it currently does
/// nothing because no tunnel is started; the type exists to lock down the
/// future API surface.
pub struct TunnelHandle {
    _private: (),
}

impl TunnelHandle {
    /// Pretends to bring the tunnel up. Always succeeds today; logs the fact
    /// to stderr so operators see it during demos.
    pub fn start_stub(label: &str) -> Self {
        eprintln!(
            "[vpn] tunnel `{label}` not started: real WireGuard integration pending (see docs/specs/03-vpn-pack.md)"
        );
        Self { _private: () }
    }
}

/// Build a `wg-quick`-compatible config file from current spoke state.
///
/// Writing the config file allows an operator to bring the tunnel up manually
/// with `wg-quick up <path>` while the in-process tunnel is still a stub.
pub fn write_wg_quick_config(
    output: &std::path::Path,
    spoke: &SpokeState,
    spoke_secret_base64: &str,
) -> Result<PathBuf> {
    let mut config = String::new();
    config.push_str("[Interface]\n");
    config.push_str(&format!("PrivateKey = {spoke_secret_base64}\n"));
    config.push_str(&format!(
        "Address = {}/{}\n",
        spoke.membership.ip, spoke.network.cidr_prefix
    ));
    config.push_str("ListenPort = 0\n\n");
    config.push_str("[Peer]\n");
    config.push_str(&format!(
        "PublicKey = {}\n",
        spoke.network.hub_pubkey.as_str()
    ));
    config.push_str(&format!("Endpoint = {}\n", spoke.network.hub_endpoint));
    let cidr = format!("{}/{}", spoke.network.cidr_base, spoke.network.cidr_prefix);
    config.push_str(&format!("AllowedIPs = {cidr}\n"));
    config.push_str("PersistentKeepalive = 25\n");

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(output, config)
        .with_context(|| format!("failed to write {}", output.display()))?;
    Ok(output.to_path_buf())
}

/// Build a `wg-quick`-compatible config file from current hub state.
///
/// The hub config lists every registered spoke as a `[Peer]` entry. Spokes are
/// not given `Endpoint` lines (they're behind NAT, the hub waits for them).
pub fn write_wg_quick_hub_config(
    output: &std::path::Path,
    hub: &HubState,
    hub_secret_base64: &str,
    hub_listen_port: u16,
) -> Result<PathBuf> {
    let network = hub
        .network
        .as_ref()
        .context("hub state has no network — run `litek8s vpn init` first")?;

    let mut config = String::new();
    config.push_str("[Interface]\n");
    config.push_str(&format!("PrivateKey = {hub_secret_base64}\n"));
    config.push_str(&format!(
        "Address = {}/{}\n",
        network.hub_ip(),
        network.cidr_prefix
    ));
    config.push_str(&format!("ListenPort = {hub_listen_port}\n"));

    for RegisteredPeer { membership, .. } in &hub.peers {
        if membership.pubkey == network.hub_pubkey {
            continue;
        }
        config.push_str("\n[Peer]\n");
        config.push_str(&format!("PublicKey = {}\n", membership.pubkey.as_str()));
        config.push_str(&format!("AllowedIPs = {}/32\n", membership.ip));
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(output, config)
        .with_context(|| format!("failed to write {}", output.display()))?;
    Ok(output.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vpn::{KeyPair, VpnMembership, VpnNetwork};
    use std::net::Ipv4Addr;

    #[test]
    fn writes_spoke_wg_quick_config() {
        let hub_pair = KeyPair::generate();
        let spoke_pair = KeyPair::generate();
        let network = VpnNetwork::new(
            Ipv4Addr::new(10, 42, 0, 0),
            24,
            "54.1.2.3:51820".to_string(),
            hub_pair.public(),
        )
        .expect("network");
        let membership = VpnMembership::new_spoke(Ipv4Addr::new(10, 42, 0, 2), spoke_pair.public());
        let spoke = SpokeState {
            network,
            membership,
        };

        let tmp = tempfile::NamedTempFile::new().expect("tmp");
        let path = write_wg_quick_config(tmp.path(), &spoke, &spoke_pair.secret_base64())
            .expect("write");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("[Interface]"));
        assert!(body.contains("[Peer]"));
        assert!(body.contains("10.42.0.2/24"));
        assert!(body.contains("54.1.2.3:51820"));
        assert!(body.contains("PersistentKeepalive = 25"));
    }
}
