use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
    *,
};
use async_trait::async_trait;
use gateway_policy::java_cors::origin_allowed;
use http::{HeaderMap, Method, StatusCode};
use serde_yaml::{Mapping, Value};
use std::sync::Arc;

#[derive(Clone, Default)]
struct Allowed {
    origins: Vec<String>,
    methods: Option<Vec<String>>,
}
#[derive(Clone)]
pub struct CorsHandler {
    enabled: bool,
    global: Allowed,
    prefixes: Vec<(String, Allowed)>,
    response: bool,
}
fn invalid() -> InitError {
    InitError::Malformed("cors.yml: unknown or malformed setting")
}
fn list(map: &Mapping, name: &str) -> Result<Option<Vec<String>>, InitError> {
    let Some(value) = map.get(name) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let parsed = if let Some(text) = value.as_str() {
        serde_yaml::from_str(text).map_err(|_| invalid())?
    } else {
        value.clone()
    };
    // Empty Java placeholders may reach the module registry as empty strings.
    // Their decoded YAML value is null, just like an absent list.
    if parsed.is_null() {
        return Ok(None);
    }
    parsed
        .as_sequence()
        .ok_or_else(invalid)?
        .iter()
        .map(|item| item.as_str().map(str::to_owned).ok_or_else(invalid))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}
fn allowed(map: &Mapping) -> Result<Allowed, InitError> {
    Ok(Allowed {
        origins: list(map, "allowedOrigins")?.unwrap_or_default(),
        methods: list(map, "allowedMethods")?,
    })
}
impl CorsHandler {
    pub fn from_yaml(value: Value, response: bool) -> Result<Self, InitError> {
        let map = value.as_mapping().ok_or_else(invalid)?;
        if map.keys().any(|key| {
            !key.as_str().is_some_and(|key| {
                [
                    "enabled",
                    "allowedOrigins",
                    "allowedMethods",
                    "pathPrefixAllowed",
                ]
                .contains(&key)
            })
        }) {
            return Err(invalid());
        }
        let enabled = match map.get("enabled") {
            None => false,
            Some(v) => v.as_bool().ok_or_else(invalid)?,
        };
        let global = allowed(map)?;
        let raw = map.get("pathPrefixAllowed").cloned().unwrap_or(Value::Null);
        let raw: Value = if let Some(text) = raw.as_str() {
            serde_yaml::from_str(text).map_err(|_| invalid())?
        } else {
            raw
        };
        let mut prefixes = Vec::new();
        if !raw.is_null() {
            for (key, value) in raw.as_mapping().ok_or_else(invalid)? {
                let prefix = key
                    .as_str()
                    .filter(|s| s.starts_with('/'))
                    .ok_or_else(invalid)?;
                let fields = value.as_mapping().ok_or_else(invalid)?;
                if fields.keys().any(|key| {
                    !key.as_str()
                        .is_some_and(|key| ["allowedOrigins", "allowedMethods"].contains(&key))
                }) {
                    return Err(invalid());
                }
                prefixes.push((prefix.to_owned(), allowed(fields)?));
            }
        }
        Ok(Self {
            enabled,
            global,
            prefixes,
            response,
        })
    }
    fn selected(&self, path: &str) -> &Allowed {
        self.prefixes
            .iter()
            .find(|(prefix, _)| path.starts_with(prefix))
            .map_or(&self.global, |(_, value)| value)
    }
}
fn denied() -> StatusError {
    StatusError::new(
        403,
        "ERR10092",
        "CORS_PREFLIGHT_REQUEST_FAILED",
        "CORS preflight request failed. Origin not allowed or method/headers unsupported..",
    )
}
#[async_trait]
impl RequestHandler for CorsHandler {
    fn id(&self) -> HandlerId {
        HandlerId::RequestCors
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        if !self.enabled {
            return Ok(HandlerOutcome::Continue);
        }
        let Some(origin) = inv.headers.get("origin") else {
            return Ok(HandlerOutcome::Continue);
        };
        let origin_text = origin.to_str().map_err(|_| HandlerFailure)?;
        let config = self.selected(&inv.path);
        if !origin_allowed(origin_text, &config.origins).map_err(|_| HandlerFailure)? {
            return Ok(HandlerOutcome::Reject(denied()));
        }
        if inv.method != Method::OPTIONS {
            return Ok(HandlerOutcome::Continue);
        }
        // Java does not check Access-Control-Request-Method against allowedMethods.
        let methods = config.methods.as_ref().ok_or(HandlerFailure)?.join(",");
        let mut headers = HeaderMap::new();
        headers.insert("access-control-allow-origin", origin.clone());
        headers.insert("vary", "Origin".parse().map_err(|_| HandlerFailure)?);
        headers.insert(
            "access-control-allow-methods",
            methods.parse().map_err(|_| HandlerFailure)?,
        );
        headers.insert(
            "access-control-allow-headers",
            inv.headers
                .get("access-control-request-headers")
                .cloned()
                .unwrap_or_else(|| {
                    http::HeaderValue::from_static("Content-Type, WWW-Authenticate, Authorization")
                }),
        );
        headers.insert(
            "access-control-allow-credentials",
            http::HeaderValue::from_static("true"),
        );
        headers.insert(
            "access-control-max-age",
            http::HeaderValue::from_static("3600"),
        );
        Ok(HandlerOutcome::ContinueWithInitialResponse(
            GatewayResponse {
                status: StatusCode::OK,
                headers,
                body: None,
                body_is_binary: false,
            },
        ))
    }
}
#[async_trait]
impl ResponseHandler for CorsHandler {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseCors
    }
    async fn on_response(
        &self,
        inv: &Invocation,
        state: &mut ExchangeState,
        response: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        // Approved ALB-multimap denial contract has no denied-origin echo.
        if self.enabled
            && !state
                .audit
                .status
                .as_ref()
                .is_some_and(|s| s.code == "ERR10092")
            && let Some(origin) = inv.headers.get("origin")
        {
            response
                .headers
                .insert("access-control-allow-origin", origin.clone());
        }
        Ok(())
    }
}
impl HandlerFactory for CorsHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Preserves
    }
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        Ok(if self.response {
            Binding::Response(Arc::new(self.clone()))
        } else {
            Binding::Request(Arc::new(self.clone()), RequestCapabilities::default())
        })
    }
}
