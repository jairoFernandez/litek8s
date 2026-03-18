#[tokio::main]
async fn main() -> anyhow::Result<()> {
    litek8s::cli::run().await
}
