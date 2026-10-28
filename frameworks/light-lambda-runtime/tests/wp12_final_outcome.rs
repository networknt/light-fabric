//! WP12 C-03/C-09/D-09 qualification: completion must observe the final ALB outcome.
//! Synthetic in-process backend only; no SDK, credentials, network, or fixture runner.
use async_trait::async_trait;
use bytes::Bytes;
use config_loader::EmbeddedConfigFile;
use http::{HeaderMap, StatusCode};
use light_lambda_runtime::{init::*, service::handle};
use serde_json::{Value, json};
use serverless_aws::target::GatewayIdentity;
use serverless_core::{compat::COMPATIBILITY, *};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

static EMBEDDED: &[EmbeddedConfigFile] = &[
    EmbeddedConfigFile {
        name: "server.yml",
        content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
    },
    EmbeddedConfigFile {
        name: "light-lambda.yml",
        content: "stage: test\ningressTimeoutMs: 1000\nresponseReserveMs: 200\npostReserveMs: 50\nmaxResponseBytes: 4096\n",
    },
    EmbeddedConfigFile {
        name: "lambda-proxy.yml",
        content: "functions:\n  /x@get: test-only-business\nregion: us-east-1\n",
    },
];

struct SyntheticBackend {
    oversized: bool,
    calls: AtomicUsize,
}
#[async_trait]
impl Backend for SyntheticBackend {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(GatewayResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Some(Bytes::from(if self.oversized {
                "x".repeat(5000)
            } else {
                "ok".to_owned()
            })),
            body_is_binary: false,
        })
    }
}
#[derive(Default)]
struct ObservedCompletion(Mutex<Vec<u16>>);
impl Finalizer for ObservedCompletion {
    fn finalize(&self, _: &Invocation, state: &ExchangeState, response: &GatewayResponse) {
        assert!(state.metrics.finalized);
        self.0.lock().unwrap().push(response.status.as_u16());
    }
}

async fn assert_final_outcome(oversized: bool) {
    let dir = tempfile::tempdir().unwrap();
    let proxy = COMPATIBILITY
        .iter()
        .find(|entry| entry.id == HandlerId::Proxy)
        .unwrap()
        .class;
    std::fs::write(
        dir.path().join("handler.yml"),
        serde_yaml::to_string(&json!({
            "enabled": true,
            "handlers": [format!("{proxy}@proxy")],
            "defaultHandlers": ["proxy"]
        }))
        .unwrap(),
    )
    .unwrap();
    let backend = Arc::new(SyntheticBackend {
        oversized,
        calls: AtomicUsize::new(0),
    });
    let completion = Arc::new(ObservedCompletion::default());
    let snapshot = initialize(InitInput {
        embedded: EMBEDDED,
        local_dir: dir.path().join("absent-optional-local"),
        default_dir: None,
        external_dir: dir.path().into(),
        environment: Environment {
            region: Some("us-east-1".into()),
            function_name: "test-only-gateway".into(),
            function_version: "1".into(),
            memory_mb: 128,
            allow_insecure_test_endpoints: false,
        },
        gateway_identity: GatewayIdentity {
            function_name: "test-only-gateway".into(),
            region: "us-east-1".into(),
            account_id: "123456789012".into(),
        },
        handlers: vec![],
        finalizers: vec![completion.clone()],
        backend: BackendConfiguration::Injected(backend.clone()),
    })
    .await
    .unwrap();
    let mut context = lambda_runtime::Context::default();
    context.request_id = "test-only-wp12-final-outcome".into();
    context.deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 5000;
    let event = lambda_runtime::LambdaEvent::new(
        json!({
            "httpMethod": "GET",
            "path": "/x",
            "requestContext": {"elb": {"targetGroupArn": "test-only"}},
            "multiValueHeaders": {}
        }),
        context,
    );
    let envelope: Value = handle(snapshot, event).await.unwrap();
    let expected = if oversized { 502 } else { 200 };
    assert_eq!(envelope["statusCode"], expected);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    let observed = completion.0.lock().unwrap();
    assert_eq!(observed.len(), 1, "exactly one completion callback");
    assert_eq!(
        observed[0], expected,
        "completion must see the final encoded outcome; no second finalization"
    );
}

#[tokio::test]
async fn ordinary_completion_observes_final_alb_status_once() {
    assert_final_outcome(false).await;
}

#[tokio::test]
async fn oversize_completion_observes_final_alb_status_once() {
    assert_final_outcome(true).await;
}
