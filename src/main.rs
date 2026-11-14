fn main() -> anyhow::Result<()> {
    // The crun runtime's per-container shim and the dev sandbox's first
    // stages run from this binary too; they must start before (and without)
    // the async runtime.
    let args: Vec<String> = std::env::args().collect();
    // iptables, ip, modprobe... live in sbin, which a user's PATH lacks on
    // Debian; preflight finds them there, so the processes we start must too.
    #[cfg(target_os = "linux")]
    if let Some(path) = with_system_dirs(&std::env::var_os("PATH").unwrap_or_default()) {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("PATH", path) };
    }
    match args.get(1).map(String::as_str) {
        Some("crun-shim") => litek8s::kube::agent::crun_shim(&args[2..]),
        Some("crun-pause") => litek8s::kube::agent::crun_pause(&args[2..]),
        Some("docker-fs") => litek8s::docker::fs::main(&args[2..]),
        #[cfg(target_os = "linux")]
        Some("dev-sandbox") => litek8s::dev::sandbox::main(&args[2..]),
        #[cfg(target_os = "macos")]
        Some("dev-vm") => litek8s::dev::vm::vm_main(&args[2..]),
        // PID 1 of the macOS dev VM.
        #[cfg(target_os = "linux")]
        None if args[0] == "/init" && std::process::id() == 1 => litek8s::dev::guest::main(),
        _ => {}
    }
    // glibc gives each thread that contends for malloc its own arena (up to
    // 8 per CPU), and a long-running server's freed memory then stays spread
    // over all of them: the API server under Argo CD's watches grew to 470 MiB
    // RSS on a 16-core machine with ~60 MiB live, 120 MiB with two arenas.
    // MALLOC_ARENA_MAX, when set, still wins.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        unsafe { libc::mallopt(libc::M_ARENA_MAX, 2) };
    }
    // The terminal UI waits on a few sockets: one thread is plenty (the
    // servers want one per core).
    let runtime = if args.get(1).map(String::as_str) == Some("ui") {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?
    } else {
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?
    };
    runtime.block_on(litek8s::cli::run())
}

/// `path` with the standard system directories it lacks appended; None if
/// it has them all.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn with_system_dirs(path: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    let mut dirs: Vec<std::path::PathBuf> = std::env::split_paths(path).filter(|d| !d.as_os_str().is_empty()).collect();
    let before = dirs.len();
    for d in ["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"] {
        if !dirs.iter().any(|p| p == std::path::Path::new(d)) {
            dirs.push(d.into());
        }
    }
    if dirs.len() == before {
        return None;
    }
    std::env::join_paths(dirs).ok()
}

#[cfg(test)]
mod tests {
    use super::with_system_dirs;
    use std::ffi::OsStr;

    #[test]
    fn path_gets_the_sbin_dirs_it_lacks() {
        let p = with_system_dirs(OsStr::new("/home/u/bin:/usr/local/bin:/usr/bin:/bin")).unwrap();
        assert_eq!(p, "/home/u/bin:/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin");
        assert_eq!(with_system_dirs(OsStr::new("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")), None);
        assert_eq!(with_system_dirs(OsStr::new("")).unwrap(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    }
}
