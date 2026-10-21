use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;

const APP_DIR: &str = "litek8s";
const SOCKET_FILE: &str = "daemon.sock";
const STATE_FILE: &str = "state.json";
const VPN_STATE_FILE: &str = "vpn-state.json";
const VPN_PRIVATE_KEY_FILE: &str = "vpn-private.key";
const VPN_SPOKE_CONFIG_FILE: &str = "vpn-spoke.json";

/// `$LITEK8S_HOME`: one directory for everything litek8s keeps per user, so
/// a second litek8s on the machine (a build from a checkout next to an
/// installed release) shares nothing with the first. Unset: the usual places.
pub const HOME_VAR: &str = "LITEK8S_HOME";

/// The kubeconfig context prefix of local clusters with `$LITEK8S_HOME` set
/// (default `lkdev-`; without `$LITEK8S_HOME` it is always `litek8s-`).
pub const CONTEXT_PREFIX_VAR: &str = "LITEK8S_CONTEXT_PREFIX";
const DEFAULT_PREFIX: &str = "litek8s-";
const HOME_PREFIX: &str = "lkdev-";

/// Every per-user path litek8s uses. With `$LITEK8S_HOME` all of them are
/// under it:
///
/// ```text
/// $LITEK8S_HOME/docker.sock, <cluster>/docker.sock  (else ~/.litek8s/...)
/// $LITEK8S_HOME/config.yaml, ui.yaml, ui-state.json (else ~/.litek8s/...)
/// $LITEK8S_HOME/kubeconfig                          (else ~/.kube/config)
/// $LITEK8S_HOME/data/{dev,vm,vpn,node-agent,...}    (else the data dir)
/// $LITEK8S_HOME/run/{daemon,agent}.sock             (else $XDG_RUNTIME_DIR/litek8s)
/// ```
///
/// The variables for a single path (`LITEK8S_DEV_HOME`,
/// `LITEK8S_DOCKER_SOCKET`, `LITEK8S_CONFIG`, `KUBECONFIG`) still win.
#[derive(Debug, Clone, PartialEq)]
pub struct Paths {
    /// `$LITEK8S_HOME`, made absolute.
    pub home: Option<PathBuf>,
    /// `~/.litek8s`: the Docker sockets, config.yaml and the UI's files.
    pub user_dir: PathBuf,
    /// None only without `$LITEK8S_HOME` and without a home directory.
    pub data_dir: Option<PathBuf>,
    /// `litek8s dev`'s data (`<data dir>/dev`).
    pub dev_home: Option<PathBuf>,
    /// The default dev cluster's Docker API.
    pub docker_socket: PathBuf,
    /// `litek8s dev`'s settings.
    pub dev_config: PathBuf,
    /// The kubeconfig `dev up` adds its contexts to.
    pub kubeconfig: PathBuf,
    /// The legacy daemon's socket.
    pub daemon_socket: PathBuf,
    /// The legacy persistent node agent's socket.
    pub agent_socket: PathBuf,
    /// `litek8s node-agent`'s default `--node-dir`.
    pub node_agent_dir: PathBuf,
    /// What local clusters' kubeconfig contexts start with.
    pub context_prefix: String,
}

impl Paths {
    pub fn from_env() -> Self {
        Self::resolve(|k| env::var_os(k).filter(|v| !v.is_empty()))
    }

    /// The paths in the environment `var` reads (tests pass their own).
    pub fn resolve(var: impl Fn(&str) -> Option<OsString>) -> Self {
        let os_home = var("HOME").map(PathBuf::from);
        let home = var(HOME_VAR).map(PathBuf::from).map(|h| match h.is_absolute() {
            true => h,
            false => env::current_dir().map(|d| d.join(&h)).unwrap_or(h),
        });
        let user_dir = match &home {
            Some(h) => h.clone(),
            None => os_home.clone().unwrap_or_else(|| PathBuf::from("/tmp")).join(".litek8s"),
        };
        let data_dir = match &home {
            Some(h) => Some(h.join("data")),
            None => platform_data_dir(),
        };
        let run_dir = match &home {
            Some(h) => h.join("run"),
            None => var("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(env::temp_dir).join(APP_DIR),
        };
        let kubeconfig = var("KUBECONFIG")
            .and_then(|v| env::split_paths(&v).next())
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| match &home {
                Some(h) => h.join("kubeconfig"),
                None => os_home.clone().unwrap_or_else(|| PathBuf::from(".")).join(".kube").join("config"),
            });
        let node_agent_dir = match &home {
            Some(h) => h.join("data").join("node-agent"),
            None => ProjectDirs::from("dev", "litek8s", "litek8s")
                .map(|d| d.data_dir().join("node-agent"))
                .unwrap_or_else(|| env::temp_dir().join("litek8s-node-agent")),
        };
        let context_prefix = match &home {
            Some(_) => var(CONTEXT_PREFIX_VAR).and_then(|p| p.into_string().ok()).unwrap_or_else(|| HOME_PREFIX.into()),
            None => DEFAULT_PREFIX.into(),
        };
        Paths {
            dev_home: var("LITEK8S_DEV_HOME").map(PathBuf::from).or_else(|| data_dir.as_ref().map(|d| d.join("dev"))),
            docker_socket: var("LITEK8S_DOCKER_SOCKET").map(PathBuf::from).unwrap_or_else(|| user_dir.join("docker.sock")),
            dev_config: var("LITEK8S_CONFIG").map(PathBuf::from).unwrap_or_else(|| user_dir.join("config.yaml")),
            // The legacy daemon has always used $XDG_RUNTIME_DIR/litek8s/
            // (and <tmp>/litek8s/litek8s/ without it).
            daemon_socket: match &home {
                Some(_) => run_dir.join(SOCKET_FILE),
                None => var("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| env::temp_dir().join(APP_DIR)).join(APP_DIR).join(SOCKET_FILE),
            },
            agent_socket: run_dir.join("agent.sock"),
            home,
            user_dir,
            data_dir,
            kubeconfig,
            node_agent_dir,
            context_prefix,
        }
    }
}

pub fn paths() -> Paths {
    Paths::from_env()
}

/// `$LITEK8S_HOME` when it is set.
pub fn litek8s_home() -> Option<PathBuf> {
    paths().home
}

/// What the kubeconfig contexts of local clusters start with: `litek8s-`,
/// or with `$LITEK8S_HOME` set `$LITEK8S_CONTEXT_PREFIX` (default `lkdev-`).
pub fn context_prefix() -> String {
    paths().context_prefix
}

/// `0.1.0` for a release build, `0.1.0 DEV <commit>` for any other
/// (build.rs).
pub const VERSION: &str = env!("LITEK8S_VERSION");

/// What `--version` prints after the name: [`VERSION`] and the
/// `$LITEK8S_HOME` in use.
pub fn version_line() -> String {
    match litek8s_home() {
        Some(h) => format!("{VERSION} (LITEK8S_HOME={})", h.display()),
        None => VERSION.to_string(),
    }
}

pub fn default_socket_path() -> PathBuf {
    paths().daemon_socket
}

/// litek8s's data directory: `$LITEK8S_HOME/data`; else
/// `~/.local/share/litek8s` on Linux (or `$XDG_DATA_HOME/litek8s`),
/// `~/Library/Application Support/dev.litek8s` on macOS. Installs from
/// before the rename keep their macOS directory (`dev.civojairo.litek8s`)
/// while it exists and the new one does not: their dev VM, disks and
/// clusters live there, so it is never moved behind their back
/// (docs/dev.md#data-directory says how to move it).
pub fn data_dir() -> Result<PathBuf> {
    paths().data_dir.context("failed to resolve a data directory for litek8s")
}

fn platform_data_dir() -> Option<PathBuf> {
    let dir = ProjectDirs::from("dev", "", APP_DIR)?.data_local_dir().to_path_buf();
    if cfg!(target_os = "macos")
        && !dir.exists()
        && let Some(legacy) = dir.parent().map(|p| p.join(LEGACY_MACOS_DIR)).filter(|p| p.is_dir())
    {
        return Some(legacy);
    }
    Some(dir)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v))
    }

    #[test]
    fn litek8s_home_holds_every_path() {
        let home = "/tmp/lk-home-test";
        let vars = [("LITEK8S_HOME", home), ("HOME", "/Users/me"), ("XDG_RUNTIME_DIR", "/run/user/501"), ("XDG_DATA_HOME", "/Users/me/.local/share")];
        let p = Paths::resolve(env_of(&vars));
        assert_eq!(p.home.as_deref(), Some(Path::new(home)));
        let dev_home = p.dev_home.clone().unwrap();
        let ci = crate::dev::cluster::Cluster { name: "ci".into() };
        let all = [
            ("user_dir", p.user_dir.clone()),
            ("data_dir", p.data_dir.clone().unwrap()),
            ("dev_home", dev_home.clone()),
            ("docker_socket", p.docker_socket.clone()),
            ("dev_config", p.dev_config.clone()),
            ("kubeconfig", p.kubeconfig.clone()),
            ("daemon_socket", p.daemon_socket.clone()),
            ("agent_socket", p.agent_socket.clone()),
            ("node_agent_dir", p.node_agent_dir.clone()),
            ("state file", p.data_dir.clone().unwrap().join(STATE_FILE)),
            // What the rest of litek8s derives from them.
            ("vm assets", crate::dev::default_vm_assets_dir(&dev_home)),
            ("env socket", crate::dev::env::control_path(&dev_home)),
            ("named cluster", ci.home(&dev_home)),
            ("named cluster data", ci.data_dir(&dev_home)),
            ("named cluster socket", ci.docker_socket_from(&p.docker_socket)),
            ("ui.yaml", p.dev_config.with_file_name("ui.yaml")),
            ("ui-state.json", p.dev_config.with_file_name("ui-state.json")),
        ];
        for (what, path) in all {
            assert!(path.starts_with(home), "{what}: {} is outside {home}", path.display());
        }
        assert_eq!(p.context_prefix, "lkdev-");
        let vars = [("LITEK8S_HOME", home), ("LITEK8S_CONTEXT_PREFIX", "mine-")];
        assert_eq!(Paths::resolve(env_of(&vars)).context_prefix, "mine-");
    }

    #[test]
    fn without_litek8s_home_the_usual_places() {
        let vars = [("HOME", "/Users/me"), ("XDG_RUNTIME_DIR", "/run/user/501"), ("LITEK8S_CONTEXT_PREFIX", "ignored-")];
        let p = Paths::resolve(env_of(&vars));
        assert_eq!(p.home, None);
        assert_eq!(p.docker_socket, Path::new("/Users/me/.litek8s/docker.sock"));
        assert_eq!(p.dev_config, Path::new("/Users/me/.litek8s/config.yaml"));
        assert_eq!(p.kubeconfig, Path::new("/Users/me/.kube/config"));
        assert_eq!(p.daemon_socket, Path::new("/run/user/501/litek8s/daemon.sock"));
        assert_eq!(p.agent_socket, Path::new("/run/user/501/litek8s/agent.sock"));
        assert_eq!(p.context_prefix, "litek8s-");
        // A variable for one path wins over $LITEK8S_HOME.
        let vars = [("LITEK8S_HOME", "/h"), ("KUBECONFIG", "/k/a:/k/b"), ("LITEK8S_DOCKER_SOCKET", "/s.sock"), ("LITEK8S_DEV_HOME", "/d")];
        let p = Paths::resolve(env_of(&vars));
        assert_eq!((p.kubeconfig.as_path(), p.docker_socket.as_path(), p.dev_home.as_deref()), (Path::new("/k/a"), Path::new("/s.sock"), Some(Path::new("/d"))));
    }
}
