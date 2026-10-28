//! Partial X-04: trusted opt-in, verified JWT principal, finite real-filter reload.
use crate::{
    init::HandlerFactoryProvider,
    init_error::InitError,
    telemetry::{self, DiagnosticLevel},
};
use async_trait::async_trait;
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serde::Deserialize;
use serde_json::Value;
use serverless_core::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, ValidatorMediaRole},
    *,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct LoggerSetPolicyProvider {
    _trusted_startup: (),
}
impl LoggerSetPolicyProvider {
    pub fn from_startup_gate() -> Result<Option<Self>, InitError> {
        match std::env::var("LIGHT_LAMBDA_ALLOW_LOGGER_SET") {
            Err(std::env::VarError::NotPresent) => Ok(None),
            Ok(value) if value == "0" => Ok(None),
            Ok(value) if value == "1" => Ok(Some(Self {
                _trusted_startup: (),
            })),
            _ => Err(InitError::InvalidConfiguration(
                "logging: invalid trusted startup gate",
            )),
        }
    }
}
impl HandlerFactoryProvider for LoggerSetPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        let value: Value = config
            .module_registry
            .load_config(config, "logging.yml")
            .map_err(|_| InitError::InvalidConfiguration("logging: invalid configuration"))?;
        let (enabled, _) = crate::local_activation::activation(HandlerId::SetLogger, &value, true)?;
        if !enabled || security.is_none() || !telemetry::diagnostic_profile_admitted() {
            return Err(InitError::InvalidConfiguration(
                "logging: setter requires enabled logging, verified JWT runtime and scalar INFO/DEBUG/TRACE startup filter",
            ));
        }
        Ok(Arc::new(Setter))
    }
}
struct Setter;
impl HandlerFactory for Setter {
    fn validator_media_role(&self) -> ValidatorMediaRole {
        ValidatorMediaRole::Preserves
    }
    fn build(&self, definition: &Compatibility, _: &str) -> Result<Binding, plan::InitError> {
        if definition.id != HandlerId::SetLogger {
            return Err(plan::InitError::FactoryContract(definition.id));
        }
        Ok(Binding::Local(Arc::new(Self)))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Directive {
    name: String,
    level: String,
}
fn invalid_command() -> StatusError {
    StatusError::new(
        400,
        "ERR11004",
        "VALIDATOR_SCHEMA",
        "Schema Validation Error - expected at most one ROOT INFO/DEBUG/TRACE directive within 1024 bytes.",
    )
}
fn command(bytes: &[u8]) -> Result<Option<DiagnosticLevel>, StatusError> {
    if bytes.is_empty() {
        return Err(StatusError::new(
            400,
            "ERR10059",
            "REQUEST_BODY_MISSING",
            "Request body is missing.",
        ));
    }
    if bytes.len() > 1024 {
        return Err(invalid_command());
    }
    let directives: Vec<Directive> =
        serde_json::from_slice(bytes).map_err(|_| invalid_command())?;
    if directives.len() > 1 {
        return Err(invalid_command());
    }
    directives
        .first()
        .map(|d| {
            if d.name != "ROOT" {
                return Err(invalid_command());
            }
            DiagnosticLevel::command(&d.level).ok_or_else(invalid_command)
        })
        .transpose()
}
#[async_trait]
impl RequestHandler for Setter {
    fn id(&self) -> HandlerId {
        HandlerId::SetLogger
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        if inv.method != http::Method::POST {
            return Ok(HandlerOutcome::Reject(StatusError::new(
                404,
                "ERR10007",
                "INVALID_REQUEST_PATH",
                "Local endpoint is unavailable.",
            )));
        }
        if state.principal.is_none() {
            return Ok(HandlerOutcome::Reject(StatusError::missing_identity()));
        }
        if inv.cancel.is_cancelled() || tokio::time::Instant::now() >= inv.deadline {
            return Ok(HandlerOutcome::Reject(StatusError::timeout()));
        }
        let body = inv.body.as_deref().unwrap_or_default();
        let level = match command(body) {
            Ok(level) => level,
            Err(status) => return Ok(HandlerOutcome::Reject(status)),
        };
        if let Some(level) = level {
            telemetry::set_diagnostic_level(level).map_err(|_| HandlerFailure)?;
        }
        // Canonical echo of the accepted command domain, never arbitrary input.
        let echo = match level {
            None => "[]",
            Some(DiagnosticLevel::Info) => r#"[{"name":"ROOT","level":"INFO"}]"#,
            Some(DiagnosticLevel::Debug) => r#"[{"name":"ROOT","level":"DEBUG"}]"#,
            Some(DiagnosticLevel::Trace) => r#"[{"name":"ROOT","level":"TRACE"}]"#,
        };
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        Ok(HandlerOutcome::Respond(GatewayResponse {
            status: http::StatusCode::OK,
            headers,
            body: Some(bytes::Bytes::from_static(echo.as_bytes())),
            body_is_binary: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_command_validates_every_field_before_mutation() {
        assert_eq!(command(br#"[]"#).unwrap(), None);
        assert_eq!(
            command(br#"[{"name":"ROOT","level":"DEBUG"}]"#).unwrap(),
            Some(DiagnosticLevel::Debug)
        );
        for body in [
            "null",
            "{}",
            "[null]",
            "[{}]",
            "[{\"name\":\"other\",\"level\":\"INFO\"}]",
            "[{\"name\":\"ROOT\",\"level\":\"OFF\"}]",
            "[{\"name\":\"ROOT\",\"level\":\"INFO\",\"extra\":0}]",
            "[{\"name\":\"ROOT\",\"level\":\"INFO\"},{\"name\":\"ROOT\",\"level\":\"TRACE\"}]",
            "[",
        ] {
            assert_eq!(command(body.as_bytes()).err().unwrap().code, "ERR11004");
        }
        assert_eq!(command(b"").err().unwrap().code, "ERR10059");
        let mut boundary = br#"[{"name":"ROOT","level":"INFO"}]"#.to_vec();
        boundary.resize(1024, b' ');
        assert_eq!(command(&boundary).unwrap(), Some(DiagnosticLevel::Info));
        boundary.push(b' ');
        assert_eq!(command(&boundary).err().unwrap().code, "ERR11004");
    }
}
