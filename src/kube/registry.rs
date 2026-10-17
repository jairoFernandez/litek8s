//! The resource types the API server serves: the built-in Kubernetes types
//! plus any registered through CustomResourceDefinitions.

use std::sync::RwLock;

use anyhow::{Context, Result, bail};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterColumn {
    pub name: String,
    pub type_: String,
    /// A simple dotted JSONPath such as `.spec.replicas`.
    pub json_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceType {
    pub group: String,
    pub version: String,
    pub kind: String,
    pub plural: String,
    pub singular: String,
    pub namespaced: bool,
    pub short_names: Vec<String>,
    pub categories: Vec<String>,
    /// Served with a `/status` subresource, which also means the main
    /// endpoint ignores status changes.
    pub has_status: bool,
    /// Served with a `/scale` subresource (`kubectl scale`).
    pub has_scale: bool,
    pub printer_columns: Vec<PrinterColumn>,
    /// Defined by a CustomResourceDefinition (no strategic merge patch).
    pub custom: bool,
}

impl ResourceType {
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.clone()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }

    pub fn list_kind(&self) -> String {
        format!("{}List", self.kind)
    }

    pub fn verbs(&self) -> Vec<&'static str> {
        if matches!(self.group.as_str(), "authentication.k8s.io" | "authorization.k8s.io") {
            return vec!["create"];
        }
        vec![
            "create",
            "delete",
            "deletecollection",
            "get",
            "list",
            "patch",
            "update",
            "watch",
        ]
    }
}

/// (group, version, kind, plural, short names, namespaced, has_status, has_scale)
type Builtin = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
    bool,
    bool,
    bool,
);

const BUILTINS: &[Builtin] = &[
    ("", "v1", "Namespace", "namespaces", &["ns"], false, true, false),
    ("", "v1", "ConfigMap", "configmaps", &["cm"], true, false, false),
    ("", "v1", "Secret", "secrets", &[], true, false, false),
    ("", "v1", "ServiceAccount", "serviceaccounts", &["sa"], true, false, false),
    ("", "v1", "Pod", "pods", &["po"], true, true, false),
    ("", "v1", "Service", "services", &["svc"], true, true, false),
    ("", "v1", "Endpoints", "endpoints", &["ep"], true, false, false),
    ("", "v1", "Event", "events", &["ev"], true, false, false),
    ("", "v1", "Node", "nodes", &["no"], false, true, false),
    ("", "v1", "PersistentVolume", "persistentvolumes", &["pv"], false, true, false),
    ("", "v1", "PersistentVolumeClaim", "persistentvolumeclaims", &["pvc"], true, true, false),
    ("", "v1", "ReplicationController", "replicationcontrollers", &["rc"], true, true, true),
    ("", "v1", "ResourceQuota", "resourcequotas", &["quota"], true, true, false),
    ("", "v1", "LimitRange", "limitranges", &["limits"], true, false, false),
    ("apps", "v1", "Deployment", "deployments", &["deploy"], true, true, true),
    ("apps", "v1", "ReplicaSet", "replicasets", &["rs"], true, true, true),
    ("apps", "v1", "StatefulSet", "statefulsets", &["sts"], true, true, true),
    ("apps", "v1", "DaemonSet", "daemonsets", &["ds"], true, true, false),
    ("apps", "v1", "ControllerRevision", "controllerrevisions", &[], true, false, false),
    ("batch", "v1", "Job", "jobs", &[], true, true, false),
    ("batch", "v1", "CronJob", "cronjobs", &["cj"], true, true, false),
    ("autoscaling", "v2", "HorizontalPodAutoscaler", "horizontalpodautoscalers", &["hpa"], true, true, false),
    ("policy", "v1", "PodDisruptionBudget", "poddisruptionbudgets", &["pdb"], true, true, false),
    ("networking.k8s.io", "v1", "Ingress", "ingresses", &["ing"], true, true, false),
    ("networking.k8s.io", "v1", "IngressClass", "ingressclasses", &[], false, false, false),
    ("networking.k8s.io", "v1", "NetworkPolicy", "networkpolicies", &["netpol"], true, false, false),
    ("discovery.k8s.io", "v1", "EndpointSlice", "endpointslices", &[], true, false, false),
    ("rbac.authorization.k8s.io", "v1", "Role", "roles", &[], true, false, false),
    ("rbac.authorization.k8s.io", "v1", "RoleBinding", "rolebindings", &[], true, false, false),
    ("rbac.authorization.k8s.io", "v1", "ClusterRole", "clusterroles", &[], false, false, false),
    ("rbac.authorization.k8s.io", "v1", "ClusterRoleBinding", "clusterrolebindings", &[], false, false, false),
    ("storage.k8s.io", "v1", "StorageClass", "storageclasses", &["sc"], false, false, false),
    ("storage.k8s.io", "v1", "CSIDriver", "csidrivers", &[], false, false, false),
    ("coordination.k8s.io", "v1", "Lease", "leases", &[], true, false, false),
    ("scheduling.k8s.io", "v1", "PriorityClass", "priorityclasses", &["pc"], false, false, false),
    ("events.k8s.io", "v1", "Event", "events", &["ev"], true, false, false),
    ("apiextensions.k8s.io", "v1", "CustomResourceDefinition", "customresourcedefinitions", &["crd", "crds"], false, true, false),
    ("apiregistration.k8s.io", "v1", "APIService", "apiservices", &[], false, true, false),
    ("admissionregistration.k8s.io", "v1", "MutatingWebhookConfiguration", "mutatingwebhookconfigurations", &[], false, false, false),
    ("admissionregistration.k8s.io", "v1", "ValidatingWebhookConfiguration", "validatingwebhookconfigurations", &[], false, false, false),
    // Stored and served; their CEL rules are not evaluated yet.
    ("admissionregistration.k8s.io", "v1", "ValidatingAdmissionPolicy", "validatingadmissionpolicies", &[], false, true, false),
    ("admissionregistration.k8s.io", "v1", "ValidatingAdmissionPolicyBinding", "validatingadmissionpolicybindings", &[], false, false, false),
    ("node.k8s.io", "v1", "RuntimeClass", "runtimeclasses", &[], false, false, false),
    ("certificates.k8s.io", "v1", "CertificateSigningRequest", "certificatesigningrequests", &["csr"], false, true, false),
    ("authentication.k8s.io", "v1", "TokenReview", "tokenreviews", &[], false, false, false),
    ("authentication.k8s.io", "v1", "SelfSubjectReview", "selfsubjectreviews", &[], false, false, false),
    ("authorization.k8s.io", "v1", "SubjectAccessReview", "subjectaccessreviews", &[], false, false, false),
    ("authorization.k8s.io", "v1", "SelfSubjectAccessReview", "selfsubjectaccessreviews", &[], false, false, false),
    ("authorization.k8s.io", "v1", "SelfSubjectRulesReview", "selfsubjectrulesreviews", &[], false, false, false),
    ("authorization.k8s.io", "v1", "LocalSubjectAccessReview", "localsubjectaccessreviews", &[], true, false, false),
];

fn category_for(kind: &str) -> Vec<String> {
    match kind {
        "Pod" | "Service" | "Deployment" | "ReplicaSet" | "StatefulSet" | "DaemonSet" | "Job"
        | "CronJob" | "ReplicationController" | "HorizontalPodAutoscaler" => vec!["all".into()],
        _ => Vec::new(),
    }
}

pub struct Registry {
    types: RwLock<Vec<ResourceType>>,
}

impl Registry {
    pub fn with_builtins() -> Self {
        let types = BUILTINS
            .iter()
            .map(|&(group, version, kind, plural, short, namespaced, has_status, has_scale)| {
                ResourceType {
                    group: group.into(),
                    version: version.into(),
                    kind: kind.into(),
                    plural: plural.into(),
                    singular: kind.to_lowercase(),
                    namespaced,
                    short_names: short.iter().map(|s| s.to_string()).collect(),
                    categories: category_for(kind),
                    has_status,
                    has_scale,
                    printer_columns: Vec::new(),
                    custom: false,
                }
            })
            .collect();
        Self {
            types: RwLock::new(types),
        }
    }

    pub fn find(&self, group: &str, version: &str, plural: &str) -> Option<ResourceType> {
        self.types
            .read()
            .unwrap()
            .iter()
            .find(|t| t.group == group && t.version == version && t.plural == plural)
            .cloned()
    }

    /// The first registered version of a resource (the preferred one).
    pub fn find_plural(&self, group: &str, plural: &str) -> Option<ResourceType> {
        self.types
            .read()
            .unwrap()
            .iter()
            .find(|t| t.group == group && t.plural == plural)
            .cloned()
    }

    pub fn find_kind(&self, group: &str, kind: &str) -> Option<ResourceType> {
        self.types
            .read()
            .unwrap()
            .iter()
            .find(|t| t.group == group && t.kind == kind)
            .cloned()
    }

    /// API groups in registration order, each with its versions (the first
    /// is the preferred one). The core group is `""`.
    pub fn groups(&self) -> Vec<(String, Vec<String>)> {
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for t in self.types.read().unwrap().iter() {
            match groups.iter_mut().find(|(g, _)| *g == t.group) {
                Some((_, versions)) => {
                    if !versions.contains(&t.version) {
                        versions.push(t.version.clone());
                    }
                }
                None => groups.push((t.group.clone(), vec![t.version.clone()])),
            }
        }
        groups
    }

    pub fn resources(&self, group: &str, version: &str) -> Vec<ResourceType> {
        self.types
            .read()
            .unwrap()
            .iter()
            .filter(|t| t.group == group && t.version == version)
            .cloned()
            .collect()
    }

    pub fn has_group_version(&self, group: &str, version: &str) -> bool {
        self.types
            .read()
            .unwrap()
            .iter()
            .any(|t| t.group == group && t.version == version)
    }

    /// Register (or re-register) the served versions of a CRD object.
    pub fn register_crd(&self, crd: &Value) -> Result<()> {
        let types = crd_types(crd)?;
        let group = types[0].group.clone();
        let plural = types[0].plural.clone();
        let mut all = self.types.write().unwrap();
        all.retain(|t| !(t.custom && t.group == group && t.plural == plural));
        all.extend(types);
        Ok(())
    }

    pub fn unregister_crd(&self, group: &str, plural: &str) {
        self.types
            .write()
            .unwrap()
            .retain(|t| !(t.custom && t.group == group && t.plural == plural));
    }
}

/// The resource types a CRD defines, one per served version.
pub fn crd_types(crd: &Value) -> Result<Vec<ResourceType>> {
    let spec = crd.get("spec").context("CRD has no spec")?;
    let s = |v: &Value, path: &str| v.pointer(path).and_then(Value::as_str).map(str::to_string);
    let group = s(spec, "/group").context("CRD spec.group is required")?;
    let plural = s(spec, "/names/plural").context("CRD spec.names.plural is required")?;
    let kind = s(spec, "/names/kind").context("CRD spec.names.kind is required")?;
    let singular = s(spec, "/names/singular").unwrap_or_else(|| kind.to_lowercase());
    let namespaced = s(spec, "/scope").as_deref() != Some("Cluster");
    let strings = |path: &str| -> Vec<String> {
        spec.pointer(path)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default()
    };
    let short_names = strings("/names/shortNames");
    let categories = strings("/names/categories");
    let versions = spec
        .get("versions")
        .and_then(Value::as_array)
        .context("CRD spec.versions is required")?;
    let mut types = Vec::new();
    for version in versions {
        if version.get("served").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let Some(name) = s(version, "/name") else {
            bail!("CRD version is missing `name`");
        };
        let printer_columns = version
            .get("additionalPrinterColumns")
            .and_then(Value::as_array)
            .map(|cols| {
                cols.iter()
                    .filter_map(|c| {
                        Some(PrinterColumn {
                            name: s(c, "/name")?,
                            type_: s(c, "/type").unwrap_or_else(|| "string".into()),
                            json_path: s(c, "/jsonPath")?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        types.push(ResourceType {
            group: group.clone(),
            version: name,
            kind: kind.clone(),
            plural: plural.clone(),
            singular: singular.clone(),
            namespaced,
            short_names: short_names.clone(),
            categories: categories.clone(),
            has_status: version.pointer("/subresources/status").is_some(),
            has_scale: version.pointer("/subresources/scale").is_some(),
            printer_columns,
            custom: true,
        });
    }
    if types.is_empty() {
        bail!("CRD serves no versions");
    }
    Ok(types)
}
