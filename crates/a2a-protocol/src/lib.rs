//! Versioned A2A HTTP/JSON-RPC contracts shared by native and integration
//! runtimes. Transport classification here never grants authorization.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use http::Method;
use jsonwebtoken::{Algorithm, DecodingKey, jwk::Jwk};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use url::Url;

pub const VERSION_HEADER: &str = "a2a-version";
pub const EXTENSIONS_HEADER: &str = "a2a-extensions";
pub const CURRENT_CARD_SUFFIX: &str = "/.well-known/agent-card.json";
pub const LEGACY_CARD_SUFFIX: &str = "/.well-known/agent.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProtocolVersion {
    #[serde(rename = "0.3")]
    V03,
    #[serde(rename = "1.0")]
    V10,
}

impl ProtocolVersion {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V03 => "0.3",
            Self::V10 => "1.0",
        }
    }

    pub fn negotiate(value: Option<&str>) -> Result<Self, ProtocolError> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("0.3") => Ok(Self::V03),
            Some("1.0") => Ok(Self::V10),
            Some(_) => Err(ProtocolError::VersionNotSupported),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum A2aOperation {
    GetAgentCard,
    GetExtendedAgentCard,
    SendMessage,
    SendStreamingMessage,
    GetTask,
    ListTasks,
    CancelTask,
    SubscribeToTask,
    CreateTaskPushNotificationConfig,
    GetTaskPushNotificationConfig,
    ListTaskPushNotificationConfigs,
    DeleteTaskPushNotificationConfig,
}

impl A2aOperation {
    pub const fn policy_class(self) -> &'static str {
        match self {
            Self::GetAgentCard => "card",
            Self::GetExtendedAgentCard => "extended-card",
            Self::CreateTaskPushNotificationConfig
            | Self::GetTaskPushNotificationConfig
            | Self::ListTaskPushNotificationConfigs
            | Self::DeleteTaskPushNotificationConfig => "push-configuration",
            _ => "invoke",
        }
    }

    pub fn from_jsonrpc_method(
        version: ProtocolVersion,
        value: &str,
    ) -> Result<Self, ProtocolError> {
        match (version, value) {
            (ProtocolVersion::V03, "message/send") | (ProtocolVersion::V10, "SendMessage") => {
                Ok(Self::SendMessage)
            }
            (ProtocolVersion::V03, "message/stream")
            | (ProtocolVersion::V10, "SendStreamingMessage") => Ok(Self::SendStreamingMessage),
            (ProtocolVersion::V03, "tasks/get") | (ProtocolVersion::V10, "GetTask") => {
                Ok(Self::GetTask)
            }
            (ProtocolVersion::V10, "ListTasks") => Ok(Self::ListTasks),
            (ProtocolVersion::V03, "tasks/cancel") | (ProtocolVersion::V10, "CancelTask") => {
                Ok(Self::CancelTask)
            }
            (ProtocolVersion::V03, "tasks/resubscribe")
            | (ProtocolVersion::V10, "SubscribeToTask") => Ok(Self::SubscribeToTask),
            (ProtocolVersion::V10, "GetExtendedAgentCard") => Ok(Self::GetExtendedAgentCard),
            (ProtocolVersion::V10, "CreateTaskPushNotificationConfig") => {
                Ok(Self::CreateTaskPushNotificationConfig)
            }
            (ProtocolVersion::V10, "GetTaskPushNotificationConfig") => {
                Ok(Self::GetTaskPushNotificationConfig)
            }
            (ProtocolVersion::V10, "ListTaskPushNotificationConfigs") => {
                Ok(Self::ListTaskPushNotificationConfigs)
            }
            (ProtocolVersion::V10, "DeleteTaskPushNotificationConfig") => {
                Ok(Self::DeleteTaskPushNotificationConfig)
            }
            _ => Err(ProtocolError::MethodNotFound),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassifiedRequest {
    pub version: ProtocolVersion,
    pub operation: A2aOperation,
    pub activated_extensions: BTreeSet<String>,
    pub jsonrpc_id: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtocolProfile {
    pub version: ProtocolVersion,
    #[serde(default)]
    pub advertised_extensions: BTreeSet<String>,
    #[serde(default)]
    pub allowed_inbound_extensions: BTreeSet<String>,
    #[serde(default)]
    pub required_extensions: BTreeSet<String>,
    #[serde(default = "default_max_extension_count")]
    pub maximum_extension_count: usize,
    #[serde(default = "default_max_extension_bytes")]
    pub maximum_extension_bytes: usize,
}

const fn default_max_extension_count() -> usize {
    8
}

const fn default_max_extension_bytes() -> usize {
    2048
}

impl ProtocolProfile {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.version == ProtocolVersion::V03
            && (!self.advertised_extensions.is_empty()
                || !self.allowed_inbound_extensions.is_empty()
                || !self.required_extensions.is_empty()
                || self.maximum_extension_count != 0
                || self.maximum_extension_bytes != 0)
        {
            return Err(ProtocolError::ExtensionsNotAllowed);
        }
        if !self
            .required_extensions
            .is_subset(&self.advertised_extensions)
            || !self
                .required_extensions
                .is_subset(&self.allowed_inbound_extensions)
        {
            return Err(ProtocolError::InvalidExtensionConfiguration);
        }
        Ok(())
    }

    pub fn classify(
        &self,
        method: &Method,
        path: &str,
        version: Option<&str>,
        extensions: Option<&str>,
        body: &[u8],
    ) -> Result<ClassifiedRequest, ProtocolError> {
        self.validate()?;
        let requested_version = ProtocolVersion::negotiate(version)?;
        if requested_version != self.version {
            return Err(ProtocolError::VersionNotSupported);
        }
        if method == Method::GET
            && (path.ends_with(CURRENT_CARD_SUFFIX) || path.ends_with(LEGACY_CARD_SUFFIX))
        {
            if extensions.is_some_and(|value| !value.trim().is_empty()) {
                return Err(ProtocolError::ExtensionsNotAllowed);
            }
            return Ok(ClassifiedRequest {
                version: requested_version,
                operation: A2aOperation::GetAgentCard,
                activated_extensions: BTreeSet::new(),
                jsonrpc_id: Value::Null,
            });
        }
        if method != Method::POST {
            return Err(ProtocolError::MethodNotAllowed);
        }
        let request: JsonRpcRequest =
            serde_json::from_slice(body).map_err(|_| ProtocolError::ParseError)?;
        if request.jsonrpc != "2.0" {
            return Err(ProtocolError::InvalidRequest);
        }
        let activated_extensions = self.negotiate_extensions(extensions)?;
        Ok(ClassifiedRequest {
            version: requested_version,
            operation: A2aOperation::from_jsonrpc_method(requested_version, &request.method)?,
            activated_extensions,
            jsonrpc_id: request.id,
        })
    }

    fn negotiate_extensions(
        &self,
        header: Option<&str>,
    ) -> Result<BTreeSet<String>, ProtocolError> {
        let header = header.unwrap_or("").trim();
        if header.len() > self.maximum_extension_bytes {
            return Err(ProtocolError::ExtensionsTooLarge);
        }
        let requested = header
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        if requested.len() > self.maximum_extension_count {
            return Err(ProtocolError::ExtensionsTooLarge);
        }
        if self.version == ProtocolVersion::V03 && !requested.is_empty() {
            return Err(ProtocolError::ExtensionsNotAllowed);
        }
        if !self.required_extensions.is_subset(&requested) {
            return Err(ProtocolError::ExtensionSupportRequired);
        }
        Ok(requested
            .intersection(&self.allowed_inbound_extensions)
            .cloned()
            .collect())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Value,
    method: String,
    #[allow(dead_code)]
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("Parse error")]
    ParseError,
    #[error("Invalid Request")]
    InvalidRequest,
    #[error("Method not found")]
    MethodNotFound,
    #[error("Method not allowed")]
    MethodNotAllowed,
    #[error("VersionNotSupportedError")]
    VersionNotSupported,
    #[error("ExtensionSupportRequiredError")]
    ExtensionSupportRequired,
    #[error("extensions are not available for this profile")]
    ExtensionsNotAllowed,
    #[error("extension service parameters exceed configured limits")]
    ExtensionsTooLarge,
    #[error("extension profile is invalid")]
    InvalidExtensionConfiguration,
    #[error("Agent Card URL is invalid")]
    InvalidAgentCardUrl,
    #[error("Agent Card signature is invalid or does not match its trusted profile")]
    InvalidAgentCardSignature,
}

impl ProtocolError {
    pub const fn jsonrpc_code(&self) -> i64 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest | Self::MethodNotAllowed => -32600,
            Self::MethodNotFound => -32601,
            Self::VersionNotSupported => -32009,
            Self::ExtensionSupportRequired => -32008,
            Self::ExtensionsNotAllowed
            | Self::ExtensionsTooLarge
            | Self::InvalidExtensionConfiguration
            | Self::InvalidAgentCardUrl
            | Self::InvalidAgentCardSignature => -32602,
        }
    }

    pub fn jsonrpc_response(&self, id: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": self.jsonrpc_code(), "message": self.to_string()}
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustedCardSigningProfile {
    pub profile_id: String,
    pub purpose: String,
    pub algorithm: String,
    pub jwks_url: String,
    pub revocation_epoch: u64,
    pub keys: Vec<Jwk>,
}

/// Verifies an A2A Agent Card JWS against the exact Portal-projected profile.
/// The caller supplies no `kid`, key URL, algorithm, or fallback key source.
pub fn verify_signed_agent_card(
    card: &Value,
    trusted: &TrustedCardSigningProfile,
) -> Result<(), ProtocolError> {
    if !trusted.jwks_url.starts_with("https://") || trusted.keys.is_empty() {
        return Err(ProtocolError::InvalidAgentCardSignature);
    }
    let signatures = card
        .get("signatures")
        .and_then(Value::as_array)
        .filter(|values| !values.is_empty())
        .ok_or(ProtocolError::InvalidAgentCardSignature)?;
    let payload = canonical_card_payload(card)?;
    let payload = URL_SAFE_NO_PAD.encode(payload);
    for signature in signatures {
        if verify_one_card_signature(signature, &payload, trusted).unwrap_or(false) {
            return Ok(());
        }
    }
    Err(ProtocolError::InvalidAgentCardSignature)
}

pub fn agent_card_etag(card: &Value, policy_digest: &str, revocation_epoch: u64) -> String {
    let material = json!({
        "card": card,
        "disclosure": "public",
        "policyDigest": policy_digest,
        "revocationEpoch": revocation_epoch
    });
    let bytes = serde_json_canonicalizer::to_vec(&material).expect("JSON values canonicalize");
    format!("\"{:x}\"", Sha256::digest(bytes))
}

fn verify_one_card_signature(
    signature: &Value,
    payload: &str,
    trusted: &TrustedCardSigningProfile,
) -> Result<bool, ProtocolError> {
    let protected = signature
        .get("protected")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::InvalidAgentCardSignature)?;
    let encoded_signature = signature
        .get("signature")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::InvalidAgentCardSignature)?;
    let header_bytes = URL_SAFE_NO_PAD
        .decode(protected)
        .map_err(|_| ProtocolError::InvalidAgentCardSignature)?;
    let header: Value = serde_json::from_slice(&header_bytes)
        .map_err(|_| ProtocolError::InvalidAgentCardSignature)?;
    let algorithm = parse_card_algorithm(
        header.get("alg").and_then(Value::as_str),
        &trusted.algorithm,
    )?;
    if header.get("typ").and_then(Value::as_str) != Some("JOSE")
        || header.get("jku").and_then(Value::as_str) != Some(trusted.jwks_url.as_str())
    {
        return Err(ProtocolError::InvalidAgentCardSignature);
    }
    let kid = header
        .get("kid")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::InvalidAgentCardSignature)?;
    let jwk = trusted
        .keys
        .iter()
        .find(|key| key.common.key_id.as_deref() == Some(kid))
        .ok_or(ProtocolError::InvalidAgentCardSignature)?;
    let key = DecodingKey::from_jwk(jwk).map_err(|_| ProtocolError::InvalidAgentCardSignature)?;
    jsonwebtoken::crypto::verify(
        encoded_signature,
        format!("{protected}.{payload}").as_bytes(),
        &key,
        algorithm,
    )
    .map_err(|_| ProtocolError::InvalidAgentCardSignature)
}

fn parse_card_algorithm(value: Option<&str>, expected: &str) -> Result<Algorithm, ProtocolError> {
    let value = value.ok_or(ProtocolError::InvalidAgentCardSignature)?;
    if value != expected {
        return Err(ProtocolError::InvalidAgentCardSignature);
    }
    match value {
        "ES256" => Ok(Algorithm::ES256),
        "RS256" => Ok(Algorithm::RS256),
        "EdDSA" => Ok(Algorithm::EdDSA),
        _ => Err(ProtocolError::InvalidAgentCardSignature),
    }
}

fn canonical_card_payload(card: &Value) -> Result<Vec<u8>, ProtocolError> {
    let mut unsigned = card.clone();
    unsigned
        .as_object_mut()
        .ok_or(ProtocolError::InvalidAgentCardSignature)?
        .remove("signatures");
    serde_json_canonicalizer::to_vec(&unsigned)
        .map_err(|_| ProtocolError::InvalidAgentCardSignature)
}

pub fn rewrite_agent_card_url(card: &Value, public_url: &str) -> Result<Value, ProtocolError> {
    let url = Url::parse(public_url).map_err(|_| ProtocolError::InvalidAgentCardUrl)?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ProtocolError::InvalidAgentCardUrl);
    }
    let mut card = card.clone();
    let object = card
        .as_object_mut()
        .ok_or(ProtocolError::InvalidAgentCardUrl)?;
    let signed = object.contains_key("signatures");
    if let Some(interfaces) = object.get_mut("supportedInterfaces") {
        let interfaces = interfaces
            .as_array_mut()
            .filter(|values| !values.is_empty())
            .ok_or(ProtocolError::InvalidAgentCardUrl)?;
        for interface in interfaces {
            let interface = interface
                .as_object_mut()
                .ok_or(ProtocolError::InvalidAgentCardUrl)?;
            if signed && interface.get("url") != Some(&json!(public_url)) {
                return Err(ProtocolError::InvalidAgentCardUrl);
            }
            interface.insert("url".into(), json!(public_url));
        }
        return Ok(card);
    }
    if signed && object.get("url") != Some(&json!(public_url)) {
        return Err(ProtocolError::InvalidAgentCardUrl);
    }
    object.insert("url".into(), json!(public_url));
    Ok(card)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v1() -> ProtocolProfile {
        ProtocolProfile {
            version: ProtocolVersion::V10,
            advertised_extensions: BTreeSet::new(),
            allowed_inbound_extensions: BTreeSet::new(),
            required_extensions: BTreeSet::new(),
            maximum_extension_count: 8,
            maximum_extension_bytes: 2048,
        }
    }

    #[test]
    fn missing_version_is_zero_three_and_never_inferred_from_body() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"message/send","params":{}}"#;
        assert_eq!(
            v1().classify(&Method::POST, "/a2a/a", None, None, body),
            Err(ProtocolError::VersionNotSupported)
        );
    }

    #[test]
    fn method_vocabulary_is_isolated_by_generation() {
        let legacy = ProtocolProfile {
            version: ProtocolVersion::V03,
            advertised_extensions: BTreeSet::new(),
            allowed_inbound_extensions: BTreeSet::new(),
            required_extensions: BTreeSet::new(),
            maximum_extension_count: 0,
            maximum_extension_bytes: 0,
        };
        let old = br#"{"jsonrpc":"2.0","id":1,"method":"message/send","params":{}}"#;
        let new = br#"{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{}}"#;
        assert!(
            legacy
                .classify(&Method::POST, "/a2a/a", None, None, old)
                .is_ok()
        );
        assert_eq!(
            legacy.classify(&Method::POST, "/a2a/a", None, None, new),
            Err(ProtocolError::MethodNotFound)
        );
        assert!(
            v1().classify(&Method::POST, "/a2a/a", Some("1.0"), None, new)
                .is_ok()
        );
        assert_eq!(
            v1().classify(&Method::POST, "/a2a/a", Some("1.0"), None, old),
            Err(ProtocolError::MethodNotFound)
        );
    }

    #[test]
    fn both_card_generations_are_exact_get_routes() {
        let legacy = ProtocolProfile {
            version: ProtocolVersion::V03,
            advertised_extensions: BTreeSet::new(),
            allowed_inbound_extensions: BTreeSet::new(),
            required_extensions: BTreeSet::new(),
            maximum_extension_count: 0,
            maximum_extension_bytes: 0,
        };
        assert_eq!(
            legacy
                .classify(
                    &Method::GET,
                    "/a2a/a/.well-known/agent.json",
                    None,
                    None,
                    &[],
                )
                .unwrap()
                .operation,
            A2aOperation::GetAgentCard
        );
        assert_eq!(
            v1().classify(
                &Method::GET,
                "/a2a/a/.well-known/agent-card.json",
                Some("1.0"),
                None,
                &[],
            )
            .unwrap()
            .operation,
            A2aOperation::GetAgentCard
        );
        assert_eq!(
            v1().classify(
                &Method::GET,
                "/a2a/a/.well-known/agent-card.json/child",
                Some("1.0"),
                None,
                &[],
            ),
            Err(ProtocolError::MethodNotAllowed)
        );
    }

    #[test]
    fn unknown_optional_extension_is_not_activated() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{}}"#;
        let request = v1()
            .classify(
                &Method::POST,
                "/a2a/a",
                Some("1.0"),
                Some("https://example.invalid/ext/v1"),
                body,
            )
            .unwrap();
        assert!(request.activated_extensions.is_empty());
    }

    #[test]
    fn phase6_jsonrpc_methods_are_v1_only() {
        for method in [
            "GetExtendedAgentCard",
            "CreateTaskPushNotificationConfig",
            "GetTaskPushNotificationConfig",
            "ListTaskPushNotificationConfigs",
            "DeleteTaskPushNotificationConfig",
        ] {
            let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{{}}}}"#);
            assert!(
                v1().classify(&Method::POST, "/a2a/a", Some("1.0"), None, body.as_bytes())
                    .is_ok(),
                "{method}"
            );
            assert_eq!(
                ProtocolProfile {
                    version: ProtocolVersion::V03,
                    advertised_extensions: BTreeSet::new(),
                    allowed_inbound_extensions: BTreeSet::new(),
                    required_extensions: BTreeSet::new(),
                    maximum_extension_count: 0,
                    maximum_extension_bytes: 0,
                }
                .classify(&Method::POST, "/a2a/a", None, None, body.as_bytes()),
                Err(ProtocolError::MethodNotFound)
            );
        }
    }

    #[test]
    fn zero_three_profile_cannot_carry_extensions() {
        let profile = ProtocolProfile {
            version: ProtocolVersion::V03,
            advertised_extensions: BTreeSet::new(),
            allowed_inbound_extensions: BTreeSet::new(),
            required_extensions: BTreeSet::new(),
            maximum_extension_count: 8,
            maximum_extension_bytes: 0,
        };
        assert_eq!(profile.validate(), Err(ProtocolError::ExtensionsNotAllowed));
    }

    #[test]
    fn signed_card_is_not_silently_rewritten() {
        let card = json!({"url":"https://old.example/a2a","signatures":[{}]});
        assert_eq!(
            rewrite_agent_card_url(&card, "https://new.example/a2a"),
            Err(ProtocolError::InvalidAgentCardUrl)
        );
    }

    #[test]
    fn card_etag_binds_policy_and_revocation_epoch() {
        let card = json!({"name":"account","url":"https://agents.example/a2a/account"});
        let first = agent_card_etag(&card, &format!("sha256:{}", "a".repeat(64)), 1);
        assert_eq!(
            first,
            agent_card_etag(&card, &format!("sha256:{}", "a".repeat(64)), 1)
        );
        assert_ne!(
            first,
            agent_card_etag(&card, &format!("sha256:{}", "a".repeat(64)), 2)
        );
        assert_ne!(
            first,
            agent_card_etag(&card, &format!("sha256:{}", "b".repeat(64)), 1)
        );
    }

    #[test]
    fn signed_card_rejects_unprojected_jku_and_algorithm() {
        let protected = URL_SAFE_NO_PAD.encode(
            br#"{"alg":"RS256","typ":"JOSE","kid":"oauth-token-key","jku":"https://evil.example/jwks"}"#,
        );
        let card = json!({
            "name":"account",
            "url":"https://agents.example/a2a/account",
            "signatures":[{"protected":protected,"signature":"invalid"}]
        });
        let profile = TrustedCardSigningProfile {
            profile_id: "native-dev".into(),
            purpose: "A2A_CARD_NATIVE".into(),
            algorithm: "ES256".into(),
            jwks_url: "https://signin.example/a2a/native-dev/jwks".into(),
            revocation_epoch: 1,
            keys: Vec::new(),
        };
        assert_eq!(
            verify_signed_agent_card(&card, &profile),
            Err(ProtocolError::InvalidAgentCardSignature)
        );
    }

    #[test]
    fn agent_card_signature_payload_uses_rfc8785_number_serialization() {
        let card = serde_json::json!({"z":1.0,"a":"value","signatures":[]});
        assert_eq!(
            String::from_utf8(canonical_card_payload(&card).unwrap()).unwrap(),
            r#"{"a":"value","z":1}"#
        );
    }

    #[test]
    fn v1_supported_interface_urls_are_rewritten_without_internal_disclosure() {
        let card = json!({
            "name":"account",
            "supportedInterfaces":[
                {"url":"http://agent.internal:8448/a2a","protocolBinding":"JSONRPC","protocolVersion":"1.0"}
            ]
        });
        let rewritten =
            rewrite_agent_card_url(&card, "https://agents.example/a2a/account").unwrap();
        assert_eq!(
            rewritten["supportedInterfaces"][0]["url"],
            "https://agents.example/a2a/account"
        );
        assert!(!rewritten.to_string().contains("agent.internal"));
    }
}
