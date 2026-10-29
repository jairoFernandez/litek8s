//! Authorization: Kubernetes RBAC over the stored Role, ClusterRole,
//! RoleBinding and ClusterRoleBinding objects, as kube-apiserver's RBAC
//! authorizer evaluates them.
//!
//! Every HTTP request is turned into [`Attributes`] (verb, API group,
//! resource, subresource, name, namespace, or a non-resource path, the way
//! upstream's RequestInfoFactory does it) and checked against the rules the
//! user's bindings grant. `system:masters` may do anything. In-process
//! controllers and the node agents' connection never go through here: they
//! call the store directly.
//!
//! The rules are compiled once per change of any RBAC object (a change
//! feed subscription bumps a generation; writes through this server bump it
//! synchronously). Also here: the default ClusterRoles and bindings every
//! cluster gets, the `aggregationRule` controller, escalation checks on
//! writes of roles and bindings, and the access reviews.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};

use serde_json::{Value, json};

use super::server::ApiState;
use super::store::ObjectKey;

pub const GROUP: &str = "rbac.authorization.k8s.io";
pub const MASTERS: &str = "system:masters";
pub const AUTHENTICATED: &str = "system:authenticated";
pub const UNAUTHENTICATED: &str = "system:unauthenticated";
pub const ANONYMOUS: &str = "system:anonymous";

/// `--authorization-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Mode {
    /// No authorization: every client may do anything (and needs no
    /// credentials), as litek8s did before RBAC.
    #[default]
    #[value(name = "AlwaysAllow")]
    AlwaysAllow,
    /// Clients authenticate (or are anonymous on public paths) and RBAC
    /// decides.
    #[value(name = "RBAC")]
    Rbac,
    /// Authenticate and evaluate RBAC, log every decision, allow anyway.
    #[value(name = "RBAC-audit")]
    RbacAudit,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::AlwaysAllow => "AlwaysAllow",
            Mode::Rbac => "RBAC",
            Mode::RbacAudit => "RBAC-audit",
        }
    }
}

/// Who a request comes from (TokenReview's `status.user`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserInfo {
    pub username: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub uid: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Vec<String>>,
}

impl UserInfo {
    pub fn anonymous() -> Self {
        Self { username: ANONYMOUS.into(), groups: vec![UNAUTHENTICATED.into()], ..Default::default() }
    }

    pub fn is_anonymous(&self) -> bool {
        self.username == ANONYMOUS
    }

    pub fn is_master(&self) -> bool {
        self.groups.iter().any(|g| g == MASTERS)
    }

    pub fn from_json(v: &Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    /// `system:authenticated` added unless there.
    pub fn authenticated(mut self) -> Self {
        if !self.groups.iter().any(|g| g == AUTHENTICATED) {
            self.groups.push(AUTHENTICATED.into());
        }
        self
    }
}

/// What a request does, as RBAC sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attributes {
    pub verb: String,
    pub resource_request: bool,
    pub namespace: String,
    pub api_group: String,
    pub api_version: String,
    pub resource: String,
    pub subresource: String,
    pub name: String,
    /// Non-resource requests: the URL path.
    pub path: String,
}

impl Attributes {
    pub fn resource(verb: &str, group: &str, resource: &str, namespace: &str, name: &str) -> Self {
        let (resource, sub) = resource.split_once('/').unwrap_or((resource, ""));
        Self {
            verb: verb.into(),
            resource_request: true,
            namespace: namespace.into(),
            api_group: group.into(),
            resource: resource.into(),
            subresource: sub.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    pub fn non_resource(verb: &str, path: &str) -> Self {
        Self { verb: verb.into(), path: path.into(), ..Default::default() }
    }

    /// For log lines: `get pods/log default/web` or `get /metrics`.
    pub fn describe(&self) -> String {
        if !self.resource_request {
            return format!("{} {}", self.verb, self.path);
        }
        let mut r = self.resource.clone();
        if !self.subresource.is_empty() {
            r = format!("{r}/{}", self.subresource);
        }
        if !self.api_group.is_empty() {
            r = format!("{r}.{}", self.api_group);
        }
        let mut where_ = String::new();
        if !self.namespace.is_empty() {
            where_ = format!(" namespace={}", self.namespace);
        }
        if !self.name.is_empty() {
            where_.push_str(&format!(" name={}", self.name));
        }
        format!("{} {r}{where_}", self.verb)
    }
}

/// The request attributes of an HTTP request (upstream's RequestInfo).
pub fn attributes_of(method: &str, path: &str, query: &HashMap<String, String>) -> Attributes {
    let verb_of_method = match method {
        "POST" => "create",
        "PUT" => "update",
        "PATCH" => "patch",
        "DELETE" => "delete",
        _ => "get",
    };
    let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let (group, version, mut rest): (&str, &str, &[&str]) = match parts.as_slice() {
        ["api", v, rest @ ..] => ("", v, rest),
        ["apis", g, v, rest @ ..] => (g, v, rest),
        _ => return Attributes::non_resource(&method.to_ascii_lowercase().replace("head", "get"), path),
    };
    if rest.is_empty() {
        // Discovery of a group version.
        return Attributes::non_resource(&method.to_ascii_lowercase().replace("head", "get"), path);
    }
    let mut a = Attributes { resource_request: true, api_group: group.into(), api_version: version.into(), ..Default::default() };
    let mut verb = verb_of_method.to_string();
    if rest[0] == "watch" {
        verb = "watch".into();
        rest = &rest[1..];
    }
    if rest.first() == Some(&"namespaces") && rest.len() > 1 {
        a.namespace = rest[1].into();
        if rest.len() > 2 && !matches!(rest[2], "status" | "finalize") {
            rest = &rest[2..];
        }
    }
    if let Some(r) = rest.first() {
        a.resource = r.to_string();
    }
    if let Some(n) = rest.get(1) {
        a.name = n.to_string();
    }
    if let Some(s) = rest.get(2) {
        a.subresource = s.to_string();
    }
    if a.name.is_empty() {
        match verb.as_str() {
            "get" => verb = "list".into(),
            "delete" => verb = "deletecollection".into(),
            _ => {}
        }
    }
    let truthy = |k: &str| matches!(query.get(k).map(String::as_str), Some("true" | "1"));
    if verb == "list" && truthy("watch") {
        verb = "watch".into();
    }
    if matches!(verb.as_str(), "list" | "watch") && a.name.is_empty() {
        // A list or watch of one object by name may be granted by name.
        if let Some(sel) = query.get("fieldSelector") {
            let terms: Vec<&str> = sel.split(',').collect();
            if let [one] = terms.as_slice()
                && let Some(n) = one.strip_prefix("metadata.name==").or_else(|| one.strip_prefix("metadata.name="))
            {
                a.name = n.to_string();
            }
        }
    }
    if a.resource.is_empty() {
        return Attributes::non_resource(&method.to_ascii_lowercase(), path);
    }
    // Streams open with GET (websockets) but need `create`, as in 1.33
    // (AuthorizePodWebsocketUpgradeCreatePermission).
    if a.resource == "pods" && matches!(a.subresource.as_str(), "exec" | "attach" | "portforward") {
        verb = "create".into();
    }
    if a.subresource == "proxy" && matches!(a.resource.as_str(), "pods" | "services" | "nodes") {
        // `.../pods/<name>[:port]/proxy/...`: the verb follows the method.
        a.name = a.name.split(':').find(|p| !p.is_empty() && *p != "http" && *p != "https").unwrap_or_default().to_string();
    }
    a.verb = verb;
    a
}

/// One PolicyRule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rule {
    pub verbs: Vec<String>,
    pub api_groups: Vec<String>,
    pub resources: Vec<String>,
    pub resource_names: Vec<String>,
    pub non_resource_urls: Vec<String>,
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

impl Rule {
    pub fn from_json(v: &Value) -> Self {
        Self {
            verbs: strings(v, "verbs"),
            api_groups: strings(v, "apiGroups"),
            resources: strings(v, "resources"),
            resource_names: strings(v, "resourceNames"),
            non_resource_urls: strings(v, "nonResourceURLs"),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("verbs".into(), json!(self.verbs));
        if !self.non_resource_urls.is_empty() {
            m.insert("nonResourceURLs".into(), json!(self.non_resource_urls));
        } else {
            m.insert("apiGroups".into(), json!(self.api_groups));
            m.insert("resources".into(), json!(self.resources));
        }
        if !self.resource_names.is_empty() {
            m.insert("resourceNames".into(), json!(self.resource_names));
        }
        Value::Object(m)
    }

    fn verb_matches(&self, verb: &str) -> bool {
        self.verbs.iter().any(|v| v == "*" || v == verb)
    }

    /// Whether this rule allows `a` (upstream's RuleAllows).
    pub fn allows(&self, a: &Attributes) -> bool {
        if !self.verb_matches(&a.verb) {
            return false;
        }
        if !a.resource_request {
            return self.non_resource_urls.iter().any(|u| u == "*" || u == &a.path || u.strip_suffix('*').is_some_and(|p| a.path.starts_with(p)));
        }
        if !self.api_groups.iter().any(|g| g == "*" || g == &a.api_group) {
            return false;
        }
        let combined = if a.subresource.is_empty() { a.resource.clone() } else { format!("{}/{}", a.resource, a.subresource) };
        let resource_ok = self.resources.iter().any(|r| {
            r == "*" || *r == combined || (!a.subresource.is_empty() && r.strip_prefix("*/").is_some_and(|s| s == a.subresource))
        });
        if !resource_ok {
            return false;
        }
        self.resource_names.is_empty() || self.resource_names.contains(&a.name)
    }
}

/// A binding subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub kind: String,
    pub name: String,
    pub namespace: String,
}

impl Subject {
    fn matches(&self, user: &UserInfo, binding_ns: &str) -> bool {
        match self.kind.as_str() {
            "User" => self.name == user.username,
            "Group" => user.groups.contains(&self.name),
            "ServiceAccount" => {
                let ns = if self.namespace.is_empty() { binding_ns } else { &self.namespace };
                user.username == format!("system:serviceaccount:{ns}:{}", self.name)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
struct Binding {
    namespace: String,
    subjects: Vec<Subject>,
    /// `ClusterRole` or `Role`.
    role_kind: String,
    role_name: String,
}

fn binding_of(obj: &Value) -> Binding {
    let s = |p: &str| obj.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
    Binding {
        namespace: s("/metadata/namespace"),
        subjects: obj
            .get("subjects")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|sub| {
                let f = |k: &str| sub.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                Subject { kind: f("kind"), name: f("name"), namespace: f("namespace") }
            })
            .collect(),
        role_kind: s("/roleRef/kind"),
        role_name: s("/roleRef/name"),
    }
}

fn rules_of(obj: &Value) -> Vec<Rule> {
    obj.get("rules").and_then(Value::as_array).into_iter().flatten().map(Rule::from_json).collect()
}

/// Every RBAC object, compiled.
#[derive(Debug, Default)]
pub struct Policy {
    cluster_roles: HashMap<String, Vec<Rule>>,
    roles: HashMap<(String, String), Vec<Rule>>,
    cluster_bindings: Vec<Binding>,
    bindings: HashMap<String, Vec<Binding>>,
}

impl Policy {
    pub fn from_objects(cluster_roles: &[Value], roles: &[Value], cluster_bindings: &[Value], bindings: &[Value]) -> Self {
        let name = |o: &Value| o.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string();
        let ns = |o: &Value| o.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut p = Policy::default();
        for r in cluster_roles {
            p.cluster_roles.insert(name(r), rules_of(r));
        }
        for r in roles {
            p.roles.insert((ns(r), name(r)), rules_of(r));
        }
        p.cluster_bindings = cluster_bindings.iter().map(binding_of).collect();
        for b in bindings {
            p.bindings.entry(ns(b)).or_default().push(binding_of(b));
        }
        p
    }

    fn load(state: &ApiState) -> Self {
        let list = |r: &str| state.store().list(GROUP, r, None).0;
        Self::from_objects(&list("clusterroles"), &list("roles"), &list("clusterrolebindings"), &list("rolebindings"))
    }

    fn role_rules(&self, b: &Binding) -> &[Rule] {
        let rules = match b.role_kind.as_str() {
            "ClusterRole" => self.cluster_roles.get(&b.role_name),
            "Role" => self.roles.get(&(b.namespace.clone(), b.role_name.clone())),
            _ => None,
        };
        rules.map(Vec::as_slice).unwrap_or_default()
    }

    /// The rules `user` holds in `namespace` (cluster-wide ones included;
    /// `""`: cluster-wide only), each with what grants it.
    pub fn rules_for<'a>(&'a self, user: &UserInfo, namespace: &str) -> Vec<(&'a Rule, String)> {
        let mut out = Vec::new();
        for b in &self.cluster_bindings {
            if b.subjects.iter().any(|s| s.matches(user, "")) {
                for r in self.role_rules(b) {
                    out.push((r, format!("ClusterRole {:?}", b.role_name)));
                }
            }
        }
        if !namespace.is_empty() {
            for b in self.bindings.get(namespace).into_iter().flatten() {
                if b.subjects.iter().any(|s| s.matches(user, &b.namespace)) {
                    for r in self.role_rules(b) {
                        out.push((r, format!("{} {:?} in {namespace}", b.role_kind, b.role_name)));
                    }
                }
            }
        }
        out
    }

    /// Whether `user` may do `a`, and why.
    pub fn authorize(&self, user: &UserInfo, a: &Attributes) -> Decision {
        if user.is_master() {
            return Decision { allowed: true, reason: format!("{MASTERS} may do anything") };
        }
        let ns = if a.resource_request { a.namespace.as_str() } else { "" };
        for (rule, why) in self.rules_for(user, ns) {
            if rule.allows(a) {
                return Decision { allowed: true, reason: format!("RBAC: allowed by {why}") };
            }
        }
        Decision { allowed: false, reason: String::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    pub reason: String,
}

/// The compiled policy, rebuilt when any RBAC object changed.
pub struct Cache {
    generation: AtomicU64,
    built: Mutex<Option<(u64, Arc<Policy>)>>,
    watching: Once,
}

impl Default for Cache {
    fn default() -> Self {
        Self { generation: AtomicU64::new(0), built: Mutex::new(None), watching: Once::new() }
    }
}

impl Cache {
    /// Mark the policy stale (an RBAC object was written).
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn policy(&self, state: &Arc<ApiState>) -> Arc<Policy> {
        self.watching.call_once(|| {
            if tokio::runtime::Handle::try_current().is_ok() {
                tokio::spawn(follow_changes(state.clone()));
            }
        });
        let generation = self.generation.load(Ordering::SeqCst);
        if let Some((g, p)) = self.built.lock().unwrap().as_ref()
            && *g == generation
        {
            return p.clone();
        }
        let policy = Arc::new(Policy::load(state));
        *self.built.lock().unwrap() = Some((generation, policy.clone()));
        policy
    }
}

/// Bump the cache's generation on every change of an RBAC object (also
/// those other HA servers make, which arrive through the Raft log).
async fn follow_changes(state: Arc<ApiState>) {
    loop {
        let Ok(sub) = state.store().subscribe_for(None, |k: &ObjectKey| k.group == GROUP) else { return };
        state.rbac.invalidate();
        let mut live = sub.live;
        loop {
            match live.recv().await {
                Ok(e) if e.key.group == GROUP => state.rbac.invalidate(),
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    state.rbac.invalidate();
                }
                Err(_) => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Escalation

/// Whether `held` (rules a user holds) covers `wanted`: every verb ×
/// group × resource × name (or URL) `wanted` names is allowed by one of
/// them. A `*` in `wanted` is only covered by a `*`.
pub fn covers(held: &[&Rule], wanted: &Rule) -> bool {
    let allowed = |a: &Attributes| held.iter().any(|r| r.allows(a));
    if !wanted.non_resource_urls.is_empty() {
        return wanted.verbs.iter().all(|v| {
            wanted.non_resource_urls.iter().all(|u| {
                // A held prefix rule `/a/*` covers a wanted `/a/*` or `/a/b`.
                held.iter().any(|r| {
                    r.verb_matches(v) && r.non_resource_urls.iter().any(|h| h == "*" || h == u || h.strip_suffix('*').is_some_and(|p| u.starts_with(p)))
                })
            })
        });
    }
    let names: Vec<&str> = if wanted.resource_names.is_empty() { vec![""] } else { wanted.resource_names.iter().map(String::as_str).collect() };
    for v in &wanted.verbs {
        for g in &wanted.api_groups {
            for res in &wanted.resources {
                for n in &names {
                    let (resource, sub) = res.split_once('/').unwrap_or((res, ""));
                    let a = Attributes {
                        verb: v.clone(),
                        resource_request: true,
                        api_group: g.clone(),
                        resource: resource.into(),
                        subresource: sub.into(),
                        name: n.to_string(),
                        ..Default::default()
                    };
                    // A wanted rule without names must be held without names.
                    let ok = if n.is_empty() {
                        held.iter().any(|r| r.resource_names.is_empty() && r.allows(&a))
                    } else {
                        allowed(&a)
                    };
                    if !ok {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// The escalation checks on a write of a Role, ClusterRole, RoleBinding or
/// ClusterRoleBinding: a user may only grant what they hold, unless they
/// may `escalate` the role (for roles) or `bind` it (for bindings).
pub fn check_escalation(state: &Arc<ApiState>, user: &UserInfo, plural: &str, namespace: &str, obj: &Value) -> Result<(), String> {
    if user.is_master() {
        return Ok(());
    }
    let policy = state.rbac.policy(state);
    let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
    let may = |verb: &str, resource: &str, name: &str| policy.authorize(user, &Attributes::resource(verb, GROUP, resource, namespace, name)).allowed;
    let held: Vec<(&Rule, String)> = policy.rules_for(user, namespace);
    let held: Vec<&Rule> = held.into_iter().map(|(r, _)| r).collect();
    let missing = |rules: &[Rule]| rules.iter().filter(|r| !covers(&held, r)).map(|r| r.to_json().to_string()).collect::<Vec<_>>();
    match plural {
        "roles" | "clusterroles" => {
            if may("escalate", plural, name) {
                return Ok(());
            }
            // An aggregated role's rules are the controller's: only the
            // selectors are the user's (as upstream, which checks them as
            // escalating anyway: only those who may escalate set them).
            if obj.get("aggregationRule").is_some_and(|a| !a.is_null()) && plural == "clusterroles" {
                return Err(format!("user {:?} may not create or update an aggregated ClusterRole without the escalate verb", user.username));
            }
            let rules = rules_of(obj);
            let gaps = missing(&rules);
            if gaps.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "user {:?} (groups {:?}) is attempting to grant RBAC permissions not currently held: {}",
                    user.username,
                    user.groups,
                    gaps.join(", ")
                ))
            }
        }
        "rolebindings" | "clusterrolebindings" => {
            let b = binding_of(obj);
            let bind_resource = if b.role_kind == "Role" { "roles" } else { "clusterroles" };
            let bind_ns = if b.role_kind == "Role" { namespace } else { "" };
            let may_bind = policy.authorize(user, &Attributes::resource("bind", GROUP, bind_resource, bind_ns, &b.role_name)).allowed
                || (plural == "rolebindings" && may("bind", bind_resource, &b.role_name));
            if may_bind {
                return Ok(());
            }
            let rules: Vec<Rule> = match b.role_kind.as_str() {
                "ClusterRole" => policy.cluster_roles.get(&b.role_name).cloned().unwrap_or_default(),
                _ => policy.roles.get(&(namespace.to_string(), b.role_name.clone())).cloned().unwrap_or_default(),
            };
            let gaps = missing(&rules);
            if gaps.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "user {:?} (groups {:?}) is attempting to grant RBAC permissions not currently held: {}",
                    user.username,
                    user.groups,
                    gaps.join(", ")
                ))
            }
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Reviews

/// `status` of a SubjectAccessReview-like `spec`, for `user`.
pub fn review_access(state: &Arc<ApiState>, mode: Mode, user: &UserInfo, spec: &Value, forced_ns: Option<&str>) -> Value {
    let s = |p: &str| spec.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
    let attrs = if spec.get("nonResourceAttributes").is_some_and(Value::is_object) {
        Attributes::non_resource(&s("/nonResourceAttributes/verb"), &s("/nonResourceAttributes/path"))
    } else {
        Attributes {
            verb: s("/resourceAttributes/verb"),
            resource_request: true,
            namespace: forced_ns.map(str::to_string).unwrap_or_else(|| s("/resourceAttributes/namespace")),
            api_group: s("/resourceAttributes/group"),
            api_version: s("/resourceAttributes/version"),
            resource: s("/resourceAttributes/resource"),
            subresource: s("/resourceAttributes/subresource"),
            name: s("/resourceAttributes/name"),
            path: String::new(),
        }
    };
    if mode == Mode::AlwaysAllow {
        return json!({"allowed": true, "reason": "authorization mode AlwaysAllow"});
    }
    let d = state.rbac.policy(state).authorize(user, &attrs);
    if d.allowed {
        json!({"allowed": true, "reason": d.reason})
    } else {
        json!({"allowed": false, "reason": ""})
    }
}

/// The user a SubjectAccessReview's `spec` names.
pub fn spec_user(spec: &Value) -> UserInfo {
    let extra = spec
        .get("extra")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect())).collect())
        .unwrap_or_default();
    UserInfo {
        username: spec.get("user").and_then(Value::as_str).unwrap_or_default().to_string(),
        uid: spec.get("uid").and_then(Value::as_str).unwrap_or_default().to_string(),
        groups: strings(spec, "groups"),
        extra,
    }
}

/// SelfSubjectRulesReview `status` for `user` in `namespace`.
pub fn review_rules(state: &Arc<ApiState>, mode: Mode, user: &UserInfo, namespace: &str) -> Value {
    if mode == Mode::AlwaysAllow || user.is_master() {
        return json!({
            "resourceRules": [{"verbs": ["*"], "apiGroups": ["*"], "resources": ["*"]}],
            "nonResourceRules": [{"verbs": ["*"], "nonResourceURLs": ["*"]}],
            "incomplete": false,
        });
    }
    let policy = state.rbac.policy(state);
    let mut resource_rules = Vec::new();
    let mut non_resource_rules = Vec::new();
    for (r, _) in policy.rules_for(user, namespace) {
        if r.non_resource_urls.is_empty() {
            resource_rules.push(r.to_json());
        } else {
            non_resource_rules.push(json!({"verbs": r.verbs, "nonResourceURLs": r.non_resource_urls}));
        }
    }
    json!({"resourceRules": resource_rules, "nonResourceRules": non_resource_rules, "incomplete": false})
}

// ---------------------------------------------------------------------------
// Default roles

/// The ClusterRoles and ClusterRoleBindings every cluster gets (a subset of
/// kube-apiserver's bootstrap policy). Objects that exist are left alone.
pub fn bootstrap_objects() -> Vec<(&'static str, Value)> {
    let auto = json!({"rbac.authorization.kubernetes.io/autoupdate": "true"});
    let bootstrap = json!({"kubernetes.io/bootstrapping": "rbac-defaults"});
    let role = |name: &str, labels: Value, rules: Value| {
        let mut l = bootstrap.clone();
        if let (Some(l), Some(extra)) = (l.as_object_mut(), labels.as_object()) {
            l.extend(extra.clone());
        }
        json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": {"name": name, "labels": l, "annotations": auto}, "rules": rules})
    };
    let aggregated = |name: &str, labels: Value, selector: &str| {
        let mut r = role(name, labels, json!([]));
        r["aggregationRule"] = json!({"clusterRoleSelectors": [{"matchLabels": {selector: "true"}}]});
        r
    };
    let binding = |name: &str, role: &str, subjects: Value| {
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding",
            "metadata": {"name": name, "labels": bootstrap, "annotations": auto},
            "roleRef": {"apiGroup": GROUP, "kind": "ClusterRole", "name": role},
            "subjects": subjects,
        })
    };
    let group = |g: &str| json!({"apiGroup": GROUP, "kind": "Group", "name": g});
    let rw = json!(["create", "delete", "deletecollection", "patch", "update"]);
    let ro = json!(["get", "list", "watch"]);
    let view_core = json!(["configmaps", "endpoints", "persistentvolumeclaims", "persistentvolumeclaims/status", "pods", "replicationcontrollers", "replicationcontrollers/scale", "serviceaccounts", "services", "services/status", "bindings", "events", "limitranges", "namespaces/status", "pods/log", "pods/status", "replicationcontrollers/status", "resourcequotas", "resourcequotas/status"]);
    let view_apps = json!(["controllerrevisions", "daemonsets", "daemonsets/status", "deployments", "deployments/scale", "deployments/status", "replicasets", "replicasets/scale", "replicasets/status", "statefulsets", "statefulsets/scale", "statefulsets/status"]);
    let objects = vec![
        ("clusterroles", role("cluster-admin", json!({}), json!([
            {"apiGroups": ["*"], "resources": ["*"], "verbs": ["*"]},
            {"nonResourceURLs": ["*"], "verbs": ["*"]},
        ]))),
        ("clusterroles", aggregated("admin", json!({}), "rbac.authorization.k8s.io/aggregate-to-admin")),
        ("clusterroles", aggregated("edit", json!({"rbac.authorization.k8s.io/aggregate-to-admin": "true"}), "rbac.authorization.k8s.io/aggregate-to-edit")),
        ("clusterroles", aggregated("view", json!({"rbac.authorization.k8s.io/aggregate-to-edit": "true"}), "rbac.authorization.k8s.io/aggregate-to-view")),
        ("clusterroles", role("system:aggregate-to-admin", json!({"rbac.authorization.k8s.io/aggregate-to-admin": "true"}), json!([
            {"apiGroups": ["authorization.k8s.io"], "resources": ["localsubjectaccessreviews"], "verbs": ["create"]},
            {"apiGroups": [GROUP], "resources": ["rolebindings", "roles"], "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]},
        ]))),
        ("clusterroles", role("system:aggregate-to-edit", json!({"rbac.authorization.k8s.io/aggregate-to-edit": "true"}), json!([
            {"apiGroups": [""], "resources": ["pods/attach", "pods/exec", "pods/portforward", "pods/proxy", "secrets", "services/proxy"], "verbs": ["get", "list", "watch"]},
            {"apiGroups": [""], "resources": ["serviceaccounts"], "verbs": ["impersonate"]},
            {"apiGroups": [""], "resources": ["pods", "pods/attach", "pods/exec", "pods/portforward", "pods/proxy"], "verbs": rw},
            {"apiGroups": [""], "resources": ["pods/eviction"], "verbs": ["create"]},
            {"apiGroups": [""], "resources": ["configmaps", "events", "persistentvolumeclaims", "replicationcontrollers", "replicationcontrollers/scale", "secrets", "serviceaccounts", "services", "services/proxy"], "verbs": rw},
            {"apiGroups": [""], "resources": ["serviceaccounts/token"], "verbs": ["create"]},
            {"apiGroups": ["apps"], "resources": ["daemonsets", "deployments", "deployments/rollback", "deployments/scale", "replicasets", "replicasets/scale", "statefulsets", "statefulsets/scale"], "verbs": rw},
            {"apiGroups": ["autoscaling"], "resources": ["horizontalpodautoscalers"], "verbs": rw},
            {"apiGroups": ["batch"], "resources": ["cronjobs", "jobs"], "verbs": rw},
            {"apiGroups": ["extensions"], "resources": ["daemonsets", "deployments", "deployments/rollback", "deployments/scale", "ingresses", "networkpolicies", "replicasets", "replicasets/scale", "replicationcontrollers/scale"], "verbs": rw},
            {"apiGroups": ["policy"], "resources": ["poddisruptionbudgets"], "verbs": rw},
            {"apiGroups": ["networking.k8s.io"], "resources": ["ingresses", "networkpolicies"], "verbs": rw},
            {"apiGroups": ["coordination.k8s.io"], "resources": ["leases"], "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]},
            {"apiGroups": ["discovery.k8s.io"], "resources": ["endpointslices"], "verbs": rw},
        ]))),
        ("clusterroles", role("system:aggregate-to-view", json!({"rbac.authorization.k8s.io/aggregate-to-view": "true"}), json!([
            {"apiGroups": [""], "resources": view_core, "verbs": ro},
            {"apiGroups": [""], "resources": ["namespaces"], "verbs": ro},
            {"apiGroups": ["discovery.k8s.io"], "resources": ["endpointslices"], "verbs": ro},
            {"apiGroups": ["apps"], "resources": view_apps, "verbs": ro},
            {"apiGroups": ["autoscaling"], "resources": ["horizontalpodautoscalers", "horizontalpodautoscalers/status"], "verbs": ro},
            {"apiGroups": ["batch"], "resources": ["cronjobs", "cronjobs/status", "jobs", "jobs/status"], "verbs": ro},
            {"apiGroups": ["extensions"], "resources": ["daemonsets", "daemonsets/status", "deployments", "deployments/scale", "deployments/status", "ingresses", "ingresses/status", "networkpolicies", "replicasets", "replicasets/scale", "replicasets/status", "replicationcontrollers/scale"], "verbs": ro},
            {"apiGroups": ["policy"], "resources": ["poddisruptionbudgets", "poddisruptionbudgets/status"], "verbs": ro},
            {"apiGroups": ["networking.k8s.io"], "resources": ["ingresses", "ingresses/status", "networkpolicies"], "verbs": ro},
            {"apiGroups": ["metrics.k8s.io"], "resources": ["pods", "nodes"], "verbs": ro},
        ]))),
        ("clusterroles", role("system:discovery", json!({}), json!([
            {"nonResourceURLs": ["/api", "/api/*", "/apis", "/apis/*", "/healthz", "/livez", "/openapi", "/openapi/*", "/readyz", "/version", "/version/"], "verbs": ["get"]},
        ]))),
        ("clusterroles", role("system:basic-user", json!({}), json!([
            {"apiGroups": ["authorization.k8s.io"], "resources": ["selfsubjectaccessreviews", "selfsubjectrulesreviews"], "verbs": ["create"]},
            {"apiGroups": ["authentication.k8s.io"], "resources": ["selfsubjectreviews"], "verbs": ["create"]},
        ]))),
        ("clusterroles", role("system:public-info-viewer", json!({}), json!([
            {"nonResourceURLs": ["/healthz", "/livez", "/readyz", "/version", "/version/"], "verbs": ["get"]},
        ]))),
        ("clusterroles", role("system:auth-delegator", json!({}), json!([
            {"apiGroups": ["authentication.k8s.io"], "resources": ["tokenreviews"], "verbs": ["create"]},
            {"apiGroups": ["authorization.k8s.io"], "resources": ["subjectaccessreviews"], "verbs": ["create"]},
        ]))),
        ("clusterroles", role("system:service-account-issuer-discovery", json!({}), json!([
            {"nonResourceURLs": ["/.well-known/openid-configuration", "/.well-known/openid-configuration/", "/openid/v1/jwks", "/openid/v1/jwks/"], "verbs": ["get"]},
        ]))),
        ("clusterroles", role("system:monitoring", json!({}), json!([
            {"nonResourceURLs": ["/healthz", "/healthz/*", "/livez", "/livez/*", "/metrics", "/metrics/slis", "/readyz", "/readyz/*"], "verbs": ["get"]},
        ]))),
        ("clusterrolebindings", binding("cluster-admin", "cluster-admin", json!([group(MASTERS)]))),
        ("clusterrolebindings", binding("system:discovery", "system:discovery", json!([group(AUTHENTICATED)]))),
        ("clusterrolebindings", binding("system:basic-user", "system:basic-user", json!([group(AUTHENTICATED)]))),
        ("clusterrolebindings", binding("system:public-info-viewer", "system:public-info-viewer", json!([group(AUTHENTICATED), group(UNAUTHENTICATED)]))),
        ("clusterrolebindings", binding("system:service-account-issuer-discovery", "system:service-account-issuer-discovery", json!([group("system:serviceaccounts")]))),
        ("clusterrolebindings", binding("system:monitoring", "system:monitoring", json!([group("system:monitoring")]))),
    ];
    objects
}

/// The Role extension API servers read their authentication settings with
/// (kube-system, bound by metrics-server's and other charts).
pub fn bootstrap_namespaced() -> Vec<(&'static str, &'static str, Value)> {
    vec![(
        "roles",
        "kube-system",
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
            "metadata": {"name": "extension-apiserver-authentication-reader", "namespace": "kube-system", "labels": {"kubernetes.io/bootstrapping": "rbac-defaults"}, "annotations": {"rbac.authorization.kubernetes.io/autoupdate": "true"}},
            "rules": [{"apiGroups": [""], "resources": ["configmaps"], "resourceNames": ["extension-apiserver-authentication"], "verbs": ["get", "list", "watch"]}],
        }),
    )]
}

// ---------------------------------------------------------------------------
// Aggregation

/// The rules an aggregated ClusterRole gets: those of every ClusterRole
/// its selectors match, deduplicated, in name order.
pub fn aggregate(role: &Value, all: &[Value]) -> Option<Vec<Value>> {
    let selectors = role.pointer("/aggregationRule/clusterRoleSelectors")?.as_array()?;
    let me = role.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
    let mut matched: Vec<&Value> = all
        .iter()
        .filter(|r| r.pointer("/metadata/name").and_then(Value::as_str) != Some(me))
        .filter(|r| selectors.iter().any(|s| super::selector::LabelSelector::from_object(Some(s)).is_some_and(|sel| !sel.is_empty() && sel.matches(r))))
        .collect();
    matched.sort_by_key(|r| r.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string());
    let mut rules: Vec<Value> = Vec::new();
    for r in matched {
        for rule in r.get("rules").and_then(Value::as_array).into_iter().flatten() {
            if !rules.contains(rule) {
                rules.push(rule.clone());
            }
        }
    }
    Some(rules)
}

/// Bring every aggregated ClusterRole's rules up to date.
pub fn reconcile_aggregation(state: &ApiState) {
    let (all, _) = state.store().list(GROUP, "clusterroles", None);
    for role in &all {
        let Some(rules) = aggregate(role, &all) else { continue };
        if role.get("rules").and_then(Value::as_array).is_some_and(|r| *r == rules) {
            continue;
        }
        let mut updated = role.clone();
        updated["rules"] = Value::Array(rules);
        if let Err(e) = state.update_object(GROUP, "clusterroles", updated)
            && !e.is_conflict()
        {
            eprintln!("apiserver: aggregating ClusterRole {}: {}", role.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default(), e.message);
        }
    }
}

/// The aggregationRule controller (on the leader in HA).
pub async fn run_aggregation(state: Arc<ApiState>) {
    loop {
        let Ok(sub) = state.store().subscribe_for(None, |k: &ObjectKey| k.group == GROUP && k.resource == "clusterroles") else { return };
        reconcile_aggregation(&state);
        let mut live = sub.live;
        loop {
            match live.recv().await {
                Ok(e) if e.key.group == GROUP && e.key.resource == "clusterroles" => {
                    // Settle a burst (a chart's roles) before recomputing.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    while live.try_recv().is_ok() {}
                    reconcile_aggregation(&state);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => reconcile_aggregation(&state),
                Err(_) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn request_attributes_like_upstream() {
        let a = attributes_of("GET", "/api/v1/namespaces/default/pods", &q(&[]));
        assert_eq!((a.verb.as_str(), a.resource.as_str(), a.namespace.as_str()), ("list", "pods", "default"));
        let a = attributes_of("GET", "/api/v1/namespaces/default/pods", &q(&[("watch", "true")]));
        assert_eq!(a.verb, "watch");
        let a = attributes_of("GET", "/api/v1/watch/namespaces/default/pods/web", &q(&[]));
        assert_eq!((a.verb.as_str(), a.name.as_str()), ("watch", "web"));
        let a = attributes_of("GET", "/api/v1/namespaces/kube-system", &q(&[]));
        assert_eq!((a.resource.as_str(), a.name.as_str(), a.namespace.as_str()), ("namespaces", "kube-system", "kube-system"));
        let a = attributes_of("PUT", "/api/v1/namespaces/x/finalize", &q(&[]));
        assert_eq!((a.verb.as_str(), a.resource.as_str(), a.name.as_str(), a.subresource.as_str()), ("update", "namespaces", "x", "finalize"));
        let a = attributes_of("GET", "/api/v1/namespaces/d/pods/web/exec", &q(&[("command", "sh")]));
        assert_eq!((a.verb.as_str(), a.subresource.as_str()), ("create", "exec"));
        let a = attributes_of("GET", "/api/v1/namespaces/d/pods/http:web:8080/proxy/metrics", &q(&[]));
        assert_eq!((a.verb.as_str(), a.name.as_str(), a.subresource.as_str()), ("get", "web", "proxy"));
        let a = attributes_of("DELETE", "/apis/apps/v1/namespaces/d/deployments", &q(&[]));
        assert_eq!((a.verb.as_str(), a.api_group.as_str()), ("deletecollection", "apps"));
        let a = attributes_of("GET", "/api/v1/namespaces/d/secrets", &q(&[("fieldSelector", "metadata.name=s")]));
        assert_eq!((a.verb.as_str(), a.name.as_str()), ("list", "s"));
        let a = attributes_of("GET", "/apis/apps/v1", &q(&[]));
        assert!(!a.resource_request);
        let a = attributes_of("GET", "/healthz/etcd", &q(&[]));
        assert_eq!((a.verb.as_str(), a.path.as_str(), a.resource_request), ("get", "/healthz/etcd", false));
        let a = attributes_of("GET", "/api/v1/nodes", &q(&[]));
        assert_eq!((a.verb.as_str(), a.namespace.as_str()), ("list", ""));
    }

    fn rule(v: Value) -> Rule {
        Rule::from_json(&v)
    }

    #[test]
    fn rules_match_like_upstream() {
        let r = rule(json!({"verbs": ["get"], "apiGroups": [""], "resources": ["pods", "*/status"], "resourceNames": []}));
        assert!(r.allows(&Attributes::resource("get", "", "pods", "d", "x")));
        assert!(!r.allows(&Attributes::resource("get", "", "pods/log", "d", "x")));
        assert!(r.allows(&Attributes::resource("get", "", "nodes/status", "", "n")));
        assert!(!r.allows(&Attributes::resource("list", "", "pods", "d", "")));
        let r = rule(json!({"verbs": ["*"], "apiGroups": ["*"], "resources": ["*"]}));
        assert!(r.allows(&Attributes::resource("create", "apps", "deployments/scale", "d", "x")));
        let r = rule(json!({"verbs": ["get"], "apiGroups": [""], "resources": ["secrets"], "resourceNames": ["a"]}));
        assert!(r.allows(&Attributes::resource("get", "", "secrets", "d", "a")));
        assert!(!r.allows(&Attributes::resource("get", "", "secrets", "d", "b")));
        assert!(!r.allows(&Attributes::resource("list", "", "secrets", "d", "")));
        let r = rule(json!({"verbs": ["get"], "nonResourceURLs": ["/healthz/*", "/version"]}));
        assert!(r.allows(&Attributes::non_resource("get", "/healthz/ping")));
        assert!(r.allows(&Attributes::non_resource("get", "/version")));
        assert!(!r.allows(&Attributes::non_resource("get", "/metrics")));
        assert!(!r.allows(&Attributes::resource("get", "", "pods", "", "")));
    }

    fn policy() -> Policy {
        let cr = vec![
            json!({"metadata": {"name": "reader"}, "rules": [{"verbs": ["get", "list"], "apiGroups": [""], "resources": ["pods"]}]}),
            json!({"metadata": {"name": "secrets"}, "rules": [{"verbs": ["get"], "apiGroups": [""], "resources": ["secrets"]}]}),
        ];
        let roles = vec![json!({"metadata": {"name": "exec", "namespace": "apps"}, "rules": [{"verbs": ["create"], "apiGroups": [""], "resources": ["pods/exec"]}]})];
        let crb = vec![json!({"metadata": {"name": "r"}, "roleRef": {"kind": "ClusterRole", "name": "reader"}, "subjects": [{"kind": "Group", "name": "devs"}]})];
        let rb = vec![
            json!({"metadata": {"name": "e", "namespace": "apps"}, "roleRef": {"kind": "Role", "name": "exec"}, "subjects": [{"kind": "ServiceAccount", "name": "ci"}]}),
            json!({"metadata": {"name": "s", "namespace": "apps"}, "roleRef": {"kind": "ClusterRole", "name": "secrets"}, "subjects": [{"kind": "User", "name": "ana"}]}),
        ];
        Policy::from_objects(&cr, &roles, &crb, &rb)
    }

    #[test]
    fn bindings_grant_their_roles() {
        let p = policy();
        let dev = UserInfo { username: "bob".into(), groups: vec!["devs".into()], ..Default::default() };
        assert!(p.authorize(&dev, &Attributes::resource("list", "", "pods", "any", "")).allowed);
        assert!(p.authorize(&dev, &Attributes::resource("list", "", "pods", "", "")).allowed);
        assert!(!p.authorize(&dev, &Attributes::resource("delete", "", "pods", "any", "x")).allowed);
        let ci = UserInfo { username: "system:serviceaccount:apps:ci".into(), ..Default::default() };
        assert!(p.authorize(&ci, &Attributes::resource("create", "", "pods/exec", "apps", "x")).allowed);
        assert!(!p.authorize(&ci, &Attributes::resource("create", "", "pods/exec", "other", "x")).allowed);
        let ana = UserInfo { username: "ana".into(), ..Default::default() };
        // A ClusterRole bound in a namespace grants only there.
        assert!(p.authorize(&ana, &Attributes::resource("get", "", "secrets", "apps", "s")).allowed);
        assert!(!p.authorize(&ana, &Attributes::resource("get", "", "secrets", "kube-system", "s")).allowed);
        let root = UserInfo { username: "x".into(), groups: vec![MASTERS.into()], ..Default::default() };
        assert!(p.authorize(&root, &Attributes::resource("delete", "", "nodes", "", "n")).allowed);
    }

    #[test]
    fn escalation_coverage() {
        let held = [rule(json!({"verbs": ["get", "list"], "apiGroups": [""], "resources": ["pods", "secrets"]}))];
        let held: Vec<&Rule> = held.iter().collect();
        assert!(covers(&held, &rule(json!({"verbs": ["get"], "apiGroups": [""], "resources": ["pods"]}))));
        assert!(covers(&held, &rule(json!({"verbs": ["get"], "apiGroups": [""], "resources": ["secrets"], "resourceNames": ["a"]}))));
        assert!(!covers(&held, &rule(json!({"verbs": ["delete"], "apiGroups": [""], "resources": ["pods"]}))));
        assert!(!covers(&held, &rule(json!({"verbs": ["get"], "apiGroups": [""], "resources": ["*"]}))));
        assert!(!covers(&held, &rule(json!({"verbs": ["*"], "apiGroups": [""], "resources": ["pods"]}))));
        let named = [rule(json!({"verbs": ["get"], "apiGroups": [""], "resources": ["secrets"], "resourceNames": ["a"]}))];
        let named: Vec<&Rule> = named.iter().collect();
        assert!(!covers(&named, &rule(json!({"verbs": ["get"], "apiGroups": [""], "resources": ["secrets"]}))));
        let star = [rule(json!({"verbs": ["*"], "apiGroups": ["*"], "resources": ["*"]})), rule(json!({"verbs": ["*"], "nonResourceURLs": ["*"]}))];
        let star: Vec<&Rule> = star.iter().collect();
        assert!(covers(&star, &rule(json!({"verbs": ["*"], "apiGroups": ["*"], "resources": ["*"]}))));
        assert!(covers(&star, &rule(json!({"verbs": ["get"], "nonResourceURLs": ["/metrics"]}))));
    }

    #[test]
    fn aggregation_unions_matching_roles() {
        let objs = bootstrap_objects();
        let all: Vec<Value> = objs.iter().filter(|(k, _)| *k == "clusterroles").map(|(_, v)| v.clone()).collect();
        let view = all.iter().find(|r| r["metadata"]["name"] == "view").unwrap();
        let rules = aggregate(view, &all).unwrap();
        assert!(rules.iter().any(|r| r["resources"].as_array().unwrap().iter().any(|x| x == "pods/log")));
        let edit = all.iter().find(|r| r["metadata"]["name"] == "edit").unwrap();
        // edit has the view role's label chain only through aggregate-to-*
        // roles, not through view itself (view carries aggregate-to-edit).
        let edit_rules = aggregate(edit, &all).unwrap();
        assert!(edit_rules.iter().any(|r| r["resources"].as_array().unwrap().iter().any(|x| x == "secrets")));
        let admin = all.iter().find(|r| r["metadata"]["name"] == "admin").unwrap();
        assert!(aggregate(admin, &all).unwrap().iter().any(|r| r["resources"].as_array().unwrap().iter().any(|x| x == "rolebindings")));
        assert!(aggregate(&all[0], &all).is_none());
    }
}
