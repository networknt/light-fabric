//! Closed fixture work only. Init compiles the injected chain; production
//! pipeline deadlines, dispatch, cancellation and finalization remain unchanged.
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use light_lambda_runtime::init::{HandlerFactoryProvider, InitInput};
use light_runtime::RuntimeConfig;
use serde_json::{Value, json};
use serverless_core::{
    plan::{Binding, HandlerFactory, RequestCapabilities},
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Default)]
pub(crate) struct Work {
    entries: AtomicUsize,
    pub counts: Arc<crate::policy_observer::Counts>,
    failure: Option<String>,
}
impl Work {
    pub fn entries(&self) -> usize {
        self.entries.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl RequestHandler for Work {
    fn id(&self) -> HandlerId {
        HandlerId::RequestHeader
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        self.entries.fetch_add(1, Ordering::SeqCst);
        if let Some(mode) = &self.failure {
            if mode == "throw-test-exception" {
                panic!("test-only request failure");
            }
            return Err(HandlerFailure);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(HandlerOutcome::Continue)
    }
}
#[async_trait]
impl ResponseHandler for Work {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseHeader
    }
    async fn on_response(
        &self,
        _: &Invocation,
        _: &mut ExchangeState,
        _: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        self.entries.fetch_add(1, Ordering::SeqCst);
        self.counts.entry();
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.counts.complete();
        Ok(())
    }
}
struct Provider(Arc<Work>, bool);
struct Factory(Arc<Work>, bool);
impl HandlerFactoryProvider for Provider {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<light_security::SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, light_lambda_runtime::init_error::InitError> {
        Ok(Arc::new(Factory(self.0.clone(), self.1)))
    }
}
impl HandlerFactory for Factory {
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<Binding, InitError> {
        Ok(if self.1 {
            Binding::Request(self.0.clone(), RequestCapabilities::default())
        } else {
            Binding::Response(self.0.clone())
        })
    }
}
pub(crate) fn configure(f: &Value, config: &mut Value) -> Result<Option<Arc<Work>>> {
    let mode = f.get("boundedWorkProbe");
    let failure = f.get("requestFailureProbe");
    if mode.is_none() && failure.is_none() {
        return Ok(None);
    }
    ensure!(
        mode.is_none() || failure.is_none(),
        "cannot combine work and failure"
    );
    if let Some(failure) = failure {
        ensure!(
            matches!(
                failure.as_str(),
                Some("return-null-status" | "throw-test-exception")
            ),
            "unknown failure recipe"
        );
    }
    let mode = mode.and_then(Value::as_str).unwrap_or("request-100ms-v1");
    let stages: &[&str] = match mode {
        "request-100ms-v1" | "request-ingress50-100ms-v1" => &["request"],
        "response-100ms-v1" => &["response"],
        "request-response-100ms-v1" => &["request", "response"],
        _ => anyhow::bail!("unknown bounded work probe"),
    };
    ensure!(
        config["files"].get("header").is_none(),
        "work probe cannot shadow header configuration"
    );
    let handlers = &mut config["files"]["handler"];
    for id in [HandlerId::RequestHeader, HandlerId::ResponseHeader] {
        let class = serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class;
        ensure!(
            !handlers["handlers"]
                .as_array()
                .context("handler declarations")?
                .iter()
                .any(|s| s.as_str().is_some_and(|s| s.starts_with(class))),
            "work probe cannot replace a production header handler"
        );
    }
    for stage in stages {
        let id = if *stage == "request" {
            HandlerId::RequestHeader
        } else {
            HandlerId::ResponseHeader
        };
        let class = serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class;
        ensure!(
            !handlers["handlers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s.as_str().is_some_and(|s| s.starts_with(class))),
            "work probe cannot replace a production header handler"
        );
        let alias = format!("fixtureWork{stage}");
        handlers["handlers"]
            .as_array_mut()
            .unwrap()
            .push(json!(format!("{class}@{alias}")));
        handlers["chains"][*stage]
            .as_array_mut()
            .unwrap()
            .push(json!(alias));
    }
    config["files"]["header"] = json!({"enabled":true});
    Ok(Some(Arc::new(Work {
        failure: failure.and_then(Value::as_str).map(str::to_owned),
        ..Work::default()
    })))
}
pub(crate) fn install(input: &mut InitInput, work: &Arc<Work>) {
    for registration in &mut input.handlers {
        let request = registration.id == HandlerId::RequestHeader;
        if request || registration.id == HandlerId::ResponseHeader {
            registration.factory = Some(Arc::new(Provider(work.clone(), request)));
        }
    }
}
