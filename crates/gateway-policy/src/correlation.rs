use crate::{HeaderWriter, Request, RequestRead};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CORRELATION_FILE: &str = "correlation.yml";
pub const CORRELATION_MODULE_ID: &str = "light-pingora/correlation";
pub const CORRELATION_CONFIG_NAME: &str = "correlation";
pub const CORRELATION_ID_HEADER: &str = "X-Correlation-Id";
pub const TRACEABILITY_ID_HEADER: &str = "X-Traceability-Id";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrelationConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_autogen_correlation_id")]
    pub autogen_correlation_id: bool,
    #[serde(default = "default_correlation_mdc_field")]
    pub correlation_mdc_field: String,
    #[serde(default = "default_traceability_mdc_field")]
    pub traceability_mdc_field: String,
}

impl Default for CorrelationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            autogen_correlation_id: true,
            correlation_mdc_field: default_correlation_mdc_field(),
            traceability_mdc_field: default_traceability_mdc_field(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorrelationState {
    pub correlation_id: Option<String>,
    pub traceability_id: Option<String>,
}

pub fn apply_correlation_request<R: Request>(
    session: &mut R,
    config: &CorrelationConfig,
) -> Result<CorrelationState, R::Error> {
    if !config.enabled {
        return Ok(CorrelationState::default());
    }

    let traceability_id = first_request_header(session, TRACEABILITY_ID_HEADER);
    let mut correlation_id = first_request_header(session, CORRELATION_ID_HEADER);

    if correlation_id.is_none() && config.autogen_correlation_id {
        let generated = java_compatible_uuid();
        session
            .req_header_mut()
            .insert_header(CORRELATION_ID_HEADER, generated.as_str())?;
        correlation_id = Some(generated);
    }

    Ok(CorrelationState {
        correlation_id,
        traceability_id,
    })
}

pub fn apply_correlation_response<W: HeaderWriter>(
    response: &mut W,
    state: &CorrelationState,
) -> Result<(), W::Error> {
    if let Some(traceability_id) = state.traceability_id.as_deref() {
        response.insert_header(TRACEABILITY_ID_HEADER, traceability_id)?;
    }
    Ok(())
}

pub fn correlation_id_for_upstream(state: &CorrelationState) -> Option<&str> {
    state.correlation_id.as_deref()
}

fn first_request_header(session: &impl RequestRead, name: &str) -> Option<String> {
    session
        .req_header()
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[doc(hidden)]
pub fn java_compatible_uuid() -> String {
    URL_SAFE_NO_PAD.encode(Uuid::now_v7().as_bytes())
}

fn default_enabled() -> bool {
    true
}

fn default_autogen_correlation_id() -> bool {
    true
}

fn default_correlation_mdc_field() -> String {
    "cId".to_string()
}

fn default_traceability_mdc_field() -> String {
    "tId".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue, Method};

    struct RequestFixture {
        headers: HeaderMap,
    }
    impl HeaderWriter for RequestFixture {
        type Error = &'static str;
        fn insert_header(
            &mut self,
            name: impl AsRef<str>,
            value: impl AsRef<str>,
        ) -> Result<(), Self::Error> {
            let name = http::header::HeaderName::from_bytes(name.as_ref().as_bytes())
                .map_err(|_| "name")?;
            let value = HeaderValue::from_str(value.as_ref()).map_err(|_| "value")?;
            self.headers.insert(name, value);
            Ok(())
        }
        fn remove_header(&mut self, name: &str) {
            self.headers.remove(name);
        }
    }
    impl Request for RequestFixture {
        type Error = &'static str;
        type Writer = Self;
        fn req_header_mut(&mut self) -> &mut Self {
            self
        }
    }
    impl RequestRead for RequestFixture {
        fn req_header(&self) -> crate::RequestHead<'_> {
            crate::RequestHead {
                headers: &self.headers,
                method: &Method::GET,
            }
        }
    }

    #[test]
    fn invalid_text_is_missing_and_generation_is_optional() {
        for autogen in [false, true] {
            let mut request = RequestFixture {
                headers: HeaderMap::new(),
            };
            request.headers.insert(
                CORRELATION_ID_HEADER,
                HeaderValue::from_bytes(&[0xff]).unwrap(),
            );
            let config = CorrelationConfig {
                autogen_correlation_id: autogen,
                ..Default::default()
            };
            let state = apply_correlation_request(&mut request, &config).unwrap();
            assert_eq!(state.correlation_id.is_some(), autogen);
            if autogen {
                assert_eq!(state.correlation_id.unwrap().len(), 22);
            } else {
                assert_eq!(request.headers[CORRELATION_ID_HEADER].as_bytes(), &[0xff]);
            }
        }
    }

    #[test]
    fn serde_defaults_and_names_remain_compatible() {
        let config: CorrelationConfig = serde_json::from_str("{}").unwrap();
        assert!(config.enabled && config.autogen_correlation_id);
        assert_eq!(config.correlation_mdc_field, "cId");
        assert_eq!(config.traceability_mdc_field, "tId");
        let config: CorrelationConfig = serde_json::from_str(
            r#"{"autogenCorrelationId":false,"correlationMdcField":"custom"}"#,
        )
        .unwrap();
        assert!(!config.autogen_correlation_id);
        assert_eq!(config.correlation_mdc_field, "custom");
        assert!(serde_json::from_str::<CorrelationConfig>(r#"{"enabled":null}"#).is_err());
    }
}
