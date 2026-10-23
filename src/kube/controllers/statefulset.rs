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
    let hash = util::hash_of(&template);
    let owned_pods = util::owned(api, "", "pods", ns, &sts);
    let mut in_use: Vec<String> = owned_pods.iter().map(revision_of).collect();
    in_use.push(s(&sts, "/status/currentRevision").to_string());
    let update_revision = super::history::sync(api, &sts, &template, &hash, &in_use)?.name;
    let current_revision = match s(&sts, "/status/currentRevision") {
        "" => update_revision.clone(),
        r => r.to_string(),
    };
    // Pods below the partition are recreated from the current revision.
    let current_template = if current_revision == update_revision {
        template.clone()
    } else {
        super::history::template_of(api, ns, &current_revision).unwrap_or_else(|| template.clone())
    };

    let mut pods: BTreeMap<i64, Value> = BTreeMap::new();
    let mut strays = Vec::new();
    for pod in owned_pods {
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

    let healthy = |p: &Value| util::phase(p) == "Running" && util::is_ready(p) && !util::is_deleting(p);
    let mut requeue = None;
    // OrderedReady: nothing moves while a pod is terminating.
    let terminating = pods.values().any(util::is_deleting);
    // Create missing and replace failed pods, in order.
    let mut blocked = !parallel && terminating;
    for ord in 0..replicas {
        match pods.get(&ord) {
            None => {
                if blocked {
                    break;
                }
                ensure_claims(api, &sts, ord)?;
                let (revision, tmpl) = if ord < partition { (&current_revision, &current_template) } else { (&update_revision, &template) };
                let pod = pod_for(&sts, tmpl, ord, revision);
                api.create_object("", "pods", Some(ns), pod)?;
                if !parallel {
                    blocked = true;
                }
            }
            Some(pod) if util::is_terminal(pod) && !util::is_deleting(pod) => {
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
    // Scale down from the highest ordinal; OrderedReady only while every
    // pod is healthy and none is terminating.
    let extra: Vec<i64> = pods.keys().copied().filter(|o| *o >= replicas).collect();
    let may_scale_down = parallel || (!terminating && pods.values().all(healthy));
    for ord in extra.iter().rev().filter(|_| may_scale_down) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn api() -> ApiState {
        ApiState::open(None, "127.0.0.1:0".into(), false).unwrap()
    }

    fn sts(spec: Value) -> Value {
        let mut spec = spec;
        spec["selector"] = json!({"matchLabels": {"app": "web"}});
        spec["template"] = json!({"metadata": {"labels": {"app": "web"}}, "spec": {"containers": [{"name": "c", "image": "nginx:1"}]}});
        json!({"apiVersion": "apps/v1", "kind": "StatefulSet", "metadata": {"name": "web"}, "spec": spec})
    }

    fn pod_names(api: &ApiState) -> Vec<String> {
        api.store().list("", "pods", Some("default")).0.iter().filter(|p| !util::is_deleting(p)).map(util::name).collect()
    }

    fn pod(api: &ApiState, name: &str) -> Value {
        api.get_object("", "pods", "default", name).unwrap()
    }

    fn mark_ready(api: &ApiState, name: &str) {
        let p = pod(api, name);
        util::write_status(api, "", "pods", &p, json!({"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]})).unwrap();
    }

    fn set_spec(api: &ApiState, f: impl FnOnce(&mut Value)) {
        let mut s = api.get_object("apps", "statefulsets", "default", "web").unwrap();
        f(&mut s["spec"]);
        api.update_object("apps", "statefulsets", s).unwrap();
    }

    fn status(api: &ApiState) -> Value {
        api.get_object("apps", "statefulsets", "default", "web").unwrap()["status"].clone()
    }

    /// Reconcile, then make every pod Running and Ready, until nothing changes.
    fn settle(api: &ApiState) {
        for _ in 0..20 {
            let before = pod_names(api);
            reconcile(api, "default", "web").unwrap();
            for name in pod_names(api) {
                mark_ready(api, &name);
            }
            if pod_names(api) == before {
                reconcile(api, "default", "web").unwrap();
                if pod_names(api) == before {
                    return;
                }
            }
        }
        panic!("statefulset did not settle: {:?}", pod_names(api));
    }

    #[test]
    fn ordinals_come_from_the_set_name() {
        let named = |n: &str| json!({"metadata": {"name": n}});
        assert_eq!(ordinal_of("web", &named("web-3")), Some(3));
        assert_eq!(ordinal_of("web", &named("web-x")), None);
        assert_eq!(ordinal_of("web", &named("db-1")), None);
        assert_eq!(ordinal_of("web", &named("web-a-1")), None);
    }

    #[test]
    fn ordered_ready_creates_one_pod_at_a_time() {
        let api = api();
        api.create_object("apps", "statefulsets", Some("default"), sts(json!({"replicas": 3, "serviceName": "web"}))).unwrap();
        reconcile(&api, "default", "web").unwrap();
        assert_eq!(pod_names(&api), ["web-0"]);
        // Not ready yet: web-1 waits.
        reconcile(&api, "default", "web").unwrap();
        assert_eq!(pod_names(&api), ["web-0"]);
        let p = pod(&api, "web-0");
        assert_eq!(s(&p, "/spec/hostname"), "web-0");
        assert_eq!(s(&p, "/spec/subdomain"), "web");
        assert_eq!(s(&p, "/metadata/labels/statefulset.kubernetes.io~1pod-name"), "web-0");
        assert_eq!(s(&p, "/metadata/labels/apps.kubernetes.io~1pod-index"), "0");
        assert!(!revision_of(&p).is_empty());

        mark_ready(&api, "web-0");
        reconcile(&api, "default", "web").unwrap();
        assert_eq!(pod_names(&api), ["web-0", "web-1"]);
        settle(&api);
        assert_eq!(pod_names(&api), ["web-0", "web-1", "web-2"]);
        let st = status(&api);
        assert_eq!(st["replicas"], 3);
        assert_eq!(st["readyReplicas"], 3);
        assert_eq!(st["updatedReplicas"], 3);
        assert_eq!(st["currentRevision"], st["updateRevision"]);
    }

    #[test]
    fn parallel_creates_every_pod_with_its_claims() {
        let api = api();
        let mut set = sts(json!({
            "replicas": 2,
            "podManagementPolicy": "Parallel",
            "persistentVolumeClaimRetentionPolicy": {"whenDeleted": "Delete"},
            "volumeClaimTemplates": [{"metadata": {"name": "data", "annotations": {"a": "b"}},
                "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}}}],
        }));
        // A template volume of the same name is replaced by the claim.
        set["spec"]["template"]["spec"]["volumes"] = json!([{"name": "data", "emptyDir": {}}]);
        let created = api.create_object("apps", "statefulsets", Some("default"), set).unwrap();
        reconcile(&api, "default", "web").unwrap();
        assert_eq!(pod_names(&api), ["web-0", "web-1"]);
        for ord in 0..2 {
            let claim = format!("data-web-{ord}");
            let pvc = api.get_object("", "persistentvolumeclaims", "default", &claim).unwrap();
            assert_eq!(s(&pvc, "/metadata/labels/app"), "web");
            assert_eq!(s(&pvc, "/metadata/annotations/a"), "b");
            assert_eq!(s(&pvc, "/metadata/ownerReferences/0/uid"), util::uid(&created));
            let volumes = pod(&api, &format!("web-{ord}"))["spec"]["volumes"].clone();
            let data: Vec<&Value> = volumes.as_array().unwrap().iter().filter(|v| v["name"] == "data").collect();
            assert_eq!(data, [&json!({"name": "data", "persistentVolumeClaim": {"claimName": claim}})]);
        }
    }

    #[test]
    fn claims_outlive_the_set_by_default() {
        let api = api();
        let set = sts(json!({"replicas": 1, "volumeClaimTemplates": [{"metadata": {"name": "data"}, "spec": {}}]}));
        api.create_object("apps", "statefulsets", Some("default"), set).unwrap();
        reconcile(&api, "default", "web").unwrap();
        let pvc = api.get_object("", "persistentvolumeclaims", "default", "data-web-0").unwrap();
        assert!(pvc.pointer("/metadata/ownerReferences").is_none());
    }

    #[test]
    fn scales_down_from_the_highest_ordinal() {
        let api = api();
        api.create_object("apps", "statefulsets", Some("default"), sts(json!({"replicas": 3}))).unwrap();
        settle(&api);
        set_spec(&api, |s| s["replicas"] = json!(1));
        reconcile(&api, "default", "web").unwrap();
        // OrderedReady: one at a time, the highest first.
        assert_eq!(pod_names(&api), ["web-0", "web-1"]);
        reconcile(&api, "default", "web").unwrap();
        assert_eq!(pod_names(&api), ["web-0"]);
        // Status counts the pods the reconcile started with.
        reconcile(&api, "default", "web").unwrap();
        assert_eq!(status(&api)["replicas"], 1);
    }

    #[test]
    fn replaces_failed_pods_and_removes_strays() {
        let api = api();
        let created = api.create_object("apps", "statefulsets", Some("default"), sts(json!({"replicas": 1}))).unwrap();
        settle(&api);
        let uid_before = util::uid(&pod(&api, "web-0"));
        let p = pod(&api, "web-0");
        util::write_status(&api, "", "pods", &p, json!({"phase": "Failed"})).unwrap();
        let stray = json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "web-extra", "ownerReferences": [util::owner_ref(&created)]},
            "spec": {"containers": [{"name": "c", "image": "x"}]}});
        api.create_object("", "pods", Some("default"), stray).unwrap();
        reconcile(&api, "default", "web").unwrap();
        assert!(api.get_object("", "pods", "default", "web-extra").is_none());
        reconcile(&api, "default", "web").unwrap();
        assert_eq!(pod_names(&api), ["web-0"]);
        assert_ne!(util::uid(&pod(&api, "web-0")), uid_before);
    }

    #[test]
    fn rolling_update_stops_at_the_partition() {
        let api = api();
        api.create_object("apps", "statefulsets", Some("default"), sts(json!({"replicas": 3}))).unwrap();
        settle(&api);
        let old_revision = revision_of(&pod(&api, "web-0"));
        set_spec(&api, |s| {
            s["template"]["spec"]["containers"][0]["image"] = json!("nginx:2");
            s["updateStrategy"] = json!({"type": "RollingUpdate", "rollingUpdate": {"partition": 1}});
        });
        settle(&api);
        let image = |n: &str| s(&pod(&api, n), "/spec/containers/0/image").to_string();
        assert_eq!([image("web-0"), image("web-1"), image("web-2")], ["nginx:1", "nginx:2", "nginx:2"]);
        assert_eq!(revision_of(&pod(&api, "web-0")), old_revision);
        let st = status(&api);
        assert_eq!(st["currentRevision"], old_revision);
        assert_ne!(st["updateRevision"], old_revision);
        assert_eq!(st["updatedReplicas"], 2);
        assert_eq!(st["currentReplicas"], 1);

        // web-0, recreated below the partition, gets the old template back.
        api.delete_named("", "pods", "default", "web-0").unwrap();
        settle(&api);
        assert_eq!(image("web-0"), "nginx:1");

        // Partition 0 finishes the rollout.
        set_spec(&api, |s| s["updateStrategy"]["rollingUpdate"]["partition"] = json!(0));
        settle(&api);
        assert_eq!(image("web-0"), "nginx:2");
        let st = status(&api);
        assert_eq!(st["currentRevision"], st["updateRevision"]);
        assert_eq!(st["currentReplicas"], 3);
    }

    #[test]
    fn on_delete_leaves_old_pods_alone() {
        let api = api();
        api.create_object("apps", "statefulsets", Some("default"), sts(json!({"replicas": 2, "updateStrategy": {"type": "OnDelete"}}))).unwrap();
        settle(&api);
        set_spec(&api, |s| s["template"]["spec"]["containers"][0]["image"] = json!("nginx:2"));
        settle(&api);
        assert_eq!(s(&pod(&api, "web-1"), "/spec/containers/0/image"), "nginx:1");
        assert_eq!(status(&api)["updatedReplicas"], 0);
    }
}
