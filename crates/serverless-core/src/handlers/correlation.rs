use super::policy_io::PolicyHeaders;
use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
    unsupported::{boolean, object},
    *,
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use gateway_policy::correlation::{CorrelationConfig, apply_correlation_request};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
pub struct CorrelationHandler {
    config: CorrelationConfig,
}
impl CorrelationHandler {
    pub fn from_value(value: &Value) -> Result<Self, InitError> {
        let map = object(value, "correlation.yml: object required")?;
        if map.keys().any(|key| {
            ![
                "enabled",
                "autogenCorrelationID",
                "correlationMdcField",
                "traceabilityMdcField",
            ]
            .contains(&key.as_str())
        }) {
            return Err(InitError::Malformed("correlation.yml: unknown key"));
        }
        for (key, default) in [
            ("correlationMdcField", "cId"),
            ("traceabilityMdcField", "tId"),
        ] {
            if map.get(key).is_some_and(|value| value != default) {
                return Err(InitError::Malformed(
                    "correlation.yml: unsupported nondefault MDC setting",
                ));
            }
        }
        Ok(Self {
            config: CorrelationConfig {
                enabled: boolean(map, "enabled", true)?,
                autogen_correlation_id: boolean(map, "autogenCorrelationID", true)?,
                ..Default::default()
            },
        })
    }
    pub fn warnings(value: &Value) -> Result<Vec<String>, InitError> {
        Self::from_value(value)?;
        Ok([
            "correlation.correlationMdcField",
            "correlation.traceabilityMdcField",
        ]
        .iter()
        .map(|key| format!("{key}: unsupported default/absent ignored"))
        .collect())
    }
}
#[async_trait]
impl RequestHandler for CorrelationHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Correlation
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        st: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        // Java uses UUID.randomUUID; the transport adapter keeps that v4 recipe
        // while retaining the shared core's existing Gateway/Pingora v7 generator.
        if self.config.enabled
            && self.config.autogen_correlation_id
            && !inv.headers.contains_key("x-correlation-id")
        {
            let value = URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes());
            inv.headers.insert(
                "x-correlation-id",
                value.parse().map_err(|_| HandlerFailure)?,
            );
        }
        let state = apply_correlation_request(
            &mut PolicyHeaders {
                headers: &mut inv.headers,
                method: &inv.method,
            },
            &self.config,
        )?;
        st.correlation_id = state.correlation_id;
        // Lambda's Java middleware does not install response echo headers or
        // traceability attachments; the distinct traceability handler owns those.
        Ok(HandlerOutcome::Continue)
    }
}
impl HandlerFactory for CorrelationHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Preserves
    }
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        Ok(Binding::Request(
            Arc::new(self.clone()),
            RequestCapabilities::default(),
        ))
    }
}
