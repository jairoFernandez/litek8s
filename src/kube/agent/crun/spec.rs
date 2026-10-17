//! The OCI runtime bundle (`config.json`) for a pod container: the process
//! (argv, env, user, capabilities), its mounts, the pod's shared namespaces
//! and the cgroup v2 limits. Defaults follow Docker's, so a pod behaves the
//! same on either backend: the same capability set, masked `/proc` paths,
//! device allow-list and a seccomp filter that blocks what Docker's default
//! profile blocks.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::super::runtime::ContainerSpec;

/// What the image's config says about how to run it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ImageConfig {
    pub env: Vec<String>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
}

impl ImageConfig {
    /// From an image config blob (the `config` object of the OCI image config).
    pub fn from_blob(blob: &Value) -> Self {
        let config = blob.get("config").cloned().unwrap_or(Value::Null);
        let strings = |field: &str| -> Option<Vec<String>> {
            config
                .get(field)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        };
        let string = |field: &str| config.get(field).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        Self {
            env: strings("Env").unwrap_or_default(),
            entrypoint: strings("Entrypoint"),
            cmd: strings("Cmd"),
            working_dir: string("WorkingDir"),
            user: string("User"),
        }
    }
}

/// The user a process runs as, resolved against the image's `/etc/passwd`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct User {
    pub uid: u32,
    pub gid: u32,
    pub additional_gids: Vec<u32>,
    pub home: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MountKind {
    Bind,
    Tmpfs,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedMount {
    pub source: String,
    pub target: String,
    pub kind: MountKind,
    pub read_only: bool,
}

pub(crate) struct BundleParams<'a> {
    pub spec: &'a ContainerSpec,
    pub image: &'a ImageConfig,
    pub user: User,
    pub hostname: &'a str,
    /// The pod's network namespace (a bind-mounted nsfs file); `None` shares
    /// the host's.
    pub netns: Option<String>,
    /// The pod's IPC namespace, shared by its containers.
    pub ipcns: Option<String>,
    /// Whether the container gets a UTS namespace of its own (not with
    /// `hostNetwork`).
    pub own_uts: bool,
    /// (target, source) of the pod's `/etc/hosts`, `/etc/resolv.conf`, ...
    pub etc_files: Vec<(String, String)>,
    /// The pod's shared `/dev/shm` directory.
    pub shm: Option<String>,
    pub mounts: Vec<ResolvedMount>,
    pub cgroups_path: String,
}

/// Docker's default capability set (also containerd's for CRI).
pub(crate) const DEFAULT_CAPS: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FSETID",
    "CAP_FOWNER",
    "CAP_MKNOD",
    "CAP_NET_RAW",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETFCAP",
    "CAP_SETPCAP",
    "CAP_NET_BIND_SERVICE",
    "CAP_SYS_CHROOT",
    "CAP_KILL",
    "CAP_AUDIT_WRITE",
];

/// Every capability up to CAP_CHECKPOINT_RESTORE (Linux 5.9), for
/// privileged containers and `add: [ALL]`.
const ALL_CAPS: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETPCAP",
    "CAP_LINUX_IMMUTABLE",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_ADMIN",
    "CAP_NET_RAW",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_SYS_MODULE",
    "CAP_SYS_RAWIO",
    "CAP_SYS_CHROOT",
    "CAP_SYS_PTRACE",
    "CAP_SYS_PACCT",
    "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT",
    "CAP_SYS_NICE",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TIME",
    "CAP_SYS_TTY_CONFIG",
    "CAP_MKNOD",
    "CAP_LEASE",
    "CAP_AUDIT_WRITE",
    "CAP_AUDIT_CONTROL",
    "CAP_SETFCAP",
    "CAP_MAC_OVERRIDE",
    "CAP_MAC_ADMIN",
    "CAP_SYSLOG",
    "CAP_WAKE_ALARM",
    "CAP_BLOCK_SUSPEND",
    "CAP_AUDIT_READ",
    "CAP_PERFMON",
    "CAP_BPF",
    "CAP_CHECKPOINT_RESTORE",
];

const MASKED_PATHS: &[&str] = &[
    "/proc/asound",
    "/proc/acpi",
    "/proc/interrupts",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/sys/firmware",
    "/sys/devices/virtual/powercap",
];

const READONLY_PATHS: &[&str] = &["/proc/bus", "/proc/fs", "/proc/irq", "/proc/sys", "/proc/sysrq-trigger"];

/// Syscalls Docker's default seccomp profile leaves out (it is an allow
/// list; this is the same boundary written as a deny list, so new harmless
/// syscalls keep working). Names the kernel or libseccomp does not know on
/// this architecture are skipped by crun.
const SECCOMP_DENY: &[&str] = &[
    "acct",
    "add_key",
    "bpf",
    "clock_adjtime",
    "clock_settime",
    "create_module",
    "delete_module",
    "finit_module",
    "fsconfig",
    "fsmount",
    "fsopen",
    "fspick",
    "get_kernel_syms",
    "get_mempolicy",
    "init_module",
    "io_uring_enter",
    "io_uring_register",
    "io_uring_setup",
    "ioperm",
    "iopl",
    "kexec_file_load",
    "kexec_load",
    "keyctl",
    "lookup_dcookie",
    "mbind",
    "mount",
    "mount_setattr",
    "move_mount",
    "move_pages",
    "name_to_handle_at",
    "nfsservctl",
    "open_by_handle_at",
    "open_tree",
    "perf_event_open",
    "pivot_root",
    "query_module",
    "quotactl",
    "reboot",
    "request_key",
    "set_mempolicy",
    "setns",
    "settimeofday",
    "swapoff",
    "swapon",
    "sysfs",
    "_sysctl",
    "umount",
    "umount2",
    "unshare",
    "uselib",
    "userfaultfd",
    "ustat",
    "vm86",
    "vm86old",
];

/// argv: Kubernetes `command` replaces the image's entrypoint (and then the
/// image's `Cmd` is dropped too), `args` replaces its `Cmd`.
pub(crate) fn process_args(spec: &ContainerSpec, image: &ImageConfig) -> Result<Vec<String>> {
    let mut argv = Vec::new();
    match &spec.entrypoint {
        Some(entrypoint) => {
            argv.extend(entrypoint.iter().cloned());
            argv.extend(spec.cmd.iter().flatten().cloned());
        }
        None => {
            argv.extend(image.entrypoint.iter().flatten().cloned());
            argv.extend(spec.cmd.as_ref().or(image.cmd.as_ref()).into_iter().flatten().cloned());
        }
    }
    if argv.is_empty() {
        bail!("no command specified");
    }
    Ok(argv)
}

/// Image env, then the container's (which wins), plus what Docker adds:
/// `PATH` if neither has one, `HOSTNAME`, and `HOME` from the passwd entry.
pub(crate) fn environment(spec: &ContainerSpec, image: &ImageConfig, hostname: &str, home: &str) -> Vec<String> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut set = |kv: &str| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        match env.iter_mut().find(|(name, _)| name == k) {
            Some(slot) => slot.1 = v.to_string(),
            None => env.push((k.to_string(), v.to_string())),
        }
    };
    set("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    set(&format!("HOSTNAME={hostname}"));
    for kv in &image.env {
        set(kv);
    }
    for kv in &spec.env {
        set(kv);
    }
    if !env.iter().any(|(k, _)| k == "HOME") {
        env.push(("HOME".into(), home.to_string()));
    }
    env.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// Resolve `user` (`name`, `uid`, `name:group`, `uid:gid`; empty is root)
/// against the image's passwd and group files, as Docker does: a numeric
/// uid not in passwd runs with gid 0, and the user's supplementary groups
/// from `/etc/group` are added.
pub(crate) fn resolve_user(user: &str, passwd: &str, group: &str) -> Result<User> {
    let (user_part, group_part) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    let user_part = if user_part.is_empty() { "0" } else { user_part };
    let passwd_entries: Vec<Vec<&str>> = passwd
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split(':').collect::<Vec<_>>())
        .filter(|f| f.len() >= 6)
        .collect();
    let group_entries: Vec<Vec<&str>> = group
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split(':').collect::<Vec<_>>())
        .filter(|f| f.len() >= 3)
        .collect();
    let entry = match user_part.parse::<u32>() {
        Ok(uid) => passwd_entries.iter().find(|f| f[2].parse::<u32>().ok() == Some(uid)),
        Err(_) => passwd_entries.iter().find(|f| f[0] == user_part),
    };
    let (uid, mut gid, name, home) = match (user_part.parse::<u32>(), entry) {
        (Ok(uid), Some(e)) => (uid, e[3].parse().unwrap_or(0), e[0].to_string(), e[5].to_string()),
        (Ok(uid), None) => (uid, 0, String::new(), "/".to_string()),
        (Err(_), Some(e)) => (
            e[2].parse().context("invalid uid in /etc/passwd")?,
            e[3].parse().unwrap_or(0),
            e[0].to_string(),
            e[5].to_string(),
        ),
        (Err(_), None) => bail!("unable to find user {user_part}: no matching entries in passwd file"),
    };
    if let Some(g) = group_part.filter(|g| !g.is_empty()) {
        gid = match g.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => group_entries
                .iter()
                .find(|f| f[0] == g)
                .and_then(|f| f[2].parse().ok())
                .with_context(|| format!("unable to find group {g}: no matching entries in group file"))?,
        };
    }
    // Every group that lists the user, the primary one included (Docker's
    // `id` in an image shows the same list).
    let mut additional_gids = Vec::new();
    if !name.is_empty() && group_part.is_none() {
        for f in &group_entries {
            let members = f.get(3).copied().unwrap_or_default();
            if members.split(',').any(|m| m == name) {
                if let Ok(g) = f[2].parse::<u32>() {
                    if !additional_gids.contains(&g) {
                        additional_gids.push(g);
                    }
                }
            }
        }
    }
    Ok(User { uid, gid, additional_gids, home: if home.is_empty() { "/".into() } else { home } })
}

fn capabilities(spec: &ContainerSpec) -> Vec<String> {
    if spec.privileged {
        return ALL_CAPS.iter().map(|c| c.to_string()).collect();
    }
    let norm = |c: &str| {
        let c = c.to_uppercase();
        if c.starts_with("CAP_") { c } else { format!("CAP_{c}") }
    };
    let mut caps: Vec<String> = DEFAULT_CAPS.iter().map(|c| c.to_string()).collect();
    if spec.cap_add.iter().any(|c| c.eq_ignore_ascii_case("ALL")) {
        caps = ALL_CAPS.iter().map(|c| c.to_string()).collect();
    } else {
        for c in &spec.cap_add {
            let c = norm(c);
            if ALL_CAPS.contains(&c.as_str()) && !caps.contains(&c) {
                caps.push(c);
            }
        }
    }
    if spec.cap_drop.iter().any(|c| c.eq_ignore_ascii_case("ALL")) {
        let added: Vec<String> = spec.cap_add.iter().map(|c| norm(c)).collect();
        caps.retain(|c| added.contains(c));
    } else {
        let dropped: Vec<String> = spec.cap_drop.iter().map(|c| norm(c)).collect();
        caps.retain(|c| !dropped.contains(c));
    }
    caps
}

fn devices(privileged: bool) -> Value {
    if privileged {
        return json!([{"allow": true, "access": "rwm"}]);
    }
    let dev = |kind: &str, major: Option<i64>, minor: Option<i64>, access: &str| {
        let mut d = json!({"allow": true, "type": kind, "access": access});
        if let Some(m) = major {
            d["major"] = json!(m);
        }
        if let Some(m) = minor {
            d["minor"] = json!(m);
        }
        d
    };
    json!([
        {"allow": false, "access": "rwm"},
        dev("c", None, None, "m"),
        dev("b", None, None, "m"),
        dev("c", Some(1), Some(3), "rwm"),
        dev("c", Some(1), Some(8), "rwm"),
        dev("c", Some(1), Some(7), "rwm"),
        dev("c", Some(5), Some(0), "rwm"),
        dev("c", Some(1), Some(5), "rwm"),
        dev("c", Some(1), Some(9), "rwm"),
        dev("c", Some(136), None, "rwm"),
        dev("c", Some(5), Some(2), "rwm"),
        dev("c", Some(10), Some(200), "rwm"),
    ])
}

fn seccomp() -> Value {
    json!({
        "defaultAction": "SCMP_ACT_ALLOW",
        "syscalls": [{"names": SECCOMP_DENY, "action": "SCMP_ACT_ERRNO", "errnoRet": 1}],
    })
}

/// cgroup v2 limits: memory (no swap), CPU quota per 100 ms period from the
/// limit and weight from the request (as `cpu.shares`, which crun converts).
fn resources(spec: &ContainerSpec) -> Value {
    let mut r = json!({"devices": devices(spec.privileged)});
    if let Some(limit) = spec.memory_limit.filter(|m| *m > 0) {
        r["memory"] = json!({"limit": limit, "swap": limit});
    }
    let mut cpu = serde_json::Map::new();
    if let Some(nano) = spec.nano_cpus.filter(|n| *n > 0) {
        let period: i64 = 100_000;
        cpu.insert("period".into(), json!(period));
        cpu.insert("quota".into(), json!((nano * period / 1_000_000_000).max(1000)));
    }
    if let Some(milli) = spec.cpu_request_milli.filter(|m| *m > 0) {
        cpu.insert("shares".into(), json!((milli * 1024 / 1000).max(2)));
    }
    if !cpu.is_empty() {
        r["cpu"] = Value::Object(cpu);
    }
    r
}

/// Mount entries: the standard Linux set, the pod's files, then the
/// container's volumes (parents before children; a volume replaces a default
/// at the same path).
fn mounts(p: &BundleParams) -> Vec<Value> {
    let ro_sys = if p.spec.privileged { "rw" } else { "ro" };
    let mut defaults: Vec<(String, Value)> = vec![
        ("/proc".into(), json!({"destination": "/proc", "type": "proc", "source": "proc", "options": ["nosuid", "noexec", "nodev"]})),
        ("/dev".into(), json!({"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]})),
        ("/dev/pts".into(), json!({"destination": "/dev/pts", "type": "devpts", "source": "devpts", "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620", "gid=5"]})),
        ("/dev/mqueue".into(), json!({"destination": "/dev/mqueue", "type": "mqueue", "source": "mqueue", "options": ["nosuid", "noexec", "nodev"]})),
        ("/sys".into(), json!({"destination": "/sys", "type": "sysfs", "source": "sysfs", "options": ["nosuid", "noexec", "nodev", ro_sys]})),
        ("/sys/fs/cgroup".into(), json!({"destination": "/sys/fs/cgroup", "type": "cgroup", "source": "cgroup", "options": ["nosuid", "noexec", "nodev", "relatime", ro_sys]})),
    ];
    match &p.shm {
        Some(shm) => defaults.push(("/dev/shm".into(), json!({"destination": "/dev/shm", "type": "bind", "source": shm, "options": ["rbind", "nosuid", "noexec", "nodev"]}))),
        None => defaults.push(("/dev/shm".into(), json!({"destination": "/dev/shm", "type": "tmpfs", "source": "shm", "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]}))),
    }
    for (target, source) in &p.etc_files {
        defaults.push((target.clone(), json!({"destination": target, "type": "bind", "source": source, "options": ["rbind", "rprivate"]})));
    }
    let user_targets: Vec<String> = p.mounts.iter().map(|m| clean_target(&m.target)).collect();
    let mut out: Vec<Value> = defaults
        .into_iter()
        .filter(|(target, _)| !user_targets.contains(target))
        .map(|(_, m)| m)
        .collect();
    let mut user: Vec<&ResolvedMount> = p.mounts.iter().collect();
    user.sort_by_key(|m| clean_target(&m.target).split('/').filter(|s| !s.is_empty()).count());
    for m in user {
        let target = clean_target(&m.target);
        let mount = match m.kind {
            MountKind::Bind => {
                let mut options = vec!["rbind", "rprivate"];
                if m.read_only {
                    options.push("ro");
                }
                json!({"destination": target, "type": "bind", "source": m.source, "options": options})
            }
            MountKind::Tmpfs => {
                let mut options = vec!["nosuid", "nodev", "mode=1777"];
                if m.read_only {
                    options.push("ro");
                }
                json!({"destination": target, "type": "tmpfs", "source": "tmpfs", "options": options})
            }
        };
        out.push(mount);
    }
    out
}

fn clean_target(t: &str) -> String {
    let joined: Vec<&str> = t.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    format!("/{}", joined.join("/"))
}

/// The bundle's `config.json`.
pub(crate) fn config(p: &BundleParams) -> Result<Value> {
    let spec = p.spec;
    let args = process_args(spec, p.image)?;
    let caps = capabilities(spec);
    let cwd = spec
        .working_dir
        .clone()
        .or_else(|| p.image.working_dir.clone())
        .filter(|w| w.starts_with('/'))
        .unwrap_or_else(|| "/".into());
    let mut process = json!({
        "terminal": spec.tty,
        "user": {"uid": p.user.uid, "gid": p.user.gid},
        "args": args,
        "env": environment(spec, p.image, p.hostname, &p.user.home),
        "cwd": cwd,
        "capabilities": {"bounding": caps, "effective": caps, "permitted": caps},
        "rlimits": [{"type": "RLIMIT_NOFILE", "hard": 1048576u64, "soft": 1048576u64}],
        "noNewPrivileges": spec.no_new_privileges && !spec.privileged,
    });
    let mut gids = p.user.additional_gids.clone();
    for g in &spec.supplemental_groups {
        if !gids.contains(g) && *g != p.user.gid {
            gids.push(*g);
        }
    }
    if !gids.is_empty() {
        process["user"]["additionalGids"] = json!(gids);
    }
    let mut namespaces = vec![json!({"type": "pid"}), json!({"type": "mount"}), json!({"type": "cgroup"})];
    match &p.ipcns {
        Some(path) => namespaces.push(json!({"type": "ipc", "path": path})),
        None => namespaces.push(json!({"type": "ipc"})),
    }
    if let Some(path) = &p.netns {
        namespaces.push(json!({"type": "network", "path": path}));
    }
    if p.own_uts {
        namespaces.push(json!({"type": "uts"}));
    }
    let mut linux = json!({
        "namespaces": namespaces,
        "cgroupsPath": p.cgroups_path,
        "resources": resources(spec),
    });
    if !spec.privileged {
        linux["maskedPaths"] = json!(MASKED_PATHS);
        linux["readonlyPaths"] = json!(READONLY_PATHS);
        if !spec.seccomp_unconfined {
            linux["seccomp"] = seccomp();
        }
    }
    let mut config = json!({
        "ociVersion": "1.0.2",
        "process": process,
        "root": {"path": "rootfs", "readonly": spec.read_only_rootfs},
        "mounts": mounts(p),
        "linux": linux,
    });
    if p.own_uts {
        config["hostname"] = json!(p.hostname);
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params<'a>(spec: &'a ContainerSpec, image: &'a ImageConfig) -> BundleParams<'a> {
        BundleParams {
            spec,
            image,
            user: User { uid: 0, gid: 0, additional_gids: vec![], home: "/root".into() },
            hostname: "web-0",
            netns: Some("/run/netns/litek8s-abc".into()),
            ipcns: Some("/run/litek8s/ipc/abc".into()),
            own_uts: true,
            etc_files: vec![("/etc/hosts".into(), "/pods/x/hosts".into()), ("/etc/resolv.conf".into(), "/pods/x/resolv.conf".into())],
            shm: Some("/pods/x/shm".into()),
            mounts: vec![],
            cgroups_path: "/litek8s/podabc/c1".into(),
        }
    }

    #[test]
    fn argv_follows_kubernetes_command_and_args() {
        let image = ImageConfig {
            entrypoint: Some(vec!["/docker-entrypoint.sh".into()]),
            cmd: Some(vec!["nginx".into(), "-g".into(), "daemon off;".into()]),
            ..Default::default()
        };
        let mut spec = ContainerSpec::default();
        assert_eq!(process_args(&spec, &image).unwrap(), ["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]);
        spec.cmd = Some(vec!["echo".into()]);
        assert_eq!(process_args(&spec, &image).unwrap(), ["/docker-entrypoint.sh", "echo"]);
        // `command` drops both the image's entrypoint and its Cmd.
        spec.entrypoint = Some(vec!["sh".into(), "-c".into()]);
        spec.cmd = None;
        assert_eq!(process_args(&spec, &image).unwrap(), ["sh", "-c"]);
        assert!(process_args(&ContainerSpec::default(), &ImageConfig::default()).is_err());
    }

    #[test]
    fn env_merges_image_and_container() {
        let image = ImageConfig { env: vec!["PATH=/bin".into(), "A=image".into()], ..Default::default() };
        let spec = ContainerSpec { env: vec!["A=pod".into(), "B=x=y".into()], ..Default::default() };
        let env = environment(&spec, &image, "h", "/root");
        assert_eq!(env, ["PATH=/bin", "HOSTNAME=h", "A=pod", "B=x=y", "HOME=/root"]);
    }

    #[test]
    fn users_resolve_like_docker() {
        let passwd = "root:x:0:0:root:/root:/bin/sh\nnginx:x:101:101:nginx:/var/cache/nginx:/sbin/nologin\n";
        let group = "root:x:0:\nnginx:x:101:nginx\nwww:x:33:nginx\n";
        assert_eq!(resolve_user("", passwd, group).unwrap(), User { uid: 0, gid: 0, additional_gids: vec![], home: "/root".into() });
        let nginx = resolve_user("nginx", passwd, group).unwrap();
        assert_eq!((nginx.uid, nginx.gid, nginx.additional_gids.as_slice(), nginx.home.as_str()), (101, 101, &[101u32, 33][..], "/var/cache/nginx"));
        let numeric = resolve_user("1000", passwd, group).unwrap();
        assert_eq!((numeric.uid, numeric.gid, numeric.home.as_str()), (1000, 0, "/"));
        assert_eq!(resolve_user("101:0", passwd, group).unwrap().gid, 0);
        assert_eq!(resolve_user("1000:www", passwd, group).unwrap().gid, 33);
        assert!(resolve_user("nobody", passwd, group).is_err());
    }

    #[test]
    fn config_has_docker_defaults() {
        let image = ImageConfig { cmd: Some(vec!["nginx".into()]), working_dir: Some("/app".into()), ..Default::default() };
        let spec = ContainerSpec {
            memory_limit: Some(128 << 20),
            nano_cpus: Some(500_000_000),
            cpu_request_milli: Some(50),
            cap_add: vec!["NET_ADMIN".into()],
            cap_drop: vec!["MKNOD".into()],
            ..Default::default()
        };
        let c = config(&params(&spec, &image)).unwrap();
        assert_eq!(c["process"]["args"], json!(["nginx"]));
        assert_eq!(c["process"]["cwd"], "/app");
        let caps: Vec<&str> = c["process"]["capabilities"]["bounding"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(caps.contains(&"CAP_NET_ADMIN") && caps.contains(&"CAP_CHOWN") && !caps.contains(&"CAP_MKNOD"));
        assert!(!caps.contains(&"CAP_SYS_ADMIN"));
        assert_eq!(c["linux"]["resources"]["memory"], json!({"limit": 134217728, "swap": 134217728}));
        assert_eq!(c["linux"]["resources"]["cpu"], json!({"period": 100000, "quota": 50000, "shares": 51}));
        assert_eq!(c["linux"]["cgroupsPath"], "/litek8s/podabc/c1");
        let ns: Vec<Value> = c["linux"]["namespaces"].as_array().unwrap().clone();
        assert!(ns.contains(&json!({"type": "network", "path": "/run/netns/litek8s-abc"})));
        assert!(ns.contains(&json!({"type": "ipc", "path": "/run/litek8s/ipc/abc"})));
        assert!(ns.contains(&json!({"type": "uts"})));
        assert_eq!(c["hostname"], "web-0");
        assert!(c["linux"]["maskedPaths"].as_array().unwrap().contains(&json!("/proc/kcore")));
        assert_eq!(c["linux"]["seccomp"]["defaultAction"], "SCMP_ACT_ALLOW");
        let targets: Vec<&str> = c["mounts"].as_array().unwrap().iter().filter_map(|m| m["destination"].as_str()).collect();
        assert_eq!(targets, ["/proc", "/dev", "/dev/pts", "/dev/mqueue", "/sys", "/sys/fs/cgroup", "/dev/shm", "/etc/hosts", "/etc/resolv.conf"]);
        assert_eq!(c["root"], json!({"path": "rootfs", "readonly": false}));
    }

    #[test]
    fn volumes_are_ordered_and_override_defaults() {
        let image = ImageConfig { cmd: Some(vec!["sh".into()]), ..Default::default() };
        let spec = ContainerSpec::default();
        let mut p = params(&spec, &image);
        p.mounts = vec![
            ResolvedMount { source: "/v/b".into(), target: "/data/sub/".into(), kind: MountKind::Bind, read_only: true },
            ResolvedMount { source: "/v/a".into(), target: "/data".into(), kind: MountKind::Bind, read_only: false },
            ResolvedMount { source: "".into(), target: "/dev/shm".into(), kind: MountKind::Tmpfs, read_only: false },
        ];
        let c = config(&p).unwrap();
        let mounts = c["mounts"].as_array().unwrap();
        let targets: Vec<&str> = mounts.iter().filter_map(|m| m["destination"].as_str()).collect();
        assert_eq!(&targets[targets.len() - 3..], ["/data", "/data/sub", "/dev/shm"]);
        assert_eq!(targets.iter().filter(|t| **t == "/dev/shm").count(), 1);
        let sub = mounts.iter().find(|m| m["destination"] == "/data/sub").unwrap();
        assert_eq!(sub["options"], json!(["rbind", "rprivate", "ro"]));
    }

    #[test]
    fn privileged_and_host_network() {
        let image = ImageConfig { cmd: Some(vec!["sh".into()]), ..Default::default() };
        let spec = ContainerSpec { privileged: true, ..Default::default() };
        let mut p = params(&spec, &image);
        p.netns = None;
        p.own_uts = false;
        let c = config(&p).unwrap();
        assert_eq!(c["process"]["capabilities"]["bounding"].as_array().unwrap().len(), ALL_CAPS.len());
        assert!(c["linux"].get("seccomp").is_none() && c["linux"].get("maskedPaths").is_none());
        assert_eq!(c["linux"]["resources"]["devices"], json!([{"allow": true, "access": "rwm"}]));
        assert!(c.get("hostname").is_none());
        assert!(!c["linux"]["namespaces"].as_array().unwrap().iter().any(|n| n["type"] == "network" || n["type"] == "uts"));
    }

    #[test]
    fn drop_all_keeps_only_added() {
        let spec = ContainerSpec { cap_drop: vec!["ALL".into()], cap_add: vec!["NET_BIND_SERVICE".into()], ..Default::default() };
        assert_eq!(capabilities(&spec), ["CAP_NET_BIND_SERVICE"]);
    }
}
