//! `startup.yml` (the standard file) and `cli.yml` (the CLI's own settings).
//!
//! `startup.yml` has the same shape in every app. The CLI uses the environment tag (which scopes
//! where its state is kept), the CA bundle that verifies the servers, timeouts, and the config
//! server to ask for its settings (`configServerUri`, with `host` and `serviceId`). Its optional
//! `authorization` is the CLI's application token: a public identifier for the platform services
//! that ask which application is calling (the config server today; controller-rs when the CLI
//! registers with it). It is sent to those and **never** to the Gateway or light-oauth, it reads
//! non-secret settings, and it is not an identity, because an open, downloadable program can hold none.
//!
//! Everything specific to the CLI lives in `cli.yml`, a template of `${cli.<property>:<default>}`
//! placeholders that `config-loader` resolves the same way it does for every other app: an
//! environment variable (the name upper-cased with `.` and `-` replaced by `_`, for example
//! `CLI_GATEWAYURI`), then the config server's `values.yml` (the key is the full placeholder name,
//! `cli.gatewayUri`), then the default.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use config_loader::ConfigLoader;
use serde::Deserialize;
use serde_yaml::Mapping;

use crate::error::CliError;

/// A credential. Its `Debug` output is redacted so it cannot reach a log.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Accept `Bearer <token>` or a bare token, as the runtime does.
    pub fn from_authorization(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        // Nothing, or only the scheme with no token after it.
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("bearer") {
            return None;
        }
        Some(match trimmed.split_once(char::is_whitespace) {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => {
                Secret(rest.trim().to_string())
            }
            _ => Secret(trimmed.to_string()),
        })
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawStartup {
    host: Option<String>,
    service_id: Option<String>,
    env_tag: Option<String>,
    config_server_uri: Option<String>,
    authorization: Option<String>,
    bootstrap_ca_cert_path: Option<String>,
    /// Milliseconds.
    timeout: Option<u64>,
    /// Milliseconds.
    connect_timeout: Option<u64>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// What `startup.yml` provides: the environment, the CA bundle, timeouts and where local state
/// lives.
#[derive(Debug)]
pub struct CliConfig {
    /// Sent to the config server as `host`.
    pub host: Option<String>,
    /// Sent to the config server as `serviceId`.
    pub service_id: Option<String>,
    pub env_tag: String,
    /// Which config server to ask for settings. `None` means use `cli.yml` and the environment.
    pub config_server_uri: Option<String>,
    /// Read access to non-secret settings on the config server. Sent there and nowhere else.
    pub config_token: Option<Secret>,
    /// CA bundle (PEM) that verifies light-oauth and the Gateway, as well as the system roots.
    pub ca_bundle: Option<PathBuf>,
    /// Timeouts for requests to light-oauth.
    pub timeout: Duration,
    pub connect_timeout: Duration,
    /// Directory holding `startup.yml` and `cli.yml`.
    pub config_dir: PathBuf,
    /// Where the login lives for this environment.
    pub store_dir: PathBuf,
}

impl CliConfig {
    /// Load `startup.yml`. `home` overrides where local state lives (the CLI
    /// passes `--home` / `LIGHT_HOME`); the default is `~/.light`.
    pub fn load(startup_path: &Path, home: Option<PathBuf>) -> Result<Self, CliError> {
        let loader = ConfigLoader::new("", None, None)
            .map_err(|e| CliError::Config(format!("config loader: {e}")))?;
        let raw: RawStartup = loader
            .load_typed([startup_path])
            .map_err(|e| CliError::Config(format!("{}: {e}", startup_path.display())))?;
        let config_dir = startup_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Self::from_raw(raw, config_dir, home)
    }

    fn from_raw(
        raw: RawStartup,
        config_dir: PathBuf,
        home: Option<PathBuf>,
    ) -> Result<Self, CliError> {
        let env_tag = non_empty(raw.env_tag)
            .ok_or_else(|| CliError::Config("startup.yml: envTag is required".to_string()))?;

        // Refuse anything that could escape the store directory.
        if !env_tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(CliError::Config(format!(
                "startup.yml: envTag {env_tag:?} is not a plain name"
            )));
        }

        let home = match home {
            Some(home) => home,
            None => PathBuf::from(std::env::var("HOME").map_err(|_| {
                CliError::Config("HOME is not set; pass --home or set LIGHT_HOME".into())
            })?)
            .join(".light"),
        };

        Ok(Self {
            host: non_empty(raw.host),
            service_id: non_empty(raw.service_id),
            config_server_uri: non_empty(raw.config_server_uri)
                .map(|u| u.trim_end_matches('/').to_string()),
            config_token: raw
                .authorization
                .as_deref()
                .and_then(Secret::from_authorization),
            store_dir: home.join(&env_tag),
            env_tag,
            ca_bundle: non_empty(raw.bootstrap_ca_cert_path).map(|p| resolve_path(&config_dir, &p)),
            timeout: Duration::from_millis(raw.timeout.unwrap_or(3000)),
            connect_timeout: Duration::from_millis(raw.connect_timeout.unwrap_or(3000)),
            config_dir,
        })
    }
}

/// Resolve a path from `startup.yml`. The standard value is `config/ca.pem`,
/// relative to the app root that contains `config/`; also accept a path relative
/// to the config directory itself.
fn resolve_path(config_dir: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return path;
    }
    let from_app_root = config_dir.parent().map(|root| root.join(&path));
    let from_config_dir = config_dir.join(&path);
    match from_app_root {
        Some(candidate) if candidate.exists() => candidate,
        _ if from_config_dir.exists() => from_config_dir,
        Some(candidate) => candidate,
        None => from_config_dir,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSettings {
    gateway_uri: Option<String>,
    agent_service_ids: Option<String>,
    oauth_uri: Option<String>,
    oauth_provider_id: Option<String>,
    oauth_client_id: Option<String>,
}

/// The CLI's own settings, from `cli.yml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub gateway_uri: Option<String>,
    /// Service ids of the agents `/chat` can talk to, from `cli.agentServiceIds` (comma separated).
    pub agent_service_ids: Vec<String>,
    pub oauth_uri: Option<String>,
    pub oauth_provider_id: Option<String>,
    pub oauth_client_id: Option<String>,
}

/// Where and as whom the user signs in: light-oauth (through the Gateway), the provider in
/// `/oauth2/{providerId}/...`, and the registered device client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OauthSettings {
    pub uri: String,
    pub provider_id: String,
    pub client_id: String,
}

impl Settings {
    /// Resolve `cli.yml` in `config_dir`, with `values` (the config server's `values.yml`)
    /// supplying the `${cli.*}` placeholders; environment variables override both.
    pub fn load(config_dir: &Path, values: Option<&Mapping>) -> Result<Self, CliError> {
        let path = config_dir.join("cli.yml");
        if !path.exists() {
            return Err(CliError::Config(format!(
                "{} not found: it holds the CLI's gatewayUri and sign-in settings",
                path.display()
            )));
        }
        let map: HashMap<String, serde_yaml::Value> = values
            .map(|values| {
                values
                    .iter()
                    .filter_map(|(k, v)| k.as_str().map(|k| (k.to_string(), v.clone())))
                    .collect()
            })
            .unwrap_or_default();
        let loader = ConfigLoader::from_values(map, None, None)
            .map_err(|e| CliError::Config(format!("config loader: {e}")))?;
        let raw: RawSettings = loader
            .load_typed([&path])
            .map_err(|e| CliError::Config(format!("{}: {e}", path.display())))?;
        let clean = |v: Option<String>| non_empty(v).map(|u| u.trim_end_matches('/').to_string());
        let plain = |v: Option<String>| non_empty(v);
        Ok(Self {
            gateway_uri: clean(raw.gateway_uri),
            agent_service_ids: split_service_ids(raw.agent_service_ids.as_deref())?,
            oauth_uri: clean(raw.oauth_uri),
            oauth_provider_id: plain(raw.oauth_provider_id),
            oauth_client_id: plain(raw.oauth_client_id),
        })
    }

    /// The sign-in settings, or which one is missing.
    pub fn oauth(&self) -> Result<OauthSettings, CliError> {
        let need = |name: &str, value: &Option<String>| {
            value.clone().ok_or_else(|| {
                CliError::Config(format!(
                    "no {name}: set it in cli.yml or config-server to sign in"
                ))
            })
        };
        let provider_id = need("cli.oauthProviderId", &self.oauth_provider_id)?;
        // The provider id goes into a URL path: refuse anything that could change the route.
        if !provider_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(CliError::Config(format!(
                "cli.oauthProviderId {provider_id:?} is not a plain identifier"
            )));
        }
        Ok(OauthSettings {
            uri: need("cli.oauthUri", &self.oauth_uri)?,
            provider_id,
            client_id: need("cli.oauthClientId", &self.oauth_client_id)?,
        })
    }
}

/// A comma-separated list of service ids. Each goes into a request, so each must be a plain
/// identifier; anything else is refused rather than passed on.
fn split_service_ids(list: Option<&str>) -> Result<Vec<String>, CliError> {
    let mut ids = Vec::new();
    for id in list
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        if !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            return Err(CliError::Config(format!(
                "cli.agentServiceIds: {id:?} is not a plain service id"
            )));
        }
        if !ids.iter().any(|seen| seen == id) {
            ids.push(id.to_string());
        }
    }
    Ok(ids)
}

/// Refuse to send a credential over cleartext HTTP to anything but loopback.
///
/// `LIGHT_ALLOW_INSECURE_ISSUER=1` overrides this for a trusted private network;
/// do not set it casually.
pub fn ensure_transport_is_safe(uri: &str) -> Result<(), CliError> {
    let parsed =
        url::Url::parse(uri).map_err(|e| CliError::Config(format!("invalid URL {uri:?}: {e}")))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let loopback = match parsed.host() {
                Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            };
            if loopback || std::env::var("LIGHT_ALLOW_INSECURE_ISSUER").as_deref() == Ok("1") {
                Ok(())
            } else {
                Err(CliError::Config(format!(
                    "refusing to send a credential over plain HTTP to {uri}. Use https, or a loopback \
                     address, or set LIGHT_ALLOW_INSECURE_ISSUER=1 if this network is trusted"
                )))
            }
        }
        other => Err(CliError::Config(format!(
            "unsupported URL scheme {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn raw(env_tag: &str) -> RawStartup {
        RawStartup {
            env_tag: Some(env_tag.into()),
            ..RawStartup::default()
        }
    }

    fn from_raw(raw: RawStartup) -> Result<CliConfig, CliError> {
        CliConfig::from_raw(raw, PathBuf::from("."), Some("/tmp/light-home".into()))
    }

    #[test]
    fn a_bearer_prefix_is_stripped_and_a_bare_token_is_kept() {
        assert_eq!(
            Secret::from_authorization("Bearer abc.def")
                .unwrap()
                .expose(),
            "abc.def"
        );
        assert_eq!(
            Secret::from_authorization("  abc.def ").unwrap().expose(),
            "abc.def"
        );
        assert!(Secret::from_authorization("").is_none());
        assert!(Secret::from_authorization("Bearer ").is_none());
    }

    #[test]
    fn a_token_is_redacted_in_debug_output() {
        let secret = Secret::new("super-secret");
        assert!(!format!("{secret:?}").contains("super-secret"));
    }

    #[test]
    fn the_store_is_scoped_by_environment_and_rejects_path_tricks() {
        assert_eq!(
            from_raw(raw("dev")).unwrap().store_dir,
            PathBuf::from("/tmp/light-home/dev")
        );
        for bad in ["../x", "a/b", "", "dev prod"] {
            assert!(from_raw(raw(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn timeouts_default_to_the_standard_three_seconds() {
        let cfg = from_raw(raw("dev")).unwrap();
        assert_eq!(
            (cfg.timeout, cfg.connect_timeout),
            (Duration::from_millis(3000), Duration::from_millis(3000))
        );
        let mut r = raw("dev");
        r.timeout = Some(500);
        assert_eq!(from_raw(r).unwrap().timeout, Duration::from_millis(500));
    }

    #[test]
    fn startup_yml_keeps_its_standard_shape() {
        // The same file every app has.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("startup.yml");
        std::fs::write(
            &path,
            "host: dev.lightapi.net\nserviceId: com.networknt.light-cli-1.0.0\nenvTag: dev\nacceptHeader: application/yaml\n\
             timeout: 1500\nconnectTimeout: 700\nconfigServerUri: https://localhost:8435\nauthorization: \"Bearer config-read-token\"\n\
             bootstrapCaCertPath: ca.pem\n",
        )
        .unwrap();
        let cfg = CliConfig::load(&path, Some(dir.path().join("home"))).unwrap();
        assert_eq!(cfg.env_tag, "dev");
        assert_eq!(
            (cfg.timeout, cfg.connect_timeout),
            (Duration::from_millis(1500), Duration::from_millis(700))
        );
        assert_eq!(cfg.store_dir, dir.path().join("home/dev"));
        assert_eq!(
            cfg.config_server_uri.as_deref(),
            Some("https://localhost:8435")
        );
        assert_eq!(
            cfg.config_token.as_ref().map(|t| t.expose()),
            Some("config-read-token")
        );
        assert!(
            !format!("{cfg:?}").contains("config-read-token"),
            "the token is redacted in debug output"
        );
    }

    #[test]
    fn the_standard_ca_path_resolves_against_the_app_root_and_falls_back_to_the_config_dir() {
        let root = TempDir::new().unwrap();
        let config = root.path().join("config");
        std::fs::create_dir(&config).unwrap();
        std::fs::write(config.join("ca.pem"), "x").unwrap();
        // `config/ca.pem` is relative to the directory that contains `config/`.
        assert_eq!(
            resolve_path(&config, "config/ca.pem"),
            config.join("ca.pem")
        );
        // A bare name is relative to the config directory.
        assert_eq!(resolve_path(&config, "ca.pem"), config.join("ca.pem"));
        assert_eq!(
            resolve_path(&config, "/etc/ca.pem"),
            PathBuf::from("/etc/ca.pem")
        );
    }

    fn write_cli_yml(dir: &TempDir, text: &str) {
        std::fs::write(dir.path().join("cli.yml"), text).unwrap();
    }

    #[test]
    fn cli_yml_defaults_apply() {
        let dir = TempDir::new().unwrap();
        write_cli_yml(&dir, "gatewayUri: ${cli.gatewayUri:https://localhost/}\n");
        let settings = Settings::load(dir.path(), None).unwrap();
        assert_eq!(
            settings.gateway_uri.as_deref(),
            Some("https://localhost"),
            "trailing slash trimmed"
        );
    }

    #[test]
    fn config_server_values_override_the_defaults_by_their_full_key() {
        let dir = TempDir::new().unwrap();
        write_cli_yml(
            &dir,
            "gatewayUri: ${cli.gatewayUri:https://default-gateway}\noauthUri: ${cli.oauthUri:https://default-oauth}\n\
             oauthProviderId: ${cli.oauthProviderId:default-provider}\noauthClientId: ${cli.oauthClientId:client-1}\n",
        );
        let values: Mapping = serde_yaml::from_str(
            "cli.oauthProviderId: tenant-2\ncli.gatewayUri: https://gw.example.test/\nserver.httpPort: 8080\n",
        )
        .unwrap();
        let settings = Settings::load(dir.path(), Some(&values)).unwrap();
        assert_eq!(
            settings.gateway_uri.as_deref(),
            Some("https://gw.example.test"),
            "overridden, trailing slash trimmed"
        );
        let oauth = settings.oauth().unwrap();
        assert_eq!(oauth.provider_id, "tenant-2", "overridden");
        assert_eq!(
            oauth.uri, "https://default-oauth",
            "a key the server does not have keeps its default"
        );
    }

    #[test]
    fn the_sign_in_settings_are_read_and_a_missing_one_is_named() {
        let dir = TempDir::new().unwrap();
        write_cli_yml(
            &dir,
            "oauthUri: ${cli.oauthUri:https://localhost/}\noauthProviderId: ${cli.oauthProviderId:prov}\n\
             oauthClientId: ${cli.oauthClientId:client-1}\n",
        );
        let oauth = Settings::load(dir.path(), None).unwrap().oauth().unwrap();
        assert_eq!(
            oauth,
            OauthSettings {
                uri: "https://localhost".into(),
                provider_id: "prov".into(),
                client_id: "client-1".into()
            }
        );

        write_cli_yml(&dir, "gatewayUri: ${cli.gatewayUri:https://g}\n");
        let error = Settings::load(dir.path(), None)
            .unwrap()
            .oauth()
            .unwrap_err();
        assert!(error.to_string().contains("cli.oauthProviderId"), "{error}");
    }

    #[test]
    fn agent_service_ids_are_a_trimmed_deduplicated_list_of_plain_identifiers() {
        let dir = TempDir::new().unwrap();
        write_cli_yml(
            &dir,
            "agentServiceIds: ${cli.agentServiceIds:a.b-1.0 , c_d-2 ,,a.b-1.0}\n",
        );
        let settings = Settings::load(dir.path(), None).unwrap();
        assert_eq!(settings.agent_service_ids, ["a.b-1.0", "c_d-2"]);

        write_cli_yml(&dir, "gatewayUri: ${cli.gatewayUri:https://g}\n");
        assert!(
            Settings::load(dir.path(), None)
                .unwrap()
                .agent_service_ids
                .is_empty()
        );

        for bad in ["a b", "a/b", "a&x=1", "../x"] {
            write_cli_yml(
                &dir,
                &format!("agentServiceIds: ${{cli.agentServiceIds:{bad}}}\n"),
            );
            assert!(Settings::load(dir.path(), None).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_path_like_provider_id_is_refused() {
        let dir = TempDir::new().unwrap();
        write_cli_yml(
            &dir,
            "oauthUri: ${cli.oauthUri:https://o}\noauthProviderId: ${cli.oauthProviderId:../x}\noauthClientId: ${cli.oauthClientId:c}\n",
        );
        let error = Settings::load(dir.path(), None)
            .unwrap()
            .oauth()
            .unwrap_err();
        assert!(
            error.to_string().contains("not a plain identifier"),
            "{error}"
        );
    }

    #[test]
    fn a_missing_cli_yml_is_a_clear_error() {
        let dir = TempDir::new().unwrap();
        let error = Settings::load(dir.path(), None).unwrap_err();
        assert!(error.to_string().contains("cli.yml not found"), "{error}");
    }

    #[test]
    fn plain_http_is_allowed_only_to_loopback() {
        assert!(ensure_transport_is_safe("http://localhost:9443").is_ok());
        assert!(ensure_transport_is_safe("http://127.0.0.1:9443").is_ok());
        assert!(ensure_transport_is_safe("https://issuer.example.com").is_ok());
        assert!(ensure_transport_is_safe("http://issuer.example.com").is_err());
        assert!(ensure_transport_is_safe("http://10.0.0.5:9443").is_err());
        assert!(ensure_transport_is_safe("ftp://localhost").is_err());
    }
}
