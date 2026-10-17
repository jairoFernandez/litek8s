fn main() -> anyhow::Result<()> {
    // The crun runtime's per-container shim runs from this binary too; it
    // must start before (and without) the async runtime.
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("crun-shim") => litek8s::kube::agent::crun_shim(&args[2..]),
        Some("crun-pause") => litek8s::kube::agent::crun_pause(&args[2..]),
        _ => {}
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(litek8s::cli::run())
}
