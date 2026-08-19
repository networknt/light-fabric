#[tokio::main]
async fn main() -> anyhow::Result<()> {
    light_knowledge_worker::run_cli().await
}
