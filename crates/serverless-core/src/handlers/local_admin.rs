//! Real local terminals; protected payloads require an established principal.
use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory},
    *,
};
use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header::CONTENT_TYPE};
use std::sync::Arc;

#[derive(Clone)]
pub struct LocalAdminHandler {
    id: HandlerId,
    installed_filter: Option<fn() -> Option<&'static str>>,
}
impl LocalAdminHandler {
    pub fn health() -> Self {
        Self {
            id: HandlerId::Health,
            installed_filter: None,
        }
    }
    pub fn info() -> Self {
        Self {
            id: HandlerId::Info,
            installed_filter: None,
        }
    }
    /// The reader must observe the installed subscriber, not later environment state.
    pub fn logger(installed_filter: fn() -> Option<&'static str>) -> Self {
        Self {
            id: HandlerId::GetLogger,
            installed_filter: Some(installed_filter),
        }
    }
}
#[async_trait]
impl RequestHandler for LocalAdminHandler {
    fn id(&self) -> HandlerId {
        self.id
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        if inv.method != Method::GET {
            return Ok(HandlerOutcome::Reject(StatusError::new(
                404,
                "ERR10007",
                "INVALID_REQUEST_PATH",
                "Local endpoint is unavailable.",
            )));
        }
        if self.id != HandlerId::Health && state.principal.is_none() {
            return Ok(HandlerOutcome::Reject(StatusError::missing_identity()));
        }
        let (content_type, body) = match self.id {
            HandlerId::Health => ("text/plain; charset=utf-8", Bytes::from_static(b"OK")),
            HandlerId::Info => (
                "application/json",
                Bytes::from_static(
                    br#"{"runtime":"rust","modules":["serverless-core","serverless-aws"]}"#,
                ),
            ),
            HandlerId::GetLogger => {
                let filter = self
                    .installed_filter
                    .and_then(|read| read())
                    .ok_or(HandlerFailure)?;
                let body = serde_json::to_vec(&serde_json::json!({"filter":filter}))
                    .map_err(|_| HandlerFailure)?;
                ("application/json", Bytes::from(body))
            }
            _ => return Err(HandlerFailure),
        };
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        Ok(HandlerOutcome::Respond(GatewayResponse {
            status: StatusCode::OK,
            headers,
            body: Some(body),
            body_is_binary: false,
        }))
    }
}
impl HandlerFactory for LocalAdminHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Preserves
    }
    fn build(&self, definition: &Compatibility, _: &str) -> Result<Binding, InitError> {
        if definition.id != self.id {
            return Err(InitError::FactoryContract(definition.id));
        }
        Ok(Binding::Local(Arc::new(self.clone())))
    }
}
