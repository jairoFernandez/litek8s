//! The Clusters view's data: the local clusters (state, API port, nodes,
//! memory, age), the VM or sandbox they share, and the kubeconfig's other
//! contexts. Sampled every 2 s while the view is on screen and never
//! otherwise: a cluster nobody looks at is not polled.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use super::app::{ClusterRow, ClustersSample, DevStatus, Msg};

pub const EVERY: Duration = Duration::from_secs(2);

pub fn spawn(tx: UnboundedSender<Msg>, visible: Arc<AtomicBool>, wake: Arc<Notify>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if !visible.load(Ordering::Relaxed) {
                wake.notified().await;
                continue;
            }
            if tx.send(Msg::Clusters(Box::new(sample().await))).is_err() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(EVERY) => {}
                _ = wake.notified() => {}
            }
        }
    })
}

pub async fn sample() -> ClustersSample {
    let infos = crate::dev::clusters().await;
    // Ready nodes of the running ones, all at once.
    let ready = futures_util::future::join_all(infos.iter().map(|c| async move {
        match &c.running {
            Some(s) => Some(crate::dev::ready_nodes(s.port).await),
            None => None,
        }
    }))
    .await;
    let local: Vec<ClusterRow> = infos
        .iter()
        .zip(ready)
        .map(|(c, ready)| ClusterRow {
            name: c.name.clone(),
            context: c.context.clone(),
            status: match (&c.running, ready) {
                (None, _) => DevStatus::Down,
                (Some(_), Some(n)) if n >= c.nodes as usize => DevStatus::Up,
                (Some(_), _) => DevStatus::Starting,
            },
            port: c.running.as_ref().map(|s| s.port),
            nodes: c.nodes,
            ready_nodes: ready,
            memory: c.memory,
            age: c.created_unix_ms.map(crate::dev::age),
        })
        .collect();
    let ours: Vec<&str> = local.iter().map(|r| r.context.as_str()).collect();
    let others: Vec<(String, Option<String>)> = crate::dev::kubeconfig::contexts(&crate::dev::kubeconfig::default_path()).into_iter().filter(|(name, _)| !ours.contains(&name.as_str())).collect();
    let others_litek8s = identify(&others).await;
    let env = crate::dev::env_state();
    ClustersSample {
        local,
        others,
        others_litek8s,
        env_memory: env.as_ref().and_then(crate::dev::env_memory),
        env_cpus: env.as_ref().and_then(|e| e.cpus),
        env_memory_mib: env.as_ref().and_then(|e| e.memory_mib),
        env_mode: env.map(|e| e.mode),
    }
}

/// How long an answer about a context is kept: the view samples every 2 s,
/// the servers are asked about once a minute (an unreachable one sooner).
const KNOWN_FOR: Duration = Duration::from_secs(60);
const UNREACHABLE_FOR: Duration = Duration::from_secs(10);
const ASK_TIMEOUT: Duration = Duration::from_secs(2);

type Known = std::collections::HashMap<String, (Option<bool>, std::time::Instant)>;
static KNOWN: std::sync::Mutex<Option<Known>> = std::sync::Mutex::new(None);

/// Which other contexts are litek8s clusters (a server made by `litek8s
/// cluster apply`, lkcloud, or another machine's dev cluster): their
/// `/version` says `+litek8s`. Asked without credentials, so a context's
/// exec plugin (a cloud login) never runs just because the view is open.
async fn identify(others: &[(String, Option<String>)]) -> std::collections::BTreeMap<String, bool> {
    let now = std::time::Instant::now();
    let stale: Vec<String> = {
        let known = KNOWN.lock().unwrap();
        others
            .iter()
            .filter(|(name, _)| match known.as_ref().and_then(|k| k.get(name)) {
                Some((Some(_), at)) => now.duration_since(*at) >= KNOWN_FOR,
                Some((None, at)) => now.duration_since(*at) >= UNREACHABLE_FOR,
                None => true,
            })
            .map(|(name, _)| name.clone())
            .collect()
    };
    if !stale.is_empty() {
        let cfg = super::kube::auth::load().ok();
        let answers = futures_util::future::join_all(stale.iter().map(|name| {
            let ep = cfg.as_ref().and_then(|c| super::kube::endpoint_for(&c.doc, name, std::path::Path::new(".")).ok());
            async move {
                let Some(mut anon) = ep else { return None };
                anon.headers.clear();
                anon.creds = None;
                match tokio::time::timeout(ASK_TIMEOUT, anon.json(hyper::Method::GET, "/version", None)).await {
                    Ok(Ok(v)) => Some(crate::mcp::guard::is_litek8s_version(&v)),
                    // Answered, but wants credentials: litek8s has no client
                    // authentication, so it is not litek8s.
                    Ok(Err(e)) if is_auth_error(&e) => Some(false),
                    _ => None,
                }
            }
        }))
        .await;
        let mut known = KNOWN.lock().unwrap();
        let k = known.get_or_insert_with(Known::new);
        for (name, a) in stale.into_iter().zip(answers) {
            k.insert(name, (a, now));
        }
    }
    let known = KNOWN.lock().unwrap();
    others.iter().filter_map(|(name, _)| known.as_ref()?.get(name)?.0.map(|l| (name.clone(), l))).collect()
}

fn is_auth_error(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}").to_lowercase();
    s.contains("unauthorized") || s.contains("forbidden") || s.contains(" 401") || s.contains(" 403")
}
