use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;

const APP_DIR: &str = "litek8s";
const SOCKET_FILE: &str = "daemon.sock";
const STATE_FILE: &str = "state.json";
const VPN_STATE_FILE: &str = "vpn-state.json";
const VPN_PRIVATE_KEY_FILE: &str = "vpn-private.key";
const VPN_SPOKE_CONFIG_FILE: &str = "vpn-spoke.json";

pub fn default_socket_path() -> PathBuf {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join(APP_DIR));
    base.join(APP_DIR).join(SOCKET_FILE)
}

/// litek8s's data directory: `~/.local/share/litek8s` on Linux (or
/// `$XDG_DATA_HOME/litek8s`), `~/Library/Application Support/dev.litek8s` on
/// macOS. Installs from before the rename keep their macOS directory
/// (`dev.civojairo.litek8s`) while it exists and the new one does not: their
/// dev VM, disks and clusters live there, so it is never moved behind their
/// back (docs/dev.md#data-directory says how to move it).
pub fn data_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("dev", "", APP_DIR)
        .context("failed to resolve a data directory for litek8s")?;
    let dir = dirs.data_local_dir().to_path_buf();
    if cfg!(target_os = "macos") && !dir.exists() {
        if let Some(legacy) = dir
            .parent()
            .map(|p| p.join(LEGACY_MACOS_DIR))
            .filter(|p| p.is_dir())
        {
            return Ok(legacy);
        }
    }
    Ok(dir)
}

/// The macOS data directory's name before litek8s had a neutral bundle id.
const LEGACY_MACOS_DIR: &str = "dev.civojairo.litek8s";

pub fn default_state_file() -> Result<PathBuf> {
    Ok(data_dir()?.join(STATE_FILE))
}

pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent directory for {}", path.display()))?;
    }
    Ok(())
}

fn vpn_data_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("vpn"))
}

pub fn default_vpn_state_file() -> Result<PathBuf> {
    Ok(vpn_data_dir()?.join(VPN_STATE_FILE))
}

/// Directory where a node agent caches uploaded weight matrices on disk so a
/// distributed matmul ships each `right` matrix once per node instead of on
/// every band call. Survives agent restarts.
pub fn weights_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("weights"))
}

pub fn default_vpn_private_key_file() -> Result<PathBuf> {
    Ok(vpn_data_dir()?.join(VPN_PRIVATE_KEY_FILE))
}

pub fn default_vpn_spoke_config_file() -> Result<PathBuf> {
    Ok(vpn_data_dir()?.join(VPN_SPOKE_CONFIG_FILE))
}
