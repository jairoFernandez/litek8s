//! Authentication and the authorization decision of every HTTP request,
//! before it is routed.
//!
//! Who a request comes from, in order:
//! - a request another HA server forwarded, with the user it authenticated
//!   (trusted only with the cluster secret alongside);
//! - `Authorization: Bearer`: a service account token this cluster signed
//!   (account, bound pod and audience checked as TokenReview does), or the
//!   cluster's join token (`system:masters`: whoever holds it can join a
//!   server and read every key anyway);
//! - a TLS client certificate signed by the client CA (CN user, O groups);
//! - else anonymous, which only reaches the public paths ([`is_public`]).
//!
//! Then `Impersonate-*` headers, checked with the `impersonate` verb, and
//! RBAC (`rbac`) per `--authorization-mode`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use base64::Engine;
use hyper::{HeaderMap, Method, StatusCode};
use serde_json::json;

use super::rbac::{self, Attributes, Mode, UserInfo};
use super::server::{ApiError, ApiState};

/// The user a forwarding HA server authenticated (base64 JSON).
pub const FORWARDED_USER_HEADER: &str = "x-litek8s-forwarded-user";
/// The cluster secret, which makes [`FORWARDED_USER_HEADER`] trusted.
pub const FORWARDED_AUTH_HEADER: &str = "x-litek8s-forwarded-auth";
/// Who presents the join token.
pub const CLUSTER_TOKEN_USER: &str = "system:litek8s:cluster-token";

/// What the connection says about the client (its certificate).
#[derive(Debug, Clone, Default)]
pub struct Conn {
    pub cert_user: Option<UserInfo>,
}

impl Conn {
    /// From the verified certificate chain of a TLS connection.
    pub fn from_certificates(certs: Option<&[rustls::pki_types::CertificateDer<'_>]>) -> Self {
        let cert_user = certs.and_then(|c| c.first()).and_then(|c| super::tls::certificate_subject(c)).map(|(user, groups)| {
            UserInfo { username: user, groups, ..Default::default() }.authenticated()
        });
        Self { cert_user }
    }
}

/// Authorization settings and audit bookkeeping of a server.
#[derive(Default)]
pub struct Authz {
    pub mode: Mode,
    /// What was already logged (RBAC-audit allows, RBAC denials), so the
    /// log names each user × verb × resource once.
    seen: Mutex<HashSet<String>>,
}

impl Authz {
    pub fn new(mode: Mode) -> Self {
        Self { mode, seen: Mutex::default() }
    }

    fn first_time(&self, key: String) -> bool {
        let mut seen = self.seen.lock().unwrap();
        if seen.len() > 20_000 {
            seen.clear();
        }
        seen.insert(key)
    }
}

/// Paths anyone may read without credentials.
pub fn is_public(method: &Method, path: &str) -> bool {
    let p = path.trim_end_matches('/');
    let get = *method == Method::GET || *method == Method::HEAD;
    match p {
        "/apis/agent.litek8s.dev/v1/connect" | "/apis/agent.litek8s.dev/v1/join" => true,
        "/version" | "/.well-known/openid-configuration" | "/openid/v1/jwks" | "/cacerts" => get,
        _ => get && ["/healthz", "/livez", "/readyz"].iter().any(|probe| p == *probe || p.starts_with(&format!("{probe}/"))),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unauthorized() -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, "Unauthorized", "Unauthorized")
}

/// Who presents `headers` on `conn`: `Ok(None)` without credentials,
/// `Err` with invalid ones.
pub fn authenticate(state: &ApiState, headers: &HeaderMap, conn: &Conn) -> Result<Option<UserInfo>, String> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default();
    if let Some(cluster) = state.ha() {
        let presented = header(FORWARDED_AUTH_HEADER);
        if !presented.is_empty() && constant_time_eq(presented.as_bytes(), cluster.peers.secret().as_bytes()) {
            let user = base64::engine::general_purpose::STANDARD
                .decode(header(FORWARDED_USER_HEADER))
                .ok()
                .and_then(|b| serde_json::from_slice::<UserInfo>(&b).ok());
            if let Some(user) = user.filter(|u| !u.username.is_empty()) {
                return Ok(Some(user));
            }
        }
    }
    let auth = header("authorization");
    if let Some(token) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")) {
        let token = token.trim();
        if token.matches('.').count() == 2 {
            return state.authenticate_service_account(token).map(Some);
        }
        let secret = super::join::Token::parse(token).map(|t| t.secret).unwrap_or_default();
        if !secret.is_empty() && constant_time_eq(secret.as_bytes(), state.join_secret().as_bytes()) {
            return Ok(Some(UserInfo { username: CLUSTER_TOKEN_USER.into(), groups: vec![rbac::MASTERS.into(), "system:nodes".into()], ..Default::default() }.authenticated()));
        }
        return Err("invalid bearer token".into());
    }
    if !auth.is_empty() {
        return Err("unsupported authorization scheme (bearer tokens and client certificates only)".into());
    }
    Ok(conn.cert_user.clone())
}

/// Authenticate and authorize a request. `Ok(user)`: go ahead, as `user`
/// (`None`: no credentials under AlwaysAllow, the old unrestricted mode).
pub(crate) fn check(state: &Arc<ApiState>, method: &Method, path: &str, query: &HashMap<String, String>, headers: &HeaderMap, conn: &Conn) -> Result<Option<UserInfo>, ApiError> {
    let mode = state.authz.mode;
    let public = is_public(method, path);
    let user = match authenticate(state, headers, conn) {
        Ok(user) => user,
        Err(reason) => match mode {
            Mode::Rbac if !public => {
                log_once(state, format!("401 {reason} {method} {path}"), || format!("apiserver: authentication failed ({reason}): {method} {path}"));
                return Err(unauthorized());
            }
            Mode::RbacAudit if !public => {
                log_once(state, format!("401 {reason} {method} {path}"), || format!("apiserver: RBAC-audit: would reject ({reason}): {method} {path}"));
                None
            }
            _ => None,
        },
    };
    let user = match user {
        Some(u) => Some(impersonate(state, u, headers)?),
        None => None,
    };
    if mode == Mode::AlwaysAllow || public {
        return Ok(user);
    }
    let Some(user) = user else {
        if mode == Mode::Rbac {
            return Err(unauthorized());
        }
        log_once(state, format!("anon {method} {path}"), || format!("apiserver: RBAC-audit: would reject an anonymous request: {method} {path}"));
        return Ok(Some(UserInfo::anonymous()));
    };
    let attrs = rbac::attributes_of(method.as_str(), path, query);
    let decision = state.rbac.policy(state).authorize(&user, &attrs);
    if decision.allowed {
        if mode == Mode::RbacAudit && !user.is_master() {
            let key = format!("allow {} {}", user.username, attrs_key(&attrs));
            log_once(state, key, || format!("apiserver: RBAC-audit: ALLOW user={:?} {} ({})", user.username, attrs.describe(), decision.reason));
        }
        return Ok(Some(user));
    }
    match mode {
        Mode::RbacAudit => {
            eprintln!("apiserver: RBAC-audit: DENY user={:?} groups={:?} {}", user.username, user.groups, attrs.describe());
            Ok(Some(user))
        }
        _ => {
            log_once(state, format!("deny {} {}", user.username, attrs_key(&attrs)), || {
                format!("apiserver: RBAC: forbidden: user={:?} groups={:?} {}", user.username, user.groups, attrs.describe())
            });
            Err(forbidden(&user, &attrs))
        }
    }
}

/// The attributes without the object name (one log line per kind of access).
fn attrs_key(a: &Attributes) -> String {
    if a.resource_request {
        format!("{} {}/{}/{} {}", a.verb, a.api_group, a.resource, a.subresource, a.namespace)
    } else {
        format!("{} {}", a.verb, a.path)
    }
}

fn log_once(state: &ApiState, key: String, line: impl FnOnce() -> String) {
    if state.authz.first_time(key) {
        eprintln!("{}", line());
    }
}

/// kube-apiserver's 403 for `attrs`.
pub(crate) fn forbidden(user: &UserInfo, a: &Attributes) -> ApiError {
    if !a.resource_request {
        return ApiError::new(StatusCode::FORBIDDEN, "Forbidden", format!("forbidden: User {:?} cannot {} path {:?}", user.username, a.verb, a.path));
    }
    let qualified = if a.api_group.is_empty() { a.resource.clone() } else { format!("{}.{}", a.resource, a.api_group) };
    let subject = if a.name.is_empty() { qualified } else { format!("{qualified} {:?}", a.name) };
    let resource = if a.subresource.is_empty() { a.resource.clone() } else { format!("{}/{}", a.resource, a.subresource) };
    let scope = if a.namespace.is_empty() { "at the cluster scope".to_string() } else { format!("in the namespace {:?}", a.namespace) };
    let message = format!("{subject} is forbidden: User {:?} cannot {} resource {resource:?} in API group {:?} {scope}", user.username, a.verb, a.api_group);
    ApiError::new(StatusCode::FORBIDDEN, "Forbidden", message).with_details(json!({"name": a.name, "group": a.api_group, "kind": a.resource}))
}

/// Apply `Impersonate-*` headers: the user must hold `impersonate` on
/// each user, group, uid and extra named.
fn impersonate(state: &Arc<ApiState>, user: UserInfo, headers: &HeaderMap) -> Result<UserInfo, ApiError> {
    let name = headers.get("impersonate-user").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let groups: Vec<String> = headers.get_all("impersonate-group").iter().filter_map(|v| v.to_str().ok()).map(str::to_string).collect();
    let uid = headers.get("impersonate-uid").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let mut extra: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (k, v) in headers.iter() {
        if let (Some(key), Ok(v)) = (k.as_str().strip_prefix("impersonate-extra-"), v.to_str()) {
            let key = percent_decode(key);
            extra.entry(key).or_default().push(v.to_string());
        }
    }
    if name.is_empty() {
        if !groups.is_empty() || !uid.is_empty() || !extra.is_empty() {
            return Err(ApiError::bad_request("requested impersonation of groups, uid or extra without a user"));
        }
        return Ok(user);
    }
    let mode = state.authz.mode;
    if mode != Mode::AlwaysAllow && !user.is_master() {
        let mut wanted = Vec::new();
        match name.strip_prefix("system:serviceaccount:").and_then(|r| r.split_once(':')) {
            Some((ns, sa)) => wanted.push(Attributes::resource("impersonate", "", "serviceaccounts", ns, sa)),
            None => wanted.push(Attributes::resource("impersonate", "", "users", "", &name)),
        }
        for g in &groups {
            wanted.push(Attributes::resource("impersonate", "", "groups", "", g));
        }
        if !uid.is_empty() {
            wanted.push(Attributes::resource("impersonate", "authentication.k8s.io", "uids", "", &uid));
        }
        for (k, vs) in &extra {
            for v in vs {
                wanted.push(Attributes::resource("impersonate", "authentication.k8s.io", &format!("userextras/{k}"), "", v));
            }
        }
        let policy = state.rbac.policy(state);
        for a in &wanted {
            if !policy.authorize(&user, a).allowed {
                if mode == Mode::RbacAudit {
                    eprintln!("apiserver: RBAC-audit: DENY user={:?} {}", user.username, a.describe());
                    continue;
                }
                return Err(forbidden(&user, a));
            }
        }
    }
    let mut out = UserInfo { username: name.clone(), uid, groups, extra };
    if let Some((ns, _)) = name.strip_prefix("system:serviceaccount:").and_then(|r| r.split_once(':')) {
        for g in ["system:serviceaccounts".to_string(), format!("system:serviceaccounts:{ns}")] {
            if !out.groups.contains(&g) {
                out.groups.push(g);
            }
        }
    }
    if out.is_anonymous() {
        if !out.groups.iter().any(|g| g == rbac::UNAUTHENTICATED) {
            out.groups.push(rbac::UNAUTHENTICATED.into());
        }
        Ok(out)
    } else {
        Ok(out.authenticated())
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_paths() {
        for p in ["/healthz", "/livez", "/readyz", "/readyz/ping", "/version", "/.well-known/openid-configuration", "/openid/v1/jwks", "/cacerts"] {
            assert!(is_public(&Method::GET, p), "{p}");
        }
        assert!(is_public(&Method::POST, "/apis/agent.litek8s.dev/v1/join"));
        for p in ["/api", "/apis", "/metrics", "/api/v1/secrets", "/healthzz", "/openapi/v2", "/litek8s/v1/logs", "/prometheus/api/v1/query"] {
            assert!(!is_public(&Method::GET, p), "{p}");
        }
        assert!(!is_public(&Method::POST, "/version"));
    }

    #[test]
    fn decodes_extra_keys() {
        assert_eq!(percent_decode("example.com%2fscope"), "example.com/scope");
        assert_eq!(percent_decode("plain"), "plain");
    }
}
