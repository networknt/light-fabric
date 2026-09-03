use crate::config_util::deserialize_string_list;
use http::header::HeaderName;
use light_runtime::RuntimeError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingRequestMatch {
    PathPrefix,
    AcceptHeader,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamingPolicy {
    #[serde(
        default = "default_stream_response_content_types",
        deserialize_with = "deserialize_string_list"
    )]
    pub stream_response_content_types: Vec<String>,
    #[serde(
        default = "default_stream_request_accept_types",
        deserialize_with = "deserialize_string_list"
    )]
    pub stream_request_accept_types: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub stream_path_prefixes: Vec<String>,
    #[serde(default)]
    pub stream_max_request_time: u64,
    #[serde(default)]
    pub stream_idle_timeout: u64,
    #[serde(
        default = "default_stream_response_header_overwrite",
        deserialize_with = "deserialize_string_list"
    )]
    pub stream_response_header_overwrite: Vec<String>,
}

impl Default for StreamingPolicy {
    fn default() -> Self {
        Self {
            stream_response_content_types: default_stream_response_content_types(),
            stream_request_accept_types: default_stream_request_accept_types(),
            stream_path_prefixes: Vec::new(),
            stream_max_request_time: 0,
            stream_idle_timeout: 0,
            stream_response_header_overwrite: default_stream_response_header_overwrite(),
        }
    }
}

impl StreamingPolicy {
    pub fn normalized(mut self, namespace: &str) -> Result<Self, RuntimeError> {
        normalize_list(&mut self.stream_response_content_types);
        normalize_list(&mut self.stream_request_accept_types);
        normalize_list(&mut self.stream_path_prefixes);
        normalize_list(&mut self.stream_response_header_overwrite);

        for header in &self.stream_response_header_overwrite {
            HeaderName::from_bytes(header.as_bytes()).map_err(|error| {
                RuntimeError::Unsupported(format!(
                    "{namespace}.streamResponseHeaderOverwrite contains invalid header name `{header}`: {error}"
                ))
            })?;
        }
        Ok(self)
    }

    pub fn classify_request<'a, I>(
        &self,
        path: &str,
        accept_values: I,
    ) -> Option<StreamingRequestMatch>
    where
        I: IntoIterator<Item = &'a str>,
    {
        if self
            .stream_path_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
        {
            return Some(StreamingRequestMatch::PathPrefix);
        }
        if contains_configured_media_type(accept_values, &self.stream_request_accept_types) {
            return Some(StreamingRequestMatch::AcceptHeader);
        }
        None
    }

    pub fn is_streaming_response<'a, I>(&self, content_type_values: I) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        contains_configured_media_type(content_type_values, &self.stream_response_content_types)
    }
}

fn contains_configured_media_type<'a, I>(values: I, configured: &[String]) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    values.into_iter().any(|value| {
        value.split(',').any(|part| {
            let media_type = part.split(';').next().unwrap_or_default().trim();
            configured.iter().any(|candidate| {
                let candidate = candidate.split(';').next().unwrap_or_default().trim();
                !candidate.is_empty() && media_type.eq_ignore_ascii_case(candidate)
            })
        })
    })
}

fn normalize_list(values: &mut Vec<String>) {
    *values = values
        .drain(..)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
}

fn default_stream_response_content_types() -> Vec<String> {
    vec!["text/event-stream".to_string()]
}

fn default_stream_request_accept_types() -> Vec<String> {
    vec!["text/event-stream".to_string()]
}

fn default_stream_response_header_overwrite() -> Vec<String> {
    [
        "Content-Type",
        "Cache-Control",
        "Connection",
        "Transfer-Encoding",
        "Content-Encoding",
        "Content-Length",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_light_4j_streaming_contract() {
        let policy: StreamingPolicy = serde_yaml::from_str("{}").expect("policy");
        assert_eq!(
            policy.stream_response_content_types,
            vec!["text/event-stream".to_string()]
        );
        assert_eq!(
            policy.stream_request_accept_types,
            vec!["text/event-stream".to_string()]
        );
        assert!(policy.stream_path_prefixes.is_empty());
        assert_eq!(policy.stream_max_request_time, 0);
        assert_eq!(policy.stream_idle_timeout, 0);
        assert_eq!(policy.stream_response_header_overwrite.len(), 6);
    }

    #[test]
    fn request_media_type_matching_is_case_insensitive_and_parameter_agnostic() {
        let policy = StreamingPolicy::default();
        assert_eq!(
            policy.classify_request(
                "/events",
                ["application/json, Text/Event-Stream; charset=utf-8"]
            ),
            Some(StreamingRequestMatch::AcceptHeader)
        );
        assert_eq!(
            policy.classify_request("/events", ["application/json"]),
            None
        );
    }

    #[test]
    fn path_prefix_matching_precedes_accept_matching() {
        let policy = StreamingPolicy {
            stream_path_prefixes: vec!["/events".to_string()],
            ..StreamingPolicy::default()
        };
        assert_eq!(
            policy.classify_request("/events/42", ["text/event-stream"]),
            Some(StreamingRequestMatch::PathPrefix)
        );
    }

    #[test]
    fn response_media_type_matching_handles_comma_separated_values() {
        let policy = StreamingPolicy {
            stream_response_content_types: vec![
                " Text/Event-Stream; compatibility=java ".to_string(),
            ],
            ..StreamingPolicy::default()
        };
        assert!(
            policy.is_streaming_response(["application/json; charset=utf-8, TEXT/EVENT-STREAM"])
        );
    }

    #[test]
    fn normalization_trims_lists_and_rejects_invalid_header_names() {
        let policy: StreamingPolicy = serde_yaml::from_str(
            r#"
streamPathPrefixes: " /events, , /watch "
streamResponseHeaderOverwrite:
  - " Content-Type "
"#,
        )
        .expect("policy");
        let policy = policy.normalized("proxy").expect("normalized policy");
        assert_eq!(
            policy.stream_path_prefixes,
            vec!["/events".to_string(), "/watch".to_string()]
        );
        assert_eq!(
            policy.stream_response_header_overwrite,
            vec!["Content-Type".to_string()]
        );

        let invalid = StreamingPolicy {
            stream_response_header_overwrite: vec!["bad header".to_string()],
            ..StreamingPolicy::default()
        };
        assert!(invalid.normalized("router").is_err());
    }

    #[test]
    fn empty_lists_disable_the_corresponding_matcher() {
        let policy: StreamingPolicy = serde_yaml::from_str(
            r#"
streamResponseContentTypes: []
streamRequestAcceptTypes: []
streamPathPrefixes: []
"#,
        )
        .expect("policy");
        assert_eq!(
            policy.classify_request("/events", ["text/event-stream"]),
            None
        );
        assert!(!policy.is_streaming_response(["text/event-stream"]));
    }
}
