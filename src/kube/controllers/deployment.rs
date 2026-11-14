//! Deployment: roll pod templates out through ReplicaSets.
//!
//! Each distinct template gets a ReplicaSet named `<deployment>-<hash>` with
//! the `pod-template-hash` label and a `deployment.kubernetes.io/revision`
//! annotation. RollingUpdate scales the new ReplicaSet up and the old ones
//! down within maxSurge/maxUnavailable; Recreate scales the old ones to zero
//! first. Rollback (`kubectl rollout undo`) needs nothing special: the
//! template is set back to an old ReplicaSet's, which is then adopted as the
//! new one under the next revision.
//!
//! The `Progressing` condition follows upstream: it moves (lastUpdateTime)
//! whenever the rollout makes progress, and turns False with reason
//! `ProgressDeadlineExceeded` once it has not for `progressDeadlineSeconds`;
//! a timer re-checks at the deadline. A paused Deployment has no deadline.

use std::time::Duration;

use serde_json::{Value, json};

use super::Requeue;
use super::util::{self, i, s};
use crate::kube::server::{ApiError, ApiState};

const HASH_LABEL: &str = "pod-template-hash";
const REVISION: &str = "deployment.kubernetes.io/revision";
const PROGRESSING: &str = "Progressing";
const NEW_RS_AVAILABLE: &str = "NewReplicaSetAvailable";
const TIMED_OUT: &str = "ProgressDeadlineExceeded";
const PAUSED: &str = "DeploymentPaused";

fn revision(rs: &Value) -> i64 {
    rs.pointer("/metadata/annotations")
        .and_then(|a| a.get(REVISION))
        .and_then(Value::as_str)
        .and_then(|r| r.parse().ok())
        .unwrap_or(0)
}

fn spec_replicas(rs: &Value) -> i64 {
    i(rs, "/spec/replicas").unwrap_or(0)
}

fn status_int(rs: &Value, field: &str) -> i64 {
    i(rs, &format!("/status/{field}")).unwrap_or(0)
}

pub(crate) fn reconcile(api: &ApiState, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    let Some(deploy) = api.get_object("apps", "deployments", ns, name) else {
        return Ok(None);
    };
    if util::is_deleting(&deploy) {
        return Ok(None);
    }
    let replicas = i(&deploy, "/spec/replicas").unwrap_or(1).max(0);
    let template = deploy.pointer("/spec/template").cloned().unwrap_or_else(|| json!({}));
    let paused = deploy.pointer("/spec/paused").and_then(Value::as_bool) == Some(true);
    let recreate = s(&deploy, "/spec/strategy/type") == "Recreate";
    let max_surge = if recreate {
        0
    } else {
        util::int_or_percent(deploy.pointer("/spec/strategy/rollingUpdate/maxSurge"), replicas, true, 1)
    };
    let mut max_unavailable = if recreate {
        replicas
    } else {
        util::int_or_percent(deploy.pointer("/spec/strategy/rollingUpdate/maxUnavailable"), replicas, false, 0)
    };
    if !recreate && max_surge == 0 && max_unavailable == 0 {
        max_unavailable = 1;
    }

    let mut all = adopt_replicasets(api, ns, &deploy)?;
    all.sort_by_key(revision);
    let new_index = all
        .iter()
        .position(|rs| util::template_without_label(rs.pointer("/spec/template").unwrap_or(&Value::Null), HASH_LABEL) == template);
    let max_old_revision = all
        .iter()
        .enumerate()
        .filter(|(idx, _)| Some(*idx) != new_index)
        .map(|(_, rs)| revision(rs))
        .max()
        .unwrap_or(0);

    let mut found = if paused { Found::Paused } else { Found::Existing };
    let mut new_rs = match new_index {
        Some(idx) => {
            let rs = all.remove(idx);
            if revision(&rs) <= max_old_revision {
                // Rolled back to an old template: it becomes the newest revision.
                util::ensure_annotation(api, "apps", "replicasets", &rs, REVISION, &(max_old_revision + 1).to_string())?
            } else {
                rs
            }
        }
        None if paused => {
            return write_status(api, &deploy, None, &all, replicas, max_unavailable, Found::Paused);
        }
        None => {
            let old_total: i64 = all.iter().map(spec_replicas).sum();
            let initial = if recreate {
                if old_total > 0 { 0 } else { replicas }
            } else if old_total == 0 {
                replicas
            } else {
                (replicas + max_surge - old_total).clamp(0, replicas)
            };
            found = Found::Created;
            create_replicaset(api, &deploy, &template, initial, max_old_revision + 1, replicas + max_surge)?
        }
    };
    let mut old = all;

    if !paused && !recreate {
        proportional_scale(api, &deploy, &mut new_rs, &mut old, replicas, max_surge)?;
    }
    if !paused {
        if recreate {
            rollout_recreate(api, ns, &deploy, &mut new_rs, &old, replicas)?;
        } else {
            rollout_rolling(api, &mut new_rs, &old, replicas, max_surge, max_unavailable)?;
        }
        cleanup_history(api, &deploy, &old)?;
    }

    let revision_str = revision(&new_rs).to_string();
    let deploy = util::ensure_annotation(api, "apps", "deployments", &deploy, REVISION, &revision_str)?;
    write_status(api, &deploy, Some(&new_rs), &old, replicas, max_unavailable, found)
}

/// How this reconcile came by the new ReplicaSet.
#[derive(Clone, Copy, PartialEq)]
enum Found {
    Existing,
    Created,
    /// Paused: no rollout, so no progress to judge.
    Paused,
}

/// The ReplicaSets the Deployment controls, after adopting orphans its
/// selector matches and releasing those it no longer matches.
fn adopt_replicasets(api: &ApiState, ns: &str, deploy: &Value) -> Result<Vec<Value>, ApiError> {
    let selector = crate::kube::selector::LabelSelector::from_object(deploy.pointer("/spec/selector"));
    let matches = |rs: &Value| selector.as_ref().is_some_and(|s| !s.is_empty() && s.matches(rs));
    let uid = util::uid(deploy);
    let (sets, _) = api.store().list("apps", "replicasets", Some(ns));
    let mut owned = Vec::new();
    for mut rs in sets {
        if util::controlled_by(&rs, &uid) {
            if matches(&rs) || util::is_deleting(&rs) {
                owned.push(rs);
                continue;
            }
            if let Some(refs) = rs.pointer_mut("/metadata/ownerReferences").and_then(Value::as_array_mut) {
                refs.retain(|r| r.get("uid").and_then(Value::as_str) != Some(uid.as_str()));
            }
            let _ = api.update_object("apps", "replicasets", rs);
        } else if util::controller_of(&rs).is_none() && !util::is_deleting(&rs) && matches(&rs) {
            let meta = rs["metadata"].as_object_mut().unwrap();
            let refs = meta.entry("ownerReferences").or_insert_with(|| json!([]));
            if !refs.is_array() {
                *refs = json!([]);
            }
            refs.as_array_mut().unwrap().push(util::owner_ref(deploy));
            match api.update_object("apps", "replicasets", rs) {
                Ok(adopted) => owned.push(adopted),
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(owned)
}

/// When the Deployment is resized mid-rollout, resize every active
/// ReplicaSet in proportion to its share, as the Deployment controller
/// does (using each set's `max-replicas` annotation as its old size).
fn proportional_scale(api: &ApiState, deploy: &Value, new_rs: &mut Value, old: &mut [Value], replicas: i64, max_surge: i64) -> Result<(), ApiError> {
    const DESIRED: &str = "deployment.kubernetes.io/desired-replicas";
    const MAX: &str = "deployment.kubernetes.io/max-replicas";
    let ann = |rs: &Value, k: &str| rs.pointer("/metadata/annotations").and_then(|a| a.get(k)).and_then(Value::as_str).and_then(|v| v.parse::<i64>().ok());
    let active = old.iter().filter(|rs| spec_replicas(rs) > 0).count() + usize::from(spec_replicas(new_rs) > 0);
    let allowed = if replicas > 0 { replicas + max_surge } else { 0 };
    let resized = std::iter::once(&*new_rs).chain(old.iter()).any(|rs| spec_replicas(rs) > 0 && ann(rs, DESIRED).is_some_and(|d| d != replicas));
    if active > 1 && resized {
        let total: i64 = old.iter().map(spec_replicas).sum::<i64>() + spec_replicas(new_rs);
        let to_add = allowed - total;
        if to_add != 0 {
            // Largest first (newest first when growing).
            let mut sets: Vec<&mut Value> = std::iter::once(new_rs as &mut Value).chain(old.iter_mut()).filter(|rs| spec_replicas(rs) > 0).collect();
            sets.sort_by_key(|rs| -spec_replicas(rs));
            let mut added = 0;
            let mut targets: Vec<i64> = Vec::new();
            for rs in sets.iter() {
                let size = spec_replicas(rs);
                let old_size = ann(rs, MAX).unwrap_or(total).max(1);
                let fraction = ((size * allowed) as f64 / old_size as f64).round() as i64 - size;
                let room = to_add - added;
                let share = if to_add > 0 { fraction.min(room).max(0) } else { fraction.max(room).min(0) };
                added += share;
                targets.push(size + share);
            }
            if let Some(first) = targets.first_mut() {
                *first += to_add - added;
            }
            for (rs, target) in sets.into_iter().zip(targets) {
                scale(api, rs, target.max(0))?;
            }
        }
    }
    // Record the size every set was scaled for.
    for rs in std::iter::once(new_rs as &mut Value).chain(old.iter_mut()) {
        if ann(rs, DESIRED) != Some(replicas) || ann(rs, MAX) != Some(allowed) {
            let mut updated = rs.clone();
            if !updated["metadata"].get("annotations").is_some_and(Value::is_object) {
                updated["metadata"]["annotations"] = json!({});
            }
            updated["metadata"]["annotations"][DESIRED] = json!(replicas.to_string());
            updated["metadata"]["annotations"][MAX] = json!(allowed.to_string());
            *rs = api.update_object("apps", "replicasets", updated)?;
        }
    }
    let _ = deploy;
    Ok(())
}

fn create_replicaset(
    api: &ApiState,
    deploy: &Value,
    template: &Value,
    replicas: i64,
    revision: i64,
    max_replicas: i64,
) -> Result<Value, ApiError> {
    let (ns, dname) = util::ns_name(deploy);
    let hash = util::hash_of(template);
    let mut rs_template = template.clone();
    if !rs_template.get("metadata").is_some_and(Value::is_object) {
        rs_template["metadata"] = json!({});
    }
    if !rs_template["metadata"].get("labels").is_some_and(Value::is_object) {
        rs_template["metadata"]["labels"] = json!({});
    }
    rs_template["metadata"]["labels"][HASH_LABEL] = json!(hash);
    let mut selector = deploy.pointer("/spec/selector").cloned().unwrap_or_else(|| json!({}));
    if !selector.get("matchLabels").is_some_and(Value::is_object) {
        selector["matchLabels"] = json!({});
    }
    selector["matchLabels"][HASH_LABEL] = json!(hash);
    let mut labels = rs_template["metadata"]["labels"].clone();
    labels[HASH_LABEL] = json!(hash);
    let rs = json!({
        "apiVersion": "apps/v1",
        "kind": "ReplicaSet",
        "metadata": {
            "name": format!("{dname}-{hash}"),
            "labels": labels,
            "annotations": {
                REVISION: revision.to_string(),
                "deployment.kubernetes.io/desired-replicas": i(deploy, "/spec/replicas").unwrap_or(1).to_string(),
                "deployment.kubernetes.io/max-replicas": max_replicas.to_string(),
            },
            "ownerReferences": [util::owner_ref(deploy)],
        },
        "spec": {
            "replicas": replicas,
            "minReadySeconds": i(deploy, "/spec/minReadySeconds").unwrap_or(0),
            "selector": selector,
            "template": rs_template,
        },
    });
    api.create_object("apps", "replicasets", Some(&ns), rs)
}

fn scale(api: &ApiState, rs: &mut Value, replicas: i64) -> Result<(), ApiError> {
    if spec_replicas(rs) == replicas {
        return Ok(());
    }
    let mut updated = rs.clone();
    updated["spec"]["replicas"] = json!(replicas);
    *rs = api.update_object("apps", "replicasets", updated)?;
    Ok(())
}

fn rollout_rolling(
    api: &ApiState,
    new_rs: &mut Value,
    old: &[Value],
    replicas: i64,
    max_surge: i64,
    max_unavailable: i64,
) -> Result<(), ApiError> {
    let old_active: i64 = old.iter().map(spec_replicas).sum();
    if old_active == 0 {
        // Nothing to roll from: plain scaling.
        return scale(api, new_rs, replicas);
    }
    // Scale up the new ReplicaSet within the surge budget.
    let total = old_active + spec_replicas(new_rs);
    let max_total = replicas + max_surge;
    if total < max_total && spec_replicas(new_rs) < replicas {
        let target = (spec_replicas(new_rs) + max_total - total).min(replicas);
        scale(api, new_rs, target)?;
    }
    // Scale down old ReplicaSets within the availability budget.
    let min_available = replicas - max_unavailable;
    let all_pods = old_active + spec_replicas(new_rs);
    let new_unavailable = (spec_replicas(new_rs) - status_int(new_rs, "availableReplicas")).max(0);
    let mut budget = all_pods - min_available - new_unavailable;
    if budget <= 0 {
        return Ok(());
    }
    let mut old: Vec<Value> = old.to_vec();
    // Unhealthy old pods go first: removing them costs no availability.
    for rs in old.iter_mut() {
        if budget <= 0 {
            break;
        }
        let unhealthy = spec_replicas(rs) - status_int(rs, "availableReplicas");
        if spec_replicas(rs) == 0 || unhealthy <= 0 {
            continue;
        }
        let cut = unhealthy.min(budget);
        let target = spec_replicas(rs) - cut;
        scale(api, rs, target)?;
        budget -= cut;
    }
    let available: i64 = old.iter().map(|rs| status_int(rs, "availableReplicas")).sum::<i64>()
        + status_int(new_rs, "availableReplicas");
    let mut budget = available - min_available;
    for rs in old.iter_mut() {
        if budget <= 0 {
            break;
        }
        let cut = spec_replicas(rs).min(budget);
        if cut > 0 {
            let target = spec_replicas(rs) - cut;
            scale(api, rs, target)?;
            budget -= cut;
        }
    }
    Ok(())
}

fn rollout_recreate(
    api: &ApiState,
    ns: &str,
    _deploy: &Value,
    new_rs: &mut Value,
    old: &[Value],
    replicas: i64,
) -> Result<(), ApiError> {
    let mut old_running = false;
    for rs in old {
        let mut rs = rs.clone();
        if spec_replicas(&rs) > 0 {
            scale(api, &mut rs, 0)?;
            old_running = true;
        }
        if !util::owned(api, "", "pods", ns, &rs).iter().all(|p| !util::is_active(p)) {
            old_running = true;
        }
    }
    if !old_running {
        scale(api, new_rs, replicas)?;
    }
    Ok(())
}

/// Delete scaled-down ReplicaSets beyond `revisionHistoryLimit`.
fn cleanup_history(api: &ApiState, deploy: &Value, old: &[Value]) -> Result<(), ApiError> {
    let limit = i(deploy, "/spec/revisionHistoryLimit").unwrap_or(10).max(0) as usize;
    let idle: Vec<&Value> = old
        .iter()
        .filter(|rs| spec_replicas(rs) == 0 && status_int(rs, "replicas") == 0)
        .collect();
    if idle.len() <= limit {
        return Ok(());
    }
    // `old` is sorted by revision, oldest first.
    for rs in &idle[..idle.len() - limit] {
        let (ns, name) = util::ns_name(rs);
        match api.delete_named("apps", "replicasets", &ns, &name) {
            Ok(_) => {}
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_status(
    api: &ApiState,
    deploy: &Value,
    new_rs: Option<&Value>,
    old: &[Value],
    replicas: i64,
    max_unavailable: i64,
    found: Found,
) -> Result<Requeue, ApiError> {
    let all: Vec<&Value> = old.iter().chain(new_rs).collect();
    let sum = |field: &str| all.iter().map(|rs| status_int(rs, field)).sum::<i64>();
    let total = sum("replicas");
    let ready = sum("readyReplicas");
    let available = sum("availableReplicas");
    let updated = new_rs.map(|rs| status_int(rs, "replicas")).unwrap_or(0);
    let mut conditions = util::conditions_of(deploy);
    if available >= replicas - max_unavailable {
        util::set_condition(&mut conditions, "Available", "True", "MinimumReplicasAvailable", "Deployment has minimum availability.");
    } else {
        util::set_condition(&mut conditions, "Available", "False", "MinimumReplicasUnavailable", "Deployment does not have minimum availability.");
    }
    let requeue = progressing(
        deploy,
        &mut conditions,
        new_rs.map(util::name).as_deref(),
        found,
        Counts { total, updated, ready, available, replicas },
    );
    // Available first, as Kubernetes orders them.
    conditions.sort_by_key(|c| if c["type"] == "Available" { 0 } else { 1 });
    let status = json!({
        "observedGeneration": i(deploy, "/metadata/generation").unwrap_or(1),
        "replicas": total,
        "updatedReplicas": updated,
        "readyReplicas": ready,
        "availableReplicas": available,
        "unavailableReplicas": (replicas - available).max(0),
        "conditions": conditions,
    });
    util::write_status(api, "apps", "deployments", deploy, status)?;
    Ok(requeue)
}

/// The replica counts of a Deployment's new status.
#[derive(Clone, Copy)]
struct Counts {
    total: i64,
    updated: i64,
    ready: i64,
    available: i64,
    replicas: i64,
}

/// Set the `Progressing` condition as upstream's syncRolloutStatus does and
/// say when to look again for the progress deadline.
fn progressing(deploy: &Value, conditions: &mut Vec<Value>, rs_name: Option<&str>, found: Found, now: Counts) -> Requeue {
    let deadline = i(deploy, "/spec/progressDeadlineSeconds").filter(|d| *d < i32::MAX as i64);
    let Some(deadline) = deadline else {
        conditions.retain(|c| c["type"] != PROGRESSING);
        return None;
    };
    let reason_of = |conditions: &[Value]| {
        conditions.iter().find(|c| c["type"] == PROGRESSING).map(|c| s(c, "/reason").to_string())
    };
    if found == Found::Paused {
        if reason_of(conditions).as_deref() != Some(PAUSED) {
            set_progressing(conditions, "Unknown", PAUSED, "Deployment is paused", false);
        }
        return None;
    }
    let Some(rs_name) = rs_name else { return None };
    match reason_of(conditions).as_deref() {
        Some(PAUSED) => set_progressing(conditions, "Unknown", "DeploymentResumed", "Deployment is resumed", false),
        _ if found == Found::Created => {
            set_progressing(conditions, "True", "NewReplicaSetCreated", &format!("Created new replica set \"{rs_name}\""), false);
        }
        None => set_progressing(conditions, "True", "FoundNewReplicaSet", &format!("Found new replica set \"{rs_name}\""), false),
        _ => {}
    }

    // (observedGeneration is always the current generation here.)
    let complete = now.updated == now.replicas && now.total == now.replicas && now.available >= now.replicas;
    let current = reason_of(conditions);
    let already_complete = now.total == now.updated && current.as_deref() == Some(NEW_RS_AVAILABLE);
    if !already_complete {
        let was = |field: &str| i(deploy, &format!("/status/{field}")).unwrap_or(0);
        let made_progress = now.updated > was("updatedReplicas")
            || now.total - now.updated < was("replicas") - was("updatedReplicas")
            || now.ready > was("readyReplicas")
            || now.available > was("availableReplicas");
        if complete {
            set_progressing(conditions, "True", NEW_RS_AVAILABLE, &format!("ReplicaSet \"{rs_name}\" has successfully progressed."), false);
        } else if made_progress {
            set_progressing(conditions, "True", "ReplicaSetUpdated", &format!("ReplicaSet \"{rs_name}\" is progressing."), true);
        } else if timed_out(conditions, deadline) == Some(Duration::ZERO) {
            set_progressing(conditions, "False", TIMED_OUT, &format!("ReplicaSet \"{rs_name}\" has timed out progressing."), false);
        }
    }
    if complete {
        return None;
    }
    // Upstream rounds up and adds a second, so the re-check sees the deadline passed.
    timed_out(conditions, deadline).filter(|left| !left.is_zero()).map(|left| Duration::from_secs(left.as_secs() + 1))
}

/// How long until the `Progressing` condition times out: zero once it has
/// (or did), None when it cannot (none, or complete).
fn timed_out(conditions: &[Value], deadline: i64) -> Option<Duration> {
    let c = conditions.iter().find(|c| c["type"] == PROGRESSING)?;
    match s(c, "/reason") {
        NEW_RS_AVAILABLE => None,
        TIMED_OUT => Some(Duration::ZERO),
        _ => {
            let from = util::parse_time(s(c, "/lastUpdateTime"))?;
            let left = from + chrono::Duration::seconds(deadline) - util::now();
            Some(if left.num_milliseconds() < 0 { Duration::ZERO } else { left.to_std().unwrap_or_default() })
        }
    }
}

/// upstream's SetDeploymentCondition: nothing changes when status and
/// reason are the same, unless `moved` (progress was made: lastUpdateTime
/// moves on); lastTransitionTime only changes with the status.
fn set_progressing(conditions: &mut Vec<Value>, status: &str, reason: &str, message: &str, moved: bool) {
    let now = util::rfc3339(util::now());
    let current = conditions.iter().position(|c| c["type"] == PROGRESSING);
    let mut transition = now.clone();
    if let Some(idx) = current {
        let c = &conditions[idx];
        if s(c, "/status") == status && s(c, "/reason") == reason && !moved {
            return;
        }
        if s(c, "/status") == status {
            transition = s(c, "/lastTransitionTime").to_string();
        }
        conditions.remove(idx);
    }
    conditions.push(json!({
        "type": PROGRESSING,
        "status": status,
        "reason": reason,
        "message": message,
        "lastUpdateTime": now,
        "lastTransitionTime": transition,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api() -> ApiState {
        ApiState::open(None, "127.0.0.1:0".into(), false).unwrap()
    }

    fn create(api: &ApiState, spec: Value) {
        let mut spec = spec;
        spec["selector"] = json!({"matchLabels": {"app": "web"}});
        spec["template"] = json!({"metadata": {"labels": {"app": "web"}}, "spec": {"containers": [{"name": "web", "image": "registry.invalid/none:1"}]}});
        let d = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"}, "spec": spec});
        api.create_object("apps", "deployments", Some("default"), d).unwrap();
    }

    fn deploy(api: &ApiState) -> Value {
        api.get_object("apps", "deployments", "default", "web").unwrap()
    }

    fn sync(api: &ApiState) -> Requeue {
        reconcile(api, "default", "web").unwrap()
    }

    fn progressing_of(api: &ApiState) -> Value {
        util::condition(&deploy(api), PROGRESSING).cloned().expect("Progressing condition")
    }

    fn new_rs(api: &ApiState) -> Value {
        api.store().list("apps", "replicasets", Some("default")).0.into_iter().next().expect("ReplicaSet")
    }

    /// What the ReplicaSet controller would report for its pods.
    fn rs_status(api: &ApiState, replicas: i64, ready: i64) {
        let status = json!({"replicas": replicas, "readyReplicas": ready, "availableReplicas": ready});
        util::write_status(api, "apps", "replicasets", &new_rs(api), status).unwrap();
    }

    /// Move the condition's lastUpdateTime back, as if no progress had been
    /// made for that long.
    fn age_progressing(api: &ApiState, seconds: i64) {
        let mut d = deploy(api);
        let then = util::rfc3339(util::now() - chrono::Duration::seconds(seconds));
        for c in d["status"]["conditions"].as_array_mut().unwrap() {
            if c["type"] == PROGRESSING {
                c["lastUpdateTime"] = json!(then);
                c["lastTransitionTime"] = json!(then);
            }
        }
        api.update_status("apps", "deployments", d).unwrap();
    }

    #[test]
    fn new_rollout_sets_progressing_and_a_timer_at_the_deadline() {
        let api = api();
        create(&api, json!({"replicas": 2, "progressDeadlineSeconds": 30}));
        let requeue = sync(&api).expect("deadline timer");
        assert!(requeue > Duration::from_secs(29) && requeue <= Duration::from_secs(31), "{requeue:?}");
        let c = progressing_of(&api);
        assert_eq!((s(&c, "/status"), s(&c, "/reason")), ("True", "NewReplicaSetCreated"));
        assert_eq!(s(&c, "/message"), format!("Created new replica set \"{}\"", util::name(&new_rs(&api))));
        // Nothing moved: the condition stays as it is.
        sync(&api).expect("deadline timer");
        assert_eq!(progressing_of(&api), c);
    }

    #[test]
    fn no_progress_for_the_deadline_times_out() {
        let api = api();
        create(&api, json!({"replicas": 1, "progressDeadlineSeconds": 10}));
        sync(&api);
        rs_status(&api, 1, 0);
        sync(&api);
        age_progressing(&api, 11);
        assert_eq!(sync(&api), None, "no timer once timed out");
        let c = progressing_of(&api);
        let rs = util::name(&new_rs(&api));
        assert_eq!((s(&c, "/status"), s(&c, "/reason")), ("False", "ProgressDeadlineExceeded"));
        assert_eq!(s(&c, "/message"), format!("ReplicaSet \"{rs}\" has timed out progressing."));
        assert!(util::parse_time(s(&c, "/lastTransitionTime")).unwrap() > util::now() - chrono::Duration::seconds(5));

        // Pods becoming ready later is progress again.
        rs_status(&api, 1, 1);
        assert_eq!(sync(&api), None);
        let c = progressing_of(&api);
        assert_eq!((s(&c, "/status"), s(&c, "/reason")), ("True", "NewReplicaSetAvailable"));
    }

    #[test]
    fn progress_moves_the_deadline() {
        let api = api();
        create(&api, json!({"replicas": 3, "progressDeadlineSeconds": 10}));
        sync(&api);
        rs_status(&api, 3, 0);
        sync(&api);
        age_progressing(&api, 8);
        let before = progressing_of(&api);
        rs_status(&api, 3, 1);
        let requeue = sync(&api).expect("deadline timer");
        assert!(requeue > Duration::from_secs(9), "{requeue:?}");
        let c = progressing_of(&api);
        assert_eq!((s(&c, "/status"), s(&c, "/reason")), ("True", "ReplicaSetUpdated"));
        assert!(s(&c, "/lastUpdateTime") > s(&before, "/lastUpdateTime"));
        assert_eq!(s(&c, "/lastTransitionTime"), s(&before, "/lastTransitionTime"), "still True");
        // Aged again without progress: times out.
        age_progressing(&api, 11);
        sync(&api);
        assert_eq!(s(&progressing_of(&api), "/reason"), "ProgressDeadlineExceeded");
    }

    #[test]
    fn a_finished_rollout_has_no_deadline() {
        let api = api();
        create(&api, json!({"replicas": 1, "progressDeadlineSeconds": 10}));
        sync(&api);
        rs_status(&api, 1, 1);
        assert_eq!(sync(&api), None);
        age_progressing(&api, 60);
        assert_eq!(sync(&api), None);
        assert_eq!(s(&progressing_of(&api), "/reason"), "NewReplicaSetAvailable");
    }

    #[test]
    fn paused_deployments_do_not_time_out() {
        let api = api();
        create(&api, json!({"replicas": 1, "progressDeadlineSeconds": 10}));
        sync(&api);
        let mut d = deploy(&api);
        d["spec"]["paused"] = json!(true);
        api.update_object("apps", "deployments", d).unwrap();
        assert_eq!(sync(&api), None);
        let c = progressing_of(&api);
        assert_eq!((s(&c, "/status"), s(&c, "/reason")), ("Unknown", "DeploymentPaused"));
        age_progressing(&api, 60);
        assert_eq!(sync(&api), None);
        assert_eq!(s(&progressing_of(&api), "/reason"), "DeploymentPaused");

        // Resuming starts the deadline afresh.
        let mut d = deploy(&api);
        d["spec"]["paused"] = json!(false);
        api.update_object("apps", "deployments", d).unwrap();
        let requeue = sync(&api).expect("deadline timer");
        assert!(requeue > Duration::from_secs(9), "{requeue:?}");
        let c = progressing_of(&api);
        assert_eq!((s(&c, "/status"), s(&c, "/reason")), ("Unknown", "DeploymentResumed"));
    }

    #[test]
    fn existing_replicaset_without_condition_is_found() {
        let api = api();
        create(&api, json!({"replicas": 1, "progressDeadlineSeconds": 10}));
        sync(&api);
        let mut d = deploy(&api);
        d["status"]["conditions"] = json!([]);
        api.update_status("apps", "deployments", d).unwrap();
        sync(&api);
        assert_eq!(s(&progressing_of(&api), "/reason"), "FoundNewReplicaSet");
    }

    #[test]
    fn no_deadline_no_condition() {
        let api = api();
        create(&api, json!({"replicas": 1, "progressDeadlineSeconds": i32::MAX}));
        assert_eq!(sync(&api), None);
        assert!(util::condition(&deploy(&api), PROGRESSING).is_none());
    }
}
