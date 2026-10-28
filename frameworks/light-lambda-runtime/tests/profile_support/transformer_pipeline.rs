//! Qualification adapter for the approved request/response body pair only.
//! No production registry, error mapping, action framework or extra CEL surface.
use super::transformer;
use async_trait::async_trait;
use serde_json::{Value, json};
use serverless_core::{plan::*, *};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;
fn quoted(s: &str) -> Result<usize, HandlerFailure> {
    if s.len() > transformer::CONTEXT_BYTES {
        return Err(HandlerFailure);
    }
    Ok(2 + s
        .chars()
        .map(|c| match c {
            '"' | '\\' | '\u{8}' | '\t' | '\n' | '\u{c}' | '\r' => 2,
            c if (c as u32) < 32 => 6,
            c => c.len_utf8(),
        })
        .sum::<usize>())
}
fn text(body: Option<&bytes::Bytes>) -> Result<Option<&str>, HandlerFailure> {
    body.map(|b| std::str::from_utf8(b).map_err(|_| HandlerFailure))
        .transpose()
}
fn input(inv: &Invocation, response: Option<&GatewayResponse>) -> Result<Value, HandlerFailure> {
    let body = text(inv.body.as_ref())?;
    let response_body = response
        .map(|r| text(r.body.as_ref()))
        .transpose()?
        .flatten();
    let mut size = b"{\"requestBody\":,\"requestPath\":,\"requestHeaders\":}".len()
        + body.map_or(Ok(4), quoted)?
        + quoted(&inv.path)?
        + 2;
    if response.is_some() {
        size += b",\"responseBody\":".len() + response_body.map_or(Ok(4), quoted)?;
    }
    if size > transformer::CONTEXT_BYTES {
        return Err(HandlerFailure);
    }
    let mut fields = Vec::new();
    for name in inv.headers.keys() {
        if fields.len() == 64 || name.as_str().len() > 256 {
            return Err(HandlerFailure);
        }
        let value = std::str::from_utf8(inv.headers.get(name).expect("header key").as_bytes())
            .map_err(|_| HandlerFailure)?;
        size += quoted(name.as_str())? + 1 + quoted(value)? + usize::from(!fields.is_empty());
        if size > transformer::CONTEXT_BYTES {
            return Err(HandlerFailure);
        }
        fields.push((name.as_str(), value));
    }
    // All borrowed sizes and counts passed before copying caller data.
    let headers = fields
        .into_iter()
        .map(|(k, v)| (k.to_owned(), Value::String(v.to_owned())))
        .collect::<serde_json::Map<_, _>>();
    let mut context = json!({"requestBody":body,"requestPath":inv.path,"requestHeaders":headers});
    if response.is_some() {
        context["responseBody"] = json!(response_body);
    }
    transformer::context(&context).map_err(|_| HandlerFailure)?;
    Ok(context)
}
struct BodyPair(Arc<transformer::Candidate>);
#[async_trait]
impl RequestHandler for BodyPair {
    fn id(&self) -> HandlerId {
        HandlerId::RequestTransformer
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        let context = input(inv, None)?;
        let candidate = self.0.clone();
        let result = transformer::worker(context, inv.deadline, inv.cancel.clone(), move |value| {
            candidate.execute(value, "/x@post", "req-tra")
        })
        .await
        .map_err(|_| HandlerFailure)?;
        inv.body = result["requestBody"]
            .as_str()
            .map(|s| bytes::Bytes::from(s.to_owned()));
        Ok(HandlerOutcome::Continue)
    }
}
#[async_trait]
impl ResponseHandler for BodyPair {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseTransformer
    }
    async fn on_response(
        &self,
        inv: &Invocation,
        _: &mut ExchangeState,
        response: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        let context = input(inv, Some(response))?;
        assert_eq!(context["requestBody"], "request-transformed");
        assert_eq!(context["requestHeaders"]["x-input"], "request-value");
        let candidate = self.0.clone();
        let result = transformer::worker(
            context,
            inv.response_deadline,
            inv.cancel.clone(),
            move |value| candidate.execute(value, "/x@post", "res-tra"),
        )
        .await
        .map_err(|_| HandlerFailure)?;
        response.body = result["responseBody"]
            .as_str()
            .map(|s| bytes::Bytes::from(s.to_owned()));
        Ok(())
    }
}
struct Factory(Binding);
impl HandlerFactory for Factory {
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<Binding, InitError> {
        Ok(self.0.clone())
    }
}
#[derive(Default)]
struct Counts {
    dispatches: AtomicUsize,
    finalizations: AtomicUsize,
}
#[async_trait]
impl Backend for Counts {
    async fn dispatch(
        &self,
        inv: &Invocation,
        _: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        assert_eq!(target, &Target::Function("FnX".into()));
        assert_eq!(inv.body.as_deref(), Some(b"request-transformed".as_slice()));
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: Some(bytes::Bytes::from_static(b"ok")),
            body_is_binary: false,
        })
    }
}
impl Finalizer for Counts {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, response: &GatewayResponse) {
        assert_eq!(
            response.body.as_deref(),
            Some(b"response-transformed".as_slice())
        );
        self.finalizations.fetch_add(1, Ordering::SeqCst);
    }
}
pub async fn actual_body_pair_uses_current_context_and_one_finalized_dispatch() {
    let fixture:Value=serde_json::from_str(include_str!("../../../../tests/serverless-conformance/fixtures/v1/t/T-02-actual-native-response-input-map-multi-contract-v1.json")).unwrap();
    let candidate = Arc::new(
        transformer::Candidate::compile(&fixture["ruleConfigurationPair"]["rust"]["rule"]).unwrap(),
    );
    transformer::aggregate(&[(&candidate, "req-tra"), (&candidate, "res-tra")]).unwrap();
    let pair = Arc::new(BodyPair(candidate));
    let mut registry = Registry::default();
    registry.register(
        HandlerId::RequestTransformer,
        Arc::new(Factory(Binding::Request(
            pair.clone(),
            RequestCapabilities::default(),
        ))),
    );
    registry.register(
        HandlerId::ResponseTransformer,
        Arc::new(Factory(Binding::Response(pair))),
    );
    registry.register(
        HandlerId::Proxy,
        Arc::new(Factory(Binding::Dispatch(
            TargetRoutes::compile([("/x@post".into(), Target::Function("FnX".into()))]).unwrap(),
        ))),
    );
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    let plan=ExecutionPlan::compile(json!({"handlers":[format!("{}@request",class(HandlerId::RequestTransformer)),format!("{}@response",class(HandlerId::ResponseTransformer)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["request","proxy","response"]}),json!({}),&registry).unwrap();
    let counts = Arc::new(Counts::default());
    let pipeline = Pipeline::new(Arc::new(plan), vec![counts.clone()]);
    let mut headers = http::HeaderMap::new();
    headers.insert("x-input", "request-value".parse().unwrap());
    let inv = Invocation {
        method: http::Method::POST,
        raw_path: "/x".into(),
        path: "/x".into(),
        raw_query: None,
        query: MultiMap::default(),
        headers,
        body: Some(bytes::Bytes::from_static(b"mutate")),
        body_was_base64: false,
        source: SourceMetadata::default(),
        request_id: "test-only".into(),
        deadline: Instant::now() + Duration::from_secs(1),
        response_deadline: Instant::now() + Duration::from_secs(2),
        cancel: tokio_util::sync::CancellationToken::new(),
        path_params: Default::default(),
        route: None,
        original: Value::Null,
    };
    let mut sizing = inv.clone();
    sizing.body = Some(bytes::Bytes::new());
    let base = input(&sizing, None).unwrap().to_string().len();
    sizing.body = Some(bytes::Bytes::from(vec![
        b'x';
        transformer::CONTEXT_BYTES - base
    ]));
    assert_eq!(
        input(&sizing, None).unwrap().to_string().len(),
        transformer::CONTEXT_BYTES
    );
    sizing.body = Some(bytes::Bytes::from(vec![
        b'x';
        transformer::CONTEXT_BYTES - base
            + 1
    ]));
    assert!(input(&sizing, None).is_err());
    sizing.body = None;
    sizing.headers.clear();
    for i in 0..64 {
        sizing.headers.insert(
            format!("h{i}").parse::<http::HeaderName>().unwrap(),
            "value".parse().unwrap(),
        );
    }
    assert!(input(&sizing, None).is_ok());
    sizing.headers.insert("extra", "value".parse().unwrap());
    assert!(input(&sizing, None).is_err());
    assert_eq!(
        pipeline.execute(inv, &*counts).await.body.as_deref(),
        Some(b"response-transformed".as_slice())
    );
    assert_eq!(counts.dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(counts.finalizations.load(Ordering::SeqCst), 1);
}
