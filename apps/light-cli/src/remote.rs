//! Where the CLI's settings come from: the config server named in `startup.yml`.
//!
//! The convention every app follows: `GET {configServerUri}/config-server/configs` with `host`,
//! `serviceId` and `envTag`. The reply is `values.yml`: a flat mapping whose keys are full
//! placeholder names such as `cli.oauthProviderId`. It is handed to `cli.yml`, whose
//! `${cli.*}` placeholders it overrides. Which config server the CLI asks decides which instance
//! it belongs to: its Gateway, its OAuth provider and where the sign-in page is. A CLI downloaded
//! from another instance carries that instance's `startup.yml`.
//!
//! Unavailable is never fatal: `cli.yml`'s defaults (and environment variables) apply.
//!
//! `authorization` in `startup.yml`, if present, is sent to the config server **and nowhere else**.
//! It reads non-secret settings; it is not an identity and the CLI presents no other credential.

use serde_yaml::Mapping;

use crate::config::{CliConfig, Settings, ensure_transport_is_safe};
use crate::error::CliError;
use crate::http;
use crate::text;

pub enum Remote {
    Found(Mapping),
    /// Not available, and why. Never fatal on its own.
    Unavailable(String),
}

pub async fn fetch_values(config: &CliConfig) -> Remote {
    let (Some(base), Some(host), Some(service_id)) = (
        config.config_server_uri.as_deref(),
        config.host.as_deref(),
        config.service_id.as_deref(),
    ) else {
        return Remote::Unavailable("configServerUri, host or serviceId is not set".into());
    };
    // The token, if there is one, goes in the Authorization header: same cleartext rule as any
    // credential.
    if let Err(error) = ensure_transport_is_safe(base) {
        return Remote::Unavailable(error.to_string());
    }
    let client = match http::client(
        config.ca_bundle.as_deref(),
        config.connect_timeout,
        config.timeout,
    ) {
        Ok(client) => client,
        Err(error) => return Remote::Unavailable(error.to_string()),
    };
    let mut request = client
        .get(format!("{base}/config-server/configs"))
        .query(&[
            ("host", host),
            ("serviceId", service_id),
            ("envTag", config.env_tag.as_str()),
        ])
        .header(reqwest::header::ACCEPT, "application/yaml");
    if let Some(token) = config.config_token.as_ref() {
        request = request.bearer_auth(token.expose());
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return Remote::Unavailable(format!("{}", error.without_url())),
    };
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail: String = text::sanitize_line(body.trim()).chars().take(120).collect();
        return Remote::Unavailable(format!("HTTP {status}: {detail}"));
    }
    match serde_yaml::from_str::<Mapping>(&body) {
        Ok(values) => Remote::Found(values),
        Err(error) => Remote::Unavailable(format!("values.yml is not a YAML mapping: {error}")),
    }
}

/// The CLI's settings: `cli.yml`, with the config server's `values.yml` overriding its
/// `${cli.*}` placeholders when there is one.
pub async fn load_settings(config: &CliConfig) -> Result<(Settings, Remote), CliError> {
    let remote = fetch_values(config).await;
    let values = match &remote {
        Remote::Found(values) => Some(values),
        Remote::Unavailable(_) => None,
    };
    let settings = Settings::load(&config.config_dir, values)?;
    Ok((settings, remote))
}

/// Say where a setting came from, so a surprising URL is easy to trace.
pub fn origin_of(remote: &Remote, key: &str) -> String {
    match remote {
        Remote::Found(values) if values.contains_key(key) => "from the config server".to_string(),
        Remote::Found(_) => format!("cli.yml default; the config server has no {key}"),
        Remote::Unavailable(reason) => format!("cli.yml default; config server: {reason}"),
    }
}
