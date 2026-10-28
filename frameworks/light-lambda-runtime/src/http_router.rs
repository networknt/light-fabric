//! Trusted direct-registry HTTP dispatch used by the application forwarding profile.
use async_trait::async_trait;
use bytes::Bytes;
use light_client::{ClientFactory, EndpointOptions};
use serverless_core::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;
use url::Url;

pub struct HttpRouterBackend {
    client: reqwest::Client,
    destinations: BTreeMap<String, DestinationSet>,
    max_request_time: Duration,
    max_response_bytes: usize,
    literal_rewrites: Vec<(String, String)>,
}
struct DestinationSet {
    urls: Vec<String>,
    next: AtomicUsize,
}
impl DestinationSet {
    fn select(&self) -> &str {
        let index = self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(if n + 1 == self.urls.len() { 0 } else { n + 1 })
            })
            .expect("infallible round robin");
        &self.urls[index]
    }
}
fn error(service: &str) -> DispatchError {
    DispatchError(StatusError::new(
        400,
        "ERR10089",
        "FAILED_TO_INVOKE_SERVICE",
        &format!("Failed to invoke HTTP service with serviceId {service}."),
    ))
}
// Invocation paths have already been decoded. Serialize them as path data,
// never parse them as a URL suffix. Reject dot segments before URL normalization.
fn upstream_url(base: &str, path: &str) -> Result<Url, ()> {
    if !path.starts_with('/') || path.split('/').any(|s| matches!(s, "." | "..")) {
        return Err(());
    }
    let mut url = Url::parse(base).map_err(|_| ())?;
    let mut encoded = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(encoded, "%{byte:02X}").map_err(|_| ())?;
        }
    }
    url.set_path(&format!("{}{}", url.path().trim_end_matches('/'), encoded));
    Ok(url)
}

impl HttpRouterBackend {
    /// Only cold-start trusted configuration may populate destinations. No
    /// request field is accepted as a URL, proxy, resolver or redirect policy.
    pub fn new(
        destinations: BTreeMap<String, String>,
        factory: &ClientFactory,
        max_request_time: Duration,
        limits: &serverless_aws::limits::Limits,
    ) -> Result<Self, crate::init_error::InitError> {
        Self::from_catalogue(
            destinations
                .into_iter()
                .map(|(id, url)| (id, vec![url]))
                .collect(),
            factory,
            max_request_time,
            limits,
        )
    }
    pub(crate) fn from_catalogue(
        destinations: BTreeMap<String, Vec<String>>,
        factory: &ClientFactory,
        max_request_time: Duration,
        limits: &serverless_aws::limits::Limits,
    ) -> Result<Self, crate::init_error::InitError> {
        let invalid = || {
            crate::init_error::InitError::InvalidConfiguration(
                "router: invalid trusted direct destination or client",
            )
        };
        limits.validate().map_err(|_| invalid())?;
        let destinations = destinations
            .into_iter()
            .map(|(id, urls)| {
                if id.is_empty() || urls.is_empty() {
                    return Err(invalid());
                }
                for raw in &urls {
                    let url = Url::parse(raw).map_err(|_| invalid())?;
                    if id.is_empty()
                        || !matches!(url.scheme(), "http" | "https")
                        || url.host_str().is_none()
                        || !url.username().is_empty()
                        || url.password().is_some()
                        || url.fragment().is_some()
                        || raw.split_once("://").is_some_and(|(_, tail)| {
                            tail.split(['/', '?', '#'])
                                .next()
                                .is_some_and(|authority| authority.contains('@'))
                        })
                    {
                        return Err(invalid());
                    }
                }
                Ok((
                    id,
                    DestinationSet {
                        urls,
                        next: AtomicUsize::new(0),
                    },
                ))
            })
            .collect::<Result<_, _>>()?;
        // Existing supported shared-client path has exactly ordinary settings
        // plus redirect:none. No shared client behavior or global policy changes.
        let client = factory
            .reqwest_client_lambda_token_no_redirect(EndpointOptions::default())
            .map_err(|_| invalid())?;
        Ok(Self {
            client,
            destinations,
            max_request_time,
            max_response_bytes: limits.max_response_bytes,
            literal_rewrites: Vec::new(),
        })
    }
    pub(crate) fn with_literal_rewrites(mut self, rules: Vec<(String, String)>) -> Self {
        self.literal_rewrites = rules;
        self
    }
    async fn send(
        &self,
        inv: &Invocation,
        id: &str,
        rewritten: Option<&str>,
    ) -> Result<GatewayResponse, DispatchError> {
        let base = self.destinations.get(id).ok_or_else(|| error(id))?.select();
        let method = inv.method.as_str();
        if !["GET", "DELETE", "POST", "PUT", "PATCH"]
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method))
        {
            return Err(error(id));
        }
        let path = rewritten.unwrap_or_else(|| {
            self.literal_rewrites
                .iter()
                .find(|(from, _)| from == &inv.path)
                .map_or(inv.path.as_str(), |(_, to)| to.as_str())
        });
        let target = format!("{base}{path}");
        // JDK HttpRequest.Builder.timeout rejects zero before send, inside
        // Java Router's transport catch. Do not invent a new Init restriction.
        if self.max_request_time.is_zero() {
            return Err(error(&target));
        }
        if !["GET", "DELETE", "POST", "PUT", "PATCH"].contains(&method) {
            return Err(error(&target));
        }
        let mut url = upstream_url(base, path).map_err(|_| error(&target))?;
        if url.origin() != Url::parse(base).map_err(|_| error(id))?.origin() {
            return Err(error(&target));
        }
        // The preserved JDK client accepts URI userinfo but does not turn it
        // into Basic authentication. reqwest otherwise does that implicitly.
        url.set_username("").map_err(|_| error(&target))?;
        url.set_password(None).map_err(|_| error(&target))?;
        let mut headers = inv.headers.clone();
        headers.remove("service_id");
        let mut request = self
            .client
            .request(inv.method.clone(), url)
            .headers(headers);
        if matches!(inv.method.as_str(), "POST" | "PUT" | "PATCH")
            && let Some(body) = &inv.body
        {
            request = request.body(body.clone());
        }
        let mut response = request
            .timeout(self.max_request_time)
            .send()
            .await
            .map_err(|_| error(&target))?;
        let status = response.status();
        // Java's conversion keeps the first value for each response header.
        let mut headers = http::HeaderMap::new();
        for key in response.headers().keys() {
            if let Some(value) = response.headers().get(key) {
                headers.insert(key.clone(), value.clone());
            }
        }
        let mut body = Vec::new();
        let bound = self.max_response_bytes.saturating_add(1);
        while let Some(chunk) = response.chunk().await.map_err(|_| error(&target))? {
            let take = chunk.len().min(bound.saturating_sub(body.len()));
            body.extend_from_slice(&chunk[..take]);
            if body.len() >= bound {
                break;
            }
        }
        // A sentinel-sized oversize result still goes through the established
        // final-envelope encoder/replacement before completion observation.
        let body = Bytes::from(String::from_utf8_lossy(&body).into_owned());
        Ok(GatewayResponse {
            status,
            headers,
            body: Some(body),
            body_is_binary: false,
        })
    }
}
#[async_trait]
impl Backend for HttpRouterBackend {
    async fn dispatch(
        &self,
        inv: &Invocation,
        _: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        if inv.cancel.is_cancelled() || Instant::now() >= inv.deadline {
            return Err(DispatchError(StatusError::timeout()));
        }
        let Target::HttpService(id) = target else {
            return Err(error("unknown"));
        };
        tokio::select! {biased;_=inv.cancel.cancelled()=>Err(DispatchError(StatusError::timeout())),_=tokio::time::sleep_until(inv.deadline)=>Err(DispatchError(StatusError::timeout())),response=self.send(inv,id,None)=>response}
    }
}
/// Opt-in local integration: unadmitted failures remain typed, without a
/// production HTTP mapping or factory registration.
pub enum JavaDispatchFailure {
    Rewrite(crate::router_java_worker::Failure),
    Transport(DispatchError),
}
/// Controller terminal connector only. Application admission and disposition of
/// unavailable/resource outcomes are deliberately not supplied here.
pub enum ControllerDispatchFailure {
    Rewrite(crate::router_java_worker::recovery::RecoveryFailure),
    Transport(DispatchError),
}
impl HttpRouterBackend {
    /// Uses the final path after request policies, and one invocation deadline
    /// for controller wait, rewrite and HTTP. Never retries a failed rewrite.
    pub async fn dispatch_with_controller(
        &self,
        inv: &Invocation,
        state: &ExchangeState,
        target: &Target,
        controller: &crate::router_java_worker::recovery::RecoveryController,
    ) -> Result<GatewayResponse, ControllerDispatchFailure> {
        self.dispatch_with_rewriter(inv, state, target, controller)
            .await
    }
    pub async fn dispatch_with_rewriter<R: crate::router_rewrite_policy::Rewriter + ?Sized>(
        &self,
        inv: &Invocation,
        state: &ExchangeState,
        target: &Target,
        controller: &R,
    ) -> Result<GatewayResponse, ControllerDispatchFailure> {
        use ControllerDispatchFailure::{Rewrite, Transport};
        if let Some(error) = state.failed_request_error() {
            return Err(Transport(DispatchError(error)));
        }
        if inv.cancel.is_cancelled() || Instant::now() >= inv.deadline {
            return Err(Transport(DispatchError(StatusError::timeout())));
        }
        let Target::HttpService(id) = target else {
            return Err(Transport(error("unknown")));
        };
        let candidate = tokio::select! {biased;
            _=inv.cancel.cancelled()=>return Err(Transport(DispatchError(StatusError::timeout()))),
            _=tokio::time::sleep_until(inv.deadline)=>return Err(Transport(DispatchError(StatusError::timeout()))),
            candidate=controller.rewrite(&inv.path, inv.deadline, inv.cancel.clone())=>candidate.map_err(Rewrite)?,
        };
        // No destination selection or HTTP happens until rewriting succeeds.
        // Candidate remains borrowed for send; exchange path is never mutated.
        tokio::select! {biased;
            _=inv.cancel.cancelled()=>Err(Transport(DispatchError(StatusError::timeout()))),
            _=tokio::time::sleep_until(inv.deadline)=>Err(Transport(DispatchError(StatusError::timeout()))),
            response=self.send(inv,id,Some(candidate.path()))=>response.map_err(Transport)
        }
    }
}
impl HttpRouterBackend {
    pub async fn dispatch_with_java(
        &self,
        inv: &Invocation,
        target: &Target,
        worker: &crate::router_java_worker::Session,
    ) -> Result<GatewayResponse, JavaDispatchFailure> {
        use JavaDispatchFailure::{Rewrite, Transport};
        let Target::HttpService(id) = target else {
            return Err(Transport(error("unknown")));
        };
        if inv.cancel.is_cancelled() || Instant::now() >= inv.deadline {
            return Err(Transport(DispatchError(StatusError::timeout())));
        }
        let path = worker
            .rewrite(&inv.path, inv.deadline, inv.cancel.clone())
            .await
            .map_err(Rewrite)?;
        // Invocation is borrowed throughout: no policy candidate is committed,
        // no full invocation clone, and no stale request-stage path is used.
        tokio::select! {biased;
            _=inv.cancel.cancelled()=>Err(Transport(DispatchError(StatusError::timeout()))),
            _=tokio::time::sleep_until(inv.deadline)=>Err(Transport(DispatchError(StatusError::timeout()))),
            response=self.send(inv,id,Some(path.path()))=>response.map_err(Transport)
        }
    }
}
pub struct ComposedBackend {
    pub lambda: Arc<dyn Backend>,
    pub http: Arc<dyn Backend>,
}
#[async_trait]
impl Backend for ComposedBackend {
    async fn dispatch(
        &self,
        inv: &Invocation,
        state: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        match target {
            Target::Function(_) => self.lambda.dispatch(inv, state, target).await,
            Target::HttpService(_) => self.http.dispatch(inv, state, target).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn immutable_catalogue_round_robin_is_exact_under_concurrency() {
        let set = Arc::new(DestinationSet {
            urls: vec!["one".into(), "two".into()],
            next: AtomicUsize::new(0),
        });
        let counts = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let set = set.clone();
                let counts = counts.clone();
                scope.spawn(move || {
                    for _ in 0..1000 {
                        let i = usize::from(set.select() == "two");
                        counts[i].fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(counts[0].load(Ordering::Relaxed), 4000);
        assert_eq!(counts[1].load(Ordering::Relaxed), 4000);
        assert_eq!(set.urls, ["one", "two"]);
    }
}

#[cfg(test)]
mod path_serialization_tests {
    use super::*;
    #[test]
    fn decoded_path_cannot_introduce_url_syntax_or_escape_base() {
        let url = upstream_url("https://example.test/base", "/x?a=1#tail\\z/%2e%2e/雪").unwrap();
        assert_eq!(
            url.path(),
            "/base/x%3Fa%3D1%23tail%5Cz/%252e%252e/%E9%9B%AA"
        );
        assert_eq!(url.query(), None);
        assert_eq!(url.fragment(), None);
        for path in ["/../escape", "/./escape", "/a/../escape"] {
            assert!(upstream_url("https://example.test/base", path).is_err());
        }
    }
}
