//! Registry configuration for image pulls: mirrors, credentials and TLS per
//! registry host, in the format of k3s's `/etc/rancher/k3s/registries.yaml`
//! (a k3s file works as is):
//!
//! ```yaml
//! mirrors:
//!   docker.io:                      # pulls of docker.io/... try these first
//!     endpoint: ["https://cache.lan:5000"]
//!   registry.lan:5000:              # plain HTTP: an http:// endpoint
//!     endpoint: ["http://registry.lan:5000"]
//!   "*":                            # every registry without its own entry
//!     endpoint: ["https://mirror.lan"]
//!     rewrite: {"^library/(.*)": "dockerhub/$1"}
//! configs:
//!   gitea.lan:                      # host[:port] of a registry or endpoint
//!     auth: {username: ci, password: secret}  # or auth: base64(user:pass), or identitytoken
//!     tls:
//!       ca_file: /etc/litek8s/lan-ca.pem      # or ca: (PEM inline, litek8s)
//!       cert_file: ..., key_file: ...         # client certificate (or cert:, key:)
//!       insecure_skip_verify: false
//! ```
//!
//! A pull tries each mirror endpoint in order, then the registry itself.
//! Extra CAs are trusted next to the public ones. `litek8s apiserver` and
//! `litek8s agent` read the file once at start (`--registry-config`, else
//! [`DEFAULT_PATH`] when it exists); pod pulls and the Docker API's pulls
//! use it. `LITEK8S_INSECURE_REGISTRIES=host:port,...` still makes those
//! registries plain HTTP.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

/// Read by installed servers and agents when it exists.
pub const DEFAULT_PATH: &str = "/etc/litek8s/registries.yaml";

/// The file as written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistriesFile {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mirrors: BTreeMap<String, Mirror>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub configs: BTreeMap<String, HostConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mirror {
    /// `https://host[:port][/path]` (no scheme: https); tried in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoint: Vec<String>,
    /// Regex on the repository -> replacement (`$1`), for the mirrors only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rewrite: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<Tls>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// base64 of `username:password`, as in ~/.docker/config.json.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// An OAuth2 refresh token for the registry's token service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identitytoken: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
    /// PEM, instead of `ca_file` (litek8s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure_skip_verify: bool,
}

/// What a registry gets to authenticate a pull.
#[derive(Clone, PartialEq)]
pub enum Credentials {
    Basic { username: String, password: String },
    /// Exchanged for an access token at the registry's token service.
    IdentityToken(String),
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::Basic { username, .. } => write!(f, "Basic({username}, ***)"),
            Credentials::IdentityToken(_) => write!(f, "IdentityToken(***)"),
        }
    }
}

impl Auth {
    fn credentials(&self) -> Result<Option<Credentials>> {
        if let Some(t) = self.identitytoken.as_ref().filter(|t| !t.is_empty()) {
            return Ok(Some(Credentials::IdentityToken(t.clone())));
        }
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => return Ok(Some(Credentials::Basic { username: u.clone(), password: p.clone() })),
            (Some(_), None) | (None, Some(_)) if self.auth.is_none() => bail!("username and password go together"),
            _ => {}
        }
        if let Some(a) = &self.auth {
            use base64::Engine;
            let raw = base64::engine::general_purpose::STANDARD.decode(a.trim()).context("auth is not base64")?;
            let text = String::from_utf8(raw).context("auth is not UTF-8")?;
            let (u, p) = text.split_once(':').context("auth is not base64 of username:password")?;
            return Ok(Some(Credentials::Basic { username: u.to_string(), password: p.to_string() }));
        }
        Ok(None)
    }
}

/// `(https, host[:port], API root path)` of a mirror endpoint.
fn parse_endpoint(e: &str) -> Result<(bool, String, String)> {
    let e = e.trim();
    let (https, rest) = match e.split_once("://") {
        Some(("https", r)) => (true, r),
        Some(("http", r)) => (false, r),
        Some((s, _)) => bail!("scheme {s} is not http or https"),
        None => (true, e),
    };
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
        None => (rest, ""),
    };
    ensure!(!host.is_empty() && !host.contains(['?', '#', '@']), "no host");
    let root = if path.ends_with("/v2") { path.to_string() } else { format!("{path}/v2") };
    Ok((https, host.to_string(), root))
}

impl RegistriesFile {
    pub fn parse(text: &str) -> Result<Self> {
        let file: Self = if text.trim().is_empty() { Self::default() } else { serde_yaml::from_str(text)? };
        file.validate()?;
        Ok(file)
    }

    fn validate(&self) -> Result<()> {
        for (name, m) in &self.mirrors {
            for e in &m.endpoint {
                parse_endpoint(e).with_context(|| format!("mirrors.{name}: endpoint {e:?}"))?;
            }
            for re in m.rewrite.keys() {
                regex::Regex::new(re).with_context(|| format!("mirrors.{name}: rewrite {re:?}"))?;
            }
        }
        for (name, c) in &self.configs {
            if let Some(a) = &c.auth {
                a.credentials().with_context(|| format!("configs.{name}.auth"))?;
            }
            if let Some(t) = &c.tls {
                ensure!(t.ca.is_none() || t.ca_file.is_none(), "configs.{name}.tls: ca and ca_file both set");
                ensure!(t.cert.is_none() || t.cert_file.is_none(), "configs.{name}.tls: cert and cert_file both set");
                ensure!(t.key.is_none() || t.key_file.is_none(), "configs.{name}.tls: key and key_file both set");
                let cert = t.cert.is_some() || t.cert_file.is_some();
                let key = t.key.is_some() || t.key_file.is_some();
                ensure!(cert == key, "configs.{name}.tls: a client certificate needs its key (and a key its certificate)");
            }
        }
        Ok(())
    }

    /// Read the files `tls` names (relative ones against `base`) into the
    /// file itself, so it works on another machine (`dev up`, `install`).
    pub fn inline_files(&mut self, base: &Path) -> Result<()> {
        for (name, c) in &mut self.configs {
            let Some(t) = &mut c.tls else { continue };
            for (file, inline) in [(&mut t.ca_file, &mut t.ca), (&mut t.cert_file, &mut t.cert), (&mut t.key_file, &mut t.key)] {
                if let Some(path) = file.take() {
                    let path = base.join(path);
                    *inline = Some(std::fs::read_to_string(&path).with_context(|| format!("configs.{name}.tls: cannot read {}", path.display()))?);
                }
            }
        }
        Ok(())
    }

    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(self).unwrap_or_default()
    }
}

/// The file at `path` made to work on another machine: checked, and the
/// files it names (relative to its directory) read into it. What `dev up`
/// and `install --registries` ship to nodes.
pub fn bundle(path: &Path) -> Result<String> {
    let invalid = || format!("{} is not a valid registry configuration", path.display());
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut file = RegistriesFile::parse(&text).with_context(invalid)?;
    file.inline_files(path.parent().unwrap_or(Path::new("/"))).with_context(invalid)?;
    Registries::new(file.clone(), Path::new("/")).with_context(invalid)?;
    Ok(file.to_yaml())
}

/// Where a pull goes: a mirror or the registry itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    /// The API root, `https://host[:port]/v2`.
    pub root: String,
    /// `host[:port]`, as `configs` names it.
    pub host: String,
    /// The repository there (a mirror's rewrite applied).
    pub repository: String,
    /// The registry the image names (`docker.io`), not a mirror.
    pub upstream: bool,
    pub registry: String,
}

impl Endpoint {
    pub fn url(&self, kind: &str, reference: &str) -> String {
        format!("{}/{}/{kind}/{reference}", self.root, self.repository)
    }

    /// The registry itself: `docker.io` is served by registry-1.docker.io;
    /// localhost and `LITEK8S_INSECURE_REGISTRIES` speak plain HTTP.
    pub fn upstream(registry: &str, repository: &str) -> Self {
        let host = if registry == "docker.io" { "registry-1.docker.io" } else { registry };
        let name = registry.split(':').next().unwrap_or_default();
        let insecure = std::env::var("LITEK8S_INSECURE_REGISTRIES").unwrap_or_default();
        let http = name == "localhost" || name == "127.0.0.1" || insecure.split(',').any(|r| r.trim() == registry);
        Endpoint {
            root: format!("{}://{host}/v2", if http { "http" } else { "https" }),
            host: host.to_string(),
            repository: repository.to_string(),
            upstream: true,
            registry: registry.to_string(),
        }
    }
}

/// The file, ready to use: TLS settings built, CA files read.
#[derive(Debug, Default)]
pub struct Registries {
    file: RegistriesFile,
    rewrites: BTreeMap<String, Vec<(regex::Regex, String)>>,
    tls: BTreeMap<String, Arc<rustls::ClientConfig>>,
    creds: BTreeMap<String, Credentials>,
}

impl Registries {
    /// `base`: what relative file names in `tls` are relative to.
    pub fn new(file: RegistriesFile, base: &Path) -> Result<Self> {
        file.validate()?;
        let mut r = Registries::default();
        for (name, m) in &file.mirrors {
            let rules = m.rewrite.iter().map(|(re, to)| Ok((regex::Regex::new(re)?, to.clone()))).collect::<Result<Vec<_>>>()?;
            r.rewrites.insert(name.clone(), rules);
        }
        for (name, c) in &file.configs {
            if let Some(creds) = c.auth.as_ref().map(Auth::credentials).transpose()?.flatten() {
                r.creds.insert(name.clone(), creds);
            }
            if let Some(t) = &c.tls {
                r.tls.insert(name.clone(), client_config(t, base).with_context(|| format!("configs.{name}.tls"))?);
            }
        }
        r.file = file;
        Ok(r)
    }

    pub fn parse(text: &str, base: &Path) -> Result<Self> {
        Self::new(RegistriesFile::parse(text)?, base)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&text, path.parent().unwrap_or(Path::new("/"))).with_context(|| format!("{} is not a valid registry configuration", path.display()))
    }

    /// `2 mirrors, 3 configs`, for the startup log.
    pub fn summary(&self) -> String {
        format!("{} mirror(s), {} config(s)", self.file.mirrors.len(), self.file.configs.len())
    }

    /// Where to pull `registry/repository` from, in order: the registry's
    /// mirrors (or `*`'s), then the registry itself.
    pub fn endpoints(&self, registry: &str, repository: &str) -> Vec<Endpoint> {
        let mut upstream = Endpoint::upstream(registry, repository);
        let key = if self.file.mirrors.contains_key(registry) { registry } else { "*" };
        let mut out = Vec::new();
        if let Some(m) = self.file.mirrors.get(key) {
            let rewritten = self.rewrites.get(key).into_iter().flatten().find(|(re, _)| re.is_match(repository)).map(|(re, to)| re.replace(repository, to.as_str()).into_owned());
            for e in &m.endpoint {
                let Ok((https, host, root)) = parse_endpoint(e) else { continue };
                if host == upstream.host && root == "/v2" {
                    // The registry itself, with the scheme to use (k3s's
                    // way to make a registry plain HTTP).
                    upstream.root = format!("{}://{host}/v2", if https { "https" } else { "http" });
                    continue;
                }
                let root = format!("{}://{host}{root}", if https { "https" } else { "http" });
                let repository = rewritten.clone().unwrap_or_else(|| repository.to_string());
                out.push(Endpoint { root, host, repository, upstream: false, registry: registry.to_string() });
            }
        }
        out.push(upstream);
        out
    }

    /// The credentials `configs` give an endpoint: its host, for the
    /// registry itself also the registry's name (`docker.io`), then `*`.
    pub fn credentials(&self, ep: &Endpoint) -> Option<Credentials> {
        let mut keys = vec![ep.host.as_str()];
        if ep.upstream {
            keys.push(ep.registry.as_str());
        }
        keys.push("*");
        keys.into_iter().find_map(|k| self.creds.get(k)).cloned()
    }

    /// The TLS settings for a connection to `host:port`.
    pub fn tls(&self, host: &str, port: u16) -> Arc<rustls::ClientConfig> {
        let with_port = format!("{host}:{port}");
        let mut keys = vec![with_port.as_str()];
        if port == 443 {
            keys.push(host);
        }
        if host == "registry-1.docker.io" {
            keys.push("docker.io");
        }
        keys.push("*");
        keys.into_iter().find_map(|k| self.tls.get(k)).cloned().unwrap_or_else(default_tls)
    }
}

/// The public CAs only.
fn default_tls() -> Arc<rustls::ClientConfig> {
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    CONFIG.get_or_init(|| client_config(&Tls::default(), Path::new("/")).expect("default TLS settings")).clone()
}

fn client_config(t: &Tls, base: &Path) -> Result<Arc<rustls::ClientConfig>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let read = |inline: &Option<String>, file: &Option<PathBuf>| -> Result<Option<Vec<u8>>> {
        Ok(match (inline, file) {
            (Some(pem), _) => Some(pem.as_bytes().to_vec()),
            (None, Some(f)) => {
                let path = base.join(f);
                Some(std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?)
            }
            (None, None) => None,
        })
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions()?;
    let builder = if t.insecure_skip_verify {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(pem) = read(&t.ca, &t.ca_file)? {
            let mut n = 0;
            for cert in CertificateDer::pem_slice_iter(&pem) {
                roots.add(cert.context("bad CA certificate")?).context("bad CA certificate")?;
                n += 1;
            }
            ensure!(n > 0, "no PEM certificate in the CA");
        }
        builder.with_root_certificates(roots)
    };
    let mut config = match (read(&t.cert, &t.cert_file)?, read(&t.key, &t.key_file)?) {
        (Some(cert), Some(key)) => {
            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert).collect::<Result<_, _>>().context("bad client certificate")?;
            builder.with_client_auth_cert(certs, PrivateKeyDer::from_pem_slice(&key).context("bad client key")?)?
        }
        _ => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Accepts any server certificate (`insecure_skip_verify`; kubeconfigs'
/// `insecure-skip-tls-verify`).
#[derive(Debug)]
pub(crate) struct NoVerify(pub(crate) Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &rustls::pki_types::CertificateDer<'_>, d: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &rustls::pki_types::CertificateDer<'_>, d: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

static CURRENT: RwLock<Option<Arc<Registries>>> = RwLock::new(None);

/// The configuration this process pulls with.
pub fn current() -> Arc<Registries> {
    CURRENT.read().unwrap().clone().unwrap_or_default()
}

pub fn set(r: Registries) {
    *CURRENT.write().unwrap() = Some(Arc::new(r));
}

/// At the start of a process that pulls images: load `path` (it must be
/// valid), else [`DEFAULT_PATH`] when it exists.
pub fn init(path: Option<&Path>) -> Result<()> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None if Path::new(DEFAULT_PATH).exists() => PathBuf::from(DEFAULT_PATH),
        None => return Ok(()),
    };
    let r = Registries::load(&path)?;
    eprintln!("registries: {} from {}", r.summary(), path.display());
    set(r);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const K3S: &str = r#"
mirrors:
  docker.io:
    endpoint:
      - "https://cache.lan:5000"
      - "mirror2.lan"
  registry.lan:5000:
    endpoint: ["http://registry.lan:5000"]
  "*":
    endpoint: ["https://any.lan/prefix/"]
    rewrite:
      "^team/(.*)": "mirrored/team/$1"
configs:
  cache.lan:5000:
    auth: {username: u, password: p}
  docker.io:
    auth: {auth: "aHViOnNlY3JldA=="}
  "*":
    auth: {identitytoken: refresh}
"#;

    fn parsed(text: &str) -> Registries {
        Registries::parse(text, Path::new("/")).unwrap()
    }

    #[test]
    fn k3s_file_parses() {
        let f = RegistriesFile::parse(K3S).unwrap();
        assert_eq!(f.mirrors["docker.io"].endpoint.len(), 2);
        assert_eq!(f.configs["cache.lan:5000"].auth.as_ref().unwrap().username.as_deref(), Some("u"));
        assert_eq!(RegistriesFile::parse("").unwrap(), RegistriesFile::default());
        assert_eq!(RegistriesFile::parse(&f.to_yaml()).unwrap(), f);
    }

    #[test]
    fn bad_files_are_refused() {
        for (text, problem) in [
            ("mirror: {}", "unknown field"),
            ("configs: {a: {tls: {insecure_skip_verfy: true}}}", "unknown field"),
            ("mirrors: {a: {endpoint: [\"ftp://x\"]}}", "scheme ftp"),
            ("mirrors: {a: {rewrite: {\"(\": x}}}", "rewrite"),
            ("configs: {a: {auth: {username: u}}}", "go together"),
            ("configs: {a: {auth: {auth: \"!!\"}}}", "base64"),
            ("configs: {a: {tls: {cert: x}}}", "needs its key"),
            ("configs: {a: {tls: {ca: x, ca_file: /y}}}", "both set"),
        ] {
            let e = format!("{:#}", RegistriesFile::parse(text).unwrap_err());
            assert!(e.contains(problem), "{text}: {e}");
        }
        let e = format!("{:#}", Registries::parse("configs: {a: {tls: {ca: nonsense}}}", Path::new("/")).unwrap_err());
        assert!(e.contains("no PEM certificate"), "{e}");
        let e = format!("{:#}", Registries::parse("configs: {a: {tls: {ca_file: missing.pem}}}", Path::new("/nonexistent")).unwrap_err());
        assert!(e.contains("/nonexistent/missing.pem"), "{e}");
    }

    #[test]
    fn mirrors_come_before_the_registry() {
        let r = parsed(K3S);
        let roots = |reg: &str, repo: &str| r.endpoints(reg, repo).into_iter().map(|e| (e.root, e.repository)).collect::<Vec<_>>();
        assert_eq!(
            roots("docker.io", "library/nginx"),
            [
                ("https://cache.lan:5000/v2".into(), "library/nginx".into()),
                ("https://mirror2.lan/v2".into(), "library/nginx".into()),
                ("https://registry-1.docker.io/v2".into(), "library/nginx".into()),
            ]
        );
        // The registry itself as an http:// "mirror": plain HTTP, no fallback to https.
        assert_eq!(roots("registry.lan:5000", "app"), [("http://registry.lan:5000/v2".into(), "app".into())]);
        // The wildcard, with a rewrite for the mirror only.
        assert_eq!(
            roots("ghcr.io", "team/app"),
            [("https://any.lan/prefix/v2".into(), "mirrored/team/app".into()), ("https://ghcr.io/v2".into(), "team/app".into())]
        );
        assert_eq!(roots("ghcr.io", "other/app")[0].1, "other/app");
        // Without a file: the registry, localhost plain HTTP.
        let none = Registries::default();
        assert_eq!(none.endpoints("quay.io", "a/b").iter().map(|e| e.root.as_str()).collect::<Vec<_>>(), ["https://quay.io/v2"]);
        assert_eq!(none.endpoints("localhost:5000", "a")[0].root, "http://localhost:5000/v2");
        assert_eq!(none.endpoints("docker.io", "library/a")[0].url("manifests", "latest"), "https://registry-1.docker.io/v2/library/a/manifests/latest");
    }

    #[test]
    fn credentials_by_endpoint_host_then_registry_then_wildcard() {
        let r = parsed(K3S);
        let eps = r.endpoints("docker.io", "library/nginx");
        let basic = |u: &str, p: &str| Some(Credentials::Basic { username: u.into(), password: p.into() });
        assert_eq!(r.credentials(&eps[0]), basic("u", "p"));
        // mirror2.lan has no entry of its own: the wildcard, not docker.io's.
        assert_eq!(r.credentials(&eps[1]), Some(Credentials::IdentityToken("refresh".into())));
        assert_eq!(r.credentials(&eps[2]), basic("hub", "secret"));
        assert_eq!(Registries::default().credentials(&eps[2]), None);
    }

    #[test]
    fn tls_by_host_and_port() {
        let ca = rcgen::generate_simple_self_signed(vec!["reg.lan".into()]).unwrap().cert.pem();
        let text = format!("configs:\n  reg.lan:\n    tls:\n      ca: |\n{}\n  other.lan:8443:\n    tls: {{insecure_skip_verify: true}}\n", ca.lines().map(|l| format!("        {l}")).collect::<Vec<_>>().join("\n"));
        let r = parsed(&text);
        let default = default_tls();
        assert!(!Arc::ptr_eq(&r.tls("reg.lan", 443), &default));
        assert!(Arc::ptr_eq(&r.tls("reg.lan", 5000), &default), "a port of its own needs its own entry");
        assert!(!Arc::ptr_eq(&r.tls("other.lan", 8443), &default));
        assert!(Arc::ptr_eq(&r.tls("quay.io", 443), &default));
    }

    #[test]
    fn files_are_inlined_for_shipping() {
        let dir = tempfile::tempdir().unwrap();
        let pem = rcgen::generate_simple_self_signed(vec!["reg.lan".into()]).unwrap().cert.pem();
        std::fs::write(dir.path().join("ca.pem"), &pem).unwrap();
        std::fs::write(dir.path().join("registries.yaml"), "configs:\n  reg.lan:\n    tls: {ca_file: ca.pem}\n").unwrap();
        // Relative to the file's directory.
        Registries::load(&dir.path().join("registries.yaml")).unwrap();
        let mut f = RegistriesFile::parse("configs:\n  reg.lan:\n    tls: {ca_file: ca.pem}\n").unwrap();
        f.inline_files(dir.path()).unwrap();
        let tls = f.configs["reg.lan"].tls.as_ref().unwrap();
        assert_eq!((tls.ca_file.as_ref(), tls.ca.as_deref()), (None, Some(pem.as_str())));
        Registries::parse(&f.to_yaml(), Path::new("/nonexistent")).unwrap();
    }
}
