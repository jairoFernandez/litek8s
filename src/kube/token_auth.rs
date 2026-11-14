//! Bearer token authenticators besides the cluster's own tokens, with
//! kube-apiserver's flags and semantics:
//! - `--token-auth-file`: a CSV of static tokens (`token,user,uid,"g1,g2"`);
//! - `--oidc-*`: ID tokens of an OpenID Connect issuer, checked against the
//!   keys its discovery document names (fetched when first needed, again
//!   when a token names a key not seen yet);
//! - `--authentication-token-webhook-config-file`: a TokenReview sent to a
//!   remote service, its answers cached (`--authentication-token-webhook-cache-ttl`).
//!
//! `authn` asks them in kube-apiserver's order: static tokens, the
//! cluster's tokens or OIDC (by the JWT's issuer), then the webhook for
//! whatever none of them recognized.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::Request;
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use serde_json::{Value, json};
use sha2::Digest;

use super::rbac::UserInfo;

/// How long a remote issuer or webhook may take.
const TIMEOUT: Duration = Duration::from_secs(10);
/// A token naming an unknown key refetches the issuer's keys at most this often.
const JWKS_REFRESH: Duration = Duration::from_secs(10);
/// How long a webhook's rejection is remembered (its approvals: the cache TTL).
const WEBHOOK_FAILURE_TTL: Duration = Duration::from_secs(10);
/// Clock skew tolerated on `exp`, `nbf` and `iat`.
const LEEWAY: i64 = 30;

/// The flags.
#[derive(Debug, Clone, Default)]
pub struct TokenAuthConfig {
    pub token_file: Option<PathBuf>,
    pub oidc: Option<OidcConfig>,
    pub webhook: Option<WebhookConfig>,
}

#[derive(Debug, Clone)]
pub struct OidcConfig {
    pub issuer_url: String,
    pub client_id: String,
    /// Default `sub`.
    pub username_claim: String,
    /// `None`: kube-apiserver's default (`<issuer>#` unless the claim is
    /// `email`); `-` for none.
    pub username_prefix: Option<String>,
    pub groups_claim: Option<String>,
    pub groups_prefix: String,
    pub ca_file: Option<PathBuf>,
    pub required_claims: Vec<(String, String)>,
    /// Default `RS256`.
    pub signing_algs: Vec<String>,
}

impl OidcConfig {
    pub fn new(issuer_url: &str, client_id: &str) -> Self {
        Self {
            issuer_url: issuer_url.into(),
            client_id: client_id.into(),
            username_claim: "sub".into(),
            username_prefix: None,
            groups_claim: None,
            groups_prefix: String::new(),
            ca_file: None,
            required_claims: Vec::new(),
            signing_algs: vec!["RS256".into()],
        }
    }
}

#[derive(Debug, Clone)]
pub struct WebhookConfig {
    /// A kubeconfig: its cluster is the service, its user our credentials.
    pub config_file: PathBuf,
    pub cache_ttl: Duration,
}

/// The authenticators of a server, loaded from [`TokenAuthConfig`].
#[derive(Default)]
pub struct TokenAuth {
    static_tokens: HashMap<String, UserInfo>,
    oidc: Option<Oidc>,
    webhook: Option<Webhook>,
}

impl TokenAuth {
    /// Reads the files the flags name; fails on a bad one (at start, not
    /// on the first request).
    pub fn load(config: &TokenAuthConfig) -> Result<Self> {
        let static_tokens = match &config.token_file {
            Some(path) => {
                let text = std::fs::read_to_string(path).with_context(|| format!("failed to read --token-auth-file {}", path.display()))?;
                parse_token_file(&text).with_context(|| format!("--token-auth-file {}", path.display()))?
            }
            None => HashMap::new(),
        };
        let oidc = config.oidc.clone().map(Oidc::new).transpose()?;
        let webhook = config.webhook.as_ref().map(Webhook::load).transpose()?;
        Ok(Self { static_tokens, oidc, webhook })
    }

    /// What the startup line lists.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.static_tokens.is_empty() {
            out.push(format!("static tokens ({})", self.static_tokens.len()));
        }
        if let Some(o) = &self.oidc {
            out.push(format!("OIDC ({})", o.config.issuer_url));
        }
        if let Some(w) = &self.webhook {
            out.push(format!("token webhook ({})", w.server));
        }
        out
    }

    pub fn static_user(&self, token: &str) -> Option<UserInfo> {
        // A map lookup by the token itself: its timing tells nothing useful
        // about other tokens' bytes (the hash decides the bucket).
        self.static_tokens.get(token).cloned()
    }

    /// The OIDC authenticator, when `token` is a JWT its issuer signed.
    pub fn oidc_for(&self, token: &str) -> Option<&Oidc> {
        let oidc = self.oidc.as_ref()?;
        let claims = unverified_claims(token)?;
        (claims.get("iss").and_then(Value::as_str) == Some(oidc.config.issuer_url.as_str())).then_some(oidc)
    }

    pub fn webhook(&self) -> Option<&Webhook> {
        self.webhook.as_ref()
    }
}

/// `token,user,uid[,"group1,group2"]` per line, as kube-apiserver reads it.
fn parse_token_file(text: &str) -> Result<HashMap<String, UserInfo>> {
    let mut out = HashMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields = csv_fields(line);
        if fields.len() < 3 {
            bail!("line {}: want token,user,uid[,groups]", i + 1);
        }
        let (token, user) = (fields[0].trim(), fields[1].trim());
        if token.is_empty() || user.is_empty() {
            bail!("line {}: empty token or user", i + 1);
        }
        let groups = fields.get(3).map(|g| g.split(',').map(str::trim).filter(|g| !g.is_empty()).map(str::to_string).collect()).unwrap_or_default();
        let info = UserInfo { username: user.into(), uid: fields[2].trim().into(), groups, ..Default::default() }.authenticated();
        if out.insert(token.to_string(), info).is_some() {
            bail!("line {}: duplicate token", i + 1);
        }
    }
    Ok(out)
}

/// The fields of one CSV line (double quotes group, `""` is a quote).
fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                fields.last_mut().unwrap().push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(String::new()),
            c => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

fn b64url(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// A JWT's payload, signature not checked.
fn unverified_claims(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let (_, payload, _) = (parts.next()?, parts.next()?, parts.next()?);
    serde_json::from_slice(&b64url(payload)?).ok()
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or_default()
}

// ---- OIDC ----

pub struct Oidc {
    config: OidcConfig,
    tls: TlsSettings,
    keys: tokio::sync::Mutex<KeySet>,
}

#[derive(Default)]
struct KeySet {
    keys: Vec<Value>,
    fetched: Option<Instant>,
}

impl Oidc {
    fn new(config: OidcConfig) -> Result<Self> {
        if !config.issuer_url.starts_with("https://") {
            bail!("--oidc-issuer-url must be https: {}", config.issuer_url);
        }
        if config.client_id.is_empty() {
            bail!("--oidc-client-id is required with --oidc-issuer-url");
        }
        for alg in &config.signing_algs {
            if !SUPPORTED_ALGS.contains(&alg.as_str()) {
                bail!("--oidc-signing-algs: {alg} is not supported (supported: {})", SUPPORTED_ALGS.join(", "));
            }
        }
        let ca = match &config.ca_file {
            Some(p) => Some(std::fs::read(p).with_context(|| format!("failed to read --oidc-ca-file {}", p.display()))?),
            None => None,
        };
        Ok(Self { config, tls: TlsSettings { ca, ..Default::default() }, keys: Default::default() })
    }

    /// The user `token` names, or why it is not valid.
    pub async fn authenticate(&self, token: &str) -> Result<UserInfo, String> {
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err("oidc: malformed token".into());
        };
        let header: Value = b64url(h).and_then(|b| serde_json::from_slice(&b).ok()).ok_or("oidc: malformed token header")?;
        let claims: Value = b64url(p).and_then(|b| serde_json::from_slice(&b).ok()).ok_or("oidc: malformed token claims")?;
        let signature = b64url(s).ok_or("oidc: malformed token signature")?;
        let alg = header.get("alg").and_then(Value::as_str).unwrap_or_default();
        if !self.config.signing_algs.iter().any(|a| a == alg) {
            return Err(format!("oidc: token signed with {alg:?}, accepted: {}", self.config.signing_algs.join(",")));
        }
        let kid = header.get("kid").and_then(Value::as_str);
        let message = &token[..h.len() + 1 + p.len()];
        self.verify_signature(alg, kid, message.as_bytes(), &signature).await?;
        self.user_of(&claims, now())
    }

    async fn verify_signature(&self, alg: &str, kid: Option<&str>, message: &[u8], signature: &[u8]) -> Result<(), String> {
        let mut set = self.keys.lock().await;
        for attempt in 0..2 {
            if attempt == 1 || set.fetched.is_none() {
                if set.fetched.is_some_and(|t| t.elapsed() < JWKS_REFRESH) {
                    break;
                }
                set.fetched = Some(Instant::now());
                match self.fetch_keys().await {
                    Ok(keys) => set.keys = keys,
                    Err(e) => return Err(format!("oidc: failed to fetch the issuer's keys: {e:#}")),
                }
            }
            let candidates: Vec<&Value> = set
                .keys
                .iter()
                .filter(|k| kid.is_none_or(|kid| k.get("kid").and_then(Value::as_str) == Some(kid)))
                .filter(|k| k.get("use").and_then(Value::as_str).is_none_or(|u| u == "sig"))
                .collect();
            if candidates.iter().any(|k| verify_jws(alg, k, message, signature)) {
                return Ok(());
            }
            if !candidates.is_empty() && kid.is_some() {
                // The key is known and the signature does not match it.
                return Err("oidc: invalid token signature".into());
            }
        }
        Err("oidc: invalid token signature (no matching key)".into())
    }

    async fn fetch_keys(&self) -> Result<Vec<Value>> {
        let issuer = self.config.issuer_url.trim_end_matches('/');
        let discovery: Value = serde_json::from_slice(&https_request("GET", &format!("{issuer}/.well-known/openid-configuration"), None, None, &self.tls).await?)?;
        let named = discovery.get("issuer").and_then(Value::as_str).unwrap_or_default();
        if named != self.config.issuer_url {
            bail!("the discovery document names issuer {named:?}, not {:?}", self.config.issuer_url);
        }
        let jwks_uri = discovery.get("jwks_uri").and_then(Value::as_str).context("the discovery document has no jwks_uri")?;
        if !jwks_uri.starts_with("https://") {
            bail!("jwks_uri must be https: {jwks_uri}");
        }
        let jwks: Value = serde_json::from_slice(&https_request("GET", jwks_uri, None, None, &self.tls).await?)?;
        Ok(jwks.get("keys").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// The user of verified `claims` at time `now`.
    fn user_of(&self, claims: &Value, now: i64) -> Result<UserInfo, String> {
        let c = &self.config;
        if claims.get("iss").and_then(Value::as_str) != Some(c.issuer_url.as_str()) {
            return Err("oidc: wrong issuer".into());
        }
        let aud_ok = match claims.get("aud") {
            Some(Value::String(a)) => *a == c.client_id,
            Some(Value::Array(a)) => a.iter().any(|a| a.as_str() == Some(c.client_id.as_str())),
            _ => false,
        };
        if !aud_ok {
            return Err(format!("oidc: token is not for client {:?}", c.client_id));
        }
        let exp = claims.get("exp").and_then(Value::as_f64).ok_or("oidc: token has no exp")? as i64;
        if now > exp + LEEWAY {
            return Err("oidc: token has expired".into());
        }
        if claims.get("nbf").and_then(Value::as_f64).is_some_and(|nbf| now + LEEWAY < nbf as i64) {
            return Err("oidc: token is not valid yet".into());
        }
        for (k, v) in &c.required_claims {
            if claims.get(k).and_then(Value::as_str) != Some(v.as_str()) {
                return Err(format!("oidc: required claim {k} is not {v:?}"));
            }
        }
        let name = match claims.get(&c.username_claim) {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => return Err(format!("oidc: token has no {:?} claim", c.username_claim)),
        };
        if c.username_claim == "email" {
            match claims.get("email_verified") {
                None | Some(Value::Bool(true)) => {}
                _ => return Err("oidc: email not verified".into()),
            }
        }
        let prefix = match c.username_prefix.as_deref() {
            Some("-") => String::new(),
            Some(p) => p.to_string(),
            None if c.username_claim == "email" => String::new(),
            None => format!("{}#", c.issuer_url),
        };
        let username = format!("{prefix}{name}");
        if username.starts_with("system:") {
            return Err("oidc: usernames starting with system: are reserved".into());
        }
        let mut groups = Vec::new();
        if let Some(claim) = &c.groups_claim {
            match claims.get(claim) {
                Some(Value::String(g)) => groups.push(g.clone()),
                Some(Value::Array(gs)) => groups.extend(gs.iter().filter_map(Value::as_str).map(str::to_string)),
                None => {}
                Some(_) => return Err(format!("oidc: claim {claim} is neither a string nor an array of strings")),
            }
            for g in &mut groups {
                g.insert_str(0, &c.groups_prefix);
            }
        }
        Ok(UserInfo { username, groups, ..Default::default() }.authenticated())
    }
}

const SUPPORTED_ALGS: &[&str] = &["RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384", "EdDSA"];

/// Whether `signature` over `message` checks with the JWK `key` under `alg`.
fn verify_jws(alg: &str, key: &Value, message: &[u8], signature: &[u8]) -> bool {
    use ring::signature as sig;
    let field = |k: &str| key.get(k).and_then(Value::as_str).and_then(b64url);
    if key.get("alg").and_then(Value::as_str).is_some_and(|a| a != alg) {
        return false;
    }
    let kty = key.get("kty").and_then(Value::as_str).unwrap_or_default();
    match (alg, kty) {
        ("RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512", "RSA") => {
            let (Some(n), Some(e)) = (field("n"), field("e")) else { return false };
            let params: &sig::RsaParameters = match alg {
                "RS256" => &sig::RSA_PKCS1_2048_8192_SHA256,
                "RS384" => &sig::RSA_PKCS1_2048_8192_SHA384,
                "RS512" => &sig::RSA_PKCS1_2048_8192_SHA512,
                "PS256" => &sig::RSA_PSS_2048_8192_SHA256,
                "PS384" => &sig::RSA_PSS_2048_8192_SHA384,
                _ => &sig::RSA_PSS_2048_8192_SHA512,
            };
            sig::RsaPublicKeyComponents { n: &n, e: &e }.verify(params, message, signature).is_ok()
        }
        ("ES256" | "ES384", "EC") => {
            let (alg, crv): (&sig::EcdsaVerificationAlgorithm, &str) =
                if alg == "ES256" { (&sig::ECDSA_P256_SHA256_FIXED, "P-256") } else { (&sig::ECDSA_P384_SHA384_FIXED, "P-384") };
            if key.get("crv").and_then(Value::as_str) != Some(crv) {
                return false;
            }
            let (Some(x), Some(y)) = (field("x"), field("y")) else { return false };
            let point = [&[4u8][..], &x, &y].concat();
            sig::UnparsedPublicKey::new(alg, point).verify(message, signature).is_ok()
        }
        ("EdDSA", "OKP") => {
            if key.get("crv").and_then(Value::as_str) != Some("Ed25519") {
                return false;
            }
            let Some(x) = field("x") else { return false };
            sig::UnparsedPublicKey::new(&sig::ED25519, x).verify(message, signature).is_ok()
        }
        _ => false,
    }
}

// ---- Webhook ----

type Cached = (Instant, Result<UserInfo, String>);

pub struct Webhook {
    server: String,
    tls: TlsSettings,
    bearer: Option<String>,
    cache_ttl: Duration,
    /// sha256 of the token → until when, and the answer.
    cache: Mutex<HashMap<[u8; 32], Cached>>,
}

impl Webhook {
    fn load(config: &WebhookConfig) -> Result<Self> {
        let path = &config.config_file;
        let text = std::fs::read_to_string(path).with_context(|| format!("failed to read --authentication-token-webhook-config-file {}", path.display()))?;
        let kubeconfig: Value = serde_yaml::from_str(&text).with_context(|| format!("{} is not a kubeconfig", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut w = webhook_from_kubeconfig(&kubeconfig, dir).with_context(|| format!("--authentication-token-webhook-config-file {}", path.display()))?;
        w.cache_ttl = config.cache_ttl;
        Ok(w)
    }

    /// The user the service says `token` is.
    pub async fn authenticate(&self, token: &str) -> Result<UserInfo, String> {
        let key: [u8; 32] = sha2::Sha256::digest(token.as_bytes()).into();
        if let Some((until, answer)) = self.cache.lock().unwrap().get(&key)
            && Instant::now() < *until
        {
            return answer.clone();
        }
        let review = json!({"apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview", "spec": {"token": token}});
        let answer = match https_request("POST", &self.server, Some(serde_json::to_vec(&review).unwrap()), self.bearer.as_deref(), &self.tls).await {
            Ok(body) => match serde_json::from_slice::<Value>(&body) {
                Ok(v) => review_answer(&v),
                Err(e) => return Err(format!("token webhook: bad answer: {e}")),
            },
            // Not cached: the next request asks again.
            Err(e) => return Err(format!("token webhook: {e:#}")),
        };
        let ttl = if answer.is_ok() { self.cache_ttl } else { WEBHOOK_FAILURE_TTL };
        let mut cache = self.cache.lock().unwrap();
        if cache.len() > 10_000 {
            let now = Instant::now();
            cache.retain(|_, (until, _)| *until > now);
            if cache.len() > 10_000 {
                cache.clear();
            }
        }
        cache.insert(key, (Instant::now() + ttl, answer.clone()));
        answer
    }
}

/// The user of a TokenReview the webhook answered.
fn review_answer(v: &Value) -> Result<UserInfo, String> {
    let status = v.get("status").cloned().unwrap_or_default();
    if status.get("authenticated").and_then(Value::as_bool) != Some(true) {
        let error = status.get("error").and_then(Value::as_str).unwrap_or("token not authenticated");
        return Err(format!("token webhook: {error}"));
    }
    let user = UserInfo::from_json(status.get("user").unwrap_or(&Value::Null));
    if user.username.is_empty() {
        return Err("token webhook: authenticated without a username".into());
    }
    Ok(user.authenticated())
}

/// The service and credentials of a webhook kubeconfig (its current
/// context, or its first cluster and user).
fn webhook_from_kubeconfig(k: &Value, dir: &Path) -> Result<Webhook> {
    let named = |list: &str, name: Option<&str>, inner: &str| -> Value {
        let items = k.get(list).and_then(Value::as_array).cloned().unwrap_or_default();
        let item = match name {
            Some(n) => items.iter().find(|i| i.get("name").and_then(Value::as_str) == Some(n)).cloned(),
            None => items.first().cloned(),
        };
        item.and_then(|i| i.get(inner).cloned()).unwrap_or_default()
    };
    let current = k.get("current-context").and_then(Value::as_str).filter(|c| !c.is_empty());
    let context = current.map(|c| named("contexts", Some(c), "context"));
    let cluster_name = context.as_ref().and_then(|c| c.get("cluster")).and_then(Value::as_str);
    let user_name = context.as_ref().and_then(|c| c.get("user")).and_then(Value::as_str);
    let cluster = named("clusters", cluster_name, "cluster");
    let user = named("users", user_name, "user");
    let s = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let file_or_data = |v: &Value, file: &str, data: &str| -> Result<Option<Vec<u8>>> {
        if let Some(d) = s(v, data) {
            return Ok(Some(base64::engine::general_purpose::STANDARD.decode(d.trim()).with_context(|| format!("{data} is not base64"))?));
        }
        match s(v, file) {
            Some(f) => {
                let p = dir.join(f);
                Ok(Some(std::fs::read(&p).with_context(|| format!("failed to read {}", p.display()))?))
            }
            None => Ok(None),
        }
    };
    let server = s(&cluster, "server").context("the kubeconfig names no cluster server")?;
    if !server.starts_with("https://") {
        bail!("the webhook server must be https: {server}");
    }
    let ca = file_or_data(&cluster, "certificate-authority", "certificate-authority-data")?;
    let insecure = cluster.get("insecure-skip-tls-verify").and_then(Value::as_bool).unwrap_or(false);
    let client = match (file_or_data(&user, "client-certificate", "client-certificate-data")?, file_or_data(&user, "client-key", "client-key-data")?) {
        (Some(cert), Some(key)) => {
            let certs = CertificateDer::pem_slice_iter(&cert).collect::<Result<Vec<_>, _>>().context("bad client certificate")?;
            let key = PrivateKeyDer::from_pem_slice(&key).context("bad client key")?;
            Some((certs, key))
        }
        (None, None) => None,
        _ => bail!("the kubeconfig user has a client certificate or key without the other"),
    };
    let bearer = match (s(&user, "token"), s(&user, "tokenFile")) {
        (Some(t), _) => Some(t),
        (None, Some(f)) => Some(std::fs::read_to_string(dir.join(&f)).with_context(|| format!("failed to read tokenFile {f}"))?.trim().to_string()),
        _ => None,
    };
    Ok(Webhook {
        server,
        tls: TlsSettings { ca, insecure, client: client.map(Arc::new) },
        bearer,
        cache_ttl: Duration::from_secs(120),
        cache: Mutex::default(),
    })
}

// ---- HTTPS client ----

#[derive(Default)]
struct TlsSettings {
    /// PEM bundle; the system roots without it.
    ca: Option<Vec<u8>>,
    insecure: bool,
    client: Option<Arc<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>>,
}

/// `https://host[:port]/path?query` as (dial address, TLS server name, path).
fn url_parts(url: &str) -> Result<(String, String, String)> {
    let rest = url.strip_prefix("https://").context("URL must be https")?;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
        Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
        None => (rest, "/".into()),
    };
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (h, after) = v6.split_once(']').context("bad IPv6 address")?;
            (h.to_string(), after.strip_prefix(':').unwrap_or("443").to_string())
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.to_string()),
            None => (authority.to_string(), "443".into()),
        },
    };
    let addr = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    Ok((addr, host, path))
}

/// The body of a 2xx answer to `method url`.
async fn https_request(method: &str, url: &str, body: Option<Vec<u8>>, bearer: Option<&str>, tls: &TlsSettings) -> Result<Bytes> {
    tokio::time::timeout(TIMEOUT, async {
        let (addr, host, path) = url_parts(url)?;
        let tcp = tokio::net::TcpStream::connect(&addr).await.with_context(|| format!("failed to connect to {addr}"))?;
        let stream: Box<dyn super::admission::Io> = if tls.insecure {
            Box::new(super::agent::probe::tls_connect_any(tcp, &host).await?)
        } else {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let builder = rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?;
            let roots = match tls.ca.as_deref().filter(|c| !c.is_empty()) {
                Some(pem) => {
                    let mut roots = rustls::RootCertStore::empty();
                    for cert in CertificateDer::pem_slice_iter(pem) {
                        roots.add(cert?)?;
                    }
                    roots
                }
                None => rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() },
            };
            let builder = builder.with_root_certificates(roots);
            let config = match &tls.client {
                Some(c) => builder.with_client_auth_cert(c.0.clone(), c.1.clone_key())?,
                None => builder.with_no_client_auth(),
            };
            let name = ServerName::try_from(host.clone())?;
            Box::new(tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await?)
        };
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = Request::builder().method(method).uri(&path).header(hyper::header::HOST, &host).header(hyper::header::ACCEPT, "application/json");
        if body.is_some() {
            req = req.header(hyper::header::CONTENT_TYPE, "application/json");
        }
        if let Some(t) = bearer {
            req = req.header(hyper::header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = sender.send_request(req.body(Full::new(Bytes::from(body.unwrap_or_default())))?).await?;
        let status = resp.status();
        let bytes = resp.into_body().collect().await?.to_bytes();
        if !status.is_success() {
            bail!("{url} answered {status}: {}", String::from_utf8_lossy(&bytes[..bytes.len().min(300)]));
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| anyhow::anyhow!("{url} did not answer within {}s", TIMEOUT.as_secs()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_like_kube_apiserver() {
        let users = parse_token_file("t1,alice,1\n\nt2,bob,2,\"dev,ops\"\nt3,carol,3,admins\n").unwrap();
        assert_eq!(users["t1"].username, "alice");
        assert!(users["t1"].groups.contains(&"system:authenticated".to_string()));
        assert_eq!(users["t2"].uid, "2");
        assert!(users["t2"].groups.starts_with(&["dev".to_string(), "ops".to_string()]));
        assert!(users["t3"].groups.contains(&"admins".to_string()));
        assert!(parse_token_file("t1,alice\n").is_err());
        assert!(parse_token_file("t1,a,1\nt1,b,2\n").is_err());
    }

    #[test]
    fn urls() {
        assert_eq!(url_parts("https://idp.example.com").unwrap(), ("idp.example.com:443".into(), "idp.example.com".into(), "/".into()));
        assert_eq!(url_parts("https://10.0.0.1:8443/authn?x=1").unwrap(), ("10.0.0.1:8443".into(), "10.0.0.1".into(), "/authn?x=1".into()));
        assert_eq!(url_parts("https://[fd00::1]:9443/a").unwrap(), ("[fd00::1]:9443".into(), "fd00::1".into(), "/a".into()));
        assert_eq!(url_parts("https://h?q").unwrap().2, "/?q");
        assert!(url_parts("http://h").is_err());
    }

    fn oidc(config: OidcConfig) -> Oidc {
        Oidc::new(config).unwrap()
    }

    fn claims(extra: Value) -> Value {
        let mut c = json!({"iss": "https://idp.example.com", "aud": "litek8s", "sub": "1234", "exp": 2000, "iat": 1000, "email": "a@example.com"});
        c.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        c
    }

    #[test]
    fn oidc_claims_like_kube_apiserver() {
        let mut cfg = OidcConfig::new("https://idp.example.com", "litek8s");
        let o = oidc(cfg.clone());
        let u = o.user_of(&claims(json!({})), 1500).unwrap();
        assert_eq!(u.username, "https://idp.example.com#1234");
        assert!(u.groups.contains(&"system:authenticated".to_string()));
        assert!(o.user_of(&claims(json!({})), 2100).unwrap_err().contains("expired"));
        assert!(o.user_of(&claims(json!({"aud": ["other"]})), 1500).is_err());
        assert!(o.user_of(&claims(json!({"aud": ["other", "litek8s"]})), 1500).is_ok());
        assert!(o.user_of(&claims(json!({"iss": "https://evil.example.com"})), 1500).is_err());
        assert!(o.user_of(&claims(json!({"nbf": 1800})), 1500).unwrap_err().contains("not valid yet"));

        cfg.username_claim = "email".into();
        cfg.groups_claim = Some("groups".into());
        cfg.groups_prefix = "oidc:".into();
        cfg.required_claims = vec![("hd".into(), "example.com".into())];
        let o = oidc(cfg.clone());
        let c = claims(json!({"groups": ["dev", "ops"], "hd": "example.com"}));
        let u = o.user_of(&c, 1500).unwrap();
        assert_eq!(u.username, "a@example.com");
        assert!(u.groups.starts_with(&["oidc:dev".to_string(), "oidc:ops".to_string()]));
        assert!(o.user_of(&claims(json!({"hd": "example.com", "email_verified": false})), 1500).unwrap_err().contains("not verified"));
        assert!(o.user_of(&claims(json!({"hd": "other.com"})), 1500).is_err());
        assert!(o.user_of(&claims(json!({"hd": "example.com", "groups": 3})), 1500).is_err());

        cfg.username_claim = "sub".into();
        cfg.username_prefix = Some("-".into());
        cfg.required_claims.clear();
        let o = oidc(cfg);
        assert_eq!(o.user_of(&claims(json!({})), 1500).unwrap().username, "1234");
        assert!(o.user_of(&claims(json!({"sub": "system:admin"})), 1500).unwrap_err().contains("reserved"));
    }

    #[test]
    fn oidc_config_is_checked() {
        assert!(Oidc::new(OidcConfig::new("http://idp", "c")).is_err());
        assert!(Oidc::new(OidcConfig::new("https://idp", "")).is_err());
        let mut c = OidcConfig::new("https://idp", "c");
        c.signing_algs = vec!["HS256".into()];
        assert!(Oidc::new(c).is_err());
    }

    #[test]
    fn jws_signatures() {
        use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair};
        let rng = ring::rand::SystemRandom::new();
        let msg = b"header.payload";

        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let point = pair.public_key().as_ref();
        let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let jwk = json!({"kty": "EC", "crv": "P-256", "x": enc(&point[1..33]), "y": enc(&point[33..])});
        let sig = pair.sign(&rng, msg).unwrap();
        assert!(verify_jws("ES256", &jwk, msg, sig.as_ref()));
        assert!(!verify_jws("ES256", &jwk, b"other", sig.as_ref()));
        assert!(!verify_jws("RS256", &jwk, msg, sig.as_ref()));
        assert!(!verify_jws("ES384", &jwk, msg, sig.as_ref()));

        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let jwk = json!({"kty": "OKP", "crv": "Ed25519", "x": enc(pair.public_key().as_ref()), "alg": "EdDSA"});
        let sig = pair.sign(msg);
        assert!(verify_jws("EdDSA", &jwk, msg, sig.as_ref()));
        assert!(!verify_jws("EdDSA", &jwk, b"x", sig.as_ref()));
    }

    #[test]
    fn webhook_answers() {
        let ok = json!({"status": {"authenticated": true, "user": {"username": "jane", "uid": "42", "groups": ["dev"]}}});
        let u = review_answer(&ok).unwrap();
        assert_eq!((u.username.as_str(), u.uid.as_str()), ("jane", "42"));
        assert!(u.groups.contains(&"dev".to_string()) && u.groups.contains(&"system:authenticated".to_string()));
        assert!(review_answer(&json!({"status": {"authenticated": false, "error": "nope"}})).unwrap_err().contains("nope"));
        assert!(review_answer(&json!({"status": {"authenticated": true, "user": {}}})).is_err());
    }

    #[test]
    fn webhook_kubeconfig() {
        let dir = std::env::temp_dir().join(format!("litek8s-authn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("token"), "s3cret\n").unwrap();
        let k: Value = serde_yaml::from_str(
            "clusters:\n- name: a\n  cluster: {server: 'https://a.example/x'}\n- name: b\n  cluster: {server: 'https://b.example/authn', insecure-skip-tls-verify: true}\n\
             users:\n- name: u\n  user: {tokenFile: token}\ncontexts:\n- name: c\n  context: {cluster: b, user: u}\ncurrent-context: c\n",
        )
        .unwrap();
        let w = webhook_from_kubeconfig(&k, &dir).unwrap();
        assert_eq!(w.server, "https://b.example/authn");
        assert!(w.tls.insecure);
        assert_eq!(w.bearer.as_deref(), Some("s3cret"));
        let k: Value = serde_yaml::from_str("clusters:\n- cluster: {server: 'http://plain'}\n").unwrap();
        assert!(webhook_from_kubeconfig(&k, &dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An HTTPS server on 127.0.0.1 answering `answer(method, path, body)`;
    /// its address and CA (PEM).
    async fn https_server(answer: impl Fn(&str, &str, &[u8]) -> (u16, Value) + Send + Sync + 'static) -> (String, Vec<u8>) {
        use hyper::service::service_fn;
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let ca = cert.cert.pem().into_bytes();
        let key = PrivateKeyDer::from_pem_slice(cert.key_pair.serialize_pem().as_bytes()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let answer = Arc::new(answer);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, answer) = (acceptor.clone(), answer.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                        let answer = answer.clone();
                        async move {
                            let (method, path) = (req.method().to_string(), req.uri().path().to_string());
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let (code, v) = answer(&method, &path, &body);
                            Ok::<_, std::convert::Infallible>(
                                hyper::Response::builder().status(code).body(Full::new(Bytes::from(serde_json::to_vec(&v).unwrap()))).unwrap(),
                            )
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tls), service).await;
                });
            }
        });
        (addr, ca)
    }

    #[tokio::test]
    async fn the_whole_chain_against_an_issuer_and_a_webhook() {
        use super::super::authn::{Conn, authenticate};
        use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let point = pair.public_key().as_ref().to_vec();
        let jwk = json!({"kty": "EC", "crv": "P-256", "kid": "k1", "use": "sig", "x": enc(&point[1..33]), "y": enc(&point[33..])});
        let issuer = Arc::new(Mutex::new(String::new()));
        let reviews = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (iss, rev) = (issuer.clone(), reviews.clone());
        let (addr, ca) = https_server(move |method, path, body| match (method, path) {
            ("GET", "/.well-known/openid-configuration") => {
                let iss = iss.lock().unwrap().clone();
                (200, json!({"issuer": iss, "jwks_uri": format!("{iss}/keys")}))
            }
            ("GET", "/keys") => (200, json!({"keys": [jwk.clone()]})),
            ("POST", "/authn") => {
                rev.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let review: Value = serde_json::from_slice(body).unwrap();
                match review.pointer("/spec/token").and_then(Value::as_str) {
                    Some("from-webhook") => (200, json!({"apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview", "status": {"authenticated": true, "user": {"username": "jane", "groups": ["dev"]}}})),
                    _ => (200, json!({"status": {"authenticated": false}})),
                }
            }
            _ => (404, json!({})),
        })
        .await;
        let issuer_url = format!("https://{addr}");
        *issuer.lock().unwrap() = issuer_url.clone();

        let dir = std::env::temp_dir().join(format!("litek8s-authn-chain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ca.pem"), &ca).unwrap();
        std::fs::write(dir.join("tokens.csv"), "static-1,ops,9,\"ops,dev\"\n").unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&ca);
        std::fs::write(dir.join("webhook.yaml"), format!("clusters:\n- name: w\n  cluster:\n    server: https://{addr}/authn\n    certificate-authority-data: {b64}\nusers:\n- name: w\n  user: {{token: hook-secret}}\n")).unwrap();
        let mut oidc = OidcConfig::new(&issuer_url, "litek8s");
        oidc.signing_algs = vec!["ES256".into()];
        oidc.ca_file = Some(dir.join("ca.pem"));
        oidc.groups_claim = Some("groups".into());
        let config = TokenAuthConfig {
            token_file: Some(dir.join("tokens.csv")),
            oidc: Some(oidc),
            webhook: Some(WebhookConfig { config_file: dir.join("webhook.yaml"), cache_ttl: Duration::from_secs(60) }),
        };
        let mut state = super::super::server::ApiState::open(None, "127.0.0.1:0".into(), false).unwrap();
        state.authz.tokens = TokenAuth::load(&config).unwrap();
        let who = |token: String| {
            let state = &state;
            async move {
                let mut h = hyper::HeaderMap::new();
                h.insert("authorization", format!("Bearer {token}").parse().unwrap());
                authenticate(state, &h, &Conn::default()).await
            }
        };
        let jwt = |claims: Value, kid: &str| {
            let head = enc(&serde_json::to_vec(&json!({"alg": "ES256", "kid": kid})).unwrap());
            let body = enc(&serde_json::to_vec(&claims).unwrap());
            let sig = pair.sign(&rng, format!("{head}.{body}").as_bytes()).unwrap();
            format!("{head}.{body}.{}", enc(sig.as_ref()))
        };
        let exp = now() + 600;

        let u = who("static-1".into()).await.unwrap().unwrap();
        assert_eq!((u.username.as_str(), u.uid.as_str()), ("ops", "9"));

        let token = jwt(json!({"iss": issuer_url, "aud": "litek8s", "sub": "u1", "exp": exp, "groups": ["devs"]}), "k1");
        let u = who(token).await.unwrap().unwrap();
        assert_eq!(u.username, format!("{issuer_url}#u1"));
        assert!(u.groups.contains(&"devs".to_string()));
        // Claims of one token with the signature of another; for another
        // client; a key the issuer does not have.
        let forged = {
            let root = jwt(json!({"iss": issuer_url, "aud": "litek8s", "sub": "root", "exp": exp}), "k1");
            let other = jwt(json!({"iss": issuer_url, "aud": "litek8s", "sub": "u1", "exp": exp}), "k1");
            format!("{}.{}", &root[..root.rfind('.').unwrap()], other.rsplit('.').next().unwrap())
        };
        assert!(who(forged).await.unwrap_err().contains("signature"));
        assert!(who(jwt(json!({"iss": issuer_url, "aud": "other", "sub": "u1", "exp": exp}), "k1")).await.is_err());
        assert!(who(jwt(json!({"iss": issuer_url, "aud": "litek8s", "sub": "u1", "exp": exp}), "unknown")).await.is_err());

        let u = who("from-webhook".into()).await.unwrap().unwrap();
        assert_eq!(u.username, "jane");
        assert!(u.groups.contains(&"dev".to_string()));
        // Cached: the webhook is not asked again.
        let asked = reviews.load(std::sync::atomic::Ordering::SeqCst);
        who("from-webhook".into()).await.unwrap();
        assert_eq!(reviews.load(std::sync::atomic::Ordering::SeqCst), asked);
        assert!(who("nobody".into()).await.is_err());
        // A JWT of no issuer we know (not this cluster's either) goes to the webhook too.
        let stranger = jwt(json!({"iss": "https://elsewhere", "aud": "litek8s", "sub": "x", "exp": exp}), "k1");
        let before = reviews.load(std::sync::atomic::Ordering::SeqCst);
        assert!(who(stranger).await.is_err());
        assert_eq!(reviews.load(std::sync::atomic::Ordering::SeqCst), before + 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
