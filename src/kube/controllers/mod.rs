//! Workload controllers, the scheduler and the garbage collector, running
//! in-process with the API server.
//!
//! One task owns them all. It subscribes to the object store's change feed
//! (no HTTP, no polling), maps each change to the objects that must be
//! reconciled, and runs those reconciles one at a time against the in-memory
//! store. A reconcile reads current state, writes the difference through the
//! same in-process API the HTTP handlers use (so defaulting and invariants
//! apply), and may ask to run again after a delay (cron schedules,
//! minReadySeconds, back-off). Writes that change nothing are skipped, so the
//! loop settles instead of feeding on its own events.

mod cronjob;
mod daemonset;
mod deployment;
mod gc;
mod history;
mod job;
mod misc;
mod replicaset;
mod scheduler;
mod statefulset;
pub(crate) mod util;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::time::Instant;

use super::server::ApiState;
use super::store::{EventType, WatchEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Kind {
    Deployment,
    ReplicaSet,
    ReplicationController,
    StatefulSet,
    DaemonSet,
    Job,
    CronJob,
    /// Bind a pending pod to a node.
    Schedule,
    /// LoadBalancer status and Endpoints of a Service.
    Service,
    /// Per-namespace bootstrap objects (`kube-root-ca.crt`).
    Namespace,
    /// Reclaim the volume of a deleted claim.
    VolumeClaim,
    /// `status.used` of a ResourceQuota.
    Quota,
    /// Status of a PodDisruptionBudget.
    Pdb,
    /// Evict pods a node's NoExecute taints no longer let run.
    Taints,
    /// Foreground deletion (`group|resource|name`).
    Foreground,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WorkKey {
    pub kind: Kind,
    pub namespace: String,
    pub name: String,
}

impl WorkKey {
    fn new(kind: Kind, namespace: &str, name: &str) -> Self {
        Self {
            kind,
            namespace: namespace.to_string(),
            name: name.to_string(),
        }
    }
}

/// What a reconcile asks for next.
pub(crate) type Requeue = Option<Duration>;

pub(crate) struct Manager {
    pub(crate) api: Arc<ApiState>,
    queue: BTreeSet<WorkKey>,
    timers: BTreeMap<Instant, Vec<WorkKey>>,
    /// uids of objects deleted since the last garbage collection pass, by
    /// the namespace they lived in.
    deleted: BTreeMap<String, HashSet<String>>,
    /// Pods the scheduler could not place; retried when nodes or pods change.
    unschedulable: BTreeSet<(String, String)>,
}

/// Run the controllers until the store goes away.
pub async fn run(api: Arc<ApiState>) {
    let mut manager = Manager {
        api,
        queue: BTreeSet::new(),
        timers: BTreeMap::new(),
        deleted: BTreeMap::new(),
        unschedulable: BTreeSet::new(),
    };
    loop {
        let mut live = match manager.api.store().subscribe(None) {
            Ok(sub) => sub.live,
            Err(_) => return,
        };
        manager.resync();
        loop {
            manager.process();
            let next_timer = manager.timers.keys().next().copied();
            let received = tokio::select! {
                r = live.recv() => Some(r),
                _ = sleep_until(next_timer) => None,
            };
            match received {
                Some(Ok(event)) => manager.observe(&event),
                Some(Err(RecvError::Lagged(_))) => break,
                Some(Err(RecvError::Closed)) => return,
                None => {}
            }
            // Batch whatever else is already queued before reconciling.
            loop {
                match live.try_recv() {
                    Ok(event) => manager.observe(&event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Lagged(_)) => break,
                    Err(TryRecvError::Closed) => return,
                }
            }
        }
        // Fell behind the change feed: start over from a full resync.
        eprintln!("controllers: fell behind the change feed, resyncing");
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl Manager {
    fn enqueue(&mut self, kind: Kind, namespace: &str, name: &str) {
        self.queue.insert(WorkKey::new(kind, namespace, name));
    }

    fn after(&mut self, delay: Duration, key: WorkKey) {
        self.timers.entry(Instant::now() + delay).or_default().push(key);
    }

    /// Queue every object a controller owns, and garbage-collect anything
    /// whose owners vanished while we were not looking.
    fn resync(&mut self) {
        let api = self.api.clone();
        let store = api.store();
        for (group, plural, kind) in [
            ("apps", "deployments", Kind::Deployment),
            ("apps", "replicasets", Kind::ReplicaSet),
            ("", "replicationcontrollers", Kind::ReplicationController),
            ("apps", "statefulsets", Kind::StatefulSet),
            ("apps", "daemonsets", Kind::DaemonSet),
            ("batch", "jobs", Kind::Job),
            ("batch", "cronjobs", Kind::CronJob),
            ("", "services", Kind::Service),
            ("", "resourcequotas", Kind::Quota),
            ("policy", "poddisruptionbudgets", Kind::Pdb),
        ] {
            for key in store.keys_of_resource(group, plural) {
                self.enqueue(kind, &key.namespace, &key.name);
            }
        }
        for key in store.keys_of_resource("", "namespaces") {
            self.enqueue(Kind::Namespace, "", &key.name);
        }
        let (pods, _) = store.list("", "pods", None);
        for pod in &pods {
            if util::needs_scheduling(pod) {
                let (ns, name) = util::ns_name(pod);
                self.enqueue(Kind::Schedule, &ns, &name);
            }
        }
        gc::collect_orphans(&self.api);
    }

    /// Map one change to the reconciles it calls for.
    fn observe(&mut self, event: &WatchEvent) {
        let key = &event.key;
        let obj = &event.object;
        let (ns, name) = (key.namespace.as_str(), key.name.as_str());
        if event.event == EventType::Deleted {
            if let Some(uid) = obj.pointer("/metadata/uid").and_then(Value::as_str) {
                self.deleted.entry(ns.to_string()).or_default().insert(uid.to_string());
            }
        }
        if event.event != EventType::Deleted
            && util::is_deleting(obj)
            && obj.pointer("/metadata/finalizers").and_then(Value::as_array).is_some_and(|f| f.iter().any(|x| x == "foregroundDeletion" || x == "orphan"))
        {
            self.enqueue(Kind::Foreground, ns, &format!("{}|{}|{}", key.group, key.resource, name));
        }
        if !ns.is_empty() && key.resource != "events" {
            if key.group.is_empty() && key.resource == "resourcequotas" {
                if event.event != EventType::Deleted {
                    self.enqueue(Kind::Quota, ns, name);
                }
            } else {
                for q in self.api.store().list("", "resourcequotas", Some(ns)).0 {
                    let qname = util::name(&q);
                    self.enqueue(Kind::Quota, ns, &qname);
                }
            }
        }
        match (key.group.as_str(), key.resource.as_str()) {
            ("apps", "deployments") => self.enqueue(Kind::Deployment, ns, name),
            ("apps", "replicasets") => {
                self.enqueue(Kind::ReplicaSet, ns, name);
                self.enqueue_owner(ns, obj);
            }
            ("", "replicationcontrollers") => self.enqueue(Kind::ReplicationController, ns, name),
            ("apps", "statefulsets") => self.enqueue(Kind::StatefulSet, ns, name),
            ("apps", "daemonsets") => self.enqueue(Kind::DaemonSet, ns, name),
            ("batch", "jobs") => {
                self.enqueue(Kind::Job, ns, name);
                self.enqueue_owner(ns, obj);
            }
            ("batch", "cronjobs") => self.enqueue(Kind::CronJob, ns, name),
            ("policy", "poddisruptionbudgets") if event.event != EventType::Deleted => self.enqueue(Kind::Pdb, ns, name),
            ("", "pods") => {
                let node = util::s(obj, "/spec/nodeName").to_string();
                if !node.is_empty() && event.event != EventType::Deleted {
                    self.enqueue(Kind::Taints, "", &node);
                }
                for pdb in self.api.store().list("policy", "poddisruptionbudgets", Some(ns)).0 {
                    let pname = util::name(&pdb);
                    self.enqueue(Kind::Pdb, ns, &pname);
                }
                self.enqueue_owner(ns, obj);
                if util::controller_of(obj).is_none() {
                    self.enqueue_adopters(ns);
                }
                if event.event != EventType::Deleted && util::needs_scheduling(obj) {
                    self.enqueue(Kind::Schedule, ns, name);
                }
                // A pod going away or finishing frees room on its node.
                let freed = event.event == EventType::Deleted || util::is_terminal(obj);
                if freed && !self.unschedulable.is_empty() {
                    self.retry_unschedulable();
                }
                self.enqueue_services_for(ns);
            }
            ("", "nodes") => {
                self.enqueue(Kind::Taints, "", name);
                for ds in self.api.store().keys_of_resource("apps", "daemonsets") {
                    self.enqueue(Kind::DaemonSet, &ds.namespace, &ds.name);
                }
                self.retry_unschedulable();
                for svc in self.api.store().keys_of_resource("", "services") {
                    self.enqueue(Kind::Service, &svc.namespace, &svc.name);
                }
            }
            ("", "services") => self.enqueue(Kind::Service, ns, name),
            // Hand-written Endpoints of selector-less Services are mirrored.
            ("", "endpoints") => self.enqueue(Kind::Service, ns, name),
            // A deleted or edited slice of ours is put back.
            ("discovery.k8s.io", "endpointslices") => {
                let svc = obj.pointer("/metadata/labels/kubernetes.io~1service-name").and_then(Value::as_str).unwrap_or_default().to_string();
                if !svc.is_empty() && util::s(obj, "/metadata/labels/endpointslice.kubernetes.io~1managed-by") == "endpointslice-controller.k8s.io" {
                    self.enqueue(Kind::Service, ns, &svc);
                }
            }
            ("", "namespaces") if event.event != EventType::Deleted => self.enqueue(Kind::Namespace, "", name),
            // Deleted or edited: put the CA bundle back.
            ("", "configmaps") if name == super::defaults::ROOT_CA_CONFIGMAP => self.enqueue(Kind::Namespace, "", ns),
            ("", "persistentvolumeclaims") => {
                if event.event == EventType::Deleted {
                    self.enqueue(Kind::VolumeClaim, ns, name);
                }
                self.retry_unschedulable();
            }
            _ => {}
        }
    }

    fn enqueue_owner(&mut self, ns: &str, obj: &Value) {
        let Some((kind, name)) = util::controller_of(obj) else { return };
        let kind = match kind.as_str() {
            "ReplicaSet" => Kind::ReplicaSet,
            "ReplicationController" => Kind::ReplicationController,
            "Deployment" => Kind::Deployment,
            "StatefulSet" => Kind::StatefulSet,
            "DaemonSet" => Kind::DaemonSet,
            "Job" => Kind::Job,
            "CronJob" => Kind::CronJob,
            _ => return,
        };
        self.enqueue(kind, ns, &name);
    }

    /// An orphan pod may be adopted by a ReplicaSet or ReplicationController.
    fn enqueue_adopters(&mut self, ns: &str) {
        for (group, plural, kind) in [
            ("apps", "replicasets", Kind::ReplicaSet),
            ("", "replicationcontrollers", Kind::ReplicationController),
            ("batch", "jobs", Kind::Job),
        ] {
            let (items, _) = self.api.store().list(group, plural, Some(ns));
            for o in items {
                self.enqueue(kind, ns, &util::name(&o));
            }
        }
    }

    fn enqueue_services_for(&mut self, ns: &str) {
        let (services, _) = self.api.store().list("", "services", Some(ns));
        for svc in services {
            if svc.pointer("/spec/selector").is_some_and(|s| s.as_object().is_some_and(|m| !m.is_empty())) {
                let name = util::name(&svc);
                self.enqueue(Kind::Service, ns, &name);
            }
        }
    }

    fn retry_unschedulable(&mut self) {
        for (ns, name) in std::mem::take(&mut self.unschedulable) {
            self.enqueue(Kind::Schedule, &ns, &name);
        }
    }

    /// Run due timers, the garbage collector and every queued reconcile,
    /// until nothing is left to do right now.
    fn process(&mut self) {
        let now = Instant::now();
        let due: Vec<Instant> = self.timers.range(..=now).map(|(t, _)| *t).collect();
        for t in due {
            for key in self.timers.remove(&t).unwrap_or_default() {
                self.queue.insert(key);
            }
        }
        if !self.deleted.is_empty() {
            let deleted = std::mem::take(&mut self.deleted);
            gc::collect(&self.api, &deleted);
        }
        while let Some(key) = self.queue.pop_first() {
            let requeue = self.reconcile(&key);
            if let Some(delay) = requeue {
                self.after(delay, key);
            }
        }
    }

    fn reconcile(&mut self, key: &WorkKey) -> Requeue {
        let (ns, name) = (key.namespace.as_str(), key.name.as_str());
        let api = self.api.clone();
        let result = match key.kind {
            Kind::Deployment => deployment::reconcile(&api, ns, name),
            Kind::ReplicaSet => replicaset::reconcile(&api, ns, name),
            Kind::ReplicationController => replicaset::reconcile_rc(&api, ns, name),
            Kind::StatefulSet => statefulset::reconcile(&api, ns, name),
            Kind::DaemonSet => daemonset::reconcile(&api, ns, name),
            Kind::Job => job::reconcile(&api, ns, name),
            Kind::CronJob => cronjob::reconcile(&api, ns, name),
            Kind::Schedule => match scheduler::schedule(&api, ns, name) {
                Ok(scheduler::Outcome::Unschedulable) => {
                    self.unschedulable.insert((ns.to_string(), name.to_string()));
                    Ok(None)
                }
                Ok(scheduler::Outcome::Done) => Ok(None),
                Err(e) => Err(e),
            },
            Kind::Service => misc::reconcile_service(&api, ns, name),
            Kind::Namespace => misc::reconcile_namespace(&api, name),
            Kind::VolumeClaim => misc::reclaim_volumes(&api, ns, name),
            Kind::Quota => misc::reconcile_quota(&api, ns, name),
            Kind::Pdb => misc::reconcile_pdb(&api, ns, name),
            Kind::Taints => scheduler::evict_for_taints(&api, name),
            Kind::Foreground => gc::foreground(&api, ns, name),
        };
        match result {
            Ok(requeue) => requeue,
            // Lost a race with another writer: try again with fresh state.
            Err(e) if e.is_conflict() || e.is_already_exists() => Some(Duration::from_millis(50)),
            Err(e) if e.is_not_found() => None,
            Err(e) => {
                eprintln!("controllers: {:?} {ns}/{name}: {e}", key.kind);
                Some(Duration::from_secs(5))
            }
        }
    }
}
