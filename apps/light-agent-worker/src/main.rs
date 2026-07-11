use tokio::io::{BufReader, stdin, stdout};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    light_agent_worker::serve(BufReader::new(stdin()), stdout()).await
}
