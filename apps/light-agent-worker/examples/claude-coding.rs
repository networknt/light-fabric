//! Local Phase 2 qualification driver, not a public workflow endpoint.
//! Host paths and policy files are supplied by the trusted operator.
use anyhow::{Context, Result, ensure};
use light_agent_worker::claude_code::{ClaudeTurn, HostContext, coding::execute_coding};
use serde::Deserialize;
use std::path::PathBuf;
use tokio::{
    sync::{mpsc, watch},
    time::{Duration, Instant},
};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    turn: ClaudeTurn,
    manifest: agent_materializer::MaterializationManifest,
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    ensure!(
        args.len() == 5,
        "usage: claude-coding EXECUTABLE NATIVE_HOME BUNDLE THREAD_SCOPE REQUEST_JSON"
    );
    let data = tokio::fs::read(&args[4]).await?;
    ensure!(data.len() <= 1024 * 1024, "request too large");
    let request: Request =
        serde_json::from_slice(&data).context("invalid local qualification request")?;
    let host = HostContext {
        executable: PathBuf::from(&args[0]),
        native_home: PathBuf::from(&args[1]),
        working_directory: std::env::current_dir()?,
        thread_scope: args[3].clone(),
    };
    let (cancel_tx, cancel) = watch::channel(false);
    let interrupt = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = cancel_tx.send(true);
        }
    });
    let (events, mut receiver) = mpsc::channel(256);
    // Consume without logging prompt, tool inputs, subscription/account metadata.
    let drain = tokio::spawn(async move { while receiver.recv().await.is_some() {} });
    let result = execute_coding(
        &host,
        &PathBuf::from(&args[2]),
        &request.manifest,
        request.turn,
        cancel,
        Instant::now() + Duration::from_secs(180),
        events,
    )
    .await;
    interrupt.abort();
    drain.await?;
    println!("{}", serde_json::to_string(&result?)?);
    Ok(())
}
