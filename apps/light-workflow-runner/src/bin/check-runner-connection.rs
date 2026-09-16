//! Read-only TLS/authentication diagnostic; sends no runner registration or lease.
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| "TLS provider initialization failed")?;
    let config = light_workflow_runner::configuration::RunnerConfig::load()
        .map_err(std::io::Error::other)?;
    let mut request = config.controller_url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "authorization",
        format!(
            "Bearer {}",
            config.read_jwt().map_err(std::io::Error::other)?
        )
        .parse()?,
    );
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio_tungstenite::connect_async(request),
    )
    .await
    {
        Ok(Ok((mut socket, _))) => {
            socket.close(None).await?;
            println!("Controller TLS and runner authentication accepted; no registration sent");
            Ok(())
        }
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
            Err(format!("Controller handshake status {}", response.status()).into())
        }
        Ok(Err(error)) => Err(format!("Controller TLS/transport failed: {error}").into()),
        Err(_) => Err("Controller handshake timed out".into()),
    }
}
