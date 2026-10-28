use super::policy_io::{PolicyHeaders, TrackedHeaders};
use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
    *,
};
use async_trait::async_trait;
use gateway_policy::header::{HeaderConfig, apply_header_request, apply_header_response};
use serde_yaml::Value;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
pub struct HeaderHandler {
    config: HeaderConfig,
    prefixes: Vec<String>,
    response: bool,
}
impl HeaderHandler {
    /// Load the common merged YAML value directly: Java applies every matching
    /// prefix in declared map order, rather than the shared longest-prefix rule.
    pub fn from_yaml(value: Value, response: bool) -> Result<Self, InitError> {
        let map = value
            .as_mapping()
            .ok_or(InitError::Malformed("header.yml: object required"))?;
        if map.keys().any(|key| {
            !key.as_str().is_some_and(|key| {
                ["enabled", "request", "response", "pathPrefixHeader"].contains(&key)
            })
        }) {
            return Err(InitError::Malformed("header.yml: unknown key"));
        }
        for key in ["request", "response"] {
            if let Some(value) = map.get(key) {
                validate_mutation(value)?;
            }
        }
        let raw = map.get("pathPrefixHeader").cloned().unwrap_or(Value::Null);
        let raw = if let Some(text) = raw.as_str() {
            serde_yaml::from_str(text)
                .map_err(|_| InitError::Malformed("header.yml: prefix map"))?
        } else {
            raw
        };
        let prefixes = match raw {
            Value::Null => vec![],
            Value::Mapping(ref map) => map
                .keys()
                .map(|key| {
                    key.as_str()
                        .filter(|key| key.starts_with('/'))
                        .map(str::to_owned)
                        .ok_or(InitError::Malformed("header.yml: invalid prefix"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(InitError::Malformed("header.yml: prefix map")),
        };
        if let Value::Mapping(prefixes) = &raw {
            for value in prefixes.values() {
                let fields = value
                    .as_mapping()
                    .ok_or(InitError::Malformed("header.yml: prefix object"))?;
                if fields.keys().any(|key| {
                    !key.as_str()
                        .is_some_and(|key| ["request", "response"].contains(&key))
                }) {
                    return Err(InitError::Malformed("header.yml: unknown prefix key"));
                }
                for key in ["request", "response"] {
                    if let Some(value) = fields.get(key) {
                        validate_mutation(value)?;
                    }
                }
            }
        }
        let config: HeaderConfig = serde_yaml::from_value(value)
            .map_err(|_| InitError::Malformed("header.yml: invalid setting"))?;
        Ok(Self {
            config,
            prefixes,
            response,
        })
    }
    fn configurations(&self) -> Vec<HeaderConfig> {
        let mut global = self.config.clone();
        global.path_prefix_header.clear();
        let mut result = vec![global];
        for prefix in &self.prefixes {
            let mut config = HeaderConfig {
                enabled: self.config.enabled,
                ..Default::default()
            };
            config.path_prefix_header = BTreeMap::from([(
                prefix.clone(),
                self.config.path_prefix_header[prefix].clone(),
            )]);
            result.push(config);
        }
        result
    }
}

fn validate_mutation(value: &Value) -> Result<(), InitError> {
    let fields = value
        .as_mapping()
        .ok_or(InitError::Malformed("header.yml: mutation object"))?;
    if fields.keys().any(|key| {
        !key.as_str()
            .is_some_and(|key| ["remove", "update"].contains(&key))
    }) {
        return Err(InitError::Malformed("header.yml: unknown mutation key"));
    }
    Ok(())
}
#[async_trait]
impl RequestHandler for HeaderHandler {
    fn id(&self) -> HandlerId {
        HandlerId::RequestHeader
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        let mut media = state.validator_media_snapshot();
        for config in self.configurations() {
            apply_header_request(
                &mut TrackedHeaders {
                    inner: PolicyHeaders {
                        headers: &mut inv.headers,
                        method: &inv.method,
                    },
                    media: &mut media,
                },
                &config,
                &inv.path,
            )?;
        }
        state.commit_validator_media(media);
        Ok(HandlerOutcome::Continue)
    }
}
#[async_trait]
impl ResponseHandler for HeaderHandler {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseHeader
    }
    async fn on_response(
        &self,
        inv: &Invocation,
        _: &mut ExchangeState,
        response: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        for config in self.configurations() {
            apply_header_response(
                &mut PolicyHeaders {
                    headers: &mut response.headers,
                    method: &inv.method,
                },
                &config,
                &inv.path,
            )?;
        }
        Ok(())
    }
}
impl HandlerFactory for HeaderHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Maintains
    }
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        if self.response {
            Ok(Binding::Response(Arc::new(self.clone())))
        } else {
            Ok(Binding::Request(
                Arc::new(self.clone()),
                RequestCapabilities::default(),
            ))
        }
    }
}
