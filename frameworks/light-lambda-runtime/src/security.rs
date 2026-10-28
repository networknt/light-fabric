//! Real Phase-1 factories. WP10 supplies app templates; WP12 supplies remaining policies.
use crate::init_error::InitError;
use light_runtime::{RuntimeConfig, RuntimeError};
use light_security::SecurityRuntime;
use serde_json::Value;
use serverless_core::{
    HandlerId,
    handlers::{
        factors::{ApiKeyHandler, BasicHandler},
        jwt::JwtHandler,
        security::{UnifiedHandler, UnifiedSettings},
    },
    plan::Registry,
    unsupported::validate_security,
};
use std::{collections::BTreeMap, sync::Arc};

fn value(config: &RuntimeConfig, file: &str) -> Result<Value, InitError> {
    match config.module_registry.load_config(config, file) {
        Ok(value) => Ok(value),
        Err(RuntimeError::MissingConfig(_)) => Ok(serde_json::json!({})),
        Err(_) => Err(InitError::InvalidConfiguration(
            "security configuration: invalid file",
        )),
    }
}
pub(crate) fn register(
    config: &RuntimeConfig,
    registry: &mut Registry,
    handler: &Value,
    custom_active: bool,
) -> Result<Option<Arc<SecurityRuntime>>, InitError> {
    let insecure = std::env::var("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG").as_deref() == Ok("1");
    let settings = validate_security(&value(config, "security.yml")?, insecure).map_err(|_| {
        InitError::InvalidConfiguration(
            "security.yml: unsupported, invalid or insecure configuration",
        )
    })?;
    let unified =
        UnifiedSettings::from_value(&value(config, "unified-security.yml")?).map_err(|_| {
            InitError::InvalidConfiguration(
                "unified-security.yml: invalid configuration or factor not supported until WP12",
            )
        })?;
    let basic = Arc::new(
        BasicHandler::from_value(&value(config, "basic-auth.yml")?).map_err(|_| {
            InitError::InvalidConfiguration("basic-auth.yml: invalid or deferred configuration")
        })?,
    );
    let apikey = Arc::new(
        ApiKeyHandler::from_value(&value(config, "apikey.yml")?).map_err(|_| {
            InitError::InvalidConfiguration("apikey.yml: invalid or unbounded configuration")
        })?,
    );
    let entries = ["handlers", "additionalHandlers"]
        .into_iter()
        .filter_map(|k| handler.get(k));
    let mut uses_jwt = false;
    let mut uses_unified = false;
    let mut aliases = BTreeMap::new();
    for value in entries {
        let entries: Vec<String> = match value {
            Value::Array(values) => values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            Value::String(s) if s.trim().starts_with('[') => {
                serde_json::from_str(s).unwrap_or_default()
            }
            Value::String(s) => s.split(',').map(|s| s.trim().to_owned()).collect(),
            _ => vec![],
        };
        for entry in entries {
            let (class, alias) = entry.split_once('@').unwrap_or((&entry, &entry));
            if let Some(c) = serverless_core::compat::lookup(class) {
                uses_jwt |= c.id == HandlerId::Jwt;
                uses_unified |= c.id == HandlerId::Security;
                aliases.insert(alias.to_owned(), c.id);
            } else {
                aliases.remove(alias);
            }
        }
    }
    let basic_binding =
        (aliases.get("basic") == Some(&HandlerId::Basic) && basic.enabled()).then(|| basic.clone());
    let apikey_binding = (aliases.get("apikey") == Some(&HandlerId::ApiKey) && apikey.enabled())
        .then(|| apikey.clone());
    // Validate every configured factor even on an unused/disabled prefix, before
    // bootstrap/network activity. Java's missing/disabled alias bypass is rejected.
    if unified.prefixes.iter().any(|p| p.basic) && basic_binding.is_none()
        || unified.prefixes.iter().any(|p| p.apikey) && apikey_binding.is_none()
    {
        return Err(InitError::InvalidConfiguration(
            "unified-security.yml: selected factor requires its enabled implemented alias",
        ));
    }
    if basic.enabled() {
        registry.register(HandlerId::Basic, basic);
    } else {
        registry.disable(HandlerId::Basic);
    }
    if apikey.enabled() {
        registry.register(HandlerId::ApiKey, apikey);
    } else {
        registry.disable(HandlerId::ApiKey);
    }
    if uses_unified
        && unified.enabled
        && unified.prefixes.iter().any(|p| p.jwt)
        && (!settings.enabled || !settings.config.enable_verify_jwt)
    {
        return Err(InitError::InvalidConfiguration(
            "unified-security.yml: selected JWT requires enabled verification",
        ));
    }
    let active = custom_active
        || (uses_jwt && settings.enabled)
        || (uses_unified && unified.enabled && unified.prefixes.iter().any(|p| p.jwt));
    let runtime = light_security::load_security_runtime(config, active)
        .map_err(|_| {
            InitError::InvalidConfiguration("security.yml/client.yml: invalid configuration")
        })?
        .map(Arc::new);
    if !active && !(uses_unified && unified.enabled) {
        registry.disable(HandlerId::Jwt);
        registry.disable(HandlerId::Security);
        return Ok(runtime);
    }
    let jwt_enabled = settings.enabled && active;
    let unified_enabled = unified.enabled;
    let jwt = Arc::new(
        JwtHandler::new(
            serverless_core::unsupported::SecuritySettings {
                enabled: jwt_enabled,
                config: settings.config,
            },
            runtime.clone(),
            vec![],
        )
        .map_err(|_| {
            InitError::InvalidConfiguration("security.yml: unavailable verified runtime")
        })?,
    );
    if jwt_enabled && active {
        registry.register(HandlerId::Jwt, jwt.clone());
    } else {
        registry.disable(HandlerId::Jwt);
    }
    if unified_enabled && (active || uses_unified) {
        registry.register(
            HandlerId::Security,
            Arc::new(
                UnifiedHandler::new(unified, jwt)
                    .with_factor_bindings(basic_binding, apikey_binding)
                    .map_err(|_| {
                        InitError::InvalidConfiguration(
                            "unified-security.yml: incomplete factor bindings",
                        )
                    })?,
            ),
        );
    } else {
        registry.disable(HandlerId::Security);
    }
    Ok(runtime)
}
