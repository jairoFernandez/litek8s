//! What the UI shows, parsed from Docker and Kubernetes API replies.
//! Parsing is lenient: a missing field shows as empty, never fails a list.

use std::collections::BTreeMap;

use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub image_id: String,
    /// `running`, `exited`, `created`, `paused`, `restarting`, `dead`.
    pub state: String,
    /// `Up 5 minutes`, `Exited (0) 2 hours ago`.
    pub status: String,
    pub project: Option<String>,
    pub service: Option<String>,
    pub ports: String,
    pub size_rw: Option<u64>,
    pub created: i64,
    pub mounts: Vec<String>,
}

impl Container {
    pub fn running(&self) -> bool {
        matches!(self.state.as_str(), "running" | "restarting" | "paused")
    }

    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(12)]
    }
}

pub fn containers(list: &Value) -> Vec<Container> {
    let mut out: Vec<Container> = list
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let labels = &c["Labels"];
            let label = |k: &str| labels[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
            let name = c["Names"].as_array().and_then(|n| n.first()).and_then(Value::as_str).unwrap_or_default().trim_start_matches('/').to_string();
            Container {
                id: c["Id"].as_str().unwrap_or_default().to_string(),
                name,
                image: c["Image"].as_str().unwrap_or_default().to_string(),
                image_id: c["ImageID"].as_str().unwrap_or_default().to_string(),
                state: c["State"].as_str().unwrap_or_default().to_string(),
                status: c["Status"].as_str().unwrap_or_default().to_string(),
                project: label("com.docker.compose.project"),
                service: label("com.docker.compose.service"),
                ports: ports(&c["Ports"]),
                size_rw: c["SizeRw"].as_u64(),
                created: c["Created"].as_i64().unwrap_or(0),
                mounts: c["Mounts"].as_array().into_iter().flatten().filter_map(|m| m["Name"].as_str().filter(|s| !s.is_empty()).map(str::to_string)).collect(),
            }
        })
        .collect();
    out.sort_by(|a, b| a.project.cmp(&b.project).then_with(|| a.name.cmp(&b.name)));
    out
}

/// `0.0.0.0:8080->80/tcp, 5432/tcp` (published first, deduplicated across
/// IPv4/IPv6).
fn ports(list: &Value) -> String {
    let mut published = Vec::new();
    let mut exposed = Vec::new();
    for p in list.as_array().into_iter().flatten() {
        let private = p["PrivatePort"].as_u64().unwrap_or(0);
        let proto = p["Type"].as_str().unwrap_or("tcp");
        match p["PublicPort"].as_u64() {
            Some(public) => {
                let s = format!("{public}->{private}/{proto}");
                if !published.contains(&s) {
                    published.push(s);
                }
            }
            None => {
                let s = format!("{private}/{proto}");
                if !exposed.contains(&s) {
                    exposed.push(s);
                }
            }
        }
    }
    published.extend(exposed);
    published.join(", ")
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Image {
    pub id: String,
    pub tags: Vec<String>,
    pub size: u64,
    pub created: i64,
    /// Containers (names) created from it.
    pub containers: Vec<String>,
    /// Pods (`ns/name`) running it.
    pub pods: Vec<String>,
}

impl Image {
    pub fn short_id(&self) -> &str {
        let id = self.id.trim_start_matches("sha256:");
        &id[..id.len().min(12)]
    }

    pub fn dangling(&self) -> bool {
        self.tags.is_empty() || self.tags.iter().all(|t| t == "<none>:<none>")
    }

    pub fn unused(&self) -> bool {
        self.containers.is_empty() && self.pods.is_empty()
    }

    pub fn name(&self) -> String {
        if self.dangling() { "<none>".into() } else { self.tags.join(", ") }
    }
}

pub fn images(list: &Value) -> Vec<Image> {
    let mut out: Vec<Image> = list
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| Image {
            id: i["Id"].as_str().unwrap_or_default().to_string(),
            tags: i["RepoTags"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|t| *t != "<none>:<none>").map(str::to_string).collect(),
            size: i["Size"].as_u64().unwrap_or(0),
            created: i["Created"].as_i64().unwrap_or(0),
            containers: Vec::new(),
            pods: Vec::new(),
        })
        .collect();
    out.sort_by(|a, b| a.dangling().cmp(&b.dangling()).then_with(|| a.name().cmp(&b.name())));
    out
}

/// A pod image reference as Docker lists it: `nginx` → `nginx:latest`,
/// `docker.io/library/nginx:1` → `nginx:1`.
pub fn short_image(name: &str) -> String {
    let n = name.split('@').next().unwrap_or(name);
    let n = n.strip_prefix("docker.io/").map(|r| r.strip_prefix("library/").unwrap_or(r)).unwrap_or(n);
    let last = n.rsplit('/').next().unwrap_or(n);
    if last.contains(':') { n.to_string() } else { format!("{n}:latest") }
}

/// Which containers and pods use each image.
pub fn link_images(images: &mut [Image], containers: &[Container], pods: &[Pod]) {
    for img in images.iter_mut() {
        img.containers = containers
            .iter()
            .filter(|c| c.image_id == img.id || img.tags.iter().any(|t| *t == c.image || *t == short_image(&c.image)))
            .map(|c| c.name.clone())
            .collect();
        img.pods = pods
            .iter()
            .filter(|p| p.images.iter().any(|i| img.tags.contains(&short_image(i)) || i.ends_with(img.id.trim_start_matches("sha256:")) || *i == img.id))
            .map(|p| format!("{}/{}", p.namespace, p.name))
            .collect();
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Volume {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    pub size: Option<u64>,
    pub users: Vec<String>,
    /// Anonymous (a 64-hex name) rather than named.
    pub anonymous: bool,
    pub project: Option<String>,
}

pub fn volumes(list: &Value) -> Vec<Volume> {
    let mut out: Vec<Volume> = list["Volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| {
            let name = v["Name"].as_str().unwrap_or_default().to_string();
            Volume {
                anonymous: name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()),
                driver: v["Driver"].as_str().unwrap_or_default().to_string(),
                mountpoint: v["Mountpoint"].as_str().unwrap_or_default().to_string(),
                size: v["UsageData"]["Size"].as_i64().filter(|s| *s >= 0).map(|s| s as u64),
                users: Vec::new(),
                project: v["Labels"]["com.docker.compose.project"].as_str().map(str::to_string),
                name,
            }
        })
        .collect();
    out.sort_by(|a, b| a.anonymous.cmp(&b.anonymous).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Volume sizes from `GET /system/df` and the containers mounting each.
pub fn link_volumes(volumes: &mut [Volume], df: &DiskUsage, containers: &[Container]) {
    for v in volumes.iter_mut() {
        if let Some(size) = df.volume_sizes.get(&v.name) {
            v.size = Some(*size);
        }
        v.users = containers.iter().filter(|c| c.mounts.contains(&v.name)).map(|c| c.name.clone()).collect();
    }
}

/// `GET /system/df`, reduced to what the free-space view needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiskUsage {
    pub layers: u64,
    pub volume_sizes: BTreeMap<String, u64>,
    pub container_sizes: BTreeMap<String, u64>,
    /// Build cache: bytes a prune frees, number of records.
    pub build_cache: u64,
    pub build_cache_records: usize,
    pub loaded: bool,
}

pub fn disk_usage(df: &Value) -> DiskUsage {
    let mut u = DiskUsage { layers: df["LayersSize"].as_u64().unwrap_or(0), loaded: true, ..Default::default() };
    for v in df["Volumes"].as_array().into_iter().flatten() {
        if let (Some(name), Some(size)) = (v["Name"].as_str(), v["UsageData"]["Size"].as_i64()) {
            u.volume_sizes.insert(name.to_string(), size.max(0) as u64);
        }
    }
    for c in df["Containers"].as_array().into_iter().flatten() {
        if let (Some(id), Some(size)) = (c["Id"].as_str(), c["SizeRw"].as_u64()) {
            u.container_sizes.insert(id.to_string(), size);
        }
    }
    for b in df["BuildCache"].as_array().into_iter().flatten() {
        if !b["InUse"].as_bool().unwrap_or(false) {
            u.build_cache += b["Size"].as_u64().unwrap_or(0);
        }
        u.build_cache_records += 1;
    }
    u
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pod {
    pub uid: String,
    pub namespace: String,
    pub name: String,
    /// What `kubectl get pods` shows in STATUS.
    pub status: String,
    pub ready: String,
    pub restarts: i64,
    pub created: i64,
    pub node: String,
    pub containers: Vec<String>,
    pub images: Vec<String>,
    /// Container ports, for a port-forward's default.
    pub ports: Vec<u16>,
    pub owner: Option<String>,
    pub deleting: bool,
}

pub fn pods(list: &Value) -> Vec<Pod> {
    let mut out: Vec<Pod> = list["items"].as_array().into_iter().flatten().map(pod).collect();
    out.sort_by(|a, b| a.namespace.cmp(&b.namespace).then_with(|| a.name.cmp(&b.name)));
    out
}

pub fn pod(p: &Value) -> Pod {
    let statuses: Vec<&Value> = p.pointer("/status/containerStatuses").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let specs: Vec<&Value> = p.pointer("/spec/containers").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let ready = statuses.iter().filter(|s| s["ready"].as_bool() == Some(true)).count();
    let deleting = p.pointer("/metadata/deletionTimestamp").is_some_and(|d| !d.is_null());
    // The reason of a waiting or terminated container wins over the phase,
    // as in kubectl.
    let mut status = p.pointer("/status/reason").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| p.pointer("/status/phase").and_then(Value::as_str).unwrap_or("Pending").to_string());
    for s in &statuses {
        if let Some(r) = s.pointer("/state/waiting/reason").and_then(Value::as_str) {
            status = r.to_string();
        } else if let Some(r) = s.pointer("/state/terminated/reason").and_then(Value::as_str) {
            if status != "Running" || s["ready"].as_bool() != Some(true) {
                status = r.to_string();
            }
        }
    }
    if deleting {
        status = "Terminating".into();
    }
    let mut images: Vec<String> = specs.iter().filter_map(|c| c["image"].as_str().map(str::to_string)).collect();
    images.extend(statuses.iter().filter_map(|s| s["imageID"].as_str().filter(|i| !i.is_empty()).map(str::to_string)));
    Pod {
        uid: p.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or_default().to_string(),
        namespace: p.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or("default").to_string(),
        name: p.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string(),
        status,
        ready: format!("{ready}/{}", specs.len()),
        restarts: statuses.iter().filter_map(|s| s["restartCount"].as_i64()).sum(),
        created: timestamp(p.pointer("/metadata/creationTimestamp")),
        node: p.pointer("/spec/nodeName").and_then(Value::as_str).unwrap_or_default().to_string(),
        containers: specs.iter().filter_map(|c| c["name"].as_str().map(str::to_string)).collect(),
        images,
        ports: specs.iter().flat_map(|c| c["ports"].as_array().into_iter().flatten()).filter_map(|p| p["containerPort"].as_u64().map(|n| n as u16)).collect(),
        owner: p.pointer("/metadata/ownerReferences/0/name").and_then(Value::as_str).map(str::to_string),
        deleting,
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Deployment {
    pub namespace: String,
    pub name: String,
    pub desired: i64,
    pub ready: i64,
    pub updated: i64,
    pub available: i64,
    pub created: i64,
    /// `matchLabels` of the selector, to find its pods.
    pub selector: BTreeMap<String, String>,
}

pub fn deployments(list: &Value) -> Vec<Deployment> {
    let mut out: Vec<Deployment> = list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| Deployment {
            namespace: d.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or("default").to_string(),
            name: d.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string(),
            desired: d.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1),
            ready: d.pointer("/status/readyReplicas").and_then(Value::as_i64).unwrap_or(0),
            updated: d.pointer("/status/updatedReplicas").and_then(Value::as_i64).unwrap_or(0),
            available: d.pointer("/status/availableReplicas").and_then(Value::as_i64).unwrap_or(0),
            created: timestamp(d.pointer("/metadata/creationTimestamp")),
            selector: d
                .pointer("/spec/selector/matchLabels")
                .and_then(Value::as_object)
                .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
                .unwrap_or_default(),
        })
        .collect();
    out.sort_by(|a, b| a.namespace.cmp(&b.namespace).then_with(|| a.name.cmp(&b.name)));
    out
}

pub fn names(list: &Value) -> Vec<String> {
    let mut out: Vec<String> = list["items"].as_array().into_iter().flatten().filter_map(|n| n.pointer("/metadata/name").and_then(Value::as_str).map(str::to_string)).collect();
    out.sort();
    out
}

fn timestamp(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_str).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|t| t.timestamp()).unwrap_or(0)
}

/// `5s`, `3m`, `2h`, `4d` like kubectl.
pub fn age(now: i64, then: i64) -> String {
    if then <= 0 {
        return "-".into();
    }
    let s = (now - then).max(0);
    match s {
        0..=119 => format!("{s}s"),
        120..=7199 => format!("{}m", s / 60),
        7200..=172_799 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// `1.2 GB`, `512 MB`, `3.4 kB` (decimal units, as Docker prints sizes).
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else if v >= 100.0 { format!("{v:.0} {}", UNITS[u]) } else { format!("{v:.1} {}", UNITS[u]) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_containers_like_docker_ps() {
        let list = json!([
            {"Id": "b".repeat(64), "Names": ["/web-1"], "Image": "nginx", "ImageID": "sha256:aa", "State": "running", "Status": "Up 2 minutes",
             "Labels": {"com.docker.compose.project": "shop", "com.docker.compose.service": "web"},
             "Ports": [{"PrivatePort": 80, "PublicPort": 8080, "Type": "tcp", "IP": "0.0.0.0"}, {"PrivatePort": 80, "PublicPort": 8080, "Type": "tcp", "IP": "::"}, {"PrivatePort": 443, "Type": "tcp"}],
             "Mounts": [{"Type": "volume", "Name": "shop_data"}]},
            {"Id": "a".repeat(64), "Names": ["/solo"], "Image": "alpine", "State": "exited", "Status": "Exited (0) 1 hour ago", "Labels": null}
        ]);
        let c = containers(&list);
        assert_eq!(c.len(), 2);
        // Standalone containers first (no project sorts before Some).
        assert_eq!(c[0].name, "solo");
        assert!(!c[0].running());
        assert_eq!(c[1].project.as_deref(), Some("shop"));
        assert_eq!(c[1].ports, "8080->80/tcp, 443/tcp");
        assert_eq!(c[1].mounts, ["shop_data"]);
        assert_eq!(c[1].short_id().len(), 12);
    }

    #[test]
    fn links_images_to_containers_and_pods() {
        let mut imgs = images(&json!([
            {"Id": "sha256:aa", "RepoTags": ["nginx:latest"], "Size": 190_000_000, "Created": 1},
            {"Id": "sha256:bb", "RepoTags": [], "Size": 5, "Created": 2, "Labels": null},
            {"Id": "sha256:cc", "RepoTags": ["redis:7"], "Size": 40_000_000}
        ]));
        assert_eq!(imgs[2].name(), "<none>");
        let ctrs = vec![Container { name: "web".into(), image: "nginx".into(), image_id: "sha256:aa".into(), ..Default::default() }];
        let pods = vec![Pod { namespace: "default".into(), name: "cache-0".into(), images: vec!["docker.io/library/redis:7".into()], ..Default::default() }];
        link_images(&mut imgs, &ctrs, &pods);
        assert_eq!(imgs[0].containers, ["web"]);
        assert_eq!(imgs[1].pods, ["default/cache-0"]);
        assert!(imgs[2].unused() && imgs[2].dangling());
        assert_eq!(short_image("ghcr.io/x/y"), "ghcr.io/x/y:latest");
        assert_eq!(short_image("localhost:5000/app"), "localhost:5000/app:latest");
        assert_eq!(short_image("nginx@sha256:00"), "nginx:latest");
    }

    #[test]
    fn pod_status_like_kubectl() {
        let p = pod(&json!({
            "metadata": {"name": "web-1", "namespace": "default", "creationTimestamp": "2026-01-01T00:00:00Z", "ownerReferences": [{"name": "web-abc"}]},
            "spec": {"nodeName": "litek8s-dev", "containers": [{"name": "nginx", "image": "nginx", "ports": [{"containerPort": 80}]}]},
            "status": {"phase": "Pending", "containerStatuses": [{"ready": false, "restartCount": 3, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}
        }));
        assert_eq!(p.status, "CrashLoopBackOff");
        assert_eq!(p.ready, "0/1");
        assert_eq!(p.restarts, 3);
        assert_eq!(p.ports, [80]);
        assert_eq!(p.owner.as_deref(), Some("web-abc"));
        assert_eq!(p.created, 1767225600);
    }

    #[test]
    fn disk_usage_and_volumes() {
        let df = disk_usage(&json!({
            "LayersSize": 10, "Containers": [{"Id": "c1", "SizeRw": 7}],
            "Volumes": [{"Name": "data", "UsageData": {"Size": 4096, "RefCount": 1}}],
            "BuildCache": [{"ID": "x", "Size": 100, "InUse": false}, {"ID": "y", "Size": 50, "InUse": true}]
        }));
        assert_eq!(df.build_cache, 100);
        assert_eq!(df.build_cache_records, 2);
        let mut vols = volumes(&json!({"Volumes": [{"Name": "data", "Driver": "local"}, {"Name": "f".repeat(64)}]}));
        let ctrs = vec![Container { name: "db".into(), mounts: vec!["data".into()], ..Default::default() }];
        link_volumes(&mut vols, &df, &ctrs);
        assert_eq!(vols[0].size, Some(4096));
        assert_eq!(vols[0].users, ["db"]);
        assert!(vols[1].anonymous);
    }

    #[test]
    fn formats() {
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_500), "1.5 kB");
        assert_eq!(bytes(190_000_000), "190 MB");
        assert_eq!(age(1000, 990), "10s");
        assert_eq!(age(10_000, 0), "-");
        assert_eq!(age(200_000, 0 + 1), "2d");
    }
}
