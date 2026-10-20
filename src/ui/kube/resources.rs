//! The resource types a cluster serves: discovery (aggregated v2, else the
//! legacy per-group documents), a built-in list for before it arrives, and
//! how a name typed after `:` finds one (plural, singular, short name,
//! kind, `plural.group`, aliases).

use std::collections::HashMap;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Resource {
    /// `""` for the core group.
    pub group: String,
    pub version: String,
    pub plural: String,
    pub singular: String,
    pub kind: String,
    pub namespaced: bool,
    pub verbs: Vec<String>,
    pub short: Vec<String>,
}

impl Resource {
    /// `v1` or `apps/v1`.
    pub fn api_version(&self) -> String {
        if self.group.is_empty() { self.version.clone() } else { format!("{}/{}", self.group, self.version) }
    }

    /// A stable id: `v1/pods`, `apps/v1/deployments`.
    pub fn id(&self) -> String {
        format!("{}/{}", self.api_version(), self.plural)
    }

    /// What the UI calls it: `pods`, `deployments`, `certificates.cert-manager.io`.
    pub fn name(&self) -> String {
        if self.group.is_empty() || builtin_group(&self.group) { self.plural.clone() } else { format!("{}.{}", self.plural, self.group) }
    }

    fn prefix(&self) -> String {
        if self.group.is_empty() { format!("/api/{}", self.version) } else { format!("/apis/{}/{}", self.group, self.version) }
    }

    /// The collection: all namespaces with `None` (or a cluster-scoped type).
    pub fn list_path(&self, ns: Option<&str>) -> String {
        match ns.filter(|_| self.namespaced) {
            Some(ns) => format!("{}/namespaces/{ns}/{}", self.prefix(), self.plural),
            None => format!("{}/{}", self.prefix(), self.plural),
        }
    }

    pub fn object_path(&self, ns: &str, name: &str) -> String {
        if self.namespaced && !ns.is_empty() { format!("{}/namespaces/{ns}/{}/{name}", self.prefix(), self.plural) } else { format!("{}/{}/{name}", self.prefix(), self.plural) }
    }

    pub fn can(&self, verb: &str) -> bool {
        self.verbs.is_empty() || self.verbs.iter().any(|v| v == verb)
    }

    pub fn is(&self, group: &str, plural: &str) -> bool {
        self.group == group && self.plural == plural
    }
}

/// Groups of Kubernetes itself (`apps`, `batch`, `*.k8s.io`): their types
/// go by their plain plural.
fn builtin_group(g: &str) -> bool {
    !g.contains('.') || g.ends_with(".k8s.io")
}

/// Which of two types with the same name wins: the core group, then
/// Kubernetes' own groups (`pods` is core's, not metrics.k8s.io's).
fn rank(r: &Resource) -> u8 {
    match r.group.as_str() {
        "" => 0,
        "metrics.k8s.io" => 3,
        g if !g.contains('.') => 1,
        g if g.ends_with(".k8s.io") => 2,
        _ => 4,
    }
}

macro_rules! res {
    ($g:expr, $v:expr, $p:expr, $s:expr, $k:expr, $ns:expr, [$($short:expr),*]) => {
        Resource { group: $g.into(), version: $v.into(), plural: $p.into(), singular: $s.into(), kind: $k.into(), namespaced: $ns, verbs: Vec::new(), short: vec![$($short.into()),*] }
    };
}

/// What every cluster serves, so `:pods` works before discovery answers
/// (and when it fails).
pub fn builtins() -> Vec<Resource> {
    vec![
        res!("", "v1", "pods", "pod", "Pod", true, ["po"]),
        res!("", "v1", "services", "service", "Service", true, ["svc"]),
        res!("", "v1", "namespaces", "namespace", "Namespace", false, ["ns"]),
        res!("", "v1", "nodes", "node", "Node", false, ["no"]),
        res!("", "v1", "events", "event", "Event", true, ["ev"]),
        res!("", "v1", "configmaps", "configmap", "ConfigMap", true, ["cm"]),
        res!("", "v1", "secrets", "secret", "Secret", true, []),
        res!("", "v1", "serviceaccounts", "serviceaccount", "ServiceAccount", true, ["sa"]),
        res!("", "v1", "persistentvolumeclaims", "persistentvolumeclaim", "PersistentVolumeClaim", true, ["pvc"]),
        res!("", "v1", "persistentvolumes", "persistentvolume", "PersistentVolume", false, ["pv"]),
        res!("", "v1", "endpoints", "endpoints", "Endpoints", true, ["ep"]),
        res!("", "v1", "replicationcontrollers", "replicationcontroller", "ReplicationController", true, ["rc"]),
        res!("", "v1", "resourcequotas", "resourcequota", "ResourceQuota", true, ["quota"]),
        res!("", "v1", "limitranges", "limitrange", "LimitRange", true, ["limits"]),
        res!("apps", "v1", "deployments", "deployment", "Deployment", true, ["deploy"]),
        res!("apps", "v1", "replicasets", "replicaset", "ReplicaSet", true, ["rs"]),
        res!("apps", "v1", "statefulsets", "statefulset", "StatefulSet", true, ["sts"]),
        res!("apps", "v1", "daemonsets", "daemonset", "DaemonSet", true, ["ds"]),
        res!("apps", "v1", "controllerrevisions", "controllerrevision", "ControllerRevision", true, []),
        res!("batch", "v1", "jobs", "job", "Job", true, []),
        res!("batch", "v1", "cronjobs", "cronjob", "CronJob", true, ["cj"]),
        res!("networking.k8s.io", "v1", "ingresses", "ingress", "Ingress", true, ["ing"]),
        res!("networking.k8s.io", "v1", "networkpolicies", "networkpolicy", "NetworkPolicy", true, ["netpol"]),
        res!("networking.k8s.io", "v1", "ingressclasses", "ingressclass", "IngressClass", false, []),
        res!("discovery.k8s.io", "v1", "endpointslices", "endpointslice", "EndpointSlice", true, []),
        res!("apiextensions.k8s.io", "v1", "customresourcedefinitions", "customresourcedefinition", "CustomResourceDefinition", false, ["crd", "crds"]),
        res!("storage.k8s.io", "v1", "storageclasses", "storageclass", "StorageClass", false, ["sc"]),
        res!("rbac.authorization.k8s.io", "v1", "roles", "role", "Role", true, []),
        res!("rbac.authorization.k8s.io", "v1", "rolebindings", "rolebinding", "RoleBinding", true, []),
        res!("rbac.authorization.k8s.io", "v1", "clusterroles", "clusterrole", "ClusterRole", false, []),
        res!("rbac.authorization.k8s.io", "v1", "clusterrolebindings", "clusterrolebinding", "ClusterRoleBinding", false, []),
        res!("autoscaling", "v2", "horizontalpodautoscalers", "horizontalpodautoscaler", "HorizontalPodAutoscaler", true, ["hpa"]),
        res!("policy", "v1", "poddisruptionbudgets", "poddisruptionbudget", "PodDisruptionBudget", true, ["pdb"]),
        res!("scheduling.k8s.io", "v1", "priorityclasses", "priorityclass", "PriorityClass", false, ["pc"]),
        res!("coordination.k8s.io", "v1", "leases", "lease", "Lease", true, []),
        res!("apiregistration.k8s.io", "v1", "apiservices", "apiservice", "APIService", false, []),
    ]
}

/// k9s's extra names, so its users' fingers work.
const EXTRA_ALIASES: &[(&str, &str)] = &[
    ("dp", "deployments"),
    ("sec", "secrets"),
    ("jo", "jobs"),
    ("cr", "clusterroles"),
    ("crb", "clusterrolebindings"),
    ("ro", "roles"),
    ("rb", "rolebindings"),
    ("np", "networkpolicies"),
    ("eps", "endpointslices"),
    ("pf", "portforwards"),
];

/// All resource types and the names they answer to.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub list: Vec<Resource>,
    by_name: HashMap<String, usize>,
}

impl Catalog {
    pub fn new(mut list: Vec<Resource>) -> Catalog {
        // Subresources are not listable here.
        list.retain(|r| !r.plural.contains('/') && !r.plural.is_empty());
        list.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.name().cmp(&b.name())));
        let mut by_name: HashMap<String, usize> = HashMap::new();
        for (i, r) in list.iter().enumerate() {
            let kind = r.kind.to_lowercase();
            let mut names = vec![r.plural.clone(), r.singular.clone(), kind.clone(), format!("{}.{}", r.plural, r.group), format!("{kind}.{}", r.group), format!("{}.{}.{}", r.plural, r.version, r.group)];
            names.extend(r.short.iter().cloned());
            for n in names {
                if n.is_empty() || n.ends_with('.') {
                    continue;
                }
                // The best-ranked type keeps a shared name.
                by_name.entry(n).or_insert(i);
            }
        }
        for (alias, target) in EXTRA_ALIASES {
            if let Some(&i) = by_name.get(*target) {
                by_name.entry(alias.to_string()).or_insert(i);
            }
        }
        Catalog { list, by_name }
    }

    pub fn find(&self, name: &str) -> Option<&Resource> {
        let n = name.to_lowercase();
        self.by_name.get(&n).map(|&i| &self.list[i])
    }

    pub fn by_id(&self, id: &str) -> Option<&Resource> {
        self.list.iter().find(|r| r.id() == id)
    }

    /// The type of `kind` in `apiVersion` (an ownerReference's, an event's
    /// involvedObject).
    pub fn by_kind(&self, api_version: &str, kind: &str) -> Option<&Resource> {
        let group = api_version.rsplit_once('/').map(|(g, _)| g).unwrap_or("");
        self.list.iter().find(|r| r.kind == kind && r.group == group).or_else(|| self.list.iter().find(|r| r.kind == kind))
    }

    /// Every name typed after `:` could complete to.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_name.keys().filter(|n| !n.contains('.')).cloned().collect();
        v.extend(self.list.iter().map(Resource::name).filter(|n| n.contains('.')));
        v.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        v.dedup();
        v
    }
}

/// Aggregated discovery (`apidiscovery.k8s.io/v2`, `APIGroupDiscoveryList`).
/// `None` when the reply is the legacy form.
pub fn parse_aggregated(doc: &Value) -> Option<Vec<Resource>> {
    if doc["kind"].as_str() != Some("APIGroupDiscoveryList") {
        return None;
    }
    let mut out = Vec::new();
    for g in doc["items"].as_array().into_iter().flatten() {
        let group = g.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
        // Versions come in preference order: the first one per type wins.
        for v in g["versions"].as_array().into_iter().flatten() {
            let version = v["version"].as_str().unwrap_or_default();
            for r in v["resources"].as_array().into_iter().flatten() {
                let plural = r["resource"].as_str().unwrap_or_default();
                if out.iter().any(|o: &Resource| o.group == group && o.plural == plural) {
                    continue;
                }
                out.push(Resource {
                    group: group.into(),
                    version: version.into(),
                    plural: plural.into(),
                    singular: r["singularResource"].as_str().unwrap_or_default().into(),
                    kind: r.pointer("/responseKind/kind").and_then(Value::as_str).unwrap_or_default().into(),
                    namespaced: r["scope"].as_str() == Some("Namespaced"),
                    verbs: strings(&r["verbs"]),
                    short: strings(&r["shortNames"]),
                });
            }
        }
    }
    Some(out)
}

/// A legacy `APIResourceList` (`/api/v1`, `/apis/<group>/<version>`).
pub fn parse_resource_list(doc: &Value) -> Vec<Resource> {
    let gv = doc["groupVersion"].as_str().unwrap_or_default();
    let (group, version) = gv.rsplit_once('/').unwrap_or(("", gv));
    doc["resources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| !r["name"].as_str().unwrap_or_default().contains('/'))
        .map(|r| Resource {
            group: group.into(),
            version: version.into(),
            plural: r["name"].as_str().unwrap_or_default().into(),
            singular: r["singularName"].as_str().filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| r["kind"].as_str().unwrap_or_default().to_lowercase()),
            kind: r["kind"].as_str().unwrap_or_default().into(),
            namespaced: r["namespaced"].as_bool().unwrap_or(false),
            verbs: strings(&r["verbs"]),
            short: strings(&r["shortNames"]),
        })
        .collect()
}

/// The preferred group-versions of a legacy `APIGroupList`.
pub fn preferred_versions(doc: &Value) -> Vec<String> {
    doc["groups"].as_array().into_iter().flatten().filter_map(|g| g.pointer("/preferredVersion/groupVersion").and_then(Value::as_str).map(str::to_string)).collect()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_types_by_any_name_and_prefers_core() {
        let mut list = builtins();
        list.push(Resource { group: "metrics.k8s.io".into(), version: "v1beta1".into(), plural: "pods".into(), singular: "pod".into(), kind: "PodMetrics".into(), namespaced: true, ..Default::default() });
        list.push(Resource { group: "cert-manager.io".into(), version: "v1".into(), plural: "certificates".into(), singular: "certificate".into(), kind: "Certificate".into(), namespaced: true, short: vec!["cert".into()], ..Default::default() });
        let c = Catalog::new(list);
        for n in ["po", "pod", "pods", "Pod", "PODS"] {
            assert_eq!(c.find(n).unwrap().id(), "v1/pods", "{n}");
        }
        assert_eq!(c.find("pods.metrics.k8s.io").unwrap().kind, "PodMetrics");
        assert_eq!(c.find("deploy").unwrap().id(), "apps/v1/deployments");
        assert_eq!(c.find("dp").unwrap().id(), "apps/v1/deployments");
        assert_eq!(c.find("cert").unwrap().name(), "certificates.cert-manager.io");
        assert_eq!(c.find("certificate.cert-manager.io").unwrap().plural, "certificates");
        assert!(c.find("nope").is_none());
        let pods = c.find("pods").unwrap();
        assert_eq!(pods.list_path(Some("default")), "/api/v1/namespaces/default/pods");
        assert_eq!(pods.list_path(None), "/api/v1/pods");
        let nodes = c.find("nodes").unwrap();
        assert_eq!(nodes.list_path(Some("default")), "/api/v1/nodes");
        assert_eq!(c.find("deploy").unwrap().object_path("web", "api"), "/apis/apps/v1/namespaces/web/deployments/api");
    }

    #[test]
    fn parses_both_discovery_forms() {
        let agg = json!({"kind": "APIGroupDiscoveryList", "items": [{"metadata": {"name": "apps"}, "versions": [
            {"version": "v1", "resources": [{"resource": "deployments", "responseKind": {"kind": "Deployment"}, "scope": "Namespaced", "singularResource": "deployment", "verbs": ["get", "list"], "shortNames": ["deploy"]}]},
            {"version": "v1beta1", "resources": [{"resource": "deployments", "responseKind": {"kind": "Deployment"}, "scope": "Namespaced"}]}
        ]}]});
        let r = parse_aggregated(&agg).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].id(), "apps/v1/deployments");
        assert!(r[0].can("list") && !r[0].can("delete"));
        assert!(parse_aggregated(&json!({"kind": "APIGroupList"})).is_none());
        let legacy = json!({"groupVersion": "v1", "resources": [{"name": "pods", "singularName": "", "kind": "Pod", "namespaced": true, "shortNames": ["po"]}, {"name": "pods/log", "kind": "Pod"}]});
        let r = parse_resource_list(&legacy);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].id().as_str(), r[0].singular.as_str()), ("v1/pods", "pod"));
        assert_eq!(preferred_versions(&json!({"groups": [{"preferredVersion": {"groupVersion": "apps/v1"}}]})), vec!["apps/v1"]);
    }
}
