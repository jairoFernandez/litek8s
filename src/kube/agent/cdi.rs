//! Devices from Container Device Interface (CDI) specs: GPUs and other
//! accelerators as Kubernetes extended resources, on crun nodes.
//!
//! A vendor tool writes a CDI spec describing the node's devices
//! (`nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml`, AMD's and
//! Intel's equivalents). Each spec has a `kind` (`vendor/class`, e.g.
//! `nvidia.com/gpu`) and named devices, each with the edits a container
//! using it needs: device nodes, mounts, environment, hooks.
//!
//! The agent reads the specs in [`spec_dirs`] at startup and advertises each
//! kind as an extended resource with one unit per device (`all` and other
//! devices that stand for several are not counted). A container asking for
//! `nvidia.com/gpu: 2` in its limits gets two of them, chosen here and kept
//! in `<node dir>/cdi-allocations.json` so an agent restart hands the same
//! ones back; the crun runtime then applies their edits to the container's
//! OCI `config.json` ([`Registry::apply`]), as containerd and CRI-O do.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// Directories searched for CDI specs, later ones overriding earlier ones
/// (the CDI defaults; `LITEK8S_CDI_SPEC_DIRS=a:b` replaces them, empty
/// turns CDI off).
pub fn spec_dirs() -> Vec<PathBuf> {
    match std::env::var("LITEK8S_CDI_SPEC_DIRS") {
        Ok(dirs) => dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from).collect(),
        Err(_) => vec![PathBuf::from("/etc/cdi"), PathBuf::from("/var/run/cdi")],
    }
}

/// One device of a spec.
#[derive(Debug, Clone)]
struct Device {
    name: String,
    edits: Value,
}

/// One CDI spec file.
#[derive(Debug, Clone)]
struct Spec {
    kind: String,
    /// Edits for any device of the spec.
    edits: Value,
    devices: Vec<Device>,
}

/// The CDI specs on this node, by kind.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    specs: BTreeMap<String, Vec<Spec>>,
}

/// `vendor/class`: a vendor that looks like a domain, a class name.
fn valid_kind(kind: &str) -> bool {
    let Some((vendor, class)) = kind.split_once('/') else { return false };
    let label = |s: &str, extra: &[char]| {
        !s.is_empty()
            && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && s.chars().last().is_some_and(|c| c.is_ascii_alphanumeric())
            && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
    };
    vendor.contains('.') && label(vendor, &['-', '.']) && label(class, &['-', '_', '.'])
}

/// Devices that stand for several (`all`) or for none in particular are not
/// counted as units of the resource.
fn countable(name: &str) -> bool {
    name != "all"
}

impl Registry {
    /// Read every `*.yaml`, `*.yml` and `*.json` in `dirs` (missing
    /// directories are fine). A broken file is skipped with a warning, as
    /// the CDI library does; a device defined twice keeps the later one.
    pub fn load(dirs: &[PathBuf]) -> Self {
        let mut reg = Self::default();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml" | "json")))
                .collect();
            files.sort();
            for file in files {
                match Self::read(&file) {
                    Ok(spec) => reg.add(spec),
                    Err(e) => eprintln!("node-agent: skipping CDI spec {}: {e:#}", file.display()),
                }
            }
        }
        reg
    }

    fn read(file: &Path) -> Result<Spec> {
        let text = std::fs::read_to_string(file)?;
        let doc: Value = serde_yaml::from_str(&text).context("not YAML or JSON")?;
        Self::parse(&doc)
    }

    fn parse(doc: &Value) -> Result<Spec> {
        let kind = doc.get("kind").and_then(Value::as_str).unwrap_or_default();
        if !valid_kind(kind) {
            bail!("kind {kind:?} is not vendor/class (e.g. nvidia.com/gpu)");
        }
        let mut devices = Vec::new();
        for d in doc.get("devices").and_then(Value::as_array).into_iter().flatten() {
            let name = d.get("name").and_then(Value::as_str).unwrap_or_default();
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c)) {
                bail!("device name {name:?} is not valid");
            }
            devices.push(Device { name: name.to_string(), edits: d.get("containerEdits").cloned().unwrap_or(Value::Null) });
        }
        if devices.is_empty() {
            bail!("no devices");
        }
        Ok(Spec { kind: kind.to_string(), edits: doc.get("containerEdits").cloned().unwrap_or(Value::Null), devices })
    }

    fn add(&mut self, spec: Spec) {
        let specs = self.specs.entry(spec.kind.clone()).or_default();
        for s in specs.iter_mut() {
            s.devices.retain(|d| !spec.devices.iter().any(|n| n.name == d.name));
        }
        specs.push(spec);
    }

    /// Each kind with its countable devices, sorted by name (numerically
    /// where they are numbers: 0, 1, ..., 10).
    pub fn inventory(&self) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        for (kind, specs) in &self.specs {
            let mut names: Vec<String> = specs.iter().flat_map(|s| &s.devices).map(|d| d.name.clone()).filter(|n| countable(n)).collect();
            names.sort_by(|a, b| match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                _ => a.cmp(b),
            });
            names.dedup();
            if !names.is_empty() {
                out.insert(kind.clone(), names);
            }
        }
        out
    }

    /// The edits for the fully qualified devices (`nvidia.com/gpu=0`): each
    /// spec's own edits once, then each device's.
    fn edits(&self, devices: &[String]) -> Result<Vec<&Value>> {
        let mut out: Vec<&Value> = Vec::new();
        let mut specs_seen: Vec<*const Spec> = Vec::new();
        for qualified in devices {
            let (kind, name) = qualified.split_once('=').with_context(|| format!("CDI device {qualified:?} is not kind=name"))?;
            let specs = self.specs.get(kind).with_context(|| format!("no CDI spec for {kind} on this node"))?;
            let (spec, device) = specs
                .iter()
                .find_map(|s| s.devices.iter().find(|d| d.name == name).map(|d| (s, d)))
                .with_context(|| format!("CDI device {qualified} is not in this node's specs"))?;
            if !specs_seen.contains(&(spec as *const Spec)) {
                specs_seen.push(spec);
                out.push(&spec.edits);
            }
            out.push(&device.edits);
        }
        Ok(out)
    }

    /// Apply the devices' edits to an OCI `config.json`.
    pub fn apply(&self, config: &mut Value, devices: &[String]) -> Result<()> {
        for edits in self.edits(devices)? {
            apply_edits(config, edits)?;
        }
        Ok(())
    }
}

fn obj(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = json!({});
    }
    v.as_object_mut().unwrap()
}

fn arr<'a>(parent: &'a mut Value, key: &str) -> &'a mut Vec<Value> {
    let slot = obj(parent).entry(key.to_string()).or_insert_with(|| json!([]));
    if !slot.is_array() {
        *slot = json!([]);
    }
    slot.as_array_mut().unwrap()
}

/// Type, major and minor of a device node, from the spec or the host file.
fn device_numbers(node: &Value, host_path: &str) -> Result<(String, i64, i64)> {
    let ty = node.get("type").and_then(Value::as_str).map(str::to_string);
    let major = node.get("major").and_then(Value::as_i64);
    let minor = node.get("minor").and_then(Value::as_i64);
    if let (Some(ty), Some(major), Some(minor)) = (ty.clone(), major, minor) {
        return Ok((ty, major, minor));
    }
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let meta = std::fs::metadata(host_path).with_context(|| format!("CDI device node {host_path}"))?;
    let ft = meta.file_type();
    let found = if ft.is_char_device() {
        "c"
    } else if ft.is_block_device() {
        "b"
    } else {
        bail!("CDI device node {host_path} is not a device");
    };
    let rdev = meta.rdev();
    let host_major = ((rdev >> 8) & 0xfff | (rdev >> 32) & !0xfff) as i64;
    let host_minor = (rdev & 0xff | (rdev >> 12) & !0xff) as i64;
    Ok((ty.unwrap_or_else(|| found.to_string()), major.unwrap_or(host_major), minor.unwrap_or(host_minor)))
}

/// One CDI `containerEdits` object onto the OCI config: environment (a
/// later value replaces an earlier one), device nodes (also allowed in the
/// device cgroup), mounts, hooks and additional groups.
fn apply_edits(config: &mut Value, edits: &Value) -> Result<()> {
    if edits.is_null() {
        return Ok(());
    }
    for kv in edits.get("env").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        let key = kv.split_once('=').map_or(kv, |(k, _)| k);
        let env = arr(&mut config["process"], "env");
        env.retain(|e| e.as_str().is_none_or(|e| e.split_once('=').map_or(e, |(k, _)| k) != key));
        env.push(json!(kv));
    }
    for node in edits.get("deviceNodes").and_then(Value::as_array).into_iter().flatten() {
        let path = node.get("path").and_then(Value::as_str).filter(|p| p.starts_with('/')).context("CDI deviceNode without an absolute path")?;
        let host_path = node.get("hostPath").and_then(Value::as_str).unwrap_or(path);
        let (ty, major, minor) = device_numbers(node, host_path)?;
        let mut dev = json!({"path": path, "type": ty, "major": major, "minor": minor});
        for field in ["fileMode", "uid", "gid"] {
            if let Some(v) = node.get(field).filter(|v| !v.is_null()) {
                dev[field] = v.clone();
            }
        }
        let devices = arr(&mut config["linux"], "devices");
        devices.retain(|d| d.get("path").and_then(Value::as_str) != Some(path));
        devices.push(dev);
        let access = node.get("permissions").and_then(Value::as_str).filter(|p| !p.is_empty()).unwrap_or("rwm");
        arr(&mut config["linux"]["resources"], "devices").push(json!({"allow": true, "type": ty, "major": major, "minor": minor, "access": access}));
    }
    for m in edits.get("mounts").and_then(Value::as_array).into_iter().flatten() {
        let source = m.get("hostPath").and_then(Value::as_str).context("CDI mount without hostPath")?;
        let destination = m.get("containerPath").and_then(Value::as_str).filter(|p| p.starts_with('/')).context("CDI mount without an absolute containerPath")?;
        let options: Vec<Value> = match m.get("options").and_then(Value::as_array) {
            Some(o) if !o.is_empty() => o.clone(),
            _ => vec![json!("rbind"), json!("rprivate")],
        };
        let ty = m.get("type").and_then(Value::as_str).unwrap_or("bind");
        let mounts = arr(config, "mounts");
        mounts.retain(|x| x.get("destination").and_then(Value::as_str) != Some(destination));
        mounts.push(json!({"destination": destination, "type": ty, "source": source, "options": options}));
    }
    for h in edits.get("hooks").and_then(Value::as_array).into_iter().flatten() {
        let name = h.get("hookName").and_then(Value::as_str).context("CDI hook without hookName")?;
        if !matches!(name, "prestart" | "createRuntime" | "createContainer" | "startContainer" | "poststart" | "poststop") {
            bail!("CDI hook {name:?} is not an OCI hook");
        }
        let mut hook = json!({"path": h.get("path").and_then(Value::as_str).context("CDI hook without path")?});
        for field in ["args", "env", "timeout"] {
            if let Some(v) = h.get(field).filter(|v| !v.is_null()) {
                hook[field] = v.clone();
            }
        }
        arr(&mut config["hooks"], name).push(hook);
    }
    for gid in edits.get("additionalGIDs").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64) {
        let gids = arr(&mut config["process"]["user"], "additionalGids");
        if !gids.iter().any(|g| g.as_u64() == Some(gid)) {
            gids.push(json!(gid));
        }
    }
    Ok(())
}

/// pod uid → kind → devices.
type Allocations = HashMap<String, BTreeMap<String, Vec<String>>>;

/// The node's CDI devices and which pod holds which.
pub struct Devices {
    inventory: BTreeMap<String, Vec<String>>,
    file: Option<PathBuf>,
    held: Mutex<Allocations>,
}

impl Default for Devices {
    fn default() -> Self {
        Self::none()
    }
}

impl Devices {
    /// No CDI devices (Docker nodes, CDI turned off).
    pub fn none() -> Self {
        Self { inventory: BTreeMap::new(), file: None, held: Mutex::default() }
    }

    /// The devices of `registry`, with the allocations kept in `file`
    /// (dropping devices that are gone).
    pub fn new(registry: &Registry, file: PathBuf) -> Self {
        let inventory = registry.inventory();
        let mut held: Allocations = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        for kinds in held.values_mut() {
            kinds.retain(|kind, names| {
                let have = inventory.get(kind);
                names.retain(|n| have.is_some_and(|h| h.contains(n)));
                !names.is_empty()
            });
        }
        held.retain(|_, kinds| !kinds.is_empty());
        Self { inventory, file: Some(file), held: Mutex::new(held) }
    }

    /// The extended resources to advertise: kind → number of devices.
    pub fn capacity(&self) -> BTreeMap<String, usize> {
        self.inventory.iter().map(|(k, v)| (k.clone(), v.len())).collect()
    }

    /// Whether `resource` is one of this node's CDI kinds.
    pub fn provides(&self, resource: &str) -> bool {
        self.inventory.contains_key(resource)
    }

    /// `count` devices of `kind` for pod `uid`: the ones it already holds,
    /// or free ones. Pods for which `live` says false give theirs back first
    /// (a pod deleted while the agent was down).
    pub fn allocate(&self, uid: &str, kind: &str, count: usize, live: &dyn Fn(&str) -> bool) -> Result<Vec<String>> {
        let Some(all) = self.inventory.get(kind) else { bail!("this node has no {kind} (no CDI spec for it)") };
        let mut held = self.held.lock().unwrap();
        if let Some(names) = held.get(uid).and_then(|k| k.get(kind)).filter(|n| n.len() >= count) {
            return Ok(names[..count].to_vec());
        }
        held.retain(|u, _| u == uid || live(u));
        let mut mine = held.get(uid).and_then(|k| k.get(kind)).cloned().unwrap_or_default();
        let taken: Vec<&String> = held.iter().filter(|(u, _)| *u != uid).filter_map(|(_, k)| k.get(kind)).flatten().collect();
        for name in all {
            if mine.len() >= count {
                break;
            }
            if !taken.contains(&name) && !mine.contains(name) {
                mine.push(name.clone());
            }
        }
        if mine.len() < count {
            bail!("Insufficient {kind}: {count} requested, {} free on this node", all.len() - taken.len());
        }
        held.entry(uid.to_string()).or_default().insert(kind.to_string(), mine.clone());
        self.save(&held);
        Ok(mine)
    }

    /// Give back the pod's devices.
    pub fn release(&self, uid: &str) {
        let mut held = self.held.lock().unwrap();
        if held.remove(uid).is_some() {
            self.save(&held);
        }
    }

    fn save(&self, held: &Allocations) {
        let Some(file) = &self.file else { return };
        let tmp = file.with_extension("json.tmp");
        let ok = serde_json::to_vec(held).ok().is_some_and(|b| std::fs::write(&tmp, b).is_ok()) && std::fs::rename(&tmp, file).is_ok();
        if !ok {
            eprintln!("node-agent: cannot save {}", file.display());
        }
    }
}

/// What a pod's containers ask of the node's CDI kinds (limits, as for any
/// extended resource): for each container, kind → count.
pub fn container_requests(spec: &Value, provides: &dyn Fn(&str) -> bool) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for (res, q) in spec.pointer("/resources/limits").and_then(Value::as_object).into_iter().flatten() {
        if provides(res) {
            let n = crate::kube::quantity::milli_of(q).unwrap_or(0) / 1000;
            if n > 0 {
                out.insert(res.clone(), n as usize);
            }
        }
    }
    out
}

/// How many devices of each kind the pod holds: the app containers' sum, or
/// the largest init container's when that is more (init containers run one
/// at a time, before the others, and reuse the pod's devices).
pub fn pod_totals(init: &[BTreeMap<String, usize>], main: &[BTreeMap<String, usize>]) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for c in main {
        for (k, n) in c {
            *out.entry(k.clone()).or_default() += n;
        }
    }
    for c in init {
        for (k, n) in c {
            let e = out.entry(k.clone()).or_default();
            *e = (*e).max(*n);
        }
    }
    out
}

/// The pod's devices of each kind split among its containers: app container
/// `i` gets the slice after those of the containers before it, an init
/// container the first ones. Fully qualified (`kind=name`).
pub fn container_devices(pod_devices: &BTreeMap<String, Vec<String>>, main: &[BTreeMap<String, usize>], init: bool, i: usize, mine: &BTreeMap<String, usize>) -> Vec<String> {
    let mut out = Vec::new();
    for (kind, n) in mine {
        let Some(names) = pod_devices.get(kind) else { continue };
        let offset: usize = if init { 0 } else { main[..i].iter().filter_map(|c| c.get(kind)).sum() };
        out.extend(names.iter().skip(offset).take(*n).map(|name| format!("{kind}={name}")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVIDIA: &str = r#"
cdiVersion: 0.6.0
kind: nvidia.com/gpu
containerEdits:
  env:
  - NVIDIA_VISIBLE_DEVICES=void
  deviceNodes:
  - path: /dev/nvidiactl
    type: c
    major: 195
    minor: 255
  hooks:
  - hookName: createContainer
    path: /usr/bin/nvidia-ctk
    args: [nvidia-ctk, hook, update-ldcache, --folder, /usr/lib/x86_64-linux-gnu]
  mounts:
  - hostPath: /usr/lib/x86_64-linux-gnu/libcuda.so.550.54.15
    containerPath: /usr/lib/x86_64-linux-gnu/libcuda.so.550.54.15
    options: [ro, nosuid, nodev, bind]
devices:
- name: "0"
  containerEdits:
    deviceNodes:
    - path: /dev/nvidia0
      type: c
      major: 195
      minor: 0
- name: "1"
  containerEdits:
    deviceNodes:
    - path: /dev/nvidia1
      type: c
      major: 195
      minor: 1
- name: all
  containerEdits:
    deviceNodes:
    - {path: /dev/nvidia0, type: c, major: 195, minor: 0}
    - {path: /dev/nvidia1, type: c, major: 195, minor: 1}
"#;

    fn registry(docs: &[&str]) -> Registry {
        let mut reg = Registry::default();
        for d in docs {
            reg.add(Registry::parse(&serde_yaml::from_str(d).unwrap()).unwrap());
        }
        reg
    }

    #[test]
    fn inventory_counts_named_devices_but_all() {
        let reg = registry(&[NVIDIA]);
        assert_eq!(reg.inventory(), BTreeMap::from([("nvidia.com/gpu".to_string(), vec!["0".to_string(), "1".to_string()])]));
        let ten: Vec<String> = (0..11).map(|i| format!("- name: \"{i}\"")).collect();
        let reg = registry(&[&format!("kind: amd.com/gpu\ndevices:\n{}", ten.join("\n"))]);
        assert_eq!(reg.inventory()["amd.com/gpu"].last().unwrap(), "10");
    }

    #[test]
    fn bad_specs_are_refused() {
        for doc in ["kind: gpu\ndevices: [{name: a}]", "kind: nvidia.com/gpu\ndevices: []", "kind: nvidia.com/gpu\ndevices: [{name: 'a/b'}]"] {
            assert!(Registry::parse(&serde_yaml::from_str(doc).unwrap()).is_err(), "{doc}");
        }
        assert!(valid_kind("nvidia.com/gpu") && valid_kind("vendor.example/fpga_x") && !valid_kind("nvidia/gpu") && !valid_kind("nvidia.com/"));
    }

    #[test]
    fn later_spec_overrides_a_device() {
        let reg = registry(&[NVIDIA, "kind: nvidia.com/gpu\ndevices:\n- name: \"1\"\n  containerEdits: {env: [WHICH=new]}"]);
        let mut config = json!({"process": {"env": []}});
        reg.apply(&mut config, &["nvidia.com/gpu=1".into()]).unwrap();
        assert_eq!(config["process"]["env"], json!(["WHICH=new"]));
    }

    #[test]
    fn edits_land_in_the_oci_config() {
        let reg = registry(&[NVIDIA]);
        let mut config = json!({
            "process": {"env": ["PATH=/bin", "NVIDIA_VISIBLE_DEVICES=all"], "user": {"uid": 0, "gid": 0}},
            "mounts": [{"destination": "/proc", "type": "proc", "source": "proc"}],
            "linux": {"resources": {"devices": [{"allow": false, "access": "rwm"}]}},
        });
        reg.apply(&mut config, &["nvidia.com/gpu=1".into()]).unwrap();
        assert_eq!(config["process"]["env"], json!(["PATH=/bin", "NVIDIA_VISIBLE_DEVICES=void"]));
        assert_eq!(
            config["linux"]["devices"],
            json!([
                {"path": "/dev/nvidiactl", "type": "c", "major": 195, "minor": 255},
                {"path": "/dev/nvidia1", "type": "c", "major": 195, "minor": 1},
            ])
        );
        // The deny-all rule stays first; the devices are allowed after it.
        let cgroup = config["linux"]["resources"]["devices"].as_array().unwrap();
        assert_eq!(cgroup[0], json!({"allow": false, "access": "rwm"}));
        assert_eq!(cgroup[2], json!({"allow": true, "type": "c", "major": 195, "minor": 1, "access": "rwm"}));
        assert_eq!(config["mounts"][1]["options"], json!(["ro", "nosuid", "nodev", "bind"]));
        assert_eq!(config["hooks"]["createContainer"][0]["path"], "/usr/bin/nvidia-ctk");
        assert!(reg.apply(&mut config, &["nvidia.com/gpu=7".into()]).is_err());
        assert!(reg.apply(&mut config, &["amd.com/gpu=0".into()]).is_err());
    }

    #[test]
    fn spec_edits_apply_once_for_several_devices() {
        let reg = registry(&[NVIDIA]);
        let mut config = json!({"process": {"env": []}});
        reg.apply(&mut config, &["nvidia.com/gpu=0".into(), "nvidia.com/gpu=1".into()]).unwrap();
        assert_eq!(config["hooks"]["createContainer"].as_array().unwrap().len(), 1);
        assert_eq!(config["linux"]["devices"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn allocation_is_sticky_exclusive_and_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cdi-allocations.json");
        let reg = registry(&[NVIDIA]);
        let devices = Devices::new(&reg, file.clone());
        assert_eq!(devices.capacity(), BTreeMap::from([("nvidia.com/gpu".to_string(), 2)]));
        let live = |_: &str| true;
        assert_eq!(devices.allocate("a", "nvidia.com/gpu", 1, &live).unwrap(), ["0"]);
        assert_eq!(devices.allocate("a", "nvidia.com/gpu", 1, &live).unwrap(), ["0"]);
        assert_eq!(devices.allocate("b", "nvidia.com/gpu", 1, &live).unwrap(), ["1"]);
        let err = devices.allocate("c", "nvidia.com/gpu", 1, &live).unwrap_err().to_string();
        assert!(err.contains("Insufficient nvidia.com/gpu"), "{err}");
        // An agent restart hands back the same devices.
        let again = Devices::new(&reg, file.clone());
        assert_eq!(again.allocate("b", "nvidia.com/gpu", 1, &live).unwrap(), ["1"]);
        // A pod that is gone gives its device up to one that needs it.
        assert_eq!(again.allocate("c", "nvidia.com/gpu", 1, &|u| u != "a").unwrap(), ["0"]);
        again.release("b");
        assert_eq!(again.allocate("d", "nvidia.com/gpu", 1, &live).unwrap(), ["1"]);
        assert!(again.allocate("e", "amd.com/gpu", 1, &live).is_err());
    }

    #[test]
    fn containers_split_the_pods_devices() {
        let spec = json!({"resources": {"limits": {"nvidia.com/gpu": "2", "cpu": "1", "example.com/foo": "1"}}});
        let provides = |r: &str| r == "nvidia.com/gpu";
        let one = container_requests(&spec, &provides);
        assert_eq!(one, BTreeMap::from([("nvidia.com/gpu".to_string(), 2)]));
        let main = vec![one.clone(), BTreeMap::from([("nvidia.com/gpu".to_string(), 1)])];
        let init = vec![BTreeMap::from([("nvidia.com/gpu".to_string(), 1)])];
        assert_eq!(pod_totals(&init, &main)["nvidia.com/gpu"], 3);
        let held = BTreeMap::from([("nvidia.com/gpu".to_string(), vec!["0".to_string(), "1".to_string(), "2".to_string()])]);
        assert_eq!(container_devices(&held, &main, false, 0, &main[0]), ["nvidia.com/gpu=0", "nvidia.com/gpu=1"]);
        assert_eq!(container_devices(&held, &main, false, 1, &main[1]), ["nvidia.com/gpu=2"]);
        assert_eq!(container_devices(&held, &main, true, 0, &init[0]), ["nvidia.com/gpu=0"]);
    }

    #[test]
    fn loads_files_and_skips_broken_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nvidia.yaml"), NVIDIA).unwrap();
        std::fs::write(dir.path().join("broken.json"), "{").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "kind: x").unwrap();
        let reg = Registry::load(&[dir.path().to_path_buf(), dir.path().join("missing")]);
        assert_eq!(reg.inventory().len(), 1);
    }
}
