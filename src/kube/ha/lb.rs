//! `litek8s lb`: one fixed address for an HA control plane.
//!
//! kubectl takes one server per cluster, so a kubeconfig cannot list all
//! three servers. This is a TCP load balancer like the one k3s runs on its
//! agents: it learns the servers from any of them
//! (`/apis/server.litek8s.dev/v1/servers`), checks each one's `/readyz`
//! every second, and passes each connection whole (plain HTTP or TLS) to a
//! ready server, preferring the one it used last so watches and long
//! requests stay put (with none ready, an election, to the last one anyway:
//! servers hold requests until there is a leader). A VIP (keepalived) or a DNS name with every server's
//! address work too; see docs/ha.md.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use super::authority;
use super::lincheck::http;

#[derive(Default)]
struct Pool {
    /// Server address → ready.
    servers: BTreeMap<String, bool>,
    last: Option<String>,
}

impl Pool {
    fn pick(&mut self) -> Option<String> {
        if let Some(last) = &self.last {
            if self.servers.get(last) == Some(&true) {
                return Some(last.clone());
            }
        }
        let ready: Vec<&String> = self.servers.iter().filter(|(_, r)| **r).map(|(s, _)| s).collect();
        // None ready (an election): any server still holds the request until
        // there is a leader, better than refusing the connection.
        let pick = match ready.first() {
            Some(s) => s.to_string(),
            None => return self.last.clone().or_else(|| self.servers.keys().next().cloned()),
        };
        self.last = Some(pick.clone());
        Some(pick)
    }
}

/// Run the load balancer on `listen`, starting from the `seeds` servers.
/// With `tcp_check`, a server is ready when it accepts a TCP connection
/// (servers whose `/readyz` needs credentials, e.g. k3s).
pub async fn run(listen: SocketAddr, seeds: Vec<String>, tcp_check: bool) -> Result<()> {
    let pool = Arc::new(Mutex::new(Pool::default()));
    for s in &seeds {
        pool.lock().unwrap().servers.insert(authority(s), false);
    }
    let listener = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("failed to listen on {listen}"))?;
    eprintln!("lb: listening on {listen}");
    tokio::spawn(health(pool.clone(), tcp_check));
    loop {
        let (client, peer) = listener.accept().await?;
        let _ = client.set_nodelay(true);
        let pool = pool.clone();
        tokio::spawn(async move {
            // A server that turns out unreachable is marked down; try the next.
            for _ in 0..3 {
                let Some(server) = pool.lock().unwrap().pick() else {
                    eprintln!("lb: no ready API server for {peer}");
                    return;
                };
                match tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(&server)).await {
                    Ok(Ok(upstream)) => {
                        let _ = upstream.set_nodelay(true);
                        let (mut a, mut b) = (client, upstream);
                        let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                        return;
                    }
                    _ => {
                        let mut p = pool.lock().unwrap();
                        p.servers.insert(server, false);
                        p.last = None;
                    }
                }
            }
        });
    }
}

/// Check every server's readiness each second; learn new servers from them.
async fn health(pool: Arc<Mutex<Pool>>, tcp_check: bool) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut round = 0u64;
    loop {
        tick.tick().await;
        let servers: Vec<String> = pool.lock().unwrap().servers.keys().cloned().collect();
        let checks = servers.iter().map(|s| async move {
            let ready = if tcp_check {
                matches!(tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(s)).await, Ok(Ok(_)))
            } else {
                matches!(http(s, "GET", "/readyz", None, Duration::from_secs(2)).await, Ok((200, _)))
            };
            (s.clone(), ready)
        });
        let results = futures_util::future::join_all(checks).await;
        let mut learned = Vec::new();
        if round % 10 == 0 && !tcp_check {
            for (s, ready) in &results {
                if !*ready {
                    continue;
                }
                if let Ok((200, v)) = http(s, "GET", "/apis/server.litek8s.dev/v1/servers", None, Duration::from_secs(2)).await {
                    learned.extend(v["servers"].as_array().into_iter().flatten().filter(|m| m["voter"] == true).filter_map(|m| m["addr"].as_str().map(str::to_string)));
                    break;
                }
            }
        }
        round += 1;
        let mut p = pool.lock().unwrap();
        for (s, ready) in results {
            let was = p.servers.insert(s.clone(), ready);
            if was != Some(ready) {
                eprintln!("lb: {s} is {}", if ready { "ready" } else { "not ready" });
            }
        }
        for s in learned {
            p.servers.entry(s).or_insert(false);
        }
    }
}

/// `litek8s server list`.
pub async fn list(server: &str) -> Result<()> {
    let addr = authority(server);
    let (code, v) = http(&addr, "GET", "/apis/server.litek8s.dev/v1/servers", None, Duration::from_secs(10)).await.map_err(anyhow::Error::msg)?;
    if code != 200 {
        anyhow::bail!("{addr} answered {code}: {}", v.get("message").and_then(Value::as_str).unwrap_or_default());
    }
    let last = v["last_log_index"].as_u64();
    println!("{:<20} {:<24} {:<8} {:<8} {}", "NAME", "ADDRESS", "ROLE", "VOTER", "LAG");
    for s in v["servers"].as_array().into_iter().flatten() {
        let role = if s["leader"] == true { "leader" } else { "follower" };
        let lag = match (last, s["matched"].as_u64()) {
            (Some(l), Some(m)) => l.saturating_sub(m).to_string(),
            _ => "-".into(),
        };
        println!("{:<20} {:<24} {:<8} {:<8} {}", s["name"].as_str().unwrap_or_default(), s["addr"].as_str().unwrap_or_default(), role, s["voter"], lag);
    }
    println!("term {}, applied {}", v["term"], v["applied"]);
    Ok(())
}

/// `litek8s server remove NAME`.
pub async fn remove(server: &str, token: &str, name: &str) -> Result<()> {
    let addr = authority(server);
    let token = crate::kube::join::Token::parse(token)?;
    let ca = crate::kube::join::fetch_ca(&addr, &token).await?;
    let peers = super::net::Peers::new(&ca, &token.secret)?;
    let (code, body) = peers
        .call(&addr, hyper::Method::DELETE, &format!("{}/servers/{name}", super::net::PREFIX), Default::default(), Duration::from_secs(60))
        .await?;
    if !code.is_success() {
        anyhow::bail!("{addr} answered {code}: {}", String::from_utf8_lossy(&body));
    }
    println!("server {name} removed; stop it and delete its data directory before it joins again");
    Ok(())
}
