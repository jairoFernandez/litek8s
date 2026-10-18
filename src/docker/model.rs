//! What the engine keeps about containers, networks and volumes, and how it
//! shows them through the API (Docker's JSON shapes).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Docker's "zero" time, used for timestamps that never happened.
pub const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

pub fn parse_time(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&chrono::Utc))
}

/// A container's mount as `docker inspect` shows it, plus what the engine
/// needs to set it up.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MountPoint {
    /// `bind`, `volume` or `tmpfs`.
    pub kind: String,
    /// The volume's name (volumes only).
    #[serde(default)]
    pub name: Option<String>,
    /// Host path (binds; for volumes their data directory).
    #[serde(default)]
    pub source: String,
    pub destination: String,
    pub rw: bool,
    #[serde(default)]
    pub mode: String,
    /// Do not copy the image's files into an empty volume.
    #[serde(default)]
    pub nocopy: bool,
    /// An anonymous volume (removed with `docker rm -v`).
    #[serde(default)]
    pub anonymous: bool,
    /// tmpfs size in bytes.
    #[serde(default)]
    pub tmpfs_size: Option<i64>,
    #[serde(default)]
    pub tmpfs_mode: Option<u32>,
}

impl MountPoint {
    pub fn to_api(&self) -> Value {
        let mut m = json!({
            "Type": self.kind,
            "Source": self.source,
            "Destination": self.destination,
            "Mode": self.mode,
            "RW": self.rw,
            "Propagation": if self.kind == "bind" { "rprivate" } else { "" },
        });
        if let Some(n) = &self.name {
            m["Name"] = json!(n);
            m["Driver"] = json!("local");
        }
        m
    }
}

/// A container's attachment to a network.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Endpoint {
    pub network_id: String,
    pub network_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// A requested static address (`--ip`).
    #[serde(default)]
    pub requested_ip: Option<String>,
    #[serde(default)]
    pub endpoint_id: String,
    #[serde(default)]
    pub mac: String,
    /// While the container runs.
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub gateway: String,
    #[serde(default)]
    pub prefix: u8,
    /// Interface name inside the container (eth0, eth1...).
    #[serde(default)]
    pub interface: String,
    #[serde(default)]
    pub driver_opts: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HealthLog {
    pub start: String,
    pub end: String,
    pub exit_code: i64,
    pub output: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Health {
    /// `starting`, `healthy`, `unhealthy`.
    pub status: String,
    pub failing_streak: u64,
    #[serde(default)]
    pub log: Vec<HealthLog>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct State {
    /// `created`, `running`, `paused`, `restarting`, `removing`, `exited`, `dead`.
    pub status: String,
    #[serde(default)]
    pub pid: i64,
    #[serde(default)]
    pub exit_code: i64,
    #[serde(default)]
    pub error: String,
    pub started_at: String,
    pub finished_at: String,
    #[serde(default)]
    pub oom_killed: bool,
    #[serde(default)]
    pub health: Option<Health>,
    /// Published ports while running: "80/tcp" -> [(host ip, host port)].
    #[serde(default)]
    pub ports: BTreeMap<String, Vec<(String, String)>>,
    /// The network namespace file while running.
    #[serde(default)]
    pub netns: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            status: "created".into(),
            pid: 0,
            exit_code: 0,
            error: String::new(),
            started_at: ZERO_TIME.into(),
            finished_at: ZERO_TIME.into(),
            oom_killed: false,
            health: None,
            ports: BTreeMap::new(),
            netns: None,
        }
    }
}

impl State {
    pub fn running(&self) -> bool {
        matches!(self.status.as_str(), "running" | "paused" | "restarting")
    }
}

/// Everything about a container that survives the engine (container.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub name: String,
    pub created: String,
    /// The image as asked for (`Config.Image`).
    pub image: String,
    /// `sha256:<config digest>`.
    pub image_id: String,
    /// The image store key holding the image's layers.
    pub image_key: String,
    /// Docker's `Config`, merged with the image's.
    pub config: Value,
    pub host_config: Value,
    /// Endpoints by network name, in attach order.
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
    #[serde(default)]
    pub mounts: Vec<MountPoint>,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub state: State,
    #[serde(default)]
    pub restart_count: u64,
    /// Stopped through the API (restart policies leave it stopped).
    #[serde(default)]
    pub manually_stopped: bool,
}

impl Record {
    pub fn tty(&self) -> bool {
        self.config["Tty"].as_bool().unwrap_or(false)
    }

    pub fn labels(&self) -> BTreeMap<String, String> {
        self.config["Labels"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
            .unwrap_or_default()
    }

    pub fn network_mode(&self) -> String {
        match self.host_config["NetworkMode"].as_str() {
            None | Some("") | Some("default") => "bridge".into(),
            Some(m) => m.to_string(),
        }
    }

    /// argv as Docker shows it: `Path` and `Args`.
    pub fn path_args(&self) -> (String, Vec<String>) {
        let strings = |v: &Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default() };
        let mut argv = strings(&self.config["Entrypoint"]);
        argv.extend(strings(&self.config["Cmd"]));
        if argv.is_empty() {
            return (String::new(), Vec::new());
        }
        let path = argv.remove(0);
        (path, argv)
    }

    pub fn restart_policy(&self) -> (String, i64) {
        let p = &self.host_config["RestartPolicy"];
        (p["Name"].as_str().unwrap_or("no").to_string(), p["MaximumRetryCount"].as_i64().unwrap_or(0))
    }

    /// `Up 5 minutes (healthy)`, `Exited (0) 3 seconds ago`, `Created`.
    pub fn status_text(&self) -> String {
        let s = &self.state;
        let ago = |t: &str| parse_time(t).map(|t| human_duration(chrono::Utc::now() - t)).unwrap_or_default();
        match s.status.as_str() {
            "running" | "paused" => {
                let mut text = format!("Up {}", ago(&s.started_at));
                if s.status == "paused" {
                    text.push_str(" (Paused)");
                }
                if let Some(h) = &s.health {
                    let label = if h.status == "starting" { "health: starting".to_string() } else { h.status.clone() };
                    text.push_str(&format!(" ({label})"));
                }
                text
            }
            "restarting" => format!("Restarting ({}) {} ago", s.exit_code, ago(&s.finished_at)),
            "exited" => format!("Exited ({}) {} ago", s.exit_code, ago(&s.finished_at)),
            "created" => "Created".into(),
            "removing" => "Removal In Progress".into(),
            other => other.to_string(),
        }
    }

    /// The `Ports` array of `GET /containers/json`.
    pub fn port_list(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let exposed: Vec<String> = self.config["ExposedPorts"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
        for key in &exposed {
            let (port, proto) = split_port(key);
            match self.state.ports.get(key).filter(|b| !b.is_empty() && self.state.running()) {
                Some(bindings) => {
                    for (ip, host_port) in bindings {
                        out.push(json!({"IP": ip, "PrivatePort": port, "PublicPort": host_port.parse::<u16>().unwrap_or(0), "Type": proto}));
                    }
                }
                None => out.push(json!({"PrivatePort": port, "Type": proto})),
            }
        }
        for (key, bindings) in self.state.ports.iter().filter(|(k, _)| !exposed.contains(k)) {
            let (port, proto) = split_port(key);
            for (ip, host_port) in bindings {
                out.push(json!({"IP": ip, "PrivatePort": port, "PublicPort": host_port.parse::<u16>().unwrap_or(0), "Type": proto}));
            }
        }
        out
    }

    pub fn health_api(&self) -> Option<Value> {
        let h = self.state.health.as_ref()?;
        Some(json!({
            "Status": h.status,
            "FailingStreak": h.failing_streak,
            "Log": h.log.iter().map(|l| json!({"Start": l.start, "End": l.end, "ExitCode": l.exit_code, "Output": l.output})).collect::<Vec<_>>(),
        }))
    }

    pub fn state_api(&self) -> Value {
        let s = &self.state;
        let mut v = json!({
            "Status": s.status,
            "Running": s.running() && s.status != "restarting",
            "Paused": s.status == "paused",
            "Restarting": s.status == "restarting",
            "OOMKilled": s.oom_killed,
            "Dead": s.status == "dead",
            "Pid": if s.running() { s.pid } else { 0 },
            "ExitCode": s.exit_code,
            "Error": s.error,
            "StartedAt": s.started_at,
            "FinishedAt": s.finished_at,
        });
        if let Some(h) = self.health_api() {
            v["Health"] = h;
        }
        v
    }

    pub fn ports_api(&self) -> Value {
        let mut ports = serde_json::Map::new();
        if let Some(exposed) = self.config["ExposedPorts"].as_object() {
            for k in exposed.keys() {
                ports.insert(k.clone(), Value::Null);
            }
        }
        if self.state.running() {
            for (k, bindings) in &self.state.ports {
                ports.insert(k.clone(), json!(bindings.iter().map(|(ip, p)| json!({"HostIp": ip, "HostPort": p})).collect::<Vec<_>>()));
            }
        }
        Value::Object(ports)
    }

    pub fn endpoint_api(&self, e: &Endpoint) -> Value {
        let running = self.state.running();
        let mut dns_names = vec![self.name.clone(), self.id[..12].to_string()];
        for a in &e.aliases {
            if !dns_names.contains(a) {
                dns_names.push(a.clone());
            }
        }
        json!({
            "IPAMConfig": e.requested_ip.as_ref().map(|ip| json!({"IPv4Address": ip})),
            "Links": null,
            "Aliases": if e.aliases.is_empty() { Value::Null } else { json!(e.aliases) },
            "MacAddress": if running { e.mac.clone() } else { String::new() },
            "DriverOpts": e.driver_opts.clone().unwrap_or(Value::Null),
            "NetworkID": e.network_id,
            "EndpointID": if running { e.endpoint_id.clone() } else { String::new() },
            "Gateway": if running { e.gateway.clone() } else { String::new() },
            "IPAddress": if running { e.ip.clone().unwrap_or_default() } else { String::new() },
            "IPPrefixLen": if running { e.prefix } else { 0 },
            "IPv6Gateway": "",
            "GlobalIPv6Address": "",
            "GlobalIPv6PrefixLen": 0,
            "DNSNames": if e.network_name == "bridge" { Value::Null } else { json!(dns_names) },
        })
    }

    pub fn networks_api(&self) -> Value {
        let mut nets = serde_json::Map::new();
        for e in &self.endpoints {
            nets.insert(e.network_name.clone(), self.endpoint_api(e));
        }
        Value::Object(nets)
    }

    /// `GET /containers/{id}/json`.
    pub fn inspect(&self, dir: &str) -> Value {
        let (path, args) = self.path_args();
        let primary = self.endpoints.iter().find(|e| e.network_name == "bridge").filter(|_| self.state.running());
        json!({
            "Id": self.id,
            "Created": self.created,
            "Path": path,
            "Args": args,
            "State": self.state_api(),
            "Image": self.image_id,
            "ResolvConfPath": format!("{dir}/resolv.conf"),
            "HostnamePath": format!("{dir}/hostname"),
            "HostsPath": format!("{dir}/hosts"),
            "LogPath": format!("{dir}/log"),
            "Name": format!("/{}", self.name),
            "RestartCount": self.restart_count,
            "Driver": "overlay2",
            "Platform": "linux",
            "MountLabel": "",
            "ProcessLabel": "",
            "AppArmorProfile": "",
            "ExecIDs": null,
            "HostConfig": self.host_config,
            "GraphDriver": {"Name": "overlay2", "Data": {"UpperDir": format!("{dir}/upper"), "WorkDir": format!("{dir}/work"), "MergedDir": format!("{dir}/rootfs")}},
            "Mounts": self.mounts.iter().map(MountPoint::to_api).collect::<Vec<_>>(),
            "Config": self.config,
            "NetworkSettings": {
                "Bridge": "",
                "SandboxID": if self.state.running() { self.id.clone() } else { String::new() },
                "SandboxKey": self.state.netns.clone().unwrap_or_default(),
                "HairpinMode": false,
                "LinkLocalIPv6Address": "",
                "LinkLocalIPv6PrefixLen": 0,
                "Ports": self.ports_api(),
                "SecondaryIPAddresses": null,
                "SecondaryIPv6Addresses": null,
                "EndpointID": primary.map(|e| e.endpoint_id.clone()).unwrap_or_default(),
                "Gateway": primary.map(|e| e.gateway.clone()).unwrap_or_default(),
                "GlobalIPv6Address": "",
                "GlobalIPv6PrefixLen": 0,
                "IPAddress": primary.and_then(|e| e.ip.clone()).unwrap_or_default(),
                "IPPrefixLen": primary.map(|e| e.prefix).unwrap_or(0),
                "IPv6Gateway": "",
                "MacAddress": primary.map(|e| e.mac.clone()).unwrap_or_default(),
                "Networks": self.networks_api(),
            },
        })
    }

    /// One entry of `GET /containers/json`.
    pub fn summary(&self) -> Value {
        let (path, args) = self.path_args();
        let command = std::iter::once(path).chain(args).collect::<Vec<_>>().join(" ");
        let mut nets = serde_json::Map::new();
        for e in &self.endpoints {
            nets.insert(e.network_name.clone(), self.endpoint_api(e));
        }
        json!({
            "Id": self.id,
            "Names": [format!("/{}", self.name)],
            "Image": self.image,
            "ImageID": self.image_id,
            "Command": command,
            "Created": parse_time(&self.created).map(|t| t.timestamp()).unwrap_or(0),
            "Ports": self.port_list(),
            "Labels": self.config["Labels"].clone(),
            "State": self.state.status,
            "Status": self.status_text(),
            "HostConfig": {"NetworkMode": self.host_config["NetworkMode"].as_str().unwrap_or("bridge")},
            "NetworkSettings": {"Networks": Value::Object(nets)},
            "Mounts": self.mounts.iter().map(MountPoint::to_api).collect::<Vec<_>>(),
        })
    }
}

/// "80/tcp" -> (80, "tcp"); a bare "80" is tcp.
pub fn split_port(key: &str) -> (u16, String) {
    let (p, proto) = key.split_once('/').unwrap_or((key, "tcp"));
    (p.parse().unwrap_or(0), proto.to_lowercase())
}

pub fn human_duration(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    match secs {
        0 => "Less than a second".into(),
        1 => "1 second".into(),
        s if s < 60 => format!("{s} seconds"),
        s if s < 120 => "About a minute".into(),
        s if s < 3600 => format!("{} minutes", s / 60),
        s if s < 7200 => "About an hour".into(),
        s if s < 172800 => format!("{} hours", s / 3600),
        s if s < 1209600 => format!("{} days", s / 86400),
        s if s < 5184000 => format!("{} weeks", s / 604800),
        s if s < 63072000 => format!("{} months", s / 2592000),
        s => format!("{} years", s / 31536000),
    }
}

/// A user-defined or predefined network (networks/<id>.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Network {
    pub id: String,
    pub name: String,
    /// `bridge`, `host` or `null`.
    pub driver: String,
    pub created: String,
    #[serde(default)]
    pub subnet: String,
    #[serde(default)]
    pub gateway: String,
    /// The Linux bridge (bridge driver).
    #[serde(default)]
    pub bridge: String,
    #[serde(default)]
    pub internal: bool,
    #[serde(default)]
    pub attachable: bool,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Predefined networks (bridge, host, none) cannot be removed.
    #[serde(default)]
    pub predefined: bool,
}

impl Network {
    pub fn inspect(&self, containers: Value) -> Value {
        let ipam_config = if self.subnet.is_empty() { json!([]) } else { json!([{"Subnet": self.subnet, "Gateway": self.gateway}]) };
        json!({
            "Name": self.name,
            "Id": self.id,
            "Created": self.created,
            "Scope": "local",
            "Driver": self.driver,
            "EnableIPv4": true,
            "EnableIPv6": false,
            "IPAM": {"Driver": "default", "Options": null, "Config": ipam_config},
            "Internal": self.internal,
            "Attachable": self.attachable,
            "Ingress": false,
            "ConfigFrom": {"Network": ""},
            "ConfigOnly": false,
            "Containers": containers,
            "Options": self.options,
            "Labels": self.labels,
        })
    }
}

/// A named or anonymous volume (volumes/<name>/volume.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    pub name: String,
    pub created: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Where the files are: `<dir>/_data`, or a bind (`o=bind,device=...`).
    pub mountpoint: String,
    #[serde(default)]
    pub anonymous: bool,
}

impl Volume {
    pub fn inspect(&self) -> Value {
        json!({
            "Name": self.name,
            "Driver": "local",
            "Mountpoint": self.mountpoint,
            "CreatedAt": self.created,
            "Labels": self.labels,
            "Scope": "local",
            "Options": if self.options.is_empty() { Value::Null } else { json!(self.options) },
        })
    }
}

/// Docker-style random names (`eager_turing`).
pub fn random_name(taken: impl Fn(&str) -> bool) -> String {
    const LEFT: &[&str] = &[
        "admiring", "agitated", "amazing", "bold", "brave", "busy", "charming", "clever", "cool", "dazzling", "eager", "ecstatic",
        "elegant", "epic", "festive", "focused", "friendly", "gallant", "gifted", "happy", "hopeful", "jolly", "keen", "kind", "laughing",
        "lucid", "magical", "modest", "nifty", "nostalgic", "peaceful", "quirky", "relaxed", "serene", "sharp", "stoic", "sweet",
        "tender", "trusting", "upbeat", "vibrant", "wizardly", "youthful", "zealous",
    ];
    const RIGHT: &[&str] = &[
        "albattani", "allen", "archimedes", "babbage", "bardeen", "bell", "bohr", "brattain", "cerf", "curie", "darwin", "dijkstra",
        "einstein", "euler", "fermat", "feynman", "galileo", "gauss", "goldberg", "hamilton", "hopper", "hypatia", "kepler", "knuth",
        "lamport", "lovelace", "mccarthy", "meitner", "newton", "noether", "pascal", "ritchie", "shannon", "thompson", "torvalds",
        "turing", "wozniak", "yalow",
    ];
    for attempt in 0..100 {
        let mut b = [0u8; 2];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut b);
        let mut name = format!("{}_{}", LEFT[b[0] as usize % LEFT.len()], RIGHT[b[1] as usize % RIGHT.len()]);
        if attempt > 10 {
            name.push_str(&(attempt % 10).to_string());
        }
        if !taken(&name) {
            return name;
        }
    }
    format!("container_{}", crate::kube::agent::crun::image::random_hex(4))
}

pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric()) && chars.all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_text_and_ports() {
        let mut r = Record {
            id: "a".repeat(64),
            name: "web".into(),
            created: now_rfc3339(),
            image: "nginx".into(),
            image_id: "sha256:x".into(),
            image_key: String::new(),
            config: json!({"ExposedPorts": {"80/tcp": {}}, "Entrypoint": ["/e"], "Cmd": ["nginx", "-g"]}),
            host_config: json!({}),
            endpoints: vec![],
            mounts: vec![],
            platform: String::new(),
            state: State::default(),
            restart_count: 0,
            manually_stopped: false,
        };
        assert_eq!(r.status_text(), "Created");
        assert_eq!(r.path_args(), ("/e".to_string(), vec!["nginx".to_string(), "-g".to_string()]));
        assert_eq!(r.port_list(), vec![json!({"PrivatePort": 80, "Type": "tcp"})]);
        r.state.status = "running".into();
        r.state.started_at = now_rfc3339();
        r.state.ports.insert("80/tcp".into(), vec![("127.0.0.1".into(), "8080".into())]);
        assert!(r.status_text().starts_with("Up "));
        assert_eq!(r.port_list(), vec![json!({"IP": "127.0.0.1", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"})]);
        assert_eq!(r.ports_api(), json!({"80/tcp": [{"HostIp": "127.0.0.1", "HostPort": "8080"}]}));
        assert_eq!(r.network_mode(), "bridge");
    }

    #[test]
    fn names() {
        assert!(valid_name("my-app_1.web"));
        assert!(!valid_name("-x") && !valid_name("") && !valid_name("a b"));
        let n = random_name(|_| false);
        assert!(n.contains('_'));
        assert_eq!(human_duration(chrono::Duration::seconds(130)), "2 minutes");
    }
}
