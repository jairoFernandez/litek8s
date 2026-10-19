fn main() -> anyhow::Result<()> {
    // The crun runtime's per-container shim and the dev sandbox's first
    // stages run from this binary too; they must start before (and without)
    // the async runtime.
    let args: Vec<String> = std::env::args().collect();
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
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(litek8s::cli::run())
}
