//! NetworkPolicy enforcement on a node (crun and Docker runtimes).
//!
//! The API server computes, per pod of this node, whether it is isolated
//! and what is allowed ([`PodRules`], see `crate::kube::netpol`), and pushes
//! changes over the agent connection (`Message::NetPolicy`), always before
//! the pod itself. Here they become one filter chain per isolated pod
//! address, behind a dispatcher chain `LITEK8S-NETPOL`:
//!
//! - `LITEK8S-FORWARD` (the bridge's forwarding chain) jumps to it first:
//!   traffic between pods of this node (bridged, seen by iptables through
//!   `br_netfilter`), from other nodes over the WireGuard overlay, and to
//!   anything outside. Service traffic is filtered after its DNAT, so the
//!   real backend is matched, and before the masquerade, so the real
//!   source is.
//! - `INPUT -i litek8s0` jumps to it too: pods talking to the node itself
//!   (the cluster DNS and the API server when they run here).
//!
//! The dispatcher sends `-d <pod>` to the pod's ingress chain and `-s
//! <pod>` to its egress chain; an allowed packet RETURNs (so a pod-to-pod
//! packet on one node passes both), anything else is dropped. Replies
//! (conntrack ESTABLISHED/RELATED) always pass. Traffic from the node to
//! its pods (probes, NodePort relays) goes through OUTPUT and is allowed.
//!
//! With the Docker runtime ([`Target::Docker`]) the same chains go into
//! Docker's host network namespace (the Linux VM of Docker Desktop or
//! OrbStack), loaded by the privileged helper container that also holds
//! the service NAT rules: the dispatcher of each node (several agents may
//! share one Docker) is hooked first into `FORWARD` (pods of the Docker
//! bridge talking to each other through `br_netfilter`, and to anything
//! outside, after the service DNAT) and `INPUT` (pods talking to the VM
//! itself). A Docker host that cannot filter bridged traffic is an error:
//! isolated pods do not start rather than run unprotected. Probes,
//! port-forwards and NodePort relays run inside the pod's network
//! namespace there, so they are never filtered.
//!
//! Ordering: a pod's rules arrive before the pod, and the pod worker
//! registers the sandbox address with [`NetPol::register`], which loads the
//! chains before any container starts. The kernel keeps the chains while
//! the agent restarts; they are only rewritten once the server has sent
//! the full set again.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

/// What the agent announces when it enforces policies.
pub(crate) const FEATURE: &str = "network-policy";
pub(crate) const CHAIN: &str = "LITEK8S-NETPOL";
const INGRESS_PREFIX: &str = "LITEK8S-NPI-";
const EGRESS_PREFIX: &str = "LITEK8S-NPE-";

/// Runs `sh -c <script>` with the given stdin where the rules go, returning
/// its stdout; fails when the script does.
pub type Shell = Arc<dyn Fn(String, Option<String>) -> BoxFuture<'static, Result<String>> + Send + Sync>;

/// Where a node's pod traffic is filtered.
pub enum Target {
    /// This machine's iptables (crun): the `litek8s0` bridge.
    Node,
    /// Docker's host network namespace, reached through `shell`.
    Docker { node: String, shell: Shell },
}

/// The chain names of one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Names {
    dispatcher: String,
    ingress: String,
    egress: String,
}

impl Names {
    fn node() -> Self {
        Self { dispatcher: CHAIN.into(), ingress: INGRESS_PREFIX.into(), egress: EGRESS_PREFIX.into() }
    }

    /// Per node, as one Docker may run the pods of several agents (and
    /// unlike the crun names, so the two never clean up each other's).
    fn docker(node: &str) -> Self {
        let h: String = Sha256::digest(node.as_bytes()).iter().take(3).map(|b| format!("{b:02x}")).collect();
        Self { dispatcher: format!("{CHAIN}-{h}"), ingress: format!("LK8S-I{h}-"), egress: format!("LK8S-E{h}-") }
    }
}

/// The rules of one pod: `None` in a direction is "not isolated".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodRules {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<Vec<Allow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<Vec<Allow>>,
}

/// Traffic one rule allows: from (ingress) or to (egress) `peers`, on
/// `ports`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Allow {
    /// CIDRs; `None`: any address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peers: Option<Vec<String>>,
    /// Also the cluster DNS server (egress), whose address the agent knows.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dns: bool,
    /// Empty: every port and protocol.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PortRange {
    /// `tcp`, `udp` or `sctp`.
    pub protocol: String,
    /// First and last port; `None`: every port of the protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<(u16, u16)>,
}

fn chain_name(prefix: &str, ip: Ipv4Addr) -> String {
    format!("{prefix}{:08x}", u32::from(ip))
}

/// One rule line's match on ports.
fn port_match(p: &PortRange) -> String {
    let proto = match p.protocol.as_str() {
        "udp" => "udp",
        "sctp" => "sctp",
        _ => "tcp",
    };
    match p.port {
        None => format!(" -p {proto}"),
        Some((a, b)) if a == b => format!(" -p {proto} -m {proto} --dport {a}"),
        Some((a, b)) => format!(" -p {proto} -m {proto} --dport {a}:{b}"),
    }
}

fn render_chain(out: &mut String, chain: &str, allows: &[Allow], peer_flag: &str, dns: &[Ipv4Addr]) {
    let _ = writeln!(out, "-A {chain} -m conntrack --ctstate RELATED,ESTABLISHED -j RETURN");
    for a in allows {
        let peers: Vec<Option<String>> = match &a.peers {
            None => vec![None],
            Some(list) => {
                let mut v: Vec<Option<String>> = list.iter().cloned().map(Some).collect();
                if a.dns {
                    v.extend(dns.iter().map(|ip| Some(format!("{ip}/32"))));
                }
                v
            }
        };
        let ports: Vec<String> = if a.ports.is_empty() { vec![String::new()] } else { a.ports.iter().map(port_match).collect() };
        for peer in &peers {
            let peer = peer.as_ref().map(|p| format!(" {peer_flag} {p}")).unwrap_or_default();
            for port in &ports {
                let _ = writeln!(out, "-A {chain}{peer}{port} -j RETURN");
            }
        }
    }
    let _ = writeln!(out, "-A {chain} -j DROP");
}

/// The `iptables-restore --noflush` input for the isolated pods of this
/// node (address → rules), the cluster DNS addresses, and the per-pod
/// chains currently in the kernel (those no longer wanted are deleted).
pub(crate) fn render(names: &Names, pods: &BTreeMap<Ipv4Addr, PodRules>, dns: &[Ipv4Addr], existing: &[String]) -> String {
    let dispatcher = &names.dispatcher;
    let mut decl = format!("*filter\n:{dispatcher} - [0:0]\n");
    let mut rules = String::new();
    let mut wanted = Vec::new();
    for (ip, r) in pods {
        for (prefix, allows, flag, dir) in [(&names.ingress, &r.ingress, "-s", "-d"), (&names.egress, &r.egress, "-d", "-s")] {
            let Some(allows) = allows else { continue };
            let chain = chain_name(prefix, *ip);
            let _ = writeln!(decl, ":{chain} - [0:0]");
            let _ = writeln!(rules, "-A {dispatcher} {dir} {ip}/32 -j {chain}");
            render_chain(&mut rules, &chain, allows, flag, dns);
            wanted.push(chain);
        }
    }
    let mut stale = String::new();
    for c in existing.iter().filter(|c| !wanted.contains(c)) {
        // Declared (so flushed) first, then deleted: the dispatcher no
        // longer refers to it.
        let _ = writeln!(decl, ":{c} - [0:0]");
        let _ = writeln!(stale, "-X {c}");
    }
    decl + &stale + &rules + "COMMIT\n"
}

/// `rules` without its SCTP allow rules.
fn without_sctp(rules: &str) -> String {
    rules.lines().filter(|l| !l.contains(" -p sctp")).map(|l| format!("{l}\n")).collect()
}

/// The per-pod chains in `iptables-save -t filter` output.
pub(crate) fn existing_chains(names: &Names, save: &str) -> Vec<String> {
    save.lines()
        .filter_map(|l| l.strip_prefix(':'))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|c| c.starts_with(&names.ingress) || c.starts_with(&names.egress))
        .map(str::to_string)
        .collect()
}

/// Created before the bridge's rules refer to it (declaring it in a
/// restore would flush it, opening isolated pods while the agent starts).
pub(crate) const CREATE_CHAIN: &str = "iptables -t filter -N LITEK8S-NETPOL 2>/dev/null; true";

/// The INPUT jump (pods talking to the node itself), added once.
const INPUT_HOOK: &str = "iptables -t filter -C INPUT -i litek8s0 -j LITEK8S-NETPOL 2>/dev/null || iptables -t filter -I INPUT 1 -i litek8s0 -j LITEK8S-NETPOL";

const BRIDGE_SYSCTL: &str = "/proc/sys/net/bridge/bridge-nf-call-iptables";

/// Docker's host: the filter table and whether bridged traffic reaches
/// iptables (turned on when it can be), first line `bridge-nf=<0|1|>`.
fn docker_probe() -> String {
    format!(
        "f={BRIDGE_SYSCTL}; [ -e $f ] || modprobe br_netfilter 2>/dev/null; \
         [ \"$(cat $f 2>/dev/null)\" = 1 ] || echo 1 > $f 2>/dev/null; \
         echo \"bridge-nf=$(cat $f 2>/dev/null)\"; iptables-save -t filter"
    )
}

/// Docker's host: the dispatcher, hooked first into FORWARD and INPUT
/// (checked on every load: Docker restarting rebuilds FORWARD), then the
/// rules from stdin.
fn docker_load(names: &Names) -> String {
    format!(
        "iptables -t filter -N {d} 2>/dev/null; for c in FORWARD INPUT; do \
         iptables -t filter -C $c -j {d} 2>/dev/null || iptables -t filter -I $c 1 -j {d} || exit 1; done; \
         exec iptables-restore --noflush",
        d = names.dispatcher
    )
}

/// The `bridge-nf=` line of [`docker_probe`]'s output: whether bridged
/// traffic is filtered.
fn bridge_filtered(probe: &str) -> bool {
    probe.lines().next().and_then(|l| l.strip_prefix("bridge-nf=")).is_some_and(|v| v.trim() == "1")
}

#[derive(Default)]
struct State {
    /// pod uid → rules (only isolated pods).
    rules: HashMap<String, PodRules>,
    /// pod uid → sandbox address on this node.
    ips: HashMap<String, Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    /// The server sent the full set (or said it has none): until then the
    /// chains in the kernel (from before a restart) stay as they are.
    synced: bool,
    warned_bridge: bool,
}

pub(crate) struct NetPol {
    state: Mutex<State>,
    /// Serializes loads, so they are applied in order.
    apply_lock: tokio::sync::Mutex<()>,
    changed: Notify,
    names: Names,
    /// `None`: this machine's iptables.
    docker: Option<Shell>,
}

impl Default for NetPol {
    fn default() -> Self {
        Self::new(Target::Node)
    }
}

impl NetPol {
    pub(crate) fn new(target: Target) -> Self {
        let (names, docker) = match target {
            Target::Node => (Names::node(), None),
            Target::Docker { node, shell } => (Names::docker(&node), Some(shell)),
        };
        Self { state: Mutex::default(), apply_lock: tokio::sync::Mutex::new(()), changed: Notify::new(), names, docker }
    }

    /// Changes from the server: `full` replaces every pod's rules; `None`
    /// removes a pod's (no longer isolated).
    pub(crate) fn update(&self, pods: BTreeMap<String, Option<PodRules>>, full: bool) {
        let mut st = self.state.lock().unwrap();
        if full {
            st.rules.clear();
            st.synced = true;
        }
        for (uid, r) in pods {
            match r {
                Some(r) => st.rules.insert(uid, r),
                None => st.rules.remove(&uid),
            };
        }
        drop(st);
        self.changed.notify_one();
    }

    /// The server does not send policies (an older one): nothing is isolated.
    pub(crate) fn unsupported(&self) {
        self.update(BTreeMap::new(), true);
    }

    pub(crate) fn set_dns(&self, dns: Vec<Ipv4Addr>) {
        let mut st = self.state.lock().unwrap();
        if st.dns != dns {
            st.dns = dns;
            drop(st);
            self.changed.notify_one();
        }
    }

    /// A pod's sandbox has `ip`: its chains are loaded before this returns
    /// (so before its containers start).
    pub(crate) async fn register(&self, uid: &str, ip: &str) -> Result<()> {
        let ip: Ipv4Addr = ip.parse().context("pod address")?;
        let isolated = {
            let mut st = self.state.lock().unwrap();
            // An address once held by a pod that is gone.
            st.ips.retain(|u, a| *a != ip || u == uid);
            st.ips.insert(uid.to_string(), ip);
            st.rules.contains_key(uid)
        };
        if isolated { self.apply().await } else { Ok(()) }
    }

    pub(crate) fn unregister(&self, uid: &str) {
        let had = self.state.lock().unwrap().ips.remove(uid).is_some();
        if had {
            self.changed.notify_one();
        }
    }

    /// Load the rules of every registered pod.
    async fn apply(&self) -> Result<()> {
        let _held = self.apply_lock.lock().await;
        let (pods, dns, any) = {
            let st = self.state.lock().unwrap();
            if !st.synced {
                return Ok(());
            }
            let pods: BTreeMap<Ipv4Addr, PodRules> = st.ips.iter().filter_map(|(uid, ip)| Some((*ip, st.rules.get(uid)?.clone()))).collect();
            let any = !pods.is_empty();
            (pods, st.dns.clone(), any)
        };
        if let Some(shell) = self.docker.clone() {
            return self.apply_docker(&shell, &pods, &dns).await;
        }
        if any {
            self.ensure_bridge_filtering();
        }
        let save = output("iptables-save", &["-t", "filter"]).await.unwrap_or_default();
        let existing = existing_chains(&self.names, &save);
        if !any && existing.is_empty() && !save.contains(&format!("-A {CHAIN} ")) {
            return Ok(());
        }
        run("sh", &["-c", CREATE_CHAIN], None).await?;
        let rules = render(&self.names, &pods, &dns, &existing);
        if let Err(e) = run("iptables-restore", &["--noflush"], Some(&rules)).await {
            // A kernel without xt_sctp: those rules allow nothing rather
            // than keep every pod of the node from starting.
            if !rules.contains("-p sctp") {
                return Err(e);
            }
            eprintln!("node-agent: network policies: SCTP rules left out ({e:#})");
            run("iptables-restore", &["--noflush"], Some(&without_sctp(&rules))).await?;
        }
        run("sh", &["-c", INPUT_HOOK], None).await
    }

    /// [`NetPol::apply`] in Docker's host network namespace.
    async fn apply_docker(&self, shell: &Shell, pods: &BTreeMap<Ipv4Addr, PodRules>, dns: &[Ipv4Addr]) -> Result<()> {
        let save = shell(docker_probe(), None).await.context("Docker's host network namespace")?;
        let existing = existing_chains(&self.names, &save);
        let any = !pods.is_empty();
        if !any && existing.is_empty() && !save.contains(&format!("-A {} ", self.names.dispatcher)) {
            return Ok(());
        }
        if any && !bridge_filtered(&save) {
            // Pods on the Docker bridge would reach each other unfiltered.
            bail!("Docker's host does not pass bridged traffic through iptables (br_netfilter, sysctl net.bridge.bridge-nf-call-iptables=1)");
        }
        let rules = render(&self.names, pods, dns, &existing);
        if let Err(e) = shell(docker_load(&self.names), Some(rules.clone())).await {
            if !rules.contains("-p sctp") {
                return Err(e);
            }
            eprintln!("node-agent: network policies: SCTP rules left out ({e:#})");
            shell(docker_load(&self.names), Some(without_sctp(&rules))).await?;
        }
        Ok(())
    }

    /// Traffic between pods on one bridge only reaches iptables with
    /// `br_netfilter` and `bridge-nf-call-iptables` on (as kube-proxy and
    /// k3s need too).
    fn ensure_bridge_filtering(&self) {
        let sysctl = std::path::Path::new("/proc/sys/net/bridge/bridge-nf-call-iptables");
        if !sysctl.exists() {
            let _ = std::process::Command::new("modprobe").arg("br_netfilter").stderr(std::process::Stdio::null()).status();
        }
        // Pods started by an agent that did not set hairpin mode on their
        // bridge port (see `crun::sys::bridge_attach`).
        for port in std::fs::read_dir("/sys/class/net/litek8s0/brif").into_iter().flatten().flatten() {
            let mode = port.path().join("hairpin_mode");
            if std::fs::read_to_string(&mode).is_ok_and(|v| v.trim() != "1") {
                let _ = std::fs::write(&mode, "1");
            }
        }
        let ok = std::fs::read_to_string(sysctl).is_ok_and(|v| v.trim() == "1") || std::fs::write(sysctl, "1").is_ok();
        let mut st = self.state.lock().unwrap();
        if !ok && !st.warned_bridge {
            st.warned_bridge = true;
            eprintln!("node-agent: br_netfilter is not available (modprobe br_netfilter; sysctl net.bridge.bridge-nf-call-iptables=1): network policies do not filter traffic between pods of this node");
        }
    }

    /// Apply changes as they come (batched).
    pub(crate) async fn run(self: std::sync::Arc<Self>) {
        loop {
            self.changed.notified().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Err(e) = self.apply().await {
                eprintln!("node-agent: network policies not applied: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                self.changed.notify_one();
            }
        }
    }
}

async fn output(program: &str, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new(program).args(args).output().await.with_context(|| format!("failed to run {program}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

async fn run(program: &str, args: &[&str], stdin: Option<&str>) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {program}"))?;
    if let Some(input) = stdin {
        child.stdin.take().context("no stdin")?.write_all(input.as_bytes()).await?;
    } else {
        drop(child.stdin.take());
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        anyhow::bail!("{program}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(p: u16) -> PortRange {
        PortRange { protocol: "tcp".into(), port: Some((p, p)) }
    }

    #[test]
    fn renders_dispatcher_and_pod_chains() {
        let mut pods = BTreeMap::new();
        pods.insert(
            "10.42.0.5".parse().unwrap(),
            PodRules {
                ingress: Some(vec![Allow { peers: Some(vec!["10.42.1.7/32".into(), "10.42.0.9/32".into()]), dns: false, ports: vec![tcp(27017)] }]),
                egress: None,
            },
        );
        pods.insert(
            "10.42.0.6".parse().unwrap(),
            PodRules {
                ingress: Some(vec![]),
                egress: Some(vec![
                    Allow { peers: Some(vec![]), dns: true, ports: vec![PortRange { protocol: "udp".into(), port: Some((53, 53)) }] },
                    Allow { peers: None, dns: false, ports: vec![PortRange { protocol: "tcp".into(), port: Some((8000, 8100)) }, PortRange { protocol: "SCTP".into(), port: None }] },
                    Allow { peers: Some(vec!["0.0.0.0/0".into()]), dns: false, ports: vec![] },
                ]),
            },
        );
        let out = render(&Names::node(), &pods, &["192.168.1.10".parse().unwrap()], &["LITEK8S-NPI-0a2a0063".into(), "LITEK8S-NPI-0a2a0005".into()]);
        let expected = "*filter
:LITEK8S-NETPOL - [0:0]
:LITEK8S-NPI-0a2a0005 - [0:0]
:LITEK8S-NPI-0a2a0006 - [0:0]
:LITEK8S-NPE-0a2a0006 - [0:0]
:LITEK8S-NPI-0a2a0063 - [0:0]
-X LITEK8S-NPI-0a2a0063
-A LITEK8S-NETPOL -d 10.42.0.5/32 -j LITEK8S-NPI-0a2a0005
-A LITEK8S-NPI-0a2a0005 -m conntrack --ctstate RELATED,ESTABLISHED -j RETURN
-A LITEK8S-NPI-0a2a0005 -s 10.42.1.7/32 -p tcp -m tcp --dport 27017 -j RETURN
-A LITEK8S-NPI-0a2a0005 -s 10.42.0.9/32 -p tcp -m tcp --dport 27017 -j RETURN
-A LITEK8S-NPI-0a2a0005 -j DROP
-A LITEK8S-NETPOL -d 10.42.0.6/32 -j LITEK8S-NPI-0a2a0006
-A LITEK8S-NPI-0a2a0006 -m conntrack --ctstate RELATED,ESTABLISHED -j RETURN
-A LITEK8S-NPI-0a2a0006 -j DROP
-A LITEK8S-NETPOL -s 10.42.0.6/32 -j LITEK8S-NPE-0a2a0006
-A LITEK8S-NPE-0a2a0006 -m conntrack --ctstate RELATED,ESTABLISHED -j RETURN
-A LITEK8S-NPE-0a2a0006 -d 192.168.1.10/32 -p udp -m udp --dport 53 -j RETURN
-A LITEK8S-NPE-0a2a0006 -p tcp -m tcp --dport 8000:8100 -j RETURN
-A LITEK8S-NPE-0a2a0006 -p tcp -j RETURN
-A LITEK8S-NPE-0a2a0006 -d 0.0.0.0/0 -j RETURN
-A LITEK8S-NPE-0a2a0006 -j DROP
COMMIT
";
        assert_eq!(out, expected);
        // Nothing isolated: the dispatcher is emptied and old chains go.
        let out = render(&Names::node(), &BTreeMap::new(), &[], &["LITEK8S-NPE-0a2a0006".into()]);
        assert_eq!(out, "*filter\n:LITEK8S-NETPOL - [0:0]\n:LITEK8S-NPE-0a2a0006 - [0:0]\n-X LITEK8S-NPE-0a2a0006\nCOMMIT\n");
    }

    #[test]
    fn sctp_ports_and_chain_names() {
        assert_eq!(port_match(&PortRange { protocol: "sctp".into(), port: Some((9, 9)) }), " -p sctp -m sctp --dport 9");
        assert_eq!(chain_name(INGRESS_PREFIX, "10.42.255.254".parse().unwrap()), "LITEK8S-NPI-0a2afffe");
        assert!(chain_name(EGRESS_PREFIX, "255.255.255.255".parse().unwrap()).len() <= 28);
        assert_eq!(without_sctp("*filter\n-A X -p sctp -j RETURN\n-A X -j DROP\nCOMMIT\n"), "*filter\n-A X -j DROP\nCOMMIT\n");
    }

    #[test]
    fn finds_existing_pod_chains() {
        let save = "*filter\n:INPUT ACCEPT [0:0]\n:LITEK8S-FORWARD - [0:0]\n:LITEK8S-NETPOL - [0:0]\n:LITEK8S-NPI-0a2a0005 - [0:0]\n:LITEK8S-NPE-0a2a0006 - [0:0]\nCOMMIT\n";
        assert_eq!(existing_chains(&Names::node(), save), vec!["LITEK8S-NPI-0a2a0005", "LITEK8S-NPE-0a2a0006"]);
    }

    #[test]
    fn docker_chains_are_per_node() {
        let a = Names::docker("n1");
        let b = Names::docker("n2");
        assert_ne!(a, b);
        assert!(a.dispatcher.starts_with("LITEK8S-NETPOL-") && a.dispatcher.len() <= 28);
        let ip: Ipv4Addr = "10.42.255.254".parse().unwrap();
        let chain = chain_name(&a.ingress, ip);
        assert!(chain.len() <= 28, "{chain}");
        let mut pods = BTreeMap::new();
        pods.insert(ip, PodRules { ingress: Some(vec![]), egress: None });
        let out = render(&a, &pods, &[], &[]);
        assert!(out.contains(&format!("-A {} -d 10.42.255.254/32 -j {chain}\n", a.dispatcher)), "{out}");
        assert!(!out.contains(":LITEK8S-NETPOL - "), "{out}");
        // Each node finds only its own chains: another node's and crun's stay.
        let save = format!(":{chain} - [0:0]\n:{} - [0:0]\n:LITEK8S-NPI-0a2a0005 - [0:0]\n", chain_name(&b.egress, ip));
        assert_eq!(existing_chains(&a, &save), vec![chain.clone()]);
        assert_eq!(existing_chains(&Names::node(), &save), vec!["LITEK8S-NPI-0a2a0005"]);
    }

    #[test]
    fn docker_scripts() {
        let n = Names::docker("n1");
        let load = docker_load(&n);
        assert!(load.contains(&format!("iptables -t filter -C $c -j {d} 2>/dev/null || iptables -t filter -I $c 1 -j {d}", d = n.dispatcher)));
        assert!(load.ends_with("exec iptables-restore --noflush"));
        assert!(docker_probe().ends_with("iptables-save -t filter"));
        assert!(bridge_filtered("bridge-nf=1\n*filter\n"));
        assert!(!bridge_filtered("bridge-nf=0\n*filter\n"));
        assert!(!bridge_filtered("bridge-nf=\n*filter\n"));
        assert!(!bridge_filtered(""));
    }

    #[tokio::test]
    async fn docker_target_loads_through_the_shell() {
        type Calls = Arc<Mutex<Vec<(String, Option<String>)>>>;
        let calls: Calls = Arc::default();
        let filtered = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (c, f) = (calls.clone(), filtered.clone());
        let shell: Shell = Arc::new(move |script, stdin| {
            c.lock().unwrap().push((script, stdin));
            let on = f.load(std::sync::atomic::Ordering::Relaxed);
            Box::pin(async move { Ok(format!("bridge-nf={}\n*filter\nCOMMIT\n", if on { 1 } else { 0 })) })
        });
        let np = NetPol::new(Target::Docker { node: "n1".into(), shell });
        // Nothing isolated and nothing loaded before: only the probe.
        np.update(BTreeMap::new(), true);
        np.apply().await.unwrap();
        assert_eq!(calls.lock().unwrap().len(), 1);
        np.update(BTreeMap::from([("u1".to_string(), Some(PodRules { ingress: Some(vec![]), egress: None }))]), false);
        np.register("u1", "10.42.0.5").await.unwrap();
        let (script, rules) = calls.lock().unwrap().last().cloned().unwrap();
        assert_eq!(script, docker_load(&np.names));
        assert!(rules.unwrap().contains("-d 10.42.0.5/32"));
        // A Docker host that cannot filter bridged traffic: an error, not open pods.
        filtered.store(false, std::sync::atomic::Ordering::Relaxed);
        let err = np.register("u1", "10.42.0.5").await.unwrap_err();
        assert!(format!("{err:#}").contains("br_netfilter"), "{err:#}");
    }

    #[test]
    fn rules_wait_for_the_full_set() {
        let np = NetPol::default();
        let mut one = BTreeMap::new();
        one.insert("u1".to_string(), Some(PodRules { ingress: Some(vec![]), egress: None }));
        np.update(one.clone(), false);
        assert!(!np.state.lock().unwrap().synced);
        np.update(one, true);
        let mut rm = BTreeMap::new();
        rm.insert("u1".to_string(), None);
        np.update(rm, false);
        let st = np.state.lock().unwrap();
        assert!(st.synced && st.rules.is_empty());
    }

    #[test]
    fn wire_format() {
        let r = PodRules { ingress: Some(vec![Allow { peers: None, dns: false, ports: vec![tcp(80)] }]), egress: None };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"ingress":[{"ports":[{"protocol":"tcp","port":[80,80]}]}]}"#);
        assert_eq!(serde_json::from_str::<PodRules>(r#"{"ingress":[{"ports":[{"protocol":"tcp","port":[80,80]}]}]}"#).unwrap(), r);
    }
}
