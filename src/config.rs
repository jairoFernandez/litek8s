use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;

const APP_DIR: &str = "litek8s";
const SOCKET_FILE: &str = "daemon.sock";
const STATE_FILE: &str = "state.json";

pub fn default_socket_path() -> PathBuf {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join(APP_DIR));
    base.join(APP_DIR).join(SOCKET_FILE)
}

pub fn default_state_file() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("dev", "civojairo", APP_DIR)
        .context("failed to resolve a data directory for the litek8s state file")?;
    Ok(dirs.data_local_dir().join(STATE_FILE))
}

pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent directory for {}", path.display()))?;
    }
    Ok(())
}
