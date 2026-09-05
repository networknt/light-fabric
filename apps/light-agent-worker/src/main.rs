use tokio::io::{BufReader, stdin, stdout};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.as_slice() == ["print-capabilities"] {
        let capabilities = light_agent_worker::capabilities();
        let capability_digest = agent_runtime_protocol::canonical_digest(&capabilities)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "capabilities": capabilities,
                "capabilityDigest": capability_digest,
            }))?
        );
        return Ok(());
    }
    if !arguments.is_empty() {
        anyhow::bail!("usage: light-agent-worker [print-capabilities]")
    }
    light_agent_worker::serve(BufReader::new(stdin()), stdout()).await
}
