//! In-process precision validator. Private package identities keep the gateway's
//! serde_json features/types unchanged. Only bytes/strings cross this API.
//! Compilation and validation share the host process failure/memory domain.
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, ffi::CStr, io, mem::MaybeUninit, str::FromStr};
mod catalogue;
mod precision;
mod request;
mod source;
mod status;
use status::StatusError;
const RAW: usize = 1 << 20;
const FRAME: usize = 4 << 20;
const EXPANDED: usize = 2 << 20;
fn bad() -> io::Error {
    io::Error::other("invalid captured validator source")
}

/// Closed infrastructure failures; no parser message/schema/body is exported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    InvalidSource,
    InvalidEvent,
    Resource,
    Internal,
}
pub struct Violation {
    pub code: String,
    pub message: String,
    pub description: String,
}
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub violation: Option<Violation>,
}
pub struct Compiled {
    adapter: request::Prepared,
    metadata: String,
    _pool: catalogue::Pool,
    pub source_sha256: [u8; 32],
    pub schema_count: usize,
}
impl Compiled {
    /// Compile all captured schemas eagerly, including unselected roots. No
    /// remote/file retrieval, worker, global allocator or process hook is added.
    pub fn compile(raw: &[u8], yaml: bool, settings_json: &[u8]) -> Result<Self, Failure> {
        let original = source::parse(raw, yaml).map_err(|_| Failure::InvalidSource)?;
        if settings_json.len() > RAW {
            return Err(Failure::InvalidSource);
        }
        let settings: Value =
            serde_json::from_slice(settings_json).map_err(|_| Failure::InvalidSource)?;
        let oas30 = original["openapi"]
            .as_str()
            .is_some_and(|v| v.starts_with("3.0."));
        let nullable = settings["handleNullableField"].as_bool().unwrap_or(true);
        let pool = catalogue::Pool::compile(
            &original,
            oas30,
            nullable,
            settings["handleNullableField"] == true,
        )
        .map_err(|_| Failure::InvalidSource)?;
        for pointer in pool.roots() {
            let _ = pool
                .at(pointer, nullable)
                .map_err(|_| Failure::InvalidSource)?;
        }
        let adapter = request::Prepared::compile(&original, &settings, &pool)
            .map_err(|_| Failure::InvalidSource)?;
        let metadata = serde_json::to_string(
            &catalogue::project(&original).map_err(|_| Failure::InvalidSource)?,
        )
        .map_err(|_| Failure::Internal)?;
        if metadata.len() > EXPANDED {
            return Err(Failure::Resource);
        }
        let schema_count = adapter.schema_count();
        Ok(Self {
            adapter,
            metadata,
            _pool: pool,
            source_sha256: Sha256::digest(raw).into(),
            schema_count,
        })
    }
    /// Retained compiler-input estimates from the preserved precision engine;
    /// excludes engine heap, allocator overhead and process RSS.
    pub fn accounted_input_bytes(&self) -> usize {
        self._pool
            .reserved()
            .saturating_add(self.adapter.compiled_input_bytes())
            .saturating_add(self.metadata.capacity())
    }
    pub fn cache_entries(&self) -> usize {
        self._pool.cache_entries()
    }
    pub fn metadata_json(&self) -> &str {
        &self.metadata
    }
    pub fn validate(&self, event_json: &[u8], body_depth: usize) -> Result<Reply, Failure> {
        if event_json.len() > FRAME {
            return Err(Failure::Resource);
        }
        let event: Value = serde_json::from_slice(event_json).map_err(|_| Failure::InvalidEvent)?;
        if let Some(body) = event["body"].as_str() {
            admit_depth(body.as_bytes(), body_depth)?;
        }
        match self.adapter.validate(&event) {
            Ok(None) => Ok(Reply {
                status: 200,
                body: "ok".into(),
                violation: None,
            }),
            Ok(Some(error)) => {
                let body = String::from_utf8(error.to_bytes()).map_err(|_| Failure::Internal)?;
                Ok(Reply {
                    status: error.status_code,
                    body,
                    violation: Some(Violation {
                        code: error.code,
                        message: error.message,
                        description: error.description,
                    }),
                })
            }
            Err(error) => Err(match error.as_str() {
                "engine-regex-resource-failure" => Failure::Resource,
                "engine-regex-internal-failure" => Failure::Internal,
                _ => Failure::InvalidEvent,
            }),
        }
    }
}

// A lexical resource check BEFORE the preserved adapter's recursive body parse.
// Syntax/validation errors remain the adapter's responsibility. Brackets inside
// strings and escaped quotes do not count as nesting.
fn admit_depth(raw: &[u8], limit: usize) -> Result<(), Failure> {
    if limit == 0 || limit > 512 {
        return Err(Failure::Resource);
    }
    let (mut string, mut escape, mut depth) = (false, false, 0usize);
    for &b in raw {
        if string {
            if escape {
                escape = false
            } else if b == b'\\' {
                escape = true
            } else if b == b'"' {
                string = false
            }
            continue;
        }
        match b {
            b'"' => string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > limit {
                    return Err(Failure::Resource);
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn source() -> &'static [u8] {
        br##"{"openapi":"3.1.0","paths":{"/pets":{"post":{"requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","required":["name"],"properties":{"name":{"type":"string"},"n":{"type":"number","multipleOf":0.1}}}}}}}}}}"##
    }
    fn event(body: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"path":"/pets","httpMethod":"POST","headers":{"Content-Type":"application/json"},"queryStringParameters":{},"pathParameters":{},"body":body})).unwrap()
    }
    #[test]
    fn real_compilation_and_valid_invalid_warm_request() {
        let compiled = Compiled::compile(source(), false, br#"{}"#).unwrap();
        assert_eq!(compiled.schema_count, 1);
        assert_eq!(
            compiled
                .validate(&event(r#"{"name":"synthetic","n":0.3}"#), 128)
                .unwrap()
                .status,
            200
        );
        assert_eq!(compiled.validate(&event("{}"), 128).unwrap().status, 400);
        assert_eq!(
            compiled
                .validate(&event(r#"{"name":"warm","n":0.2}"#), 128)
                .unwrap()
                .status,
            200
        );
    }
    #[test]
    fn precise_decimal_rejection_and_large_exponents() {
        let compiled = Compiled::compile(source(), false, br#"{}"#).unwrap();
        for body in [
            r#"{"name":"x","n":0.30000000000000000000000001}"#,
            r#"{"name":"x","n":0.31}"#,
        ] {
            assert_eq!(compiled.validate(&event(body), 128).unwrap().status, 400);
        }
        assert_eq!(
            compiled
                .validate(&event(r#"{"name":"x","n":1e400}"#), 128)
                .unwrap()
                .status,
            200
        );
    }
    #[test]
    fn bounded_request_depth_ignores_strings_and_fails_before_recursive_parse() {
        assert!(admit_depth(br#"{"x":"[\\\"["}"#, 1).is_ok());
        assert_eq!(admit_depth(b"[[[]]]", 2), Err(Failure::Resource));
        let compiled = Compiled::compile(source(), false, br#"{}"#).unwrap();
        let deep = format!("{}0{}", "[".repeat(129), "]".repeat(129));
        assert!(matches!(
            compiled.validate(&event(&deep), 128),
            Err(Failure::Resource)
        ));
        assert_eq!(
            compiled
                .validate(&event(r#"{"name":"warm"}"#), 128)
                .unwrap()
                .status,
            200
        );
    }
    #[test]
    fn invalid_sources_and_remote_references_are_rejected_without_fetch() {
        for raw in [br#"not-json"#.as_slice(),br#"{"openapi":"3.1.0","paths":{},"components":{"schemas":{"Bad":{"type":"invented"}}}}"#,
            br#"{"openapi":"3.1.0","paths":{"/pets":{"post":{"requestBody":{"content":{"application/json":{"schema":{"$ref":"https://unowned.invalid/schema"}}}}}}}}"#] {
            assert!(matches!(Compiled::compile(raw,false,br#"{}"#),Err(Failure::InvalidSource)));
        }
    }
    #[test]
    fn yaml_and_captured_reference_compile_before_requests() {
        let raw = br##"openapi: 3.1.0
paths:
  /pets:
    post:
      requestBody:
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/Pet'
components:
  schemas:
    Pet:
      type: object
      required: [name]
"##;
        let compiled = Compiled::compile(raw, true, br#"{}"#).unwrap();
        assert!(compiled.cache_entries() > 0);
        assert!(compiled.accounted_input_bytes() > 0);
        assert_eq!(compiled.validate(&event("{}"), 128).unwrap().status, 400);
        assert_eq!(
            compiled
                .validate(&event(r#"{"name":"warm"}"#), 128)
                .unwrap()
                .status,
            200
        );
    }
    #[test]
    fn ordinary_malformed_json_is_original_validation_error_not_infrastructure_fault() {
        let compiled = Compiled::compile(source(), false, br#"{}"#).unwrap();
        let reply = compiled.validate(&event("{"), 128).unwrap();
        assert_eq!(reply.status, 400);
        assert_eq!(reply.violation.unwrap().code, "ERR10015");
    }
}
