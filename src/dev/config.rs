//! `~/.litek8s/config.yaml`: the dev cluster's settings, what `litek8s dev
//! up` uses when no flag says otherwise (and what `litek8s ui` edits).
//!
//! ```yaml
//! cpus: 4              # macOS VM CPUs (default: half the Mac's, at most 8)
//! memory: 4096         # macOS VM memory ceiling in MiB
//! shares:              # macOS: Mac directories shared at the same path
//!   - /Users/me        # (default: $HOME)
//! port: 6443           # API server port on localhost
//! publish: localhost   # where published ports and NodePorts listen: localhost | all
//! ```
//!
//! The file is read when the cluster starts: changing it while the cluster
//! runs takes effect on the next `dev up`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Where published container ports and NodePorts are reachable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Publish {
    /// 127.0.0.1 (and ::1 on macOS): this machine only, like Docker Desktop.
    #[default]
    Localhost,
    /// Every interface: other machines on the network reach them too.
    All,
}

impl Publish {
    pub fn as_str(self) -> &'static str {
        match self {
            Publish::Localhost => "localhost",
            Publish::All => "all",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "localhost" | "loopback" | "127.0.0.1" => Ok(Publish::Localhost),
            "all" | "lan" | "0.0.0.0" => Ok(Publish::All),
            other => bail!("publish must be `localhost` or `all`, not {other:?}"),
        }
    }

    pub fn ipv4(self) -> std::net::Ipv4Addr {
        match self {
            Publish::Localhost => std::net::Ipv4Addr::LOCALHOST,
            Publish::All => std::net::Ipv4Addr::UNSPECIFIED,
        }
    }

    pub fn ipv6(self) -> std::net::Ipv6Addr {
        match self {
            Publish::Localhost => std::net::Ipv6Addr::LOCALHOST,
            Publish::All => std::net::Ipv6Addr::UNSPECIFIED,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DevConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<usize>,
    /// MiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shares: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "is_default_publish")]
    pub publish: Publish,
}

fn is_default_publish(p: &Publish) -> bool {
    *p == Publish::Localhost
}

/// `$LITEK8S_CONFIG`, else `~/.litek8s/config.yaml` (next to docker.sock;
/// `$LITEK8S_HOME/config.yaml`).
pub fn path() -> PathBuf {
    crate::config::paths().dev_config
}

const HEADER: &str = "# litek8s dev settings, read by `litek8s dev up` (its flags win).\n\
# cpus, memory (MiB) and shares are for the macOS VM; publish: localhost | all.\n\
# Changes apply on the next `litek8s dev up`.\n";

impl DevConfig {
    /// The settings in `path`; defaults when the file does not exist.
    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("{} is not valid", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&path())
    }

    pub fn parse(text: &str) -> Result<Self> {
        if text.trim().is_empty() || text.lines().all(|l| l.trim().is_empty() || l.trim_start().starts_with('#')) {
            return Ok(Self::default());
        }
        let config: Self = serde_yaml::from_str(text)?;
        let problems = config.problems();
        if !problems.is_empty() {
            bail!("{}", problems.join("; "));
        }
        Ok(config)
    }

    pub fn to_yaml(&self) -> String {
        let body = serde_yaml::to_string(self).unwrap_or_default();
        let body = if body.trim() == "{}" { String::new() } else { body };
        format!("{HEADER}{body}")
    }

    /// Validate, then write atomically.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let problems = self.problems();
        if !problems.is_empty() {
            bail!("{}", problems.join("; "));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("yaml.{}.tmp", std::process::id()));
        std::fs::write(&tmp, self.to_yaml()).with_context(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))
    }

    /// What is wrong with these settings on this machine (empty: valid).
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        if let Some(c) = self.cpus {
            if c == 0 || c > ncpu {
                out.push(format!("cpus must be between 1 and {ncpu} (this machine's CPUs), not {c}"));
            }
        }
        if let Some(m) = self.memory {
            let total = host_memory_mib();
            if m < 1024 {
                out.push(format!("memory must be at least 1024 MiB, not {m}"));
            } else if total > 0 && m > total {
                out.push(format!("memory must be at most {total} MiB (this machine's RAM), not {m}"));
            }
        }
        if let Some(p) = self.port {
            if p < 1024 {
                out.push(format!("port must be 1024 or above (ports below need root), not {p}"));
            }
        }
        for (i, s) in self.shares.iter().enumerate() {
            let shown = s.display();
            if !s.is_absolute() {
                out.push(format!("share {shown} must be an absolute path"));
                continue;
            }
            // The kernel command line carries the list: `tag:path,tag:path`.
            if s.to_string_lossy().chars().any(|c| c.is_whitespace() || c == ',' || c == ':') {
                out.push(format!("share {shown} cannot contain spaces, commas or colons"));
            }
            if !s.is_dir() {
                out.push(format!("share {shown} is not a directory"));
            }
            // macOS: the VM always shares these (Docker Desktop does too).
            if cfg!(target_os = "macos") && ["/tmp", "/private/tmp", "/var/folders", "/private/var/folders"].iter().any(|d| s.starts_with(d)) {
                out.push(format!("share {shown} is shared already (/tmp and /var/folders always are)"));
            }
            for other in &self.shares[..i] {
                if s.starts_with(other) || other.starts_with(s) {
                    out.push(format!("shares {} and {shown} overlap", other.display()));
                }
            }
        }
        if self.shares.len() > 8 {
            out.push("at most 8 shared directories".into());
        }
        out
    }

    /// The directories the VM shares: the configured ones, else $HOME.
    pub fn effective_shares(&self) -> Vec<PathBuf> {
        if !self.shares.is_empty() {
            return self.shares.clone();
        }
        std::env::var_os("HOME").map(PathBuf::from).into_iter().collect()
    }
}

/// Defaults the VM uses for what the settings leave out.
pub fn default_cpus() -> usize {
    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    (ncpu / 2).clamp(1, 8)
}

pub const DEFAULT_MEMORY_MIB: u64 = 4096;

/// This machine's RAM in MiB (0 when unknown).
pub fn host_memory_mib() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| m.lines().find_map(|l| l.strip_prefix("MemTotal:")?.trim().trim_end_matches("kB").trim().parse::<u64>().ok()))
            .map(|kb| kb / 1024)
            .unwrap_or(0)
    }
    #[cfg(target_os = "macos")]
    {
        let mut size: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: sysctlbyname writes at most `len` bytes into `size`.
        let rc = unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&mut size as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) };
        if rc == 0 { size >> 20 } else { 0 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_validates() {
        // Not under /var/folders: the macOS VM shares that already, and a
        // share there is refused.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&base).unwrap();
        let dir = tempfile::tempdir_in(&base).unwrap();
        let share = dir.path().join("src");
        std::fs::create_dir(&share).unwrap();
        let c = DevConfig { cpus: Some(1), memory: Some(2048), shares: vec![share.clone()], port: Some(7443), publish: Publish::All };
        let path = dir.path().join("config.yaml");
        c.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# litek8s dev settings"), "{text}");
        assert!(text.contains("publish: all"), "{text}");
        assert_eq!(DevConfig::load_from(&path).unwrap(), c);

        assert_eq!(DevConfig::load_from(&dir.path().join("missing.yaml")).unwrap(), DevConfig::default());
        assert_eq!(DevConfig::parse("# only comments\n").unwrap(), DevConfig::default());
        // The defaults write nothing but the header.
        assert!(DevConfig::default().to_yaml().lines().all(|l| l.starts_with('#')));

        let err = DevConfig::parse("cpus: 0\n").unwrap_err().to_string();
        assert!(err.contains("cpus must be between 1"), "{err}");
        assert!(DevConfig::parse("memory: 12\n").unwrap_err().to_string().contains("at least 1024"));
        assert!(DevConfig::parse("port: 80\n").unwrap_err().to_string().contains("1024 or above"));
        assert!(DevConfig::parse("publish: everywhere\n").is_err());
        assert!(DevConfig::parse("cpu: 2\n").unwrap_err().to_string().contains("unknown field"));
        assert!(DevConfig::parse("shares: [relative/dir]\n").unwrap_err().to_string().contains("absolute"));
        let nested = format!("shares: [{}, {}]\n", dir.path().display(), share.display());
        assert!(DevConfig::parse(&nested).unwrap_err().to_string().contains("overlap"));
        if cfg!(target_os = "macos") {
            let tmp = tempfile::tempdir().unwrap();
            let shared = format!("shares: [{}]\n", tmp.path().canonicalize().unwrap().display());
            assert!(DevConfig::parse(&shared).unwrap_err().to_string().contains("shared already"));
        }
        let invalid = DevConfig { cpus: Some(0), ..Default::default() };
        assert!(invalid.save_to(&path).is_err());
        // A failed save leaves the file as it was.
        assert_eq!(DevConfig::load_from(&path).unwrap(), c);
    }

    #[test]
    fn publish_names() {
        assert_eq!(Publish::parse("all").unwrap(), Publish::All);
        assert_eq!(Publish::parse(" localhost ").unwrap(), Publish::Localhost);
        assert_eq!(Publish::All.ipv4(), std::net::Ipv4Addr::UNSPECIFIED);
        assert!(Publish::parse("x").is_err());
    }
}
