//! ReplicaSet and ReplicationController: keep `spec.replicas` active pods
//! built from the template, adopting matching orphans and releasing pods
//! that no longer match the selector.

use std::time::Duration;

use serde_json::{Value, json};

use super::Requeue;
use super::util::{self, i};
use crate::kube::selector::LabelSelector;
use crate::kube::server::{ApiError, ApiState};

/// Pods created per reconcile, so a large scale-up does not stall the loop.
const CREATE_BURST: usize = 100;

pub(crate) fn reconcile(api: &ApiState, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    reconcile_in(api, "apps", "replicasets", ns, name)
}

pub(crate) fn reconcile_rc(api: &ApiState, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    reconcile_in(api, "", "replicationcontrollers", ns, name)
}

fn reconcile_in(api: &ApiState, group: &str, plural: &str, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    let Some(rs) = api.get_object(group, plural, ns, name) else {
        return Ok(None);
    };
    if util::is_deleting(&rs) {
        return Ok(None);
    }
    let selector = match rs.pointer("/spec/selector") {
        // ReplicationControllers select with a plain label map.
        Some(Value::Object(m)) if group.is_empty() => {
            LabelSelector::from_object(Some(&json!({"matchLabels": m})))
        }
        other => LabelSelector::from_object(other),
    };
    let pods = adopt_and_release(api, ns, &rs, selector.as_ref())?;
    let mut active: Vec<Value> = pods.into_iter().filter(util::is_active).collect();
    let desired = i(&rs, "/spec/replicas").unwrap_or(1).max(0) as usize;
    let min_ready = i(&rs, "/spec/minReadySeconds").unwrap_or(0);

    let mut requeue = None;
    let mut failure: Option<String> = None;
    if active.len() < desired {
        let template = rs.pointer("/spec/template").cloned().unwrap_or_else(|| json!({}));
        let missing = (desired - active.len()).min(CREATE_BURST);
        for _ in 0..missing {
            let mut pod = util::pod_from_template(&template, &rs, &[]);
            pod["metadata"]["generateName"] = json!(format!("{name}-"));
            match api.create_object("", "pods", Some(ns), pod) {
                Ok(_) => {}
                // Quota or admission said no: report it and retry later.
                Err(e) if e.is_forbidden() => {
                    failure = Some(e.message.clone());
                    requeue = Some(Duration::from_secs(5));
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        if desired - active.len() > missing {
            requeue = Some(Duration::from_millis(10));
        }
    } else if active.len() > desired {
        util::deletion_order(&mut active);
        let excess = active.len() - desired;
        for pod in active.drain(..excess) {
            match api.delete_named("", "pods", ns, &util::name(&pod)) {
                Ok(_) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e),
            }
        }
    }

    let template_labels = rs.pointer("/spec/template/metadata/labels").and_then(Value::as_object);
    let mut ready = 0;
    let mut available = 0;
    let mut fully_labeled = 0;
    for pod in &active {
        if util::is_ready(pod) {
            ready += 1;
        }
        let (is_available, wait) = util::availability(pod, min_ready);
        if is_available {
            available += 1;
        }
        if let Some(wait) = wait {
            requeue = Some(requeue.map_or(wait, |r: Duration| r.min(wait)));
        }
        let labels = util::labels_of(pod);
        if template_labels.is_none_or(|t| t.iter().all(|(k, v)| labels.get(k) == Some(v))) {
            fully_labeled += 1;
        }
    }
    let mut status = json!({
        "replicas": active.len(),
        "fullyLabeledReplicas": fully_labeled,
        "readyReplicas": ready,
        "availableReplicas": available,
        "observedGeneration": i(&rs, "/metadata/generation").unwrap_or(1),
    });
    let mut conditions = util::conditions_of(&rs);
    match &failure {
        // Keep the first message: each failed create names a new pod.
        Some(msg) => {
            if !util::condition_true(&rs, "ReplicaFailure") {
                util::set_condition(&mut conditions, "ReplicaFailure", "True", "FailedCreate", msg);
            }
        }
        None => conditions.retain(|c| util::s(c, "/type") != "ReplicaFailure"),
    }
    if !conditions.is_empty() {
        status["conditions"] = json!(conditions);
    }
    util::write_status(api, group, plural, &rs, status)?;
    Ok(requeue)
}

/// The pods `owner` controls after adopting orphans its selector matches
/// and releasing its pods that the selector no longer matches.
pub(crate) fn adopt_and_release(api: &ApiState, ns: &str, owner: &Value, selector: Option<&LabelSelector>) -> Result<Vec<Value>, ApiError> {
    let owner_uid = util::uid(owner);
    let matches = |p: &Value| selector.is_some_and(|s| !s.is_empty() && s.matches(p));
    let (pods, _) = api.store().list("", "pods", Some(ns));
    let mut owned = Vec::new();
    for mut pod in pods {
        if util::controlled_by(&pod, &owner_uid) {
            if matches(&pod) || util::is_deleting(&pod) {
                owned.push(pod);
                continue;
            }
            if let Some(refs) = pod.pointer_mut("/metadata/ownerReferences").and_then(Value::as_array_mut) {
                refs.retain(|r| r.get("uid").and_then(Value::as_str) != Some(owner_uid.as_str()));
            }
            match api.update_object("", "pods", pod) {
                Ok(_) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e),
            }
        } else if util::controller_of(&pod).is_none() && util::is_active(&pod) && matches(&pod) {
            let meta = pod["metadata"].as_object_mut().unwrap();
            let refs = meta.entry("ownerReferences").or_insert_with(|| json!([]));
            if !refs.is_array() {
                *refs = json!([]);
            }
            refs.as_array_mut().unwrap().push(util::owner_ref(owner));
            match api.update_object("", "pods", pod) {
                Ok(adopted) => owned.push(adopted),
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(owned)
}
