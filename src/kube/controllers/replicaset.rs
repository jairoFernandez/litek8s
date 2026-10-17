//! ReplicaSet: keep `spec.replicas` active pods built from the template.

use std::time::Duration;

use serde_json::{Value, json};

use super::Requeue;
use super::util::{self, i};
use crate::kube::server::{ApiError, ApiState};

/// Pods created per reconcile, so a large scale-up does not stall the loop.
const CREATE_BURST: usize = 100;

pub(crate) fn reconcile(api: &ApiState, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    let Some(rs) = api.get_object("apps", "replicasets", ns, name) else {
        return Ok(None);
    };
    if util::is_deleting(&rs) {
        return Ok(None);
    }
    let pods = util::owned(api, "", "pods", ns, &rs);
    let mut active: Vec<Value> = pods.into_iter().filter(util::is_active).collect();
    let desired = i(&rs, "/spec/replicas").unwrap_or(1).max(0) as usize;
    let min_ready = i(&rs, "/spec/minReadySeconds").unwrap_or(0);

    let mut requeue = None;
    if active.len() < desired {
        let template = rs.pointer("/spec/template").cloned().unwrap_or_else(|| json!({}));
        let missing = (desired - active.len()).min(CREATE_BURST);
        for _ in 0..missing {
            let mut pod = util::pod_from_template(&template, &rs, &[]);
            pod["metadata"]["generateName"] = json!(format!("{name}-"));
            api.create_object("", "pods", Some(ns), pod)?;
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
    if let Some(conditions) = rs.pointer("/status/conditions") {
        status["conditions"] = conditions.clone();
    }
    util::write_status(api, "apps", "replicasets", &rs, status)?;
    Ok(requeue)
}
