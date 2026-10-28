//! Cold-start admission only. No factory is implied by a valid configuration.
use crate::init_error::InitError;
use light_runtime::{RuntimeConfig, RuntimeError};
use serverless_core::unsupported::remaining::{self, HealthSettings, ValidatorSettings};
pub(crate) fn prepare_with_sources(
    config: &RuntimeConfig,
    sources: Option<&config_loader::BoundedLocalSources>,
) -> Result<
    (
        Vec<String>,
        Option<std::sync::Arc<crate::transformer_program::Candidate>>,
    ),
    InitError,
> {
    let mut warnings = Vec::new();
    let mut program = None;
    for file in [
        "openapi-validator.yml",
        "sanitizer.yml",
        "request-transformer.yml",
        "response-transformer.yml",
        "health.yml",
        "router.yml",
        "rule.yml",
        "status.yml",
    ] {
        let loaded = if file == "rule.yml" {
            sources
                .map_or_else(
                    || config.module_registry.load_config_bounded(config, file),
                    |sources| {
                        config
                            .module_registry
                            .load_config_bounded_from_sources(config, sources)
                    },
                )
                .and_then(|value| {
                    serde_json::to_value(value).map_err(|_| {
                        RuntimeError::Unsupported("transformer: unsupported rule profile".into())
                    })
                })
        } else {
            config.module_registry.load_config(config, file)
        };
        let value = match loaded {
            Ok(value) => value,
            Err(RuntimeError::MissingConfig(_)) if file == "status.yml" => serde_json::json!({}),
            Err(RuntimeError::MissingConfig(_)) => continue,
            Err(_) => {
                if file == "rule.yml" {
                    return Err(InitError::InvalidConfiguration(
                        "transformer: unsupported rule profile",
                    ));
                }
                return Err(InitError::InvalidConfiguration(
                    "policy configuration: invalid setting",
                ));
            }
        };
        let result = match file {
            "openapi-validator.yml" => ValidatorSettings::from_value(&value).map(|(_, w)| w),
            "sanitizer.yml" => remaining::sanitizer(&value),
            "health.yml" => HealthSettings::from_value(&value).map(|(_, w)| w),
            "router.yml" => remaining::router(&value),
            "rule.yml" => remaining::rule_declarations(&value),
            "status.yml" => status_flags(&value),
            _ => remaining::transformer(&value, file == "response-transformer.yml"),
        }
        .map_err(|_| {
            InitError::InvalidConfiguration(
                "policy configuration: unknown, malformed or unsupported nondefault setting",
            )
        })?;
        warnings.extend(result);
        if file == "rule.yml" {
            program = Some(std::sync::Arc::new(
                crate::transformer_program::Candidate::compile(&value).map_err(|_| {
                    InitError::InvalidConfiguration("transformer: unsupported rule profile")
                })?,
            ));
        }
    }
    Ok((warnings, program))
}

// Admission only: the private pinned catalogue never supplies runtime responses
// or module/Info data. Java's missing-map metadata accessor defaults false, while
// its shipped template resolves true. Owner-approved admission uses the latter.
fn status_flags(value: &serde_json::Value) -> Result<Vec<String>, serverless_core::InitError> {
    let invalid = || serverless_core::InitError::Malformed("status.yml: unsupported setting");
    let map = value.as_object().ok_or_else(invalid)?;
    let mut warnings = Vec::new();
    for (key, default) in [
        ("showMessage", true),
        ("showDescription", true),
        ("showMetadata", true),
    ] {
        if map
            .get(key)
            .is_some_and(|value| value.as_bool() != Some(default))
        {
            return Err(invalid());
        }
        warnings.push(format!("status.{key}: unsupported default/absent ignored"));
    }
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("status_catalogue.json")).map_err(|_| invalid())?;
    let mut stock_entries = false;
    for (key, value) in map {
        if ["showMessage", "showDescription", "showMetadata"].contains(&key.as_str()) {
            continue;
        }
        if baseline.get(key) != Some(value) {
            return Err(invalid());
        }
        stock_entries = true;
    }
    if stock_entries {
        warnings.push("status.catalogue: stock entries ignored".into());
    }
    Ok(warnings)
}
