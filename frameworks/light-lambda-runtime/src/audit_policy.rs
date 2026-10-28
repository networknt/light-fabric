//! Common offline/startup Audit validation and immutable Lambda factory.
use crate::init_error::InitError;
use gateway_policy::observation::{RecordKind, RecordSink, StdoutSink};
use light_runtime::RuntimeConfig;
use serverless_core::{handlers::audit::AuditHandler, plan::HandlerFactory};
use std::sync::Arc;
fn value(config: &RuntimeConfig, file: &'static str) -> Result<serde_json::Value, InitError> {
    match config.module_registry.load_config(config, file) {
        Ok(value) => Ok(value),
        Err(light_runtime::RuntimeError::MissingConfig(_)) => Ok(serde_json::json!({})),
        Err(_) => Err(InitError::InvalidConfiguration(
            "audit/mask configuration: malformed file",
        )),
    }
}
fn invalid(error: serverless_core::InitError) -> InitError {
    match error {
        serverless_core::InitError::Malformed(message) => InitError::InvalidConfiguration(message),
        _ => InitError::InvalidConfiguration("audit/mask configuration: invalid setting"),
    }
}
/// Aggregate credential safety, independent of policy activation or selection.
pub(crate) fn reject_credential_headers(config: &RuntimeConfig) -> Result<(), InitError> {
    const ERROR: &str = "audit.yml: credential header selection unsupported";
    fn list(value: Option<&serde_json::Value>) -> Result<Vec<serde_json::Value>, InitError> {
        match value {
            None | Some(serde_json::Value::Null) => Ok(Vec::new()),
            Some(serde_json::Value::Array(values)) => Ok(values.clone()),
            Some(serde_json::Value::String(text)) if text.trim().is_empty() => Ok(Vec::new()),
            Some(serde_json::Value::String(text)) => {
                let parsed: serde_json::Value = serde_yaml::from_str(text)
                    .map_err(|_| InitError::InvalidConfiguration(ERROR))?;
                match parsed {
                    serde_json::Value::Array(values) => Ok(values),
                    _ => Err(InitError::InvalidConfiguration(ERROR)),
                }
            }
            _ => Err(InitError::InvalidConfiguration(ERROR)),
        }
    }
    let audit = value(config, "audit.yml")?;
    let apikey = value(config, "apikey.yml")?;
    let headers = if audit.get("headers").is_some() {
        serverless_core::unsupported::strings(audit.get("headers"))
            .map_err(|_| InitError::InvalidConfiguration(ERROR))?
    } else {
        vec![
            "X-Correlation-Id".into(),
            "X-Traceability-Id".into(),
            "caller_id".into(),
        ]
    };
    let rules = list(apikey.get("pathPrefixAuths"))?;
    for header in headers {
        for rule in &rules {
            let credential = rule
                .get("headerName")
                .and_then(serde_json::Value::as_str)
                .ok_or(InitError::InvalidConfiguration(ERROR))?;
            if header.eq_ignore_ascii_case(credential) {
                return Err(InitError::InvalidConfiguration(ERROR));
            }
        }
    }
    Ok(())
}
pub(crate) fn warnings(config: &RuntimeConfig) -> Result<Vec<String>, InitError> {
    let audit = value(config, "audit.yml")?;
    serverless_core::unsupported::audit::reject_response_body_selection(&audit).map_err(invalid)?;
    serverless_core::unsupported::audit::timestamp_format(&audit).map_err(invalid)?;
    let mask = value(config, "mask.yml")?;
    let warnings = serverless_core::unsupported::audit::validate_mask(&mask).map_err(invalid)?;
    AuditHandler::from_value(
        &audit,
        "configuration-validation-only".into(),
        Arc::new(ValidationSink),
    )
    .map_err(invalid)?
    .with_mask_regex(&regex_setting(&mask)?)
    .map_err(invalid)?;
    Ok(warnings)
}

/// Construction and validation never submit, and therefore never start the
/// stdout writer. This sink is not used for actual configured policy execution.
struct ValidationSink;
impl RecordSink for ValidationSink {
    fn try_emit(&self, _: RecordKind, _: serde_json::Value) {}
}

fn regex_setting(mask: &serde_json::Value) -> Result<serde_json::Value, InitError> {
    match mask.get("regex") {
        None | Some(serde_json::Value::Null) => Ok(serde_json::Value::Null),
        Some(serde_json::Value::String(text)) if text.trim().is_empty() => {
            Ok(serde_json::Value::Null)
        }
        Some(serde_json::Value::String(text)) => serde_json::from_str(text)
            .map_err(|_| InitError::InvalidConfiguration("mask.yml: malformed map setting")),
        Some(value) => Ok(value.clone()),
    }
}

pub struct AuditPolicyProvider;
impl crate::init::HandlerFactoryProvider for AuditPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<light_security::SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        warnings(config)?;
        let app = value(config, "lambda-app.yml")?;
        let service_id = app
            .get("lambdaAppId")
            .and_then(serde_json::Value::as_str)
            .ok_or(InitError::InvalidConfiguration(
                "audit.yml: invalid trusted service id",
            ))?;
        let handler = AuditHandler::from_value(
            &value(config, "audit.yml")?,
            service_id.into(),
            Arc::new(StdoutSink),
        )
        .map_err(invalid)?
        .with_mask_regex(&regex_setting(&value(config, "mask.yml")?)?)
        .map_err(invalid)?;
        Ok(Arc::new(handler))
    }
}
