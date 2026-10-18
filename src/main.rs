fn main() -> anyhow::Result<()> {
    // The crun runtime's per-container shim and the dev sandbox's first
    // stages run from this binary too; they must start before (and without)
    // the async runtime.
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("crun-shim") => litek8s::kube::agent::crun_shim(&args[2..]),
        Some("crun-pause") => litek8s::kube::agent::crun_pause(&args[2..]),
        #[cfg(target_os = "linux")]
        Some("dev-sandbox") => litek8s::dev::sandbox::main(&args[2..]),
        // PID 1 of the macOS dev VM.
        #[cfg(target_os = "linux")]
        None if args[0] == "/init" && std::process::id() == 1 => litek8s::dev::guest::main(),
        _ => {}
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(litek8s::cli::run())
}
