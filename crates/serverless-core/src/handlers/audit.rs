//! Selected audit records capture at actual policy entry and finalize once.
use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
    unsupported::{boolean, object, strings},
    *,
};
use async_trait::async_trait;
use gateway_policy::{
    audit::{MAX_SELECTORS, SAFE_FIELDS, insert_selected, safe_header},
    observation::{RecordKind, RecordSink},
};
use serde_json::{Map, Value, json};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;
#[derive(Clone)]
pub struct AuditHandler {
    enabled: bool,
    mask: bool,
    mask_plan: gateway_policy::audit_mask::AuditMask,
    service_id: String,
    headers: Vec<String>,
    fields: Vec<String>,
    status_code: bool,
    response_time: bool,
    errors_only: bool,
    error_level: bool,
    stack_trace: bool,
    timestamp_format: Option<gateway_policy::audit_timestamp::AuditTimestamp>,
    sink: Arc<dyn RecordSink>,
}
impl AuditHandler {
    pub fn from_value(
        value: &Value,
        service_id: String,
        sink: Arc<dyn RecordSink>,
    ) -> Result<Self, InitError> {
        crate::unsupported::audit::reject_response_body_selection(value)?;
        let map = object(value, "audit.yml: object required")?;
        if map.keys().any(|key| {
            ![
                "enabled",
                "mask",
                "statusCode",
                "responseTime",
                "auditOnError",
                "auditStackTrace",
                "logLevelIsError",
                "timestampFormat",
                "headers",
                "audit",
                "requestBodyMaxSize",
                "responseBodyMaxSize",
            ]
            .contains(&key.as_str())
        }) {
            return Err(InitError::Malformed("audit.yml: unknown key"));
        }
        let enabled = boolean(map, "enabled", true)?;
        let mask = boolean(map, "mask", true)?;
        for key in ["requestBodyMaxSize", "responseBodyMaxSize"] {
            if map
                .get(key)
                .is_some_and(|v| v.as_u64().is_none_or(|n| n == 0 || n > 65536))
            {
                return Err(InitError::Malformed("audit.yml: invalid body limit"));
            }
        }
        let headers = if map.contains_key("headers") {
            strings(map.get("headers"))?
        } else {
            vec![
                "X-Correlation-Id".into(),
                "X-Traceability-Id".into(),
                "caller_id".into(),
            ]
        };
        let fields = if map.contains_key("audit") {
            strings(map.get("audit"))?
        } else {
            vec![
                "client_id".into(),
                "user_id".into(),
                "scope_client_id".into(),
                "endpoint".into(),
                "serviceId".into(),
            ]
        };
        if headers.len() > MAX_SELECTORS
            || fields.len() > MAX_SELECTORS
            || headers.iter().any(|h| !safe_header(h))
            || fields.iter().any(|f| !SAFE_FIELDS.contains(&f.as_str()))
        {
            return Err(InitError::Malformed(
                "audit.yml: unsafe or unbounded selector",
            ));
        }
        let timestamp_format = crate::unsupported::audit::timestamp_format(value)?;
        if service_id.is_empty()
            || service_id.len() > 1024
            || service_id.chars().any(char::is_control)
        {
            return Err(InitError::Malformed(
                "audit.yml: invalid trusted service id",
            ));
        }
        Ok(Self {
            enabled,
            mask,
            mask_plan: Default::default(),
            service_id,
            headers,
            fields,
            status_code: boolean(map, "statusCode", true)?,
            response_time: boolean(map, "responseTime", true)?,
            errors_only: boolean(map, "auditOnError", true)?,
            error_level: boolean(map, "logLevelIsError", false)?,
            stack_trace: boolean(map, "auditStackTrace", false)?,
            timestamp_format,
            sink,
        })
    }
    pub fn with_mask_regex(mut self, value: &Value) -> Result<Self, InitError> {
        self.mask_plan = gateway_policy::audit_mask::AuditMask::from_regex(value)
            .map_err(InitError::Malformed)?;
        Ok(self)
    }
    fn timestamp(&self) -> Value {
        let Some(format) = &self.timestamp_format else {
            return json!(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64
            );
        };
        // UTC/ROOT is the local qualification stand-in. Deployment locale/zone
        // compatibility remains WP5 evidence; no environment authority is read.
        let input = gateway_policy::audit_timestamp::TimeInput {
            date: chrono::Utc::now().fixed_offset(),
            zone_id: "UTC",
            short_zone: "UTC",
            long_zone: "Coordinated Universal Time",
            generic_short_zone: "UTC",
            generic_long_zone: "Coordinated Universal Time",
            locale: gateway_policy::audit_timestamp::Locale::Root,
        };
        match format.format(&input) {
            Ok(value) => gateway_policy::audit::bounded_value(&value),
            Err(_) => json!("<redacted>"),
        }
    }
    fn insert_field(&self, data: &mut Map<String, Value>, name: &str, value: &str) {
        if self.mask && name != "serviceId" {
            let masked = self.mask_plan.apply("audit", name, value);
            insert_selected(data, name, &masked);
        } else {
            insert_selected(data, name, value);
        }
    }
    fn capture(&self, inv: &Invocation, state: Option<&ExchangeState>) -> Completion {
        let mut data = Map::new();
        data.insert("timestamp".into(), self.timestamp());
        for name in &self.headers {
            if let Some(value) = inv
                .headers
                .get(name)
                .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
            {
                let masked = if self.mask {
                    self.mask_plan.apply("requestHeader", name, value)
                } else if value.len() > 1024 {
                    "<truncated>".into()
                } else {
                    value.into()
                };
                insert_selected(&mut data, name, &masked);
            }
        }
        if self.fields.iter().any(|f| f == "serviceId") {
            insert_selected(&mut data, "serviceId", &self.service_id);
        }
        if let Some(state) = state {
            self.principal_fields(&mut data, state);
        }
        Completion {
            handler: self.clone(),
            data,
            start: Instant::now(),
        }
    }
    fn principal_fields(&self, data: &mut Map<String, Value>, state: &ExchangeState) {
        for name in &self.fields {
            if let Some(value) = state.audit.fields.get(name).and_then(Value::as_str) {
                self.insert_field(data, name, value);
            }
        }
        if let Some(principal) = &state.principal {
            for name in &self.fields {
                let value = match name.as_str() {
                    "client_id" => principal.client_id(),
                    "user_id" => principal.user_id(),
                    "scope_client_id" | "caller_id" => {
                        principal.claims().get(name).and_then(Value::as_str)
                    }
                    _ => None,
                };
                if let Some(value) = value {
                    self.insert_field(data, name, value);
                }
            }
        }
    }
}
struct Completion {
    handler: AuditHandler,
    data: Map<String, Value>,
    start: Instant,
}
impl Finalizer for Completion {
    fn finalize(&self, _: &Invocation, state: &ExchangeState, response: &GatewayResponse) {
        let h = &self.handler;
        // Java's immediate branch ignores auditOnError when both switches are
        // false. Emission is deferred to the sole bounded finalization boundary.
        if (h.status_code || h.response_time) && h.errors_only && response.status.as_u16() < 400 {
            return;
        }
        let mut data = self.data.clone();
        if h.status_code || h.response_time {
            h.principal_fields(&mut data, state);
        }
        if h.status_code {
            data.insert("statusCode".into(), json!(response.status.as_u16()));
        }
        if h.response_time {
            data.insert(
                "responseTime".into(),
                json!(self.start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
            );
        }
        h.sink.try_emit(
            RecordKind::Audit {
                error_level: h.error_level,
                stack_trace: h.stack_trace,
            },
            Value::Object(data),
        );
    }
}
#[async_trait]
impl RequestHandler for AuditHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Audit
    }
    fn is_disabled_audit(&self) -> bool {
        !self.enabled
    }
    fn prepare_completion(&self, _: &Invocation) -> Option<Arc<dyn Finalizer>> {
        if !self.enabled {
            return None;
        }
        // Preparation captures configuration only. Selected request headers and
        // masking actions retain their actual on_request timing when reached.
        let mut data = Map::new();
        data.insert("timestamp".into(), self.timestamp());
        if self.fields.iter().any(|field| field == "serviceId") {
            insert_selected(&mut data, "serviceId", &self.service_id);
        }
        Some(Arc::new(Completion {
            handler: self.clone(),
            data,
            start: Instant::now(),
        }))
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        if !self.enabled {
            return Ok(HandlerOutcome::Continue);
        }
        let callback = Arc::new(self.capture(inv, Some(state)));
        state
            .completion
            .capture_prepared(callback)
            .map_err(|_| HandlerFailure)?;
        Ok(HandlerOutcome::Continue)
    }
}
impl HandlerFactory for AuditHandler {
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
