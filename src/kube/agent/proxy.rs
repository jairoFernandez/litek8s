//! The node's service proxy (what kube-proxy does).
//!
//! The API server pushes every Service with its Endpoints over the agent
//! connection. From them the agent keeps, after a short debounce:
//!
//! - ClusterIPs: one NAT chain of DNAT rules (spread over the ready
//!   endpoints with `statistic`), loaded atomically with `iptables-restore`
//!   into the network namespace the pods route through. The runtime decides
//!   how (for Docker, a small privileged helper container on the host
//!   network, so it also works when Docker runs inside a VM).
//! - NodePorts: plain TCP listeners on the node that relay to an endpoint,
//!   so they answer on the node's own address wherever the runtime runs.
//!   When the node cannot reach the endpoint's address (Docker in a VM, see
//!   `probe.rs`), the relay goes through the pod's sandbox instead.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::runtime::{ExecSession, PodRuntime};

/// Connect to `ip:port` from inside the network namespace of the pod with
/// that address (set by the agent).
pub(crate) type InPodDial = Arc<dyn Fn(String, u16) -> futures_util::future::BoxFuture<'static, Option<ExecSession>> + Send + Sync>;

pub(crate) const CHAIN: &str = "LITEK8S-SERVICES";

#[derive(Default)]
pub(crate) struct Proxy {
    /// (namespace, name) → (Service, Endpoints)
    services: Mutex<BTreeMap<(String, String), (Value, Option<Value>)>>,
    changed: Notify,
    node_ports: Mutex<HashMap<u16, JoinHandle<()>>>,
    /// Listeners on the host's loopback when the node runs in the dev
    /// sandbox (host port → task), see [`crate::dev::broker`].
    host_ports: Mutex<HashMap<u16, JoinHandle<()>>>,
    pub(crate) in_pod: std::sync::OnceLock<InPodDial>,
    /// The node routes to every pod address (crun, also across nodes over
    /// the overlay): a slow connect is a slow backend, not a missing route.
    pub(crate) direct: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone, PartialEq)]
struct Backend {
    ip: String,
    port: u16,
}

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default()
}

/// Ready backends for one Service port, by its name or number.
fn backends(svc_port: &Value, endpoints: Option<&Value>) -> Vec<Backend> {
    let name = s(svc_port, "/name");
    let mut out = Vec::new();
    for subset in endpoints.and_then(|e| e.get("subsets")).and_then(Value::as_array).into_iter().flatten() {
        let ports = subset.get("ports").and_then(Value::as_array).cloned().unwrap_or_default();
        let port = ports
            .iter()
            .find(|p| s(p, "/name") == name)
            .or_else(|| (ports.len() == 1).then(|| &ports[0]))
            .and_then(|p| p.get("port").and_then(Value::as_u64));
        let Some(port) = port else { continue };
        for addr in subset.get("addresses").and_then(Value::as_array).into_iter().flatten() {
            if let Some(ip) = addr.get("ip").and_then(Value::as_str) {
                out.push(Backend { ip: ip.to_string(), port: port as u16 });
            }
        }
    }
    out
}

impl Proxy {
    pub(crate) fn update(&self, namespace: String, name: String, service: Option<Value>, endpoints: Option<Value>) {
        let mut services = self.services.lock().unwrap();
        match service {
            Some(svc) => {
                services.insert((namespace, name), (svc, endpoints));
            }
            None => {
                services.remove(&(namespace, name));
            }
        }
        drop(services);
        self.changed.notify_one();
    }

    /// The NAT table for the current Services.
    fn rules(&self) -> String {
        let mut out = format!("*nat\n:{CHAIN} - [0:0]\n");
        for (svc, eps) in self.services.lock().unwrap().values() {
            let ip = s(svc, "/spec/clusterIP");
            if ip.is_empty() || ip == "None" {
                continue;
            }
            for port in svc.pointer("/spec/ports").and_then(Value::as_array).into_iter().flatten() {
                let protocol = s(port, "/protocol").to_lowercase();
                let protocol = if protocol.is_empty() { "tcp".to_string() } else { protocol };
                if protocol != "tcp" && protocol != "udp" {
                    continue;
                }
                let Some(svc_port) = port.get("port").and_then(Value::as_u64) else { continue };
                let backends = backends(port, eps.as_ref());
                let n = backends.len();
                for (i, b) in backends.iter().enumerate() {
                    let _ = write!(out, "-A {CHAIN} -d {ip}/32 -p {protocol} -m {protocol} --dport {svc_port}");
                    if i + 1 < n {
                        let _ = write!(out, " -m statistic --mode random --probability {:.5}", 1.0 / (n - i) as f64);
                    }
                    let _ = writeln!(out, " -j DNAT --to-destination {}:{}", b.ip, b.port);
                }
            }
        }
        out.push_str("COMMIT\n");
        out
    }

    /// NodePort → backends, for the userspace listeners.
    fn node_port_backends(&self, node_port: u16) -> Vec<Backend> {
        for (svc, eps) in self.services.lock().unwrap().values() {
            for port in svc.pointer("/spec/ports").and_then(Value::as_array).into_iter().flatten() {
                if port.get("nodePort").and_then(Value::as_u64) == Some(node_port as u64) {
                    return backends(port, eps.as_ref());
                }
            }
        }
        Vec::new()
    }

    fn wanted_node_ports(&self) -> Vec<u16> {
        let mut ports = Vec::new();
        for (svc, _) in self.services.lock().unwrap().values() {
            if !matches!(s(svc, "/spec/type"), "NodePort" | "LoadBalancer") {
                continue;
            }
            for port in svc.pointer("/spec/ports").and_then(Value::as_array).into_iter().flatten() {
                let tcp = matches!(s(port, "/protocol"), "" | "TCP");
                if let (true, Some(np)) = (tcp, port.get("nodePort").and_then(Value::as_u64)) {
                    ports.push(np as u16);
                }
            }
        }
        ports
    }

    /// (host port, NodePort) pairs to expose on the host's loopback in the
    /// dev sandbox: every NodePort, and a LoadBalancer's own ports (as
    /// Docker Desktop's Kubernetes does on localhost).
    fn wanted_host_ports(&self) -> Vec<(u16, u16)> {
        let mut ports = Vec::new();
        for (svc, _) in self.services.lock().unwrap().values() {
            let kind = s(svc, "/spec/type");
            if !matches!(kind, "NodePort" | "LoadBalancer") {
                continue;
            }
            for port in svc.pointer("/spec/ports").and_then(Value::as_array).into_iter().flatten() {
                if !matches!(s(port, "/protocol"), "" | "TCP") {
                    continue;
                }
                let Some(np) = port.get("nodePort").and_then(Value::as_u64) else { continue };
                ports.push((np as u16, np as u16));
                if kind == "LoadBalancer" {
                    if let Some(p) = port.get("port").and_then(Value::as_u64).filter(|p| *p != np) {
                        ports.push((p as u16, np as u16));
                    }
                }
            }
        }
        ports.sort();
        ports.dedup_by_key(|(host, _)| *host);
        ports
    }
}

/// Keep the node's rules and listeners in line with the Services, forever.
pub(crate) async fn run<R: PodRuntime>(proxy: Arc<Proxy>, rt: Arc<R>) {
    let mut applied = String::new();
    loop {
        proxy.changed.notified().await;
        // Debounce: a rollout changes many endpoints at once.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let rules = proxy.rules();
        // Nothing to route yet: do not set anything up on the node.
        if applied.is_empty() && !rules.contains(" -j DNAT ") {
            sync_node_ports(&proxy);
            continue;
        }
        if rules != applied {
            match rt.apply_service_rules(&rules).await {
                Ok(()) => applied = rules,
                Err(e) => {
                    eprintln!("node-agent: service rules not applied: {e:#}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    proxy.changed.notify_one();
                }
            }
        }
        sync_node_ports(&proxy);
    }
}

fn sync_node_ports(proxy: &Arc<Proxy>) {
    let wanted = proxy.wanted_node_ports();
    let mut listeners = proxy.node_ports.lock().unwrap();
    listeners.retain(|port, task| {
        let keep = wanted.contains(port);
        if !keep {
            task.abort();
        }
        keep
    });
    for port in wanted {
        if listeners.contains_key(&port) {
            continue;
        }
        let proxy = proxy.clone();
        listeners.insert(port, tokio::spawn(async move { serve_node_port(proxy, port).await }));
    }
    drop(listeners);
    if crate::dev::broker::installed() {
        let wanted = proxy.wanted_host_ports();
        let mut host = proxy.host_ports.lock().unwrap();
        host.retain(|port, task| {
            let keep = wanted.iter().any(|(h, _)| h == port);
            if !keep {
                task.abort();
            }
            keep
        });
        for (host_port, node_port) in wanted {
            if host.contains_key(&host_port) {
                continue;
            }
            let proxy = proxy.clone();
            host.insert(
                host_port,
                tokio::spawn(async move {
                    let bound = tokio::task::spawn_blocking(move || crate::dev::broker::bind_host_port(host_port)).await;
                    if let Some(listener) = bound.ok().flatten().and_then(|l| TcpListener::from_std(l).ok()) {
                        relay_node_port(proxy, listener, node_port).await;
                    }
                }),
            );
        }
    }
}

async fn serve_node_port(proxy: Arc<Proxy>, port: u16) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => return eprintln!("node-agent: NodePort {port} unavailable: {e}"),
    };
    relay_node_port(proxy, listener, port).await
}

/// Accept on `listener` and relay each connection to a backend of
/// `port`'s Service.
async fn relay_node_port(proxy: Arc<Proxy>, listener: TcpListener, port: u16) {
    let mut next = 0usize;
    loop {
        let Ok((mut client, _)) = listener.accept().await else { continue };
        let backends = proxy.node_port_backends(port);
        if backends.is_empty() {
            continue;
        }
        let b = backends[next % backends.len()].clone();
        next = next.wrapping_add(1);
        let in_pod = proxy.in_pod.get().cloned();
        let direct = proxy.direct.load(std::sync::atomic::Ordering::Relaxed);
        tokio::spawn(async move {
            let _ = client.set_nodelay(true);
            let connect = async {
                if !direct && super::probe::is_unroutable(&b.ip) {
                    return Err(());
                }
                let timeout = Duration::from_secs(if direct { 10 } else { 1 });
                match tokio::time::timeout(timeout, TcpStream::connect((b.ip.as_str(), b.port))).await {
                    Ok(result) => Ok(result),
                    Err(_) => {
                        if !direct {
                            super::probe::mark_unroutable(&b.ip);
                        }
                        Err(())
                    }
                }
            };
            match connect.await {
                Ok(Ok(mut upstream)) => {
                    let _ = upstream.set_nodelay(true);
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {}
                _ => {
                    let Some(dial) = in_pod else { return };
                    if let Some(session) = dial(b.ip.clone(), b.port).await {
                        relay_exec(client, session).await;
                    }
                }
            }
        });
    }
}

/// Copy bytes between a client connection and an exec session's stdio.
async fn relay_exec(client: TcpStream, session: ExecSession) {
    use futures_util::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut read, mut write) = client.into_split();
    let mut input = session.input;
    let upload = async move {
        let mut buf = vec![0u8; 32 * 1024];
        while let Ok(n) = read.read(&mut buf).await {
            if n == 0 || input.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = input.shutdown().await;
    };
    let mut output = session.output;
    let download = async move {
        while let Some(Ok((channel, bytes))) = output.next().await {
            if channel != 2 && write.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = write.shutdown().await;
    };
    // The session ends when the pod side closes; the client may stop
    // sending long before that.
    let upload = tokio::spawn(upload);
    download.await;
    upload.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spreads_cluster_ips_over_ready_endpoints() {
        let proxy = Proxy::default();
        let svc = json!({"spec": {"clusterIP": "10.43.0.20", "type": "NodePort", "ports": [{"name": "http", "port": 80, "protocol": "TCP", "nodePort": 30080}]}});
        let eps = json!({"subsets": [{"addresses": [{"ip": "192.168.1.2"}, {"ip": "192.168.1.3"}], "ports": [{"name": "http", "port": 8080}]}]});
        proxy.update("ns".into(), "web".into(), Some(svc), Some(eps));
        let rules = proxy.rules();
        assert!(rules.contains("-d 10.43.0.20/32 -p tcp -m tcp --dport 80 -m statistic --mode random --probability 0.50000 -j DNAT --to-destination 192.168.1.2:8080"));
        assert!(rules.contains("-j DNAT --to-destination 192.168.1.3:8080\n"));
        assert!(rules.ends_with("COMMIT\n"));
        assert_eq!(proxy.wanted_node_ports(), vec![30080]);
        assert_eq!(proxy.node_port_backends(30080).len(), 2);
        proxy.update("ns".into(), "web".into(), None, None);
        assert!(!proxy.rules().contains("DNAT"));
    }
}
