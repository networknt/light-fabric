//! Issuer-owned vocabulary shared by the issuer, credential broker and receivers.
//! These checks supplement signature/issuer/audience/expiry validation.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssuerGrant {
    pub grant_id: uuid::Uuid,
    pub auth_host_id: uuid::Uuid,
    pub provider_id: String,
    pub client_id: uuid::Uuid,
    pub host_id: uuid::Uuid,
    pub user_id: uuid::Uuid,
    pub session_id: uuid::Uuid,
    pub scope: String,
    pub binding: serde_json::Value,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub generation: i64,
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenUse {
    User,
    App,
}

impl TokenUse {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::App => "app",
        }
    }

    /// Only call after cryptographic token validation. No legacy/claim inference.
    pub fn validate(self, verified_claims: &serde_json::Value) -> Result<(), &'static str> {
        if verified_claims
            .get("token_use")
            .and_then(serde_json::Value::as_str)
            == Some(self.as_str())
        {
            Ok(())
        } else {
            Err("access token purpose is missing, malformed or incompatible")
        }
    }
}

/// Read the marker from an already signature-verified payload without losing
/// duplicate-field evidence through a serde_json::Value map.
pub fn verified_payload_purpose(payload: &[u8]) -> Result<Option<TokenUse>, serde_json::Error> {
    fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<TokenUse>, D::Error> {
        TokenUse::deserialize(d).map(Some)
    }
    #[derive(Deserialize)]
    struct Purpose {
        #[serde(default, deserialize_with = "present")]
        token_use: Option<TokenUse>,
    }
    serde_json::from_slice::<Purpose>(payload).map(|p| p.token_use)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_like_app_claims_do_not_change_token_purpose() {
        let app = json!({"token_use":"app", "uid":"user", "role":"admin"});
        assert!(TokenUse::App.validate(&app).is_ok());
        assert!(TokenUse::User.validate(&app).is_err());
        for value in [
            json!({}),
            json!({"token_use":null}),
            json!({"token_use":["user"]}),
            json!({"token_use":"USER"}),
            json!({"token_use":"unknown"}),
        ] {
            assert!(TokenUse::User.validate(&value).is_err());
            assert!(TokenUse::App.validate(&value).is_err());
        }
        assert!(
            TokenUse::App
                .validate(&json!({"token_use":"user"}))
                .is_err()
        );
    }

    #[test]
    fn conflicting_null_and_duplicate_markers_are_not_legacy_tokens() {
        for input in [
            r#"{"token_use":null}"#,
            r#"{"token_use":"user","token_use":"app"}"#,
            r#"{"token_use":"app","token_use":"app"}"#,
            r#"{"token_use":42}"#,
        ] {
            assert!(verified_payload_purpose(input.as_bytes()).is_err());
        }
        assert_eq!(verified_payload_purpose(b"{}").unwrap(), None);
    }
}
