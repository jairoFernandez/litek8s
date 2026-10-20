//! Named local clusters: where each one keeps its files, and its names
//! outside (kubectl context, Docker socket, node name).
//!
//! The default cluster, `dev`, keeps the paths litek8s has always used:
//! the dev data directory itself (`dev.json`, `dev.log`, `cluster/` on
//! Linux, the VM's disk in `vm-state/` on macOS), context `litek8s-dev`,
//! `~/.litek8s/docker.sock`. Every other cluster lives in
//! `<dev data>/clusters/<name>/`, with context `litek8s-<name>` and socket
//! `~/.litek8s/<name>/docker.sock`. Inside the macOS VM a cluster's data is
//! `/var/lib/litek8s/<name>` (the default's was always `/var/lib/litek8s/dev`).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const DEFAULT: &str = "dev";

/// What `dev up` records about a cluster when it creates it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// Nodes: the control-plane node and `nodes - 1` workers.
    #[serde(default = "one")]
    pub nodes: u32,
    #[serde(default)]
    pub created_unix_ms: u64,
    /// The API port it had last (kept across restarts when free).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

fn one() -> u32 {
    1
}

/// A cluster name: what kind and k3d accept, short enough for node names.
pub fn validate(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 20
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!("cluster name {name:?}: 1-20 characters, lowercase letters, digits and '-', starting with a letter");
    }
    // Directories next to the clusters' sockets in ~/.litek8s (or
    // $LITEK8S_HOME).
    if ["stable", "clusters", "run", "vm", "data"].contains(&name) {
        bail!("cluster name {name:?} is reserved");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cluster {
    pub name: String,
}

impl Cluster {
    pub fn new(name: &str) -> Result<Self> {
        validate(name)?;
        Ok(Cluster { name: name.to_string() })
    }

    pub fn default_cluster() -> Self {
        Cluster { name: DEFAULT.into() }
    }

    pub fn is_default(&self) -> bool {
        self.name == DEFAULT
    }

    /// Where its host-side files are.
    pub fn home(&self, dev_home: &Path) -> PathBuf {
        if self.is_default() { dev_home.to_path_buf() } else { dev_home.join("clusters").join(&self.name) }
    }

    /// Its Kubernetes objects, images and volumes on a Linux host.
    pub fn data_dir(&self, dev_home: &Path) -> PathBuf {
        if self.is_default() { dev_home.join("cluster") } else { self.home(dev_home).join("data") }
    }

    /// The same inside the macOS VM.
    pub fn guest_data_dir(&self) -> PathBuf {
        Path::new(super::guest::DATA_DIR).join(&self.name)
    }

    /// Its kubeconfig context: `litek8s-<name>`, `lkdev-<name>` with
    /// `$LITEK8S_HOME` (see [`crate::config::context_prefix`]).
    pub fn context(&self) -> String {
        format!("{}{}", crate::config::context_prefix(), self.name)
    }

    /// The control-plane node's name (and the cell's hostname).
    pub fn node_name(&self) -> String {
        format!("litek8s-{}", self.name)
    }

    /// The name of node `i` (0: the control plane), as kind names them.
    pub fn node_name_at(&self, i: u32) -> String {
        match i {
            0 => self.node_name(),
            1 => format!("{}-worker", self.node_name()),
            n => format!("{}-worker{n}", self.node_name()),
        }
    }

    /// Where its NodePorts start in 30000-32767: the default cluster at
    /// 30000 as always, the others spread out by name, so two clusters
    /// rarely want the same port on the host's localhost.
    pub fn node_port_offset(&self) -> u16 {
        let prefix = crate::config::context_prefix();
        // Another litek8s on the machine ($LITEK8S_HOME) spreads its default
        // cluster out too: both VMs forward their NodePorts to localhost.
        let key = match (self.is_default(), prefix == "litek8s-") {
            (true, true) => return 0,
            (true, false) => format!("{prefix}{}", self.name),
            (false, _) => self.name.clone(),
        };
        // FNV-1a: stable across builds and platforms.
        let h = key.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193));
        // Steps of 100 from 30100: easy to read, 27 starting points.
        100 + (h % 27) as u16 * 100
    }

    pub fn docker_socket(&self) -> PathBuf {
        self.docker_socket_from(&crate::docker::host_socket_path())
    }

    /// Its socket when the default cluster's is `default`: that one, or
    /// `<name>/docker.sock` next to it.
    pub fn docker_socket_from(&self, default: &Path) -> PathBuf {
        if self.is_default() {
            return default.to_path_buf();
        }
        default.parent().unwrap_or(Path::new("/tmp")).join(&self.name).join("docker.sock")
    }

    /// The docker CLI context that points at its socket: `litek8s` (`lkdev`
    /// with `$LITEK8S_HOME`) for the default cluster, else its kubeconfig
    /// context's name.
    pub fn docker_context(&self) -> String {
        if self.is_default() { crate::config::context_prefix().trim_end_matches('-').to_string() } else { self.context() }
    }

    pub fn meta(&self, dev_home: &Path) -> Option<Meta> {
        serde_json::from_slice(&std::fs::read(self.home(dev_home).join("cluster.json")).ok()?).ok()
    }

    pub fn save_meta(&self, dev_home: &Path, meta: &Meta) -> Result<()> {
        let home = self.home(dev_home);
        std::fs::create_dir_all(&home)?;
        let path = home.join("cluster.json");
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(meta)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Whether it was ever created (a cluster from before named clusters
    /// has a log but no `cluster.json`).
    pub fn exists(&self, dev_home: &Path) -> bool {
        let home = self.home(dev_home);
        home.join("cluster.json").exists() || home.join("dev.json").exists() || (self.is_default() && home.join("dev.log").exists())
    }
}

/// The clusters on this machine, the default first.
pub fn list(dev_home: &Path) -> Vec<Cluster> {
    let mut out = Vec::new();
    if Cluster::default_cluster().exists(dev_home) {
        out.push(Cluster::default_cluster());
    }
    let mut named: Vec<Cluster> = std::fs::read_dir(dev_home.join("clusters"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n != DEFAULT && validate(n).is_ok())
        .map(|name| Cluster { name })
        .filter(|c| c.exists(dev_home))
        .collect();
    named.sort();
    out.extend(named);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        let home = Path::new("/d/dev");
        let dev = Cluster::default_cluster();
        assert_eq!((dev.home(home), dev.data_dir(home)), (PathBuf::from("/d/dev"), PathBuf::from("/d/dev/cluster")));
        assert_eq!((dev.context(), dev.node_name(), dev.docker_context()), ("litek8s-dev".into(), "litek8s-dev".into(), "litek8s".into()));
        assert_eq!(dev.guest_data_dir(), PathBuf::from("/var/lib/litek8s/dev"));
        let ci = Cluster::new("ci").unwrap();
        assert_eq!(ci.home(home), PathBuf::from("/d/dev/clusters/ci"));
        assert_eq!(ci.data_dir(home), PathBuf::from("/d/dev/clusters/ci/data"));
        assert_eq!(ci.guest_data_dir(), PathBuf::from("/var/lib/litek8s/ci"));
        assert_eq!(ci.docker_context(), "litek8s-ci");
        assert!(ci.docker_socket().ends_with("ci/docker.sock"));
        assert_eq!([ci.node_name_at(0), ci.node_name_at(1), ci.node_name_at(2)], ["litek8s-ci", "litek8s-ci-worker", "litek8s-ci-worker2"]);
        assert_eq!(dev.node_port_offset(), 0);
        assert!((100..2768).contains(&ci.node_port_offset()) && ci.node_port_offset() % 100 == 0);
        for bad in ["", "Dev", "1a", "a_b", "a-", "stable", "abcdefghijklmnopqrstu"] {
            assert!(validate(bad).is_err(), "{bad}");
        }
        assert!(validate("my-cluster-2").is_ok());
    }

    #[test]
    fn lists_what_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(dir.path()).is_empty());
        let b = Cluster::new("b").unwrap();
        b.save_meta(dir.path(), &Meta { nodes: 2, created_unix_ms: 1, port: None }).unwrap();
        Cluster::new("a").unwrap().save_meta(dir.path(), &Meta::default()).unwrap();
        std::fs::create_dir_all(dir.path().join("clusters/stray")).unwrap();
        assert_eq!(list(dir.path()).iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        // A default cluster from before named clusters: only its log.
        std::fs::write(dir.path().join("dev.log"), "").unwrap();
        assert_eq!(list(dir.path())[0].name, "dev");
        assert_eq!(b.meta(dir.path()).unwrap().nodes, 2);
    }
}
