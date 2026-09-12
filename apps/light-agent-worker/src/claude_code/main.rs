#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.as_slice() == ["print-capabilities"] {
        let caps = coding_agent_runtime::claude::capabilities();
        println!(
            "{}",
            serde_json::json!({"capabilityDigest":agent_runtime_protocol::canonical_digest(&caps)?,"capabilities":caps})
        );
        return Ok(());
    }
    anyhow::ensure!(
        args.is_empty(),
        "usage: light-claude-worker [print-capabilities]"
    );
    light_agent_worker::serve_claude(
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}
