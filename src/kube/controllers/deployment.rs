//! Deployment: roll pod templates out through ReplicaSets.
//!
//! Each distinct template gets a ReplicaSet named `<deployment>-<hash>` with
//! the `pod-template-hash` label and a `deployment.kubernetes.io/revision`
//! annotation. RollingUpdate scales the new ReplicaSet up and the old ones
//! down within maxSurge/maxUnavailable; Recreate scales the old ones to zero
//! first. Rollback (`kubectl rollout undo`) needs nothing special: the
//! template is set back to an old ReplicaSet's, which is then adopted as the
//! new one under the next revision.

use serde_json::{Value, json};

use super::Requeue;
use super::util::{self, i, s};
use crate::kube::server::{ApiError, ApiState};

const HASH_LABEL: &str = "pod-template-hash";
const REVISION: &str = "deployment.kubernetes.io/revision";

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
            return write_status(api, &deploy, None, &all, replicas, max_unavailable);
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
    write_status(api, &deploy, Some(&new_rs), &old, replicas, max_unavailable)
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
    if let Some(rs) = new_rs {
        let rs_name = util::name(rs);
        let complete = updated == replicas && available >= replicas && total == replicas;
        if complete {
            util::set_condition(
                &mut conditions,
                "Progressing",
                "True",
                "NewReplicaSetAvailable",
                &format!("ReplicaSet \"{rs_name}\" has successfully progressed."),
            );
        } else {
            util::set_condition(
                &mut conditions,
                "Progressing",
                "True",
                "ReplicaSetUpdated",
                &format!("ReplicaSet \"{rs_name}\" is progressing."),
            );
        }
    }
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
    Ok(None)
}
