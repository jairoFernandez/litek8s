//! `litek8s install server|agent` and `litek8s uninstall`: run litek8s on a
//! Linux server as a systemd service, like k3s's install script.
//!
//! `install` checks the machine (root, systemd, crun, cgroup v2, kernel
//! WireGuard, free ports), puts the running binary at /usr/local/bin/litek8s,
//! writes `litek8s-server.service` (`litek8s apiserver`) or
//! `litek8s-agent.service` (`litek8s agent --join`) with sensible defaults
//! filled in, keeps the join token in /etc/litek8s/<role>.env (0600, never on
//! a command line), enables and (re)starts the unit and waits until the
//! server answers /readyz or the agent's Node is Ready. Re-running it
//! replaces the flags and restarts the service.
//!
//! The units use KillMode=process: restarting or upgrading litek8s leaves
//! the crun shims and the containers they run alone (they live in
//! /sys/fs/cgroup/litek8s anyway), and the new process adopts them.
//! `uninstall` stops everything: the unit, then every pod (cgroup.kill), the
//! pod network (bridge, WireGuard, iptables chains, netns mounts); `--purge`
//! also deletes the data directories.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub const BIN: &str = "/usr/local/bin/litek8s";
pub const ETC_DIR: &str = "/etc/litek8s";
pub const KUBECONFIG: &str = "/etc/litek8s/kubeconfig.yaml";
pub const DATA_DIR: &str = "/var/lib/litek8s/apiserver";
pub const NODE_DIR: &str = "/var/lib/litek8s/node-agent";
const UNIT_DIR: &str = "/etc/systemd/system";
const INSTALL_SCRIPT: &str = "https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh";
/// Where the crun runtime puts shims and pods (see kube::agent::crun).
const CGROUP_DIR: &str = "/sys/fs/cgroup/litek8s";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Server,
    Agent,
}

impl Role {
    pub fn unit(self) -> &'static str {
        match self {
            Role::Server => "litek8s-server.service",
            Role::Agent => "litek8s-agent.service",
        }
    }

    pub fn env_file(self) -> &'static str {
        match self {
            Role::Server => "/etc/litek8s/server.env",
            Role::Agent => "/etc/litek8s/agent.env",
        }
    }

    /// The litek8s subcommand the unit runs.
    pub fn subcommand(self) -> &'static str {
        match self {
            Role::Server => "apiserver",
            Role::Agent => "agent",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Role::Server => "litek8s server (Kubernetes API server, controllers and a node)",
            Role::Agent => "litek8s agent (a node of a litek8s cluster)",
        }
    }

    fn unit_path(self) -> PathBuf {
        Path::new(UNIT_DIR).join(self.unit())
    }

    fn other(self) -> Role {
        match self {
            Role::Server => Role::Agent,
            Role::Agent => Role::Server,
        }
    }
}

/// What the installer needs to know of the service's flags, as the service
/// itself will parse them (`cli::parse_service_args`).
#[derive(Debug, Clone, Default)]
pub struct ServiceArgs {
    /// Server: the API listen address.
    pub listen: Option<SocketAddr>,
    pub raft_port: Option<u16>,
    /// Server: --cluster-init or --server (an HA control plane).
    pub ha: bool,
    pub data_dir: Option<PathBuf>,
    pub node_dir: Option<PathBuf>,
    pub kubeconfig: Option<PathBuf>,
    pub run_node: bool,
    pub node_name: Option<String>,
    pub node_ip: Option<IpAddr>,
    pub advertise_address: Option<String>,
    /// The runtime may be crun (crun or auto).
    pub crun: bool,
    pub wireguard: bool,
    /// Agent: the server URL.
    pub join: Option<String>,
}

/// The unit's flags with the defaults filled in and the token taken out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub role: Role,
    pub args: Vec<String>,
    pub token: Option<String>,
}

fn flag_matches(arg: &str, name: &str) -> bool {
    arg == name || arg.strip_prefix(name).is_some_and(|rest| rest.starts_with('='))
}

fn has_flag(args: &[String], names: &[&str]) -> bool {
    args.iter().any(|a| names.iter().any(|n| flag_matches(a, n)))
}

/// Remove `--name value` / `--name=value` from `args`, returning the value.
fn take_flag(args: &mut Vec<String>, name: &str) -> Result<Option<String>> {
    let mut value = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            if i + 1 >= args.len() {
                bail!("{name} needs a value");
            }
            value = Some(args.remove(i + 1));
            args.remove(i);
        } else if let Some(v) = args[i].strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            value = Some(v.to_string());
            args.remove(i);
        } else {
            i += 1;
        }
    }
    Ok(value)
}

/// The unit's flags: the user's, after defaults for a server install. The
/// token comes from `--token`, else `fallback_token` (the environment or
/// the existing env file).
pub fn plan(role: Role, user_args: &[String], fallback_token: Option<String>, node_ip: impl FnOnce() -> Option<String>) -> Result<Plan> {
    let mut user = user_args.to_vec();
    let token = take_flag(&mut user, "--token")?.or(fallback_token).filter(|t| !t.is_empty());
    if token.as_deref().is_some_and(|t| t.contains(['\n', '\r', '\0'])) {
        bail!("the token contains a line break");
    }
    let mut args: Vec<String> = Vec::new();
    let mut default = |flag: &str, value: &str| {
        if !has_flag(&user, &[flag]) {
            args.push(flag.to_string());
            args.push(value.to_string());
        }
    };
    match role {
        Role::Server => {
            default("--listen", "0.0.0.0:6443");
            default("--data-dir", DATA_DIR);
            if !has_flag(&user, &["--no-node"]) {
                default("--node-dir", NODE_DIR);
            }
            default("--runtime", "crun");
            default("--kubeconfig", KUBECONFIG);
        }
        Role::Agent => {
            if !has_flag(&user, &["--join", "--server"]) {
                bail!("`litek8s install agent` needs --join https://<server>:6443 and --token <token> (both printed by `litek8s install server`)");
            }
            if token.is_none() {
                bail!("`litek8s install agent` needs --token <token> (printed by `litek8s install server`, kept in {DATA_DIR}/join-token on the server)");
            }
            default("--node-dir", NODE_DIR);
            default("--runtime", "crun");
        }
    }
    if !has_flag(&user, &["--node-ip"]) && !(role == Role::Server && has_flag(&user, &["--no-node"])) {
        if let Some(ip) = node_ip() {
            args.push("--node-ip".into());
            args.push(ip);
        }
    }
    args.extend(user);
    Ok(Plan { role, args, token })
}

/// One ExecStart argument, quoted for systemd when it needs it (`$` and `%`
/// are expanded by systemd even inside quotes).
pub fn systemd_quote(arg: &str) -> String {
    let plain = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=,@+[]".contains(c));
    if plain {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("$$"),
            '%' => out.push_str("%%"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The ExecStart line's arguments back from a unit `render_unit` wrote.
pub fn unit_args(unit: &str) -> Vec<String> {
    let joined = unit.replace("\\\n", " ");
    let Some(line) = joined.lines().find_map(|l| l.trim().strip_prefix("ExecStart=")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut word = String::new();
        let mut quoted = false;
        while let Some(c) = chars.next() {
            match c {
                '"' => quoted = !quoted,
                '\\' if quoted => match chars.next() {
                    Some('n') => word.push('\n'),
                    Some(c) => word.push(c),
                    None => {}
                },
                '$' | '%' if chars.peek() == Some(&c) => {
                    chars.next();
                    word.push(c);
                }
                c if c.is_whitespace() && !quoted => break,
                c => word.push(c),
            }
        }
        out.push(word);
    }
    out
}

/// The systemd unit for `plan`.
pub fn render_unit(plan: &Plan, wireguard: bool) -> String {
    let mut exec = format!("{BIN} {}", plan.role.subcommand());
    let mut i = 0;
    while i < plan.args.len() {
        let arg = &plan.args[i];
        exec.push_str(" \\\n    ");
        exec.push_str(&systemd_quote(arg));
        // A flag and its value on one line.
        if arg.starts_with("--") && !arg.contains('=') && plan.args.get(i + 1).is_some_and(|v| !v.starts_with("--")) {
            exec.push(' ');
            exec.push_str(&systemd_quote(&plan.args[i + 1]));
            i += 1;
        }
        i += 1;
    }
    let modules = if wireguard { "ExecStartPre=-/sbin/modprobe overlay\nExecStartPre=-/sbin/modprobe wireguard\n" } else { "ExecStartPre=-/sbin/modprobe overlay\n" };
    format!(
        "# Written by `litek8s install {role}`; re-run it to change the flags.\n\
         [Unit]\n\
         Description={description}\n\
         Documentation=https://github.com/jairoFernandez/litek8s\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         StartLimitIntervalSec=0\n\
         \n\
         [Service]\n\
         Type=simple\n\
         EnvironmentFile=-{env}\n\
         {modules}\
         ExecStart={exec}\n\
         Restart=always\n\
         RestartSec=2\n\
         # Only litek8s itself: pods (crun shims and containers) outlive a\n\
         # restart or an upgrade and are adopted by the new process.\n\
         KillMode=process\n\
         LimitNOFILE=65536\n\
         LimitNPROC=infinity\n\
         LimitCORE=infinity\n\
         TasksMax=infinity\n\
         TimeoutStartSec=0\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        role = match plan.role {
            Role::Server => "server",
            Role::Agent => "agent",
        },
        description = plan.role.description(),
        env = plan.role.env_file(),
    )
}

/// The EnvironmentFile holding the token (`LITEK8S_TOKEN` is read by both
/// `apiserver` and `agent`).
pub fn render_env(token: &str) -> String {
    format!("LITEK8S_TOKEN=\"{}\"\n", token.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The token in an env file `render_env` wrote.
pub fn env_token(text: &str) -> Option<String> {
    text.lines().find_map(|l| l.strip_prefix("LITEK8S_TOKEN=")).map(|v| {
        let v = v.trim();
        let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v);
        v.replace("\\\"", "\"").replace("\\\\", "\\")
    })
}

/// `https://host:port` that nodes and servers join, from the server's flags.
pub fn join_url(svc: &ServiceArgs) -> String {
    let port = svc.listen.map_or(6443, |l| l.port());
    let host = svc
        .advertise_address
        .clone()
        .or_else(|| svc.node_ip.map(|ip| ip.to_string()))
        .or_else(|| svc.listen.filter(|l| !l.ip().is_unspecified()).map(|l| l.ip().to_string()))
        .unwrap_or_else(crate::kube::agent::local_ip);
    let has_port = if host.starts_with('[') { host.contains("]:") } else { host.matches(':').count() == 1 };
    let host = if !has_port && host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
    if has_port { format!("https://{host}") } else { format!("https://{host}:{port}") }
}

/// What this machine lacks to run `role` with `svc`.
pub fn preflight(role: Role, svc: &ServiceArgs, check_ports: bool) -> Vec<String> {
    let mut problems = Vec::new();
    let runs_pods = role == Role::Agent || svc.run_node;
    if runs_pods && svc.crun && crate::kube::agent::crun_binary().is_none() {
        problems.push("crun is not installed: `apt-get install crun` (Debian/Ubuntu), `dnf install crun` (Fedora/RHEL); or pass --runtime docker".into());
    }
    if runs_pods && svc.crun {
        for tool in ["iptables", "iptables-restore"] {
            if crate::dev::which(tool).is_none() {
                problems.push(format!("{tool} is not installed (package `iptables`)"));
            }
        }
    }
    if runs_pods && !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        problems.push("cgroup v2 is not mounted at /sys/fs/cgroup (boot with systemd.unified_cgroup_hierarchy=1)".into());
    }
    if runs_pods && svc.wireguard && !wireguard_available() {
        problems.push(
            "the kernel has no WireGuard module (`modprobe wireguard` failed): install your kernel's extra modules \
             (e.g. `apt-get install linux-modules-extra-$(uname -r)`), or pass --no-wireguard for a single node"
                .into(),
        );
    }
    if role == Role::Server && check_ports {
        let listen = svc.listen.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 6443)));
        let mut ports = vec![(listen.port(), "the API server")];
        if svc.ha {
            ports.push((svc.raft_port.unwrap_or(listen.port().wrapping_add(1)), "Raft between servers"));
        }
        for (port, what) in ports {
            if let Err(e) = std::net::TcpListener::bind(SocketAddr::new(listen.ip(), port)) {
                problems.push(format!("TCP port {port} ({what}) is not free: {e} (`ss -ltnp 'sport = :{port}'` shows who has it)"));
            }
        }
    }
    problems
}

fn wireguard_available() -> bool {
    Path::new("/sys/module/wireguard").exists()
        || std::process::Command::new("modprobe")
            .args(["-q", "wireguard"])
            .status()
            .is_ok_and(|s| s.success())
}

fn is_root() -> bool {
    // SAFETY: no preconditions.
    unsafe { libc::geteuid() == 0 }
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("systemctl").args(args).status().context("failed to run systemctl")?;
    if !status.success() {
        bail!("`systemctl {}` failed", args.join(" "));
    }
    Ok(())
}

fn unit_active(role: Role) -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", role.unit()])
        .status()
        .is_ok_and(|s| s.success())
}

fn check_host() -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("`litek8s install` sets up a systemd service on a Linux server; on this machine use `litek8s dev up`");
    }
    if !is_root() {
        bail!("`litek8s install` and `uninstall` need root: run them with sudo");
    }
    if !Path::new("/run/systemd/system").exists() {
        bail!("systemd is not running here; run `litek8s apiserver` / `litek8s agent` under your init system instead");
    }
    Ok(())
}

/// Write `contents` to `path` with `mode`, replacing it atomically.
fn write_file(path: &Path, contents: &str, mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to write {}", path.display()))
}

/// Put the running binary at /usr/local/bin/litek8s (the units run that
/// path), unless it is already there.
fn ensure_binary() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe().context("cannot find the running binary")?.canonicalize()?;
    let target = Path::new(BIN);
    if target.canonicalize().ok().as_deref() == Some(exe.as_path()) {
        return Ok(());
    }
    if let (Ok(a), Ok(b)) = (std::fs::metadata(&exe), std::fs::metadata(target)) {
        if a.len() == b.len() && std::fs::read(&exe)? == std::fs::read(target)? {
            return Ok(());
        }
    }
    std::fs::create_dir_all(target.parent().expect("has a parent"))?;
    // A new inode: a running litek8s and its shims keep the old one.
    let tmp = target.with_file_name(".litek8s.new");
    std::fs::copy(&exe, &tmp).with_context(|| format!("failed to copy {} to {}", exe.display(), tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, target)?;
    println!("installed {} (from {})", target.display(), exe.display());
    Ok(())
}

pub struct InstallOptions {
    pub role: Role,
    pub args: Vec<String>,
    pub skip_checks: bool,
    pub timeout: Duration,
}

pub async fn install(mut o: InstallOptions) -> Result<()> {
    // `install server --skip-checks` means the installer's flag too.
    if let Some(i) = o.args.iter().position(|a| a == "--skip-checks") {
        o.args.remove(i);
        o.skip_checks = true;
    }
    if let Some(t) = take_flag(&mut o.args, "--timeout")? {
        o.timeout = crate::cli::parse_duration(&t).map_err(anyhow::Error::msg)?;
    }
    check_host()?;
    let role = o.role;
    if role.other().unit_path().exists() {
        bail!(
            "this machine already runs {} ({}); `litek8s uninstall` it first",
            role.other().unit(),
            if role == Role::Agent { "a server is also a node" } else { "an agent cannot also be a server" }
        );
    }
    let saved_token = std::fs::read_to_string(role.env_file()).ok().and_then(|t| env_token(&t));
    let env = std::env::var("LITEK8S_TOKEN").ok().filter(|t| !t.is_empty());
    let plan = plan(role, &o.args, env.or(saved_token), || {
        let ip = crate::kube::agent::local_ip();
        (!ip.starts_with("127.")).then_some(ip)
    })?;
    let mut parse_args = plan.args.clone();
    if let Some(token) = &plan.token {
        parse_args.extend(["--token".to_string(), token.clone()]);
    }
    let svc = crate::cli::parse_service_args(role, &parse_args)?;
    if !o.skip_checks {
        let problems = preflight(role, &svc, !unit_active(role));
        if !problems.is_empty() {
            bail!("this machine is not ready for litek8s (--skip-checks to go on anyway):\n  - {}", problems.join("\n  - "));
        }
    }
    ensure_binary()?;
    std::fs::create_dir_all(ETC_DIR)?;
    match &plan.token {
        Some(token) => write_file(Path::new(role.env_file()), &render_env(token), 0o600)?,
        None => {
            let _ = std::fs::remove_file(role.env_file());
        }
    }
    if let Some(kubeconfig) = &svc.kubeconfig {
        // The server rewrites it on every start; an existing file keeps
        // its mode.
        if let Some(parent) = kubeconfig.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let _ = std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(kubeconfig);
        std::fs::set_permissions(kubeconfig, std::fs::Permissions::from_mode(0o600))?;
    }
    write_file(&role.unit_path(), &render_unit(&plan, svc.wireguard), 0o644)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--quiet", role.unit()])?;
    systemctl(&["restart", role.unit()])?;
    println!("started {} (logs: journalctl -u {} -f)", role.unit(), role.unit().trim_end_matches(".service"));
    let ready = match role {
        Role::Server => wait_server(&svc, o.timeout).await,
        Role::Agent => wait_agent(&svc, plan.token.as_deref().unwrap_or_default(), o.timeout).await,
    };
    if let Err(e) = ready {
        let journal = std::process::Command::new("journalctl")
            .args(["-u", role.unit(), "-n", "30", "--no-pager"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        bail!("{e:#}\n\nlast lines of the log:\n{journal}");
    }
    match role {
        Role::Server => print_server_summary(&svc),
        Role::Agent => {
            let node = svc.node_name.clone().unwrap_or_else(crate::kube::agent::default_node_name);
            println!("node {node} joined {} and is Ready", svc.join.as_deref().unwrap_or_default());
        }
    }
    Ok(())
}

fn print_server_summary(svc: &ServiceArgs) {
    let token = svc.data_dir.as_ref().and_then(|d| std::fs::read_to_string(d.join("join-token")).ok()).map(|t| t.trim().to_string());
    println!("litek8s server is ready");
    if let Some(k) = &svc.kubeconfig {
        println!("\nkubeconfig (root only): {}\n  sudo kubectl --kubeconfig {} get nodes", k.display(), k.display());
    }
    println!(
        "\nThe API on port {} has no authentication: keep it on a trusted network (or firewall it).",
        svc.listen.map_or(6443, |l| l.port())
    );
    let Some(token) = token else { return };
    let url = join_url(svc);
    println!("\nJoin another machine as a node:\n  curl -fsSL {INSTALL_SCRIPT} | sudo sh -s -- agent --join {url} --token {token}");
    println!("or, where litek8s is installed:\n  sudo litek8s install agent --join {url} --token {token}");
    if svc.ha {
        println!("\nAdd a server to the control plane (3 or 5 in all):\n  curl -fsSL {INSTALL_SCRIPT} | sudo sh -s -- server --server {url} --token {token}");
    }
}

/// GET `path` over plain HTTP; the status code.
async fn http_status(addr: SocketAddr, path: &str) -> Result<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(addr)).await??;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes()).await?;
    let mut buf = vec![0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await??;
    let line = String::from_utf8_lossy(&buf[..n]);
    line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).context("not an HTTP answer")
}

async fn wait_server(svc: &ServiceArgs, timeout: Duration) -> Result<()> {
    let listen = svc.listen.unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 6443)));
    let addr = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::from(([127, 0, 0, 1], listen.port())),
        IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), listen.port()),
        _ => listen,
    };
    let started = Instant::now();
    println!("waiting for the API server to be ready (http://{addr}/readyz)");
    loop {
        if matches!(http_status(addr, "/readyz").await, Ok(200)) {
            return Ok(());
        }
        if started.elapsed() > timeout {
            bail!("the API server did not answer /readyz within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_agent(svc: &ServiceArgs, token: &str, timeout: Duration) -> Result<()> {
    let node_dir = svc.node_dir.clone().unwrap_or_else(|| PathBuf::from(NODE_DIR));
    let join = svc.join.clone().context("no --join")?;
    let node = svc.node_name.clone().unwrap_or_else(crate::kube::agent::default_node_name);
    let started = Instant::now();
    println!("waiting for node {node} to join {join} and be Ready");
    // The agent writes its node password before joining; only then can a
    // client here use the same identity.
    while !node_dir.join("join.json").exists() {
        if started.elapsed() > timeout {
            bail!("the agent did not join {join} within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut client = crate::kube::join::Client::new(&join, token, &node, &node_dir)?;
    loop {
        let last = match client.get(&format!("/api/v1/nodes/{node}")).await {
            Ok((200, body)) => {
                let obj: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let ready = obj["status"]["conditions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|c| c["type"] == "Ready")
                    .map(|c| c["status"].as_str().unwrap_or_default().to_string());
                if ready.as_deref() == Some("True") {
                    return Ok(());
                }
                format!("Ready={}", ready.unwrap_or_else(|| "unknown".into()))
            }
            Ok((status, _)) => format!("GET /api/v1/nodes/{node} answered {status}"),
            Err(e) => format!("{e:#}"),
        };
        if started.elapsed() > timeout {
            bail!("node {node} was not Ready within {timeout:?} ({last})");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// `litek8s uninstall`.
pub fn uninstall(purge: bool) -> Result<()> {
    check_host()?;
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut found = false;
    for role in [Role::Server, Role::Agent] {
        let path = role.unit_path();
        let Ok(unit) = std::fs::read_to_string(&path) else { continue };
        found = true;
        let args = unit_args(&unit);
        for flag in ["--data-dir", "--node-dir"] {
            if let Some(i) = args.iter().position(|a| a == flag) {
                if let Some(dir) = args.get(i + 1) {
                    dirs.push(PathBuf::from(dir));
                }
            } else if let Some(v) = args.iter().find_map(|a| a.strip_prefix(&format!("{flag}="))) {
                dirs.push(PathBuf::from(v));
            }
        }
        let _ = systemctl(&["disable", "--now", "--quiet", role.unit()]);
        std::fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
        let _ = std::fs::remove_file(role.env_file());
        println!("removed {}", role.unit());
    }
    let _ = systemctl(&["daemon-reload"]);
    let _ = std::process::Command::new("systemctl")
        .args(["reset-failed", "litek8s-server.service", "litek8s-agent.service"])
        .stderr(std::process::Stdio::null())
        .status();
    if !found {
        println!("no litek8s service installed; cleaning up what is left");
    }
    if dirs.is_empty() {
        dirs = vec![PathBuf::from(DATA_DIR), PathBuf::from(NODE_DIR)];
    }
    stop_pods();
    unmount_under(&dirs);
    clean_network();
    let _ = std::fs::remove_file(KUBECONFIG);
    if purge {
        for dir in &dirs {
            if !safe_to_purge(dir) {
                eprintln!("not deleting {} (not a litek8s data directory)", dir.display());
                continue;
            }
            match std::fs::remove_dir_all(dir) {
                Ok(()) => println!("deleted {}", dir.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("failed to delete {}: {e}", dir.display()),
            }
        }
        let _ = std::fs::remove_dir_all(ETC_DIR);
        let _ = std::fs::remove_dir("/var/lib/litek8s");
    } else {
        println!("kept the data in {} (`litek8s uninstall --purge` deletes it)", dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", "));
    }
    println!("{BIN} stays; delete it to remove litek8s entirely");
    Ok(())
}

/// Whether `dir` may be deleted by `--purge`: absolute, at least two levels
/// deep and not a system directory.
pub fn safe_to_purge(dir: &Path) -> bool {
    const SYSTEM: &[&str] = &["/usr", "/etc", "/var", "/var/lib", "/home", "/root", "/opt", "/srv", "/boot", "/tmp", "/run"];
    dir.is_absolute() && dir.components().count() >= 3 && !SYSTEM.iter().any(|s| Path::new(s) == dir)
}

/// Kill every pod: the crun shims and containers live under
/// /sys/fs/cgroup/litek8s.
fn stop_pods() {
    let root = Path::new(CGROUP_DIR);
    if !root.exists() {
        return;
    }
    if std::fs::write(root.join("cgroup.kill"), "1").is_err() {
        // Kernels before 5.14: signal every process of the subtree.
        for_each_cgroup(root, &mut |dir| {
            for pid in std::fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default().lines() {
                if let Ok(pid) = pid.trim().parse::<i32>() {
                    // SAFETY: plain syscall.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        });
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !std::fs::read_to_string(root.join("cgroup.events")).unwrap_or_default().contains("populated 0") {
        std::thread::sleep(Duration::from_millis(100));
    }
    // Leaves first.
    let mut all = Vec::new();
    for_each_cgroup(root, &mut |dir| all.push(dir.to_path_buf()));
    all.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for dir in all {
        let _ = std::fs::remove_dir(dir);
    }
    println!("stopped all pods");
}

fn for_each_cgroup(dir: &Path, f: &mut dyn FnMut(&Path)) {
    f(dir);
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            for_each_cgroup(&entry.path(), f);
        }
    }
}

/// Unescape a mountinfo path (`\040` for a space).
fn mountinfo_path(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let code: String = chars.by_ref().take(3).collect();
            match u8::from_str_radix(&code, 8) {
                Ok(b) => out.push(b as char),
                Err(_) => out.push_str(&code),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The mount points under one of `dirs` or a pod netns, deepest first.
pub fn mounts_to_remove(mountinfo: &str, dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = mountinfo
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|p| PathBuf::from(mountinfo_path(p)))
        .filter(|p| dirs.iter().any(|d| p.starts_with(d)) || p.to_string_lossy().starts_with("/run/netns/litek8s-"))
        .collect();
    out.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    out.dedup();
    out
}

fn unmount_under(dirs: &[PathBuf]) {
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    for mount in mounts_to_remove(&info, dirs) {
        let _ = std::process::Command::new("umount").arg("-l").arg(&mount).status();
    }
    for entry in std::fs::read_dir("/run/netns").into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with("litek8s-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The pod network: bridge, WireGuard interface, iptables chains.
fn clean_network() {
    for link in ["litek8s0", "litek8s-wg"] {
        let _ = std::process::Command::new("ip")
            .args(["link", "delete", link])
            .stderr(std::process::Stdio::null())
            .status();
    }
    for table in ["nat", "filter", "mangle", "raw"] {
        let Ok(out) = std::process::Command::new("iptables-save").args(["-t", table]).output() else { return };
        let rules = String::from_utf8_lossy(&out.stdout).to_string();
        for cmd in iptables_cleanup(table, &rules) {
            let _ = std::process::Command::new("sh").arg("-c").arg(&cmd).stderr(std::process::Stdio::null()).status();
        }
    }
}

/// Commands that remove litek8s's iptables rules and chains from `table`
/// (given as `iptables-save -t table` prints it).
pub fn iptables_cleanup(table: &str, rules: &str) -> Vec<String> {
    let mut cmds = Vec::new();
    let mut chains = Vec::new();
    for line in rules.lines() {
        if let Some(rest) = line.strip_prefix(':') {
            let chain = rest.split_whitespace().next().unwrap_or_default();
            if chain.starts_with("LITEK8S-") {
                chains.push(chain.to_string());
            }
        } else if let Some(rule) = line.strip_prefix("-A ") {
            let chain = rule.split_whitespace().next().unwrap_or_default();
            // Jumps into our chains from the others; ours go with -F.
            if !chain.starts_with("LITEK8S-") && rule.contains("-j LITEK8S-") {
                cmds.push(format!("iptables -t {table} -D {rule}"));
            }
        }
    }
    for chain in &chains {
        cmds.push(format!("iptables -t {table} -F {chain}"));
    }
    for chain in &chains {
        cmds.push(format!("iptables -t {table} -X {chain}"));
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn ip() -> Option<String> {
        Some("192.168.1.10".into())
    }

    #[test]
    fn server_defaults() {
        let p = plan(Role::Server, &[], None, ip).unwrap();
        assert_eq!(
            p.args,
            s(&[
                "--listen", "0.0.0.0:6443", "--data-dir", DATA_DIR, "--node-dir", NODE_DIR, "--runtime", "crun", "--kubeconfig", KUBECONFIG, "--node-ip",
                "192.168.1.10"
            ])
        );
        assert_eq!(p.token, None);
    }

    #[test]
    fn user_flags_win_over_defaults() {
        let p = plan(Role::Server, &s(&["--listen=10.0.0.1:7443", "--node-ip", "10.0.0.1", "--runtime", "docker", "--cluster-init"]), None, || panic!("no detection")).unwrap();
        assert!(!p.args.iter().any(|a| a == "0.0.0.0:6443" || a == "crun"));
        assert_eq!(p.args.iter().filter(|a| a.starts_with("--node-ip")).count(), 1);
        assert!(p.args.ends_with(&s(&["--listen=10.0.0.1:7443", "--node-ip", "10.0.0.1", "--runtime", "docker", "--cluster-init"])));
        let p = plan(Role::Server, &s(&["--no-node"]), None, || panic!("no detection")).unwrap();
        assert!(!p.args.iter().any(|a| a == "--node-dir" || a == "--node-ip"));
    }

    #[test]
    fn token_moves_out_of_the_flags() {
        for args in [s(&["--server", "https://a:6443", "--token", "K10ab::cd"]), s(&["--server", "https://a:6443", "--token=K10ab::cd"])] {
            let p = plan(Role::Server, &args, Some("old".into()), ip).unwrap();
            assert_eq!(p.token.as_deref(), Some("K10ab::cd"));
            assert!(!p.args.iter().any(|a| a.contains("K10ab")), "{:?}", p.args);
        }
        // Re-running without --token keeps the saved one.
        let p = plan(Role::Agent, &s(&["--join", "https://a:6443"]), Some("saved".into()), ip).unwrap();
        assert_eq!(p.token.as_deref(), Some("saved"));
        assert!(plan(Role::Server, &s(&["--token"]), None, ip).is_err());
    }

    #[test]
    fn agent_needs_join_and_token() {
        assert!(plan(Role::Agent, &s(&["--token", "t"]), None, ip).unwrap_err().to_string().contains("--join"));
        assert!(plan(Role::Agent, &s(&["--join", "https://a:6443"]), None, ip).unwrap_err().to_string().contains("--token"));
        let p = plan(Role::Agent, &s(&["--join", "https://a:6443", "--token", "t"]), None, ip).unwrap();
        assert_eq!(p.args, s(&["--node-dir", NODE_DIR, "--runtime", "crun", "--node-ip", "192.168.1.10", "--join", "https://a:6443"]));
        // The --server alias counts as --join.
        assert!(plan(Role::Agent, &s(&["--server=https://a:6443", "--token", "t"]), None, ip).is_ok());
    }

    #[test]
    fn unit_renders_and_round_trips() {
        let p = Plan {
            role: Role::Server,
            args: s(&["--listen", "0.0.0.0:6443", "--cluster-init", "--node-name", "my node", "--advertise-address", "$HOST%h\"\\"]),
            token: Some("K10x::y".into()),
        };
        let unit = render_unit(&p, true);
        assert!(unit.contains("ExecStart=/usr/local/bin/litek8s apiserver \\\n    --listen 0.0.0.0:6443 \\\n    --cluster-init \\\n    --node-name \"my node\""));
        assert!(unit.contains("\"$$HOST%%h\\\"\\\\\""), "{unit}");
        for line in ["KillMode=process", "Restart=always", "LimitNOFILE=65536", "After=network-online.target", "EnvironmentFile=-/etc/litek8s/server.env", "ExecStartPre=-/sbin/modprobe wireguard", "WantedBy=multi-user.target"] {
            assert!(unit.lines().any(|l| l == line), "missing {line}");
        }
        assert!(!unit.contains("K10x"), "the token never goes into the unit");
        let mut back = s(&[BIN, "apiserver"]);
        back.extend(p.args.clone());
        assert_eq!(unit_args(&unit), back);
        assert!(!render_unit(&p, false).contains("modprobe wireguard"));
        let agent = render_unit(&Plan { role: Role::Agent, args: s(&["--join", "https://a:6443"]), token: None }, true);
        assert!(agent.contains("EnvironmentFile=-/etc/litek8s/agent.env"));
        assert!(agent.contains("litek8s agent \\\n    --join https://a:6443\n"));
    }

    #[test]
    fn planned_flags_parse_as_the_service_will() {
        let p = plan(Role::Server, &s(&["--cluster-init", "--no-wireguard"]), None, ip).unwrap();
        let svc = crate::cli::parse_service_args(Role::Server, &p.args).unwrap();
        assert_eq!(svc.listen, Some("0.0.0.0:6443".parse().unwrap()));
        assert_eq!(svc.data_dir.as_deref(), Some(Path::new(DATA_DIR)));
        assert_eq!(svc.kubeconfig.as_deref(), Some(Path::new(KUBECONFIG)));
        assert_eq!(svc.node_ip, Some("192.168.1.10".parse().unwrap()));
        assert!(svc.ha && svc.crun && svc.run_node && !svc.wireguard);
        // An HA server joining another needs its token, which the installer
        // passes back for the check.
        let p = plan(Role::Server, &s(&["--server", "https://a:6443", "--token", "K10x::y"]), None, ip).unwrap();
        assert!(crate::cli::parse_service_args(Role::Server, &p.args).is_err());
        let mut args = p.args.clone();
        args.extend(s(&["--token", "K10x::y"]));
        assert!(crate::cli::parse_service_args(Role::Server, &args).unwrap().ha);
        // A typo fails the install, not the service.
        let err = crate::cli::parse_service_args(Role::Server, &s(&["--data-dir", "/d", "--node-ipp", "1.2.3.4"])).unwrap_err();
        assert!(format!("{err:#}").contains("--node-ipp"), "{err:#}");
        let p = plan(Role::Agent, &s(&["--join", "https://a:6443", "--token", "t", "--runtime", "docker", "--node-name", "pi"]), None, ip).unwrap();
        let mut args = p.args.clone();
        args.extend(s(&["--token", "t"]));
        let svc = crate::cli::parse_service_args(Role::Agent, &args).unwrap();
        assert_eq!(svc.join.as_deref(), Some("https://a:6443"));
        assert_eq!(svc.node_name.as_deref(), Some("pi"));
        assert!(!svc.crun && svc.wireguard);
    }

    #[test]
    fn env_file_round_trips() {
        for token in ["K10abc::def", "we\"ird\\"] {
            assert_eq!(env_token(&render_env(token)).as_deref(), Some(token));
        }
        assert_eq!(env_token("LITEK8S_TOKEN=plain\n").as_deref(), Some("plain"));
    }

    #[test]
    fn join_url_prefers_advertise_then_node_ip() {
        let mut svc = ServiceArgs { listen: Some("0.0.0.0:7443".parse().unwrap()), node_ip: Some("10.0.0.5".parse().unwrap()), ..Default::default() };
        assert_eq!(join_url(&svc), "https://10.0.0.5:7443");
        svc.advertise_address = Some("k8s.home:443".into());
        assert_eq!(join_url(&svc), "https://k8s.home:443");
        svc.advertise_address = Some("fd00::1".into());
        assert_eq!(join_url(&svc), "https://[fd00::1]:7443");
    }

    #[test]
    fn purge_refuses_system_dirs() {
        assert!(safe_to_purge(Path::new(DATA_DIR)));
        assert!(safe_to_purge(Path::new("/srv/litek8s/data")));
        for d in ["/", "/var", "/var/lib", "/etc", "relative/dir", "/data"] {
            assert!(!safe_to_purge(Path::new(d)), "{d}");
        }
    }

    #[test]
    fn mounts_under_data_dirs_deepest_first() {
        let info = "\
25 1 8:1 / / rw - ext4 /dev/sda1 rw
40 25 0:50 / /var/lib/litek8s/node-agent/pods/a/rootfs rw - overlay overlay rw
41 40 0:51 / /var/lib/litek8s/node-agent/pods/a/rootfs/dev rw - tmpfs tmpfs rw
42 25 0:4 net:[4026532] /run/netns/litek8s-abc rw - nsfs nsfs rw
43 25 0:52 / /var/lib/other rw - tmpfs tmpfs rw
44 25 0:53 / /var/lib/litek8s/node-agent/my\\040vol rw - tmpfs tmpfs rw";
        let got = mounts_to_remove(info, &[PathBuf::from(NODE_DIR)]);
        assert_eq!(
            got,
            [
                "/var/lib/litek8s/node-agent/pods/a/rootfs/dev",
                "/var/lib/litek8s/node-agent/pods/a/rootfs",
                "/var/lib/litek8s/node-agent/my vol",
                "/run/netns/litek8s-abc"
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn iptables_cleanup_deletes_jumps_then_chains() {
        let rules = "\
*nat
:PREROUTING ACCEPT [0:0]
:POSTROUTING ACCEPT [0:0]
:LITEK8S-POSTROUTING - [0:0]
:LITEK8S-HOSTPORTS - [0:0]
-A PREROUTING -m addrtype --dst-type LOCAL -j LITEK8S-HOSTPORTS
-A POSTROUTING -j LITEK8S-POSTROUTING
-A POSTROUTING -s 172.17.0.0/16 -j MASQUERADE
-A LITEK8S-POSTROUTING -s 10.42.0.0/24 -j MASQUERADE
COMMIT";
        assert_eq!(
            iptables_cleanup("nat", rules),
            [
                "iptables -t nat -D PREROUTING -m addrtype --dst-type LOCAL -j LITEK8S-HOSTPORTS",
                "iptables -t nat -D POSTROUTING -j LITEK8S-POSTROUTING",
                "iptables -t nat -F LITEK8S-POSTROUTING",
                "iptables -t nat -F LITEK8S-HOSTPORTS",
                "iptables -t nat -X LITEK8S-POSTROUTING",
                "iptables -t nat -X LITEK8S-HOSTPORTS",
            ]
        );
    }
}
