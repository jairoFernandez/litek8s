//! Port-forwards: listeners on 127.0.0.1 relaying each connection to a
//! pod's port through `pods/<pod>/portforward` (SPDY, as kubectl), kept
//! while the UI runs; a service's or a workload's forward goes to one of
//! its ready pods.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use hyper::Method;
use tokio::sync::mpsc::UnboundedSender;

use super::state::{FwdInfo, KMsg};
use crate::ui::app::Msg;
use crate::ui::client::Endpoint;

struct Entry {
    info: FwdInfo,
    task: tokio::task::JoinHandle<()>,
    conns: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
}

#[derive(Default, Clone)]
pub struct Forwards {
    inner: Arc<Mutex<Vec<Entry>>>,
}

impl Forwards {
    pub fn list(&self) -> Vec<FwdInfo> {
        let mut list = self.inner.lock().unwrap();
        list.retain(|e| !e.task.is_finished());
        list.iter()
            .map(|e| {
                let mut i = e.info.clone();
                i.connections = e.conns.load(Ordering::Relaxed);
                i.error = e.error.lock().unwrap().clone();
                i
            })
            .collect()
    }

    pub fn stop(&self, local: u16) -> bool {
        let mut list = self.inner.lock().unwrap();
        let before = list.len();
        list.retain(|e| {
            if e.info.local == local {
                e.task.abort();
                false
            } else {
                true
            }
        });
        list.len() != before
    }

    pub fn stop_all(&self) {
        for e in self.inner.lock().unwrap().drain(..) {
            e.task.abort();
        }
    }

    /// Listen on `local` and relay to `pod`'s `remote` port.
    pub async fn start(&self, ep: Endpoint, ns: String, pod: String, target: String, local: u16, remote: u16, tx: UnboundedSender<Msg>) -> Result<String> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", local)).await.with_context(|| format!("cannot listen on 127.0.0.1:{local}"))?;
        // Fail now, not on the first connection, if the pod is not there.
        ep.json(Method::GET, &format!("/api/v1/namespaces/{ns}/pods/{pod}"), None).await?;
        let info = FwdInfo { local, target: target.clone(), ns: ns.clone(), pod: pod.clone(), remote, connections: 0, error: None, started: chrono::Utc::now().timestamp() };
        let conns = Arc::new(AtomicU64::new(0));
        let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let path = format!("/api/v1/namespaces/{ns}/pods/{pod}/portforward?ports={remote}");
        let (c2, e2, me) = (conns.clone(), error.clone(), self.clone());
        let task = tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                c2.fetch_add(1, Ordering::Relaxed);
                let (ep, path, e3, me, tx) = (ep.clone(), path.clone(), e2.clone(), me.clone(), tx.clone());
                tokio::spawn(async move {
                    let extra = [("X-Stream-Protocol-Version", "portforward.k8s.io")];
                    let r = match ep.upgrade(Method::POST, &path, "SPDY/3.1", &extra, None).await {
                        Ok((_, io)) => crate::kube::spdy::forward_one(io, remote, conn).await,
                        Err(e) => Err(e),
                    };
                    let new = r.err().map(|e| format!("{e:#}"));
                    let changed = {
                        let mut cur = e3.lock().unwrap();
                        let changed = *cur != new;
                        *cur = new;
                        changed
                    };
                    if changed {
                        let _ = tx.send(Msg::Kube(KMsg::Forwards(me.list())));
                    }
                });
            }
        });
        self.inner.lock().unwrap().push(Entry { info, task, conns, error });
        Ok(format!("forwarding 127.0.0.1:{local} to {ns}/{target} (pod {pod}) port {remote}"))
    }
}
