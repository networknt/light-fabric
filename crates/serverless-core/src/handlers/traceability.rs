use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
    unsupported::{boolean, object},
    *,
};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
pub struct TraceabilityHandler {
    enabled: bool,
}
impl TraceabilityHandler {
    pub fn from_value(value: &Value) -> Result<Self, InitError> {
        let map = object(value, "traceability.yml: object required")?;
        if map.keys().any(|key| key != "enabled") {
            return Err(InitError::Malformed("traceability.yml: unknown key"));
        }
        Ok(Self {
            enabled: boolean(map, "enabled", false)?,
        })
    }
}
#[async_trait]
impl RequestHandler for TraceabilityHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Traceability
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        st: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        if self.enabled {
            st.traceability_id = inv
                .headers
                .get("x-traceability-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
        }
        Ok(HandlerOutcome::Continue)
    }
}
impl HandlerFactory for TraceabilityHandler {
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
