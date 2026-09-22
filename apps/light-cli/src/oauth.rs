//! Client for light-oauth's device grant, as the Gateway exposes it: the OAuth 2.0 Device
//! Authorization Grant (RFC 8628), plus refresh and revocation (RFC 7009).
//!
//! The CLI is a **public client**: there is no client secret, no certificate and no application
//! token. The requests are ordinary OAuth form posts. The design is
//! `light-portal-doc/src/design/light-oauth/device-authorization.md`.

use std::time::Duration;

use serde::Deserialize;

use crate::config::{OauthSettings, Secret, ensure_transport_is_safe};
use crate::error::CliError;
use crate::http;

/// What the device authorization request returns.
#[derive(Debug)]
pub struct DeviceCode {
    pub device_code: Secret,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug)]
pub enum Poll {
    /// The user has not approved yet.
    Pending,
    /// Polling too fast: wait longer between polls.
    SlowDown,
    Tokens(Tokens),
}

#[derive(Debug)]
pub struct Tokens {
    pub access_token: Secret,
    /// Absent from a refresh only if the server did not rotate it.
    pub refresh_token: Option<Secret>,
    pub expires_in: i64,
    /// Seconds until the login ends. Only the first issuance says.
    pub refresh_expires_in: Option<i64>,
    pub remember: bool,
}

#[derive(Deserialize)]
struct RawDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct RawTokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    refresh_expires_in: Option<i64>,
    remember: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawError {
    error: Option<String>,
    error_description: Option<String>,
}

/// An OAuth error reply, kept apart from transport failures.
struct OauthError {
    status: u16,
    code: String,
    description: String,
}

pub struct DeviceClient {
    http: reqwest::Client,
    base: String,
    provider_id: String,
    client_id: String,
}

impl DeviceClient {
    /// `ca_bundle` verifies the server's certificate, as well as the system roots.
    pub fn new(
        settings: &OauthSettings,
        ca_bundle: Option<&std::path::Path>,
    ) -> Result<Self, CliError> {
        // The login's tokens travel here: never plain HTTP, not even on loopback.
        ensure_transport_is_safe(&settings.uri)?;
        if !settings.uri.starts_with("https://") {
            return Err(CliError::Config(format!(
                "cli.oauthUri {} must be https: it carries the user's tokens",
                settings.uri
            )));
        }
        Ok(Self {
            http: http::client(ca_bundle, Duration::from_secs(5), Duration::from_secs(15))?,
            base: settings.uri.trim_end_matches('/').to_string(),
            provider_id: settings.provider_id.clone(),
            client_id: settings.client_id.clone(),
        })
    }

    async fn post(
        &self,
        path: &str,
        form: &[(&str, &str)],
    ) -> Result<Result<String, OauthError>, CliError> {
        let url = format!("{}/oauth2/{}/{path}", self.base, self.provider_id);
        let response = self
            .http
            .post(&url)
            .form(form)
            .send()
            .await
            .map_err(|e| transport_failure(&self.base, e))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| {
            CliError::Unreachable(format!("reading light-oauth's reply: {}", e.without_url()))
        })?;
        if status.is_success() {
            return Ok(Ok(text));
        }
        let raw: RawError = serde_json::from_str(&text).unwrap_or_else(|_| RawError {
            // light-oauth's refresh path currently returns the OAuth error code as plain text.
            // Accept that wire shape as well as the RFC-style JSON used by the device flow.
            error: text
                .trim()
                .split_ascii_whitespace()
                .next()
                .filter(|code| !code.is_empty())
                .map(str::to_string),
            error_description: None,
        });
        Ok(Err(OauthError {
            status: status.as_u16(),
            code: raw.error.unwrap_or_default(),
            description: raw
                .error_description
                .unwrap_or_else(|| text.trim().chars().take(120).collect())
                .chars()
                .filter(|c| !c.is_control())
                .collect(),
        }))
    }

    /// Step 1: ask for a device code and a user code.
    pub async fn request_code(&self, scope: Option<&str>) -> Result<DeviceCode, CliError> {
        let mut form = vec![("client_id", self.client_id.as_str())];
        if let Some(scope) = scope {
            form.push(("scope", scope));
        }
        let text = match self.post("device_authorization", &form).await? {
            Ok(text) => text,
            Err(e) => {
                return Err(match (e.status, e.code.as_str()) {
                    (_, "invalid_client") => CliError::Denied(format!(
                        "light-oauth does not accept client {} for sign-in. It must exist, be active, be linked \
                         to the provider, and have Client Profile \"cli\" and Client Type \"public\" or \"trusted\" (set them on \
                         Portal's client page). Check cli.oauthClientId; light-oauth's log says which of these fails",
                        self.client_id
                    )),
                    (401 | 403, _) => CliError::Denied(format!(
                        "the sign-in request was refused before it reached light-oauth's client check (HTTP {}: {}). \
                         Check the Gateway route for POST /oauth2/{}/device_authorization",
                        e.status, e.description, self.provider_id
                    )),
                    (_, "invalid_scope") => {
                        CliError::Failed(format!("that scope is not allowed: {}", e.description))
                    }
                    (429, _) | (_, "temporarily_unavailable") => CliError::Failed(format!(
                        "light-oauth is limiting sign-in requests from this install; wait and try again ({})",
                        e.description
                    )),
                    (500..=599, _) => CliError::Unreachable(format!(
                        "light-oauth failed (HTTP {}): {}",
                        e.status, e.description
                    )),
                    _ => CliError::Failed(format!(
                        "light-oauth refused the sign-in request (HTTP {}): {}",
                        e.status, e.description
                    )),
                });
            }
        };
        let raw: RawDeviceCode = serde_json::from_str(&text).map_err(|e| {
            CliError::Failed(format!(
                "light-oauth's device authorization reply was not understood: {e}"
            ))
        })?;
        Ok(DeviceCode {
            device_code: Secret::new(raw.device_code),
            user_code: raw.user_code,
            verification_uri: raw.verification_uri,
            verification_uri_complete: raw.verification_uri_complete,
            expires_in: raw.expires_in,
            interval: raw.interval.unwrap_or(5),
        })
    }

    /// Step 3: has the user approved?
    pub async fn poll(&self, device_code: &Secret) -> Result<Poll, CliError> {
        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code.expose()),
            ("client_id", self.client_id.as_str()),
        ];
        match self.post("token", &form).await? {
            Ok(text) => Ok(Poll::Tokens(parse_tokens(&text)?)),
            Err(e) => match (e.status, e.code.as_str()) {
                (_, "authorization_pending") => Ok(Poll::Pending),
                (429, _) | (_, "slow_down") => Ok(Poll::SlowDown),
                (_, "access_denied") => Err(CliError::Denied("the sign-in was denied".into())),
                (_, "expired_token") => Err(CliError::Failed(
                    "the sign-in code expired before it was approved; run `/login` again".into(),
                )),
                (_, "invalid_grant") => Err(CliError::Failed(
                    "light-oauth no longer knows this sign-in request; run `/login` again".into(),
                )),
                (_, "invalid_client") => Err(CliError::Denied(format!(
                    "light-oauth refused this install: {}",
                    e.description
                ))),
                (500..=u16::MAX, _) => Err(CliError::Unreachable(format!(
                    "light-oauth failed (HTTP {}): {}",
                    e.status, e.description
                ))),
                _ => Err(CliError::Failed(format!(
                    "light-oauth refused the poll (HTTP {}): {}",
                    e.status, e.description
                ))),
            },
        }
    }

    /// Exchange the refresh token for a new access token and a rotated refresh token.
    /// `invalid_grant` means the login has ended (expired, revoked, or the user was
    /// locked): the user must sign in again.
    pub async fn refresh(&self, refresh_token: &Secret) -> Result<Tokens, CliError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.expose()),
            ("client_id", self.client_id.as_str()),
        ];
        match self.post("token", &form).await? {
            Ok(text) => parse_tokens(&text),
            Err(e) => Err(match e.code.as_str() {
                "invalid_grant" => CliError::LoginRequired(
                    "your login has ended or was revoked; run `/login`".into(),
                ),
                "invalid_client" => CliError::Denied(format!(
                    "light-oauth refused this install: {}",
                    e.description
                )),
                _ if e.status >= 500 => CliError::Unreachable(format!(
                    "light-oauth failed (HTTP {}): {}",
                    e.status, e.description
                )),
                _ => CliError::Failed(format!(
                    "light-oauth refused the refresh (HTTP {}): {}",
                    e.status, e.description
                )),
            }),
        }
    }

    /// RFC 7009: end the login on the server. The server answers 200 whether or not it
    /// knew the token.
    pub async fn revoke(&self, refresh_token: &Secret) -> Result<(), CliError> {
        let form = [
            ("token", refresh_token.expose()),
            ("client_id", self.client_id.as_str()),
        ];
        match self.post("revoke", &form).await? {
            Ok(_) => Ok(()),
            Err(e) if e.status >= 500 => Err(CliError::Unreachable(format!(
                "light-oauth failed (HTTP {}): {}",
                e.status, e.description
            ))),
            Err(e) => Err(CliError::Failed(format!(
                "light-oauth refused the revocation (HTTP {}): {}",
                e.status, e.description
            ))),
        }
    }
}

fn parse_tokens(text: &str) -> Result<Tokens, CliError> {
    let raw: RawTokens = serde_json::from_str(text).map_err(|e| {
        CliError::Failed(format!("light-oauth's token reply was not understood: {e}"))
    })?;
    if raw.access_token.is_empty() {
        return Err(CliError::Failed(
            "light-oauth returned an empty access token".into(),
        ));
    }
    if raw.expires_in <= 0 || raw.refresh_expires_in.is_some_and(|seconds| seconds <= 0) {
        return Err(CliError::Failed(
            "light-oauth returned an invalid token lifetime".into(),
        ));
    }
    Ok(Tokens {
        access_token: Secret::new(raw.access_token),
        refresh_token: raw.refresh_token.filter(|t| !t.is_empty()).map(Secret::new),
        expires_in: raw.expires_in,
        refresh_expires_in: raw.refresh_expires_in,
        remember: raw.remember.as_deref() == Some("Y"),
    })
}

fn transport_failure(base: &str, error: reqwest::Error) -> CliError {
    let error = error.without_url();
    let mut detail = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(inner) = source {
        detail.push_str(": ");
        detail.push_str(&inner.to_string());
        source = inner.source();
    }
    let lower = detail.to_ascii_lowercase();
    let hint = if lower.contains("certificate")
        || lower.contains("handshake")
        || lower.contains("alert")
    {
        " (a TLS failure: this CLI must trust the server's certificate via bootstrapCaCertPath)"
    } else {
        ""
    };
    CliError::Unreachable(format!("{base}: {detail}{hint}"))
}
