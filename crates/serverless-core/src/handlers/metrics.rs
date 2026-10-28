//! One policy invocation registers one bounded completion record.
use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
    *,
};
use async_trait::async_trait;
use gateway_policy::observation::{RecordKind, RecordSink};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

#[derive(Clone)]
pub struct MetricsHandler {
    function_name: String,
    sink: Arc<dyn RecordSink>,
    routes: Arc<crate::plan::TargetRoutes>,
}
impl MetricsHandler {
    pub fn new(function_name: String, sink: Arc<dyn RecordSink>) -> Result<Self, InitError> {
        if function_name.is_empty()
            || function_name.len() > 256
            || function_name.chars().any(char::is_control)
        {
            return Err(InitError::Malformed(
                "metrics: invalid trusted function name",
            ));
        }
        Ok(Self {
            function_name,
            sink,
            routes: Arc::new(crate::plan::TargetRoutes::default()),
        })
    }
    pub fn with_routes(mut self, routes: Arc<crate::plan::TargetRoutes>) -> Self {
        self.routes = routes;
        self
    }
}
struct Completion {
    handler: MetricsHandler,
    start: Instant,
    timestamp_ms: u64,
}
impl Finalizer for Completion {
    fn finalize(&self, inv: &Invocation, _: &ExchangeState, response: &GatewayResponse) {
        let route = inv
            .route
            .as_ref()
            .map(|route| route.template.as_str())
            .or_else(|| {
                self.handler
                    .routes
                    .matching_template(&inv.method, &inv.path)
            })
            .unwrap_or("<unmatched>");
        self.handler.sink.try_emit(
            RecordKind::Emf,
            gateway_policy::metrics::record(
                &self.handler.function_name,
                route,
                self.timestamp_ms,
                self.start.elapsed(),
                response.status.is_client_error() || response.status.is_server_error(),
            ),
        );
    }
}
#[async_trait]
impl RequestHandler for MetricsHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Metrics
    }
    fn metrics_fallback_routes(&self) -> Option<&crate::plan::TargetRoutes> {
        Some(&self.routes)
    }
    fn prepare_completion(&self, _: &Invocation) -> Option<Arc<dyn Finalizer>> {
        Some(Arc::new(Completion {
            handler: self.clone(),
            start: Instant::now(),
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        }))
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        state
            .completion
            .capture_prepared(Arc::new(Completion {
                handler: self.clone(),
                start: Instant::now(),
                timestamp_ms,
            }))
            .map_err(|_| HandlerFailure)?;
        Ok(HandlerOutcome::Continue)
    }
}
impl HandlerFactory for MetricsHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Preserves
    }
    fn build(&self, compat: &Compatibility, _: &str) -> Result<Binding, InitError> {
        if compat.class.contains("InfluxMetricsMiddleware") {
            return Err(InitError::Malformed("metrics: Influx sink deferred"));
        }
        Ok(Binding::Request(
            Arc::new(self.clone()),
            RequestCapabilities::default(),
        ))
    }
}
