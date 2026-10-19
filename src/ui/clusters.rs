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
    let others = crate::dev::kubeconfig::contexts(&crate::dev::kubeconfig::default_path()).into_iter().filter(|(name, _)| !ours.contains(&name.as_str())).collect();
    let env = crate::dev::env_state();
    ClustersSample {
        local,
        others,
        env_memory: env.as_ref().and_then(crate::dev::env_memory),
        env_cpus: env.as_ref().and_then(|e| e.cpus),
        env_memory_mib: env.as_ref().and_then(|e| e.memory_mib),
        env_mode: env.map(|e| e.mode),
    }
}
