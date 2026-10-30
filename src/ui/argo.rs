//! The Apps view's backend: the cluster's Argo CD Applications and
//! ApplicationSets (`argoproj.io/v1alpha1`, served by Argo CD or by any
//! controller that reconciles the same manifests, such as litecd), listed
//! every 5 s while the view is on screen and not at all otherwise; which
//! controller runs them; and, for the app the view drills into, its
//! resources' health from the live objects when the status has none.
//!
//! Everything here reads. Syncs, refreshes and terminations are the
//! Kubernetes view's effects (`kube::gitops`), patches of the Application
//! itself, so they work the same whatever controller reconciles it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use hyper::Method;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use super::app::Msg;
use super::client::Endpoint;
use super::kube::gitops;
use super::kube::resources::Resource;
use super::kube::table::Health;

/// How often the view refreshes while shown.
pub const EVERY: Duration = Duration::from_secs(5);

const APPS: &str = "/apis/argoproj.io/v1alpha1/applications";
const APPSETS: &str = "/apis/argoproj.io/v1alpha1/applicationsets";

/// What runs the Applications.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Controller {
    /// `Argo CD`, `litecd`, or empty when no known controller was found.
    pub name: String,
    /// Its image's tag (`v3.1.0`, `latest`), when known.
    pub version: String,
    /// Where it runs.
    pub namespace: String,
}

impl Controller {
    /// `litecd latest · argocd`, `Argo CD v3.1.0 · argocd`, `controller unknown`.
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            return "controller unknown".into();
        }
        let mut s = self.name.clone();
        if !self.version.is_empty() {
            s.push(' ');
            s.push_str(&self.version);
        }
        if !self.namespace.is_empty() {
            s.push_str(" · ");
            s.push_str(&self.namespace);
        }
        s
    }
}

/// One sample of the Apps view.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArgoSample {
    /// Applications, by namespace and name, trimmed (no managedFields, no
    /// last-applied annotation).
    pub apps: Vec<Value>,
    /// ApplicationSets; `None` when the cluster does not serve them.
    pub appsets: Option<Vec<Value>>,
    pub controller: Option<Controller>,
    pub error: Option<String>,
    /// When (unix seconds); 0 before the first sample.
    pub at: i64,
}

/// What the view asks the poller for besides the lists: the app it drills
/// into, and the cluster's resource types to read its resources with.
#[derive(Debug, Clone, Default)]
pub struct Focus {
    pub app: Option<(String, String)>,
    pub kinds: Vec<Resource>,
}

pub type Shared = Arc<Mutex<Option<Endpoint>>>;
pub type SharedFocus = Arc<Mutex<Focus>>;

pub fn spawn(tx: UnboundedSender<Msg>, visible: Arc<AtomicBool>, wake: Arc<Notify>, ep: Shared, focus: SharedFocus) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The controller, found once per API server.
        let mut known: Option<(String, Option<Controller>)> = None;
        loop {
            if !visible.load(Ordering::Relaxed) {
                wake.notified().await;
                continue;
            }
            let endpoint = ep.lock().unwrap().clone();
            let sample = match endpoint {
                Some(ep) => {
                    let mut s = match sample(&ep).await {
                        Ok(s) => s,
                        Err(e) => ArgoSample { error: Some(format!("{e:#}")), ..Default::default() },
                    };
                    if s.error.is_none() {
                        let controller = match &known {
                            Some((display, c)) if *display == ep.display => c.clone(),
                            _ => {
                                let c = controller(&ep, &s.apps).await;
                                known = Some((ep.display.clone(), c.clone()));
                                c
                            }
                        };
                        s.controller = controller;
                        let f = focus.lock().unwrap().clone();
                        if let Some((ns, name)) = &f.app {
                            fill_focused(&ep, &mut s.apps, ns, name, &f.kinds).await;
                        }
                    }
                    s
                }
                None => ArgoSample { error: Some("the cluster is not running".into()), ..Default::default() },
            };
            if tx.send(Msg::Argo(Box::new(sample))).is_err() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(EVERY) => {}
                _ = wake.notified() => {}
            }
        }
    })
}

pub async fn sample(ep: &Endpoint) -> Result<ArgoSample> {
    let list = ep.json(Method::GET, APPS, None).await?;
    let mut apps: Vec<Value> = list["items"].as_array().cloned().unwrap_or_default();
    apps.iter_mut().for_each(trim);
    apps.sort_by_key(key);
    child_health(&mut apps);
    // ApplicationSets are optional (a cluster with Applications alone).
    let appsets = match ep.json(Method::GET, APPSETS, None).await {
        Ok(l) => {
            let mut v: Vec<Value> = l["items"].as_array().cloned().unwrap_or_default();
            v.iter_mut().for_each(trim);
            v.sort_by_key(key);
            Some(v)
        }
        Err(_) => None,
    };
    Ok(ArgoSample { apps, appsets, controller: None, error: None, at: chrono::Utc::now().timestamp() })
}

fn key(v: &Value) -> (String, String) {
    (s(v, "/metadata/namespace").to_string(), s(v, "/metadata/name").to_string())
}

/// What the view never shows: managed fields and the last-applied copy.
fn trim(v: &mut Value) {
    if let Some(m) = v.get_mut("metadata").and_then(Value::as_object_mut) {
        m.remove("managedFields");
        if let Some(a) = m.get_mut("annotations").and_then(Value::as_object_mut) {
            a.remove("kubectl.kubernetes.io/last-applied-configuration");
        }
    }
}

/// An app's resources that are apps (app of apps) get those apps' health,
/// which the parent's status leaves out: their own status has it.
pub fn child_health(apps: &mut [Value]) {
    let health: std::collections::HashMap<(String, String), Value> = apps.iter().filter_map(|a| Some((key(a), a.pointer("/status/health").filter(|h| h["status"].is_string())?.clone()))).collect();
    for a in apps.iter_mut() {
        for r in a.pointer_mut("/status/resources").and_then(Value::as_array_mut).into_iter().flatten() {
            if r.get("health").is_none()
                && r["group"] == gitops::GROUP
                && r["kind"] == "Application"
                && let Some(h) = health.get(&(s(r, "/namespace").to_string(), s(r, "/name").to_string()))
            {
                r["health"] = h.clone();
            }
        }
    }
}

/// The resources' health of the focused app from their live objects, where
/// its status has none (Argo CD v3 keeps it in its own cache).
async fn fill_focused(ep: &Endpoint, apps: &mut [Value], ns: &str, name: &str, kinds: &[Resource]) {
    let Some(app) = apps.iter_mut().find(|a| s(a, "/metadata/namespace") == ns && s(a, "/metadata/name") == name) else { return };
    let find = |group: &str, kind: &str| kinds.iter().find(|r| r.group == group && r.kind == kind).cloned();
    gitops::fill_health(ep, app, &find).await;
}

/// Which controller runs the Applications: the Deployments and
/// StatefulSets of the namespace their status names (else theirs, else
/// `argocd`): Argo CD's application controller, or litecd. `None` when
/// they cannot be read (RBAC).
pub async fn controller(ep: &Endpoint, apps: &[Value]) -> Option<Controller> {
    let mut namespaces: Vec<String> = apps.iter().map(|a| s(a, "/status/controllerNamespace")).filter(|n| !n.is_empty()).map(str::to_string).collect();
    namespaces.extend(apps.iter().map(|a| s(a, "/metadata/namespace").to_string()));
    namespaces.push("argocd".into());
    namespaces.dedup();
    let mut readable = false;
    for ns in namespaces.iter().take(3) {
        let mut workloads = Vec::new();
        for kind in ["deployments", "statefulsets"] {
            if let Ok(l) = ep.json(Method::GET, &format!("/apis/apps/v1/namespaces/{ns}/{kind}"), None).await {
                readable = true;
                workloads.extend(l["items"].as_array().cloned().unwrap_or_default());
            }
        }
        if let Some(c) = identify(&workloads) {
            return Some(c);
        }
    }
    readable.then(Controller::default)
}

/// The controller among these workloads, by name or image.
pub fn identify(workloads: &[Value]) -> Option<Controller> {
    for w in workloads {
        let name = s(w, "/metadata/name");
        let images: Vec<&str> = w.pointer("/spec/template/spec/containers").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c["image"].as_str()).collect();
        let tag_of = |needle: &str| images.iter().find(|i| i.contains(needle)).map(|i| tag(i)).unwrap_or_default();
        let found = if name.contains("litecd") || images.iter().any(|i| i.contains("litecd")) {
            Some(("litecd", tag_of("litecd")))
        } else if name.ends_with("application-controller") {
            Some(("Argo CD", tag_of("argocd")))
        } else {
            None
        };
        if let Some((n, v)) = found {
            return Some(Controller { name: n.into(), version: v, namespace: s(w, "/metadata/namespace").into() });
        }
    }
    None
}

/// An image's tag (`latest` when it has none; empty for a digest alone).
fn tag(image: &str) -> String {
    let image = image.split('@').next().unwrap_or(image);
    let last = image.rsplit('/').next().unwrap_or(image);
    match last.split_once(':') {
        Some((_, t)) => t.to_string(),
        None => "latest".into(),
    }
}

fn s<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default()
}

// ---- what the view shows of an app ----

pub fn name(app: &Value) -> &str {
    s(app, "/metadata/name")
}

pub fn namespace(app: &Value) -> &str {
    s(app, "/metadata/namespace")
}

pub fn sync_status(app: &Value) -> &str {
    or(s(app, "/status/sync/status"), "Unknown")
}

pub fn health_status(app: &Value) -> &str {
    or(s(app, "/status/health/status"), "Unknown")
}

fn or<'a>(v: &'a str, d: &'a str) -> &'a str {
    if v.is_empty() { d } else { v }
}

/// The revision it is synced to, short.
pub fn revision(app: &Value) -> String {
    let r = s(app, "/status/sync/revision");
    if !r.is_empty() {
        return gitops::short_rev(r).to_string();
    }
    // Multi-source: one revision per source.
    let revs: Vec<&str> = app.pointer("/status/sync/revisions").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(gitops::short_rev).collect();
    revs.join(",")
}

/// The sources: `repo path@target` (a chart's name for Helm repos), one
/// per source.
pub fn sources(app: &Value) -> Vec<String> {
    let one = |src: &Value| {
        let repo = s(src, "/repoURL");
        let repo = repo.trim_end_matches('/').trim_end_matches(".git");
        let what = [s(src, "/path"), s(src, "/chart")].into_iter().find(|p| !p.is_empty()).unwrap_or_default();
        let target = or(s(src, "/targetRevision"), "HEAD");
        let mut out = repo.to_string();
        if !what.is_empty() {
            out.push(' ');
            out.push_str(what);
        }
        out.push('@');
        out.push_str(target);
        out
    };
    match (app.pointer("/spec/source"), app.pointer("/spec/sources").and_then(Value::as_array)) {
        (Some(src), _) if src.is_object() => vec![one(src)],
        (_, Some(list)) => list.iter().map(one).collect(),
        _ => Vec::new(),
    }
}

/// A source as short as it can say where: `owner/repo path@target` (the
/// URL's last two segments; with one, the host's first label and it:
/// `git-server/gitops`, `argoproj/argo-helm`).
pub fn short_repo(src: &str) -> String {
    let (head, target) = src.rsplit_once('@').map(|(h, t)| (h, Some(t))).unwrap_or((src, None));
    let (repo, path) = head.split_once(' ').map(|(a, b)| (a, Some(b))).unwrap_or((head, None));
    let repo = repo.split("://").last().unwrap_or(repo);
    // `git@github.com:acme/x`: the user goes.
    let repo = repo.split_once('@').map(|(_, r)| r).unwrap_or(repo);
    let mut segs: Vec<&str> = repo.split(['/', ':']).filter(|x| !x.is_empty()).collect();
    if let Some(host) = segs.first_mut().filter(|h| h.contains('.')) {
        *host = host.split('.').next().unwrap_or(host);
    }
    let mut out = if segs.len() > 2 { segs[segs.len() - 2..].join("/") } else { segs.join("/") };
    if let Some(p) = path {
        out.push(' ');
        out.push_str(p);
    }
    if let Some(t) = target {
        out.push('@');
        out.push_str(t);
    }
    out
}

/// `server` or `name`, and the namespace.
pub fn destination(app: &Value) -> String {
    let d = app.pointer("/spec/destination").cloned().unwrap_or(Value::Null);
    let server = [s(&d, "/name"), s(&d, "/server")].into_iter().find(|x| !x.is_empty()).unwrap_or("?");
    let server = if server == "https://kubernetes.default.svc" { "in-cluster" } else { server };
    let ns = s(&d, "/namespace");
    if ns.is_empty() { server.to_string() } else { format!("{server}/{ns}") }
}

/// `auto (prune, self-heal)` or `manual`.
pub fn policy(app: &Value) -> String {
    let Some(a) = app.pointer("/spec/syncPolicy/automated").filter(|a| a.is_object() && a["enabled"].as_bool() != Some(false)) else { return "manual".into() };
    let opts: Vec<&str> = [("prune", "prune"), ("selfHeal", "self-heal")].into_iter().filter(|(k, _)| a[*k].as_bool() == Some(true)).map(|(_, w)| w).collect();
    if opts.is_empty() { "auto".into() } else { format!("auto ({})", opts.join(", ")) }
}

/// When it last synced (unix seconds): the operation's end, else its
/// start, else the last history entry's deploy.
pub fn last_sync(app: &Value) -> Option<i64> {
    let at = [s(app, "/status/operationState/finishedAt"), s(app, "/status/operationState/startedAt")].into_iter().find(|x| !x.is_empty()).map(str::to_string).or_else(|| {
        app.pointer("/status/history").and_then(Value::as_array).and_then(|h| h.last()).map(|e| s(e, "/deployedAt").to_string())
    })?;
    parse_time(&at)
}

pub fn parse_time(t: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(t).ok().map(|d| d.timestamp())
}

/// The operation: phase and message, whether one is requested and not
/// started yet.
pub fn operation(app: &Value) -> Option<(String, String)> {
    let phase = s(app, "/status/operationState/phase");
    if phase.is_empty() {
        return app["operation"].is_object().then(|| ("Requested".to_string(), String::new()));
    }
    Some((phase.to_string(), s(app, "/status/operationState/message").to_string()))
}

/// Its glyph: the app's health and sync (as the Kubernetes view's), a
/// running sync on its way.
pub fn glyph(app: &Value) -> Health {
    let h = gitops::app_health(app);
    if h == Health::Ok && (running(app) || app["operation"].is_object()) { Health::Warn } else { h }
}

pub fn running(app: &Value) -> bool {
    matches!(s(app, "/status/operationState/phase"), "Running" | "Terminating")
}

/// What the list's last column says: a running or failed operation's
/// message, else the first condition, else the health message.
pub fn note(app: &Value) -> (String, Health) {
    if let Some((phase, msg)) = operation(app) {
        match phase.as_str() {
            "Running" | "Terminating" | "Requested" => return (join(&format!("sync {}", phase.to_lowercase()), &msg), Health::Warn),
            "Failed" | "Error" => return (join(&format!("sync {}", phase.to_lowercase()), &msg), Health::Bad),
            _ => {}
        }
    }
    if let Some(c) = conditions(app).into_iter().next() {
        let h = if c.0.ends_with("Error") { Health::Bad } else { Health::Warn };
        return (format!("{}: {}", c.0, c.1), h);
    }
    let m = s(app, "/status/health/message");
    (m.to_string(), Health::Ok)
}

fn join(a: &str, b: &str) -> String {
    if b.is_empty() { a.to_string() } else { format!("{a}: {b}") }
}

/// Its conditions: type, message, when (unix seconds).
pub fn conditions(app: &Value) -> Vec<(String, String, Option<i64>)> {
    app.pointer("/status/conditions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| (s(c, "/type").to_string(), s(c, "/message").to_string(), parse_time(s(c, "/lastTransitionTime"))))
        .collect()
}

/// One of an app's resources as the detail lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct AppResource {
    pub group: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub sync: String,
    pub health: String,
    pub message: String,
    /// Its health read from the live object (the status had none).
    pub live: bool,
    pub prune: bool,
    pub hook: bool,
}

impl AppResource {
    pub fn glyph(&self) -> Health {
        let h = gitops::health_of(&self.health, &self.sync);
        if self.prune && h == Health::Ok { Health::Warn } else { h }
    }
}

pub fn resources(app: &Value) -> Vec<AppResource> {
    app.pointer("/status/resources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|r| AppResource {
            group: s(r, "/group").into(),
            kind: s(r, "/kind").into(),
            namespace: s(r, "/namespace").into(),
            name: s(r, "/name").into(),
            sync: s(r, "/status").into(),
            health: s(r, "/health/status").into(),
            message: s(r, "/health/message").into(),
            live: r.pointer("/health/assessed").is_some(),
            prune: r["requiresPruning"].as_bool() == Some(true),
            hook: r["hook"].as_bool() == Some(true),
        })
        .collect()
}

/// A sync history entry, newest first in [`history`].
#[derive(Debug, Clone, PartialEq)]
pub struct Deploy {
    pub id: i64,
    pub revision: String,
    pub at: Option<i64>,
    /// `auto` or the user.
    pub by: String,
    pub source: String,
}

pub fn history(app: &Value) -> Vec<Deploy> {
    let mut v: Vec<Deploy> = app
        .pointer("/status/history")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| {
            let rev = [s(e, "/revision")].into_iter().find(|r| !r.is_empty()).map(|r| gitops::short_rev(r).to_string()).unwrap_or_else(|| {
                e.pointer("/revisions").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(gitops::short_rev).collect::<Vec<_>>().join(",")
            });
            let by = if e.pointer("/initiatedBy/automated").and_then(Value::as_bool) == Some(true) { "auto".to_string() } else { or(s(e, "/initiatedBy/username"), "-").to_string() };
            let src = e.get("source").filter(|x| x.is_object()).map(|x| short_repo(&sources(&serde_json::json!({"spec": {"source": x}})).concat())).unwrap_or_default();
            Deploy { id: e["id"].as_i64().unwrap_or(0), revision: rev, at: parse_time(s(e, "/deployedAt")), by, source: src }
        })
        .collect();
    v.reverse();
    v
}

/// The last operation's results per resource: kind/name, status, message.
pub fn sync_result(app: &Value) -> Vec<(String, String, String)> {
    app.pointer("/status/operationState/syncResult/resources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|r| {
            let ns = s(r, "/namespace");
            let name = if ns.is_empty() { s(r, "/name").to_string() } else { format!("{ns}/{}", s(r, "/name")) };
            (format!("{} {name}", s(r, "/kind").to_lowercase()), s(r, "/status").to_string(), s(r, "/message").to_string())
        })
        .collect()
}

/// An ApplicationSet's line: generators, the apps it owns (of `apps`), its
/// first condition that is not fine.
pub fn appset_line(set: &Value, apps: &[Value]) -> (String, usize, String, Health) {
    let gens: Vec<String> = set.pointer("/spec/generators").and_then(Value::as_array).into_iter().flatten().filter_map(|g| g.as_object().and_then(|o| o.keys().next().cloned())).collect();
    let uid = s(set, "/metadata/uid");
    let owned = apps.iter().filter(|a| a.pointer("/metadata/ownerReferences").and_then(Value::as_array).into_iter().flatten().any(|o| o["kind"] == "ApplicationSet" && (uid.is_empty() || o["uid"] == uid) && o["name"] == set["metadata"]["name"])).count();
    let conds = set.pointer("/status/conditions").and_then(Value::as_array).cloned().unwrap_or_default();
    let bad = conds.iter().find(|c| (c["type"] == "ErrorOccurred" && c["status"] == "True") || (c["type"] == "ResourcesUpToDate" && c["status"] == "False"));
    let (msg, h) = match bad {
        Some(c) => (format!("{}: {}", s(c, "/type"), s(c, "/message")), Health::Bad),
        None => (conds.iter().find(|c| c["status"] == "True").map(|c| s(c, "/message").to_string()).unwrap_or_default(), Health::Ok),
    };
    (gens.join(", "), owned, msg, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn controllers_are_told_apart() {
        let litecd = json!({"metadata": {"name": "litecd", "namespace": "argocd"}, "spec": {"template": {"spec": {"containers": [{"image": "ghcr.io/ideascoldigital/litecd:latest"}]}}}});
        let argo = json!({"metadata": {"name": "argocd-application-controller", "namespace": "argocd"}, "spec": {"template": {"spec": {"containers": [{"image": "quay.io/argoproj/argocd:v3.1.0"}]}}}});
        let other = json!({"metadata": {"name": "argocd-server", "namespace": "argocd"}, "spec": {"template": {"spec": {"containers": [{"image": "quay.io/argoproj/argocd:v3.1.0"}]}}}});
        assert_eq!(identify(&[other.clone(), litecd]).unwrap().label(), "litecd latest · argocd");
        assert_eq!(identify(&[other.clone(), argo]).unwrap().label(), "Argo CD v3.1.0 · argocd");
        assert_eq!(identify(&[other]), None);
        assert_eq!(Controller::default().label(), "controller unknown");
        assert_eq!(tag("reg:5000/x/litecd@sha256:ab"), "latest");
        assert_eq!(tag("reg:5000/x/litecd:1.2@sha256:ab"), "1.2");
    }

    #[test]
    fn an_app_in_a_few_words() {
        let app = json!({"metadata": {"namespace": "argocd", "name": "web"},
            "spec": {"source": {"repoURL": "https://github.com/acme/deploy.git", "path": "apps/web", "targetRevision": "main"}, "destination": {"server": "https://kubernetes.default.svc", "namespace": "web"},
                     "syncPolicy": {"automated": {"prune": true, "selfHeal": true}}},
            "status": {"sync": {"status": "OutOfSync", "revision": "0123456789abcdef0123456789abcdef01234567"}, "health": {"status": "Degraded"},
                       "operationState": {"phase": "Failed", "message": "one or more objects failed to apply", "startedAt": "2026-01-01T00:00:00Z", "finishedAt": "2026-01-01T00:01:00Z"},
                       "history": [{"id": 1, "revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "deployedAt": "2025-12-31T00:00:00Z", "initiatedBy": {"automated": true}},
                                   {"id": 2, "revision": "0123456789abcdef0123456789abcdef01234567", "deployedAt": "2026-01-01T00:01:00Z", "initiatedBy": {"username": "admin"}, "source": {"repoURL": "https://github.com/acme/deploy.git", "path": "apps/web"}}],
                       "resources": [{"group": "apps", "kind": "Deployment", "namespace": "web", "name": "web", "status": "OutOfSync", "health": {"status": "Degraded", "message": "deadline"}},
                                     {"kind": "ConfigMap", "namespace": "web", "name": "old", "status": "OutOfSync", "requiresPruning": true}]}});
        assert_eq!((sync_status(&app), health_status(&app), revision(&app)), ("OutOfSync", "Degraded", "0123456".to_string()));
        assert_eq!(sources(&app), vec!["https://github.com/acme/deploy apps/web@main".to_string()]);
        assert_eq!(short_repo(&sources(&app)[0]), "acme/deploy apps/web@main");
        assert_eq!(short_repo("git://git-server.gitops.svc.cluster.local/gitops registry/x@HEAD"), "git-server/gitops registry/x@HEAD");
        assert_eq!(short_repo("https://argoproj.github.io/argo-helm argo-workflows@0.20.1"), "argoproj/argo-helm argo-workflows@0.20.1");
        assert_eq!(short_repo("git@github.com:acme/values@main"), "acme/values@main");
        assert_eq!(destination(&app), "in-cluster/web");
        assert_eq!(policy(&app), "auto (prune, self-heal)");
        assert_eq!(policy(&json!({})), "manual");
        assert_eq!(last_sync(&app), parse_time("2026-01-01T00:01:00Z"));
        assert_eq!(note(&app), ("sync failed: one or more objects failed to apply".to_string(), Health::Bad));
        let h = history(&app);
        assert_eq!((h[0].id, h[0].by.as_str(), h[0].revision.as_str(), h[0].source.as_str()), (2, "admin", "0123456", "acme/deploy apps/web@HEAD"));
        assert_eq!(h[1].by, "auto");
        let r = resources(&app);
        assert_eq!(r[0].glyph(), Health::Bad);
        assert!(r[1].prune && r[1].glyph() == Health::Warn);
        let requested = json!({"operation": {"sync": {}}, "status": {}});
        assert_eq!(note(&requested).0, "sync requested");
        let multi = json!({"spec": {"sources": [{"repoURL": "https://charts.example.com", "chart": "redis", "targetRevision": "18.0.0"}, {"repoURL": "git@github.com:acme/values.git", "targetRevision": "main"}]},
            "status": {"sync": {"revisions": ["18.0.0", "0123456789abcdef0123456789abcdef01234567"]}}});
        assert_eq!(sources(&multi), vec!["https://charts.example.com redis@18.0.0".to_string(), "git@github.com:acme/values@main".to_string()]);
        assert_eq!(revision(&multi), "18.0.0,0123456");
    }

    #[test]
    fn child_apps_get_their_own_health() {
        let mut apps = vec![
            json!({"metadata": {"namespace": "argocd", "name": "root"}, "status": {"resources": [{"group": "argoproj.io", "kind": "Application", "namespace": "argocd", "name": "web", "status": "Synced"}, {"group": "argoproj.io", "kind": "Application", "namespace": "argocd", "name": "gone", "status": "OutOfSync"}]}}),
            json!({"metadata": {"namespace": "argocd", "name": "web"}, "status": {"health": {"status": "Degraded", "message": "deadline"}}}),
        ];
        child_health(&mut apps);
        let r = resources(&apps[0]);
        assert_eq!((r[0].health.as_str(), r[0].message.as_str(), r[0].glyph()), ("Degraded", "deadline", Health::Bad));
        assert_eq!(r[1].health, "");
    }

    #[test]
    fn appsets_count_their_apps_and_say_what_is_wrong() {
        let set = json!({"metadata": {"name": "envs", "uid": "u1"}, "spec": {"generators": [{"list": {}}, {"git": {}}]},
            "status": {"conditions": [{"type": "ErrorOccurred", "status": "True", "message": "repo not found"}]}});
        let owned = json!({"metadata": {"ownerReferences": [{"kind": "ApplicationSet", "name": "envs", "uid": "u1"}]}});
        let (gens, n, msg, h) = appset_line(&set, &[owned, json!({})]);
        assert_eq!((gens.as_str(), n, msg.as_str(), h), ("list, git", 1, "ErrorOccurred: repo not found", Health::Bad));
    }
}
