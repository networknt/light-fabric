use llm_gateway::config::RuntimeAuth;
use pingora::http::RequestHeader;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

pub const SIDECAR_MANIFEST_SCHEMA_VERSION: &str = "1";
pub const SIDECAR_DENY_HANDLER: &str = "sidecar-deny";
pub const SIDECAR_IDENTITY_HANDLER: &str = "sidecar-identity";
const UPSTREAM_SELECTING_HANDLERS: &[&str] =
    &["proxy", "router", "virtual", "resource", "path-resource"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarOperation {
    Chat,
    Responses,
    Embeddings,
}

impl SidecarOperation {
    pub const fn method(self) -> &'static str {
        "POST"
    }

    pub const fn path(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
            Self::Embeddings => "/v1/embeddings",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SidecarProfileRequest {
    pub profile_version: String,
    pub physical_runtime_id: String,
    pub runtime_base_url: String,
    pub certificate_identity_sha256: String,
    pub isolation_evidence_sha256: String,
    pub operations: BTreeSet<SidecarOperation>,
    pub jwt_trust: SidecarJwtTrust,
    #[serde(default)]
    pub runtime_auth: RuntimeAuth,
    #[serde(default = "default_max_request_time_ms")]
    pub max_request_time_ms: u64,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_stream_setup_timeout_ms")]
    pub stream_setup_timeout_ms: u64,
    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,
    #[serde(default = "default_max_response_body_bytes")]
    pub max_response_body_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SidecarJwtTrust {
    pub issuer: String,
    pub audience: String,
    pub key_server_url: String,
    #[serde(default = "default_key_uri")]
    pub key_uri: String,
    #[serde(default)]
    pub key_service_id: Option<String>,
    #[serde(default)]
    pub ca_cert_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnabledPath {
    pub method: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SidecarManifest {
    pub schema_version: String,
    pub profile_version: String,
    pub config_sha256: String,
    pub certificate_identity_sha256: String,
    pub isolation_evidence_sha256: String,
    pub physical_runtime_id: String,
    pub enabled_paths: Vec<EnabledPath>,
    pub runtime_base_url_sha256: String,
    pub runtime_auth: RuntimeAuth,
    pub max_request_time_ms: u64,
    pub connect_timeout_ms: u64,
    pub stream_setup_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub max_request_body_bytes: usize,
    pub max_response_body_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarIdentityResponse {
    pub profile_version: String,
    pub config_sha256: String,
    pub certificate_identity_sha256: String,
    pub physical_runtime_id: String,
    pub enabled_paths: Vec<EnabledPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GeneratedHandlerConfig {
    pub enabled: bool,
    pub handlers: Vec<String>,
    pub chains: BTreeMap<String, GeneratedHandlerChain>,
    pub paths: Vec<GeneratedHandlerPath>,
    pub default_handlers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GeneratedHandlerChain {
    pub exec: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GeneratedHandlerPath {
    pub path: String,
    pub method: String,
    pub exec: Vec<String>,
}

pub fn generate_sidecar_bundle(
    request: &SidecarProfileRequest,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    validate_request(request)?;
    let mut enabled_paths = request
        .operations
        .iter()
        .map(|operation| EnabledPath {
            method: operation.method().to_string(),
            path: operation.path().to_string(),
        })
        .collect::<Vec<_>>();
    enabled_paths.push(EnabledPath {
        method: "GET".to_string(),
        path: "/sidecar/health".to_string(),
    });
    enabled_paths.push(EnabledPath {
        method: "GET".to_string(),
        path: "/sidecar/identity".to_string(),
    });
    enabled_paths
        .sort_by(|left, right| (&left.path, &left.method).cmp(&(&right.path, &right.method)));

    let mut chains = BTreeMap::new();
    chains.insert(
        "sidecar-operation-chain".to_string(),
        GeneratedHandlerChain {
            exec: vec![
                "correlation".to_string(),
                "unified-security".to_string(),
                "headers".to_string(),
                "proxy".to_string(),
            ],
        },
    );
    chains.insert(
        "sidecar-health-chain".to_string(),
        GeneratedHandlerChain {
            exec: vec!["correlation".to_string(), "health".to_string()],
        },
    );
    chains.insert(
        "sidecar-identity-chain".to_string(),
        GeneratedHandlerChain {
            exec: vec![
                "correlation".to_string(),
                "unified-security".to_string(),
                SIDECAR_IDENTITY_HANDLER.to_string(),
            ],
        },
    );
    chains.insert(
        "sidecar-deny-chain".to_string(),
        GeneratedHandlerChain {
            exec: vec![SIDECAR_DENY_HANDLER.to_string()],
        },
    );
    let paths = enabled_paths
        .iter()
        .map(|enabled| GeneratedHandlerPath {
            path: enabled.path.clone(),
            method: enabled.method.clone(),
            exec: vec![
                match enabled.path.as_str() {
                    "/sidecar/health" => "sidecar-health-chain",
                    "/sidecar/identity" => "sidecar-identity-chain",
                    _ => "sidecar-operation-chain",
                }
                .to_string(),
            ],
        })
        .collect();
    let handler = GeneratedHandlerConfig {
        enabled: true,
        handlers: vec![
            "correlation".to_string(),
            "unified-security".to_string(),
            "headers".to_string(),
            "proxy".to_string(),
            "health".to_string(),
            SIDECAR_DENY_HANDLER.to_string(),
            SIDECAR_IDENTITY_HANDLER.to_string(),
        ],
        chains,
        paths,
        default_handlers: vec!["sidecar-deny-chain".to_string()],
    };
    validate_generated_profile(&handler)?;

    let protected_paths = request
        .operations
        .iter()
        .map(|operation| serde_json::json!({"prefix":operation.path(),"jwt":true}))
        .chain(std::iter::once(
            serde_json::json!({"prefix":"/sidecar/identity","jwt":true}),
        ))
        .collect::<Vec<_>>();
    let mut files = BTreeMap::new();
    files.insert(
        "handler.yml".to_string(),
        serde_yaml::to_string(&handler)
            .map_err(|error| error.to_string())?
            .into_bytes(),
    );
    files.insert(
        "unified-security.yml".to_string(),
        serde_yaml::to_string(&serde_json::json!({
            "enabled":true,
            "anonymousPrefixes":["/sidecar/health"],
            "pathPrefixAuths":protected_paths
        }))
        .map_err(|error| error.to_string())?
        .into_bytes(),
    );
    files.insert(
        "proxy.yml".to_string(),
        serde_yaml::to_string(&serde_json::json!({
            "enabled":true,
            "http2Enabled":false,
            "hosts":request.runtime_base_url,
            "connectionsPerThread":1,
            "maxRequestTime":request.max_request_time_ms,
            "rewriteHostHeader":true,
            "reuseXForwarded":false,
            "maxConnectionRetries":1,
            "maxQueueSize":0,
            "forwardJwtClaims":false
        }))
        .map_err(|error| error.to_string())?
        .into_bytes(),
    );
    files.insert(
        "server.yml".to_string(),
        serde_yaml::to_string(&serde_json::json!({
            "ip":"0.0.0.0",
            "advertisedAddress":"127.0.0.1",
            "httpPort":8080,
            "enableHttp":false,
            "httpsPort":8443,
            "enableHttps":true,
            "tlsCertPath":"/var/run/secrets/sidecar-server/tls.crt",
            "tlsKeyPath":"/var/run/secrets/sidecar-server/tls.key",
            "serviceId":"com.networknt.model-provider-sidecar-1.0.0",
            "enableRegistry":false,
            "startOnRegistryFailure":false,
            "dynamicPort":false,
            "environment":"prod",
            "shutdownGracefulPeriod":2000
        }))
        .map_err(|error| error.to_string())?
        .into_bytes(),
    );
    files.insert(
        "security.yml".to_string(),
        serde_yaml::to_string(&serde_json::json!({
            "enableVerifyJwt":true,
            "enableVerifyScope":false,
            "ignoreJwtExpiry":false,
            "enableRelaxedKeyValidation":false,
            "issuer":request.jwt_trust.issuer,
            "audience":[request.jwt_trust.audience],
            "bootstrapFromKeyService":true,
            "providerId":request.jwt_trust.key_service_id.clone().unwrap_or_default(),
            "jwt":{"clockSkewInSeconds":60}
        }))
        .map_err(|error| error.to_string())?
        .into_bytes(),
    );
    files.insert(
        "client.yml".to_string(),
        serde_yaml::to_string(&serde_json::json!({
            "tls":{
                "verifyHostname":true,
                "caCertPath":request.jwt_trust.ca_cert_path
            },
            "oauth":{"token":{"key":{
                "server_url":request.jwt_trust.key_server_url,
                "serviceId":request.jwt_trust.key_service_id,
                "uri":request.jwt_trust.key_uri,
                "audience":request.jwt_trust.audience
            }}}
        }))
        .map_err(|error| error.to_string())?
        .into_bytes(),
    );
    let config_sha256 = digest_bundle(&files);
    let manifest = SidecarManifest {
        schema_version: SIDECAR_MANIFEST_SCHEMA_VERSION.to_string(),
        profile_version: request.profile_version.clone(),
        config_sha256,
        certificate_identity_sha256: request.certificate_identity_sha256.clone(),
        isolation_evidence_sha256: request.isolation_evidence_sha256.clone(),
        physical_runtime_id: request.physical_runtime_id.clone(),
        enabled_paths,
        runtime_base_url_sha256: sha256_hex(request.runtime_base_url.as_bytes()),
        runtime_auth: request.runtime_auth.clone(),
        max_request_time_ms: request.max_request_time_ms,
        connect_timeout_ms: request.connect_timeout_ms,
        stream_setup_timeout_ms: request.stream_setup_timeout_ms,
        idle_timeout_ms: request.idle_timeout_ms,
        max_request_body_bytes: request.max_request_body_bytes,
        max_response_body_bytes: request.max_response_body_bytes,
    };
    files.insert(
        "sidecar-manifest.json".to_string(),
        serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
    );
    Ok(files)
}

pub fn validate_generated_profile(handler: &GeneratedHandlerConfig) -> Result<(), String> {
    if handler.default_handlers.is_empty() {
        return Err("model-provider sidecar default chain must be non-empty".to_string());
    }
    for default in &handler.default_handlers {
        let chain = handler
            .chains
            .get(default)
            .ok_or_else(|| format!("sidecar default chain `{default}` is missing"))?;
        if chain.exec.last().map(String::as_str) != Some(SIDECAR_DENY_HANDLER)
            || chain
                .exec
                .iter()
                .any(|handler| UPSTREAM_SELECTING_HANDLERS.contains(&handler.as_str()))
        {
            return Err(
                "sidecar default chain must terminate locally without an upstream selector"
                    .to_string(),
            );
        }
    }
    let mut exact = BTreeSet::new();
    for path in &handler.paths {
        if !exact.insert((path.method.clone(), path.path.clone())) || path.exec.len() != 1 {
            return Err(
                "sidecar path entries must be unique exact method/path mappings".to_string(),
            );
        }
    }
    Ok(())
}

pub fn write_sidecar_bundle(
    output: &Path,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    fs::create_dir_all(output).map_err(|error| error.to_string())?;
    for (name, bytes) in files {
        fs::write(output.join(name), bytes).map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn sidecar_identity_json() -> Result<Vec<u8>, String> {
    sidecar_identity_json_for(loaded_manifest()?)
}

fn sidecar_identity_json_for(manifest: &SidecarManifest) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&SidecarIdentityResponse {
        profile_version: manifest.profile_version.clone(),
        config_sha256: manifest.config_sha256.clone(),
        certificate_identity_sha256: manifest.certificate_identity_sha256.clone(),
        physical_runtime_id: manifest.physical_runtime_id.clone(),
        enabled_paths: manifest.enabled_paths.clone(),
    })
    .map_err(|error| error.to_string())
}

pub fn sidecar_limits() -> Result<(u64, u64, u64, usize, usize), String> {
    let manifest = loaded_manifest()?;
    Ok((
        manifest.connect_timeout_ms,
        manifest.stream_setup_timeout_ms,
        manifest.idle_timeout_ms,
        manifest.max_request_body_bytes,
        manifest.max_response_body_bytes,
    ))
}

pub async fn apply_sidecar_upstream_headers(request: &mut RequestHeader) -> Result<(), String> {
    apply_sidecar_upstream_headers_for(loaded_manifest()?, request).await
}

async fn apply_sidecar_upstream_headers_for(
    manifest: &SidecarManifest,
    request: &mut RequestHeader,
) -> Result<(), String> {
    for name in [
        "authorization",
        "x-api-key",
        "connection",
        "proxy-connection",
        "keep-alive",
        "te",
        "trailer",
        "upgrade",
    ] {
        request.remove_header(name);
    }
    // Pingora inserts this framing header before the filter for HTTP/2 and
    // chunked HTTP/1 requests that have a body but no content length. Its HTTP/1
    // upstream writer needs the marker to avoid selecting a zero-length body.
    // Preserve only the canonical framing value generated by Pingora's h2-to-h1
    // conversion. Pingora's parser also accepts stacked transfer codings whose
    // final token is `chunked` (for example `gzip, chunked`); rejecting those
    // here is deliberate to keep the sidecar's request-framing boundary strict
    // and avoid reintroducing ambiguous smuggling inputs.
    if request
        .headers
        .get("transfer-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("chunked"))
    {
        // Never forward conflicting HTTP/1 framing.
        request.remove_header("content-length");
    } else {
        request.remove_header("transfer-encoding");
    }
    match &manifest.runtime_auth {
        RuntimeAuth::None => {}
        RuntimeAuth::Bearer { credential_ref } => {
            let secret = resolve_runtime_credential(credential_ref).await?;
            request
                .insert_header("authorization", format!("Bearer {secret}"))
                .map_err(|error| error.to_string())?;
        }
        RuntimeAuth::ApiKey {
            credential_ref,
            header,
        } => {
            let secret = resolve_runtime_credential(credential_ref).await?;
            request
                .insert_header(header.wire_name(), secret)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn loaded_manifest() -> Result<&'static SidecarManifest, String> {
    static MANIFEST: OnceLock<Result<SidecarManifest, String>> = OnceLock::new();
    MANIFEST
        .get_or_init(|| {
            let path = std::env::var("MODEL_PROVIDER_SIDECAR_MANIFEST")
                .map_err(|_| "MODEL_PROVIDER_SIDECAR_MANIFEST is required".to_string())?;
            let bytes = fs::read(path).map_err(|error| error.to_string())?;
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(Clone::clone)
}

async fn resolve_runtime_credential(reference: &str) -> Result<String, String> {
    let value = if let Some(name) = reference
        .strip_prefix("env://")
        .or_else(|| reference.strip_prefix("env:"))
    {
        std::env::var(name)
            .map_err(|_| format!("runtime credential environment `{name}` is unavailable"))?
    } else if let Some(path) = reference.strip_prefix("file://") {
        tokio::fs::read_to_string(path)
            .await
            .map_err(|error| error.to_string())?
    } else {
        return Err("SIDECAR_RUNTIME credential reference must use env:// or file://".to_string());
    };
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err("SIDECAR_RUNTIME credential resolved to an empty value".to_string());
    }
    Ok(value)
}

fn validate_request(request: &SidecarProfileRequest) -> Result<(), String> {
    if request.profile_version.trim().is_empty()
        || request.physical_runtime_id.trim().is_empty()
        || request.operations.is_empty()
        || !is_sha256(&request.certificate_identity_sha256)
        || !is_sha256(&request.isolation_evidence_sha256)
        || request.max_request_time_ms == 0
        || request.connect_timeout_ms == 0
        || request.stream_setup_timeout_ms == 0
        || request.idle_timeout_ms == 0
        || request.max_request_body_bytes == 0
        || request.max_response_body_bytes == 0
        || request.jwt_trust.issuer.trim().is_empty()
        || request.jwt_trust.audience.trim().is_empty()
        || request.jwt_trust.key_uri.trim().is_empty()
    {
        return Err(
            "sidecar profile identity, operations, and evidence digests are required".to_string(),
        );
    }
    let url = url::Url::parse(&request.runtime_base_url).map_err(|error| error.to_string())?;
    if url.scheme() != "http"
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.host_str(), Some("127.0.0.1" | "::1" | "localhost"))
    {
        return Err("sidecar runtime upstream must be a fixed loopback HTTP origin".to_string());
    }
    let key_server = url::Url::parse(&request.jwt_trust.key_server_url)
        .map_err(|error| format!("invalid sidecar JWT key server URL: {error}"))?;
    if key_server.scheme() != "https"
        || key_server.host_str().is_none()
        || key_server.query().is_some()
        || key_server.fragment().is_some()
        || !key_server.username().is_empty()
        || key_server.password().is_some()
    {
        return Err("sidecar JWT key server must be a fixed HTTPS origin".to_string());
    }
    Ok(())
}

fn digest_bundle(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hasher = Sha256::new();
    for (name, bytes) in files {
        hasher.update((name.len() as u64).to_be_bytes());
        hasher.update(name.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    format!("{:x}", hasher.finalize())
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

const fn default_max_request_time_ms() -> u64 {
    120_000
}

const fn default_connect_timeout_ms() -> u64 {
    5_000
}

const fn default_stream_setup_timeout_ms() -> u64 {
    30_000
}

const fn default_idle_timeout_ms() -> u64 {
    30_000
}

const fn default_max_request_body_bytes() -> usize {
    4 * 1024 * 1024
}

const fn default_max_response_body_bytes() -> usize {
    16 * 1024 * 1024
}

fn default_key_uri() -> String {
    "/oauth2/key".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(value: &SidecarProfileRequest) -> SidecarManifest {
        let bundle = generate_sidecar_bundle(value).unwrap();
        serde_json::from_slice(bundle.get("sidecar-manifest.json").unwrap()).unwrap()
    }

    fn request() -> SidecarProfileRequest {
        SidecarProfileRequest {
            profile_version: "ollama-embedding-v1".to_string(),
            physical_runtime_id: "gpu-node-a/ollama".to_string(),
            runtime_base_url: "http://127.0.0.1:11434".to_string(),
            certificate_identity_sha256: "a".repeat(64),
            isolation_evidence_sha256: "b".repeat(64),
            operations: BTreeSet::from([SidecarOperation::Embeddings]),
            jwt_trust: SidecarJwtTrust {
                issuer: "https://issuer.example".to_string(),
                audience: "model-provider-sidecar".to_string(),
                key_server_url: "https://oauth.example".to_string(),
                key_uri: "/oauth2/key".to_string(),
                key_service_id: None,
                ca_cert_path: Some("/var/run/secrets/sidecar-jwt-ca/ca.crt".to_string()),
            },
            runtime_auth: RuntimeAuth::None,
            max_request_time_ms: 120_000,
            connect_timeout_ms: 5_000,
            stream_setup_timeout_ms: 30_000,
            idle_timeout_ms: 30_000,
            max_request_body_bytes: 4 * 1024 * 1024,
            max_response_body_bytes: 16 * 1024 * 1024,
        }
    }

    #[test]
    fn embedding_profile_is_exact_and_deny_by_default() {
        let bundle = generate_sidecar_bundle(&request()).unwrap();
        let server = String::from_utf8(bundle["server.yml"].clone()).unwrap();
        let security = String::from_utf8(bundle["security.yml"].clone()).unwrap();
        let client = String::from_utf8(bundle["client.yml"].clone()).unwrap();
        assert!(server.contains("tlsCertPath: /var/run/secrets/sidecar-server/tls.crt"));
        assert!(server.contains("tlsKeyPath: /var/run/secrets/sidecar-server/tls.key"));
        assert!(security.contains("bootstrapFromKeyService: true"));
        assert!(security.contains("https://issuer.example"));
        assert!(client.contains("https://oauth.example"));
        let handler: GeneratedHandlerConfig =
            serde_yaml::from_slice(bundle.get("handler.yml").unwrap()).unwrap();
        validate_generated_profile(&handler).unwrap();
        assert!(
            handler
                .paths
                .iter()
                .any(|path| { path.method == "POST" && path.path == "/v1/embeddings" })
        );
        for denied in [
            ("POST", "/api/tags"),
            ("POST", "/api/pull"),
            ("GET", "/v1/embeddings"),
            ("POST", "/v1/chat/completions"),
        ] {
            assert!(
                !handler
                    .paths
                    .iter()
                    .any(|path| { path.method == denied.0 && path.path == denied.1 })
            );
        }
        assert_eq!(handler.default_handlers, vec!["sidecar-deny-chain"]);
    }

    #[test]
    fn empty_or_upstream_selecting_default_chain_is_rejected_by_profile_validator_only() {
        let bundle = generate_sidecar_bundle(&request()).unwrap();
        let mut handler: GeneratedHandlerConfig =
            serde_yaml::from_slice(bundle.get("handler.yml").unwrap()).unwrap();
        handler.default_handlers.clear();
        assert!(validate_generated_profile(&handler).is_err());
        handler.default_handlers = vec!["sidecar-deny-chain".to_string()];
        handler.chains.get_mut("sidecar-deny-chain").unwrap().exec = vec!["proxy".to_string()];
        assert!(validate_generated_profile(&handler).is_err());
    }

    #[test]
    fn non_loopback_runtime_is_rejected() {
        let mut value = request();
        value.runtime_base_url = "http://10.0.0.4:11434".to_string();
        assert!(generate_sidecar_bundle(&value).is_err());
    }

    #[tokio::test]
    async fn upstream_auth_is_replaced_and_identity_never_exposes_runtime_material() {
        let mut value = request();
        value.runtime_auth = RuntimeAuth::Bearer {
            credential_ref: "env://LIGHT_GATEWAY_SIDECAR_RUNTIME_TEST_KEY".to_string(),
        };
        let manifest = manifest(&value);
        // SAFETY: this test owns a uniquely named process environment binding.
        unsafe {
            std::env::set_var(
                "LIGHT_GATEWAY_SIDECAR_RUNTIME_TEST_KEY",
                "runtime-only-secret",
            )
        };
        let mut upstream = RequestHeader::build("POST", b"/v1/embeddings", Some(8)).unwrap();
        upstream
            .insert_header("authorization", "Bearer attacker")
            .unwrap();
        upstream.insert_header("x-api-key", "attacker-key").unwrap();
        upstream.insert_header("connection", "upgrade").unwrap();
        upstream.insert_header("upgrade", "websocket").unwrap();
        upstream
            .insert_header("transfer-encoding", "chunked")
            .unwrap();
        upstream.insert_header("content-length", "8").unwrap();

        apply_sidecar_upstream_headers_for(&manifest, &mut upstream)
            .await
            .unwrap();

        assert_eq!(
            upstream
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer runtime-only-secret")
        );
        assert!(upstream.headers.get("x-api-key").is_none());
        assert!(upstream.headers.get("connection").is_none());
        assert!(upstream.headers.get("upgrade").is_none());
        assert_eq!(
            upstream
                .headers
                .get("transfer-encoding")
                .and_then(|value| value.to_str().ok()),
            Some("chunked")
        );
        assert!(upstream.headers.get("content-length").is_none());
        let identity = String::from_utf8(sidecar_identity_json_for(&manifest).unwrap()).unwrap();
        assert!(!identity.contains("runtime-only-secret"));
        assert!(!identity.contains("LIGHT_GATEWAY_SIDECAR_RUNTIME_TEST_KEY"));
        assert!(!identity.contains(&value.runtime_base_url));
        unsafe { std::env::remove_var("LIGHT_GATEWAY_SIDECAR_RUNTIME_TEST_KEY") };
    }
}
