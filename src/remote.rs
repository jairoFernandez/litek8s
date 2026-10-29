//! `litek8s install server|agent --ssh user@host` and `litek8s uninstall
//! --ssh user@host`: the installer of `install.rs`, run on another machine
//! from this one.
//!
//! Everything goes through the user's own `ssh` (their ~/.ssh/config, agent,
//! keys, known_hosts, ProxyJump), one command per step: a probe (OS, arch,
//! sudo, what litek8s is installed), the binary (kept when the remote one is
//! the same; else uploaded when it is a Linux build for the remote's
//! architecture, or downloaded by the remote's curl from the GitHub release
//! of this version), crun and iptables from the package manager if missing,
//! then `sudo litek8s install ...` with its output streamed here. The token
//! travels in a 0600 file, never on a command line. The remote install ends
//! with a JSON line ([`crate::install::InstallResult`]): whether anything
//! changed, the node, and for a server the join URL, token and CA, from
//! which `install server --ssh` adds a kubeconfig context here.

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::install::{InstallResult, RESULT_ENV, RESULT_MARKER, Role};

pub const REPO: &str = "jairoFernandez/litek8s";
/// Overrides where release archives come from (as for install.sh): mirrors, tests.
pub const DOWNLOAD_URL_VAR: &str = "LITEK8S_DOWNLOAD_URL";

/// How to reach a machine: what `ssh` gets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ssh {
    /// `user@host`, `host` or a Host alias of ~/.ssh/config.
    pub dest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<PathBuf>,
    /// `-o` options (`StrictHostKeyChecking=accept-new`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

impl Ssh {
    pub fn new(dest: &str) -> Self {
        Ssh { dest: dest.to_string(), ..Default::default() }
    }

    pub fn validate(&self) -> Result<()> {
        if self.dest.is_empty() {
            bail!("empty ssh destination");
        }
        // ssh would take it for an option (`-oProxyCommand=...`).
        if self.dest.starts_with('-') || self.dest.chars().any(|c| c.is_whitespace() || c.is_control()) {
            bail!("invalid ssh destination {:?}", self.dest);
        }
        for o in &self.options {
            if !o.contains('=') && !o.contains(' ') {
                bail!("ssh option {o:?} is not Key=Value");
            }
        }
        Ok(())
    }

    /// `user@host[:port]` for messages.
    pub fn display(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.dest),
            None => self.dest.clone(),
        }
    }

    /// The host part of the destination.
    pub fn host(&self) -> &str {
        self.dest.rsplit_once('@').map_or(self.dest.as_str(), |(_, h)| h)
    }

    /// ssh's arguments before the remote command. `batch`: no prompts
    /// (no terminal to answer them).
    pub fn args(&self, batch: bool) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(p) = self.port {
            a.extend(["-p".to_string(), p.to_string()]);
        }
        if let Some(i) = &self.identity_file {
            a.extend(["-i".to_string(), i.display().to_string()]);
        }
        for o in &self.options {
            a.extend(["-o".to_string(), o.clone()]);
        }
        if batch {
            a.extend(["-o".to_string(), "BatchMode=yes".to_string()]);
        }
        a.extend(["-o", "ConnectTimeout=20", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=4"].map(String::from));
        a.push(self.dest.clone());
        a
    }

    /// The host name ssh connects to after ~/.ssh/config (`ssh -G`): an
    /// alias becomes its HostName.
    pub async fn resolved_host(&self) -> Option<String> {
        let mut args = vec!["-G".to_string()];
        args.extend(self.args(true));
        let out = tokio::process::Command::new("ssh").args(&args).stdin(Stdio::null()).output().await.ok()?;
        parse_ssh_g_hostname(&String::from_utf8_lossy(&out.stdout))
    }
}

pub fn parse_ssh_g_hostname(out: &str) -> Option<String> {
    out.lines().find_map(|l| l.strip_prefix("hostname ")).map(|h| h.trim().to_string()).filter(|h| !h.is_empty())
}

/// Quote `s` for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=,@+%".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `script` run by `sh -c` with `args` as $1..., one remote command line.
pub fn sh_command(script: &str, args: &[String]) -> String {
    let mut cmd = format!("sh -c {} sh", sh_quote(script));
    for a in args {
        cmd.push(' ');
        cmd.push_str(&sh_quote(a));
    }
    cmd
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// The release archive's target (`litek8s-<tag>-<target>.tar.gz`).
    pub fn target(self) -> &'static str {
        match self {
            Arch::X86_64 => "linux-x86_64",
            Arch::Aarch64 => "linux-aarch64",
        }
    }

    /// bench/build-linux.sh's name for it.
    pub fn docker(self) -> &'static str {
        match self {
            Arch::X86_64 => "amd64",
            Arch::Aarch64 => "arm64",
        }
    }

    /// This build's, when it is a Linux one.
    pub fn local_linux() -> Option<Arch> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        if cfg!(target_arch = "x86_64") {
            Some(Arch::X86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Arch::Aarch64)
        } else {
            None
        }
    }
}

/// `uname -s -m`.
pub fn parse_uname(uname: &str) -> Result<Arch> {
    let parts: Vec<&str> = uname.split_whitespace().collect();
    match parts.as_slice() {
        ["Linux", "x86_64" | "amd64"] => Ok(Arch::X86_64),
        ["Linux", "aarch64" | "arm64"] => Ok(Arch::Aarch64),
        [os, arch] => bail!("litek8s runs on Linux x86_64 and aarch64 servers; this one is {os} {arch}"),
        _ => bail!("cannot tell the platform from `uname -s -m`: {uname:?}"),
    }
}

/// The architecture of an ELF executable, from its header.
pub fn elf_arch(head: &[u8]) -> Option<Arch> {
    if head.len() < 20 || &head[..4] != b"\x7fELF" {
        return None;
    }
    // e_machine, little endian (both targets are).
    match u16::from_le_bytes([head[18], head[19]]) {
        0x3e => Some(Arch::X86_64),
        0xb7 => Some(Arch::Aarch64),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SudoMode {
    Root,
    NoPassword,
    Password,
    Missing,
}

/// What the probe found on the remote machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub arch: Result<Arch, String>,
    pub sudo: SudoMode,
    pub user: String,
    pub hostname: String,
    pub systemd: bool,
    /// `litek8s --version` of /usr/local/bin/litek8s, without the name.
    pub version: Option<String>,
    pub sha256: Option<String>,
    /// Tools found: curl, crun, iptables-restore, sha256sum, tar.
    pub tools: BTreeSet<String>,
    pub server_active: bool,
    pub agent_active: bool,
    pub server_installed: bool,
    pub agent_installed: bool,
}

const PROBE: &str = r#"
echo "uname=$(uname -s -m)"
echo "user=$(id -un)"
echo "hostname=$(hostname)"
if [ "$(id -u)" = 0 ]; then echo sudo=root
elif ! command -v sudo >/dev/null 2>&1; then echo sudo=missing
elif sudo -n true 2>/dev/null; then echo sudo=nopassword
else echo sudo=password; fi
[ -d /run/systemd/system ] && echo systemd=1
b=/usr/local/bin/litek8s
if [ -x "$b" ]; then
  echo "version=$("$b" --version 2>/dev/null | head -n 1)"
  echo "sha256=$( (sha256sum "$b" 2>/dev/null || shasum -a 256 "$b" 2>/dev/null) | cut -d ' ' -f 1)"
fi
for t in curl tar sha256sum crun iptables-restore; do
  if command -v "$t" >/dev/null 2>&1 || [ -x "/usr/sbin/$t" ] || [ -x "/sbin/$t" ] || [ -x "/usr/local/bin/$t" ]; then echo "tool=$t"; fi
done
[ -f /etc/systemd/system/litek8s-server.service ] && echo installed=server
[ -f /etc/systemd/system/litek8s-agent.service ] && echo installed=agent
systemctl is-active --quiet litek8s-server.service 2>/dev/null && echo active=server
systemctl is-active --quiet litek8s-agent.service 2>/dev/null && echo active=agent
true
"#;

pub fn parse_probe(out: &str) -> Probe {
    let mut p = Probe {
        arch: Err("no answer".into()),
        sudo: SudoMode::Missing,
        user: String::new(),
        hostname: String::new(),
        systemd: false,
        version: None,
        sha256: None,
        tools: BTreeSet::new(),
        server_active: false,
        agent_active: false,
        server_installed: false,
        agent_installed: false,
    };
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "uname" => p.arch = parse_uname(v).map_err(|e| e.to_string()),
            "user" => p.user = v.to_string(),
            "hostname" => p.hostname = v.to_string(),
            "sudo" => {
                p.sudo = match v {
                    "root" => SudoMode::Root,
                    "nopassword" => SudoMode::NoPassword,
                    "password" => SudoMode::Password,
                    _ => SudoMode::Missing,
                }
            }
            "systemd" => p.systemd = true,
            "version" => p.version = Some(v.strip_prefix("litek8s ").unwrap_or(v).split(" (").next().unwrap_or_default().trim().to_string()).filter(|v| !v.is_empty()),
            "sha256" => p.sha256 = Some(v.trim().to_string()).filter(|v| v.len() == 64),
            "tool" => {
                p.tools.insert(v.to_string());
            }
            "installed" if v == "server" => p.server_installed = true,
            "installed" if v == "agent" => p.agent_installed = true,
            "active" if v == "server" => p.server_active = true,
            "active" if v == "agent" => p.agent_active = true,
            _ => {}
        }
    }
    p
}

/// Where the binary for the remote machine comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinarySource {
    /// /usr/local/bin/litek8s there is already the one wanted.
    Installed,
    Upload(PathBuf),
    /// The release with this tag, fetched by the remote machine.
    Download(String),
}

/// What this machine has to offer as a binary.
#[derive(Debug, Clone, Default)]
pub struct LocalBinary {
    /// `--binary`.
    pub path: Option<PathBuf>,
    pub path_sha256: Option<String>,
    pub path_arch: Option<Arch>,
    /// `--release`.
    pub release: Option<String>,
    /// [`crate::config::VERSION`].
    pub version: String,
    /// The running binary, when it is a static Linux build (musl).
    pub exe: Option<(PathBuf, Arch, String)>,
}

impl LocalBinary {
    pub fn detect(path: Option<PathBuf>, release: Option<String>) -> Result<Self> {
        let mut b = LocalBinary { release, version: crate::config::VERSION.to_string(), ..Default::default() };
        if let Some(p) = path {
            let bytes = std::fs::read(&p).with_context(|| format!("cannot read --binary {}", p.display()))?;
            b.path_arch = elf_arch(&bytes);
            b.path_sha256 = Some(sha256_hex(&bytes));
            b.path = Some(p);
        }
        if cfg!(target_env = "musl")
            && let (Some(arch), Ok(exe)) = (Arch::local_linux(), std::env::current_exe())
            && let Ok(bytes) = std::fs::read(&exe)
        {
            b.exe = Some((exe, arch, sha256_hex(&bytes)));
        }
        Ok(b)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// A tag (`v0.2.0`) for a version (`0.2.0`), and back.
fn tag_version(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

pub fn choose_binary(local: &LocalBinary, probe: &Probe, arch: Arch) -> Result<BinarySource> {
    if let Some(p) = &local.path {
        match local.path_arch {
            Some(a) if a == arch => {}
            Some(a) => bail!("--binary {} is a {} build, the machine is {}", p.display(), a.target(), arch.target()),
            None => bail!("--binary {} is not a Linux executable", p.display()),
        }
        return Ok(if local.path_sha256.is_some() && local.path_sha256 == probe.sha256 { BinarySource::Installed } else { BinarySource::Upload(p.clone()) });
    }
    if let Some(tag) = &local.release {
        return Ok(if probe.version.as_deref() == Some(tag_version(tag)) { BinarySource::Installed } else { BinarySource::Download(tag.clone()) });
    }
    if let Some((exe, a, sha)) = &local.exe
        && *a == arch
    {
        return Ok(if probe.sha256.as_deref() == Some(sha.as_str()) { BinarySource::Installed } else { BinarySource::Upload(exe.clone()) });
    }
    if local.version.contains(" DEV ") {
        bail!(
            "this litek8s is a development build ({}): there is no release of it to download. Pass --binary with a \
             {} build (bench/build-linux.sh {}) or --release vX.Y.Z",
            local.version,
            arch.target(),
            arch.docker()
        );
    }
    Ok(if probe.version.as_deref() == Some(local.version.as_str()) { BinarySource::Installed } else { BinarySource::Download(format!("v{}", local.version)) })
}

/// Where the archives of release `tag` are.
pub fn download_base(tag: &str) -> String {
    match std::env::var(DOWNLOAD_URL_VAR).ok().filter(|v| !v.is_empty()) {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => format!("https://github.com/{REPO}/releases/download/{tag}"),
    }
}

/// Fetch and check a release archive into $1 (as install.sh does).
const DOWNLOAD: &str = r#"set -eu
cd "$1"; base="$2"; archive="$3"
curl -fsSL "$base/$archive" -o "$archive" || { echo "download failed: $base/$archive" >&2; exit 1; }
curl -fsSL "$base/SHA256SUMS" -o SHA256SUMS || { echo "download failed: $base/SHA256SUMS" >&2; exit 1; }
expected="$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1 }' SHA256SUMS)"
[ -n "$expected" ] || { echo "$archive is not in SHA256SUMS" >&2; exit 1; }
actual="$( (sha256sum "$archive" 2>/dev/null || shasum -a 256 "$archive") | cut -d ' ' -f 1)"
[ "$expected" = "$actual" ] || { echo "checksum mismatch for $archive" >&2; exit 1; }
tar -xzf "$archive"
mv "${archive%.tar.gz}/bin/litek8s" litek8s
chmod 755 litek8s
rm -rf "$archive" SHA256SUMS "${archive%.tar.gz}"
"#;

/// crun and iptables from the package manager (install.sh's install_deps).
const DEPS: &str = r#"missing=""
command -v crun >/dev/null 2>&1 || [ -x /usr/bin/crun ] || [ -x /usr/local/bin/crun ] || missing="$missing crun"
command -v iptables-restore >/dev/null 2>&1 || [ -x /usr/sbin/iptables-restore ] || [ -x /sbin/iptables-restore ] || missing="$missing iptables"
[ -n "$missing" ] || exit 0
echo "installing$missing"
if command -v apt-get >/dev/null 2>&1; then
  env DEBIAN_FRONTEND=noninteractive NEEDRESTART_SUSPEND=1 apt-get update -qq >/dev/null && env DEBIAN_FRONTEND=noninteractive NEEDRESTART_SUSPEND=1 apt-get install -y -qq $missing >/dev/null
elif command -v dnf >/dev/null 2>&1; then dnf install -y -q $missing
elif command -v yum >/dev/null 2>&1; then yum install -y -q $missing
elif command -v zypper >/dev/null 2>&1; then zypper --non-interactive install $missing
elif command -v apk >/dev/null 2>&1; then apk add $missing
elif command -v pacman >/dev/null 2>&1; then pacman -S --noconfirm --needed $missing
else echo "no known package manager: install$missing yourself"; exit 0; fi || echo "warning: could not install$missing (the install says what is missing)"
"#;

/// Run the staged binary's install as root, then remove the staging dir.
const RUN_INSTALL: &str = r#"dir="$1"; bin="$2"; shift 2
if [ -f "$dir/token" ]; then LITEK8S_TOKEN="$(cat "$dir/token")"; export LITEK8S_TOKEN; fi
rm -f "$dir/token"
"$bin" install "$@"; rc=$?
rm -rf "$dir"
exit $rc
"#;

/// A connected remote machine: its probe and how to be root there.
pub struct Remote {
    pub ssh: Ssh,
    /// Prefix of streamed lines (`[pi@10.0.0.2]`), none for a single host.
    pub label: Option<String>,
    pub probe: Probe,
    password: Option<String>,
    batch: bool,
}

/// Whether to run an ssh command again: ssh itself failed (255) before the
/// command printed anything, i.e. the connection was not made (every step
/// is safe to repeat). Waits before the next attempt.
async fn retry_connection(code: Option<i32>, output: bool, attempt: &mut u32) -> bool {
    if code != Some(255) || output || *attempt >= 3 {
        return false;
    }
    tokio::time::sleep(Duration::from_secs(2 * u64::from(*attempt))).await;
    *attempt += 1;
    true
}

/// Read a line from the terminal without echo.
fn read_password(prompt: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    let tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").context("no terminal to ask for the sudo password")?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&tty);
    // SAFETY: termios on an open fd.
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    let saved = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
    let original = term;
    if saved {
        term.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
    }
    let mut out = &tty;
    let _ = write!(out, "{prompt}");
    let _ = out.flush();
    let mut line = String::new();
    let read = std::io::BufReader::new(&tty).read_line(&mut line);
    if saved {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
    }
    let _ = writeln!(out);
    read?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

impl Remote {
    /// Probe the machine; ask for the sudo password if it needs one and
    /// there is a terminal.
    pub async fn connect(ssh: &Ssh, label: Option<String>, need_root: bool) -> Result<Self> {
        ssh.validate()?;
        let batch = !std::io::stdin().is_terminal();
        let mut r = Remote { ssh: ssh.clone(), label, probe: parse_probe(""), password: None, batch };
        let out = r.capture(PROBE, &[], None).await.with_context(|| format!("cannot reach {} over ssh", ssh.display()))?;
        r.probe = parse_probe(&out);
        if need_root {
            match r.probe.sudo {
                SudoMode::Root | SudoMode::NoPassword => {}
                SudoMode::Missing => bail!("{} is not root and has no sudo: litek8s installs a systemd service and needs root", ssh.display()),
                SudoMode::Password if batch => bail!(
                    "sudo on {} asks for a password and there is no terminal to type it: run this from a terminal, or allow \
                     {} passwordless sudo (`echo '{} ALL=(ALL) NOPASSWD:ALL' | sudo tee /etc/sudoers.d/litek8s`)",
                    ssh.display(),
                    r.probe.user,
                    r.probe.user
                ),
                SudoMode::Password => {
                    let pw = tokio::task::spawn_blocking({
                        let prompt = format!("[sudo] password for {}@{}: ", r.probe.user, ssh.host());
                        move || read_password(&prompt)
                    })
                    .await??;
                    r.password = Some(pw);
                    let check = r.root_capture("true", &[]).await;
                    if check.is_err() {
                        bail!("sudo on {} did not take that password", ssh.display());
                    }
                }
            }
        }
        Ok(r)
    }

    pub fn display(&self) -> String {
        self.ssh.display()
    }

    fn command(&self, remote_cmd: &str) -> tokio::process::Command {
        let mut c = tokio::process::Command::new("ssh");
        c.args(self.ssh.args(self.batch)).arg(remote_cmd);
        c.kill_on_drop(true);
        c
    }

    /// `script` with `args`, as root: the remote command line and what to
    /// write first on its stdin (the sudo password).
    fn as_root(&self, script: &str, args: &[String]) -> (String, Option<String>) {
        let cmd = sh_command(script, args);
        match self.probe.sudo {
            SudoMode::Root => (cmd, None),
            _ if self.password.is_some() => (format!("sudo -S -p '' {cmd}"), self.password.as_ref().map(|p| format!("{p}\n"))),
            _ => (format!("sudo -n {cmd}"), None),
        }
    }

    /// Run `script` and return its stdout.
    pub async fn capture(&self, script: &str, args: &[String], stdin: Option<String>) -> Result<String> {
        self.capture_cmd(&sh_command(script, args), stdin).await
    }

    async fn root_capture(&self, script: &str, args: &[String]) -> Result<String> {
        let (cmd, stdin) = self.as_root(script, args);
        self.capture_cmd(&cmd, stdin).await
    }

    async fn capture_cmd(&self, cmd: &str, stdin: Option<String>) -> Result<String> {
        let mut attempt = 1;
        loop {
            let mut c = self.command(cmd);
            c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = c.spawn().context("cannot run ssh")?;
            if let (Some(text), Some(mut pipe)) = (&stdin, child.stdin.take()) {
                let _ = pipe.write_all(text.as_bytes()).await;
            }
            let out = child.wait_with_output().await?;
            if out.status.success() {
                return Ok(String::from_utf8_lossy(&out.stdout).to_string());
            }
            if retry_connection(out.status.code(), !out.stdout.is_empty(), &mut attempt).await {
                continue;
            }
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("{} on {}: {}", if out.status.code() == Some(255) { "ssh failed" } else { "command failed" }, self.ssh.display(), err.trim());
        }
    }

    /// Run a remote command line, printing its output as it comes (with the
    /// label), its stdin `stdin` or the file `input`; the install result
    /// line, if one came.
    async fn stream_cmd(&self, cmd: &str, stdin: Option<Vec<u8>>, input: Option<&std::path::Path>) -> Result<Option<String>> {
        let mut attempt = 1;
        loop {
            let (status, result, output) = self.stream_once(cmd, stdin.as_deref(), input).await?;
            if status.success() {
                return Ok(result);
            }
            if retry_connection(status.code(), output, &mut attempt).await {
                continue;
            }
            bail!("{} on {} (exit {})", if status.code() == Some(255) { "ssh failed" } else { "the command failed" }, self.ssh.display(), status.code().unwrap_or(-1));
        }
    }

    async fn stream_once(&self, cmd: &str, stdin: Option<&[u8]>, input: Option<&std::path::Path>) -> Result<(std::process::ExitStatus, Option<String>, bool)> {
        let mut c = self.command(cmd);
        match input {
            Some(path) => c.stdin(Stdio::from(std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?)),
            None if stdin.is_some() => c.stdin(Stdio::piped()),
            None => c.stdin(Stdio::null()),
        };
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn().context("cannot run ssh")?;
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let _ = pipe.write_all(bytes).await;
            drop(pipe);
        }
        let prefix = self.label.as_ref().map(|l| format!("[{l}] ")).unwrap_or_default();
        let out = child.stdout.take().expect("piped");
        let err = child.stderr.take().expect("piped");
        let p1 = prefix.clone();
        let out_task = tokio::spawn(async move {
            let mut result = None;
            let mut any = false;
            let mut lines = tokio::io::BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                any = true;
                let line = line.trim_end_matches('\r');
                if let Some(json) = line.strip_prefix(RESULT_MARKER) {
                    result = Some(json.trim().to_string());
                } else {
                    println!("{p1}{line}");
                }
            }
            (result, any)
        });
        let err_task = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("{prefix}{}", line.trim_end_matches('\r'));
            }
        });
        let status = child.wait().await?;
        let (result, any) = out_task.await.unwrap_or((None, false));
        let _ = err_task.await;
        Ok((status, result, any))
    }

    async fn root_stream(&self, script: &str, args: &[String]) -> Result<Option<String>> {
        let (cmd, stdin) = self.as_root(script, args);
        self.stream_cmd(&cmd, stdin.map(String::into_bytes), None).await
    }

    fn say(&self, msg: &str) {
        match &self.label {
            Some(l) => println!("[{l}] {msg}"),
            None => println!("{msg}"),
        }
    }
}

/// `litek8s install <role> --ssh ...`: what the installer needs besides the
/// flags for the service.
#[derive(Debug, Clone, Default)]
pub struct RemoteInstall {
    pub ssh: Ssh,
    pub role: Option<Role>,
    pub args: Vec<String>,
    pub token: Option<String>,
    pub binary: Option<PathBuf>,
    pub release: Option<String>,
    pub skip_checks: bool,
    pub timeout: Option<Duration>,
    pub label: Option<String>,
    /// A registry configuration to install (`registries::bundle`).
    pub registries: Option<String>,
}

/// Whether the service will run pods with crun (so crun and iptables have
/// to be there).
fn uses_crun(role: Role, args: &[String]) -> bool {
    let mut runtime = None;
    let mut no_node = false;
    for (i, a) in args.iter().enumerate() {
        if a == "--runtime" {
            runtime = args.get(i + 1).map(String::as_str);
        } else if let Some(v) = a.strip_prefix("--runtime=") {
            runtime = Some(v);
        } else if a == "--no-node" {
            no_node = true;
        }
    }
    !(role == Role::Server && no_node) && runtime != Some("docker")
}

/// What follows `litek8s install` on the remote command line.
pub fn install_args(role: Role, args: &[String], skip_checks: bool, timeout: Option<Duration>) -> Vec<String> {
    let mut out = Vec::new();
    if skip_checks {
        out.push("--skip-checks".into());
    }
    if let Some(t) = timeout {
        out.extend(["--timeout".to_string(), format!("{}s", t.as_secs().max(1))]);
    }
    out.push(match role {
        Role::Server => "server".into(),
        Role::Agent => "agent".into(),
    });
    out.extend(args.iter().cloned());
    out
}

/// Install litek8s as a service on a connected machine.
pub async fn install_on(remote: &Remote, o: &RemoteInstall, local: &LocalBinary) -> Result<InstallResult> {
    let role = o.role.context("no role")?;
    let probe = &remote.probe;
    let arch = probe.arch.clone().map_err(anyhow::Error::msg).with_context(|| format!("cannot install on {}", remote.display()))?;
    if !probe.systemd {
        bail!("{} does not run systemd: run `litek8s apiserver` / `litek8s agent` under its init system yourself", remote.display());
    }
    let other = match role {
        Role::Server => probe.agent_installed,
        Role::Agent => probe.server_installed,
    };
    if other {
        bail!("{} already runs litek8s as {}; `litek8s uninstall --ssh {}` it first", remote.display(), if role == Role::Server { "an agent" } else { "a server" }, remote.ssh.dest);
    }
    let source = choose_binary(local, probe, arch)?;
    let dir = remote.capture(r#"umask 077; mktemp -d "${HOME:-/tmp}/.litek8s-install.XXXXXX""#, &[], None).await?.trim().to_string();
    if dir.is_empty() {
        bail!("mktemp on {} printed nothing", remote.display());
    }
    let staged = format!("{dir}/litek8s");
    let bin = match &source {
        BinarySource::Installed => {
            remote.say(&format!("litek8s {} is already installed", probe.version.as_deref().unwrap_or("?")));
            crate::install::BIN.to_string()
        }
        BinarySource::Upload(path) => {
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            remote.say(&format!("uploading {} ({} MiB)", path.display(), size / (1 << 20)));
            let cmd = sh_command(r#"cat > "$1" && chmod 755 "$1""#, std::slice::from_ref(&staged));
            remote.stream_cmd(&cmd, None, Some(path)).await?;
            staged
        }
        BinarySource::Download(tag) => {
            for tool in ["curl", "tar"] {
                if !probe.tools.contains(tool) {
                    bail!("{} has no {tool} to download litek8s {tag}: install it, or pass --binary with a {} build", remote.display(), arch.target());
                }
            }
            let archive = format!("litek8s-{tag}-{}.tar.gz", arch.target());
            let base = download_base(tag);
            remote.say(&format!("downloading {base}/{archive}"));
            remote.stream_cmd(&sh_command(DOWNLOAD, &[dir.clone(), base, archive]), None, None).await?;
            staged
        }
    };
    if let Some(token) = &o.token {
        remote.capture(r#"umask 077; cat > "$1/token""#, std::slice::from_ref(&dir), Some(token.clone())).await?;
    }
    let mut service_args = o.args.clone();
    if let Some(text) = &o.registries {
        remote.capture(r#"umask 077; cat > "$1/registries.yaml""#, std::slice::from_ref(&dir), Some(text.clone())).await?;
        service_args.extend(["--registries".to_string(), format!("{dir}/registries.yaml")]);
    }
    if uses_crun(role, &o.args) && !(probe.tools.contains("crun") && probe.tools.contains("iptables-restore")) {
        remote.root_stream(DEPS, &[]).await?;
    }
    remote.say(&format!("running litek8s install {} as root", if role == Role::Server { "server" } else { "agent" }));
    let mut args = vec![dir, bin];
    args.extend(install_args(role, &service_args, o.skip_checks, o.timeout));
    let script = format!("{RESULT_ENV}=1; export {RESULT_ENV}\n{RUN_INSTALL}");
    let line = remote.root_stream(&script, &args).await?;
    let Some(line) = line else {
        bail!("the install on {} reported no result: is its litek8s older than this one ({})?", remote.display(), crate::config::VERSION);
    };
    serde_json::from_str(&line).with_context(|| format!("unexpected install result from {}: {line}", remote.display()))
}

/// Uninstall litek8s from a connected machine.
pub async fn uninstall_on(remote: &Remote, purge: bool) -> Result<()> {
    if remote.probe.version.is_none() {
        if !remote.probe.server_installed && !remote.probe.agent_installed {
            remote.say("litek8s is not installed");
            return Ok(());
        }
        bail!("{} has a litek8s service but no {}", remote.display(), crate::install::BIN);
    }
    let mut args = vec![crate::install::BIN.to_string(), "uninstall".to_string()];
    if purge {
        args.push("--purge".into());
    }
    remote.root_stream(r#""$@""#, &args).await?;
    Ok(())
}

/// The flags of `litek8s install` that are for the SSH side, taken out of
/// the service's flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshFlags {
    pub ssh: Option<Ssh>,
    pub binary: Option<PathBuf>,
    pub release: Option<String>,
    pub context: Option<String>,
    pub api_address: Option<String>,
}

pub fn take_ssh_flags(args: &mut Vec<String>) -> Result<SshFlags> {
    let take = |args: &mut Vec<String>, names: &[&str]| -> Result<Vec<String>> {
        let mut values = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(name) = names.iter().find(|n| a == *n) {
                if i + 1 >= args.len() {
                    bail!("{name} needs a value");
                }
                values.push(args.remove(i + 1));
                args.remove(i);
            } else if let Some(v) = names.iter().find_map(|n| a.strip_prefix(n).and_then(|r| r.strip_prefix('='))) {
                values.push(v.to_string());
                args.remove(i);
            } else {
                i += 1;
            }
        }
        Ok(values)
    };
    let dest = take(args, &["--ssh"])?.pop();
    let identity = take(args, &["-i", "--identity-file"])?.pop();
    let port = take(args, &["--ssh-port"])?.pop();
    let options = take(args, &["--ssh-option"])?;
    let binary = take(args, &["--binary"])?.pop().map(PathBuf::from);
    let release = take(args, &["--release"])?.pop();
    let context = take(args, &["--context"])?.pop();
    let api_address = take(args, &["--api-address"])?.pop();
    let ssh = match dest {
        Some(dest) => Some(Ssh {
            dest,
            port: port.map(|p| p.parse().context("--ssh-port is not a port number")).transpose()?,
            identity_file: identity.map(PathBuf::from),
            options,
        }),
        None => {
            if identity.is_some() || port.is_some() || !options.is_empty() || binary.is_some() || release.is_some() || context.is_some() || api_address.is_some() {
                bail!("-i, --ssh-port, --ssh-option, --binary, --release, --context and --api-address go with --ssh user@host");
            }
            None
        }
    };
    Ok(SshFlags { ssh, binary, release, context, api_address })
}

/// The value of `--flag v` / `--flag=v` in `args`.
pub fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().enumerate().find_map(|(i, a)| {
        if a == name {
            args.get(i + 1).map(String::as_str)
        } else {
            a.strip_prefix(name).and_then(|r| r.strip_prefix('='))
        }
    })
}

/// The API server URL for clients on this machine: `api_address` (host or
/// host:port), else the server's --advertise-address, else the host ssh
/// connects to; the port of --listen unless given.
pub fn api_url(api_address: Option<&str>, server_args: &[String], ssh_host: &str) -> String {
    let port = flag_value(server_args, "--listen").and_then(|l| l.parse::<std::net::SocketAddr>().ok()).map_or(6443, |l| l.port());
    let host = api_address.or_else(|| flag_value(server_args, "--advertise-address")).unwrap_or(ssh_host);
    let host = host.trim_start_matches("https://").trim_end_matches('/');
    let bracketed = host.starts_with('[');
    let has_port = if bracketed { host.contains("]:") } else { host.matches(':').count() == 1 };
    if has_port {
        format!("https://{host}")
    } else if host.contains(':') && !bracketed {
        format!("https://[{host}]:{port}")
    } else {
        format!("https://{host}:{port}")
    }
}

/// The kubeconfig user's token for a server too old to hand out an admin
/// certificate (it took any token).
pub const NO_AUTH_TOKEN: &str = "litek8s-no-client-auth";

/// Add the server's context to this machine's kubeconfig.
pub fn write_context(context: &str, server_url: &str, result: &InstallResult) -> Result<PathBuf> {
    let path = crate::config::paths().kubeconfig;
    let entry = crate::dev::kubeconfig::ClusterEntry {
        server: server_url.to_string(),
        ca: result.ca.clone(),
        // The serving certificate names the in-cluster service, not the
        // addresses the server is reached at.
        tls_server_name: result.ca.as_ref().map(|_| crate::kube::join::SERVER_NAME.to_string()),
        client_cert: result.client_cert.clone().zip(result.client_key.clone()),
        // A server from before client authentication took any token
        // (kubectl asks for a username and password on an https server
        // without credentials).
        token: result.client_cert.is_none().then(|| NO_AUTH_TOKEN.to_string()),
    };
    crate::dev::kubeconfig::install_cluster(&path, context, &entry, true)?;
    Ok(path)
}

/// `litek8s install <role> --ssh user@host [flags]`.
pub async fn install_cmd(role: Role, mut args: Vec<String>, flags: SshFlags, skip_checks: bool, timeout: Duration) -> Result<()> {
    let ssh = flags.ssh.clone().context("no --ssh")?;
    let token = crate::install::take_flag(&mut args, "--token")?.or_else(|| std::env::var("LITEK8S_TOKEN").ok().filter(|t| !t.is_empty()));
    // Read here (with the files it names), installed there.
    let registries = crate::install::take_flag(&mut args, "--registries")?.map(|p| crate::registries::bundle(std::path::Path::new(&p))).transpose()?;
    if role == Role::Agent && token.is_none() {
        bail!("`litek8s install agent --ssh` needs --join https://<server>:6443 and --token <token> (printed by `litek8s install server`)");
    }
    let local = LocalBinary::detect(flags.binary.clone(), flags.release.clone())?;
    let remote = Remote::connect(&ssh, None, true).await?;
    let o = RemoteInstall { ssh: ssh.clone(), role: Some(role), args: args.clone(), token, binary: flags.binary, release: flags.release, skip_checks, timeout: Some(timeout), label: None, registries };
    let result = install_on(&remote, &o, &local).await?;
    if role == Role::Agent {
        println!("{} on {}", if result.changed { "installed" } else { "unchanged" }, ssh.display());
        return Ok(());
    }
    let host = ssh.resolved_host().await.unwrap_or_else(|| ssh.host().to_string());
    let url = api_url(flags.api_address.as_deref(), &args, &host);
    let context = flags.context.clone().unwrap_or_else(|| if result.node.is_empty() { remote.probe.hostname.clone() } else { result.node.clone() });
    let path = write_context(&context, &url, &result)?;
    println!("\nkubeconfig context {context} ({url}) is in {} and is the current one:\n  kubectl get nodes", path.display());
    if let (Some(join), Some(token)) = (&result.url, &result.token) {
        println!("\nJoin another machine as a node:\n  litek8s install agent --ssh <user@host> --join {join} --token {token}");
    }
    Ok(())
}

/// `litek8s uninstall --ssh user@host [--purge]`.
pub async fn uninstall_cmd(ssh: Ssh, purge: bool) -> Result<()> {
    let remote = Remote::connect(&ssh, None, true).await?;
    uninstall_on(&remote, purge).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn ssh_args_carry_port_key_and_options() {
        let ssh = Ssh { dest: "pi@10.0.0.2".into(), port: Some(2222), identity_file: Some("/k/id".into()), options: s(&["StrictHostKeyChecking=no"]) };
        let a = ssh.args(true);
        assert_eq!(&a[..6], &s(&["-p", "2222", "-i", "/k/id", "-o", "StrictHostKeyChecking=no"])[..]);
        assert!(a.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert_eq!(a.last().unwrap(), "pi@10.0.0.2");
        assert!(!Ssh::new("pi").args(false).iter().any(|x| x == "BatchMode=yes"));
        assert_eq!(ssh.host(), "10.0.0.2");
        assert_eq!(ssh.display(), "pi@10.0.0.2:2222");
        assert!(Ssh::new("-oProxyCommand=x").validate().is_err());
        assert!(Ssh::new("a b").validate().is_err());
        assert!(Ssh { options: s(&["Bad"]), ..Ssh::new("h") }.validate().is_err());
        assert!(Ssh::new("me@host").validate().is_ok());
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote("plain-1.2/x"), "plain-1.2/x");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("it's $x"), "'it'\\''s $x'");
        assert_eq!(sh_command("echo \"$1\"", &s(&["a b", "--x=1"])), "sh -c 'echo \"$1\"' sh 'a b' --x=1");
    }

    #[test]
    fn ssh_g_hostname() {
        assert_eq!(parse_ssh_g_hostname("user pi\nhostname 192.168.1.10\nport 22\n").as_deref(), Some("192.168.1.10"));
        assert_eq!(parse_ssh_g_hostname("port 22\n"), None);
    }

    #[test]
    fn platform_from_uname_and_elf() {
        assert_eq!(parse_uname("Linux x86_64\n").unwrap(), Arch::X86_64);
        assert_eq!(parse_uname("Linux aarch64").unwrap(), Arch::Aarch64);
        assert!(parse_uname("Darwin arm64").unwrap_err().to_string().contains("Darwin arm64"));
        assert!(parse_uname("Linux armv7l").is_err());
        assert!(parse_uname("").is_err());
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[18] = 0xb7;
        assert_eq!(elf_arch(&elf), Some(Arch::Aarch64));
        elf[18] = 0x3e;
        assert_eq!(elf_arch(&elf), Some(Arch::X86_64));
        assert_eq!(elf_arch(b"\xcf\xfa\xed\xfe macho"), None);
    }

    const PROBE_OUT: &str = "uname=Linux aarch64\nuser=pi\nhostname=pi-1\nsudo=nopassword\nsystemd=1\n\
        version=litek8s 0.2.0\nsha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
        tool=curl\ntool=tar\ntool=crun\ninstalled=agent\nactive=agent\n";

    #[test]
    fn probe_parses() {
        let p = parse_probe(PROBE_OUT);
        assert_eq!(p.arch, Ok(Arch::Aarch64));
        assert_eq!(p.sudo, SudoMode::NoPassword);
        assert_eq!((p.user.as_str(), p.hostname.as_str()), ("pi", "pi-1"));
        assert!(p.systemd && p.agent_installed && p.agent_active && !p.server_installed && !p.server_active);
        assert_eq!(p.version.as_deref(), Some("0.2.0"));
        assert!(p.tools.contains("crun") && !p.tools.contains("iptables-restore"));
        let p = parse_probe("uname=Darwin arm64\r\nsudo=root\r\nversion=litek8s 0.2.0 DEV abc (LITEK8S_HOME=/x)\r\nsha256=short\n");
        assert!(p.arch.is_err());
        assert_eq!(p.sudo, SudoMode::Root);
        assert_eq!(p.version.as_deref(), Some("0.2.0 DEV abc"));
        assert_eq!(p.sha256, None);
        assert!(!p.systemd);
    }

    #[test]
    fn binary_choice() {
        let probe = parse_probe(PROBE_OUT);
        let sha = "a".repeat(64);
        let release = LocalBinary { version: "0.2.0".into(), ..Default::default() };
        // Same release there already.
        assert_eq!(choose_binary(&release, &probe, Arch::Aarch64).unwrap(), BinarySource::Installed);
        let newer = LocalBinary { version: "0.2.1".into(), ..Default::default() };
        assert_eq!(choose_binary(&newer, &probe, Arch::Aarch64).unwrap(), BinarySource::Download("v0.2.1".into()));
        // A dev build has no release to fetch.
        let dev = LocalBinary { version: "0.2.0 DEV abc".into(), ..Default::default() };
        let e = choose_binary(&dev, &probe, Arch::Aarch64).unwrap_err().to_string();
        assert!(e.contains("--binary") && e.contains("build-linux.sh arm64"), "{e}");
        assert_eq!(choose_binary(&LocalBinary { release: Some("v0.1.2".into()), ..dev.clone() }, &probe, Arch::Aarch64).unwrap(), BinarySource::Download("v0.1.2".into()));
        assert_eq!(choose_binary(&LocalBinary { release: Some("v0.2.0".into()), ..dev.clone() }, &probe, Arch::Aarch64).unwrap(), BinarySource::Installed);
        // --binary: uploaded unless identical; must match the arch.
        let given = LocalBinary { path: Some("/b".into()), path_arch: Some(Arch::Aarch64), path_sha256: Some("b".repeat(64)), ..dev.clone() };
        assert_eq!(choose_binary(&given, &probe, Arch::Aarch64).unwrap(), BinarySource::Upload("/b".into()));
        assert_eq!(choose_binary(&LocalBinary { path_sha256: Some(sha.clone()), ..given.clone() }, &probe, Arch::Aarch64).unwrap(), BinarySource::Installed);
        assert!(choose_binary(&given, &probe, Arch::X86_64).unwrap_err().to_string().contains("linux-aarch64 build"));
        assert!(choose_binary(&LocalBinary { path_arch: None, ..given }, &probe, Arch::Aarch64).is_err());
        // A static Linux build of the same arch uploads itself.
        let musl = LocalBinary { exe: Some(("/me".into(), Arch::Aarch64, "c".repeat(64))), ..dev.clone() };
        assert_eq!(choose_binary(&musl, &probe, Arch::Aarch64).unwrap(), BinarySource::Upload("/me".into()));
        assert!(choose_binary(&musl, &probe, Arch::X86_64).is_err());
    }

    #[test]
    fn ssh_flags_come_out_of_the_service_flags() {
        let mut args = s(&["--cluster-init", "--ssh", "pi@h", "-i", "~/.ssh/k", "--ssh-port=2200", "--ssh-option", "A=b", "--binary", "/b", "--node-ip", "1.2.3.4", "--context=home"]);
        let f = take_ssh_flags(&mut args).unwrap();
        assert_eq!(args, s(&["--cluster-init", "--node-ip", "1.2.3.4"]));
        let ssh = f.ssh.unwrap();
        assert_eq!((ssh.dest.as_str(), ssh.port, ssh.options.clone()), ("pi@h", Some(2200), s(&["A=b"])));
        assert_eq!(ssh.identity_file, Some(PathBuf::from("~/.ssh/k")));
        assert_eq!(f.binary, Some(PathBuf::from("/b")));
        assert_eq!(f.context.as_deref(), Some("home"));
        let mut plain = s(&["--cluster-init"]);
        assert_eq!(take_ssh_flags(&mut plain).unwrap(), SshFlags::default());
        assert!(take_ssh_flags(&mut s(&["--binary", "/b"])).is_err());
        assert!(take_ssh_flags(&mut s(&["--ssh"])).is_err());
    }

    #[test]
    fn remote_install_command() {
        assert_eq!(
            install_args(Role::Agent, &s(&["--join", "https://a:6443"]), true, Some(Duration::from_secs(180))),
            s(&["--skip-checks", "--timeout", "180s", "agent", "--join", "https://a:6443"])
        );
        assert!(uses_crun(Role::Agent, &[]));
        assert!(!uses_crun(Role::Agent, &s(&["--runtime", "docker"])));
        assert!(!uses_crun(Role::Server, &s(&["--no-node"])));
        assert!(uses_crun(Role::Server, &s(&["--runtime=crun"])));
    }

    #[test]
    fn api_url_from_flags_and_ssh() {
        assert_eq!(api_url(None, &[], "192.168.1.10"), "https://192.168.1.10:6443");
        assert_eq!(api_url(None, &s(&["--listen", "0.0.0.0:7443"]), "pi"), "https://pi:7443");
        assert_eq!(api_url(None, &s(&["--advertise-address=k8s.home"]), "pi"), "https://k8s.home:6443");
        assert_eq!(api_url(Some("127.0.0.1:16443"), &s(&["--advertise-address=k8s.home"]), "pi"), "https://127.0.0.1:16443");
        assert_eq!(api_url(None, &[], "fd00::1"), "https://[fd00::1]:6443");
    }
}
