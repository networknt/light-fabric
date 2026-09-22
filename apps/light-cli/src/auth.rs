//! `/login`, `/whoami`, `/logout`, and the user token every Gateway call needs.
//!
//! Signing in uses the OAuth device grant against light-oauth, as a public client, through
//! the Gateway. There is no certificate or application credential involved. The design and its security analysis are in
//! `light-portal-doc/src/design/light-oauth/device-authorization.md`.
//!
//! Rules this module keeps:
//!
//! * A refreshed token pair is written to disk **before** the new access token is used,
//!   under the store lock, so a crash or a parallel invocation cannot lose the only valid
//!   refresh token.
//! * A refresh answered `invalid_grant` ends the local session and asks the user to sign
//!   in again ([`CliError::LoginRequired`], exit code 6). Any other failure leaves the
//!   session alone: a network error must not sign the user out.
//! * The login end is absolute. Refreshing never extends it.
//! * Refresh and logout go to the endpoint recorded in the session, not whatever
//!   `cli.yml` says now.

use std::time::Duration;

use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::config::{CliConfig, OauthSettings, Secret};
use crate::error::CliError;
use crate::oauth::{DeviceClient, DeviceCode, Poll, Tokens};
use crate::remote;
use crate::session::{self, UserSession};
use crate::store::Store;

/// Consecutive network failures tolerated while waiting for approval.
const POLL_FAILURES_TOLERATED: u32 = 3;

pub struct LoginOptions {
    pub scope: Option<String>,
    /// The server's interval is honoured, but never polled faster than this.
    pub min_interval: Duration,
    /// How much longer to wait after `slow_down` (RFC 8628 says five seconds).
    pub slow_down_step: Duration,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self {
            scope: None,
            min_interval: Duration::from_secs(1),
            slow_down_step: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    /// `signed-out`, `signed-in`, `access-expired` (a refresh will fix it) or
    /// `login-ended` (sign in again).
    pub state: &'static str,
    pub user_id: Option<String>,
    pub email: Option<String>,
    pub roles: Option<String>,
    pub remember: Option<bool>,
    /// Negative once the access token has expired.
    pub access_expires_in: Option<i64>,
    pub login_expires_at: Option<String>,
    pub login_expires_in: Option<i64>,
    pub oauth_uri: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogoutOutcome {
    pub was_signed_in: bool,
    pub revoked_on_server: bool,
}

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

fn rfc3339(unix: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(unix)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

fn describe(session: Option<&UserSession>, at: i64) -> AuthStatus {
    let Some(session) = session else {
        return AuthStatus {
            state: "signed-out",
            user_id: None,
            email: None,
            roles: None,
            remember: None,
            access_expires_in: None,
            login_expires_at: None,
            login_expires_in: None,
            oauth_uri: None,
        };
    };
    let claims = session.claims();
    AuthStatus {
        state: if session.login_ended(at) {
            "login-ended"
        } else if session.access_is_fresh(at) {
            "signed-in"
        } else {
            "access-expired"
        },
        user_id: claims.user_id,
        email: claims.email,
        roles: claims.roles,
        remember: Some(session.remember),
        access_expires_in: Some(session.access_remaining(at)),
        login_expires_at: session.login_expires_at.and_then(rfc3339),
        login_expires_in: session.login_expires_at.map(|end| end - at),
        oauth_uri: Some(session.oauth_uri.clone()),
    }
}

/// What is signed in, from the local session alone. Contacts no server.
pub fn status(config: &CliConfig) -> Result<AuthStatus, CliError> {
    let store = Store::new(config.store_dir.clone());
    Ok(describe(session::load(&store)?.as_ref(), now()))
}

/// Sign in. `announce` is called once with the code the user must approve, before
/// polling starts.
pub async fn login(
    config: &CliConfig,
    options: &LoginOptions,
    announce: impl FnOnce(&DeviceCode),
) -> Result<AuthStatus, CliError> {
    let (settings, from) = remote::load_settings(config).await?;
    let oauth = settings.oauth()?;
    eprintln!(
        "sign-in service: {} ({})",
        oauth.uri,
        remote::origin_of(&from, "cli.oauthUri")
    );
    let client = DeviceClient::new(&oauth, config.ca_bundle.as_deref())?;

    let code = client.request_code(options.scope.as_deref()).await?;
    let deadline = tokio::time::Instant::now()
        .checked_add(Duration::from_secs(code.expires_in))
        .ok_or_else(|| {
            CliError::Failed("light-oauth returned an invalid device-code lifetime".into())
        })?;
    let mut interval = Duration::from_secs(code.interval).max(options.min_interval);
    announce(&code);

    let mut failures = 0;
    let tokens = loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(CliError::Failed(
                "the sign-in code expired before it was approved; run `/login` again".into(),
            ));
        }
        match client.poll(&code.device_code).await {
            Ok(Poll::Tokens(tokens)) => break tokens,
            Ok(Poll::Pending) => failures = 0,
            Ok(Poll::SlowDown) => {
                failures = 0;
                interval += options.slow_down_step;
            }
            // The server did not answer: the approval may still be coming.
            Err(CliError::Unreachable(reason)) => {
                failures += 1;
                if failures >= POLL_FAILURES_TOLERATED {
                    return Err(CliError::Unreachable(reason));
                }
            }
            Err(other) => return Err(other),
        }
    };

    let at = now();
    let session = new_session(&oauth, tokens, at)?;
    let store = Store::new(config.store_dir.clone());
    let _lock = store.lock()?;
    session::save(&store, &session)?;
    Ok(describe(Some(&session), at))
}

fn new_session(oauth: &OauthSettings, tokens: Tokens, at: i64) -> Result<UserSession, CliError> {
    let refresh_token = tokens.refresh_token.ok_or_else(|| {
        CliError::Failed(
            "light-oauth issued no refresh token, so the login could not be kept".into(),
        )
    })?;
    Ok(UserSession {
        oauth_uri: oauth.uri.clone(),
        provider_id: oauth.provider_id.clone(),
        client_id: oauth.client_id.clone(),
        access_token: tokens.access_token.expose().to_string(),
        access_expires_at: at.checked_add(tokens.expires_in).ok_or_else(|| {
            CliError::Failed("light-oauth returned an invalid access-token lifetime".into())
        })?,
        refresh_token: refresh_token.expose().to_string(),
        login_expires_at: tokens
            .refresh_expires_in
            .map(|seconds| {
                at.checked_add(seconds).ok_or_else(|| {
                    CliError::Failed("light-oauth returned an invalid login lifetime".into())
                })
            })
            .transpose()?,
        remember: tokens.remember,
        signed_in_at: at,
    })
}

/// The user's access token for a Gateway call, refreshed first if it has under a minute
/// left. `Ok(None)` when nobody is signed in.
pub async fn user_token(config: &CliConfig) -> Result<Option<Secret>, CliError> {
    let store = Store::new(config.store_dir.clone());
    let Some(session) = session::load(&store)? else {
        return Ok(None);
    };
    let at = now();
    if session.access_is_fresh(at) {
        return Ok(Some(Secret::new(session.access_token)));
    }
    if session.login_ended(at) {
        return Err(login_ended());
    }
    refresh(config, &store).await.map(Some)
}

fn login_ended() -> CliError {
    CliError::LoginRequired("your login has ended; run `/login`".into())
}

fn client_for(session: &UserSession, config: &CliConfig) -> Result<DeviceClient, CliError> {
    DeviceClient::new(
        &OauthSettings {
            uri: session.oauth_uri.clone(),
            provider_id: session.provider_id.clone(),
            client_id: session.client_id.clone(),
        },
        config.ca_bundle.as_deref(),
    )
}

async fn refresh(config: &CliConfig, store: &Store) -> Result<Secret, CliError> {
    // Hold the lock for the whole refresh, so parallel invocations refresh once.
    let _lock = store.lock()?;

    // Another invocation may have refreshed, or signed out, while we waited.
    let session = session::load(store)?
        .ok_or_else(|| CliError::LoginRequired("you are signed out; run `/login`".into()))?;
    let at = now();
    if session.access_is_fresh(at) {
        return Ok(Secret::new(session.access_token));
    }
    if session.login_ended(at) {
        return Err(login_ended());
    }

    let client = client_for(&session, config)?;
    match client
        .refresh(&Secret::new(session.refresh_token.clone()))
        .await
    {
        Ok(tokens) => {
            let rotated = tokens
                .refresh_token
                .map(|t| t.expose().to_string())
                .unwrap_or_else(|| session.refresh_token.clone());
            let updated = UserSession {
                access_token: tokens.access_token.expose().to_string(),
                access_expires_at: at.checked_add(tokens.expires_in).ok_or_else(|| {
                    CliError::Failed("light-oauth returned an invalid access-token lifetime".into())
                })?,
                refresh_token: rotated,
                ..session
            };
            // Saved before the access token is handed out.
            session::save(store, &updated)?;
            Ok(Secret::new(updated.access_token))
        }
        Err(CliError::LoginRequired(message)) => {
            // The server says this refresh token is dead; keeping it only means asking again.
            session::clear(store)?;
            Err(CliError::LoginRequired(message))
        }
        Err(other) => Err(other),
    }
}

/// End the login on the server, then delete the local session. With `local_only`, only
/// delete the local session (for when the server cannot be reached).
pub async fn logout(config: &CliConfig, local_only: bool) -> Result<LogoutOutcome, CliError> {
    let store = Store::new(config.store_dir.clone());
    // Held from reading the session to deleting it. Without that, another invocation could finish
    // a login while the revocation is in flight, and this one would then delete the new login for
    // having revoked the old; a refresh in between would rotate the token being revoked.
    let _lock = store.lock()?;
    let Some(session) = session::load(&store)? else {
        return Ok(LogoutOutcome {
            was_signed_in: false,
            revoked_on_server: false,
        });
    };
    if local_only {
        session::clear(&store)?;
        return Ok(LogoutOutcome {
            was_signed_in: true,
            revoked_on_server: false,
        });
    }

    let revoked = async {
        client_for(&session, config)?
            .revoke(&Secret::new(session.refresh_token.clone()))
            .await
    }
    .await;
    if let Err(error) = revoked {
        eprintln!(
            "the login is still active: nothing was deleted. `/logout --local` deletes the local tokens only \
             (the login then ends by itself when it expires)"
        );
        return Err(error);
    }
    session::clear(&store)?;
    Ok(LogoutOutcome {
        was_signed_in: true,
        revoked_on_server: true,
    })
}

/// Text from a server, made safe to print: no control characters, so a hostile reply
/// cannot move the cursor or recolour the terminal.
pub fn printable(text: &str) -> String {
    crate::text::strip_controls(text)
}

/// Open the browser only when asked to and only where one can be: never over SSH.
pub fn should_open_browser(requested: bool, ssh_connection: bool) -> bool {
    requested && !ssh_connection
}

/// Best effort. Only http(s) URLs are ever handed to the opener.
pub fn open_browser(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(parsed.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_is_opened_only_when_asked_and_never_over_ssh() {
        assert!(should_open_browser(true, false));
        assert!(!should_open_browser(false, false));
        assert!(!should_open_browser(true, true));
        assert!(!should_open_browser(false, true));
    }

    #[test]
    fn server_text_is_stripped_of_control_characters_before_printing() {
        assert_eq!(printable("BCDF-GHJK\u{1b}[2J\r\n"), "BCDF-GHJK[2J");
    }

    #[test]
    fn only_web_urls_are_ever_handed_to_the_opener() {
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "not a url",
            "ftp://x/y",
            "-x",
        ] {
            assert!(!open_browser(bad), "{bad}");
        }
    }
}
