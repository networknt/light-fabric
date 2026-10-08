//! Verified, pinned Portal View releases. No request-time configuration reads.
use base64::Engine;
use bytes::Bytes;
use light_runtime::RuntimeError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::handler::HandlerConfig;
use crate::resource::{SpaConfig, StaticResourceSet};

pub const PORTAL_SPA_CAPABILITY_VERSION: u32 = 1;
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CacheClass {
    Immutable,
    Revalidate,
}

#[derive(Debug, Clone)]
pub struct SpaRoute {
    pub path: String,
    pub exact: bool,
}

#[derive(Debug)]
pub struct LoadedSpa {
    pub release_dir: PathBuf,
    pub manifest_digest: String,
    pub cache_classes: BTreeMap<String, CacheClass>,
    pub spa_routes: Vec<SpaRoute>,
    pub rendered_index: Bytes,
    pub runtime_config_json: Bytes,
    pub runtime_config_digest: String,
    pub public_base_path: String,
    pub api_base_path: String,
    pub authentication_mode: String,
    pub version: String,
    pub index: String,
    pub loaded_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    manifest_version: u32,
    artifact: String,
    version: String,
    archive: Archive,
    build_commit: String,
    build_timestamp: String,
    minimum_gateway_capability: u32,
    runtime_config_schema_versions: Vec<u32>,
    signature: Signature,
    members: Vec<Member>,
    spa_routes: Vec<ManifestRoute>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    name: String,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Signature {
    algorithm: String,
    key_id: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Member {
    path: String,
    sha256: String,
    size: u64,
    cache_class: CacheClass,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestRoute {
    path: String,
    r#match: String,
}

fn error(host: &str, check: &str) -> RuntimeError {
    RuntimeError::Unsupported(format!("SPA host `{host}`: {check}"))
}
fn resolve(config_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        config_dir.join(path)
    }
}
fn bounded_read(path: &Path, limit: u64) -> Result<Vec<u8>, &'static str> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "file is missing or unreadable")?;
    if !metadata.is_file() {
        return Err("expected a regular file, not a symlink");
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| "file is unreadable")?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "file read failed")?;
    if bytes.len() as u64 > limit {
        return Err("file exceeds size limit");
    }
    Ok(bytes)
}
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn valid_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn safe_member(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.chars().any(|c| c.is_control() || "\\:*?[]".contains(c))
        && s.split('/').all(|p| !p.is_empty() && !p.starts_with('.'))
}
fn walk_dist(
    root: &Path,
    relative: &Path,
    files: &mut BTreeSet<String>,
) -> Result<(), &'static str> {
    let path = root.join(relative);
    let meta = std::fs::symlink_metadata(&path).map_err(|_| "dist member is unreadable")?;
    if meta.file_type().is_symlink() {
        return Err("symlink in dist");
    }
    if meta.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|_| "dist directory is unreadable")? {
            let entry = entry.map_err(|_| "dist directory read failed")?;
            let name = entry.file_name();
            let name = name.to_str().ok_or("non-UTF-8 dist name")?;
            let child = relative.join(name);
            if !safe_member(child.to_str().ok_or("non-UTF-8 dist path")?) {
                return Err("unsafe or dot path in dist");
            }
            walk_dist(root, &child, files)?;
        }
    } else if meta.is_file() {
        files.insert(relative.to_str().ok_or("non-UTF-8 dist name")?.to_string());
    } else {
        return Err("non-regular dist member");
    }
    Ok(())
}

pub fn load_spa(
    host: &str,
    configured_base: &Path,
    spa: &SpaConfig,
    config_dir: &Path,
) -> Result<LoadedSpa, RuntimeError> {
    let fail = |check: &str| error(host, check);
    let manifest_path = resolve(config_dir, &spa.release_manifest);
    let parent = manifest_path
        .parent()
        .ok_or_else(|| fail("releaseManifest has no parent"))?;
    if configured_base.as_os_str() != parent.join("dist").as_os_str()
        || manifest_path.file_name().and_then(|s| s.to_str()) != Some("release-manifest.json")
    {
        return Err(fail(
            "virtual-host base must be <release>/dist next to releaseManifest",
        ));
    }
    let release_dir =
        std::fs::canonicalize(parent).map_err(|_| fail("cannot pin release directory"))?;
    let bytes = bounded_read(&release_dir.join("release-manifest.json"), 4 * 1024 * 1024)
        .map_err(|e| fail(&format!("manifest: {e}")))?;
    let sig = bounded_read(&release_dir.join("release-manifest.sig"), 64)
        .map_err(|e| fail(&format!("signature: {e}")))?;
    if sig.len() != 64 {
        return Err(fail("signature must be exactly 64 bytes"));
    }
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|_| fail("invalid strict manifest structure"))?;
    let key_id = &manifest.signature.key_id;
    if manifest.signature.algorithm != "Ed25519" {
        return Err(fail("signature algorithm must be Ed25519"));
    }
    if key_id.is_empty()
        || key_id.len() > 64
        || !key_id.as_bytes()[0].is_ascii_lowercase() && !key_id.as_bytes()[0].is_ascii_digit()
        || !key_id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
    {
        return Err(fail("invalid keyId"));
    }
    let pem = bounded_read(
        &resolve(config_dir, &spa.release_key_dir).join(format!("{key_id}.pem")),
        4096,
    )
    .map_err(|_| fail("unknown or unreadable trusted keyId"))?;
    let pem = std::str::from_utf8(&pem)
        .map_err(|_| fail("invalid SPKI PEM"))?
        .trim();
    let body = pem
        .strip_prefix("-----BEGIN PUBLIC KEY-----")
        .and_then(|s| s.strip_suffix("-----END PUBLIC KEY-----"))
        .ok_or_else(|| fail("invalid SPKI PEM structure"))?;
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.split_whitespace().collect::<String>())
        .map_err(|_| fail("invalid SPKI base64"))?;
    if der.len() != 44 || der[..12] != SPKI_PREFIX {
        return Err(fail("trusted key is not Ed25519 SPKI"));
    }
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &der[12..])
        .verify(&bytes, &sig)
        .map_err(|_| fail("manifest signature verification failed"))?;
    if manifest.manifest_version != 1 || manifest.artifact != "portal-view" {
        return Err(fail("unsupported manifest version or artifact"));
    }
    if manifest.minimum_gateway_capability > PORTAL_SPA_CAPABILITY_VERSION {
        return Err(fail("unsupported minimumGatewayCapability"));
    }
    if !regex::Regex::new(r"\A[0-9]{8}-[0-9a-f]{12}\z")
        .expect("version regex")
        .is_match(&manifest.version)
        || !regex::Regex::new(r"\A[0-9a-f]{40}\z")
            .expect("commit regex")
            .is_match(&manifest.build_commit)
        || chrono::DateTime::parse_from_rfc3339(&manifest.build_timestamp).is_err()
        || manifest.archive.name != format!("portal-view-{}.zip", manifest.version)
        || !valid_digest(&manifest.archive.sha256)
    {
        return Err(fail("invalid release metadata"));
    }
    let mut cache_classes = BTreeMap::new();
    let hashed_asset =
        regex::Regex::new(r"-[A-Za-z0-9_-]{8,}\.[A-Za-z0-9]+\z").expect("asset regex");
    for member in &manifest.members {
        if !safe_member(&member.path)
            || !valid_digest(&member.sha256)
            || cache_classes
                .insert(member.path.clone(), member.cache_class)
                .is_some()
        {
            return Err(fail("unsafe, duplicate, or malformed manifest member"));
        }
        let hashed = member.path.starts_with("assets/")
            && hashed_asset.is_match(member.path.rsplit('/').next().unwrap_or(""));
        if (member.cache_class == CacheClass::Immutable) != hashed {
            return Err(fail("member cache classification mismatch"));
        }
    }
    let dist = release_dir.join("dist");
    let mut files = BTreeSet::new();
    walk_dist(&dist, Path::new(""), &mut files).map_err(fail)?;
    if files != cache_classes.keys().cloned().collect() {
        return Err(fail("missing or extra dist member"));
    }
    for member in &manifest.members {
        let mut file =
            File::open(dist.join(&member.path)).map_err(|_| fail("member read failed"))?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut buffer = [0u8; 65536];
        loop {
            let n = file
                .read(&mut buffer)
                .map_err(|_| fail("member read failed"))?;
            if n == 0 {
                break;
            }
            size += n as u64;
            hash.update(&buffer[..n]);
        }
        if size != member.size || hex::encode(hash.finalize()) != member.sha256 {
            return Err(fail("member size or SHA-256 mismatch"));
        }
    }
    if !cache_classes.contains_key("portal-config.schema.json")
        || !cache_classes.contains_key(&spa.index)
        || !safe_member(&spa.index)
    {
        return Err(fail("index and schema must be verified members"));
    }
    let schema: serde_json::Value = serde_json::from_slice(
        &bounded_read(&dist.join("portal-config.schema.json"), 4 * 1024 * 1024).map_err(fail)?,
    )
    .map_err(|_| fail("invalid verified schema JSON"))?;
    let document = bounded_read(&resolve(config_dir, &spa.runtime_config), 65536)
        .map_err(|e| fail(&format!("runtime config: {e}")))?;
    let config = validate_runtime_config(&document, &schema).map_err(fail)?;
    if !manifest
        .runtime_config_schema_versions
        .contains(&config.schema_version)
    {
        return Err(fail("runtime schemaVersion unsupported by manifest"));
    }
    let runtime_config_json =
        serde_json::to_vec(&config).map_err(|_| fail("runtime config serialization failed"))?;
    let index =
        std::fs::read_to_string(dist.join(&spa.index)).map_err(|_| fail("index must be UTF-8"))?;
    if spa.base_placeholder.is_empty() || index.matches(&spa.base_placeholder).count() != 1 {
        return Err(fail("index must contain exactly one base placeholder"));
    }
    let base = if config.routing.public_base_path == "/" {
        "/".to_string()
    } else {
        format!("{}/", config.routing.public_base_path)
    };
    let base = base
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    let mut spa_routes = Vec::new();
    for route in manifest.spa_routes {
        if !canonical_path(&route.path, true, false)
            || !matches!(route.r#match.as_str(), "exact" | "prefix")
        {
            return Err(fail("invalid SPA route reservation"));
        }
        spa_routes.push(SpaRoute {
            path: route.path,
            exact: route.r#match == "exact",
        });
    }
    Ok(LoadedSpa {
        release_dir,
        manifest_digest: digest(&bytes),
        cache_classes,
        spa_routes,
        rendered_index: Bytes::from(index.replace(&spa.base_placeholder, &base)),
        runtime_config_digest: digest(&runtime_config_json),
        runtime_config_json: Bytes::from(runtime_config_json),
        public_base_path: config.routing.public_base_path,
        api_base_path: config.routing.api_base_path,
        authentication_mode: config.authentication.mode().to_string(),
        version: manifest.version,
        index: spa.index.clone(),
        loaded_at: chrono::Utc::now().to_rfc3339(),
    })
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(deserialize_with = "deserialize_schema_version")]
    pub schema_version: u32,
    pub routing: Routing,
    pub authentication: Authentication,
    pub features: Features,
    pub external_links: ExternalLinks,
}

fn deserialize_schema_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u32, D::Error> {
    // JSON/JavaScript numbers do not distinguish 1, 1.0 and 1e0.
    let number = serde_json::Number::deserialize(deserializer)?;
    if number.as_f64() == Some(1.0) {
        Ok(1)
    } else {
        number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| serde::de::Error::custom("schemaVersion must be an integer"))
    }
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Routing {
    pub public_base_path: String,
    pub api_base_path: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "mode", deny_unknown_fields)]
pub enum Authentication {
    #[serde(rename = "oauth2")]
    Oauth2 {
        #[serde(rename = "signInUrl")]
        sign_in_url: String,
    },
    #[serde(rename = "entra-sso")]
    Entra {
        #[serde(rename = "tenantId")]
        tenant_id: String,
        #[serde(rename = "clientId")]
        client_id: String,
        #[serde(
            rename = "redirectUri",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        redirect_uri: Option<String>,
        #[serde(
            rename = "postLogoutRedirectUri",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        post_logout_redirect_uri: Option<String>,
    },
}
impl Authentication {
    fn mode(&self) -> &'static str {
        match self {
            Self::Oauth2 { .. } => "oauth2",
            Self::Entra { .. } => "entra-sso",
        }
    }
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Features {
    #[serde(default)]
    pre_registration_enabled: bool,
    #[serde(default)]
    pre_registration_url: String,
    #[serde(default = "api_id")]
    pre_registration_api_id_path: String,
    #[serde(default)]
    pre_registration_service_id_path: String,
    #[serde(default)]
    pre_registration_error_path: String,
    #[serde(default)]
    pre_registration_payload_mapping: BTreeMap<String, String>,
    #[serde(default)]
    tools_sync_enabled: bool,
    #[serde(default)]
    tools_sync_url: String,
    #[serde(default)]
    tools_sync_error_path: String,
    #[serde(default = "wizard_fields")]
    wizard_required_api_fields: Vec<String>,
}
fn api_id() -> String {
    "apiId".into()
}
fn wizard_fields() -> Vec<String> {
    [
        "categoryIds",
        "apiDesc",
        "region",
        "businessGroup",
        "lob",
        "platform",
    ]
    .map(String::from)
    .to_vec()
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalLinks {
    #[serde(default = "documentation")]
    portal_documentation: String,
    #[serde(default = "onboarding")]
    api_onboarding: String,
    #[serde(default = "releases")]
    product_releases: String,
}
fn documentation() -> String {
    "https://doc.lightapi.net".into()
}
fn onboarding() -> String {
    "https://lightapi.net".into()
}
fn releases() -> String {
    "https://lightapi.net/releases".into()
}
fn canonical_path(s: &str, root: bool, empty: bool) -> bool {
    if root && s == "/" || empty && s.is_empty() {
        return true;
    }
    s.len() <= 256
        && s.starts_with('/')
        && s[1..].split('/').all(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._~!$&'()*+,;=:@-".contains(&c))
        })
}
fn strict_url(s: &str, relative: bool, http: bool) -> Result<url::Url, &'static str> {
    let before_query = s.split('?').next().unwrap_or(s);
    if s.chars()
        .any(|c| c.is_whitespace() || c.is_control() || "\\#".contains(c))
        || before_query.contains('@')
    {
        return Err("unsafe raw URL characters");
    }
    let lower = before_query.to_ascii_lowercase();
    if ["%2f", "%5c", "%2e"].iter().any(|p| lower.contains(p)) {
        return Err("unsafe URL path encoding");
    }
    let bytes = s.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        if *c == b'%'
            && (i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit())
        {
            return Err("malformed URL percent escape");
        }
    }
    let is_relative = s.starts_with('/') && !s.starts_with("//");
    let absolute =
        regex::Regex::new(r"\A(https?)://[A-Za-z0-9.-]+(:[0-9]{1,5})?(/[^?]*)?(\?.*)?\z")
            .expect("URL regex");
    if !(relative && is_relative || absolute.is_match(s) && (s.starts_with("https://") || http)) {
        return Err("unsupported URL shape or scheme");
    }
    let raw_path = if is_relative {
        before_query
    } else {
        before_query
            .split_once("://")
            .and_then(|(_, a)| a.find('/').map(|i| &a[i..]))
            .unwrap_or("")
    };
    if raw_path.split('/').any(|p| p == "." || p == "..") {
        return Err("URL dot segment");
    }
    let parsed = url::Url::parse("https://placeholder.invalid")
        .expect("base URL")
        .join(s)
        .map_err(|_| "invalid URL")?;
    if parsed.scheme() == "http"
        && !parsed
            .host_str()
            .is_some_and(|h| h == "localhost" || h.ends_with(".localhost"))
    {
        return Err("HTTP redirect requires localhost");
    }
    Ok(parsed)
}
fn valid_uuid(s: &str) -> bool {
    if !regex::Regex::new(
        r"\A[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\z",
    )
    .expect("UUID regex")
    .is_match(s)
    {
        return false;
    }
    let digits: Vec<_> = s
        .bytes()
        .filter(|c| *c != b'-')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    digits.iter().any(|c| *c != digits[0])
}
pub fn validate_runtime_config(
    document: &[u8],
    schema: &serde_json::Value,
) -> Result<RuntimeConfig, &'static str> {
    // Offline validation must never resolve a remote schema or local file URL.
    fn local_refs(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(map) => map.iter().all(|(key, value)| {
                if matches!(key.as_str(), "$ref" | "$dynamicRef") {
                    value
                        .as_str()
                        .is_some_and(|reference| reference.starts_with('#'))
                } else {
                    local_refs(value)
                }
            }),
            serde_json::Value::Array(items) => items.iter().all(local_refs),
            _ => true,
        }
    }
    if !local_refs(schema) {
        return Err("verified schema must use local references for offline validation");
    }
    let value: serde_json::Value =
        serde_json::from_slice(document).map_err(|_| "invalid runtime JSON")?;
    let validator =
        jsonschema::validator_for(schema).map_err(|_| "invalid verified JSON Schema")?;
    if !validator.is_valid(&value) {
        return Err("runtime JSON Schema validation failed");
    }
    let config: RuntimeConfig =
        serde_json::from_slice(document).map_err(|_| "invalid strict runtime structure")?;
    if config.schema_version != 1 {
        return Err("unsupported runtime schemaVersion");
    }
    if !canonical_path(&config.routing.public_base_path, true, false)
        || !canonical_path(&config.routing.api_base_path, false, true)
    {
        return Err("invalid runtime routing path");
    }
    match &config.authentication {
        Authentication::Oauth2 { sign_in_url } => {
            let parsed = strict_url(sign_in_url, true, false)?;
            let pairs: Vec<_> = parsed.query_pairs().collect();
            if pairs.iter().any(|(k, _)| k == "state" || k == "user_type") {
                return Err("signInUrl contains generated query fields");
            }
            let clients: Vec<_> = pairs.iter().filter(|(k, _)| k == "client_id").collect();
            if clients.len() != 1 || clients[0].1.is_empty() {
                return Err("signInUrl requires one non-empty client_id");
            }
        }
        Authentication::Entra {
            tenant_id,
            client_id,
            redirect_uri,
            post_logout_redirect_uri,
        } => {
            if !valid_uuid(tenant_id) || !valid_uuid(client_id) {
                return Err("invalid or placeholder Entra UUID");
            }
            for redirect in [redirect_uri, post_logout_redirect_uri]
                .into_iter()
                .flatten()
            {
                if !redirect.is_empty() {
                    strict_url(redirect, false, true)?;
                    if redirect.contains('?') {
                        return Err("redirect query is forbidden");
                    }
                }
            }
        }
    }
    for s in [
        &config.features.pre_registration_url,
        &config.features.tools_sync_url,
    ] {
        if s.starts_with('/') || s.is_empty() {
            if !canonical_path(s, true, true) {
                return Err("invalid feature URL path");
            }
        } else {
            strict_url(s, false, false)?;
        }
    }
    for v in config.features.pre_registration_payload_mapping.values() {
        let v = v.to_ascii_lowercase();
        if ["secret", "password", "token"]
            .iter()
            .any(|p| v.contains(p))
        {
            return Err("secret-shaped mapping value");
        }
    }
    for link in [
        &config.external_links.portal_documentation,
        &config.external_links.api_onboarding,
        &config.external_links.product_releases,
    ] {
        if !link.to_ascii_lowercase().starts_with("https://")
            || url::Url::parse(link).map_or(true, |u| u.scheme() != "https")
        {
            return Err("external link requires absolute HTTPS URL");
        }
    }
    Ok(config)
}

pub fn join_mount(mount: &str, path: &str) -> String {
    let full = format!(
        "{}/{}",
        mount.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    if full.len() > 1 {
        full.trim_end_matches('/').to_string()
    } else {
        full
    }
}
fn segment_prefix(a: &str, b: &str) -> bool {
    a == "/" || a == b || b.strip_prefix(a).is_some_and(|r| r.starts_with('/'))
}

/// Pure WP9.4 dependency only; no startup/reload integration or dispatch changes.
pub fn validate_spa_guard_collisions(
    handler: &HandlerConfig,
    statics: &StaticResourceSet,
) -> Result<(), RuntimeError> {
    let mut chains = handler.chains.clone();
    for (name, chain) in &handler.additional_chains {
        // Published configs already contain their materialized additions.
        if let Some(existing) = chains.get(name) {
            if existing.exec != chain.exec {
                return Err(error("handler", "duplicate additional chain"));
            }
        } else {
            chains.insert(name.clone(), chain.clone());
        }
    }
    fn contains_guard(
        exec: &[String],
        chains: &BTreeMap<String, crate::handler::HandlerChain>,
        visiting: &mut BTreeSet<String>,
    ) -> Result<bool, RuntimeError> {
        let mut found = false;
        for id in exec {
            if let Some(chain) = chains.get(id) {
                if !visiting.insert(id.clone()) {
                    return Err(error("handler", "recursive exec chain"));
                }
                found |= contains_guard(&chain.exec, chains, visiting)?;
                visiting.remove(id);
            } else {
                found |= id == "not-found";
            }
        }
        Ok(found)
    }
    for path in handler.paths.iter().chain(&handler.additional_paths) {
        let Some(prefix) = path.path.strip_suffix("/*") else {
            continue;
        };
        if !contains_guard(&path.exec, &chains, &mut BTreeSet::new())? {
            continue;
        }
        let guard = if prefix.is_empty() { "/" } else { prefix };
        let mut guards = vec![guard.to_string()];
        if handler.base_path != "/" {
            guards.push(join_mount(&handler.base_path, guard));
        }
        for (host, site) in statics
            .virtual_hosts
            .iter()
            .chain(&statics.wildcard_virtual_hosts)
        {
            let Some(spa) = &site.spa else {
                continue;
            };
            let mut reservations: Vec<_> = spa
                .spa_routes
                .iter()
                .map(|r| (join_mount(&site.path, &r.path), r.exact))
                .collect();
            reservations.extend([
                (join_mount(&site.path, "portal-config.json"), true),
                (site.path.clone(), true),
                (format!("{}/", site.path.trim_end_matches('/')), true),
            ]);
            for g in &guards {
                for (r, exact) in &reservations {
                    if segment_prefix(g, r) || !exact && segment_prefix(r, g) {
                        return Err(error(
                            host,
                            &format!("guard `{g}` conflicts with reservation `{r}`"),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;
    use ring::signature::KeyPair;
    pub const SCHEMA: &str = r####"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Portal runtime configuration, schema version 1",
  "$comment": "Structural validation only. Also run the handwritten semantic validator: URL parsing and query inspection, UUID placeholder rejection, and secret-shaped field/mapping-value checks. Defaults are annotations; validators apply them. The loader enforces the 64 KiB document limit.",
  "type": "object",
  "additionalProperties": false,
  "required": [
    "schemaVersion",
    "routing",
    "authentication",
    "features",
    "externalLinks"
  ],
  "properties": {
    "schemaVersion": {
      "type": "integer",
      "const": 1
    },
    "routing": {
      "type": "object",
      "additionalProperties": false,
      "required": [
        "publicBasePath",
        "apiBasePath"
      ],
      "properties": {
        "publicBasePath": {
          "anyOf": [
            {
              "const": "/"
            },
            {
              "$ref": "#/$defs/path"
            }
          ]
        },
        "apiBasePath": {
          "anyOf": [
            {
              "const": ""
            },
            {
              "$ref": "#/$defs/path"
            }
          ]
        }
      }
    },
    "authentication": {
      "oneOf": [
        {
          "type": "object",
          "additionalProperties": false,
          "required": [
            "mode",
            "signInUrl"
          ],
          "properties": {
            "mode": {
              "const": "oauth2"
            },
            "signInUrl": {
              "$ref": "#/$defs/signInUrl"
            }
          }
        },
        {
          "type": "object",
          "additionalProperties": false,
          "required": [
            "mode",
            "tenantId",
            "clientId"
          ],
          "properties": {
            "mode": {
              "const": "entra-sso"
            },
            "tenantId": {
              "$ref": "#/$defs/uuid"
            },
            "clientId": {
              "$ref": "#/$defs/uuid"
            },
            "redirectUri": {
              "$ref": "#/$defs/redirect"
            },
            "postLogoutRedirectUri": {
              "$ref": "#/$defs/redirect"
            }
          }
        }
      ]
    },
    "features": {
      "type": "object",
      "additionalProperties": false,
      "required": [],
      "properties": {
        "preRegistrationApiIdPath": {
          "type": "string",
          "maxLength": 2048,
          "default": "apiId"
        },
        "preRegistrationServiceIdPath": {
          "type": "string",
          "maxLength": 2048,
          "default": ""
        },
        "preRegistrationErrorPath": {
          "type": "string",
          "maxLength": 2048,
          "default": ""
        },
        "toolsSyncErrorPath": {
          "type": "string",
          "maxLength": 2048,
          "default": ""
        },
        "preRegistrationEnabled": {
          "type": "boolean",
          "default": false
        },
        "toolsSyncEnabled": {
          "type": "boolean",
          "default": false
        },
        "preRegistrationUrl": {
          "$ref": "#/$defs/featureUrl",
          "default": ""
        },
        "toolsSyncUrl": {
          "$ref": "#/$defs/featureUrl",
          "default": ""
        },
        "preRegistrationPayloadMapping": {
          "type": "object",
          "maxProperties": 64,
          "propertyNames": {
            "type": "string",
            "maxLength": 2048
          },
          "additionalProperties": {
            "type": "string",
            "maxLength": 2048
          },
          "default": {},
          "$comment": "Keys are data. Secret/password/token restrictions on values are semantic validation."
        },
        "wizardRequiredApiFields": {
          "type": "array",
          "maxItems": 32,
          "items": {
            "type": "string",
            "maxLength": 64
          },
          "default": [
            "categoryIds",
            "apiDesc",
            "region",
            "businessGroup",
            "lob",
            "platform"
          ]
        }
      }
    },
    "externalLinks": {
      "type": "object",
      "additionalProperties": false,
      "required": [],
      "properties": {
        "portalDocumentation": {
          "type": "string",
          "maxLength": 2048,
          "pattern": "^[hH][tT][tT][pP][sS]://",
          "format": "uri",
          "default": "https://doc.lightapi.net"
        },
        "apiOnboarding": {
          "type": "string",
          "maxLength": 2048,
          "pattern": "^[hH][tT][tT][pP][sS]://",
          "format": "uri",
          "default": "https://lightapi.net"
        },
        "productReleases": {
          "type": "string",
          "maxLength": 2048,
          "pattern": "^[hH][tT][tT][pP][sS]://",
          "format": "uri",
          "default": "https://lightapi.net/releases"
        }
      }
    }
  },
  "$defs": {
    "path": {
      "type": "string",
      "maxLength": 256,
      "pattern": "^(/[A-Za-z0-9._~!$&'()*+,;=:@-]+)+$",
      "not": {
        "anyOf": [
          {
            "pattern": "(^|/)\\.{1,2}(/|$)"
          },
          {
            "pattern": "[\\s\\u0000-\\u001f\\u007f]"
          }
        ]
      }
    },
    "rawUrl": {
      "allOf": [
        {
          "not": {
            "pattern": "[\\\\\\s\\u0000-\\u001f\\u007f#]"
          }
        },
        {
          "not": {
            "pattern": "^[^?]*@"
          }
        },
        {
          "not": {
            "pattern": "^[^?]*%([2][fFeE]|[5][cC])"
          }
        },
        {
          "not": {
            "pattern": "%(?![0-9a-fA-F]{2})"
          }
        },
        {
          "not": {
            "pattern": "^(https?://[^/?]+)?/([^?]*/)?\\.{1,2}(/|\\?|$)"
          }
        }
      ]
    },
    "httpsUrl": {
      "type": "string",
      "maxLength": 2048,
      "allOf": [
        {
          "$ref": "#/$defs/rawUrl"
        },
        {
          "pattern": "^https://[A-Za-z0-9.-]+(:[0-9]{1,5})?(/[^?]*)?(\\?.*)?$"
        }
      ]
    },
    "signInUrl": {
      "type": "string",
      "maxLength": 2048,
      "minLength": 1,
      "allOf": [
        {
          "$ref": "#/$defs/rawUrl"
        },
        {
          "anyOf": [
            {
              "pattern": "^/(?![/\\\\])[^?]*(\\?.*)?$"
            },
            {
              "pattern": "^https://[A-Za-z0-9.-]+(:[0-9]{1,5})?(/[^?]*)?(\\?.*)?$"
            }
          ]
        }
      ],
      "$comment": "Exactly one non-empty decoded client_id and no decoded state/user_type keys are required by semantic validation."
    },
    "uuid": {
      "type": "string",
      "maxLength": 36,
      "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
      "$comment": "No version restriction. Repeated-digit placeholders (including nil/max) are rejected semantically.",
      "minLength": 36
    },
    "redirect": {
      "anyOf": [
        {
          "const": ""
        },
        {
          "type": "string",
          "maxLength": 2048,
          "allOf": [
            {
              "$ref": "#/$defs/rawUrl"
            },
            {
              "pattern": "^(https://[A-Za-z0-9.-]+|http://([A-Za-z0-9.-]*\\.)?[lL][oO][cC][aA][lL][hH][oO][sS][tT])(:[0-9]{1,5})?(/[^?]*)?$"
            },
            {
              "not": {
                "pattern": "\\?"
              }
            }
          ]
        }
      ]
    },
    "featureUrl": {
      "anyOf": [
        {
          "const": ""
        },
        {
          "const": "/"
        },
        {
          "$ref": "#/$defs/path"
        },
        {
          "$ref": "#/$defs/httpsUrl"
        }
      ]
    }
  }
}
"####;

    /// Fixture helper compiled only for local tests/test-support.
    pub struct TestRelease {
        pub root: PathBuf,
        pub runtime: PathBuf,
        pub keys: PathBuf,
        pub key: ring::signature::Ed25519KeyPair,
    }
    impl TestRelease {
        pub fn new(root: &Path, version: &str) -> Self {
            std::fs::create_dir_all(root.join("dist/assets")).unwrap();
            std::fs::create_dir_all(root.join("dist/sub")).unwrap();
            std::fs::create_dir_all(root.join("dist/empty")).unwrap();
            let keys = root.join("keys");
            std::fs::create_dir_all(&keys).unwrap();
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
            std::fs::write(root.join("test-only-private.pkcs8"), pkcs8.as_ref()).unwrap();
            let key = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
            let mut der = SPKI_PREFIX.to_vec();
            der.extend_from_slice(key.public_key().as_ref());
            std::fs::write(
                keys.join("test-key.pem"),
                format!(
                    "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
                    base64::engine::general_purpose::STANDARD.encode(der)
                ),
            )
            .unwrap();
            std::fs::write(
                root.join("dist/index.html"),
                format!("<base href=\"__PORTAL_BASE_HREF__\" />{version}"),
            )
            .unwrap();
            std::fs::write(root.join("dist/assets/app-m7DsXjYC.js"), version).unwrap();
            std::fs::write(root.join("dist/VERSION"), version).unwrap();
            std::fs::write(root.join("dist/sub/index.html"), "subdirectory unchanged").unwrap();
            std::fs::write(root.join("dist/portal-config.schema.json"), SCHEMA).unwrap();
            let runtime = root.join("runtime.json");
            std::fs::write(&runtime, r#"{"schemaVersion":1,"routing":{"publicBasePath":"/","apiBasePath":""},"authentication":{"mode":"oauth2","signInUrl":"https://signin.example?client_id=public"},"features":{},"externalLinks":{}}"#).unwrap();
            let result = Self {
                root: root.to_path_buf(),
                runtime,
                keys,
                key,
            };
            result.resign(version, |_| {});
            result
        }
        pub fn copy_test_key(&self) -> ring::signature::Ed25519KeyPair {
            ring::signature::Ed25519KeyPair::from_pkcs8(
                &std::fs::read(self.root.join("test-only-private.pkcs8")).unwrap(),
            )
            .unwrap()
        }
        pub fn resign(&self, version: &str, mutate: impl FnOnce(&mut serde_json::Value)) {
            let mut paths = BTreeSet::new();
            walk_dist(&self.root.join("dist"), Path::new(""), &mut paths).unwrap();
            let members: Vec<_> = paths.iter().map(|path| { let b = std::fs::read(self.root.join("dist").join(path)).unwrap();
            serde_json::json!({"path":path,"sha256":digest(&b),"size":b.len(),"cacheClass":if path.starts_with("assets/") {"immutable"} else {"revalidate"}}) }).collect();
            let mut manifest = serde_json::json!({"manifestVersion":1,"artifact":"portal-view","version":version,"archive":{"name":format!("portal-view-{version}.zip"),"sha256":"0".repeat(64)},"buildCommit":"1".repeat(40),"buildTimestamp":"2026-10-08T00:00:00Z","minimumGatewayCapability":1,"runtimeConfigSchemaVersions":[1],"signature":{"algorithm":"Ed25519","keyId":"test-key"},"members":members,"spaRoutes":[{"path":"/","match":"exact"},{"path":"/app","match":"prefix"},{"path":"/redirect","match":"exact"},{"path":"/device","match":"exact"}]});
            mutate(&mut manifest);
            let bytes = serde_json::to_vec(&manifest).unwrap();
            std::fs::write(self.root.join("release-manifest.json"), &bytes).unwrap();
            std::fs::write(
                self.root.join("release-manifest.sig"),
                self.key.sign(&bytes).as_ref(),
            )
            .unwrap();
        }
        pub fn spa(&self) -> SpaConfig {
            SpaConfig {
                enabled: true,
                index: "index.html".into(),
                runtime_config: self.runtime.to_str().unwrap().into(),
                release_manifest: self
                    .root
                    .join("release-manifest.json")
                    .to_str()
                    .unwrap()
                    .into(),
                release_key_dir: self.keys.to_str().unwrap().into(),
                base_placeholder: "__PORTAL_BASE_HREF__".into(),
            }
        }
        pub fn load(&self) -> Result<LoadedSpa, RuntimeError> {
            load_spa(
                "test.example",
                &self.root.join("dist"),
                &self.spa(),
                Path::new(""),
            )
        }
        pub fn site(&self, mount: &str) -> crate::resource::StaticSite {
            let loaded = self.load().unwrap();
            crate::resource::StaticSite {
                path: mount.into(),
                base: loaded.release_dir.join("dist"),
                spa: Some(std::sync::Arc::new(loaded)),
                prefix: true,
                transfer_min_size: 10_245_760,
                directory_listing_enabled: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{SCHEMA, TestRelease};
    use super::*;
    use crate::resource::StaticResolution;
    use ring::signature::KeyPair;
    const V: &str = "20261008-0123456789ab";
    fn fixture() -> (tempfile::TempDir, TestRelease) {
        let t = tempfile::tempdir().unwrap();
        let r = TestRelease::new(t.path(), V);
        (t, r)
    }
    fn generated(
        site: &crate::resource::StaticSite,
        path: &str,
    ) -> crate::resource::GeneratedResponse {
        match site.resolve(path) {
            StaticResolution::Generated(r) => r,
            other => panic!("expected generated: {other:?}"),
        }
    }
    #[test]
    fn spa_runtime_fixture_parity_and_defaults() {
        let schema = serde_json::from_str(SCHEMA).unwrap();
        let mut valid = 0;
        let mut invalid = 0;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/portal-config");
        for class in ["valid", "invalid"] {
            for entry in std::fs::read_dir(root.join(class)).unwrap() {
                let entry = entry.unwrap();
                let bytes = std::fs::read(entry.path()).unwrap();
                let result = validate_runtime_config(&bytes, &schema);
                assert_eq!(
                    result.is_ok(),
                    class == "valid",
                    "{}: {:?}",
                    entry.path().display(),
                    result
                );
                if class == "valid" {
                    valid += 1;
                    let source: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    let out = serde_json::to_value(result.unwrap()).unwrap();
                    let defaults = serde_json::json!({"preRegistrationEnabled":false,"preRegistrationUrl":"","preRegistrationApiIdPath":"apiId","preRegistrationServiceIdPath":"","preRegistrationErrorPath":"","preRegistrationPayloadMapping":{},"toolsSyncEnabled":false,"toolsSyncUrl":"","toolsSyncErrorPath":"","wizardRequiredApiFields":["categoryIds","apiDesc","region","businessGroup","lob","platform"]});
                    let links = serde_json::json!({"portalDocumentation":"https://doc.lightapi.net","apiOnboarding":"https://lightapi.net","productReleases":"https://lightapi.net/releases"});
                    let mut expected = source.clone();
                    for (group, fallback) in [("features", defaults), ("externalLinks", links)] {
                        for (k, v) in fallback.as_object().unwrap() {
                            if expected[group].get(k).is_none() {
                                expected[group][k] = v.clone();
                            }
                        }
                    }
                    assert_eq!(out, expected);
                } else {
                    invalid += 1;
                }
            }
        }
        assert_eq!((valid, invalid), (6, 27));
    }
    #[test]
    fn spa_root_generated_config_cache_and_subdirectory() {
        let (_t, r) = fixture();
        let site = r.site("/");
        for p in ["/", "/index.html", "/app/dashboard", "/app/a/b/c"] {
            let out = generated(&site, p);
            assert!(
                std::str::from_utf8(&out.body)
                    .unwrap()
                    .contains("<base href=\"/\" />")
            );
            assert_eq!(out.cache_control, "no-cache");
            assert!(
                out.headers
                    .contains(&("Content-Security-Policy".into(), "base-uri 'self'".into()))
            );
        }
        let config = generated(&site, "/portal-config.json");
        assert_eq!(config.content_type, "application/json");
        assert_eq!(config.cache_control, "no-store");
        let spa = site.spa.as_ref().unwrap();
        assert!(config.headers.contains(&(
            "ETag".into(),
            format!("\"sha256-{}\"", spa.runtime_config_digest)
        )));
        assert!(
            config
                .headers
                .contains(&("X-Content-Type-Options".into(), "nosniff".into()))
        );
        assert!(config.headers.contains(&(
            "X-Portal-Config-Digest".into(),
            spa.runtime_config_digest.clone()
        )));
        assert!(config.headers.contains(&(
            "X-Portal-Release-Digest".into(),
            spa.manifest_digest.clone()
        )));
        assert_eq!(config.body, generated(&site, "/portal-config.json").body);
        assert!(!r.root.join("dist/portal-config.json").exists());
        std::fs::write(&r.runtime, b"invalid changed external source").unwrap();
        assert_eq!(config.body, generated(&site, "/portal-config.json").body);
        for (p, cache) in [
            (
                "/assets/app-m7DsXjYC.js",
                "public, max-age=31536000, immutable",
            ),
            ("/VERSION", "no-cache"),
            ("/portal-config.schema.json", "no-cache"),
            ("/sub", "no-cache"),
        ] {
            match site.resolve(p) {
                StaticResolution::File(f) => {
                    assert_eq!(f.cache_control, cache);
                    if p == "/sub" {
                        assert_eq!(
                            std::fs::read_to_string(f.path).unwrap(),
                            "subdirectory unchanged"
                        );
                    }
                }
                o => panic!("{o:?}"),
            }
        }
        assert_eq!(site.resolve("/empty"), StaticResolution::NotFound);
        assert_eq!(
            site.resolve("/assets/missing.js"),
            StaticResolution::NotFound
        );
        assert_eq!(site.resolve("/.env"), StaticResolution::Forbidden);
    }
    #[test]
    fn spa_prefixed_mount_and_html_escape() {
        let (_t, r) = fixture();
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&r.runtime).unwrap()).unwrap();
        config["routing"]["publicBasePath"] = "/namespace-dev/service/ai/portal".into();
        std::fs::write(&r.runtime, serde_json::to_vec(&config).unwrap()).unwrap();
        let site = r.site("/ai/portal");
        assert!(
            std::str::from_utf8(&generated(&site, "/ai/portal/app/a/b/c").body)
                .unwrap()
                .contains("/namespace-dev/service/ai/portal/")
        );
        assert_eq!(
            generated(&site, "/ai/portal/portal-config.json").cache_control,
            "no-store"
        );
        assert_eq!(site.resolve("/ai/portalx"), StaticResolution::NotFound);
        config["routing"]["publicBasePath"] = "/a&'b".into();
        std::fs::write(&r.runtime, serde_json::to_vec(&config).unwrap()).unwrap();
        assert!(
            std::str::from_utf8(&r.load().unwrap().rendered_index)
                .unwrap()
                .contains("/a&amp;&#39;b/")
        );
    }
    macro_rules! failure {
        ($name:ident, $mutate:expr, $check:expr) => {
            #[test]
            fn $name() {
                let (_t, r) = fixture();
                ($mutate)(&r);
                let e = r.load().unwrap_err().to_string();
                assert!(e.contains("test.example") && e.contains($check), "{e}");
            }
        };
    }
    failure!(
        spa_load_bad_signature,
        |r: &TestRelease| std::fs::write(r.root.join("release-manifest.sig"), [0; 64]).unwrap(),
        "signature verification"
    );
    failure!(
        spa_load_unknown_key,
        |r: &TestRelease| r.resign(V, |m| m["signature"]["keyId"] = "unknown".into()),
        "trusted keyId"
    );
    failure!(
        spa_load_extra_file,
        |r: &TestRelease| std::fs::write(r.root.join("dist/extra"), "extra").unwrap(),
        "extra dist member"
    );
    failure!(
        spa_load_missing_member,
        |r: &TestRelease| std::fs::remove_file(r.root.join("dist/VERSION")).unwrap(),
        "missing or extra"
    );
    failure!(
        spa_load_changed_member,
        |r: &TestRelease| std::fs::write(r.root.join("dist/VERSION"), "changed").unwrap(),
        "SHA-256 mismatch"
    );
    failure!(
        spa_load_two_placeholders,
        |r: &TestRelease| {
            std::fs::write(
                r.root.join("dist/index.html"),
                "__PORTAL_BASE_HREF____PORTAL_BASE_HREF__",
            )
            .unwrap();
            r.resign(V, |_| {});
        },
        "exactly one"
    );
    failure!(
        spa_load_zero_placeholders,
        |r: &TestRelease| {
            std::fs::write(r.root.join("dist/index.html"), "no placeholder").unwrap();
            r.resign(V, |_| {});
        },
        "exactly one"
    );
    failure!(
        spa_load_invalid_runtime,
        |r: &TestRelease| std::fs::write(&r.runtime, "{}").unwrap(),
        "runtime JSON Schema"
    );
    failure!(
        spa_load_unsupported_schema_version,
        |r: &TestRelease| r.resign(V, |m| m["runtimeConfigSchemaVersions"] =
            serde_json::json!([2])),
        "unsupported by manifest"
    );
    failure!(
        spa_load_unsupported_capability,
        |r: &TestRelease| r.resign(V, |m| m["minimumGatewayCapability"] = 2.into()),
        "minimumGatewayCapability"
    );
    failure!(
        spa_load_short_signature,
        |r: &TestRelease| std::fs::write(r.root.join("release-manifest.sig"), [0; 63]).unwrap(),
        "exactly 64"
    );
    failure!(
        spa_load_wrong_algorithm,
        |r: &TestRelease| r.resign(V, |m| m["signature"]["algorithm"] = "RSA".into()),
        "algorithm"
    );
    failure!(
        spa_load_duplicate_member,
        |r: &TestRelease| r.resign(V, |m| {
            let a = m["members"][0].clone();
            m["members"].as_array_mut().unwrap().push(a);
        }),
        "duplicate"
    );
    failure!(
        spa_load_unsafe_member,
        |r: &TestRelease| r.resign(V, |m| m["members"][0]["path"] = "../escape".into()),
        "unsafe"
    );
    failure!(
        spa_load_dot_file,
        |r: &TestRelease| std::fs::write(r.root.join("dist/.env"), "x").unwrap(),
        "dot path"
    );
    failure!(
        spa_load_invalid_pem,
        |r: &TestRelease| std::fs::write(r.keys.join("test-key.pem"), "invalid PEM").unwrap(),
        "SPKI PEM"
    );
    failure!(
        spa_load_invalid_route,
        |r: &TestRelease| r.resign(V, |m| m["spaRoutes"][0]["match"] = "glob".into()),
        "route reservation"
    );
    failure!(
        spa_load_invalid_key_id,
        |r: &TestRelease| r.resign(V, |m| m["signature"]["keyId"] = "../key".into()),
        "invalid keyId"
    );
    failure!(
        spa_load_strict_manifest,
        |r: &TestRelease| r.resign(V, |m| m["unexpected"] = true.into()),
        "strict manifest"
    );
    #[test]
    fn spa_load_wrong_base() {
        let (_t, r) = fixture();
        let e = load_spa("test.example", &r.root, &r.spa(), Path::new("")).unwrap_err();
        assert!(e.to_string().contains("base must"));
    }
    #[cfg(unix)]
    failure!(
        spa_load_symlink,
        |r: &TestRelease| std::os::unix::fs::symlink("VERSION", r.root.join("dist/link")).unwrap(),
        "symlink"
    );
    #[cfg(unix)]
    failure!(
        spa_load_non_utf8,
        |r: &TestRelease| {
            use std::os::unix::ffi::OsStringExt;
            std::fs::write(
                r.root
                    .join("dist")
                    .join(std::ffi::OsString::from_vec(vec![255])),
                "x",
            )
            .unwrap();
        },
        "non-UTF-8"
    );
    #[test]
    fn spa_collision_reservations_chains_alias_and_boundaries() {
        let (_t, r) = fixture();
        let mut statics = StaticResourceSet::empty();
        statics
            .virtual_hosts
            .insert("test.example".into(), r.site("/"));
        let mut handler = HandlerConfig::default();
        for (path, ok) in [
            ("/portal/*", true),
            ("/chat/*", true),
            ("/application/*", true),
            ("/app/*", false),
            ("/redirect/*", false),
            ("/*", false),
            ("/app/admin/*", false),
            ("/portal-config.json/*", false),
            ("/device/*", false),
            ("/appx/*", true),
        ] {
            handler.paths = vec![crate::HandlerPath {
                path: path.into(),
                method: "*".into(),
                exec: vec!["not-found".into()],
            }];
            assert_eq!(
                validate_spa_guard_collisions(&handler, &statics).is_ok(),
                ok,
                "{path}"
            );
        }
        statics
            .virtual_hosts
            .insert("test.example".into(), r.site("/ai/portal"));
        for (path, ok) in [("/app/*", true), ("/ai/portal/*", false), ("/ai/*", false)] {
            handler.paths[0].path = path.into();
            assert_eq!(
                validate_spa_guard_collisions(&handler, &statics).is_ok(),
                ok,
                "{path}"
            );
        }
        handler.base_path = "/bff".into();
        handler.paths[0].path = "/app/*".into();
        statics
            .virtual_hosts
            .insert("test.example".into(), r.site("/"));
        assert!(validate_spa_guard_collisions(&handler, &statics).is_err());
        statics
            .virtual_hosts
            .insert("test.example".into(), r.site("/bff"));
        assert!(validate_spa_guard_collisions(&handler, &statics).is_err());
        handler.paths[0].exec = vec!["outer".into()];
        handler.chains.insert(
            "outer".into(),
            crate::HandlerChain {
                exec: vec!["inner".into()],
            },
        );
        handler.additional_chains.insert(
            "inner".into(),
            crate::HandlerChain {
                exec: vec!["cors".into(), "not-found".into()],
            },
        );
        assert!(validate_spa_guard_collisions(&handler, &statics).is_err());
        handler.chains.get_mut("outer").unwrap().exec = vec!["outer".into()];
        assert!(
            validate_spa_guard_collisions(&handler, &statics)
                .unwrap_err()
                .to_string()
                .contains("recursive")
        );
        handler.chains.clear();
        handler.additional_chains.clear();
        handler.paths.clear();
        handler.additional_paths = vec![crate::HandlerPath {
            path: "/app/*".into(),
            method: "GET".into(),
            exec: vec!["not-found".into()],
        }];
        assert!(validate_spa_guard_collisions(&handler, &statics).is_err());
    }
    #[test]
    fn spa_runtime_semantic_edges_and_mapping_key_exception() {
        let (_t, r) = fixture();
        let schema = serde_json::from_str(SCHEMA).unwrap();
        let raw = std::fs::read_to_string(&r.runtime)
            .unwrap()
            .replace("schemaVersion\":1", "schemaVersion\":1.0");
        assert!(validate_runtime_config(raw.as_bytes(), &schema).is_ok());
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        value["features"]["preRegistrationPayloadMapping"] =
            serde_json::json!({"secret":"apiId","__proto__":"safe","constructor":"data"});
        assert!(validate_runtime_config(&serde_json::to_vec(&value).unwrap(), &schema).is_ok());
        value["features"]["preRegistrationPayloadMapping"]["secret"] = "accessToken".into();
        assert!(
            validate_runtime_config(&serde_json::to_vec(&value).unwrap(), &schema)
                .unwrap_err()
                .contains("mapping value")
        );
        value["features"]["preRegistrationPayloadMapping"] = serde_json::json!({});
        for url in [
            "https://signin.example?client_id=x&client%5fid=y",
            "https://signin.example?client_id=x&%73tate=hidden",
            "https://signin.example?client_id=x&user%5ftype=E",
            "https://signin.example:99999?client_id=x",
        ] {
            value["authentication"]["signInUrl"] = url.into();
            assert!(
                validate_runtime_config(&serde_json::to_vec(&value).unwrap(), &schema).is_err(),
                "{url}"
            );
        }
        value["authentication"]["signInUrl"] =
            "https://signin.example?client_id=x&data=%2f%2e%5c".into();
        assert!(validate_runtime_config(&serde_json::to_vec(&value).unwrap(), &schema).is_ok());
        let remote = serde_json::json!({"$ref":"http://127.0.0.1:1/never-fetch"});
        assert!(
            validate_runtime_config(raw.as_bytes(), &remote)
                .unwrap_err()
                .contains("local references")
        );
    }
    failure!(
        spa_load_runtime_document_limit,
        |r: &TestRelease| std::fs::write(&r.runtime, vec![b' '; 65537]).unwrap(),
        "runtime config: file exceeds"
    );
    failure!(
        spa_load_manifest_limit,
        |r: &TestRelease| std::fs::write(
            r.root.join("release-manifest.json"),
            vec![b' '; 4 * 1024 * 1024 + 1]
        )
        .unwrap(),
        "manifest: file exceeds"
    );
    failure!(
        spa_load_oversized_signature,
        |r: &TestRelease| std::fs::write(r.root.join("release-manifest.sig"), [0; 65]).unwrap(),
        "signature: file exceeds"
    );
    failure!(
        spa_load_size_mismatch,
        |r: &TestRelease| r.resign(V, |m| m["members"][0]["size"] = 0.into()),
        "size or SHA-256"
    );
    failure!(
        spa_load_wrong_artifact,
        |r: &TestRelease| r.resign(V, |m| m["artifact"] = "other".into()),
        "artifact"
    );
    failure!(
        spa_load_wrong_spki,
        |r: &TestRelease| {
            let mut der = vec![0; 44];
            der[12..].copy_from_slice(r.key.public_key().as_ref());
            std::fs::write(
                r.keys.join("test-key.pem"),
                format!(
                    "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----",
                    base64::engine::general_purpose::STANDARD.encode(der)
                ),
            )
            .unwrap();
        },
        "Ed25519 SPKI"
    );
}
