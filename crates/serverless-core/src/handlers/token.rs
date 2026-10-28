//! Lambda token selection/publication around the shared OAuth client/cache.
use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
    *,
};
use async_trait::async_trait;
use gateway_policy::token::TokenRuntime;
use http::HeaderValue;
use serde_yaml::Value;
use std::sync::Arc;

#[derive(Clone)]
pub struct TokenHandler {
    enabled: bool,
    prefixes: Vec<String>,
    runtime: Arc<TokenRuntime>,
    caller_id: String,
}
fn invalid() -> InitError {
    InitError::Malformed("token.yml/client.yml: unknown or malformed setting")
}
impl TokenHandler {
    pub fn from_yaml(
        value: Value,
        runtime: Arc<TokenRuntime>,
        caller_id: String,
    ) -> Result<Self, InitError> {
        let map = value.as_mapping().ok_or_else(invalid)?;
        if map.keys().any(|key| {
            !key.as_str()
                .is_some_and(|key| ["enabled", "appliedPathPrefixes"].contains(&key))
        }) {
            return Err(invalid());
        }
        let enabled = match map.get("enabled") {
            None => false,
            Some(v) => v.as_bool().ok_or_else(invalid)?,
        };
        let raw = map
            .get("appliedPathPrefixes")
            .cloned()
            .unwrap_or(Value::Null);
        let raw: Value = if let Some(text) = raw.as_str() {
            serde_yaml::from_str(text).map_err(|_| invalid())?
        } else {
            raw
        };
        let prefixes = match raw {
            Value::Null => vec![],
            Value::Sequence(values) => values
                .iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or_else(invalid))
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(invalid()),
        };
        if runtime.client_config().request.inject_caller_id
            && (caller_id.is_empty()
                || caller_id.len() > 256
                || HeaderValue::from_str(&caller_id).is_err())
        {
            return Err(invalid());
        }
        Ok(Self {
            enabled,
            prefixes,
            runtime,
            caller_id,
        })
    }
}
#[async_trait]
impl RequestHandler for TokenHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Token
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        if !self.enabled
            || !self
                .prefixes
                .iter()
                .any(|prefix| inv.path.starts_with(prefix))
        {
            return Ok(HandlerOutcome::Continue);
        }
        // Java requires service_id presence, not service_url or a boundary match.
        let Some(service_id) = inv.headers.get("service_id") else {
            return Ok(HandlerOutcome::Continue);
        };
        let service_id = service_id.to_str().map_err(|_| HandlerFailure)?.to_owned();
        let result = self
            .runtime
            .lambda_client_credentials_token_with_caller(
                &inv.path,
                Some(service_id),
                inv.deadline,
                &self.caller_id,
            )
            .await;
        if tokio::time::Instant::now() >= inv.deadline || inv.cancel.is_cancelled() {
            return Ok(HandlerOutcome::Reject(StatusError::timeout()));
        }
        let token = match result {
            Ok(token) => token,
            Err(error) => {
                return Ok(HandlerOutcome::Reject(match error.code.as_str() {
                    "ERR10000" if error.status == 401 => super::jwt::invalid(),
                    "ERR14017" => StatusError::timeout(),
                    _ => StatusError::handler_failure(),
                }));
            }
        };
        let mut value =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| HandlerFailure)?;
        value.set_sensitive(true);
        let name = if inv.headers.contains_key("authorization") {
            "x-scope-token"
        } else {
            "authorization"
        };
        inv.headers.insert(name, value);
        Ok(HandlerOutcome::Continue)
    }
}
impl HandlerFactory for TokenHandler {
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
