use crate::{HandlerId, StatusError};
use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use serde_json::Value;
use std::{collections::BTreeMap, net::IpAddr, sync::Arc, time::Duration};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MultiMap(pub Vec<(String, String)>);
impl MultiMap {
    /// Java path captures overwrite the query value of the same (case-sensitive) key.
    pub fn set(&mut self, key: String, value: String) {
        self.0.retain(|(k, _)| k != &key);
        self.0.push((key, value));
    }
}

#[derive(Clone, Debug, Default)]
pub struct SourceMetadata {
    /// Supplied by the trusted ingress adapter, never inferred here from headers.
    pub source_ip: Option<IpAddr>,
    pub ingress_kind: String,
}

#[derive(Clone, Debug)]
pub struct Invocation {
    pub method: Method,
    pub raw_path: String,
    pub path: String,
    pub raw_query: Option<String>,
    pub query: MultiMap,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
    pub body_was_base64: bool,
    pub source: SourceMetadata,
    pub request_id: String,
    pub deadline: Instant,
    pub response_deadline: Instant,
    pub cancel: CancellationToken,
    pub path_params: BTreeMap<String, String>,
    pub route: Option<RouteMatch>,
    pub original: Value,
}

/// Only trusted authentication code may construct this from a successful verifier
/// result. Deliberately no Deserialize, Default, or header constructor. WP9a owns
/// conversion from light_security::AuthPrincipal through handlers::jwt's local
/// adapter. aws-lc-rs/aws-lc-sys are cryptography, not AWS service SDKs.
/// Like AuthPrincipal, this
/// is a trusted-code boundary, not a cryptographic proof against malicious Rust code.
#[derive(Clone, Debug)]
pub struct Principal {
    principal_id: String,
    client_id: Option<String>,
    user_id: Option<String>,
    claims: Value,
}
impl Principal {
    /// Inputs must come from successful authentication, never the incoming envelope.
    pub fn from_verified(
        principal_id: String,
        client_id: Option<String>,
        user_id: Option<String>,
        claims: Value,
    ) -> Self {
        Self {
            principal_id,
            client_id,
            user_id,
            claims,
        }
    }
    pub fn principal_id(&self) -> &str {
        &self.principal_id
    }
    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }
    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }
    pub fn claims(&self) -> &Value {
        &self.claims
    }
}

#[derive(Clone, Debug, Default)]
pub struct AuditInfo {
    pub status: Option<StatusError>,
    pub fields: gateway_policy::audit::AuditFields,
}
#[derive(Clone, Debug, Default)]
pub struct MetricsRecord {
    pub dispatches: u32,
    pub dispatch_latency: Duration,
    pub finalized: bool,
}
#[derive(Default)]
pub struct ExchangeState {
    request_state: crate::request_state::RequestState,
    validator_media: crate::ValidatorMediaProof,
    validator_media_pending: Option<crate::ValidatorMediaProof>,
    validator_media_transaction: bool,
    http_route_target: Option<Target>,
    pub principal: Option<Principal>,
    /// Specification alone selects this from an immutable local document.
    /// It is not deserialized from ingress, authentication proof, or a backend
    /// RouteMatch. A fresh invocation starts None; consumers remain separate.
    pub operation: Option<Arc<gateway_policy::openapi_operation::OperationDescriptor>>,
    pub audit: AuditInfo,
    pub correlation_id: Option<String>,
    pub traceability_id: Option<String>,
    pub response_headers: HeaderMap,
    pub metrics: MetricsRecord,
    pub completion: CompletionCallbacks,
}

impl ExchangeState {
    /// Trusted policy API only; no ingress deserialization or caller-supplied flags.
    /// Record the first ERROR request status, retaining its original cause.
    pub fn record_request_status(&mut self, status: StatusError) {
        self.request_state.record(status);
    }
    pub fn request_failure(&self) -> Option<&StatusError> {
        self.request_state.failure()
    }
    /// Native request-phase projection only, not a complete Java exchange lifecycle.
    pub fn native_request_state(&self) -> u16 {
        self.request_state.native_flags()
    }
    pub fn failed_request_error(&self) -> Option<StatusError> {
        self.request_state.routing_rejection()
    }

    pub fn validator_media_is_json(&self) -> Result<bool, HandlerFailure> {
        self.validator_media.validates_json()
    }
    pub fn validator_content_type(&self) -> Result<Option<&http::HeaderValue>, HandlerFailure> {
        self.validator_media.exact_content_type()
    }
    /// Opaque trusted staging snapshot; never serialization or telemetry data.
    pub fn validator_media_snapshot(&self) -> crate::ValidatorMediaProof {
        self.validator_media.clone()
    }
    pub fn commit_validator_media(&mut self, proof: crate::ValidatorMediaProof) {
        if self.validator_media_transaction {
            self.validator_media_pending = Some(proof);
        } else {
            self.validator_media = proof;
        }
    }
    pub(crate) fn begin_validator_media_transaction(&mut self) {
        self.validator_media_pending = None;
        self.validator_media_transaction = true;
    }
    pub(crate) fn end_validator_media_transaction(&mut self, success: bool) {
        let pending = self.validator_media_pending.take();
        self.validator_media_transaction = false;
        if success && let Some(proof) = pending {
            self.validator_media = proof;
        }
    }
    pub(crate) fn select_http_target(&mut self, service: String) {
        if self.http_route_target.is_none() {
            self.http_route_target = Some(Target::HttpService(service));
        }
    }
    pub(crate) fn has_http_target(&self) -> bool {
        self.http_route_target.is_some()
    }
    pub(crate) fn http_target(&self) -> Option<&Target> {
        self.http_route_target.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
    pub body_is_binary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A configured logical alias. Provider validation/resolution belongs to WP8.
    Function(String),
    HttpService(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteMatch {
    pub template: String,
    pub target: Option<Target>,
}

pub enum HandlerOutcome {
    Continue,
    /// Java CORS preflight installs the first response but still runs the chain.
    /// A later denial, failure or exhausted deadline overrides this candidate.
    ContinueWithInitialResponse(GatewayResponse),
    Respond(GatewayResponse),
    Reject(StatusError),
}
/// Failures are distinct from deliberate policy rejections; never expose diagnostics.
#[derive(Debug)]
pub struct HandlerFailure;
/// Compiler-only immutable role facts; this does not perform authentication.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OperationAuthorizationRole {
    #[default]
    None,
    MetadataProducer,
    /// A real request validator consumes preceding qualified metadata.
    MetadataConsumer,
    /// A possible JWT scope path. Mixed/skip paths cannot guarantee a check.
    JwtScopes {
        guaranteed: bool,
    },
}
#[async_trait]
pub trait RequestHandler: Send + Sync {
    fn id(&self) -> HandlerId;
    /// Audit-only Init removal signal; all other request handlers remain active.
    fn is_disabled_audit(&self) -> bool {
        false
    }
    fn operation_authorization_role(&self) -> OperationAuthorizationRole {
        OperationAuthorizationRole::None
    }
    /// Init-only read-only selection facts; default preserves global behavior.
    fn operation_authorization_role_for_selection(
        &self,
        _: &crate::plan::OperationSelection,
    ) -> OperationAuthorizationRole {
        self.operation_authorization_role()
    }
    /// Immutable fallback templates for Metrics admission only. The compiler
    /// consults this only for selected active Metrics occurrences.
    fn metrics_fallback_routes(&self) -> Option<&crate::plan::TargetRoutes> {
        None
    }
    /// Bounded invocation-local completion capture; implementations must not do
    /// request policy work here. Only observation policies use this hook.
    fn prepare_completion(&self, _: &Invocation) -> Option<Arc<dyn Finalizer>> {
        None
    }
    #[must_use = "the on_request future must be awaited to perform the operation"]
    async fn on_request(
        &self,
        inv: &mut Invocation,
        st: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure>;
}
#[async_trait]
pub trait ResponseHandler: Send + Sync {
    fn id(&self) -> HandlerId;
    #[must_use = "the on_response future must be awaited to perform the operation"]
    async fn on_response(
        &self,
        inv: &Invocation,
        st: &mut ExchangeState,
        resp: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure>;
}
#[derive(Clone, Debug)]
pub struct DispatchError(pub StatusError);
#[async_trait]
pub trait Backend: Send + Sync {
    /// The absolute inv.deadline includes all request policy time already spent.
    #[must_use = "the dispatch future must be awaited to perform the operation"]
    async fn dispatch(
        &self,
        inv: &Invocation,
        st: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError>;
}

/// Synchronous, bounded, nonblocking completion hooks. Concrete audit/EMF sinks are
/// later wiring. Callbacks are consumed in reverse registration order, even if one panics.
pub trait Finalizer: Send + Sync {
    fn finalize(&self, inv: &Invocation, st: &ExchangeState, response: &GatewayResponse);
}
pub type Finalizers = Vec<Arc<dyn Finalizer>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletionId(u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlreadyFinalized;
/// Request-local completion registrations. Closed after drain; callbacks are
/// returned in reverse order and can never run twice through this collection.
#[derive(Default)]
pub struct CompletionCallbacks {
    next_id: u64,
    finished: bool,
    callbacks: Vec<(CompletionId, Arc<dyn Finalizer>)>,
    slots: Vec<(CompletionId, usize)>,
    prepared: Vec<(usize, CompletionId)>,
    active: Option<(usize, u64)>,
}
impl CompletionCallbacks {
    pub(crate) fn prepare(&mut self, slot: usize, callback: Arc<dyn Finalizer>) {
        if let Ok(id) = self.add(callback) {
            self.slots.push((id, slot));
            self.prepared.push((slot, id));
        }
    }
    pub(crate) fn enter_policy(&mut self, slot: usize) {
        self.active = Some((slot, self.next_id));
    }
    pub(crate) fn leave_policy(&mut self) {
        if let Some((slot, first)) = self.active.take() {
            for (id, _) in &self.callbacks {
                if id.0 >= first {
                    self.slots.push((*id, slot));
                }
            }
        }
    }
    /// Replace the current occurrence's fallback capture without registering
    /// twice or resurrecting a removed callback. Direct callers retain add.
    pub fn capture_prepared(
        &mut self,
        callback: Arc<dyn Finalizer>,
    ) -> Result<(), AlreadyFinalized> {
        if self.finished {
            return Err(AlreadyFinalized);
        }
        if let Some((slot, _)) = self.active
            && let Some((_, id)) = self.prepared.iter().find(|(s, _)| *s == slot)
        {
            if let Some((_, existing)) = self.callbacks.iter_mut().find(|(i, _)| i == id) {
                *existing = callback;
            }
            return Ok(());
        }
        self.add(callback).map(|_| ())
    }
    pub fn add(&mut self, callback: Arc<dyn Finalizer>) -> Result<CompletionId, AlreadyFinalized> {
        if self.finished {
            return Err(AlreadyFinalized);
        }
        let id = CompletionId(self.next_id);
        self.next_id = self.next_id.checked_add(1).expect("completion id overflow");
        self.callbacks.push((id, callback));
        Ok(id)
    }
    pub fn remove(&mut self, id: CompletionId) -> Result<bool, AlreadyFinalized> {
        if self.finished {
            return Err(AlreadyFinalized);
        }
        let before = self.callbacks.len();
        self.callbacks.retain(|(i, _)| *i != id);
        Ok(self.callbacks.len() != before)
    }
    pub(crate) fn finish(&mut self) -> Result<Finalizers, AlreadyFinalized> {
        if self.finished {
            return Err(AlreadyFinalized);
        }
        self.leave_policy();
        self.finished = true;
        // Reached callbacks retain their registration order within each policy.
        // Unreached observation slots keep their selected-chain position. Later
        // response-phase registrations follow all request slots, as before.
        self.callbacks.sort_by_key(|(id, _)| {
            (
                self.slots
                    .iter()
                    .find(|(i, _)| i == id)
                    .map_or(usize::MAX, |(_, slot)| *slot),
                id.0,
            )
        });
        Ok(self.callbacks.drain(..).rev().map(|(_, c)| c).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Observer;
    impl Finalizer for Observer {
        fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {}
    }
    #[test]
    fn completion_removal_and_repeated_finalization_rejection() {
        let mut c = CompletionCallbacks::default();
        let a: Arc<dyn Finalizer> = Arc::new(Observer);
        let b: Arc<dyn Finalizer> = Arc::new(Observer);
        let id = c.add(a.clone()).unwrap();
        c.add(b.clone()).unwrap();
        let removed = c.add(Arc::new(Observer)).unwrap();
        assert!(c.remove(removed).unwrap());
        assert!(!c.remove(removed).unwrap());
        let callbacks = c.finish().unwrap();
        assert_eq!(callbacks.len(), 2);
        assert!(Arc::ptr_eq(&callbacks[0], &b));
        assert!(Arc::ptr_eq(&callbacks[1], &a));
        assert!(c.finish().is_err());
        assert_eq!(c.add(a), Err(AlreadyFinalized));
        assert_eq!(c.remove(id), Err(AlreadyFinalized));
    }
}

#[cfg(test)]
mod completion_slot_tests {
    use super::*;
    struct Observer;
    impl Finalizer for Observer {
        fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {}
    }
    #[test]
    fn prepared_slots_preserve_relative_order_and_replacement() {
        let mut c = CompletionCallbacks::default();
        let first: Arc<dyn Finalizer> = Arc::new(Observer);
        let middle: Arc<dyn Finalizer> = Arc::new(Observer);
        let last: Arc<dyn Finalizer> = Arc::new(Observer);
        let actual: Arc<dyn Finalizer> = Arc::new(Observer);
        c.prepare(0, first.clone());
        c.prepare(2, last.clone());
        c.enter_policy(0);
        c.capture_prepared(actual.clone()).unwrap();
        c.leave_policy();
        c.enter_policy(1);
        c.add(middle.clone()).unwrap();
        // Finalization may occur while an entered future is canceled.
        let callbacks = c.finish().unwrap();
        assert_eq!(callbacks.len(), 3);
        assert!(Arc::ptr_eq(&callbacks[0], &last));
        assert!(Arc::ptr_eq(&callbacks[1], &middle));
        assert!(Arc::ptr_eq(&callbacks[2], &actual));
        assert!(c.finish().is_err());
    }
    #[test]
    fn removed_prepared_slot_is_not_resurrected_and_occurrences_are_distinct() {
        let mut c = CompletionCallbacks::default();
        let observer: Arc<dyn Finalizer> = Arc::new(Observer);
        c.prepare(0, observer.clone());
        c.prepare(1, observer.clone());
        let removed = c.prepared[0].1;
        assert!(c.remove(removed).unwrap());
        c.enter_policy(0);
        c.capture_prepared(observer.clone()).unwrap();
        c.leave_policy();
        c.enter_policy(1);
        c.capture_prepared(observer).unwrap();
        c.leave_policy();
        assert_eq!(c.finish().unwrap().len(), 1);
    }
}

impl CompletionCallbacks {
    /// Exercise the actual finish operation on a read-only state snapshot.
    /// Observers cannot reopen or mutate the invocation's original collection.
    pub fn observe_repeat(&self) -> Result<(), AlreadyFinalized> {
        let mut snapshot = Self {
            next_id: self.next_id,
            finished: self.finished,
            callbacks: self.callbacks.clone(),
            slots: self.slots.clone(),
            prepared: self.prepared.clone(),
            active: self.active,
        };
        snapshot.finish().map(|_| ())
    }
}
