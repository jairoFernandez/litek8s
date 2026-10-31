//! `litek8s volume move`: a local-path volume to another node.
//!
//! A local-path PersistentVolume is a directory on one node
//! (`<node dir>/crun/volumes/litek8s-pv-<pv>`) and its node affinity pins the
//! pods that use it there. Moving it: stop those pods (`--scale` scales their
//! StatefulSet or Deployment to 0 and back), copy the directory over SSH (the
//! `cluster.litek8s.dev/ssh` annotation `litek8s cluster apply` puts on each
//! Node), check both copies hold the same files and bytes, and re-create the
//! PV (same name, same claim) with the new node in its affinity.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hyper::Method;
use serde_json::{Value, json};

use crate::remote::{Ssh, sh_quote};
use crate::ui::client::{Endpoint, enc};

#[derive(Debug, clap::Args)]
pub struct MoveArgs {
    /// The claim: `name` (in --namespace) or `namespace/name`.
    pub claim: String,
    #[arg(
        short = 'n',
        long,
        default_value = "default",
        help = "The claim's namespace"
    )]
    pub namespace: String,
    #[arg(long, value_name = "NODE", help = "The node the volume goes to")]
    pub to: String,
    #[arg(
        long,
        help = "Scale the StatefulSets/Deployments using the claim to 0 for the copy, then back"
    )]
    pub scale: bool,
    #[arg(
        long,
        help = "Remove the copy on the old node once the volume is moved"
    )]
    pub delete_source: bool,
    #[arg(long, value_name = "DIR", default_value = crate::install::NODE_DIR, help = "The node agents' directory on both nodes")]
    pub node_dir: PathBuf,
    #[arg(long, help = "kubeconfig context (default: the current one)")]
    pub context: Option<String>,
    #[arg(short = 'y', long, help = "Do not ask before stopping pods and moving")]
    pub yes: bool,
}

/// A workload scaled down for the copy, to scale back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub kind: &'static str,
    pub name: String,
    pub replicas: i64,
}

impl Owner {
    fn path(&self, ns: &str) -> String {
        let plural = if self.kind == "StatefulSet" {
            "statefulsets"
        } else {
            "deployments"
        };
        format!(
            "/apis/apps/v1/namespaces/{}/{plural}/{}",
            enc(ns),
            enc(&self.name)
        )
    }
}

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default()
}

/// `ns/name` or `name` (in `namespace`).
pub fn parse_claim<'a>(claim: &'a str, namespace: &'a str) -> Result<(&'a str, &'a str)> {
    let (ns, name) = claim.split_once('/').unwrap_or((namespace, claim));
    if ns.is_empty() || name.is_empty() || name.contains('/') {
        bail!("claim {claim:?}: want name or namespace/name");
    }
    Ok((ns, name))
}

/// The node a local volume lives on (its node affinity's single hostname).
pub fn pv_node(pv: &Value) -> Option<String> {
    let terms = pv
        .pointer("/spec/nodeAffinity/required/nodeSelectorTerms")?
        .as_array()?;
    let [term] = terms.as_slice() else {
        return None;
    };
    let exprs = term["matchExpressions"].as_array()?;
    let [e] = exprs.as_slice() else { return None };
    let values = e["values"].as_array()?;
    match (e["operator"].as_str(), values.as_slice()) {
        (Some("In"), [v]) => v.as_str().map(str::to_string),
        _ => None,
    }
}

/// Object metadata to create again: no server-set fields.
fn fresh_metadata(meta: &Value, drop_annotations: &[&str]) -> Value {
    let mut m = serde_json::Map::new();
    for key in ["name", "namespace", "labels"] {
        if let Some(v) = meta.get(key) {
            m.insert(key.into(), v.clone());
        }
    }
    let annotations: serde_json::Map<String, Value> = meta["annotations"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| !drop_annotations.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !annotations.is_empty() {
        m.insert("annotations".into(), Value::Object(annotations));
    }
    Value::Object(m)
}

/// The PV again, on `node`, bound in advance to its claim.
pub fn moved_pv(pv: &Value, node: &str) -> Value {
    let mut spec = pv["spec"].clone();
    let key = pv
        .pointer("/spec/nodeAffinity/required/nodeSelectorTerms/0/matchExpressions/0/key")
        .and_then(Value::as_str)
        .unwrap_or("kubernetes.io/hostname");
    spec["nodeAffinity"] = json!({"required": {"nodeSelectorTerms": [
        {"matchExpressions": [{"key": key, "operator": "In", "values": [node]}]}
    ]}});
    spec["claimRef"] = json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "namespace": s(pv, "/spec/claimRef/namespace"),
        "name": s(pv, "/spec/claimRef/name"),
    });
    json!({"apiVersion": "v1", "kind": "PersistentVolume", "metadata": fresh_metadata(&pv["metadata"], &[]), "spec": spec})
}

/// The claim again, asking for that PV by name.
pub fn moved_pvc(pvc: &Value, pv_name: &str) -> Value {
    let mut spec = pvc["spec"].clone();
    spec["volumeName"] = json!(pv_name);
    let meta = fresh_metadata(
        &pvc["metadata"],
        &[
            "pv.kubernetes.io/bind-completed",
            "pv.kubernetes.io/bound-by-controller",
            "volume.kubernetes.io/selected-node",
        ],
    );
    json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": meta, "spec": spec})
}

/// Pods (not finished) with a volume from `claim`.
pub fn pods_using<'a>(pods: &'a [Value], claim: &str) -> Vec<&'a Value> {
    pods.iter()
        .filter(|p| !matches!(s(p, "/status/phase"), "Succeeded" | "Failed"))
        .filter(|p| {
            p.pointer("/spec/volumes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|v| {
                    v.pointer("/persistentVolumeClaim/claimName")
                        .and_then(Value::as_str)
                        == Some(claim)
                })
        })
        .collect()
}

/// The shell commands on a node: where the data is, and how to read it.
pub fn volume_dir(node_dir: &std::path::Path, pv: &str) -> String {
    format!("{}/crun/volumes/litek8s-pv-{pv}", node_dir.display())
}

/// `<files> <bytes>` of a directory, as both sides print it.
pub fn measure_script(dir: &str) -> String {
    let d = sh_quote(dir);
    format!(
        "test -d {d} || {{ echo missing; exit 0; }}; echo \"$(find {d} | wc -l) $(du -sb {d} | cut -f1)\""
    )
}

fn node_ssh(node: &Value) -> Result<Ssh> {
    let name = s(node, "/metadata/name");
    let raw = node
        .pointer("/metadata/annotations/cluster.litek8s.dev~1ssh")
        .and_then(Value::as_str)
        .with_context(|| format!("node {name} has no {} annotation: it was not installed by `litek8s cluster apply`, so litek8s does not know how to reach it over SSH", crate::cluster::ANNOTATION_SSH))?;
    let ssh: Ssh =
        serde_json::from_str(raw).with_context(|| format!("node {name}: bad SSH annotation"))?;
    ssh.validate()?;
    Ok(ssh)
}

async fn ssh_output(ssh: &Ssh, script: &str) -> Result<String> {
    let out = tokio::process::Command::new("ssh")
        .args(ssh.args(true))
        .arg(script)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running ssh {}", ssh.display()))?;
    if !out.status.success() {
        bail!(
            "ssh {}: {}",
            ssh.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// tar on the old node, through this machine, into the new one.
async fn copy(from: &Ssh, to: &Ssh, dir: &str) -> Result<()> {
    let d = sh_quote(dir);
    let mut src = tokio::process::Command::new("ssh")
        .args(from.args(true))
        .arg(format!("tar -C {d} --numeric-owner -cf - ."))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("starting ssh to the old node")?;
    let out: Stdio = src.stdout.take().context("no tar output")?.try_into()?;
    let dst = tokio::process::Command::new("ssh")
        .args(to.args(true))
        .arg(format!("mkdir -p {d} && tar -C {d} --numeric-owner -xpf -"))
        .stdin(out)
        .status();
    let (a, b) = tokio::join!(src.wait(), dst);
    if !a?.success() {
        bail!("reading the volume on {} failed", from.display());
    }
    if !b?.success() {
        bail!("writing the volume on {} failed", to.display());
    }
    Ok(())
}

fn gone(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}").to_ascii_lowercase();
    m.contains("not found") || m.contains("404")
}

async fn wait_gone(ep: &Endpoint, path: &str, what: &str) -> Result<()> {
    let start = Instant::now();
    loop {
        match ep.json(Method::GET, path, None).await {
            Err(e) if gone(&e) => return Ok(()),
            Err(e) => return Err(e),
            Ok(_) if start.elapsed() > Duration::from_secs(120) => {
                bail!("{what} is still there after 2 minutes")
            }
            Ok(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

async fn list_pods(ep: &Endpoint, ns: &str) -> Result<Vec<Value>> {
    let v = ep
        .json(
            Method::GET,
            &format!("/api/v1/namespaces/{}/pods", enc(ns)),
            None,
        )
        .await?;
    Ok(v["items"].as_array().cloned().unwrap_or_default())
}

/// The StatefulSets and Deployments behind `pods` (a ReplicaSet's owner).
async fn owners_of(ep: &Endpoint, ns: &str, pods: &[&Value]) -> Result<Vec<Owner>> {
    let mut out: Vec<Owner> = Vec::new();
    for p in pods {
        let pname = s(p, "/metadata/name");
        let Some(o) = p.pointer("/metadata/ownerReferences/0") else {
            bail!("pod {pname} has no owner: stop it yourself, then run again without --scale");
        };
        let (kind, name) = match s(o, "/kind") {
            "StatefulSet" => ("StatefulSet", s(o, "/name").to_string()),
            "ReplicaSet" => {
                let rs = ep
                    .json(
                        Method::GET,
                        &format!(
                            "/apis/apps/v1/namespaces/{}/replicasets/{}",
                            enc(ns),
                            enc(s(o, "/name"))
                        ),
                        None,
                    )
                    .await?;
                match rs.pointer("/metadata/ownerReferences/0") {
                    Some(d) if s(d, "/kind") == "Deployment" => {
                        ("Deployment", s(d, "/name").to_string())
                    }
                    _ => bail!("pod {pname}: its ReplicaSet has no Deployment; stop it yourself"),
                }
            }
            k => bail!(
                "pod {pname} belongs to a {k}: stop it yourself, then run again without --scale"
            ),
        };
        if out.iter().any(|x| x.kind == kind && x.name == name) {
            continue;
        }
        let mut owner = Owner {
            kind,
            name,
            replicas: 0,
        };
        let obj = ep.json(Method::GET, &owner.path(ns), None).await?;
        owner.replicas = obj
            .pointer("/spec/replicas")
            .and_then(Value::as_i64)
            .unwrap_or(1);
        out.push(owner);
    }
    Ok(out)
}

async fn scale(ep: &Endpoint, ns: &str, owners: &[Owner], to_zero: bool) -> Result<()> {
    for o in owners {
        let replicas = if to_zero { 0 } else { o.replicas };
        ep.json_typed(
            Method::PATCH,
            &o.path(ns),
            "application/merge-patch+json",
            Some(&json!({"spec": {"replicas": replicas}})),
        )
        .await
        .with_context(|| format!("scaling {} {} to {replicas}", o.kind, o.name))?;
        println!("  {} {} → {replicas}", o.kind, o.name);
    }
    Ok(())
}

async fn wait_no_pods(ep: &Endpoint, ns: &str, claim: &str) -> Result<()> {
    let start = Instant::now();
    loop {
        let pods = list_pods(ep, ns).await?;
        let left = pods_using(&pods, claim);
        if left.is_empty() {
            return Ok(());
        }
        if start.elapsed() > Duration::from_secs(300) {
            let names: Vec<&str> = left.iter().map(|p| s(p, "/metadata/name")).collect();
            bail!(
                "pods still using {claim} after 5 minutes: {}",
                names.join(", ")
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

pub async fn run(args: MoveArgs) -> Result<()> {
    let (ns, claim) = parse_claim(&args.claim, &args.namespace)?;
    let ep = crate::obs::cli::endpoint(args.context.as_deref())?;
    let pvc_path = format!(
        "/api/v1/namespaces/{}/persistentvolumeclaims/{}",
        enc(ns),
        enc(claim)
    );
    let pvc = ep
        .json(Method::GET, &pvc_path, None)
        .await
        .with_context(|| format!("claim {ns}/{claim}"))?;
    let pv_name = s(&pvc, "/spec/volumeName").to_string();
    if pv_name.is_empty() {
        bail!("claim {ns}/{claim} is not bound to a volume yet");
    }
    let pv_path = format!("/api/v1/persistentvolumes/{}", enc(&pv_name));
    let pv = ep.json(Method::GET, &pv_path, None).await?;
    if pv.pointer("/spec/local").is_none() && pv.pointer("/spec/hostPath").is_none() {
        bail!("{pv_name} is not a local volume: nothing on a node to move");
    }
    let from = pv_node(&pv)
        .with_context(|| format!("{pv_name}: its node affinity does not name one node"))?;
    if from == args.to {
        bail!("{ns}/{claim} is already on {from}");
    }
    let from_node = ep
        .json(Method::GET, &format!("/api/v1/nodes/{}", enc(&from)), None)
        .await
        .with_context(|| format!("node {from}"))?;
    let to_node = ep
        .json(
            Method::GET,
            &format!("/api/v1/nodes/{}", enc(&args.to)),
            None,
        )
        .await
        .with_context(|| format!("node {}", args.to))?;
    let (from_ssh, to_ssh) = (node_ssh(&from_node)?, node_ssh(&to_node)?);
    let dir = volume_dir(&args.node_dir, &pv_name);

    let before = ssh_output(&from_ssh, &measure_script(&dir)).await?;
    if before == "missing" {
        bail!("{dir} is not on {from} (another --node-dir, or a node runtime other than crun?)");
    }
    let target = ssh_output(
        &to_ssh,
        &format!(
            "test -e {d} && ls -A {d} | head -1; true",
            d = sh_quote(&dir)
        ),
    )
    .await?;
    if !target.is_empty() {
        bail!(
            "{dir} already exists on {} and is not empty: remove it first",
            args.to
        );
    }

    let pods = list_pods(&ep, ns).await?;
    let using = pods_using(&pods, claim);
    let owners = if using.is_empty() {
        Vec::new()
    } else if args.scale {
        owners_of(&ep, ns, &using).await?
    } else {
        let names: Vec<&str> = using.iter().map(|p| s(p, "/metadata/name")).collect();
        bail!(
            "{} use {ns}/{claim}: stop them, or pass --scale to scale their owners to 0 and back",
            names.join(", ")
        );
    };
    let (files, bytes) = before.split_once(' ').unwrap_or((&before, "?"));
    println!("{ns}/{claim} ({pv_name}): {from} → {}", args.to);
    println!("  {files} files, {bytes} bytes, copied over SSH through this machine");
    for o in &owners {
        println!(
            "  {} {} is scaled to 0 for the copy, then back to {}",
            o.kind, o.name, o.replicas
        );
    }
    println!(
        "  a GitOps controller that heals replicas (Argo CD, litecd) may scale them back early: pause it first"
    );
    if !args.yes && !confirm("Move it?")? {
        bail!("nothing moved");
    }

    scale(&ep, ns, &owners, true).await?;
    let moved = async {
        wait_no_pods(&ep, ns, claim).await?;
        copy(&from_ssh, &to_ssh, &dir).await?;
        let after = ssh_output(&to_ssh, &measure_script(&dir)).await?;
        let source = ssh_output(&from_ssh, &measure_script(&dir)).await?;
        if after != source {
            bail!("the copy on {} ({after}) differs from the original ({source}); nothing else changed", args.to);
        }
        println!("  copied and checked: {after} (files bytes)");
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = moved {
        let _ = scale(&ep, ns, &owners, false).await;
        return Err(e);
    }

    // Keep the original manifests where a person can find them if the swap stops halfway.
    let saved = std::env::temp_dir().join(format!("litek8s-volume-move-{pv_name}.json"));
    std::fs::write(
        &saved,
        serde_json::to_vec_pretty(&json!({"pv": pv, "pvc": pvc}))?,
    )?;
    let new_pv = moved_pv(&pv, &args.to);
    let new_pvc = moved_pvc(&pvc, &pv_name);
    // Retain: deleting the PV object must not take the old copy with it.
    ep.json_typed(
        Method::PATCH,
        &pv_path,
        "application/merge-patch+json",
        Some(&json!({"spec": {"persistentVolumeReclaimPolicy": "Retain"}})),
    )
    .await?;
    ep.json(Method::DELETE, &pvc_path, None).await?;
    wait_gone(&ep, &pvc_path, "the claim").await?;
    ep.json(Method::DELETE, &pv_path, None).await?;
    wait_gone(&ep, &pv_path, "the volume").await?;
    ep.json(Method::POST, "/api/v1/persistentvolumes", Some(&new_pv))
        .await
        .with_context(|| {
            format!(
                "re-creating {pv_name} (the original is in {})",
                saved.display()
            )
        })?;
    ep.json(
        Method::POST,
        &format!("/api/v1/namespaces/{}/persistentvolumeclaims", enc(ns)),
        Some(&new_pvc),
    )
    .await
    .with_context(|| {
        format!(
            "re-creating {ns}/{claim} (the original is in {})",
            saved.display()
        )
    })?;
    println!("  {pv_name} now on {}", args.to);

    scale(&ep, ns, &owners, false).await?;
    if args.delete_source {
        ssh_output(
            &from_ssh,
            &format!("rm -rf {d} {d}.labels", d = sh_quote(&dir)),
        )
        .await?;
        println!("  removed the old copy on {from}");
    } else {
        let left = ssh_output(
            &from_ssh,
            &format!("test -d {} && echo kept || true", sh_quote(&dir)),
        )
        .await?;
        if left == "kept" {
            println!(
                "  the old copy stays on {from} at {dir}: remove it once the workload looks right"
            );
        } else {
            println!(
                "  {from} removed its old copy with the PV (litek8s before 0.2.19 ignores Retain there)"
            );
        }
    }
    let _ = std::fs::remove_file(saved);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pv() -> Value {
        json!({
            "metadata": {"name": "pvc-1", "uid": "u", "resourceVersion": "9", "creationTimestamp": "t",
                "finalizers": ["kubernetes.io/pv-protection"],
                "annotations": {"pv.kubernetes.io/provisioned-by": "litek8s.dev/local-path"}},
            "spec": {
                "capacity": {"storage": "5Gi"}, "accessModes": ["ReadWriteOnce"],
                "persistentVolumeReclaimPolicy": "Delete", "storageClassName": "local-path",
                "local": {"path": "/var/lib/litek8s/volumes/pvc-1"},
                "claimRef": {"namespace": "db", "name": "data-0", "uid": "c", "resourceVersion": "3"},
                "nodeAffinity": {"required": {"nodeSelectorTerms": [{"matchExpressions": [
                    {"key": "kubernetes.io/hostname", "operator": "In", "values": ["n1"]}]}]}}
            },
            "status": {"phase": "Bound"}
        })
    }

    #[test]
    fn claims_parse_with_or_without_namespace() {
        assert_eq!(
            parse_claim("db/data-0", "default").unwrap(),
            ("db", "data-0")
        );
        assert_eq!(parse_claim("data-0", "db").unwrap(), ("db", "data-0"));
        assert!(parse_claim("a/b/c", "x").is_err());
        assert!(parse_claim("/x", "x").is_err());
    }

    #[test]
    fn a_volume_names_one_node() {
        assert_eq!(pv_node(&pv()).as_deref(), Some("n1"));
        let mut two = pv();
        two["spec"]["nodeAffinity"]["required"]["nodeSelectorTerms"][0]["matchExpressions"][0]["values"] =
            json!(["n1", "n2"]);
        assert_eq!(pv_node(&two), None);
        assert_eq!(pv_node(&json!({"spec": {}})), None);
    }

    #[test]
    fn the_moved_volume_keeps_its_claim_and_policy_on_the_new_node() {
        let m = moved_pv(&pv(), "n2");
        assert_eq!(pv_node(&m).as_deref(), Some("n2"));
        assert_eq!(
            m["metadata"],
            json!({"name": "pvc-1", "annotations": {"pv.kubernetes.io/provisioned-by": "litek8s.dev/local-path"}})
        );
        assert_eq!(
            m["spec"]["claimRef"],
            json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim", "namespace": "db", "name": "data-0"})
        );
        assert_eq!(m["spec"]["persistentVolumeReclaimPolicy"], "Delete");
        assert_eq!(m["spec"]["local"]["path"], "/var/lib/litek8s/volumes/pvc-1");
        assert!(m.get("status").is_none());
    }

    #[test]
    fn the_moved_claim_asks_for_the_volume_by_name() {
        let pvc = json!({
            "metadata": {"name": "data-0", "namespace": "db", "uid": "c", "labels": {"app": "db"},
                "annotations": {"pv.kubernetes.io/bind-completed": "yes", "keep": "me", "volume.kubernetes.io/selected-node": "n1"}},
            "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "5Gi"}},
                "storageClassName": "local-path", "volumeName": "pvc-1"},
            "status": {"phase": "Bound"}
        });
        let m = moved_pvc(&pvc, "pvc-1");
        assert_eq!(
            m["metadata"],
            json!({"name": "data-0", "namespace": "db", "labels": {"app": "db"}, "annotations": {"keep": "me"}})
        );
        assert_eq!(m["spec"]["volumeName"], "pvc-1");
        assert_eq!(m["spec"]["resources"]["requests"]["storage"], "5Gi");
    }

    #[test]
    fn only_live_pods_with_the_claim_count() {
        let pod = |name: &str, claim: &str, phase: &str| {
            json!({"metadata": {"name": name}, "status": {"phase": phase},
                "spec": {"volumes": [{"name": "d", "persistentVolumeClaim": {"claimName": claim}}]}})
        };
        let pods = vec![
            pod("a", "data-0", "Running"),
            pod("b", "data-1", "Running"),
            pod("c", "data-0", "Succeeded"),
        ];
        let names: Vec<&str> = pods_using(&pods, "data-0")
            .iter()
            .map(|p| s(p, "/metadata/name"))
            .collect();
        assert_eq!(names, ["a"]);
    }

    #[test]
    fn the_volume_dir_is_the_crun_runtime_volume() {
        assert_eq!(
            volume_dir(std::path::Path::new("/var/lib/litek8s/node-agent"), "pvc-1"),
            "/var/lib/litek8s/node-agent/crun/volumes/litek8s-pv-pvc-1"
        );
        assert!(measure_script("/a b").contains("'/a b'"));
    }
}
