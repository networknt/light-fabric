//! Offline token configuration qualification and Lambda-only shared runtime wiring.
use crate::{init::HandlerFactoryProvider, init_error::InitError};
use gateway_policy::token::{SidecarTrafficConfig, TokenHandlerConfig, TokenRuntime};
use light_client::{ClientConfig, ClientFactory, EndpointOptions, OAuthProviderResolver};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serde_json::{Value, json};
use serverless_core::{handlers::token::TokenHandler, plan::HandlerFactory};
use std::{sync::Arc, time::Duration};

pub struct TokenPolicyProvider;
fn invalid() -> InitError {
    InitError::InvalidConfiguration(
        "token.yml/client.yml/cache.yml: unsupported, invalid or insecure token configuration",
    )
}
fn load(config: &RuntimeConfig, file: &str, fallback: Value) -> Result<Value, InitError> {
    match config.module_registry.load_config(config, file) {
        Ok(value) => Ok(value),
        Err(light_runtime::RuntimeError::MissingConfig(_)) => Ok(fallback),
        Err(_) => Err(invalid()),
    }
}
fn object(value: &Value) -> Result<&serde_json::Map<String, Value>, InitError> {
    value.as_object().ok_or_else(invalid)
}
fn keys(value: &Value, allowed: &[&str]) -> Result<(), InitError> {
    if object(value)?
        .keys()
        .any(|key| !allowed.contains(&key.as_str()))
    {
        return Err(invalid());
    }
    Ok(())
}
fn nested(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or_else(|| json!({}))
}
fn structured(value: &Value) -> Result<Value, InitError> {
    if value.is_null() {
        return Ok(json!({}));
    }
    if let Some(text) = value.as_str() {
        serde_yaml::from_str(text).map_err(|_| invalid())
    } else {
        Ok(value.clone())
    }
}
fn unsupported(
    value: &Value,
    prefix: &str,
    defaults: &[(&str, Value)],
    warnings: &mut Vec<String>,
) -> Result<(), InitError> {
    for (key, default) in defaults {
        if value.get(*key).is_some_and(|v| v != default) {
            return Err(invalid());
        }
        warnings.push(format!(
            "{prefix}.{key}: default/absent unsupported setting; no Lambda behavior"
        ));
    }
    Ok(())
}
fn providers_shape(value: &Value) -> Result<(), InitError> {
    let value = structured(value)?;
    for provider in object(&value)?.values() {
        keys(
            provider,
            &[
                "server_url",
                "serviceId",
                "uri",
                "client_id",
                "client_secret",
                "scope",
                "audience",
                "proxyHost",
                "proxyPort",
                "enableHttp2",
                "tokenRenewBeforeExpired",
                "expiredRefreshRetryDelay",
                "earlyRefreshRetryDelay",
            ],
        )?;
    }
    Ok(())
}
fn raw_client(value: &Value, warnings: &mut Vec<String>) -> Result<(), InitError> {
    keys(
        value,
        &[
            "tls",
            "request",
            "oauth",
            "pathPrefixServices",
            "verifyHostname",
        ],
    )?;
    let tls = nested(value, "tls");
    keys(
        &tls,
        &[
            "trustMode",
            "verifyHostname",
            "caCertPath",
            "clientCertPath",
            "clientKeyPath",
            "tlsVersion",
            "loadKeyStore",
            "loadTrustStore",
            "keyStore",
            "keyStorePass",
            "trustStore",
            "trustStorePass",
        ],
    )?;
    for key in ["loadKeyStore", "loadTrustStore"] {
        if tls.get(key).is_some_and(|v| v != &Value::Bool(false)) {
            return Err(invalid());
        }
        warnings.push(format!(
            "client.tls.{key}: absent/disabled Java store; PEM trust configuration required"
        ));
    }
    for key in ["keyStore", "keyStorePass", "trustStore", "trustStorePass"] {
        if tls
            .get(key)
            .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
        {
            return Err(invalid());
        }
        warnings.push(format!(
            "client.tls.{key}: absent/empty unsupported Java store setting"
        ));
    }
    let request = nested(value, "request");
    keys(
        &request,
        &[
            "errorThreshold",
            "connectTimeout",
            "timeout",
            "resetTimeout",
            "injectOpenTracing",
            "injectCallerId",
            "enableHttp2",
            "connectionPoolSize",
            "connectionExpireTime",
            "maxReqPerConn",
            "maxConnectionNumPerHost",
            "minConnectionNumPerHost",
            "maxRequestRetry",
            "requestRetryDelay",
            "poolMetricsEnabled",
            "poolWarmUpEnabled",
            "poolWarmUpSize",
            "healthCheckEnabled",
            "healthCheckIntervalMs",
        ],
    )?;
    unsupported(
        &request,
        "client.request",
        &[
            ("errorThreshold", json!(2)),
            ("resetTimeout", json!(7000)),
            ("injectOpenTracing", json!(false)),
            ("connectionPoolSize", json!(1000)),
            ("connectionExpireTime", json!(1800000)),
            ("maxReqPerConn", json!(1000000)),
            ("maxConnectionNumPerHost", json!(1000)),
            ("minConnectionNumPerHost", json!(250)),
            ("poolMetricsEnabled", json!(false)),
            ("poolWarmUpEnabled", json!(false)),
            ("poolWarmUpSize", json!(1)),
            ("healthCheckEnabled", json!(true)),
            ("healthCheckIntervalMs", json!(30000)),
        ],
        warnings,
    )?;
    let oauth = nested(value, "oauth");
    keys(&oauth, &["multipleAuthServers", "token", "sign", "deref"])?;
    let token = nested(&oauth, "token");
    keys(
        &token,
        &[
            "cache",
            "tokenRenewBeforeExpired",
            "expiredRefreshRetryDelay",
            "earlyRefreshRetryDelay",
            "server_url",
            "serviceId",
            "proxyHost",
            "proxyPort",
            "enableHttp2",
            "client_credentials",
            "authorization_code",
            "refresh_token",
            "token_exchange",
            "workflow_long",
            "key",
        ],
    )?;
    keys(&nested(&token, "cache"), &["capacity"])?;
    let cc = nested(&token, "client_credentials");
    keys(
        &cc,
        &[
            "uri",
            "client_id",
            "client_secret",
            "scope",
            "serviceIdAuthServers",
        ],
    )?;
    providers_shape(&nested(&cc, "serviceIdAuthServers"))?;
    let key = nested(&token, "key");
    keys(
        &key,
        &[
            "server_url",
            "serviceId",
            "uri",
            "client_id",
            "client_secret",
            "serviceIdAuthServers",
            "enableHttp2",
            "audience",
        ],
    )?;
    providers_shape(&nested(&key, "serviceIdAuthServers"))?;
    for (name, allowed) in [
        (
            "authorization_code",
            vec!["uri", "client_id", "client_secret", "redirect_uri", "scope"],
        ),
        (
            "refresh_token",
            vec!["uri", "client_id", "client_secret", "scope"],
        ),
        (
            "token_exchange",
            vec![
                "uri",
                "client_id",
                "client_secret",
                "scope",
                "subjectToken",
                "subjectTokenType",
                "requestedTokenType",
                "audience",
            ],
        ),
        (
            "workflow_long",
            vec![
                "gateway_url",
                "provider_id",
                "client_id",
                "client_secret",
                "database_url_file",
                "keyring_file",
                "ca_file",
            ],
        ),
    ] {
        keys(&nested(&token, name), &allowed)?;
    }
    let sign = nested(&oauth, "sign");
    keys(
        &sign,
        &[
            "server_url",
            "serviceId",
            "uri",
            "timeout",
            "client_id",
            "client_secret",
            "proxyHost",
            "proxyPort",
            "enableHttp2",
            "key",
        ],
    )?;
    keys(
        &nested(&sign, "key"),
        &[
            "server_url",
            "serviceId",
            "uri",
            "client_id",
            "client_secret",
            "enableHttp2",
            "audience",
        ],
    )?;
    keys(
        &nested(&oauth, "deref"),
        &[
            "server_url",
            "serviceId",
            "uri",
            "client_id",
            "client_secret",
            "proxyHost",
            "proxyPort",
            "enableHttp2",
        ],
    )?;
    Ok(())
}

fn cache_limits(value: &Value) -> Result<Option<(usize, Duration)>, InitError> {
    keys(value, &["enabled", "caches"])?;
    if value
        .get("enabled")
        .is_some_and(|v| v != &Value::Bool(false))
    {
        return Err(invalid());
    }
    let Some(caches) = value.get("caches") else {
        return Ok(None);
    };
    if caches.is_null() {
        return Ok(None);
    }
    let caches = if let Some(text) = caches.as_str() {
        serde_yaml::from_str(text).map_err(|_| invalid())?
    } else {
        caches.clone()
    };
    let mut result = None;
    for cache in caches.as_array().ok_or_else(invalid)? {
        keys(cache, &["cacheName", "maxSize", "expiryInMinutes"])?;
        if cache.get("cacheName").and_then(Value::as_str) != Some("token") || result.is_some() {
            return Err(invalid());
        }
        let max = cache
            .get("maxSize")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(invalid)?;
        let seconds = cache
            .get("expiryInMinutes")
            .and_then(Value::as_u64)
            .and_then(|v| v.checked_mul(60))
            .ok_or_else(invalid)?;
        result = Some((max, Duration::from_secs(seconds)));
    }
    Ok(result)
}
fn destination(url: &str, uri: &str, insecure: bool) -> Result<(), InitError> {
    let url = url::Url::parse(url).map_err(|_| invalid())?;
    let test_http = url.scheme() == "http"
        && insecure
        && url.host_str().is_some_and(|h| {
            h == "localhost"
                || h.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    if (!test_http && url.scheme() != "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !uri.starts_with('/')
        || uri.starts_with("//")
        || uri.contains(['?', '#', '\\'])
    {
        return Err(invalid());
    }
    Ok(())
}
fn default_flow<T: serde::Serialize>(actual: &T, default: &T) -> Result<bool, InitError> {
    fn compare(actual: &Value, default: &Value) -> bool {
        // Approved blank optional placeholders mean absent; this is not an
        // activation switch, and nonempty destinations/credentials still fail.
        if default.is_null() && actual.as_str() == Some("") {
            return true;
        }
        match (actual, default) {
            (Value::Object(a), Value::Object(d)) => {
                a.len() == d.len()
                    && d.iter()
                        .all(|(k, v)| a.get(k).is_some_and(|a| compare(a, v)))
            }
            _ => actual == default,
        }
    }
    Ok(compare(
        &serde_json::to_value(actual).map_err(|_| invalid())?,
        &serde_json::to_value(default).map_err(|_| invalid())?,
    ))
}
fn prepare(config: &RuntimeConfig) -> Result<(TokenHandler, Vec<String>), InitError> {
    let token = load(config, "token.yml", json!({"enabled":false}))?;
    let raw = load(config, "client.yml", json!({}))?;
    let raw = if raw.is_null() { json!({}) } else { raw };
    let mut warnings = Vec::new();
    raw_client(&raw, &mut warnings).map_err(|_| {
        InitError::InvalidConfiguration(
            "client.yml: unknown/unsupported token-owned key or invalid shape",
        )
    })?;
    let client = config.client.clone().unwrap_or_default();
    let defaults = ClientConfig::default();
    if !default_flow(
        &client.oauth.token.authorization_code,
        &defaults.oauth.token.authorization_code,
    )? || !default_flow(
        &client.oauth.token.refresh_token,
        &defaults.oauth.token.refresh_token,
    )? || !default_flow(
        &client.oauth.token.token_exchange,
        &defaults.oauth.token.token_exchange,
    )? || !default_flow(
        &client.oauth.token.workflow_long,
        &defaults.oauth.token.workflow_long,
    )? || !default_flow(&client.oauth.sign, &defaults.oauth.sign)?
    {
        return Err(InitError::InvalidConfiguration(
            "client.yml: deferred OAuth flow has non-default configuration",
        ));
    }
    for key in [
        "authorization_code",
        "refresh_token",
        "token_exchange",
        "workflow_long",
    ] {
        warnings.push(format!(
            "client.oauth.token.{key}: default/absent deferred flow; not activated"
        ));
    }
    warnings.push("client.oauth.sign: default/absent deferred flow; not activated".into());
    let limits = cache_limits(&load(config, "cache.yml", json!({"enabled":false}))?)?;
    let enabled = token
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    if enabled {
        let insecure =
            std::env::var("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG").as_deref() == Ok("1");
        let test_endpoints =
            std::env::var("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS").as_deref() == Ok("1");
        if !client.tls.verify_hostname && !insecure {
            return Err(invalid());
        }
        let resolver = OAuthProviderResolver::new(&client);
        let providers = if resolver.client_credentials_multi_provider() {
            if client
                .oauth
                .token
                .client_credentials
                .service_id_auth_servers
                .is_empty()
            {
                return Err(invalid());
            }
            client
                .oauth
                .token
                .client_credentials
                .service_id_auth_servers
                .keys()
                .map(|id| {
                    resolver
                        .client_credentials_provider(Some(id))
                        .map_err(|_| invalid())
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            vec![
                resolver
                    .client_credentials_provider(None)
                    .map_err(|_| invalid())?,
            ]
        };
        for provider in providers {
            if provider.client_id.is_empty()
                || provider.client_secret.is_empty()
                || provider.client_id.len() > 4096
                || provider.client_secret.len() > 4096
                || provider.scope.iter().map(String::len).sum::<usize>() > 16384
            {
                return Err(invalid());
            }
            let url = if let Some(url) = &provider.server_url {
                url.as_str()
            } else {
                gateway_policy::direct_registry::direct_registry_match(
                    &config.direct_registry,
                    provider.token_service_id.as_deref().ok_or_else(invalid)?,
                    None,
                )
                .ok_or_else(invalid)?
                .url
            };
            destination(url, &provider.uri, test_endpoints)?;
            ClientFactory::from_config(&client)
                .reqwest_client_lambda_token_no_redirect(EndpointOptions {
                    proxy_host: provider.proxy_host,
                    proxy_port: provider.proxy_port,
                    enable_http2: Some(provider.enable_http2),
                    ..Default::default()
                })
                .map_err(|_| invalid())?;
        }
    }
    let runtime = TokenRuntime::new(
        TokenHandlerConfig::default(),
        SidecarTrafficConfig::default(),
        client,
        config.direct_registry.clone(),
        None,
    );
    let runtime = if let Some((max, expiry)) = limits {
        runtime.with_lambda_cache_limits(max, expiry)
    } else {
        runtime
    };
    let token = serde_yaml::to_value(token).map_err(|_| invalid())?;
    Ok((
        TokenHandler::from_yaml(
            token,
            Arc::new(runtime),
            config.service_identity.service_id.clone(),
        )
        .map_err(|_| invalid())?,
        warnings,
    ))
}
pub(crate) fn warnings(config: &RuntimeConfig) -> Result<Vec<String>, InitError> {
    prepare(config).map(|(_, warnings)| warnings)
}
impl HandlerFactoryProvider for TokenPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        Ok(Arc::new(prepare(config)?.0))
    }
}
