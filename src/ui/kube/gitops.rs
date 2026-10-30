//! Argo CD's Applications (`argoproj.io/v1alpha1`), read and driven through
//! the Kubernetes API alone, as any controller that reconciles them sees
//! them: their status says what is synced and healthy, an `operation` on
//! the object starts a sync (what the argocd CLI writes), the
//! `argocd.argoproj.io/refresh` annotation asks for a refresh, and a
//! Terminating operation phase stops one. Used by the Kubernetes view and
//! by `litek8s mcp`.

use anyhow::{Result, bail};
use hyper::Method;
use serde_json::{Value, json};

use super::resources::Resource;
use super::state::{ObjRef, XNode};
use super::table::Health;
use crate::ui::client::Endpoint;

pub const GROUP: &str = "argoproj.io";
pub const REFRESH: &str = "argocd.argoproj.io/refresh";

/// Whether a resource type is Argo CD's Application.
pub fn is_app(res: &Resource) -> bool {
    res.group == GROUP && res.kind == "Application"
}

fn s<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default()
}

/// A health and sync status as one glyph: Degraded is bad; Progressing,
/// Missing, Unknown or out of sync are on their way; Suspended is idle.
pub fn health_of(health: &str, sync: &str) -> Health {
    match health {
        "Degraded" => Health::Bad,
        "Progressing" | "Missing" | "Unknown" => Health::Warn,
        "Suspended" => Health::Done,
        _ if sync == "OutOfSync" || sync == "Unknown" => Health::Warn,
        _ => Health::Ok,
    }
}

/// An app's state in a few words: `Synced · Healthy`, the revision, and
/// what its operation is doing.
pub fn state_line(app: &Value) -> String {
    let mut out = format!("{} · {}", or_unknown(s(app, "/status/sync/status")), or_unknown(s(app, "/status/health/status")));
    let rev = s(app, "/status/sync/revision");
    if !rev.is_empty() {
        out.push_str(&format!(" @ {}", short_rev(rev)));
    }
    match operation(app) {
        Some((phase, msg)) if phase == "Running" || phase == "Terminating" => out.push_str(&format!(", sync {}: {msg}", phase.to_lowercase())),
        Some((phase, msg)) if phase == "Failed" || phase == "Error" => out.push_str(&format!(", last sync {}: {msg}", phase.to_lowercase())),
        _ => {}
    }
    out
}

fn or_unknown(v: &str) -> &str {
    if v.is_empty() { "Unknown" } else { v }
}

/// A git SHA shortened as Argo CD's UI shows it; other revisions as they are.
pub fn short_rev(r: &str) -> &str {
    if r.len() == 40 && r.chars().all(|c| c.is_ascii_hexdigit()) { &r[..7] } else { r }
}

/// The last (or current) operation's phase and message.
pub fn operation(app: &Value) -> Option<(String, String)> {
    let op = app.pointer("/status/operationState")?;
    Some((s(op, "/phase").to_string(), s(op, "/message").to_string()))
}

/// The health of an app as its glyph.
pub fn app_health(app: &Value) -> Health {
    let h = health_of(s(app, "/status/health/status"), s(app, "/status/sync/status"));
    match operation(app) {
        Some((p, _)) if (p == "Failed" || p == "Error") && h == Health::Ok => Health::Warn,
        _ => h,
    }
}

/// The `operation` a manual sync writes, as the argocd CLI does: the app's
/// sync options, the hook strategy; `prune` deletes what git no longer has,
/// `force` replaces what cannot be patched (deletes and recreates it, as
/// `argocd app sync --force`).
pub fn sync_operation(app: &Value, user: &str, prune: bool, force: bool, revision: Option<&str>) -> Value {
    let mut sync = if force { json!({"syncStrategy": {"hook": {"force": true}}}) } else { json!({"syncStrategy": {"hook": {}}}) };
    if let Some(o) = app.pointer("/spec/syncPolicy/syncOptions").and_then(Value::as_array).filter(|o| !o.is_empty()) {
        sync["syncOptions"] = json!(o);
    }
    if prune {
        sync["prune"] = json!(true);
    }
    if let Some(r) = revision.filter(|r| !r.is_empty()) {
        sync["revision"] = json!(r);
    }
    json!({"initiatedBy": {"username": user}, "sync": sync})
}

/// Start a sync; refused while another operation runs.
pub async fn sync(ep: &Endpoint, obj: &ObjRef, user: &str, prune: bool, force: bool, revision: Option<&str>) -> Result<String> {
    let path = obj.res.object_path(&obj.ns, &obj.name);
    let app = ep.json(Method::GET, &path, None).await?;
    if app["operation"].is_object() {
        bail!("{} already has an operation running", obj.label());
    }
    let patch = json!({"operation": sync_operation(&app, user, prune, force, revision)});
    ep.json_typed(Method::PATCH, &path, "application/merge-patch+json", Some(&patch)).await?;
    let opts: Vec<&str> = [(prune, "prune"), (force, "force")].into_iter().filter(|(on, _)| *on).map(|(_, w)| w).collect();
    Ok(format!("{} syncing{}", obj.label(), if opts.is_empty() { String::new() } else { format!(" ({})", opts.join(", ")) }))
}

/// Ask for a refresh: `normal` compares with git again, `hard` also renders
/// the manifests again.
pub async fn refresh(ep: &Endpoint, obj: &ObjRef, hard: bool) -> Result<String> {
    let kind = if hard { "hard" } else { "normal" };
    let patch = json!({"metadata": {"annotations": {REFRESH: kind}}});
    ep.json_typed(Method::PATCH, &obj.res.object_path(&obj.ns, &obj.name), "application/merge-patch+json", Some(&patch)).await?;
    Ok(format!("{} refreshing ({kind})", obj.label()))
}

/// Stop the running operation (as `argocd app terminate-op`).
pub async fn terminate(ep: &Endpoint, obj: &ObjRef) -> Result<String> {
    let path = obj.res.object_path(&obj.ns, &obj.name);
    let app = ep.json(Method::GET, &path, None).await?;
    if s(&app, "/status/operationState/phase") != "Running" {
        bail!("{} has no operation running", obj.label());
    }
    let patch = json!({"status": {"operationState": {"phase": "Terminating"}}});
    ep.json_typed(Method::PATCH, &path, "application/merge-patch+json", Some(&patch)).await?;
    Ok(format!("{} terminating its sync", obj.label()))
}

/// A live object's health as Argo CD words it, for the kinds whose health
/// says why an app is not Healthy; `None` for kinds that have none.
pub fn live_health(o: &Value) -> Option<(&'static str, String)> {
    let n = |p: &str| o.pointer(p).and_then(Value::as_i64).unwrap_or(0);
    let cond = |t: &str| o.pointer("/status/conditions").and_then(Value::as_array).and_then(|c| c.iter().find(|c| c["type"] == t)).cloned();
    let kind = s(o, "/kind");
    Some(match kind {
        "Deployment" | "StatefulSet" | "ReplicaSet" | "DaemonSet" => {
            if let Some(c) = cond("Progressing").filter(|c| c["reason"] == "ProgressDeadlineExceeded") {
                return Some(("Degraded", or_else(s(&c, "/message"), &format!("{kind} has exceeded its progress deadline"))));
            }
            let want = o.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or_else(|| n("/status/desiredNumberScheduled"));
            let ready = if kind == "DaemonSet" { n("/status/numberReady") } else { n("/status/readyReplicas") };
            let updated = if kind == "DaemonSet" { n("/status/updatedNumberScheduled") } else { o.pointer("/status/updatedReplicas").and_then(Value::as_i64).unwrap_or(ready) };
            if ready < want || updated < want {
                ("Progressing", format!("{ready} of {want} replicas ready, {updated} updated"))
            } else {
                ("Healthy", String::new())
            }
        }
        "Job" => match (cond("Failed").filter(|c| c["status"] == "True"), cond("Complete").filter(|c| c["status"] == "True")) {
            (Some(c), _) => ("Degraded", or_else(s(&c, "/message"), "the job failed")),
            (_, Some(_)) => ("Healthy", String::new()),
            _ => ("Progressing", format!("{} active, {} failed", n("/status/active"), n("/status/failed"))),
        },
        "Pod" => {
            let waiting = o.pointer("/status/containerStatuses").and_then(Value::as_array).into_iter().flatten().find_map(|c| c.pointer("/state/waiting/reason").and_then(Value::as_str).filter(|r| *r != "ContainerCreating" && *r != "PodInitializing"));
            match (s(o, "/status/phase"), waiting) {
                (_, Some(r)) => ("Degraded", r.to_string()),
                ("Failed", _) => ("Degraded", or_else(s(o, "/status/message"), "the pod failed")),
                ("Succeeded", _) => ("Healthy", String::new()),
                ("Running", _) if cond("Ready").is_some_and(|c| c["status"] == "True") => ("Healthy", String::new()),
                _ => ("Progressing", String::new()),
            }
        }
        "PersistentVolumeClaim" => match s(o, "/status/phase") {
            "Bound" => ("Healthy", String::new()),
            "Lost" => ("Degraded", "the volume is lost".into()),
            _ => ("Progressing", "waiting to be bound".into()),
        },
        _ => return None,
    })
}

fn or_else(v: &str, d: &str) -> String {
    if v.is_empty() { d.to_string() } else { v.to_string() }
}

/// Health of the resources an app's status lists without one (Argo CD v3
/// keeps it out of the status: `resourceHealthSource: appTree`), from the
/// live objects: written into the app's `status.resources` as
/// `health: {status, message, assessed: "live"}`. At most 200 reads.
pub async fn fill_health(ep: &Endpoint, app: &mut Value, find: &(dyn Fn(&str, &str) -> Option<Resource> + Sync)) {
    let Some(list) = app.pointer_mut("/status/resources").and_then(Value::as_array_mut) else { return };
    for r in list.iter_mut().filter(|r| r.get("health").is_none()).take(200) {
        let Some(res) = find(s(r, "/group"), s(r, "/kind")) else { continue };
        let Ok(live) = ep.json(Method::GET, &res.object_path(s(r, "/namespace"), s(r, "/name")), None).await else { continue };
        if let Some((h, m)) = live_health(&live) {
            r["health"] = if m.is_empty() { json!({"status": h, "assessed": "live"}) } else { json!({"status": h, "message": m, "assessed": "live"}) };
        }
    }
}

/// The resources an app's status lists, compactly: kind, namespace, name,
/// sync and health (with its message), hook, requires pruning.
pub fn resources(app: &Value) -> Vec<Value> {
    app.pointer("/status/resources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|r| {
            let mut o = json!({"kind": r["kind"], "name": r["name"], "sync": r["status"]});
            for k in ["group", "namespace"] {
                if r[k].as_str().is_some_and(|v| !v.is_empty()) {
                    o[k] = r[k].clone();
                }
            }
            if let Some(h) = r.pointer("/health/status") {
                o["health"] = h.clone();
            }
            if let Some(m) = r.pointer("/health/message") {
                o["message"] = m.clone();
            }
            if r.pointer("/health/assessed").is_some() {
                o["health_from"] = json!("live object");
            }
            for k in ["hook", "requiresPruning"] {
                if r[k].as_bool() == Some(true) {
                    o[k] = json!(true);
                }
            }
            o
        })
        .collect()
}

/// One app for a list: where it comes from and goes, sync, health,
/// revision, its operation, conditions.
pub fn summary(app: &Value) -> Value {
    let src = app.pointer("/spec/source").or_else(|| app.pointer("/spec/sources")).cloned().unwrap_or(Value::Null);
    let mut o = json!({
        "namespace": s(app, "/metadata/namespace"), "name": s(app, "/metadata/name"),
        "project": or_default(s(app, "/spec/project")),
        "source": src, "destination": app.pointer("/spec/destination"),
        "sync": or_unknown(s(app, "/status/sync/status")), "health": or_unknown(s(app, "/status/health/status")),
        "automated": app.pointer("/spec/syncPolicy/automated").is_some_and(|a| a.is_object() && a["enabled"].as_bool() != Some(false)),
    });
    let rev = s(app, "/status/sync/revision");
    if !rev.is_empty() {
        o["revision"] = json!(rev);
    }
    if let Some(op) = app.pointer("/status/operationState") {
        o["operation"] = json!({"phase": op["phase"], "message": op["message"], "startedAt": op["startedAt"], "finishedAt": op["finishedAt"]});
    }
    if app["operation"].is_object() {
        o["operationRequested"] = json!(true);
    }
    if let Some(c) = app.pointer("/status/conditions").and_then(Value::as_array).filter(|c| !c.is_empty()) {
        o["conditions"] = json!(c.iter().map(|c| json!({"type": c["type"], "message": c["message"]})).collect::<Vec<_>>());
    }
    o
}

fn or_default(p: &str) -> &str {
    if p.is_empty() { "default" } else { p }
}

/// The xray tree of apps: each app, its resources with their sync and
/// health, and under an app it manages (app of apps) that app's own.
pub fn tree(apps: &[Value], res: &Resource, find: &dyn Fn(&str, &str) -> Option<Resource>, all_ns: bool) -> Vec<XNode> {
    let by_name = |ns: &str, name: &str| apps.iter().find(|a| s(a, "/metadata/namespace") == ns && s(a, "/metadata/name") == name);
    // An app another app in the list manages is drawn under it.
    let children: Vec<(String, String)> = apps
        .iter()
        .flat_map(|a| {
            a.pointer("/status/resources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|r| r["group"] == GROUP && r["kind"] == "Application")
                .map(|r| (r["namespace"].as_str().unwrap_or_default().to_string(), r["name"].as_str().unwrap_or_default().to_string()))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut out = Vec::new();
    for a in apps {
        let key = (s(a, "/metadata/namespace").to_string(), s(a, "/metadata/name").to_string());
        if children.contains(&key) {
            continue;
        }
        let mut seen = Vec::new();
        app_nodes(a, 0, res, find, all_ns, &by_name, &mut seen, &mut out);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn app_nodes<'a>(app: &'a Value, depth: u8, res: &Resource, find: &dyn Fn(&str, &str) -> Option<Resource>, all_ns: bool, by_name: &dyn Fn(&str, &str) -> Option<&'a Value>, seen: &mut Vec<(String, String)>, out: &mut Vec<XNode>) {
    let (ns, name) = (s(app, "/metadata/namespace"), s(app, "/metadata/name"));
    seen.push((ns.to_string(), name.to_string()));
    out.push(XNode {
        depth,
        kind: "app".into(),
        name: if all_ns || depth > 0 { format!("{ns}/{name}") } else { name.to_string() },
        status: state_line(app),
        health: app_health(app),
        obj: Some(ObjRef { res: res.clone(), ns: ns.into(), name: name.into() }),
        container_of: None,
    });
    for r in app.pointer("/status/resources").and_then(Value::as_array).into_iter().flatten() {
        let (group, kind) = (s(r, "/group"), s(r, "/kind"));
        let (rns, rname) = (s(r, "/namespace"), s(r, "/name"));
        if group == GROUP
            && kind == "Application"
            && let Some(child) = by_name(rns, rname).filter(|_| !seen.iter().any(|(a, b)| a == rns && b == rname))
        {
            app_nodes(child, depth + 1, res, find, all_ns, by_name, seen, out);
            continue;
        }
        let sync = s(r, "/status");
        let health = s(r, "/health/status");
        let mut status = sync.to_string();
        if !health.is_empty() {
            status.push_str(&format!(" · {health}"));
        }
        if r["requiresPruning"].as_bool() == Some(true) {
            status.push_str(", to prune");
        }
        if r["hook"].as_bool() == Some(true) {
            status.push_str(", hook");
        }
        if let Some(m) = r.pointer("/health/message").and_then(Value::as_str).filter(|m| !m.is_empty()) {
            status.push_str(&format!(": {m}"));
        }
        out.push(XNode {
            depth: depth + 1,
            kind: kind.to_lowercase(),
            name: if rns.is_empty() { rname.to_string() } else { format!("{rns}/{rname}") },
            status,
            health: health_of(health, sync),
            obj: find(group, kind).map(|res| ObjRef { res, ns: rns.into(), name: rname.into() }),
            container_of: None,
        });
    }
}

/// What an app's diff is, where a controller publishes it: a Secret
/// `<app>.diff` next to the app (label `gitops.litek8s.dev/diff`), its
/// `diff.json` `{revisions, resources: [{group, version, kind, namespace,
/// name, status, desired, live, omitted}]}` with, per resource out of sync,
/// what git wants and the live object cut to those fields (null for the
/// side that does not exist; Secret values masked). Argo CD itself keeps
/// its diffs in argocd-server; then there is none.
pub fn diff_secret(app: &str) -> String {
    format!("{app}.diff")
}

/// The published diff of an app; none if there is no such Secret.
pub async fn diff_doc(ep: &Endpoint, ns: &str, name: &str) -> Result<Option<Value>> {
    use base64::Engine as _;
    let path = format!("/api/v1/namespaces/{ns}/secrets/{}", diff_secret(name));
    let resp = ep.send(Method::GET, &path, None, Vec::new()).await?;
    let status = resp.status();
    let data = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
    if status == hyper::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    crate::ui::client::check(status, &data)?;
    let secret: Value = serde_json::from_slice(&data)?;
    let raw = base64::engine::general_purpose::STANDARD.decode(s(&secret, "/data/diff.json"))?;
    Ok(Some(serde_json::from_slice(&raw)?))
}

/// The diff as unified-diff lines: per resource `-` live, `+` git.
pub fn diff_lines(app: &Value, doc: Option<&Value>) -> Vec<String> {
    let sync = s(app, "/status/sync/status");
    let Some(doc) = doc else {
        return vec![if sync == "Synced" {
            "In sync: nothing differs from git.".to_string()
        } else {
            format!("{} but its controller published no diff (Argo CD keeps diffs in argocd-server: argocd app diff {}).", if sync.is_empty() { "Not compared yet" } else { sync }, s(app, "/metadata/name"))
        }];
    };
    let revs: Vec<&str> = doc["revisions"].as_array().into_iter().flatten().filter_map(Value::as_str).map(short_rev).collect();
    let mut out = vec![format!("{} at {}; - live, + git", if sync.is_empty() { "?" } else { sync }, if revs.is_empty() { "?".to_string() } else { revs.join(", ") })];
    let yaml = |v: &Value| if v.is_null() { String::new() } else { super::yaml::to_yaml(v, false) };
    for r in doc["resources"].as_array().into_iter().flatten() {
        let ns = s(r, "/namespace");
        let name = if ns.is_empty() { s(r, "/name").to_string() } else { format!("{ns}/{}", s(r, "/name")) };
        out.push(String::new());
        out.push(format!("=== {} {name}: {}", s(r, "/kind").to_lowercase(), s(r, "/status")));
        if r["omitted"] == true {
            out.push("(too big to publish)".into());
            continue;
        }
        let lines = super::yaml::unified(&yaml(&r["live"]), &yaml(&r["desired"]), 3);
        if lines.is_empty() {
            out.push("(the same fields; they differ only in form, as the server's dry run says)".into());
        }
        out.extend(lines);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res() -> Resource {
        Resource { group: GROUP.into(), version: "v1alpha1".into(), plural: "applications".into(), singular: "application".into(), kind: "Application".into(), namespaced: true, verbs: Vec::new(), short: vec!["app".into()] }
    }

    #[test]
    fn states_and_glyphs() {
        let app = json!({"status": {"sync": {"status": "OutOfSync", "revision": "0123456789abcdef0123456789abcdef01234567"}, "health": {"status": "Healthy"},
            "operationState": {"phase": "Running", "message": "waiting for healthy state of apps/Deployment/web"}}});
        assert_eq!(state_line(&app), "OutOfSync · Healthy @ 0123456, sync running: waiting for healthy state of apps/Deployment/web");
        assert_eq!(app_health(&app), Health::Warn);
        assert_eq!(health_of("Degraded", "Synced"), Health::Bad);
        assert_eq!(health_of("Healthy", "Synced"), Health::Ok);
        assert_eq!(state_line(&json!({})), "Unknown · Unknown");
        let failed = json!({"status": {"sync": {"status": "Synced"}, "health": {"status": "Healthy"}, "operationState": {"phase": "Failed", "message": "boom"}}});
        assert_eq!(app_health(&failed), Health::Warn);
    }

    #[test]
    fn a_manual_sync_carries_the_apps_options() {
        let app = json!({"spec": {"syncPolicy": {"syncOptions": ["CreateNamespace=true"]}}});
        assert_eq!(sync_operation(&app, "litek8s", true, false, Some("main")), json!({"initiatedBy": {"username": "litek8s"}, "sync": {"syncStrategy": {"hook": {}}, "syncOptions": ["CreateNamespace=true"], "prune": true, "revision": "main"}}));
        assert_eq!(sync_operation(&json!({}), "u", false, false, None), json!({"initiatedBy": {"username": "u"}, "sync": {"syncStrategy": {"hook": {}}}}));
        assert_eq!(sync_operation(&json!({}), "u", false, true, None), json!({"initiatedBy": {"username": "u"}, "sync": {"syncStrategy": {"hook": {"force": true}}}}));
    }

    #[test]
    fn app_of_apps_trees() {
        let parent = json!({"metadata": {"namespace": "argocd", "name": "root"}, "status": {"sync": {"status": "Synced"}, "health": {"status": "Healthy"},
            "resources": [{"group": GROUP, "kind": "Application", "namespace": "argocd", "name": "web", "status": "Synced", "health": {"status": "Healthy"}}]}});
        let child = json!({"metadata": {"namespace": "argocd", "name": "web"}, "status": {"sync": {"status": "OutOfSync"}, "health": {"status": "Degraded"},
            "resources": [{"group": "apps", "kind": "Deployment", "namespace": "web", "name": "web", "status": "OutOfSync", "health": {"status": "Degraded", "message": "exceeded its progress deadline"}},
                          {"kind": "ConfigMap", "namespace": "web", "name": "old", "status": "OutOfSync", "requiresPruning": true}]}});
        let find = |g: &str, k: &str| (g == "apps" && k == "Deployment").then(|| Resource { group: "apps".into(), version: "v1".into(), plural: "deployments".into(), singular: "deployment".into(), kind: "Deployment".into(), namespaced: true, verbs: Vec::new(), short: vec!["deploy".into()] });
        let nodes = tree(&[parent, child], &res(), &find, false);
        let lines: Vec<String> = nodes.iter().map(|n| format!("{} {} {} {}", n.depth, n.kind, n.name, n.status)).collect();
        assert_eq!(
            lines,
            [
                "0 app root Synced · Healthy",
                "1 app argocd/web OutOfSync · Degraded",
                "2 deployment web/web OutOfSync · Degraded: exceeded its progress deadline",
                "2 configmap web/old OutOfSync, to prune",
            ]
        );
        assert_eq!(nodes[1].health, Health::Bad);
        assert!(nodes[2].obj.is_some() && nodes[3].obj.is_none());
    }

    #[test]
    fn health_from_live_objects() {
        let job = json!({"kind": "Job", "status": {"failed": 2, "conditions": [{"type": "Failed", "status": "True", "message": "Job has reached the specified backoff limit"}]}});
        assert_eq!(live_health(&job), Some(("Degraded", "Job has reached the specified backoff limit".to_string())));
        let d = json!({"kind": "Deployment", "spec": {"replicas": 2}, "status": {"readyReplicas": 1, "updatedReplicas": 2, "conditions": [{"type": "Progressing", "reason": "ProgressDeadlineExceeded", "message": "ReplicaSet \"web-1\" has timed out progressing."}]}});
        assert_eq!(live_health(&d), Some(("Degraded", "ReplicaSet \"web-1\" has timed out progressing.".to_string())));
        let d = json!({"kind": "Deployment", "spec": {"replicas": 2}, "status": {"readyReplicas": 1, "updatedReplicas": 2}});
        assert_eq!(live_health(&d).unwrap().0, "Progressing");
        let pod = json!({"kind": "Pod", "status": {"phase": "Running", "containerStatuses": [{"state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}});
        assert_eq!(live_health(&pod), Some(("Degraded", "CrashLoopBackOff".to_string())));
        assert_eq!(live_health(&json!({"kind": "ConfigMap"})), None);
    }

    #[test]
    fn summaries_and_resources() {
        let app = json!({"metadata": {"namespace": "argocd", "name": "web"}, "spec": {"source": {"repoURL": "r", "path": "p"}, "syncPolicy": {"automated": {}}},
            "status": {"sync": {"status": "Synced", "revision": "abc"}, "health": {"status": "Healthy"}, "conditions": [{"type": "SyncError", "message": "x", "lastTransitionTime": "t"}],
                "resources": [{"kind": "ConfigMap", "namespace": "", "name": "c", "status": "Synced"}]}});
        let s = summary(&app);
        assert_eq!(s["project"], "default");
        assert_eq!(s["automated"], true);
        assert_eq!(s["conditions"], json!([{"type": "SyncError", "message": "x"}]));
        assert_eq!(resources(&app), vec![json!({"kind": "ConfigMap", "name": "c", "sync": "Synced"})]);
    }

    #[test]
    fn diff_lines_per_resource_live_against_git() {
        let app = json!({"metadata": {"name": "web"}, "status": {"sync": {"status": "OutOfSync"}}});
        assert!(diff_lines(&app, None)[0].contains("argocd app diff web"));
        let synced = json!({"status": {"sync": {"status": "Synced"}}});
        assert!(diff_lines(&synced, None)[0].starts_with("In sync"));
        let doc = json!({"revisions": ["0123456789abcdef0123456789abcdef01234567"], "resources": [
            {"kind": "Deployment", "namespace": "web", "name": "web", "status": "OutOfSync", "desired": {"spec": {"replicas": 3}}, "live": {"spec": {"replicas": 1}}},
            {"kind": "Service", "namespace": "web", "name": "web", "status": "Missing", "desired": {"spec": {"type": "ClusterIP"}}, "live": null},
            {"kind": "ConfigMap", "namespace": "web", "name": "big", "status": "OutOfSync", "omitted": true}]});
        let l = diff_lines(&app, Some(&doc));
        assert_eq!(l[0], "OutOfSync at 0123456; - live, + git");
        let at = |h: &str| l.iter().position(|x| x == h).unwrap_or_else(|| panic!("{h}: {l:?}"));
        let d = at("=== deployment web/web: OutOfSync");
        assert!(l[d + 1..].iter().take(5).any(|x| x == "-  replicas: 1") && l[d + 1..].iter().take(5).any(|x| x == "+  replicas: 3"), "{l:?}");
        let m = at("=== service web/web: Missing");
        assert!(l[m + 2..].iter().take(2).all(|x| x.starts_with('+')), "{l:?}");
        assert_eq!(l[at("=== configmap web/big: OutOfSync") + 1], "(too big to publish)");
    }
}
