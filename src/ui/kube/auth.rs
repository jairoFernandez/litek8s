//! Kubeconfigs as kubectl reads them: every file of `$KUBECONFIG` merged
//! (the first to name a cluster, context or user wins; relative paths are
//! relative to the file that has them), and credentials from exec plugins
//! (`gke-gcloud-auth-plugin`, `aws eks get-token`, `kubelogin`, …), run
//! when a token is needed and cached until it expires.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use serde_yaml::Value as Yaml;

use super::super::client::Credentials;

/// Where an entry came from (its directory), put into merged entries.
const DIR_KEY: &str = "x-litek8s-dir";

/// The kubeconfig files kubectl would read.
pub fn paths() -> Vec<PathBuf> {
    if let Some(v) = std::env::var_os("KUBECONFIG") {
        let list: Vec<PathBuf> = std::env::split_paths(&v).filter(|p| !p.as_os_str().is_empty()).collect();
        if !list.is_empty() {
            return list;
        }
    }
    vec![crate::dev::kubeconfig::default_path()]
}

/// The merged kubeconfig.
#[derive(Debug, Clone, Default)]
pub struct Kubeconfig {
    pub doc: Yaml,
}

pub fn load() -> Result<Kubeconfig> {
    let mut merged = serde_yaml::Mapping::new();
    let mut found = false;
    let mut errors = Vec::new();
    for p in paths() {
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                errors.push(format!("{}: {e}", p.display()));
                continue;
            }
        };
        let doc: Yaml = match serde_yaml::from_str(&text) {
            Ok(d) => d,
            Err(e) => {
                errors.push(format!("{} is not valid YAML: {e}", p.display()));
                continue;
            }
        };
        found = true;
        merge(&mut merged, &doc, p.parent().unwrap_or(Path::new(".")));
    }
    if !found {
        match errors.first() {
            Some(e) => bail!("{e}"),
            None => bail!("no kubeconfig at {}", paths().iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":")),
        }
    }
    Ok(Kubeconfig { doc: Yaml::Mapping(merged) })
}

fn merge(into: &mut serde_yaml::Mapping, doc: &Yaml, dir: &Path) {
    let cur = Yaml::from("current-context");
    if into.get(&cur).and_then(Yaml::as_str).unwrap_or("").is_empty() {
        if let Some(c) = doc["current-context"].as_str().filter(|c| !c.is_empty()) {
            into.insert(cur, Yaml::from(c));
        }
    }
    for list in ["clusters", "contexts", "users"] {
        let key = Yaml::from(list);
        let entry = into.entry(key).or_insert_with(|| Yaml::Sequence(Vec::new()));
        let Yaml::Sequence(seq) = entry else { continue };
        for e in doc[list].as_sequence().into_iter().flatten() {
            let name = e["name"].as_str().unwrap_or_default();
            if seq.iter().any(|x| x["name"].as_str() == Some(name)) {
                continue;
            }
            let mut e = e.clone();
            if let Yaml::Mapping(m) = &mut e {
                m.insert(Yaml::from(DIR_KEY), Yaml::from(dir.display().to_string()));
            }
            seq.push(e);
        }
    }
}

impl Kubeconfig {
    pub fn named(&self, list: &str, name: &str) -> Option<&Yaml> {
        self.doc[list].as_sequence()?.iter().find(|e| e["name"].as_str() == Some(name))
    }

    pub fn contexts(&self) -> Vec<String> {
        let mut v: Vec<String> = self.doc["contexts"].as_sequence().into_iter().flatten().filter_map(|c| c["name"].as_str().map(str::to_string)).collect();
        v.sort();
        v
    }

    pub fn current(&self) -> Option<String> {
        self.doc["current-context"].as_str().filter(|c| !c.is_empty()).map(str::to_string)
    }

    /// The namespace a context starts in.
    pub fn namespace(&self, context: &str) -> Option<String> {
        self.named("contexts", context)?["context"]["namespace"].as_str().filter(|n| !n.is_empty()).map(str::to_string)
    }

}

fn dir_of(entry: &Yaml, default: &Path) -> PathBuf {
    entry[DIR_KEY].as_str().map(PathBuf::from).unwrap_or_else(|| default.to_path_buf())
}

/// A user's credentials when they come from a plugin or an auth provider;
/// `None` when the kubeconfig has them inline (token, certificates).
pub fn dynamic(user: &Yaml, dir: &Path) -> Result<Option<std::sync::Arc<dyn Credentials>>> {
    let dir = dir_of(user, dir);
    let u = &user["user"];
    if let Some(exec) = u.get("exec").filter(|e| !e.is_null()) {
        let command = exec["command"].as_str().context("exec credentials without a command")?.to_string();
        let args = exec["args"].as_sequence().into_iter().flatten().filter_map(Yaml::as_str).map(str::to_string).collect();
        let env = exec["env"].as_sequence().into_iter().flatten().filter_map(|e| Some((e["name"].as_str()?.to_string(), e["value"].as_str().unwrap_or_default().to_string()))).collect();
        return Ok(Some(std::sync::Arc::new(ExecAuth {
            command,
            args,
            env,
            api_version: exec["apiVersion"].as_str().unwrap_or("client.authentication.k8s.io/v1beta1").to_string(),
            hint: exec["installHint"].as_str().map(str::to_string),
            dir,
            cache: Mutex::new(None),
        })));
    }
    if let Some(ap) = u.get("auth-provider").filter(|a| !a.is_null()) {
        let cfg = &ap["config"];
        // OIDC's id-token, the old GCP provider's access-token.
        if let Some(t) = cfg["id-token"].as_str().or_else(|| cfg["access-token"].as_str()) {
            return Ok(Some(std::sync::Arc::new(Static(format!("Bearer {t}")))));
        }
        bail!("auth-provider {} without a token is not supported (use an exec plugin)", ap["name"].as_str().unwrap_or("?"));
    }
    Ok(None)
}

struct Static(String);

impl Credentials for Static {
    fn header(&self) -> Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
    fn invalidate(&self) {}
}

/// An exec credential plugin.
pub struct ExecAuth {
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    api_version: String,
    hint: Option<String>,
    dir: PathBuf,
    /// The token and when it expires (Unix seconds).
    cache: Mutex<Option<(String, Option<i64>)>>,
}

/// The token in an ExecCredential's status, and its expiry.
pub fn parse_exec_credential(out: &[u8]) -> Result<(String, Option<i64>)> {
    let v: Value = serde_json::from_slice(out).context("the credential plugin did not print an ExecCredential")?;
    let status = &v["status"];
    let token = status["token"].as_str().filter(|t| !t.is_empty());
    let Some(token) = token else {
        if status["clientCertificateData"].is_string() {
            bail!("the credential plugin returns client certificates, which litek8s ui does not support yet");
        }
        bail!("the credential plugin returned no token");
    };
    let exp = status["expirationTimestamp"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp());
    Ok((token.to_string(), exp))
}

impl ExecAuth {
    async fn fetch(&self) -> Result<(String, Option<i64>)> {
        let cmd = if self.command.contains('/') && !Path::new(&self.command).is_absolute() { self.dir.join(&self.command).display().to_string() } else { self.command.clone() };
        let info = json!({"apiVersion": self.api_version, "kind": "ExecCredential", "spec": {"interactive": false}});
        let child = tokio::process::Command::new(&cmd)
            .args(&self.args)
            .envs(self.env.iter().cloned())
            .env("KUBERNETES_EXEC_INFO", info.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(std::time::Duration::from_secs(60), child).await {
            Err(_) => bail!("credential plugin {cmd} took over 60 s"),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("credential plugin {cmd} not found{}", self.hint.as_ref().map(|h| format!(": {}", h.trim())).unwrap_or_default())
            }
            Ok(r) => r.with_context(|| format!("cannot run credential plugin {cmd}"))?,
        };
        if !out.status.success() {
            bail!("credential plugin {cmd} failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
        }
        parse_exec_credential(&out.stdout)
    }
}

impl Credentials for ExecAuth {
    fn header(&self) -> Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async move {
            let now = chrono::Utc::now().timestamp();
            if let Some((t, exp)) = self.cache.lock().unwrap().clone() {
                if exp.is_none_or(|e| e > now + 30) {
                    return Ok(format!("Bearer {t}"));
                }
            }
            let (t, exp) = self.fetch().await?;
            *self.cache.lock().unwrap() = Some((t.clone(), exp));
            Ok(format!("Bearer {t}"))
        })
    }

    fn invalidate(&self) {
        *self.cache.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_files_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let a: Yaml = serde_yaml::from_str("current-context: a\ncontexts:\n- {name: a, context: {cluster: c1, user: u1, namespace: web}}\nclusters:\n- {name: c1, cluster: {server: 'https://a:6443'}}\nusers:\n- {name: u1, user: {token: t}}\n").unwrap();
        let b: Yaml = serde_yaml::from_str("current-context: b\ncontexts:\n- {name: a, context: {cluster: other, user: u1}}\n- {name: b, context: {cluster: c2, user: u2}}\nclusters:\n- {name: c2, cluster: {server: 'https://b:6443'}}\nusers:\n- {name: u2, user: {exec: {command: ./plug, apiVersion: client.authentication.k8s.io/v1}}}\n").unwrap();
        let mut m = serde_yaml::Mapping::new();
        merge(&mut m, &a, dir.path());
        merge(&mut m, &b, Path::new("/elsewhere"));
        let k = Kubeconfig { doc: Yaml::Mapping(m) };
        assert_eq!(k.current().as_deref(), Some("a"));
        assert_eq!(k.contexts(), vec!["a", "b"]);
        assert_eq!(k.namespace("a").as_deref(), Some("web"));
        let u2 = k.named("users", "u2").unwrap();
        assert_eq!(dir_of(u2, Path::new(".")), PathBuf::from("/elsewhere"));
        assert!(dynamic(u2, Path::new(".")).unwrap().is_some());
        assert!(dynamic(k.named("users", "u1").unwrap(), Path::new(".")).unwrap().is_none());
    }

    #[test]
    fn exec_credentials() {
        let (t, exp) = parse_exec_credential(br#"{"kind":"ExecCredential","status":{"token":"abc","expirationTimestamp":"2026-10-19T10:00:00Z"}}"#).unwrap();
        assert_eq!(t, "abc");
        assert_eq!(exp, Some(1_792_404_000));
        assert!(parse_exec_credential(br#"{"status":{}}"#).is_err());
        assert!(parse_exec_credential(b"oops").is_err());
    }

    #[tokio::test]
    async fn runs_the_plugin_once_until_it_expires() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let script = dir.path().join("plug.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho x >> {}\necho '{{\"apiVersion\":\"client.authentication.k8s.io/v1\",\"kind\":\"ExecCredential\",\"status\":{{\"token\":\"tok-'$MYVAR'\"}}}}'\n", count.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let user: Yaml = serde_yaml::from_str(&format!("name: u\nuser:\n  exec:\n    command: {}\n    env: [{{name: MYVAR, value: v}}]\n", script.display())).unwrap();
        let c = dynamic(&user, dir.path()).unwrap().unwrap();
        assert_eq!(c.header().await.unwrap(), "Bearer tok-v");
        assert_eq!(c.header().await.unwrap(), "Bearer tok-v");
        assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1);
        c.invalidate();
        c.header().await.unwrap();
        assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 2);
        let missing: Yaml = serde_yaml::from_str("name: u\nuser:\n  exec: {command: no-such-plugin-xyz, installHint: 'install it'}\n").unwrap();
        let e = dynamic(&missing, dir.path()).unwrap().unwrap().header().await.unwrap_err().to_string();
        assert!(e.contains("not found: install it"), "{e}");
    }
}
