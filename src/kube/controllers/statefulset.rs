//! StatefulSet: pods with stable names `<set>-<ordinal>`, created in order
//! (OrderedReady) or all at once (Parallel), each with its own claims from
//! `volumeClaimTemplates`; rolling updates replace pods from the highest
//! ordinal down to the partition.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use super::Requeue;
use super::util::{self, i, s};
use crate::kube::server::{ApiError, ApiState};

const REVISION_LABEL: &str = "controller-revision-hash";

fn ordinal_of(set: &str, pod: &Value) -> Option<i64> {
    util::name(pod).strip_prefix(&format!("{set}-"))?.parse().ok()
}

fn revision_of(pod: &Value) -> String {
    util::labels_of(pod)
        .get(REVISION_LABEL)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub(crate) fn reconcile(api: &ApiState, ns: &str, name: &str) -> Result<Requeue, ApiError> {
    let Some(sts) = api.get_object("apps", "statefulsets", ns, name) else {
        return Ok(None);
    };
    if util::is_deleting(&sts) {
        return Ok(None);
    }
    let replicas = i(&sts, "/spec/replicas").unwrap_or(1).max(0);
    let parallel = s(&sts, "/spec/podManagementPolicy") == "Parallel";
    let on_delete = s(&sts, "/spec/updateStrategy/type") == "OnDelete";
    let partition = i(&sts, "/spec/updateStrategy/rollingUpdate/partition").unwrap_or(0).max(0);
    let min_ready = i(&sts, "/spec/minReadySeconds").unwrap_or(0);
    let template = sts.pointer("/spec/template").cloned().unwrap_or_else(|| json!({}));
    let update_revision = format!("{name}-{}", util::hash_of(&template));
    let current_revision = match s(&sts, "/status/currentRevision") {
        "" => update_revision.clone(),
        r => r.to_string(),
    };

    let mut pods: BTreeMap<i64, Value> = BTreeMap::new();
    let mut strays = Vec::new();
    for pod in util::owned(api, "", "pods", ns, &sts) {
        match ordinal_of(name, &pod) {
            Some(ord) => {
                pods.insert(ord, pod);
            }
            None => strays.push(pod),
        }
    }
    for pod in strays {
        let _ = api.delete_named("", "pods", ns, &util::name(&pod));
    }

    let healthy = |p: &Value| util::phase(p) == "Running" && util::is_ready(p);
    let mut requeue = None;
    // Create missing and replace failed pods, in order.
    let mut blocked = false;
    for ord in 0..replicas {
        match pods.get(&ord) {
            None => {
                if blocked {
                    break;
                }
                ensure_claims(api, &sts, ord)?;
                let revision = if ord < partition { &current_revision } else { &update_revision };
                let pod = pod_for(&sts, &template, ord, revision);
                api.create_object("", "pods", Some(ns), pod)?;
                if !parallel {
                    blocked = true;
                }
            }
            Some(pod) if util::is_terminal(pod) => {
                let _ = api.delete_named("", "pods", ns, &util::name(pod));
                if !parallel {
                    blocked = true;
                }
            }
            Some(pod) => {
                if !parallel && !healthy(pod) {
                    blocked = true;
                }
            }
        }
    }
    // Scale down from the highest ordinal.
    let extra: Vec<i64> = pods.keys().copied().filter(|o| *o >= replicas).collect();
    for ord in extra.iter().rev() {
        let pod = &pods[ord];
        let _ = api.delete_named("", "pods", ns, &util::name(pod));
        if !parallel {
            break;
        }
    }
    // Rolling update: once everything is in place and healthy, replace the
    // highest outdated pod at or above the partition.
    let all_present = (0..replicas).all(|o| pods.get(&o).is_some_and(healthy));
    if !on_delete && extra.is_empty() && all_present {
        if let Some((_, pod)) = pods
            .range(partition..replicas)
            .rev()
            .find(|(_, p)| revision_of(p) != update_revision)
        {
            let _ = api.delete_named("", "pods", ns, &util::name(pod));
        }
    }

    let live: Vec<&Value> = pods.values().filter(|p| util::is_active(p)).collect();
    let mut ready = 0;
    let mut available = 0;
    for pod in &live {
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
    }
    let updated = live.iter().filter(|p| revision_of(p) == update_revision).count() as i64;
    let mut current = live.iter().filter(|p| revision_of(p) == current_revision).count() as i64;
    let mut current_revision = current_revision;
    if updated == replicas && ready == replicas {
        current_revision = update_revision.clone();
        current = updated;
    }
    let status = json!({
        "observedGeneration": i(&sts, "/metadata/generation").unwrap_or(1),
        "replicas": live.len(),
        "readyReplicas": ready,
        "availableReplicas": available,
        "currentReplicas": current,
        "updatedReplicas": updated,
        "currentRevision": current_revision,
        "updateRevision": update_revision,
        "collisionCount": 0,
    });
    util::write_status(api, "apps", "statefulsets", &sts, status)?;
    Ok(requeue)
}

fn claim_name(template: &Value, set: &str, ord: i64) -> String {
    format!("{}-{set}-{ord}", util::name(template))
}

fn ensure_claims(api: &ApiState, sts: &Value, ord: i64) -> Result<(), ApiError> {
    let (ns, set) = util::ns_name(sts);
    let delete_with_set = s(sts, "/spec/persistentVolumeClaimRetentionPolicy/whenDeleted") == "Delete";
    for template in sts.pointer("/spec/volumeClaimTemplates").and_then(Value::as_array).into_iter().flatten() {
        let claim = claim_name(template, &set, ord);
        if api.get_object("", "persistentvolumeclaims", &ns, &claim).is_some() {
            continue;
        }
        let mut labels = template.pointer("/metadata/labels").cloned().unwrap_or_else(|| json!({}));
        if let Some(sel) = sts.pointer("/spec/selector/matchLabels").and_then(Value::as_object) {
            for (k, v) in sel {
                labels[k] = v.clone();
            }
        }
        let mut meta = json!({"name": claim, "labels": labels});
        if let Some(annotations) = template.pointer("/metadata/annotations") {
            meta["annotations"] = annotations.clone();
        }
        if delete_with_set {
            meta["ownerReferences"] = json!([util::owner_ref(sts)]);
        }
        let pvc = json!({
            "apiVersion": "v1",
            "kind": "PersistentVolumeClaim",
            "metadata": meta,
            "spec": template.get("spec").cloned().unwrap_or_else(|| json!({})),
        });
        match api.create_object("", "persistentvolumeclaims", Some(&ns), pvc) {
            Ok(_) => {}
            Err(e) if e.is_already_exists() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn pod_for(sts: &Value, template: &Value, ord: i64, revision: &str) -> Value {
    let (_, set) = util::ns_name(sts);
    let pod_name = format!("{set}-{ord}");
    let mut pod = util::pod_from_template(
        template,
        sts,
        &[
            ("statefulset.kubernetes.io/pod-name", pod_name.clone()),
            ("apps.kubernetes.io/pod-index", ord.to_string()),
            (REVISION_LABEL, revision.to_string()),
        ],
    );
    pod["metadata"]["name"] = json!(pod_name);
    pod["spec"]["hostname"] = json!(pod_name);
    let service = s(sts, "/spec/serviceName");
    if !service.is_empty() {
        pod["spec"]["subdomain"] = json!(service);
    }
    // Each claim template becomes a volume backed by this pod's own claim.
    let claims: Vec<(String, String)> = sts
        .pointer("/spec/volumeClaimTemplates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|t| (util::name(t), claim_name(t, &set, ord)))
        .collect();
    if !claims.is_empty() {
        if !pod["spec"].get("volumes").is_some_and(Value::is_array) {
            pod["spec"]["volumes"] = json!([]);
        }
        let volumes = pod["spec"]["volumes"].as_array_mut().unwrap();
        volumes.retain(|v| !claims.iter().any(|(n, _)| v.get("name").and_then(Value::as_str) == Some(n)));
        for (volume, claim) in claims {
            volumes.push(json!({"name": volume, "persistentVolumeClaim": {"claimName": claim}}));
        }
    }
    pod
}
