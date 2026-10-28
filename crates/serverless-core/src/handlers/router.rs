//! Trusted destination selection only. Dispatch remains a terminal operation.
use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
    *,
};
use async_trait::async_trait;
use std::{collections::BTreeSet, sync::Arc};
#[derive(Clone)]
pub struct RouterHandler {
    services: Arc<BTreeSet<String>>,
}
impl RouterHandler {
    pub fn new(services: impl IntoIterator<Item = String>) -> Self {
        Self {
            services: Arc::new(services.into_iter().collect()),
        }
    }
}
#[async_trait]
impl RequestHandler for RouterHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Router
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        // Reject before header removal, destination selection or terminal dispatch.
        // The original request failure stays resident; only its typed state is reported.
        if let Some(error) = state.failed_request_error() {
            return Ok(HandlerOutcome::Reject(error));
        }
        let service = inv.headers.get("service_id").cloned();
        inv.headers.remove("service_id");
        if state.has_http_target() {
            return Ok(HandlerOutcome::Continue);
        }
        let Some(service) = service else {
            return Ok(HandlerOutcome::Continue);
        };
        let service = service.to_str().map_err(|_| HandlerFailure)?;
        if !self.services.contains(service)
            || !["GET", "DELETE", "POST", "PUT", "PATCH"]
                .iter()
                .any(|method| method.eq_ignore_ascii_case(inv.method.as_str()))
        {
            return Ok(HandlerOutcome::Reject(StatusError::new(
                400,
                "ERR10089",
                "FAILED_TO_INVOKE_SERVICE",
                &format!("Failed to invoke HTTP service with serviceId {service}."),
            )));
        }
        state.select_http_target(service.to_owned());
        Ok(HandlerOutcome::Continue)
    }
}
impl HandlerFactory for RouterHandler {
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
