//! The user's sign-in: access token, refresh token and when the login ends.
//!
//! One file, `user-session.json`, next to the identity in the environment's store
//! directory (`0600`). It is replaced atomically (a fresh file, fsynced, then
//! renamed over the old one), so a crash can never leave a half-written file and, above
//! all, can never lose the only valid refresh token: the CLI saves a rotated refresh
//! token here **before** it uses the access token that came with it.
//!
//! The device code is never stored. Claims read from the access token are for display
//! only (`/whoami`); nothing here trusts them, and the server decides what the
//! token allows.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::error::CliError;
use crate::store::Store;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const SESSION_FILE: &str = "user-session.json";

/// Refresh when the access token has less than this long left.
pub const REFRESH_MARGIN_SECONDS: i64 = 60;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserSession {
    /// Where the tokens came from: refresh and logout go back to exactly this
    /// endpoint and client, even if `cli.yml` changes later.
    pub oauth_uri: String,
    pub provider_id: String,
    pub client_id: String,
    pub access_token: String,
    /// Unix seconds.
    pub access_expires_at: i64,
    pub refresh_token: String,
    /// Unix seconds. The end of the login: absolute, refreshing does not extend it.
    pub login_expires_at: Option<i64>,
    pub remember: bool,
    pub signed_in_at: i64,
}

impl std::fmt::Debug for UserSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserSession")
            .field("oauth_uri", &self.oauth_uri)
            .field("access_expires_at", &self.access_expires_at)
            .field("login_expires_at", &self.login_expires_at)
            .field("tokens", &"<redacted>")
            .finish()
    }
}

/// What the access token says about the user, for display.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Claims {
    pub user_id: Option<String>,
    pub email: Option<String>,
    pub roles: Option<String>,
}

/// The claims of a JWT **without verifying it**. Display only.
pub fn unverified_claims(jwt: &str) -> Claims {
    let payload = jwt
        .split('.')
        .nth(1)
        .and_then(|part| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(part)
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let text = |name: &str| {
        payload
            .as_ref()
            .and_then(|p| p.get(name))
            .and_then(Value::as_str)
            .map(|v| v.chars().filter(|c| !c.is_control()).collect())
    };
    Claims {
        user_id: text("uid"),
        email: text("eml"),
        roles: text("role"),
    }
}

impl UserSession {
    pub fn claims(&self) -> Claims {
        unverified_claims(&self.access_token)
    }

    /// Seconds until the access token expires (negative if it already has).
    pub fn access_remaining(&self, now: i64) -> i64 {
        self.access_expires_at - now
    }

    /// The login has ended: no refresh can succeed.
    pub fn login_ended(&self, now: i64) -> bool {
        self.login_expires_at.is_some_and(|end| end <= now)
    }

    /// The access token is good for at least the refresh margin.
    pub fn access_is_fresh(&self, now: i64) -> bool {
        self.access_remaining(now) > REFRESH_MARGIN_SECONDS
    }
}

fn path_of(store: &Store) -> PathBuf {
    store.root().join(SESSION_FILE)
}

/// Read the session, if there is one.
pub fn load(store: &Store) -> Result<Option<UserSession>, CliError> {
    let path = path_of(store);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(CliError::Failed(format!(
                "could not read {}: {e}",
                path.display()
            )));
        }
    };
    serde_json::from_str(&text).map(Some).map_err(|_| {
        CliError::Failed(format!(
            "{} is corrupt; delete it and run `/login`",
            path.display()
        ))
    })
}

/// Replace the session. The caller holds the store lock.
pub fn save(store: &Store, session: &UserSession) -> Result<(), CliError> {
    Store::create_private_dir(store.root())?;
    let json = serde_json::to_string_pretty(session)
        .map_err(|e| CliError::Failed(format!("could not encode the session: {e}")))?;
    let staging = store
        .root()
        .join(format!("{SESSION_FILE}.tmp-{}", Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let written = (|| -> std::io::Result<()> {
        let mut file = options.open(&staging)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        fs::rename(&staging, path_of(store))?;
        File::open(store.root())?.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&staging);
        return Err(e.into());
    }
    Ok(())
}

/// Delete the session. The caller holds the store lock. Absent is fine.
pub fn clear(store: &Store) -> Result<(), CliError> {
    match fs::remove_file(path_of(store)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Where the session file is, for `status` output and tests.
pub fn path(store_root: &Path) -> PathBuf {
    store_root.join(SESSION_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn jwt(claims: serde_json::Value) -> String {
        let b64 = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!("{}.{}.sig", b64(b"{}"), b64(claims.to_string().as_bytes()))
    }

    fn session(now: i64) -> UserSession {
        UserSession {
            oauth_uri: "https://localhost:7444".into(),
            provider_id: "prov".into(),
            client_id: "client".into(),
            access_token: jwt(
                serde_json::json!({"uid": "u-1", "eml": "a@example.test", "role": "admin user"}),
            ),
            access_expires_at: now + 600,
            refresh_token: "refresh-1".into(),
            login_expires_at: Some(now + 86_400),
            remember: false,
            signed_in_at: now,
        }
    }

    #[test]
    fn a_saved_session_is_loaded_back_and_an_empty_store_has_none() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().join("dev"));
        assert!(load(&store).unwrap().is_none());
        save(&store, &session(1_000)).unwrap();
        let back = load(&store).unwrap().expect("saved");
        assert_eq!(
            (back.refresh_token.as_str(), back.login_expires_at),
            ("refresh-1", Some(87_400))
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only_and_no_temp_file_is_left_behind() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().join("dev"));
        save(&store, &session(1_000)).unwrap();
        save(&store, &session(2_000)).unwrap();
        assert_eq!(
            fs::metadata(path(store.root()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let names: Vec<String> = fs::read_dir(store.root())
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        assert_eq!(names, [SESSION_FILE], "{names:?}");
    }

    #[test]
    fn replacing_the_session_replaces_the_refresh_token() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().join("dev"));
        save(&store, &session(1_000)).unwrap();
        let mut next = session(1_000);
        next.refresh_token = "refresh-2".into();
        save(&store, &next).unwrap();
        assert_eq!(load(&store).unwrap().unwrap().refresh_token, "refresh-2");
    }

    #[test]
    fn clearing_removes_the_file_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().join("dev"));
        save(&store, &session(1_000)).unwrap();
        clear(&store).unwrap();
        clear(&store).unwrap();
        assert!(load(&store).unwrap().is_none());
    }

    #[test]
    fn a_corrupt_file_is_an_error_that_names_the_fix_and_leaks_no_token() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().join("dev"));
        Store::create_private_dir(store.root()).unwrap();
        fs::write(
            path(store.root()),
            "{\"refreshToken\": \"secret-refresh\", broken",
        )
        .unwrap();
        let error = load(&store).unwrap_err().to_string();
        assert!(error.contains("/login"), "{error}");
        assert!(!error.contains("secret-refresh"), "{error}");
    }

    #[test]
    fn freshness_uses_a_sixty_second_margin_and_the_login_end_is_absolute() {
        let s = session(1_000);
        assert!(s.access_is_fresh(1_000));
        assert!(s.access_is_fresh(1_000 + 600 - 61));
        assert!(
            !s.access_is_fresh(1_000 + 600 - 60),
            "exactly at the margin is refreshed"
        );
        assert!(!s.login_ended(1_000 + 86_399));
        assert!(
            s.login_ended(1_000 + 86_400),
            "the end instant itself has ended"
        );
        let open_ended = UserSession {
            login_expires_at: None,
            ..s
        };
        assert!(!open_ended.login_ended(i64::MAX));
    }

    #[test]
    fn claims_are_read_for_display_and_bad_tokens_yield_nothing() {
        let claims = session(0).claims();
        assert_eq!(claims.user_id.as_deref(), Some("u-1"));
        assert_eq!(claims.email.as_deref(), Some("a@example.test"));
        assert_eq!(claims.roles.as_deref(), Some("admin user"));
        for bad in ["", "abc", "a.b.c", "a.!!!.c"] {
            assert_eq!(unverified_claims(bad), Claims::default(), "{bad:?}");
        }
        // Control characters cannot reach the terminal through a claim.
        let sneaky = unverified_claims(&jwt(serde_json::json!({"eml": "a\u{1b}[31m@x"})));
        assert_eq!(sneaky.email.as_deref(), Some("a[31m@x"));
    }

    #[test]
    fn debug_output_never_contains_a_token() {
        let text = format!("{:?}", session(0));
        assert!(
            !text.contains("refresh-1") && !text.contains("eyJ"),
            "{text}"
        );
    }
}
