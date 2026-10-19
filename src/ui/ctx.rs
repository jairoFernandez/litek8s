//! Whether the user's `docker` and `kubectl` point at litek8s, and
//! switching them there and back (what Docker Desktop and OrbStack do when
//! they start).
//!
//! The Docker side uses the CLI's own context store
//! (`$DOCKER_CONFIG/contexts`, `currentContext` in config.json), through
//! `docker context` when the CLI is installed. The kube side changes only
//! `current-context`. What was current before is remembered in
//! `ui-state.json` next to the settings file, for "switch back".

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::app::Contexts;
use crate::dev::cluster::Cluster;
use crate::dev::kubeconfig;

/// The docker host of a local cluster.
pub fn litek8s_docker_host(cluster: &Cluster) -> String {
    format!("unix://{}", cluster.docker_socket().display())
}

fn docker_config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DOCKER_CONFIG") {
        return PathBuf::from(d);
    }
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".docker")
}

fn meta_dir(name: &str) -> PathBuf {
    docker_config_dir().join("contexts").join("meta").join(hex::encode(Sha256::digest(name.as_bytes())))
}

fn read_json(path: &std::path::Path) -> Value {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

/// The docker CLI's current context (`DOCKER_CONTEXT` wins).
pub fn docker_context() -> String {
    if let Ok(c) = std::env::var("DOCKER_CONTEXT") {
        if !c.is_empty() {
            return c;
        }
    }
    read_json(&docker_config_dir().join("config.json"))["currentContext"].as_str().filter(|s| !s.is_empty()).unwrap_or("default").to_string()
}

/// The Docker host of a context (`default`: the system socket).
pub fn docker_context_host(name: &str) -> Option<String> {
    if name == "default" {
        return Some("unix:///var/run/docker.sock".into());
    }
    read_json(&meta_dir(name).join("meta.json")).pointer("/Endpoints/docker/Host").and_then(Value::as_str).map(str::to_string)
}

fn same_host(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let p = s.strip_prefix("unix://").unwrap_or(s);
        std::fs::canonicalize(p).map(|c| c.display().to_string()).unwrap_or_else(|_| p.to_string())
    };
    a == b || norm(a) == norm(b)
}

pub fn detect(cluster: &Cluster, dev_port: Option<u16>) -> Contexts {
    let ours = litek8s_docker_host(cluster);
    let docker_host_env = std::env::var("DOCKER_HOST").ok().filter(|s| !s.is_empty());
    let docker_context = docker_context();
    let ctx_host = docker_context_host(&docker_context).unwrap_or_default();
    let path = kubeconfig::default_path();
    let (kube_context, server) = kubeconfig::current(&path).unwrap_or_default();
    let dev_server = dev_port.map(|p| format!("http://127.0.0.1:{p}"));
    Contexts {
        docker_host_is_litek8s: docker_host_env.as_deref().is_some_and(|h| same_host(h, &ours)),
        docker_host_env,
        docker_context_is_litek8s: same_host(&ctx_host, &ours),
        docker_context,
        kube_is_litek8s: kube_context == cluster.context() || (server.is_some() && server == dev_server),
        kube_context,
        kubeconfig: path.display().to_string(),
    }
}

/// The local cluster the UI showed last.
pub fn active_cluster() -> Option<String> {
    remembered("cluster").filter(|c| crate::dev::cluster::validate(c).is_ok())
}

pub fn set_active_cluster(name: &str) {
    remember("cluster", name);
}

fn state_path() -> PathBuf {
    crate::dev::config::path().with_file_name("ui-state.json")
}

fn remember(key: &str, value: &str) {
    let path = state_path();
    let mut state = read_json(&path);
    if !state.is_object() {
        state = json!({});
    }
    state[key] = json!(value);
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&path, serde_json::to_vec_pretty(&state).unwrap_or_default());
}

fn remembered(key: &str) -> Option<String> {
    read_json(&state_path())[key].as_str().map(str::to_string)
}

fn docker_cli(args: &[&str]) -> Option<Result<String>> {
    crate::dev::which("docker")?;
    Some(match std::process::Command::new("docker").args(args).stdin(std::process::Stdio::null()).output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        Ok(o) => Err(anyhow::anyhow!("docker {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim())),
        Err(e) => Err(e.into()),
    })
}

fn is_ours(context: &str) -> bool {
    context == "litek8s" || context.starts_with("litek8s-")
}

/// Point the docker CLI at a local cluster (`on`) or back at what it used
/// before litek8s.
pub fn switch_docker(on: bool, cluster: &Cluster) -> Result<String> {
    let current = docker_context();
    let ours = litek8s_docker_host(cluster);
    let name = cluster.docker_context();
    let target = if on {
        if !is_ours(&current) {
            remember("docker_context_before", &current);
        }
        ensure_docker_context(&name, &ours, cluster)?;
        name
    } else {
        remembered("docker_context_before").filter(|c| !is_ours(c) && (c == "default" || docker_context_host(c).is_some())).unwrap_or_else(|| "default".into())
    };
    match docker_cli(&["context", "use", &target]) {
        Some(r) => {
            r?;
        }
        None => set_current_docker_context(&target)?,
    }
    let mut msg = format!("docker context: {target}");
    if std::env::var("DOCKER_HOST").is_ok_and(|h| !h.is_empty()) {
        msg.push_str(" (DOCKER_HOST is set in this shell and wins over contexts)");
    }
    Ok(msg)
}

fn ensure_docker_context(name: &str, host: &str, cluster: &Cluster) -> Result<()> {
    let existing = docker_context_host(name);
    if existing.as_deref() == Some(host) {
        return Ok(());
    }
    let docker_arg = format!("host={host}");
    let verb = if existing.is_some() { "update" } else { "create" };
    let description = format!("litek8s {}", cluster.name);
    if let Some(r) = docker_cli(&["context", verb, name, "--description", &description, "--docker", &docker_arg]) {
        return r.map(|_| ());
    }
    // No CLI: write the context the way `docker context create` does.
    let dir = meta_dir(name);
    std::fs::create_dir_all(&dir)?;
    let meta = json!({"Name": name, "Metadata": {"Description": description}, "Endpoints": {"docker": {"Host": host, "SkipTLSVerify": false}}});
    std::fs::write(dir.join("meta.json"), serde_json::to_vec(&meta)?).context("failed to write the docker context")?;
    Ok(())
}

fn set_current_docker_context(name: &str) -> Result<()> {
    let path = docker_config_dir().join("config.json");
    let mut config = read_json(&path);
    if !config.is_object() {
        config = json!({});
    }
    let map = config.as_object_mut().unwrap();
    if name == "default" {
        map.remove("currentContext");
    } else {
        map.insert("currentContext".into(), json!(name));
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?).with_context(|| format!("failed to write {}", path.display()))
}

/// Make a local cluster's context kubectl's current one (`on`), or the one
/// before litek8s.
pub fn switch_kube(on: bool, cluster: &Cluster, dev_port: Option<u16>) -> Result<String> {
    let path = kubeconfig::default_path();
    let current = kubeconfig::current(&path).map(|(c, _)| c).unwrap_or_default();
    let context = cluster.context();
    if on {
        if !is_ours(&current) && !current.is_empty() {
            remember("kube_context_before", &current);
        }
        match dev_port {
            Some(port) => kubeconfig::install(&path, &context, &format!("http://127.0.0.1:{port}"), true)?,
            None => kubeconfig::set_current(&path, &context).with_context(|| format!("start {} first (it adds the {context} context)", cluster.name))?,
        }
        return Ok(format!("kubectl context: {context}"));
    }
    let Some(before) = remembered("kube_context_before").filter(|c| !is_ours(c)) else {
        bail!("no earlier kubectl context recorded; use `kubectl config use-context <name>`");
    };
    kubeconfig::set_current(&path, &before)?;
    Ok(format!("kubectl context: {before}"))
}
