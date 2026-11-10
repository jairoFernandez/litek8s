//! A pod's `imagePullSecrets`: the registry credentials for one image, from
//! `kubernetes.io/dockerconfigjson` (`.dockerconfigjson`) and
//! `kubernetes.io/dockercfg` (`.dockercfg`) Secrets, as the kubelet reads
//! them. They win over the node's registry configuration for the image's
//! registry (not its mirrors, which keep their own).

use base64::Engine;
use serde_json::Value;

use super::crun::image::Reference;

/// Docker Hub under every name a config may give it.
const DOCKER_HUB: [&str; 4] = ["docker.io", "index.docker.io", "registry-1.docker.io", "registry.hub.docker.com"];

/// `host[:port]` of a config key: `https://index.docker.io/v1/`,
/// `ghcr.io`, `registry.example.com:5000/team` all work.
fn key_host(key: &str) -> &str {
    let k = key.trim().trim_start_matches("https://").trim_start_matches("http://");
    k.split('/').next().unwrap_or_default()
}

fn same_registry(key: &str, registry: &str) -> bool {
    let host = key_host(key).to_ascii_lowercase();
    let registry = registry.to_ascii_lowercase();
    if host == registry || (DOCKER_HUB.contains(&host.as_str()) && DOCKER_HUB.contains(&registry.as_str())) {
        return true;
    }
    // `*.example.com` matches one label, as the kubelet's keyring does.
    host.strip_prefix("*.")
        .is_some_and(|suffix| registry.strip_suffix(suffix).and_then(|r| r.strip_suffix('.')).is_some_and(|label| !label.is_empty() && !label.contains('.')))
}

/// (user, password) of one `auths` entry.
fn entry_credentials(entry: &Value) -> Option<(String, String)> {
    let user = entry.get("username").and_then(Value::as_str).unwrap_or_default();
    let pass = entry.get("password").and_then(Value::as_str).unwrap_or_default();
    if !user.is_empty() || !pass.is_empty() {
        return Some((user.to_string(), pass.to_string()));
    }
    let auth = entry.get("auth").and_then(Value::as_str)?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(auth.trim()).ok()?;
    let (user, pass) = std::str::from_utf8(&decoded).ok()?.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

/// The `auths` map of a pull Secret (`None`: not one).
fn auths(secret: &Value) -> Option<serde_json::Map<String, Value>> {
    let data = secret.get("data")?;
    let decode = |key: &str| -> Option<Value> {
        let raw = data.get(key).and_then(Value::as_str)?;
        serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(raw.trim()).ok()?).ok()
    };
    if let Some(config) = decode(".dockerconfigjson") {
        return config.get("auths").and_then(Value::as_object).cloned();
    }
    // The legacy format is the `auths` map itself.
    decode(".dockercfg").and_then(|c| c.as_object().cloned())
}

/// The credentials for `image` among `secrets`, in the pod's order: the
/// first Secret with an entry for the image's registry wins.
pub(crate) fn credentials_for(image: &str, secrets: &[Value]) -> Option<(String, String)> {
    let registry = Reference::parse(image).ok()?.registry;
    secrets.iter().filter_map(auths).find_map(|auths| auths.iter().find(|(k, _)| same_registry(k, &registry)).and_then(|(_, v)| entry_credentials(v)))
}

/// The names in a pod's `spec.imagePullSecrets`.
pub(crate) fn names(pod: &Value) -> Vec<String> {
    pod.pointer("/spec/imagePullSecrets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("name").and_then(Value::as_str))
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    fn config_json(auths: Value) -> Value {
        json!({"type": "kubernetes.io/dockerconfigjson", "data": {".dockerconfigjson": b64(&json!({"auths": auths}).to_string())}})
    }

    #[test]
    fn picks_the_entry_of_the_images_registry() {
        let secrets = vec![
            config_json(json!({"ghcr.io": {"auth": b64("gh-user:gh-token")}})),
            config_json(json!({"https://index.docker.io/v1/": {"username": "hub", "password": "pw"}, "registry.example.com:5000/team": {"auth": b64("me:s3:cret")}})),
        ];
        assert_eq!(credentials_for("ghcr.io/acme/app:1", &secrets), Some(("gh-user".into(), "gh-token".into())));
        assert_eq!(credentials_for("nginx", &secrets), Some(("hub".into(), "pw".into())));
        assert_eq!(credentials_for("docker.io/library/redis:7", &secrets), Some(("hub".into(), "pw".into())));
        // The password may contain ':'.
        assert_eq!(credentials_for("registry.example.com:5000/team/x", &secrets), Some(("me".into(), "s3:cret".into())));
        assert_eq!(credentials_for("quay.io/x/y", &secrets), None);
        assert_eq!(credentials_for("registry.example.com/team/x", &secrets), None, "another port is another registry");
    }

    #[test]
    fn first_secret_wins_and_wildcards_match_one_label() {
        let secrets = vec![
            config_json(json!({"*.example.com": {"auth": b64("wild:1")}})),
            config_json(json!({"a.example.com": {"auth": b64("exact:2")}})),
        ];
        assert_eq!(credentials_for("a.example.com/x", &secrets), Some(("wild".into(), "1".into())));
        assert_eq!(credentials_for("a.b.example.com/x", &secrets), None);
        assert_eq!(credentials_for("example.com/x", &secrets), None);
    }

    #[test]
    fn legacy_dockercfg_and_junk() {
        let legacy = json!({"type": "kubernetes.io/dockercfg", "data": {".dockercfg": b64(&json!({"ghcr.io": {"auth": b64("old:pw")}}).to_string())}});
        let opaque = json!({"type": "Opaque", "data": {"token": b64("x")}});
        let broken = json!({"data": {".dockerconfigjson": "not base64!"}});
        let secrets = vec![opaque, broken, legacy];
        assert_eq!(credentials_for("ghcr.io/a/b", &secrets), Some(("old".into(), "pw".into())));
        assert_eq!(credentials_for("", &secrets), None);
    }

    #[test]
    fn names_of_the_pods_pull_secrets() {
        let pod = json!({"spec": {"imagePullSecrets": [{"name": "a"}, {"name": ""}, {}, {"name": "b"}]}});
        assert_eq!(names(&pod), vec!["a", "b"]);
        assert!(names(&json!({"spec": {}})).is_empty());
    }
}
